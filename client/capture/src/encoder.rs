//! Encode H.264 direto da textura que a captura já tem na GPU.
//!
//! # Por que existe
//!
//! O caminho antigo levava cada quadro pelo pior percurso possível: o WGC
//! entrega uma textura na GPU, nós a copiávamos para a CPU (`frame.buffer()`),
//! convertíamos BGRA→NV12 no processador, empurrávamos por um named pipe e o
//! `ffmpeg.exe` mandava tudo de volta para a GPU para encodar. Duas travessias
//! do barramento e uma conversão de cor, por quadro, para acabar no mesmo
//! lugar de onde o quadro saiu.
//!
//! Medido em 1080p60 nesta máquina: ~168% de um núcleo no caminho antigo
//! (aplicativo + ffmpeg somados) contra **0,94 ms de CPU por quadro** aqui —
//! algo como 6% de um núcleo. O encoder passa a receber a própria textura, e a
//! única cópia que resta é GPU→GPU.
//!
//! # Como
//!
//! A libavcodec aceita um `AVHWDeviceContext` do tipo `D3D11VA` construído a
//! partir de um `ID3D11Device` **que já existe** — no nosso caso o que a
//! `windows-capture` criou. Isso é o que torna tudo simples: não há dois
//! dispositivos, então não há textura compartilhada entre adaptadores nem
//! keyed mutex. `CopySubresourceRegion` resolve.
//!
//! O formato de software do pool é **BGRA**, não NV12. Parece errado — o
//! encoder quer YUV — mas o nvenc converte internamente, de graça, e criar
//! textura NV12 falha com `E_INVALIDARG` em parte das GPUs. Menos trabalho e
//! mais compatível.

use std::ffi::{c_int, c_void, CStr, CString};
use std::mem::ManuallyDrop;
use std::os::windows::ffi::OsStrExt as _;
use std::path::Path;
use std::ptr;
use std::sync::OnceLock;

use anyhow::{bail, Result};

use crate::ffmpeg::sem_prefixo;
use crate::libav::{self, tipos as ff, MAPA_DIRETO};

use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::System::LibraryLoader::LoadLibraryW;

/// As funções da libav.
///
/// Um `expect` e não um `Option` propagado por toda parte: a regra do módulo é
/// que nada aqui roda antes de `libav::carrega` ter dado certo, e quem inicia a
/// transmissão a respeita. Se ela for quebrada, um pânico com esta mensagem é
/// muito melhor que chamar um ponteiro nulo, que derruba o processo sem dizer
/// por quê.
fn av() -> &'static libav::Api {
    libav::api().expect("a libav não foi carregada — ver libav::carrega")
}

/// Espelho de `AVD3D11VADeviceContext` (`libavutil/hwcontext_d3d11va.h`).
///
/// O `ffmpeg-sys-next` não gera binding para este header, então declaramos a
/// struct à mão. A ordem e os tipos têm de bater exatamente com os do ffmpeg
/// contra o qual linkamos — ver `vendor/ffmpeg/include/libavutil/`.
#[repr(C)]
struct DispositivoD3D11 {
    device: *mut c_void,
    device_context: *mut c_void,
    video_device: *mut c_void,
    video_context: *mut c_void,
    lock: Option<unsafe extern "C" fn(*mut c_void)>,
    unlock: Option<unsafe extern "C" fn(*mut c_void)>,
    lock_ctx: *mut c_void,
    bind_flags: u32,
    misc_flags: u32,
}

/// Espelho de `AVD3D11VAFramesContext` (mesmo header).
///
/// Só existe por causa do `BindFlags`: sem ele não há como pedir que as
/// texturas do pool nasçam utilizáveis pelo AMF.
#[repr(C)]
struct FramesD3D11 {
    texture: *mut c_void,
    bind_flags: u32,
    misc_flags: u32,
    texture_infos: *mut c_void,
}

/// Encoders de hardware que sabemos alimentar com a textura da captura, na
/// ordem em que preferimos.
///
/// `h264_qsv` vem por último porque é o único que não recebe a textura direto:
/// ele recusa `AV_PIX_FMT_D3D11` e exige que ela seja mapeada para uma
/// superfície dele — ver [`Encoder::monta`]. Numa máquina com placa dedicada os
/// dois primeiros respondem antes e ele nem é tentado.
const CANDIDATOS: [&str; 3] = ["h264_nvenc", "h264_amf", "h264_qsv"];

/// O nome do encoder que fala QSV.
const QSV: &str = "h264_qsv";

/// O único que recebe a textura como está, sem conversão de cor.
const NVENC: &str = "h264_nvenc";

/// O que define o encode. Mudar qualquer campo exige recriar o
/// `AVCodecContext` — barato (milissegundos) e, ao contrário do processo
/// externo, invisível para o RTP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ajuste {
    pub largura: u32,
    pub altura: u32,
    pub fps: u32,
    pub kbps: u32,
}

/// Um quadro codificado.
pub struct Saida<'a> {
    pub bytes: &'a [u8],
    pub pts: i64,
    pub chave: bool,
}

pub struct Encoder {
    /// `AVBufferRef` do dispositivo. Sobrevive às reconfigurações.
    dispositivo: *mut ff::AVBufferRef,
    /// `AVBufferRef` do pool de texturas. Recriado quando o ajuste muda.
    frames: *mut ff::AVBufferRef,
    codec: *mut ff::AVCodecContext,
    pacote: *mut ff::AVPacket,
    contexto: ID3D11DeviceContext,
    ajuste: Ajuste,
    nome: &'static str,
    /// Contador de apresentação. Não reinicia entre reconfigurações: o
    /// consumidor enxerga uma linha do tempo só.
    pts: i64,
    /// O próximo quadro tem de ser um IDR. Ver [`Encoder::pede_chave`].
    chave: bool,
    /// Formato em que o pool guarda a imagem.
    ///
    /// BGRA para nvenc e AMF, que convertem a cor internamente de graça. NV12
    /// para o QSV, que só aceita esse — e por isso a escrita no pool passa
    /// obrigatoriamente pelo processador de vídeo, que converte.
    sw: ff::AVPixelFormat,
    /// Contexto de frames do QSV, quando é ele o encoder. `None` nos outros.
    ///
    /// O pool continua sendo o de texturas D3D11; este é uma VISTA dele em
    /// termos de QSV, criada por derivação. Cada quadro é escrito na textura e
    /// depois mapeado — sem cópia, é a mesma memória vista de outro jeito.
    frames_qsv: *mut ff::AVBufferRef,
    dispositivo_qsv: *mut ff::AVBufferRef,
}

// A libavcodec não impõe afinidade de thread; exige acesso serializado, que
// quem usa garante (há um único dono do encoder por transmissão). O
// `ID3D11Device` recebe `SetMultithreadProtected` em `Encoder::abre`.
unsafe impl Send for Encoder {}

