//! A libavcodec resolvida em tempo de execução, sem importação nenhuma.
//!
//! # Por que assim
//!
//! As DLLs são baixadas sob demanda, como o `ffmpeg.exe` sempre foi — são
//! 71 MB e não cabem no executável. Se o binário as importasse, o Windows as
//! exigiria ANTES do `main`, e na primeira execução o app não abriria: morre
//! com `STATUS_DLL_NOT_FOUND`, sem janela de erro nenhuma.
//!
//! `/DELAYLOAD` deveria resolver e não resolve — o link.exe aceita a opção e a
//! descarta em silêncio, porque o Rust chama pelo thunk da import lib e não por
//! `__imp_` (detalhes em `build.rs`). Então nada é linkado: os tipos vêm do
//! bindgen, as funções vêm de `GetProcAddress`, e o executável não sabe que a
//! libav existe até alguém chamar [`carrega`].
//!
//! # A regra
//!
//! Ninguém toca em função da libav sem passar por [`api`], e [`api`] só
//! devolve algo depois de [`carrega`] ter encontrado as três DLLs. Não é
//! disciplina opcional: chamar um ponteiro nulo aqui derruba o processo.

#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case)]

use std::ffi::{c_char, c_int, c_uint, c_void};
use std::os::windows::ffi::OsStrExt as _;
use std::path::Path;
use std::sync::OnceLock;

use windows::core::PCSTR;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
    LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
};

/// Tipos, `enum`s e constantes gerados pelo bindgen. Sem uma função sequer.
pub mod tipos {
    #![allow(
        non_camel_case_types,
        non_upper_case_globals,
        non_snake_case,
        dead_code,
        clippy::all,
        clippy::pedantic
    )]
    include!(concat!(env!("OUT_DIR"), "/libav.rs"));
}

use tipos::{
    AVBufferRef, AVCodec, AVCodecContext, AVDictionary, AVFrame, AVHWDeviceType, AVPacket,
};

/// `va_list` no Windows x64.
///
/// Definido aqui e não pelo bindgen porque, sem nenhuma função declarada, o
/// tipo não é referenciado por nada e não chega a ser gerado. Na ABI x64 da
/// Microsoft `va_list` é só um ponteiro para a área de argumentos — e nós nunca
/// o interpretamos: apenas o repassamos ao `av_log_format_line2`, que é quem
/// sabe lê-lo.
pub type va_list = *mut c_char;

/// `AVERROR(EAGAIN)`. O encoder ainda não tem pacote pronto — o retorno mais
/// comum de todos, e não é erro.
///
/// Escrito à mão porque `AVERROR(x)` é macro, e o bindgen não gera macros com
/// conta dentro. `EAGAIN` é 11 no runtime da Microsoft, e `AVERROR(e)` é `-e`.
pub const AVERRO_DE_NOVO: c_int = -11;

/// `AVERROR_EOF`, que é `-MKTAG('E','O','F',' ')`.
pub const AVERRO_FIM: c_int = -(0x2046_4F45_i32);

/// `AV_LOG_WARNING`. Do aviso para cima é o que vale a pena registrar.
pub const NIVEL_AVISO: c_int = 24;

/// `AV_PKT_FLAG_KEY`.
pub const PACOTE_CHAVE: c_int = 0x0001;

/// `AV_HWFRAME_MAP_READ | AV_HWFRAME_MAP_DIRECT`.
///
/// `DIRECT` é o que faz o mapeamento apontar para a MESMA memória em vez de
/// copiar — que é o ponto de tudo isto. `READ` porque o encoder só lê.
pub const MAPA_DIRETO: c_int = 1 | 8;

/// Nomes-base das DLLs, sem o sufixo de versão (que muda a cada major).
const BASES: [&str; 3] = ["avcodec-", "avutil-", "swresample-"];

/// O que o `GetProcAddress` devolve: um ponteiro de função sem assinatura.
type Simbolo = unsafe extern "system" fn() -> isize;

macro_rules! tabela {
    ($($nome:ident : $tipo:ty),+ $(,)?) => {
        /// As funções da libav que usamos. Nada além delas é resolvido.
        pub struct Api {
            $(pub $nome: $tipo,)+
        }

        impl Api {
            fn resolve(modulos: &[HMODULE]) -> Option<Self> {
                Some(Self {
                    // O NOME de quem falta, e não só "faltou alguma função":
                    // sem ele, o log de outra máquina não diz o que procurar, e
                    // a diferença entre "DLL de outra versão" e "outra coisa"
                    // fica em aberto. Aconteceu com a máquina do Fael.
                    $($nome: unsafe {
                        std::mem::transmute::<Simbolo, $tipo>(
                            match simbolo(modulos, concat!(stringify!($nome), "\0")) {
                                Some(p) => p,
                                None => {
                                    anota_falta(stringify!($nome));
                                    return None;
                                }
                            }
                        )
                    },)+
                })
            }
        }
    };
}

