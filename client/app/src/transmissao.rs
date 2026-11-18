//! Orquestração de uma transmissão: captura → pipes → ffmpeg → WHIP.
//!
//! A sequência (e a ordem dela, que não é estética) veio do `cmd_share` do
//! CLI, que é onde ela foi validada por medição. O que muda aqui é só o
//! entorno: em vez de imprimir e sair, este módulo mantém a sessão viva,
//! reporta telemetria para a interface e sabe parar.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Serialize;
use tela_capture::{audio, ffmpeg, foco, pipes, video, whip};
use tokio::runtime::Runtime;

/// Intervalo entre amostras de telemetria. Dois segundos é curto o bastante
/// para a pessoa perceber que algo travou e longo o bastante para a taxa
/// medida não virar ruído.
const INTERVALO: Duration = Duration::from_secs(2);

/// Rodadas de imagem preta antes de avisar. A `INTERVALO` de 2 s, são ~6 s.
///
/// Nem tão curto que uma tela de carregamento ou um fade do jogo disparem o
/// aviso, nem tão longo que a sala fique minutos olhando para o nada.
const RODADAS_PARA_ESCURO: u32 = 3;

/// Teto de espera pelo primeiro frame do WGC.
/// Quanto esperar pelo primeiro quadro já CODIFICADO no caminho em processo.
///
/// Generoso porque inclui abrir o encoder de hardware, que na primeira vez
/// carrega o driver; e porque o custo de esperar demais é uma transmissão que
/// demora a começar, contra o de esperar de menos, que é cair para o
/// `ffmpeg.exe` numa máquina onde a GPU daria conta.
const ESPERA_PRIMEIRO_CODIFICADO: Duration = Duration::from_secs(8);

const ESPERA_PRIMEIRO_FRAME: Duration = Duration::from_secs(5);

/// Qual fonte transmitir. `hwnd`/`pid` vêm do mesmo registro de
/// [`video::list_windows`] — nunca de duas buscas separadas, senão o vídeo de
/// uma janela poderia sair com o áudio de outra.
#[derive(Debug, Clone)]
pub enum Fonte {
    Janela {
        hwnd: isize,
        pid: u32,
        titulo: String,
    },
    Monitor {
        indice: usize,
        nome: String,
        /// PID de um aplicativo de voz a **excluir** do áudio do sistema.
        ///
        /// Sem isto, compartilhar o monitor manda junto a saída do Discord —
        /// ou seja, a voz das outras pessoas da call — e elas se ouvem de volta
        /// com atraso. Excluir aquele processo mata o eco sem perder o som do
        /// jogo nem do vídeo.
        excluir_pid: Option<u32>,
    },
}

impl Fonte {
    /// `"window"` ou `"monitor"`, como o protocolo da sala espera.
    pub fn tipo_protocolo(&self) -> &'static str {
        match self {
            Self::Janela { .. } => "window",
            Self::Monitor { .. } => "monitor",
        }
    }

    pub fn titulo(&self) -> &str {
        match self {
            Self::Janela { titulo, .. } => titulo,
            Self::Monitor { nome, .. } => nome,
        }
    }
}

/// Motivo de encerramento quando o programa mudou de janela sem fechar a
/// antiga.
///
/// É o caso do League: o cliente NÃO fecha quando a partida começa, ele só
/// some de vista. Sem este caminho, quem compartilhou o cliente passava a
/// partida inteira transmitindo uma janela escondida — foi o que aconteceu num
/// log de 46 minutos, com uma captura só, do começo ao fim.
pub const MOTIVO_FONTE_TROCOU: &str = "o programa passou para outra janela";

/// Motivo de encerramento quando o encoder não emite os parâmetros da imagem.
///
/// É um fato sobre o encoder, não um palpite sobre a tela: sem SPS o fluxo é
/// indecodificável para qualquer um. Serve de gatilho para trocar de encoder
/// sozinho, em vez de a sala descobrir olhando para o preto.
pub const MOTIVO_ENCODER_MUDO: &str = "o encoder não emite os parâmetros da imagem";

/// Motivo de encerramento quando ninguém consegue decodificar o que sai daqui.
///
/// O sinal são os PLIs — os pedidos de quadro-chave que o navegador manda
/// quando não consegue montar imagem. Alguns são normais (alguém entrou,
/// houve perda); uma enxurrada contínua significa que o fluxo é inútil para
/// quem assiste, por mais que tudo do nosso lado pareça saudável.
pub const MOTIVO_INDECIFRAVEL: &str = "ninguém está conseguindo decodificar o vídeo";

/// PLIs por segundo que denunciam um fluxo indecifrável.
///
/// Um stream saudável recebe um punhado no início e quase nada depois. Numa
/// sessão quebrada de verdade foram 486. Um por segundo sustentado está bem
/// acima do normal e bem abaixo do patológico.
const PLIS_POR_SEGUNDO_RUIM: u64 = 1;

/// Segundos observando antes de julgar. Dá tempo de todo mundo entrar e de o
/// primeiro quadro-chave circular.
const SEGUNDOS_ANTES_DE_JULGAR_PLI: u64 = 15;

/// Segundos no ar antes de desistir de um encoder que nunca mandou SPS.
///
/// Generoso de propósito: o primeiro quadro-chave leva um instante, e derrubar
/// uma transmissão boa por pressa seria pior que o defeito.
const SEGUNDOS_PARA_DESISTIR: u64 = 10;

/// Motivo de encerramento quando a janela capturada deixou de existir.
///
/// Constante e não texto solto porque quem decide seguir o programa para a
/// janela seguinte precisa distinguir este caso de "o ffmpeg morreu": o
/// primeiro é normal num jogo que troca de processo, o segundo é defeito.
pub const MOTIVO_FONTE_SUMIU: &str = "a fonte deixou de existir (janela fechada?)";

/// Ajustes de qualidade pedidos pela interface.
#[derive(Debug, Clone, Copy)]
pub struct Qualidade {
    /// Altura alvo. `0` = mandar no tamanho nativo.
    pub altura: u32,
    pub fps: u32,
    pub kbps: u32,
}

/// Taxa por segundo entre duas leituras de um contador acumulado.
///
/// `saturating_sub` porque o contador reinicia quando a transmissão recomeça,
/// e uma subtração ao contrário viraria um número gigante em vez de zero.
#[must_use]
fn taxa_por_segundo(agora: u64, antes: u64, dt: f64) -> i32 {
    if dt <= 0.0 {
        return 0;
    }
    (agora.saturating_sub(antes) as f64 / dt).round() as i32
}

/// Uma leitura dos contadores, guardada para derivar a próxima por diferença.
///
/// Quase tudo que interessa é taxa, não total: 8000 kbps, 60 fps, 3 quadros
/// perdidos NESTE intervalo. Todos os contadores de origem são acumulados,
/// então cada medida precisa da anterior.
struct Marca {
    quando: Instant,
    stats: whip::Estatisticas,
    /// Quadros que o ffmpeg já codificou, acumulado.
    quadros: u64,
    /// Quadros que o Windows já entregou, acumulado.
    entregues: u64,
    /// Quadros que já convertemos, acumulado.
    convertidos: u64,
    /// Quadros já descartados por nós, acumulado.
    descartados: u64,
    /// Retransmissões já pedidas pelo servidor, acumulado.
    nacks: u64,
    /// Voltas do relógio já perdidas por atraso, acumulado.
    atrasadas: u64,
}

/// O que a interface mostra na tela "em transmissão".
#[derive(Debug, Clone, Default, Serialize)]
pub struct Telemetria {
    pub kbps: i32,
    /// Quadros por segundo NO INTERVALO medido.
    ///
    /// Derivado do contador acumulado do ffmpeg, não do `fps=` dele. Aquele é
    /// a média desde o início da transmissão, e média longa não mostra
    /// travada: depois de vinte minutos no ar, dois segundos ruins não movem
    /// o número. Era o defeito que deixava "está congelando" sem resposta.
    pub fps: i32,
    /// A média desde o início, que era o `fps` antigo. Continua útil como
    /// linha de base para comparar com a do intervalo.
    pub fps_medio: i32,
    /// Quanto a volta do laço passou dos 2s previstos, em ms.
    ///
    /// Mede a máquina parando por dentro. Todo o resto é dividido pelo tempo
    /// decorrido, e a divisão esconde exatamente a demora que interessa.
    pub atraso_ms: i64,
    /// Quadros descartados NO INTERVALO. `dropped` é o total acumulado.
    pub descartados_intervalo: i32,
    /// Quadros que o Windows nos entregou no intervalo.
    ///
    /// Junto com `convertidos` diz de quem é a culpa quando falta imagem: os
    /// dois baixos é o compositor não entregando (jogo em tela cheia, janela
    /// minimizada); entregues alto e convertidos baixo somos nós não dando
    /// conta da conversão.
    pub entregues: i32,
    /// Quadros que conseguimos converter no intervalo.
    pub convertidos: i32,
    pub dropped: i32,
    /// Estado do `RTCPeerConnection`, em texto.
    pub estado: String,
    /// Perda no caminho até o servidor, em partes por mil (10 = 1%).
    ///
    /// Separada de `dropped` de propósito: `dropped` é o que NÓS jogamos fora e
    /// dá para consertar mexendo no app; esta é o que não chegou do outro lado,
    /// e o conserto é baixar a qualidade.
    pub perda_por_mil: u64,
    /// Variação de chegada dos pacotes, em ms. Explica travada com perda zero.
    pub jitter_ms: u64,
    /// Última mudança automática de qualidade, em texto, ou vazio.
    ///
    /// Fica na telemetria em vez de virar evento próprio porque precisa
    /// PERSISTIR: quem olha a tela dois minutos depois da queda continua
    /// merecendo saber por que a imagem piorou. Um evento avisaria uma vez e
    /// sumiria, e a pessoa ficaria com a impressão de que o app estragou algo
    /// sozinho — que foi exatamente a confusão que isto existe para evitar.
    pub ultima_mudanca: String,