impl Encoder {
    /// Abre o primeiro encoder de [`CANDIDATOS`] que aceitar este dispositivo.
    ///
    /// Devolve erro se nenhum abrir — máquina sem GPU compatível, driver
    /// ausente. Quem chama deve cair no caminho do `ffmpeg.exe`.
    pub fn abre(
        dispositivo: &ID3D11Device,
        contexto: &ID3D11DeviceContext,
        ajuste: Ajuste,
    ) -> Result<Self> {
        // TODAS as recusas, e não só a última. Numa máquina que não é a nossa,
        // saber que o nvenc não existe é tão informativo quanto saber por que o
        // AMF recusou — e a última costuma ser a menos interessante, porque a
        // ordem dos candidatos põe o menos provável no fim.
        let mut recusas = Vec::new();
        for nome in CANDIDATOS {
            match Self::monta(dispositivo, contexto, ajuste, nome) {
                Ok(e) => return Ok(e),
                Err(e) => recusas.push(format!("{nome}: {}", uma_linha(&format!("{e:#}")))),
            }
        }
        bail!(
            "nenhum encoder de hardware aceitou a textura — {}",
            recusas.join("; ")
        )
    }

    fn monta(
        dispositivo: &ID3D11Device,
        contexto: &ID3D11DeviceContext,
        ajuste: Ajuste,
        nome: &'static str,
    ) -> Result<Self> {
        // Duas coisas dependem disto, e a segunda não é óbvia. O ffmpeg e a
        // thread de callback do WGC tocam o mesmo contexto: sem serialização o
        // resultado é corrupção silenciosa. E o MSDK, que é quem atende o QSV,
        // RECUSA um dispositivo D3D11 desprotegido — devolve "Error setting
        // child device handle: -16" na derivação, sem dizer o porquê.
        //
        // Fica aqui, e não em `abre`, para valer também para quem monta um
        // encoder específico direto (os testes fazem isso).
        if let Ok(mt) = dispositivo.cast::<ID3D11Multithread>() {
            // Devolve o estado anterior, que não nos interessa.
            unsafe {
                let _ = mt.SetMultithreadProtected(true);
            };
        }
        let disp = unsafe { Self::adota(dispositivo, contexto)? };
        let mut eu = Self {
            dispositivo: disp,
            frames: ptr::null_mut(),
            codec: ptr::null_mut(),
            pacote: unsafe { (av().av_packet_alloc)() },
            contexto: contexto.clone(),
            ajuste,
            nome,
            pts: 0,
            chave: false,
            // BGRA só para o nvenc, que converte a cor internamente de graça.
            //
            // O AMF NÃO aceita: o caminho de textura dele quer NV12, e entregar
            // BGRA devolve `AMF_DIRECTX_FAILED` (erro 18) no `SubmitInput` —
            // foi o que apareceu na máquina de um dos amigos, uma RX 6600. O
            // OBS, que é a referência para encode por textura em AMD, também só
            // entrega NV12 ou P010 nesse caminho, e recusa o resto na checagem
            // de capacidade.
            //
            // O QSV, pelo mesmo motivo, já usava NV12.
            sw: if nome == NVENC {
                ff::AVPixelFormat::AV_PIX_FMT_BGRA
            } else {
                ff::AVPixelFormat::AV_PIX_FMT_NV12
            },
            frames_qsv: ptr::null_mut(),
            dispositivo_qsv: ptr::null_mut(),
        };
        unsafe { eu.reabre(ajuste)? };
        Ok(eu)
    }

    /// Constrói o `AVHWDeviceContext` em cima do dispositivo que já temos.
    unsafe fn adota(
        dispositivo: &ID3D11Device,
        contexto: &ID3D11DeviceContext,
    ) -> Result<*mut ff::AVBufferRef> {
        let refer = (av().av_hwdevice_ctx_alloc)(ff::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA);
        if refer.is_null() {
            bail!("av_hwdevice_ctx_alloc devolveu nulo");
        }
        let hw = (*((*refer).data as *mut ff::AVHWDeviceContext)).hwctx as *mut DispositivoD3D11;
        // `ManuallyDrop` porque o ffmpeg passa a ser dono destas referências e
        // chama `Release` quando o contexto morre; deixar o `Drop` do Rust
        // rodar liberaria duas vezes.
        (*hw).device = ManuallyDrop::new(dispositivo.clone()).as_raw();
        (*hw).device_context = ManuallyDrop::new(contexto.clone()).as_raw();
        let rc = (av().av_hwdevice_ctx_init)(refer);
        if rc < 0 {
            bail!("av_hwdevice_ctx_init: {}", texto_do_erro(rc));
        }
        Ok(refer)
    }

    /// Recria o pool e o `AVCodecContext` para um novo ajuste.
    ///
    /// O dispositivo é preservado; só o que depende de resolução, taxa e fps
    /// nasce de novo.
    pub fn reconfigura(&mut self, ajuste: Ajuste) -> Result<()> {
        if ajuste == self.ajuste {
            return Ok(());
        }
        unsafe { self.reabre(ajuste) }
    }

