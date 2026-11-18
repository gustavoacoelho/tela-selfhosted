//! Descobre se um monitor está em HDR.
//!
//! # Por que isto importa para transmitir
//!
//! Com HDR ligado, a área de trabalho é composta em alta faixa dinâmica e o
//! conteúdo SDR — jogo, navegador, tudo que não é HDR — é exibido no nível de
//! branco que a pessoa escolheu no controle "Brilho do conteúdo SDR". O padrão
//! escurece bastante.
//!
//! A captura em BGRA8 recebe o conteúdo já convertido para SDR de 8 bits, mas
//! SEM esse escurecimento: ela vê os valores cheios. O resultado é que a
//! transmissão sai mais clara do que o dono da tela está vendo, e ele não tem
//! como saber por quê — os dois lados estão "certos", só que medem coisas
//! diferentes.
//!
//! Não dá para consertar escurecendo a imagem por conta própria: o buffer que
//! capturamos é o correto, e escurecê-lo estragaria a imagem de todo mundo
//! para combinar com o painel de uma pessoa. O que dá para fazer é AVISAR.

use windows::core::Interface;
use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput6, DXGI_ERROR_NOT_FOUND,
};

/// Um monitor e o estado de HDR dele.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Monitor {
    /// Nome do dispositivo, no formato `\\.\DISPLAY1`.
    pub dispositivo: String,
    pub hdr: bool,
}

/// Lista os monitores e diz quais estão em HDR.
///
/// Devolve vazio quando o DXGI não coopera — placa em modo de software,
/// sessão remota, driver antigo. Falha em silêncio de propósito: isto alimenta
/// um aviso, e um aviso que impede de transmitir seria pior que não avisar.
#[must_use]
pub fn monitores() -> Vec<Monitor> {
    // SAFETY: chamadas COM do DXGI, todas checadas; nenhum ponteiro escapa.
    unsafe {
        let Ok(fabrica) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return Vec::new();
        };
        let mut achados = Vec::new();

        for i in 0.. {
            let adaptador: IDXGIAdapter1 = match fabrica.EnumAdapters1(i) {
                Ok(a) => a,
                // Fim da lista. Qualquer outro erro também encerra: insistir
                // num índice que falhou não muda o resultado.
                Err(_) => break,
            };

            for j in 0.. {
                let saida = match adaptador.EnumOutputs(j) {
                    Ok(s) => s,
                    Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                    Err(_) => break,
                };
                // `IDXGIOutput6` é quem tem `GetDesc1` com o espaço de cor.
                // Existe desde o Windows 10 1703; antes disso não há HDR de
                // desktop para detectar, então não achar é a resposta certa.
                let Ok(saida6) = saida.cast::<IDXGIOutput6>() else {
                    continue;
                };
                let Ok(desc) = saida6.GetDesc1() else {
                    continue;
                };

                achados.push(Monitor {
                    dispositivo: String::from_utf16_lossy(&desc.DeviceName)
                        .trim_end_matches('\0')
                        .to_string(),
                    // O espaço de cor da SAÍDA é o que diz se o HDR está
                    // ligado agora — não a capacidade do monitor, que estaria
                    // em `BitsPerColor` ou nos metadados de luminância. Um
                    // monitor capaz de HDR com HDR desligado compõe em sRGB e
                    // não tem problema nenhum.
                    hdr: desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
                });
            }
        }
        achados
    }
}

/// Algum monitor está em HDR?
#[must_use]
pub fn algum_em_hdr() -> bool {
    monitores().iter().any(|m| m.hdr)
}

#[cfg(test)]
mod testes {
    use super::*;

    /// Não pode explodir nem travar em máquina nenhuma — inclusive sem placa
    /// de vídeo de verdade, que é o caso de um runner de CI.
    #[test]
    fn nunca_entra_em_panico() {
        let _ = monitores();
        let _ = algum_em_hdr();
    }

    /// Todo monitor devolvido tem de vir com nome de dispositivo utilizável.
    #[test]
    fn nomes_sao_utilizaveis() {
        for m in monitores() {
            assert!(
                m.dispositivo.starts_with(r"\\.\DISPLAY"),
                "nome inesperado: {:?}",
                m.dispositivo
            );
            assert!(!m.dispositivo.contains('\0'), "sobrou NUL: {m:?}");
        }
    }

    /// Sonda manual: `cargo test -p tela-capture mostra_os_monitores -- --nocapture --ignored`
    #[test]
    #[ignore = "depende do hardware da máquina; serve para inspeção"]
    fn mostra_os_monitores() {
        for m in monitores() {
            println!("{} -> hdr={}", m.dispositivo, m.hdr);
        }
    }
}

