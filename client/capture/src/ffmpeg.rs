//! Montagem e supervisão do processo ffmpeg, que encoda vídeo e áudio.
//!
//! O ffmpeg **não** publica mais via WHIP: o muxer WHIP dele depende de
//! DTLS-SRTP por OpenSSL, e o build que distribuímos usa SChannel, que falha
//! com `SEC_E_ALGORITHM_MISMATCH`. Hoje ele encoda e cospe RTP em portas UDP
//! locais; quem faz WebRTC é [`crate::whip`].

use std::os::windows::process::CommandExt;
use std::process::{Child, ChildStdout, Command, Stdio};

use anyhow::{Context, Result};

/// Para onde o ffmpeg escreve o que encodou.
pub enum Destino<'a> {
    /// Grava em arquivo local; formato inferido pela extensão. Diagnóstico:
    /// separa problema de captura de problema de transporte.
    Arquivo(&'a str),
    /// Emite RTP em duas portas UDP locais, para o publicador WHIP em Rust.
    Rtp { porta_video: u16, porta_audio: u16 },
}

/// Parâmetros de uma sessão de publicação.
pub struct Params<'a> {
    pub video_pipe: &'a str,
    pub audio_pipe: &'a str,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub kbps: u32,
    pub encoder: &'a str,
    /// Resolução de saída, quando diferente da capturada. `None` = enviar no
    /// tamanho nativo.
    ///
    /// Existe porque os presets do app são em 1080p e um monitor 1440p
    /// custaria banda à toa: reduzir aqui é a diferença entre caber e não
    /// caber na conexão de quem assiste. `width`/`height` continuam sendo o
    /// tamanho do que ENTRA pelo pipe — o rawvideo precisa do tamanho exato
    /// dos bytes, não do desejado.
    pub escala: Option<(u32, u32)>,
    /// Argumentos extras do encoder, vindos do `tela-config.json`.
    pub encoder_args: &'a [String],
    pub destino: Destino<'a>,
    /// Só encoda áudio: o vídeo está sendo encodado em processo.
    ///
    /// Quando ligado, `video_pipe`, `width`, `height`, `escala` e `encoder`
    /// são ignorados, e o áudio passa a ser a entrada 0. O ffmpeg continua
    /// existindo porque o Opus não custa quase nada e reescrever a trilha de
    /// áudio agora seria mudar duas coisas ao mesmo tempo.
    pub apenas_audio: bool,
}

/// Payload types pedidos ao ffmpeg. São só um valor inicial: a `webrtc`
/// sobrescreve o campo com o PT negociado no SDP antes de encriptar (o porquê
/// está em [`crate::whip::publica`]).
const PT_VIDEO: &str = "96";
const PT_AUDIO: &str = "111";

/// Tamanho máximo do pacote RTP pedido ao ffmpeg.
///
/// O padrão do ffmpeg (1472) enche exatamente uma MTU de 1500 com IP+UDP; o
/// SRTP ainda soma um auth tag de 10 bytes por cima, e o datagrama passa a
/// fragmentar. 1200 é o valor que o WebRTC usa na prática e sobrevive a
/// qualquer túnel no caminho.
const PKT_SIZE_RTP: &str = "1200";

/// `CREATE_NO_WINDOW`: impede o Windows de abrir um console para o ffmpeg.
///
/// Um processo GUI que dispara um programa de console faz o sistema criar uma
/// janela preta para ele. Sem esta flag, uma janela de `cmd` pisca (ou fica) na
/// tela toda vez que o app detecta o encoder ou começa a transmitir — parece
/// defeito, e para quem não conhece o programa parece coisa pior.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Encoders em cascata, do melhor para o fallback garantido.
/// Ordem de preferência dos encoders.
///
/// # O `h264_amf` voltou ao lugar dele
///
/// Ele chegou a ficar depois do `libx264` — o que o desativava — porque
/// produzia um fluxo que ninguém conseguia montar. A causa foi encontrada e
/// corrigida em `whip::Parametros`: o AMF um fluxo que nenhum navegador consegue montar: ele manda
/// SPS/PPS uma única vez, no começo, e nunca mais. Quem já estava assistindo
/// funciona; quem entra depois — ou dá F5 — nunca mais monta imagem. Medido no
/// navegador de quem assistia: 37 MB recebidos, ZERO pacotes perdidos, ZERO
/// quadros montados e 486 pedidos de keyframe sem resposta. E confirmado pelo
/// caso decisivo: quem transmitia via a própria tela, quem chegou depois não.
///
/// O `dump_extra` do ffmpeg, que é a correção citada em toda parte, foi
/// medido e NÃO funciona nesta saída: reinsere os cabeçalhos num arquivo e não
/// num fluxo RTP. Por isso a correção mora no nosso repasse de pacotes.
///
/// Se ainda assim algum encoder nunca emitir os parâmetros, o supervisor o
/// descarta depois de dez segundos e o app troca sozinho para o próximo —
/// `libx264` fecha a lista e sempre funciona.
///
/// `"encoder"` no `tela-config.json` tem precedência sobre esta ordem.
pub const ENCODER_CASCADE: [&str; 4] = ["h264_nvenc", "h264_qsv", "h264_amf", "libx264"];

