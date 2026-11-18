//! Consultas ao Windows que a interface precisa saber antes de transmitir.

use std::process::Command;

use anyhow::{Context, Result};
use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;

/// Build mínimo do Windows com loopback de áudio **por processo**
/// (`ActivateAudioInterfaceAsync` com `AUDIOCLIENT_ACTIVATION_PARAMS`).
/// Abaixo disso só existe loopback do dispositivo inteiro.
const BUILD_LOOPBACK_POR_PROCESSO: u32 = 20348;

/// Número de build do Windows.
///
/// Via `RtlGetVersion` e não `GetVersionEx`: a segunda mente por
/// compatibilidade desde o Windows 8.1 — sem um manifesto declarando suporte à
/// versão nova ela devolve 6.2, e o app concluiria que nenhuma máquina moderna
/// tem áudio por processo.
pub fn build_do_windows() -> u32 {
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: u32::try_from(std::mem::size_of::<OSVERSIONINFOW>()).unwrap_or(0),
        ..Default::default()
    };
    // SAFETY: `info` é um OSVERSIONINFOW válido com o tamanho preenchido, que
    // é exatamente o contrato de RtlGetVersion.
    let status = unsafe { RtlGetVersion(&mut info) };
    if status.is_ok() {
        info.dwBuildNumber
    } else {
        0
    }
}

/// Esta máquina consegue isolar o áudio de um processo só.
///
/// Quando `false`, compartilhar uma janela ainda funciona — mas o som vai ser
/// o do sistema inteiro, e a interface **avisa**. Falhar em silêncio aqui
/// significaria vazar a conversa do Discord no meio do jogo sem ninguém saber.
pub fn tem_audio_por_processo() -> bool {
    let build = build_do_windows();
    // Build 0 = não consegui descobrir. Assumir que TEM é o certo: a captura
    // por processo falha com erro claro se não tiver, enquanto assumir que não
    // tem degradaria o produto em toda máquina onde a consulta falhasse.
    build == 0 || build >= BUILD_LOOPBACK_POR_PROCESSO
}

/// Abre uma URL no navegador padrão.
///
/// `ShellExecuteW` via `rundll32` seria mais direto, mas `cmd /C start` é o
/// caminho que funciona igual em qualquer Windows suportado. O primeiro
/// argumento vazio não é decorativo: sem ele o `start` interpreta a URL como
/// título da janela e não abre nada.
pub fn abre_no_navegador(url: &str) -> Result<()> {
    // A URL é montada a partir da configuração e não de entrada da rede, mas
    // recusar aspas e `&` custa nada e fecha a porta para injeção de comando.
    anyhow::ensure!(
        !url.contains(['"', '&', '|', '^', '<', '>', '\n']),
        "URL com caractere que o interpretador de comandos trataria como sintaxe"
    );
    Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn()
        .context("abrir o navegador")?;
    Ok(())
}

/// Placas de vídeo que o Windows enxerga, com a memória dedicada de cada uma.
///
/// # Por que isto está no log
///
/// Dois sintomas relatados por um usuário tinham a mesma causa provável e
/// nenhuma forma de confirmar a distância: o encoder por hardware da AMD não
/// carregava (`amfrt64.dll`), e a captura de **monitor** voltava preta enquanto
/// a de janela funcionava.
///
/// As duas coisas acontecem quando o Windows está sem o driver adequado da
/// placa e caiu para o adaptador básico: o runtime do encoder não existe, e a
/// captura de tela não tem de onde ler o quadro composto. A captura de janela
/// continua funcionando porque passa pelo DWM.
///
/// Uma linha dizendo `Microsoft Basic Display Adapter` responde isso na hora.
pub fn placas_de_video() -> Vec<String> {
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, DXGI_ADAPTER_FLAG,
        DXGI_ADAPTER_FLAG_SOFTWARE,
    };

    let mut out = Vec::new();
    // SAFETY: chamadas COM de leitura; cada ponteiro devolvido é usado apenas
    // dentro do laço e liberado pelo `Drop` do wrapper da crate `windows`.
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return vec!["não consegui consultar o DXGI".to_string()];
        };
        let mut i = 0;
        while let Ok(adaptador) = factory.EnumAdapters1(i) {
            i += 1;
            let Ok(desc) = adaptador.GetDesc1() else {
                continue;
            };
            let nome = String::from_utf16_lossy(&desc.Description)
                .trim_end_matches('\0')
                .trim()
                .to_string();
            let vram_mb = desc.DedicatedVideoMemory / (1024 * 1024);
            let software = DXGI_ADAPTER_FLAG(desc.Flags as i32) == DXGI_ADAPTER_FLAG_SOFTWARE;
            out.push(if software {
                format!("{nome} (renderização por software)")
            } else {
                format!("{nome} ({vram_mb} MB dedicados)")
            });
            let _: IDXGIAdapter1 = adaptador;
        }
    }
    if out.is_empty() {
        out.push("nenhuma placa enumerada".to_string());
    }
    out
}