    /// Pacotes que o servidor pediu de volta NO INTERVALO.
    ///
    /// Separa "a perda foi recuperada" de "a perda virou buraco": os dois
    /// aparecem igual em `perda_por_mil`, e pedem conserto em lugares
    /// diferentes. Retransmissão acontecendo significa que o caminho para
    /// melhorar não é baixar a taxa.
    pub nacks: i32,
    /// Voltas do relógio de conversão perdidas NO INTERVALO.
    ///
    /// Separado de `descartados` porque a causa é outra e o conserto também: um
    /// quadro descartado existiu e foi jogado fora; uma volta perdida é um
    /// quadro que nunca chegou a ser produzido, porque o relógio não teve volta
    /// para produzi-lo. Sem esta coluna, uma cadência 5% lenta não aparecia em
    /// lugar nenhum — a rede limpa, a CPU em 10%, `descartados` em zero — e o
    /// diagnóstico virou uma sequência de hipóteses.
    pub atrasadas: i32,
    /// PID do ffmpeg, para quem quiser medir o custo dele.
    ///
    /// O encoder roda em processo separado e era invisível na telemetria:
    /// alguém dizia "travou o PC" e o número mostrado era só o nosso.
    pub pid_encoder: u32,
    /// Vezes que o encoder mandou os parâmetros da imagem por conta própria.
    pub parametros_vistos: u64,
    /// Vezes que precisamos reinseri-los no lugar dele.
    pub parametros_reinseridos: u64,
    /// Segundos no ar.
    pub segundos: u64,
    pub largura: u32,
    pub altura: u32,
    pub encoder: String,
    /// `true` quando o áudio por processo não estava disponível e caímos para
    /// o sistema inteiro. A pessoa PRECISA saber que está vazando o resto.
    pub audio_do_sistema: bool,
}

/// Resolução de saída a partir da capturada e da altura alvo.
///
/// Só reduz, nunca amplia: esticar 720p para 1080p gasta banda e não devolve
/// detalhe nenhum. Sai sempre em dimensões pares porque o H.264 em yuv420p
/// exige isso.
pub fn escala_para(largura: u32, altura: u32, alvo: u32) -> Option<(u32, u32)> {
    if alvo == 0 || altura == 0 || alvo >= altura {
        return None;
    }
    let nova_l = ((u64::from(largura) * u64::from(alvo)) / u64::from(altura)) as u32;
    let (l, a) = (nova_l & !1, alvo & !1);
    if l == 0 || a == 0 || (l == largura && a == altura) {
        return None;
    }
    Some((l, a))
}

/// Uma transmissão em andamento.
pub struct Transmissao {
    parar: Arc<AtomicBool>,
    supervisor: Option<std::thread::JoinHandle<()>>,
    /// Última leitura publicada. A UI lê daqui via evento; a sala lê daqui para
    /// mandar `publish_stats`.
    pub telemetria: Arc<UltimaLeitura>,
}

/// Última leitura de telemetria, compartilhada entre a thread supervisora (que
/// escreve) e quem perguntar (que lê).
///
/// É o único estado mutável compartilhado desta transmissão. Um `Mutex` em vez
/// de canal porque ninguém quer o histórico: só interessa o valor de agora, e
/// um leitor lento não pode segurar o supervisor.
#[derive(Default)]
pub struct UltimaLeitura {
    atual: std::sync::Mutex<Telemetria>,
}

impl UltimaLeitura {
    pub fn grava(&self, t: Telemetria) {
        if let Ok(mut slot) = self.atual.lock() {
            *slot = t;
        }
    }

    pub fn le(&self) -> Telemetria {
        self.atual.lock().map(|t| t.clone()).unwrap_or_default()
    }
}

/// O que `prepara` devolve antes de a transmissão de fato começar: a
/// resolução, que a sala precisa saber no `publish_start`.
pub struct Preparada {
    buf: Arc<video::FrameBuffer>,
    captura: video::Captura,
    /// Controles do encoder em processo. `None` no caminho do `ffmpeg.exe`.
    cod: Option<Arc<video::Codificacao>>,
    /// Por onde os quadros codificados chegam, no caminho em processo.
    quadros_codificados: Option<std::sync::mpsc::Receiver<video::Codificado>>,
    /// O primeiro quadro codificado, já consumido do canal para provar que o
    /// encoder desta máquina funciona. A ponte o reenvia antes dos demais.
    primeiro_codificado: Option<video::Codificado>,
    pub largura: u32,
    pub altura: u32,
    /// Resolução efetivamente enviada, já com a escala aplicada.
    pub largura_saida: u32,
    /// O primeiro quadro do monitor veio inteiramente preto. A interface avisa.
    pub monitor_preto: bool,
    pub altura_saida: u32,
    fonte_audio: audio::AudioSource,
    audio_do_sistema: bool,
    /// Como reconhecer o programa se ele trocar de janela. `None` para
    /// monitor, que não troca de nada.
    assinatura: Option<foco::Assinatura>,
}

/// Primeira metade: abre pipes e captura, e descobre a resolução.
///
/// Separada de [`inicia`] porque o `publish_start` da sala exige `w`/`h`
/// reais, e a resolução só é conhecida quando o primeiro frame chega — o WGC é
/// quem dita o tamanho.
/// Um passo da agenda de teste: ou uma taxa, ou um degrau da escada.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PassoTeste {
    Taxa(u32),
    Degrau(usize),
}

/// Mudanças agendadas por variável de ambiente, para exercitar sem depender
/// de a rede piorar.
///
/// `TELA_TAXA_TESTE="20:6000,50:720p30,90:nativa"` troca aos 20 s para 6000
/// kbps, aos 50 para o degrau de 720p30 e aos 90 para o nativo.
///
/// Um número puro é taxa em kbps; qualquer outra coisa é procurada como nome
/// de degrau. Os nomes saem da própria `ESCADA`, então não há tabela para
/// desatualizar.
///
/// Existe porque o gatilho normal é perda de pacote, e numa conexão boa ela
/// simplesmente não acontece. Trocar de DEGRAU importa mais que trocar de
/// taxa: mexe no SPS, e é o pedaço da costura RTP que mais precisa ser visto
/// funcionando antes de chegar em quem está jogando.
///
/// Entrada inválida é ignorada em silêncio. É ferramenta de diagnóstico ligada
/// à mão; recusar a transmissão por causa de uma vírgula seria pior.
fn agenda_de_teste(valor: &str) -> Vec<(u64, PassoTeste)> {
    let mut v: Vec<(u64, PassoTeste)> = valor
        .split(',')
        .filter_map(|passo| {
            let (s, alvo) = passo.trim().split_once(':')?;
            let segundos = s.trim().parse().ok()?;
            let alvo = alvo.trim();
            let passo = match alvo.parse::<u32>() {
                Ok(kbps) if kbps > 0 => PassoTeste::Taxa(kbps),
                Ok(_) => return None,
                Err(_) => PassoTeste::Degrau(degrau_por_nome(alvo)?),
            };
            Some((segundos, passo))
        })
        .collect();
    v.sort_by_key(|(s, _)| *s);
    v
}

/// Índice do degrau com este nome: `720p30`, `1080p60`, `nativa`.
fn degrau_por_nome(nome: &str) -> Option<usize> {
    let nome = nome.to_lowercase();
    crate::escada::ESCADA.iter().position(|d| {
        let esperado = if d.altura == 0 {
            "nativa".to_string()
        } else {
            format!("{}p{}", d.altura, d.fps)
        };
        esperado == nome
    })
}

#[cfg(test)]
mod testes_agenda {
    use super::{agenda_de_teste, PassoTeste};

    #[test]
    fn le_taxas() {
        assert_eq!(
            agenda_de_teste("30:6000,60:3000"),
            vec![(30, PassoTeste::Taxa(6000)), (60, PassoTeste::Taxa(3000))]
        );
    }

    /// O caso que importa: forçar troca de RESOLUÇÃO, que é o que mexe no SPS.
    #[test]
    fn le_degraus_pelo_nome() {
        assert_eq!(
            agenda_de_teste("10:720p30,20:1080p60,30:nativa"),
            vec![
                (10, PassoTeste::Degrau(0)),
                (20, PassoTeste::Degrau(2)),
                (30, PassoTeste::Degrau(3)),
            ]
        );
    }

    #[test]
    fn mistura_os_dois_e_ordena() {
        assert_eq!(
            agenda_de_teste(" 60 : 720p60 , 30 : 5000 "),
            vec![(30, PassoTeste::Taxa(5000)), (60, PassoTeste::Degrau(1))]
        );
    }

