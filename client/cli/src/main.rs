//! Ferramenta de diagnóstico do motor de captura do `tela'`.
//!
//! Não é scaffolding descartável: os subcomandos `probe-*` são a única forma
//! honesta de verificar captura de hardware, que não tem teste unitário
//! possível. Eles ficam.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use tela_capture::{audio, ffmpeg, pipes, video, whip};

#[derive(Parser, Debug)]
#[command(
    name = "tela",
    about = "Motor de captura do tela' — diagnóstico e publicação",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Lista monitores e janelas capturáveis, com HWND, PID e título.
    List,

    /// Captura vídeo por alguns segundos sem publicar e informa quantos frames
    /// saíram. Com a tela parada o número deve ficar em fps*segundos — é assim
    /// que se prova que a repetição de frame funciona.
    ProbeVideo(ProbeVideoArgs),

    /// Grava PCM cru e informa quantos frames de áudio saíram. Prova o
    /// isolamento por processo e o preenchimento de silêncio.
    ProbeAudio(ProbeAudioArgs),

    /// Fluxo completo: captura, pipes, ffmpeg e publicação via WHIP.
    Share(ShareArgs),
}

#[derive(Args, Debug)]
struct ProbeVideoArgs {
    /// Título (parcial) da janela. Omitido = monitor.
    #[arg(long)]
    window: Option<String>,

    /// Índice do monitor (ver `list`). Omitido = monitor primário.
    #[arg(long)]
    monitor_index: Option<usize>,

    /// Quadros por segundo alvo.
    #[arg(long, default_value_t = 30)]
    fps: u32,

    /// Duração da captura, em segundos.
    #[arg(long, default_value_t = 10)]
    secs: u64,
}

#[derive(Args, Debug)]
struct ProbeAudioArgs {
    /// Título (parcial) da janela: grava só o áudio do processo dono dela.
    #[arg(long, required_unless_present_any = ["system", "system_exceto", "exceto_pid"])]
    window: Option<String>,

    /// Grava o áudio do sistema inteiro (equivale a compartilhar um monitor).
    #[arg(long)]
    system: bool,

    /// Como `--system`, mas EXCLUINDO o app cuja janela contém este texto.
    ///
    /// É o modo anti-eco: ao compartilhar o monitor, exclui o Discord para que
    /// as pessoas da call não se ouçam de volta.
    #[arg(long, conflicts_with_all = ["window", "system"])]
    system_exceto: Option<String>,

    /// Como --system-exceto, mas pelo PID exato. Evita casar com o processo
    /// errado quando vários compartilham prefixo no nome.
    #[arg(long, conflicts_with_all = ["window", "system", "system_exceto"])]
    exceto_pid: Option<u32>,

    /// Duração da gravação, em segundos.
    #[arg(long, default_value_t = 10)]
    secs: u64,

    /// Arquivo de saída, em PCM cru s16le 48 kHz estéreo.
    #[arg(long, default_value = "out.pcm")]
    out: String,
}

#[derive(Args, Debug)]
struct ShareArgs {
    /// Título (parcial) da janela: envia só o áudio daquele aplicativo.
    #[arg(long, conflicts_with = "monitor", required_unless_present = "monitor")]
    window: Option<String>,

    /// Compartilha o monitor primário, com o áudio do sistema inteiro.
    #[arg(long)]
    monitor: bool,

    /// Endpoint WHIP de destino.
    #[arg(long, conflicts_with = "record", required_unless_present = "record")]
    whip: Option<String>,

    /// Em vez de publicar, grava num arquivo local (ex.: teste.mkv).
    ///
    /// Serve para separar problema de captura de problema de rede: se o
    /// arquivo sai com áudio e vídeo sincronizados, a falha está no
    /// transporte, não no motor.
    #[arg(long)]
    record: Option<String>,

    /// Quadros por segundo alvo.
    #[arg(long, default_value_t = 30)]
    fps: u32,

    /// Bitrate de vídeo em kbps.
    #[arg(long, default_value_t = 6000)]
    kbps: u32,

    /// Caminho do executável do ffmpeg.
    #[arg(long, default_value = "ffmpeg")]
    ffmpeg: String,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::List => cmd_list(),
        Command::ProbeVideo(a) => cmd_probe_video(&a),
        Command::ProbeAudio(a) => cmd_probe_audio(&a),
        Command::Share(a) => cmd_share(&a),
    }
}

