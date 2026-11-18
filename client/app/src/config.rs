//! Preferências do app: `tela-config.json` em `%LOCALAPPDATA%\tela`.
//!
//! # Por que não ao lado do executável
//!
//! O desenho original guardava tudo junto do `.exe`, para a configuração viajar
//! com ele em pendrive. Na prática o efeito visível era outro: quem baixava o
//! app via um JSON brotar na pasta de Downloads na primeira execução, o que
//! contradiz a promessa de "baixa um arquivo e usa".
//!
//! O custo da troca é real — levar o `.exe` para outro PC significa reescolher
//! apelido e qualidade. É aceitável porque a senha lembrada já não viajava de
//! qualquer forma: o DPAPI a prende à conta e à máquina (ver `segredo.rs`).
//!
//! # Compatibilidade
//!
//! Um `tela-config.json` que já exista ao lado do executável continua sendo
//! respeitado. Ignorar a configuração de quem já usava o app seria pior do que
//! deixar o arquivo onde está.
//!
//! Nada é gravado no registro do Windows.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Padrão de `sem_eco` para configurações antigas, que não têm o campo.
/// O padrão do [`Config::caminho_video`].
fn caminho_video_padrao() -> String {
    "auto".to_string()
}

fn verdadeiro() -> bool {
    true
}

/// Nome do arquivo, fixo. Não é configurável de propósito: seria uma
/// configuração para achar a configuração.
const ARQUIVO: &str = "tela-config.json";

/// Preferências que sobrevivem ao fechamento do app.
///
/// A senha está aqui apenas **cifrada** pelo DPAPI, e só quando a pessoa pede
/// para lembrar. Ver `segredo.rs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Apelido usado na sala.
    pub apelido: String,
    /// Preset escolhido da última vez: `trabalho`, `esporte`, `jogo` ou
    /// `manual`.
    pub preset: String,
    /// Altura alvo do vídeo. `0` = mandar no tamanho nativo da fonte.
    pub altura: u32,
    pub fps: u32,
    pub kbps: u32,
    /// Última fonte: `monitor` ou `janela`.
    pub fonte_tipo: String,
    /// Título da janela ou nome do monitor da última vez. Casamos por título e
    /// não por HWND porque o HWND morre junto com a janela — o título é o que
    /// a pessoa reconhece na volta.
    pub fonte_titulo: String,
    /// Excluir o áudio do aplicativo de voz ao compartilhar o monitor inteiro.
    ///
    /// Ligado por padrão: mandar a voz da call de volta é sempre indesejado, e
    /// quem quiser o contrário (mostrar uma conversa do Discord) desmarca.
    #[serde(default = "verdadeiro")]
    pub sem_eco: bool,
    /// Senha da sala, cifrada pelo DPAPI do Windows (ver `segredo.rs`).
    ///
    /// `None` = a pessoa não pediu para lembrar. Nunca guarda texto puro:
    /// mesmo em %LOCALAPPDATA% o arquivo vaza para backup em nuvem, perfil
    /// móvel e qualquer programa que leia o disco.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub senha_protegida: Option<String>,
    /// Caminho do `ffmpeg.exe`.
    ///
    /// Só é consultado quando o app **não** foi compilado com a feature
    /// `ffmpeg-embutido` — na distribuição o ffmpeg vem dentro do `.exe` e
    /// tem precedência (ver `embutido.rs`). Em desenvolvimento aponta para o
    /// `./bin/ffmpeg.exe` ao lado do executável.
    pub ffmpeg: String,
    /// Base pública do serviço `sala`. Fica só no JSON, sem controle na UI:
    /// serve para apontar o app a uma instância local em teste sem virar mais
    /// um campo para o usuário comum errar.
    pub servidor: String,
    /// Application ID do Discord, para a presença no perfil.
    ///
    /// Vazio = desligado, que é o padrão. Criar o aplicativo é um ato na conta
    /// de alguém, então isto fica em branco até uma pessoa decidir preencher.
    /// Sem ele nada é enviado e nenhuma thread é criada.
    #[serde(default)]
    pub discord_app_id: String,
    /// Força um encoder específico, ignorando a sonda.
    ///
    /// Vazio = automático, que é o certo para quase todo mundo. Existe para
    /// investigar sem precisar de build nova: quando um encoder produz um
    /// fluxo que só falha na máquina de outra pessoa, poder fixá-lo por
    /// arquivo é a diferença entre testar em minutos e testar em horas.
    #[serde(default)]
    pub encoder: String,
    /// Argumentos extras para o encoder, acrescentados aos padrões.
    ///
    /// Existe para reproduzir defeito de encoder sem hardware específico: com
    /// `["-x264-params", "repeat-headers=0"]` o `libx264` passa a se comportar
    /// como o `h264_amf`, que não repete os parâmetros da imagem. Foi assim
    /// que a reinserção de SPS/PPS pôde ser testada de ponta a ponta numa
    /// máquina NVIDIA.
    #[serde(default)]
    pub encoder_args: Vec<String>,
    /// Por onde o vídeo é codificado: `auto`, `gpu` ou `cpu`.
    ///
    /// `gpu` entrega a textura da captura direto ao encoder, sem trazer o
    /// quadro para o processador — é a diferença entre ~168% de um núcleo e
    /// uns poucos por cento. `cpu` é o caminho antigo, pelo `ffmpeg.exe`.
    ///
    /// `auto` (o padrão) tenta a GPU e cai para a CPU sozinho se o encoder
    /// desta máquina não aceitar a textura. Fixar existe pelo mesmo motivo do
    /// `encoder`: se o caminho novo sair torto na máquina de alguém, dá para
    /// voltar pelo arquivo, sem build nova e sem esperar por mim.
    #[serde(default = "caminho_video_padrao")]
    pub caminho_video: String,
    /// Atalho global para transmitir o monitor onde o ponteiro está.
    ///
    /// No JSON e não na UI pelo mesmo motivo do `servidor`: quem precisar de
    /// outra combinação (porque a padrão bate com algo do jogo) troca aqui,
    /// sem mais um campo na tela para todo mundo errar. Sintaxe do Tauri:
    /// `"Ctrl+Alt+M"`, `"Shift+F9"`. Vazio desliga o atalho.
    #[serde(default = "atalho_monitor_padrao")]
    pub atalho_monitor: String,
    /// Atalho global para transmitir a janela em primeiro plano.
    #[serde(default = "atalho_janela_padrao")]
    pub atalho_janela: String,
}

