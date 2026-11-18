//! Captura de áudio via WASAPI: loopback por processo (janela) ou loopback do
//! dispositivo de render padrão (monitor).

use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use wasapi::{
    initialize_mta, AudioClient, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat,
};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
pub const BYTES_PER_SAMPLE: usize = 2; // s16le
/// Bytes de um frame de áudio (todos os canais de um instante).
pub const FRAME_BYTES: usize = CHANNELS * BYTES_PER_SAMPLE;

/// Duração do buffer do dispositivo, em unidades de 100 ns (20 ms).
///
/// Curto de propósito: é latência que entra direto na experiência da chamada,
/// e o preenchimento de silêncio já cobre o caso de underrun.
const BUFFER_DURATION_HNS: i64 = 200_000;

/// Tamanho máximo de um bloco escrito de uma vez, em frames (100 ms).
const MAX_BATCH_FRAMES: u64 = SAMPLE_RATE as u64 / 10;

/// De onde o áudio vem.
#[derive(Debug, Clone, Copy)]
pub enum AudioSource {
    /// Apenas o processo indicado e seus filhos. Regra central do produto:
    /// compartilhar uma JANELA envia só o áudio daquele aplicativo.
    Process(u32),
    /// Loopback do dispositivo de render padrão. Usado quando a fonte de vídeo
    /// é um MONITOR inteiro.
    System,
    /// Áudio do sistema **menos** o processo indicado e seus filhos.
    ///
    /// Existe por causa do eco: ao compartilhar o monitor inteiro, o áudio do
    /// sistema inclui a saída do Discord — ou seja, a voz das outras pessoas da
    /// call. Elas se ouvem de volta, com atraso. Excluindo o processo da voz,
    /// o jogo e o vídeo continuam indo e o eco some.
    ///
    /// Exclui **um** processo: é o que a API do Windows aceita por sessão.
    SistemaExceto(u32),
}

impl std::fmt::Display for AudioSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Process(pid) => write!(f, "somente PID {pid} (e filhos)"),
            Self::System => write!(f, "sistema inteiro"),
            Self::SistemaExceto(pid) => write!(f, "sistema inteiro, menos o PID {pid} (e filhos)"),
        }
    }
}

/// Bytes esperados para `secs` segundos de PCM no formato que este módulo
/// produz. Serve para conferir, do lado de fora, que o preenchimento de
/// silêncio manteve o fluxo.
#[must_use]
pub fn expected_bytes(secs: u64) -> u64 {
    secs * u64::from(SAMPLE_RATE) * FRAME_BYTES as u64
}

/// Abre o cliente WASAPI já inicializado para a fonte pedida.
fn open_client(source: AudioSource) -> Result<AudioClient> {
    let mut client = match source {
        AudioSource::Process(pid) => {
            // include_tree = true: captura o processo E os filhos dele.
            // Navegadores e launchers de jogo tocam áudio em subprocessos —
            // com `false` o Chrome sairia mudo, porque quem toca é o processo
            // filho de áudio, não a janela.
            AudioClient::new_application_loopback_client(pid, true)
                .context("loopback por processo — requer Windows 10 build 20348+")?
        }
        AudioSource::SistemaExceto(pid) => {
            // `false` = PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE:
            // tudo que toca na máquina, MENOS aquele processo e seus filhos.
            //
            // É o que evita o eco ao compartilhar o monitor inteiro: sem isto,
            // o áudio do sistema carrega a saída do Discord — a voz das outras
            // pessoas da call — e elas se ouvem de volta com atraso.
            AudioClient::new_application_loopback_client(pid, false)
                .context("loopback com exclusão — requer Windows 10 build 20348+")?
        }
        AudioSource::System => {
            // Cliente vindo de um dispositivo de RENDER e inicializado na
            // direção Capture: é assim que a wasapi liga o modo loopback.
            DeviceEnumerator::new()
                .context("enumerador de dispositivos de áudio")?
                .get_default_device(&Direction::Render)
                .context("dispositivo de render padrão")?
                .get_iaudioclient()
                .context("iaudioclient")?
        }
    };

    let format = WaveFormat::new(
        16,
        16,
        &SampleType::Int,
        SAMPLE_RATE as usize,
        CHANNELS,
        None,
    );

    // `autoconvert` deixa o motor de áudio reamostrar do formato nativo do
    // dispositivo para s16le/48 kHz estéreo, que é o que o ffmpeg espera. Sem
    // isso teríamos de converter à mão para cada formato de placa existente.
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: BUFFER_DURATION_HNS,
    };
    client
        .initialize_client(&format, &Direction::Capture, &mode)
        .context("inicializar cliente de captura")?;

    Ok(client)
}