    /// Lixo não pode derrubar a transmissão.
    #[test]
    fn ignora_lixo_sem_reclamar() {
        assert!(agenda_de_teste("").is_empty());
        assert!(agenda_de_teste("30").is_empty());
        assert!(agenda_de_teste("30:0").is_empty(), "taxa zero não serve");
        assert!(agenda_de_teste("30:4k").is_empty(), "degrau inexistente");
        assert_eq!(
            agenda_de_teste("x:y,30:6000"),
            vec![(30, PassoTeste::Taxa(6000))]
        );
    }

    /// Os nomes têm de casar com a escada de verdade — se alguém mexer nela,
    /// este teste avisa antes de a ferramenta parar de funcionar em silêncio.
    #[test]
    fn todo_degrau_da_escada_tem_nome_valido() {
        for (i, d) in crate::escada::ESCADA.iter().enumerate() {
            let nome = if d.altura == 0 {
                "nativa".to_string()
            } else {
                format!("{}p{}", d.altura, d.fps)
            };
            assert_eq!(
                agenda_de_teste(&format!("1:{nome}")),
                vec![(1, PassoTeste::Degrau(i))],
                "degrau {i} ({nome}) não é alcançável pela agenda"
            );
        }
    }
}

/// Converte um degrau da escada no alvo concreto para ESTA fonte.
///
/// A escada fala em altura (`720`, `1080`, `0` para nativa); o encoder precisa
/// da escala já resolvida contra a resolução capturada. E `escala_para` só
/// reduz: pedir 1080p de uma fonte de 720p devolve `None`, que é mandar
/// nativo — esticar gastaria banda sem devolver detalhe.
fn alvo_do_degrau(d: crate::escada::Degrau, kbps: u32, nativa: (u32, u32)) -> Alvo {
    Alvo {
        kbps,
        escala: escala_para(nativa.0, nativa.1, d.altura),
        fps: d.fps,
    }
}

/// Um alvo em texto, para o log. `1080p60 a 8000 kbps`.
fn descreve(a: &Alvo) -> String {
    let altura = a
        .escala
        .map_or_else(|| "nativo".to_string(), |(_, h)| format!("{h}p"));
    format!("{altura}{} a {} kbps", a.fps, a.kbps)
}

/// O que muda de um encoder para o próximo.
///
/// Bitrate, escala e fps juntos porque é assim que a escada de qualidade
/// funciona: um degrau é a combinação dos três, não três ajustes soltos.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Alvo {
    pub kbps: u32,
    /// `None` = manda no tamanho nativo, sem reescalar.
    pub escala: Option<(u32, u32)>,
    pub fps: u32,
}

/// O que NÃO muda entre reinícios do encoder.
///
/// Fica com os valores em posse própria — e não emprestados — porque quem
/// reinicia é a thread do supervisor, que sobrevive à função que a criou.
struct Receita {
    ffmpeg: String,
    encoder: String,
    encoder_args: Vec<String>,
    buf: Arc<video::FrameBuffer>,
    fonte_audio: audio::AudioSource,
    /// Resolução capturada. O pipe carrega o quadro como veio e é o ffmpeg
    /// que reescala, então esta não muda quando o degrau muda.
    nativa: (u32, u32),
    porta_video: u16,
    porta_audio: u16,
    /// O vídeo é encodado em processo; o ffmpeg cuida só do áudio.
    apenas_audio: bool,
}

/// Um encoder vivo: o processo do ffmpeg e os dois pumps que o alimentam.
///
/// Existe como unidade separada porque trocar a taxa exige matar e recriar
/// exatamente estas peças — o ffmpeg não aceita mudar bitrate em tempo de
/// execução, nenhum comando dele chega ao encoder. O que fica de FORA daqui é
/// o que sobrevive à troca: a captura, a conexão WebRTC e a stream na sala.
/// É essa divisão que faz o reinício custar meio segundo de imagem congelada
/// em vez de derrubar todo mundo.
struct Encoder {
    child: std::process::Child,
    /// Para APENAS os pumps deste encoder.
    ///
    /// Separado do `parar` da transmissão de propósito: a transmissão continua
    /// viva quando o encoder é trocado, e usar a mesma bandeira encerraria as
    /// duas coisas juntas.
    parar_pumps: Arc<AtomicBool>,
    /// `None` quando o vídeo é encodado em processo: não há pipe a alimentar.
    video: Option<std::thread::JoinHandle<Result<u64>>>,
    audio: std::thread::JoinHandle<Result<u64>>,
    pid: u32,
    /// Contador acumulado de quadros codificados, alimentado pelo `-progress`.
    quadros: Arc<AtomicU64>,
    fps_medio: Arc<AtomicU64>,
    descartes: Arc<AtomicU64>,
}

impl Encoder {
    /// Mata o processo e espera os pumps saírem.
    ///
    /// A ordem importa: matar o ffmpeg primeiro faz os pumps tomarem erro de
    /// pipe quebrado e sairem sozinhos. Na ordem inversa, o ffmpeg ficaria
    /// bloqueado lendo um pipe que ninguém mais alimenta.
    fn encerra(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.parar_pumps.store(true, Ordering::Relaxed);
        if let Some(v) = self.video {
            let _ = v.join();
        }
        let _ = self.audio.join();
    }
}

/// Sobe um encoder novo: pipes, processo e pumps.
///
/// Cada chamada cria pipes com geração própria. Reaproveitar o nome seria
/// corrida com o pipe anterior fechando — ver `pipes::PipeServer::create`.
fn sobe_encoder(r: &Receita, alvo: Alvo) -> Result<Encoder> {
    let g = proxima_geracao();
    // Sem vídeo não há pipe de vídeo. Criá-lo à toa deixaria um servidor de
    // named pipe esperando uma conexão que nunca vem.
    let vpipe = (!r.apenas_audio)
        .then(|| pipes::PipeServer::create("v", g))
        .transpose()?;
    let apipe = pipes::PipeServer::create("a", g)?;
    let vpath = vpipe.as_ref().map_or_else(String::new, |p| p.path.clone());
    let apath = apipe.path.clone();

    let mut child = ffmpeg::spawn(
        &r.ffmpeg,
        &ffmpeg::Params {
            encoder_args: &r.encoder_args,
            video_pipe: &vpath,
            audio_pipe: &apath,
            width: r.nativa.0,
            height: r.nativa.1,
            fps: alvo.fps,
            kbps: alvo.kbps,
            encoder: &r.encoder,
            escala: alvo.escala,
            destino: ffmpeg::Destino::Rtp {
                porta_video: r.porta_video,
                porta_audio: r.porta_audio,
            },
            apenas_audio: r.apenas_audio,
        },
    )?;
    let pid = child.id();

    let quadros = Arc::new(AtomicU64::new(0));
    let fps_medio = Arc::new(AtomicU64::new(0));
    let descartes = Arc::new(AtomicU64::new(0));

    if let Some(stderr) = child.stderr.take() {
        ffmpeg::drain_stderr(stderr, |l| crate::registro::linha(&format!("ffmpeg: {l}")));
    }
    if let Some(stdout) = child.stdout.take() {
        let (q, f, d) = (
            Arc::clone(&quadros),
            Arc::clone(&fps_medio),
            Arc::clone(&descartes),
        );
        ffmpeg::drain_progress(stdout, move |linha| {
            if let Some(v) = linha.strip_prefix("frame=") {
                if let Ok(n) = v.trim().parse::<u64>() {
                    q.store(n, Ordering::Relaxed);
                }
            } else if let Some(v) = linha.strip_prefix("fps=") {
                if let Ok(x) = v.trim().parse::<f64>() {
                    f.store(x.round().max(0.0) as u64, Ordering::Relaxed);
                }
            } else if let Some(v) = linha.strip_prefix("drop_frames=") {
                if let Ok(n) = v.trim().parse::<u64>() {
                    d.store(n, Ordering::Relaxed);
                }
            }
        });
    }

    let parar_pumps = Arc::new(AtomicBool::new(false));
    let (vstop, astop) = (Arc::clone(&parar_pumps), Arc::clone(&parar_pumps));
    let vbuf = Arc::clone(&r.buf);
    let nativa = r.nativa;
    let fps = alvo.fps;
    let fonte_audio = r.fonte_audio;

    // A ordem aqui não é estética. O ffmpeg abre as entradas em sequência e faz
    // o probe da primeira antes de abrir a segunda: se ninguém estiver
    // escrevendo no pipe de vídeo, ele fica preso lendo e nunca abre o de
    // áudio — e o `accept` do áudio esperaria para sempre. Por isso o pump de
    // vídeo sobe ENTRE os dois accepts.
    let video = match vpipe {
        Some(vpipe) => {
            let vwriter = vpipe.accept()?;
            Some(std::thread::spawn(move || {
                // Resolução NATIVA, não a de saída: o pipe carrega o frame
                // como capturado e é o ffmpeg que aplica a escala com
                // `-vf scale`. Passar a de saída aqui reintroduziria a
                // dessincronia que isto veio corrigir.
                video::pump(&vbuf, fps, Some(nativa), vwriter, &vstop)
            }))
        }
        None => {
            // O caminho em processo não usa nenhum dos dois; evita o aviso de
            // variável não usada sem espalhar `cfg` pela função.
            let _ = (vbuf, nativa, fps, vstop);
            None
        }
    };
    let awriter = apipe.accept()?;
    let audio = std::thread::spawn(move || audio::pump(fonte_audio, awriter, &astop));

    Ok(Encoder {
        child,
        parar_pumps,
        video,
        audio,
        pid,
        quadros,
        fps_medio,
        descartes,
    })
}

