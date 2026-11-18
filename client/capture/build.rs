//! Gera os tipos da libavcodec — e **nada** que faça o Windows carregá-la.
//!
//! # Por que não linkar
//!
//! Linkar pela biblioteca de importação faz o Windows resolver as DLLs antes
//! do `main`. Como elas são baixadas sob demanda (são 71 MB, não cabem no
//! executável), na primeira execução ainda não existem — e o app não abriria.
//! Nem uma janela de erro: o processo morre com `STATUS_DLL_NOT_FOUND`.
//!
//! O `/DELAYLOAD` deveria resolver isso e não resolve: o link.exe aceita a
//! opção e a descarta em silêncio (`LNK4199`, visível só com `/WX`), porque o
//! Rust chama as funções pelo thunk que a import lib fornece e não por
//! `__imp_`, e o linker só sabe adiar as segundas. Foi verificado num programa
//! mínimo, então não é peculiaridade do nosso link.
//!
//! Daí a saída: o bindgen gera só `struct`s, `enum`s e constantes, com TODA
//! função bloqueada, e ninguém emite `cargo:rustc-link-lib`. As funções são
//! resolvidas em tempo de execução por `GetProcAddress` — ver `src/libav.rs`.
//! O executável não tem importação nenhuma da libav, abre sempre, e usa o
//! encoder em processo só quando as DLLs estão lá.

use std::env;
#[cfg(feature = "encoder-d3d11")]
use std::fs;
#[cfg(feature = "encoder-d3d11")]
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    gera();
}

/// Sem a feature não há bindgen no grafo, então nem o corpo pode existir.
/// Compilar sem ela é a saída para quem clonou o repositório e ainda não
/// preparou o `vendor/`.
#[cfg(not(feature = "encoder-d3d11"))]
fn gera() {}

#[cfg(feature = "encoder-d3d11")]
fn gera() {
    let Some(dir) = env::var_os("FFMPEG_DIR").map(PathBuf::from) else {
        println!(
            "cargo:warning=FFMPEG_DIR não definido; \
             rode client/scripts/preparar-ffmpeg.ps1"
        );
        return;
    };
    let include = dir.join("include");

    // Um cabeçalho só, com o que o encoder toca. Menos headers, menos tipos
    // gerados, compilação mais rápida.
    let fonte = r"
        #include <libavcodec/avcodec.h>
        #include <libavutil/hwcontext.h>
        #include <libavutil/opt.h>
    ";

    let bindings = bindgen::Builder::default()
        .header_contents("libav.h", fonte)
        .clang_arg(format!("-I{}", include.display()))
        // O ponto de tudo: nenhuma declaração de função. Sem elas não há
        // símbolo indefinido, sem símbolo não há import, e o executável abre
        // com ou sem as DLLs.
        .blocklist_function(".*")
        .allowlist_type("AV.*")
        .allowlist_var("AV_.*|AVERROR.*|FF_.*")
        // Enums em Rust dão exaustividade nos `match` e nomes legíveis no log.
        .rustified_enum("AVPixelFormat|AVHWDeviceType|AVPictureType|AVMediaType")
        .derive_debug(true)
        .layout_tests(false)
        // Sem os comentários do ffmpeg: eles trazem trechos de C que o rustdoc
        // tenta compilar como Rust, e a suíte enche de doctest reprovado por
        // documentação que não é nossa.
        .generate_comments(false)
        .generate()
        .expect("gerar bindings da libav");

    let saida = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("libav.rs");
    bindings.write_to_file(&saida).expect("gravar bindings");

    // Conferência barata contra o erro que seria silencioso e caríssimo: se
    // algum `extern` escapou do bloqueio, o linker voltaria a exigir a DLL na
    // abertura e o app pararia de abrir em quem ainda não a baixou.
    let texto = fs::read_to_string(&saida).expect("reler bindings");
    assert!(
        !texto.contains("extern \"C\" {"),
        "os bindings declararam funções; isso recria a importação que este \
         build.rs existe para evitar"
    );
}