tabela! {
    avcodec_version: unsafe extern "C" fn() -> c_uint,
    av_hwdevice_ctx_alloc: unsafe extern "C" fn(AVHWDeviceType) -> *mut AVBufferRef,
    av_hwdevice_ctx_init: unsafe extern "C" fn(*mut AVBufferRef) -> c_int,
    av_hwframe_ctx_alloc: unsafe extern "C" fn(*mut AVBufferRef) -> *mut AVBufferRef,
    av_hwframe_ctx_init: unsafe extern "C" fn(*mut AVBufferRef) -> c_int,
    av_hwframe_get_buffer: unsafe extern "C" fn(*mut AVBufferRef, *mut AVFrame, c_int) -> c_int,
    // Os três abaixo existem por causa do QSV: ele recusa `AV_PIX_FMT_D3D11` e
    // exige que a textura seja MAPEADA para uma superfície dele. Ver
    // `Encoder::monta`.
    av_hwdevice_ctx_create_derived: unsafe extern "C" fn(
        *mut *mut AVBufferRef, AVHWDeviceType, *mut AVBufferRef, c_int
    ) -> c_int,
    av_hwframe_ctx_create_derived: unsafe extern "C" fn(
        *mut *mut AVBufferRef, c_int, *mut AVBufferRef, *mut AVBufferRef, c_int
    ) -> c_int,
    av_hwframe_map: unsafe extern "C" fn(*mut AVFrame, *const AVFrame, c_int) -> c_int,
    av_buffer_ref: unsafe extern "C" fn(*const AVBufferRef) -> *mut AVBufferRef,
    av_buffer_unref: unsafe extern "C" fn(*mut *mut AVBufferRef),
    av_frame_alloc: unsafe extern "C" fn() -> *mut AVFrame,
    // Áudio: o quadro de PCM precisa de memória própria, e o layout de canais
    // no ffmpeg 7+ é uma struct que só a própria libav sabe preencher.
    av_frame_get_buffer: unsafe extern "C" fn(*mut AVFrame, c_int) -> c_int,
    av_channel_layout_default: unsafe extern "C" fn(*mut tipos::AVChannelLayout, c_int),
    av_frame_free: unsafe extern "C" fn(*mut *mut AVFrame),
    av_packet_alloc: unsafe extern "C" fn() -> *mut AVPacket,
    av_packet_free: unsafe extern "C" fn(*mut *mut AVPacket),
    av_packet_unref: unsafe extern "C" fn(*mut AVPacket),
    av_opt_set: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, c_int) -> c_int,
    av_strerror: unsafe extern "C" fn(c_int, *mut c_char, usize) -> c_int,
    av_log_set_callback:
        unsafe extern "C" fn(Option<unsafe extern "C" fn(*mut c_void, c_int, *const c_char, va_list)>),
    av_log_default_callback: unsafe extern "C" fn(*mut c_void, c_int, *const c_char, va_list),
    av_log_format_line2: unsafe extern "C" fn(
        *mut c_void, c_int, *const c_char, va_list, *mut c_char, c_int, *mut c_int
    ) -> c_int,
    avcodec_find_encoder_by_name: unsafe extern "C" fn(*const c_char) -> *const AVCodec,
    avcodec_alloc_context3: unsafe extern "C" fn(*const AVCodec) -> *mut AVCodecContext,
    avcodec_free_context: unsafe extern "C" fn(*mut *mut AVCodecContext),
    avcodec_open2:
        unsafe extern "C" fn(*mut AVCodecContext, *const AVCodec, *mut *mut AVDictionary) -> c_int,
    avcodec_send_frame: unsafe extern "C" fn(*mut AVCodecContext, *const AVFrame) -> c_int,
    avcodec_receive_packet: unsafe extern "C" fn(*mut AVCodecContext, *mut AVPacket) -> c_int,
}

/// Só o SUCESSO é memorizado.
///
/// Uma falha não pode ser definitiva: as DLLs são baixadas durante a execução,
/// e memorizar a recusa da partida fazia o app ignorar um download que acabara
/// de chegar — a pessoa precisava fechar e abrir sem que nada dissesse isso.
/// Aconteceu com a máquina de um dos amigos.
///
/// Tentar de novo é barato: `carrega` roda uma vez por transmissão, e uma DLL
/// já carregada devolve o mesmo módulo sem tocar em disco.
static API: OnceLock<Api> = OnceLock::new();

