# tela' — Compartilhamento de tela self-hosted

**Data:** 2026-08-19
**Status:** Servidor e motor de captura em produção; UI em construção
**Revisões:** §4.2, §6 e §9 revisados em 2026-08-19 — assistir migrou para o navegador
**Domínio:** `tela.example.com`

---

## 1. Problema

O compartilhamento de tela do Discord parou de funcionar de forma confiável para o grupo de amigos no Brasil. O grupo usa a tela para três coisas: assistir partidas de esporte, mostrar jogos, e trabalhar junto.

O Discord continua servindo bem para **voz, chat e presença**. O que falta é só **pixel**. Este projeto entrega apenas isso.

## 2. Objetivo

Uma sala única e permanente, protegida por senha, onde de 3 a 8 pessoas entram, veem quem está lá, e podem compartilhar tela — com áudio restrito ao aplicativo escolhido.

### Critérios de sucesso

- Latência de ponta a ponta abaixo de 1 segundo (esporte e jogo exigem reação em tempo real na call de voz paralela).
- Até 3 telas simultâneas com 8 pessoas na sala.
- Compartilhar uma janela envia **apenas o áudio daquele aplicativo**; compartilhar um monitor inteiro envia o áudio do sistema.
- Entrar é: abrir o site, digitar senha e apelido. Nada além disso, e sem
  instalar nada.
- Quem **transmite** usa um app Windows **portátil**: um único executável, sem
  instalação, sem admin, sem registro.

## 3. Não-objetivos

Explicitamente fora de escopo. Cada item abaixo foi considerado e cortado:

- **Voz e câmera** — o Discord já faz.
- **Chat de texto** — idem.
- **Gerenciamento de salas** — existe uma sala, só uma, sempre a mesma.
- **Contas, OAuth, cadastro** — uma senha compartilhada, definida por variável de ambiente.
- **Gravação, replay, histórico.**
- **Controle remoto do mouse/teclado** — isto não é Parsec nem RustDesk.
- **Transmitir de macOS/Linux/mobile** — o motor de captura é Win32. *Assistir*
  funciona em qualquer plataforma, porque acontece no navegador (§6).

---

## 4. Decisões técnicas fundamentais

### 4.1 Não é uma Discord Activity

Foi a primeira hipótese e foi descartada por três bloqueios, em ordem de gravidade:

1. **WebRTC não é suportado em Activities.** A documentação oficial de networking do Discord afirma isso literalmente. Todo o tráfego passa por um proxy em Cloudflare Workers e apenas WebSocket e HTTPS funcionam. Sem WebRTC não há caminho viável para vídeo em tempo real.
2. **`getDisplayMedia` em iframe cross-origin** exige que o embedder conceda `allow="display-capture"`. O default da Permissions Policy é `self` e não há indício de que o Discord conceda isso a Activities de terceiros. No client desktop (Electron), o seletor de janela é implementação própria do Discord, não exposta ao iframe.
3. A alternativa remanescente — empurrar frames por WebSocket — teria latência, custo de CPU e qualidade inaceitáveis.

**Consequência:** o produto é um app próprio, fora do Discord. O app circula pelo chat do Discord.

### 4.2 Transmitir exige código nativo Windows

O requisito de **áudio por aplicativo** é o único motivo, e é suficiente. Nenhum navegador consegue: o Chrome no Windows oferece áudio de *aba* ou do *sistema inteiro*, nunca de um processo arbitrário. Capturar o áudio de um jogo específico exige `ActivateAudioInterfaceAsync` com `AUDIOCLIENT_ACTIVATION_PARAMS` / `PROCESS_LOOPBACK` — API Win32.

Benefício secundário: captura de janela via **Windows Graphics Capture** funciona com jogos em modo borderless, onde `getDisplayMedia` frequentemente entrega tela preta em fullscreen exclusivo.

**Alcance desta decisão:** vale só para quem transmite. *Assistir* não toca em
nenhuma dessas APIs e por isso vive no navegador — ver §6.