/// Escreve um alvo nos controles do encoder em processo.
///
/// Frouxo de propósito: a thread de callback lê no quadro seguinte. Uma
/// mudança que só vale daqui a 16 ms não faz diferença nenhuma, e evita travar
/// a captura num mutex.
fn aplica_alvo(c: &video::Codificacao, alvo: Alvo) {
    c.kbps.store(alvo.kbps, Ordering::Relaxed);
    c.fps.store(alvo.fps.max(1), Ordering::Relaxed);
    // `escala` ausente quer dizer nativo, que é o zero que o handler espera.
    c.altura_alvo
        .store(alvo.escala.map_or(0, |(_, a)| a), Ordering::Relaxed);
}

/// Taxa do áudio, em kbps.
///
/// O mesmo 128 que sempre pedimos ao ffmpeg. É o valor em que o Opus já é
/// transparente para voz e música, e o vídeo custa cinquenta vezes mais — não
/// há o que economizar aqui.
const KBPS_AUDIO: u32 = 128;

/// Número de série dos pipes desta máquina, nesta execução.
///
/// Trocar a taxa reinicia o ffmpeg dentro da mesma transmissão, e o pipe novo
/// nasce enquanto o antigo ainda pode estar fechando. Um contador que só sobe
/// garante que os dois nunca disputem o mesmo nome.
fn proxima_geracao() -> u64 {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

pub fn prepara(
    fonte: &Fonte,
    qualidade: Qualidade,
    audio_por_processo: bool,
    // `na_gpu`: entregar a textura ao encoder em processo em vez de trazer
    // cada quadro para a CPU. Quem decide é [`crate::main`], a partir do
    // config e do que a sonda encontrou.
    na_gpu: bool,
) -> Result<Preparada> {
    let buf = video::FrameBuffer::shared();

    // A fila é curta de propósito. Ela existe só para desacoplar a thread de
    // callback do WGC da que empacota RTP; se encher, o certo é descartar
    // quadro e seguir, não acumular atraso numa transmissão ao vivo.
    let (cod, quadros_codificados) = if na_gpu {
        let (envio, recebe) = std::sync::mpsc::sync_channel(16);
        // A `qualidade` que chega aqui é um envelope: altura zero e kbps zero,
        // porque quem decide isso é a política, e ela só nasce depois — precisa
        // da resolução da fonte, que só o primeiro quadro revela. Um encoder
        // não abre com bitrate zero, então começa no degrau padrão da escada e
        // `inicia` corrige assim que a política existir.
        let inicial = crate::escada::ESCADA[crate::escada::INICIO];
        let c = Arc::new(video::Codificacao {
            envio,
            kbps: std::sync::atomic::AtomicU32::new(if qualidade.kbps == 0 {
                inicial.max
            } else {
                qualidade.kbps
            }),
            fps: std::sync::atomic::AtomicU32::new(qualidade.fps.max(1)),
            altura_alvo: std::sync::atomic::AtomicU32::new(qualidade.altura),
            chave_pedida: AtomicBool::new(false),
            amostra: Arc::clone(&buf),
            retido: std::sync::Mutex::new(None),
            dispositivo: std::sync::OnceLock::new(),
            parar: AtomicBool::new(false),
        });
        // O relógio precisa existir ANTES da captura: é ele que encoda, e sem
        // ele o primeiro quadro ficaria retido para sempre.
        {
            let c = Arc::clone(&c);
            std::thread::spawn(move || video::relogio(&c));
        }
        (Some(c), Some(recebe))
    } else {
        (None, None)
    };

    // O fps pedido vai junto: sem ele a captura converte tudo que a tela
    // apresenta, e num monitor rápido isso é o dobro do trabalho necessário —
    // roubado de quem está jogando.
    let cfg_captura = video::Config {
        destino: match &cod {
            Some(c) => video::Destino::Gpu(Arc::clone(c)),
            None => video::Destino::Cpu(Arc::clone(&buf)),
        },
        fps: qualidade.fps,
    };
    let captura = match fonte {
        Fonte::Janela { hwnd, .. } => video::start_window(*hwnd, cfg_captura)?,
        Fonte::Monitor { indice, .. } => video::start_monitor(Some(*indice), cfg_captura)?,
    };

    if !buf.wait_first(ESPERA_PRIMEIRO_FRAME) {
        // A captura já subiu; largá-la aqui deixaria a sessão do WGC pendurada.
        let _ = captura.encerra();
        anyhow::bail!("nenhum frame em 5 s — o Windows Graphics Capture não iniciou");
    }

    let (largura, altura) = buf.dims();
    let primeiro = buf.latest().context("primeiro frame sumiu do buffer")?;

    // Captura de monitor que volta toda preta é falha silenciosa: o WGC entrega
    // quadros válidos, a transmissão sobe, o servidor recebe, e quem assiste vê
    // preto sem ninguém receber erro nenhum. Acontece em algumas máquinas e a
    // captura de JANELA continua funcionando nelas — foi exatamente o relato de
    // um usuário. Aqui pelo menos a pessoa fica sabendo.
    //
    // Só avisa, não impede: alguém pode estar compartilhando uma tela
    // legitimamente preta, e bloquear seria pior que o problema.
    let preto = matches!(fonte, Fonte::Monitor { .. })
        && video::parece_preto(&primeiro, largura as usize, altura as usize);
    if preto {
        crate::registro::linha(
            "AVISO: a captura do monitor voltou toda preta.              Costuma ser driver de video desatualizado ou incompatibilidade de              placa. Compartilhar uma JANELA costuma funcionar nesses casos.",
        );
    }
    // NV12 = w*h*1,5. Falhar aqui é muito mais barato que depurar um vídeo
    // distorcido depois de já estar no ar.
    if primeiro.len() != (largura as usize) * (altura as usize) * 3 / 2 {
        let _ = captura.encerra();
        anyhow::bail!(
            "frame de {} bytes não corresponde a {largura}x{altura} em NV12",
            primeiro.len()
        );
    }

    // A regra central do produto, em duas linhas: a fonte de VÍDEO decide o
    // escopo do ÁUDIO. Janela = só aquele aplicativo; monitor = som do sistema.
    // Afrouxar isto é vazar a conversa do Discord no meio do jogo.
    let (fonte_audio, audio_do_sistema) = match fonte {
        Fonte::Janela { pid, .. } if audio_por_processo => {
            (audio::AudioSource::Process(*pid), false)
        }
        // Windows anterior ao build 20348 não tem loopback por processo. Cair
        // para o sistema inteiro é melhor que não transmitir, mas a interface
        // avisa — silenciar isso seria vazar áudio sem a pessoa saber.
        Fonte::Janela { .. } => (audio::AudioSource::System, true),
        // Excluir exige a mesma API do loopback por processo; sem ela, resta o
        // sistema inteiro (e o eco).
        Fonte::Monitor {
            excluir_pid: Some(pid),
            ..
        } if audio_por_processo => (audio::AudioSource::SistemaExceto(*pid), true),
        Fonte::Monitor { .. } => (audio::AudioSource::System, true),
    };

    // No caminho em processo, a captura ter subido não prova nada: o encoder
    // pode não ter aberto, ou abrir e não produzir. Esperar o primeiro quadro
    // CODIFICADO é o que distingue "funciona nesta máquina" de "compila".
    //
    // Quem chamou com `na_gpu` em modo automático trata a falha caindo para o
    // `ffmpeg.exe`; quem fixou `gpu` no config recebe o erro, que é o que ele
    // pediu ao fixar.
    let primeiro_codificado = match &quadros_codificados {
        Some(fila) => match fila.recv_timeout(ESPERA_PRIMEIRO_CODIFICADO) {
            Ok(q) => Some(q),
            Err(_) => {
                let _ = captura.encerra();
                if let Some(c) = &cod {
                    c.parar.store(true, Ordering::Relaxed);
                }
                // A causa real, quando existe, vem de dentro do relógio; sem
                // ela sobraria "não veio quadro", que não diz o que fazer.
                match video::erro_do_encoder() {
                    Some(e) => anyhow::bail!("o encoder de hardware falhou: {e}"),
                    None => anyhow::bail!(
                        "o encoder de hardware não devolveu quadro em {} s",
                        ESPERA_PRIMEIRO_CODIFICADO.as_secs()
                    ),
                }
            }
        },
        None => None,
    };

    let escala = escala_para(largura, altura, qualidade.altura);
    let (largura_saida, altura_saida) = escala.unwrap_or((largura, altura));

    Ok(Preparada {
        buf,
        captura,
        cod,
        quadros_codificados,
        primeiro_codificado,
        largura,
        altura,
        largura_saida,
        monitor_preto: preto,
        altura_saida,
        fonte_audio,
        audio_do_sistema,
        // Tirada aqui, com a janela ainda viva: depois de ela sumir de vista
        // o caminho do executável pode já não ser consultável.
        assinatura: match fonte {
            Fonte::Janela { hwnd, pid, titulo } => Some(foco::assina(*hwnd, *pid, titulo)),
            Fonte::Monitor { .. } => None,
        },
    })
}

impl Preparada {
    /// Desfaz o preparo sem transmitir — usado quando a sala recusa o
    /// `publish_start` ou o usuário desiste no meio.
    pub fn descarta(self) {
        let _ = self.captura.encerra();
    }
}

/// Segunda metade: sobe WHIP e ffmpeg e devolve a transmissão viva.
///
/// A publicação WHIP sobe **antes** do ffmpeg de propósito: assim os sockets
/// UDP já estão ligados e o DTLS já fechou quando o primeiro pacote sai. Na
/// ordem inversa, os primeiros segundos — inclusive o keyframe inicial —
/// cairiam num socket que ninguém está lendo.
// Oito parâmetros porque três deles são retornos de chamada distintos —
// medida, escurecimento e fim — e juntá-los num objeto só esconderia quem
// avisa o quê sem tirar nada de complexidade.
#[allow(clippy::too_many_arguments)]
pub fn inicia(
    rt: &Arc<Runtime>,
    p: Preparada,
    politica: crate::escada::Politica,
    caminho_ffmpeg: &str,
    whip_url: &str,
    // `encoder_forcado`: fixado no `tela-config.json`; vazio = sonda escolhe.
    encoder_forcado: &str,
    // `encoder_args`: extras do `tela-config.json`, acrescentados ao tuning.
    encoder_args: &[String],
    mut ao_medir: impl FnMut(&Telemetria) + Send + 'static,
    // `ao_escurecer`: avisado quando a imagem apaga (`true`) e quando volta.
    mut ao_escurecer: impl FnMut(bool) + Send + 'static,
    ao_terminar: impl FnOnce(String) + Send + 'static,
) -> Result<Transmissao> {
    // Testa cada encoder de verdade antes de escolher; leva alguns décimos de
    // segundo. O nome escolhido vai para o log porque foi justamente isso que
    // faltou para diagnosticar uma máquina sem driver NVIDIA.
    // Fixado por arquivo tem precedência: é a saída para investigar um
    // encoder que só falha na máquina de outra pessoa, sem build nova.
    let encoder = if let Some(e) = ffmpeg::ENCODER_CASCADE
        .iter()
        .find(|c| **c == encoder_forcado)
    {
        crate::registro::linha(&format!("encoder fixado no config: {e}"));
        *e
    } else {
        match ffmpeg::sonda_encoders(caminho_ffmpeg) {
            Ok((escolhido, recusados)) => {
                // As recusas vão para o log MESMO quando algum encoder passa.
                // Cair em `libx264` numa máquina com GPU capaz custa quadros do
                // jogo de quem está compartilhando, e sem esta linha não havia
                // como saber que tinha acontecido, muito menos por quê.
                if !recusados.is_empty() {
                    crate::registro::linha(&format!(
                        "encoders recusados: {}",
                        recusados.join("; ")
                    ));
                }
                match escolhido {
                    Some(e) => {
                        crate::registro::linha(&format!("encoder escolhido: {e}"));
                        if e == "libx264" {
                            crate::registro::linha(
                                "ATENCAO: codificando por software — a GPU nao foi usada",
                            );
                        }
                        e
                    }
                    None => {
                        let erro = anyhow::anyhow!(
                            "nenhum encoder H.264 utilizável nesta máquina — {}",
                            recusados.join("; ")
                        );
                        crate::registro::linha(&format!("{erro:#}"));
                        return Err(erro);
                    }
                }
            }
            Err(e) => {
                crate::registro::linha(&format!("nao consegui sondar encoders: {e:#}"));
                return Err(e);
            }
        }
    };

    let na_gpu = p.cod.is_some();
    let (porta_video, porta_audio) = whip::portas_livres()?;
    let mut publicacao = rt
        .block_on(whip::publica(whip::Config {
            whip_url,
            porta_video,
            porta_audio,
            kbps_video: politica.kbps(),
            em_processo: na_gpu,
        }))
        .context("publicar via WHIP")?;

    // Ponte entre o encoder e o transporte: uma thread que tira os quadros da
    // fila e os empacota em RTP.
    //
    // Thread própria, e não a de callback do WGC, porque empacotar um keyframe
    // de 1080p são ~100 pacotes; fazer isso dentro do callback seguraria a
    // entrega do quadro seguinte e roubaria tempo de quem está jogando.
    let ponte = match (p.quadros_codificados, publicacao.toma_emissor()) {
        (Some(recebe), Some(mut emissor)) => {
            let primeiro = p.primeiro_codificado;
            let cod_ponte = p.cod.clone();
            Some(std::thread::spawn(move || {
                // O quadro que `prepara` tirou da fila para provar que o
                // encoder funciona entra aqui, na frente. Sem isto a
                // transmissão começaria sem o primeiro quadro-chave e quem
                // estivesse na sala veria preto até o GOP seguinte.
                if let Some(q) = primeiro {
                    emissor.envia(&q.bytes, q.instante, q.chave);
                }
                // Sai pela bandeira, e NÃO esperando o canal fechar. O
                // `SyncSender` mora dentro do `Codificacao`, que o supervisor
                // também segura — esperar por ele seria esperar por quem está
                // esperando por nós. Foi exatamente esse impasse que deixou o
                // app pendurado no primeiro teste ao vivo, sem encerrar e sem
                // dizer por quê.
                loop {
                    if cod_ponte
                        .as_ref()
                        .is_some_and(|c| c.parar.load(Ordering::Relaxed))
                    {
                        break;
                    }
                    match recebe.recv_timeout(Duration::from_millis(200)) {
                        Ok(q) => emissor.envia(&q.bytes, q.instante, q.chave),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            }))
        }
        _ => None,
    };

    // Áudio em processo: o `pump` do WASAPI escreve num sumidouro que encoda
    // Opus e entrega o RTP direto. Sem pipe, sem processo, sem salto por UDP.
    //
    // Thread própria porque o `pump` é um laço bloqueante que dita o ritmo do
    // áudio pelo relógio — é ele que injeta silêncio quando o aplicativo está
    // mudo, e isso não pode parar.
    let parar_audio = Arc::new(AtomicBool::new(false));
    let audio_processo = match publicacao.toma_emissor_audio() {
        Some(mut emissor) => {
            let fonte = p.fonte_audio;
            let parar = Arc::clone(&parar_audio);
            Some(std::thread::spawn(move || {
                let sumidouro =
                    tela_capture::opus::Sumidouro::novo(KBPS_AUDIO, move |bytes, amostras| {
                        emissor.envia(bytes, amostras);
                        Ok(())
                    });
                match sumidouro {
                    Ok(s) => {
                        if let Err(e) = audio::pump(fonte, s, &parar) {
                            crate::registro::linha(&format!("áudio parou: {e:#}"));
                        }
                    }
                    Err(e) => crate::registro::linha(&format!("áudio não abriu: {e:#}")),
                }
            }))
        }
        None => None,
    };

    // Tudo que o encoder precisa e que NÃO muda quando a taxa muda. Fica em
    // posse da thread do supervisor, que é quem reinicia.
    let receita = Receita {
        ffmpeg: caminho_ffmpeg.to_string(),
        encoder: encoder.to_string(),
        encoder_args: encoder_args.to_vec(),
        buf: Arc::clone(&p.buf),
        fonte_audio: p.fonte_audio,
        nativa: (p.largura, p.altura),
        porta_video,
        porta_audio,
        apenas_audio: na_gpu,
    };
    // A política decide resolução, fps e taxa. Manual trava a resolução e
    // deixa a taxa flutuar; automático anda a escada inteira.
    let mut qualidade = politica;

    let mut agenda = std::env::var("TELA_TAXA_TESTE")
        .map(|v| agenda_de_teste(&v))
        .unwrap_or_default();
    if !agenda.is_empty() {
        crate::registro::linha(&format!("agenda de teste de taxa: {agenda:?}"));
    }
    let mut alvo = alvo_do_degrau(qualidade.degrau(), qualidade.kbps(), (p.largura, p.altura));
    // O encoder em processo já está rodando desde `prepara`, com o degrau
    // padrão — a política não existia lá. Alinhá-lo AGORA, e não só quando a
    // escada mudar de degrau: sem isto, um preset "Leve" transmitiria no
    // degrau padrão até a rede piorar, que pode ser a transmissão inteira.
    if let Some(c) = &p.cod {
        aplica_alvo(c, alvo);
    }
    // `Option` porque a troca de taxa tira o encoder e põe outro no lugar: o
    // antigo precisa morrer ANTES de o novo nascer, já que os dois escreveriam
    // nas mesmas portas UDP.
    // Em processo não sobe processo nenhum: o vídeo vem do relógio da captura e
    // o áudio do `opus::Sumidouro`, ambos alimentando o RTP direto. O
    // `ffmpeg.exe` só existe no caminho de recuo.
    let mut enc = if na_gpu {
        None
    } else {
        Some(sobe_encoder(&receita, alvo)?)
    };

    let parar = Arc::new(AtomicBool::new(false));

    let telemetria = Arc::new(UltimaLeitura::default());
    let sink_telemetria = Arc::clone(&telemetria);
    let rt_super = Arc::clone(rt);
    let parar_super = Arc::clone(&parar);
    let captura = p.captura;
    let nativa = (p.largura, p.altura);
    let audio_do_sistema = p.audio_do_sistema;
    let assinatura = p.assinatura;
    let buf_vigia = Arc::clone(&p.buf);
    let cod_super = p.cod.clone();

    let supervisor = std::thread::spawn(move || {
        let inicio = Instant::now();
        let mut anterior: Option<Marca> = None;
        let mut motivo = String::new();
        let mut ultima_mudanca = String::new();
        // Quantas rodadas seguidas a janela ficou fora de vista.
        let mut sumida: u32 = 0;
        // Rodadas seguidas com a imagem preta, e se o aviso já foi dado.
        let mut escuro: u32 = 0;
        let mut avisou_escuro = false;
        let mut quadro_amostra: Vec<u8> = Vec::new();
        // Diagnóstico da tela preta: sem uma máquina que reproduza o caso, o
        // jeito de descobrir a causa é o app registrar os fatos que separam as
        // hipóteses — o Windows parou de entregar quadro, a resolução mudou,
        // ou o quadro chega mas vem preto.
        let mut dims_antes = buf_vigia.dims();
        let (mut entregues_antes, _) = video::contadores();
        let mut parado: u32 = 0;
        let mut avisou_parado = false;
        let mut avisou_tempo = false;

        loop {
            if parar_super.load(Ordering::Relaxed) {
                break;
            }

            // PLI: alguém entrou na sala, ou perdeu o fio da meada e precisa de
            // um quadro-chave para recomeçar. O caminho antigo só contava o
            // pedido — o ffmpeg não aceita esse comando em tempo de execução —
            // e a pessoa esperava o GOP seguinte, até dois segundos de nada.
            if let Some(c) = &cod_super {
                if publicacao.pediram_chave() {
                    c.chave_pedida.store(true, Ordering::Relaxed);
                }
            }

            // Sem processo não há o que vigiar aqui — em processo, quem morre
            // avisa pelo `erro_do_encoder`, conferido logo abaixo.
            if let Some(atual_enc) = enc.as_mut() {
                match atual_enc.child.try_wait() {
                    Ok(Some(status)) => {
                        motivo = format!("o ffmpeg encerrou ({status})");
                        break;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        motivo = format!("perdi o ffmpeg de vista: {e}");
                        break;
                    }
                }
            }

            // Carimbo fora de ordem faz o servidor derrubar quem assiste, e
            // do nosso lado nada acusa. A trava do emissor corrige, mas o
            // aviso precisa aparecer: se subir, alguma coisa a montante voltou
            // a entregar quadro com tempo mais velho que o anterior.
            if !avisou_tempo {
                let e = rt_super.block_on(publicacao.estatisticas());
                if e.tempo_torto > 0 {
                    avisou_tempo = true;
                    crate::registro::linha(&format!(
                        "ATENCAO: {} carimbo(s) de tempo fora de ordem corrigidos",
                        e.tempo_torto
                    ));
                }
            }

            // No caminho em processo é aqui que uma falha do encoder aparece:
            // o relógio guarda a mensagem e sai, e sem esta conferência a
            // transmissão seguiria "no ar" sem mandar quadro nenhum.
            if cod_super.is_some() {
                if let Some(e) = video::erro_do_encoder() {
                    motivo = format!("o encoder em processo parou: {e}");
                    break;
                }
            }

            // A janela pode ter sido fechada no meio da transmissão.
            if captura.terminou() {
                // Texto reconhecido por quem decide seguir a janela para
                // outra: ver `MOTIVO_FONTE_SUMIU`.
                motivo = MOTIVO_FONTE_SUMIU.to_string();
                break;
            }

            // O encoder nunca mandou os parâmetros da imagem?
            //
            // Depois de alguns segundos no ar isso deixa de ser "ainda não" e
            // passa a ser defeito: ninguém consegue decodificar o que sai
            // daqui. Foi exatamente o caso do `h264_amf`, e a sala levou horas
            // para descobrir olhando para uma tela preta.
            let segundos = inicio.elapsed().as_secs();
            if segundos >= SEGUNDOS_PARA_DESISTIR {
                let e = rt_super.block_on(publicacao.estatisticas());
                if e.parametros_vistos == 0 {
                    motivo = MOTIVO_ENCODER_MUDO.to_string();
                    break;
                }
                // Os parâmetros saem, mas ninguém monta imagem: o navegador
                // pede quadro-chave sem parar. Só o outro lado sabe disso —
                // daqui tudo parece saudável, com bitrate no alvo e perda zero.
                if segundos >= SEGUNDOS_ANTES_DE_JULGAR_PLI
                    && e.plis >= segundos * PLIS_POR_SEGUNDO_RUIM
                {
                    crate::registro::linha(&format!(
                        "{} pedidos de quadro-chave em {}s — o fluxo não serve",
                        e.plis, segundos
                    ));
                    motivo = MOTIVO_INDECIFRAVEL.to_string();
                    break;
                }
            }

            // A imagem pode ter apagado no meio da transmissão.
            //
            // Hoje isto só era checado no PRIMEIRO quadro. Quando um jogo
            // assume a tela em modo exclusivo, ele sai do compositor do
            // Windows e a captura passa a entregar preto — e ninguém percebia:
            // quem transmite está dentro da partida, e a sala fica olhando
            // para o nada até alguém falar no chat. Aconteceu, e o único
            // rastro foi o bitrate caindo para 53 kbps num log.
            // A resolução da fonte mudou? Um jogo que troca o modo de vídeo
            // faz isso, e é uma das explicações possíveis para o preto.
            let dims_agora = buf_vigia.dims();
            if dims_agora != dims_antes {
                crate::registro::linha(&format!(
                    "a fonte mudou de tamanho: {}x{} -> {}x{}",
                    dims_antes.0, dims_antes.1, dims_agora.0, dims_agora.1
                ));
                dims_antes = dims_agora;
            }

            // O Windows ainda está entregando quadro? Se um jogo em tela cheia
            // exclusiva sai do compositor, a entrega simplesmente cessa e nós
            // ficamos repetindo o último quadro — que é outra explicação
            // possível, e bem diferente da anterior.
            let (entregues_agora, _) = video::contadores();
            if entregues_agora == entregues_antes {
                parado += 1;
                if parado >= RODADAS_PARA_ESCURO && !avisou_parado {
                    avisou_parado = true;
                    crate::registro::linha(
                        "sem quadros novos ha alguns segundos (tela parada, ou fonte fora do compositor)",
                    );
                    // SEM aviso sonoro aqui, e o motivo é o erro que isto já
                    // causou: "sem quadros novos" é o estado NORMAL de uma tela
                    // parada, porque o WGC só entrega quando algo muda. Ligado
                    // ao som, o app ficava apitando sem parar na cara de quem
                    // estava com o desktop quieto. A linha de log basta.
                }
            } else {
                if avisou_parado {
                    crate::registro::linha("voltaram a chegar quadros novos");
                }
                parado = 0;
                avisou_parado = false;
                entregues_antes = entregues_agora;
            }

            if buf_vigia.copy_latest(&mut quadro_amostra) {
                let (w, h) = buf_vigia.dims();
                // Terceira hipótese: o quadro chega, mas com tamanho que não
                // bate com o anunciado. `ajusta_para` responde a isso emitindo
                // preto, calado — este é o único lugar onde isso vira registro.
                let esperado = w as usize * h as usize * 3 / 2;
                if quadro_amostra.len() < esperado {
                    crate::registro::linha(&format!(
                        "quadro menor que o anunciado ({} bytes para {}x{}) — sai preto",
                        quadro_amostra.len(),
                        w,
                        h
                    ));
                }
                if video::parece_preto(&quadro_amostra, w as usize, h as usize) {
                    escuro += 1;
                    // ~5 s. Uma tela de carregamento ou um fade do jogo passam
                    // rápido; avisar neles seria só barulho.
                    if escuro >= RODADAS_PARA_ESCURO && !avisou_escuro {
                        avisou_escuro = true;
                        ao_escurecer(true);
                    }
                } else {
                    escuro = 0;
                    if avisou_escuro {
                        avisou_escuro = false;
                        ao_escurecer(false);
                    }
                }
            }

            // Ou o programa pode ter trocado de janela sem fechar a antiga.
            //
            // A checagem cara (varrer janelas e ler caminhos de executável) só
            // acontece depois de a janela sumir de vista por algumas rodadas
            // seguidas: enquanto ela está à mostra, não há nada a decidir, e
            // uma varredura a cada segundo custaria mais que a transmissão.
            if let Some(a) = assinatura.as_ref() {
                if video::estado_da_janela(a.hwnd).capturavel() {
                    sumida = 0;
                } else {
                    sumida += 1;
                    // ~3 s escondida. Menos que isso pegaria a janela no meio
                    // de uma troca de tela cheia e trocaria à toa.
                    if sumida >= 3 {
                        if let Some(nova) = foco::acha_sucessora(a) {
                            if !nova.minimizada {
                                motivo = MOTIVO_FONTE_TROCOU.to_string();
                                break;
                            }
                        }
                        // Nenhuma sucessora à vista: continua transmitindo a
                        // janela escondida em vez de derrubar a transmissão
                        // por nada. Recomeça a contagem para não varrer a cada
                        // rodada.
                        sumida = 0;
                    }
                }
            }

            std::thread::sleep(INTERVALO);

            let agora = Instant::now();
            let atual = rt_super.block_on(publicacao.estatisticas());
            let (entregues_agora, convertidos_agora) = tela_capture::video::contadores();
            let atrasadas_agora = tela_capture::video::atrasados();
            // Quem conta os quadros muda com o caminho. No antigo é o
            // `-progress` do ffmpeg; no novo o ffmpeg não vê mais o vídeo, e
            // quem sabe é a captura — `convertidos` são exatamente os quadros
            // que passaram pelo corte de cadência e foram ao encoder.
            let quadros_agora = if cod_super.is_some() {
                convertidos_agora
            } else {
                enc.as_ref()
                    .map_or(0, |e| e.quadros.load(Ordering::Relaxed))
            };
            let descartados_agora = atual.descartados
                + atual.fila_cheia
                + enc.as_ref().map_or(0, |e| e.descartes.load(Ordering::Relaxed))
                // Quadro codificado que não coube na fila de quem publica. Só
                // existe no caminho novo, e é o sinal de que a ponte de RTP
                // não está dando conta.
                + tela_capture::video::engasgados();

            let mut fps_intervalo = 0;
            let mut descartados_intervalo = 0;
            let mut entregues_intervalo = 0;
            let mut convertidos_intervalo = 0;
            // Quanto o laço passou do previsto. Uma volta que deveria levar 2s
            // e levou 3,5s é a máquina inteira parando, vista de dentro: nada
            // no resto da medida mostra isso, porque tudo é dividido pelo
            // tempo decorrido e a divisão esconde justamente a demora.
            let mut atraso_ms = 0i64;
            let mut nacks_intervalo = 0u64;
            let mut atrasadas_intervalo = 0u64;

            let kbps = match anterior {
                Some(ref m) => {
                    let dt = agora.duration_since(m.quando).as_secs_f64();
                    atraso_ms = (dt * 1000.0).round() as i64 - INTERVALO.as_millis() as i64;
                    descartados_intervalo = descartados_agora.saturating_sub(m.descartados);
                    entregues_intervalo = entregues_agora.saturating_sub(m.entregues);
                    convertidos_intervalo = convertidos_agora.saturating_sub(m.convertidos);
                    nacks_intervalo = atual.nacks.saturating_sub(m.nacks);
                    atrasadas_intervalo = atrasadas_agora.saturating_sub(m.atrasadas);
                    let bytes = (atual.bytes_video + atual.bytes_audio)
                        .saturating_sub(m.stats.bytes_video + m.stats.bytes_audio);
                    fps_intervalo = taxa_por_segundo(quadros_agora, m.quadros, dt);
                    if dt > 0.0 {
                        (bytes as f64 * 8.0 / 1000.0 / dt).round() as i32
                    } else {
                        0
                    }
                }
                None => 0,
            };
            anterior = Some(Marca {
                quando: agora,
                stats: atual,
                quadros: quadros_agora,
                entregues: entregues_agora,
                convertidos: convertidos_agora,
                descartados: descartados_agora,
                nacks: atual.nacks,
                atrasadas: atrasadas_agora,
            });

            let t = Telemetria {
                kbps,
                // Do intervalo, não a média desde o início: é a única forma de
                // uma travada de dois segundos aparecer.
                fps: fps_intervalo,
                // O `fps=` do ffmpeg é média desde o início do processo. No
                // caminho novo não existe equivalente, e inventar um a partir
                // do nosso contador seria repetir o `fps` do intervalo com
                // outro nome — melhor zero, que se lê como "não medido".
                fps_medio: if cod_super.is_some() {
                    0
                } else {
                    enc.as_ref()
                        .map_or(0, |e| e.fps_medio.load(Ordering::Relaxed))
                        as i32
                },
                atraso_ms,
                descartados_intervalo: descartados_intervalo as i32,
                entregues: entregues_intervalo as i32,
                convertidos: convertidos_intervalo as i32,
                // Três origens de perda, somadas porque para quem assiste dão
                // no mesmo: imagem faltando.
                //
                // `fila_cheia` estava de fora e isso escondia o pior caso: a
                // fila entre leitura e envio transbordando aparecia como
                // "0 perdidos" enquanto a transmissão saía quebrada.
                dropped: descartados_agora as i32,
                estado: atual.estado.to_string(),
                perda_por_mil: atual.perda_por_mil,
                jitter_ms: atual.jitter_ms,
                nacks: nacks_intervalo as i32,
                atrasadas: atrasadas_intervalo as i32,
                ultima_mudanca: ultima_mudanca.clone(),
                pid_encoder: enc.as_ref().map_or(0, |e| e.pid),
                parametros_vistos: atual.parametros_vistos,
                parametros_reinseridos: atual.parametros_reinseridos,
                segundos: inicio.elapsed().as_secs(),
                // Do ALVO em vigor, lido a cada volta — não de um valor
                // capturado antes do laço. A escada troca de resolução no meio
                // da transmissão, e a versão anterior reportava para sempre a
                // resolução inicial: um teste ao vivo mostrou `1920x1080` na
                // telemetria com a escada já em 720p30.
                largura: alvo.escala.map_or(nativa.0, |(l, _)| l),
                altura: alvo.escala.map_or(nativa.1, |(_, h)| h),
                // Com o sufixo do CAMINHO, e isso não é enfeite: sem ele a
                // única pista de estar no caminho antigo é a CPU alta, que
                // ninguém olha quando está funcionando. Duas sessões inteiras
                // rodaram assim antes de alguém reparar.
                encoder: if cod_super.is_some() {
                    format!("{encoder}/gpu")
                } else {
                    format!("{encoder}/ffmpeg")
                },
                audio_do_sistema,
            };
            sink_telemetria.grava(t.clone());
            ao_medir(&t);

            // --- taxa dinâmica ---
            //
            // O controlador vive AQUI, dentro do supervisor, e não num estático
            // global. Só dá para fazer assim porque o reinício do encoder não
            // derruba a transmissão: se derrubasse, o supervisor morreria junto
            // e o controlador renasceria no teto, desceria, renasceria, num
            // laço. Foi essa a razão de o caminho barato exigir estado global.
            // A agenda de teste tem precedência: ela existe justamente para
            // exercitar a troca sem depender de a rede piorar.
            let mut motivo_troca = "agenda de teste".to_string();
            let agendada = agenda
                .first()
                .filter(|(s, _)| inicio.elapsed().as_secs() >= *s)
                .map(|(_, k)| *k);
            if agendada.is_some() {
                agenda.remove(0);
            }

            let novo_alvo = match agendada {
                Some(PassoTeste::Taxa(k)) => Alvo { kbps: k, ..alvo },
                Some(PassoTeste::Degrau(i)) => {
                    let d = crate::escada::ESCADA[i];
                    alvo_do_degrau(d, d.max, receita.nativa)
                }
                None => match qualidade.amostra(t.perda_por_mil) {
                    Some((degrau, kbps, porque)) => {
                        motivo_troca = porque;
                        alvo_do_degrau(degrau, kbps, receita.nativa)
                    }
                    None => continue,
                },
            };
            if novo_alvo == alvo {
                continue;
            }
            crate::registro::linha(&format!(
                "qualidade: {} -> {} ({motivo_troca})",
                descreve(&alvo),
                descreve(&novo_alvo)
            ));
            ultima_mudanca = format!("{} — {motivo_troca}", descreve(&novo_alvo));

            // No caminho em processo não há nada a reiniciar: o encoder mora
            // dentro da captura e se reconfigura no quadro seguinte. O ffmpeg
            // que sobrou cuida só do áudio e não tem opinião sobre resolução.
            //
            // É a diferença mais visível entre os dois caminhos: aqui a troca
            // de qualidade custa um quadro, contra o meio segundo de imagem
            // congelada de matar e recriar um processo.
            if let Some(c) = &cod_super {
                aplica_alvo(c, novo_alvo);
                alvo = novo_alvo;
                qualidade.assume(novo_alvo.kbps);
                publicacao.ajusta_taxa(novo_alvo.kbps);
                continue;
            }

            // Mata o antigo ANTES de subir o novo. Os dois escrevem nas mesmas
            // portas UDP; sobrepostos, o que chega do outro lado é RTP de duas
            // fontes intercalado, que é pior que a pausa de meio segundo.
            let Some(velho) = enc.take() else { break };
            velho.encerra();

            match sobe_encoder(&receita, novo_alvo) {
                Ok(e) => {
                    enc = Some(e);
                    alvo = novo_alvo;
                    // O controlador precisa saber da troca mesmo quando ela
                    // não partiu dele — pela agenda de teste, hoje — senão a
                    // próxima conta dele parte de uma taxa que não existe mais.
                    qualidade.assume(novo_alvo.kbps);
                    // O espaçador de pacotes persegue a taxa antiga até ser
                    // avisado. Alto demais devolve a rajada que ele contém;
                    // baixo demais estrangula o encoder novo.
                    publicacao.ajusta_taxa(novo_alvo.kbps);
                }
                Err(e) => {
                    crate::registro::linha(&format!("não consegui trocar a qualidade: {e:#}"));
                    motivo = format!("o encoder não voltou depois de mudar a qualidade: {e}");
                    break;
                }
            }
        }

        // Encerramento, na ordem inversa de quem depende de quem. A bandeira do
        // relógio sobe primeiro: é ela que solta a ponte e para de produzir
        // quadro para um canal que ninguém mais vai ler.
        parar_super.store(true, Ordering::Relaxed);
        if let Some(c) = &cod_super {
            c.parar.store(true, Ordering::Relaxed);
        }
        if let Some(e) = enc.take() {
            e.encerra();
        }
        if let Err(e) = captura.encerra() {
            eprintln!("encerrar captura: {e:#}");
        }
        // Depois da captura, nunca antes: é o fim dela que fecha o canal e faz
        // a ponte sair do `recv`. Juntar antes seria esperar para sempre.
        if let Some(p) = ponte {
            let _ = p.join();
        }
        parar_audio.store(true, Ordering::Relaxed);
        if let Some(a) = audio_processo {
            let _ = a.join();
        }
        // DELETE no recurso WHIP: sem isso o servidor só libera o path quando
        // o ICE dele expirar, e a tela fica pendurada na sala para todo mundo.
        if let Err(e) = rt_super.block_on(publicacao.encerra()) {
            eprintln!("encerrar publicação WHIP: {e:#}");
        }
        ao_terminar(motivo);
    });

    Ok(Transmissao {
        parar,
        supervisor: Some(supervisor),
        telemetria,
    })
}

impl Transmissao {
    /// Larga a transmissão sem esperar o supervisor terminar.
    ///
    /// Existe para o seguidor de janela, que roda DENTRO da thread do
    /// supervisor: `encerra()` ali faria essa thread esperar por si mesma e
    /// travaria para sempre. Neste ponto o desmonte já aconteceu — o ffmpeg
    /// foi morto, as threads juntadas, a captura fechada e o WHIP encerrado —,
    /// então só resta soltar o handle, e soltar um `JoinHandle` apenas o
    /// desanexa.
    pub fn abandona(mut self) {
        self.parar.store(true, Ordering::Relaxed);
        drop(self.supervisor.take());
    }

    /// Pede a parada e espera o desmonte terminar.
    pub fn encerra(mut self) {
        self.parar.store(true, Ordering::Relaxed);
        if let Some(h) = self.supervisor.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Transmissao {
    fn drop(&mut self) {
        // Rede de segurança para o caso de o app fechar com a transmissão no
        // ar: sem isso o ffmpeg viraria processo órfão.
        self.parar.store(true, Ordering::Relaxed);
        if let Some(h) = self.supervisor.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod testes {
    use super::escala_para;

    #[test]
    fn nao_amplia() {
        assert_eq!(escala_para(1280, 720, 1080), None);
        assert_eq!(escala_para(1920, 1080, 1080), None);
    }

    #[test]
    fn reduz_mantendo_proporcao_e_paridade() {
        assert_eq!(escala_para(2560, 1440, 1080), Some((1920, 1080)));
        assert_eq!(escala_para(3840, 2160, 720), Some((1280, 720)));
        // 1366x768 para 720: 1366*720/768 = 1280,6 -> 1280 (par).
        assert_eq!(escala_para(1366, 768, 720), Some((1280, 720)));
    }

    #[test]
    fn altura_zero_e_nativo() {
        assert_eq!(escala_para(2560, 1440, 0), None);
    }
}

#[cfg(test)]
mod testes_taxa {
    use super::taxa_por_segundo;

    /// O caso normal: 120 quadros em 2 segundos são 60 por segundo.
    #[test]
    fn deriva_a_taxa_do_intervalo() {
        assert_eq!(taxa_por_segundo(1200, 1080, 2.0), 60);
        assert_eq!(taxa_por_segundo(1200, 1140, 2.0), 30);
    }

    /// Contador que volta a zero (transmissão recomeçou) tem de dar 0, não um
    /// número absurdo vindo de subtração ao contrário.
    ///
    /// Perde-se a taxa real daquele intervalo — 5 quadros em 2s seriam 2 —, e
    /// tudo bem: é uma amostra só, e ela se corrige na volta seguinte. O erro
    /// que importa evitar é o outro, o de reportar bilhões de fps.
    #[test]
    fn contador_reiniciado_nao_vira_numero_gigante() {
        assert_eq!(taxa_por_segundo(5, 100_000, 2.0), 0);
        assert_eq!(taxa_por_segundo(0, 100_000, 2.0), 0);
    }

    /// Intervalo zero ou negativo não pode dividir.
    #[test]
    fn intervalo_invalido_nao_divide() {
        assert_eq!(taxa_por_segundo(1200, 1080, 0.0), 0);
        assert_eq!(taxa_por_segundo(1200, 1080, -1.0), 0);
    }

    /// O DEFEITO que motivou tudo isto, escrito como teste.
    ///
    /// Depois de vinte minutos a 60fps, dois segundos parado somem na média
    /// acumulada — ela sai de 60 para 59,9, que arredonda de volta para 60. A
    /// taxa do intervalo mostra o mesmo instante como 0. Enquanto a telemetria
    /// só tinha a média, "está congelando" caía num log onde nada aparecia.
    #[test]
    fn media_longa_esconde_a_travada_que_o_intervalo_mostra() {
        let segundos_no_ar = 20.0 * 60.0;
        let quadros = (segundos_no_ar * 60.0) as u64;

        // Dois segundos sem produzir um quadro sequer.
        let media_depois = quadros as f64 / (segundos_no_ar + 2.0);
        assert_eq!(media_depois.round() as i32, 60, "a média não se move");

        assert_eq!(
            taxa_por_segundo(quadros, quadros, 2.0),
            0,
            "o intervalo mostra a parada"
        );
    }
}

#[cfg(test)]
mod testes_aplica_alvo {
    use super::{aplica_alvo, Alvo};
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use tela_capture::video::{Codificacao, FrameBuffer};

    fn controles() -> Codificacao {
        let (envio, _recebe) = std::sync::mpsc::sync_channel(1);
        Codificacao {
            envio,
            kbps: AtomicU32::new(0),
            fps: AtomicU32::new(0),
            altura_alvo: AtomicU32::new(9999),
            chave_pedida: AtomicBool::new(false),
            amostra: FrameBuffer::shared(),
            retido: std::sync::Mutex::new(None),
            dispositivo: std::sync::OnceLock::new(),
            parar: AtomicBool::new(false),
        }
    }

    #[test]
    fn escala_ausente_vira_altura_zero() {
        let c = controles();
        aplica_alvo(
            &c,
            Alvo {
                kbps: 8000,
                escala: None,
                fps: 60,
            },
        );
        assert_eq!(c.kbps.load(Ordering::Relaxed), 8000);
        assert_eq!(c.fps.load(Ordering::Relaxed), 60);
        // Zero é como o handler entende "manda no tamanho da fonte". Deixar o
        // valor anterior faria a transmissão continuar reduzindo depois de a
        // escada ter subido para o nativo.
        assert_eq!(c.altura_alvo.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn escala_presente_leva_a_altura() {
        let c = controles();
        aplica_alvo(
            &c,
            Alvo {
                kbps: 4000,
                escala: Some((1280, 720)),
                fps: 30,
            },
        );
        assert_eq!(c.altura_alvo.load(Ordering::Relaxed), 720);
        assert_eq!(c.fps.load(Ordering::Relaxed), 30);
    }

    #[test]
    fn fps_zero_nao_passa() {
        // Dividir por zero no cálculo do corte de cadência travaria a captura.
        let c = controles();
        aplica_alvo(
            &c,
            Alvo {
                kbps: 4000,
                escala: None,
                fps: 0,
            },
        );
        assert_eq!(c.fps.load(Ordering::Relaxed), 1);
    }
}

#[cfg(test)]
mod testes_prepara_gpu {
    use super::*;

    /// Reproduz exatamente o que a transmissão faz ao escolher o caminho novo.
    ///
    /// Existe porque o recuo é silencioso por natureza: a transmissão sobe, a
    /// imagem aparece, e a única pista de que se está no caminho antigo é a CPU
    /// alta. Aqui o erro vem inteiro, e não pela ausência de log.
    #[test]
    #[ignore = "abre captura de verdade; depende da máquina"]
    fn prepara_pelo_caminho_novo() {
        let Some(base) = std::env::var_os("LOCALAPPDATA") else {
            eprintln!("sem LOCALAPPDATA, pulando");
            return;
        };
        let pasta = std::path::Path::new(&base).join("tela").join("libav");
        if !tela_capture::libav::carrega(&pasta) {
            eprintln!("a libav não carregou de {}, pulando", pasta.display());
            return;
        }
        let Ok(monitores) = video::list_monitors() else {
            eprintln!("sem monitores, pulando");
            return;
        };
        let Some(m) = monitores.first() else {
            eprintln!("nenhum monitor, pulando");
            return;
        };
        let fonte = Fonte::Monitor {
            indice: m.index,
            nome: m.name.clone(),
            excluir_pid: None,
        };
        // Os mesmos valores que `main` passa: um envelope, porque a política
        // só nasce depois de a captura revelar a resolução.
        let qualidade = Qualidade {
            altura: 0,
            fps: crate::escada::ESCADA
                .iter()
                .map(|d| d.fps)
                .max()
                .unwrap_or(60),
            kbps: 0,
        };
        match prepara(&fonte, qualidade, true, true) {
            Ok(p) => {
                println!("OK — {}x{} pelo caminho novo", p.largura, p.altura);
                drop(p);
            }
            Err(e) => panic!("o caminho novo recusou: {e:#}"),
        }
    }
}
