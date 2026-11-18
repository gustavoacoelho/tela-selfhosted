//! Captura de vídeo via Windows Graphics Capture (WGC), conversão para NV12 e
//! entrega em cadência fixa.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
// Sem `cfg`: o `Codificacao` guarda o dispositivo e a textura retida em
// qualquer compilação, para quem o constrói não precisar de `cfg` também. Só o
// uso deles depende da feature.
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

#[cfg(feature = "encoder-d3d11")]
use crate::encoder;

/// Uma janela capturável.
///
/// O `pid` é o campo que amarra vídeo e áudio na mesma fonte: é ele que vai
/// para o loopback por processo, garantindo que compartilhar uma janela envie
/// só o som daquele aplicativo.
#[derive(Debug, Clone)]
pub struct WindowSource {
    pub hwnd: isize,
    pub pid: u32,
    pub title: String,
    /// A janela está minimizada. Continua na lista — a pessoa sabe que o
    /// aplicativo está aberto e quer compartilhá-lo —, mas precisa ser
    /// restaurada antes de capturar: minimizada, o Windows não a desenha e o
    /// WGC não entrega quadro nenhum.
    pub minimizada: bool,
}

/// Um monitor capturável.
#[derive(Debug, Clone)]
pub struct MonitorSource {
    pub index: usize,
    pub name: String,
    pub width: u32,
    pub height: u32,
}

/// Janelas capturáveis, cada uma com o PID dono.
///
/// A filtragem (visível, com título, não-filha, não encoberta) fica por conta
/// do `Window::enumerate` da `windows-capture`: é exatamente o mesmo critério
/// que a captura aplica depois, então listar aqui e falhar ao capturar não
/// pode acontecer por divergência de filtro.
pub fn list_windows() -> Result<Vec<WindowSource>> {
    let mut out = Vec::new();
    for w in Window::enumerate().context("enumerar janelas")? {
        // Uma janela pode fechar entre o enumerate e a consulta; nesse caso ela
        // simplesmente não entra na lista.
        let (Ok(title), Ok(pid)) = (w.title(), w.process_id()) else {
            continue;
        };
        if title.is_empty() {
            continue;
        }
        let hwnd = w.as_raw_hwnd() as isize;
        let estado = estado_da_janela(hwnd);

        // O critério mora em `oferecivel` para a lista e a captura não
        // divergirem: esta checagem já esteve escrita à mão aqui, mais fraca
        // que a da captura, e o resultado era oferecer janela de tamanho zero
        // que a captura recusava em seguida.
        if !estado.oferecivel() {
            continue;
        }

        out.push(WindowSource {
            hwnd,
            pid,
            title,
            minimizada: estado.minimizada,
        });
    }
    Ok(out)
}

/// Monitores capturáveis.
pub fn list_monitors() -> Result<Vec<MonitorSource>> {
    let mut out = Vec::new();
    for m in Monitor::enumerate().context("enumerar monitores")? {
        let (Ok(index), Ok(width), Ok(height)) = (m.index(), m.width(), m.height()) else {
            continue;
        };
        out.push(MonitorSource {
            index,
            name: m.name().unwrap_or_else(|_| "(sem nome)".to_string()),
            width,
            height,
        });
    }
    Ok(out)
}

/// Primeira janela cujo título contenha `needle` (sem diferenciar maiúsculas).
///
/// Resolver a janela uma única vez e carregar o `WindowSource` inteiro é
/// deliberado: o HWND usado para o vídeo e o PID usado para o áudio saem do
/// mesmo registro. Buscar por título duas vezes poderia casar com janelas
/// diferentes e vazar o áudio de outro aplicativo.
pub fn find_window(needle: &str) -> Result<WindowSource> {
    let needle_lower = needle.to_lowercase();
    list_windows()?
        .into_iter()
        .find(|w| w.title.to_lowercase().contains(&needle_lower))
        .ok_or_else(|| anyhow::anyhow!("janela não encontrada: {needle}"))
}

/// Último frame capturado, em NV12, compartilhado entre o callback do WGC e o
/// relógio que alimenta o pipe.
///
/// A resolução só é conhecida quando o primeiro frame chega — o WGC é quem
/// dita o tamanho. Por isso `width`/`height` são atômicos preenchidos no `put`,
/// e não parâmetros do construtor: o ffmpeg precisa da resolução EXATA ou o
/// vídeo sai distorcido.
pub struct FrameBuffer {
    inner: Mutex<Option<Vec<u8>>>,
    ready: Condvar,
    width: AtomicU32,
    height: AtomicU32,
}

impl FrameBuffer {
    /// Construtor com nome explícito em vez de `new`: o que sai daqui já vem
    /// dentro de um `Arc`, porque o buffer sempre é compartilhado entre a
    /// thread do WGC e a thread do relógio.
    #[must_use]
    pub fn shared() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(None),
            ready: Condvar::new(),
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
        })
    }

    /// Resolução do último frame. `(0, 0)` antes do primeiro.
    #[must_use]
    pub fn dims(&self) -> (u32, u32) {
        (
            self.width.load(Ordering::Relaxed),
            self.height.load(Ordering::Relaxed),
        )
    }

    /// Chamado pelo callback do WGC a cada frame novo.
    pub fn put(&self, nv12: Vec<u8>, w: u32, h: u32) {
        self.width.store(w, Ordering::Relaxed);
        self.height.store(h, Ordering::Relaxed);
        // `lock()` só falha se outra thread entrou em pânico segurando o mutex.
        // Não há caminho de recuperação: o conteúdo estaria em estado
        // desconhecido, e transmitir lixo é pior que morrer.
        let mut slot = self.inner.lock().expect("FrameBuffer envenenado");
        *slot = Some(nv12);
        self.ready.notify_all();
    }

    /// Bloqueia até existir um primeiro frame. `false` em caso de timeout.
    pub fn wait_first(&self, timeout: Duration) -> bool {
        let slot = self.inner.lock().expect("FrameBuffer envenenado");
        if slot.is_some() {
            return true;
        }
        let (slot, res) = self
            .ready
            .wait_timeout(slot, timeout)
            .expect("FrameBuffer envenenado");
        !res.timed_out() && slot.is_some()
    }

    /// Cópia do último frame. `None` só antes do primeiro frame.
    #[must_use]
    pub fn latest(&self) -> Option<Vec<u8>> {
        self.inner.lock().expect("FrameBuffer envenenado").clone()
    }

    /// Copia o último frame para `dst`, reaproveitando a alocação.
    ///
    /// Existe separado de [`FrameBuffer::latest`] porque é o caminho quente: a
    /// 1080p60 um `Vec` novo por frame são ~190 MB/s de alocação e descarte
    /// desnecessários. A cópia acontece sob o mutex, mas a escrita no pipe (que
    /// pode bloquear por muito tempo) acontece fora dele — segurar o mutex
    /// durante a escrita travaria o callback do WGC.
    pub fn copy_latest(&self, dst: &mut Vec<u8>) -> bool {
        let slot = self.inner.lock().expect("FrameBuffer envenenado");
        match slot.as_ref() {
            Some(frame) => {
                dst.clear();
                dst.extend_from_slice(frame);
                true
            }
            None => false,
        }
    }
}

/// Escreve frames no `sink` na cadência de `fps`, **repetindo o último** quando
/// o WGC não entregou nada novo. Devolve quantos frames foram escritos.
///
/// Por que isto existe: o Windows Graphics Capture é orientado a evento. Com a
/// tela parada ele simplesmente não entrega frame nenhum — a própria doc da
/// `windows-capture` diz que quem precisa de cadência fixa tem de marcar o
/// ritmo por conta própria. Sem esta reemissão o pipe do ffmpeg estagna, o
/// encoder para de produzir e a sincronia áudio/vídeo quebra de forma
/// cumulativa (risco R1 da spec). O relógio local aqui é a mitigação.
/// Escreve frames no `sink` na cadência de `fps`.
///
/// `saida` é a resolução combinada com o ffmpeg. **Todo** frame escrito tem
/// exatamente esse tamanho: a fonte pode mudar de dimensão no meio da
/// transmissão (tela cheia, janela redimensionada) e um único bloco de tamanho
/// diferente dessincroniza o pipe para sempre. Ver [`ajusta_para`].
///
/// `None` em `saida` mantém o comportamento antigo de escrever o frame como
/// veio — usado pelo `probe-video`, que só conta frames e não alimenta ffmpeg.
pub fn pump<W: Write>(
    buf: &Arc<FrameBuffer>,
    fps: u32,
    saida: Option<(u32, u32)>,
    mut sink: W,
    stop: &Arc<AtomicBool>,
) -> Result<u64> {
    anyhow::ensure!(fps > 0, "fps precisa ser maior que zero");

    // Sem estes dois ajustes, esta thread simplesmente não cumpre o horário
    // quando a máquina está ocupada — que é sempre, já que quem compartilha
    // costuma estar jogando. Medido: 60 fps pedidos viraram 28 entregues com
    // uma transmissão concorrente, e 60 certinhos com a máquina livre.
    //
    // `timeBeginPeriod(1)` aperta o relógio do escalonador, e a prioridade
    // acima do normal faz esta thread voltar a rodar assim que o sono acaba,
    // em vez de esperar a vez atrás do jogo. É pouco privilégio: a thread
    // dorme quase o tempo todo e só copia um quadro quando acorda.
    let _relogio = RelogioFino::novo();
    prioriza_thread_atual();

    let period = Duration::from_secs_f64(1.0 / f64::from(fps));
    let mut next = Instant::now();
    let mut written: u64 = 0;
    let mut scratch: Vec<u8> = Vec::new();
    let mut ajustado: Vec<u8> = Vec::new();

    while !stop.load(Ordering::Relaxed) {
        if buf.copy_latest(&mut scratch) {
            match saida {
                Some((dw, dh)) => {
                    let (sw, sh) = buf.dims();
                    ajusta_para(
                        &scratch,
                        sw as usize,
                        sh as usize,
                        dw as usize,
                        dh as usize,
                        &mut ajustado,
                    );
                    sink.write_all(&ajustado)?;
                }
                None => sink.write_all(&scratch)?,
            }
            written += 1;
        }
        next += period;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            // Atrasamos: reancora o relógio para não acumular dívida e depois
            // disparar uma rajada de frames tentando recuperar o tempo perdido.
            next = now;
        }
    }
    sink.flush()?;
    Ok(written)
}

/// Aperta a granularidade do escalonador do Windows enquanto existir.
///
/// O padrão é de ~15,6 ms, então um `sleep` de 16,67 ms (60 fps) pode custar
/// bem mais que isso e derrubar a taxa pela metade. É um ajuste global do
/// sistema, por isso é desfeito no `Drop` — deixar ligado depois de parar de
/// transmitir gastaria bateria de todo mundo à toa.
struct RelogioFino;

impl RelogioFino {
    fn novo() -> Self {
        // SAFETY: chamada sem parâmetros de ponteiro; o par com `timeEndPeriod`
        // é garantido pelo `Drop`.
        unsafe {
            let _ = windows::Win32::Media::timeBeginPeriod(1);
        }
        Self
    }
}

impl Drop for RelogioFino {
    fn drop(&mut self) {
        // SAFETY: desfaz exatamente o `timeBeginPeriod(1)` do construtor.
        unsafe {
            let _ = windows::Win32::Media::timeEndPeriod(1);
        }
    }
}

/// Põe a thread atual um degrau acima do normal.
///
/// Falha em silêncio: sem o degrau a transmissão fica menos fluida, o que é
/// muito melhor que não transmitir.
fn prioriza_thread_atual() {
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
    };
    // SAFETY: pseudo-handle da própria thread, sempre válido.
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }
}