    unsafe fn reabre(&mut self, ajuste: Ajuste) -> Result<()> {
        self.libera_codec();

        let mut frames = (av().av_hwframe_ctx_alloc)(self.dispositivo);
        if frames.is_null() {
            bail!("av_hwframe_ctx_alloc devolveu nulo");
        }
        let fc = (*frames).data as *mut ff::AVHWFramesContext;
        (*fc).format = ff::AVPixelFormat::AV_PIX_FMT_D3D11;
        (*fc).sw_format = self.sw;
        (*fc).width = ajuste.largura as i32;
        (*fc).height = ajuste.altura as i32;
        // O encoder segura quadros em voo enquanto trabalha. Um pool apertado
        // devolve ENOMEM no meio da transmissão — o valor é folgado de
        // propósito, e cada entrada é só uma textura na GPU.
        //
        // Um pool de tamanho fixo também faz o ffmpeg alocar UMA textura em
        // array em vez de várias soltas, que é a forma que o `amfenc` espera:
        // ele publica o índice da camada em `AMFTextureArrayIndexGUID` antes
        // de chamar `CreateSurfaceFromDX11Native`.
        // Pool de tamanho fixo vira UMA textura em array; zero faz o ffmpeg
        // alocar texturas avulsas.
        //
        // Com NV12 tem de ser avulsa: medido nas duas marcas, um array NV12 só
        // é aceito com `BIND_DECODER` sozinho, e aí o processador de vídeo —
        // que é quem escreve nesse caso, porque converte a cor — não consegue
        // criar a view de saída, que exige `RENDER_TARGET`.
        //
        // Avulsa também serve ao AMF: ele lê o índice da camada em `data[1]`,
        // que numa textura sozinha é zero. É o que o OBS faz — uma textura por
        // encode, não um array.
        (*fc).initial_pool_size = if matches!(self.sw, ff::AVPixelFormat::AV_PIX_FMT_NV12) {
            0
        } else {
            40
        };

        // Flags de uso da textura do pool. Precisam vir ANTES do
        // `av_hwframe_ctx_init`, que é quem cria as texturas.
        //
        // BGRA leva os dois: o AMF converte a cor DESENHANDO na textura e
        // recusa uma que não possa ser alvo de renderização; o nvenc não se
        // importa, então pedir ambos cobre as duas GPUs com uma configuração
        // só.
        //
        // NV12 leva só `RENDER_TARGET`, e isso não é preferência: NV12 com
        // `SHADER_RESOURCE` faz o `CreateTexture2D` recusar com `E_INVALIDARG`.
        // O alvo de renderização é indispensável porque quem escreve no pool
        // nesse caso é o processador de vídeo, e a view de saída dele exige.
        let fhw = (*fc).hwctx as *mut FramesD3D11;
        (*fhw).bind_flags |= if matches!(self.sw, ff::AVPixelFormat::AV_PIX_FMT_NV12) {
            D3D11_BIND_RENDER_TARGET.0 as u32
        } else {
            (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32
        };
        let rc = (av().av_hwframe_ctx_init)(frames);
        if rc < 0 {
            (av().av_buffer_unref)(&mut frames);
            bail!("av_hwframe_ctx_init: {}", texto_do_erro(rc));
        }

        // O QSV não enxerga textura D3D11. A saída é derivar um dispositivo
        // QSV do nosso e criar uma vista do pool em termos dele: a memória é a
        // mesma, o mapeamento é direto, e não há cópia nenhuma no caminho.
        let mut frames_encoder = frames;
        if self.nome == QSV {
            let rc = (av().av_hwdevice_ctx_create_derived)(
                &mut self.dispositivo_qsv,
                ff::AVHWDeviceType::AV_HWDEVICE_TYPE_QSV,
                self.dispositivo,
                0,
            );
            if rc < 0 {
                (av().av_buffer_unref)(&mut frames);
                bail!("derivar dispositivo QSV: {}", texto_do_erro(rc));
            }
            let rc = (av().av_hwframe_ctx_create_derived)(
                &mut self.frames_qsv,
                ff::AVPixelFormat::AV_PIX_FMT_QSV as c_int,
                self.dispositivo_qsv,
                frames,
                MAPA_DIRETO,
            );
            if rc < 0 {
                (av().av_buffer_unref)(&mut frames);
                bail!("derivar pool QSV: {}", texto_do_erro(rc));
            }
            frames_encoder = self.frames_qsv;
        }

        let nome = CString::new(self.nome)?;
        let codec = (av().avcodec_find_encoder_by_name)(nome.as_ptr());
        if codec.is_null() {
            (av().av_buffer_unref)(&mut frames);
            bail!("encoder {} não existe nesta build", self.nome);
        }
        let mut cc = (av().avcodec_alloc_context3)(codec);
        (*cc).width = ajuste.largura as i32;
        (*cc).height = ajuste.altura as i32;
        (*cc).pix_fmt = if self.nome == QSV {
            ff::AVPixelFormat::AV_PIX_FMT_QSV
        } else {
            ff::AVPixelFormat::AV_PIX_FMT_D3D11
        };
        (*cc).time_base = ff::AVRational {
            num: 1,
            den: ajuste.fps.max(1) as i32,
        };
        (*cc).framerate = ff::AVRational {
            num: ajuste.fps.max(1) as i32,
            den: 1,
        };
        (*cc).bit_rate = i64::from(ajuste.kbps) * 1000;
        (*cc).rc_max_rate = (*cc).bit_rate;
        // Buffer de um quadro. Um VBV curto é o que impede o pico que estoura
        // a banda logo depois de uma cena mudar inteira.
        (*cc).rc_buffer_size = ((*cc).bit_rate / i64::from(ajuste.fps.max(1))) as i32;
        // Sem quadros B: eles adiam a saída, e o que queremos é latência baixa.
        (*cc).max_b_frames = 0;
        // Dois segundos. É o mesmo número que vai para o `header_spacing` do
        // AMF, então fica numa variável em vez de repetido.
        let gop = ajuste.fps.max(1) * 2;
        (*cc).gop_size = gop as i32;
        (*cc).hw_frames_ctx = (av().av_buffer_ref)(frames_encoder);

        for (chave, valor) in opcoes(self.nome, gop) {
            let k = CString::new(chave.as_str())?;
            let v = CString::new(valor.as_str())?;
            // Opção desconhecida não é fatal: a build pode não ter o preset e
            // o encoder ainda funciona com o padrão dele.
            (av().av_opt_set)((*cc).priv_data, k.as_ptr(), v.as_ptr(), 0);
        }

        let rc = (av().avcodec_open2)(cc, codec, ptr::null_mut());
        if rc < 0 {
            (av().avcodec_free_context)(&mut cc);
            (av().av_buffer_unref)(&mut frames);
            bail!("avcodec_open2 ({}): {}", self.nome, texto_do_erro(rc));
        }

        self.frames = frames;
        self.codec = cc;
        self.ajuste = ajuste;
        Ok(())
    }

    /// Entrega um quadro.
    ///
    /// `sub` é o índice dentro da textura de origem, 0 para as texturas normais
    /// que o WGC devolve. `recorte` limita o que é copiado ao canto superior
    /// esquerdo — usado para cortar a linha ou coluna ímpar que o H.264 não
    /// aceita. Sem ele, uma origem maior que o pool faz o D3D11 recusar a cópia
    /// em silêncio.
    pub fn envia(
        &mut self,
        origem: &ID3D11Texture2D,
        sub: u32,
        recorte: Option<(u32, u32)>,
    ) -> Result<()> {
        let contexto = self.contexto.clone();
        self.preenche(|destino, camada| {
            let caixa = recorte.map(|(l, a)| D3D11_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: l,
                bottom: a,
                back: 1,
            });
            unsafe {
                contexto.CopySubresourceRegion(
                    destino,
                    camada,
                    0,
                    0,
                    0,
                    origem,
                    sub,
                    caixa.as_ref().map(std::ptr::from_ref),
                );
            }
            Ok(())
        })
    }

