//! Named pipes do Windows: nós somos o servidor que escreve, o ffmpeg é o
//! cliente que lê.
//!
//! Pipe e não stdin porque são dois fluxos simultâneos (vídeo e áudio) para o
//! mesmo processo, e um processo só tem um stdin.

use std::fs::File;
use std::os::windows::io::FromRawHandle;

use anyhow::{Context, Result};
use windows::core::{HRESULT, HSTRING};
use windows::Win32::Foundation::{CloseHandle, ERROR_PIPE_CONNECTED, HANDLE};
use windows::Win32::Storage::FileSystem::PIPE_ACCESS_OUTBOUND;
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_TYPE_BYTE, PIPE_WAIT,
};

/// Buffer de saída do pipe. Generoso porque 1080p60 em NV12 é ~190 MB/s: um
/// soluço momentâneo do ffmpeg não pode bloquear a thread de captura.
const OUT_BUFFER_BYTES: u32 = 8 * 1024 * 1024;

/// Um named pipe de saída aguardando o ffmpeg conectar.
pub struct PipeServer {
    pub path: String,
    /// `None` depois de [`PipeServer::accept`], que transfere a posse do handle
    /// para o `File`. É o que evita fechar o mesmo handle duas vezes.
    handle: Option<HANDLE>,
}

impl PipeServer {
    /// Cria o pipe. `name` é só o sufixo: `"v"` vira
    /// `\\.\pipe\noch_tela_v_<pid>_<geracao>`.
    ///
    /// O PID entra no nome para que duas instâncias do cliente rodando na
    /// mesma máquina não disputem o mesmo pipe.
    ///
    /// A GERAÇÃO entra porque trocar a taxa reinicia o ffmpeg dentro da mesma
    /// transmissão, e o pipe do encoder novo nasce enquanto o do antigo ainda
    /// pode estar fechando. Reaproveitar o nome seria corrida com o sistema
    /// operacional: o pipe antigo ainda existir faz a criação falhar, e a
    /// troca quebraria de vez em quando, sem padrão reproduzível.
    pub fn create(name: &str, geracao: u64) -> Result<Self> {
        let path = format!(
            "\\\\.\\pipe\\noch_tela_{}_{}_{}",
            name,
            std::process::id(),
            geracao
        );
        // SAFETY: `path` vive até o fim da chamada e os demais argumentos são
        // valores por cópia. `None` em lpSecurityAttributes = descritor de
        // segurança padrão, o que restringe o pipe à sessão do usuário. A posse
        // do handle devolvido passa a ser deste struct.
        let handle = unsafe {
            CreateNamedPipeW(
                &HSTRING::from(path.as_str()),
                PIPE_ACCESS_OUTBOUND,
                PIPE_TYPE_BYTE | PIPE_WAIT,
                1,
                OUT_BUFFER_BYTES,
                0,
                0,
                None,
            )
        };
        anyhow::ensure!(!handle.is_invalid(), "CreateNamedPipeW falhou para {path}");
        Ok(Self {
            path,
            handle: Some(handle),
        })
    }

    /// Bloqueia até o ffmpeg abrir a outra ponta e devolve o writer.
    ///
    /// Bloqueia *indefinidamente*: o pipe é síncrono, e sem I/O sobreposto não
    /// existe timeout. Se o ffmpeg morrer antes de abrir esta ponta (linha de
    /// comando inválida, por exemplo), a espera não termina. Quando houver UI,
    /// vale trocar por `FILE_FLAG_OVERLAPPED` e uma espera cancelável.
    pub fn accept(mut self) -> Result<File> {
        let handle = self.handle.take().context("pipe já entregue a um writer")?;

        // SAFETY: `handle` é um pipe válido criado por `create` e ainda não
        // transferido. `None` em lpOverlapped = operação síncrona, que é o
        // modo em que o pipe foi criado (PIPE_WAIT).
        let connected = unsafe { ConnectNamedPipe(handle, None) };
        if let Err(e) = connected {
            // ERROR_PIPE_CONNECTED significa que o ffmpeg foi mais rápido que
            // nós e já estava do outro lado. É sucesso, não falha.
            if e.code() != HRESULT::from_win32(ERROR_PIPE_CONNECTED.0) {
                // SAFETY: handle válido e ainda nosso; ninguém mais o usa.
                let _ = unsafe { CloseHandle(handle) };
                return Err(anyhow::Error::new(e).context(format!(
                    "ConnectNamedPipe falhou em {} — o ffmpeg subiu?",
                    self.path
                )));
            }
        }

        // SAFETY: transferimos a posse exclusiva do handle para o `File`, que
        // passa a ser o responsável por fechá-lo. `self.handle` já é `None`,
        // então o Drop abaixo não vai fechá-lo de novo.
        Ok(unsafe { File::from_raw_handle(handle.0.cast()) })
    }
}

impl Drop for PipeServer {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            // SAFETY: só chega aqui se `accept` nunca rodou, ou seja, o handle
            // ainda é nosso e ninguém mais o referencia.
            let _ = unsafe { CloseHandle(handle) };
        }
    }
}