### 4.3 SFU, não P2P mesh

Com 8 pessoas na sala, uma malha P2P obrigaria quem compartilha a subir a mesma tela 7 vezes. Um SFU na VPS significa subir **1 vez**; o servidor distribui. A VPS está em São Paulo (Oracle `sa-saopaulo-1`), então o RTT até o grupo é baixo.

### 4.4 MediaMTX como SFU

Avaliados: MediaMTX, LiveKit, Jitsi Meet.

| | MediaMTX | LiveKit | Jitsi |
|---|---|---|---|
| RAM | ~30 MB | ~300 MB | ~1,5–2 GB |
| Dependências | zero (binário Go) | zero (binário Go) | Prosody + Jicofo + JVB (JVM) |
| Simulcast / bitrate adaptativo | não | **sim** | sim |
| Ingest de OBS sem container extra | **sim** | não (`livekit-ingress`) | não |
| Frontend necessário | próprio | próprio (tem lib React) | nenhum |

**Escolhido: MediaMTX.** A VPS já hospeda outros serviços — leveza importa. A ausência de simulcast é aceitável para 3–8 amigos em fibra, e é o único que aceita OBS nativamente como fonte alternativa (escape hatch se a captura própria falhar em algum caso).

**Gatilho de migração para LiveKit:** se na prática alguém em conexão fraca degradar a experiência de todos, a troca é do servidor, não do conceito — cliente e protocolo (WHIP/WHEP) permanecem.

### 4.5 FFmpeg faz encode e transporte

O muxer **WHIP** foi mergeado no FFmpeg em junho/2025 e está disponível no FFmpeg 8.x. Isso elimina do escopo próprio: encoder H.264, aceleração por hardware, empacotamento RTP, negociação WebRTC e DTLS.

O código próprio fica restrito a **capturar** e **alimentar os pipes**. Esta decisão foi o que derrubou a estimativa de semanas para dias.

---

## 5. Arquitetura — Servidor

Dois containers em `/srv/`, somando ~100 MB de RAM.

### 5.1 `mediamtx`

- Ingest **WHIP**, playback **WHEP**. WebRTC puro, latência sub-segundo.
- Um *path* por transmissor: `tela/<streamId>`, com `streamId` aleatório emitido pelo serviço `sala`.
- Autenticação externa via HTTP apontando para `sala` (`authMethod: http`).
- Multiplexação numa **única porta UDP** — evita abrir faixa de portas.

### 5.2 `sala`

Serviço pequeno (~200 linhas) em **Go** — binário estático único, compilação ARM64 trivial, mesma stack do MediaMTX. Faz exatamente quatro coisas:

1. **Porta de entrada** — valida a senha única (variável de ambiente), emite um token de sessão de curta duração.
2. **Presença** — mantém um WebSocket com cada cliente: apelido, papel (transmitindo / assistindo / ocioso), ping.
3. **Medição de ping** — ping/pong no WebSocket já aberto, para todos os participantes uniformemente. Ver §8.
4. **Autorizador do MediaMTX** — responde ao webhook de auth externo validando o token da sessão e o path.

Sem banco de dados. Estado inteiro em memória: se o serviço reiniciar, todo mundo reconecta.

### 5.3 `caddy`

Bloco novo para `tela.example.com`, TLS via DNS-01 (padrão já estabelecido no projeto), com proxy para os dois serviços:

- `/whip/*`, `/whep/*` → `mediamtx`
- `/ws`, `/api/*` → `sala`

> **Atenção — gotcha documentado do projeto:** editar `o Caddyfile` com `sed -i` cria um novo inode e deixa o bind-mount stale. Sempre `docker restart caddy` depois de editar.

### 5.4 Rede

Requer **uma porta UDP** aberta para o WebRTC.