/// Sinaliza `stop` depois de `secs` segundos, numa thread separada.
fn stop_after(secs: u64) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let handle = Arc::clone(&stop);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(secs));
        handle.store(true, Ordering::Relaxed);
    });
    stop
}

/// Dispara a captura de vídeo e devolve o buffer compartilhado já com o
/// primeiro frame recebido (é ele que revela a resolução), junto da sessão de
/// captura — que precisa continuar viva enquanto se lê do buffer.
fn start_capture(
    window: Option<&video::WindowSource>,
    fps: u32,
) -> Result<(Arc<video::FrameBuffer>, video::Captura)> {
    let buf = video::FrameBuffer::shared();
    let captura = match window {
        Some(w) => video::start_window(
            w.hwnd,
            video::Config {
                destino: video::Destino::Cpu(Arc::clone(&buf)),
                fps,
            },
        )?,
        None => video::start_monitor(
            None,
            video::Config {
                destino: video::Destino::Cpu(Arc::clone(&buf)),
                fps,
            },
        )?,
    };

    anyhow::ensure!(
        buf.wait_first(Duration::from_secs(5)),
        "nenhum frame em 5 s — o Windows Graphics Capture não iniciou"
    );
    Ok((buf, captura))
}

fn cmd_list() -> Result<()> {
    println!("== monitores ==");
    for m in video::list_monitors()? {
        println!(
            "índice={:<3} {}x{:<10} {}",
            m.index, m.width, m.height, m.name
        );
    }

    println!("\n== janelas ==");
    for w in video::list_windows()? {
        let e = video::estado_da_janela(w.hwnd);
        let marca = e
            .porque_nao()
            .map_or(String::new(), |m| format!("  <- {m}"));
        println!(
            "hwnd={:<10} pid={:<7} {:>5}x{:<5} vis={:<5} oculta={:<5} min={:<5} {}{}",
            w.hwnd,
            w.pid,
            e.largura,
            e.altura,
            e.visivel,
            e.oculta_pelo_dwm,
            e.minimizada,
            w.title,
            marca
        );
    }
    Ok(())
}

fn cmd_probe_video(args: &ProbeVideoArgs) -> Result<()> {
    let window = args.window.as_deref().map(video::find_window).transpose()?;
    match &window {
        Some(w) => println!("fonte: janela \"{}\" (pid {})", w.title, w.pid),
        None => println!("fonte: monitor primário"),
    }

    let (buf, captura) = match (&window, args.monitor_index) {
        (Some(w), _) => {
            let buf = video::FrameBuffer::shared();
            let c = video::start_window(
                w.hwnd,
                video::Config {
                    destino: video::Destino::Cpu(std::sync::Arc::clone(&buf)),
                    fps: args.fps,
                },
            )?;
            anyhow::ensure!(
                buf.wait_first(Duration::from_secs(5)),
                "nenhum frame em 5 s"
            );
            (buf, c)
        }
        (None, indice) => {
            let buf = video::FrameBuffer::shared();
            let c = video::start_monitor(
                indice,
                video::Config {
                    destino: video::Destino::Cpu(std::sync::Arc::clone(&buf)),
                    fps: args.fps,
                },
            )?;
            anyhow::ensure!(
                buf.wait_first(Duration::from_secs(5)),
                "nenhum frame em 5 s"
            );
            (buf, c)
        }
    };
    let (width, height) = buf.dims();
    println!("resolução: {width}x{height}");
    // Diagnóstico do relato de "monitor sai preto": um quadro todo preto aqui
    // isola o problema na CAPTURA; se aqui vier imagem, é depois dela.
    if let Some(f) = buf.latest() {
        println!(
            "primeiro quadro: {}",
            if video::parece_preto(&f, width as usize, height as usize) {
                "TODO PRETO"
            } else {
                "com imagem"
            }
        );
    }

    let stop = stop_after(args.secs);
    // `io::sink()` descarta os bytes: aqui só interessa a CONTAGEM. Se o
    // relógio de repetição não estivesse funcionando, com a tela parada este
    // número seria praticamente zero.
    let frames = video::pump(&buf, args.fps, None, std::io::sink(), &stop)?;
    // Quantos o Windows mandou contra quantos convertemos. Num monitor rápido
    // com um jogo aberto, os dois números são bem diferentes — e essa
    // diferença é o trabalho que deixou de ser roubado de quem está jogando.
    let (entregues, convertidos) = video::contadores();
    println!(
        "quadros do Windows: {entregues}  convertidos: {convertidos}  descartados: {}",
        entregues.saturating_sub(convertidos)
    );
    let expected = u64::from(args.fps) * args.secs;
    println!("frames em {}s: {frames} (esperado ~{expected})", args.secs);
    captura.encerra()?;
    Ok(())
}

