//! Arquivo de log do app.
//!
//! # Por que existe
//!
//! O app é GUI: no Windows, um binário de subsistema Windows não tem console, e
//! todo `eprintln!` — inclusive o `stderr` do ffmpeg — cai no vazio. Quando um
//! amigo diz "não funciona aqui", não há absolutamente nada para olhar.
//!
//! Sem isto o diagnóstico vira adivinhação a distância: pedir para tentar de
//! novo, mudar uma configuração, tentar outra vez. Com o arquivo, ele manda um
//! texto e o problema aparece.
//!
//! # Cuidado com o que entra aqui
//!
//! O arquivo vai ser enviado por chat quando alguém pedir ajuda. **Nada de
//! senha, token ou URL com token.** As funções abaixo mascaram o que passa por
//! elas; qualquer chamada nova precisa manter essa disciplina.

use std::fmt::Write as _;
use std::io::{Seek as _, Write as _};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Quantas linhas o arquivo guarda antes de recomeçar.
///
/// Uma transmissão longa gera bastante linha. O teto evita que o arquivo cresça
/// sem limite no disco de quem só quer usar o programa.
///
/// # Recomeçar, e não parar
///
/// Ao bater no teto o registro RECOMEÇA o arquivo. A versão anterior parava de
/// escrever de vez, e parar guarda o COMEÇO — que é justamente a parte que não
/// interessa. O fim é o que responde "o que aconteceu agora".
///
/// Custou uma conclusão errada e afirmada duas vezes: com o app aberto há
/// horas, o arquivo travou às 03:21 e isso foi lido como "parou de transmitir
/// às 03:21", quando era "parou de gravar às 03:21".
///
/// Pior, o `return` acontecia antes de enfileirar a linha para o servidor,
/// então o log remoto emudecia junto — as duas cópias cegas ao mesmo tempo.
///
/// A 1 linha de telemetria a cada 2 s, 4000 linhas são ~2h de transmissão. O
/// servidor já tomava a decisão certa para o arquivo dele: "recomeça em vez de
/// rodar arquivos: o interessante é sempre o fim" (`sala/log.go`).
const MAX_LINHAS: usize = 4000;

static ARQUIVO: Mutex<Option<std::fs::File>> = Mutex::new(None);
static LINHAS: Mutex<usize> = Mutex::new(0);
/// Onde o log está sendo gravado de fato.
static ONDE: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Linhas ainda não enviadas ao servidor.
///
/// Fila e não envio direto: gravar uma linha não pode esperar a rede, e
/// disparar uma requisição por linha inundaria o servidor. Quem esvazia é o
/// `remoto`, de tempos em tempos.
static PENDENTES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Teto da fila, para o caso de nunca haver quem a esvazie.
///
/// Sem isto, alguém que nunca entra na sala acumularia linhas até o fim da
/// memória. Ao estourar, as mais antigas caem — o fim é o que interessa.
const MAX_PENDENTES: usize = 3000;

/// Tira as linhas acumuladas para envio.
pub fn drena() -> Vec<String> {
    match PENDENTES.lock() {
        Ok(mut p) => std::mem::take(&mut *p),
        Err(_) => Vec::new(),
    }
}

/// Devolve à fila linhas que não puderam ser enviadas.
///
/// Sem devolver, uma queda de rede engoliria justamente o trecho em que algo
/// deu errado — que é o único trecho que interessa.
pub fn devolve(mut linhas: Vec<String>) {
    if let Ok(mut p) = PENDENTES.lock() {
        linhas.extend(p.drain(..));
        *p = linhas;
        let excesso = p.len().saturating_sub(MAX_PENDENTES);
        if excesso > 0 {
            p.drain(..excesso);
        }
    }
}

/// Onde o log mora. Fica junto da configuração para quem for pedir ajuda
/// encontrar os dois no mesmo lugar.
#[must_use]
pub fn caminho(dir_config: &std::path::Path) -> PathBuf {
    dir_config.join("tela.log")
}

