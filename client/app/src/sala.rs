//! Cliente WebSocket do serviço `sala`.
//!
//! O WebSocket vive no Rust e **não** no WebView, por um motivo concreto: o
//! servidor valida `Origin` contra `tela.example.com`
//! (`coder/websocket`, `OriginPatterns`). Um `new WebSocket()` disparado pelo
//! WebView2 mandaria `Origin: http://tauri.localhost` e tomaria 403. Cliente
//! nativo não manda `Origin` nenhum, e passa.
//!
//! O protocolo está em `sala/protocol.go` e a implementação de referência em
//! JavaScript em `sala/web/index.html`. Este módulo é a terceira ponta do
//! mesmo contrato — divergir aqui quebra a sala inteira, não só o app.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;
use tokio::sync::mpsc::UnboundedSender;
use tokio_tungstenite::tungstenite::Message;

/// Teto de espera pelo `welcome` (ou `denied`) depois do `hello`.
const ESPERA_ENTRADA: Duration = Duration::from_secs(15);

/// Teto de espera pelo `stream_ready` depois do `publish_start`.
const ESPERA_STREAM: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------------------
// Mensagens do protocolo
// ---------------------------------------------------------------------------

/// Envelope `{type, data}` — o formato de toda mensagem nos dois sentidos.
#[derive(Debug, Serialize, Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    tipo: String,
    #[serde(default)]
    data: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct Hello<'a> {
    password: &'a str,
    nick: &'a str,
    /// Versão do app, para a sala mostrar ao lado de quem transmite.
    ///
    /// Existe porque "atualiza aí" e "já atualizei" não fecham sozinhos: numa
    /// noite de correções seguidas, ninguém sabia quem estava em qual versão
    /// enquanto se comparavam sintomas. Vai no `hello` e não no `publish_start`
    /// porque é propriedade da pessoa, não da transmissão — assim também
    /// aparece de quem só está assistindo.
    versao: &'a str,
}

#[derive(Debug, Serialize)]
struct Pong {
    t: i64,
}

/// Ajustes anunciados no `publish_start`. Nomes em inglês porque são o
/// contrato JSON do servidor, não nomes nossos.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Settings {
    pub w: i32,
    pub h: i32,
    pub fps: i32,
    pub kbps: i32,
}

#[derive(Debug, Serialize)]
struct PublishStart<'a> {
    #[serde(rename = "sourceType")]
    source_type: &'a str,
    #[serde(rename = "sourceTitle")]
    source_title: &'a str,
    settings: Settings,
}

#[derive(Debug, Serialize)]
struct PublishStats {
    kbps: i32,
    fps: i32,
    dropped: i32,
    /// Resolução do que está indo AGORA.
    ///
    /// Vai junto das estatísticas e não só no `publish_start` porque ela muda:
    /// a escada troca de resolução quando a conexão aperta, e o valor do
    /// início passaria o resto da sessão mentindo para quem assiste.
    w: i32,
    h: i32,
}

/// O `token` do `welcome` não é preciso para transmitir — o servidor já o
/// embute na `whipUrl` do `stream_ready`. Ele é guardado por causa do
/// **download**: o encoder e as atualizações do app ficam atrás do mesmo token,
/// e estar na sala é a prova de que a pessoa sabe a senha.
#[derive(Debug, Clone, Deserialize)]
pub struct Welcome {
    #[serde(rename = "selfId")]
    pub self_id: String,
    #[serde(default)]
    pub token: String,
}

