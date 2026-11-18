//! Presença no Discord (Rich Presence).
//!
//! # Por que isto, e não um mod do Discord
//!
//! Mostra no perfil de quem transmite que há algo no ar, com um botão que abre
//! a sala. É a API oficial — a mesma que os jogos usam —, então não depende de
//! modificar o cliente, não viola os Termos e não quebra a cada atualização.
//!
//! As alternativas foram descartadas por limitação técnica, não por gosto:
//! Atividades do Discord rodam num iframe que NÃO permite WebRTC, e um mod de
//! cliente exigiria cada amigo instalar o mod — o mesmo atrito de instalar o
//! app, com risco para a conta deles por cima.
//!
//! # Protocolo
//!
//! Named pipe local, quadros de `[opcode u32 LE][tamanho u32 LE][json]`.
//! Handshake com o Application ID, depois `SET_ACTIVITY` a cada mudança.
//! Falha em silêncio: sem Discord aberto não há nada a fazer, e transmitir
//! nunca pode depender disso.

use std::io::{Read, Write};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::registro;

/// O que mostrar no perfil.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Estado {
    /// Fora da sala: nada a anunciar.
    Nada,
    /// Na sala, sem transmitir.
    NaSala { pessoas: usize },
    /// Transmitindo, com o instante em que começou (epoch em segundos).
    NoAr { desde: u64 },
}

static DESEJADO: Mutex<Option<Estado>> = Mutex::new(None);

/// Define o que deve aparecer. Barato: só anota, quem aplica é a thread.
pub fn define(novo: Estado) {
    if let Ok(mut d) = DESEJADO.lock() {
        *d = Some(novo);
    }
}

/// Instante atual em segundos desde a época, para o cronômetro do Discord.
#[must_use]
pub fn agora() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Monta o JSON da atividade.
///
/// Separado do envio para poder ser testado: o que entra aqui vai para o
/// perfil PÚBLICO da pessoa, em todos os servidores dela.
#[must_use]
fn json_atividade(estado: &Estado, url_sala: &str) -> String {
    match estado {
        // `activity: null` limpa a presença.
        Estado::Nada => "null".to_string(),
        Estado::NaSala { pessoas } => format!(
            concat!(
                r#"{{"details":"Na sala do tela","state":"{} na sala","#,
                r#""buttons":[{{"label":"Entrar na sala","url":"{}"}}]}}"#
            ),
            pessoas, url_sala
        ),
        // O título da janela NÃO entra aqui, de propósito. A presença é pública
        // em todos os servidores da pessoa, e "Transmitindo: contrato.pdf"
        // seria um vazamento que ninguém pediu — quem transmite escolheu
        // mostrar a tela para a SALA, não para o Discord inteiro.
        Estado::NoAr { desde } => format!(
            concat!(
                r#"{{"details":"Transmitindo a tela","state":"ao vivo","#,
                r#""timestamps":{{"start":{}}},"#,
                r#""buttons":[{{"label":"Assistir","url":"{}"}}]}}"#
            ),
            desde, url_sala
        ),
    }
}

/// Começa a manter a presença em segundo plano.
///
/// `app_id` vazio desliga tudo — é o padrão até alguém criar o aplicativo no
/// portal do Discord.
pub fn inicia(app_id: String, url_sala: String) {
    if app_id.trim().is_empty() {
        return;
    }
    std::thread::spawn(move || {
        let mut conexao: Option<Conexao> = None;
        let mut aplicado: Option<Estado> = None;

        loop {
            std::thread::sleep(Duration::from_secs(2));

            let desejado = match DESEJADO.lock() {
                Ok(d) => d.clone(),
                Err(_) => continue,
            };
            let Some(desejado) = desejado else { continue };

            // Reconecta quando o Discord abre depois de nós, ou reinicia.
            if conexao.is_none() {
                conexao = Conexao::abre(&app_id);
                aplicado = None;
            }
            let Some(c) = conexao.as_mut() else { continue };

            // Só reenvia quando muda: sem isto seria uma mensagem a cada dois
            // segundos, para sempre, sem nenhum motivo.
            if aplicado.as_ref() == Some(&desejado) {
                continue;
            }
            if c.manda(&desejado, &url_sala).is_ok() {
                aplicado = Some(desejado);
            } else {
                // Discord fechou: solta e tenta de novo na próxima volta.
                conexao = None;
                aplicado = None;
            }
        }
    });
}

struct Conexao {
    pipe: std::fs::File,
}

