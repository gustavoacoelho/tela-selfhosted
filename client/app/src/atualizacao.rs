//! Verificação de versão, download do encoder e auto-atualização.
//!
//! # Por que existe
//!
//! Durante o desenvolvimento saíram oito versões num dia, e a cada uma alguém
//! precisava avisar cada pessoa para baixar de novo. O atrito da correção
//! passou a ser maior que a correção.
//!
//! # Por que o ffmpeg é baixado em vez de embutido
//!
//! Ele tem 140 MB dos 160 do executável e quase nunca muda. Embutido, cada
//! correção de dez linhas custava 160 MB de download para cada pessoa. Baixado
//! uma vez e guardado em cache, uma atualização passa a custar ~20 MB.
//!
//! # Por que não instala sozinho
//!
//! Um build com defeito se espalharia sem ninguém ter chance de segurar — e
//! dois dos oito builds daquele dia tinham defeito. O app avisa e atualiza com
//! um clique; a decisão continua sendo de quem usa.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::registro;

/// O que o servidor publica em `/download/versao.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifesto {
    pub versao: String,
    pub app: Arquivo,
    pub ffmpeg: Arquivo,
    /// DLLs da libavcodec, para o encoder que recebe a textura direto da GPU.
    ///
    /// Lista e não um `.zip` porque o download já verifica SHA-256 por arquivo
    /// e não há descompactador no app — três downloads custam menos que uma
    /// dependência nova.
    ///
    /// Ausente ou vazia é normal: um servidor que ainda não as publicou deixa
    /// todo mundo no caminho do `ffmpeg.exe`, que é o de sempre.
    #[serde(default)]
    pub libav: Vec<Arquivo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Arquivo {
    pub arquivo: String,
    pub bytes: u64,
    pub sha256: String,
}

/// O que a interface precisa saber sobre atualização.
#[derive(Debug, Clone, Serialize, Default)]
pub struct Situacao {
    /// Versão publicada, quando foi possível consultar.
    pub disponivel: String,
    /// Versão em execução.
    pub atual: String,
    /// Há versão nova.
    pub tem_nova: bool,
    /// Tamanho do download, em MB, para a interface poder avisar.
    pub mb: u64,
}

/// Versão deste executável.
#[must_use]
pub fn versao_atual() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Consulta o manifesto publicado.
///
/// Falha em silêncio para quem chama: ficar sem internet, ou o servidor estar
/// fora, não pode impedir alguém de transmitir numa rede local.
pub async fn consulta(servidor: &str) -> Option<Manifesto> {
    let url = format!("{}/download/versao.json", servidor.trim_end_matches('/'));
    match reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => match r.json::<Manifesto>().await {
            Ok(m) => Some(m),
            Err(e) => {
                registro::linha(&format!("manifesto ilegível: {e}"));
                None
            }
        },
        Ok(r) => {
            registro::linha(&format!("manifesto: HTTP {}", r.status()));
            None
        }
        Err(e) => {
            registro::linha(&format!("não consegui consultar versão: {e}"));
            None
        }
    }
}

/// Compara a versão publicada com a em execução.
///
/// Comparação por partes numéricas, não por texto: `"0.10.0" > "0.9.0"` é falso
/// em ordem alfabética, e passaríamos a nunca mais avisar de atualização depois
/// da décima versão menor.
#[must_use]
pub fn e_mais_nova(publicada: &str, atual: &str) -> bool {
    let partes = |v: &str| -> Vec<u64> {
        v.split(['.', '-'])
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let (a, b) = (partes(publicada), partes(atual));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x > y;
        }
    }
    false
}

/// Baixa um arquivo do servidor, conferindo tamanho e hash.
///
/// `ao_progredir` recebe (bytes recebidos, bytes totais) para a interface poder
/// mostrar andamento — 140 MB sem retorno nenhum parece travamento.
/// Nome do arquivo temporário de um download, distinto a cada chamada.
///
/// O desempate é um contador, e não o PID: as duas execuções concorrentes que
/// causaram o problema eram TAREFAS DO MESMO PROCESSO, então `process::id()`
/// devolveria o mesmo número para as duas e não separaria nada.
fn nome_parcial(destino: &Path) -> PathBuf {
    static SEQUENCIA: AtomicU64 = AtomicU64::new(0);
    let n = SEQUENCIA.fetch_add(1, Ordering::Relaxed);
    destino.with_extension(format!("parcial{n}"))
}

