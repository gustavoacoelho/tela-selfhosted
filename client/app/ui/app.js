/* ===========================================================================
   tela' — app de transmissão, camada de apresentação.

   Este arquivo NÃO tem estado de verdade: nenhum WebSocket, nenhum caminho de
   arquivo, nenhuma decisão de qualidade. Tudo isso vive no Rust, porque é lá
   que a conexão com a sala, a captura e o ffmpeg vivem — e um estado
   duplicado no JavaScript acabaria discordando do real na pior hora possível.
   Aqui só se pinta o que o Rust manda e se repassa o que a pessoa clicou.
   =========================================================================== */
"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (s) => document.querySelector(s);

/* Os números da qualidade moram no Rust (`escada.rs`), não aqui.
   Duplicá-los criaria duas fontes de verdade para a mesma tabela — e a escada
   muda de degrau sozinha durante a transmissão, então a interface não teria
   como acompanhar de qualquer jeito. Daqui sai só o NOME do preset; o que
   está no ar de fato vem de volta na telemetria. */
const PRESETS_VALIDOS = ["auto", "leve", "qualidade", "nativa"];

const estado = {
  inicial: null,
  aba: "monitor",
  fontes: { monitores: [], janelas: [] },
  escolhida: null,   // {tipo, hwnd, pid, indice, titulo, processo}
  preset: "trabalho",
  pingMs: 0,
  /** `Date.now()` de quando a transmissão começou, para o relógio no ar. */
  inicio: 0,
};

/* ---------------- navegação entre telas ---------------- */

function mostra(id) {
  for (const t of document.querySelectorAll(".tela")) {
    t.classList.toggle("on", t.id === id);
  }
}

function texto(sel, valor) { $(sel).textContent = valor; }

/* O plugin quer o código da tecla física (`NumpadDivide`); ninguém reconhece
   o próprio teclado nisso. Traduz só o que aparece nos padrões — o resto
   passa como está, que é o razoável para um atalho escolhido à mão. */
const TECLAS = {
  NumpadDivide: "/ (numérico)",
  NumpadMultiply: "* (numérico)",
  NumpadSubtract: "- (numérico)",
  NumpadAdd: "+ (numérico)",
};
function legivel(atalho) {
  return atalho.split("+").map((p) => TECLAS[p] || p).join(" + ");
}

/* ---------------- abertura ---------------- */