/// Nível de branco SDR de cada caminho de vídeo, em nits.
///
/// É este número que explica o brilho: com HDR ligado, o Windows exibe branco
/// SDR nesse nível. O padrão da referência SDR são 80 nits; o valor que a
/// pessoa escolhe no controle "Brilho do conteúdo SDR" mexe exatamente aqui.
///
/// Se o valor for MAIOR que 80, o conteúdo SDR aparece na tela mais claro que
/// a referência; menor, mais escuro. A captura em BGRA8 não sabe disso e
/// sempre entrega a referência — a diferença entre os dois é o desencontro.
#[must_use]
pub fn niveis_de_branco_sdr() -> Vec<(String, f32)> {
    use windows::Win32::Devices::Display::{
        DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
        DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_DEVICE_INFO_HEADER,
        DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SDR_WHITE_LEVEL,
        QDC_ONLY_ACTIVE_PATHS,
    };

    let mut n_caminhos = 0u32;
    let mut n_modos = 0u32;
    // SAFETY: as duas chamadas preenchem os tamanhos antes de qualquer uso.
    unsafe {
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut n_caminhos, &mut n_modos)
            .is_err()
        {
            return Vec::new();
        }
    }
    let mut caminhos = vec![DISPLAYCONFIG_PATH_INFO::default(); n_caminhos as usize];
    let mut modos = vec![DISPLAYCONFIG_MODE_INFO::default(); n_modos as usize];
    // SAFETY: os vetores têm o tamanho que a chamada anterior pediu.
    unsafe {
        if QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut n_caminhos,
            caminhos.as_mut_ptr(),
            &mut n_modos,
            modos.as_mut_ptr(),
            None,
        )
        .is_err()
        {
            return Vec::new();
        }
    }
    caminhos.truncate(n_caminhos as usize);

    let mut fora = Vec::new();
    for c in &caminhos {
        let mut pedido = DISPLAYCONFIG_SDR_WHITE_LEVEL {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                size: u32::try_from(std::mem::size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>())
                    .unwrap_or(0),
                adapterId: c.targetInfo.adapterId,
                id: c.targetInfo.id,
            },
            ..Default::default()
        };
        // SAFETY: cabeçalho preenchido com o tipo e o tamanho da própria struct.
        let ok = unsafe { DisplayConfigGetDeviceInfo(&mut pedido.header) };
        if ok != 0 {
            continue;
        }
        // A unidade da API é 1/1000 do branco de referência SDR, que são
        // 80 nits. `1000` significa exatamente a referência.
        let nits = pedido.SDRWhiteLevel as f32 / 1000.0 * 80.0;
        fora.push((format!("alvo {}", c.targetInfo.id), nits));
    }
    fora
}

#[cfg(test)]
mod testes_branco {
    /// Sonda manual, junto com o estado de HDR de cada saída.
    #[test]
    #[ignore = "depende do hardware; serve para inspeção"]
    fn mostra_o_nivel_de_branco() {
        for m in super::monitores() {
            println!("{} -> hdr={}", m.dispositivo, m.hdr);
        }
        for (alvo, nits) in super::niveis_de_branco_sdr() {
            println!(
                "{alvo} -> branco SDR {nits:.0} nits  (referência 80; fator {:.2}x)",
                nits / 80.0
            );
        }
    }

    #[test]
    fn nunca_entra_em_panico() {
        let _ = super::niveis_de_branco_sdr();
    }
}

#[cfg(test)]
mod sonda_estouro {
    use crate::video::{list_monitors, start_monitor, Config, Destino, FrameBuffer};
    use std::time::Duration;

    /// Captura cada monitor e mostra o histograma de luminância.
    ///
    /// A pergunta que isto responde: com HDR ligado, a captura em 8 bits
    /// ESCALA o conteúdo SDR (que um ajuste de brilho desfaz) ou ESTOURA nele
    /// (informação perdida, e aí nenhum ajuste recupera)? Um desktop comum tem
    /// poucos pixels de branco puro; uma captura estourada tem muitos.
    #[test]
    #[ignore = "depende do hardware e do que está na tela; serve para inspeção"]
    fn histograma_por_monitor() {
        // O estado de HDR sai em lista separada de propósito: `list_monitors`
        // dá o nome comercial ("VG259Q5A") e o DXGI dá o nome de dispositivo
        // (`\\.\DISPLAY4`). Não há como casar os dois com o que temos, e uma
        // tentativa aproximada rotulava a linha errada — pior que não rotular.
        for m in super::monitores() {
            println!("  {} -> hdr={}", m.dispositivo, m.hdr);
        }

        for m in list_monitors().expect("listar monitores") {
            let buf = FrameBuffer::shared();
            let cfg = Config {
                destino: Destino::Cpu(buf.clone()),
                fps: 30,
            };
            let Ok(captura) = start_monitor(Some(m.index), cfg) else {
                println!("{}: não capturou", m.name);
                continue;
            };
            // Alguns quadros para o WGC estabilizar.
            let veio = buf.wait_first(Duration::from_secs(3));
            std::thread::sleep(Duration::from_millis(400));
            let quadro = if veio { buf.latest() } else { None };
            drop(captura);

            let Some(nv12) = quadro else {
                println!("{}: sem quadro", m.name);
                continue;
            };
            let (w, h) = buf.dims();
            let n = (w as usize) * (h as usize);
            if n == 0 || nv12.len() < n {
                println!("{}: quadro curto", m.name);
                continue;
            }
            let y = &nv12[..n];
            let soma: u64 = y.iter().map(|&v| u64::from(v)).sum();
            let media = soma as f64 / n as f64;
            // 235 é o branco do limited range que nossa conversão produz.
            let no_teto = y.iter().filter(|&&v| v >= 235).count();
            let quase = y.iter().filter(|&&v| v >= 220).count();
            let no_piso = y.iter().filter(|&&v| v <= 17).count();
            let pct = |c: usize| c as f64 * 100.0 / n as f64;

            println!(
                "{:<28} {}x{}  Y medio {:.1}  no teto(>=235) {:.1}%  quase(>=220) {:.1}%  no piso(<=17) {:.1}%",
                m.name, w, h, media, pct(no_teto), pct(quase), pct(no_piso)
            );
        }
    }
}
