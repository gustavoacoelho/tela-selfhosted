//! Publicação WHIP nativa: negocia a sessão WebRTC em Rust e repassa para
//! dentro dela o RTP que o ffmpeg entrega em portas UDP locais.
//!
//! # Por que não usar o muxer WHIP do ffmpeg
//!
//! O build de ffmpeg que distribuímos tem `--enable-schannel` e não tem
//! OpenSSL. O caminho de DTLS-SRTP do ffmpeg só é exercitado com OpenSSL; com
//! SChannel ele falha em `Creating security context failed (0x80090331)` —
//! `SEC_E_ALGORITHM_MISMATCH`, porque o SChannel não expõe os perfis de
//! DTLS-SRTP que o WebRTC exige. Trocar o build de ffmpeg resolveria, mas nos
//! amarraria a distribuir um binário específico; fazer o WebRTC em Rust não.
//!
//! # Divisão de trabalho
//!
//! O ffmpeg continua fazendo o que já faz bem: encodar H.264 (por hardware) e
//! Opus, e **fragmentar as NAL units em pacotes RTP**. Reimplementar
//! packetização de H.264 seria refazer código maduro sem ganho. Este módulo só
//! transporta: lê os datagramas RTP prontos e os injeta nas tracks WebRTC, que
//! cuidam de SRTP, DTLS e ICE.

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use std::time::Instant;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS};
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::ice::mdns::MulticastDnsMode;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtcp::receiver_report::ReceiverReport;
use webrtc::rtcp::transport_feedbacks::transport_layer_nack::TransportLayerNack;
use webrtc::rtp::codecs::h264::H264Payloader;
use webrtc::rtp::header::Header;
use webrtc::rtp::packet::Packet;
use webrtc::rtp::packetizer::Payloader;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::rtp_transceiver::rtp_sender::RTCRtpSender;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_local::{TrackLocal, TrackLocalWriter};
use webrtc::util::Unmarshal;

/// Teto de espera pelo ICE gathering. Sem servidor STUN configurado só há
/// candidatos host, que aparecem em milissegundos; o teto existe para não
/// travar para sempre se alguma interface de rede se recusar a responder.
const ESPERA_GATHERING: Duration = Duration::from_secs(10);

/// Teto de espera até o `RTCPeerConnection` chegar a `Connected` (ICE + DTLS).
const ESPERA_CONEXAO: Duration = Duration::from_secs(20);

/// MTU de recepção. Pedimos ao ffmpeg pacotes de 1200 bytes (ver
/// `ffmpeg::Destino::Rtp`), então 2 KiB sobra com folga.
const MTU_RECEPCAO: usize = 2048;

/// MTU dos pacotes RTP que nós mesmos montamos.
///
/// O mesmo 1200 que pedimos ao ffmpeg em `ffmpeg::Destino::Rtp`, pelo mesmo
/// motivo: cabe num datagrama de 1500 depois de RTP, SRTP, UDP e IP, com folga
/// para túneis pelo caminho.
const MTU_ENVIO: usize = 1200;

/// Configuração de uma publicação WHIP.
pub struct Config<'a> {
    /// Endpoint WHIP (`http`/`https`), ex.: `https://tela.example.com/sala/whip`.
    pub whip_url: &'a str,
    /// Porta UDP local onde o ffmpeg entrega o RTP de vídeo.
    pub porta_video: u16,
    /// Porta UDP local onde o ffmpeg entrega o RTP de áudio.
    pub porta_audio: u16,
    /// Bitrate de vídeo pedido ao encoder, em kbps. É a base do ritmo de
    /// envio — ver [`Pacer`].
    pub kbps_video: u32,
    /// Vídeo e áudio vêm dos nossos encoders, não do `ffmpeg.exe` por UDP.
    ///
    /// Um flag só para os dois porque eles andam juntos: ou o `ffmpeg.exe`
    /// existe e cuida do áudio, ou não existe e nós cuidamos dos dois. Quando
    /// ligado, `porta_video` e `porta_audio` não são usadas — nada escuta
    /// nelas e o RTP é montado aqui, por [`EmissorH264`] e [`EmissorOpus`].
    pub em_processo: bool,
}

/// Fator sobre o bitrate alvo usado para espaçar os pacotes.
///
/// Não pode ser 1,0: o encoder é CBR na média, mas um keyframe é vários
/// múltiplos do quadro comum. Com folga de 1,5x a rajada do keyframe escoa em
/// vez de acumular atraso, e ainda assim o pico instantâneo não estoura o
/// enlace. É a mesma ordem de grandeza que o `libwebrtc` usa.
const FATOR_PACING: f64 = 1.5;

/// Folga do balde, em segundos de bitrate. Quantos bytes podem sair de uma vez
/// antes de o espaçamento passar a valer.
///
/// A 3 Mbps são ~11 KB, cerca de 9 pacotes. Precisa ser bem menor que um
/// keyframe (~100 KB), senão o balde o absorve inteiro e não há espaçamento.
/// Muito menor que isso também não ajuda: o `sleep` do Windows tem granularidade
/// grosseira e a transmissão ficaria aos solavancos.
const JANELA_BURST: f64 = 0.02;

/// Quantos pacotes cabem na fila entre a leitura do socket e o envio.
///
/// Um keyframe de 1080p a 6 Mbps passa de 80 pacotes; 512 dá folga para alguns
/// deles sem virar atraso perceptível se algo travar. Cheia, a fila descarta —
/// perder quadro é melhor que acumular latência sem limite numa transmissão ao
/// vivo.
const FILA_PACOTES: usize = 512;

/// Espaçador de pacotes, em balde de fichas.
///
/// O ffmpeg entrega os pacotes RTP de um quadro **todos de uma vez**. Sem
/// espaçamento, essa rajada estoura o buffer do primeiro enlace estreito do
/// caminho e o servidor registra perda — que em H.264 quebra fragmentos FU-A
/// no meio e corrompe o quadro até o próximo keyframe. Medido em produção:
/// 1,4% de perda com rajada, zero em loopback (onde não há gargalo).
struct Pacer {
    /// Taxa alvo, compartilhada e mutável — ver [`Pacer::novo`].
    kbps: Arc<AtomicU32>,
    /// Fichas disponíveis, em bytes.
    balde: f64,
    ultimo: Instant,
}

impl Pacer {
    /// A taxa vem num `Arc` e não por valor porque ela MUDA: trocar o bitrate
    /// reinicia o ffmpeg, mas não esta tarefa. Um pacer preso à taxa antiga
    /// ficaria espaçando para 8 Mbps um fluxo que passou a ser de 6, e o
    /// espaçamento é justamente o que impede a rajada de estourar o gargalo.
    fn novo(kbps: Arc<AtomicU32>) -> Self {
        Self {
            kbps,
            balde: 0.0,
            ultimo: Instant::now(),
        }
    }

    fn bytes_por_s(&self) -> f64 {
        (self.kbps.load(Ordering::Relaxed).max(1) as f64) * 1000.0 / 8.0 * FATOR_PACING
    }

    /// Crédito acumulável.
    ///
    /// A folga precisa ser MENOR que a rajada que se quer conter, senão o
    /// balde absorve o keyframe inteiro e ele sai sem espaçamento nenhum —
    /// foi exatamente o defeito da primeira versão, com 0,25 s de crédito
    /// (140 KB a 3 Mbps) contra um keyframe de ~100 KB: passava direto.
    fn capacidade(&self) -> f64 {
        self.bytes_por_s() * JANELA_BURST
    }

    /// Espera até haver crédito para `bytes`.
    async fn aguarda(&mut self, bytes: usize) {
        let bytes = bytes as f64;
        loop {
            let agora = Instant::now();
            let taxa = self.bytes_por_s();
            self.balde = (self.balde + agora.duration_since(self.ultimo).as_secs_f64() * taxa)
                .min(self.capacidade());
            self.ultimo = agora;

            if self.balde >= bytes {
                self.balde -= bytes;
                return;
            }

            // Dorme só o que falta. O teto de 5 ms evita que um cálculo ruim
            // (bitrate absurdamente baixo) congele a transmissão.
            let espera = ((bytes - self.balde) / taxa).min(0.005);
            tokio::time::sleep(Duration::from_secs_f64(espera)).await;
        }
    }
}

/// Contadores compartilhados entre as tarefas de fundo e a UI.
///
/// Tudo atômico porque as tarefas de repasse rodam no runtime do tokio e quem
/// lê é a thread principal do CLI (e, no Plano 2, a thread da UI).
#[derive(Debug, Default)]
struct Contadores {
    pacotes_video: AtomicU64,
    bytes_video: AtomicU64,
    pacotes_audio: AtomicU64,
    bytes_audio: AtomicU64,
    /// Datagramas que chegaram mas não são RTP válido.
    descartados: AtomicU64,
    /// `write_rtp` recusado — normal enquanto o DTLS ainda não subiu.
    falhas_envio: AtomicU64,
    /// Pacotes largados porque a fila entre leitura e envio encheu. Diferente
    /// de `descartados`: aqui o pacote era válido, faltou vazão.
    fila_cheia: AtomicU64,
    /// PLIs recebidos do servidor. Ver [`tarefa_rtcp`] para o porquê de só contar.
    plis: AtomicU64,
    /// Pedidos de retransmissão vindos do servidor.
    ///
    /// A retransmissão está ligada do nosso lado (o `Responder` do NACK entra
    /// pelo `register_default_interceptors`), mas ligada não é o mesmo que
    /// acontecendo: o servidor precisa PEDIR. Sem este contador não dá para
    /// distinguir "a perda foi recuperada e o artefato veio de outro lugar" de
    /// "ninguém pediu nada e a perda virou buraco na imagem" — que pedem
    /// conserto em lugares completamente diferentes.
    nacks: AtomicU64,
    /// Perda no caminho, em partes por mil, como o servidor a reporta no
    /// Receiver Report. Diferente de tudo o mais aqui: não é algo que nós
    /// descartamos, é o que não chegou do outro lado.
    /// Pior fração de perda vista desde a última leitura, em partes por mil.
    ///
    /// PICO e não último valor: `fraction_lost` cobre só o intervalo desde o
    /// relatório anterior, e guardar apenas o mais recente faz um estouro
    /// entre duas amostras desaparecer. Foi assim que a telemetria marcou
    /// "perda 0/1000" enquanto o servidor contava 155 pacotes perdidos e a
    /// sala inteira via tela preta.
    perda_por_mil: AtomicU64,
    /// Variação de chegada dos pacotes, em milissegundos.
    ///
    /// Vinha nos relatórios desde sempre e era jogada fora. É ela que explica
    /// travadas com perda zero: os pacotes chegam, mas em ritmo irregular, e o
    /// buffer de quem assiste não absorve. Numa sessão com ping alto a 8000
    /// kbps, tudo o que mediamos estava verde enquanto a imagem engasgava.
    jitter_ms: AtomicU64,
    /// Total acumulado de pacotes que o servidor não recebeu.
    perdidos_no_caminho: AtomicU64,
    /// Carimbos de tempo que teriam saído fora de ordem e a trava corrigiu.
    ///
    /// Deveria ser zero para sempre. Se subir, alguma coisa a montante voltou a
    /// entregar quadro com tempo mais velho que o anterior — e é isso que faz o
    /// servidor derrubar quem está assistindo.
    tempo_torto: AtomicU64,
    /// Quantas vezes reinserimos SPS/PPS antes de um quadro-chave.
    parametros_reinseridos: AtomicU64,
    /// Quantas vezes o encoder mandou os parâmetros por conta própria.
    ///
    /// Zero depois de alguns segundos no ar significa que o fluxo é
    /// indecodificável para quem quer que seja — é um fato sobre o encoder, não
    /// um palpite sobre a imagem, e é o gatilho honesto para trocar de encoder.
    parametros_vistos: AtomicU64,
    /// Quantos Receiver Reports chegaram. Distingue "sem perda" de "sem dado":
    /// se ficar em zero, o servidor não está reportando e a perda exibida não
    /// significa nada.
    relatorios: AtomicU64,
    pt_video: AtomicU8,
    pt_audio: AtomicU8,
}