fn cmd_probe_audio(args: &ProbeAudioArgs) -> Result<()> {
    if let Some(pid) = args.exceto_pid {
        println!("excluindo pid {pid}");
    }
    let source = match (args.window.as_deref(), args.system_exceto.as_deref()) {
        (Some(title), _) => {
            let w = video::find_window(title)?;
            println!("fonte: janela \"{}\" (pid {})", w.title, w.pid);
            audio::AudioSource::Process(w.pid)
        }
        (None, Some(alvo)) => {
            // Casa por nome de EXECUTÁVEL: o app de voz costuma estar
            // minimizado na bandeja, sem janela para casar por título.
            let p = tela_capture::processos::lista()?
                .into_iter()
                .find(|p| p.nome.to_lowercase().contains(&alvo.to_lowercase()))
                .ok_or_else(|| anyhow::anyhow!("nenhum processo com \"{alvo}\" no nome"))?;
            println!("excluindo: {} (pid {})", p.nome, p.pid);
            audio::AudioSource::SistemaExceto(p.pid)
        }
        (None, None) => match args.exceto_pid {
            Some(pid) => audio::AudioSource::SistemaExceto(pid),
            None => audio::AudioSource::System,
        },
    };
    println!("áudio: {source}");

    let file = std::fs::File::create(&args.out).with_context(|| format!("criar {}", args.out))?;
    let stop = stop_after(args.secs);
    let frames = audio::pump(source, file, &stop)?;

    let expected_frames = u64::from(audio::SAMPLE_RATE) * args.secs;
    let expected_bytes = audio::expected_bytes(args.secs);
    println!(
        "frames de áudio: {frames} (esperado ~{expected_frames})\n\
         arquivo: {} — esperado ~{expected_bytes} bytes\n\
         ouvir: ffmpeg -f s16le -ar {} -ac {} -i {} out.wav",
        args.out,
        audio::SAMPLE_RATE,
        audio::CHANNELS,
        args.out
    );
    Ok(())
}

