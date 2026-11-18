//! App de transmissão do `tela'`.
//!
//! **O app não é a sala.** Assistir, o grid de telas e a lista de gente vivem
//! no navegador, em `tela.example.com`, e já estão no ar. Aqui só existe
//! o que exige código nativo: captura de janela pelo WGC e áudio isolado por
//! processo pelo WASAPI. Por isso a janela é pequena e fixa — é um painel de
//! transmissão, não um aplicativo.
//!
//! A divisão de responsabilidades é: o Rust é dono de tudo que tem estado
//! (conexão com a sala, captura, ffmpeg, WHIP, configuração) e o WebView é
//! dono só da apresentação. Nenhum WebSocket, nenhum caminho de arquivo e
//! nenhuma decisão de qualidade vivem no JavaScript.

// Sem console: um duplo clique no `.exe` não deve abrir uma janela preta
// atrás. Em depuração o console fica, porque é onde os erros aparecem.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod atualizacao;
mod bandeja;
mod bitrate;
mod config;
mod discord;
mod embutido;
mod escada;
mod registro;
mod remoto;
mod sala;
mod segredo;
mod sistema;
mod som;
mod transmissao;

use std::sync::{Arc, Mutex};

use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};

use tauri::{AppHandle, Emitter, Manager, State};
use tokio::runtime::Runtime;

/// Tudo que o app carrega entre chamadas. Um `Mutex` só, porque as transições
/// (entrar, começar a transmitir, parar) são exclusivas por natureza — não há
/// concorrência real a ganhar aqui, e sim invariantes a proteger.
struct Estado {
    rt: Arc<Runtime>,
    /// Token da sessão, compartilhado com a thread que envia o log.
    ///
    /// Cópia separada da que vive dentro da `Sala` porque o envio precisa
    /// dele sem tomar o `Mutex` da sala — que fica preso durante entrada e
    /// saída, justo quando há mais o que registrar.
    token_log: Arc<Mutex<Option<String>>>,
    local: config::Local,
    cfg: Mutex<config::Config>,
    sala: Mutex<Option<sala::Sala>>,
    transmissao: Mutex<Option<transmissao::Transmissao>>,
    /// Quantas transmissões já começaram nesta sessão.
    ///
    /// Serve de identidade para o aviso de encerramento. Sem ela, a morte de
    /// uma transmissão ANTIGA — que chega assíncrona, e pode chegar depois de
    /// a seguinte já estar no ar — derrubava a nova: a interface via
    /// `transmissao-fim` e concluía que a atual tinha caído. Era o "trocar
    /// fonte" falhando com a transmissão certa já no ar.
    geracao: AtomicU64,
    /// Apelido e senha da entrada atual, para a reconexão automática.
    ///
    /// Ficam no Rust e não na interface de propósito: a senha não precisa
    /// atravessar para o webview nem ficar guardada lá pela sessão inteira.
    credencial: Mutex<Option<(String, String)>>,
    /// O que estava sendo transmitido quando a sala caiu.
    ///
    /// Reentrar recria a sessão do zero — token novo, caminho novo no
    /// servidor —, então a transmissão anterior não sobrevive: ela tem de ser
    /// REFEITA. Sem isto guardado, reconectar devolveria a pessoa a uma sala
    /// onde ela não está mais transmitindo, que é metade do conserto.
    ultimo_pedido: Mutex<Option<PedidoTransmitir>>,
}

/// Erro que atravessa a fronteira para o JavaScript.
///
/// `anyhow` traz a cadeia inteira de contexto; `{:#}` a achata numa linha só,
/// que é o que cabe numa faixa de erro na interface.
/// Converte um erro para a mensagem que a interface mostra — e **registra no
/// log pelo caminho**.
///
/// Registrar aqui e não em cada chamada é deliberado: toda falha que chega ao
/// usuário passa por esta função. Quando um amigo disser "deu erro", o log tem
/// a mensagem exata, em vez de só as linhas de inicialização — que foi
/// exatamente o que aconteceu e nos deixou cegos pela terceira vez.
fn erro(e: &anyhow::Error) -> String {
    let msg = format!("{e:#}");
    registro::linha(&format!("ERRO: {msg}"));
    msg
}

/// Mesmo papel de [`erro`], para falhas que já nascem como texto.
fn erro_txt(msg: impl Into<String>) -> String {
    let msg = msg.into();
    registro::linha(&format!("ERRO: {msg}"));
    msg
}

// ---------------------------------------------------------------------------
// Comandos
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct Inicial {
    apelido: String,
    preset: String,
    altura: u32,
    fps: u32,
    kbps: u32,
    fonte_tipo: String,
    fonte_titulo: String,
    caminho_config: String,
    /// A configuração não coube ao lado do executável e foi para
    /// `%LOCALAPPDATA%`. A interface avisa — deixar de avisar seria a pessoa
    /// descobrir sozinha depois de copiar o `.exe` para outra máquina.
    config_fallback: bool,
    audio_por_processo: bool,
    /// Nome do aplicativo de voz em execução, se houver. A interface só oferece
    /// a opção anti-eco quando há o que excluir.
    app_de_voz: Option<String>,
    /// Preferência salva de excluir o áudio do app de voz.
    sem_eco: bool,
    build_windows: u32,
    servidor: String,
    sala_url: String,
    caminho_ffmpeg: String,
    /// Senha decifrada, para pré-preencher o formulário. Vazia quando não há
    /// senha salva — ou quando o blob veio de outra conta ou outra máquina,
    /// caso em que o DPAPI recusa e isto é o comportamento correto.
    senha_lembrada: String,
    /// Onde está o arquivo de log. A interface mostra para a pessoa poder
    /// mandar quando pedir ajuda.
    caminho_log: String,
    ffmpeg_existe: bool,
    /// Atalhos em vigor, para a interface mostrar os de verdade em vez de
    /// repetir os padrões escritos à mão — que ficariam errados no instante
    /// em que alguém trocasse no JSON.
    atalho_monitor: String,
    atalho_janela: String,
}

/// Pasta onde ficam o encoder baixado e os arquivos de apoio.
fn dir_cache() -> Option<std::path::PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .map(|b| b.join("tela"))
}

/// Procura um ffmpeg extraído por uma versão antiga do app.
fn ffmpeg_extraido_antes(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let entradas = std::fs::read_dir(dir).ok()?;
    entradas.flatten().map(|e| e.path()).find(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("ffmpeg-") && n.ends_with(".exe"))
    })
}

/// Resolve qual ffmpeg usar.
///
/// O embutido tem precedência: se o app foi compilado com
/// `--features ffmpeg-embutido`, ele é a fonte da verdade e o usuário não
/// precisa saber que existe um ffmpeg no meio. Sem a feature — ou se a extração
/// falhar — cai no caminho configurado, que por padrão é `bin/ffmpeg.exe` ao
/// lado do executável.
fn resolve_ffmpeg(cfg: &config::Config) -> String {
    // Encoder já baixado numa execução anterior. É o caminho normal depois da
    // primeira vez — 140 MB que não se baixa de novo a cada atualização do app.
    if let Some(dir) = dir_cache() {
        let baixado = atualizacao::caminho_ffmpeg_baixado(&dir);
        if baixado.exists() {
            return baixado.to_string_lossy().into_owned();
        }
        // Sobra da época em que o ffmpeg vinha dentro do `.exe`: quem já usou
        // uma versão antiga tem `ffmpeg-<tamanho>.exe` extraído aqui. É o mesmo
        // binário — aproveitar poupa 140 MB de download a cada amigo.
        if let Some(velho) = ffmpeg_extraido_antes(&dir) {
            return velho.to_string_lossy().into_owned();
        }
    }
    match embutido::garante() {
        Ok(Some(caminho)) => caminho.to_string_lossy().into_owned(),
        Ok(None) => config::caminho_ffmpeg(cfg),
        Err(e) => {
            // Não é fatal: ainda pode haver um ffmpeg ao lado do executável.
            eprintln!("aviso: falhei ao extrair o ffmpeg embutido: {e:#}");
            config::caminho_ffmpeg(cfg)
        }
    }
}

#[tauri::command]
fn inicial(estado: State<'_, Estado>) -> Result<Inicial, String> {
    let cfg = estado
        .cfg
        .lock()
        .map_err(|_| erro_txt("configuração envenenada"))?
        .clone();
    let caminho_ffmpeg = resolve_ffmpeg(&cfg);
    Ok(Inicial {
        apelido: cfg.apelido.clone(),
        preset: cfg.preset.clone(),
        altura: cfg.altura,
        fps: cfg.fps,
        kbps: cfg.kbps,
        fonte_tipo: cfg.fonte_tipo.clone(),
        fonte_titulo: cfg.fonte_titulo.clone(),
        caminho_config: estado.local.caminho.to_string_lossy().into_owned(),
        config_fallback: estado.local.fallback,
        audio_por_processo: sistema::tem_audio_por_processo(),
        app_de_voz: tela_capture::processos::acha_app_de_voz().map(|p| p.nome),
        sem_eco: cfg.sem_eco,
        build_windows: sistema::build_do_windows(),
        servidor: cfg.servidor.clone(),
        sala_url: cfg.servidor.clone(),
        ffmpeg_existe: std::path::Path::new(&caminho_ffmpeg).exists(),
        caminho_ffmpeg,
        atalho_monitor: cfg.atalho_monitor.clone(),
        atalho_janela: cfg.atalho_janela.clone(),
        // Um blob que não decifra (veio de outra conta ou outra máquina) vira
        // string vazia, não erro: para quem usa, é indistinguível de "não
        // salvei senha aqui", que é exatamente o que aconteceu.
        caminho_log: registro::caminho(
            estado
                .local
                .caminho
                .parent()
                .unwrap_or(std::path::Path::new(".")),
        )
        .to_string_lossy()
        .into_owned(),
        senha_lembrada: cfg
            .senha_protegida
            .as_deref()
            .and_then(|b| segredo::revela(b).ok())
            .unwrap_or_default(),
    })
}

#[derive(Serialize)]
struct Fontes {
    monitores: Vec<MonitorView>,
    janelas: Vec<JanelaView>,
}

#[derive(Serialize)]
struct MonitorView {
    indice: usize,
    nome: String,
    largura: u32,
    altura: u32,
}

#[derive(Serialize)]
struct JanelaView {
    hwnd: String,
    pid: u32,
    titulo: String,
    /// Nome do executável dono da janela. É o que a interface mostra em
    /// "Áudio de: ___" — dizer "PID 22328" não informa ninguém.
    processo: String,
}