async function abre() {
  try {
    estado.inicial = await invoke("inicial");
  } catch (e) {
    texto("#erro-entrar", String(e));
    return;
  }
  const i = estado.inicial;

  estado.appDeVoz = i.app_de_voz || null;
  $("#in-sem-eco").checked = i.sem_eco !== false;
  if (estado.appDeVoz) {
    // Nomeia o app encontrado: "não enviar o áudio do Discord.exe" é muito
    // mais claro que uma opção genérica sobre áudio.
    texto("#txt-sem-eco", `Não enviar o áudio do ${estado.appDeVoz.replace(/\.exe$/i, "")}`);
  }
  $("#in-nick").value = i.apelido || "";
  // Senha lembrada vem decifrada do Rust. Vazia quando não há, ou quando o
  // blob veio de outra conta/máquina e o DPAPI recusou — para quem usa, os
  // dois casos são a mesma coisa.
  $("#in-senha").value = i.senha_lembrada || "";
  $("#in-lembrar").checked = !!i.senha_lembrada;
  // Automática é o padrão: é o que menos exige da pessoa saber, e o único que
  // conserta sozinho uma conexão que piora no meio da partida.
  aplicaPreset(PRESETS_VALIDOS.includes(i.preset) ? i.preset : "auto");

  // Onde a configuração foi parar é informação, não enfeite: se caiu no
  // LOCALAPPDATA a pessoa precisa saber que copiar o .exe não leva mais os
  // ajustes junto.
  const nc = $("#nota-config");
  nc.textContent = i.config_fallback
    ? `Sem %LOCALAPPDATA%: guardando ajustes em ${i.caminho_config}, ao lado do programa.`
    : `Ajustes em ${i.caminho_config}`;
  nc.classList.toggle("aviso", i.config_fallback);

  // Caminho do log, clicável para copiar. Quando alguém diz "não funciona
  // aqui", este arquivo é a única coisa que responde por quê — e pedir para a
  // pessoa caçar a pasta sozinha não funciona.
  const nl = $("#nota-log");
  if (nl) {
    nl.textContent = `Log: ${i.caminho_log} (clique para copiar)`;
    nl.onclick = async () => {
      try {
        await navigator.clipboard.writeText(i.caminho_log);
        nl.textContent = "Caminho copiado — mande o arquivo se pedirem ajuda";
        setTimeout(() => { nl.textContent = `Log: ${i.caminho_log} (clique para copiar)`; }, 2500);
      } catch { /* sem clipboard: o caminho continua visível para copiar à mão */ }
    };
  }

  if (!i.audio_por_processo) {
    const na = $("#nota-audio");
    na.className = "nota-linha aviso";
    na.textContent =
      `Este Windows (build ${i.build_windows}) não isola áudio por aplicativo — ` +
      `é preciso o build 20348 ou mais novo. Transmitir uma janela vai levar o ` +
      `som do sistema inteiro junto.`;
  }

  if (!i.ffmpeg_existe) {
    // Sem aviso alarmante: o codificador é baixado sozinho depois de entrar.
    // A pessoa só precisa saber que a primeira vez demora um pouco mais.
    texto("#nota-ffmpeg", "Na primeira vez, o codificador de vídeo é baixado depois de entrar.");
  }

  // Os atalhos funcionam com o app em segundo plano — é o caso de uso
  // inteiro. Anunciá-los aqui, com os valores em vigor, é a única forma de
  // alguém descobrir que existem.
  const at = [];
  if (i.atalho_monitor) at.push(`<b>${legivel(i.atalho_monitor)}</b> transmite o monitor onde está o mouse`);
  if (i.atalho_janela) at.push(`<b>${legivel(i.atalho_janela)}</b> transmite a janela em primeiro plano`);
  if (at.length) $("#dica-atalhos").innerHTML = at.join(" · ") + " — funcionam de dentro do jogo.";

  // Com apelido e senha já preenchidos, o foco vai direto para o botão: não
  // há campo a completar.
  $(i.senha_lembrada ? "#btn-entrar" : i.apelido ? "#in-senha" : "#in-nick").focus();

  // Sem `await`: a consulta tem 10s de timeout e não pode segurar a tela de
  // entrada de quem só quer transmitir numa rede local.
  checaVersao();
}

/* ---------------- atualização ---------------- */

async function checaVersao() {
  try {
    estado.versao = await invoke("checa_versao");
  } catch (e) {
    console.warn("versão:", e);
    return;
  }
  const v = estado.versao;
  const nv = $("#nota-versao");
  if (nv) {
    nv.textContent = v.tem_nova
      ? `Versão ${v.disponivel} disponível (${v.mb} MB) — atualize depois de entrar na sala.`
      : `Versão ${v.atual}`;
    nv.classList.toggle("aviso", !!v.tem_nova);
  }
}

/* O popup de atualização e o de "preparando o codificador" são o mesmo
   elemento em dois papéis: os dois interrompem, os dois mostram progresso, e
   nunca aparecem ao mesmo tempo. */
function abreModalAtt({ titulo, texto: txt, acoes, barra }) {
  texto("#att-titulo", titulo);
  texto("#att-texto", txt);
  texto("#att-erro", "");
  $("#att-barra").hidden = !barra;
  $("#barra-att").style.width = "0%";
  $("#att-acoes").hidden = !acoes;
  $("#modal-att").classList.add("on");
}

function fechaModalAtt() {
  $("#modal-att").classList.remove("on");
}

/* Verifica de novo, de tempos em tempos e em momentos oportunos.
 *
 * A verificação acontecia UMA vez, na tela de entrada. Quem deixa o app aberto
 * por dias nunca ficava sabendo de versão nova: teve gente rodando a 0.5.1
 * enquanto a 0.5.7 estava publicada, e só atualizou porque reiniciou o
 * Windows por outro motivo.
 *
 * O gatilho NÃO é apertar "Transmitir", que era o candidato óbvio e é o pior
 * momento que existe: quem clica ali quer estar no ar agora, geralmente com os
 * amigos esperando, e um popup de atualização com reinício no meio disso vira
 * "depois" para sempre. Os gatilhos são os dois momentos em que a pessoa está
 * no app e sem pressa — o relógio, e o fim de uma transmissão. */