/// Fotografia do transporte, para diagnóstico e para a UI do Plano 2.
#[derive(Debug, Clone, Copy)]
pub struct Estatisticas {
    pub estado: RTCPeerConnectionState,
    pub pacotes_video: u64,
    pub bytes_video: u64,
    pub pacotes_audio: u64,
    pub bytes_audio: u64,
    pub descartados: u64,
    pub falhas_envio: u64,
    /// Pacotes largados por fila cheia — sinal de que o envio não acompanha.
    pub fila_cheia: u64,
    /// Perda no caminho até o servidor, em partes por mil (10 = 1%).
    pub perda_por_mil: u64,
    /// Variação de chegada dos pacotes, em milissegundos.
    pub jitter_ms: u64,
    /// Total de pacotes que o servidor não recebeu.
    pub perdidos_no_caminho: u64,
    /// Vezes que o encoder mandou SPS por conta própria. Ver o campo homônimo
    /// em `Contadores`.
    pub parametros_vistos: u64,
    /// Vezes que precisamos reinserir os parâmetros no lugar dele.
    pub parametros_reinseridos: u64,
    /// Ver [`Contadores::tempo_torto`]. Deveria ser zero para sempre.
    pub tempo_torto: u64,
    /// Receiver Reports recebidos. Zero significa que não há medida de perda —
    /// não que a perda seja zero.
    pub relatorios: u64,
    pub plis: u64,
    /// Pedidos de retransmissão recebidos. Ver `Contadores::nacks`.
    pub nacks: u64,
    /// Payload type negociado no SDP para cada codec — ver [`publica`].
    pub pt_video: u8,
    pub pt_audio: u8,
}

/// Monta pacotes RTP a partir dos quadros H.264 do nosso encoder.
///
/// # Por que aqui e não no ffmpeg
///
/// Quando o `ffmpeg.exe` encodava, ele também empacotava e mandava por UDP; a
/// [`Continuidade`] existia para remendar a numeração quando o processo
/// reiniciava. Encodando em processo, esse remendo some: a numeração é nossa
/// desde o começo e simplesmente nunca reinicia. Trocar de resolução vira um
/// detalhe interno do encoder, e quem assiste não vê nada.
///
/// # Marcação de tempo
///
/// O timestamp vem do relógio de verdade, não de um contador de quadros. O WGC
/// entrega por evento — nada muda na tela, nenhum quadro chega —, então o
/// intervalo entre quadros varia. Um contador fixo faria a imagem acelerar e
/// desacelerar em relação ao áudio.
pub struct EmissorH264 {
    payloader: H264Payloader,
    /// Numeração própria, contínua por toda a sessão.
    seq: u16,
    fila: mpsc::Sender<(Packet, usize)>,
    contadores: Arc<Contadores>,
    /// Zero do relógio de apresentação, fixado no PRIMEIRO quadro.
    ///
    /// E não na criação do emissor: os primeiros quadros são capturados durante
    /// o preparo da transmissão, antes de existir sessão WHIP. Com o zero fixado
    /// aqui, todos eles cairiam em tempo zero — vários quadros com o mesmo
    /// carimbo, que é justamente o que faz o receptor concluir que há quadros B.
    inicio: Option<Instant>,
    /// Último carimbo emitido, para nenhum quadro sair antes de outro.
    ultimo_ts: Option<u32>,
    /// SPS e PPS mais recentes, em Annex-B com o código de início.
    ///
    /// Guardados para poderem ser reinseridos antes de cada IDR que não os
    /// traga. Ver [`EmissorH264::envia`].
    sps: Vec<u8>,
    pps: Vec<u8>,
}

impl EmissorH264 {
    /// Um emissor solto sem zero fixado, como o de verdade nasce.
    #[cfg(test)]
    fn de_teste_sem_zero(fila: mpsc::Sender<(Packet, usize)>) -> Self {
        Self {
            payloader: H264Payloader::default(),
            seq: 0,
            fila,
            contadores: Arc::new(Contadores::default()),
            inicio: None,
            ultimo_ts: None,
            sps: Vec::new(),
            pps: Vec::new(),
        }
    }

    /// Um emissor solto, para teste.
    ///
    /// O de verdade nasce dentro de [`publica`], amarrado à sessão. Este
    /// existe para poder exercitar o empacotamento sem subir WebRTC — que é o
    /// que separa "o RTP sai certo" de "a chamada não deu erro".
    #[cfg(test)]
    fn de_teste(fila: mpsc::Sender<(Packet, usize)>, inicio: Instant) -> Self {
        Self {
            payloader: H264Payloader::default(),
            seq: 0,
            fila,
            contadores: Arc::new(Contadores::default()),
            inicio: Some(inicio),
            ultimo_ts: None,
            sps: Vec::new(),
            pps: Vec::new(),
        }
    }