#[tauri::command]
fn listar_fontes() -> Result<Fontes, String> {
    let monitores = tela_capture::video::list_monitors()
        .map_err(|e| {
            // Falha ao enumerar é rara e some da tela junto com a mensagem.
            // No arquivo ela fica.
            registro::linha(&format!("ERRO ao listar monitores: {e}"));
            erro(&e)
        })?
        .into_iter()
        .map(|m| MonitorView {
            indice: m.index,
            nome: if m.name.trim().is_empty() {
                format!("Monitor {}", m.index)
            } else {
                m.name
            },
            largura: m.width,
            altura: m.height,
        })
        .collect::<Vec<_>>();

    let janelas = tela_capture::video::list_windows()
        .map_err(|e| {
            registro::linha(&format!("ERRO ao listar janelas: {e}"));
            erro(&e)
        })?
        .into_iter()
        .map(|w| JanelaView {
            // HWND vai como string: é `isize` e o `number` do JavaScript
            // perderia precisão em máquinas com HWND alto.
            hwnd: w.hwnd.to_string(),
            processo: nome_do_processo(w.pid),
            pid: w.pid,
            titulo: w.title,
        })
        .collect::<Vec<_>>();

    // Quantas fontes a pessoa está vendo, e nada além disso.
    //
    // Sem esta linha, "não consegui compartilhar o monitor" era indecidível
    // pelo log: se a aba de monitor estivesse vazia, não havia o que clicar e
    // nenhum rastro aparecia — `transmitir:` só é escrito depois de escolher.
    // Ficamos sem saber se a lista estava vazia ou se ninguém clicou.
    //
    // Só a contagem. Título de janela é o que a pessoa tem aberto na hora, e
    // isso vai para um arquivo no servidor que outra pessoa vai ler.
    registro::linha(&format!(
        "fontes: {} monitor(es), {} janela(s)",
        monitores.len(),
        janelas.len()
    ));

    Ok(Fontes { monitores, janelas })
}

/// Prepara a captura, escolhendo o caminho do vídeo e recuando se preciso.
///
/// # Os três modos
///
/// `gpu` e `cpu` fixam o caminho e não recuam: quem fixou quer justamente
/// aquele, e mascarar a falha esconderia o que ele foi investigar.
///
/// `auto` — o padrão — tenta a GPU e cai para o `ffmpeg.exe` se qualquer coisa
/// der errado. É deliberadamente uma rede larga: falta das DLLs, encoder que
/// não abre, encoder que abre e não produz. Tentar distinguir os casos custaria
/// código para escolher a MESMA saída em todos eles.
///
/// O preço do recuo é uma captura que sobe e cai — alguns segundos, uma vez, e
/// só em máquina onde o caminho novo não serve. Vai para o log com o motivo,
/// que é o que permite descobrir depois por que a máquina de alguém está no
/// caminho antigo.
fn prepara_com_recuo(
    fonte: &transmissao::Fonte,
    qualidade: transmissao::Qualidade,
    caminho_video: &str,
    // `tem_ffmpeg`: o encoder externo está baixado. Quem vai pelo caminho em
    // processo não precisa dele, e é por isso que a exigência mora aqui e não
    // antes: cobrá-la de todo mundo obrigava a baixar 140 MB que talvez nunca
    // fossem usados.
    tem_ffmpeg: bool,
) -> anyhow::Result<transmissao::Preparada> {
    let audio_por_processo = sistema::tem_audio_por_processo();
    let modo = caminho_video.trim();
    let sem_encoder = || {
        anyhow::anyhow!(
            "o codificador de vídeo ainda não foi baixado, e esta máquina precisa              dele. Saia e entre de novo no app para baixá-lo."
        )
    };

    // Nada toca na libavcodec antes disto dar certo — ver `libav::carrega`. Na
    // primeira execução, antes do download, devolve `false` e a transmissão
    // segue pelo `ffmpeg.exe`, que é o comportamento de sempre.
    let pasta = dir_cache().map(|d| atualizacao::pasta_libav(&d));
    let pronta = modo != "cpu" && pasta.as_deref().is_some_and(tela_capture::libav::carrega);
    if !pronta {
        // Sempre no log, e não só quando alguém FIXOU o caminho. O recuo
        // silencioso custou caro: a transmissão sobe, tudo parece bem, e a
        // única pista de que se está no caminho antigo é a CPU alta — que
        // ninguém olha quando está funcionando.
        registro::linha(&format!(
            "vídeo: usando o ffmpeg.exe — {}",
            match (modo, &pasta) {
                ("cpu", _) => "caminho_video=cpu no config".to_string(),
                (_, None) => "não achei a pasta de cache".to_string(),
                (_, Some(p)) => format!(
                    "a libavcodec não carregou de {} — {}",
                    p.display(),
                    tela_capture::libav::porque_falhou()
                ),
            }
        ));
        if !tem_ffmpeg {
            return Err(sem_encoder());
        }
        return transmissao::prepara(fonte, qualidade, audio_por_processo, false);
    }

    match transmissao::prepara(fonte, qualidade, audio_por_processo, true) {
        Ok(p) => {
            registro::linha("vídeo: encoder em processo, textura direto da GPU");
            Ok(p)
        }
        // Sem o encoder externo não há para onde recuar. O erro do caminho novo
        // vai junto: sem ele a mensagem seria "baixe o encoder" para alguém
        // cujo problema é outro.
        Err(e) if modo != "gpu" && !tem_ffmpeg => {
            registro::linha(&format!(
                "vídeo: o caminho novo falhou e não há recuo — {e:#}"
            ));
            Err(sem_encoder().context(format!("{e:#}")))
        }
        Err(e) if modo != "gpu" => {
            registro::linha(&format!("vídeo: caindo para o ffmpeg.exe — {e:#}"));
            transmissao::prepara(fonte, qualidade, audio_por_processo, false)
        }
        Err(e) => Err(e),
    }
}

/// Tenta voltar para a sala e refazer a transmissão que estava no ar.
///
/// # Por que isto existe
///
/// A conexão com a sala cai por motivos banais: o serviço reiniciando, a rede
/// oscilando. O navegador de quem assiste reconecta sozinho desde sempre; o app
/// não, e a transmissão morria junto — a pessoa descobria olhando para a tela
/// de entrar. Aconteceu numa implantação da sala, com gente no ar.
///
/// Reentrar cria uma sessão NOVA, com outro token e outro caminho no servidor,
/// então a transmissão anterior não sobrevive de jeito nenhum: ela é refeita.
/// Quem assiste vê o card sumir e voltar, que é bem melhor que sumir e ficar.
fn reconecta(app: &AppHandle, motivo: &str) {
    let estado = app.state::<Estado>();
    let Some((apelido, senha)) = estado.credencial.lock().ok().and_then(|c| c.clone()) else {
        // Sem credencial não há o que tentar — é o caso de a queda ter
        // acontecido antes mesmo de entrar.
        let _ = app.emit("sala-caiu", motivo.to_string());
        return;
    };

    // Espera crescente até 15 s. O caso comum é o serviço reiniciando, que
    // volta em segundos; o teto existe para uma queda longa não virar uma
    // tentativa por hora.
    for tentativa in 0..TENTATIVAS_DE_RECONEXAO {
        let espera = std::time::Duration::from_secs((2 << tentativa.min(3)).min(15));
        std::thread::sleep(espera);
        registro::linha(&format!(
            "sala: tentando reconectar ({}/{TENTATIVAS_DE_RECONEXAO})",
            tentativa + 1
        ));

        // `Manter`: a credencial JÁ está guardada como a pessoa pediu, e
        // reconectar não é hora de mudar essa escolha. Antes isto passava
        // `false`, que dentro de `faz_entrar` significa APAGAR — ver `Lembrar`.
        match faz_entrar(
            app,
            &estado,
            apelido.clone(),
            senha.clone(),
            Lembrar::Manter,
        ) {
            Ok(_) => {}
            // Recusa que não melhora com o tempo: parar aqui é o conserto.
            //
            // Antes, toda falha valia uma nova tentativa. Quando a senha da
            // sala mudou, isso viraram seis tentativas com a credencial errada
            // — o bastante para estourar o limitador do servidor (cinco falhas)
            // e bloquear o IP por quinze minutos. A pessoa ficava trancada para
            // fora mesmo já tendo a senha nova em mãos, e a mensagem que ela via
            // era "tentativas demais", que não explica nada.
            Err(e) if !e.vale_tentar => {
                registro::linha(&format!("sala: a sala recusou ({}) — não insisto", e.texto));
                let _ = app.emit("sala-caiu", e.texto);
                return;
            }
            Err(_) => continue,
        }
        registro::linha("sala: reconectado");
        let _ = app.emit("sala-voltou", ());

        let pedido = estado.ultimo_pedido.lock().ok().and_then(|p| p.clone());
        if let Some(pedido) = pedido {
            match inicia_transmissao(app, &estado, pedido) {
                Ok(()) => registro::linha("sala: transmissão refeita"),
                Err(e) => {
                    registro::linha(&format!("sala: não consegui refazer a transmissão: {e}"));
                    let _ = app.emit(
                        "sala-caiu",
                        format!("voltei à sala, mas a transmissão não: {e}"),
                    );
                }
            }
        }
        return;
    }

    registro::linha("sala: desisti de reconectar");
    let _ = app.emit("sala-caiu", motivo.to_string());
}

/// Quantas vezes tentar antes de desistir e devolver a pessoa à tela de entrar.
///
/// Seis tentativas com espera crescente dão cerca de um minuto e meio — folga
/// para um reinício de serviço, e curto o bastante para não deixar alguém
/// olhando para "reconectando" quando a sala não vai voltar.
const TENTATIVAS_DE_RECONEXAO: u32 = 6;

/// Nome do executável de um PID, para o indicador de áudio.
///
/// Via `tasklist` e não `OpenProcess`+`QueryFullProcessImageName` porque a
/// segunda exige `PROCESS_QUERY_LIMITED_INFORMATION`, que um usuário comum não
/// tem sobre processos de outra sessão ou elevados — e a lista de janelas tem
/// desses. `tasklist` responde para todos e é o mesmo dado.
fn nome_do_processo(pid: u32) -> String {
    use std::os::windows::process::CommandExt;
    /// Sem isso, cada consulta pisca uma janela de console na frente do app.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let saida = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    let Ok(saida) = saida else {
        return format!("PID {pid}");
    };
    // Linha CSV: "nome.exe","1234","Console","1","12.345 K"
    let texto = String::from_utf8_lossy(&saida.stdout);
    texto
        .lines()
        .find_map(|l| l.trim().strip_prefix('"'))
        .and_then(|l| l.split('"').next())
        .filter(|n| !n.is_empty())
        .map_or_else(|| format!("PID {pid}"), str::to_string)
}

#[derive(Serialize)]
struct Entrou {
    apelido: String,
}

#[tauri::command]
fn entrar(
    app: AppHandle,
    estado: State<'_, Estado>,
    apelido: String,
    senha: String,
    lembrar: bool,
) -> Result<Entrou, String> {
    let lembrar = if lembrar { Lembrar::Sim } else { Lembrar::Nao };
    faz_entrar(&app, &estado, apelido, senha, lembrar).map_err(|e| e.texto)
}