const INTERVALO_CHECAGEM = 30 * 60 * 1000;

async function verificaSeTemVersaoNova() {
  // Nunca durante a transmissão. Nem o popup, nem a consulta: quem está no ar
  // não pode ser interrompido, e a caixa é a mesma que mostra o progresso do
  // download do codificador.
  if ($("#t-ar").classList.contains("on")) return;
  if ($("#modal-att").classList.contains("on")) return;
  // Baixar a versão nova exige o token da sessão, que só existe depois de
  // entrar. Oferecer antes disso seria oferecer o que vai falhar.
  if (!estado.entrou) return;

  await checaVersao();
  const v = estado.versao;
  if (!v || !v.tem_nova) return;
  // Uma oferta por versão. Quem respondeu "depois" já decidiu; repetir a cada
  // meia hora seria cobrança, não aviso. A nota de versão continua na tela.
  if (estado.versaoOferecida === v.disponivel) return;
  estado.versaoOferecida = v.disponivel;
  ofereceAtualizar();
}

setInterval(verificaSeTemVersaoNova, INTERVALO_CHECAGEM);

/* Chamado depois de entrar, porque baixar depende do token da sessão.
   Devolve se chegou a mostrar algo — os dois popups disputam a mesma caixa e
   quem chama precisa saber se a vez é sua. */
function ofereceAtualizar() {
  const v = estado.versao;
  if (!v || !v.tem_nova) return false;
  estado.versaoOferecida = v.disponivel;
  abreModalAtt({
    titulo: `Versão ${v.disponivel} disponível`,
    texto: `Você está na ${v.atual}. São ${v.mb} MB e o app reinicia sozinho ao terminar.`,
    acoes: true,
    barra: false,
  });
  return true;
}

$("#btn-att-depois").addEventListener("click", () => {
  fechaModalAtt();
  // Adiada enquanto o popup de atualização ocupava a caixa. `garanteEncoder`
  // não faz nada se o codificador já estiver aí ou se já tiver falhado.
  garanteEncoder();
});

$("#btn-baixar-versao").addEventListener("click", async () => {
  const b = $("#btn-baixar-versao");
  b.disabled = true;
  b.textContent = "Baixando…";
  $("#att-barra").hidden = false;
  $("#btn-att-depois").disabled = true;
  try {
    // Quando dá certo o app reinicia sozinho e esta linha nunca retorna.
    await invoke("atualizar");
  } catch (e) {
    texto("#att-erro", `Não consegui atualizar: ${e}`);
    b.disabled = false;
    b.textContent = "Tentar de novo";
    $("#btn-att-depois").disabled = false;
  }
});

/* O encoder é obrigatório para transmitir. Baixar aqui, logo depois de entrar,
   evita a pessoa descobrir que falta 140 MB no momento em que clica em
   "Transmitir" — que é justamente quando ela tem menos paciência. */
async function garanteEncoder() {
  if (estado.inicial && estado.inicial.ffmpeg_existe) return;
  // Uma tentativa por sessão: sem isto, fechar o aviso de falha reabriria o
  // popup na mesma hora e a pessoa não conseguiria sair dele.
  if (estado.encoderFalhou) return;
  abreModalAtt({
    titulo: "Preparando o codificador",
    texto: "Só na primeira vez. Depois disso o app abre direto.",
    acoes: false,
    barra: true,
  });
  try {
    await invoke("garante_encoder");
    estado.inicial.ffmpeg_existe = true;
    fechaModalAtt();
  } catch (e) {
    estado.encoderFalhou = true;
    texto("#att-erro", `Falta o codificador de vídeo: ${e}`);
    // Deixa fechar: sem o codificador não dá para transmitir, mas prender a
    // pessoa num popup sem saída é pior — ela ainda pode sair e voltar.
    $("#att-acoes").hidden = false;
    $("#btn-baixar-versao").hidden = true;
    $("#btn-att-depois").textContent = "Fechar";
  }
}