    /// Empacota uma unidade de acesso (um quadro em Annex-B) e a enfileira.
    ///
    /// `instante` é quando o quadro foi capturado e `chave` é o que o encoder
    /// disse sobre ele (`AV_PKT_FLAG_KEY`). O `H264Payloader` cuida de
    /// fragmentar em FU-A o que não couber na MTU; repor os parâmetros da
    /// imagem é conosco, ver abaixo.
    pub fn envia(&mut self, unidade: &[u8], instante: Instant, chave: bool) {
        if unidade.is_empty() {
            return;
        }
        let zero = *self.inicio.get_or_insert(instante);
        let decorrido = instante.saturating_duration_since(zero);
        // Trunca de propósito: o timestamp RTP é de 32 bits e dá a volta, o
        // que é normal e esperado pelo receptor.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let ts = (decorrido.as_secs_f64() * f64::from(RELOGIO_VIDEO)) as u64 as u32;

        // O vigia do app troca de encoder quando NINGUÉM emite os parâmetros
        // da imagem — foi ele que pegou o `h264_amf` transmitindo preto. Ele
        // lê este contador, que no caminho do `ffmpeg.exe` é alimentado pela
        // leitura do socket. Aqui não há socket, então quem conta somos nós:
        // sem isto o vigia conclui "encoder mudo", troca de encoder até
        // esgotar a cascata e derruba uma transmissão que estava perfeita.
        // Guarda os parâmetros e descobre se este quadro é IDR, numa passada.
        let mut sps_novo = None;
        let mut pps_novo = None;
        let mut tem_idr = false;
        para_cada_nal(unidade, |tipo, fatia| match tipo {
            7 => sps_novo = Some(fatia.to_vec()),
            8 => pps_novo = Some(fatia.to_vec()),
            5 => tem_idr = true,
            _ => {}
        });
        let trouxe_parametros = sps_novo.is_some() || pps_novo.is_some();
        if let Some(v) = sps_novo {
            self.sps = v;
        }
        if let Some(v) = pps_novo {
            self.pps = v;
        }

        // Reinsere SPS e PPS antes de todo IDR que não os traga.
        //
        // # Por que não dá para confiar no encoder nem no empacotador
        //
        // O `H264Payloader` guarda o par e emite um STAP-A quando o VÊ — uma
        // vez, não antes de cada IDR. Então tudo depende de o encoder repetir
        // os parâmetros em banda, e isso varia: o nvenc repete a cada IDR, o
        // AMF mandou UMA vez e nunca mais. Resultado ao vivo: `params 1+0`
        // congelado e tela preta para todo mundo que entrou na sala depois do
        // primeiro quadro — não havia como o decodificador saber as dimensões
        // da imagem.
        //
        // Fazendo aqui, vale para qualquer encoder, que é o mesmo que o
        // caminho do `ffmpeg.exe` já fazia sobre os pacotes RTP.
        // O gatilho é a bandeira do encoder OU uma NAL de IDR. As duas porque
        // elas discordam: o AMF sem `forced_idr` emite I-frame comum, que não
        // traz NAL tipo 5 e mesmo assim precisa dos parâmetros para servir a
        // quem acabou de entrar.
        let e_chave = chave || tem_idr;
        let mut reinseriu = false;
        let com_parametros;
        let unidade: &[u8] =
            if e_chave && !trouxe_parametros && !self.sps.is_empty() && !self.pps.is_empty() {
                let mut v = Vec::with_capacity(self.sps.len() + self.pps.len() + unidade.len());
                v.extend_from_slice(&self.sps);
                v.extend_from_slice(&self.pps);
                v.extend_from_slice(unidade);
                com_parametros = v;
                reinseriu = true;
                &com_parametros
            } else {
                unidade
            };

        // O vigia do app troca de encoder quando NINGUÉM emite os parâmetros
        // da imagem — foi ele que pegou o `h264_amf` transmitindo preto. Ele
        // lê este contador, que no caminho do `ffmpeg.exe` é alimentado pela
        // leitura do socket. Aqui não há socket, então quem conta somos nós:
        // sem isto o vigia conclui "encoder mudo", troca de encoder até
        // esgotar a cascata e derruba uma transmissão que estava perfeita.
        if trouxe_parametros || reinseriu {
            self.contadores
                .parametros_vistos
                .fetch_add(1, Ordering::Relaxed);
        }
        if reinseriu {
            self.contadores
                .parametros_reinseridos
                .fetch_add(1, Ordering::Relaxed);
        }

        // Trava de monotonicidade. Quem alimenta já garante ordem, mas o
        // preço de errar aqui é alto e desproporcional ao custo da guarda:
        // um único carimbo fora de ordem faz o receptor concluir que há
        // quadros B, e o servidor DERRUBA a sessão de quem está assistindo.
        let ts = match self.ultimo_ts {
            Some(anterior) if ts <= anterior => {
                self.contadores.tempo_torto.fetch_add(1, Ordering::Relaxed);
                anterior.wrapping_add(1)
            }
            _ => ts,
        };
        self.ultimo_ts = Some(ts);

        let carga = bytes::Bytes::copy_from_slice(unidade);
        let Ok(pedacos) = self.payloader.payload(MTU_ENVIO, &carga) else {
            self.contadores.descartados.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let ultimo = pedacos.len().saturating_sub(1);
        for (i, pedaco) in pedacos.into_iter().enumerate() {
            let pacote = Packet {
                header: Header {
                    version: 2,
                    // Fecha a unidade de acesso. Sem isto o receptor não sabe
                    // que o quadro acabou e segura a exibição até o próximo.
                    marker: i == ultimo,
                    sequence_number: self.seq,
                    timestamp: ts,
                    // `payload_type` e `ssrc` são reescritos pela track com os
                    // valores negociados — ver o comentário sobre payload type
                    // em [`publica`].
                    ..Default::default()
                },
                payload: pedaco,
            };
            self.seq = self.seq.wrapping_add(1);
            let tamanho = pacote.payload.len() + 12;
            if self.fila.try_send((pacote, tamanho)).is_err() {
                self.contadores.fila_cheia.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Percorre as NAL da unidade de acesso em Annex-B.
///
/// Entrega o tipo (os cinco bits baixos do primeiro byte) e a fatia que
/// COMEÇA no código de início — assim quem chama pode recopiar a NAL inteira
/// como está, sem precisar remontar o prefixo.
fn para_cada_nal(unidade: &[u8], mut f: impl FnMut(u8, &[u8])) {
    // Primeiro os marcos, depois as fatias: o fim de uma NAL é o começo do
    // código de início da seguinte, e isso só se sabe olhando adiante.
    let mut marcos: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i + 3 < unidade.len() {
        if unidade[i] != 0 || unidade[i + 1] != 0 {
            i += 1;
            continue;
        }
        // Annex-B permite `00 00 01` e `00 00 00 01`, e a libav usa os dois.
        let cabeca = if unidade[i + 2] == 1 {
            i + 3
        } else if unidade[i + 2] == 0 && unidade[i + 3] == 1 {
            i + 4
        } else {
            i += 1;
            continue;
        };
        marcos.push((i, cabeca));
        i = cabeca;
    }
    for (n, &(inicio, cabeca)) in marcos.iter().enumerate() {
        let fim = marcos.get(n + 1).map_or(unidade.len(), |&(p, _)| p);
        if let Some(b) = unidade.get(cabeca) {
            f(b & 0x1F, &unidade[inicio..fim]);
        }
    }
}

/// Monta pacotes RTP a partir dos quadros de Opus do nosso encoder.
///
/// Muito mais simples que o de vídeo: um quadro de Opus cabe inteiro num
/// pacote (são ~320 bytes a 128 kbps), então não há fragmentação, e o carimbo
/// já vem em amostras a 48 kHz — que é exatamente o relógio RTP do Opus.
pub struct EmissorOpus {
    seq: u16,
    fila: mpsc::Sender<(Packet, usize)>,
    contadores: Arc<Contadores>,
}

impl EmissorOpus {
    /// Enfileira um quadro de Opus. `amostras` é o carimbo do encoder.
    pub fn envia(&mut self, quadro: &[u8], amostras: i64) {
        if quadro.is_empty() {
            return;
        }
        let pacote = Packet {
            header: Header {
                version: 2,
                // Cada quadro de Opus é uma unidade completa, então todos
                // fecham. É o que o RFC 7587 pede.
                marker: true,
                sequence_number: self.seq,
                // Trunca de propósito: o timestamp RTP é de 32 bits e dá a
                // volta, o que o receptor espera.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                timestamp: amostras as u64 as u32,
                // `payload_type` e `ssrc` são reescritos pela track com os
                // valores negociados — ver o comentário sobre payload type em
                // [`publica`].
                ..Default::default()
            },
            payload: bytes::Bytes::copy_from_slice(quadro),
        };
        self.seq = self.seq.wrapping_add(1);
        let tamanho = pacote.payload.len() + 12;
        if self.fila.try_send((pacote, tamanho)).is_err() {
            self.contadores.fila_cheia.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Uma sessão WHIP ativa. Enquanto este valor viver, as tarefas de repasse
/// seguem rodando no runtime do tokio.
pub struct Publicacao {
    pc: Arc<RTCPeerConnection>,
    /// URL do recurso devolvida no header `Location` do 201. É nela que o
    /// `DELETE` encerra a sessão do lado do servidor (WHIP, RFC 9725 §4.2).
    recurso: Option<String>,
    http: reqwest::Client,
    contadores: Arc<Contadores>,
    tarefas: Vec<JoinHandle<()>>,
    /// Taxa que o espaçador de pacotes persegue, em kbps.
    ///
    /// Compartilhada e mutável porque trocar o bitrate reinicia o ffmpeg mas
    /// NÃO esta sessão: sem atualizá-la, o espaçador continuaria mirando a
    /// taxa antiga. Mirar alto demais devolve a rajada que ele existe para
    /// conter; mirar baixo demais estrangula o encoder novo.
    kbps_video: Arc<AtomicU32>,
    /// Por onde os quadros dos nossos encoders entram.
    ///
    /// `Option` porque só existem no caminho em processo; no do `ffmpeg.exe`
    /// quem alimenta as filas é a leitura dos sockets.
    emissor: Option<EmissorH264>,
    emissor_audio: Option<EmissorOpus>,
    /// Alguém pediu um quadro-chave (PLI). Ver [`Publicacao::pediram_chave`].
    chave_pedida: Arc<AtomicBool>,
}

/// Procura duas portas UDP livres para o RTP de vídeo e o de áudio.
///
/// Cada mídia precisa de um **par** `(p, p+1)` com `p` par: o protocolo RTP do
/// ffmpeg também abre RTCP em `p+1` (convenção do RFC 3550), e devolver uma
/// porta cujo vizinho esteja ocupado faria o ffmpeg falhar com uma mensagem
/// pouco óbvia.
///
/// As portas candidatas ficam ligadas até o fim da busca — é o que garante que
/// as duas mídias não recebam a mesma porta. Sobra uma janela de corrida entre
/// soltá-las aqui e [`publica`] ligá-las de fato; na prática é irrelevante numa
/// máquina de usuário, e o custo de eliminá-la (passar sockets já ligados pela
/// API) não se paga.
pub fn portas_livres() -> Result<(u16, u16)> {
    // Segura tudo o que foi testado, inclusive o que deu certo, para a segunda
    // busca não reencontrar a mesma porta. Solto de uma vez no fim.
    let mut reservas: Vec<std::net::UdpSocket> = Vec::new();
    let mut achadas: Vec<u16> = Vec::new();

    for _ in 0..128 {
        let s = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .context("procurar porta UDP livre")?;
        let porta = s.local_addr().context("ler porta local")?.port();
        reservas.push(s);
        if porta % 2 != 0 || porta == u16::MAX {
            continue;
        }
        if let Ok(vizinho) = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, porta + 1)) {
            reservas.push(vizinho);
            achadas.push(porta);
            if achadas.len() == 2 {
                return Ok((achadas[0], achadas[1]));
            }
        }
    }
    anyhow::bail!("não achei dois pares de portas UDP livres em 128 tentativas")
}

/// Liga o par RTP/RTCP de uma mídia em 127.0.0.1.
///
/// O socket de RTCP é ligado mesmo sem uso: o ffmpeg envia Sender Reports para
/// `porta+1` e, se ninguém estiver escutando, o Windows devolve ICMP port
/// unreachable — o que faz um socket UDP conectado passar a errar com
/// `WSAECONNRESET`. Ligar e descartar é mais barato que depurar isso.
async fn liga_par(porta: u16, rotulo: &str) -> Result<(UdpSocket, UdpSocket)> {
    let rtp = UdpSocket::bind((Ipv4Addr::LOCALHOST, porta))
        .await
        .with_context(|| format!("ligar porta RTP de {rotulo} em 127.0.0.1:{porta}"))?;
    let rtcp = UdpSocket::bind((Ipv4Addr::LOCALHOST, porta + 1))
        .await
        .with_context(|| format!("ligar porta RTCP de {rotulo} em 127.0.0.1:{}", porta + 1))?;
    Ok((rtp, rtcp))
}

/// Negocia a sessão WHIP e começa a repassar RTP.
///
/// Devolve quando a conexão se estabelece; a publicação segue em tarefas de
/// fundo, presas ao [`Publicacao`] devolvido.
pub async fn publica(cfg: Config<'_>) -> Result<Publicacao> {
    // Ligar os sockets ANTES de qualquer coisa de rede: se a porta estiver
    // ocupada, falhar aqui é muito mais claro do que falhar depois de já ter
    // criado uma sessão no servidor que ninguém vai encerrar.
    let (video_rtp, video_rtcp) = liga_par(cfg.porta_video, "vídeo").await?;
    let (audio_rtp, audio_rtcp) = liga_par(cfg.porta_audio, "áudio").await?;

    let mut m = MediaEngine::default();
    m.register_default_codecs()
        .context("registrar codecs padrão")?;
    let registry =
        register_default_interceptors(Registry::new(), &mut m).context("registrar interceptors")?;

    let mut se = SettingEngine::default();
    // Candidatos mDNS (`*.local`) só servem para esconder o IP de um par na
    // mesma rede local. Um servidor WHIP não resolve `.local`, então publicar
    // esses candidatos só atrasa o ICE. Desligado.
    se.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);

    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_interceptor_registry(registry)
        .with_setting_engine(se)
        .build();

    // Sem servidores STUN: quem publica é sempre o lado ativo, e o servidor
    // WHIP tem IP público e oferece os candidatos dele. Candidatos host nossos
    // bastam para o handshake sair pela NAT. Configurar STUN aqui só somaria
    // latência ao gathering — que é bloqueante, porque WHIP é vanilla ICE.
    let pc = Arc::new(
        api.new_peer_connection(RTCConfiguration::default())
            .await
            .context("criar RTCPeerConnection")?,
    );

    // O `fmtp` do H.264 precisa dizer o que realmente mandamos.
    //
    // Antes ficava vazio, para o casamento de codec cair no mime type e não
    // depender do encoder disponível. Parecia inofensivo e não era: pelo
    // RFC 6184, `packetization-mode` AUSENTE vale 0 — um NAL por pacote, sem
    // fragmentação. Só que keyframe de 1080p sempre sai fragmentado em FU-A.
    // Negociávamos "não fragmento" e mandávamos fragmentado.
    //
    // O sintoma foi cruel de diagnosticar porque nada acusa erro: o servidor
    // recebe tudo, não há perda, o bitrate está certo, e o navegador acumula
    // megabytes com `framesReceived: 0` — pacotes que nunca viram quadro. Foi
    // medido assim: 9,4 MB recebidos, zero perdidos, zero quadros montados,
    // `codec: undefined`.
    //
    // `42e01f` é Constrained Baseline 3.1, que é justamente o que o `tuning`
    // passou a forçar em todos os encoders — as duas coisas têm de concordar,
    // e é por isso que a objeção antiga ("falharia com o encoder errado") não
    // vale mais: não existe mais encoder produzindo outro profile.
    let track_video = Arc::new(TrackLocalStaticRTP::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.to_owned(),
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                .to_owned(),
            ..Default::default()
        },
        "video".to_owned(),
        "tela".to_owned(),
    ));
    let track_audio = Arc::new(TrackLocalStaticRTP::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: 48_000,
            channels: 2,
            ..Default::default()
        },
        "audio".to_owned(),
        "tela".to_owned(),
    ));

    // `sendonly` e não `sendrecv`: nós só publicamos. Um transceiver sendrecv
    // faria o servidor reservar recursos de recepção à toa.
    let init = || {
        Some(RTCRtpTransceiverInit {
            direction: RTCRtpTransceiverDirection::Sendonly,
            send_encodings: Vec::new(),
        })
    };
    let tr_video = pc
        .add_transceiver_from_track(
            Arc::clone(&track_video) as Arc<dyn TrackLocal + Send + Sync>,
            init(),
        )
        .await
        .context("adicionar transceiver de vídeo")?;
    let tr_audio = pc
        .add_transceiver_from_track(
            Arc::clone(&track_audio) as Arc<dyn TrackLocal + Send + Sync>,
            init(),
        )
        .await
        .context("adicionar transceiver de áudio")?;

    let offer = pc.create_offer(None).await.context("criar offer")?;

    // A promessa precisa ser pega ANTES do set_local_description, senão o
    // gathering pode terminar antes de alguém estar escutando e a espera
    // abaixo nunca acorda.
    let mut gathering = pc.gathering_complete_promise().await;
    pc.set_local_description(offer)
        .await
        .context("set_local_description")?;

    // WHIP é **vanilla ICE**: o SDP vai numa única requisição HTTP, sem canal
    // de sinalização para trickle. Por isso esperamos o gathering terminar —
    // mandar a offer antes significaria mandar um SDP sem candidato nenhum, e
    // o servidor não teria para onde responder.
    if tokio::time::timeout(ESPERA_GATHERING, gathering.recv())
        .await
        .is_err()
    {
        // Não é fatal: os candidatos host já coletados costumam bastar.
        eprintln!(
            "aviso: ICE gathering não completou em {ESPERA_GATHERING:?}, seguindo com o que há"
        );
    }

    let local = pc
        .local_description()
        .await
        .context("offer local sumiu depois do gathering")?;

    let http = reqwest::Client::builder()
        .build()
        .context("construir cliente HTTP")?;

    let resp = http
        .post(cfg.whip_url)
        .header(reqwest::header::CONTENT_TYPE, "application/sdp")
        .body(local.sdp)
        .send()
        .await
        .with_context(|| format!("POST WHIP em {}", cfg.whip_url))?;

    let status = resp.status();
    // O `Location` pode vir relativo; resolvê-lo contra a URL da requisição é
    // o que a RFC manda e o que o MediaMTX de fato devolve.
    let recurso = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|loc| resp.url().join(loc).ok())
        .map(|u| u.to_string());
    let corpo = resp.text().await.context("ler corpo da resposta WHIP")?;

    if status != reqwest::StatusCode::CREATED && !status.is_success() {
        anyhow::bail!("servidor WHIP respondeu {status}: {}", corpo.trim());
    }
    anyhow::ensure!(
        !corpo.trim().is_empty(),
        "servidor WHIP respondeu {status} sem SDP no corpo"
    );

    let answer = RTCSessionDescription::answer(corpo).context("parsear answer do servidor")?;
    pc.set_remote_description(answer)
        .await
        .context("set_remote_description")?;

    let contadores = Arc::new(Contadores::default());

    // # Payload type
    //
    // O ffmpeg emite com o PT que mandamos na linha de comando (96 e 111), mas
    // o PT negociado no SDP pode ser outro — quem escolhe é a answer do
    // servidor. Enviar RTP com o PT errado faz o vídeo simplesmente não
    // aparecer, sem erro visível em lugar nenhum.
    //
    // A boa notícia é que a `webrtc` 0.17 já resolve isso por dentro:
    // `TrackLocalStaticRTP::write_rtp_with_extensions_attributes` sobrescreve
    // `header.payload_type` (e o `ssrc`) com os valores do binding negociado
    // antes de encriptar. Reescrever aqui seria redundante e, pior, mascararia
    // uma eventual regressão nesse comportamento. O que fazemos é **ler** o PT
    // negociado e expô-lo em `Estatisticas`, para que um humano consiga
    // conferir num relance que a negociação saiu como esperado.
    let sender_video = tr_video.sender().await;
    let sender_audio = tr_audio.sender().await;
    contadores.pt_video.store(
        pt_negociado(&sender_video, MIME_TYPE_H264).await,
        Ordering::Relaxed,
    );
    contadores.pt_audio.store(
        pt_negociado(&sender_audio, MIME_TYPE_OPUS).await,
        Ordering::Relaxed,
    );

    let kbps_video = Arc::new(AtomicU32::new(cfg.kbps_video));
    let chave_pedida = Arc::new(AtomicBool::new(false));
    let (env_video, rec_video) = mpsc::channel(FILA_PACOTES);
    let (env_audio, rec_audio) = mpsc::channel(FILA_PACOTES);

    // Encodando em processo, o vídeo entra pelo emissor e ninguém escuta a
    // porta UDP; no caminho do `ffmpeg.exe`, o contrário.
    let emissor = cfg.em_processo.then(|| EmissorH264 {
        payloader: H264Payloader::default(),
        seq: 0,
        fila: env_video.clone(),
        contadores: Arc::clone(&contadores),
        inicio: None,
        ultimo_ts: None,
        sps: Vec::new(),
        pps: Vec::new(),
    });
    let emissor_audio = cfg.em_processo.then(|| EmissorOpus {
        seq: 0,
        fila: env_audio.clone(),
        contadores: Arc::clone(&contadores),
    });

