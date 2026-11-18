//! Motor de captura do `tela'`.
//!
//! Biblioteca permanente: hoje só o CLI de diagnóstico a consome, mas o app
//! Tauri do Plano 2 usa exatamente estes módulos. Por isso nada aqui imprime
//! em stdout nem decide política de linha de comando — a lib captura e
//! entrega bytes, quem decide o que fazer com eles é o chamador.
//!
//! Os dois problemas que justificam a complexidade deste código estão
//! documentados em [`video::pump`] (tela parada) e [`audio::pump`]
//! (aplicativo mudo).

pub mod audio;
#[cfg(feature = "encoder-d3d11")]
pub mod encoder;
#[cfg(feature = "encoder-d3d11")]
pub mod escalador;
#[cfg(feature = "encoder-d3d11")]
pub mod libav;
#[cfg(feature = "encoder-d3d11")]
pub mod opus;

/// Sem a feature não há libavcodec nenhuma neste binário.
///
/// O módulo continua existindo, com `carrega` que sempre recusa, para quem o
/// chama não precisar de `cfg` — a resposta "não dá para usar o encoder em
/// processo" é a mesma de uma máquina sem as DLLs, e o tratamento também.
#[cfg(not(feature = "encoder-d3d11"))]
pub mod libav {
    use std::path::Path;

    #[must_use]
    pub fn carrega(_pasta: &Path) -> bool {
        false
    }
}
pub mod ffmpeg;
pub mod foco;
pub mod hdr;
pub mod pipes;
pub mod processos;
pub mod video;
pub mod whip;