listen("download-progresso", (ev) => {
  const p = ev.payload || {};
  $("#barra-att").style.width = `${p.pct || 0}%`;
  const qual = p.qual === "encoder" ? "o codificador" : "a versão nova";
  texto("#att-texto", `Baixando ${qual}… ${p.pct || 0}% de ${p.mb || 0} MB`);
});

/* ---------------- entrar ---------------- */

$("#form-entrar").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const apelido = $("#in-nick").value.trim();
  const senha = $("#in-senha").value;
  if (!apelido) return;

  texto("#erro-entrar", "");
  $("#btn-entrar").disabled = true;
  $("#btn-entrar").textContent = "Entrando…";
  try {
    await invoke("entrar", { apelido, senha, lembrar: $("#in-lembrar").checked });
    // A senha some da memória da interface assim que serviu.
    $("#in-senha").value = "";
    estado.entrou = true;
    mostra("t-fonte");
    await atualizaFontes();
    // Um popup de cada vez. Havendo atualização, ela vem primeiro e o
    // codificador espera o "Depois" — ou o reinício, que refaz tudo isto.
    if (!ofereceAtualizar()) await garanteEncoder();
  } catch (e) {
    texto("#erro-entrar", String(e));
  } finally {
    $("#btn-entrar").disabled = false;
    $("#btn-entrar").textContent = "Entrar na sala";
  }
});

$("#btn-sair").addEventListener("click", async () => {
  try { await invoke("sair"); } catch (e) { console.warn(e); }
  estado.entrou = false;
  estado.escolhida = null;
  mostra("t-entrar");
});

/* ---------------- lista de fontes ---------------- */

for (const b of document.querySelectorAll(".aba")) {
  b.addEventListener("click", () => {
    estado.aba = b.dataset.aba;
    for (const o of document.querySelectorAll(".aba")) o.classList.toggle("on", o === b);
    pintaFontes();
  });
}

$("#btn-atualizar").addEventListener("click", atualizaFontes);

async function atualizaFontes() {
  try {
    estado.fontes = await invoke("listar_fontes");
    texto("#erro-fonte", "");
  } catch (e) {
    texto("#erro-fonte", String(e));
    return;
  }
  // Reencontra a fonte da sessão passada pelo título: o HWND morre junto com a
  // janela, mas o título é o que a pessoa reconhece.
  if (!estado.escolhida && estado.inicial && estado.inicial.fonte_titulo) {
    const alvo = estado.inicial.fonte_titulo;
    if (estado.inicial.fonte_tipo === "janela") {
      const j = estado.fontes.janelas.find((x) => x.titulo === alvo);
      if (j) { estado.aba = "janela"; escolhe(fonteDeJanela(j)); }
    } else {
      const m = estado.fontes.monitores.find((x) => x.nome === alvo);
      if (m) { estado.aba = "monitor"; escolhe(fonteDeMonitor(m)); }
    }
    for (const o of document.querySelectorAll(".aba")) {
      o.classList.toggle("on", o.dataset.aba === estado.aba);
    }
  }
  pintaFontes();
}

const fonteDeMonitor = (m) => ({
  tipo: "monitor", indice: m.indice, hwnd: "0", pid: 0,
  titulo: m.nome, processo: "", dims: `${m.largura}×${m.altura}`,
});
const fonteDeJanela = (j) => ({
  tipo: "janela", indice: 0, hwnd: j.hwnd, pid: j.pid,
  titulo: j.titulo, processo: j.processo, dims: "",
});

function pintaFontes() {
  const lista = $("#lista-fontes");
  lista.replaceChildren();

  const itens = estado.aba === "monitor"
    ? estado.fontes.monitores.map(fonteDeMonitor)
    : estado.fontes.janelas.map(fonteDeJanela);

  if (itens.length === 0) {
    const vazio = document.createElement("div");
    vazio.className = "lista-vazia";
    vazio.textContent = "Nada aqui. Abra a janela que quer transmitir e atualize a lista.";
    lista.append(vazio);
    return;
  }

  for (const f of itens) {
    const el = document.createElement("button");
    el.className = "item" + (mesmaFonte(f, estado.escolhida) ? " on" : "");
    // textContent em tudo, nunca innerHTML: títulos de janela são texto de
    // terceiros e um `<img onerror>` no título viraria execução aqui dentro.
    const nome = document.createElement("span");
    nome.className = "nome";
    nome.textContent = f.titulo;
    const meta = document.createElement("span");
    meta.className = "meta";
    meta.textContent = f.tipo === "monitor" ? f.dims : f.processo;
    el.append(nome, meta);
    el.addEventListener("click", () => { escolhe(f); pintaFontes(); });
    lista.append(el);
  }
}