fn cmd_share(args: &ShareArgs) -> Result<()> {
    let window = args.window.as_deref().map(video::find_window).transpose()?;

    // A regra central do produto, em duas linhas: a fonte de vídeo decide o
    // escopo do áudio. Janela = só aquele aplicativo; monitor = som do sistema.
    let audio_source = match &window {
        Some(w) => audio::AudioSource::Process(w.pid),
        None => audio::AudioSource::System,
    };
    match &window {
        Some(w) => println!("fonte: janela \"{}\" (pid {})", w.title, w.pid),
        None => println!("fonte: monitor primário"),
    }
    println!("áudio: {audio_source}");

    // Os pipes são criados ANTES do ffmpeg subir: ele é o cliente, e tentar
    // abrir um pipe que ainda não existe falha na hora.
    let vpipe = pipes::PipeServer::create("v", 0)?;
    let apipe = pipes::PipeServer::create("a", 0)?;
    let (vpath, apath) = (vpipe.path.clone(), apipe.path.clone());

    let (buf, captura) = start_capture(window.as_ref(), args.fps)?;
    let (width, height) = buf.dims();
    let first = buf.latest().context("primeiro frame sumiu do buffer")?;
    // NV12 = w*h*1,5 bytes. Se não bater, a conversão está errada e o vídeo
    // sairia distorcido — falhar aqui é muito mais barato que depurar depois.
    anyhow::ensure!(
        first.len() == (width as usize) * (height as usize) * 3 / 2,
        "frame de {} bytes não corresponde a {width}x{height} em NV12",
        first.len()
    );
    println!("resolução: {width}x{height}");

    let encoder = ffmpeg::pick_encoder(&args.ffmpeg)?;
    println!("encoder: {encoder}");

    // `--record` grava local; senão publica via WHIP. O clap já garante que
    // exatamente um dos dois veio.
    //
    // A publicação WHIP sobe ANTES do ffmpeg de propósito: assim os sockets UDP
    // já estão ligados e o DTLS já fechou quando o primeiro pacote sai. Na
    // ordem inversa, os primeiros segundos de vídeo — inclusive o keyframe
    // inicial — cairiam num socket que ninguém está lendo.
    let mut sessao = match &args.whip {
        Some(url) => Some(abre_whip(url, args.kbps)?),
        None => None,
    };

    let destino = match (&args.record, &sessao) {
        (Some(arquivo), _) => {
            println!("destino: arquivo {arquivo}");
            ffmpeg::Destino::Arquivo(arquivo.as_str())
        }
        (None, Some(s)) => ffmpeg::Destino::Rtp {
            porta_video: s.porta_video,
            porta_audio: s.porta_audio,
        },
        (None, None) => unreachable!("clap exige --whip ou --record"),
    };

    let mut child = ffmpeg::spawn(
        &args.ffmpeg,
        &ffmpeg::Params {
            // O CLI é ferramenta de diagnóstico e segue no caminho antigo.
            apenas_audio: false,
            video_pipe: &vpath,
            audio_pipe: &apath,
            width,
            height,
            fps: args.fps,
            kbps: args.kbps,
            encoder,
            // O CLI não reescala: ele existe para provar o que a captura
            // entrega, e um filtro no meio esconderia justamente isso.
            escala: None,
            // O CLI não lê o `tela-config.json`: quem investiga por aqui passa
            // os argumentos na linha de comando.
            encoder_args: &[],
            destino,
        },
    )?;

    // Sem isto o buffer de stdout do ffmpeg enche com as linhas de -progress e
    // ele trava no meio de um write, congelando a transmissão.
    if let Some(stdout) = child.stdout.take() {
        ffmpeg::drain_progress(stdout, |_line| {});
    }

    let stop = Arc::new(AtomicBool::new(false));
    let (vstop, astop) = (Arc::clone(&stop), Arc::clone(&stop));
    let vbuf = Arc::clone(&buf);
    let fps = args.fps;

    // A ordem aqui não é estética. O ffmpeg abre as entradas em sequência e faz
    // o probe da primeira antes de abrir a segunda: se ninguém estiver
    // escrevendo no pipe de vídeo, ele fica preso lendo e nunca abre o de
    // áudio — e o `accept` do áudio esperaria para sempre. Por isso o pump de
    // vídeo sobe ENTRE os dois accepts.
    let vwriter = vpipe.accept()?;
    let video_thread =
        std::thread::spawn(move || video::pump(&vbuf, fps, Some((width, height)), vwriter, &vstop));
    let awriter = apipe.accept()?;
    let audio_thread = std::thread::spawn(move || audio::pump(audio_source, awriter, &astop));

    let status = supervisiona(&mut child, sessao.as_mut())?;
    stop.store(true, Ordering::Relaxed);
    println!("ffmpeg saiu: {status}");

    // Encerra a sessão no servidor (DELETE no recurso WHIP). Se o processo for
    // morto de fora — Ctrl+C, por exemplo — isto não roda, e o servidor só
    // libera o path quando o ICE dele expirar.
    if let Some(s) = sessao {
        if let Err(e) = s.runtime.block_on(s.publicacao.encerra()) {
            eprintln!("encerrar publicação WHIP: {e:#}");
        }
    }

    // Os dois contadores são a evidência de sincronia A/V: se
    // `frames_de_video / fps` e `frames_de_audio / 48000` divergirem, é sinal
    // de que um dos relógios não segurou a cadência.
    report_pump("vídeo", video_thread.join());
    report_pump("áudio", audio_thread.join());

    if let Err(e) = captura.encerra() {
        eprintln!("encerrar captura: {e:#}");
    }
    Ok(())
}

/// Imprime o resultado de um `pump`, incluindo o caso de a thread ter entrado
/// em pânico.
fn report_pump(nome: &str, resultado: std::thread::Result<Result<u64>>) {
    match resultado {
        Ok(Ok(n)) => println!("frames de {nome}: {n}"),
        Ok(Err(e)) => eprintln!("pump de {nome} falhou: {e:#}"),
        Err(_) => eprintln!("pump de {nome} entrou em pânico"),
    }
}

