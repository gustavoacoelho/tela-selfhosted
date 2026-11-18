//! Guarda a senha da sala cifrada pelo Windows.
//!
//! # Por que não texto puro
//!
//! O `tela-config.json` fica ao lado do executável. Senha em texto ali vaza
//! para tudo que toca a pasta: backup em nuvem, pendrive, outra pessoa no mesmo
//! PC, qualquer programa lendo o disco.
//!
//! # O que o DPAPI garante — e o que não garante
//!
//! `CryptProtectData` cifra com uma chave derivada da conta do Windows. O texto
//! cifrado **só volta a ser legível pela mesma conta na mesma máquina**: copiar
//! o arquivo para outro PC, ou abri-lo por outro usuário, devolve erro em vez
//! de senha.
//!
//! Não protege contra um programa rodando **como o próprio usuário** — esse
//! consegue chamar `CryptUnprotectData` igual. Ou seja, isto defende contra o
//! arquivo vazar, não contra a máquina estar comprometida. Para uma senha
//! compartilhada de sala de amigos, é a proteção proporcional.
//!
//! Efeito colateral coerente com o modelo: levar a pasta do app para outro
//! computador não leva a senha junto — lá ela é digitada de novo.

use anyhow::{bail, Context, Result};
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB,
};

/// Rótulo gravado junto do blob. O Windows o devolve no `CryptUnprotectData`;
/// serve para identificar a origem do dado ao inspecionar.
const ROTULO: &str = "tela: senha da sala";

/// Cifra `senha` e devolve o blob em base64, pronto para ir ao JSON.
pub fn protege(senha: &str) -> Result<String> {
    if senha.is_empty() {
        bail!("senha vazia");
    }
    let mut entrada = senha.as_bytes().to_vec();
    let blob_entrada = CRYPT_INTEGER_BLOB {
        cbData: entrada.len() as u32,
        pbData: entrada.as_mut_ptr(),
    };

    let rotulo: Vec<u16> = ROTULO.encode_utf16().chain(std::iter::once(0)).collect();
    let mut saida = CRYPT_INTEGER_BLOB::default();

    // SAFETY: os dois blobs apontam para memória viva durante a chamada, e o
    // buffer devolvido em `saida` é liberado logo abaixo com `LocalFree`, como
    // a documentação da API exige.
    unsafe {
        CryptProtectData(
            &blob_entrada,
            windows::core::PCWSTR(rotulo.as_ptr()),
            None,
            None,
            None,
            0,
            &mut saida,
        )
        .context("CryptProtectData")?;
    }

    let bytes = unsafe { std::slice::from_raw_parts(saida.pbData, saida.cbData as usize) }.to_vec();
    unsafe {
        let _ = LocalFree(Some(HLOCAL(saida.pbData as *mut _)));
    }
    Ok(base64(&bytes))
}

/// Decifra um blob produzido por [`protege`].
///
/// Devolve `Err` quando o blob veio de outra máquina ou de outra conta — que é
/// justamente o comportamento desejado. Quem chama deve tratar isso como
/// "não tenho senha salva", e não como falha.
pub fn revela(b64: &str) -> Result<String> {
    let mut bytes = de_base64(b64).context("blob de senha corrompido")?;
    let blob = CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_mut_ptr(),
    };
    let mut saida = CRYPT_INTEGER_BLOB::default();

    // SAFETY: mesmas condições de `protege`.
    unsafe {
        CryptUnprotectData(&blob, None, None, None, None, 0, &mut saida)
            .context("CryptUnprotectData")?;
    }

    let claro = unsafe { std::slice::from_raw_parts(saida.pbData, saida.cbData as usize) }.to_vec();
    unsafe {
        let _ = LocalFree(Some(HLOCAL(saida.pbData as *mut _)));
    }

    String::from_utf8(claro).context("senha decifrada não é UTF-8")
}

// Base64 escrito à mão: é meia dúzia de linhas e evita uma dependência inteira
// só para transportar um blob dentro de um JSON.
const ALFABETO: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(dados: &[u8]) -> String {
    let mut s = String::with_capacity(dados.len().div_ceil(3) * 4);
    for bloco in dados.chunks(3) {
        let b = [
            bloco[0],
            *bloco.get(1).unwrap_or(&0),
            *bloco.get(2).unwrap_or(&0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= bloco.len() {
                s.push(ALFABETO[(n >> (18 - i * 6)) as usize & 63] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

fn de_base64(s: &str) -> Result<Vec<u8>> {
    let mut n = 0u32;
    let mut bits = 0;
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        let v = ALFABETO
            .iter()
            .position(|&a| a == c)
            .with_context(|| format!("caractere inválido em base64: {}", c as char))?;
        n = n << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((n >> bits) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod testes {
    use super::*;

    #[test]
    fn base64_ida_e_volta() {
        for caso in [
            &b""[..],
            &b"a"[..],
            &b"ab"[..],
            &b"abc"[..],
            &b"abcd"[..],
            &b"\x00\xff\x10qualquer coisa"[..],
        ] {
            let codificado = base64(caso);
            let voltou = de_base64(&codificado).expect("decodificar");
            assert_eq!(voltou, caso, "falhou em {codificado:?}");
        }
    }

    /// Cifrar e decifrar na mesma conta tem de devolver a senha original.
    /// Roda de verdade: o DPAPI está disponível em qualquer Windows.
    #[test]
    fn protege_e_revela() {
        let senha = "tela-folha-chuva-barco-2693";
        let blob = protege(senha).expect("proteger");
        assert_ne!(blob, senha, "o blob não pode conter a senha em claro");
        assert!(!blob.contains("chuva"), "vazou pedaço da senha no blob");
        assert_eq!(revela(&blob).expect("revelar"), senha);
    }

    #[test]
    fn blob_adulterado_nao_revela() {
        let blob = protege("segredo").expect("proteger");
        let mut ruim = blob.clone();
        // Troca um caractere do meio: o DPAPI tem verificação de integridade e
        // deve recusar, em vez de devolver lixo.
        let meio = ruim.len() / 2;
        let novo = if ruim.as_bytes()[meio] == b'A' {
            'B'
        } else {
            'A'
        };
        ruim.replace_range(meio..meio + 1, &novo.to_string());
        assert!(revela(&ruim).is_err(), "blob adulterado não pode revelar");
    }

    #[test]
    fn senha_vazia_e_recusada() {
        assert!(protege("").is_err());
    }
}