> **Atenção — gotcha documentado do projeto:** abrir porta externa na VM Oracle exige **dois lugares**: `iptables -I INPUT ... --dport <p> -j ACCEPT` + `netfilter-persistent save` na VM, **e** a Security List no console OCI (`sa-saopaulo-1`). A VCN tem mais de uma Security List; a subnet da VM é `10.0.1.0/24` e a regra precisa ir na lista anexada a **essa** subnet. Não há `oci-cli` na VM — passo manual no console.

---

## 6. Arquitetura — Cliente

**Revisado em 2026-08-19.** O desenho original punha tudo no app Tauri. Ao
implementar ficou claro que isso cobra um preço sem receber nada: **assistir
não precisa de código nativo**. WHEP é `RTCPeerConnection` + `<video>`, que
qualquer navegador faz — foi assim que o stream foi validado nos testes.

Só **compartilhar** exige nativo, por causa do áudio por processo e da captura
de janela. Logo o cliente é assimétrico:

| Papel | Onde vive | Instala algo? |
|---|---|---|
| **Sala + assistir** | `tela.example.com` no navegador | **não** |
| **Compartilhar** | App Windows (Tauri) | sim |

O que isso ganha:

- Quem só quer assistir **abre um link**. Sem download, sem aviso do SmartScreen
  (R6 deixa de afetar a maioria das pessoas), sem atrito.
- Funciona em **celular, Mac, Linux, tablet**. Só quem transmite precisa ser
  Windows.
- O app encolhe muito: sem grid, sem lista, sem painel. Vira uma janela de
  status de transmissão. Menos código e menos superfície de falha.
- A UI rica vive na página, onde iterar é barato. O Tauri renderiza HTML, então
  o CSS do design system serve aos dois.

Os dois lados falam o **mesmo serviço `sala`** e o mesmo protocolo WebSocket
(§10), sem duplicação.

**Considerado e adiado:** compartilhar pelo navegador via `getDisplayMedia`
funcionaria como plano B, mas sem áudio isolado e com risco de tela preta em
jogo fullscreen — exatamente o que motiva o app existir. Só entra se fizer falta.

### 6.1 Sala e assistir — no navegador

Servida pelo próprio `sala`. Senha, apelido, lista de participantes, grid de
telas, telemetria. `RTCPeerConnection` + `<video>` contra o endpoint WHEP.
Nenhum código nativo envolvido.

### 6.2 Compartilhar — o app Windows

Janela pequena, só o necessário para transmitir: entrar na sala, escolher a
fonte, ajustar qualidade, e ver o estado do envio. Quem quiser assistir abre a
página no navegador, mesmo estando transmitindo.

```
┌─ Rust ──────────────────────────────┐
│  WGC (windows-capture)              │──▶ named pipe (NV12) ──┐
│  WASAPI loopback (wasapi crate)     │──▶ named pipe (s16le) ─┤
└─────────────────────────────────────┘                        │
                                                               ▼
                                              ffmpeg -f whip ──▶ MediaMTX
```

**Vídeo** — crate `windows-capture` (Windows Graphics Capture). O usuário escolhe um monitor ou uma janela específica de uma lista. Conversão para NV12 na callback de captura, antes do pipe.

**Áudio** — crate `wasapi`, que já expõe loopback por PID com `include_tree` (captura o processo alvo e seus filhos):

| Fonte escolhida | Áudio capturado |
|---|---|
| Janela específica | **Somente o PID daquela janela** e processos filhos |
| Monitor inteiro | Loopback do dispositivo de render padrão (áudio do sistema) |

Esta é a regra central do produto e não deve ser afrouxada.

**Encode e publicação** — `ffmpeg.exe` embutido, alimentado por dois named pipes do Windows.

Seleção de encoder em cascata, detectada em runtime: `h264_nvenc` → `h264_qsv` → `h264_amf` → `libx264` (fallback software).

Forma do comando:

