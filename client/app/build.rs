fn main() {
    tauri_build::build();
}

// # Por que NÃO há `/DELAYLOAD` aqui
//
// A ideia natural seria adiar a carga das DLLs da libavcodec para a primeira
// chamada, e assim o mesmo `tela.exe` abrir com ou sem elas. Foi tentado e não
// funciona: o link.exe aceita `/DELAYLOAD:avcodec-62.dll` na linha de comando e
// o descarta em silêncio (`LNK4199: nenhuma importação encontrada`, visível só
// com `/WX`), porque o Rust chama as funções pelo thunk que a biblioteca de
// importação fornece, e não por `__imp_`. O linker só sabe adiar as segundas.
//
// Enquanto isso não for resolvido — resolvendo os símbolos com `GetProcAddress`
// em vez de linkar —, a compilação COM a feature `encoder-d3d11` gera um
// binário que exige as DLLs ao lado, e por isso é empacotado à parte por
// `scripts/pacote-gpu.ps1`. A build que vai para todo mundo não liga a feature
// e continua sendo um arquivo só.