    let mut tarefas = vec![
        tokio::spawn(tarefa_envio(
            rec_video,
            track_video,
            Arc::clone(&contadores),
            Midia::Video,
            Some(Pacer::novo(Arc::clone(&kbps_video))),
        )),
        // Áudio não é espaçado: o Opus já sai em quadros de 20 ms, ou seja
        // naturalmente distribuído no tempo, e são ~120 kbps contra vários
        // Mbps do vídeo. Espaçar só adicionaria latência sem tirar rajada
        // nenhuma do enlace.
        tokio::spawn(tarefa_envio(
            rec_audio,
            track_audio,
            Arc::clone(&contadores),
            Midia::Audio,
            None,
        )),
        tokio::spawn(tarefa_rtcp(
            sender_video,
            Arc::clone(&contadores),
            Midia::Video,
            Arc::clone(&chave_pedida),
        )),
        tokio::spawn(tarefa_rtcp(
            sender_audio,
            Arc::clone(&contadores),
            Midia::Audio,
            Arc::clone(&chave_pedida),
        )),
    ];

    // Só há o que ler das portas quando é o `ffmpeg.exe` que encoda.
    if !cfg.em_processo {
        tarefas.push(tokio::spawn(tarefa_leitura(
            video_rtp,
            env_video,
            Arc::clone(&contadores),
            Midia::Video,
        )));
        tarefas.push(tokio::spawn(tarefa_leitura(
            audio_rtp,
            env_audio,
            Arc::clone(&contadores),
            Midia::Audio,
        )));
        tarefas.push(tokio::spawn(tarefa_descarte(video_rtcp)));
        tarefas.push(tokio::spawn(tarefa_descarte(audio_rtcp)));
    }

    let publicacao = Publicacao {
        emissor,
        emissor_audio,
        chave_pedida,
        kbps_video: Arc::clone(&kbps_video),
        pc: Arc::clone(&pc),
        recurso,
        http,
        contadores,
        tarefas,
    };

    espera_conexao(&pc).await?;
    Ok(publicacao)
}

/// Descobre o payload type que a negociação fixou para um codec.
async fn pt_negociado(sender: &Arc<RTCRtpSender>, mime: &str) -> u8 {
    let params = sender.get_parameters().await;
    params
        .rtp_parameters
        .codecs
        .iter()
        .find(|c| c.capability.mime_type.eq_ignore_ascii_case(mime))
        .map(|c| c.payload_type)
        .unwrap_or(0)
}

/// Bloqueia até o `RTCPeerConnection` chegar a `Connected`, ou falhar.
async fn espera_conexao(pc: &Arc<RTCPeerConnection>) -> Result<()> {
    let (tx, mut rx) = tokio::sync::watch::channel(pc.connection_state());
    pc.on_peer_connection_state_change(Box::new(move |estado: RTCPeerConnectionState| {
        // Falha de envio no watch só acontece se ninguém mais escuta, que é
        // exatamente o caso depois que a espera termina. Ignorar é correto.
        let _ = tx.send(estado);
        Box::pin(async {})
    }));

    let espera = async {
        loop {
            match *rx.borrow_and_update() {
                RTCPeerConnectionState::Connected => return Ok(()),
                RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => {
                    anyhow::bail!("conexão WebRTC falhou durante o handshake (ICE ou DTLS)")
                }
                _ => {}
            }
            if rx.changed().await.is_err() {
                anyhow::bail!("monitor de estado da conexão encerrou inesperadamente");
            }
        }
    };

    match tokio::time::timeout(ESPERA_CONEXAO, espera).await {
        Ok(r) => r,
        Err(_) => anyhow::bail!("conexão WebRTC não estabeleceu em {ESPERA_CONEXAO:?}"),
    }
}

/// Qual contador incrementar.
#[derive(Clone, Copy)]
enum Midia {
    Video,
    Audio,
}

/// Guarda os parâmetros da imagem e os reinsere antes de cada quadro-chave.
///
/// # Por que isto existe
///
/// Um decodificador H.264 não monta imagem nenhuma sem SPS e PPS. Encoders
/// bem-comportados os repetem em todo quadro-chave — medido no RTP real:
/// `libx264` manda SPS=5/PPS=5 em 25 keyframes. O `h264_amf` os manda **uma
/// vez, no começo, e nunca mais**.
///
/// O efeito é cruel de diagnosticar porque nada acusa erro: o servidor recebe
/// tudo, não há perda, o bitrate está no alvo. Quem já estava assistindo vê
/// normalmente; quem chega depois — ou dá F5, ou perde um pacote e precisa
/// recomeçar — acumula megabytes sem montar um único quadro, pedindo
/// keyframe para sempre. Medido no navegador de quem assistia: 37 MB, zero
/// perdidos, zero quadros, 486 PLIs.
///
/// # Por que aqui e não no ffmpeg
///
/// O filtro `dump_extra` é a solução citada em todo lugar e **não funciona
/// nesta saída**: medido, ele reinsere os cabeçalhos numa saída de arquivo e
/// não numa de RTP. Cabeçalhos dentro do fluxo sobrevivem ao muxer; injetados
/// pelo filtro, não. Fazer aqui vale para qualquer encoder, hoje e amanhã, e
/// é o que os SFUs fazem.
#[derive(Default)]
struct Parametros {
    /// Um pacote só que já carrega SPS E PPS (agregado num STAP-A).
    ///
    /// É o caso comum: o muxer RTP do ffmpeg agrega os dois. A primeira versão
    /// disto guardava esse pacote como se fosse só o SPS e nunca preenchia o
    /// PPS — e como a reinserção exigia os dois, ela nunca acontecia. O log de
    /// quem transmitia mostrou `params 1+0`: um conjunto visto, zero
    /// reinserções, tela preta igual.
    juntos: Option<Packet>,
    sps: Option<Packet>,
    pps: Option<Packet>,
}

impl Parametros {
    /// Registra o pacote se ele carregar SPS ou PPS.
    /// Registra o pacote se ele carregar SPS ou PPS. Devolve se era SPS.
    fn observa(&mut self, pacote: &Packet) -> bool {
        // Agregado com SPS e PPS juntos: guardar o pacote inteiro basta, e é o
        // que o ffmpeg manda na prática.
        if agrega_sps_e_pps(&pacote.payload) {
            self.juntos = Some(pacote.clone());
            return true;
        }
        match tipo_nal(&pacote.payload) {
            Some(7) => {
                self.sps = Some(pacote.clone());
                true
            }
            Some(8) => {
                self.pps = Some(pacote.clone());
                false
            }
            _ => false,
        }
    }

    /// Os pacotes a mandar ANTES deste, se ele começa um quadro-chave que não
    /// traz os parâmetros consigo.
    fn antes_de(&self, pacote: &Packet) -> Vec<Packet> {
        if !comeca_idr(&pacote.payload) || traz_parametros(&pacote.payload) {
            return Vec::new();
        }
        // Um pacote agregado já resolve; se vieram separados, precisa dos dois.
        let base: Vec<&Packet> = if let Some(j) = self.juntos.as_ref() {
            vec![j]
        } else if let (Some(sps), Some(pps)) = (self.sps.as_ref(), self.pps.as_ref()) {
            vec![sps, pps]
        } else {
            // Nunca vimos os parâmetros: não há o que reinserir. Acontece antes
            // do primeiro quadro-chave e é normal.
            return Vec::new();
        };
        base.into_iter()
            .map(|base| {
                let mut p = base.clone();
                // Mesmo instante do quadro que eles descrevem, e sem marca de
                // fim de quadro: eles são o começo dele, não o fim.
                p.header.timestamp = pacote.header.timestamp;
                p.header.marker = false;
                p
            })
            .collect()
    }
}

/// Tipo do NAL que um payload RTP de H.264 carrega, desembrulhando agregação
/// (STAP-A) e fragmentação (FU-A).
fn tipo_nal(payload: &[u8]) -> Option<u8> {
    match payload.first()? & 0x1F {
        // STAP-A: vários NALs num pacote. O primeiro começa no byte 3, depois
        // do cabeçalho de 1 byte e do tamanho de 2.
        24 => payload.get(3).map(|b| b & 0x1F),
        // FU-A: fragmento. O tipo real está no segundo byte.
        28 => payload.get(1).map(|b| b & 0x1F),
        outro => Some(outro),
    }
}

/// O pacote INICIA um quadro-chave.
///
/// Num FU-A só o primeiro fragmento conta: reinserir os parâmetros no meio de
/// um quadro fragmentado o corromperia em vez de consertar.
fn comeca_idr(payload: &[u8]) -> bool {
    if payload.first().is_some_and(|b| (b & 0x1F) == 28) {
        return payload
            .get(1)
            .is_some_and(|b| (b & 0x80) != 0 && (b & 0x1F) == 5);
    }
    tipo_nal(payload) == Some(5)
}

/// O pacote agrega SPS E PPS no mesmo STAP-A.
///
/// Distinto de [`traz_parametros`] de propósito: aquele responde "dispensa
/// reinserção?", este responde "serve sozinho como parâmetros?". Confundir os
/// dois foi o que fez a reinserção nunca disparar.
fn agrega_sps_e_pps(payload: &[u8]) -> bool {
    let (mut tem_sps, mut tem_pps) = (false, false);
    for t in nals_agregados(payload) {
        match t {
            7 => tem_sps = true,
            8 => tem_pps = true,
            _ => {}
        }
    }
    tem_sps && tem_pps
}

/// Tipos dos NALs dentro de um STAP-A. Vazio se não for um.
fn nals_agregados(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    if payload.first().is_none_or(|b| (b & 0x1F) != 24) {
        return out;
    }
    let mut i = 1usize;
    while i + 2 <= payload.len() {
        let tam = usize::from(u16::from_be_bytes([payload[i], payload[i + 1]]));
        i += 2;
        // Tamanho declarado tem de caber no que chegou: agregado truncado é
        // dado malformado, e dado malformado não afirma nada.
        if tam == 0 || i + tam > payload.len() {
            break;
        }
        out.push(payload[i] & 0x1F);
        i += tam;
    }
    out
}

/// O pacote já carrega SPS, e portanto dispensa reinserção.
///
/// Um STAP-A costuma agregar SPS+PPS+IDR de uma vez; duplicá-los seria
/// desperdício de banda em todo quadro-chave.
fn traz_parametros(payload: &[u8]) -> bool {
    if payload.first().is_some_and(|b| (b & 0x1F) != 24) {
        return tipo_nal(payload) == Some(7);
    }
    // Percorre os NALs agregados procurando um SPS.
    let mut i = 1usize;
    while i + 2 <= payload.len() {
        let tam = usize::from(u16::from_be_bytes([payload[i], payload[i + 1]]));
        i += 2;
        // O tamanho declarado tem de caber no que chegou. Um agregado truncado
        // é dado malformado, e dado malformado NÃO pode afirmar que os
        // parâmetros vieram junto — a resposta segura é reinserir, não confiar.
        if tam == 0 || i + tam > payload.len() {
            break;
        }
        if payload[i] & 0x1F == 7 {
            return true;
        }
        i += tam;
    }
    false
}

