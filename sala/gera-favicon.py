#!/usr/bin/env python3
"""Gera os PNGs do favicon a partir da mesma geometria do `favicon.svg`.

A fonte da verdade é o SVG; estes PNGs existem só como reserva para quem não
aceita favicon vetorial (Safari antigo) e para o atalho de tela inicial no iOS.

Roda sem dependência nenhuma: escreve PNG na mão com `zlib`. Instalar Pillow
para desenhar dois polígonos seria desproporcional, e a máquina que gera isto é
a de desenvolvimento, não um servidor de build.

    python sala/gera-favicon.py
"""

import os
import struct
import zlib

FUNDO = (0x13, 0x13, 0x13)
TRACO = (0xC3, 0xC0, 0xFF)

# O quadrilátero do apóstrofo, em coordenadas de uma caixa 32x32 — os mesmos
# números do `favicon.svg`. Extraídos de `client/app/icons/bandeja.png`.
CAIXA = 32.0
VERTICES = [(14.0, 6.0), (20.0, 6.0), (14.0, 20.0), (11.0, 20.0)]

# Supersample: cada pixel vira NxN amostras e a média vira a cor. É o que dá a
# borda suave da diagonal — sem isso o traço fica serrilhado a 16px, que é o
# tamanho em que o favicon mais aparece.
AMOSTRAS = 4


def dentro(px, py):
    """O ponto está dentro do quadrilátero? Regra do número de cruzamentos."""
    dentro_ = False
    n = len(VERTICES)
    for i in range(n):
        x1, y1 = VERTICES[i]
        x2, y2 = VERTICES[(i + 1) % n]
        if (y1 > py) != (y2 > py):
            corte = x1 + (py - y1) * (x2 - x1) / (y2 - y1)
            if px < corte:
                dentro_ = not dentro_
    return dentro_


def desenha(lado):
    escala = CAIXA / lado
    linhas = []
    for y in range(lado):
        linha = bytearray()
        for x in range(lado):
            acertos = 0
            for sy in range(AMOSTRAS):
                for sx in range(AMOSTRAS):
                    px = (x + (sx + 0.5) / AMOSTRAS) * escala
                    py = (y + (sy + 0.5) / AMOSTRAS) * escala
                    if dentro(px, py):
                        acertos += 1
            t = acertos / (AMOSTRAS * AMOSTRAS)
            for c in range(3):
                linha.append(round(FUNDO[c] + (TRACO[c] - FUNDO[c]) * t))
            linha.append(255)
        linhas.append(bytes(linha))
    return linhas


def escreve_png(caminho, lado, linhas):
    cru = b"".join(b"\x00" + l for l in linhas)  # filtro 0 em cada linha

    def bloco(tipo, dados):
        return (struct.pack(">I", len(dados)) + tipo + dados
                + struct.pack(">I", zlib.crc32(tipo + dados) & 0xFFFFFFFF))

    png = (b"\x89PNG\r\n\x1a\n"
           + bloco(b"IHDR", struct.pack(">IIBBBBB", lado, lado, 8, 6, 0, 0, 0))
           + bloco(b"IDAT", zlib.compress(cru, 9))
           + bloco(b"IEND", b""))
    with open(caminho, "wb") as f:
        f.write(png)
    print("%s (%dx%d, %d bytes)" % (os.path.basename(caminho), lado, lado, len(png)))


if __name__ == "__main__":
    # Sai em `web/`, mas o script NAO mora la: tudo dentro de `web/` e servido
    # publicamente pelo FileServer, e ferramenta de desenvolvimento nao tem por
    # que estar no ar.
    destino = os.path.join(os.path.dirname(os.path.abspath(__file__)), "web")
    for nome, lado in [("favicon-32.png", 32), ("apple-touch-icon.png", 180)]:
        escreve_png(os.path.join(destino, nome), lado, desenha(lado))