/// Falha ao entrar, com a informação de que a reconexão precisa.
///
/// `texto` é o que a pessoa lê; `vale_tentar` é o que a reconexão obedece.
/// Separado porque são perguntas diferentes: "o que aconteceu" e "adianta
/// insistir". Tratá-las como a mesma foi o que fez o app queimar o limitador do
/// servidor depois de uma troca de senha.
struct ErroEntrar {
    texto: String,
    vale_tentar: bool,
}

impl From<String> for ErroEntrar {
    fn from(texto: String) -> Self {
        // Falha nossa, antes de falar com a sala: apelido em branco, mutex
        // envenenado, configuração ilegível. Nenhuma delas melhora tentando.
        Self {
            texto,
            vale_tentar: false,
        }
    }
}

/// O que fazer com a senha guardada ao entrar.
///
/// # Por que três estados e não um `bool`
///
/// Porque o `bool` só sabia "guardar" ou "apagar", e a reconexão precisava de
/// um terceiro: NÃO MEXER. Ela passava `false` achando que isso significava
/// "deixa como está" — o comentário ali dizia exatamente isso —, mas dentro de
/// [`faz_entrar`] `false` quer dizer "a pessoa desmarcou a caixa, apague".
///
/// Resultado: toda reconexão automática bem-sucedida APAGAVA a senha salva. E
/// reconexão acontece a cada reinício da sala, ou seja, a cada implantação.
/// A pessoa fechava o app, abria de novo, e a senha tinha sumido — sem nada
/// que explicasse por quê.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lembrar {
    /// A caixa estava marcada: guarda a senha.
    Sim,
    /// A caixa estava desmarcada: apaga o que houver.
    Nao,
    /// Não foi a pessoa que decidiu nada agora — é a reconexão. Não toca.
    Manter,
}

/// Se a senha que a sala recusou é a que está guardada, esquece a guardada.
///
/// # Por que existe
///
/// A página esquece a senha recusada desde 26/08; o app não esquecia, e a
/// reoferecia pré-preenchida para sempre. Quando a senha da sala foi trocada,
/// um amigo abriu o app, confirmou a senha que já vinha no campo — a antiga —
/// e foi recusado. Numa versão antiga do app isso virou bloqueio de quinze
/// minutos. Ele não errou nada: o app ofereceu uma senha caducada.
///
/// Só esquece se for A MESMA. Se a pessoa digitou outra coisa e errou, a
/// guardada pode perfeitamente estar certa, e apagá-la seria punir um erro de
/// digitação com a perda da senha boa.
fn esquece_se_for_a_guardada(estado: &Estado, recusada: &str) {
    let guardada = estado
        .cfg
        .lock()
        .ok()
        .and_then(|c| c.senha_protegida.clone());
    let Some(b64) = guardada else { return };
    let Ok(clara) = segredo::revela(&b64) else {
        return;
    };
    if clara == recusada {
        salva_config(estado, |c| c.senha_protegida = None);
        registro::linha("sala: a senha guardada foi recusada — esquecida");
    }
}

/// O corpo de [`entrar`], separado para a reconexão poder reusá-lo.
fn faz_entrar(
    app: &AppHandle,
    estado: &Estado,
    apelido: String,
    senha: String,
    lembrar: Lembrar,
) -> Result<Entrou, ErroEntrar> {
    let apelido = apelido.trim().to_string();
    if apelido.is_empty() {
        return Err(erro_txt("escolha um apelido").into());
    }

    let servidor = {
        let cfg = estado
            .cfg
            .lock()
            .map_err(|_| erro_txt("configuração envenenada"))?;
        cfg.servidor.clone()
    };

    if let Ok(mut c) = estado.credencial.lock() {
        *c = Some((apelido.clone(), senha.clone()));
    }

    let app_evento = app.clone();
    let app_queda = app.clone();
    let nova = sala::Sala::entra(
        &estado.rt,
        &servidor,
        &senha,
        &apelido,
        move |ev| match ev {
            sala::Evento::Estado(e) => {
                // A presenca acompanha a sala: enquanto ninguem transmite, ela
                // mostra quantos estao la e convida quem vir o perfil.
                if e.transmitindo == 0 {
                    discord::define(discord::Estado::NaSala { pessoas: e.pessoas });
                }
                let _ = app_evento.emit("sala-estado", e);
            }
            // Recusa não se reconecta. Ver `sala::Evento::Recusado`: tratar
            // isto como queda fazia o app se rebloquear sozinho para sempre.
            sala::Evento::Recusado(motivo) => {
                registro::linha(&format!(
                    "sala: recusou a credencial ({motivo}) — não reconecto"
                ));
                let _ = app_evento.emit("sala-caiu", motivo);
            }
            sala::Evento::Caiu(motivo) => {
                // A interface fica sabendo na hora — quem está olhando merece
                // ver "reconectando" em vez de nada acontecendo.
                let _ = app_evento.emit("sala-reconectando", motivo.clone());
                let app = app_queda.clone();
                std::thread::spawn(move || reconecta(&app, &motivo));
            }
        },
    )
    .map_err(|e| {
        if sala::e_senha_recusada(&e) {
            esquece_se_for_a_guardada(estado, &senha);
        }
        ErroEntrar {
            // A decisão de insistir vem do CÓDIGO que a sala mandou, não do texto.
            vale_tentar: sala::vale_tentar_de_novo(&e),
            texto: erro(&e),
        }
    })?;

    if let Ok(mut t) = estado.token_log.lock() {
        *t = Some(nova.token().to_string());
    }
    discord::define(discord::Estado::NaSala { pessoas: 1 });
    *estado
        .sala
        .lock()
        .map_err(|_| erro_txt("sala envenenada"))? = Some(nova);

    // Só grava depois de a sala aceitar: guardar credencial recusada faria a
    // pessoa reencontrá-la pré-preenchida e errada na próxima abertura.
    // Reconexão: só confirma o apelido e não toca na senha guardada.
    if lembrar == Lembrar::Manter {
        salva_config(estado, |c| c.apelido = apelido.clone());
        return Ok(Entrou { apelido });
    }

    let protegida = if lembrar == Lembrar::Sim {
        match segredo::protege(&senha) {
            Ok(b) => Some(b),
            Err(e) => {
                // Não impede de entrar: a pessoa já está na sala. Só não vai
                // encontrar a senha lembrada na próxima vez.
                eprintln!("aviso: não consegui proteger a senha: {e:#}");
                None
            }
        }
    } else {
        None
    };
    salva_config(estado, |c| {
        c.apelido = apelido.clone();
        // Desmarcar a caixa APAGA a senha guardada. Sem isso, desmarcar não
        // teria efeito nenhum e a senha ficaria no arquivo para sempre.
        c.senha_protegida = protegida.clone();
    });

    Ok(Entrou { apelido })
}

#[tauri::command]
fn sair(estado: State<'_, Estado>) -> Result<(), String> {
    // Derruba a transmissão antes de largar a sala: a ordem inversa deixaria o
    // ffmpeg publicando num path que o servidor já considera órfão.
    let viva = estado
        .transmissao
        .lock()
        .map_err(|_| erro_txt("transmissão envenenada"))?
        .take();
    if let Some(t) = viva {
        t.encerra();
    }
    // O token morre junto com a sessão: mandar log com ele depois disso só
    // renderia 401 e devolveria as linhas à fila para sempre.
    if let Ok(mut t) = estado.token_log.lock() {
        *t = None;
    }
    discord::define(discord::Estado::Nada);
    // Largar a `Sala` fecha o WebSocket no `Drop` dela.
    *estado
        .sala
        .lock()
        .map_err(|_| erro_txt("sala envenenada"))? = None;
    Ok(())
}

/// O que a interface manda ao pedir uma transmissão.
///
/// `camelCase` porque é assim que o JavaScript escreve, e o Tauri só converte
/// os argumentos do comando — dentro de uma struct aninhada quem manda é o
/// serde. Sem isto, `semEco` não casa com `sem_eco` e o campo fica no valor
/// padrão: o anti-eco ficou meses ligado na tela e desligado de fato.
#[derive(serde::Deserialize, Clone)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PedidoTransmitir {
    tipo: String,
    /// Excluir o áudio do aplicativo de voz ao compartilhar o monitor.
    /// Ignorado para janela, onde o áudio já é só o do app escolhido.
    ///
    /// Sem `#[serde(default)]` de propósito: com ele, um nome errado vindo da
    /// interface vira `false` em silêncio, que foi exatamente como este bug
    /// passou despercebido. Faltando, agora, dá erro na cara.
    sem_eco: bool,
    /// HWND como string — ver [`JanelaView::hwnd`].
    hwnd: String,
    pid: u32,
    indice: usize,
    titulo: String,
    /// Nome do preset: `auto`, `leve`, `qualidade` ou `nativa`.
    ///
    /// Só o nome. Resolução, fps e taxa saíram do pedido porque quem os decide
    /// é a escada, e ela os muda sozinha durante a transmissão — mandá-los
    /// daqui seria congelar no pedido uma coisa que não fica parada.
    preset: String,
}

#[tauri::command]
fn transmitir(
    app: AppHandle,
    estado: State<'_, Estado>,
    pedido: PedidoTransmitir,
) -> Result<(), String> {
    inicia_transmissao(&app, &estado, pedido)
}

