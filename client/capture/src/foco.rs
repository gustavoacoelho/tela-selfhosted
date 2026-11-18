//! O que está debaixo do mouse e o que está em primeiro plano.
//!
//! Serve aos atalhos globais: quem está jogando não vai sair do jogo, achar a
//! janela do tela', procurar o monitor certo na lista e clicar em transmitir.
//! Aperta o atalho e vai.

use anyhow::{Context as _, Result};
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, HMONITOR, MONITOR_DEFAULTTONEAREST};
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, GetForegroundWindow};
use windows_capture::monitor::Monitor;

use crate::video::{list_monitors, list_windows, MonitorSource, WindowSource};

/// O monitor onde o ponteiro está.
///
/// Casa pelo índice e não pelo `HMONITOR` porque é o índice que o resto do
/// programa usa para abrir a captura — e ele vem do nome do dispositivo
/// (`\.\DISPLAY2`), que é estável enquanto os monitores não mudam.
pub fn monitor_sob_o_mouse() -> Result<MonitorSource> {
    let mut ponto = POINT::default();
    // SAFETY: `ponto` é uma struct válida e a API só escreve nela.
    unsafe { GetCursorPos(&mut ponto) }.context("posição do ponteiro")?;

    // SAFETY: `MonitorFromPoint` não falha — com `DEFAULTTONEAREST` sempre
    // devolve um monitor, mesmo com o ponto fora de todos eles.
    let hmon: HMONITOR = unsafe { MonitorFromPoint(ponto, MONITOR_DEFAULTTONEAREST) };

    let indice = Monitor::from_raw_hmonitor(hmon.0)
        .index()
        .map_err(|e| anyhow::anyhow!("índice do monitor sob o ponteiro: {e}"))?;

    list_monitors()?
        .into_iter()
        .find(|m| m.index == indice)
        .with_context(|| format!("o monitor {indice} não está na lista de capturáveis"))
}

/// A janela em primeiro plano.
///
/// Passa pela `list_windows` de propósito, em vez de devolver o `HWND` cru: é
/// a mesma lista que a interface mostra, com os mesmos filtros. Uma janela que
/// não está lá também não é capturável, e é melhor dizer isso do que abrir uma
/// captura que nunca entrega quadro.
pub fn janela_em_foco() -> Result<WindowSource> {
    // SAFETY: sem parâmetros; devolve HWND nulo quando não há janela ativa.
    let hwnd: HWND = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        anyhow::bail!("nenhuma janela em primeiro plano");
    }
    let alvo = hwnd.0 as isize;

    list_windows()?
        .into_iter()
        .find(|w| w.hwnd == alvo)
        .context("a janela em primeiro plano não é capturável")
}

#[cfg(test)]
mod testes {
    use super::*;

    /// O ponteiro está sempre em algum monitor — inclusive na máquina de CI,
    /// que tem um só. Se isto falhar, o mapeamento ponto→índice quebrou.
    #[test]
    fn acha_o_monitor_do_ponteiro() {
        let m = monitor_sob_o_mouse().expect("monitor sob o ponteiro");
        assert!(m.index >= 1, "índice inválido: {}", m.index);
        assert!(m.width > 0 && m.height > 0, "resolução inválida: {m:?}");

        let todos = list_monitors().expect("listar monitores");
        assert!(
            todos.iter().any(|o| o.index == m.index),
            "o monitor devolvido não está entre os listados"
        );
    }

    /// Não afirma QUAL janela: num teste não há como saber. Afirma que a
    /// resposta é coerente — ou uma janela da lista, ou um erro explicando.
    #[test]
    fn janela_em_foco_e_coerente() {
        match janela_em_foco() {
            Ok(j) => {
                let todas = list_windows().expect("listar janelas");
                assert!(
                    todas.iter().any(|o| o.hwnd == j.hwnd),
                    "devolveu uma janela que não está na lista"
                );
                assert!(j.pid > 0);
            }
            Err(e) => {
                let msg = format!("{e:#}");
                assert!(
                    msg.contains("primeiro plano"),
                    "erro sem explicação útil: {msg}"
                );
            }
        }
    }
}