/// Handler do WGC: encaminha cada frame para o [`Destino`] configurado.
pub struct Handler {
    destino: Destino,
    /// Buffer reaproveitado na remoção do padding de linha feita pela
    /// `windows-capture`. Fica no handler para não realocar a cada frame.
    /// Só o caminho da CPU o usa.
    scratch: Vec<u8>,
    /// Tempo mínimo entre duas conversões.
    intervalo: Duration,
    /// Quando o último quadro foi convertido.
    ultima: Option<Instant>,
    /// O dispositivo da captura, para montar o encoder em cima dele.
    #[cfg(feature = "encoder-d3d11")]
    dispositivo: ID3D11Device,
    #[cfg(feature = "encoder-d3d11")]
    contexto: ID3D11DeviceContext,
    /// Textura nossa onde a do WGC é copiada, com o tamanho dela.
    ///
    /// A textura que o callback recebe só vale durante o callback; o relógio
    /// encoda depois, então precisa de uma que sobreviva. Recriada quando a
    /// fonte muda de tamanho.
    #[cfg(feature = "encoder-d3d11")]
    copia: Option<(ID3D11Texture2D, u32, u32)>,
    /// A amostra do primeiro quadro já foi tirada.
    #[cfg(feature = "encoder-d3d11")]
    amostrado: bool,
}

/// Quadros que o WGC entregou e quadros que realmente viraram trabalho.
///
/// No caminho da CPU, "convertidos" são os que passaram pelo BGRA→NV12. No da
/// GPU, os que o relógio encodou — quem retém não conta, porque reter é quase
/// de graça e não é isso que se quer medir.
///
/// Contadores globais e não por sessão porque só existe uma captura por vez, e
/// a diferença entre os dois é o número que interessa: quanto trabalho o
/// descarte poupou. Sem ele, "acho que está melhor" é tudo que se pode dizer.
static ENTREGUES: AtomicU64 = AtomicU64::new(0);
static CONVERTIDOS: AtomicU64 = AtomicU64::new(0);
/// Quadros codificados que não couberam na fila de quem publica.
static ENGASGADOS: AtomicU64 = AtomicU64::new(0);
/// Quadros que o relógio repetiu porque a tela não mudou.
///
/// Não é desperdício: é o que mantém o fluxo vivo num desktop quieto, e
/// comprime a quase nada porque não há diferença nenhuma para codificar.
static REPETIDOS: AtomicU64 = AtomicU64::new(0);
/// Voltas do relógio que se perderam por a anterior ter estourado o período.
///
/// Diferente de [`REPETIDOS`] (tela parada, fluxo saudável) e de
/// [`ENGASGADOS`] (fila de saída cheia): aqui o quadro nem chegou a existir,
/// porque o relógio não teve volta para produzi-lo.
///
/// Existe porque a falta dele custou caro: uma cadência 5% lenta não aparecia
/// em contador nenhum — `descartados` ficava em zero, a perda de rede em zero,
/// a CPU em 10% — e a investigação virou uma sequência de hipóteses. O OBS
/// mostra o equivalente (`lagged_frames`) na própria interface.
static ATRASADOS: AtomicU64 = AtomicU64::new(0);

/// A última falha do encoder em processo.
///
/// O `on_frame_arrived` devolve `Err` e a `windows-capture` encerra a captura;
/// sem guardar a mensagem, o sintoma que sobra é "não veio quadro nenhum" e a
/// causa se perde. É ela que aparece no log quando o caminho novo recua para o
/// `ffmpeg.exe`.
static ERRO_ENCODER: Mutex<Option<String>> = Mutex::new(None);

/// (entregues, convertidos) desde a última chamada de [`zera_contadores`].
#[must_use]
pub fn contadores() -> (u64, u64) {
    (
        ENTREGUES.load(Ordering::Relaxed),
        CONVERTIDOS.load(Ordering::Relaxed),
    )
}

/// A última falha do encoder em processo, se houve alguma.
#[must_use]
pub fn erro_do_encoder() -> Option<String> {
    ERRO_ENCODER.lock().ok().and_then(|e| e.clone())
}

/// Quadros que o encoder produziu e quem publica não conseguiu receber.
///
/// Diferente dos descartes por cadência: aqui o trabalho já foi feito e foi
/// jogado fora, o que significa que o consumidor não está dando conta.
#[must_use]
pub fn engasgados() -> u64 {
    ENGASGADOS.load(Ordering::Relaxed)
}

/// Voltas perdidas por atraso do relógio. Ver [`ATRASADOS`].
#[must_use]
pub fn atrasados() -> u64 {
    ATRASADOS.load(Ordering::Relaxed)
}

/// Quadros que o relógio repetiu por a tela estar parada.
#[must_use]
pub fn repetidos() -> u64 {
    REPETIDOS.load(Ordering::Relaxed)
}

/// Zera os contadores. Chamado ao iniciar uma captura.
pub fn zera_contadores() {
    ENTREGUES.store(0, Ordering::Relaxed);
    CONVERTIDOS.store(0, Ordering::Relaxed);
    ENGASGADOS.store(0, Ordering::Relaxed);
    REPETIDOS.store(0, Ordering::Relaxed);
    if let Ok(mut e) = ERRO_ENCODER.lock() {
        *e = None;
    }
}

/// Um quadro já codificado, saindo do encoder de hardware.
pub struct Codificado {
    pub bytes: Vec<u8>,
    /// Quando o quadro foi CAPTURADO, não quando saiu do encoder.
    ///
    /// O encoder segura quadros enquanto trabalha, então carimbar na saída
    /// embaralharia o tempo: quadros capturados em ritmo regular sairiam com
    /// intervalos irregulares, e a imagem aceleraria e frearia em relação ao
    /// áudio. Sem quadros B a ordem de saída é a de entrada, então uma fila
    /// simples de instantes basta para reencontrar o par.
    pub instante: Instant,
    pub chave: bool,
}

/// O canal e os controles do caminho que encoda na GPU.
///
/// A escada escreve em `kbps` de qualquer thread; o handler lê no quadro
/// seguinte e reconfigura o encoder se mudou. É deliberadamente frouxo: uma
/// mudança de taxa que só vale daqui a 16 ms não faz diferença nenhuma, e
/// evita travar a thread de callback num mutex.
pub struct Codificacao {
    pub envio: std::sync::mpsc::SyncSender<Codificado>,
    pub kbps: AtomicU32,
    pub fps: AtomicU32,
    /// Altura desejada da transmissão; `0` = mandar no tamanho da fonte.
    ///
    /// A escada escreve aqui. Reduzir acontece na GPU, pelo
    /// [`crate::escalador::Escalador`] — no caminho antigo quem reescalava era
    /// o `-vf scale` do ffmpeg, na CPU.
    pub altura_alvo: AtomicU32,
    /// Alguém pediu quadro-chave (PLI). Lido e limpo no próximo quadro.
    pub chave_pedida: AtomicBool,
    /// Onde depositar UMA amostra do primeiro quadro, em NV12.
    ///
    /// Custa uma leitura da GPU e serve para as conferências que já existem
    /// continuarem valendo neste caminho: a de captura de monitor toda preta
    /// (que pegou um defeito real de driver) e a de tamanho do quadro. Sem
    /// isto, ligar o caminho novo silenciaria as duas justamente em quem mais
    /// compartilha monitor.
    pub amostra: Arc<FrameBuffer>,
    /// O último quadro capturado, retido para o relógio poder repeti-lo.
    ///
    /// A textura que o WGC entrega só vale durante o callback, então guardamos
    /// uma cópia nossa. Uma cópia GPU→GPU por quadro, que é o que o caminho
    /// antigo já fazia — só que para a CPU, e caríssimo.
    pub retido: Mutex<Option<Retido>>,
    /// O dispositivo da captura, publicado no primeiro quadro para o relógio
    /// montar o encoder em cima dele.
    pub dispositivo: std::sync::OnceLock<Dispositivo>,
    /// Pedido de parada do relógio.
    pub parar: AtomicBool,
}

/// O par que o encoder e o escalador precisam.
///
/// Os dois campos são ponteiros com contagem de referência para objetos COM
/// que o D3D11 já serializa (o dispositivo recebe `SetMultithreadProtected` em
/// `Encoder::abre`), então atravessar para a thread do relógio é seguro.
pub struct Dispositivo(pub ID3D11Device, pub ID3D11DeviceContext);
unsafe impl Send for Dispositivo {}
unsafe impl Sync for Dispositivo {}

/// Um quadro capturado, guardado até o relógio pedir.
pub struct Retido {
    pub textura: ID3D11Texture2D,
    /// Tamanho da fonte, já cortado para par.
    pub fonte: (u32, u32),
    pub instante: Instant,
    /// Ainda não foi encodado. `false` quer dizer que o relógio vai repeti-lo.
    pub novo: bool,
}

// Mesma justificativa do `Dispositivo`.
unsafe impl Send for Retido {}
unsafe impl Sync for Retido {}

/// Para onde vai o quadro que o WGC entregou.
#[derive(Clone)]
pub enum Destino {
    /// Traz para a CPU, converte para NV12 e deposita no buffer que o pump lê.
    /// É o caminho que o `ffmpeg.exe` consome pelo pipe.
    Cpu(Arc<FrameBuffer>),
    /// Entrega a textura ao encoder de hardware, sem readback nem conversão.
    ///
    /// A variante existe sempre; o que depende da feature é conseguir
    /// atendê-la. Sem ela, `encoda` devolve erro e quem preparou a transmissão
    /// cai para o `ffmpeg.exe` — o mesmo recuo de uma máquina cujo encoder não
    /// aceita a textura.
    Gpu(Arc<Codificacao>),
}

/// O que a captura precisa saber de quem a inicia.
#[derive(Clone)]
pub struct Config {
    pub destino: Destino,
    /// Quadros por segundo que a transmissão vai realmente usar.
    ///
    /// Não é enfeite: o WGC entrega na taxa em que a TELA apresenta, que num
    /// monitor de 200 Hz com um jogo a 130 fps são 130 quadros por segundo.
    /// Sem isto convertíamos todos eles para usar 60 — mais que o dobro do
    /// trabalho, feito na thread do callback, disputando com o jogo.
    pub fps: u32,
}

