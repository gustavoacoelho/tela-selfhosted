package main

import (
	"context"
	"encoding/json"
	"testing"
	"time"

	"github.com/coder/websocket"
)

// entraEFica abre uma conexao, manda hello e devolve a conexao ABERTA, para o
// teste decidir se o cliente continua falando ou emudece.
func entraEFica(t *testing.T, wsURL, senha, nick string) *websocket.Conn {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	conn, _, err := websocket.Dial(ctx, wsURL, nil)
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	env, _ := NewEnvelope("hello", HelloData{Password: senha, Nick: nick})
	b, _ := json.Marshal(env)
	if err := conn.Write(ctx, websocket.MessageText, b); err != nil {
		t.Fatalf("write hello: %v", err)
	}
	_, data, err := conn.Read(ctx)
	if err != nil {
		t.Fatalf("read welcome: %v", err)
	}
	var got Envelope
	_ = json.Unmarshal(data, &got)
	if got.Type != "welcome" {
		t.Fatalf("esperava welcome, veio %q", got.Type)
	}
	return conn
}

func quantos(h *Hub) int {
	return len(h.Snapshot().Participants)
}

// esperaAte espera a condicao virar verdadeira, para o teste nao depender de um
// sleep fixo que fica lento ou instavel conforme a maquina.
func esperaAte(t *testing.T, limite time.Duration, cond func() bool) bool {
	t.Helper()
	fim := time.Now().Add(limite)
	for time.Now().Before(fim) {
		if cond() {
			return true
		}
		time.Sleep(10 * time.Millisecond)
	}
	return cond()
}

// O defeito: cliente que some sem fechar o TCP deixava a sessao viva para
// sempre.
//
// # O que aconteceu
//
// A leitura do WebSocket nao tinha prazo. Notebook suspendendo, Wi-Fi virando
// 4G ou aba congelada deixam o socket meio-aberto: o servidor fica bloqueado
// na leitura, o `Leave` do defer nunca roda e a sessao vira fantasma.
//
// Em 17 dias no ar isso acumulou fantasmas de 199 entradas e encheu o teto de
// 16 participantes. A sala passou a responder "sala cheia" para gente de
// verdade, e na lista apareciam varias copias da mesma pessoa.
func TestSessaoQueEmudeceEhRemovida(t *testing.T) {
	antes := PrazoSemSinal
	PrazoSemSinal = 300 * time.Millisecond
	t.Cleanup(func() { PrazoSemSinal = antes })

	h := NewHub("segredo")
	srv, wsURL := newTestServer(h, NewThrottle(5, time.Minute, time.Minute))
	defer srv.Close()

	conn := entraEFica(t, wsURL, "segredo", "fantasma")
	defer conn.CloseNow()

	if got := quantos(h); got != 1 {
		t.Fatalf("depois de entrar deveria haver 1 participante, ha %d", got)
	}

	// A partir daqui o cliente nao fala mais nada — e nao fecha a conexao.
	if !esperaAte(t, 3*time.Second, func() bool { return quantos(h) == 0 }) {
		t.Fatalf("a sessao muda continuou na sala: %d participante(s) — e o fantasma que enchia a sala",
			quantos(h))
	}
}

// Contraparte obrigatoria: quem continua falando NAO pode ser derrubado.
//
// Sem este teste, "remover sessao muda" passaria igual se o prazo derrubasse
// todo mundo — que seria um defeito muito pior que o original.
func TestQuemContinuaFalandoNaoEhDerrubado(t *testing.T) {
	antes := PrazoSemSinal
	PrazoSemSinal = 300 * time.Millisecond
	t.Cleanup(func() { PrazoSemSinal = antes })

	h := NewHub("segredo")
	srv, wsURL := newTestServer(h, NewThrottle(5, time.Minute, time.Minute))
	defer srv.Close()

	conn := entraEFica(t, wsURL, "segredo", "vivo")
	defer conn.CloseNow()

	// Fala a cada 100 ms por mais de tres prazos inteiros.
	fim := time.Now().Add(1100 * time.Millisecond)
	for time.Now().Before(fim) {
		ctx, cancel := context.WithTimeout(context.Background(), time.Second)
		env, _ := NewEnvelope("pong", PongData{T: nowMs()})
		b, _ := json.Marshal(env)
		err := conn.Write(ctx, websocket.MessageText, b)
		cancel()
		if err != nil {
			t.Fatalf("a sala derrubou quem estava falando: %v", err)
		}
		time.Sleep(100 * time.Millisecond)
	}

	if got := quantos(h); got != 1 {
		t.Fatalf("quem fala a cada 100ms nao pode ser removido; ha %d participante(s)", got)
	}
}

// Uma pessoa que reconecta nao pode deixar a copia velha para tras.
//
// E o sintoma que o Gustavo viu: "varios participantes iguais" na lista.
func TestReconectarNaoDeixaCopiaParaTras(t *testing.T) {
	antes := PrazoSemSinal
	PrazoSemSinal = 300 * time.Millisecond
	t.Cleanup(func() { PrazoSemSinal = antes })

	h := NewHub("segredo")
	srv, wsURL := newTestServer(h, NewThrottle(50, time.Minute, time.Minute))
	defer srv.Close()

	// Tres "quedas" seguidas sem fechar o socket, como acontece de verdade.
	var abertas []*websocket.Conn
	for i := 0; i < 3; i++ {
		c := entraEFica(t, wsURL, "segredo", "Grugs")
		abertas = append(abertas, c)
	}
	t.Cleanup(func() {
		for _, c := range abertas {
			c.CloseNow()
		}
	})

	if got := quantos(h); got != 3 {
		t.Fatalf("montagem errada: esperava 3 sessoes vivas, ha %d", got)
	}

	// Nenhuma fala: todas tem de cair.
	if !esperaAte(t, 3*time.Second, func() bool { return quantos(h) == 0 }) {
		t.Fatalf("sobraram %d copia(s) de Grugs na lista", quantos(h))
	}
}