#[cfg(test)]
mod diagnostico {
    /// Imprime o que os atalhos escolheriam agora. Manual, para conferir
    /// contra a realidade da tela.
    #[test]
    #[ignore = "diagnóstico manual"]
    fn o_que_os_atalhos_pegariam() {
        let mut p = windows::Win32::Foundation::POINT::default();
        let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut p) };
        println!("ponteiro em ({}, {})", p.x, p.y);
        println!("monitores: {:?}", crate::video::list_monitors());
        println!("monitor sob o mouse: {:?}", super::monitor_sob_o_mouse());
        println!("janela em foco: {:?}", super::janela_em_foco());
    }
}

// ---------------------------------------------------------------------------
// Seguir a fonte quando ela troca de janela
// ---------------------------------------------------------------------------

/// O que se sabe sobre a janela que estava sendo transmitida.
///
/// Guardado ANTES de ela morrer: depois, o processo pode já não existir e não
/// haveria como perguntar nada a respeito dele.
#[derive(Debug, Clone)]
pub struct Assinatura {
    pub hwnd: isize,
    pub pid: u32,
    pub titulo: String,
    /// Nome do executável, minúsculo.
    pub exe: String,
    /// Caminho completo do executável, se deu para descobrir.
    pub caminho: Option<String>,
    /// O processo e seus ancestrais.
    pub linhagem: Vec<u32>,
}

/// Fotografa a janela para poder reconhecê-la depois de ela trocar de forma.
#[must_use]
pub fn assina(hwnd: isize, pid: u32, titulo: &str) -> Assinatura {
    let caminho = crate::processos::caminho_do_executavel(pid);
    let exe = caminho
        .as_deref()
        .and_then(|c| c.rsplit(['\\', '/']).next())
        .unwrap_or_default()
        .to_lowercase();
    Assinatura {
        hwnd,
        pid,
        titulo: titulo.to_string(),
        exe,
        caminho,
        linhagem: crate::processos::linhagem(pid),
    }
}

/// Pastas que muitos programas dividem, e que portanto não identificam
/// nenhum deles.
///
/// Sem esta lista, `C:\Program Files\WindowsApps\...` faria todo aplicativo
/// da Store parecer o mesmo programa, e `AppData` faria isso com metade do que
/// há instalado — encontrado rodando o diagnóstico numa máquina de verdade,
/// não imaginando casos.
const PASTAS_GENERICAS: [&str; 14] = [
    "program files",
    "program files (x86)",
    "programfiles",
    "windowsapps",
    "common files",
    "appdata",
    "local",
    "locallow",
    "roaming",
    "programs",
    "users",
    "windows",
    "steamapps",
    "common",
];

/// Os dois executáveis pertencem à mesma instalação.
///
/// Compara o prefixo comum em vez de cortar num número fixo de pastas:
/// `C:\Riot Games\League of Legends\LeagueClient.exe` e
/// `...\League of Legends\Game\League of Legends.exe` moram em níveis
/// diferentes, e qualquer corte fixo erraria um dos dois.
///
/// O prefixo ainda precisa ser específico: três pastas no mínimo, e a última
/// delas não pode ser uma que todo mundo divide — senão dois jogos da Steam,
/// ou dois programas em `AppData`, virariam o mesmo.
#[must_use]
pub fn mesma_instalacao(a: &str, b: &str) -> bool {
    let partes = |c: &str| -> Vec<String> {
        c.split(['\\', '/'])
            .filter(|p| !p.is_empty())
            .map(str::to_lowercase)
            .collect()
    };
    let (x, y) = (partes(a), partes(b));

    // Descarta o nome do executável: comparar instalação é comparar pastas.
    let (x, y) = (
        &x[..x.len().saturating_sub(1)],
        &y[..y.len().saturating_sub(1)],
    );

    let comum: Vec<&String> = x
        .iter()
        .zip(y.iter())
        .take_while(|(p, q)| p == q)
        .map(|(p, _)| p)
        .collect();
    if comum.len() < 3 {
        return false;
    }
    let ultima = comum[comum.len() - 1].as_str();
    !PASTAS_GENERICAS.contains(&ultima)
}