impl GraphicsCaptureApiHandler for Handler {
    type Flags = Config;
    type Error = anyhow::Error;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let fps = ctx.flags.fps.max(1);
        Ok(Self {
            destino: ctx.flags.destino,
            scratch: Vec::new(),
            intervalo: intervalo_de_corte(fps),
            ultima: None,
            #[cfg(feature = "encoder-d3d11")]
            dispositivo: ctx.device,
            #[cfg(feature = "encoder-d3d11")]
            contexto: ctx.device_context,
            #[cfg(feature = "encoder-d3d11")]
            copia: None,
            #[cfg(feature = "encoder-d3d11")]
            amostrado: false,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        // Descarte antes de qualquer trabalho. O `MinimumUpdateInterval` já
        // pede ao Windows para segurar, mas ele é uma dica — depende da
        // versão e nem sempre é respeitado. Este corte é o que garante.
        ENTREGUES.fetch_add(1, Ordering::Relaxed);
        let agora = Instant::now();
        // No caminho da GPU o fps é da política e MUDA com a escada, então o
        // corte é recalculado a cada quadro (é uma divisão). No da CPU segue
        // o valor fixo de quando a captura subiu.
        let corte = match &self.destino {
            Destino::Gpu(c) => intervalo_de_corte(c.fps.load(Ordering::Relaxed)),
            Destino::Cpu(_) => self.intervalo,
        };
        if let Some(t) = self.ultima {
            if agora.duration_since(t) < corte {
                return Ok(());
            }
        }
        self.ultima = Some(agora);

        match self.destino.clone() {
            Destino::Cpu(buf) => {
                // O WGC entrega BGRA. Convertemos para NV12 aqui, antes do
                // pipe: 1080p60 em BGRA são ~500 MB/s; em NV12, ~190 MB/s
                // (R5 da spec).
                let fb = frame
                    .buffer()
                    .map_err(|e| anyhow::anyhow!("buffer do frame: {e}"))?;
                let (src_w, src_h) = (fb.width() as usize, fb.height() as usize);
                let (w, h) = even_dims(src_w, src_h);
                // `scratch` e `buf` são campos distintos do handler: o
                // empréstimo mutável de um não conflita com a leitura do outro.
                let nv12 = bgra_to_nv12(fb.as_nopadding_buffer(&mut self.scratch), src_w, src_h);
                // A resolução anunciada é a JÁ CORTADA para par, porque é essa
                // que descreve os bytes que vão para o pipe.
                buf.put(nv12, w as u32, h as u32);
                CONVERTIDOS.fetch_add(1, Ordering::Relaxed);
            }
            Destino::Gpu(cod) => {
                if let Err(e) = self.retem(frame, &cod, agora) {
                    // Guardar ANTES de propagar: quem propaga encerra a
                    // captura, e a partir daí ninguém mais passa por aqui.
                    if let Ok(mut slot) = ERRO_ENCODER.lock() {
                        *slot = Some(format!("{e:#}"));
                    }
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[cfg(not(feature = "encoder-d3d11"))]
impl Handler {
    /// Este binário foi compilado sem o encoder em processo.
    ///
    /// Não é um caminho de erro exótico: é a build que vai para as pessoas.
    /// Quem preparou a transmissão trata isto como qualquer outra recusa do
    /// caminho novo e usa o `ffmpeg.exe`.
    fn retem(
        &mut self,
        _frame: &mut Frame,
        _cod: &Codificacao,
        _agora: Instant,
    ) -> anyhow::Result<()> {
        anyhow::bail!("compilado sem o encoder em processo")
    }
}

#[cfg(feature = "encoder-d3d11")]
impl Handler {
    /// Guarda uma cópia da textura para o relógio encodar.
    ///
    /// # Por que só guardar
    ///
    /// O WGC entrega por evento: tela parada, nenhum quadro chega. Encodar
    /// aqui faria a transmissão simplesmente cessar quando ninguém mexe no
    /// computador — e quem entrasse na sala nesse momento não receberia nada,
    /// nem o quadro-chave que pediu por PLI. O caminho do `ffmpeg.exe` não
    /// tem esse problema porque o [`pump`] repete o último quadro na cadência;
    /// aqui quem repete é [`relogio`].
    ///
    /// De quebra, a thread de callback passa a fazer só uma cópia GPU→GPU. É a
    /// thread que disputa com o jogo, e cada coisa tirada dela é tempo que
    /// volta para quem está jogando.
    fn retem(
        &mut self,
        frame: &mut Frame,
        cod: &Codificacao,
        agora: Instant,
    ) -> anyhow::Result<()> {
        let desc = *frame.desc();
        // O H.264 exige dimensões pares. Cortar uma linha ou coluna é
        // invisível e mais barato que qualquer escala.
        let (largura, altura) = even_dims(desc.Width as usize, desc.Height as usize);

        // O dispositivo, uma vez, para o relógio montar o encoder em cima dele.
        let _ = cod
            .dispositivo
            .set(Dispositivo(self.dispositivo.clone(), self.contexto.clone()));

        // Uma leitura para a CPU, uma vez. Ver `Codificacao::amostra`.
        if !self.amostrado {
            self.amostrado = true;
            if let Ok(fb) = frame.buffer() {
                let (sw, sh) = (fb.width() as usize, fb.height() as usize);
                let nv12 = bgra_to_nv12(fb.as_nopadding_buffer(&mut self.scratch), sw, sh);
                let (w, h) = even_dims(sw, sh);
                cod.amostra.put(nv12, w as u32, h as u32);
            }
        }

        // A textura própria precisa do mesmo descritor da fonte: o
        // `CopyResource` exige formato e dimensões idênticos. Recriada quando a
        // janela é redimensionada.
        let precisa = self
            .copia
            .as_ref()
            .is_none_or(|(_, l, a)| *l != desc.Width || *a != desc.Height);
        if precisa {
            let d = D3D11_TEXTURE2D_DESC {
                BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
                Usage: D3D11_USAGE_DEFAULT,
                CPUAccessFlags: 0,
                MiscFlags: 0,
                ..desc
            };
            let mut t = None;
            unsafe { self.dispositivo.CreateTexture2D(&d, None, Some(&mut t))? };
            let t = t.context("CreateTexture2D não devolveu textura")?;
            self.copia = Some((t, desc.Width, desc.Height));
        }
        let Some((copia, _, _)) = self.copia.as_ref() else {
            anyhow::bail!("a textura de retenção sumiu entre criar e usar");
        };
        unsafe { self.contexto.CopyResource(copia, frame.as_raw_texture()) };

        if let Ok(mut slot) = cod.retido.lock() {
            *slot = Some(Retido {
                textura: copia.clone(),
                fonte: (largura as u32, altura as u32),
                instante: agora,
                novo: true,
            });
        }
        Ok(())
    }
}

/// Encoda na cadência alvo, repetindo o último quadro quando a tela para.
///
/// É o gêmeo de [`pump`] para o caminho da GPU, e existe pelo mesmo motivo: um
/// fluxo de vídeo precisa de quadros a intervalos regulares mesmo quando nada
/// muda na tela. Sem isto a transmissão cessa num desktop quieto, e o PLI de
/// quem acabou de entrar na sala fica sem resposta.
///
/// Roda em thread própria e é dona do encoder e do escalador — nada disso
/// atravessa para o callback do WGC, que só copia a textura.
/// Onde a próxima volta deve começar, e quantas se perderam no caminho.
///
/// `alvo` é o instante em que a próxima volta deveria começar segundo a
/// agenda; `agora`, quando de fato se terminou a anterior.
///
/// # Por que não é `sleep(período - decorrido)`
///
/// Porque aquilo é espera RELATIVA, e o `sleep` do Windows sempre estoura um
/// pouco — ~1 ms mesmo com o relógio fino ligado. Recomeçando a conta a partir
/// da hora em que se acordou, esse excesso ACUMULA: o período de 16,67 ms vira
/// ~17,5 ms e o que devia ser 60 fps sai 57. Foi medido ao vivo, com a captura
/// entregando 120 quadros por janela e o relógio convertendo 114.
///
/// Com uma agenda absoluta que anda de `período` em `período`, o excesso morre
/// na volta em que aconteceu em vez de empurrar todas as seguintes.
///
/// # Por que estouro pequeno não conta como perda
///
/// Porque descartar um quadro por 0,2 ms de atraso seria pior que o atraso.
/// Enquanto o desvio couber dentro de um período, a agenda absorve: a volta
/// seguinte sai imediatamente e a agenda não anda a mais. É o `clamped_diff`
/// do OBS (`video_sleep`, em `libobs/obs-video.c`), e foi por não ter isto que
/// a primeira versão desta função descartava quadro à toa.
///
/// # Por que não correr atrás do atraso grande
///
/// Quando já se perdeu uma volta inteira, a tentação é emendar voltas até
/// recuperar. Isso produziria uma RAJADA — exatamente o "trava e volta no ao
/// vivo de uma vez" que esta mudança existe para eliminar. Então pula-se para
/// a próxima fronteira e conta-se o que ficou pelo caminho.
fn proximo_alvo(alvo: Instant, agora: Instant, periodo: Duration) -> (Instant, u64) {
    let nanos = periodo.as_nanos().max(1);
    // O começo da volta que acabou de rodar. É dele que a agenda é medida —
    // medir de `agora` é justamente o erro que se está corrigindo.
    let Some(base) = alvo.checked_sub(periodo) else {
        return (agora + periodo, 0);
    };
    // Pelo menos um período, senão o estouro pequeno viraria descarte.
    let decorrido = agora.saturating_duration_since(base).max(periodo);
    let voltas = decorrido.as_nanos() / nanos;
    // Atraso absurdo (a máquina suspendeu) não vira aritmética gigante nem
    // rajada: recomeça a agenda a partir de agora.
    let Ok(n) = u32::try_from(voltas) else {
        return (agora + periodo, 0);
    };
    match periodo.checked_mul(n) {
        Some(salto) => (base + salto, u64::from(n) - 1),
        None => (agora + periodo, 0),
    }
}

#[cfg(feature = "encoder-d3d11")]
pub fn relogio(cod: &Codificacao) {
    // Sem isto o `sleep` do Windows tem granularidade de ~15 ms e a cadência de
    // 60 fps (16,7 ms) sai aos solavancos.
    let _fino = RelogioFino::novo();
    prioriza_thread_atual();

    let mut encoder: Option<encoder::Encoder> = None;
    let mut escalador: Option<crate::escalador::Escalador> = None;
    let mut instantes: VecDeque<Instant> = VecDeque::new();
    // A agenda. Anda de `período` em `período`, independente de quando cada
    // volta de fato acordou — ver `proximo_alvo`.
    let mut alvo = Instant::now();
    while !cod.parar.load(Ordering::Relaxed) {
        let fps = cod.fps.load(Ordering::Relaxed).max(1);
        // A escada troca de fps no meio da transmissão, então o período é lido
        // a cada volta; o alvo absoluto continua valendo com o período novo.
        let periodo = Duration::from_secs_f64(1.0 / f64::from(fps));

        if let Err(e) = uma_volta(cod, &mut encoder, &mut escalador, &mut instantes) {
            if let Ok(mut slot) = ERRO_ENCODER.lock() {
                *slot = Some(format!("{e:#}"));
            }
            return;
        }

        alvo += periodo;
        let (novo_alvo, perdidas) = proximo_alvo(alvo, Instant::now(), periodo);
        alvo = novo_alvo;
        if perdidas > 0 {
            ATRASADOS.fetch_add(perdidas, Ordering::Relaxed);
        }
        if let Some(resta) = alvo.checked_duration_since(Instant::now()) {
            std::thread::sleep(resta);
        }
    }
}

/// Uma volta do relógio: pega o quadro retido, encoda e escoa o que sair.
#[cfg(feature = "encoder-d3d11")]
fn uma_volta(
    cod: &Codificacao,
    encoder: &mut Option<encoder::Encoder>,
    escalador: &mut Option<crate::escalador::Escalador>,
    instantes: &mut VecDeque<Instant>,
) -> anyhow::Result<()> {
    // Copia o necessário e solta o mutex antes de encodar: segurá-lo durante o
    // encode faria o callback do WGC esperar por nós.
    let (textura, fonte, instante, novo) = {
        let Ok(mut slot) = cod.retido.lock() else {
            return Ok(());
        };
        let Some(r) = slot.as_mut() else {
            // Ainda não veio o primeiro quadro.
            return Ok(());
        };
        let era_novo = std::mem::replace(&mut r.novo, false);
        (r.textura.clone(), r.fonte, r.instante, era_novo)
    };
    let Some(Dispositivo(dispositivo, contexto)) = cod.dispositivo.get() else {
        return Ok(());
    };

    // O carimbo é a HORA DESTA VOLTA, sempre — inclusive para quadro fresco.
    //
    // Usar o instante da captura parecia mais fiel e não é: o callback do WGC
    // pega a hora na entrada e só guarda o quadro no fim, então se ele demora,
    // o relógio já repetiu um quadro com hora mais nova no meio-tempo. O quadro
    // fresco sairia com carimbo ANTERIOR ao que já foi enviado.
    //
    // Um receptor de WebRTC lê isso como reordenação, que em H.264 só existe
    // com quadros B — e o MediaMTX derruba a sessão com "WebRTC doesn't support
    // H264 streams with B-frames". Foi assim que descobrimos: espectadores
    // caindo depois de alguns segundos, sem nada errado do nosso lado.
    //
    // A hora da volta é monotônica por construção, e continua sendo tempo real:
    // o relógio emite um quadro por período, então é ele mesmo a apresentação.
    let carimbo = Instant::now();
    let _ = instante;
    if novo {
        CONVERTIDOS.fetch_add(1, Ordering::Relaxed);
    } else {
        REPETIDOS.fetch_add(1, Ordering::Relaxed);
    }

    let saida =
        crate::escalador::reduz_para(fonte.0, fonte.1, cod.altura_alvo.load(Ordering::Relaxed))
            .unwrap_or(fonte);
    let alvo = encoder::Ajuste {
        largura: saida.0,
        altura: saida.1,
        fps: cod.fps.load(Ordering::Relaxed).max(1),
        kbps: cod.kbps.load(Ordering::Relaxed),
    };

    // A resolução muda quando a janela é redimensionada; a taxa, quando a
    // escada decide. Os dois casos são a mesma reconfiguração.
    match encoder.as_mut() {
        Some(e) if e.ajuste() != alvo => e.reconfigura(alvo)?,
        Some(_) => {}
        None => *encoder = Some(encoder::Encoder::abre(dispositivo, contexto, alvo)?),
    }
    let Some(e) = encoder.as_mut() else {
        return Ok(());
    };

    if cod.chave_pedida.swap(false, Ordering::Relaxed) {
        e.pede_chave();
    }
    // A cópia direta só serve quando não há nada a converter: o `CopyResource`
    // move bytes, não muda formato. Com o QSV o pool guarda NV12 e a captura
    // entrega BGRA, então mesmo em resolução nativa quem escreve é o
    // processador de vídeo, que converte a cor de passagem.
    if saida == fonte && !e.converte_cor() {
        // Cópia direta, e o recorte só apara a linha ou coluna ímpar que o
        // H.264 não aceita.
        e.envia(&textura, 0, Some((alvo.largura, alvo.altura)))?;
    } else {
        // O escalador é preso ao par (fonte, saída); trocar de degrau troca o
        // par e exige outro.
        if escalador.as_ref().map(crate::escalador::Escalador::par) != Some((fonte, saida)) {
            *escalador = Some(crate::escalador::Escalador::novo(
                dispositivo,
                contexto,
                fonte,
                saida,
            )?);
        }
        let atual = escalador.as_ref();
        e.preenche(|destino, camada| match atual {
            Some(s) => s.aplica(&textura, 0, destino, camada),
            None => anyhow::bail!("escalador sumiu entre criar e usar"),
        })?;
    }
    instantes.push_back(carimbo);

    e.drena(|s| {
        // Sem quadros B a saída sai na ordem da entrada, então o instante do
        // quadro mais antigo ainda na fila é o deste pacote. O `unwrap_or` é
        // rede de segurança: se algum dia a ordem mudar, vale mais carimbar com
        // o relógio de agora que travar.
        let instante = instantes.pop_front().unwrap_or(carimbo);
        // `try_send`: se quem publica está atrasado, descartar é melhor do que
        // segurar o relógio e atrasar todos os quadros seguintes.
        if cod
            .envio
            .try_send(Codificado {
                bytes: s.bytes.to_vec(),
                instante,
                chave: s.chave,
            })
            .is_err()
        {
            ENGASGADOS.fetch_add(1, Ordering::Relaxed);
        }
    })?;
    Ok(())
}

/// Dimensões efetivas da conversão, sempre pares.
///
/// NV12 é 4:2:0: cada par de linhas e colunas divide uma amostra de croma. Uma
/// janela pode perfeitamente ter 1279x721, e nesse caso o plano de croma não
/// fecha — o último bloco escreveria fora do buffer, e o H.264 em yuv420p
/// recusaria a resolução de qualquer forma. Cortar o pixel ímpar da borda é a
/// saída barata e correta.
#[must_use]
pub fn even_dims(w: usize, h: usize) -> (usize, usize) {
    (w & !1, h & !1)
}

/// Preto em NV12 (BT.601 limited range): luma no piso, croma neutro.
const PRETO_Y: u8 = 16;
const PRETO_UV: u8 = 128;

/// Ajusta um frame NV12 para a resolução exata que o ffmpeg espera.
///
/// # Por que isto existe
///
/// O ffmpeg recebe `-s <largura>x<altura>` **uma vez**, na partida, e a partir
/// daí lê blocos de tamanho fixo do pipe. A fonte, porém, pode mudar de tamanho
/// no meio da transmissão — basta a pessoa pôr um vídeo em tela cheia, ou
/// redimensionar a janela compartilhada.
///
/// Se um frame de outro tamanho for escrito no pipe, todos os blocos seguintes
/// ficam deslocados: a luminância ainda casa mais ou menos, mas os planos de
/// croma escorregam e a imagem vira magenta e verde com um rasgo diagonal.
///
/// Aqui o frame é reescalado mantendo a proporção e centralizado sobre fundo
/// preto. Distorcer para preencher seria mais simples, mas deixaria as pessoas
/// achatadas quando a janela mudasse de formato.
pub fn ajusta_para(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize, dst: &mut Vec<u8>) {
    let (dw, dh) = even_dims(dw, dh);
    let destino_y = dw * dh;
    dst.clear();
    dst.resize(destino_y + destino_y / 2, 0);

    // Caminho rápido e esmagadoramente mais comum: já é do tamanho certo.
    if sw == dw && sh == dh && src.len() == dst.len() {
        dst.copy_from_slice(src);
        return;
    }

    // Fundo preto, para as faixas que sobrarem nas bordas.
    dst[..destino_y].fill(PRETO_Y);
    dst[destino_y..].fill(PRETO_UV);

    let (sw, sh) = even_dims(sw, sh);
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 || src.len() < sw * sh * 3 / 2 {
        return; // Frame inutilizável: fica o preto, que ao menos mantém o ritmo.
    }

    // Maior retângulo com a proporção da origem que cabe no destino, com
    // origem e tamanho pares — NV12 amostra o croma de dois em dois pixels.
    let escala = f64::min(dw as f64 / sw as f64, dh as f64 / sh as f64);
    let (aw, ah) = even_dims(
        ((sw as f64 * escala) as usize).max(2).min(dw),
        ((sh as f64 * escala) as usize).max(2).min(dh),
    );
    let (ox, oy) = even_dims((dw - aw) / 2, (dh - ah) / 2);

    let origem_y = sw * sh;
    let destino_uv = destino_y;

    // Luma, vizinho mais próximo. Interpolação bilinear renderia mais nitidez,
    // mas isto roda por frame no caminho quente e o caso é excepcional — só
    // acontece enquanto a fonte está com tamanho diferente do combinado.
    for y in 0..ah {
        let sy = y * sh / ah;
        let linha_src = sy * sw;
        let linha_dst = (oy + y) * dw + ox;
        for x in 0..aw {
            dst[linha_dst + x] = src[linha_src + x * sw / aw];
        }
    }

    // Croma: metade da resolução, com U e V intercalados. Os índices andam de
    // dois em dois porque cada posição carrega o par.
    let (cw, ch) = (aw / 2, ah / 2);
    let (scw, sch) = (sw / 2, sh / 2);
    for y in 0..ch {
        let sy = y * sch / ch;
        for x in 0..cw {
            let sx = x * scw / cw;
            let s = origem_y + (sy * scw + sx) * 2;
            let d = destino_uv + ((oy / 2 + y) * (dw / 2) + (ox / 2 + x)) * 2;
            dst[d] = src[s];
            dst[d + 1] = src[s + 1];
        }
    }
}

/// BGRA8 -> NV12 (BT.601 limited range), com subamostragem 2x2 do croma.
///
/// `src_w`/`src_h` são as dimensões do buffer de origem; a saída usa
/// [`even_dims`] delas, logo tem exatamente `w * h * 3 / 2` bytes.
#[must_use]
pub fn bgra_to_nv12(bgra: &[u8], src_w: usize, src_h: usize) -> Vec<u8> {
    let (w, h) = even_dims(src_w, src_h);
    let y_size = w * h;
    let mut out = vec![0u8; y_size + y_size / 2];
    if y_size == 0 || bgra.len() < src_w * src_h * 4 {
        // Frame truncado: devolve NV12 preto no tamanho certo em vez de entrar
        // em pânico. O pipe continua alimentado e o ffmpeg não recebe um frame
        // de tamanho errado, que o dessincronizaria de vez.
        return out;
    }

    let (plano_y, plano_uv) = out.split_at_mut(y_size);
    let bandas = divide_em_bandas(h);

    if bandas == 1 {
        converte_banda(bgra, src_w, w, 0, h, plano_y, plano_uv);
        return out;
    }

    // Cada banda escreve numa fatia própria dos dois planos e lê uma região
    // disjunta da origem, então não há sincronização nenhuma no caminho quente.
    // `scope` garante que todas terminem antes de `out` ser devolvido.
    let linhas = linhas_por_banda(h, bandas);
    std::thread::scope(|escopo| {
        let mut resto_y = &mut plano_y[..];
        let mut resto_uv = &mut plano_uv[..];
        let mut y0 = 0;
        while y0 < h {
            let altura = linhas.min(h - y0);
            let (fatia_y, r_y) = resto_y.split_at_mut(altura * w);
            let (fatia_uv, r_uv) = resto_uv.split_at_mut(altura / 2 * w);
            resto_y = r_y;
            resto_uv = r_uv;
            escopo.spawn(move || {
                converte_banda(bgra, src_w, w, y0, altura, fatia_y, fatia_uv);
            });
            y0 += altura;
        }
    });

    out
}

/// Quantas threads usar na conversão.
///
/// **Não** usa todos os núcleos, por dois motivos independentes.
///
/// O primeiro é orçamento: quem não tem encoder por hardware cai no `libx264`,
/// que é justamente quem mais precisa de CPU. Tomar a máquina inteira para
/// converter cor faria o encode atrasar e a transmissão engasgar.
///
/// O segundo é que **não adiantaria**. Medido a 1080p60 numa máquina de 8
/// núcleos (mínimo de 8 rodadas, por thread):
///
/// | threads | ms/frame |
/// |---|---|
/// | 1 | 6,03 |
/// | 2 | 3,73 |
/// | 4 | 3,08 |
/// | 6 | 3,49 |
/// | 8 | 4,44 |
///
/// Estabiliza em 2x e depois regride: a conversão é limitada por **largura de
/// banda de memória**, não por aritmética (8 MB de BGRA lidos por quadro, mais
/// 3 MB escritos). Passar de 4 threads só aumenta disputa por cache.
///
/// O mesmo dado explica por que não há SIMD aqui: vetorizar acelera cálculo, e
/// cálculo não é o gargalo. Quem quiser eliminar esse custo de verdade precisa
/// tirar o quadro do caminho da CPU — converter na GPU e alimentar o encoder
/// com a textura.
fn divide_em_bandas(h: usize) -> usize {
    // Override para medir escalabilidade sem recompilar. Útil para descobrir
    // se o limite é cálculo ou memória: se dobrar as threads não melhora, é
    // memória, e otimizar aritmética (SIMD) não adiantaria.
    if let Some(n) = std::env::var("TELA_THREADS_CONVERSAO")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
    {
        return n.min(h / 2).max(1);
    }
    let nucleos = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    let alvo = (nucleos / 2).clamp(1, 4);
    // Banda pequena demais não paga o custo de criar a thread.
    alvo.min(h / 128).max(1)
}

/// Altura de cada banda, sempre **par**: o croma é amostrado de dois em dois
/// pixels, e uma banda de altura ímpar partiria um bloco 2x2 entre threads.
fn linhas_por_banda(h: usize, bandas: usize) -> usize {
    (h.div_ceil(bandas) + 1) & !1
}

/// Converte as linhas `[y0, y0 + altura)` da origem para as fatias dadas.
///
/// `fatia_y` e `fatia_uv` já apontam para o pedaço certo dos planos de destino,
/// então os índices aqui dentro são relativos à banda.
fn converte_banda(
    bgra: &[u8],
    src_w: usize,
    w: usize,
    y0: usize,
    altura: usize,
    fatia_y: &mut [u8],
    fatia_uv: &mut [u8],
) {
    // O passo de linha da origem é `src_w`, não `w`: quando a largura é ímpar,
    // lemos com o passo original e descartamos a última coluna.
    for y in 0..altura {
        let linha_src = (y0 + y) * src_w;
        let linha_dst = y * w;
        for x in 0..w {
            let p = (linha_src + x) * 4;
            let (b, g, r) = (
                i32::from(bgra[p]),
                i32::from(bgra[p + 1]),
                i32::from(bgra[p + 2]),
            );
            fatia_y[linha_dst + x] =
                ((66 * r + 129 * g + 25 * b + 128) >> 8).clamp(0, 219) as u8 + 16;
        }
    }

    // `w` e a altura da banda são pares, então todo bloco 2x2 cabe inteiro.
    for y in (0..altura).step_by(2) {
        let linha_uv = (y / 2) * w;
        for x in (0..w).step_by(2) {
            let mut sb = 0i32;
            let mut sg = 0i32;
            let mut sr = 0i32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let p = ((y0 + y + dy) * src_w + (x + dx)) * 4;
                    sb += i32::from(bgra[p]);
                    sg += i32::from(bgra[p + 1]);
                    sr += i32::from(bgra[p + 2]);
                }
            }
            let (b, g, r) = (sb / 4, sg / 4, sr / 4);
            let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
            let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
            fatia_uv[linha_uv + x] = u.clamp(0, 255) as u8;
            fatia_uv[linha_uv + x + 1] = v.clamp(0, 255) as u8;
        }
    }
}

/// Ajustes comuns a toda captura: cursor visível e BGRA8, que é o formato que
/// a conversão para NV12 espera.
///
/// Intervalo e borda vêm de fora porque nem todo Windows aceita os dois — ver
/// [`inicia_captura!`].
///
/// É macro e não função porque `Settings` é genérico sobre o tipo do item
/// (`Monitor` ou `Window`) e cada chamada instancia um genérico diferente.
macro_rules! capture_settings {
    ($item:expr, $flags:expr, $intervalo:expr, $borda:expr) => {
        Settings::new(
            $item,
            CursorCaptureSettings::WithCursor,
            $borda,
            SecondaryWindowSettings::Default,
            $intervalo,
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            $flags,
        )
    };
}

/// Pede ao Windows para não entregar quadro mais rápido que o necessário.
///
/// Cada quadro entregue custa cópia da GPU e passa pelo caminho de composição
/// do desktop; pedir 200 por segundo para usar 60 tira quadro do jogo de quem
/// está compartilhando.
fn intervalo_minimo(fps: u32) -> MinimumUpdateIntervalSettings {
    MinimumUpdateIntervalSettings::Custom(Duration::from_secs_f64(0.9 / f64::from(fps.max(1))))
}

/// A falha foi por este Windows não ter o recurso pedido?
///
/// Só esse caso merece nova tentativa. Uma janela que fechou, um monitor
/// desconectado ou permissão negada não melhoram tentando de novo — e mascarar
/// esses erros com uma segunda tentativa esconderia a causa real.
///
/// A comparação é pela mensagem porque a biblioteca não expõe código de erro.
/// Casa com QUALQUER recurso recusado por falta de suporte, e não com um
/// nome específico:
///
/// - "Setting a minimum update interval is not supported by the Graphics
///   Capture API on this platform." (medida num build 22631)
/// - "Toggling the capture border is not supported by the Graphics Capture
///   API on this platform." (medida num build 19045, Windows 10 22H2)
///
/// A versão anterior citava só o primeiro. Quando alguém em Windows 10 entrou,
/// a mesma classe de problema derrubou a captura dele inteira — o mesmo defeito
/// pela segunda vez, com outro nome de recurso. Por isso agora é genérico.
fn e_falta_de_suporte(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("not supported") && m.contains("graphics capture api")
}

/// Degradações que o Windows de quem transmite impôs, para o app registrar.
///
/// A captura roda noutro crate e não alcança o log do app; sem isto, a única
/// pista de que algo foi desligado era um `eprintln!` que ninguém vê. Foi
/// exatamente o que faltou para entender por que uma máquina não transmitia.
static AVISOS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn anota_aviso(texto: &str) {
    eprintln!("{texto}");
    if let Ok(mut v) = AVISOS.lock() {
        if v.len() < 8 {
            v.push(texto.to_string());
        }
    }
}

/// Recolhe e limpa os avisos acumulados desde a última chamada.
#[must_use]
pub fn avisos() -> Vec<String> {
    AVISOS
        .lock()
        .map(|mut v| std::mem::take(&mut *v))
        .unwrap_or_default()
}

/// Inicia a captura desligando um recurso de cada vez até o Windows aceitar.
///
/// # Por que uma escada e não uma tentativa
///
/// Dois ajustes que pedimos não existem em todo Windows, e a biblioteca não
/// degrada: o `start` falha INTEIRO, e a pessoa simplesmente não transmite.
///
/// - **Afunilar a entrega de quadros** exige build 26100 (Windows 11 24H2).
/// - **Tirar a borda de destaque** exige Windows 11; no 10 nem existe.
///
/// Isso já derrubou duas pessoas, em momentos diferentes: uma no build 22631
/// (afunilamento) e outra no 19045, Windows 10 (borda). O conserto da primeira
/// tratou só o afunilamento, então a segunda quebrou igual. Agora a escada
/// cobre as combinações, e `e_falta_de_suporte` casa com qualquer recurso
/// recusado por falta de suporte — inclusive um que ainda não conhecemos.
///
/// Detecção por TENTATIVA e não por número de versão: é o comportamento real
/// que interessa, e checar build erra sempre que a Microsoft mexe na
/// numeração.
///
/// O que se perde em cada degrau é pequeno: sem afunilamento a captura entrega
/// mais quadro do que o necessário e o excedente é descartado no callback;
/// sem o desligamento da borda, quem transmite vê a moldura amarela do Windows
/// em volta da própria tela. Os dois são muito melhores que não transmitir.
macro_rules! inicia_captura {
    ($item:expr, $cfg:expr, $o_que:literal) => {{
        let fps = $cfg.fps;
        // Do mais desejável para o mais tolerante.
        let degraus: [(MinimumUpdateIntervalSettings, DrawBorderSettings, &str); 4] = [
            (intervalo_minimo(fps), DrawBorderSettings::WithoutBorder, ""),
            (
                MinimumUpdateIntervalSettings::Default,
                DrawBorderSettings::WithoutBorder,
                "este Windows não suporta afunilar a entrega de quadros; seguindo sem",
            ),
            (
                intervalo_minimo(fps),
                DrawBorderSettings::Default,
                "este Windows não deixa tirar a borda da captura; seguindo com ela",
            ),
            (
                MinimumUpdateIntervalSettings::Default,
                DrawBorderSettings::Default,
                "este Windows não suporta afunilar quadros nem tirar a borda; seguindo sem os dois",
            ),
        ];

        let mut ultimo = None;
        let mut saida = Err(anyhow::anyhow!(concat!("captura ", $o_que, " falhou")));
        for (intervalo, borda, aviso) in degraus {
            match Handler::start_free_threaded(capture_settings!(
                $item.clone(),
                $cfg.clone(),
                intervalo,
                borda
            )) {
                Ok(c) => {
                    if !aviso.is_empty() {
                        anota_aviso(aviso);
                    }
                    saida = Ok(Captura(c));
                    break;
                }
                Err(e) => {
                    let msg = e.to_string();
                    // Erro que não é falta de suporte para de vez: janela
                    // fechada ou permissão negada não melhoram no degrau
                    // seguinte, e insistir esconderia a causa real.
                    if !e_falta_de_suporte(&msg) {
                        return Err(anyhow::anyhow!(
                            concat!("captura ", $o_que, " falhou: {}"),
                            e
                        ));
                    }
                    ultimo = Some(msg);
                }
            }
        }
        if let (Err(_), Some(msg)) = (&saida, ultimo) {
            saida = Err(anyhow::anyhow!(
                concat!("captura ", $o_que, " falhou: {}"),
                msg
            ));
        }
        saida
    }};
}

/// Uma sessão de captura viva, rodando na própria thread.
///
/// Existe porque o app precisa **parar** a captura sem encerrar o processo:
/// quem transmite escolhe uma fonte, para, escolhe outra, e cada sessão
/// abandonada continuaria convertendo frames para NV12 para sempre. O CLI
/// sobrevivia sem isso só porque morria logo depois.
///
/// Encerrar de verdade exige [`Captura::encerra`] e não um sinalizador lido no
/// callback: com a tela parada o WGC não entrega frame nenhum, então um `stop`
/// checado em `on_frame_arrived` poderia nunca ser lido. O `stop()` da
/// `windows-capture` posta `WM_QUIT` no laço de mensagens da thread, que
/// funciona mesmo com a tela imóvel.
pub struct Captura(CaptureControl<Handler, anyhow::Error>);

impl Captura {
    /// A captura terminou sozinha (janela fechada, monitor desconectado).
    #[must_use]
    pub fn terminou(&self) -> bool {
        self.0.is_finished()
    }

    /// Pede a parada e espera a thread da captura sair.
    pub fn encerra(self) -> Result<()> {
        self.0
            .stop()
            .map_err(|e| anyhow::anyhow!("encerrar captura: {e}"))
    }
}

/// Inicia a captura de um monitor. `None` = monitor primário.
///
/// O índice é o mesmo que [`list_monitors`] reporta, e a busca é feita
/// comparando `index()` item a item em vez de usar `Monitor::from_index`: o
/// primeiro vem do nome do dispositivo (`\\.\DISPLAY3`) e o segundo é a
/// posição na enumeração. Com monitores desconectados no meio, os dois
/// divergem — e capturar o monitor errado é exatamente o tipo de vazamento
/// que este produto não pode ter.
pub fn start_monitor(indice: Option<usize>, cfg: Config) -> Result<Captura> {
    zera_contadores();
    let monitor = match indice {
        None => Monitor::primary().context("monitor primário")?,
        Some(i) => Monitor::enumerate()
            .context("enumerar monitores")?
            .into_iter()
            .find(|m| m.index().is_ok_and(|n| n == i))
            .with_context(|| format!("monitor de índice {i} não existe mais"))?,
    };
    inicia_captura!(monitor, cfg, "do monitor")
}

/// Inicia a captura de uma janela pelo HWND.
///
/// Recebe HWND e não título de propósito: o chamador já resolveu a janela com
/// [`find_window`], e o áudio usa o PID daquele mesmo registro.
pub fn start_window(hwnd: isize, cfg: Config) -> Result<Captura> {
    zera_contadores();
    let window = Window::from_raw_hwnd(hwnd as *mut std::ffi::c_void);
    anyhow::ensure!(
        window.is_valid(),
        "janela não é mais capturável (hwnd={hwnd})"
    );

    // Janela minimizada não é desenhada pelo Windows, e o WGC fica esperando um
    // quadro que nunca vem — o sintoma é um "nenhum frame em 5 s" que não
    // explica nada. Restaurar é o que a pessoa faria de qualquer forma para
    // compartilhar.
    let estado = estado_da_janela(hwnd);
    if estado.minimizada {
        restaura_janela(hwnd);
        // O DWM leva alguns quadros para voltar a compor a janela.
        std::thread::sleep(Duration::from_millis(400));
    }
    if let Some(motivo) = estado_da_janela(hwnd).porque_nao() {
        anyhow::bail!("não dá para capturar esta janela: {motivo}");
    }
    inicia_captura!(window, cfg, "da janela")
}

#[cfg(test)]
mod testes_ajuste {
    use super::*;

    /// Tamanho de saída é o que o ffmpeg espera. Qualquer entrada, MESMO
    /// tamanho de saída — é a invariante que impede a dessincronia do pipe.
    #[test]
    fn saida_tem_sempre_o_tamanho_combinado() {
        let mut dst = Vec::new();
        for (sw, sh) in [
            (1920, 1080), // igual
            (1280, 720),  // menor
            (2560, 1440), // maior
            (1920, 1200), // outra proporção
            (640, 480),   // 4:3
            (1279, 721),  // ímpar dos dois lados
            (2, 2),       // mínimo
        ] {
            let src = vec![128u8; sw * sh * 3 / 2];
            ajusta_para(&src, sw, sh, 1920, 1080, &mut dst);
            assert_eq!(
                dst.len(),
                1920 * 1080 * 3 / 2,
                "entrada {sw}x{sh} produziu {} bytes",
                dst.len()
            );
        }
    }

    /// Frame truncado ou vazio não pode entrar em pânico nem produzir tamanho
    /// errado: o pipe precisa continuar alimentado.
    #[test]
    fn entrada_ruim_vira_preto_do_tamanho_certo() {
        let mut dst = Vec::new();
        for src in [vec![], vec![0u8; 10], vec![255u8; 100]] {
            ajusta_para(&src, 1920, 1080, 640, 480, &mut dst);
            assert_eq!(dst.len(), 640 * 480 * 3 / 2);
            assert_eq!(dst[0], PRETO_Y, "luma deveria ser preto");
            assert_eq!(dst[640 * 480], PRETO_UV, "croma deveria ser neutro");
        }
    }

    /// Mesma proporção: a imagem preenche tudo, sem faixa preta.
    #[test]
    fn mesma_proporcao_nao_deixa_borda() {
        let (sw, sh) = (1280, 720);
        let src = vec![200u8; sw * sh * 3 / 2];
        let mut dst = Vec::new();
        ajusta_para(&src, sw, sh, 1920, 1080, &mut dst);
        // Canto superior esquerdo e inferior direito do plano de luma vieram da
        // origem, não do preenchimento.
        assert_eq!(dst[0], 200);
        assert_eq!(dst[1920 * 1080 - 1], 200);
    }

    /// Proporção diferente: sobra faixa preta, e a imagem fica centralizada em
    /// vez de esticada.
    #[test]
    fn proporcao_diferente_gera_faixa_preta_centralizada() {
        let (sw, sh) = (640, 480); // 4:3 dentro de 16:9
        let src = vec![200u8; sw * sh * 3 / 2];
        let mut dst = Vec::new();
        ajusta_para(&src, sw, sh, 1920, 1080, &mut dst);

        // Borda esquerda: preta. Centro da primeira linha útil: imagem.
        assert_eq!(dst[0], PRETO_Y, "canto esquerdo deveria ser faixa preta");
        let meio = (1080 / 2) * 1920 + 1920 / 2;
        assert_eq!(dst[meio], 200, "centro deveria ter imagem");
    }

    /// O caminho rápido (mesmo tamanho) tem de copiar exatamente, sem reescala.
    #[test]
    fn tamanho_igual_copia_identico() {
        let (w, h) = (64, 48);
        let src: Vec<u8> = (0..w * h * 3 / 2).map(|i| (i % 251) as u8).collect();
        let mut dst = Vec::new();
        ajusta_para(&src, w, h, w, h, &mut dst);
        assert_eq!(dst, src);
    }
}

#[cfg(test)]
mod testes_conversao {
    use super::*;

    /// Implementação de referência: sequencial, escrita do jeito mais direto
    /// possível. Existe só para a versão paralela ter contra o que ser
    /// comparada — se as duas divergirem, é a paralela que está errada.
    fn referencia(bgra: &[u8], src_w: usize, src_h: usize) -> Vec<u8> {
        let (w, h) = even_dims(src_w, src_h);
        let y_size = w * h;
        let mut out = vec![0u8; y_size + y_size / 2];
        if y_size == 0 || bgra.len() < src_w * src_h * 4 {
            return out;
        }
        for y in 0..h {
            for x in 0..w {
                let p = (y * src_w + x) * 4;
                let (b, g, r) = (
                    i32::from(bgra[p]),
                    i32::from(bgra[p + 1]),
                    i32::from(bgra[p + 2]),
                );
                out[y * w + x] = ((66 * r + 129 * g + 25 * b + 128) >> 8).clamp(0, 219) as u8 + 16;
            }
        }
        for y in (0..h).step_by(2) {
            for x in (0..w).step_by(2) {
                let (mut sb, mut sg, mut sr) = (0i32, 0i32, 0i32);
                for dy in 0..2 {
                    for dx in 0..2 {
                        let p = ((y + dy) * src_w + (x + dx)) * 4;
                        sb += i32::from(bgra[p]);
                        sg += i32::from(bgra[p + 1]);
                        sr += i32::from(bgra[p + 2]);
                    }
                }
                let (b, g, r) = (sb / 4, sg / 4, sr / 4);
                let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                let idx = y_size + (y / 2) * w + x;
                out[idx] = u.clamp(0, 255) as u8;
                out[idx + 1] = v.clamp(0, 255) as u8;
            }
        }
        out
    }

    /// Padrão pseudoaleatório determinístico: cor achatada esconderia erro de
    /// índice, porque qualquer troca de pixel daria o mesmo byte.
    fn imagem(w: usize, h: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(w * h * 4);
        let mut estado = 0x9E37_79B9u32;
        for _ in 0..w * h * 4 {
            estado = estado.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            v.push((estado >> 24) as u8);
        }
        v
    }

    /// A invariante que importa: paralelizar não pode mudar um único byte.
    #[test]
    fn paralelo_bate_com_a_referencia() {
        for (w, h) in [
            (1920, 1080), // caso real, várias bandas
            (2560, 1440),
            (640, 480),
            (320, 200),
            (16, 16), // pequeno demais para valer thread: cai no caminho serial
            (2, 2),
            (1279, 721), // ímpar nos dois lados
            (100, 3),    // altura ímpar e menor que uma banda
        ] {
            let src = imagem(w, h);
            assert_eq!(
                bgra_to_nv12(&src, w, h),
                referencia(&src, w, h),
                "divergiu em {w}x{h}"
            );
        }
    }

    /// Bandas de altura ímpar partiriam um bloco 2x2 do croma entre threads.
    #[test]
    fn bandas_tem_altura_par() {
        for h in [2usize, 4, 100, 1080, 1440, 721] {
            for bandas in 1..=6 {
                let linhas = linhas_por_banda(h, bandas);
                assert_eq!(linhas % 2, 0, "h={h} bandas={bandas} deu {linhas}");
                assert!(linhas > 0);
            }
        }
    }

    /// Entrada truncada continua devolvendo preto do tamanho certo, sem pânico.
    #[test]
    fn entrada_truncada_nao_entra_em_panico() {
        let out = bgra_to_nv12(&[1, 2, 3], 1920, 1080);
        assert_eq!(out.len(), 1920 * 1080 * 3 / 2);
        assert!(out.iter().all(|&b| b == 0));
    }
}

/// Luma máxima que ainda conta como preto.
///
/// Preto em NV12 limited range é 16. A folga cobre ruído de compressão e
/// gradientes quase pretos que um degradê de fundo poderia produzir.
const LIMIAR_PRETO: u8 = 20;

/// Diz se o quadro é essencialmente preto.
///
/// # Para que serve
///
/// Em notebook com placa híbrida, a captura de **monitor** só funciona na GPU
/// que faz a saída final de vídeo — em geral a integrada. Se o app estiver
/// rodando na dedicada, o WGC entrega quadros válidos e completamente pretos,
/// sem erro nenhum: a transmissão sobe, o servidor recebe, e quem assiste vê
/// uma tela preta. Captura de janela não sofre disso.
///
/// Sem esta checagem a falha é silenciosa dos dois lados. Com ela, dá para
/// dizer à pessoa o que aconteceu e como resolver.
///
/// Amostra em vez de varrer tudo: a 1080p são 2 milhões de pixels, e uma
/// amostragem esparsa responde a mesma pergunta por uma fração do custo.
#[must_use]
pub fn parece_preto(nv12: &[u8], w: usize, h: usize) -> bool {
    let y_size = w * h;
    if y_size == 0 || nv12.len() < y_size {
        return false; // Sem quadro não se afirma nada.
    }
    // Passo primo para a amostragem não cair sempre na mesma coluna, o que
    // deixaria passar uma imagem com barras verticais pretas.
    let passo = (y_size / 4096).max(1) | 1;
    nv12[..y_size]
        .iter()
        .step_by(passo)
        .all(|&y| y <= LIMIAR_PRETO)
}

#[cfg(test)]
mod testes_preto {
    use super::*;

    #[test]
    fn preto_e_reconhecido() {
        let (w, h) = (1920, 1080);
        let mut f = vec![128u8; w * h * 3 / 2];
        f[..w * h].fill(16); // luma no piso, croma neutro
        assert!(parece_preto(&f, w, h));
    }

    #[test]
    fn imagem_com_conteudo_nao_e_preta() {
        let (w, h) = (1920, 1080);
        let mut f = vec![128u8; w * h * 3 / 2];
        f[..w * h].fill(16);
        // Uma faixa clara no meio já basta: não é uma tela preta.
        let meio = (h / 2) * w;
        f[meio..meio + w].fill(200);
        assert!(!parece_preto(&f, w, h));
    }

    #[test]
    fn quadro_ausente_nao_afirma_nada() {
        assert!(!parece_preto(&[], 1920, 1080));
        assert!(!parece_preto(&[16; 10], 1920, 1080));
    }
}

/// Estado de uma janela, para decidir se vale oferecê-la e para explicar
/// falhas de captura.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EstadoJanela {
    pub visivel: bool,
    /// Ocultada pelo compositor do Windows (DWM). É o caso dos hosts de
    /// aplicativos UWP suspensos: continuam "visíveis" para a API antiga, mas
    /// não são compostos e portanto não geram quadro nenhum.
    pub oculta_pelo_dwm: bool,
    pub minimizada: bool,
    pub largura: i32,
    pub altura: i32,
}

impl EstadoJanela {
    /// Se faz sentido oferecer esta janela para captura.
    ///
    /// Sem esta checagem a lista enche de fantasmas — `SystemSettings.exe`,
    /// `ApplicationFrameHost.exe`, `TextInputHost.exe` — que estão sempre em
    /// execução com janelas invisíveis. A pessoa vê "Configurações" duas vezes
    /// sem ter aberto Configurações nenhuma, escolhe, e a captura falha.
    #[must_use]
    pub fn capturavel(&self) -> bool {
        self.visivel
            && !self.oculta_pelo_dwm
            && !self.minimizada
            && self.largura > 1
            && self.altura > 1
    }

    /// Se vale oferecer esta janela na lista de fontes.
    ///
    /// É [`EstadoJanela::capturavel`] com uma exceção: minimizada FICA. Esse é
    /// estado que a pessoa reverte, e nós restauramos a janela sozinhos ao
    /// capturar — some da lista seria pior.
    ///
    /// Existe porque a lista e a captura estavam divergindo. A lista filtrava
    /// só janela invisível e oculta pelo DWM; a captura também exigia tamanho.
    /// Resultado num log real: o cliente do League aparecia com tamanho zero,
    /// a pessoa escolhia, e recebia "a janela não tem tamanho" três vezes
    /// seguidas. Oferecer o que nunca vai funcionar é pior que não oferecer —
    /// de fora, parece que quem errou foi ela.
    #[must_use]
    pub fn oferecivel(&self) -> bool {
        self.visivel
            && !self.oculta_pelo_dwm
            && (self.minimizada || (self.largura > 1 && self.altura > 1))
    }

    /// Explicação curta de por que não dá para capturar. `None` se dá.
    #[must_use]
    pub fn porque_nao(&self) -> Option<&'static str> {
        if !self.visivel {
            Some("a janela não está visível")
        } else if self.minimizada {
            Some("a janela está minimizada — restaure-a antes de compartilhar")
        } else if self.oculta_pelo_dwm {
            Some("a janela existe mas não está sendo desenhada pelo Windows (aplicativo suspenso?)")
        } else if self.largura <= 1 || self.altura <= 1 {
            Some("a janela não tem tamanho")
        } else {
            None
        }
    }
}

/// Consulta o estado de uma janela pelo HWND.
#[must_use]
pub fn estado_da_janela(hwnd: isize) -> EstadoJanela {
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, IsIconic, IsWindowVisible};

    let h = HWND(hwnd as *mut std::ffi::c_void);
    let mut cloaked: u32 = 0;
    let mut rect = RECT::default();

    // SAFETY: consultas de leitura sobre um HWND. Um handle inválido faz as
    // funções falharem, e os valores padrão levam a `capturavel() == false`,
    // que é o desfecho seguro.
    unsafe {
        let _ = DwmGetWindowAttribute(
            h,
            DWMWA_CLOAKED,
            std::ptr::addr_of_mut!(cloaked).cast(),
            u32::try_from(std::mem::size_of::<u32>()).expect("u32 cabe em u32"),
        );
        let tem_rect = GetWindowRect(h, &mut rect).is_ok();
        EstadoJanela {
            visivel: IsWindowVisible(h).as_bool(),
            oculta_pelo_dwm: cloaked != 0,
            minimizada: IsIconic(h).as_bool(),
            largura: if tem_rect { rect.right - rect.left } else { 0 },
            altura: if tem_rect { rect.bottom - rect.top } else { 0 },
        }
    }
}

/// Restaura uma janela minimizada, sem roubar o foco de quem está usando o PC.
///
/// `SW_SHOWNOACTIVATE` em vez de `SW_RESTORE`: a pessoa está no app do `tela'`
/// escolhendo o que compartilhar, e jogar a janela restaurada para a frente
/// tiraria o foco no meio do clique.
pub fn restaura_janela(hwnd: isize) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_SHOWNOACTIVATE};

    // SAFETY: `ShowWindow` sobre um HWND inválido apenas devolve `false`.
    unsafe {
        let _ = ShowWindow(HWND(hwnd as *mut std::ffi::c_void), SW_SHOWNOACTIVATE);
    }
}