/// O corpo de [`transmitir`].
///
/// Separado para o seguidor de janela poder recomeçar sozinho, sem passar pela
/// interface — quem está no meio de uma partida não vai clicar em nada.
fn inicia_transmissao(
    app: &AppHandle,
    estado: &Estado,
    pedido: PedidoTransmitir,
) -> Result<(), String> {
    // Guardado ANTES de tentar: se a sala cair no meio do preparo, a
    // reconexão ainda sabe o que refazer.
    if let Ok(mut p) = estado.ultimo_pedido.lock() {
        *p = Some(pedido.clone());
    }
    // Trocar de fonte substitui a transmissão em vez de ser recusado.
    //
    // Antes o botão "trocar" precisava parar primeiro, e a tela de quem assiste
    // ficava sem nada enquanto a pessoa escolhia a fonte nova. Encerrando aqui,
    // no último instante, o corte fica restrito à troca em si.
    // A geração sobe ANTES de encerrar a anterior, e a ordem é o ponto.
    //
    // O aviso de morte da anterior é disparado de dentro do `encerra()`. Se a
    // geração só subisse depois, esse aviso ainda se veria como o atual e a
    // interface concluiria que a transmissão vigente caiu — que é exatamente o
    // que fazia o botão "trocar fonte" falhar com a nova já no ar.
    let minha_geracao = estado.geracao.fetch_add(1, Ordering::SeqCst) + 1;

    let anterior = estado
        .transmissao
        .lock()
        .map_err(|_| erro_txt("transmissão envenenada"))?
        .take();
    if let Some(v) = anterior {
        registro::linha("trocando de fonte: encerrando a transmissão anterior");
        v.encerra();
    }

    let fonte = match pedido.tipo.as_str() {
        "janela" => transmissao::Fonte::Janela {
            hwnd: pedido
                .hwnd
                .parse::<isize>()
                .map_err(|_| erro_txt("identificador de janela inválido"))?,
            pid: pedido.pid,
            titulo: pedido.titulo.clone(),
        },
        "monitor" => {
            // Descobre o app de voz na hora de transmitir, e não quando a
            // interface montou a lista: alguém pode ter aberto o Discord no
            // meio do caminho.
            let excluir_pid = if pedido.sem_eco {
                let achado = tela_capture::processos::acha_app_de_voz();
                match &achado {
                    Some(p) => registro::linha(&format!(
                        "sem eco: excluindo {} (pid {}) do áudio",
                        p.nome, p.pid
                    )),
                    None => {
                        registro::linha("sem eco pedido, mas nenhum aplicativo de voz em execução")
                    }
                }
                achado.map(|p| p.pid)
            } else {
                None
            };
            salva_config(estado, |c| c.sem_eco = pedido.sem_eco);
            transmissao::Fonte::Monitor {
                indice: pedido.indice,
                nome: pedido.titulo.clone(),
                excluir_pid,
            }
        }
        outro => return Err(format!("tipo de fonte desconhecido: {outro}")),
    };

    // A captura é configurada no MÁXIMO que qualquer degrau da escada pode
    // pedir, não no que o preset escolhido usa agora: se a escada subir para
    // 60 fps depois, uma captura presa em 30 estrangularia o encoder e não
    // haveria como reconfigurá-la sem derrubar a transmissão — a captura é
    // justamente a peça que o reinício do encoder preserva.
    //
    // A altura vai zerada (nativo) porque quem decide a escala é a política, e
    // ela nasce depois daqui.
    let qualidade = transmissao::Qualidade {
        altura: 0,
        fps: escada::ESCADA.iter().map(|d| d.fps).max().unwrap_or(60),
        kbps: 0, // decidido pela política; ver acima
    };

    let (caminho_ffmpeg, caminho_video) = {
        let cfg = estado
            .cfg
            .lock()
            .map_err(|_| erro_txt("configuração envenenada"))?;
        (resolve_ffmpeg(&cfg), cfg.caminho_video.clone())
    };
    // A exigência do `ffmpeg.exe` NÃO é feita aqui, e isso é deliberado: quem
    // transmite pelo encoder em processo não o usa para nada. Cobrá-la antes de
    // escolher o caminho obrigava todo mundo a baixar 140 MB que talvez nunca
    // fossem tocados. Quem precisar dele recebe o pedido em
    // `prepara_com_recuo`, que é onde já se sabe se ele vai ser usado.
    let tem_ffmpeg = std::path::Path::new(&caminho_ffmpeg).exists();

    // Preparar primeiro: a resolução só é conhecida depois do primeiro frame, e
    // o `publish_start` da sala exige w/h reais.
    registro::linha(&format!(
        "transmitir: fonte={} \"{}\" qualidade={}p {}fps {}kbps",
        fonte.tipo_protocolo(),
        fonte.titulo(),
        qualidade.altura,
        qualidade.fps,
        qualidade.kbps
    ));

    let preparada =
        prepara_com_recuo(&fonte, qualidade, &caminho_video, tem_ffmpeg).map_err(|e| erro(&e))?;
    registro::linha(&format!(
        "captura pronta: {}x{} -> {}x{}",
        preparada.largura, preparada.altura, preparada.largura_saida, preparada.altura_saida
    ));
    // Recursos que o Windows de quem transmite recusou. A captura vive noutro
    // crate e não alcança este log; sem esta drenagem, uma degradação só
    // apareceria num `eprintln!` que ninguém lê — e foi o que faltou para
    // entender por que uma máquina não conseguia compartilhar.
    for aviso in tela_capture::video::avisos() {
        registro::linha(&format!("captura: {aviso}"));
    }

    // A política só pode nascer aqui: o automático precisa saber a altura da
    // fonte para decidir se o degrau nativo existe, e isso só se sabe depois
    // do primeiro quadro — é o WGC que dita o tamanho.
    let politica = escada::Politica::do_preset(&pedido.preset, preparada.altura);
    let saida = transmissao::escala_para(
        preparada.largura,
        preparada.altura,
        politica.degrau().altura,
    )
    .unwrap_or((preparada.largura, preparada.altura));
    registro::linha(&format!(
        "qualidade: preset {:?} -> {}p{} até {} kbps",
        pedido.preset,
        politica.degrau().altura,
        politica.degrau().fps,
        politica.degrau().max
    ));

    // Monitor que captura preto é falha silenciosa dos dois lados: aqui parece
    // que está tudo bem, e quem assiste vê preto. Avisar é o mínimo.
    if preparada.monitor_preto {
        let _ = app.emit(
            "aviso",
            "A captura deste monitor voltou toda preta. Costuma ser driver de              vídeo desatualizado. Compartilhar uma janela específica costuma              funcionar nesses casos.",
        );
    }

    let whip_url = {
        let guarda = estado
            .sala
            .lock()
            .map_err(|_| erro_txt("sala envenenada"))?;
        let Some(s) = guarda.as_ref() else {
            preparada.descarta();
            return Err(erro_txt("você não está na sala"));
        };
        match s.publica(
            fonte.tipo_protocolo(),
            fonte.titulo(),
            sala::Settings {
                // Da política, pelo mesmo motivo do supervisor: `prepara` roda
                // antes de a escada existir e não sabe a resolução final.
                w: saida.0 as i32,
                h: saida.1 as i32,
                fps: qualidade.fps as i32,
                kbps: qualidade.kbps as i32,
            },
        ) {
            Ok(u) => u,
            Err(e) => {
                preparada.descarta();
                return Err(erro(&e));
            }
        }
    };

    // Fotografa a janela agora: se ela morrer, o processo já não existirá e
    // não haveria a quem perguntar de onde ela veio.
    let assinatura = if pedido.tipo == "janela" {
        pedido
            .hwnd
            .parse::<isize>()
            .ok()
            .map(|h| tela_capture::foco::assina(h, pedido.pid, &pedido.titulo))
    } else {
        None
    };
    let pedido_para_seguir = pedido.clone();

    let (encoder_forcado, encoder_args) = {
        let c = estado
            .cfg
            .lock()
            .map_err(|_| erro_txt("configuração envenenada"))?;
        (c.encoder.clone(), c.encoder_args.clone())
    };

    let app_medida = app.clone();
    let app_fim = app.clone();
    let viva = transmissao::inicia(
        &estado.rt,
        preparada,
        politica,
        &caminho_ffmpeg,
        &whip_url,
        &encoder_forcado,
        &encoder_args,
        move |t| {
            // Uma linha por medida no log — que agora vai para o servidor.
            // "Está travando meu PC" só vira diagnóstico com número: dá para
            // ver se somos nós, qual codificador pegou, e a que custo.
            if let Some(cpu) = sistema::cpu_do_processo() {
                registro::linha(&format!(
                    "medida: {} kbps  {} fps (med {})  {}x{}  {}  cpu {:.0}%  quadros {}>{}  descartados {} (total {})  atraso {}ms  atrasadas {}  perda {}/1000  retransmitidos {}  jitter {}ms  cpu-encoder {}  params {}+{}",
                    t.kbps,
                    // Do intervalo. A média longa não mostra travada de dois
                    // segundos, e travada de dois segundos é o que reclamam.
                    t.fps,
                    t.fps_medio,
                    t.largura,
                    t.altura,
                    t.encoder,
                    cpu,
                    // Entregues pelo Windows > convertidos por nós. Os dois
                    // baixos é o compositor parando (jogo em tela cheia,
                    // janela minimizada); o primeiro alto e o segundo baixo
                    // somos nós não dando conta da conversão. Sem os dois,
                    // "congelou" não tem dono.
                    t.entregues,
                    t.convertidos,
                    // Quadros que NÓS jogamos fora: conversão que não acompanhou,
                    // fila cheia entre leitura e envio, e drops do ffmpeg.
                    //
                    // Estava faltando, e é a coluna que responde "está
                    // congelando". O número ia para a sala (é o "N perdidos" no
                    // rodapé da tela) mas não para o log, então quem relatava
                    // travada por chat não deixava rastro nenhum: `perda` cobre
                    // só o que se perde na REDE, e travada por CPU não aparece lá.
                    t.descartados_intervalo,
                    t.dropped,
                    // A máquina parando, vista de dentro: o resto da medida é
                    // dividido pelo tempo decorrido, e a divisão esconde a demora.
                    t.atraso_ms,
                    // Voltas do relógio de conversão que se perderam. Zero é o
                    // esperado; subindo, a cadência não está saindo no ritmo
                    // pedido e a imagem sai irregular mesmo com a rede limpa.
                    t.atrasadas,
                    t.perda_por_mil,
                    // Perda recuperada e perda perdida somam igual em
                    // `perda`. Este número separa as duas: com retransmissão
                    // acontecendo, baixar a taxa não é o caminho.
                    t.nacks,
                    // Jitter alto com perda zero é a assinatura da travada que
                    // nossa telemetria não enxergava.
                    t.jitter_ms,
                    // O encoder roda em processo separado. Sem esta coluna,
                    // "travou o PC" ficava sem resposta: a nossa medição
                    // mostrava 20% enquanto o ffmpeg trabalhava sem ninguem contar.
                    sistema::cpu_de(t.pid_encoder)
                        .map_or_else(|| "?".to_string(), |c| format!("{c:.0}%")),
                    // Do encoder + reinseridos por nós. O segundo subindo é a
                    // prova de que a reinserção está fazendo efeito; os dois em
                    // zero significa que ninguém consegue decodificar.
                    t.parametros_vistos,
                    t.parametros_reinseridos
                ));
            }
            let _ = app_medida.emit("telemetria", t);
            // A mesma medida vai para a sala, que a mostra no rodapé da tela
            // de quem assiste. Repassar daqui evita uma segunda amostragem
            // que discordaria da que a pessoa está vendo no app.
            if let Some(estado) = app_medida.try_state::<Estado>() {
                if let Ok(guarda) = estado.sala.lock() {
                    if let Some(s) = guarda.as_ref() {
                        let _ = s.estatisticas(t.kbps, t.fps, t.dropped, t.largura as i32, t.altura as i32);
                    }
                }
            }
        },
        {
            let app_escuro = app.clone();
            move |apagou: bool| {
                if apagou {
                    registro::linha(
                        "ATENCAO: a imagem transmitida ficou preta — jogo em tela cheia exclusiva?                          (em Sem bordas / Borderless a captura volta a enxergar)",
                    );
                    // Som de parada: quem transmite está dentro da partida e
                    // não vai ver aviso nenhum na tela.
                    som::toca(som::Aviso::Parou);
                } else {
                    registro::linha("a imagem voltou");
                    som::toca(som::Aviso::Comecou);
                }
                let _ = app_escuro.emit("imagem-preta", apagou);
            }
        },
        move |motivo| {
            // O supervisor terminou sozinho (ffmpeg caiu, janela fechou). A
            // interface precisa voltar para a tela de escolher fonte, senão
            // ficaria mostrando telemetria congelada de algo que já morreu.
            if let Some(estado) = app_fim.try_state::<Estado>() {
                if let Ok(guarda) = estado.sala.lock() {
                    if let Some(s) = guarda.as_ref() {
                        let _ = s.para();
                    }
                }
            }
            // Jogo não é um programa só: o League fecha o cliente para abrir
            // a partida e o reabre no fim. Antes disto a transmissão morria no
            // meio — quem assistia via a tela sumir, e quem jogava teria de
            // largar a partida para clicar em transmitir de novo.
            // O encoder escolhido não serve para transmitir. Troca para o
            // seguinte e recomeça, sem a pessoa precisar descobrir nada.
            if (motivo == transmissao::MOTIVO_ENCODER_MUDO
                || motivo == transmissao::MOTIVO_INDECIFRAVEL)
                && troca_de_encoder(&app_fim, &pedido_para_seguir)
            {
                return;
            }

            if motivo == transmissao::MOTIVO_FONTE_SUMIU
                || motivo == transmissao::MOTIVO_FONTE_TROCOU
            {
                if let Some(a) = assinatura.clone() {
                    if segue_para_a_sucessora(&app_fim, &a, &pedido_para_seguir) {
                        // Deu certo: nem para nem avisa que parou.
                        return;
                    }
                }
            }
            // Já fui substituída? Então este aviso é sobre uma transmissão que
            // não existe mais, e anunciá-lo derrubaria a que está no ar.
            if app_fim
                .try_state::<Estado>()
                .is_some_and(|e| e.geracao.load(Ordering::SeqCst) != minha_geracao)
            {
                registro::linha(&format!(
                    "transmissão anterior terminou ({motivo}) — já substituída, ignorando"
                ));
                return;
            }
            bandeja::atualiza(&app_fim, None);
            // Volta a anunciar a sala: quem olhar o perfil ainda pode entrar.
            discord::define(discord::Estado::NaSala { pessoas: 1 });
            // Também quando a transmissão cai sozinha: é justamente aí que o
            // aviso vale mais, porque ninguém está olhando para o app.
            som::toca(som::Aviso::Parou);
            let _ = app_fim.emit("transmissao-fim", motivo);
        },
    )
    .map_err(|e| erro(&e))?;

    *estado
        .transmissao
        .lock()
        .map_err(|_| erro_txt("transmissão envenenada"))? = Some(viva);
    bandeja::atualiza(app, Some(&pedido.titulo));
    discord::define(discord::Estado::NoAr {
        desde: discord::agora(),
    });

    // Avisa a interface quando a codificação está na CPU. A pessoa merece
    // saber por que o computador ficou mais pesado — e é o motivo de o jogo
    // dela perder quadros enquanto transmite.
    if let Ok(t) = estado.transmissao.lock() {
        if let Some(v) = t.as_ref() {
            let e = v.telemetria.le().encoder;
            if e == "libx264" {
                registro::linha("ATENCAO: codificando por software — a GPU nao esta sendo usada");
                let _ = app.emit("encoder-por-software", e);
            }
        }
    }
    som::toca(som::Aviso::Comecou);

    salva_config(estado, |c| {
        // Só o preset. Resolução, fps e taxa não são mais escolha da pessoa —
        // são consequência do preset e da rede, e guardá-los faria a próxima
        // sessão começar num ponto que talvez não valha mais.
        c.preset = pedido.preset.clone();
        c.fonte_tipo = pedido.tipo.clone();
        c.fonte_titulo = pedido.titulo.clone();
    });

    Ok(())
}

