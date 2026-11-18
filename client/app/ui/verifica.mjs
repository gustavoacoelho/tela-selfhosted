// Conferências da interface que não precisam de navegador.
//
// Existe porque cada uma delas já quebrou o app em produção pelo menos uma vez,
// e nenhuma aparece no `cargo build`: o HTML e o JS não são compilados.
//
//   node verifica.mjs
import { readFileSync } from "node:fs";

const html = readFileSync(new URL("index.html", import.meta.url), "utf8");
const js = readFileSync(new URL("app.js", import.meta.url), "utf8");
const css = readFileSync(new URL("estilo.css", import.meta.url), "utf8");

const falhas = [];
const exige = (ok, msg) => { if (!ok) falhas.push(msg); };

// 1. O script tem de esperar o documento inteiro.
//
// Sem `defer`, um `$("#algo")` no corpo do arquivo roda antes de o elemento
// existir se a tag do script vier antes dele no HTML. O `$` devolve null, o
// `addEventListener` estoura, e o arquivo INTEIRO morre — inclusive o
// `submit` do formulário de entrada, que volta a ser o envio nativo do
// navegador: a página recarrega e os campos aparecem apagados. Foi
// exatamente isso que aconteceu ao mover o popup para o fim do body.
exige(
  /<script[^>]+src="app\.js"[^>]*\sdefer[^>]*>/.test(html),
  'a tag <script src="app.js"> precisa de `defer`',
);

// 2. Todo id usado pelo JS tem de existir no HTML.
//
// Qualquer literal `"#algo"`, não só `$("#algo")`: metade das referências
// passa por ajudantes como `texto("#att-titulo", …)`, e um regex que só via a
// forma direta deixou passar exatamente esse caso quando foi testado.
const ids = new Set([...html.matchAll(/id="([^"]+)"/g)].map((m) => m[1]));
for (const m of js.matchAll(/["'`]#([\w-]+)["'`]/g)) {
  exige(ids.has(m[1]), `o JS usa #${m[1]}, que não existe no HTML`);
}

// 3. Quem usa o atributo `hidden` não pode declarar `display` numa classe.
//
// `[hidden]{display:none}` da folha do navegador tem especificidade 0,1,0 e
// empata com qualquer classe nossa — que ganha por vir depois. O elemento
// fica visível com `hidden` ligado. A regra global com `!important` no topo
// do estilo resolve; esta conferência garante que ela continue lá.
exige(
  /\[hidden\]\s*\{[^}]*display\s*:\s*none\s*!important/.test(css),
  "falta a regra [hidden]{display:none !important} no estilo",
);

// 4. Nomes de classe não podem colidir.
//
// `.barra` já era a barra de ações do rodapé quando a barra de progresso
// nasceu com o mesmo nome: ela herdou o `padding` e virou um retângulo vazio
// de 28px. Duas declarações da mesma classe raramente são de propósito.
const classes = [...css.matchAll(/^\.([\w-]+)\s*\{/gm)].map((m) => m[1]);
const vistas = new Set();
for (const c of classes) {
  exige(!vistas.has(c), `a classe .${c} é declarada duas vezes no estilo`);
  vistas.add(c);
}

if (falhas.length) {
  console.error("interface com problema:");
  for (const f of falhas) console.error("  - " + f);
  process.exit(1);
}
console.log("interface ok");
