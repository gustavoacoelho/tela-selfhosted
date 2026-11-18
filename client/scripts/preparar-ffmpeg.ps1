# Baixa o que a compilacao precisa para linkar contra o ffmpeg.
#
# Duas coisas, nenhuma versionada (juntas passam de 200 MB):
#
#   vendor/ffmpeg    headers + bibliotecas de importacao (.lib) + DLLs
#   vendor/libclang  libclang.dll, exigida pelo bindgen do ffmpeg-sys-next
#
# A build e a **LGPL**: linkar GPL obrigaria a abrir o codigo do tela'. A LGPL
# ja traz h264_nvenc, h264_amf, h264_qsv e libopus, que e tudo o que usamos.
#
# Os caminhos aqui casam com os de `.cargo/config.toml`; mudar um exige mudar
# o outro.

$ErrorActionPreference = 'Stop'
$raiz   = Split-Path -Parent $PSScriptRoot
$vendor = Join-Path $raiz 'vendor'
New-Item -ItemType Directory -Force -Path $vendor | Out-Null

# --- ffmpeg -----------------------------------------------------------------
$versao = 'ffmpeg-n8.1-latest-win64-lgpl-shared-8.1'
$zip    = Join-Path $vendor "$versao.zip"
$alvo   = Join-Path $vendor 'ffmpeg'

if (Test-Path (Join-Path $alvo 'lib\avcodec.lib')) {
    Write-Host "ffmpeg ja esta em $alvo"
} else {
    $url = "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/$versao.zip"
    Write-Host "baixando $versao (~71 MB)..."
    Invoke-WebRequest -Uri $url -OutFile $zip

    Write-Host 'extraindo include/, lib/ e bin/...'
    $tmp = Join-Path $vendor '_tmp'
    if (Test-Path $tmp) { Remove-Item -Recurse -Force $tmp }
    Expand-Archive -Path $zip -DestinationPath $tmp
    if (Test-Path $alvo) { Remove-Item -Recurse -Force $alvo }
    New-Item -ItemType Directory -Force -Path $alvo | Out-Null
    foreach ($p in 'include','lib','bin') {
        Move-Item (Join-Path $tmp "$versao\$p") (Join-Path $alvo $p)
    }
    Remove-Item -Recurse -Force $tmp
    Remove-Item -Force $zip   # o zip nao serve para mais nada e pesa 71 MB
    Write-Host "ffmpeg pronto em $alvo"
}

# --- libclang ---------------------------------------------------------------
# Vem do pacote Python `libclang`, que e so a DLL empacotada. Muito menor que
# instalar o LLVM inteiro (~350 MB) so para o bindgen rodar.
$libclang = Join-Path $vendor 'libclang'
if (Test-Path (Join-Path $libclang 'libclang.dll')) {
    Write-Host "libclang ja esta em $libclang"
} else {
    Write-Host 'obtendo libclang.dll...'
    python -m pip install --quiet --upgrade libclang
    $dll = python -c "import clang.cindex, os; print(os.path.join(os.path.dirname(clang.cindex.__file__), 'native', 'libclang.dll'))"
    if (-not (Test-Path $dll)) { throw "nao achei libclang.dll em $dll" }
    New-Item -ItemType Directory -Force -Path $libclang | Out-Null
    Copy-Item $dll $libclang
    Write-Host "libclang pronto em $libclang"
}

Write-Host ''
Write-Host 'pronto — `cargo build` ja acha tudo via .cargo/config.toml'