function mesmaFonte(a, b) {
  if (!a || !b || a.tipo !== b.tipo) return false;
  return a.tipo === "monitor" ? a.indice === b.indice : a.hwnd === b.hwnd;
}

function escolhe(f) {
  estado.escolhida = f;
  pintaAudio();
  // A altura vem do texto que a lista já mostra (`1920×1080`). É estimativa:
  // a resolução real só se sabe no primeiro quadro, e o Rust decide de novo
  // com o valor verdadeiro. Aqui serve só para não oferecer um botão que não
  // faria nada.
  ajustaPresetNativa(Number(String(f.dims || "").split("×")[1]) || 0);
  $("#btn-transmitir").disabled = false;
  // Só faz sentido no monitor: compartilhando uma janela o áudio já é só o
  // daquele app, e o Discord nunca entra.
  const mostrarSemEco = estado.aba === "monitor" && !!estado.appDeVoz;
  $("#linha-sem-eco").hidden = !mostrarSemEco;
}

/* ---------------- indicador de áudio ----------------
   A regra central do produto: a fonte de VÍDEO decide o escopo do ÁUDIO.
   Mostrar isso antes de transmitir é a diferença entre compartilhar o jogo e
   compartilhar a conversa do Discord junto sem perceber. */

function pintaAudio() {
  const bloco = $("#bloco-audio");
  const alvo = $("#audio-alvo");
  const explica = $("#audio-explica");
  const aviso = $("#audio-aviso");
  const f = estado.escolhida;

  bloco.className = "audio";
  aviso.textContent = "";

  if (!f) {
    alvo.textContent = "Escolha uma fonte acima";
    explica.textContent =
      "Janela envia só o som daquele aplicativo. Monitor envia o som do sistema inteiro.";
    return;
  }

  if (f.tipo === "monitor") {
    bloco.className = "audio sistema";
    alvo.textContent = "Áudio do sistema inteiro";
    explica.textContent =
      "Tudo que sair pelos alto-falantes vai junto — inclusive Discord, notificações e vídeo em outra aba.";
    return;
  }

  if (estado.inicial && !estado.inicial.audio_por_processo) {
    bloco.className = "audio sistema";
    alvo.textContent = "Áudio do sistema inteiro";
    explica.textContent = `Você escolheu a janela de ${f.processo || "este aplicativo"}.`;
    aviso.textContent =
      "Este Windows não sabe isolar áudio por aplicativo, então o som do sistema inteiro vai junto.";
    return;
  }

  bloco.className = "audio definido";
  alvo.textContent = `Áudio de: ${f.processo || "este aplicativo"}`;
  explica.textContent =
    "Só o som deste aplicativo (e dos processos filhos dele). Nada mais do sistema vaza.";
}

/* ---------------- qualidade ---------------- */

for (const b of document.querySelectorAll(".preset")) {
  b.addEventListener("click", () => aplicaPreset(b.dataset.preset));
}

function aplicaPreset(nome) {
  estado.preset = nome;
  for (const b of document.querySelectorAll(".preset")) {
    b.classList.toggle("on", b.dataset.preset === nome);
  }
}

/* "Nativa" só faz sentido quando a fonte é maior que 1080p.
   Abaixo disso ela É o preset de Qualidade — a escala nunca amplia, então os
   dois mandam exatamente os mesmos pixels, e o botão só ofereceria gastar 50%
   mais banda à toa. Botão que não faz nada confunde mais do que ajuda. */