/// Ctrl + `/` do teclado numérico: transmite o monitor sob o ponteiro.
///
/// O nome é o código da tecla física (padrão de `KeyboardEvent.code`), não o
/// símbolo — `NumpadDivide` é o `/` do numérico, distinto do `/` da fileira
/// principal. É o que o plugin entende.
pub const ATALHO_MONITOR: &str = "Ctrl+NumpadDivide";

/// Ctrl + `*` do teclado numérico: transmite a janela em primeiro plano.
pub const ATALHO_JANELA: &str = "Ctrl+NumpadMultiply";

fn atalho_monitor_padrao() -> String {
    ATALHO_MONITOR.to_string()
}

fn atalho_janela_padrao() -> String {
    ATALHO_JANELA.to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            apelido: String::new(),
            preset: "trabalho".to_string(),
            altura: 1080,
            fps: 30,
            kbps: 3000,
            fonte_tipo: String::new(),
            fonte_titulo: String::new(),
            sem_eco: true,
            senha_protegida: None,
            ffmpeg: "./bin/ffmpeg.exe".to_string(),
            // Fixado na compilação: TELA_SERVIDOR=https://tela.seudominio cargo build
            servidor: option_env!("TELA_SERVIDOR").unwrap_or("http://localhost:8090").to_string(),
            discord_app_id: String::new(),
            encoder: String::new(),
            encoder_args: Vec::new(),
            caminho_video: caminho_video_padrao(),
            atalho_monitor: atalho_monitor_padrao(),
            atalho_janela: atalho_janela_padrao(),
        }
    }
}