#[derive(Debug, Clone, Deserialize)]
struct StreamReady {
    #[serde(rename = "whipUrl")]
    whip_url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Participante {
    pub id: String,
    pub nick: String,
    #[serde(rename = "pingMs", default)]
    pub ping_ms: i64,
    #[serde(default)]
    pub sharing: String,
}

/// Só o que o app precisa do `state`. A sala web mostra o resto; aqui um
/// contador de gente e o meu ping bastam — o app não é a sala.
#[derive(Debug, Clone, Serialize)]
pub struct Estado {
    pub pessoas: usize,
    pub transmitindo: usize,
    pub ping_ms: i64,
}

// ---------------------------------------------------------------------------
// Conexão
// ---------------------------------------------------------------------------

/// Eventos que o laço de leitura entrega a quem está usando a sala.
pub enum Evento {
    Estado(Estado),
    /// A conexão caiu. O motivo já vem legível para mostrar na interface.
    ///
    /// Isto significa "tente voltar": é o serviço reiniciando, a rede
    /// oscilando. Quem trata dispara a reconexão.
    Caiu(String),
    /// A sala RECUSOU a credencial. Não é queda, e reconectar não resolve.
    ///
    /// # Por que é um evento separado
    ///
    /// Porque era `Caiu`, e isso criou um laço infinito. O tratamento de queda
    /// dispara uma reconexão; a reconexão tentava entrar; a sala recusava de
    /// novo; a recusa virava outro `Caiu`. Uma tentativa a cada 2 s, para
    /// sempre — o bastante para o app BLOQUEAR o próprio IP no limitador do
    /// servidor e se manter bloqueado sozinho, renovando o castigo a cada vez
    /// que ele expirava.
    ///
    /// O conserto anterior (0.7.7) fez a reconexão parar de insistir depois de
    /// uma recusa, e isso funcionava — o log dizia "não insisto" a cada volta.
    /// Só que o laço não estava lá dentro: estava aqui, recriando a reconexão
    /// do zero. Parar o laço de dentro não adianta quando quem o recria é o de
    /// fora.
    Recusado(String),
}

/// Que evento anuncia o fim do leitor.
///
/// # Por que é uma função e não um `if` no meio do laço
///
/// Porque o `if` no meio do laço foi onde o erro morou, e ali não dá para
/// testar. A regra é curta e o preço de errá-la é alto: recusa que vira
/// [`Evento::Caiu`] faz quem trata disparar uma reconexão, a reconexão é
/// recusada de novo, e o app entra num laço que bloqueia o próprio IP no
/// limitador do servidor — indefinidamente, renovando o castigo a cada vez que
/// ele expira.
fn fim_do_leitor(recusado: bool, motivo: String) -> Evento {
    if recusado {
        // O ramo do `denied` já consumiu o canal de entrada, então sem esta
        // distinção TODA recusa chegaria aqui como se fosse queda.
        Evento::Recusado(motivo)
    } else {
        Evento::Caiu(motivo)
    }
}

/// Uma sessão viva na sala.
pub struct Sala {
    /// Fila de saída. Escrever aqui nunca bloqueia a thread da UI.
    saida: UnboundedSender<Message>,
    /// `stream_ready` chega assíncrono depois do `publish_start`; este canal é
    /// onde `espera_whip` o pesca.
    whip_rx: Mutex<Receiver<String>>,
    viva: Arc<AtomicBool>,
    /// Autoriza os downloads enquanto a sessão durar.
    token: String,
}

/// Deriva a URL do WebSocket da base pública (`https://…` → `wss://…/ws`).
fn url_ws(base: &str) -> String {
    let base = base.trim_end_matches('/');
    let ws = if let Some(resto) = base.strip_prefix("https://") {
        format!("wss://{resto}")
    } else if let Some(resto) = base.strip_prefix("http://") {
        format!("ws://{resto}")
    } else {
        // Sem esquema: assume TLS, que é o caso de produção.
        format!("wss://{base}")
    };
    format!("{ws}/ws")
}

impl Sala {
    /// Conecta, manda `hello` e espera o `welcome`.
    ///
    /// Bloqueia até a sala responder porque é exatamente isso que a tela de
    /// entrada precisa: ou entrou, ou tem um motivo para mostrar. Um retorno
    /// otimista aqui viraria uma tela de transmissão que não transmite.
    pub fn entra(
        rt: &Arc<Runtime>,
        base: &str,
        senha: &str,
        apelido: &str,
        mut ao_evento: impl FnMut(Evento) + Send + 'static,
    ) -> Result<Self> {
        let url = url_ws(base);
        let (ws, _) = rt
            .block_on(tokio_tungstenite::connect_async(&url))
            .with_context(|| format!("conectar em {url}"))?;

        let (mut sink, mut stream) = ws.split();
        let (saida, mut saida_rx) = tokio::sync::mpsc::unbounded_channel::<Message>();

        // Escritor: uma única tarefa é dona do sink, então nada mais precisa
        // sincronizar escrita.
        rt.spawn(async move {
            while let Some(msg) = saida_rx.recv().await {
                if sink.send(msg).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        envia(
            &saida,
            "hello",
            &Hello {
                password: senha,
                nick: apelido,
                versao: env!("CARGO_PKG_VERSION"),
            },
        )?;

        // Canais para a thread chamadora: `welcome`/`denied` chegam uma vez,
        // `stream_ready` uma vez por transmissão.
        let (entrada_tx, entrada_rx) = std::sync::mpsc::sync_channel::<Result<Welcome>>(1);
        let (whip_tx, whip_rx) = std::sync::mpsc::sync_channel::<String>(1);

        let viva = Arc::new(AtomicBool::new(true));
        let viva_leitor = Arc::clone(&viva);
        let saida_leitor = saida.clone();
        let mut self_id = String::new();
        let mut entrada_tx = Some(entrada_tx);
        // A sala recusou a credencial? Decide, no fim do leitor, entre
        // `Evento::Recusado` e `Evento::Caiu` — e é essa distinção que impede
        // o laço de reconexão infinito.
        let mut recusado = false;

        rt.spawn(async move {
            let motivo = loop {
                let Some(item) = stream.next().await else {
                    break "o servidor encerrou a conexão".to_string();
                };
                let msg = match item {
                    Ok(m) => m,
                    Err(e) => break format!("conexão perdida: {e}"),
                };
                let texto = match msg {
                    Message::Text(t) => t.to_string(),
                    Message::Close(_) => break "o servidor encerrou a conexão".to_string(),
                    // Ping/Pong do próprio WebSocket a tungstenite responde
                    // sozinha; binário a sala não usa.
                    _ => continue,
                };
                let Ok(env) = serde_json::from_str::<Envelope>(&texto) else {
                    continue;
                };

                match env.tipo.as_str() {
                    "welcome" => match serde_json::from_value::<Welcome>(env.data.clone()) {
                        Ok(w) => {
                            self_id = w.self_id.clone();
                            if let Some(tx) = entrada_tx.take() {
                                let _ = tx.send(Ok(w.clone()));
                            }
                            if let Some(e) = estado_de(&env.data, &self_id) {
                                ao_evento(Evento::Estado(e));
                            }
                        }
                        Err(e) => {
                            if let Some(tx) = entrada_tx.take() {
                                let _ = tx.send(Err(anyhow::anyhow!(
                                    "welcome ilegível do servidor: {e}"
                                )));
                            }
                        }
                    },
                    "denied" => {
                        recusado = true;
                        let motivo = env
                            .data
                            .get("reason")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("acesso negado")
                            .to_string();
                        let codigo = env
                            .data
                            .get("codigo")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        if let Some(tx) = entrada_tx.take() {
                            let _ = tx.send(Err(anyhow::Error::new(Recusa {
                                codigo,
                                motivo: motivo.clone(),
                            })));
                        }
                        break motivo;
                    }
                    "state" => {
                        if let Some(e) = estado_de(&env.data, &self_id) {
                            ao_evento(Evento::Estado(e));
                        }
                    }
                    // É assim que o servidor mede o RTT de todo mundo — o
                    // indicador de ping da sala inteira depende deste eco.
                    // Não responder deixa o grupo achando que a conexão de
                    // quem transmite está ruim.
                    "ping" => {
                        let t = env
                            .data
                            .get("t")
                            .and_then(serde_json::Value::as_i64)
                            .unwrap_or(0);
                        if envia(&saida_leitor, "pong", &Pong { t }).is_err() {
                            break "não consegui responder ao ping".to_string();
                        }
                    }
                    "stream_ready" => {
                        if let Ok(s) = serde_json::from_value::<StreamReady>(env.data) {
                            let _ = whip_tx.try_send(s.whip_url);
                        }
                    }
                    _ => {}
                }
            };

            viva_leitor.store(false, Ordering::Relaxed);
            // Se a queda veio antes do `welcome`, quem espera na entrada
            // precisa ser solto com um motivo em vez de estourar o timeout.
            if let Some(tx) = entrada_tx.take() {
                let _ = tx.send(Err(anyhow::anyhow!("{motivo}")));
            } else {
                ao_evento(fim_do_leitor(recusado, motivo));
            }
        });

        // O que importa aqui é que o `welcome` CHEGOU: enquanto não chega, não
        // se sabe se a senha valeu. Do conteúdo, só o `token` sobrevive — ele
        // autoriza baixar o encoder e as atualizações.
        let bem_vindo = entrada_rx
            .recv_timeout(ESPERA_ENTRADA)
            .context("o servidor não respondeu ao entrar")??;

        Ok(Self {
            saida,
            whip_rx: Mutex::new(whip_rx),
            viva,
            token: bem_vindo.token,
        })
    }

    /// Token desta sessão, para os downloads protegidos.
    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// A conexão ainda está de pé.
    pub fn viva(&self) -> bool {
        self.viva.load(Ordering::Relaxed)
    }

    /// Anuncia a transmissão e devolve a URL WHIP que o servidor reservou.
    pub fn publica(&self, tipo_fonte: &str, titulo: &str, settings: Settings) -> Result<String> {
        // Drena sobras de uma transmissão anterior: um `stream_ready` velho
        // apontaria para um path já encerrado, e o sintoma seria uma
        // publicação que conecta e ninguém vê.
        if let Ok(rx) = self.whip_rx.lock() {
            while rx.try_recv().is_ok() {}
        }

        envia(
            &self.saida,
            "publish_start",
            &PublishStart {
                source_type: tipo_fonte,
                source_title: titulo,
                settings,
            },
        )?;

        let rx = self
            .whip_rx
            .lock()
            .map_err(|_| anyhow::anyhow!("canal do stream_ready envenenado"))?;
        rx.recv_timeout(ESPERA_STREAM)
            .context("o servidor não devolveu o endereço de publicação")
    }

    /// Telemetria periódica, que a sala mostra no rodapé de cada tela.
    pub fn estatisticas(&self, kbps: i32, fps: i32, dropped: i32, w: i32, h: i32) -> Result<()> {
        envia(
            &self.saida,
            "publish_stats",
            &PublishStats {
                kbps,
                fps,
                dropped,
                w,
                h,
            },
        )
    }

    pub fn para(&self) -> Result<()> {
        envia(&self.saida, "publish_stop", &serde_json::json!({}))
    }
}

impl Drop for Sala {
    fn drop(&mut self) {
        // Fecha educadamente: sem isto o servidor só descobre a saída quando o
        // TCP expira, e o apelido fica pendurado na lista de participantes.
        let _ = self.saida.send(Message::Close(None));
    }
}

/// Serializa `{type, data}` e enfileira para envio.
fn envia<T: Serialize>(saida: &UnboundedSender<Message>, tipo: &str, data: &T) -> Result<()> {
    let env = serde_json::json!({ "type": tipo, "data": data });
    let texto = serde_json::to_string(&env).context("serializar mensagem")?;
    saida
        .send(Message::Text(texto.into()))
        .map_err(|_| anyhow::anyhow!("conexão com a sala já encerrada"))
}

/// Extrai do `state`/`welcome` só o que o app mostra.
fn estado_de(data: &serde_json::Value, self_id: &str) -> Option<Estado> {
    let participantes: Vec<Participante> =
        serde_json::from_value(data.get("participants")?.clone()).ok()?;
    let transmitindo = participantes
        .iter()
        .filter(|p| !p.sharing.is_empty())
        .count();
    let ping_ms = participantes
        .iter()
        .find(|p| p.id == self_id)
        .map_or(0, |p| p.ping_ms);
    Some(Estado {
        pessoas: participantes.len(),
        transmitindo,
        ping_ms,
    })
}

#[cfg(test)]
mod testes {
    use super::*;

    #[test]
    fn url_ws_deriva_esquema() {
        assert_eq!(
            url_ws("https://tela.example.com"),
            "wss://tela.example.com/ws"
        );
        assert_eq!(
            url_ws("https://tela.example.com/"),
            "wss://tela.example.com/ws"
        );
        assert_eq!(url_ws("http://127.0.0.1:8090"), "ws://127.0.0.1:8090/ws");
        assert_eq!(
            url_ws("tela.example.com"),
            "wss://tela.example.com/ws"
        );
    }

    #[test]
    fn estado_pega_o_meu_ping() {
        let data = serde_json::json!({
            "participants": [
                {"id": "a", "nick": "ana", "pingMs": 31, "sharing": "s1"},
                {"id": "b", "nick": "eu", "pingMs": 12, "sharing": ""}
            ]
        });
        let e = estado_de(&data, "b").expect("estado");
        assert_eq!(e.pessoas, 2);
        assert_eq!(e.transmitindo, 1);
        assert_eq!(e.ping_ms, 12);
    }
}

/// A sala recusou a entrada, com o código do motivo.
///
/// O código vem do servidor (`DeniedData.Codigo`) e existe para esta decisão
/// não depender de ler a frase em português: reescrever uma mensagem passaria a
/// mudar comportamento sem ninguém notar.
#[derive(Debug)]
pub struct Recusa {
    pub codigo: String,
    pub motivo: String,
}

impl std::fmt::Display for Recusa {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // O texto continua idêntico ao de antes: quem só mostra a mensagem não
        // percebe que o erro virou tipado.
        f.write_str(&self.motivo)
    }
}

impl std::error::Error for Recusa {}

/// Insistir nesta recusa pode dar certo mais tarde?
///
/// # Por que isto existe
///
/// A reconexão tratava toda falha igual e tentava seis vezes. Quando a senha da
/// sala mudou, isso viraram seis tentativas com a credencial errada — o
/// bastante para estourar o limitador do servidor (cinco falhas) e bloquear o
/// IP da pessoa por quinze minutos. Ela ficava trancada para fora mesmo já
/// tendo a senha nova em mãos.
///
/// Só "sala cheia" melhora sozinho com o tempo. Credencial recusada, apelido
/// inválido e bloqueio não melhoram tentando — bloqueio, aliás, piora.
/// A sala recusou a SENHA — e não outra coisa (bloqueio, sala cheia)?
///
/// Distinção que importa: em "tentativas demais" a senha pode estar certa, e
/// esquecê-la por isso castigaria a pessoa duas vezes.
#[must_use]
pub fn e_senha_recusada(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Recusa>()
        .is_some_and(|r| r.codigo == "senha")
}

#[must_use]
pub fn vale_tentar_de_novo(e: &anyhow::Error) -> bool {
    match e.downcast_ref::<Recusa>() {
        // Sem código: recusa de uma versão antiga do servidor, ou falha que
        // nem chegou a ser recusa (rede). Mantém o comportamento de tentar.
        None => true,
        Some(r) => !matches!(
            r.codigo.as_str(),
            "senha" | "convite" | "apelido" | "bloqueio" | "revogado"
        ),
    }
}

#[cfg(test)]
mod testes_recusa {
    use super::{e_senha_recusada, vale_tentar_de_novo, Recusa};

    fn recusa(codigo: &str) -> anyhow::Error {
        anyhow::Error::new(Recusa {
            codigo: codigo.to_string(),
            motivo: "qualquer".into(),
        })
    }

    /// O defeito que trancou gente de casa para fora.
    ///
    /// Depois da troca de senha, a reconexão tentava seis vezes com a
    /// credencial errada — cinco falhas já bloqueiam o IP por quinze minutos no
    /// servidor. A pessoa ficava de fora mesmo tendo a senha nova em mãos.
    #[test]
    fn credencial_recusada_nao_se_tenta_de_novo() {
        for codigo in ["senha", "convite", "apelido", "revogado"] {
            assert!(
                !vale_tentar_de_novo(&recusa(codigo)),
                "insistir em {codigo} só queima o limitador do servidor"
            );
        }
    }

    /// Insistir em bloqueio é o pior caso: cada tentativa RENOVA o bloqueio.
    #[test]
    fn bloqueio_nao_se_tenta_de_novo() {
        assert!(!vale_tentar_de_novo(&recusa("bloqueio")));
    }

    /// Sala cheia melhora sozinha quando alguém sai.
    #[test]
    fn sala_cheia_vale_tentar() {
        assert!(vale_tentar_de_novo(&recusa("cheia")));
    }

    /// Falha que não é recusa — rede caindo, servidor reiniciando — é
    /// exatamente o caso para o qual a reconexão existe.
    #[test]
    fn falha_de_rede_vale_tentar() {
        assert!(vale_tentar_de_novo(&anyhow::anyhow!("conexão recusada")));
    }

    /// Servidor antigo não manda código. Na dúvida, tenta — que é o
    /// comportamento anterior, e o pior caso é o de hoje, não um novo.
    #[test]
    fn recusa_sem_codigo_vale_tentar() {
        assert!(vale_tentar_de_novo(&recusa("")));
        assert!(vale_tentar_de_novo(&recusa("motivo-que-nao-conheco")));
    }

    /// Só a recusa de SENHA pode fazer o app esquecer a senha guardada.
    ///
    /// Em "tentativas demais" a senha guardada pode estar certa — quem
    /// bloqueou foi o limitador, não a credencial. Esquecê-la aí castigaria a
    /// pessoa duas vezes: bloqueada E sem a senha para quando o bloqueio
    /// acabar.
    #[test]
    fn so_recusa_de_senha_autoriza_esquecer() {
        assert!(e_senha_recusada(&recusa("senha")));
        for outro in ["bloqueio", "cheia", "apelido", "convite", "revogado", ""] {
            assert!(
                !e_senha_recusada(&recusa(outro)),
                "{outro:?} não prova que a senha guardada caducou"
            );
        }
    }

    /// Falha de rede não é recusa de nada.
    #[test]
    fn falha_de_rede_nao_apaga_senha() {
        assert!(!e_senha_recusada(&anyhow::anyhow!("senha incorreta")));
    }

    /// A mensagem que a pessoa lê não pode mudar por causa disto.
    #[test]
    fn o_texto_para_quem_le_continua_o_motivo() {
        let e = anyhow::Error::new(Recusa {
            codigo: "senha".into(),
            motivo: "senha incorreta".into(),
        });
        assert_eq!(format!("{e}"), "senha incorreta");
    }
}

#[cfg(test)]
mod testes_fim_do_leitor {
    use super::{fim_do_leitor, Evento};

    /// O defeito que trancou alguém para fora do próprio app.
    ///
    /// A sala recusava a credencial, o leitor anunciava `Caiu`, quem tratava
    /// disparava reconexão, a reconexão era recusada — e virava outro `Caiu`.
    /// Uma tentativa a cada 2 s, para sempre. Cinco delas em dez segundos são
    /// exatamente o limite do anti-força-bruta do servidor, então o app
    /// bloqueava o próprio IP e renovava o bloqueio sozinho a cada expiração.
    ///
    /// O conserto anterior fez a RECONEXÃO parar de insistir, e o log até
    /// dizia "não insisto" a cada volta. Não adiantou: o laço não estava lá
    /// dentro, estava aqui, recriando a reconexão do zero.
    #[test]
    fn recusa_nunca_vira_queda() {
        let e = fim_do_leitor(true, "senha incorreta".into());
        assert!(
            matches!(e, Evento::Recusado(_)),
            "recusa anunciada como queda faz o app se rebloquear em laço"
        );
    }

    /// Queda de verdade continua pedindo reconexão — é para isso que ela existe.
    #[test]
    fn queda_continua_sendo_queda() {
        let e = fim_do_leitor(false, "conexão perdida".into());
        assert!(matches!(e, Evento::Caiu(_)));
    }

    /// O motivo chega inteiro nos dois casos: é o que a pessoa lê na tela.
    #[test]
    fn o_motivo_atravessa_intacto() {
        match fim_do_leitor(true, "tentativas demais".into()) {
            Evento::Recusado(m) => assert_eq!(m, "tentativas demais"),
            _ => panic!("evento errado"),
        }
        match fim_do_leitor(false, "servidor reiniciando".into()) {
            Evento::Caiu(m) => assert_eq!(m, "servidor reiniciando"),
            _ => panic!("evento errado"),
        }
    }
}