function ajustaPresetNativa(alturaDaFonte) {
  const b = $("#preset-nativa");
  const cabe = alturaDaFonte > 1080;
  b.hidden = !cabe;
  if (!cabe && estado.preset === "nativa") aplicaPreset("qualidade");
}

/* ---------------- transmitir ---------------- */

/* O caminho único para entrar no ar, usado pelo botão e pelos atalhos globais.
   Ter dois caminhos separados foi como o `semEco` ficou meses divergindo entre
   um lugar e outro sem ninguém notar. */
async function transmiteFonte(f) {
  if (!f) return false;
  // O anti-eco só faz sentido no monitor; para janela o áudio já é só o do
  // app. Calculado UMA vez e reaproveitado na tela do ar: enquanto o pedido e
  // o texto eram deduzidos separadamente, a tela dizia "sistema inteiro" com
  // a exclusão ligada — e vice-versa.
  const semEco = f.tipo === "monitor" && $("#in-sem-eco").checked;

  texto("#erro-fonte", "");
  $("#btn-transmitir").disabled = true;
  $("#btn-transmitir").textContent = "Preparando…";
  try {
    await invoke("transmitir", {
      pedido: {
        tipo: f.tipo, hwnd: f.hwnd, pid: f.pid, indice: f.indice, titulo: f.titulo,
        // Só o NOME do preset. Resolução, fps e taxa são decididos no Rust,
        // que é quem também vai mudá-los sozinho durante a transmissão.
        preset: estado.preset,
        semEco,
      },
    });
    estado.escolhida = f;
    estado.trocando = false;
    $("#btn-voltar-ar").hidden = true;
    preparaTelaDoAr(f, semEco);
    mostra("t-ar");
    return true;
  } catch (e) {
    texto("#erro-fonte", String(e));
    return false;
  } finally {
    $("#btn-transmitir").disabled = false;
    $("#btn-transmitir").textContent = estado.trocando ? "Trocar para esta" : "Transmitir";
  }
}

$("#btn-transmitir").addEventListener("click", () => transmiteFonte(estado.escolhida));

/* ---------------- atalhos globais ---------------- */

/* O Rust dispara isto quando a tecla é apertada, esteja o app onde estiver.
   A resolução da fonte fica lá (é Win32); a qualidade, o anti-eco e a
   navegação ficam aqui, para o atalho passar exatamente pelo mesmo caminho
   do botão. */
listen("atalho", async (ev) => {
  const qual = ev.payload;
  if (!estado.entrou) {
    // Sem sala não há para onde transmitir. Traz a janela à frente para a
    // pessoa ver o motivo, em vez de ficar apertando a tecla no vazio.
    try { await invoke("aparece"); } catch { /* sem janela, sem o que fazer */ }
    return;
  }
  try {
    const f = await invoke("fonte_do_atalho", { qual });
    // O anti-eco só existe para monitor; ligar a caixa aqui evita que o
    // atalho mande a call junto por a aba estar em "Janelas".
    if (f.tipo === "monitor" && estado.appDeVoz) $("#in-sem-eco").checked = true;
    await transmiteFonte(f);
  } catch (e) {
    try { await invoke("aparece"); } catch { /* idem */ }
    texto("#erro-fonte", String(e));
    mostra("t-fonte");
  }
});

function preparaTelaDoAr(f, semEco) {
  // O aviso de CPU é reposto pelo evento, se for o caso desta transmissão.
  $("#aviso-cpu").hidden = true;
  estado.inicio = Date.now();
  texto("#ar-titulo", f.titulo);
  // `semEco` vem de quem montou o pedido, não é rededuzido daqui: a tela do ar
  // tem de dizer o que está indo DE FATO.
  const voz = (estado.appDeVoz || "").replace(/\.exe$/i, "");
  texto("#ar-audio", f.tipo === "monitor" || (estado.inicial && !estado.inicial.audio_por_processo)
    ? (semEco && voz
        ? `Enviando o áudio do sistema, menos o ${voz}`
        : "Enviando o áudio do sistema inteiro")
    : `Enviando só o áudio de ${f.processo || "este aplicativo"}`);
  for (const id of ["#m-kbps", "#m-fps", "#m-drop", "#m-tempo", "#m-res"]) texto(id, "—");
  $("#ar-estado").className = "ar-estado";
  texto("#ar-estado", "negociando…");
}