#[cfg(test)]
mod testes_suporte {
    use super::e_falta_de_suporte;

    /// A mensagem REAL de um Windows build 22631, copiada do log de quem não
    /// conseguiu transmitir. É por ela que o app decide tentar de novo.
    #[test]
    fn reconhece_a_mensagem_real() {
        assert!(e_falta_de_suporte(
            "Graphics capture error: Setting a minimum update interval is not \
             supported by the Graphics Capture API on this platform."
        ));
    }

    /// Erros que NÃO melhoram tentando de novo precisam continuar falhando.
    /// Mascará-los com uma segunda tentativa esconderia a causa real.
    #[test]
    fn nao_confunde_com_outras_falhas() {
        for outro in [
            "Graphics capture error: The window is no longer valid",
            "Graphics capture error: Access is denied",
            "Graphics capture error: item creation failed",
            "",
        ] {
            assert!(!e_falta_de_suporte(outro), "reconheceu errado: {outro:?}");
        }
    }

    /// A mensagem REAL de um Windows 10 22H2 (build 19045), copiada do log de
    /// quem não conseguia compartilhar.
    ///
    /// # Por que este teste existe
    ///
    /// A versão anterior de `e_falta_de_suporte` citava só o afunilamento de
    /// quadros. Quando alguém em Windows 10 entrou, o MESMO defeito derrubou a
    /// captura dele inteira — outro recurso, mesma classe. Tirar a borda não
    /// existe fora do Windows 11, e a biblioteca falha o `start` em vez de
    /// ignorar o pedido.
    #[test]
    fn reconhece_a_borda_nao_suportada() {
        assert!(e_falta_de_suporte(
            "Graphics capture error: Toggling the capture border is not supported by the Graphics Capture API on this platform."
        ));
    }