/// Encerra a transmissão sem segurar a thread principal.
///
/// O desmonte mata o ffmpeg, junta três threads e manda o DELETE do WHIP —
/// décimos de segundo no melhor caso, mais que isso quando a rede está ruim.
/// Comando síncrono do Tauri roda NA thread principal, então fazer isso lá
/// congela a janela. Pior: o supervisor avisa a bandeja ao terminar, e a
/// bandeja precisa da principal — que estaria presa esperando o supervisor.
pub fn encerra_em_segundo_plano(app: AppHandle, motivo: &'static str) {
    std::thread::spawn(move || {
        let Some(estado) = app.try_state::<Estado>() else {
            return;
        };
        let viva = estado.transmissao.lock().ok().and_then(|mut g| g.take());
        if let Some(t) = viva {
            t.encerra();
        }
        // `publish_stop` mesmo que nada estivesse no ar: é idempotente do lado
        // do servidor e cobre o caso de o supervisor ter morrido sem avisar.
        if let Ok(guarda) = estado.sala.lock() {
            if let Some(s) = guarda.as_ref() {
                let _ = s.para();
            }
        }
        // Bandeja, som e evento NÃO são repetidos aqui: quando havia
        // transmissão, o supervisor já fez os três ao terminar, e duplicar
        // daria dois bipes e dois avisos na tela para uma parada só.
        registro::linha(&format!("transmissão encerrada: {motivo}"));
    });
}

#[tauri::command]
fn parar(app: AppHandle, estado: State<'_, Estado>) -> Result<(), String> {
    // Esquecer o pedido é parte de parar: quem parou de propósito não quer a
    // transmissão de volta se a sala oscilar depois.
    if let Ok(mut p) = estado.ultimo_pedido.lock() {
        *p = None;
    }
    encerra_em_segundo_plano(app, "parada pelo usuário");
    Ok(())
}

#[tauri::command]
fn telemetria_atual(estado: State<'_, Estado>) -> Option<transmissao::Telemetria> {
    estado
        .transmissao
        .lock()
        .ok()?
        .as_ref()
        .map(|t| t.telemetria.le())
}

#[tauri::command]
fn conectado(estado: State<'_, Estado>) -> bool {
    estado
        .sala
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(sala::Sala::viva))
        .unwrap_or(false)
}

#[tauri::command]
fn abrir_sala(estado: State<'_, Estado>) -> Result<(), String> {
    let url = {
        let cfg = estado
            .cfg
            .lock()
            .map_err(|_| erro_txt("configuração envenenada"))?;
        cfg.servidor.clone()
    };
    sistema::abre_no_navegador(&url).map_err(|e| erro(&e))
}

/// Encoders já descartados nesta sessão por não servirem.
///
/// Sem esta lista o app trocaria para o mesmo encoder ruim de novo a cada
/// tentativa, num laço.
static ENCODERS_DESCARTADOS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Troca para o próximo encoder da cascata e recomeça a transmissão.
///
/// Devolve se conseguiu. Chamado quando o encoder atual se prova incapaz de
/// produzir um fluxo decodificável — hoje, quando nunca emite os parâmetros da
/// imagem. Trocar sozinho é o que separa "a sala vê de novo em segundos" de
/// "alguém descobre horas depois olhando para o preto".
fn troca_de_encoder(app: &AppHandle, anterior: &PedidoTransmitir) -> bool {
    let Some(estado) = app.try_state::<Estado>() else {
        return false;
    };
    let atual = {
        let Ok(cfg) = estado.cfg.lock() else {
            return false;
        };
        cfg.encoder.clone()
    };

    let Ok(mut descartados) = ENCODERS_DESCARTADOS.lock() else {
        return false;
    };
    if !atual.is_empty() && !descartados.contains(&atual) {
        descartados.push(atual.clone());
    }

    // O primeiro da cascata que ainda não foi descartado. `libx264` está lá e
    // sempre funciona, então existe saída mesmo com todo hardware reprovado.
    let Some(proximo) = tela_capture::ffmpeg::ENCODER_CASCADE
        .iter()
        .find(|e| !descartados.iter().any(|d| d == *e))
    else {
        registro::linha("nenhum encoder sobrou para tentar");
        return false;
    };
    let proximo = (*proximo).to_string();
    descartados.push(proximo.clone());
    drop(descartados);

    registro::linha(&format!(
        "o encoder anterior não emitia os parâmetros da imagem; trocando para {proximo}"
    ));
    salva_config(&estado, |c| c.encoder = proximo.clone());

    if let Ok(mut g) = estado.transmissao.lock() {
        if let Some(v) = g.take() {
            // Somos a thread do supervisor: esperar por nós mesmos travaria.
            v.abandona();
        }
    }

    match inicia_transmissao(app, &estado, anterior.clone()) {
        Ok(()) => {
            let _ = app.emit("encoder-trocado", proximo);
            true
        }
        Err(e) => {
            registro::linha(&format!("não consegui trocar de encoder: {e}"));
            false
        }
    }
}

/// Quanto tempo esperar pela janela que substitui a que morreu.
///
/// O League leva alguns segundos entre fechar o cliente e o jogo aparecer.
/// Vinte segundos cobrem isso com folga; passar muito disso seria ficar
/// transmitindo o nada para quem está do outro lado.
const ESPERA_SUCESSORA: std::time::Duration = std::time::Duration::from_secs(20);