pub async fn baixa(
    servidor: &str,
    token: &str,
    arq: &Arquivo,
    destino: &Path,
    mut ao_progredir: impl FnMut(u64, u64),
) -> Result<()> {
    use futures_util::StreamExt as _;
    use std::io::Write as _;

    let url = format!(
        "{}/download/{}?token={}",
        servidor.trim_end_matches('/'),
        arq.arquivo,
        token
    );

    let resposta = reqwest::Client::new()
        .get(&url)
        .send()
        .await
        .context("iniciar download")?;
    if !resposta.status().is_success() {
        bail!("servidor respondeu {}", resposta.status());
    }

    if let Some(pai) = destino.parent() {
        std::fs::create_dir_all(pai).with_context(|| format!("criar {}", pai.display()))?;
    }

    // Escreve em `.parcial` e só renomeia no fim: um download interrompido
    // deixaria um arquivo truncado no lugar definitivo, e o erro apareceria
    // muito depois, na forma de um encoder que não abre.
    // Nome único por download, e não um `.parcial` fixo.
    //
    // Duas verificações de atualização podem rodar ao mesmo tempo — a da
    // abertura e a do temporizador, ou dois cliques. Com nome fixo elas
    // disputam o MESMO arquivo: uma renomeia primeiro e a outra falha no
    // `rename` com "não pode encontrar o arquivo". Foi assim que a máquina de
    // um dos amigos ficou sem o `avcodec-62.dll` e transmitiu no caminho antigo
    // sem ninguém entender por quê.
    //
    // O desempate é um contador, e não o PID: as duas execuções concorrentes
    // são TAREFAS DO MESMO PROCESSO, então `process::id()` devolveria o mesmo
    // número para as duas e não separaria nada.
    let parcial = nome_parcial(destino);
    let mut f =
        std::fs::File::create(&parcial).with_context(|| format!("criar {}", parcial.display()))?;

    let mut hasher = Sha256::novo();
    let mut recebidos: u64 = 0;
    let mut fluxo = resposta.bytes_stream();
    while let Some(pedaco) = fluxo.next().await {
        let pedaco = pedaco.context("ler do servidor")?;
        f.write_all(&pedaco).context("gravar em disco")?;
        hasher.consome(&pedaco);
        recebidos += pedaco.len() as u64;
        ao_progredir(recebidos, arq.bytes);
    }
    f.flush().context("finalizar gravação")?;
    drop(f);

    if recebidos != arq.bytes {
        let _ = std::fs::remove_file(&parcial);
        bail!(
            "tamanho não confere: {recebidos} bytes, esperados {}",
            arq.bytes
        );
    }
    let obtido = hasher.hexa();
    if !arq.sha256.is_empty() && obtido != arq.sha256 {
        let _ = std::fs::remove_file(&parcial);
        bail!(
            "conteúdo corrompido: sha256 {obtido}, esperado {}",
            arq.sha256
        );
    }

    let _ = std::fs::remove_file(destino);
    std::fs::rename(&parcial, destino)
        .with_context(|| format!("mover para {}", destino.display()))?;
    Ok(())
}

/// Onde as DLLs da libavcodec ficam.
///
/// Ao lado do `ffmpeg.exe` baixado, no cache: é material da mesma natureza —
/// grande, raramente muda, e não cabe dentro do executável.
#[must_use]
pub fn pasta_libav(dir_cache: &Path) -> PathBuf {
    dir_cache.join("libav")
}