    /// Pega um quadro do pool, deixa `escreve` preenchê-lo e o entrega ao
    /// encoder.
    ///
    /// Existe separado de [`Encoder::envia`] porque nem sempre a origem cabe
    /// numa cópia: quando a escada pede resolução menor, quem escreve é o
    /// [`crate::escalador::Escalador`]. Os dois casos compartilham tudo o
    /// resto — tirar do pool, carimbar tempo, pedir IDR, enfileirar.
    pub fn preenche(
        &mut self,
        escreve: impl FnOnce(&ID3D11Texture2D, u32) -> Result<()>,
    ) -> Result<()> {
        unsafe {
            let mut frame = (av().av_frame_alloc)();
            let rc = (av().av_hwframe_get_buffer)(self.frames, frame, 0);
            if rc < 0 {
                (av().av_frame_free)(&mut frame);
                bail!("av_hwframe_get_buffer: {}", texto_do_erro(rc));
            }
            // `data[0]` é a `ID3D11Texture2D` do pool e `data[1]` o índice
            // dela no array — o pool é uma textura só, com várias camadas.
            let destino =
                ID3D11Texture2D::from_raw_borrowed(&((*frame).data[0] as *mut c_void)).cloned();
            let Some(destino) = destino else {
                (av().av_frame_free)(&mut frame);
                bail!("o frame do pool não trouxe textura");
            };
            let camada = (*frame).data[1] as u32;
            if let Err(e) = escreve(&destino, camada) {
                (av().av_frame_free)(&mut frame);
                return Err(e);
            }

            (*frame).pts = self.pts;
            self.pts += 1;
            if std::mem::take(&mut self.chave) {
                // Pedir tipo I na entrada é como se força IDR tanto no nvenc
                // quanto no AMF; não há opção de linha de comando equivalente
                // que valha em tempo de execução.
                (*frame).pict_type = ff::AVPictureType::AV_PICTURE_TYPE_I;
            }
            // No QSV o que vai para o encoder é o MAPEAMENTO da textura, não
            // ela. `MAPA_DIRETO` garante que é a mesma memória — se fosse
            // cópia, teríamos reintroduzido o que este módulo veio eliminar.
            let mut mapeado = ptr::null_mut();
            let enviar = if self.frames_qsv.is_null() {
                frame
            } else {
                mapeado = (av().av_frame_alloc)();
                (*mapeado).format = ff::AVPixelFormat::AV_PIX_FMT_QSV as c_int;
                (*mapeado).hw_frames_ctx = (av().av_buffer_ref)(self.frames_qsv);
                let rc = (av().av_hwframe_map)(mapeado, frame, MAPA_DIRETO);
                if rc < 0 {
                    (av().av_frame_free)(&mut mapeado);
                    (av().av_frame_free)(&mut frame);
                    bail!("av_hwframe_map para QSV: {}", texto_do_erro(rc));
                }
                // O mapeamento não herda tempo nem tipo de quadro.
                (*mapeado).pts = (*frame).pts;
                (*mapeado).pict_type = (*frame).pict_type;
                mapeado
            };

            let rc = (av().avcodec_send_frame)(self.codec, enviar);
            if !mapeado.is_null() {
                (av().av_frame_free)(&mut mapeado);
            }
            (av().av_frame_free)(&mut frame);
            if rc < 0 {
                bail!("avcodec_send_frame: {}", texto_do_erro(rc));
            }
            Ok(())
        }
    }

    /// Chama `f` para cada quadro pronto. Não bloqueia: retorna quando o
    /// encoder não tem mais nada no momento.
    pub fn drena(&mut self, mut f: impl FnMut(Saida<'_>)) -> Result<()> {
        unsafe {
            loop {
                let rc = (av().avcodec_receive_packet)(self.codec, self.pacote);
                if rc == libav::AVERRO_DE_NOVO || rc == libav::AVERRO_FIM {
                    return Ok(());
                }
                if rc < 0 {
                    bail!("avcodec_receive_packet: {}", texto_do_erro(rc));
                }
                let p = &*self.pacote;
                f(Saida {
                    bytes: std::slice::from_raw_parts(p.data, p.size as usize),
                    pts: p.pts,
                    chave: p.flags & libav::PACOTE_CHAVE != 0,
                });
                (av().av_packet_unref)(self.pacote);
            }
        }
    }

    /// Faz o próximo quadro sair como IDR.
    ///
    /// Existe para atender PLI. Quem entra numa transmissão em andamento não
    /// consegue decodificar nada até receber um quadro-chave; sem isto teria
    /// de esperar o próximo GOP, que são dois segundos de tela preta.
    pub const fn pede_chave(&mut self) {
        self.chave = true;
    }

    /// O pool guarda NV12, e não a BGRA que a captura entrega.
    ///
    /// Quando é o caso, escrever no pool NÃO pode ser uma cópia: `CopyResource`
    /// não converte cor. Tem de passar pelo processador de vídeo, que converte.
    #[must_use]
    pub fn converte_cor(&self) -> bool {
        matches!(self.sw, ff::AVPixelFormat::AV_PIX_FMT_NV12)
    }

    /// Qual encoder abriu, para o log dizer o que está rodando.
    #[must_use]
    pub const fn nome(&self) -> &'static str {
        self.nome
    }

    #[must_use]
    pub const fn ajuste(&self) -> Ajuste {
        self.ajuste
    }

    unsafe fn libera_codec(&mut self) {
        if !self.codec.is_null() {
            (av().avcodec_free_context)(&mut self.codec);
        }
        // A vista QSV antes do pool que ela enxerga: ela o referencia.
        if !self.frames_qsv.is_null() {
            (av().av_buffer_unref)(&mut self.frames_qsv);
        }
        if !self.frames.is_null() {
            (av().av_buffer_unref)(&mut self.frames);
        }
        if !self.dispositivo_qsv.is_null() {
            (av().av_buffer_unref)(&mut self.dispositivo_qsv);
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            self.libera_codec();
            if !self.pacote.is_null() {
                (av().av_packet_free)(&mut self.pacote);
            }
            if !self.dispositivo.is_null() {
                (av().av_buffer_unref)(&mut self.dispositivo);
            }
        }
    }
}

/// Opções específicas de cada encoder, todas mirando latência.
/// Opções por encoder. `gop` é o intervalo de quadro-chave, em quadros.
///
/// # Por que o AMF precisa de tanta coisa explícita
///
/// Porque ele não faz NADA por padrão que uma transmissão ao vivo precise, e
/// isso custou uma noite de tela preta. O AMD documenta que o AMF **não inclui
/// SPS e PPS nos IDR** por conta própria (`GPUOpen-LibrariesAndSDKs/AMF#293`),
/// e o `h264_amf` do libavcodec só traduz `pict_type = I` para IDR de verdade
/// se `forced_idr` estiver ligado — sem ele sai um I-frame comum, que não serve
/// para quem está entrando na sala agora.
///
/// O OBS fixa as três coisas na mão, e é o que passamos a fazer
/// (`plugins/obs-ffmpeg/texture-amf.cpp`): `IDR_PERIOD` = gop,
/// `HEADER_INSERTION_SPACING` = gop e `FORCE_PICTURE_TYPE` = IDR. Aqui os nomes
/// são os do invólucro do libavcodec: `g`, `header_spacing` e `forced_idr`.
fn opcoes(nome: &str, gop: u32) -> Vec<(String, String)> {
    let p = |c: &str, v: &str| (c.to_string(), v.to_string());
    match nome {
        // p1 é o preset mais rápido e `ll` a sintonia de baixa latência.
        // `cbr` mantém a taxa previsível, que é o que a escada assume.
        "h264_nvenc" => vec![
            p("preset", "p1"),
            p("tune", "ll"),
            p("rc", "cbr"),
            p("zerolatency", "1"),
            // O nvenc já repete os parâmetros a cada IDR sozinho — isto é só
            // para o quadro-chave pedido por PLI sair IDR e não I.
            p("forced_idr", "1"),
        ],
        "h264_amf" => vec![
            p("usage", "ultralowlatency"),
            p("rc", "cbr"),
            p("quality", "speed"),
            // Repete SPS e PPS junto do quadro-chave. Sem isto eles saem UMA
            // vez, na abertura, e quem entra depois nunca monta imagem.
            p("header_spacing", &gop.to_string()),
            p("forced_idr", "1"),
        ],
        _ => Vec::new(),
    }
}

