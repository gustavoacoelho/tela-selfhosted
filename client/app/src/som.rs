//! Avisos sonoros de começo e fim de transmissão.
//!
//! Existem pelo mesmo motivo que no Discord: quem compartilha costuma estar
//! olhando para o jogo, não para o app. Sem um retorno audível, a única forma
//! de saber se a transmissão pegou é procurar a janela — e a única forma de
//! notar que ela caiu é alguém avisar no chat.
//!
//! Toca com `PlaySoundW` do `winmm`, direto da memória. É uma chamada de
//! sistema contra trazer um mixer inteiro para tocar meio segundo de senoide.

use windows::core::PCWSTR;
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};

/// WAVs embutidos. Estáticos de propósito: `SND_MEMORY | SND_ASYNC` devolve na
/// hora e o Windows continua lendo o buffer depois — um `Vec` local viraria
/// leitura de memória liberada.
static COMECOU: &[u8] = include_bytes!("../sons/comecou.wav");
static PAROU: &[u8] = include_bytes!("../sons/parou.wav");

/// Qual aviso tocar.
#[derive(Debug, Clone, Copy)]
pub enum Aviso {
    Comecou,
    Parou,
}

/// Toca o aviso, sem bloquear.
///
/// Falha em silêncio: sem placa de som, com o áudio mudo ou com o dispositivo
/// tomado por outro programa, não há nada a fazer nem nada a reportar — o
/// aviso é conveniência, não parte da transmissão.
pub fn toca(aviso: Aviso) {
    let dados = match aviso {
        Aviso::Comecou => COMECOU,
        Aviso::Parou => PAROU,
    };
    // SAFETY: `dados` é `'static`, então continua válido enquanto o Windows
    // toca em segundo plano. `SND_NODEFAULT` evita o bipe padrão do sistema
    // caso o buffer não sirva — um erro nosso não pode virar barulho.
    let ok = unsafe {
        PlaySoundW(
            PCWSTR(dados.as_ptr().cast()),
            None,
            SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
        )
    };
    if !ok.as_bool() {
        crate::registro::linha(&format!("aviso sonoro não tocou ({aviso:?})"));
    }
}

#[cfg(test)]
mod testes {
    use super::*;

    /// Toca os dois avisos de verdade. Ignorado por padrão — é para ouvir.
    #[test]
    #[ignore = "faz barulho"]
    fn ouve_os_dois() {
        toca(Aviso::Comecou);
        std::thread::sleep(std::time::Duration::from_millis(700));
        toca(Aviso::Parou);
        std::thread::sleep(std::time::Duration::from_millis(700));
    }

    /// Os WAVs embutidos são WAVs mesmo, com conteúdo.
    ///
    /// `PlaySoundW` com `SND_NODEFAULT` falha calado num buffer inválido —
    /// nada tocaria e nada explicaria por quê.
    #[test]
    fn os_wavs_embutidos_sao_validos() {
        for (nome, d) in [("comecou", COMECOU), ("parou", PAROU)] {
            assert_eq!(&d[0..4], b"RIFF", "{nome} não é RIFF");
            assert_eq!(&d[8..12], b"WAVE", "{nome} não é WAVE");
            assert!(
                d.len() > 10_000,
                "{nome} está curto demais: {} bytes",
                d.len()
            );
        }
    }
}