/// Baixa as DLLs que faltarem. Devolve quantas foram buscadas.
///
/// Nada aqui é fatal: sem as DLLs o app continua transmitindo pelo
/// `ffmpeg.exe`, que é o que ele sempre fez. Por isso os erros vão para o log e
/// a função segue para o arquivo seguinte, em vez de abortar.
pub async fn garante_libav(
    servidor: &str,
    token: &str,
    dir_cache: &Path,
    lista: &[Arquivo],
) -> usize {
    if lista.is_empty() {
        return 0;
    }
    // Uma de cada vez. O nome único já impede o estrago, mas baixar 71 MB duas
    // vezes em paralelo desperdiça banda de quem está transmitindo — que é
    // justamente quem mais precisa dela.
    static SO_UMA: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guarda = SO_UMA.lock().await;
    let pasta = pasta_libav(dir_cache);
    if let Err(e) = std::fs::create_dir_all(&pasta) {
        registro::linha(&format!(
            "libav: não consegui criar {}: {e}",
            pasta.display()
        ));
        return 0;
    }

    let mut baixadas = 0;
    for arq in lista {
        let destino = pasta.join(&arq.arquivo);
        // O tamanho basta como identidade: cada versão da DLL tem o seu, e o
        // SHA-256 já é conferido no download. Reconferir o hash de 68 MB a cada
        // abertura do app custaria mais do que o download economiza.
        if std::fs::metadata(&destino).is_ok_and(|m| m.len() == arq.bytes) {
            continue;
        }
        match baixa(servidor, token, arq, &destino, |_, _| {}).await {
            Ok(()) => {
                registro::linha(&format!("libav: {} baixada", arq.arquivo));
                baixadas += 1;
            }
            Err(e) => registro::linha(&format!("libav: {} falhou — {e:#}", arq.arquivo)),
        }
    }
    baixadas
}

/// Substitui o próprio executável e reinicia.
///
/// O Windows não deixa sobrescrever um `.exe` em execução, mas deixa
/// **renomeá-lo**. Então: o atual vira `.antigo`, o novo toma o lugar, e o
/// processo reinicia. A limpeza do `.antigo` fica para a próxima abertura, que
/// é quando ele já não está em uso.
pub fn troca_por(novo: &Path) -> Result<()> {
    let atual = std::env::current_exe().context("descobrir o próprio caminho")?;
    poe_no_lugar(novo, &atual)?;

    registro::linha("atualizado; reiniciando");
    // Manda a cópia nova ESPERAR esta aqui morrer antes de subir.
    //
    // Sem isso ela nasce enquanto este processo ainda está encerrando, o
    // controle de instância única a vê como segunda cópia e ela sai calada —
    // e este processo sai logo em seguida. Resultado: nada rodando, e o app
    // "não abre mais" depois de atualizar. Foi exatamente o que aconteceu.
    std::process::Command::new(&atual)
        .arg(ARG_AGUARDA)
        .arg(std::process::id().to_string())
        .spawn()
        .context("reiniciar")?;
    Ok(())
}

/// Argumento que pede à cópia nova para esperar a antiga sair.
pub const ARG_AGUARDA: &str = "--aguarda-pid";

/// Espera o processo indicado em `--aguarda-pid` terminar.
///
/// Chamado no comecinho do `main`, **antes** de qualquer coisa do Tauri: é
/// justamente o controle de instância única que precisa não ver o processo
/// antigo. Sem argumento, devolve na hora.
///
/// Teto de 15 s para não ficar preso se o processo antigo travar ao sair —
/// abrir uma segunda cópia é ruim, mas não abrir nenhuma é pior.
pub fn aguarda_anterior() {
    let mut args = std::env::args().skip(1);
    let pid = args
        .find(|a| a == ARG_AGUARDA)
        .and_then(|_| args.next())
        .and_then(|p| p.parse::<u32>().ok());

    if let Some(pid) = pid {
        espera_ate(std::time::Duration::from_secs(15), || !processo_vivo(pid));
        return;
    }

    // Sem argumento, mas com um `.antigo` recém-criado ao lado: esta cópia
    // nasceu de uma atualização feita por uma versão que ainda não sabia
    // passar o argumento. Espera as outras cópias saírem do mesmo jeito.
    //
    // Existe porque a correção mora no executável NOVO, e quem reinicia é o
    // VELHO — sem isto, a primeira atualização depois do conserto ainda
    // fecharia o app e não abriria nada.
    if acabou_de_atualizar() {
        espera_ate(std::time::Duration::from_secs(10), || {
            outras_copias().is_empty()
        });
    }
}