pub(crate) fn texto_do_erro(rc: i32) -> String {
    let mut buf = [0i8; 128];
    unsafe {
        (av().av_strerror)(rc, buf.as_mut_ptr(), buf.len());
        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    }
}

/// Todos os encoders que vale a pena interrogar numa sonda.
///
/// Inclui o `h264_qsv`, que NÃO está em [`CANDIDATOS`]: ele recusa
/// `AV_PIX_FMT_D3D11` e por isso não serve, mas saber disso pela máquina de
/// quem usa QSV vale mais que supor.
const SONDADOS: [&str; 3] = ["h264_nvenc", "h264_amf", "h264_qsv"];

/// As DLLs da libavcodec estão carregadas e é seguro chamá-la.
///
/// # Por que isto existe
///
/// As DLLs são baixadas para um cache, então na primeira execução elas não
/// existem — e o app é linkado com `/DELAYLOAD` justamente para conseguir
/// abrir assim mesmo (ver `app/build.rs`). O preço é que a primeira chamada à
/// libavcodec, se a DLL faltar, derruba o processo com uma exceção estruturada
/// que o Rust não captura.
///
/// Então a regra é simples e vale para o módulo inteiro: **nada aqui chama a
/// libavcodec antes de isto devolver `true`**. Carregamos as DLLs por caminho
/// completo; o auxiliar do delay-load encontra os módulos já carregados pelo
/// nome e nunca chega a procurar em disco.
///
/// Uma vez só por processo: o resultado não muda no meio da execução, e tentar
/// de novo a cada transmissão só somaria latência.
pub fn disponivel(pasta: &Path) -> bool {
    static PRONTO: OnceLock<bool> = OnceLock::new();
    *PRONTO.get_or_init(|| carrega_dlls(pasta))
}

/// Nomes-base das DLLs de que precisamos, sem o sufixo de versão.
const BASES: [&str; 3] = ["avcodec-", "avutil-", "swresample-"];

fn carrega_dlls(pasta: &Path) -> bool {
    let Ok(itens) = std::fs::read_dir(pasta) else {
        return false;
    };
    let mut carregadas = 0;
    for item in itens.flatten() {
        let nome = item.file_name();
        let Some(nome) = nome.to_str() else { continue };
        if !nome.ends_with(".dll") || !BASES.iter().any(|b| nome.starts_with(b)) {
            continue;
        }
        // Caminho completo de propósito: pelo nome, o Windows procuraria na
        // ordem de busca padrão e poderia achar OUTRA versão instalada na
        // máquina — que é como se linka contra uma ABI e se executa contra
        // outra.
        let largo: Vec<u16> = item
            .path()
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        if unsafe { LoadLibraryW(PCWSTR(largo.as_ptr())) }.is_ok() {
            carregadas += 1;
        }
    }
    // Uma que falte já basta para a primeira chamada derrubar o processo.
    carregadas == BASES.len()
}

/// Descobre quais encoders desta máquina aceitam textura D3D11.
///
/// # Por que uma sonda em vez de simplesmente ligar o caminho novo
///
/// Um encoder pode ABRIR e mesmo assim produzir imagem errada — cor trocada,
/// artefato, quadro preto. Ligar o caminho novo direto na máquina de alguém
/// arrisca justamente o que queremos preservar: uma transmissão que hoje
/// funciona. A sonda roda ao lado, gasta uns 200 ms uma vez, escreve no log o
/// que encontrou e some. A transmissão nunca depende dela.
///
/// Usa um dispositivo próprio, e não o da captura, para poder rodar antes de
/// existir captura alguma. Num notebook com duas GPUs isso pode escolher um
/// adaptador diferente do que o WGC escolheria — é a imprecisão aceita em
/// troca de a sonda não tocar em nada.
#[must_use]
pub fn sonda(pasta_libav: &Path) -> String {
    if !libav::carrega(pasta_libav) {
        return format!("libav não carregou de {}", pasta_libav.display());
    }
    let (dispositivo, contexto) = match dispositivo_de_sonda() {
        Ok(par) => par,
        Err(e) => return format!("sem dispositivo D3D11: {e}"),
    };
    let ajuste = Ajuste {
        largura: 1280,
        altura: 720,
        fps: 60,
        kbps: 4000,
    };

    // O `avcodec_open2` devolve "Unknown error occurred" para quase tudo; o
    // motivo de verdade ("DLL amfrt64.dll failed to open") o ffmpeg só escreve
    // no log dele. Como o relatório da sonda pode ser a ÚNICA coisa que vamos
    // ter da máquina de outra pessoa, capturamos esse log e o juntamos ao erro.
    let _captura = CapturaDeLog::liga();

    let mut partes = Vec::new();
    for nome in SONDADOS {
        recolhido().lock().map(|mut v| v.clear()).ok();
        partes.push(match sonda_um(&dispositivo, &contexto, ajuste, nome) {
            Ok((quadros, bytes)) => format!("{nome} ok ({quadros} quadros, {bytes} B)"),
            Err(e) => {
                let ffmpeg = recolhido()
                    .lock()
                    .map(|v| v.join(" / "))
                    .unwrap_or_default();
                let detalhe = if ffmpeg.is_empty() {
                    uma_linha(&format!("{e:#}"))
                } else {
                    uma_linha(&ffmpeg)
                };
                format!("{nome} não: {detalhe}")
            }
        });
    }
    partes.join("; ")
}

/// Linhas que o ffmpeg escreveu enquanto a sonda rodava.
fn recolhido() -> &'static std::sync::Mutex<Vec<String>> {
    static LINHAS: std::sync::OnceLock<std::sync::Mutex<Vec<String>>> = std::sync::OnceLock::new();
    LINHAS.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Desvia o log do ffmpeg para [`recolhido`] enquanto viver.
///
/// O callback do ffmpeg é global e sem estado por instância, daí o `Drop`
/// restaurar o padrão: deixá-lo desviado faria o resto do programa engolir
/// mensagens de erro em silêncio.
struct CapturaDeLog;

impl CapturaDeLog {
    fn liga() -> Self {
        unsafe { (av().av_log_set_callback)(Some(coleta)) };
        Self
    }
}

impl Drop for CapturaDeLog {
    fn drop(&mut self) {
        unsafe { (av().av_log_set_callback)(Some(av().av_log_default_callback)) };
    }
}