/// Primeiro encoder da cascata que **funciona de verdade nesta máquina**.
///
/// # Por que não basta `ffmpeg -encoders`
///
/// Aquela lista mostra o que foi **compilado** no binário, não o que a máquina
/// consegue executar. O build que distribuímos traz NVENC, QSV e AMF — então os
/// três aparecem em qualquer PC, inclusive nos que não têm a GPU
/// correspondente.
///
/// O resultado era pior que um erro: escolhíamos `h264_nvenc` num PC sem driver
/// NVIDIA, o ffmpeg falhava com `Cannot load nvcuda.dll` só ao abrir o encoder,
/// e como isso acontece **depois** de a sessão WHIP subir, o app continuava
/// dizendo "transmitindo" enquanto nada saía. Diagnosticar isso a distância
/// custou três rodadas.
///
/// Aqui cada candidato é exercitado num encode minúsculo e sintético. Custa
/// alguns décimos de segundo uma vez por transmissão, e é a única resposta que
/// não mente.
pub fn pick_encoder(ffmpeg: &str) -> Result<&'static str> {
    let (escolhido, recusados) = sonda_encoders(ffmpeg)?;
    if let Some(e) = escolhido {
        return Ok(e);
    }
    anyhow::bail!(
        "nenhum encoder H.264 utilizável nesta máquina — {}",
        recusados.join("; ")
    )
}

/// Como [`pick_encoder`], mas devolve também por que os anteriores foram
/// recusados.
///
/// Existe porque cair em `libx264` num PC com GPU capaz é um problema sério —
/// o jogo de quem compartilha perde quadros — e antes disto o log não dizia
/// uma palavra sobre o motivo. Só descobrimos porque alguém reclamou de lag.
pub fn sonda_encoders(ffmpeg: &str) -> Result<(Option<&'static str>, Vec<String>)> {
    let compilados = encoders_compilados(ffmpeg)?;
    let mut recusados: Vec<String> = Vec::new();

    for e in ENCODER_CASCADE {
        if !compilados.contains(e) {
            recusados.push(format!("{e}: não está neste build"));
            continue;
        }
        match testa_encoder(ffmpeg, e) {
            Ok(()) => return Ok((Some(e), recusados)),
            Err(motivo) => recusados.push(format!("{e}: {motivo}")),
        }
    }

    Ok((None, recusados))
}