/// Espera `cond` virar verdadeira, com teto.
///
/// O teto importa mais que a espera: abrir uma segunda cópia é ruim, não abrir
/// nenhuma é pior.
fn espera_ate(teto: std::time::Duration, cond: impl Fn() -> bool) {
    let limite = std::time::Instant::now() + teto;
    while std::time::Instant::now() < limite {
        if cond() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
}

/// Existe um `.antigo` ao lado, criado nos últimos 30 segundos.
///
/// A idade é o que evita esperar à toa: um `.antigo` que sobrou de dias atrás
/// (antivírus segurando o arquivo) não faria toda abertura demorar.
fn acabou_de_atualizar() -> bool {
    let Ok(atual) = std::env::current_exe() else {
        return false;
    };
    let Ok(meta) = std::fs::metadata(atual.with_extension("antigo")) else {
        return false;
    };
    meta.modified()
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|idade| idade < std::time::Duration::from_secs(30))
}

/// PIDs de outras cópias deste mesmo executável.
fn outras_copias() -> Vec<u32> {
    let Ok(atual) = std::env::current_exe() else {
        return Vec::new();
    };
    let Some(nome) = atual.file_name().and_then(|n| n.to_str()) else {
        return Vec::new();
    };
    let nome = nome.to_lowercase();
    let eu = std::process::id();
    tela_capture::processos::lista()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.pid != eu && p.nome.to_lowercase() == nome)
        .map(|p| p.pid)
        .collect()
}

/// O processo ainda existe.
fn processo_vivo(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: o handle é fechado nos dois caminhos. Um PID que já não existe
    // faz `OpenProcess` falhar, que é a resposta que interessa.
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut code = 0u32;
        let vivo = GetExitCodeProcess(h, &mut code).is_ok() && code == STILL_ACTIVE.0 as u32;
        let _ = CloseHandle(h);
        vivo
    }
}

/// A troca em si, sem reiniciar.
///
/// Separada de `troca_por` para poder ser testada: `current_exe()` dentro de um
/// teste é o próprio binário de teste, e trocá-lo faria a suíte se reexecutar.
fn poe_no_lugar(novo: &Path, atual: &Path) -> Result<()> {
    let antigo = atual.with_extension("antigo");

    let _ = std::fs::remove_file(&antigo);
    std::fs::rename(atual, &antigo)
        .with_context(|| format!("renomear {} — a pasta é gravável?", atual.display()))?;

    if let Err(e) = std::fs::rename(novo, atual) {
        // Desfaz: sem isto a pessoa ficaria sem executável nenhum.
        let _ = std::fs::rename(&antigo, atual);
        return Err(e).context("pôr a versão nova no lugar");
    }
    Ok(())
}

/// Apaga o executável antigo deixado por uma atualização anterior.
pub fn limpa_antigo() {
    let Ok(atual) = std::env::current_exe() else {
        return;
    };
    let antigo = atual.with_extension("antigo");
    if !antigo.exists() {
        return;
    }

    // Em segundo plano, com algumas tentativas.
    //
    // Na primeira abertura depois de atualizar, o processo ANTIGO ainda está
    // vivo: ele copia o novo para o lugar, dispara e só então sai. O Windows
    // não deixa apagar um `.exe` em uso, então a primeira tentativa falha
    // sempre — e era por isso que o `tela.antigo` ficava visível na pasta até
    // a abertura seguinte. Esperar alguns segundos resolve sem travar nada.
    std::thread::spawn(move || {
        for tentativa in 1..=10 {
            match std::fs::remove_file(&antigo) {
                Ok(()) => {
                    registro::linha(&format!("versão anterior removida (tentativa {tentativa})"));
                    return;
                }
                Err(e) if tentativa == 10 => {
                    // Antivírus segurando o arquivo, ou o processo antigo
                    // pendurado: fica para a próxima abertura.
                    registro::linha(&format!("versão anterior ainda em uso: {e}"));
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(600)),
            }
        }
    });
}

/// Onde o encoder baixado fica.
#[must_use]
pub fn caminho_ffmpeg_baixado(dir_cache: &Path) -> PathBuf {
    dir_cache.join("ffmpeg.exe")
}

// ---------------------------------------------------------------------------
// SHA-256
// ---------------------------------------------------------------------------

/// Implementação mínima de SHA-256.
///
/// Escrita à mão em vez de trazer a crate `sha2`: são ~60 linhas de aritmética
/// fechada, com vetores de teste públicos que provam a correção, contra uma
/// dependência a mais no grafo de um app que já é grande.
struct Sha256 {
    estado: [u32; 8],
    buffer: Vec<u8>,
    total: u64,
}