    /// Um recurso que ainda não conhecemos tem de cair na mesma degradação.
    ///
    /// É o ponto de a detecção ser genérica: se a Microsoft acrescentar mais
    /// um ajuste que o Windows antigo recusa, ninguém fica sem transmitir
    /// esperando a gente descobrir o nome dele.
    #[test]
    fn reconhece_recurso_desconhecido_recusado() {
        assert!(e_falta_de_suporte(
            "Graphics capture error: Setting the quantum flux capacitor is not supported by the Graphics Capture API on this platform."
        ));
    }

    /// "Não suportado" sozinho não basta: precisa vir da API de captura.
    ///
    /// Sem essa segunda âncora, qualquer erro do encoder ou do áudio que
    /// contenha "not supported" faria a captura tentar degraus que não têm
    /// nada a ver com o problema.
    #[test]
    fn exige_as_duas_partes() {
        assert!(!e_falta_de_suporte("minimum update interval too large"));
        assert!(!e_falta_de_suporte("feature not supported"));
        assert!(!e_falta_de_suporte("h264_amf: format not supported"));
        assert!(!e_falta_de_suporte("graphics capture api"));
    }
}

#[cfg(test)]
mod testes_oferecivel {
    use super::EstadoJanela;

    fn sadia() -> EstadoJanela {
        EstadoJanela {
            visivel: true,
            oculta_pelo_dwm: false,
            minimizada: false,
            largura: 1920,
            altura: 1080,
        }
    }

