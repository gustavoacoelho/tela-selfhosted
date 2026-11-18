//! Descoberta de processos por nome de executável.
//!
//! Serve ao modo anti-eco: ao compartilhar o monitor inteiro, o áudio do
//! sistema carrega a saída do aplicativo de voz — a fala das outras pessoas da
//! call — e elas se ouvem de volta. Excluir aquele processo resolve, mas antes
//! é preciso achá-lo.
//!
//! # Por que por processo e não por janela
//!
//! A primeira versão procurava pelo título da janela. Falhava justamente no
//! caso mais comum: o Discord costuma ficar **minimizado na bandeja** enquanto
//! a pessoa joga, e aí não tem janela enumerável. Varrer processos acha do
//! mesmo jeito, minimizado ou não.

use anyhow::{Context, Result};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};

/// Um processo em execução.
#[derive(Debug, Clone)]
pub struct Processo {
    pub pid: u32,
    /// Nome do executável, como o Windows reporta (ex.: `Discord.exe`).
    pub nome: String,
    /// Quem o criou.
    ///
    /// Serve para seguir um jogo que troca de processo: o cliente do League
    /// abre o jogo, e é pela árvore que se descobre que os dois são a mesma
    /// coisa — os nomes de executável não têm nada em comum.
    pub pai: u32,
}

/// Executáveis que reproduzem a voz de outras pessoas.
///
/// `vesktop` e `discordptb`/`canary` são variantes usadas na prática; o Vencord
/// não entra na lista porque é um mod que roda dentro do próprio `Discord.exe`.
pub const APPS_DE_VOZ: [&str; 5] = [
    "discord.exe",
    "discordcanary.exe",
    "discordptb.exe",
    "vesktop.exe",
    "discorddevelopment.exe",
];

/// Lista os processos em execução.
pub fn lista() -> Result<Vec<Processo>> {
    let mut out = Vec::new();

    // SAFETY: o snapshot é fechado ao final, inclusive nos caminhos de erro.
    // `PROCESSENTRY32W` exige `dwSize` preenchido antes da primeira chamada.
    unsafe {
        let snap =
            CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).context("CreateToolhelp32Snapshot")?;

        let mut entrada = PROCESSENTRY32W {
            dwSize: u32::try_from(std::mem::size_of::<PROCESSENTRY32W>())
                .expect("PROCESSENTRY32W cabe em u32"),
            ..Default::default()
        };

        if Process32FirstW(snap, &mut entrada).is_ok() {
            loop {
                let nome = String::from_utf16_lossy(&entrada.szExeFile);
                let nome = nome.trim_end_matches('\0').to_string();
                if !nome.is_empty() {
                    out.push(Processo {
                        pid: entrada.th32ProcessID,
                        nome,
                        pai: entrada.th32ParentProcessID,
                    });
                }
                if Process32NextW(snap, &mut entrada).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }

    Ok(out)
}

/// Primeiro processo cujo executável esteja em `nomes` (sem diferenciar
/// maiúsculas).
///
/// Devolve o primeiro encontrado: a API de exclusão do Windows aceita **um**
/// processo por sessão, então não haveria o que fazer com os demais.
#[must_use]
pub fn acha(nomes: &[&str]) -> Option<Processo> {
    let todos = lista().ok()?;
    todos.into_iter().find(|p| {
        let n = p.nome.to_lowercase();
        nomes.iter().any(|alvo| n == *alvo)
    })
}

/// Atalho para [`acha`] com [`APPS_DE_VOZ`].
#[must_use]
pub fn acha_app_de_voz() -> Option<Processo> {
    acha(&APPS_DE_VOZ)
}

#[cfg(test)]
mod testes {
    use super::*;

    /// A varredura tem de achar o próprio processo de teste — se não achar,
    /// está quebrada de forma silenciosa.
    #[test]
    fn lista_inclui_o_proprio_processo() {
        let meu = std::process::id();
        let todos = lista().expect("listar processos");
        assert!(todos.len() > 5, "lista suspeita: {} processos", todos.len());
        assert!(
            todos.iter().any(|p| p.pid == meu),
            "não achei o próprio PID {meu} entre {} processos",
            todos.len()
        );
    }

    #[test]
    fn nome_inexistente_nao_casa() {
        assert!(acha(&["nao-existe-mesmo-12345.exe"]).is_none());
    }
}

#[cfg(test)]
mod diagnostico {
    /// Não é asserção: imprime o que a varredura escolheria, para conferir
    /// contra a árvore real de processos.
    #[test]
    #[ignore = "diagnóstico manual"]
    fn qual_processo_de_voz_seria_escolhido() {
        let todos = super::lista().expect("listar");
        let voz: Vec<_> = todos
            .iter()
            .filter(|p| super::APPS_DE_VOZ.contains(&p.nome.to_lowercase().as_str()))
            .collect();
        println!("na ordem da varredura: {voz:?}");
        println!("escolhido hoje: {:?}", super::acha_app_de_voz());
    }
}

/// Caminho completo do executável de um PID.
///
/// Serve para reconhecer que dois processos de nomes diferentes são o mesmo
/// jogo: `LeagueClient.exe` e `League of Legends.exe` não têm nada em comum no
/// nome, mas moram os dois dentro de `Riot Games\League of Legends`.
///
/// `PROCESS_QUERY_LIMITED_INFORMATION` é o direito mínimo, e o Windows o
/// concede entre processos do mesmo usuário — que é o caso de tudo que a
/// pessoa pode compartilhar. Processos do sistema simplesmente devolvem `None`.
#[must_use]
pub fn caminho_do_executavel(pid: u32) -> Option<String> {
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: o handle é fechado logo abaixo, inclusive no caminho de erro.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 260 * 2];
        let mut tam = u32::try_from(buf.len()).ok()?;
        let r = QueryFullProcessImageNameW(
            h,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut tam,
        );
        let _ = CloseHandle(h);
        r.ok()?;
        Some(String::from_utf16_lossy(&buf[..tam as usize]))
    }
}

/// Ancestrais de um processo, do pai para cima, incluindo ele mesmo.
///
/// Para em 12 níveis: uma árvore de processos real nunca chega perto disso, e
/// um PID reciclado poderia formar ciclo.
#[must_use]
pub fn linhagem(pid: u32) -> Vec<u32> {
    let Ok(todos) = lista() else {
        return vec![pid];
    };
    let mut fora = vec![pid];
    let mut atual = pid;
    for _ in 0..12 {
        let Some(p) = todos.iter().find(|p| p.pid == atual) else {
            break;
        };
        // PID 0 e 4 são do sistema; subir até eles junta processos que não têm
        // nada a ver um com o outro.
        if p.pai == 0 || p.pai == 4 || fora.contains(&p.pai) {
            break;
        }
        fora.push(p.pai);
        atual = p.pai;
    }
    fora
}