/// Callback do `av_log`. Formata com o próprio ffmpeg — lidar com `va_list` no
/// Rust não é portável, e `av_log_format_line2` existe exatamente para isto.
unsafe extern "C" fn coleta(
    ptr: *mut c_void,
    nivel: i32,
    fmt: *const std::os::raw::c_char,
    vl: libav::va_list,
) {
    // Aviso e acima; o resto é ruído de depuração do ffmpeg.
    if nivel > libav::NIVEL_AVISO {
        return;
    }
    let mut buf = [0i8; 512];
    let mut prefixo = 1i32;
    let n = (av().av_log_format_line2)(
        ptr,
        nivel,
        fmt,
        vl,
        buf.as_mut_ptr(),
        buf.len() as i32,
        &raw mut prefixo,
    );
    if n <= 0 {
        return;
    }
    let texto = CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned();
    let texto = sem_prefixo(texto.trim());
    if texto.is_empty() {
        return;
    }
    if let Ok(mut v) = recolhido().lock() {
        // Teto para uma falha em cascata não encher a memória.
        if v.len() < 12 {
            v.push(texto);
        }
    }
}

/// Abre um encoder, encoda alguns quadros e devolve (quadros, bytes).
fn sonda_um(
    dispositivo: &ID3D11Device,
    contexto: &ID3D11DeviceContext,
    ajuste: Ajuste,
    nome: &'static str,
) -> Result<(usize, usize)> {
    let mut enc = Encoder::monta(dispositivo, contexto, ajuste, nome)?;
    let origem = textura_de_sonda(dispositivo, ajuste.largura, ajuste.altura)?;
    let (mut quadros, mut bytes) = (0usize, 0usize);
    for _ in 0..10 {
        enc.envia(&origem, 0, None)?;
        enc.drena(|s| {
            quadros += 1;
            bytes += s.bytes.len();
        })?;
    }
    if quadros == 0 {
        bail!("abriu mas não devolveu quadro");
    }
    Ok((quadros, bytes))
}

pub(crate) fn dispositivo_de_sonda() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let (mut dev, mut ctx) = (None, None);
    let mut nivel = D3D_FEATURE_LEVEL::default();
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut dev),
            Some(&mut nivel),
            Some(&mut ctx),
        )?;
    }
    match (dev, ctx) {
        (Some(d), Some(c)) => Ok((d, c)),
        _ => bail!("D3D11CreateDevice não devolveu dispositivo"),
    }
}

pub(crate) fn textura_de_sonda(dev: &ID3D11Device, l: u32, a: u32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: l,
        Height: a,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        ..Default::default()
    };
    let mut t = None;
    unsafe { dev.CreateTexture2D(&desc, None, Some(&mut t))? };
    t.ok_or_else(|| anyhow::anyhow!("CreateTexture2D não devolveu textura"))
}

/// Achata a mensagem para caber numa linha do log.
fn uma_linha(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Grava o que um encoder produz e devolve se a imagem decodifica sem ser preta.
///
/// Compartilhado pelos testes de imagem: o modo de falha que interessa —
/// inicializa bem e transmite preto — não é específico de um encoder, e cada um
/// deles converte cor de um jeito diferente.
#[cfg(test)]
pub(crate) fn grava_e_confere(
    enc: &mut Encoder,
    escreve: &mut dyn FnMut(&mut Encoder) -> Result<()>,
    quadros: usize,
) -> std::result::Result<(), String> {
    use std::io::Write as _;

    let ff = concat!(env!("CARGO_MANIFEST_DIR"), "/../bin/ffmpeg.exe");
    if !Path::new(ff).exists() {
        return Err("sem bin/ffmpeg.exe para decodificar".into());
    }
    let caminho = std::env::temp_dir().join(format!("tela-teste-{}.h264", enc.nome()));
    let _ = std::fs::remove_file(&caminho);
    let mut arquivo = std::fs::File::create(&caminho).map_err(|e| e.to_string())?;
    let mut bytes = 0usize;
    for _ in 0..quadros {
        escreve(enc).map_err(|e| format!("{e:#}"))?;
        enc.drena(|s| {
            let _ = arquivo.write_all(s.bytes);
            bytes += s.bytes.len();
        })
        .map_err(|e| format!("{e:#}"))?;
    }
    let _ = arquivo.flush();
    drop(arquivo);
    if bytes == 0 {
        let _ = std::fs::remove_file(&caminho);
        return Err("o encoder não produziu byte nenhum".into());
    }
    let veredito = crate::ffmpeg::ultimo_quadro_tem_imagem(ff, &caminho);
    let _ = std::fs::remove_file(&caminho);
    match veredito {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!("o encoder {} produziu imagem PRETA", enc.nome())),
        Err(e) => Err(format!("não consegui decodificar: {e}")),
    }
}

/// Ajudante compartilhado pelos testes que precisam de um adaptador específico.
#[cfg(test)]
pub(crate) mod testes_qsv_apoio {
    use super::*;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1};

    /// Um dispositivo D3D11 no adaptador cujo nome contenha `marca`.
    ///
    /// O `dispositivo_de_sonda` pega o adaptador padrão, que num notebook com
    /// placa dedicada é a dedicada — e aí o QSV nunca seria exercitado. Aqui o
    /// adaptador é escolhido a dedo para o teste poder existir.
    pub(crate) fn dispositivo_de(marca: &str) -> Option<(ID3D11Device, ID3D11DeviceContext)> {
        let fabrica: IDXGIFactory1 = unsafe { CreateDXGIFactory1().ok()? };
        for i in 0.. {
            let Ok(adaptador): Result<IDXGIAdapter, _> = (unsafe { fabrica.EnumAdapters(i) })
            else {
                return None;
            };
            let Ok(desc) = (unsafe { adaptador.GetDesc() }) else {
                continue;
            };
            let nome = String::from_utf16_lossy(&desc.Description)
                .trim_end_matches('\0')
                .to_string();
            if !nome.to_lowercase().contains(&marca.to_lowercase()) {
                continue;
            }
            let (mut dev, mut ctx) = (None, None);
            let mut nivel = D3D_FEATURE_LEVEL::default();
            let ok = unsafe {
                D3D11CreateDevice(
                    &adaptador,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut dev),
                    Some(&mut nivel),
                    Some(&mut ctx),
                )
            };
            if ok.is_err() {
                continue;
            }
            return dev.zip(ctx);
        }
        None
    }
}

#[cfg(test)]
mod testes {
    use super::*;

    /// Um dispositivo e uma textura BGRA, no lugar dos que o WGC entrega.
    ///
    /// Reusa os ajudantes da sonda de propósito: se o teste montasse o cenário
    /// por conta própria, passaria a exercitar um caminho que ninguém roda.
    fn cenario(l: u32, a: u32) -> (ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D) {
        let (dev, ctx) = dispositivo_de_sonda().expect("dispositivo D3D11");
        let t = textura_de_sonda(&dev, l, a).expect("textura");
        (dev, ctx, t)
    }

    /// Pinta a textura para haver algo real a comprimir.
    fn pinta(dev: &ID3D11Device, ctx: &ID3D11DeviceContext, t: &ID3D11Texture2D, tom: f32) {
        let mut rtv = None;
        unsafe {
            dev.CreateRenderTargetView(t, None, Some(&mut rtv))
                .expect("rtv");
            ctx.ClearRenderTargetView(&rtv.unwrap(), &[tom, 1.0 - tom, 0.5, 1.0]);
        }
    }