/// Carrega as DLLs de `pasta` e resolve as funções. Uma vez por processo.
///
/// Devolve `false` quando falta alguma DLL, quando o carregamento falha, ou
/// quando alguma função não está lá — este último caso significa DLL de outra
/// versão, que é pior que DLL ausente porque falharia mais tarde e de forma
/// mais confusa.
pub fn carrega(pasta: &Path) -> bool {
    if API.get().is_some() {
        return true;
    }
    // Cada tentativa parte do zero, senão as falhas de uma tentativa antiga
    // apareceriam no diagnóstico da nova.
    if let Ok(mut v) = falhas().lock() {
        v.clear();
    }
    let modulos = carrega_dlls(pasta);
    if modulos.len() < BASES.len() {
        if let Ok(mut v) = falhas().lock() {
            if v.is_empty() {
                v.push(format!(
                    "achei {} das {} DLLs em {}",
                    modulos.len(),
                    BASES.len(),
                    pasta.display()
                ));
            }
        }
        return false;
    }
    let Some(api) = Api::resolve(&modulos) else {
        return false;
    };
    let _ = API.set(api);
    true
}

/// As funções, se já foram carregadas com sucesso.
///
/// Não carrega: quem carrega é [`carrega`], no início, quando ainda dá para
/// decidir por qual caminho a transmissão vai.
#[must_use]
pub fn api() -> Option<&'static Api> {
    API.get()
}

fn carrega_dlls(pasta: &Path) -> Vec<HMODULE> {
    let Ok(itens) = std::fs::read_dir(pasta) else {
        return Vec::new();
    };
    let mut caminhos = Vec::new();
    for item in itens.flatten() {
        let nome = item.file_name();
        let Some(nome) = nome.to_str() else { continue };
        if !nome.ends_with(".dll") || !BASES.iter().any(|b| nome.starts_with(b)) {
            continue;
        }
        caminhos.push((nome.to_string(), item.path()));
    }
    // Dependência antes de quem depende: o `avcodec` importa dos outros dois.
    // Com eles já carregados, a resolução acha os módulos pelo nome e nem chega
    // a procurar em disco. É cinto sobre o suspensório da flag abaixo.
    caminhos.sort_by_key(|(nome, _)| usize::from(nome.starts_with("avcodec-")));

    let mut modulos = Vec::new();
    for (_, caminho) in caminhos {
        // Caminho completo, nunca o nome: pelo nome o Windows percorreria a
        // ordem de busca padrão e poderia achar outra versão instalada na
        // máquina — que é gerar os tipos contra uma ABI e chamar outra.
        let largo: Vec<u16> = caminho
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // `LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR` é o que faz as DEPENDÊNCIAS serem
        // procuradas na pasta da própria DLL. Sem ele o Windows as procura no
        // diretório do EXECUTÁVEL — e como as nossas moram no cache, longe do
        // `tela.exe`, o `avcodec` não achava o `avutil` e falhava.
        //
        // Custou uma sessão de teste inteira para aparecer, porque nas minhas
        // compilações o `build.rs` copia as DLLs para junto do binário: ali
        // funcionava, e só na máquina de verdade não.
        let bandeiras = LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS;
        match unsafe { LoadLibraryExW(windows::core::PCWSTR(largo.as_ptr()), None, bandeiras) } {
            Ok(m) => modulos.push(m),
            // O erro do sistema é o que distingue "não está lá" de "está lá e
            // não carrega". Engoli-lo custou uma sessão de teste: o sintoma era
            // "indisponível", e a causa era uma dependência não encontrada.
            Err(e) => {
                if let Ok(mut v) = falhas().lock() {
                    v.push(format!(
                        "{}: {e}",
                        caminho.file_name().unwrap_or_default().to_string_lossy()
                    ));
                }
            }
        }
    }
    modulos
}

/// Registra uma função que não foi encontrada.
fn anota_falta(nome: &str) {
    if let Ok(mut v) = falhas().lock() {
        v.push(format!("função ausente: {nome}"));
    }
}

/// Por que alguma DLL não carregou, se foi o caso.
fn falhas() -> &'static std::sync::Mutex<Vec<String>> {
    static V: OnceLock<std::sync::Mutex<Vec<String>>> = OnceLock::new();
    V.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Explica por que [`carrega`] recusou, para o log poder dizer.
#[must_use]
pub fn porque_falhou() -> String {
    match falhas().lock() {
        Ok(v) if !v.is_empty() => v.join("; "),
        _ => {
            "as DLLs carregaram e as funções resolveram — a recusa veio de outro lugar".to_string()
        }
    }
}