/// Lê datagramas RTP de um socket e os injeta na track correspondente.
/// Lê o socket sem parar e empurra os pacotes para a fila.
///
/// Fica separada do envio de propósito. Se o espaçamento (ver [`Pacer`])
/// acontecesse aqui dentro, o socket deixaria de ser drenado enquanto
/// esperamos, o buffer UDP do sistema encheria, e a perda voltaria — só que
/// agora antes mesmo de enxergarmos o pacote. Ler rápido e segurar na nossa
/// própria fila deixa o descarte sob nosso controle e contabilizado.
async fn tarefa_leitura(
    sock: UdpSocket,
    fila: mpsc::Sender<(Packet, usize)>,
    contadores: Arc<Contadores>,
    midia: Midia,
) {
    let mut buf = vec![0u8; MTU_RECEPCAO];
    let mut parametros = Parametros::default();
    // Numeração própria de sequência e timestamp. Ver [`Continuidade`]: é o
    // que permite trocar a taxa reiniciando só o ffmpeg, sem quem assiste
    // perder a imagem nem a sala criar uma stream nova.
    let mut cont = Continuidade::nova(match midia {
        Midia::Video => RELOGIO_VIDEO,
        Midia::Audio => RELOGIO_AUDIO,
    });
    loop {
        let n = match sock.recv(&mut buf).await {
            Ok(n) => n,
            // Socket morto: o processo está encerrando. Sair em silêncio.
            Err(_) => return,
        };

        let mut fatia = &buf[..n];
        let pacote = match Packet::unmarshal(&mut fatia) {
            Ok(p) => p,
            Err(_) => {
                contadores.descartados.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };

        // Calculado UMA vez por pacote recebido, antes da reinserção: os
        // pacotes de parâmetro copiam o timestamp do quadro que descrevem, e
        // todos eles têm de sair com o mesmo valor já corrigido.
        let (ts_saida, reiniciou) = cont.timestamp(pacote.header.timestamp, Instant::now());

        // Encoder novo pode vir com resolução nova, e o SPS guardado descreve a
        // antiga. Reinjetar dimensões que não batem com o quadro é pior que
        // não reinjetar nada: o decodificador monta a imagem errada em vez de
        // esperar o parâmetro certo, que vem logo atrás no primeiro IDR.
        if reiniciou {
            parametros = Parametros::default();
        }

        // Áudio passa direto: Opus não tem parâmetros fora de banda.
        let saida = match midia {
            Midia::Audio => vec![pacote],
            Midia::Video => {
                if parametros.observa(&pacote) {
                    contadores.parametros_vistos.fetch_add(1, Ordering::Relaxed);
                }
                let mut v = parametros.antes_de(&pacote);
                if !v.is_empty() {
                    contadores
                        .parametros_reinseridos
                        .fetch_add(1, Ordering::Relaxed);
                }
                v.push(pacote);
                v
            }
        };

        for mut p in saida {
            // Sequência e timestamp vêm da nossa contagem, nunca da do ffmpeg.
            //
            // No vídeo a renumeração já era obrigatória por causa da
            // reinserção de parâmetros, que insere pacotes no meio e abriria
            // buracos — e buraco de sequência é lido como perda do outro lado.
            // Agora vale também para o áudio, e passa a cobrir o timestamp:
            // é o que deixa o encoder reiniciar sem quem assiste notar.
            p.header.sequence_number = cont.seq(p.header.sequence_number);
            p.header.timestamp = ts_saida;
            // `try_send` em vez de `send`: se a fila encheu, o envio não está
            // dando conta e esperar aqui só encheria o buffer do sistema em
            // seguida. Descartar agora, contabilizado, é o comportamento
            // honesto.
            let tamanho = p.payload.len() + 12;
            if fila.try_send((p, tamanho)).is_err() {
                contadores.fila_cheia.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Esvazia a fila na track, espaçando os pacotes quando há um [`Pacer`].
async fn tarefa_envio(
    mut fila: mpsc::Receiver<(Packet, usize)>,
    track: Arc<TrackLocalStaticRTP>,
    contadores: Arc<Contadores>,
    midia: Midia,
    mut pacer: Option<Pacer>,
) {
    while let Some((pacote, n)) = fila.recv().await {
        if let Some(p) = pacer.as_mut() {
            p.aguarda(n).await;
        }

        if track.write_rtp(&pacote).await.is_err() {
            // Esperado enquanto o DTLS ainda não subiu, ou depois do encerramento.
            contadores.falhas_envio.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        let (pacotes, bytes) = match midia {
            Midia::Video => (&contadores.pacotes_video, &contadores.bytes_video),
            Midia::Audio => (&contadores.pacotes_audio, &contadores.bytes_audio),
        };
        pacotes.fetch_add(1, Ordering::Relaxed);
        bytes.fetch_add(n as u64, Ordering::Relaxed);
    }
}

/// Consome e descarta o RTCP que o ffmpeg emite (ver [`liga_par`]).
async fn tarefa_descarte(sock: UdpSocket) {
    let mut buf = vec![0u8; MTU_RECEPCAO];
    while sock.recv(&mut buf).await.is_ok() {}
}

/// Drena o RTCP que o servidor manda de volta.
///
/// Ler é obrigatório: é esse `read` que roda a cadeia de interceptors
/// (NACK, sender reports, TWCC). Sem alguém lendo, essa cadeia não avança.
///
/// **Não** tentamos forçar keyframe ao receber PLI. O ffmpeg roda como
/// subprocesso, alimentado por pipes de bytes crus; não há canal para pedir
/// um IDR sob demanda (`force_key_frames` é decidido na linha de comando, e
/// o `-` do stdin já está tomado). O que compensa isso é o GOP de 2 s em
/// `ffmpeg::spawn`: quem entra no meio da transmissão espera no máximo 2 s
/// por um keyframe. Aqui só contamos os PLIs — se esse número subir muito, é
/// sinal de perda de pacote no caminho, e é isso que a estatística serve para
/// mostrar.
async fn tarefa_rtcp(
    sender: Arc<RTCRtpSender>,
    contadores: Arc<Contadores>,
    midia: Midia,
    chave_pedida: Arc<AtomicBool>,
) {
    let mut buf = vec![0u8; MTU_RECEPCAO];
    while let Ok((pacotes, _)) = sender.read(&mut buf).await {
        for p in pacotes {
            if p.as_any().downcast_ref::<PictureLossIndication>().is_some() {
                contadores.plis.fetch_add(1, Ordering::Relaxed);
                // Encodando em processo dá para ATENDER o pedido, e não só
                // contá-lo: quem acabou de entrar na sala recebe imagem no
                // quadro seguinte em vez de esperar o próximo GOP.
                if matches!(midia, Midia::Video) {
                    chave_pedida.store(true, Ordering::Relaxed);
                }
            }

            // Um NACK carrega VÁRIOS números de sequência; contar o pacote
            // subestimaria o estrago. O que interessa é quantos quadros de
            // dados o servidor deu por perdidos e pediu de volta.
            if let Some(n) = p.as_any().downcast_ref::<TransportLayerNack>() {
                let perdidos: u64 = n
                    .nacks
                    .iter()
                    .map(|par| 1 + u64::from(par.lost_packets.count_ones()))
                    .sum();
                contadores.nacks.fetch_add(perdidos, Ordering::Relaxed);
            }

            // Receiver Report: é aqui que o servidor conta o que NÃO chegou.
            //
            // Sem isto o app só enxerga o que ele mesmo descartou, e por isso
            // mostrava "0 perdidos" enquanto quem assistia via tela preta — a
            // perda acontecia no caminho, entre nós e o servidor, e nada no
            // app sabia disso.
            if let Some(rr) = p.as_any().downcast_ref::<ReceiverReport>() {
                contadores.relatorios.fetch_add(1, Ordering::Relaxed);
                // Só o vídeo alimenta o sinal de congestionamento.
                //
                // Antes áudio e vídeo escreviam nos mesmos contadores, e um
                // relatório de áudio limpo — que é quase sempre, porque são
                // ~120 kbps contra vários Mbps — apagava a perda do vídeo que
                // tinha acabado de chegar.
                if !matches!(midia, Midia::Video) {
                    continue;
                }
                for r in &rr.reports {
                    // `fraction_lost` é 0..255 desde o relatório anterior.
                    // Guardamos como por mil para distinguir 0,4% de 1,2%.
                    let por_mil = u64::from(r.fraction_lost) * 1000 / 256;
                    // Guarda o PIOR desde a última leitura, não o mais recente.
                    contadores
                        .perda_por_mil
                        .fetch_max(por_mil, Ordering::Relaxed);
                    contadores
                        .perdidos_no_caminho
                        .store(u64::from(r.total_lost), Ordering::Relaxed);
                    // O jitter vem em unidades do relógio RTP; o vídeo usa
                    // 90 kHz, então dividir por 90 dá milissegundos.
                    contadores
                        .jitter_ms
                        .store(u64::from(r.jitter) / 90, Ordering::Relaxed);
                }
            }
        }
    }
}

impl Publicacao {
    /// Avisa o espaçador de pacotes que a taxa mudou.
    ///
    /// Chamado quando o encoder é reiniciado com bitrate novo. Ver
    /// [`Publicacao::kbps_video`].
    pub fn ajusta_taxa(&self, kbps: u32) {
        self.kbps_video.store(kbps, Ordering::Relaxed);
    }

    /// Retira o emissor de vídeo, para quem for alimentá-lo com os quadros.
    ///
    /// Retira em vez de emprestar porque quem consome roda numa thread
    /// própria, e ninguém mais tem o que fazer com ele. Devolve `None` no
    /// caminho do `ffmpeg.exe`, e também na segunda chamada.
    pub fn toma_emissor(&mut self) -> Option<EmissorH264> {
        self.emissor.take()
    }

    /// O mesmo para o áudio. Ver [`Publicacao::toma_emissor`].
    pub fn toma_emissor_audio(&mut self) -> Option<EmissorOpus> {
        self.emissor_audio.take()
    }

    /// Alguém pediu um quadro-chave desde a última vez que se perguntou.
    ///
    /// Consome o pedido: dois PLIs entre uma pergunta e outra viram um
    /// keyframe só, que é o certo — mandar dois seguidos gastaria banda para
    /// nada.
    pub fn pediram_chave(&self) -> bool {
        self.chave_pedida.swap(false, Ordering::Relaxed)
    }

    /// Estatísticas do transporte, para a UI (Plano 2) e para o diagnóstico.
    pub async fn estatisticas(&self) -> Estatisticas {
        let c = &self.contadores;
        Estatisticas {
            tempo_torto: c.tempo_torto.load(Ordering::Relaxed),
            estado: self.pc.connection_state(),
            pacotes_video: c.pacotes_video.load(Ordering::Relaxed),
            bytes_video: c.bytes_video.load(Ordering::Relaxed),
            pacotes_audio: c.pacotes_audio.load(Ordering::Relaxed),
            bytes_audio: c.bytes_audio.load(Ordering::Relaxed),
            descartados: c.descartados.load(Ordering::Relaxed),
            falhas_envio: c.falhas_envio.load(Ordering::Relaxed),
            fila_cheia: c.fila_cheia.load(Ordering::Relaxed),
            // Leitura DESTRUTIVA do pico: cada amostra reporta o pior desde
            // a anterior. Sem zerar, um estouro de dez segundos atrás
            // continuaria alarmando para sempre.
            perda_por_mil: c.perda_por_mil.swap(0, Ordering::Relaxed),
            jitter_ms: c.jitter_ms.load(Ordering::Relaxed),
            perdidos_no_caminho: c.perdidos_no_caminho.load(Ordering::Relaxed),
            parametros_vistos: c.parametros_vistos.load(Ordering::Relaxed),
            parametros_reinseridos: c.parametros_reinseridos.load(Ordering::Relaxed),
            relatorios: c.relatorios.load(Ordering::Relaxed),
            plis: c.plis.load(Ordering::Relaxed),
            nacks: c.nacks.load(Ordering::Relaxed),
            pt_video: c.pt_video.load(Ordering::Relaxed),
            pt_audio: c.pt_audio.load(Ordering::Relaxed),
        }
    }

    /// URL do recurso WHIP criado pelo servidor, se ele mandou `Location`.
    pub fn recurso(&self) -> Option<&str> {
        self.recurso.as_deref()
    }

    /// Encerra a sessão: para o repasse, avisa o servidor e fecha o transporte.
    pub async fn encerra(self) -> Result<()> {
        for t in &self.tarefas {
            t.abort();
        }

        // O DELETE é como o WHIP encerra a sessão. Sem ele o servidor mantém o
        // path publicado até algum timeout, e um `share` seguinte esbarra num
        // "already publishing".
        let mut erro_delete = None;
        if let Some(url) = &self.recurso {
            match self.http.delete(url).send().await {
                Ok(r) if !r.status().is_success() => {
                    erro_delete = Some(anyhow::anyhow!("DELETE em {url} respondeu {}", r.status()));
                }
                Err(e) => {
                    erro_delete = Some(anyhow::Error::new(e).context(format!("DELETE em {url}")))
                }
                Ok(_) => {}
            }
        }

        // Fechar a PeerConnection mesmo que o DELETE tenha falhado: soltar os
        // sockets locais não depende do servidor ter entendido o pedido.
        let fechamento = self.pc.close().await.context("fechar RTCPeerConnection");

        match (erro_delete, fechamento) {
            (Some(e), _) => Err(e),
            (None, r) => r,
        }
    }
}

#[cfg(test)]
mod testes_parametros {
    use super::*;

    fn pacote(payload: Vec<u8>, seq: u16, ts: u32) -> Packet {
        let mut p = Packet::default();
        p.header.sequence_number = seq;
        p.header.timestamp = ts;
        p.payload = payload.into();
        p
    }

    /// NAL solto: o tipo está nos 5 bits baixos do primeiro byte.
    #[test]
    fn le_nal_solto() {
        assert_eq!(tipo_nal(&[0x67, 0x42]), Some(7)); // SPS
        assert_eq!(tipo_nal(&[0x68, 0xCE]), Some(8)); // PPS
        assert_eq!(tipo_nal(&[0x65, 0x88]), Some(5)); // IDR
        assert_eq!(tipo_nal(&[0x41, 0x9A]), Some(1)); // P
        assert_eq!(tipo_nal(&[]), None);
    }

    /// FU-A: o tipo real mora no segundo byte, não no primeiro.
    #[test]
    fn le_fragmento() {
        // 0x7C = FU-A; 0x85 = início (bit S) + tipo 5 (IDR).
        assert_eq!(tipo_nal(&[0x7C, 0x85, 0x00]), Some(5));
        assert!(comeca_idr(&[0x7C, 0x85, 0x00]));
        // 0x05 = continuação (sem bit S) do mesmo IDR: NÃO começa quadro.
        assert!(!comeca_idr(&[0x7C, 0x05, 0x00]));
        // 0x45 = bit E (fim), também não é início.
        assert!(!comeca_idr(&[0x7C, 0x45, 0x00]));
    }

    /// Reinserir no meio de um quadro fragmentado o corromperia — este é o
    /// caso que mais importa, porque keyframe de 1080p sempre é fragmentado.
    #[test]
    fn so_o_primeiro_fragmento_dispara() {
        let mut par = Parametros::default();
        par.observa(&pacote(vec![0x67, 0x42], 1, 100));
        par.observa(&pacote(vec![0x68, 0xCE], 2, 100));

        let inicio = pacote(vec![0x7C, 0x85, 0x00], 3, 200);
        let meio = pacote(vec![0x7C, 0x05, 0x00], 4, 200);
        assert_eq!(
            par.antes_de(&inicio).len(),
            2,
            "o início do IDR precisa dos parâmetros"
        );
        assert_eq!(
            par.antes_de(&meio).len(),
            0,
            "o meio do IDR não pode receber nada"
        );
    }

    /// STAP-A que já agrega SPS dispensa reinserção.
    #[test]
    fn nao_duplica_quando_ja_vem_junto() {
        // STAP-A (24) com SPS(4 bytes) + IDR(2 bytes).
        let stap = vec![
            0x78, 0x00, 0x04, 0x67, 0x42, 0x00, 0x1F, 0x00, 0x02, 0x65, 0x88,
        ];
        assert!(traz_parametros(&stap));

        let mut par = Parametros::default();
        par.observa(&pacote(vec![0x67, 0x42], 1, 100));
        par.observa(&pacote(vec![0x68, 0xCE], 2, 100));
        assert_eq!(par.antes_de(&pacote(stap, 3, 200)).len(), 0);
    }

    /// O caso que passou batido em produção: o ffmpeg agrega SPS e PPS num
    /// STAP-A só, e a primeira versão guardava esse pacote como se fosse
    /// apenas o SPS. O PPS ficava vazio, a reinserção exigia os dois e nunca
    /// acontecia. O log de quem transmitia mostrou `params 1+0` — um conjunto
    /// visto, zero reinserções — e a tela continuou preta.
    #[test]
    fn sps_e_pps_agregados_bastam_sozinhos() {
        // STAP-A: [0x78][tam][SPS 4B][tam][PPS 3B]
        let stap = vec![
            0x78, 0x00, 0x04, 0x67, 0x42, 0x00, 0x1F, 0x00, 0x03, 0x68, 0xCE, 0x3C,
        ];
        assert!(agrega_sps_e_pps(&stap), "o agregado tem SPS e PPS");

        let mut par = Parametros::default();
        assert!(
            par.observa(&pacote(stap, 1, 100)),
            "tem de contar como visto"
        );

        // E agora a reinserção PRECISA disparar num IDR fragmentado.
        let idr = pacote(vec![0x7C, 0x85, 0x00], 2, 500);
        let antes = par.antes_de(&idr);
        assert_eq!(antes.len(), 1, "um pacote agregado basta: {antes:?}");
        assert_eq!(antes[0].header.timestamp, 500, "casa com o quadro");
        assert!(agrega_sps_e_pps(&antes[0].payload), "reinjeta os dois");
    }

    /// Um STAP-A que só tem SPS, sem PPS, não basta sozinho.
    #[test]
    fn agregado_incompleto_nao_basta() {
        let so_sps = vec![0x78, 0x00, 0x04, 0x67, 0x42, 0x00, 0x1F];
        assert!(!agrega_sps_e_pps(&so_sps));
    }

    /// Sem nunca ter visto os parâmetros, não há o que reinserir — e não pode
    /// entrar em pânico nem inventar pacote.
    #[test]
    fn sem_parametros_conhecidos_nao_inventa() {
        let par = Parametros::default();
        assert_eq!(par.antes_de(&pacote(vec![0x65, 0x88], 1, 100)).len(), 0);
    }

    /// Os reinseridos levam o instante do quadro que descrevem, e não a marca
    /// de fim de quadro — senão o outro lado fecharia o quadro cedo demais.
    #[test]
    fn reinseridos_herdam_o_instante_do_quadro() {
        let mut par = Parametros::default();
        let mut sps = pacote(vec![0x67, 0x42], 1, 100);
        sps.header.marker = true;
        par.observa(&sps);
        par.observa(&pacote(vec![0x68, 0xCE], 2, 100));

        let idr = pacote(vec![0x65, 0x88], 9, 777);
        let antes = par.antes_de(&idr);
        assert_eq!(antes.len(), 2);
        for p in &antes {
            assert_eq!(p.header.timestamp, 777, "tem de casar com o quadro");
            assert!(!p.header.marker, "não pode fechar o quadro");
        }
        assert_eq!(tipo_nal(&antes[0].payload), Some(7), "SPS primeiro");
        assert_eq!(tipo_nal(&antes[1].payload), Some(8), "PPS depois");
    }

    /// Quadro que não é chave passa intocado — reinserir em todo quadro
    /// gastaria banda sem motivo.
    #[test]
    fn quadro_comum_passa_intocado() {
        let mut par = Parametros::default();
        par.observa(&pacote(vec![0x67, 0x42], 1, 100));
        par.observa(&pacote(vec![0x68, 0xCE], 2, 100));
        assert_eq!(par.antes_de(&pacote(vec![0x41, 0x9A], 3, 200)).len(), 0);
    }

    /// Um STAP-A truncado não pode travar o laço nem estourar índice.
    #[test]
    fn stap_malformado_nao_quebra() {
        assert!(!traz_parametros(&[0x78]));
        assert!(!traz_parametros(&[0x78, 0x00]));
        assert!(!traz_parametros(&[0x78, 0x00, 0xFF, 0x67]));
        assert!(!traz_parametros(&[0x78, 0x00, 0x00, 0x67]));
    }
}

#[cfg(test)]
mod testes_medicao {
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Guardar o PICO, e não o último valor, é o que faz um estouro entre duas
    /// amostras não desaparecer. Foi assim que a telemetria marcou "perda
    /// 0/1000" enquanto o servidor contava 155 pacotes perdidos.
    #[test]
    fn o_pico_de_perda_sobrevive_ate_a_leitura() {
        let pico = AtomicU64::new(0);

        // Três relatórios entre duas amostras: um estouro no meio e calmaria
        // depois. Guardar o último daria zero.
        for por_mil in [0_u64, 140, 0] {
            pico.fetch_max(por_mil, Ordering::Relaxed);
        }

        assert_eq!(
            pico.swap(0, Ordering::Relaxed),
            140,
            "o estouro tem de aparecer"
        );
        // E a leitura zera: um estouro de dez segundos atrás não pode alarmar
        // para sempre.
        assert_eq!(pico.load(Ordering::Relaxed), 0, "a leitura tem de zerar");
    }

    /// Conversão do jitter: vem em unidades do relógio RTP, e o vídeo usa
    /// 90 kHz. Reportar o número cru seria 90 vezes maior que a realidade.
    #[test]
    fn jitter_vira_milissegundos() {
        let em_ms = |cru: u32| u64::from(cru) / 90;
        assert_eq!(em_ms(0), 0);
        assert_eq!(em_ms(90), 1, "90 unidades = 1 ms");
        assert_eq!(em_ms(2700), 30, "30 ms é onde começa a incomodar");
        assert_eq!(em_ms(9000), 100);
    }

    /// A fração vem em 0..255 e vira partes por mil. Precisa distinguir
    /// valores pequenos: 0,4% e 1,2% pedem reações diferentes.
    #[test]
    fn fracao_vira_partes_por_mil() {
        let por_mil = |f: u8| u64::from(f) * 1000 / 256;
        assert_eq!(por_mil(0), 0);
        assert_eq!(por_mil(255), 996, "perda quase total");
        assert!(por_mil(1) < por_mil(3), "precisa separar valores pequenos");
        // 1% de perda é ~2,56 em 0..255.
        assert_eq!(por_mil(3), 11);
    }
}

/// Mantém sequência e timestamp contínuos na saída quando o encoder reinicia.
///
/// # Por que isto existe
///
/// Mudar a taxa da transmissão exige reiniciar o ffmpeg: nenhum comando em
/// tempo de execução dele chega ao encoder — só aos filtros. Mas reiniciar o
/// ffmpeg NÃO precisa derrubar a transmissão: ele fala com a gente por UDP
/// local, e o socket, a conexão WebRTC e a stream na sala continuam de pé. A
/// task de leitura nem percebe a troca.
///
/// O que ela percebe é o efeito: um ffmpeg novo começa a numerar sequência e
/// timestamp de um ponto arbitrário. Sem costura, quem assiste vê a sequência
/// dar um salto (lido como perda maciça) e o timestamp voltar no tempo (o
/// buffer de jitter engasga ou zera). Esta struct absorve os dois.
///
/// O SSRC não entra na conta: a `webrtc` sobrescreve com o do binding
/// negociado antes de encriptar, então o do ffmpeg é irrelevante.
/// Silêncio no socket que denuncia o encoder tendo reiniciado.
///
/// Folgado em relação ao fluxo normal — que entrega a cada 16 ms no vídeo e
/// 20 ms no áudio — e curto em relação ao reinício, que leva de meio a dois
/// segundos entre matar o processo e o primeiro quadro do novo.
const PAUSA_DE_REINICIO: Duration = Duration::from_millis(300);

/// Relógio RTP do H.264, em ciclos por segundo. Fixado pela RFC 6184.
const RELOGIO_VIDEO: u32 = 90_000;
/// Relógio RTP do Opus: a taxa de amostragem que negociamos no SDP.
const RELOGIO_AUDIO: u32 = 48_000;

struct Continuidade {
    /// Ciclos por segundo do relógio RTP: 90000 para vídeo, 48000 para Opus.
    relogio: u32,
    proxima_seq: Option<u16>,
    ultimo_entrada: Option<u32>,
    /// Soma aplicada à entrada para chegar na saída.
    deslocamento: u32,
    ultimo_saida: u32,
    /// Quando o último pacote chegou, para medir de verdade quanto tempo a
    /// transmissão ficou parada durante a troca.
    ultimo_instante: Option<Instant>,
}

impl Continuidade {
    fn nova(relogio: u32) -> Self {
        Self {
            relogio,
            proxima_seq: None,
            ultimo_entrada: None,
            deslocamento: 0,
            ultimo_saida: 0,
            ultimo_instante: None,
        }
    }

    /// Timestamp de saída para um pacote que chegou com `entrada` em `agora`,
    /// e se este pacote veio de um encoder que acabou de reiniciar.
    ///
    /// O segundo valor importa porque o reinício pode trazer resolução nova, e
    /// os parâmetros de imagem guardados descrevem a antiga. Reinjetar um SPS
    /// com dimensões que não batem com o quadro é pior que não reinjetar nada.
    ///
    /// `agora` é injetável para o teste não depender do relógio real.
    fn timestamp(&mut self, entrada: u32, agora: Instant) -> (u32, bool) {
        // O sinal é o tempo de CHEGADA, não a aritmética do timestamp.
        //
        // A primeira versão comparava só os timestamps e perdia o caso do
        // áudio: um encoder novo começando em 3 quando o anterior estava em
        // 10960 dá uma distância de 10957, que num relógio de 48000 não chega
        // a um segundo e passava batido — o timestamp então andava para trás.
        //
        // Chegada é robusto porque num fluxo vivo pacote nunca para: o pump
        // repete o último quadro mesmo com a tela parada, e o áudio corre
        // sempre. Uma pausa de 300 ms só acontece com o encoder morrendo.
        //
        // E erra bem: se houver um engasgo de 300 ms SEM reinício, o
        // deslocamento é recalculado a partir do tempo real decorrido e dá
        // praticamente no mesmo valor de antes. Falso positivo é inofensivo.
        let gap = self
            .ultimo_instante
            .map_or(Duration::ZERO, |t| agora.duration_since(t));
        let recuou = self.ultimo_entrada.is_some_and(|ant| {
            // Tolerância de 100 ms: reordenação no socket local existe e não é
            // reinício. Um encoder novo cai muito além disso.
            (entrada.wrapping_sub(ant) as i32) < -(self.relogio as i32 / 10)
        });
        let reiniciou = gap > PAUSA_DE_REINICIO || recuou;

        if reiniciou {
            // O tempo parado é REAL e precisa aparecer no timestamp. Inventar
            // um passo fixo faria a linha do tempo encolher, o áudio e o vídeo
            // saírem de sincronia e o buffer de jitter receber quadros como se
            // tivessem sido produzidos mais rápido do que foram.
            let parado = self
                .ultimo_instante
                .map_or(0.0, |t| agora.duration_since(t).as_secs_f64());
            let avanco = (parado * f64::from(self.relogio)) as u32;
            self.deslocamento = self.ultimo_saida.wrapping_add(avanco).wrapping_sub(entrada);
        }

        self.ultimo_entrada = Some(entrada);
        self.ultimo_instante = Some(agora);
        let saida = entrada.wrapping_add(self.deslocamento);
        self.ultimo_saida = saida;
        (saida, reiniciou)
    }

    /// Próximo número de sequência, sempre contínuo.
    ///
    /// Vale para os dois tipos de mídia. No vídeo já era obrigatório por causa
    /// da reinserção de parâmetros, que insere pacotes no meio; no áudio passa
    /// a ser por causa do reinício do encoder.
    fn seq(&mut self, entrada: u16) -> u16 {
        let s = self.proxima_seq.unwrap_or(entrada);
        self.proxima_seq = Some(s.wrapping_add(1));
        s
    }
}

#[cfg(test)]
mod testes_continuidade {
    use super::Continuidade;
    use std::time::{Duration, Instant};

    const VIDEO: u32 = 90_000;

    /// Só o timestamp. A maioria dos testes não olha a detecção de reinício,
    /// e destrinchar a tupla em toda linha só atrapalharia a leitura.
    fn so_ts(c: &mut Continuidade, entrada: u32, agora: Instant) -> u32 {
        c.timestamp(entrada, agora).0
    }

    /// Fluxo normal passa intocado: sem reinício, saída é igual à entrada.
    #[test]
    fn fluxo_normal_nao_e_alterado() {
        let mut c = Continuidade::nova(VIDEO);
        let t0 = Instant::now();
        let mut ts = 1_000_000u32;
        for i in 0..100 {
            let agora = t0 + Duration::from_millis(i * 16);
            assert_eq!(so_ts(&mut c, ts, agora), ts, "quadro {i}");
            ts = ts.wrapping_add(1500); // 60 fps a 90 kHz
        }
    }

    /// O caso que motiva tudo: encoder reinicia com base nova e o timestamp
    /// não pode voltar no tempo.
    #[test]
    fn reinicio_do_encoder_nao_volta_no_tempo() {
        let mut c = Continuidade::nova(VIDEO);
        let t0 = Instant::now();
        let ultimo = so_ts(&mut c, 1_000_000, t0);

        // Meio segundo parado e um ffmpeg novo, numerando de outro lugar.
        let depois = so_ts(&mut c, 7, t0 + Duration::from_millis(500));
        assert!(
            depois > ultimo,
            "saída andou para trás: {ultimo} -> {depois}"
        );
        // O tempo parado tem de aparecer: meio segundo a 90 kHz são 45000.
        let avanco = depois - ultimo;
        assert!(
            (44_000..=46_000).contains(&avanco),
            "avanço deveria refletir os 0,5s reais, veio {avanco}"
        );
    }

    /// Depois do reinício o fluxo volta a andar no passo normal, sem degrau a
    /// cada quadro.
    #[test]
    fn depois_do_reinicio_o_passo_volta_ao_normal() {
        let mut c = Continuidade::nova(VIDEO);
        let t0 = Instant::now();
        so_ts(&mut c, 1_000_000, t0);
        let base = so_ts(&mut c, 500, t0 + Duration::from_millis(500));
        let seguinte = so_ts(&mut c, 2000, t0 + Duration::from_millis(516));
        assert_eq!(seguinte - base, 1500, "um quadro de 60 fps");
    }

    /// Um encoder novo pode começar num número MAIOR. Continua sendo reinício.
    #[test]
    fn reinicio_para_numero_maior_tambem_e_costurado() {
        let mut c = Continuidade::nova(VIDEO);
        let t0 = Instant::now();
        let a = so_ts(&mut c, 1000, t0);
        let b = so_ts(&mut c, 900_000_000, t0 + Duration::from_millis(500));
        assert!(
            b > a && b - a < VIDEO,
            "salto não foi absorvido: {a} -> {b}"
        );
    }

    /// O relógio RTP é de 32 bits e dá a volta. A volta é fluxo normal, não
    /// reinício — tratá-la como reinício inseriria uma pausa que não houve.
    #[test]
    fn a_volta_do_contador_de_32_bits_nao_e_reinicio() {
        let mut c = Continuidade::nova(VIDEO);
        let t0 = Instant::now();
        let antes = u32::MAX - 750;
        let a = so_ts(&mut c, antes, t0);
        let b = so_ts(
            &mut c,
            antes.wrapping_add(1500),
            t0 + Duration::from_millis(16),
        );
        assert_eq!(b, a.wrapping_add(1500), "a volta virou salto");
    }

    /// Sequência é contínua sempre, inclusive quando a entrada dá salto.
    #[test]
    fn sequencia_e_continua_mesmo_com_entrada_saltando() {
        let mut c = Continuidade::nova(VIDEO);
        let primeiro = c.seq(5000);
        assert_eq!(primeiro, 5000, "começa de onde a entrada estava");
        assert_eq!(c.seq(5001), 5001);
        // Encoder novo recomeçando do zero não pode abrir buraco.
        assert_eq!(c.seq(0), 5002);
        assert_eq!(c.seq(1), 5003);
    }

    /// Áudio usa o mesmo mecanismo com outro relógio.
    #[test]
    fn audio_tem_o_proprio_relogio() {
        let mut c = Continuidade::nova(48_000);
        let t0 = Instant::now();
        let a = so_ts(&mut c, 10_000, t0);
        // Opus manda 20 ms por pacote: 960 ciclos. Não é reinício.
        let b = so_ts(&mut c, 10_960, t0 + Duration::from_millis(20));
        assert_eq!(b - a, 960);
        // Agora um reinício de verdade, com 0,5 s parado.
        let d = so_ts(&mut c, 3, t0 + Duration::from_millis(520));
        let avanco = d - b;
        assert!(
            (23_000..=25_000).contains(&avanco),
            "0,5s a 48 kHz são ~24000, veio {avanco}"
        );
    }
}

#[cfg(test)]
mod testes_continuidade_engasgo {
    use super::Continuidade;
    use std::time::{Duration, Instant};

    const VIDEO: u32 = 90_000;

    /// Só o timestamp. A maioria dos testes não olha a detecção de reinício,
    /// e destrinchar a tupla em toda linha só atrapalharia a leitura.
    fn so_ts(c: &mut Continuidade, entrada: u32, agora: Instant) -> u32 {
        c.timestamp(entrada, agora).0
    }

    /// Um engasgo de 300 ms SEM reinício — a máquina travou, o socket ficou
    /// mudo, o ffmpeg continuou o mesmo — dispara a detecção por engano.
    ///
    /// O comentário no código afirma que errar assim é inofensivo. Isto
    /// verifica: o deslocamento é recalculado a partir do tempo real
    /// decorrido, então o resultado tem de bater com o que sairia sem
    /// detecção nenhuma, a menos de arredondamento.
    #[test]
    fn engasgo_sem_reinicio_nao_estraga_a_linha_do_tempo() {
        let mut c = Continuidade::nova(VIDEO);
        let t0 = Instant::now();

        let a = so_ts(&mut c, 1_000_000, t0);
        // 400 ms de silêncio, e o encoder segue de onde parou: 0,4 s a 90 kHz
        // são 36000 ciclos.
        let esperado = 1_000_000 + 36_000;
        let b = so_ts(&mut c, esperado, t0 + Duration::from_millis(400));

        let desvio = (b as i64 - (a as i64 + 36_000)).abs();
        assert!(
            desvio <= 2,
            "o falso positivo deslocou a linha do tempo em {desvio} ciclos"
        );

        // E o passo seguinte volta ao normal, sem degrau acumulado.
        let d = so_ts(&mut c, esperado + 1500, t0 + Duration::from_millis(416));
        assert_eq!(d.wrapping_sub(b), 1500);
    }

    /// Vários reinícios seguidos não podem fazer a linha do tempo andar para
    /// trás em nenhum momento — é o que quebraria o buffer de jitter.
    #[test]
    fn reinicios_seguidos_nunca_andam_para_tras() {
        let mut c = Continuidade::nova(VIDEO);
        let t0 = Instant::now();
        let mut ms = 0u64;
        let mut anterior = 0u32;
        let mut primeiro = true;

        for volta in 0..10u32 {
            // Cada reinício começa numa base arbitrária diferente.
            let base = volta.wrapping_mul(987_654_321).wrapping_add(17);
            ms += 700; // pausa do reinício
            for quadro in 0..30u32 {
                let entrada = base.wrapping_add(quadro.wrapping_mul(1500));
                let saida = so_ts(&mut c, entrada, t0 + Duration::from_millis(ms));
                if !primeiro {
                    assert!(
                        saida.wrapping_sub(anterior) < VIDEO * 5,
                        "salto absurdo na volta {volta}, quadro {quadro}: \
                         {anterior} -> {saida}"
                    );
                    assert!(
                        (saida.wrapping_sub(anterior) as i32) >= 0,
                        "andou para trás na volta {volta}, quadro {quadro}: \
                         {anterior} -> {saida}"
                    );
                }
                primeiro = false;
                anterior = saida;
                ms += 16;
            }
        }
    }
}

#[cfg(test)]
mod testes_reinicio_avisa {
    use super::Continuidade;
    use std::time::{Duration, Instant};

    /// O reinício precisa ser ANUNCIADO, não só absorvido.
    ///
    /// Quem lê usa o aviso para jogar fora os parâmetros de imagem guardados:
    /// um encoder novo pode vir com resolução nova, e o SPS antigo descreve a
    /// antiga. Reinjetar dimensões que não batem com o quadro faz o
    /// decodificador montar imagem errada, em vez de esperar meio segundo pelo
    /// parâmetro certo que vem no primeiro IDR.
    ///
    /// É o passo que transforma "trocar a taxa" em "trocar a taxa E a
    /// resolução" sem custo adicional.
    #[test]
    fn o_reinicio_e_anunciado_para_quem_le() {
        let mut c = Continuidade::nova(90_000);
        let t0 = Instant::now();

        let (_, primeiro) = c.timestamp(1000, t0);
        assert!(!primeiro, "o primeiro pacote da vida não é reinício");

        let (_, normal) = c.timestamp(2500, t0 + Duration::from_millis(16));
        assert!(!normal, "quadro seguinte no ritmo não é reinício");

        let (_, trocou) = c.timestamp(90, t0 + Duration::from_millis(700));
        assert!(trocou, "pausa de 700 ms com base nova é reinício");

        let (_, depois) = c.timestamp(1590, t0 + Duration::from_millis(716));
        assert!(!depois, "o quadro seguinte ao reinício já é fluxo normal");
    }
}

#[cfg(test)]
mod testes_emissor {
    use super::{EmissorH264, MTU_ENVIO, RELOGIO_VIDEO};
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc;

    /// Uma unidade de acesso em Annex-B: SPS, PPS e um IDR de `n` bytes.
    fn unidade_chave(n: usize) -> Vec<u8> {
        let mut v = Vec::new();
        // SPS (tipo 7) e PPS (tipo 8), mínimos mas com o tipo certo — é o que
        // o empacotador usa para decidir emitir STAP-A antes do IDR.
        v.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x42, 0xe0, 0x1f, 0x11]);
        v.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80]);
        v.extend_from_slice(&[0, 0, 0, 1, 0x65]);
        v.extend(std::iter::repeat_n(0xAB, n));
        v
    }

    #[tokio::test]
    async fn fragmenta_e_marca_o_fim_do_quadro() {
        let (envio, mut fila) = mpsc::channel(512);
        let inicio = Instant::now();
        let mut e = EmissorH264::de_teste(envio, inicio);

        // Bem maior que a MTU: tem de sair fragmentado.
        e.envia(&unidade_chave(8000), inicio, true);

        let mut pacotes = Vec::new();
        while let Ok((p, _)) = fila.try_recv() {
            pacotes.push(p);
        }
        assert!(
            pacotes.len() > 5,
            "8 KB deveriam virar vários pacotes, vieram {}",
            pacotes.len()
        );
        assert!(
            pacotes.iter().all(|p| p.payload.len() <= MTU_ENVIO),
            "algum pacote passou da MTU"
        );

        // Sequência contígua a partir de zero.
        for (i, p) in pacotes.iter().enumerate() {
            assert_eq!(
                p.header.sequence_number, i as u16,
                "sequência furada em {i}"
            );
        }
        // Só o último fecha a unidade de acesso.
        let marcados: Vec<usize> = pacotes
            .iter()
            .enumerate()
            .filter(|(_, p)| p.header.marker)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            marcados,
            vec![pacotes.len() - 1],
            "o marcador tem de estar só no último pacote"
        );
        // Todos do mesmo quadro compartilham o timestamp.
        let ts = pacotes[0].header.timestamp;
        assert!(pacotes.iter().all(|p| p.header.timestamp == ts));
    }

    #[tokio::test]
    async fn o_tempo_vem_do_relogio_e_nao_da_contagem() {
        let (envio, mut fila) = mpsc::channel(512);
        let inicio = Instant::now();
        let mut e = EmissorH264::de_teste(envio, inicio);

        // Dois quadros separados por meio segundo REAL. Um contador de quadros
        // diria "2 quadros"; o relógio diz meio segundo, que é o que o
        // receptor precisa para não acelerar a imagem.
        e.envia(&unidade_chave(100), inicio, true);
        e.envia(
            &unidade_chave(100),
            inicio + Duration::from_millis(500),
            true,
        );

        let mut ts = Vec::new();
        while let Ok((p, _)) = fila.try_recv() {
            if p.header.marker {
                ts.push(p.header.timestamp);
            }
        }
        assert_eq!(ts.len(), 2, "esperava dois quadros");
        let diferenca = ts[1].wrapping_sub(ts[0]);
        // Meio segundo a 90 kHz são 45000.
        assert_eq!(diferenca, RELOGIO_VIDEO / 2);
    }

    #[tokio::test]
    async fn a_sequencia_continua_entre_quadros() {
        let (envio, mut fila) = mpsc::channel(512);
        let inicio = Instant::now();
        let mut e = EmissorH264::de_teste(envio, inicio);
        for i in 0..5 {
            e.envia(
                &unidade_chave(3000),
                inicio + Duration::from_millis(i * 16),
                true,
            );
        }
        let mut esperado = 0u16;
        while let Ok((p, _)) = fila.try_recv() {
            assert_eq!(p.header.sequence_number, esperado);
            esperado = esperado.wrapping_add(1);
        }
        assert!(esperado > 5, "poucos pacotes: {esperado}");
    }
}

#[cfg(test)]
mod testes_reinsercao_de_parametros {
    use super::{EmissorH264, Ordering};
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc;

    fn nal(tipo: u8, corpo: &[u8]) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1, tipo & 0x1F];
        v.extend_from_slice(corpo);
        v
    }

    /// Um quadro-chave com os parâmetros junto, como o nvenc entrega.
    fn idr_com_parametros() -> Vec<u8> {
        let mut v = nal(7, &[0x42, 0xe0, 0x1f]);
        v.extend(nal(8, &[0xce, 0x3c, 0x80]));
        v.extend(nal(5, &[0xCD; 60]));
        v
    }

    /// Um quadro-chave PELADO, como o AMF entrega da segunda vez em diante.
    fn idr_sem_parametros() -> Vec<u8> {
        nal(5, &[0xCD; 60])
    }

    fn quadro_comum() -> Vec<u8> {
        nal(1, &[0xAB; 60])
    }

    /// O defeito que deixou a sala preta.
    ///
    /// # O que aconteceu
    ///
    /// O `h264_amf` mandou SPS e PPS UMA vez, no primeiro quadro, e nunca mais.
    /// O `H264Payloader` guarda o par e emite o STAP-A quando o vê — uma vez,
    /// não antes de cada IDR —, então ninguém repunha os parâmetros. A
    /// telemetria mostrou `params 1+0` congelado enquanto o bitrate seguia em
    /// 7 Mbps: dado chegando, e nenhum decodificador capaz de montar imagem.
    ///
    /// O nvenc repete os parâmetros sozinho, e por isso o defeito passou
    /// despercebido em duas máquinas antes de aparecer na terceira.
    #[tokio::test]
    async fn idr_pelado_sai_com_os_parametros_repostos() {
        let (envio, mut fila) = mpsc::channel(512);
        let inicio = Instant::now();
        let mut e = EmissorH264::de_teste(envio, inicio);

        // O primeiro quadro traz os parâmetros; daí em diante, nunca mais.
        e.envia(&idr_com_parametros(), inicio, true);
        for n in 1..=4 {
            e.envia(
                &idr_sem_parametros(),
                inicio + Duration::from_millis(n * 100),
                false,
            );
        }

        let vistos = e.contadores.parametros_vistos.load(Ordering::Relaxed);
        let repostos = e.contadores.parametros_reinseridos.load(Ordering::Relaxed);
        assert_eq!(
            repostos, 4,
            "os quatro IDR pelados tinham de sair com os parâmetros repostos"
        );
        assert_eq!(
            vistos, 5,
            "todo IDR tem de sair carregando parâmetros — foi o `1` congelado que deixou a sala preta"
        );
        assert!(fila.try_recv().is_ok(), "não saiu pacote nenhum");
    }

    /// Repor no que já traz duplicaria os parâmetros em cada quadro-chave.
    #[tokio::test]
    async fn quem_ja_traz_os_parametros_nao_recebe_de_novo() {
        let (envio, _fila) = mpsc::channel(512);
        let inicio = Instant::now();
        let mut e = EmissorH264::de_teste(envio, inicio);
        for n in 0..3 {
            e.envia(
                &idr_com_parametros(),
                inicio + Duration::from_millis(n * 100),
                true,
            );
        }
        assert_eq!(
            e.contadores.parametros_reinseridos.load(Ordering::Relaxed),
            0
        );
        assert_eq!(e.contadores.parametros_vistos.load(Ordering::Relaxed), 3);
    }

    /// Só o quadro-chave precisa: pôr em todos gastaria banda à toa.
    #[tokio::test]
    async fn quadro_comum_nao_recebe_parametros() {
        let (envio, _fila) = mpsc::channel(512);
        let inicio = Instant::now();
        let mut e = EmissorH264::de_teste(envio, inicio);
        e.envia(&idr_com_parametros(), inicio, true);
        for n in 1..=5 {
            e.envia(
                &quadro_comum(),
                inicio + Duration::from_millis(n * 100),
                false,
            );
        }
        assert_eq!(
            e.contadores.parametros_reinseridos.load(Ordering::Relaxed),
            0
        );
        assert_eq!(e.contadores.parametros_vistos.load(Ordering::Relaxed), 1);
    }

    /// Sem nunca ter visto os parâmetros não há o que repor — e não pode quebrar.
    #[tokio::test]
    async fn idr_pelado_antes_de_qualquer_parametro_apenas_passa() {
        let (envio, mut fila) = mpsc::channel(512);
        let inicio = Instant::now();
        let mut e = EmissorH264::de_teste(envio, inicio);
        e.envia(&idr_sem_parametros(), inicio, false);
        assert_eq!(
            e.contadores.parametros_reinseridos.load(Ordering::Relaxed),
            0
        );
        assert_eq!(e.contadores.parametros_vistos.load(Ordering::Relaxed), 0);
        assert!(
            fila.try_recv().is_ok(),
            "o quadro tinha de sair mesmo assim"
        );
    }

    /// O caso do AMF sem `forced_idr`: o encoder marca o pacote como chave, mas
    /// o quadro é um I-frame comum — não há NAL tipo 5 para achar.
    ///
    /// Varrer só o Annex-B não pegaria este, e ele precisa dos parâmetros tanto
    /// quanto um IDR: é o quadro que quem entrou agora vai tentar decodificar.
    #[tokio::test]
    async fn i_frame_marcado_pelo_encoder_tambem_recebe_parametros() {
        let (envio, _fila) = mpsc::channel(512);
        let inicio = Instant::now();
        let mut e = EmissorH264::de_teste(envio, inicio);
        e.envia(&idr_com_parametros(), inicio, true);
        // Tipo 1: I-frame sem IDR. Só a bandeira do encoder o denuncia.
        e.envia(&quadro_comum(), inicio + Duration::from_millis(100), true);
        assert_eq!(
            e.contadores.parametros_reinseridos.load(Ordering::Relaxed),
            1,
            "a bandeira do encoder tinha de bastar, sem depender de achar NAL 5"
        );
    }
}