/// Quanto de CPU este processo consumiu desde a última chamada, em porcentagem
/// de UM núcleo (pode passar de 100 com várias threads ocupadas).
///
/// Existe porque "está travando meu PC" não é diagnosticável: pode ser a
/// captura, o codificador, outra coisa na máquina, ou impressão. Com o número
/// no log, e o log chegando no servidor, dá para separar as três.
///
/// Guarda o estado da chamada anterior — só faz sentido a partir da segunda.
/// CPU de OUTRO processo, em porcentagem de um núcleo desde a chamada anterior.
///
/// Existe porque o encoder roda num processo separado e era invisível para
/// nós: alguém dizia "travou o PC" e a telemetria mostrava 20%, medindo só o
/// nosso processo enquanto o ffmpeg trabalhava ao lado sem ninguém contar.
///
/// Guarda o estado por PID, então só faz sentido a partir da segunda chamada
/// para o mesmo processo.
pub fn cpu_de(pid: u32) -> Option<f32> {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use windows::Win32::Foundation::{CloseHandle, FILETIME};
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    static ANTERIOR: Mutex<Option<HashMap<u32, (u64, std::time::Instant)>>> = Mutex::new(None);

    let (mut criacao, mut saida, mut kernel, mut usuario) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: o handle é fechado nos dois caminhos; as structs são de saída e
    // pertencem a esta pilha.
    let gasto = unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let ok = GetProcessTimes(h, &mut criacao, &mut saida, &mut kernel, &mut usuario);
        let _ = CloseHandle(h);
        ok.ok()?;
        let junta = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
        junta(kernel) + junta(usuario)
    };
    let agora = std::time::Instant::now();

    let mut guarda = ANTERIOR.lock().ok()?;
    let mapa = guarda.get_or_insert_with(HashMap::new);
    let resposta = mapa.get(&pid).and_then(|(antes, quando)| {
        let decorrido = agora.duration_since(*quando).as_secs_f64();
        if decorrido < 0.2 {
            return None;
        }
        let cpu = gasto.saturating_sub(*antes) as f64 / 10_000_000.0;
        #[allow(clippy::cast_possible_truncation)]
        Some((cpu / decorrido * 100.0) as f32)
    });
    mapa.insert(pid, (gasto, agora));
    resposta
}

pub fn cpu_do_processo() -> Option<f32> {
    use std::sync::Mutex;
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    /// (soma kernel+user em unidades de 100 ns, instante da medida).
    static ANTERIOR: Mutex<Option<(u64, std::time::Instant)>> = Mutex::new(None);

    let (mut criacao, mut saida, mut kernel, mut usuario) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: pseudo-handle do próprio processo, nunca inválido; as quatro
    // structs são de saída e pertencem a esta pilha.
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut criacao,
            &mut saida,
            &mut kernel,
            &mut usuario,
        )
    };
    if ok.is_err() {
        return None;
    }

    let junta = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
    let gasto = junta(kernel) + junta(usuario);
    let agora = std::time::Instant::now();

    let mut guarda = ANTERIOR.lock().ok()?;
    let resposta = guarda.and_then(|(antes, quando)| {
        let decorrido = agora.duration_since(quando).as_secs_f64();
        // Chamadas coladas dariam divisão por quase zero e um número absurdo.
        if decorrido < 0.2 {
            return None;
        }
        // 1 unidade = 100 ns, então 10_000_000 por segundo de CPU.
        let cpu = gasto.saturating_sub(antes) as f64 / 10_000_000.0;
        #[allow(clippy::cast_possible_truncation)]
        Some((cpu / decorrido * 100.0) as f32)
    });
    *guarda = Some((gasto, agora));
    resposta
}