/// Nomes de encoder presentes no build.
fn encoders_compilados(ffmpeg: &str) -> Result<String> {
    let out = Command::new(ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .with_context(|| format!("executar {ffmpeg} -encoders"))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Encoda um quadro de teste e confere que o resultado TEM IMAGEM.
///
/// # Por que decodificar de volta
///
/// A primeira versão só via se o encoder abria, e codificava `nullsrc` — que é
/// uma imagem preta. Um encoder que inicializa bem e produz preto passava
/// direto: foi assim que uma Radeon RX 6600 foi promovida para `h264_amf` e
/// transmitiu tela preta para a sala inteira. Codificar um padrão colorido e
/// decodificar de volta é o único jeito de a sonda responder à pergunta que
/// interessa, que não é "abre?" e sim "sai imagem?".
///
/// # Tamanho realista
///
/// 1280x720 e não 64x64: encoders de hardware têm mínimos e alinhamentos, e
/// recusar um quadro minúsculo não diz nada sobre o que a transmissão pede.
fn testa_encoder(ffmpeg: &str, encoder: &str) -> std::result::Result<(), String> {
    let temp = std::env::temp_dir().join(format!("tela-sonda-{encoder}.h264"));
    let _ = std::fs::remove_file(&temp);

    // `testsrc` é um padrão com barras coloridas e um contador — nada nele é
    // preto por acaso.
    let saida = Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc=s=1280x720:r=30"])
        .args(["-pix_fmt", "nv12"])
        .args(["-frames:v", "8", "-c:v", encoder])
        .args(tuning(encoder))
        .args(["-f", "h264"])
        .arg(&temp)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("não consegui executar o teste: {e}"))?;

    if !saida.status.success() {
        let _ = std::fs::remove_file(&temp);
        // A mensagem do ffmpeg é o que explica a recusa para quem for ler o
        // log ("Cannot load nvcuda.dll" é bem mais útil que "falhou").
        let erro = String::from_utf8_lossy(&saida.stderr);
        let primeira = erro
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("sem detalhe")
            .trim()
            .to_string();
        return Err(primeira);
    }

    let veredito = ultimo_quadro_tem_imagem(ffmpeg, &temp);
    let _ = std::fs::remove_file(&temp);
    match veredito {
        Ok(true) => Ok(()),
        Ok(false) => Err("codifica, mas a imagem sai preta".to_string()),
        Err(e) => Err(e),
    }
}

/// Tira o `[componente @ 000001A2B3C4]` que o ffmpeg põe na frente.
///
/// O endereço muda a cada execução, o que faz duas falhas idênticas parecerem
/// diferentes — e o relatório da sonda existe justamente para comparar máquinas.
pub(crate) fn sem_prefixo(s: &str) -> String {
    let Some(resto) = s.strip_prefix('[') else {
        return s.to_string();
    };
    match resto.split_once("] ") {
        // Só corta se o miolo tem cara de `nome @ endereço`; um `[algo]` que
        // faça parte da mensagem fica onde está.
        Some((dentro, depois)) if dentro.contains(" @ ") => depois.trim().to_string(),
        _ => s.to_string(),
    }
}

/// Diz, para cada encoder, se ele aceita quadro que **já está na GPU**.
///
/// # Para que serve
///
/// Estamos construindo um caminho em que a textura da captura vai direto ao
/// encoder, sem passar pela CPU — 0,94 ms por quadro contra os ~168% de um
/// núcleo de hoje. Só que nem todo encoder aceita `AV_PIX_FMT_D3D11`, e das
/// quatro máquinas que usam isto, só duas puderam ser testadas.
///
/// Esta sonda responde pela máquina de quem usa, sem arriscar nada: ela roda
/// no `ffmpeg.exe` que já está em cache, não muda uma vírgula da transmissão,
/// e o resultado vai só para o log.
///
/// # Por que não captura a tela
///
/// Testar com `ddagrab` seria mais fiel, mas capturaria a tela de outra pessoa
/// para responder uma pergunta nossa. `testsrc2` subido para a GPU com
/// `hwupload` exercita a mesma coisa: o encoder recebendo `AV_PIX_FMT_D3D11`.
#[must_use]
pub fn sonda_d3d11(ffmpeg: &str) -> String {
    let mut partes = Vec::new();
    for encoder in ["h264_nvenc", "h264_amf", "h264_qsv"] {
        partes.push(match testa_d3d11(ffmpeg, encoder) {
            Ok(()) => format!("{encoder} ok"),
            Err(e) => format!("{encoder} não: {e}"),
        });
    }
    partes.join("; ")
}

/// Encoda um padrão colorido que já nasce na GPU e confere se sai imagem.
///
/// Decodifica de volta pelo mesmo motivo que [`testa_encoder`]: uma Radeon
/// RX 6600 já inicializou bem e transmitiu preto para a sala inteira. "Abre"
/// não é a pergunta; "sai imagem" é.
fn testa_d3d11(ffmpeg: &str, encoder: &str) -> std::result::Result<(), String> {
    let temp = std::env::temp_dir().join(format!("tela-sonda-d3d11-{encoder}.h264"));
    let _ = std::fs::remove_file(&temp);

    let saida = Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        // O dispositivo D3D11 tem de existir ANTES do filtro, porque é o
        // `hwupload` que o usa para alocar a textura de destino.
        .args(["-init_hw_device", "d3d11va=dx", "-filter_hw_device", "dx"])
        .args(["-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30"])
        // BGRA e não NV12: é o formato em que a captura entrega, e criar
        // textura NV12 falha com `E_INVALIDARG` em parte das GPUs.
        .args(["-vf", "format=bgra,hwupload"])
        .args(["-frames:v", "8", "-c:v", encoder])
        .args(tuning(encoder))
        .args(["-f", "h264"])
        .arg(&temp)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("não consegui executar o teste: {e}"))?;

    if !saida.status.success() {
        let _ = std::fs::remove_file(&temp);
        let erro = String::from_utf8_lossy(&saida.stderr);
        let primeira = erro
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("sem detalhe")
            .trim()
            .to_string();
        return Err(sem_prefixo(&primeira));
    }

    let veredito = ultimo_quadro_tem_imagem(ffmpeg, &temp);
    let _ = std::fs::remove_file(&temp);
    match veredito {
        Ok(true) => Ok(()),
        Ok(false) => Err("codifica da GPU, mas a imagem sai preta".to_string()),
        Err(e) => Err(e),
    }
}