#[cfg(test)]
mod testes_parametros_na_unidade {
    use super::para_cada_nal;

    fn nal(tipo: u8, corpo: &[u8]) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1, tipo & 0x1F];
        v.extend_from_slice(corpo);
        v
    }

    /// Os tipos encontrados, na ordem.
    fn tipos(unidade: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        para_cada_nal(unidade, |t, _| v.push(t));
        v
    }

    #[test]
    fn acha_sps_e_pps() {
        assert_eq!(tipos(&nal(7, &[1, 2, 3])), vec![7], "SPS");
        assert_eq!(tipos(&nal(8, &[1, 2, 3])), vec![8], "PPS");
    }

    #[test]
    fn quadro_comum_nao_tem() {
        // 5 = IDR, 1 = quadro não-IDR. Nenhum dos dois é parâmetro.
        assert_eq!(tipos(&nal(5, &[0xAB; 40])), vec![5]);
        assert_eq!(tipos(&nal(1, &[0xAB; 40])), vec![1]);
    }

    #[test]
    fn acha_no_meio_de_uma_unidade_com_varios_nals() {
        let mut v = nal(1, &[0xAB; 10]);
        v.extend(nal(7, &[1, 2]));
        v.extend(nal(5, &[0xCD; 20]));
        assert_eq!(tipos(&v), vec![1, 7, 5]);
    }

    #[test]
    fn aceita_codigo_de_inicio_de_tres_bytes() {
        // Annex-B permite `00 00 01`, e a libav usa os dois formatos.
        let v = vec![0, 0, 1, 0x67, 0x42, 0xe0];
        assert_eq!(tipos(&v), vec![7]);
    }

    #[test]
    fn nao_quebra_em_entrada_curta_ou_lixo() {
        for entrada in [&[][..], &[0, 0][..], &[0, 0, 0][..], &[0xFF; 16][..]] {
            assert!(tipos(entrada).is_empty(), "achou NAL em {entrada:?}");
        }
    }

    /// A fatia entregue tem de conter a NAL INTEIRA com o código de início.
    ///
    /// É dela que sai o SPS reinserido antes de cada IDR; entregar sem o
    /// prefixo produziria um fluxo que nenhum decodificador lê.
    #[test]
    fn a_fatia_vem_com_o_codigo_de_inicio_e_o_corpo_todo() {
        let mut v = nal(7, &[0x42, 0xe0, 0x1f]);
        v.extend(nal(5, &[0xCD; 8]));
        let mut fatias = Vec::new();
        para_cada_nal(&v, |t, f| fatias.push((t, f.to_vec())));
        assert_eq!(fatias.len(), 2);
        assert_eq!(fatias[0].1, nal(7, &[0x42, 0xe0, 0x1f]));
        assert_eq!(fatias[1].1, nal(5, &[0xCD; 8]));
    }
}

