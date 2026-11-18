package main

import (
	"context"
	"encoding/json"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/coder/websocket"
)

// attemptLogin abre uma conexão WS nova (como um cliente real faria a cada
// tentativa de senha), manda hello e devolve o tipo do primeiro envelope de
// resposta e, se for "denied", o motivo.
func attemptLogin(t *testing.T, wsURL, ip, password, nick string) (typ string, reason string) {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	header := map[string][]string{}
	if ip != "" {
		header["CF-Connecting-IP"] = []string{ip}
	}
	conn, _, err := websocket.Dial(ctx, wsURL, &websocket.DialOptions{HTTPHeader: header})
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	defer conn.CloseNow()

	env, err := NewEnvelope("hello", HelloData{Password: password, Nick: nick})
	if err != nil {
		t.Fatalf("envelope: %v", err)
	}
	b, _ := json.Marshal(env)
	if err := conn.Write(ctx, websocket.MessageText, b); err != nil {
		t.Fatalf("write hello: %v", err)
	}

	_, data, err := conn.Read(ctx)
	if err != nil {
		t.Fatalf("read resposta: %v", err)
	}
	var got Envelope
	if err := json.Unmarshal(data, &got); err != nil {
		t.Fatalf("unmarshal resposta: %v", err)
	}
	if got.Type == "denied" {
		var d DeniedData
		_ = json.Unmarshal(got.Data, &d)
		return got.Type, d.Reason
	}
	return got.Type, ""
}

func newTestServer(h *Hub, th *Throttle) (*httptest.Server, string) {
	srv := httptest.NewServer(WSHandler(h, th, []string{"localhost"}))
	wsURL := "ws" + strings.TrimPrefix(srv.URL, "http") + "/ws"
	return srv, wsURL
}

func TestWSBloqueiaApos5FalhasDoMesmoIP(t *testing.T) {
	h := NewHub("segredo")
	th := NewThrottle(5, 5*time.Minute, 15*time.Minute)
	srv, wsURL := newTestServer(h, th)
	defer srv.Close()

	for i := 0; i < 5; i++ {
		typ, reason := attemptLogin(t, wsURL, "9.9.9.9", "errada", "gus")
		if typ != "denied" {
			t.Fatalf("tentativa %d: tipo = %q, esperado denied", i+1, typ)
		}
		if reason != ErrSenhaIncorreta.Error() {
			t.Fatalf("tentativa %d: motivo = %q, esperado %q", i+1, reason, ErrSenhaIncorreta.Error())
		}
	}

	// 6ª tentativa: nem chega a checar a senha, o IP já está bloqueado.
	typ, reason := attemptLogin(t, wsURL, "9.9.9.9", "segredo", "gus")
	if typ != "denied" {
		t.Fatalf("tipo = %q, esperado denied (bloqueado)", typ)
	}
	if reason == ErrSenhaIncorreta.Error() {
		t.Fatal("deveria ter sido bloqueado antes de checar a senha")
	}
}

func TestWSLoginCorretoNaoEhBloqueado(t *testing.T) {
	h := NewHub("segredo")
	th := NewThrottle(5, 5*time.Minute, 15*time.Minute)
	srv, wsURL := newTestServer(h, th)
	defer srv.Close()

	typ, _ := attemptLogin(t, wsURL, "1.1.1.1", "segredo", "gus")
	if typ != "welcome" {
		t.Fatalf("tipo = %q, esperado welcome", typ)
	}
}

func TestWSSucessoZeraContadorNoServidor(t *testing.T) {
	h := NewHub("segredo")
	th := NewThrottle(5, 5*time.Minute, 15*time.Minute)
	srv, wsURL := newTestServer(h, th)
	defer srv.Close()

	for i := 0; i < 3; i++ {
		attemptLogin(t, wsURL, "2.2.2.2", "errada", "gus")
	}
	if typ, _ := attemptLogin(t, wsURL, "2.2.2.2", "segredo", "gus2"); typ != "welcome" {
		t.Fatalf("login correto deveria ter passado, tipo = %q", typ)
	}

	// Mais 4 falhas depois do sucesso: total seria 7 sem reset, mas o
	// sucesso deveria ter zerado — então a 4ª aqui ainda não bloqueia.
	for i := 0; i < 4; i++ {
		typ, reason := attemptLogin(t, wsURL, "2.2.2.2", "errada", "gus3")
		if typ != "denied" || reason != ErrSenhaIncorreta.Error() {
			t.Fatalf("tentativa pós-sucesso %d: tipo=%q reason=%q, esperado denied/senha incorreta", i+1, typ, reason)
		}
	}
}

func TestWSSalaCheiaNaoContaComoFalha(t *testing.T) {
	h := NewHub("segredo")
	th := NewThrottle(5, 5*time.Minute, 15*time.Minute)
	srv, wsURL := newTestServer(h, th)
	defer srv.Close()

	// Lota a sala com conexões que ficam penduradas (não fecham).
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	var conns []*websocket.Conn
	for i := 0; i < MaxParticipantes; i++ {
		conn, _, err := websocket.Dial(ctx, wsURL, nil)
		if err != nil {
			t.Fatalf("dial %d: %v", i, err)
		}
		conns = append(conns, conn)
		env, _ := NewEnvelope("hello", HelloData{Password: "segredo", Nick: "p"})
		b, _ := json.Marshal(env)
		if err := conn.Write(ctx, websocket.MessageText, b); err != nil {
			t.Fatalf("write hello %d: %v", i, err)
		}
		if _, _, err := conn.Read(ctx); err != nil {
			t.Fatalf("read welcome %d: %v", i, err)
		}
	}
	defer func() {
		for _, c := range conns {
			c.CloseNow()
		}
	}()

	// Sala cheia: 6 tentativas do mesmo IP não deveriam bloquear (mais que
	// maxFalhas=5, o que provaria que ErrSalaCheia não é tratado como falha).
	for i := 0; i < 6; i++ {
		typ, reason := attemptLogin(t, wsURL, "3.3.3.3", "segredo", "outro")
		if typ != "denied" {
			t.Fatalf("tentativa %d: tipo = %q, esperado denied (sala cheia)", i+1, typ)
		}
		if reason != ErrSalaCheia.Error() {
			t.Fatalf("tentativa %d: reason = %q, esperado %q (não deveria estar bloqueado)", i+1, reason, ErrSalaCheia.Error())
		}
	}
}