/// Onde a configuração acabou morando, e por quê.
#[derive(Debug, Clone)]
pub struct Local {
    pub caminho: PathBuf,
    /// `true` quando a configuração foi parar num lugar incomum — hoje, ao
    /// lado do executável por não haver `%LOCALAPPDATA%`. A interface avisa,
    /// porque nesse caso o arquivo aparece na pasta de quem baixou o app.
    ///
    /// O caso normal (`%LOCALAPPDATA%/tela`) é `false`: nada a avisar.
    pub fallback: bool,
}

/// Diretório do executável. Base de tudo que é portátil.
fn dir_do_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("descobrir o caminho do executável")?;
    Ok(exe
        .parent()
        .context("executável sem diretório pai")?
        .to_path_buf())
}

/// Testa se dá para gravar em `dir` de verdade — criando e apagando um
/// arquivo.
///
/// Consultar permissão pela API do Windows dá respostas erradas com frequência
/// (herança de ACL, redirecionamento de pasta, mídia somente leitura). Tentar
/// escrever é a única resposta que não mente.
fn grava_em(dir: &Path) -> bool {
    let teste = dir.join(".tela-escrita-teste");
    match std::fs::write(&teste, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&teste);
            true
        }
        Err(_) => false,
    }
}

/// Decide onde a configuração vive nesta execução.
pub fn local() -> Result<Local> {
    let dir = dir_do_exe()?;

    // Se o arquivo JÁ existe ao lado do executável, ele manda. Versões
    // anteriores gravavam ali por padrão, e ignorar a configuração de quem já
    // usava o app seria pior do que a pasta ficar com um arquivo a mais.
    let ao_lado = dir.join(ARQUIVO);
    if ao_lado.exists() {
        return Ok(Local {
            caminho: ao_lado,
            fallback: false,
        });
    }

    // Padrão: `%LOCALAPPDATA%\tela`.
    //
    // A promessa do produto é "baixa um arquivo e usa". Um JSON aparecendo na
    // pasta de Downloads ao lado do `.exe` contradiz isso na primeira execução.
    //
    // O custo é real e vale registrar: as configurações deixam de viajar junto
    // com o executável. Levar o `.exe` para outro PC significa reescolher
    // apelido e qualidade — o que é aceitável, porque a senha lembrada já não
    // viajava de qualquer forma (o DPAPI a prende à conta e à máquina).
    if let Some(base) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
        let pasta = base.join("tela");
        if std::fs::create_dir_all(&pasta).is_ok() {
            return Ok(Local {
                caminho: pasta.join(ARQUIVO),
                fallback: false,
            });
        }
    }

    // Sem LOCALAPPDATA (raro), tenta ao lado do executável antes de desistir.
    if grava_em(&dir) {
        return Ok(Local {
            caminho: ao_lado,
            fallback: true,
        });
    }
    anyhow::bail!("nem LOCALAPPDATA nem a pasta do executável aceitam escrita")
}

/// Lê a configuração. Arquivo ausente ou corrompido devolve o padrão — nunca
/// impede o app de abrir, porque a alternativa é a pessoa não conseguir
/// transmitir por causa de um JSON quebrado.
pub fn carrega(local: &Local) -> Config {
    let mut cfg = match std::fs::read_to_string(&local.caminho) {
        Ok(texto) => serde_json::from_str(&texto).unwrap_or_default(),
        Err(_) => Config::default(),
    };
    migra_atalhos(&mut cfg);
    cfg
}

/// Atalhos anteriores ao teclado numérico.
const ATALHO_MONITOR_ANTIGO: &str = "Ctrl+Alt+M";
const ATALHO_JANELA_ANTIGO: &str = "Ctrl+Alt+J";