    /// Sem a libav carregada nada aqui roda; ver `libav::carrega`.
    fn pronto() -> bool {
        if crate::libav::carrega_de_teste() {
            return true;
        }
        eprintln!("sem vendor/ffmpeg, pulando");
        false
    }

    /// Máquina sem GPU compatível não deve reprovar o teste — o caminho do
    /// `ffmpeg.exe` existe exatamente para ela.
    macro_rules! ou_pula {
        ($e:expr) => {
            match $e {
                Ok(v) => v,
                Err(erro) => {
                    eprintln!("sem encoder de hardware aqui, pulando: {erro}");
                    return;
                }
            }
        };
    }

    #[test]
    fn encoda_textura_sem_passar_pela_cpu() {
        if !pronto() {
            return;
        }
        let (dev, ctx, origem) = cenario(1920, 1080);
        let ajuste = Ajuste {
            largura: 1920,
            altura: 1080,
            fps: 60,
            kbps: 8000,
        };
        let mut enc = ou_pula!(Encoder::abre(&dev, &ctx, ajuste));

        let mut quadros = 0usize;
        let mut bytes = 0usize;
        let mut chaves = 0usize;
        for i in 0..60 {
            pinta(&dev, &ctx, &origem, i as f32 / 60.0);
            enc.envia(&origem, 0, None).expect("envia");
            enc.drena(|s| {
                quadros += 1;
                bytes += s.bytes.len();
                chaves += usize::from(s.chave);
            })
            .expect("drena");
        }

        assert!(
            quadros > 0,
            "o encoder ({}) não devolveu quadro",
            enc.nome()
        );
        assert!(bytes > 0, "quadros vazios");
        assert!(
            chaves > 0,
            "nenhum quadro-chave: o decodificador não teria por onde começar"
        );
    }

    #[test]
    fn reconfigura_mantendo_a_linha_do_tempo() {
        if !pronto() {
            return;
        }
        let (dev, ctx, origem) = cenario(1280, 720);
        let ajuste = Ajuste {
            largura: 1280,
            altura: 720,
            fps: 60,
            kbps: 4000,
        };
        let mut enc = ou_pula!(Encoder::abre(&dev, &ctx, ajuste));

        let mut ultimo = -1i64;
        let confere = |e: &mut Encoder, u: &mut i64| {
            for _ in 0..20 {
                pinta(&dev, &ctx, &origem, 0.3);
                e.envia(&origem, 0, None).expect("envia");
                e.drena(|s| {
                    assert!(s.pts > *u, "pts andou para trás: {} depois de {}", s.pts, u);
                    *u = s.pts;
                })
                .expect("drena");
            }
        };
        confere(&mut enc, &mut ultimo);
        let antes = ultimo;

        // A escada mexendo só na taxa.
        enc.reconfigura(Ajuste {
            kbps: 2000,
            ..ajuste
        })
        .expect("reconfigura");
        assert_eq!(enc.ajuste().kbps, 2000);
        confere(&mut enc, &mut ultimo);

        assert!(
            ultimo > antes,
            "a contagem de apresentação reiniciou na reconfiguração"
        );
    }

    #[test]
    fn reconfigurar_para_o_mesmo_ajuste_nao_faz_nada() {
        if !pronto() {
            return;
        }
        let (dev, ctx, _) = cenario(1280, 720);
        let ajuste = Ajuste {
            largura: 1280,
            altura: 720,
            fps: 60,
            kbps: 4000,
        };
        let mut enc = ou_pula!(Encoder::abre(&dev, &ctx, ajuste));
        let codec_antes = enc.codec;
        enc.reconfigura(ajuste).expect("reconfigura");
        assert_eq!(codec_antes, enc.codec, "recriou o contexto à toa");
    }
}

#[cfg(test)]
mod testes_sonda {
    /// Não afirma nada sobre o resultado — o que a máquina tem varia. Serve
    /// para ver o relatório e para garantir que a sonda nunca entra em pânico.
    /// A sonda carrega a libav sozinha — é ela que recebe a pasta —, então não
    /// precisa da guarda que os outros testes usam. Em máquina sem `vendor/`
    /// ela relata "libav não carregou", que também é uma resposta válida.
    #[test]
    fn a_sonda_relata_sem_quebrar() {
        let relato = super::sonda(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../vendor/ffmpeg/bin"
        )));
        eprintln!("sonda: {relato}");
        assert!(!relato.is_empty());
    }
}

#[cfg(test)]
mod testes_sai_imagem {
    use super::*;
    use std::io::Write as _;

    /// O que sai do nosso encoder tem de DECODIFICAR e ter imagem.
    ///
    /// # Por que este teste existe
    ///
    /// "Abre" não é a pergunta. Uma Radeon RX 6600 já inicializou o `h264_amf`
    /// sem reclamar e transmitiu preto para a sala inteira — está no comentário
    /// de `ffmpeg::testa_encoder`, e foi o que motivou a sonda a decodificar de
    /// volta. O caminho em processo precisa do mesmo rigor: aqui o encoder é
    /// outro, o formato de entrada é outro (textura BGRA em vez de NV12), e
    /// nada disso passa pelas conferências que já existiam.
    ///
    /// Grava o Annex-B cru que o encoder produz, manda o ffmpeg decodificar e
    /// reduzir a um pixel em escala de cinza. Quadro preto dá zero.
    #[test]
    fn o_que_encodamos_decodifica_com_imagem() {
        if !crate::libav::carrega_de_teste() {
            eprintln!("sem vendor/ffmpeg, pulando");
            return;
        }
        let ff = concat!(env!("CARGO_MANIFEST_DIR"), "/../bin/ffmpeg.exe");
        if !std::path::Path::new(ff).exists() {
            eprintln!("sem bin/ffmpeg.exe para decodificar, pulando");
            return;
        }
        let Ok((dev, ctx)) = dispositivo_de_sonda() else {
            eprintln!("sem D3D11, pulando");
            return;
        };
        let ajuste = Ajuste {
            largura: 1280,
            altura: 720,
            fps: 60,
            kbps: 6000,
        };
        let Ok(origem) = textura_de_sonda(&dev, ajuste.largura, ajuste.altura) else {
            eprintln!("sem textura, pulando");
            return;
        };
        let Ok(mut enc) = Encoder::abre(&dev, &ctx, ajuste) else {
            eprintln!("sem encoder de hardware, pulando");
            return;
        };

        // Cores que variam a cada quadro: preto por acaso fica impossível.
        let mut rtv = None;
        unsafe {
            dev.CreateRenderTargetView(&origem, None, Some(&mut rtv))
                .expect("rtv");
        }
        let rtv = rtv.expect("rtv");

        let caminho = std::env::temp_dir().join("tela-teste-saida.h264");
        let _ = std::fs::remove_file(&caminho);
        let mut arquivo = std::fs::File::create(&caminho).expect("criar arquivo");
        let mut bytes = 0usize;
        for i in 0..90 {
            let t = i as f32 / 90.0;
            unsafe { ctx.ClearRenderTargetView(&rtv, &[t, 1.0 - t, 0.5, 1.0]) };
            enc.envia(&origem, 0, None).expect("envia");
            enc.drena(|s| {
                arquivo.write_all(s.bytes).expect("gravar");
                bytes += s.bytes.len();
            })
            .expect("drena");
        }
        arquivo.flush().expect("flush");
        drop(arquivo);
        assert!(bytes > 0, "o encoder não produziu byte nenhum");

        let veredito = crate::ffmpeg::ultimo_quadro_tem_imagem(ff, &caminho);
        let _ = std::fs::remove_file(&caminho);
        match veredito {
            Ok(true) => {}
            Ok(false) => panic!("o encoder ({}) produziu imagem PRETA", enc.nome()),
            Err(e) => panic!("não consegui decodificar o que gravamos: {e}"),
        }
    }
}

