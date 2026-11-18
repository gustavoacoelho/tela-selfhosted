//! Envio do log para o servidor.
//!
//! # Por que
//!
//! Depurar na máquina dos outros por telefone não funciona: quem está com o
//! problema é justamente quem não vai achar o arquivo, e reabrir o app depois
//! de um travamento apagava a única prova do que houve. Com o log chegando
//! sozinho, o problema de qualquer um fica visível de onde dá para investigar.
//!
//! O arquivo local continua: ele funciona sem rede e antes de entrar na sala,
//! que é onde metade dos problemas de configuração aparece.
//!
//! # O que vai junto
//!
//! Só o que já estava no arquivo, e ele passa por `registro::mascara` antes de
//! ser gravado — nada de senha nem de token. Ainda assim vão nomes de janela e
//! de processo, então isto vale entre amigos que sabem que existe, não como
//! coleta silenciosa.

use std::time::Duration;

use crate::registro;

/// De quanto em quanto tempo a fila é esvaziada.
///
/// Frouxo de propósito: o log não é telemetria ao vivo e o servidor é uma VM
/// pequena e compartilhada. Cinco segundos deixam a investigação em ritmo
/// humano sem virar uma requisição por linha.
const INTERVALO: Duration = Duration::from_secs(5);

/// Começa o envio periódico em segundo plano.
///
/// `pega_token` é consultado a cada rodada em vez de receber o token uma vez:
/// ele só existe depois de entrar na sala, muda a cada entrada, e morre junto
/// com a conexão.
pub fn inicia<F>(servidor: String, pega_token: F)
where
    F: Fn() -> Option<String> + Send + 'static,
{
    std::thread::spawn(move || {
        let cliente = match reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                registro::linha(&format!("envio de log indisponível: {e}"));
                return;
            }
        };
        let url_base = format!("{}/log", servidor.trim_end_matches('/'));

        loop {
            std::thread::sleep(INTERVALO);
            let Some(token) = pega_token() else {
                continue;
            };
            let linhas = registro::drena();
            if linhas.is_empty() {
                continue;
            }
            let corpo = linhas.join("\n") + "\n";
            match cliente
                .post(format!("{url_base}?token={token}"))
                .body(corpo)
                .send()
            {
                Ok(r) if r.status().is_success() => {}
                // Devolve à fila: perder justo o trecho em que a rede falhou
                // seria perder o que interessa. Sem `registro::linha` aqui —
                // uma falha de envio que gera linha nova daria um laço.
                Ok(_) | Err(_) => registro::devolve(linhas),
            }
        }
    });
}
