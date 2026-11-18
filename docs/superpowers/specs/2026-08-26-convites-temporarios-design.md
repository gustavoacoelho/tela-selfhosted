# Convites temporários para a sala

Data: 2026-08-26

## Problema

A sala tem uma senha só. Quem a recebe entra para sempre, e tirar uma pessoa
obriga os cinco de casa a trocar de senha.

Aconteceu em 25/08/2026: a senha foi passada para um grupo grande num evento e
no dia seguinte foi preciso rotacioná-la. A rotação funcionou, mas custou uma
noite — e a próxima vez que alguém for convidado o problema volta igual.

## O que se quer

Chamar alguém para assistir por um período, sem dar a chave de casa e sem que
tirar essa pessoa depois afete quem já estava.

## Decisões

| Questão | Decisão | Por quê |
|---|---|---|
| O que o convidado faz | Só assiste | Superfície menor: não mexe no gate de download nem no `publish`. Cobre o caso real (chamar gente para ver algo). |
| Como o convite morre | Prazo (1h/6h/24h) **e** revogação | Prazo cobre o esquecimento; revogação cobre o abuso durante o evento. Só um dos dois deixa um buraco. |
| Formato | Link (`/c/<codigo>`) | Nada para ditar ou errar. O convidado só escolhe apelido. |
| Quem convida | Só o admin | Foi perder o controle da senha que motivou isto. |
| Como reconhecer o admin | `TELA_ADMIN_PASSWORD` no login | Apelido NÃO serve: é texto livre, qualquer um digitaria "Grugs". Mesma tela, mesmo campo. Se vazar, trocar só ela não mexe na senha dos outros. |
| Armazenamento | JSON em disco | O `Hub` é todo em memória e a sala reinicia a cada implantação (quatro vezes na noite de 25/08). Convite que morre em restart derruba convidado no meio do evento. |

### Alternativas descartadas

- **Token assinado (HMAC), sem armazenar** — sobrevive a restart de graça, mas
  revogar exige lista de revogados em disco de qualquer jeito. Paga-se o
  armazenamento e perde-se a listagem.
- **SQLite** — banco, migrações e dependência nova para dez linhas de dado.
- **Marcar apelidos como admin** — inseguro: apelido é digitado livremente.

## Modelo

Um convite tem: código, rótulo, quando expira, quem criou, e se foi revogado.

O código usa o mesmo alfabeto da senha da sala (minúsculas e dígitos, sem `l/1`
nem `o/0`), ~100 bits de entropia.

**O código fica em texto, não hash.** Quem lê o arquivo já tem o `.env` com a
senha de casa ao lado — hashear não protegeria de nada real e custaria a
capacidade de reabrir a lista e recopiar um link perdido.

Vive em `/srv/tela/dados/convites.json`, volume gravável novo. Carregado no
boot, regravado a cada mudança, com escrita atômica (temporário + `rename`) para
um restart no meio não deixar arquivo pela metade.

Convites vencidos ou revogados saem da lista na faxina periódica, junto da que o
`Throttle` já faz.

## Papéis

Três: **admin** (entra com `TELA_ADMIN_PASSWORD`), **casa** (senha de sempre),
**convidado** (link). O papel é decidido no `hello` e fica na sessão.

O `hello` passa a levar `password` **ou** `convite`.

## Imposição

Cada coisa escondida na interface tem contraparte no servidor — botão sumido não
é segurança.

| Restrição | Onde é imposta |
|---|---|
| Convidado não baixa o app | `DownloadHandler` recusa token de convidado |
| Convidado não transmite | `AuthHandler` nega `publish`; `read` segue liberado |
| Convidado não transmite (2ª barreira) | `publish_start` recusado no WS |
| Só admin convida | criar/revogar no WS exige papel admin |

**Revogar derruba as sessões abertas por aquele convite.** Sem isso a revogação
não serve durante o evento, que é quando ela é necessária.

**Sem `TELA_ADMIN_PASSWORD` não existe admin nem convite.** Falha fechada: uma
variável esquecida desliga o recurso em vez de liberá-lo.

**Link inválido ou vencido conta no anti-força-bruta**, como senha errada — é a
mesma superfície de adivinhar segredo. Só é seguro porque a página parou de
reconectar após recusa (correção de 26/08); sem ela um link vencido viraria laço
de tentativas e bloquearia o convidado.

## Interface

Um link serve várias pessoas até vencer. Não há limite por número de usos: o
controle é o prazo mais o botão de revogar.

Botão "Convidar" só para admin. Abre painel com rótulo, prazo e botão de gerar;
o link sai com botão de copiar. Abaixo, convites ativos com prazo restante,
quantos estão usando e botão de revogar.

Convidados aparecem etiquetados na lista de participantes.

## Testes

Em Go, com o relógio injetado (`agora time.Time`), que é o padrão que já deixou
os testes do `Throttle` determinísticos:

- convite válido entra; vencido, revogado e inexistente não entram
- revogar derruba as sessões daquele convite
- convidado recebe 401 no download e `publish` negado no autorizador
- quem não é admin não cria nem revoga
- sem `TELA_ADMIN_PASSWORD`, criar convite falha
- grava e recarrega do disco; arquivo pela metade não derruba o serviço

O JavaScript da página passa por `node --check` antes de qualquer implantação —
uma aspa não escapada já derrubou a sala inteira em 26/08.