#[cfg(test)]
mod testes_qsv {
    use super::testes_qsv_apoio::dispositivo_de;
    use super::*;

    /// O QSV precisa da textura MAPEADA, não copiada — e o mapeamento tem de
    /// funcionar de ponta a ponta.
    ///
    /// Máquina sem Intel se pula. Aqui ele importa porque é o caminho de quem
    /// só tem integrada, e de quem tem placa dedicada com driver velho demais
    /// para o nvenc (foi o caso real de um dos amigos: RTX 3060 recusada por
    /// exigir API 13.1 e o driver oferecer 13.0).
    #[test]
    fn encoda_pela_intel_mapeando_a_textura() {
        if !crate::libav::carrega_de_teste() {
            eprintln!("sem vendor/ffmpeg, pulando");
            return;
        }
        let Some((dev, ctx)) = dispositivo_de("intel") else {
            eprintln!("sem adaptador Intel nesta máquina, pulando");
            return;
        };
        let ajuste = Ajuste {
            largura: 1280,
            altura: 720,
            fps: 60,
            kbps: 4000,
        };
        let mut enc = match Encoder::monta(&dev, &ctx, ajuste, "h264_qsv") {
            Ok(e) => e,
            Err(erro) => {
                eprintln!("QSV não abriu aqui: {erro:#}");
                return;
            }
        };
        assert!(enc.converte_cor(), "o pool do QSV tem de ser NV12");

        // O processador de vídeo é obrigatório aqui: ele converte BGRA→NV12.
        let origem = textura_de_sonda(&dev, 1280, 720).expect("textura");
        let escalador =
            crate::escalador::Escalador::novo(&dev, &ctx, (1280, 720), (1280, 720)).expect("vp");

        let mut quadros = 0usize;
        for _ in 0..30 {
            enc.preenche(|destino, camada| escalador.aplica(&origem, 0, destino, camada))
                .expect("preenche");
            enc.drena(|_| quadros += 1).expect("drena");
        }
        assert!(quadros > 0, "o QSV não devolveu quadro nenhum");
        eprintln!("QSV: {quadros} quadros pela Intel");
    }
}

#[cfg(test)]
mod sonda_nv12 {
    use super::testes_qsv_apoio::dispositivo_de;
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};

    /// Que combinação de flags a GPU aceita para uma textura NV12 em array?
    ///
    /// O pool do QSV é isso, e o `E_INVALIDARG` do `CreateTexture2D` não diz
    /// qual campo ele recusou. Em vez de adivinhar, pergunta.
    #[test]
    #[ignore = "sonda de hardware; serve para inspeção"]
    fn que_flags_a_gpu_aceita() {
        for marca in ["intel", "nvidia"] {
            let Some((dev, _)) = dispositivo_de(marca) else {
                println!("{marca}: sem adaptador");
                continue;
            };
            println!("--- {marca} ---");
            for camadas in [1u32, 40] {
                for (rotulo, bind) in [
                    ("nenhum", 0u32),
                    ("RT", D3D11_BIND_RENDER_TARGET.0 as u32),
                    ("SR", D3D11_BIND_SHADER_RESOURCE.0 as u32),
                    (
                        "RT|SR",
                        (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                    ),
                    ("DECODER", D3D11_BIND_DECODER.0 as u32),
                    ("VIDEO_ENCODER", D3D11_BIND_VIDEO_ENCODER.0 as u32),
                    (
                        "DECODER|RT",
                        (D3D11_BIND_DECODER.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
                    ),
                ] {
                    let d = D3D11_TEXTURE2D_DESC {
                        Width: 1280,
                        Height: 720,
                        MipLevels: 1,
                        ArraySize: camadas,
                        Format: DXGI_FORMAT_NV12,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: bind,
                        ..Default::default()
                    };
                    let mut t = None;
                    let r = unsafe { dev.CreateTexture2D(&d, None, Some(&mut t)) };
                    println!(
                        "  {camadas:>3} camada(s)  {rotulo:<14} {}",
                        if r.is_ok() { "ok" } else { "recusado" }
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod testes_imagem_qsv {
    use super::testes_qsv_apoio::dispositivo_de;
    use super::*;

    /// O QSV também tem de produzir IMAGEM, não só quadros.
    ///
    /// Aqui a conversão de cor é feita pelo processador de vídeo (BGRA→NV12),
    /// e não pelo encoder — um caminho a mais para sair preto sem ninguém
    /// perceber até a sala inteira estar olhando para o nada.
    #[test]
    fn qsv_produz_imagem_de_verdade() {
        if !crate::libav::carrega_de_teste() {
            eprintln!("sem vendor/ffmpeg, pulando");
            return;
        }
        let Some((dev, ctx)) = dispositivo_de("intel") else {
            eprintln!("sem adaptador Intel, pulando");
            return;
        };
        let ajuste = Ajuste {
            largura: 1280,
            altura: 720,
            fps: 60,
            kbps: 4000,
        };
        let mut enc = match Encoder::monta(&dev, &ctx, ajuste, "h264_qsv") {
            Ok(e) => e,
            Err(erro) => {
                eprintln!("QSV não abriu aqui: {erro:#}");
                return;
            }
        };
        let origem = textura_de_sonda(&dev, 1280, 720).expect("textura");
        let mut rtv = None;
        unsafe {
            dev.CreateRenderTargetView(&origem, None, Some(&mut rtv))
                .expect("rtv");
        }
        let rtv = rtv.expect("rtv");
        let escalador =
            crate::escalador::Escalador::novo(&dev, &ctx, (1280, 720), (1280, 720)).expect("vp");

        let mut i = 0f32;
        let mut escreve = |e: &mut Encoder| {
            i += 1.0;
            let t = (i / 60.0).fract();
            unsafe { ctx.ClearRenderTargetView(&rtv, &[t, 1.0 - t, 0.5, 1.0]) };
            e.preenche(|destino, camada| escalador.aplica(&origem, 0, destino, camada))
        };
        match grava_e_confere(&mut enc, &mut escreve, 60) {
            Ok(()) => eprintln!("QSV: imagem confere"),
            Err(e) if e.starts_with("sem bin/ffmpeg") => eprintln!("{e}, pulando"),
            Err(e) => panic!("{e}"),
        }
    }
}