/// Onde o log está sendo gravado de fato, que pode não ser onde se pediu.
#[must_use]
pub fn onde() -> Option<PathBuf> {
    ONDE.lock().ok().and_then(|o| o.clone())
}

/// Caminho do log da execução ANTERIOR.
#[must_use]
pub fn caminho_anterior(dir_config: &std::path::Path) -> PathBuf {
    dir_config.join("tela-anterior.log")
}

/// Abre (recriando) o arquivo de log. Chamado uma vez, na partida.
///
/// Recria em vez de acrescentar: o que interessa é a execução atual, e um
/// arquivo que só cresce acaba com o histórico de meses misturado.
///
/// Mas a execução anterior é guardada ao lado, e isso não é luxo: quando o
/// app trava ou fecha sozinho, a primeira coisa que a pessoa faz é reabrir —
/// e era exatamente aí que o log do problema era destruído. Aconteceu: o
/// registro de um congelamento se perdeu porque o app foi reaberto antes de
/// alguém pensar em copiar o arquivo.
pub fn inicia(caminho: &std::path::Path) {
    if let Some(pai) = caminho.parent() {
        let _ = std::fs::create_dir_all(pai);
        let _ = std::fs::rename(caminho, caminho_anterior(pai));
    }
    // Se o arquivo não abrir, o app fica MUDO — e foi assim que uma sessão
    // inteira de teste rodou sem deixar rastro, com o motivo de um recuo
    // perdido para sempre. Então há um segundo lugar, e a falha vai para o
    // stderr, que aparece para quem roda pelo console.
    let escolhido = match std::fs::File::create(caminho) {
        Ok(f) => Some((f, caminho.to_path_buf())),
        Err(e) => {
            eprintln!("não consegui gravar o log em {}: {e}", caminho.display());
            let alternativo = std::env::temp_dir().join("tela.log");
            match std::fs::File::create(&alternativo) {
                Ok(f) => {
                    eprintln!("gravando em {} no lugar", alternativo.display());
                    Some((f, alternativo))
                }
                Err(e) => {
                    eprintln!("nem em {}: {e}", alternativo.display());
                    None
                }
            }
        }
    };
    if let Some((f, onde)) = escolhido {
        if let Ok(mut guard) = ARQUIVO.lock() {
            *guard = Some(f);
        }
        if let Ok(mut n) = LINHAS.lock() {
            *n = 0;
        }
        if let Ok(mut o) = ONDE.lock() {
            *o = Some(onde);
        }
    }
    linha(&format!(
        "tela {} — {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS
    ));
}

/// Escreve uma linha, com carimbo de tempo relativo ao início do processo.
pub fn linha(texto: &str) {
    let seg = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() % 86400)
        .unwrap_or(0);
    let mut buf = String::new();
    let _ = write!(
        buf,
        "[{:02}:{:02}:{:02}] {}",
        seg / 3600,
        (seg % 3600) / 60,
        seg % 60,
        mascara(texto)
    );

    // Console também, para quem rodar pelo terminal durante o desenvolvimento.
    eprintln!("{buf}");

    let Ok(mut n) = LINHAS.lock() else { return };
    *n += 1;
    let recomecar = *n > MAX_LINHAS;
    if recomecar {
        *n = 1;
    }

    if let Ok(mut guard) = ARQUIVO.lock() {
        if let Some(f) = guard.as_mut() {
            if recomecar {
                // `set_len` sozinho nao basta: o arquivo e aberto com
                // `File::create`, sem `append`, entao o cursor continua no fim
                // e a proxima escrita deixaria um buraco de zeros antes dela.
                let _ = f.set_len(0);
                let _ = f.seek(std::io::SeekFrom::Start(0));
                let _ = writeln!(f, "[log recomecado: {MAX_LINHAS} linhas]");
            }
            let _ = writeln!(f, "{buf}");
            let _ = f.flush();
        }
    }

    // A mesma linha, já mascarada, entra na fila do envio.
    if let Ok(mut p) = PENDENTES.lock() {
        if p.len() < MAX_PENDENTES {
            p.push(buf);
        }
    }
}

