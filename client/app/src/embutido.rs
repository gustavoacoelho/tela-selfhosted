//! O `ffmpeg.exe` embutido no próprio executável.
//!
//! A promessa do produto é **um arquivo só**: o amigo baixa `tela.exe`, dá dois
//! cliques e transmite. Um `.zip` com dois arquivos ao lado quebra isso na
//! prática — alguém extrai só o `.exe`, ou move para outra pasta, e o app
//! reclama de um ffmpeg que "sumiu".
//!
//! # Por que atrás de uma feature
//!
//! O binário do ffmpeg tem ~140 MB. Um `include_bytes!` desse tamanho deixa
//! **todo** build lento, inclusive os de desenvolvimento, onde o ffmpeg de
//! `bin/` já está ao lado. A feature `ffmpeg-embutido` fica ligada só na
//! compilação de distribuição.
//!
//! # Onde é extraído
//!
//! Num diretório de cache, com o tamanho no nome. Extrair 140 MB a cada
//! execução seria absurdo, e um nome estável permite reaproveitar. O tamanho no
//! nome faz a troca de versão gerar um arquivo novo sozinha, sem precisar de
//! invalidação manual.

use std::path::PathBuf;

use anyhow::Result;
// Só o caminho da extração usa `Context`; sem a feature ele nem existe.
#[cfg(feature = "ffmpeg-embutido")]
use anyhow::Context;

/// Bytes do ffmpeg, embutidos em tempo de compilação.
///
/// O caminho é relativo a este arquivo. `client/bin/ffmpeg.exe` não é
/// versionado (é grande e vem de download), então a compilação com esta feature
/// exige tê-lo baixado — ver `deploy/RESULTADO.md`.
#[cfg(feature = "ffmpeg-embutido")]
const FFMPEG: &[u8] = include_bytes!("../../bin/ffmpeg.exe");

/// Garante um ffmpeg utilizável e devolve o caminho.
///
/// Com a feature ligada, extrai o embutido para o cache (uma vez) e devolve
/// esse caminho. Sem a feature, devolve `None` para quem chamou cair no
/// caminho configurado — o `bin/ffmpeg.exe` ao lado do executável.
#[cfg(feature = "ffmpeg-embutido")]
pub fn garante() -> Result<Option<PathBuf>> {
    let destino = caminho_cache(FFMPEG.len())?;

    // Já extraído numa execução anterior. Confere o tamanho porque uma
    // extração interrompida (falta de espaço, processo morto no meio) deixa um
    // arquivo truncado que falharia de forma confusa lá na frente.
    if let Ok(meta) = std::fs::metadata(&destino) {
        if meta.len() == FFMPEG.len() as u64 {
            return Ok(Some(destino));
        }
    }

    let pasta = destino
        .parent()
        .context("caminho de cache sem diretório pai")?;
    std::fs::create_dir_all(pasta).with_context(|| format!("criar {}", pasta.display()))?;

    // Escreve em arquivo temporário e só então renomeia. Sem isso, duas
    // instâncias abertas ao mesmo tempo — ou uma interrompida no meio —
    // deixariam um ffmpeg pela metade no lugar definitivo.
    let parcial = destino.with_extension("parcial");
    std::fs::write(&parcial, FFMPEG)
        .with_context(|| format!("extrair ffmpeg para {}", parcial.display()))?;

    match std::fs::rename(&parcial, &destino) {
        Ok(()) => {}
        Err(e) => {
            // Outra instância pode ter vencido a corrida e já renomeado. Se o
            // destino está lá e com o tamanho certo, o objetivo foi cumprido.
            let _ = std::fs::remove_file(&parcial);
            let ok = std::fs::metadata(&destino)
                .map(|m| m.len() == FFMPEG.len() as u64)
                .unwrap_or(false);
            if !ok {
                return Err(e).with_context(|| format!("mover para {}", destino.display()));
            }
        }
    }

    Ok(Some(destino))
}

#[cfg(not(feature = "ffmpeg-embutido"))]
pub fn garante() -> Result<Option<PathBuf>> {
    Ok(None)
}

/// `%LOCALAPPDATA%\tela\ffmpeg-<tamanho>.exe`, com o diretório temporário do
/// sistema como reserva.
#[cfg(feature = "ffmpeg-embutido")]
fn caminho_cache(tamanho: usize) -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    Ok(base.join("tela").join(format!("ffmpeg-{tamanho}.exe")))
}