const K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

impl Sha256 {
    fn novo() -> Self {
        Self {
            estado: [
                0x6a09_e667,
                0xbb67_ae85,
                0x3c6e_f372,
                0xa54f_f53a,
                0x510e_527f,
                0x9b05_688c,
                0x1f83_d9ab,
                0x5be0_cd19,
            ],
            buffer: Vec::with_capacity(64),
            total: 0,
        }
    }

    fn consome(&mut self, dados: &[u8]) {
        self.total += dados.len() as u64;
        self.buffer.extend_from_slice(dados);
        while self.buffer.len() >= 64 {
            let bloco: [u8; 64] = self.buffer[..64].try_into().expect("64 bytes");
            self.comprime(&bloco);
            self.buffer.drain(..64);
        }
    }

    fn comprime(&mut self, bloco: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(bloco[i * 4..i * 4 + 4].try_into().expect("4 bytes"));
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let mut h = self.estado;
        for i in 0..64 {
            let s1 = h[4].rotate_right(6) ^ h[4].rotate_right(11) ^ h[4].rotate_right(25);
            let ch = (h[4] & h[5]) ^ ((!h[4]) & h[6]);
            let t1 = h[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = h[0].rotate_right(2) ^ h[0].rotate_right(13) ^ h[0].rotate_right(22);
            let maj = (h[0] & h[1]) ^ (h[0] & h[2]) ^ (h[1] & h[2]);
            let t2 = s0.wrapping_add(maj);

            h[7] = h[6];
            h[6] = h[5];
            h[5] = h[4];
            h[4] = h[3].wrapping_add(t1);
            h[3] = h[2];
            h[2] = h[1];
            h[1] = h[0];
            h[0] = t1.wrapping_add(t2);
        }
        for (e, novo) in self.estado.iter_mut().zip(h) {
            *e = e.wrapping_add(novo);
        }
    }

    fn hexa(mut self) -> String {
        let bits = self.total * 8;
        self.buffer.push(0x80);
        while self.buffer.len() % 64 != 56 {
            self.buffer.push(0);
        }
        self.buffer.extend_from_slice(&bits.to_be_bytes());
        let blocos: Vec<[u8; 64]> = self
            .buffer
            .chunks_exact(64)
            .map(|c| c.try_into().expect("64 bytes"))
            .collect();
        for b in blocos {
            self.comprime(&b);
        }
        self.estado.iter().map(|w| format!("{w:08x}")).collect()
    }
}

#[cfg(test)]
mod testes {

    /// Dois downloads simultâneos não podem disputar o mesmo arquivo temporário.
    ///
    /// # O que quebrou
    ///
    /// A verificação da abertura e a do temporizador rodaram juntas e as duas
    /// escreveram no mesmo `.parcial`: uma renomeou primeiro, a outra falhou o
    /// `rename` com "não pode encontrar o arquivo", e a máquina de um dos amigos
    /// ficou sem o `avcodec-62.dll`. Ele transmitiu a noite inteira no caminho
    /// antigo, com o dobro de CPU, sem ninguém entender por quê.
    ///
    /// A primeira tentativa de conserto usou `process::id()` no nome — que NÃO
    /// resolve, porque as duas execuções são tarefas do mesmo processo e
    /// receberiam o mesmo número. É esse engano que este teste tranca.
    #[test]
    fn cada_download_tem_seu_proprio_arquivo_parcial() {
        let destino = std::path::Path::new(r"C:\cache\libav\avcodec-62.dll");
        let nomes: std::collections::HashSet<_> = (0..64).map(|_| nome_parcial(destino)).collect();
        assert_eq!(
            nomes.len(),
            64,
            "dois downloads receberam o mesmo arquivo temporário"
        );
        // E continua ao lado do destino: renomear entre volumes falharia.
        assert_eq!(
            nome_parcial(destino).parent(),
            destino.parent(),
            "o parcial saiu da pasta do destino"
        );
    }

    use super::*;