/// Quão bem uma janela candidata substitui a que morreu. Maior é melhor;
/// `None` é "não tem nada a ver".
fn parecenca(a: &Assinatura, cand: &WindowSource) -> Option<u32> {
    // A própria janela de antes, se ainda estiver por aí, não serve: quem
    // chama só procura sucessora porque ela morreu.
    if cand.hwnd == a.hwnd {
        return None;
    }
    let caminho = crate::processos::caminho_do_executavel(cand.pid);
    let exe = caminho
        .as_deref()
        .and_then(|c| c.rsplit(['\\', '/']).next())
        .unwrap_or_default()
        .to_lowercase();

    // O mesmo processo abriu outra janela: é certeza, não palpite.
    if cand.pid == a.pid {
        return Some(100);
    }
    // Mesmo executável, outro processo: o programa foi reaberto.
    if !exe.is_empty() && exe == a.exe {
        return Some(90);
    }
    // Mesma pasta de instalação: lançador e jogo.
    if let (Some(x), Some(y)) = (a.caminho.as_deref(), caminho.as_deref()) {
        if mesma_instalacao(x, y) {
            return Some(80);
        }
    }
    // Parentesco: o processo que abrimos gerou este, ou os dois vieram do
    // mesmo pai. Vale menos porque um lançador genérico (Steam) aparentaria
    // jogos que não têm relação entre si.
    let linhagem_cand = crate::processos::linhagem(cand.pid);
    if linhagem_cand.contains(&a.pid) || a.linhagem.contains(&cand.pid) {
        return Some(70);
    }
    // Mesmo título: raro, mas é o que sobra quando o programa se recria
    // inteiro. Exige título não trivial para não casar por acaso.
    if a.titulo.len() > 3 && cand.title == a.titulo {
        return Some(60);
    }
    None
}

/// Procura a janela que substituiu a que estava sendo transmitida.
///
/// Existe porque jogo não é um programa só. O League fecha o cliente para abrir
/// o jogo e o reabre ao terminar a partida, e sem isto a transmissão morria no
/// meio — quem estava assistindo via a tela sumir e quem jogava tinha de largar
/// a partida para clicar em transmitir de novo.
#[must_use]
pub fn acha_sucessora(a: &Assinatura) -> Option<WindowSource> {
    let janelas = list_windows().ok()?;
    janelas
        .into_iter()
        .filter_map(|j| parecenca(a, &j).map(|nota| (nota, j)))
        // Empate: a janela maior tende a ser o jogo, e não uma janelinha de
        // aviso do mesmo programa.
        .max_by_key(|(nota, j)| (*nota, u32::from(!j.minimizada), j.title.len()))
        .map(|(_, j)| j)
}

#[cfg(test)]
mod testes_sucessora {
    use super::*;

    /// O caso que motivou tudo isto: o cliente do League e o jogo em si sao
    /// executaveis com nomes sem nada em comum, em niveis diferentes da mesma
    /// instalacao.
    #[test]
    fn cliente_e_jogo_do_league_sao_a_mesma_instalacao() {
        assert!(mesma_instalacao(
            r"C:\Riot Games\League of Legends\LeagueClient.exe",
            r"C:\Riot Games\League of Legends\Game\League of Legends.exe",
        ));
    }

    /// Programas diferentes nao podem virar a mesma coisa.
    #[test]
    fn programas_diferentes_nao_se_confundem() {
        assert!(!mesma_instalacao(
            r"C:\Program Files\Mozilla Firefox\firefox.exe",
            r"C:\Program Files\Notepad++\notepad++.exe",
        ));
    }

    /// Todo aplicativo da Store mora em `WindowsApps`. Sem tratar isso, o
    /// primeiro que aparecesse na lista sequestraria a transmissao de qualquer
    /// outro — achado rodando o diagnostico numa maquina real, nao imaginando.
    #[test]
    fn apps_da_store_nao_se_confundem() {
        assert!(!mesma_instalacao(
            r"C:\Program Files\WindowsApps\Claude_1.0_x64__abc\app\claude.exe",
            r"C:\Program Files\WindowsApps\Spotify_2.0_x64__xyz\Spotify.exe",
        ));
    }

    /// `AppData` e o `WindowsApps` de quem instala sem administrador.
    #[test]
    fn programas_em_appdata_nao_se_confundem() {
        assert!(!mesma_instalacao(
            r"C:\Users\gus\AppData\Local\Discord\app-1.0\Discord.exe",
            r"C:\Users\gus\AppData\Local\Programs\Outro\outro.exe",
        ));
    }