/// Decodifica o que a sonda gravou e diz se há imagem.
///
/// Reduz a um único pixel em escala de cinza: é a média de tudo, e um quadro
/// preto dá zero. Barato e suficiente — a pergunta é binária.
pub(crate) fn ultimo_quadro_tem_imagem(
    ffmpeg: &str,
    arquivo: &std::path::Path,
) -> std::result::Result<bool, String> {
    let saida = Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error"])
        .arg("-i")
        .arg(arquivo)
        .args(["-vf", "scale=1:1"])
        .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "gray", "-"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("não consegui decodificar o teste: {e}"))?;

    if !saida.status.success() || saida.stdout.is_empty() {
        return Err("o que foi codificado não decodifica de volta".to_string());
    }
    // Limiar baixo de propósito: qualquer coisa acima de preto absoluto conta.
    // O padrão de teste tem barras claras e daria bem mais que isso.
    Ok(saida.stdout.iter().any(|&v| v > 8))
}

/// Ajustes de baixa latência específicos por encoder.
/// Ajustes por encoder, e o perfil H.264 que cada um deve produzir.
///
/// # Por que o perfil é explícito
///
/// Nunca dizíamos qual queríamos, e cada encoder escolhia o seu: medido neste
/// build, `libx264` sai em Constrained Baseline e `h264_nvenc` em High. O
/// WebRTC negocia perfil no SDP, e mandar um mais alto que o combinado é um
/// jeito silencioso de o navegador não decodificar nada — imagem preta com
/// áudio funcionando. Constrained Baseline é o que todo navegador aceita.
///
/// # E por que B-frames vão desligados
///
/// Quadros B saem fora de ordem de apresentação, e o RTP do WebRTC não tem
/// como remontá-los. O `libx264` já os desliga por causa do `zerolatency` —
/// que é parte de por que ele nunca deu problema —, mas os de hardware não
/// desligam sozinhos.
fn tuning(encoder: &str) -> Vec<&'static str> {
    match encoder {
        "libx264" => vec![
            "-preset",
            "ultrafast",
            "-tune",
            "zerolatency",
            "-profile:v",
            "baseline",
            "-bf",
            "0",
        ],
        "h264_nvenc" => vec![
            "-preset",
            "p1",
            "-tune",
            "ll",
            "-rc",
            "cbr",
            "-profile:v",
            "baseline",
            "-bf",
            "0",
        ],
        "h264_qsv" => vec![
            "-preset",
            "veryfast",
            "-low_power",
            "1",
            "-profile:v",
            "baseline",
            "-bf",
            "0",
        ],
        // O AMF precisa de dois ajustes que os outros não precisam, e os dois
        // padrões dele são incompatíveis com WebRTC:
        //
        // `header_spacing` vem -1 (desligado): ele NÃO reinsere SPS/PPS ao
        // longo do fluxo. Sem esses parâmetros o decodificador não sabe montar
        // imagem nenhuma. `libx264` e `nvenc` os repetem a cada keyframe
        // sozinhos — medido: SPS=3 e PPS=3 para 3 keyframes —, e é por isso
        // que só o AMF quebrava.
        //
        // `forced_idr` vem false: os quadros I podem não ser IDR, e um I que
        // não é IDR não serve de ponto de entrada para quem chega depois.
        //
        // Juntos davam o sintoma exato medido no navegador de quem assistia:
        // 37 MB recebidos, zero pacotes perdidos, zero quadros montados, e 486
        // pedidos de keyframe sem resposta.
        "h264_amf" => vec![
            "-usage",
            "lowlatency",
            "-rc",
            "cbr",
            "-profile:v",
            "constrained_baseline",
            "-bf",
            "0",
            "-header_spacing",
            "1",
            "-forced_idr",
            "1",
        ],
        _ => vec![],
    }
}