#[cfg(test)]
mod testes_zero_do_relogio {
    use super::EmissorH264;
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc;

    fn unidade() -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1, 0x65];
        v.extend(std::iter::repeat_n(0xAB, 200));
        v
    }

    /// Quadros capturados ANTES de o emissor existir não podem colidir em zero.
    ///
    /// O preparo da transmissão captura alguns quadros para provar que o
    /// encoder funciona; só depois a sessão WHIP sobe e o emissor nasce. Com o
    /// zero fixado no nascimento, todos eles teriam o mesmo carimbo — e vários
    /// quadros com o mesmo tempo é o que faz o receptor concluir que há quadros
    /// B e derrubar quem está assistindo.
    #[tokio::test]
    async fn o_zero_e_o_primeiro_quadro_e_nao_o_nascimento() {
        let (envio, mut fila) = mpsc::channel(512);
        // Nasce "agora", mas os quadros vêm de antes.
        let nascimento = Instant::now();
        let mut e = EmissorH264::de_teste_sem_zero(envio);

        let antes = nascimento - Duration::from_millis(500);
        for ms in [0u64, 16, 33, 50] {
            e.envia(&unidade(), antes + Duration::from_millis(ms), true);
        }

        let mut ts = Vec::new();
        while let Ok((p, _)) = fila.try_recv() {
            if p.header.marker {
                ts.push(p.header.timestamp);
            }
        }
        assert_eq!(ts.len(), 4);
        assert_eq!(ts[0], 0, "o primeiro quadro é o zero da linha do tempo");
        for par in ts.windows(2) {
            assert!(par[1] > par[0], "carimbos colidiram: {ts:?}");
        }
        // 16 ms a 90 kHz são 1440.
        assert_eq!(ts[1], 1440);
    }
}