```
ffmpeg -hide_banner
  -use_wallclock_as_timestamps 1 -f rawvideo -pix_fmt nv12 -s <W>x<H> -r <FPS> -i <pipe_video>
  -use_wallclock_as_timestamps 1 -f s16le -ar 48000 -ac 2            -i <pipe_audio>
  -c:v <encoder> -rc cbr -b:v <BITRATE> -maxrate <BITRATE> -bufsize <BITRATE/4> -g <FPS*2>
  -c:a libopus -b:a 128k -ar 48000
  -progress pipe:1
  -f whip https://tela.example.com/whip/<streamId>?token=<token>
```

Os pipes são named pipes do Windows no formato `\\.\pipe\noch_tela_v` e `\\.\pipe\noch_tela_a`, com sufixo único por sessão para permitir instâncias simultâneas.

### 6.3 Portabilidade

O cliente é **um único executável, sem instalação**. Requisito, não preferência.

| Ponto que normalmente exige instalador | Como é resolvido |
|---|---|
| `ffmpeg.exe` como binário externo | Embutido como recurso no `.exe`, extraído para pasta temporária no primeiro uso |
| Configurações do usuário | `tela-config.json` **ao lado do executável**, nunca em `AppData` |
| Associações e chaves de registro | Nenhuma é criada |
| Privilégio elevado | Não requerido — WGC e loopback por processo funcionam com usuário comum |

Se o diretório do executável não for gravável (por exemplo, rodando de um pendrive protegido), o app cai para `%LOCALAPPDATA%` e avisa na UI.

**Única dependência externa: WebView2 runtime.** Vem de fábrica no Windows 11 e em qualquer Windows 10 com Edge atual — na prática, universal. O app verifica na inicialização e, se ausente, exibe o link do instalador oficial da Microsoft em vez de falhar em silêncio. Ver R8.

---

## 7. Configuração de compartilhamento

Três controles, expostos antes de iniciar a transmissão e ajustáveis durante:

- **Resolução** — com downscale. Permite compartilhar um monitor 1440p em 1080p, economizando banda sem trocar de monitor.
- **FPS** — 30 para trabalho e apresentação, 60 para esporte e jogo.
- **Bitrate** — o controle que mais afeta a qualidade percebida. Resolução alta com bitrate baixo vira borrão em cena de futebol; 1080p a 8 Mbps fica nítido.

Presets sugeridos como ponto de partida: **Trabalho** (1080p / 30 / 3 Mbps), **Esporte** (1080p / 60 / 8 Mbps), **Jogo** (1080p / 60 / 12 Mbps).

---

## 8. Telemetria

Painel visível na sala, alimentado por três fontes distintas:

| Métrica | Para quem | Origem |
|---|---|---|
| **Ping** | Todos os participantes | ping/pong no WebSocket do `sala` |
| **kbps enviados, fps real, frames dropados** | Quem transmite | `-progress` do ffmpeg, parseado pelo Rust |
| **kbps recebidos, perda de pacote** | Quem assiste | `getStats()` do `RTCPeerConnection` |

**Por que o ping vem do WebSocket e não da mídia:** o muxer WHIP do FFmpeg não expõe RTT. Medir pelo WebSocket que todo mundo já mantém aberto é mais simples, funciona igual para transmissor e espectador, e mede exatamente o que importa — a latência até a VPS.

**Uso derivado:** se `frames dropados` subir, a UI avisa quem transmite para baixar fps ou bitrate. Isso transforma telemetria em ação, em vez de número decorativo.

---

## 9. Interface

### 9.1 Direção visual

**Ergonomia do Discord, material da noch'.** Clonamos o esqueleto que o grupo já tem no dedo, não a pele.

Do Discord vem a **arquitetura de informação**: coluna de participantes à esquerda com indicador de status, palco central, barra de controle inferior fixa.

Do `DESIGN SYSTEM.md` vem tudo que é visual. Onde os dois conflitam, **o padrão noch' vence**:

| Aspecto | Discord | Aqui |
|---|---|---|
| Cantos | arredondados (8px, pills) | **0px, sempre** |
| Separadores | linhas de 1px | **mudança de cor de superfície** |
| Acento | blurple `#5865F2` | **lavanda `#c3c0ff`** |
| Fundo | cinza-azulado `#313338` | **obsidiana `#131313`** |
| Tipografia | gg sans | **Space Grotesk** (títulos) + **Inter** (corpo) |

A transição de acento é natural: blurple e lavanda são ambos roxo.

Camadas de superfície conforme o design system: `surface` (#131313) no fundo, `surface-container-low` (#1c1b1b) nas seções aninhadas, `surface-container-high` (#2a2a2a) nos cards ativos. Glow lavanda difuso (blur 40–60px, 6% de opacidade) no card de quem está transmitindo — nunca sombra cinza.

Números de telemetria usam o estilo **Data Tag**: chip retangular, `surface-bright`, texto `on-surface-variant`, aparência de número de série de techwear.

Matte Orange (`#ffb77d`) fica reservado ao seu papel legítimo: aviso de frames dropados ou conexão degradada.

### 9.2 Layout do palco

**Grid por padrão**, com todas as telas ativas visíveis simultaneamente. Clicar em uma promove ela a **foco**, e as demais viram miniaturas.

São o mesmo componente com um toggle de layout — não duas telas diferentes.

Quem está transmitindo **não** recebe o próprio stream de volta pelo WHEP: seria banda desperdiçada e meio segundo de eco visual. No lugar, vê um card local com prévia da captura e a telemetria de envio.

### 9.3 Fluxos

**Entrar:** abre `tela.example.com` → senha + apelido → sala. Sem instalar
nada, em qualquer dispositivo.

**Compartilhar:** baixa e abre o app Windows → mesma senha e apelido → escolhe
monitor ou janela numa lista com miniaturas → ajusta preset se quiser → transmite. O indicador de áudio mostra explicitamente o que está sendo capturado ("Áudio de: `eldenring.exe`" ou "Áudio do sistema"), para que ninguém vaze áudio sem perceber.

**Assistir:** clica em quem está transmitindo no grid.

---

## 10. Protocolo do serviço `sala`

WebSocket, mensagens JSON.

**Cliente → servidor**

| Mensagem | Carga |
|---|---|
| `hello` | `{password, nick}` |
| `pong` | `{t}` |
| `publish_start` | `{sourceType, sourceTitle, settings:{w,h,fps,kbps}}` |
| `publish_stop` | — |
| `publish_stats` | `{kbps, fps, dropped}` |
| `watch_start` / `watch_stop` | `{streamId}` |

**Servidor → cliente**

| Mensagem | Carga |
|---|---|
| `welcome` | `{selfId, token, participants[], streams[]}` |
| `denied` | `{reason}` |
| `state` | `{participants[], streams[]}` — broadcast a cada mudança |
| `stream_ready` | `{streamId, whepUrl}` — resposta ao `publish_start` |
| `ping` | `{t}` |

**Modelos**

```
Participant = {id, nick, pingMs, sharing: streamId|null, watching: [streamId]}
Stream      = {id, ownerId, sourceType, sourceTitle, settings, stats:{kbps,fps,dropped}}
```

---

## 11. Riscos e mitigações

Os quatro primeiros são de implementação e precisam ser resolvidos no código. Os demais são limitações aceitas.

### R1 — WGC não entrega frames em tela estática *(alto)*

O Windows Graphics Capture é orientado a evento: se nada muda na tela, nenhum frame chega. O pipe do ffmpeg estagna e a sincronia A/V quebra.

**Mitigação:** o loop de captura **repete o último frame** na cadência do FPS alvo quando não há frame novo.

### R2 — Aplicativo silencioso trava o pipe de áudio *(alto)*

O loopback por processo não produz amostras quando o app não está tocando nada. Mesmo efeito de R1, no áudio.

**Mitigação:** gerar **silêncio** na cadência do relógio de áudio quando não há amostras reais.

### R3 — Sincronia A/V entre dois pipes *(médio)*

São duas entradas independentes chegando no ffmpeg.

**Mitigação:** `-use_wallclock_as_timestamps 1` nas duas entradas, mais R1 e R2 garantindo fluxo contínuo. Validar cedo, no spike.

### R4 — Espectador que chega tarde espera keyframe *(médio)*

Não há garantia de que o muxer WHIP do FFmpeg reaja a PLI vindo do MediaMTX. Sem isso, quem entra no meio da transmissão fica com tela preta até o próximo IDR.

**Mitigação:** GOP curto — `-g <FPS*2>`, ou seja, keyframe a cada 2 segundos. Custa um pouco de bitrate e resolve o problema de forma determinística.

### R5 — Banda do pipe de vídeo *(baixo)*

1080p60 em BGRA cru são ~500 MB/s. Convertendo para NV12 na captura cai para ~190 MB/s, tranquilo para um pipe local.

### R6 — SmartScreen no executável sem assinatura *(aceito)*

O `.exe` não será assinado (certificado custa ~US$200/ano). Cada amigo verá o aviso do SmartScreen na primeira execução e precisará de "Mais informações → Executar assim mesmo". Documentar no passo a passo da Fase 5.

### R7 — Windows anterior ao build 20348 *(aceito)*

O loopback por processo exige **Windows 10 build 20348+ ou Windows 11**. Em versões anteriores, o app detecta e cai para áudio do sistema, avisando o usuário explicitamente na UI.

### R8 — WebView2 ausente *(baixo, aceito)*

Presente de fábrica no Windows 11 e em Windows 10 com Edge atual. Se ausente, o app detecta na inicialização e exibe o link do instalador oficial da Microsoft, em vez de falhar em silêncio.

---

## 12. Fases de entrega

**Fase 0 — Spike de validação.** Um executável feio, sem UI, que captura uma janela e o áudio dela e publica num MediaMTX rodando local. Prova R1, R2 e R3 antes de qualquer investimento em UI. Se falhar, pivotamos para OBS portátil controlado por `obs-websocket` sem ter perdido tempo.

**Fase 1 — Servidor.** MediaMTX + `sala` + bloco no Caddy + porta UDP (iptables e Security List OCI).

**Fase 2 — Cliente, assistir.** Tauri, shell da UI, WebSocket de presença, WHEP na WebView2. Testável contra um stream publicado à mão por OBS.

**Fase 3 — Cliente, compartilhar.** Captura nativa, seletor de fonte, regra de áudio, ffmpeg embutido.

**Fase 4 — Config e telemetria.** Presets, controles de qualidade, painel de ping/kbps/drops.

**Fase 5 — Empacotamento portátil.** Executável único com ffmpeg embutido, config ao lado do binário, e instruções para os amigos incluindo o passo do SmartScreen.

---

## 13. Referências

- [Discord — Activities networking (WebRTC não suportado)](https://docs.discord.com/developers/activities/development-guides/networking)
- [MDN — Permissions-Policy: display-capture](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Permissions-Policy/display-capture)
- [Microsoft — Application loopback audio capture (sample)](https://learn.microsoft.com/en-us/samples/microsoft/windows-classic-samples/applicationloopbackaudio-sample/)
- [Microsoft — PROCESS_LOOPBACK_MODE](https://learn.microsoft.com/en-us/windows/win32/api/audioclientactivationparams/ne-audioclientactivationparams-process_loopback_mode)
- [crate `wasapi`](https://docs.rs/wasapi/latest/wasapi/struct.AudioClient.html)
- [Phoronix — WHIP muxer mergeado no FFmpeg](https://www.phoronix.com/news/FFmpeg-Lands-WHIP-Muxer)
- [MediaMTX](https://github.com/bluenviron/mediamtx)
- `DESIGN SYSTEM.md` na raiz do repositório