/// Acrescenta um grupo de argumentos à linha de comando.
fn push(args: &mut Vec<String>, xs: &[&str]) {
    args.extend(xs.iter().map(|s| (*s).to_string()));
}

/// Sobe o ffmpeg lendo dos dois pipes e publicando no endpoint WHIP.
///
/// O stdout do processo fica em `Stdio::piped()` para carregar as linhas de
/// `-progress`. Quem chamar **precisa** drenar esse stdout (ver
/// [`drain_progress`]): o buffer do pipe é finito e, se ninguém ler, o ffmpeg
/// bloqueia ao escrever telemetria e a transmissão congela.
pub fn spawn(ffmpeg: &str, p: &Params) -> Result<Child> {
    let bitrate = format!("{}k", p.kbps);
    let bufsize = format!("{}k", p.kbps / 4);
    // GOP de 2 s: garante que quem entra no meio da transmissão receba um
    // keyframe rápido, já que o muxer WHIP não propaga PLI (R4 da spec).
    let gop = (p.fps * 2).to_string();
    let fps = p.fps.to_string();
    let size = format!("{}x{}", p.width, p.height);

    // A ordem importa: toda opção de saída precisa vir ANTES da URL final.
    let mut args: Vec<String> = Vec::new();

    push(&mut args, &["-hide_banner", "-loglevel", "warning"]);
    // Telemetria de envio, lida pela UI no Plano 2. É opção global, então vive
    // aqui em cima — no modo RTP há duas saídas e ela não pertence a nenhuma.
    push(&mut args, &["-progress", "pipe:1", "-nostats"]);
    // Entrada de vídeo.
    //
    // SEM `-r` aqui, e isso é o oposto do que parece natural.
    //
    // Com `-r 60`, o demuxer de rawvideo carimba cada quadro por ÍNDICE:
    // quadro n vale n/60 de linha do tempo, doa o que doer no relógio. Se a
    // captura entrega 36 quadros por segundo em vez de 60, a linha do tempo
    // anda a 0,6× o tempo real — e como o navegador sincroniza o áudio pelo
    // vídeo, ele SEGURA o áudio para casar. O resultado é áudio chegando cada
    // vez mais atrasado, sem perder um pacote sequer.
    //
    // Medido: alimentando 11 quadros em 15,6 s reais, o vídeo com `-r 60`
    // saiu com 0,18 s de duração (11÷60). Sem `-r`, 120 quadros em 4,01 s
    // saíram com 4,04 s — o relógio de verdade.
    //
    // Sem `-r`, o `-use_wallclock_as_timestamps` passa a valer: cada quadro
    // recebe a hora em que chegou, e entregar menos quadros vira um vídeo
    // menos fluido em vez de um atraso que só cresce.
    if !p.apenas_audio {
        push(&mut args, &["-use_wallclock_as_timestamps", "1"]);
        push(&mut args, &["-f", "rawvideo", "-pix_fmt", "nv12"]);
        // `-framerate` e NÃO `-r`. A diferença é tudo aqui:
        //
        // `-r` na entrada **carimba** cada quadro por índice (quadro n vale n/60),
        // e aí entregar menos quadros que o declarado encolhe a linha do tempo —
        // o vídeo anda mais devagar que o relógio e o navegador segura o áudio
        // para casar. Era a origem do áudio chegando cada vez mais atrasado.
        //
        // `-framerate` só declara a taxa NOMINAL do fluxo, sem mexer nos
        // carimbos, que continuam vindo do relógio. Tirar a declaração inteira
        // também não serve: o demuxer de rawvideo assume 25 fps e o caminho real
        // passou a andar a 25 (medido: 58 fps antes, 25 depois).
        push(&mut args, &["-framerate", &fps]);
        push(&mut args, &["-s", &size]);
        push(&mut args, &["-i", p.video_pipe]);
    }
    // Entrada de áudio.
    //
    // Sem `-use_wallclock_as_timestamps` aqui, e isso é deliberado: PCM cru não
    // carrega marca de tempo, então o wallclock carimbaria o INSTANTE DA LEITURA
    // do pipe. Leitura de pipe acontece em rajadas, os carimbos andam para trás,
    // e o libopus rejeita DTS não-monotônico e simplesmente para de emitir
    // (medido: 563 pacotes caem para 8). O muxer de arquivo tolera e mascara o
    // problema; o RTP não.
    //
    // Deixar o ffmpeg derivar o tempo da contagem de amostras é melhor em todo
    // sentido: é monotônico por construção, e continua sendo tempo real porque
    // o pump de áudio já entrega no ritmo do relógio (mitigação do R2).
    push(&mut args, &["-f", "s16le", "-ar", "48000", "-ac", "2"]);
    push(&mut args, &["-i", p.audio_pipe]);
    // Encode de vídeo. No modo RTP isto vira as opções da primeira saída; no
    // modo arquivo, da única saída. São as mesmas em qualquer caso — o que
    // muda é só onde o resultado é escrito.
    let mut video: Vec<String> = Vec::new();
    if let Some((w, h)) = p.escala {
        // `flags=fast_bilinear` porque isto roda em tempo real na CPU a cada
        // frame: qualidade de reescala não é o gargalo perceptual aqui, o
        // bitrate é.
        video.push("-vf".to_string());
        video.push(format!("scale={w}:{h}:flags=fast_bilinear"));
    }
    push(&mut video, &["-c:v", p.encoder]);
    push(&mut video, &tuning(p.encoder));
    // Depois do tuning, para poder sobrescrevê-lo.
    video.extend(p.encoder_args.iter().cloned());
    push(
        &mut video,
        &["-b:v", &bitrate, "-maxrate", &bitrate, "-bufsize", &bufsize],
    );
    // `passthrough`: cada quadro que entra sai, com o carimbo de tempo que
    // trouxe. Sem isto o ffmpeg reamostra a saída para a taxa NOMINAL da
    // entrada — e como o demuxer de rawvideo assume 25 fps quando nada é
    // declarado, ele descartava tudo que passasse disso.
    //
    // Medido alimentando 120 quadros em 4,00 s: sem `passthrough` saíram 92
    // (25 fps); com ele, 120, e a duração bateu em 4,00 s. A alternativa de
    // declarar `-r 60` na entrada mantinha os 120 quadros mas comprimia a
    // duração para 2,00 s — que é justamente o atraso de áudio que isto veio
    // consertar. Só as duas coisas juntas ficam certas.
    // Cinto e suspensório para o SPS/PPS: reinsere os cabeçalhos antes de cada
    // keyframe, seja qual for o encoder. Quem já os repetia não muda (medido
    // com o nvenc: mesma contagem de NAL antes e depois, e a imagem decodifica
    // igual); quem não repetia passa a repetir. Custa algumas dezenas de bytes
    // por keyframe contra uma sala inteira olhando para o nada.
    push(&mut video, &["-bsf:v", "dump_extra=freq=keyframe"]);
    push(&mut video, &["-fps_mode", "passthrough"]);
    // Quadro-chave a cada dois segundos, imposto pelo ffmpeg e não pelo
    // encoder.
    //
    // O `-g` é só uma sugestão de GOP, e encoder de hardware em modo de baixa
    // latência pode ignorá-la: o `h264_amf` produzia UM único IDR e depois só
    // quadros P, para sempre. Quem já assistia via normalmente; quem chegava
    // depois esperava um quadro-chave que nunca vinha.
    //
    // Medido com GOP infinito para reproduzir o caso: sem isto, 1 keyframe em
    // 6 segundos; com isto, 3 — um a cada dois segundos, como pedido. E como
    // cada keyframe leva os parâmetros da imagem junto, isto e a reinserção do
    // `whip::Parametros` resolvem partes diferentes do mesmo problema.
    //
    // O outro caminho seria atender aos PLIs que o navegador manda pedindo
    // keyframe — nós os contamos e nunca agimos sobre eles —, mas o ffmpeg não
    // aceita esse pedido em tempo de execução. Forçar por tempo não depende de
    // ninguém cooperar.
    push(&mut video, &["-force_key_frames", "expr:gte(t,n_forced*2)"]);
    push(&mut video, &["-g", &gop, "-pix_fmt", "yuv420p"]);

    let mut audio: Vec<String> = Vec::new();
    push(
        &mut audio,
        &[
            "-c:a", "libopus", "-b:a", "128k", "-ar", "48000", "-ac", "2",
        ],
    );

    match p.destino {
        Destino::Arquivo(caminho) => {
            args.extend(video);
            args.extend(audio);
            // Sem `-f`: a extensão decide o container.
            push(&mut args, &[caminho]);
        }
        Destino::Rtp {
            porta_video,
            porta_audio,
        } if p.apenas_audio => {
            // Sem vídeo o áudio é a entrada 0, e a porta de vídeo não é usada:
            // quem publica o vídeo é o `whip::EmissorH264`.
            let _ = porta_video;
            let url_a = format!("rtp://127.0.0.1:{porta_audio}?pkt_size={PKT_SIZE_RTP}");
            push(&mut args, &["-map", "0:a"]);
            args.extend(audio);
            push(&mut args, &["-payload_type", PT_AUDIO, "-f", "rtp"]);
            args.push(url_a);
        }
        Destino::Rtp {
            porta_video,
            porta_audio,
        } => {
            // Duas saídas, uma por mídia. O muxer RTP não multiplexa dois
            // streams num socket só (cada mídia tem seu SSRC e sua timebase),
            // e o WebRTC quer justamente isso: uma track por mídia.
            let url_v = format!("rtp://127.0.0.1:{porta_video}?pkt_size={PKT_SIZE_RTP}");
            let url_a = format!("rtp://127.0.0.1:{porta_audio}?pkt_size={PKT_SIZE_RTP}");

            push(&mut args, &["-map", "0:v"]);
            args.extend(video);
            push(&mut args, &["-payload_type", PT_VIDEO, "-f", "rtp"]);
            args.push(url_v);

            push(&mut args, &["-map", "1:a"]);
            args.extend(audio);
            push(&mut args, &["-payload_type", PT_AUDIO, "-f", "rtp"]);
            args.push(url_a);
        }
    }

    // Com TELA_DEBUG_FFMPEG=1 imprime a linha de comando montada. Depurar
    // problema de encode sem ver os argumentos exatos é adivinhação.
    if std::env::var_os("TELA_DEBUG_FFMPEG").is_some() {
        let linha: Vec<String> = args
            .iter()
            .map(|a| {
                if a.contains(' ') {
                    format!("\"{a}\"")
                } else {
                    a.clone()
                }
            })
            .collect();
        eprintln!("[ffmpeg] {ffmpeg} {}", linha.join(" "));
    }

    Command::new(ffmpeg)
        .args(&args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::piped())
        // `piped` e nao `inherit`: num app GUI do Windows nao existe console,
        // entao `inherit` joga o stderr do ffmpeg fora. Quem chama drena com
        // `drain_stderr` e manda para o log — sem isso, um erro de encoder na
        // maquina de outra pessoa e invisivel.
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("iniciar {ffmpeg}"))
}

