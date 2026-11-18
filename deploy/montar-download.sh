#!/usr/bin/env bash
# Monta a pasta download/ que a sala serve para o app: o executável, o ffmpeg,
# as DLLs da libavcodec e o manifesto de versão que o app consulta para se
# atualizar. Rode depois de compilar o app; depois copie download/ para o
# servidor, ao lado do docker-compose.yml.
#
# O manifesto tem de descrever exatamente os arquivos desta pasta, por isso é
# gerado aqui em vez de escrito à mão.
set -euo pipefail

RAIZ="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EXE="$RAIZ/client/target/release/tela-app.exe"
# Um ffmpeg 8.x estático (LGPL), que já traz o muxer WHIP. Ex.: o
# ffmpeg-n8.1-latest-win64-lgpl-8.1.zip das releases de BtbN/FFmpeg-Builds.
FFMPEG="${FFMPEG:-$RAIZ/client/bin/ffmpeg.exe}"
# Só estas três DLLs: o encoder em processo não chama nenhuma outra.
LIBAV_DIR="$RAIZ/client/vendor/ffmpeg/bin"
LIBAV_BASES=(avcodec- avutil- swresample-)
DESTINO="$RAIZ/download"

[[ -f "$EXE" ]] || { echo "não achei $EXE — compile antes"; exit 1; }
[[ -f "$FFMPEG" ]] || { echo "não achei $FFMPEG — o app precisa dele como recuo"; exit 1; }
mkdir -p "$DESTINO"

versao=$(grep -m1 '^version' "$RAIZ/client/Cargo.toml" | sed -E 's/.*"(.*)".*/\1/')
entrada() { echo "{ \"arquivo\": \"$1\", \"bytes\": $(stat -c %s "$2"), \"sha256\": \"$(sha256sum "$2" | cut -d' ' -f1)\" }"; }

cp "$EXE" "$DESTINO/tela.exe"
cp "$FFMPEG" "$DESTINO/ffmpeg.exe"

libav=""
for base in "${LIBAV_BASES[@]}"; do
  for arq in "$LIBAV_DIR/$base"*.dll; do
    [[ -f "$arq" ]] || continue
    nome=$(basename "$arq")
    cp "$arq" "$DESTINO/$nome"
    [[ -n "$libav" ]] && libav+=","
    libav+=$'\n    '"$(entrada "$nome" "$DESTINO/$nome")"
  done
done

# Manifesto por último: até ele existir, o app não anuncia a versão nova.
cat > "$DESTINO/versao.json" <<JSON
{
  "versao": "$versao",
  "app": $(entrada tela.exe "$DESTINO/tela.exe"),
  "ffmpeg": $(entrada ffmpeg.exe "$DESTINO/ffmpeg.exe"),
  "libav": [$libav
  ]
}
JSON

echo "download/ pronto: versão $versao"
ls -lh "$DESTINO"
