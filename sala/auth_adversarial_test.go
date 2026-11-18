package main

import (
	"net/http"
	"testing"
	"time"
)

// Casos adversariais escritos independentemente do plano, para verificar a
// postura fail-closed do autorizador. Arquivo temporario, nao entra no commit.
func TestAdversarial(t *testing.T) {
	h := NewHub("segredo")
	dono, _ := h.Join(&fakeSender{}, "segredo", "gus", "", "", time.Now())
	intruso, _ := h.Join(&fakeSender{}, "segredo", "ana", "", "", time.Now())
	st, _ := h.PublishStart(dono.ID, PublishStartData{SourceType: "monitor"})

	casos := []struct {
		nome string
		body string
	}{
		{"path traversal", `{"action":"read","path":"tela/../admin","query":"token=` + dono.Token + `"}`},
		{"streamID vazio", `{"action":"read","path":"tela/","query":"token=` + dono.Token + `"}`},
		{"prefixo parecido", `{"action":"read","path":"telax/` + st.ID + `","query":"token=` + dono.Token + `"}`},
		{"acao api", `{"action":"api","path":"tela/` + st.ID + `","query":"token=` + dono.Token + `"}`},
		{"acao playback", `{"action":"playback","path":"tela/` + st.ID + `","query":"token=` + dono.Token + `"}`},
		{"token vazio", `{"action":"read","path":"tela/` + st.ID + `","query":"token="}`},
		{"token inventado", `{"action":"read","path":"tela/` + st.ID + `","query":"token=deadbeef"}`},
		{"query lixo", `{"action":"read","path":"tela/` + st.ID + `","query":"%%%"}`},
		{"corpo vazio", ``},
		{"json truncado", `{"action":`},
		{"publish stream inexistente", `{"action":"publish","path":"tela/naoexiste","query":"token=` + dono.Token + `"}`},
		{"publish com token de quem nao transmite", `{"action":"publish","path":"tela/` + st.ID + `","query":"token=` + intruso.Token + `"}`},
	}

	for _, c := range casos {
		code := postAuth(t, h, c.body)
		if code != http.StatusUnauthorized {
			t.Errorf("FALHA %-45s -> %d (esperado 401)", c.nome, code)
		} else {
			t.Logf("ok    %-45s -> 401", c.nome)
		}
	}

	// Controle: o caminho feliz DEVE continuar liberando, senao o teste
	// acima passaria por estar negando tudo.
	if code := postAuth(t, h, `{"action":"publish","path":"tela/`+st.ID+`","query":"token=`+dono.Token+`"}`); code != http.StatusOK {
		t.Errorf("FALHA controle: dono publicando -> %d (esperado 200)", code)
	}
	if code := postAuth(t, h, `{"action":"read","path":"tela/`+st.ID+`","query":"token=`+intruso.Token+`"}`); code != http.StatusOK {
		t.Errorf("FALHA controle: outro membro assistindo -> %d (esperado 200)", code)
	}
}