    /// O defeito que motivou isto, num log de verdade: o cliente do League
    /// aparecia na lista com tamanho zero, a pessoa escolhia, e a captura
    /// respondia "a janela não tem tamanho" — três vezes seguidas.
    ///
    /// A lista e a captura estavam usando critérios diferentes: a lista
    /// filtrava só janela invisível e oculta pelo DWM, e a captura também
    /// exigia tamanho. Oferecer o que nunca vai funcionar é pior que não
    /// oferecer: a pessoa não tem como saber que o problema não é ela.
    #[test]
    fn janela_sem_tamanho_nao_entra_na_lista() {
        let fantasma = EstadoJanela {
            largura: 0,
            altura: 0,
            ..sadia()
        };
        assert!(!fantasma.oferecivel(), "não pode ser oferecida");
        assert!(
            fantasma.porque_nao().is_some(),
            "a captura recusa; a lista tem de concordar"
        );
    }

    /// Minimizada CONTINUA na lista: é estado que a pessoa reverte, e nós
    /// restauramos sozinhos ao capturar. Este é o caso que impede de resolver
    /// com um `capturavel()` simples.
    #[test]
    fn minimizada_continua_sendo_oferecida() {
        let m = EstadoJanela {
            minimizada: true,
            ..sadia()
        };
        assert!(!m.capturavel(), "capturável não, ainda");
        assert!(m.oferecivel(), "mas oferecível sim");
    }