/// Remove segredos de uma linha antes de gravar.
///
/// O `token=` aparece nas URLs de WHIP e no download; a senha nunca deveria
/// passar por aqui, mas mascarar é mais barato que confiar.
#[must_use]
pub fn mascara(texto: &str) -> String {
    let mut out = String::with_capacity(texto.len());
    let mut resto = texto;
    while let Some(i) = resto.find("token=") {
        out.push_str(&resto[..i + 6]);
        out.push_str("<oculto>");
        let depois = &resto[i + 6..];
        // Vai até o fim do valor: o que corta um parâmetro de query ou a linha.
        let fim = depois.find(['&', ' ', '"', '\'']).unwrap_or(depois.len());
        resto = &depois[fim..];
    }
    out.push_str(resto);
    out
}

#[cfg(test)]
mod testes {
    use super::*;

    #[test]
    fn mascara_esconde_token_em_url() {
        let m = mascara("POST https://tela.example.com/tela/abc/whip?token=deadbeef1234 ok");
        assert!(!m.contains("deadbeef"), "token vazou: {m}");
        assert!(m.contains("token=<oculto>"));
        assert!(
            m.contains("/tela/abc/whip"),
            "resto da URL deveria continuar"
        );
        assert!(m.ends_with(" ok"), "fim da linha deveria continuar: {m}");
    }

    #[test]
    fn mascara_esconde_varios_tokens() {
        let m = mascara("a token=111&b=2 e token=222 fim");
        assert!(!m.contains("111") && !m.contains("222"), "vazou: {m}");
        assert_eq!(m.matches("<oculto>").count(), 2);
    }

    #[test]
    fn texto_sem_token_passa_intacto() {
        let t = "ffmpeg: Guessed Channel Layout: stereo";
        assert_eq!(mascara(t), t);
    }
}