$("#btn-parar").addEventListener("click", async () => {
  $("#btn-parar").disabled = true;
  try { await invoke("parar"); } catch (e) { console.warn(e); }
  $("#btn-parar").disabled = false;
  mostra("t-fonte");
  // Acabou de sair do ar: está no app e sem pressa. É o melhor momento que
  // existe para oferecer uma atualização que reinicia o programa.
  verificaSeTemVersaoNova();
});

/* Trocar de fonte sem sair da sala.
 *
 * Encerra só a captura e o encoder; a conexão com a sala continua de pé, então
 * ninguém vê a pessoa sair e voltar. Do outro lado o navegador troca de stream
 * sozinho — a sala anuncia o novo e o card reassina.
 *
 * Um botão para isso porque "parar, escolher outra, transmitir" é a operação
 * mais repetida em uso real: começa mostrando o jogo, alguém pergunta algo, vai
 * para a janela do navegador e volta. */
/* Trocar de fonte SEM parar a transmissão.
 *
 * A versão anterior chamava `parar` antes de mostrar a lista, e quem assistia
 * ficava sem imagem durante todo o tempo em que a pessoa escolhia — na prática
 * era o mesmo que parar. Agora a transmissão atual segue no ar; o Rust encerra
 * a anterior só quando a nova começa, então o corte é o da troca em si. */
$("#btn-trocar").addEventListener("click", async () => {
  estado.trocando = true;
  mostra("t-fonte");
  $("#btn-voltar-ar").hidden = false;
  $("#btn-transmitir").textContent = "Trocar para esta";
  await atualizaFontes();
});

/* Desistir da troca: volta para a tela do ar, que nunca deixou de existir. */
$("#btn-voltar-ar").addEventListener("click", () => {
  estado.trocando = false;
  $("#btn-voltar-ar").hidden = true;
  $("#btn-transmitir").textContent = "Transmitir";
  mostra("t-ar");
});

$("#btn-abrir-sala").addEventListener("click", async () => {
  try { await invoke("abrir_sala"); } catch (e) { texto("#erro-fonte", String(e)); }
});

/* ---------------- eventos vindos do Rust ---------------- */

listen("sala-estado", (ev) => {
  const e = ev.payload;
  estado.pingMs = e.ping_ms;
  texto("#tag-pessoas", `${e.pessoas} na sala`);
  for (const sel of ["#tag-ping", "#tag-ping-ar"]) {
    const el = $(sel);
    el.textContent = e.ping_ms ? `${e.ping_ms} ms` : "— ms";
    // Acima de 150 ms o chip fica laranja, mesmo limiar da sala web: quem está
    // com a conexão ruim precisa ver isso antes de culpar o servidor.
    el.className = "tag" + (e.ping_ms > 150 ? " tag--warn" : "");
  }
  texto("#m-ping", e.ping_ms ? String(e.ping_ms) : "—");
});

listen("telemetria", (ev) => pintaTelemetria(ev.payload));

/* A codificação caiu para a CPU — ou porque a GPU não tem encoder utilizável,
   ou porque o dela produzia um fluxo que ninguém conseguia decodificar. Vale
   avisar: é a explicação de o computador ficar mais pesado e de o jogo perder
   quadros enquanto transmite. */
listen("encoder-por-software", () => {
  const a = $("#aviso-cpu");
  a.hidden = false;
  a.textContent = "Codificando pelo processador, não pela placa de vídeo — "
    + "o computador vai ficar mais pesado e o jogo pode perder quadros. "
    + "A placa não tem codificador utilizável, ou o dela não funcionou aqui.";
});

/* O programa trocou de janela sozinho — o cliente do jogo virou a partida, ou
   o contrário. A transmissão não parou, então a tela do ar só precisa passar a
   dizer a verdade sobre o que está indo. */
listen("fonte-trocada", (ev) => {
  const titulo = String(ev.payload || "");
  if (estado.escolhida) estado.escolhida.titulo = titulo;
  texto("#ar-titulo", titulo);
  const e = $("#ar-estado");
  e.className = "ar-estado";
  texto("#ar-estado", "o programa trocou de janela — seguindo");
});