    /// Uma janela normal passa nos dois.
    #[test]
    fn janela_normal_passa() {
        assert!(sadia().capturavel());
        assert!(sadia().oferecivel());
        assert!(sadia().porque_nao().is_none());
    }

    /// Fantasmas do Windows continuam fora.
    #[test]
    fn invisivel_e_oculta_ficam_de_fora() {
        assert!(!EstadoJanela {
            visivel: false,
            ..sadia()
        }
        .oferecivel());
        assert!(!EstadoJanela {
            oculta_pelo_dwm: true,
            ..sadia()
        }
        .oferecivel());
    }

    /// Tudo que a lista oferece e que NÃO está minimizada tem de ser aceito
    /// pela captura. É a regra que estava quebrada.
    #[test]
    fn oferecido_e_nao_minimizado_implica_capturavel() {
        for largura in [0, 1, 2, 1920] {
            for altura in [0, 1, 2, 1080] {
                for minimizada in [false, true] {
                    let e = EstadoJanela {
                        largura,
                        altura,
                        minimizada,
                        ..sadia()
                    };
                    if e.oferecivel() && !e.minimizada {
                        assert!(e.capturavel(), "oferecida mas não capturável: {e:?}");
                        assert!(e.porque_nao().is_none(), "oferecida mas recusada: {e:?}");
                    }
                }
            }
        }
    }
}

/// Tempo mínimo entre duas conversões, para uma taxa alvo.
///
/// É rede de segurança: o `MinimumUpdateInterval` já pede ao Windows para
/// segurar a entrega, mas ele é uma dica e nem toda versão do Windows a
/// respeita. O corte aqui garante que não convertemos mais do que vamos usar.
///
/// A folga é generosa DE PROPÓSITO, e isto já foi um defeito. A primeira
/// versão cortava em `0,9/fps` — 15 ms para 60 fps —, pensando em "10% de
/// folga". Só que uma tela de 60 Hz apresenta a cada 16,67 ms: a folga real
/// era de 1,67 ms, e qualquer variação de agendamento acima disso descartava
/// um quadro ÚNICO. Medido na prática, 13% dos quadros de um vídeo se perdiam
/// assim, e o efeito é engasgo visível na imagem com o áudio liso.
///
/// Quem tem tela rápida nunca viu: a 200 Hz os quadros chegam a cada 5 ms e o
/// que o corte joga fora é excedente de verdade. Só quem estava em 60 Hz — a
/// maioria — pagava.
#[must_use]
pub fn intervalo_de_corte(fps: u32) -> Duration {
    Duration::from_secs_f64(0.5 / f64::from(fps.max(1)))
}

#[cfg(test)]
mod testes_corte {
    use super::intervalo_de_corte;
    use std::time::Duration;