/// Consome o stderr do ffmpeg numa thread, entregando cada linha a `on_line`.
///
/// Obrigatorio pelo mesmo motivo do stdout: buffer de pipe e finito, e se
/// ninguem ler o ffmpeg trava ao escrever uma mensagem de erro — justamente
/// quando havia algo errado para contar.
pub fn drain_stderr<F>(
    stderr: std::process::ChildStderr,
    mut on_line: F,
) -> std::thread::JoinHandle<()>
where
    F: FnMut(&str) + Send + 'static,
{
    std::thread::spawn(move || {
        use std::io::BufRead;
        let reader = std::io::BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            on_line(&line);
        }
    })
}

/// Consome o stdout do ffmpeg numa thread, entregando cada linha de
/// `-progress` a `on_line`.
///
/// Não é opcional: sem alguém lendo, o buffer do pipe de stdout enche e o
/// ffmpeg trava dentro de um `write`, parando a transmissão inteira.
pub fn drain_progress<F>(stdout: ChildStdout, mut on_line: F) -> std::thread::JoinHandle<()>
where
    F: FnMut(&str) + Send + 'static,
{
    std::thread::spawn(move || {
        use std::io::BufRead;
        let reader = std::io::BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(line) => on_line(&line),
                // ffmpeg terminou ou o pipe quebrou: nada a fazer além de sair.
                Err(_) => break,
            }
        }
    })
}