listen("transmissao-fim", async (ev) => {
  // O supervisor no Rust já desmontou tudo, mas o objeto morto continua no
  // estado do app — sem este `parar` a próxima tentativa esbarraria em "já
  // existe uma transmissão no ar".
  try { await invoke("parar"); } catch (e) { console.warn(e); }
  const motivo = ev.payload;
  if (motivo) texto("#erro-fonte", `A transmissão parou: ${motivo}`);
  mostra("t-fonte");
  verificaSeTemVersaoNova();
});

listen("sala-caiu", (motivo) => {
  texto("#erro-entrar", `Conexão com a sala: ${motivo.payload}`);
  mostra("t-entrar");
});

/* A sala caiu e o app está tentando voltar sozinho.
 *
 * NÃO manda para a tela de entrar: na maioria das vezes é o serviço
 * reiniciando e volta em segundos. Jogar a pessoa para fora, apagar a
 * transmissão e obrigá-la a refazer tudo era pior que esperar — e era o que
 * acontecia antes. Se as tentativas se esgotarem, aí sim vem o `sala-caiu`. */
listen("sala-reconectando", (ev) => {
  const el = $("#erro-fonte") || $("#erro-entrar");
  if (el) el.textContent = `Conexão com a sala caiu (${ev.payload}) — reconectando…`;
});

listen("sala-voltou", () => {
  for (const id of ["#erro-fonte", "#erro-entrar"]) {
    const el = $(id);
    if (el) el.textContent = "";
  }
});

// Avisos do Rust que não impedem a transmissão, mas a pessoa precisa saber —
// hoje só o de monitor capturando preto.
listen("aviso", (ev) => {
  const el = $("#erro-fonte") || $("#erro-entrar");
  if (el) el.textContent = String(ev.payload);
});

function pintaTelemetria(t) {
  texto("#m-kbps", String(t.kbps));
  texto("#m-fps", String(t.fps));
  texto("#m-drop", String(t.dropped));
  texto("#m-res", `${t.largura}×${t.altura}`);
  texto("#m-encoder", t.encoder);
  $("#m-drop").classList.toggle("alerta", t.dropped > 0);

  // Perda no caminho tem conserto diferente das outras: não adianta mexer no
  // app, é a conexão não dando conta do bitrate escolhido. Dizer isso é mais
  // útil que mostrar um número.
  const perda = (t.perda_por_mil || 0) / 10;
  const avisoPerda = $("#ar-perda");
  if (avisoPerda) {
    // Com a qualidade automática o conselho de "baixe a qualidade" virou
    // mentira: o app já está fazendo isso. Dizer para a pessoa mexer no que
    // ela não controla é pior que não dizer nada.
    let recado = "";
    if (t.ultima_mudanca) {
      recado = `Ajustado para ${t.ultima_mudanca}`;
    } else if (perda >= 1) {
      recado = estado.preset === "auto"
        ? `${perda.toFixed(1)}% do que você envia não está chegando — ajustando`
        : `${perda.toFixed(1)}% do que você envia não está chegando — `
          + `a qualidade automática resolveria isso`;
    }
    avisoPerda.textContent = recado;
    avisoPerda.classList.toggle("alerta", perda >= 1);
  }

  const el = $("#ar-estado");
  const conectado = t.estado === "connected";
  el.className = "ar-estado " + (conectado ? "ok" : (t.estado === "failed" ? "ruim" : ""));
  el.textContent = conectado
    ? "conexão estabelecida — a sala já está recebendo"
    : `conexão: ${t.estado}`;
}

/* O relógio do "no ar há" é local e não vem da telemetria: a telemetria chega
   de 2 em 2 s e um cronômetro que anda aos saltos parece travado. O número que
   importa aqui é quanto tempo a pessoa está transmitindo, e disso o próprio
   navegador dá conta. */
setInterval(() => {
  if (!$("#t-ar").classList.contains("on") || !estado.inicio) return;
  const seg = Math.floor((Date.now() - estado.inicio) / 1000);
  texto("#m-tempo", `${Math.floor(seg / 60)}:${String(seg % 60).padStart(2, "0")}`);
}, 1000);

abre();