    /// Uma tela que apresenta EXATAMENTE na taxa pedida não pode ter quadro
    /// descartado, nem com variação de agendamento.
    ///
    /// É o caso mais comum que existe: 60 fps pedidos numa tela de 60 Hz.
    #[test]
    fn tela_na_taxa_alvo_nao_perde_quadro() {
        for fps in [24, 30, 60, 120, 144] {
            let periodo = 1.0 / f64::from(fps);
            let corte = intervalo_de_corte(fps).as_secs_f64();
            // Metade do período de folga aguenta qualquer jitter realista.
            assert!(
                corte <= periodo * 0.51,
                "{fps} fps: corte de {:.1} ms contra período de {:.1} ms — \
                 folga de só {:.1} ms",
                corte * 1000.0,
                periodo * 1000.0,
                (periodo - corte) * 1000.0
            );
        }
    }

    /// O caso concreto que motivou tudo: 60 fps numa tela de 60 Hz, com 2 ms
    /// de atraso no agendamento. Com o corte antigo (15 ms) o quadro morria.
    #[test]
    fn sessenta_hertz_com_jitter_real_passa() {
        let corte = intervalo_de_corte(60).as_secs_f64() * 1000.0;
        let chegada_pior_caso = 16.67 - 4.0; // 4 ms de adiantamento, generoso
        assert!(
            chegada_pior_caso > corte,
            "quadro chegando {chegada_pior_caso:.1} ms depois do anterior seria \
             descartado por um corte de {corte:.1} ms"
        );
    }

    /// Continua sendo rede de segurança: uma tela muito mais rápida que o alvo
    /// tem de ter o excedente cortado, senão convertemos quadro que ninguém
    /// vai usar — foi para isso que o corte existe.
    #[test]
    fn tela_rapida_ainda_tem_excedente_cortado() {
        let corte = intervalo_de_corte(60);
        let periodo_200hz = Duration::from_secs_f64(1.0 / 200.0);
        assert!(
            periodo_200hz < corte,
            "a 200 Hz o quadro chega a cada 5 ms e precisa ser afunilado"
        );
    }
}

#[cfg(all(test, feature = "encoder-d3d11"))]
mod testes_destino_gpu {
    use super::*;
    use std::sync::mpsc::sync_channel;

    /// Captura um monitor de verdade pelo caminho que encoda na GPU.
    ///
    /// Vale como prova de ponta a ponta do caminho novo: WGC entrega a textura,
    /// o encoder de hardware a recebe sem readback, e sai H.264. Máquina sem
    /// GPU compatível não reprova — é para ela que o `ffmpeg.exe` continua
    /// existindo.
    #[test]
    fn encoda_a_tela_sem_readback() {
        // O relógio chama a libav; sem ela carregada ele entra em pânico na
        // primeira volta e o teste falharia por um motivo que não é o dele.
        if !crate::libav::carrega_de_teste() {
            eprintln!("sem vendor/ffmpeg, pulando");
            return;
        }
        let Ok(monitores) = list_monitors() else {
            eprintln!("sem monitor listável, pulando");
            return;
        };
        let Some(m) = monitores.first() else {
            eprintln!("sem monitor, pulando");
            return;
        };

        let (envio, recebe) = sync_channel(60);
        let cod = Arc::new(Codificacao {
            envio,
            kbps: AtomicU32::new(4000),
            fps: AtomicU32::new(30),
            // 720 e não nativo de propósito: exercita o escalador, que é a
            // peça nova e a que pode falhar por driver.
            altura_alvo: AtomicU32::new(720),
            chave_pedida: AtomicBool::new(false),
            amostra: FrameBuffer::shared(),
            retido: Mutex::new(None),
            dispositivo: std::sync::OnceLock::new(),
            parar: AtomicBool::new(false),
        });
        // O relógio é quem encoda; sem ele o quadro retido nunca vira pacote.
        {
            let c = Arc::clone(&cod);
            std::thread::spawn(move || relogio(&c));
        }
        zera_contadores();
        let cfg = Config {
            destino: Destino::Gpu(Arc::clone(&cod)),
            fps: 30,
        };
        let Ok(captura) = start_monitor(Some(m.index), cfg) else {
            eprintln!("não capturou o monitor, pulando");
            return;
        };

        // A tela precisa mudar para haver quadro; um segundo é folga bastante
        // até num desktop parado, porque o WGC entrega ao menos o primeiro.
        std::thread::sleep(Duration::from_millis(1200));
        drop(captura);
        cod.parar.store(true, Ordering::Relaxed);

        let quadros: Vec<_> = recebe.try_iter().collect();
        let (entregues, convertidos) = contadores();
        eprintln!(
            "wgc entregou {entregues}, novos {convertidos}, repetidos {}, saíram {} ({} engasgados)",
            repetidos(),
            quadros.len(),
            engasgados()
        );

        if let Some(e) = erro_do_encoder() {
            panic!("o encoder falhou: {e}");
        }
        // Esta é a afirmação que importa, e a que teria pego o defeito que a
        // primeira transmissão ao vivo revelou: mesmo com a tela PARADA — o
        // WGC entregando um quadro e mais nenhum — o relógio precisa continuar
        // produzindo. Antes dele, um desktop quieto zerava a transmissão.
        assert!(
            !quadros.is_empty(),
            "o WGC entregou {entregues} quadros e nenhum saiu codificado"
        );
        assert!(
            quadros.iter().any(|q| q.chave),
            "nenhum quadro-chave: o decodificador não teria por onde começar"
        );
        assert!(
            quadros.iter().all(|q| !q.bytes.is_empty()),
            "quadro codificado vazio"
        );
        // Os instantes de captura têm de ser crescentes: é deles que sai o
        // timestamp RTP, e um que ande para trás quebra a reprodução.
        for par in quadros.windows(2) {
            assert!(
                par[1].instante >= par[0].instante,
                "o instante de captura andou para trás"
            );
        }
    }
}

#[cfg(test)]
mod testes_cadencia {
    use super::proximo_alvo;
    use std::time::{Duration, Instant};

    /// 60 fps.
    const P: Duration = Duration::from_nanos(16_666_667);

    /// A propriedade que impede rajada: depois de decidir, nunca se está mais
    /// de um período atrasado. Se estivesse, as voltas seguintes sairiam
    /// emendadas tentando recuperar — que é o sintoma a eliminar.
    fn nao_ficou_para_tras(novo: Instant, agora: Instant, periodo: Duration) {
        assert!(
            agora.saturating_duration_since(novo) < periodo,
            "sobrou atraso depois do ajuste: viraria rajada"
        );
    }

    /// O caso comum: acordou antes do alvo, a agenda não se mexe.
    ///
    /// É esta linha que conserta os 57 fps. A versão anterior recomeçava a
    /// contagem a partir da hora em que acordou, então o excesso do `sleep`
    /// entrava no período seguinte e no seguinte — 5% de déficit acumulado.
    #[test]
    fn acordar_adiantado_nao_mexe_na_agenda() {
        let agora = Instant::now();
        let alvo = agora + Duration::from_millis(5);
        let (novo, perdidas) = proximo_alvo(alvo, agora, P);
        assert_eq!(novo, alvo, "o alvo não podia ter andado");
        assert_eq!(perdidas, 0);
    }

    /// Estouro menor que um período é absorvido, sem descartar quadro.
    ///
    /// Descartar um quadro por 0,2 ms de atraso seria pior que o atraso. A
    /// agenda não anda a mais: a volta seguinte sai imediatamente e o desvio
    /// morre ali. Foi o que faltou na minha primeira versão desta função.
    #[test]
    fn estouro_pequeno_e_absorvido_sem_perder_quadro() {
        let alvo = Instant::now();
        let agora = alvo + Duration::from_micros(800);
        let (novo, perdidas) = proximo_alvo(alvo, agora, P);
        assert_eq!(perdidas, 0, "0,8 ms de estouro não é quadro perdido");
        assert_eq!(novo, alvo, "a agenda não podia ter andado a mais");
        nao_ficou_para_tras(novo, agora, P);
    }

    /// Perdeu uma volta inteira: conta uma e pula para a fronteira.
    #[test]
    fn atraso_de_uma_volta_conta_uma() {
        let alvo = Instant::now();
        let agora = alvo + P + Duration::from_millis(2);
        let (novo, perdidas) = proximo_alvo(alvo, agora, P);
        assert_eq!(perdidas, 1);
        // Múltiplo exato do período a partir da agenda, não de `agora`.
        assert_eq!(novo, alvo + P);
        nao_ficou_para_tras(novo, agora, P);
    }

    /// O ponto que mais importa: atraso grande não vira rajada.
    ///
    /// Emendar voltas para recuperar meio segundo produziria trinta quadros de
    /// uma vez — o "trava e volta no ao vivo de uma vez" que esta mudança
    /// existe para eliminar. Tem de pular, e o que ficou pelo caminho vira
    /// número em vez de silêncio.
    #[test]
    fn atraso_grande_pula_em_vez_de_correr_atras() {
        // Período redondo de propósito: com 16,666667 ms o número exato depende
        // de arredondamento, e o que este teste afirma é o comportamento, não a
        // aritmética.
        let p = Duration::from_millis(10);
        let alvo = Instant::now();
        let agora = alvo + Duration::from_millis(500);
        let (novo, perdidas) = proximo_alvo(alvo, agora, p);
        // Meio segundo a 100 voltas por segundo, mais a volta que já estava em
        // curso: 51 fronteiras, 50 delas perdidas.
        assert_eq!(perdidas, 50);
        nao_ficou_para_tras(novo, agora, p);
    }

    /// A máquina suspendeu. Não pode virar aritmética gigante nem rajada.
    #[test]
    fn atraso_absurdo_recomeca_a_agenda() {
        let alvo = Instant::now();
        let agora = alvo + Duration::from_secs(3600);
        let (novo, _) = proximo_alvo(alvo, agora, P);
        nao_ficou_para_tras(novo, agora, P);
        assert!(
            novo.duration_since(agora) <= P,
            "recomeçar a agenda não pode deixar o alvo longe"
        );
    }

    /// A escada troca de fps no meio da transmissão.
    #[test]
    fn funciona_com_periodo_de_30_fps() {
        let p30 = Duration::from_nanos(33_333_333);
        let alvo = Instant::now();
        let agora = alvo + p30 + Duration::from_millis(1);
        let (novo, perdidas) = proximo_alvo(alvo, agora, p30);
        assert_eq!(perdidas, 1);
        nao_ficou_para_tras(novo, agora, p30);
    }

    /// Muitas voltas seguidas não podem acumular desvio.
    ///
    /// É o teste que reprova a implementação antiga: somando 1 ms de estouro
    /// por volta, sessenta voltas terminavam 60 ms atrasadas — os 57 fps.
    #[test]
    fn sessenta_voltas_com_estouro_nao_acumulam_desvio() {
        let inicio = Instant::now();
        let mut alvo = inicio;
        let mut perdidas_total = 0;
        for n in 1..=60u32 {
            alvo += P;
            // Cada volta acorda 1 ms depois do alvo, como o `sleep` do Windows.
            let agora = alvo + Duration::from_millis(1);
            let (novo, perdidas) = proximo_alvo(alvo, agora, P);
            perdidas_total += perdidas;
            alvo = novo;
            // A agenda continua ancorada no início, sem escorregar.
            assert_eq!(
                alvo,
                inicio + P * n,
                "a agenda escorregou na volta {n}: é o defeito dos 57 fps"
            );
        }
        assert_eq!(perdidas_total, 0, "estouro de 1 ms não é quadro perdido");
    }
}