/// Retoma a transmissão na janela que substituiu a que fechou.
///
/// Devolve se conseguiu. Roda na thread do supervisor, que está terminando de
/// qualquer forma — bloquear aqui não segura ninguém.
fn segue_para_a_sucessora(
    app: &AppHandle,
    assinatura: &tela_capture::foco::Assinatura,
    anterior: &PedidoTransmitir,
) -> bool {
    registro::linha(&format!(
        "a janela \"{}\" saiu de cena; procurando para onde o programa foi",
        assinatura.titulo
    ));

    let limite = std::time::Instant::now() + ESPERA_SUCESSORA;
    let nova = loop {
        if let Some(j) = tela_capture::foco::acha_sucessora(assinatura) {
            break Some(j);
        }
        if std::time::Instant::now() >= limite {
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(700));
    };

    let Some(j) = nova else {
        registro::linha("não achei janela sucessora; encerrando");
        return false;
    };
    registro::linha(&format!("seguindo para \"{}\" (pid {})", j.title, j.pid));

    // A transmissão velha ainda está guardada no `Estado`. Tirar antes evita
    // que `inicia_transmissao` tente encerrá-la — o que daria `join` nesta
    // própria thread e travaria.
    let Some(estado) = app.try_state::<Estado>() else {
        return false;
    };
    if let Ok(mut g) = estado.transmissao.lock() {
        if let Some(v) = g.take() {
            v.abandona();
        }
    }

    let pedido = PedidoTransmitir {
        hwnd: j.hwnd.to_string(),
        pid: j.pid,
        titulo: j.title.clone(),
        ..anterior.clone()
    };
    match inicia_transmissao(app, &estado, pedido) {
        Ok(()) => {
            let _ = app.emit("fonte-trocada", j.title);
            true
        }
        Err(e) => {
            registro::linha(&format!("não consegui seguir: {e}"));
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Atalhos globais
// ---------------------------------------------------------------------------

/// Resolve a fonte que um atalho escolheu, no formato que a interface já usa
/// para montar o pedido de transmissão.
///
/// A resolução mora no Rust porque é Win32 puro; a qualidade e o anti-eco
/// continuam na interface, para o atalho passar pelo mesmo caminho do botão.
#[tauri::command]
fn fonte_do_atalho(qual: String) -> Result<serde_json::Value, String> {
    match qual.as_str() {
        "monitor" => {
            let m = tela_capture::foco::monitor_sob_o_mouse().map_err(|e| erro(&e))?;
            registro::linha(&format!("atalho: monitor {} ({})", m.index, m.name));
            Ok(serde_json::json!({
                "tipo": "monitor",
                "indice": m.index,
                "hwnd": "0",
                "pid": 0,
                "titulo": if m.name.trim().is_empty() {
                    format!("Monitor {}", m.index)
                } else {
                    m.name
                },
            }))
        }
        "janela" => {
            let j = tela_capture::foco::janela_em_foco().map_err(|e| erro(&e))?;
            registro::linha(&format!("atalho: janela \"{}\" (pid {})", j.title, j.pid));
            Ok(serde_json::json!({
                "tipo": "janela",
                "indice": 0,
                "hwnd": j.hwnd.to_string(),
                "pid": j.pid,
                "titulo": j.title,
                "processo": nome_do_processo(j.pid),
            }))
        }
        outro => Err(erro_txt(format!("atalho desconhecido: {outro}"))),
    }
}

/// Traz a janela do app para a frente.
///
/// Usada quando um atalho não pôde fazer nada — fora da sala, ou fonte não
/// capturável. Sem isto a pessoa aperta a tecla no meio do jogo e não recebe
/// sinal nenhum de que deu errado.
#[tauri::command]
fn aparece(app: AppHandle) -> Result<(), String> {
    let Some(janela) = app.get_webview_window("main") else {
        return Err(erro_txt("janela principal não encontrada"));
    };
    let _ = janela.unminimize();
    let _ = janela.show();
    let _ = janela.set_focus();
    Ok(())
}

// ---------------------------------------------------------------------------
// Atualização e encoder
// ---------------------------------------------------------------------------

/// Guarda o manifesto entre a consulta e o clique em "atualizar", para não
/// baixar uma versão diferente da que foi anunciada.
static MANIFESTO: Mutex<Option<atualizacao::Manifesto>> = Mutex::new(None);

/// Última versão sobre a qual já escrevemos no log, para não repetir.
static JA_AVISADA: Mutex<Option<String>> = Mutex::new(None);

/// Consulta se há versão nova. Chamada pela interface logo depois de abrir.
///
/// Devolve `Situacao` mesmo quando a consulta falha (com `tem_nova: false`):
/// ficar sem internet não é um erro que valha uma faixa vermelha num app cujo
/// trabalho é transmitir numa rede que pode ser só local.
#[tauri::command]
async fn checa_versao(estado: State<'_, Estado>) -> Result<atualizacao::Situacao, String> {
    let servidor = {
        let cfg = estado
            .cfg
            .lock()
            .map_err(|_| erro_txt("configuração envenenada"))?;
        cfg.servidor.clone()
    };
    let atual = atualizacao::versao_atual().to_string();
    let Some(m) = atualizacao::consulta(&servidor).await else {
        return Ok(atualizacao::Situacao {
            atual,
            ..Default::default()
        });
    };
    let tem_nova = atualizacao::e_mais_nova(&m.versao, &atual);
    let situacao = atualizacao::Situacao {
        disponivel: m.versao.clone(),
        atual,
        tem_nova,
        mb: m.app.bytes / 1024 / 1024,
    };
    // Só quando MUDA. A interface passou a reconsultar a cada meia hora para
    // quem deixa o app aberto por dias descobrir versão nova sem reiniciar, e
    // sem esta guarda a mesma linha entraria no log para sempre — dezenas de
    // repetições idênticas afogando o que interessa.
    if tem_nova {
        let novidade = JA_AVISADA.lock().is_ok_and(|mut ja| {
            ja.as_deref() != Some(m.versao.as_str()) && {
                *ja = Some(m.versao.clone());
                true
            }
        });
        if novidade {
            registro::linha(&format!(
                "versão {} disponível (rodando {})",
                m.versao, situacao.atual
            ));
        }
    }
    // As DLLs do encoder em processo vêm por aqui, uma vez. Em silêncio e sem
    // barra de progresso: nada depende delas para o app funcionar, e enquanto
    // não chegam a transmissão usa o `ffmpeg.exe` como sempre usou.
    if !m.libav.is_empty() {
        if let (Some(dir), Ok(token)) = (dir_cache(), token_da_sala(&estado)) {
            let quantas = atualizacao::garante_libav(&servidor, &token, &dir, &m.libav).await;
            if quantas > 0 {
                registro::linha(&format!("libav: {quantas} arquivo(s) prontos"));
            }
        }
    }

    if let Ok(mut guarda) = MANIFESTO.lock() {
        *guarda = Some(m);
    }
    Ok(situacao)
}

/// Token da sessão, ou erro legível. Os downloads ficam atrás dele.
fn token_da_sala(estado: &State<'_, Estado>) -> Result<String, String> {
    let sala = estado
        .sala
        .lock()
        .map_err(|_| erro_txt("sala envenenada"))?;
    match sala.as_ref() {
        Some(s) => Ok(s.token().to_string()),
        None => Err(erro_txt("entre na sala primeiro")),
    }
}

/// Baixa a versão nova e reinicia o app.
///
/// Exige estar na sala porque o binário é protegido por token — é a mesma
/// porta por onde os amigos baixam o app pela primeira vez.
#[tauri::command]
async fn atualizar(app: AppHandle, estado: State<'_, Estado>) -> Result<(), String> {
    let token = token_da_sala(&estado)?;
    let servidor = {
        let cfg = estado
            .cfg
            .lock()
            .map_err(|_| erro_txt("configuração envenenada"))?;
        cfg.servidor.clone()
    };
    let manifesto = MANIFESTO
        .lock()
        .map_err(|_| erro_txt("manifesto envenenado"))?
        .clone()
        .ok_or_else(|| erro_txt("consulte a versão antes de atualizar"))?;

    // Recusa trocar por algo que não é mais novo. A interface já decide isso
    // antes de mostrar o botão, mas quando ela errou — e errou — o resultado
    // foi baixar 23 MB para reinstalar a mesma versão, de novo e de novo.
    if !atualizacao::e_mais_nova(&manifesto.versao, atualizacao::versao_atual()) {
        return Err(erro_txt(format!(
            "você já está na versão {}",
            atualizacao::versao_atual()
        )));
    }

    // Ao lado do executável: `troca_por` renomeia, e renomear entre volumes
    // diferentes falha no Windows.
    let atual = std::env::current_exe().map_err(|_| erro_txt("não sei onde estou instalado"))?;
    let destino = atual.with_extension("novo");

    baixa_com_progresso(&app, &servidor, &token, &manifesto.app, &destino, "app").await?;

    // A partir daqui o app fecha: quem chamou não recebe mais nada.
    atualizacao::troca_por(&destino).map_err(|e| erro(&e))?;
    app.exit(0);
    Ok(())
}

/// Baixa o encoder para o cache, se ainda não estiver lá.
///
/// Separado do `.exe` de propósito: são 140 MB que mudam quase nunca, contra um
/// app de 20 MB que muda toda semana. Embutir os dois faria cada atualização
/// custar sete vezes mais banda a cada amigo.
#[tauri::command]
async fn garante_encoder(app: AppHandle, estado: State<'_, Estado>) -> Result<String, String> {
    let dir = dir_cache().ok_or_else(|| erro_txt("sem %LOCALAPPDATA%"))?;
    let destino = atualizacao::caminho_ffmpeg_baixado(&dir);
    if destino.exists() {
        return Ok(destino.to_string_lossy().into_owned());
    }
    // Já existe embutido ou ao lado do executável: não há o que baixar.
    let embutido_ou_local = {
        let cfg = estado
            .cfg
            .lock()
            .map_err(|_| erro_txt("configuração envenenada"))?;
        resolve_ffmpeg(&cfg)
    };
    if std::path::Path::new(&embutido_ou_local).exists() {
        return Ok(embutido_ou_local);
    }

    let token = token_da_sala(&estado)?;
    let servidor = {
        let cfg = estado
            .cfg
            .lock()
            .map_err(|_| erro_txt("configuração envenenada"))?;
        cfg.servidor.clone()
    };
    let manifesto = MANIFESTO
        .lock()
        .map_err(|_| erro_txt("manifesto envenenado"))?
        .clone()
        .ok_or_else(|| erro_txt("não sei qual encoder baixar"))?;
    if manifesto.ffmpeg.bytes == 0 {
        return Err(erro_txt("o servidor não publicou um encoder"));
    }

    std::fs::create_dir_all(&dir).map_err(|e| erro_txt(format!("criar {}: {e}", dir.display())))?;
    baixa_com_progresso(
        &app,
        &servidor,
        &token,
        &manifesto.ffmpeg,
        &destino,
        "encoder",
    )
    .await?;
    Ok(destino.to_string_lossy().into_owned())
}

/// Baixa emitindo `download-progresso` para a interface.
///
/// O evento sai no máximo a cada 1% porque 140 MB em pedaços de 8 KB são ~18
/// mil eventos: a interface passaria mais tempo redesenhando do que o download
/// leva.
async fn baixa_com_progresso(
    app: &AppHandle,
    servidor: &str,
    token: &str,
    arq: &atualizacao::Arquivo,
    destino: &std::path::Path,
    qual: &str,
) -> Result<(), String> {
    let app_ev = app.clone();
    let qual_ev = qual.to_string();
    let mut ultimo = u64::MAX;
    atualizacao::baixa(servidor, token, arq, destino, move |feito, total| {
        let pct = (feito * 100).checked_div(total).unwrap_or(0);
        if pct != ultimo {
            ultimo = pct;
            let _ = app_ev.emit(
                "download-progresso",
                serde_json::json!({ "qual": qual_ev, "pct": pct, "mb": total / 1024 / 1024 }),
            );
        }
    })
    .await
    .map_err(|e| erro(&e))
}

/// Aplica uma mudança na configuração e grava.
///
/// Falha de gravação não vira erro na interface de propósito: a pessoa está no
/// meio de entrar ou de transmitir, e interromper isso por causa de uma
/// preferência que não persistiu seria trocar um incômodo por um bloqueio. O
/// aviso do diretório não-gravável já apareceu na abertura.
fn salva_config(estado: &Estado, muda: impl FnOnce(&mut config::Config)) {
    let Ok(mut cfg) = estado.cfg.lock() else {
        return;
    };
    muda(&mut cfg);
    if let Err(e) = config::salva(&estado.local, &cfg) {
        eprintln!("gravar configuração: {e:#}");
    }
}

// ---------------------------------------------------------------------------

/// Registra os atalhos globais, se configurados.
///
/// Falhar aqui NÃO impede o app de abrir: uma combinação já tomada por outro
/// programa é um contratempo, não um motivo para ninguém conseguir
/// transmitir. O log diz qual não pegou.
fn registra_atalhos(app: &AppHandle, monitor: &str, janela: &str) {
    use tauri_plugin_global_shortcut::GlobalShortcutExt as _;

    for (tecla, qual) in [(monitor, "monitor"), (janela, "janela")] {
        if tecla.trim().is_empty() {
            continue;
        }
        let alvo = app.clone();
        let nome = qual;
        let qual = qual.to_string();
        // O `handler` dispara em qualquer aplicativo em foco — é para isso que
        // o atalho serve. A interface faz o resto pelo evento.
        let r = app
            .global_shortcut()
            .on_shortcut(tecla, move |_, _, evento| {
                // Só na descida: sem isto, cada toque dispararia duas vezes.
                if evento.state() != tauri_plugin_global_shortcut::ShortcutState::Pressed {
                    return;
                }
                registro::linha(&format!("atalho acionado: {qual}"));
                let _ = alvo.emit("atalho", qual.clone());
            });
        match r {
            Ok(()) => registro::linha(&format!("atalho {tecla} → {nome}")),
            Err(e) => registro::linha(&format!(
                "não consegui registrar {tecla} ({nome}): {e} — outro programa já usa?"
            )),
        }
    }
}

fn main() {
    // ANTES de tudo, inclusive do Tauri: se esta cópia nasceu de uma
    // atualização, espera a anterior sair. O controle de instância única roda
    // dentro do `Builder` e mataria esta cópia se a antiga ainda estivesse de
    // pé — ver `atualizacao::aguarda_anterior`.
    atualizacao::aguarda_anterior();

    // Um runtime só para tudo: a sala e o WHIP compartilham workers. Multi-thread
    // e não `current_thread` porque as tarefas de repasse RTP precisam continuar
    // bombeando enquanto o supervisor está bloqueado num `block_on`.
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => Arc::new(rt),
        Err(e) => {
            eprintln!("não consegui criar o runtime: {e}");
            std::process::exit(1);
        }
    };

    // Se nem o diretório do executável nem o LOCALAPPDATA servirem, o app ainda
    // abre — só não lembra nada. Melhor que não abrir.
    let local = config::local().unwrap_or_else(|e| {
        eprintln!("configuração: {e:#}");
        config::Local {
            caminho: std::path::PathBuf::from("tela-config.json"),
            fallback: true,
        }
    });
    let cfg = config::carrega(&local);

    // O log NÃO é aberto aqui.
    //
    // Abrir aqui significaria que uma segunda cópia do app — aberta por quem
    // achou que tinha fechado, já que o X esconde na bandeja — rotacionaria o
    // arquivo e apagaria o registro da cópia que está rodando, antes de o
    // controle de instância única matá-la. Aconteceu no teste: sumiram até as
    // linhas dos atalhos. Fica para o `setup`, por onde só passa quem vive.
    let dir_log = local
        .caminho
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let token_log: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    // Copiados antes de o estado consumir os originais.
    let discord_id = cfg.discord_app_id.clone();
    let sala_publica = cfg.servidor.clone();
    let caminho_cfg = local.caminho.display().to_string();
    let servidor_log = cfg.servidor.clone();
    let compartilhado = Arc::clone(&token_log);

    let estado = Estado {
        credencial: Mutex::new(None),
        ultimo_pedido: Mutex::new(None),
        rt,
        token_log,
        local,
        cfg: Mutex::new(cfg),
        sala: Mutex::new(None),
        transmissao: Mutex::new(None),
        geracao: AtomicU64::new(0),
    };

    // Os atalhos são lidos antes do `manage`, que consome o estado.
    let (atalho_monitor, atalho_janela) = match estado.cfg.lock() {
        Ok(c) => (c.atalho_monitor.clone(), c.atalho_janela.clone()),
        Err(_) => (String::new(), String::new()),
    };
    // Pelo mesmo motivo, uma cópia da configuração para a sonda de encoders
    // resolver o caminho do ffmpeg lá na thread dela.
    let cfg_sonda = estado.cfg.lock().ok().map(|c| c.clone());

    let resultado = tauri::Builder::default()
        // Uma cópia só. Desde que o X passou a esconder na bandeja, abrir o
        // `.exe` de novo virou o reflexo natural de quem acha que fechou o
        // app — e duas cópias custam caro: cada uma captura e codifica por
        // conta própria, e a segunda nem consegue registrar os atalhos,
        // porque a primeira já os tomou. O sintoma foi um atalho que parou de
        // responder no meio do jogo.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            registro::linha("já havia uma cópia aberta; trazendo ela para a frente");
            if let Some(j) = app.get_webview_window("main") {
                let _ = j.show();
                let _ = j.unminimize();
                let _ = j.set_focus();
            }
        }))
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .setup(move |app| {
            // Daqui para baixo só roda a cópia que sobreviveu ao controle de
            // instância única — ver o comentário no `main`.
            registro::inicia(&registro::caminho(&dir_log));
            // Restos de uma atualização anterior: o Windows não deixa apagar
            // um `.exe` em uso, então a limpeza fica para a abertura seguinte.
            atualizacao::limpa_antigo();
            registro::linha(&format!("config em {caminho_cfg}"));
            // Onde o log foi parar, dito dentro do próprio log: quando ele não
            // está onde se espera, é isto que aponta para onde procurar.
            if let Some(onde) = registro::onde() {
                registro::linha(&format!("log em {}", onde.display()));
            }
            registro::linha(&format!("windows build {}", sistema::build_do_windows()));
            // Placas de vídeo no log: 'Microsoft Basic Display Adapter'
            // explicaria de uma vez tanto encoder por hardware que não carrega
            // quanto captura de monitor preta.
            for placa in sistema::placas_de_video() {
                registro::linha(&format!("gpu: {placa}"));
            }
            // Quais encoders desta máquina aceitam receber a textura direto da
            // GPU. Não muda nada no que o app faz — a transmissão continua
            // inteira pelo `ffmpeg.exe`. É só para saber, das máquinas de quem
            // usa, quais suportam o caminho novo antes de ligá-lo em alguém.
            //
            // Roda no `ffmpeg.exe` que já está em cache, e não na libavcodec
            // linkada, justamente para esta versão não exigir download nenhum
            // nem correr risco de não abrir.
            //
            // Em thread separada porque leva alguns segundos e ninguém espera
            // pelo resultado; segurar a janela por isso seria absurdo.
            std::thread::spawn(move || {
                let Some(cfg) = cfg_sonda else { return };
                let ffmpeg = resolve_ffmpeg(&cfg);
                if !std::path::Path::new(&ffmpeg).exists() {
                    // Primeira execução, antes do download. Na próxima vez sai.
                    registro::linha("sonda d3d11: sem ffmpeg em cache ainda");
                    return;
                }
                registro::linha(&format!(
                    "sonda d3d11: {}",
                    tela_capture::ffmpeg::sonda_d3d11(&ffmpeg)
                ));
                // E se a libavcodec está no cache. Junto com a linha acima, o
                // log responde de uma vez as duas perguntas que decidem por
                // qual caminho a máquina vai transmitir.
                if let Some(dir) = dir_cache() {
                    let pasta = atualizacao::pasta_libav(&dir);
                    registro::linha(&format!(
                        "libav: {}",
                        if tela_capture::libav::carrega(&pasta) {
                            "carregada".to_string()
                        } else {
                            format!(
                                "indisponível em {} — {}",
                                pasta.display(),
                                tela_capture::libav::porque_falhou()
                            )
                        }
                    ));
                }
            });
            // Presença no Discord. Sem Application ID no config, isto não
            // cria thread nenhuma nem fala com ninguém.
            discord::inicia(discord_id, sala_publica);

            remoto::inicia(servidor_log, move || {
                compartilhado.lock().ok().and_then(|t| t.clone())
            });

            registra_atalhos(app.handle(), &atalho_monitor, &atalho_janela);
            if let Err(e) = bandeja::instala(app.handle()) {
                // Sem bandeja o app ainda serve; só volta a ocupar a barra de
                // tarefas. Fechar a janela passa a encerrar, o que é o
                // comportamento correto quando não há para onde esconder.
                registro::linha(&format!("bandeja indisponível: {e}"));
            }
            Ok(())
        })
        .on_window_event(|janela, evento| {
            // O X esconde em vez de encerrar: é o processo que escuta os
            // atalhos globais, e matá-lo desligaria as teclas sem aviso. Sair
            // de verdade é pelo menu da bandeja.
            if let tauri::WindowEvent::CloseRequested { api, .. } = evento {
                if janela.app_handle().tray_by_id(bandeja::ID).is_some() {
                    api.prevent_close();
                    let _ = janela.hide();
                }
            }
        })
        .manage(estado)
        .invoke_handler(tauri::generate_handler![
            inicial,
            listar_fontes,
            entrar,
            sair,
            transmitir,
            parar,
            telemetria_atual,
            conectado,
            abrir_sala,
            checa_versao,
            atualizar,
            garante_encoder,
            fonte_do_atalho,
            aparece,
        ])
        .run(tauri::generate_context!());

    if let Err(e) = resultado {
        eprintln!("erro fatal na interface: {e}");
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// Contrato com a interface
// ---------------------------------------------------------------------------

#[cfg(test)]
mod geracao {
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Imita a ordem que `inicia_transmissao` executa, que é o que importa.
    ///
    /// A comparação em si é trivial; o que já quebrou foi a ORDEM. Enquanto a
    /// geração subia depois de encerrar a anterior, o aviso de morte da
    /// anterior — disparado de dentro do `encerra()` — ainda se via como o
    /// atual e derrubava a transmissão que tinha acabado de entrar no ar.
    #[test]
    fn aviso_da_anterior_nao_derruba_a_nova() {
        let geracao = AtomicU64::new(0);
        let vale = |minha: u64| geracao.load(Ordering::SeqCst) == minha;

        // Primeira transmissão entra no ar.
        let primeira = geracao.fetch_add(1, Ordering::SeqCst) + 1;
        assert!(vale(primeira), "sozinha, a primeira é a atual");

        // "Trocar fonte": a geração sobe ANTES de encerrar a anterior.
        let segunda = geracao.fetch_add(1, Ordering::SeqCst) + 1;

        // Só então a anterior morre e dispara o aviso.
        assert!(
            !vale(primeira),
            "o aviso da anterior tinha de ser reconhecido como velho"
        );
        assert!(vale(segunda), "a nova continua sendo a atual");

        // E quando a nova terminar de verdade, o aviso dela vale.
        assert!(vale(segunda));
    }

    /// Na ordem errada — geração subindo só depois — o aviso velho passaria.
    /// Serve para provar que o teste acima está medindo alguma coisa.
    #[test]
    fn a_ordem_invertida_deixaria_passar() {
        let geracao = AtomicU64::new(0);
        let primeira = geracao.fetch_add(1, Ordering::SeqCst) + 1;

        // Encerra a anterior ANTES de subir a geração (o jeito antigo).
        let aviso_passa = geracao.load(Ordering::SeqCst) == primeira;
        let _segunda = geracao.fetch_add(1, Ordering::SeqCst) + 1;

        assert!(
            aviso_passa,
            "na ordem antiga o aviso velho passava — é o defeito que foi corrigido"
        );
    }
}

#[cfg(test)]
mod atalhos {
    /// Os atalhos padrão têm de ser aceitos pelo parser do plugin.
    ///
    /// Um nome de tecla errado só apareceria como uma linha no log e um atalho
    /// que não faz nada — o tipo de falha que ninguém investiga, porque parece
    /// "não implementaram direito".
    #[test]
    fn os_padroes_sao_reconhecidos() {
        use std::str::FromStr as _;
        for tecla in [crate::config::ATALHO_MONITOR, crate::config::ATALHO_JANELA] {
            assert!(
                tauri_plugin_global_shortcut::Shortcut::from_str(tecla).is_ok(),
                "o plugin não entende {tecla:?}"
            );
        }
    }

    /// E um nome inventado tem de falhar, senão o teste acima não prova nada.
    #[test]
    fn nome_inventado_e_recusado() {
        use std::str::FromStr as _;
        assert!(tauri_plugin_global_shortcut::Shortcut::from_str("Ctrl+Asterisco").is_err());
    }
}

#[cfg(test)]
mod contrato {
    use super::*;

    /// O JSON exato que `app.js` monta em `invoke("transmitir", …)`.
    ///
    /// Este teste existe porque o campo `semEco` chegava como `sem_eco` do
    /// lado do Rust, não casava, e virava `false` sem reclamar: quem
    /// compartilhava o monitor inteiro mandava o áudio do Discord junto,
    /// com a caixa "não enviar" marcada na tela.
    #[test]
    fn o_pedido_da_interface_desserializa_com_os_nomes_certos() {
        let json = serde_json::json!({
            "tipo": "monitor",
            "hwnd": "0",
            "pid": 0,
            "indice": 1,
            "titulo": "Monitor 2",
            "preset": "qualidade",
            "semEco": true
        });
        let p: PedidoTransmitir = serde_json::from_value(json).expect("desserializar");
        assert!(p.sem_eco, "semEco=true tem de chegar como sem_eco=true");
        assert_eq!(p.indice, 1);
        assert_eq!(p.preset, "qualidade");
    }

    /// Faltando o campo, é erro — não `false` calado.
    #[test]
    fn pedido_sem_o_campo_de_eco_falha() {
        let json = serde_json::json!({
            "tipo": "monitor", "hwnd": "0", "pid": 0, "indice": 0,
            "titulo": "Monitor 1", "preset": "qualidade"
        });
        assert!(serde_json::from_value::<PedidoTransmitir>(json).is_err());
    }

    /// Um nome que a struct não conhece também é erro, pelo mesmo motivo.
    #[test]
    fn pedido_com_nome_desconhecido_falha() {
        let json = serde_json::json!({
            "tipo": "monitor", "hwnd": "0", "pid": 0, "indice": 0,
            "titulo": "Monitor 1", "preset": "jogo",
            "altura": 1080, "fps": 60, "kbps": 8000,
            "sem_eco": true
        });
        assert!(
            serde_json::from_value::<PedidoTransmitir>(json).is_err(),
            "snake_case vindo do JS não é o contrato e não pode passar calado"
        );
    }
}

// ---------------------------------------------------------------------------
// Teste de ponta a ponta
// ---------------------------------------------------------------------------

/// Roda a sequência inteira que o comando [`transmitir`] executa — entrar na
/// sala, anunciar, capturar, encodar e publicar via WHIP — sem a interface no
/// caminho.
///
/// Fica `#[ignore]` porque exige uma sala e um MediaMTX de verdade no ar e uma
/// tela para capturar; nenhum dos dois cabe numa suíte automática. É o
/// equivalente do `probe-*` do CLI para a camada do app: a única forma honesta
/// de verificar transporte, que não tem teste unitário possível.
///
/// ```text
/// TELA_E2E_SERVIDOR=http://127.0.0.1:8090 TELA_E2E_SENHA=segredo \
///   cargo test -p tela-app -- --ignored --nocapture
/// ```
#[cfg(test)]
mod ponta_a_ponta {
    use std::sync::Arc;
    use std::time::Duration;

    use crate::{sala, transmissao};

    #[test]
    #[ignore = "exige sala + MediaMTX no ar e uma tela para capturar"]
    fn transmite_de_verdade() {
        let servidor = std::env::var("TELA_E2E_SERVIDOR").expect("TELA_E2E_SERVIDOR");
        let senha = std::env::var("TELA_E2E_SENHA").expect("TELA_E2E_SENHA");
        // Mesma resolução que o app usa em produção: o embutido tem
        // precedência. Resolver diferente aqui faria o teste exercitar um
        // caminho que ninguém executa de verdade.
        let ffmpeg = std::env::var("TELA_E2E_FFMPEG").unwrap_or_else(|_| {
            let local = crate::config::local().expect("local da configuração");
            crate::resolve_ffmpeg(&crate::config::carrega(&local))
        });
        println!("[ffmpeg] {ffmpeg}");
        let segundos: u64 = std::env::var("TELA_E2E_SEGUNDOS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(14);

        let rt = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("runtime"),
        );

        let sala = sala::Sala::entra(&rt, &servidor, &senha, "e2e", |ev| {
            if let sala::Evento::Estado(e) = ev {
                println!("[sala] {} pessoas, ping {} ms", e.pessoas, e.ping_ms);
            }
        })
        .expect("entrar na sala");
        println!("[sala] entrou");

        let monitores = tela_capture::video::list_monitors().expect("monitores");
        let m = monitores.first().expect("nenhum monitor").clone();
        println!("[fonte] monitor {} ({}x{})", m.name, m.width, m.height);

        let qualidade = transmissao::Qualidade {
            altura: 720,
            fps: 30,
            kbps: 2500,
        };
        // Exercita tambem o anti-eco: se houver app de voz aberto ele e
        // excluido, que e o caminho que a maioria vai usar na pratica.
        let excluir_pid = tela_capture::processos::acha_app_de_voz().map(|p| p.pid);
        println!("[audio] excluindo pid {excluir_pid:?}");
        let fonte = transmissao::Fonte::Monitor {
            indice: m.index,
            nome: m.name.clone(),
            excluir_pid,
        };
        let preparada =
            transmissao::prepara(&fonte, qualidade, true, false).expect("preparar captura");
        let politica = crate::escada::Politica::fixa(crate::escada::ESCADA[0]);
        println!(
            "[captura] {}x{} -> {}x{}",
            preparada.largura, preparada.altura, preparada.largura_saida, preparada.altura_saida
        );

        let whip = sala
            .publica(
                fonte.tipo_protocolo(),
                fonte.titulo(),
                sala::Settings {
                    w: preparada.largura_saida as i32,
                    h: preparada.altura_saida as i32,
                    fps: qualidade.fps as i32,
                    kbps: qualidade.kbps as i32,
                },
            )
            .expect("stream_ready");
        println!("[sala] whip: {whip}");

        let viva = transmissao::inicia(
            &rt,
            preparada,
            politica,
            &ffmpeg,
            &whip,
            "",
            &[],
            |t| {
                println!(
                    "[telemetria] {} kbps  {} fps  {} perdidos  {}x{}  {}  {}s  estado={}",
                    t.kbps, t.fps, t.dropped, t.largura, t.altura, t.encoder, t.segundos, t.estado
                );
            },
            |apagou| println!("[imagem] {}", if apagou { "apagou" } else { "voltou" }),
            |motivo| println!("[fim] {motivo}"),
        )
        .expect("iniciar transmissão");

        std::thread::sleep(Duration::from_secs(segundos));
        let ultima = viva.telemetria.le();
        viva.encerra();
        sala.para().expect("publish_stop");

        println!("[última leitura] {ultima:?}");
        assert_eq!(
            ultima.estado, "connected",
            "o WebRTC não chegou a connected"
        );
        assert!(ultima.kbps > 0, "nenhum byte saiu pela rede");
        assert!(ultima.fps > 0, "o encoder não produziu quadro nenhum");
    }
}

#[cfg(test)]
mod testes_troca_encoder {
    /// A escolha do próximo encoder não pode repetir os já descartados, e tem
    /// de terminar — um laço trocando para o mesmo encoder ruim seria pior que
    /// não trocar.
    #[test]
    fn a_cascata_termina_e_nao_repete() {
        let cascata = tela_capture::ffmpeg::ENCODER_CASCADE;
        let mut descartados: Vec<String> = Vec::new();
        let mut escolhidos: Vec<String> = Vec::new();

        // Simula todos falhando, um após o outro.
        for _ in 0..cascata.len() + 2 {
            let Some(proximo) = cascata
                .iter()
                .find(|e| !descartados.iter().any(|d| d == *e))
            else {
                break;
            };
            escolhidos.push((*proximo).to_string());
            descartados.push((*proximo).to_string());
        }

        assert_eq!(
            escolhidos.len(),
            cascata.len(),
            "devia tentar cada encoder exatamente uma vez: {escolhidos:?}"
        );
        let mut unicos = escolhidos.clone();
        unicos.sort_unstable();
        unicos.dedup();
        assert_eq!(
            unicos.len(),
            escolhidos.len(),
            "repetiu algum: {escolhidos:?}"
        );
    }

    /// O `libx264` precisa estar na cascata: é a saída que sempre funciona
    /// quando todo o hardware reprova.
    #[test]
    fn existe_sempre_uma_saida_por_software() {
        assert!(
            tela_capture::ffmpeg::ENCODER_CASCADE.contains(&"libx264"),
            "sem software na cascata, todo hardware ruim deixaria a sala sem imagem"
        );
    }
}