impl Conexao {
    /// O Discord numera os pipes de 0 a 9; instâncias adicionais usam os
    /// seguintes. Tentar todos é o que o protocolo manda fazer.
    fn abre(app_id: &str) -> Option<Self> {
        for i in 0..10 {
            let caminho = format!(r"\\.\pipe\discord-ipc-{i}");
            let Ok(pipe) = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&caminho)
            else {
                continue;
            };
            let mut c = Self { pipe };
            let ola = format!(r#"{{"v":1,"client_id":"{app_id}"}}"#);
            if c.quadro(0, &ola).is_ok() && c.le_resposta().is_ok() {
                registro::linha("presenca no Discord conectada");
                return Some(c);
            }
        }
        None
    }

    /// Escreve um quadro do protocolo.
    fn quadro(&mut self, opcode: u32, json: &str) -> std::io::Result<()> {
        let corpo = json.as_bytes();
        let mut buf = Vec::with_capacity(8 + corpo.len());
        buf.extend_from_slice(&opcode.to_le_bytes());
        buf.extend_from_slice(&u32::try_from(corpo.len()).unwrap_or(0).to_le_bytes());
        buf.extend_from_slice(corpo);
        self.pipe.write_all(&buf)?;
        self.pipe.flush()
    }

    /// Lê e descarta a resposta. O conteúdo não interessa; o que interessa é
    /// que o outro lado ainda esteja vivo.
    fn le_resposta(&mut self) -> std::io::Result<()> {
        let mut cab = [0u8; 8];
        self.pipe.read_exact(&mut cab)?;
        let tam = u32::from_le_bytes([cab[4], cab[5], cab[6], cab[7]]) as usize;
        // Teto de sanidade: resposta gigante significa protocolo fora de
        // sincronia, e alocar o que ela pedir seria pior que desistir.
        if tam > 64 * 1024 {
            return Err(std::io::Error::other("resposta absurda do Discord"));
        }
        let mut corpo = vec![0u8; tam];
        self.pipe.read_exact(&mut corpo)
    }

    fn manda(&mut self, estado: &Estado, url_sala: &str) -> std::io::Result<()> {
        let json = format!(
            r#"{{"cmd":"SET_ACTIVITY","args":{{"pid":{},"activity":{}}},"nonce":"{}"}}"#,
            std::process::id(),
            json_atividade(estado, url_sala),
            agora()
        );
        self.quadro(1, &json)?;
        self.le_resposta()
    }
}

#[cfg(test)]
mod testes {
    use super::*;

    const SALA: &str = "https://tela.example.com";

    /// O que está sendo transmitido NUNCA pode entrar na presença: ela é
    /// pública em todos os servidores da pessoa, e um nome de arquivo ali é
    /// vazamento — ela escolheu mostrar a tela para a SALA, não para o Discord.
    #[test]
    fn nao_vaza_o_que_esta_sendo_transmitido() {
        let j = json_atividade(&Estado::NoAr { desde: 1000 }, SALA);
        assert!(j.contains("Transmitindo a tela"), "{j}");
        assert!(j.contains("\"start\":1000"), "cronometro: {j}");
        assert!(j.contains("Assistir"), "botao: {j}");
        // Nada de titulo de janela, caminho de arquivo ou nome de monitor.
        for proibido in [".exe", ".pdf", "Monitor", "Chrome"] {
            assert!(!j.contains(proibido), "vazou {proibido:?}: {j}");
        }
    }

    /// Sair da sala precisa LIMPAR a presença, não deixá-la parada dizendo que
    /// alguém está transmitindo quando não está.
    #[test]
    fn sair_limpa_a_presenca() {
        assert_eq!(json_atividade(&Estado::Nada, SALA), "null");
    }

    /// Na sala sem transmitir, o botão continua — é o que faz alguém entrar.
    #[test]
    fn na_sala_convida() {
        let j = json_atividade(&Estado::NaSala { pessoas: 3 }, SALA);
        assert!(j.contains("3 na sala"), "{j}");
        assert!(j.contains("Entrar na sala"), "{j}");
        assert!(j.contains(SALA), "{j}");
    }

    /// Sem Application ID, nada é iniciado — o padrão de quem não configurou.
    #[test]
    fn sem_app_id_nao_faz_nada() {
        inicia(String::new(), SALA.to_string());
        inicia("   ".to_string(), SALA.to_string());
    }

    /// Os estados precisam ser comparáveis: a thread só reenvia quando muda, e
    /// sem isso ela falaria com o Discord a cada dois segundos para sempre.
    #[test]
    fn estados_iguais_sao_reconhecidos() {
        assert_eq!(Estado::NoAr { desde: 10 }, Estado::NoAr { desde: 10 });
        assert_ne!(Estado::NoAr { desde: 10 }, Estado::NoAr { desde: 11 });
        assert_ne!(Estado::Nada, Estado::NaSala { pessoas: 1 });
    }
}