    /// Vetores públicos do SHA-256. Implementação de hash sem vetor de teste é
    /// uma que ninguém sabe se está certa.
    #[test]
    fn sha256_bate_com_os_vetores_conhecidos() {
        let casos: [(&[u8], &str); 3] = [
            (
                b"",
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                b"abc",
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
        ];
        for (entrada, esperado) in casos {
            let mut h = Sha256::novo();
            h.consome(entrada);
            assert_eq!(h.hexa(), esperado, "falhou em {:?}", entrada.len());
        }
    }

    /// O hash não pode depender de como os bytes foram entregues.
    #[test]
    fn consumo_em_pedacos_da_o_mesmo_resultado() {
        let dados: Vec<u8> = (0..1000u32).map(|i| (i % 256) as u8).collect();
        let mut inteiro = Sha256::novo();
        inteiro.consome(&dados);

        let mut picado = Sha256::novo();
        for p in dados.chunks(7) {
            picado.consome(p);
        }
        assert_eq!(inteiro.hexa(), picado.hexa());
    }

    /// Prova em escala: 1 MB gerado deterministicamente, conferido contra o
    /// `hashlib` do Python. Os vetores oficiais são curtos e não exercem
    /// múltiplos blocos nem o comprimento em bits de um arquivo grande — e um
    /// erro aí quebraria a atualização de todo mundo de uma vez.
    #[test]
    fn sha256_de_um_megabyte_bate_com_referencia_externa() {
        let mut x: u64 = 12345;
        let dados: Vec<u8> = (0..1_000_003)
            .map(|_| {
                x = 1_103_515_245u64.wrapping_mul(x).wrapping_add(12345) & 0x7fff_ffff;
                ((x >> 16) & 0xFF) as u8
            })
            .collect();
        let mut h = Sha256::novo();
        h.consome(&dados);
        assert_eq!(
            h.hexa(),
            "c0b23fda0a9f73110875ed831b45fda5fab85a8f4b69a2d4dcf3623950d91c08"
        );
    }

    /// A troca deixa a versão nova no lugar e a anterior guardada ao lado.
    #[test]
    fn troca_preserva_a_versao_anterior() {
        let dir = std::env::temp_dir().join("tela-troca-ok");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let atual = dir.join("app.exe");
        let novo = dir.join("app.novo");
        std::fs::write(&atual, b"versao velha").unwrap();
        std::fs::write(&novo, b"versao nova").unwrap();

        poe_no_lugar(&novo, &atual).unwrap();

        assert_eq!(std::fs::read(&atual).unwrap(), b"versao nova");
        assert_eq!(
            std::fs::read(dir.join("app.antigo")).unwrap(),
            b"versao velha"
        );
        assert!(!novo.exists(), "o arquivo baixado deveria ter sido movido");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Se o arquivo novo sumir no meio do caminho, o antigo volta ao lugar.
    ///
    /// Sem o desfazer, a pessoa ficaria com uma pasta sem executável nenhum e
    /// sem entender por quê — o pior desfecho possível para uma atualização.
    #[test]
    fn troca_que_falha_devolve_a_versao_antiga() {
        let dir = std::env::temp_dir().join("tela-troca-falha");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let atual = dir.join("app.exe");
        std::fs::write(&atual, b"versao velha").unwrap();

        let erro = poe_no_lugar(&dir.join("nao-existe.novo"), &atual);

        assert!(erro.is_err());
        assert_eq!(
            std::fs::read(&atual).unwrap(),
            b"versao velha",
            "o executável tem de continuar lá"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compara_versao_numericamente() {
        assert!(e_mais_nova("0.2.0", "0.1.0"));
        assert!(e_mais_nova("1.0.0", "0.9.9"));
        // O caso que a comparação de texto erra: "10" < "9" em ordem alfabética.
        assert!(e_mais_nova("0.10.0", "0.9.0"));
        assert!(!e_mais_nova("0.1.0", "0.1.0"));
        assert!(!e_mais_nova("0.1.0", "0.2.0"));
        // Versão com sufixo não deve ser tratada como mais nova que a limpa.
        assert!(!e_mais_nova("0.1.0-beta", "0.1.0"));
    }
}

#[cfg(test)]
mod testes_espera {
    use super::*;

    /// Sem o argumento, não espera nada — é o caminho de toda abertura normal.
    #[test]
    fn sem_argumento_volta_na_hora() {
        // Três segundos, e não trezentos milissegundos: o que se prova aqui é
        // "não esperou o teto de 15 s", não a velocidade da máquina. O limite
        // apertado chegou a falhar uma vez com a máquina sob carga, e um teste
        // que grita à toa é pior que teste nenhum.
        let antes = std::time::Instant::now();
        aguarda_anterior();
        assert!(
            antes.elapsed() < std::time::Duration::from_secs(3),
            "não podia ter esperado: {:?}",
            antes.elapsed()
        );
    }

    /// O próprio processo está vivo; um PID que nunca existiu, não.
    ///
    /// Sem esta distinção, `aguarda_anterior` esperaria os 15 s inteiros a
    /// cada atualização, ou não esperaria nada e o bug voltaria.
    #[test]
    fn reconhece_processo_vivo_e_morto() {
        assert!(
            processo_vivo(std::process::id()),
            "o próprio processo está vivo"
        );
        // PIDs no Windows são múltiplos de 4; um ímpar alto não existe.
        assert!(
            !processo_vivo(0xFFFF_FFF7),
            "PID inventado não pode parecer vivo"
        );
    }

    /// Espera um processo de verdade terminar, e volta quando ele termina.
    #[test]
    fn espera_um_processo_de_verdade() {
        let mut filho = std::process::Command::new("cmd")
            .args(["/C", "ping -n 3 127.0.0.1 > nul"])
            .spawn()
            .expect("abrir processo de teste");
        let pid = filho.id();
        assert!(processo_vivo(pid));

        let antes = std::time::Instant::now();
        let _ = filho.wait();
        // Depois de `wait`, o processo já morreu — a checagem tem de concordar.
        assert!(!processo_vivo(pid), "devia estar morto depois do wait");
        assert!(antes.elapsed() < std::time::Duration::from_secs(10));
    }
}

#[cfg(test)]
mod testes_pos_atualizacao {
    use super::*;

    /// Serializa os testes que mexem no `.antigo`.
    ///
    /// Eles disputam O MESMO arquivo, ao lado do binário de teste: em paralelo,
    /// o que apaga derruba o que acabou de criar. Falhou exatamente assim.
    static SO_UM: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Sem `.antigo` ao lado, não é atualização recente — é abertura normal, e
    /// não pode custar espera nenhuma.
    #[test]
    fn sem_antigo_nao_e_atualizacao() {
        let _g = SO_UM.lock();
        let atual = std::env::current_exe().expect("caminho do teste");
        let _ = std::fs::remove_file(atual.with_extension("antigo"));
        assert!(!acabou_de_atualizar());
    }

    /// Com um `.antigo` recém-criado, sim.
    #[test]
    fn antigo_recente_conta_como_atualizacao() {
        let _g = SO_UM.lock();
        let atual = std::env::current_exe().expect("caminho do teste");
        let antigo = atual.with_extension("antigo");
        std::fs::write(&antigo, b"resto de atualizacao").expect("criar .antigo");

        let resposta = acabou_de_atualizar();
        let _ = std::fs::remove_file(&antigo);
        assert!(resposta, "um .antigo recém-escrito devia contar");
    }

    /// A busca por outras cópias nunca inclui o próprio processo — incluir-se
    /// faria a espera nunca terminar.
    #[test]
    fn nao_conta_a_si_mesmo() {
        assert!(!outras_copias().contains(&std::process::id()));
    }

    /// A espera respeita o teto quando a condição nunca se cumpre.
    #[test]
    fn espera_desiste_no_teto() {
        let antes = std::time::Instant::now();
        espera_ate(std::time::Duration::from_millis(600), || false);
        let passou = antes.elapsed();
        assert!(
            passou >= std::time::Duration::from_millis(500),
            "saiu cedo: {passou:?}"
        );
        // Folga generosa no teto: prova que o limite existe, sem depender de
        // quão ocupada a máquina está.
        assert!(
            passou < std::time::Duration::from_secs(6),
            "passou do teto: {passou:?}"
        );
    }

    /// E volta na hora quando ela já está cumprida.
    #[test]
    fn espera_volta_na_hora_se_ja_esta_ok() {
        // Mesma folga do teste acima, pelo mesmo motivo.
        let antes = std::time::Instant::now();
        espera_ate(std::time::Duration::from_secs(10), || true);
        assert!(antes.elapsed() < std::time::Duration::from_secs(3));
    }
}