/// Trava unica dos testes que mexem no registro.
///
/// `ARQUIVO` e `LINHAS` sao estaticos do processo, e `cargo test` roda os
/// testes em paralelo dentro do MESMO binario. Cada modulo com a sua propria
/// trava nao trava nada: `testes_rotacao` chamava `inicia` — que zera os dois —
/// no meio de um teste de outro modulo, e o resultado dependia da ordem.
///
/// Uma trava so, no modulo pai, e o que serializa de verdade.
#[cfg(test)]
static TRAVA_REGISTRO: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod testes_rotacao {
    use super::*;

    /// A execução anterior tem de sobreviver a uma abertura nova.
    ///
    /// Sem isto, reabrir o app depois de um travamento apaga a única prova do
    /// que aconteceu — que é a hora em que a pessoa mais reabre o app.
    #[test]
    fn abrir_de_novo_preserva_o_log_anterior() {
        let _trava = TRAVA_REGISTRO.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join("tela-log-rotacao");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let atual = caminho(&dir);

        std::fs::write(&atual, b"o que aconteceu antes do travamento").unwrap();
        inicia(&atual);

        assert_eq!(
            std::fs::read(caminho_anterior(&dir)).unwrap(),
            b"o que aconteceu antes do travamento",
            "o log anterior tinha de ter sido preservado"
        );
        let novo = std::fs::read_to_string(&atual).unwrap();
        assert!(
            novo.contains("tela "),
            "o log novo devia começar do zero: {novo:?}"
        );
        assert!(!novo.contains("travamento"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod testes_teto_de_linhas {
    use super::*;

    /// Bater no teto tem de RECOMECAR, nunca parar.
    ///
    /// # O que quebrou
    ///
    /// A versao anterior parava de escrever de vez ao atingir `MAX_LINHAS`.
    /// Guardar o comeco e o contrario do que se quer: o fim e o que responde
    /// "o que aconteceu agora". Com o app aberto ha horas, o arquivo de alguem
    /// congelou as 03:21 e isso foi lido como "parou de transmitir as 03:21",
    /// quando era "parou de gravar as 03:21" — e a conclusao errada foi
    /// afirmada duas vezes antes de alguem perceber.
    #[test]
    fn ao_bater_no_teto_o_log_recomeca_em_vez_de_parar() {
        let _trava = TRAVA_REGISTRO.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join("tela-log-teto");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let atual = caminho(&dir);
        inicia(&atual);

        // Uma volta inteira e mais um punhado, para passar do teto.
        for i in 0..(MAX_LINHAS + 50) {
            linha(&format!("evento numero {i}"));
        }

        let conteudo = std::fs::read_to_string(&atual).unwrap();
        assert!(
            conteudo.contains(&format!("evento numero {}", MAX_LINHAS + 49)),
            "o ULTIMO evento tinha de estar no arquivo — perder o fim foi o defeito"
        );
        assert!(
            !conteudo.contains(
                "evento numero 0
"
            ),
            "o comeco devia ter saido para o fim caber"
        );
        assert!(
            conteudo.contains("log recomecado"),
            "o recomeco tem de ficar explicito, senao some tempo sem explicacao"
        );
        // E nao pode ter virado arquivo esparso cheio de zeros.
        assert!(
            !conteudo.contains('\0'),
            "truncar sem voltar o cursor deixa um buraco de zeros"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// O envio para o servidor nao pode emudecer junto.
    ///
    /// O `return` do teto acontecia ANTES de enfileirar, entao as duas copias
    /// do log — a local e a do servidor — cegavam ao mesmo tempo.
    #[test]
    fn depois_do_teto_as_linhas_continuam_indo_para_o_servidor() {
        let _trava = TRAVA_REGISTRO.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join("tela-log-teto-fila");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        inicia(&caminho(&dir));

        for i in 0..(MAX_LINHAS + 5) {
            linha(&format!("evento {i}"));
        }
        let pendentes = PENDENTES.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            !pendentes.is_empty(),
            "depois do teto nada mais era enfileirado para o servidor"
        );
        drop(pendentes);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod testes_fila {
    use super::*;

    #[test]
    fn drena_leva_o_que_foi_registrado_e_esvazia() {
        let _g = TRAVA_REGISTRO.lock().unwrap_or_else(|e| e.into_inner());
        let _ = drena();

        linha("primeira");
        linha("segunda");
        let levadas = drena();

        assert!(levadas.iter().any(|l| l.contains("primeira")));
        assert!(levadas.iter().any(|l| l.contains("segunda")));
        assert!(drena().is_empty(), "a fila tinha de ter ficado vazia");
    }

    /// Falhou o envio, as linhas voltam — e voltam ANTES do que veio depois,
    /// senão o log do servidor sai fora de ordem justamente no trecho em que
    /// a rede estava ruim.
    #[test]
    fn devolve_recoloca_na_frente() {
        let _g = TRAVA_REGISTRO.lock().unwrap_or_else(|e| e.into_inner());
        let _ = drena();

        linha("veio depois");
        devolve(vec!["[00:00:00] falhou o envio".to_string()]);

        let fila = drena();
        let pos_falha = fila.iter().position(|l| l.contains("falhou o envio"));
        let pos_depois = fila.iter().position(|l| l.contains("veio depois"));
        assert!(pos_falha < pos_depois, "ordem trocada: {fila:?}");
    }

    /// O que entra na fila já vem mascarado — é a mesma string gravada no
    /// arquivo. Sem isso, um token viajaria para o servidor.
    #[test]
    fn a_fila_nao_carrega_token() {
        let _g = TRAVA_REGISTRO.lock().unwrap_or_else(|e| e.into_inner());
        let _ = drena();

        linha("publicando em https://exemplo/whip?token=abcdef123456");
        let fila = drena();
        let tudo = fila.join("\n");
        assert!(!tudo.contains("abcdef123456"), "token vazou: {tudo}");
    }
}