#[cfg(test)]
mod testes_sonda {
    /// A sonda tem de continuar APROVANDO um encoder que funciona.
    ///
    /// Guarda contra o risco de trocar um defeito por outro: ao passar a exigir
    /// imagem de verdade, um limiar mal escolhido reprovaria todo mundo e
    /// ninguém conseguiria transmitir. Roda contra o ffmpeg de `bin/`, então é
    /// ignorado por padrão — em outra máquina o caminho não existe.
    #[test]
    #[ignore = "precisa do ffmpeg em bin/"]
    fn aprova_um_encoder_que_presta() {
        // Relativo ao crate, não ao diretório de trabalho: `cargo test` roda
        // dentro de `capture/`, e um caminho relativo à raiz simplesmente não
        // existia — o teste se pulava em silêncio e não provava nada.
        let ff = concat!(env!("CARGO_MANIFEST_DIR"), "/../bin/ffmpeg.exe");
        assert!(
            std::path::Path::new(ff).exists(),
            "não achei o ffmpeg em {ff}"
        );
        let (escolhido, recusados) = super::sonda_encoders(ff).expect("sondar");
        for r in &recusados {
            println!("recusado: {r}");
        }
        println!("escolhido: {escolhido:?}");
        assert!(
            escolhido.is_some(),
            "a sonda reprovou TODOS os encoders — recusas: {recusados:?}"
        );
    }