    /// Dois jogos da Steam dividem a mesma pasta `common`.
    #[test]
    fn jogos_da_steam_nao_se_confundem() {
        assert!(!mesma_instalacao(
            r"D:\SteamLibrary\steamapps\common\Jogo A\a.exe",
            r"D:\SteamLibrary\steamapps\common\Jogo B\b.exe",
        ));
    }

    /// Mas duas partes do MESMO jogo da Steam, sim.
    #[test]
    fn partes_do_mesmo_jogo_da_steam_se_reconhecem() {
        assert!(mesma_instalacao(
            r"D:\SteamLibrary\steamapps\common\Jogo A\launcher.exe",
            r"D:\SteamLibrary\steamapps\common\Jogo A\bin\jogo.exe",
        ));
    }

    /// Caminho raso nao identifica instalacao nenhuma.
    #[test]
    fn caminho_raso_nao_casa() {
        assert!(!mesma_instalacao(r"C:\jogo.exe", r"C:\outro.exe"));
        assert!(!mesma_instalacao("", ""));
    }
    fn janela(hwnd: isize, pid: u32, titulo: &str) -> WindowSource {
        WindowSource {
            hwnd,
            pid,
            title: titulo.to_string(),
            minimizada: false,
        }
    }

    fn assinatura(hwnd: isize, pid: u32, titulo: &str) -> Assinatura {
        Assinatura {
            hwnd,
            pid,
            titulo: titulo.to_string(),
            exe: "leagueclient.exe".to_string(),
            caminho: Some(r"C:\Riot Games\League of Legends\LeagueClient.exe".to_string()),
            linhagem: vec![pid],
        }
    }

    /// A janela que morreu nunca é a própria sucessora — senão o app ficaria
    /// tentando reabrir algo que já não existe.
    #[test]
    fn nao_se_escolhe_a_si_mesma() {
        let a = assinatura(10, 100, "League of Legends");
        assert_eq!(parecenca(&a, &janela(10, 100, "League of Legends")), None);
    }

    /// Mesmo processo, janela nova: é a resposta mais forte possível.
    #[test]
    fn mesmo_processo_ganha_de_tudo() {
        let a = assinatura(10, 100, "League of Legends");
        let mesmo = parecenca(&a, &janela(11, 100, "outro titulo"));
        let outro = parecenca(&a, &janela(12, 999, "League of Legends"));
        assert!(
            mesmo > outro,
            "mesmo pid deveria valer mais: {mesmo:?} vs {outro:?}"
        );
    }

    /// Título igual só vale quando é específico o bastante.
    #[test]
    fn titulo_curto_nao_casa() {
        let mut a = assinatura(10, 100, "ab");
        a.caminho = None;
        a.exe = String::new();
        assert_eq!(parecenca(&a, &janela(11, 999, "ab")), None);
    }
}

#[cfg(test)]
mod diagnostico_sucessora {
    /// Fotografa a janela em foco e mostra o que seria escolhido se ela
    /// morresse agora. Manual: serve para conferir a heurística contra a
    /// máquina de verdade, com os programas que a pessoa realmente usa.
    #[test]
    #[ignore = "diagnóstico manual"]
    fn quem_substituiria_a_janela_em_foco() {
        let Ok(j) = super::janela_em_foco() else {
            println!("nenhuma janela em foco");
            return;
        };
        let a = super::assina(j.hwnd, j.pid, &j.title);
        println!("janela: \"{}\" pid={} exe={}", j.title, j.pid, a.exe);
        println!("caminho: {:?}", a.caminho);
        println!("linhagem: {:?}", a.linhagem);
        println!(
            "sucessora que seria escolhida: {:?}",
            super::acha_sucessora(&a)
        );
    }
}

#[cfg(test)]
mod testes_sucessora_ao_vivo {
    use super::*;