/// Uma publicação WHIP viva, com o runtime que a sustenta.
///
/// O runtime fica aqui dentro para garantir a ordem de destruição: as tarefas
/// de repasse são abortadas em `encerra()` antes de o runtime cair.
struct SessaoWhip {
    runtime: tokio::runtime::Runtime,
    publicacao: whip::Publicacao,
    porta_video: u16,
    porta_audio: u16,
    /// Leitura anterior, para calcular taxa entre duas amostras.
    anterior: Option<(std::time::Instant, whip::Estatisticas)>,
}

/// Escolhe as portas UDP locais e negocia a sessão WHIP.
fn abre_whip(url: &str, kbps: u32) -> Result<SessaoWhip> {
    // Multi-thread e não `current_thread`: a thread principal fica bloqueada
    // supervisionando o ffmpeg, e as tarefas de repasse precisam de workers
    // próprios para continuar bombeando RTP nesse meio-tempo.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("criar runtime tokio")?;

    let (porta_video, porta_audio) = whip::portas_livres()?;
    println!("destino: {url}");
    println!("rtp local: vídeo 127.0.0.1:{porta_video}, áudio 127.0.0.1:{porta_audio}");

    let publicacao = runtime.block_on(whip::publica(whip::Config {
        em_processo: false,
        whip_url: url,
        porta_video,
        porta_audio,
        kbps_video: kbps,
    }))?;

    println!("whip: conectado");
    if let Some(r) = publicacao.recurso() {
        println!("recurso: {r}");
    }

    Ok(SessaoWhip {
        runtime,
        publicacao,
        porta_video,
        porta_audio,
        anterior: None,
    })
}

/// Espera o ffmpeg terminar, imprimindo as estatísticas do transporte a cada
/// [`INTERVALO_ESTATISTICAS`].
///
/// Sondar com `try_wait` em vez de bloquear em `wait` é o que deixa esta
/// thread livre para reportar o andamento — que é o ponto de uma ferramenta de
/// diagnóstico.
fn supervisiona(
    child: &mut std::process::Child,
    mut sessao: Option<&mut SessaoWhip>,
) -> Result<std::process::ExitStatus> {
    loop {
        if let Some(status) = child.try_wait().context("consultar o ffmpeg")? {
            return Ok(status);
        }
        std::thread::sleep(INTERVALO_ESTATISTICAS);
        if let Some(s) = sessao.as_deref_mut() {
            relata(s);
        }
    }
}

const INTERVALO_ESTATISTICAS: Duration = Duration::from_secs(2);

/// Imprime uma linha de estatísticas, com as taxas medidas contra a amostra
/// anterior.
fn relata(s: &mut SessaoWhip) {
    let agora = std::time::Instant::now();
    let atual = s.runtime.block_on(s.publicacao.estatisticas());

    let (kbps_v, kbps_a) = match s.anterior {
        Some((antes, ref anterior)) => {
            let dt = agora.duration_since(antes).as_secs_f64();
            (
                taxa_kbps(atual.bytes_video, anterior.bytes_video, dt),
                taxa_kbps(atual.bytes_audio, anterior.bytes_audio, dt),
            )
        }
        None => (0.0, 0.0),
    };
    s.anterior = Some((agora, atual));

    println!(
        "[whip] {:<12} vídeo {} pkts {kbps_v:>7.0} kbps (pt {})  áudio {} pkts {kbps_a:>6.0} kbps (pt {})  plis {}  descartados {}  fila-cheia {}  falhas {}  perda-no-caminho {:.1}% (rr {})",
        atual.estado.to_string(),
        atual.pacotes_video,
        atual.pt_video,
        atual.pacotes_audio,
        atual.pt_audio,
        atual.plis,
        atual.descartados,
        atual.fila_cheia,
        atual.falhas_envio,
        atual.perda_por_mil as f64 / 10.0,
        atual.relatorios,
    );
}

/// Taxa em kbps entre duas leituras acumuladas.
fn taxa_kbps(agora: u64, antes: u64, segundos: f64) -> f64 {
    if segundos <= 0.0 {
        return 0.0;
    }
    (agora.saturating_sub(antes) as f64) * 8.0 / 1000.0 / segundos
}