    /// E o `libx264` — o último da cascata, que existe em todo build — tem de
    /// passar sozinho. Se nem ele passar, o problema é o teste de imagem.
    #[test]
    #[ignore = "precisa do ffmpeg em bin/"]
    fn libx264_passa_no_teste_de_imagem() {
        let ff = concat!(env!("CARGO_MANIFEST_DIR"), "/../bin/ffmpeg.exe");
        assert!(
            std::path::Path::new(ff).exists(),
            "não achei o ffmpeg em {ff}"
        );
        assert_eq!(super::testa_encoder(ff, "libx264"), Ok(()));
    }
}

#[cfg(test)]
mod testes_sonda_d3d11 {
    /// Não afirma qual encoder passa — depende da GPU. Garante que a sonda
    /// responde algo sobre os três e não entra em pânico.
    #[test]
    #[ignore = "precisa do ffmpeg em bin/"]
    fn relata_os_tres_encoders() {
        // Mesmo caminho relativo ao crate que os outros testes de sonda usam.
        let ff = concat!(env!("CARGO_MANIFEST_DIR"), "/../bin/ffmpeg.exe");
        assert!(
            std::path::Path::new(ff).exists(),
            "não achei o ffmpeg em {ff}"
        );
        let relato = super::sonda_d3d11(ff);
        eprintln!("sonda d3d11: {relato}");
        for encoder in ["h264_nvenc", "h264_amf", "h264_qsv"] {
            assert!(relato.contains(encoder), "faltou {encoder} em: {relato}");
        }
    }
}

#[cfg(test)]
mod testes_prefixo {
    use super::sem_prefixo;

    #[test]
    fn tira_o_prefixo_do_ffmpeg() {
        assert_eq!(
            sem_prefixo("[AMF @ 0000019D] DLL amfrt64.dll failed to open"),
            "DLL amfrt64.dll failed to open"
        );
    }

    #[test]
    fn preserva_colchete_que_e_da_mensagem() {
        // Sem ` @ ` dentro não é prefixo do ffmpeg, e cortar comeria conteúdo.
        assert_eq!(sem_prefixo("[aviso] algo"), "[aviso] algo");
        assert_eq!(sem_prefixo("sem colchete nenhum"), "sem colchete nenhum");
    }
}