/// Lê o áudio da fonte e escreve s16le no `sink`, **preenchendo com silêncio**
/// quando o aplicativo não está emitindo som. Devolve quantos frames de áudio
/// foram escritos.
///
/// Por que o preenchimento existe: o loopback por processo não produz amostra
/// nenhuma enquanto o aplicativo está mudo — o mesmo problema da tela parada,
/// do outro lado. Se escrevêssemos só o que o WASAPI entrega, o pipe de áudio
/// pararia durante o silêncio, o ffmpeg travaria a trilha e, quando o som
/// voltasse, ele estaria adiantado em relação ao vídeo pelo tempo exato do
/// silêncio. Por isso a cadência é ditada por um relógio (`start.elapsed()`) e
/// não pelo dispositivo: o que faltar de amostra vira zero, que é silêncio
/// digital (risco R2 da spec).
pub fn pump<W: Write>(source: AudioSource, mut sink: W, stop: &Arc<AtomicBool>) -> Result<u64> {
    // MTA e não STA: o `pump` roda numa thread de trabalho nossa, nunca na
    // thread de UI.
    initialize_mta().ok().context("inicializar COM em MTA")?;

    let client = open_client(source)?;
    let event = client
        .set_get_eventhandle()
        .context("handle de evento do cliente de áudio")?;
    let capture = client
        .get_audiocaptureclient()
        .context("cliente de captura")?;

    let mut queue: VecDeque<u8> = VecDeque::new();
    client.start_stream().context("iniciar stream de áudio")?;

    // Ao abrir um cliente com exclusão, o Windows leva cerca de um segundo para
    // a exclusão valer de fato: medido aqui, o primeiro meio segundo sai no
    // volume cheio, decai, e só depois de ~1 s fica em zero. É pouco, mas é
    // exatamente a voz da call vazando para os outros no instante em que
    // alguém começa a compartilhar. Silenciar esse trecho custa um segundo de
    // áudio no começo — barato perto de mandar o eco.
    let mudo_ate = match source {
        AudioSource::SistemaExceto(_) => Duration::from_millis(1500),
        _ => Duration::ZERO,
    };

    let start = Instant::now();
    let mut written_frames: u64 = 0;
    let mut out: Vec<u8> = Vec::new();

    while !stop.load(Ordering::Relaxed) {
        // Espera curta e erro ignorado de propósito: se o aplicativo está mudo
        // o evento nunca dispara, e o timeout é justamente o que nos deixa
        // seguir em frente para injetar silêncio.
        let _ = event.wait_for_event(50);
        let _ = capture.read_from_device_to_deque(&mut queue);

        // Quantos frames de áudio DEVERIAM ter saído até agora, pelo relógio.
        let due = (start.elapsed().as_secs_f64() * f64::from(SAMPLE_RATE)) as u64;
        while written_frames < due {
            let batch = (due - written_frames).min(MAX_BATCH_FRAMES) as usize;
            out.clear();
            out.resize(batch * FRAME_BYTES, 0);

            let avail = (queue.len() / FRAME_BYTES).min(batch) * FRAME_BYTES;
            // Dentro da janela de mudez, as amostras são consumidas e
            // DESCARTADAS. Consumir importa: deixá-las na fila só adiaria o
            // vazamento para o lote seguinte.
            let mudo = start.elapsed() < mudo_ate;
            for slot in out.iter_mut().take(avail) {
                // `avail` já foi limitado pelo tamanho da fila, então só há
                // elemento; o `unwrap_or` é rede de segurança sem pânico.
                let amostra = queue.pop_front().unwrap_or(0);
                if !mudo {
                    *slot = amostra;
                }
            }
            // O resto de `out` continua zerado = silêncio. É este preenchimento
            // que impede o pipe de estagnar quando o aplicativo está mudo.
            sink.write_all(&out)?;
            written_frames += batch as u64;
        }
    }

    client.stop_stream().context("parar stream de áudio")?;
    sink.flush()?;
    Ok(written_frames)
}