    /// Cria dois processos do mesmo executável, mata o que estava sendo
    /// "transmitido" e confere que a busca acha o outro.
    ///
    /// Vale mais que os testes de caminho: exercita a enumeração de janelas, a
    /// leitura do caminho do executável e a pontuação, tudo contra o Windows
    /// de verdade. Ignorado por padrão porque abre janelas na tela.
    ///
    /// `winver` e não `powershell`: a janela de um programa de console
    /// pertence ao terminal, não ao processo — os dois PIDs saíram iguais na
    /// primeira tentativa e o teste não provava nada.
    #[test]
    #[ignore = "abre janelas"]
    fn segue_para_outra_janela_do_mesmo_programa() {
        use std::process::Command;

        let mut a = Command::new("winver.exe").spawn().expect("abrir winver");
        std::thread::sleep(std::time::Duration::from_millis(800));
        let mut b = Command::new("winver.exe").spawn().expect("abrir winver");
        std::thread::sleep(std::time::Duration::from_secs(2));

        let janelas_de = |pid: u32| {
            list_windows()
                .expect("listar janelas")
                .into_iter()
                .find(|j| j.pid == pid)
        };
        let limpa = |a: &mut std::process::Child, b: &mut std::process::Child| {
            let _ = a.kill();
            let _ = b.kill();
        };

        let (Some(ja), Some(jb)) = (janelas_de(a.id()), janelas_de(b.id())) else {
            limpa(&mut a, &mut b);
            panic!("as duas janelas de teste tinham de estar visíveis");
        };
        assert_ne!(ja.pid, jb.pid, "os dois processos tinham de ser distintos");

        let assinatura = assina(ja.hwnd, ja.pid, &ja.title);
        assert_eq!(assinatura.exe, "winver.exe");

        // Some com a janela que estava no ar.
        let _ = a.kill();
        let _ = a.wait();
        std::thread::sleep(std::time::Duration::from_secs(2));

        let escolhida = acha_sucessora(&assinatura);
        let _ = b.kill();
        let _ = b.wait();

        let escolhida = escolhida.expect("devia ter achado a outra janela");
        assert_eq!(
            escolhida.pid, jb.pid,
            "escolheu \"{}\" (pid {}) em vez da outra janela do mesmo programa",
            escolhida.title, escolhida.pid
        );
    }
}

#[cfg(test)]
mod testes_troca_sem_fechar {
    use super::*;

    /// O caso do League: a janela antiga continua EXISTINDO, só sai de vista,
    /// e outra do mesmo programa aparece.
    ///
    /// Vale como teste separado porque a busca por sucessora não pode exigir
    /// que a anterior tenha morrido — foi assim que a primeira versão deixou
    /// alguém transmitindo o lobby por 46 minutos durante a partida.
    #[test]
    #[ignore = "abre janelas"]
    fn acha_sucessora_com_a_anterior_ainda_viva() {
        use crate::video::estado_da_janela;
        use std::process::Command;

        let mut a = Command::new("winver.exe").spawn().expect("abrir winver");
        std::thread::sleep(std::time::Duration::from_millis(800));
        let mut b = Command::new("winver.exe").spawn().expect("abrir winver");
        std::thread::sleep(std::time::Duration::from_secs(2));

        let janela_de = |pid: u32| {
            list_windows()
                .expect("listar")
                .into_iter()
                .find(|j| j.pid == pid)
        };
        let (Some(ja), Some(jb)) = (janela_de(a.id()), janela_de(b.id())) else {
            let _ = a.kill();
            let _ = b.kill();
            panic!("as duas janelas tinham de existir");
        };

        let assinatura = assina(ja.hwnd, ja.pid, &ja.title);
        // A anterior continua viva e capturável — é justamente o ponto.
        assert!(
            estado_da_janela(ja.hwnd).capturavel(),
            "a janela anterior tinha de continuar viva neste teste"
        );

        let escolhida = acha_sucessora(&assinatura);
        let _ = a.kill();
        let _ = b.kill();
        let _ = a.wait();
        let _ = b.wait();

        let escolhida = escolhida.expect("devia ter achado a outra janela");
        assert_eq!(
            escolhida.pid, jb.pid,
            "escolheu \"{}\" (pid {}), esperava a outra janela do mesmo programa",
            escolhida.title, escolhida.pid
        );
        assert_ne!(escolhida.hwnd, ja.hwnd, "não pode escolher a própria");
    }
}