/// Troca os atalhos antigos pelos novos, uma vez.
///
/// Sem isto, mudar o padrão no código não alcançaria ninguém: o arquivo é
/// reescrito inteiro a cada `salva`, então quem já usou a versão anterior tem
/// `Ctrl+Alt+M` gravado e continuaria com ele para sempre. Só troca o valor
/// que era exatamente o padrão antigo — quem escolheu o próprio atalho fica
/// com o dele.
fn migra_atalhos(cfg: &mut Config) {
    if cfg.atalho_monitor == ATALHO_MONITOR_ANTIGO {
        cfg.atalho_monitor = ATALHO_MONITOR.to_string();
    }
    if cfg.atalho_janela == ATALHO_JANELA_ANTIGO {
        cfg.atalho_janela = ATALHO_JANELA.to_string();
    }
}

/// Grava a configuração.
pub fn salva(local: &Local, cfg: &Config) -> Result<()> {
    let texto = serde_json::to_string_pretty(cfg).context("serializar configuração")?;
    std::fs::write(&local.caminho, texto)
        .with_context(|| format!("gravar {}", local.caminho.display()))
}

/// Resolve o caminho do ffmpeg relativo ao **executável**, e não ao diretório
/// de trabalho.
///
/// Quem abre o app por duplo clique herda um diretório de trabalho qualquer —
/// muitas vezes `C:\Windows\System32`. Um `./bin/ffmpeg.exe` interpretado a
/// partir dali simplesmente não existe, e o erro que sai ("O sistema não pode
/// encontrar o arquivo") não diz nada sobre a causa.
pub fn caminho_ffmpeg(cfg: &Config) -> String {
    let bruto = Path::new(&cfg.ffmpeg);
    // Nome solto, sem separador ("ffmpeg"): é para o PATH resolver.
    if bruto.is_absolute() || bruto.parent().is_none_or(|p| p.as_os_str().is_empty()) {
        return cfg.ffmpeg.clone();
    }
    match dir_do_exe() {
        Ok(dir) => dir.join(bruto).to_string_lossy().into_owned(),
        Err(_) => cfg.ffmpeg.clone(),
    }
}

#[cfg(test)]
mod testes_atalhos {
    use super::*;

    /// O padrão antigo é trocado; um atalho escolhido pela pessoa, não.
    #[test]
    fn migra_so_o_que_era_o_padrao_antigo() {
        let mut c = Config {
            atalho_monitor: ATALHO_MONITOR_ANTIGO.to_string(),
            atalho_janela: "Shift+F9".to_string(),
            ..Config::default()
        };
        migra_atalhos(&mut c);
        assert_eq!(
            c.atalho_monitor, ATALHO_MONITOR,
            "o padrão antigo tinha de virar o novo"
        );
        assert_eq!(
            c.atalho_janela, "Shift+F9",
            "atalho escolhido à mão não pode ser mexido"
        );
    }

    /// Rodar duas vezes não pode fazer diferença — `carrega` roda em toda
    /// abertura, e uma migração que se aplica de novo sobre o próprio
    /// resultado é como se apagam escolhas de quem já migrou.
    #[test]
    fn migrar_duas_vezes_da_no_mesmo() {
        let mut c = Config {
            atalho_monitor: ATALHO_MONITOR_ANTIGO.to_string(),
            ..Config::default()
        };
        migra_atalhos(&mut c);
        let depois = c.clone();
        migra_atalhos(&mut c);
        assert_eq!(c.atalho_monitor, depois.atalho_monitor);
        assert_eq!(c.atalho_janela, depois.atalho_janela);
    }

    /// Config vazia, sem os campos: os padrões novos entram pelo serde.
    #[test]
    fn config_antiga_sem_os_campos_ganha_os_novos() {
        let c: Config = serde_json::from_str(
            r#"{
            "apelido":"x","preset":"jogo","altura":1080,"fps":60,"kbps":8000,
            "fonte_tipo":"","fonte_titulo":"","ffmpeg":"./bin/ffmpeg.exe",
            "servidor":"https://exemplo"
        }"#,
        )
        .expect("desserializar config antiga");
        assert_eq!(c.atalho_monitor, ATALHO_MONITOR);
        assert_eq!(c.atalho_janela, ATALHO_JANELA);
    }
}
