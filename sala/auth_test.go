package main

import (
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"
)

func postAuth(t *testing.T, h *Hub, body string) int {
	t.Helper()
	rec := httptest.NewRecorder()
	req := httptest.NewRequest(http.MethodPost, "/internal/auth", strings.NewReader(body))
	AuthHandler(h).ServeHTTP(rec, req)
	return rec.Code
}

func TestAuthPublicaNoProprioPath(t *testing.T) {
	h := NewHub("segredo")
	s, _ := h.Join(&fakeSender{}, "segredo", "gus", "", "", time.Now())
	st, _ := h.PublishStart(s.ID, PublishStartData{SourceType: "monitor"})

	body := `{"action":"publish","path":"tela/` + st.ID + `","query":"token=` + s.Token + `"}`
	if code := postAuth(t, h, body); code != http.StatusOK {
		t.Fatalf("code = %d, esperado 200", code)
	}
}

func TestAuthNegaPublicarNoPathDeOutro(t *testing.T) {
	h := NewHub("segredo")
	a, _ := h.Join(&fakeSender{}, "segredo", "gus", "", "", time.Now())
	b, _ := h.Join(&fakeSender{}, "segredo", "ana", "", "", time.Now())
	stA, _ := h.PublishStart(a.ID, PublishStartData{SourceType: "monitor"})

	// b tenta publicar no path de a
	body := `{"action":"publish","path":"tela/` + stA.ID + `","query":"token=` + b.Token + `"}`
	if code := postAuth(t, h, body); code != http.StatusUnauthorized {
		t.Fatalf("code = %d, esperado 401", code)
	}
}

func TestAuthLeituraComTokenValido(t *testing.T) {
	h := NewHub("segredo")
	a, _ := h.Join(&fakeSender{}, "segredo", "gus", "", "", time.Now())
	b, _ := h.Join(&fakeSender{}, "segredo", "ana", "", "", time.Now())
	st, _ := h.PublishStart(a.ID, PublishStartData{SourceType: "monitor"})

	body := `{"action":"read","path":"tela/` + st.ID + `","query":"token=` + b.Token + `"}`
	if code := postAuth(t, h, body); code != http.StatusOK {
		t.Fatalf("code = %d, esperado 200", code)
	}
}

func TestAuthNegaSemToken(t *testing.T) {
	h := NewHub("segredo")
	a, _ := h.Join(&fakeSender{}, "segredo", "gus", "", "", time.Now())
	st, _ := h.PublishStart(a.ID, PublishStartData{SourceType: "monitor"})

	body := `{"action":"read","path":"tela/` + st.ID + `","query":""}`
	if code := postAuth(t, h, body); code != http.StatusUnauthorized {
		t.Fatalf("code = %d, esperado 401", code)
	}
}

func TestAuthNegaPathForaDoNamespace(t *testing.T) {
	h := NewHub("segredo")
	s, _ := h.Join(&fakeSender{}, "segredo", "gus", "", "", time.Now())

	body := `{"action":"read","path":"outra/coisa","query":"token=` + s.Token + `"}`
	if code := postAuth(t, h, body); code != http.StatusUnauthorized {
		t.Fatalf("code = %d, esperado 401", code)
	}
}