/// Procura o símbolo em todos os módulos carregados.
///
/// Sem mapear função para DLL de propósito: a divisão entre `avcodec` e
/// `avutil` já mudou entre versões do ffmpeg, e uma tabela dessas viraria uma
/// falha de "função não encontrada" numa atualização futura. Três buscas por
/// símbolo, uma vez na vida do processo, não custam nada.
fn simbolo(modulos: &[HMODULE], nome: &str) -> Option<Simbolo> {
    debug_assert!(nome.ends_with('\0'), "o nome precisa terminar em nulo");
    modulos
        .iter()
        .find_map(|m| unsafe { GetProcAddress(*m, PCSTR(nome.as_ptr())) })
}

/// Carrega a libav de `vendor/`, para os testes que precisam dela.
///
/// Devolve `false` em máquina sem o `vendor/` preparado, e aí o teste se pula
/// em vez de reprovar — quem clonou o repositório e não rodou
/// `scripts/preparar-ffmpeg.ps1` não tem o que consertar no código.
#[cfg(test)]
pub(crate) fn carrega_de_teste() -> bool {
    carrega(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../vendor/ffmpeg/bin"
    )))
}

#[cfg(test)]
mod testes {
    use super::*;

    #[test]
    fn carrega_e_resolve_as_funcoes() {
        if !carrega_de_teste() {
            eprintln!("sem vendor/ffmpeg, pulando");
            return;
        }
        let api = api().expect("carregou, então tem api");
        // Uma chamada de verdade: resolver o ponteiro não prova que ele aponta
        // para o que pensamos.
        let v = unsafe { (api.avcodec_version)() };
        assert!(v > 0, "avcodec_version devolveu zero");
        eprintln!("avcodec {}.{}.{}", v >> 16, (v >> 8) & 0xff, v & 0xff);
    }

    #[test]
    fn as_constantes_batem_com_o_que_o_ffmpeg_define() {
        // `AVERROR(EAGAIN)` = -11 e `AVERROR_EOF` = -MKTAG('E','O','F',' ').
        // Escritas à mão porque são macros; um erro aqui faria o laço de drenagem
        // tratar "ainda não tem pacote" como erro fatal, ou nunca terminar.
        assert_eq!(AVERRO_DE_NOVO, -11);
        let mktag = i32::from(b'E')
            | (i32::from(b'O') << 8)
            | (i32::from(b'F') << 16)
            | (i32::from(b' ') << 24);
        assert_eq!(AVERRO_FIM, -mktag);
    }

    #[test]
    fn pasta_inexistente_nao_carrega_nada() {
        assert!(carrega_dlls(Path::new(r"Z:\isto\nao\existe")).is_empty());
    }
}

#[cfg(test)]
mod testes_cache_real {
    use super::*;

    /// Carrega da MESMA pasta que o app usa, não do `vendor/`.
    ///
    /// As duas podem divergir — versões diferentes das DLLs, ou uma função
    /// nova que existe numa e não na outra. Quando divergem, `carrega` devolve
    /// `false` e a transmissão recua para o `ffmpeg.exe` em silêncio; este
    /// teste é o que transforma isso numa resposta em vez de um mistério.
    #[test]
    #[ignore = "depende do cache desta máquina"]
    fn carrega_da_pasta_do_app() {
        let Some(base) = std::env::var_os("LOCALAPPDATA") else {
            eprintln!("sem LOCALAPPDATA, pulando");
            return;
        };
        let pasta = Path::new(&base).join("tela").join("libav");
        println!("pasta: {}", pasta.display());
        for item in std::fs::read_dir(&pasta).into_iter().flatten().flatten() {
            let m = item.metadata().map(|m| m.len()).unwrap_or(0);
            println!("  {} ({m} bytes)", item.file_name().to_string_lossy());
        }
        let modulos = carrega_dlls(&pasta);
        println!("DLLs carregadas: {} de {}", modulos.len(), BASES.len());
        assert_eq!(modulos.len(), BASES.len(), "faltou DLL");

        // Qual função não resolve, se alguma.
        for nome in [
            "avcodec_version\0",
            "av_hwdevice_ctx_alloc\0",
            "av_hwdevice_ctx_create_derived\0",
            "av_hwframe_ctx_create_derived\0",
            "av_hwframe_map\0",
            "av_frame_get_buffer\0",
            "av_channel_layout_default\0",
            "av_log_format_line2\0",
        ] {
            let achou = simbolo(&modulos, nome).is_some();
            println!(
                "  {:<32} {}",
                nome.trim_end_matches('\0'),
                if achou { "ok" } else { "NAO ACHOU" }
            );
            assert!(
                achou,
                "{} não existe nestas DLLs",
                nome.trim_end_matches('\0')
            );
        }
        assert!(carrega(&pasta), "carrega() recusou");
    }
}
