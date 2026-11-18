package main

import (
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// hubComConvites devolve um hub com convites ligados e o store, para os testes
// poderem criar convite sem passar pelo caminho de admin.
func hubComConvites(t *testing.T) (*Hub, *Convites) {
	t.Helper()
	c, err := AbreConvites(filepath.Join(t.TempDir(), "convites.json"))
	if err != nil {
		t.Fatal(err)
	}
	h := NewHub("senha-de-casa")
	h.HabilitaConvites("senha-de-admin", c)
	return h, c
}

func TestConvidadoEntraComLinkValido(t *testing.T) {
	h, c := hubComConvites(t)
	cv, _ := c.Cria("pessoal", "6h", "Grugs", t0)

	s, err := h.Join(&fakeSender{}, "", "visitante", "", cv.Codigo, t0)
	if err != nil {
		t.Fatalf("convidado nao entrou: %v", err)
	}
	if s.Papel != PapelConvidado {
		t.Fatalf("papel errado: %q", s.Papel)
	}
	if s.Convite != cv.Codigo {
		t.Fatal("a sessao tem de lembrar por qual convite entrou, senao revogar nao sabe quem derrubar")
	}
}

func TestConvidadoNaoEntraComConviteRuim(t *testing.T) {
	h, c := hubComConvites(t)
	vencido, _ := c.Cria("vencido", "1h", "Grugs", t0)
	revogado, _ := c.Cria("revogado", "6h", "Grugs", t0)
	c.Revoga(revogado.Codigo)

	casos := []struct {
		nome   string
		codigo string
		agora  time.Time
	}{
		{"vencido", vencido.Codigo, t0.Add(2 * time.Hour)},
		{"revogado", revogado.Codigo, t0},
		{"inventado", "codigoquenaoexiste", t0},
	}
	for _, caso := range casos {
		t.Run(caso.nome, func(t *testing.T) {
			_, err := h.Join(&fakeSender{}, "", "x", "", caso.codigo, caso.agora)
			if err != ErrConviteInvalido {
				t.Fatalf("esperava ErrConviteInvalido, veio %v", err)
			}
		})
	}
}

// Falha fechada: sem senha de admin, link nenhum vale.
func TestSemAdminNenhumConviteVale(t *testing.T) {
	c, _ := AbreConvites(filepath.Join(t.TempDir(), "c.json"))
	cv, _ := c.Cria("x", "6h", "alguem", t0)

	h := NewHub("senha-de-casa") // convites NAO habilitados
	if _, err := h.Join(&fakeSender{}, "", "x", "", cv.Codigo, t0); err != ErrConviteInvalido {
		t.Fatalf("sem admin, convite nao pode valer. veio %v", err)
	}
	if h.ConvitesLigados() {
		t.Fatal("convites nao deviam estar ligados")
	}
}

func TestAdminEntraComSenhaDeAdmin(t *testing.T) {
	h, _ := hubComConvites(t)
	s, err := h.Join(&fakeSender{}, "senha-de-admin", "Grugs", "", "", t0)
	if err != nil {
		t.Fatal(err)
	}
	if s.Papel != PapelAdmin {
		t.Fatalf("devia ser admin, veio %q", s.Papel)
	}
}

func TestSenhaDeCasaContinuaEntrandoComoCasa(t *testing.T) {
	h, _ := hubComConvites(t)
	s, err := h.Join(&fakeSender{}, "senha-de-casa", "Fael", "", "", t0)
	if err != nil {
		t.Fatal(err)
	}
	if s.Papel != PapelCasa {
		t.Fatalf("devia ser casa, veio %q", s.Papel)
	}
}

func TestSoAdminCriaERevoga(t *testing.T) {
	h, c := hubComConvites(t)
	casa, _ := h.Join(&fakeSender{}, "senha-de-casa", "Fael", "", "", t0)
	cv, _ := c.Cria("alvo", "6h", "Grugs", t0)

	if _, err := h.CriaConvite(casa.ID, "meus amigos", "6h", t0); err != ErrNaoEhAdmin {
		t.Fatalf("quem eh de casa nao pode convidar. veio %v", err)
	}
	if err := h.RevogaConvite(casa.ID, cv.Codigo); err != ErrNaoEhAdmin {
		t.Fatalf("quem eh de casa nao pode revogar. veio %v", err)
	}
	if _, err := h.ListaConvites(casa.ID, t0); err != ErrNaoEhAdmin {
		t.Fatalf("quem eh de casa nao ve a lista. veio %v", err)
	}

	conv, _ := h.Join(&fakeSender{}, "", "visita", "", cv.Codigo, t0)
	if _, err := h.CriaConvite(conv.ID, "x", "6h", t0); err != ErrNaoEhAdmin {
		t.Fatalf("convidado nao pode convidar. veio %v", err)
	}
}

// Revogar durante o evento tem de valer para quem ja esta dentro — e quando se
// revoga.
func TestRevogarDerrubaQuemEntrouPorAqueleConvite(t *testing.T) {
	h, c := hubComConvites(t)
	admin, _ := h.Join(&fakeSender{}, "senha-de-admin", "Grugs", "", "", t0)
	cvA, _ := c.Cria("grupo A", "6h", "Grugs", t0)
	cvB, _ := c.Cria("grupo B", "6h", "Grugs", t0)

	a1, _ := h.Join(&fakeSender{}, "", "a1", "", cvA.Codigo, t0)
	a2, _ := h.Join(&fakeSender{}, "", "a2", "", cvA.Codigo, t0)
	b1, _ := h.Join(&fakeSender{}, "", "b1", "", cvB.Codigo, t0)
	casa, _ := h.Join(&fakeSender{}, "senha-de-casa", "Fael", "", "", t0)

	if err := h.RevogaConvite(admin.ID, cvA.Codigo); err != nil {
		t.Fatal(err)
	}

	vivos := map[string]bool{}
	for _, p := range h.Snapshot().Participants {
		vivos[p.ID] = true
	}
	if vivos[a1.ID] || vivos[a2.ID] {
		t.Fatal("os convidados do grupo A deviam ter sido derrubados")
	}
	if !vivos[b1.ID] {
		t.Fatal("revogar o grupo A nao pode derrubar o grupo B")
	}
	if !vivos[casa.ID] || !vivos[admin.ID] {
		t.Fatal("revogar convite nao pode derrubar quem eh de casa")
	}
}

func TestConvidadoNaoBaixaOApp(t *testing.T) {
	h, c := hubComConvites(t)
	cv, _ := c.Cria("x", "6h", "Grugs", t0)
	conv, _ := h.Join(&fakeSender{}, "", "visita", "", cv.Codigo, t0)
	casa, _ := h.Join(&fakeSender{}, "senha-de-casa", "Fael", "", "", t0)

	criaBinarioFalso(t)

	pede := func(token string) int {
		r := httptest.NewRequest(http.MethodGet, "/download/tela.exe?token="+token, nil)
		w := httptest.NewRecorder()
		DownloadHandler(h).ServeHTTP(w, r)
		return w.Code
	}
	if got := pede(conv.Token); got != http.StatusUnauthorized {
		t.Fatalf("convidado devia levar 401 no download, veio %d", got)
	}
	if got := pede(casa.Token); got != http.StatusOK {
		t.Fatalf("quem eh de casa devia baixar, veio %d", got)
	}
}

func TestConvidadoNaoPublicaNoMediaMTX(t *testing.T) {
	h, c := hubComConvites(t)
	cv, _ := c.Cria("x", "6h", "Grugs", t0)
	conv, _ := h.Join(&fakeSender{}, "", "visita", "", cv.Codigo, t0)

	corpo := `{"action":"publish","path":"tela/qualquer","query":"token=` + conv.Token + `","protocol":"webrtc","ip":"1.2.3.4"}`
	r := httptest.NewRequest(http.MethodPost, "/internal/auth", strings.NewReader(corpo))
	w := httptest.NewRecorder()
	AuthHandler(h).ServeHTTP(w, r)
	if w.Code != http.StatusUnauthorized {
		t.Fatalf("convidado devia ser barrado ao publicar, veio %d", w.Code)
	}
}

func TestConvidadoPodeAssistir(t *testing.T) {
	h, c := hubComConvites(t)
	cv, _ := c.Cria("x", "6h", "Grugs", t0)
	dono, _ := h.Join(&fakeSender{}, "senha-de-casa", "Fael", "", "", t0)
	conv, _ := h.Join(&fakeSender{}, "", "visita", "", cv.Codigo, t0)

	st, err := h.PublishStart(dono.ID, PublishStartData{SourceType: "monitor"})
	if err != nil {
		t.Fatal(err)
	}
	corpo := `{"action":"read","path":"tela/` + st.ID + `","query":"token=` + conv.Token + `","protocol":"webrtc","ip":"1.2.3.4"}`
	r := httptest.NewRequest(http.MethodPost, "/internal/auth", strings.NewReader(corpo))
	w := httptest.NewRecorder()
	AuthHandler(h).ServeHTTP(w, r)
	if w.Code != http.StatusOK {
		t.Fatalf("convidado tinha de conseguir assistir, veio %d", w.Code)
	}
}

// A lista mostra quantos estao usando cada convite — e o que responde "quem
// entrou pelo link que mandei".
func TestListaMostraQuantosEstaoUsando(t *testing.T) {
	h, c := hubComConvites(t)
	admin, _ := h.Join(&fakeSender{}, "senha-de-admin", "Grugs", "", "", t0)
	cv, _ := c.Cria("grupo", "6h", "Grugs", t0)
	h.Join(&fakeSender{}, "", "a", "", cv.Codigo, t0)
	h.Join(&fakeSender{}, "", "b", "", cv.Codigo, t0)

	lista, err := h.ListaConvites(admin.ID, t0)
	if err != nil {
		t.Fatal(err)
	}
	if len(lista) != 1 {
		t.Fatalf("esperava 1 convite, veio %d", len(lista))
	}
	if lista[0].EmUso != 2 {
		t.Fatalf("esperava 2 em uso, veio %d", lista[0].EmUso)
	}
	if !strings.HasSuffix(lista[0].Link, "/c/"+cv.Codigo) {
		t.Fatalf("o link tem de vir pronto para copiar: %q", lista[0].Link)
	}
}

// Senha e convite sao credenciais alternativas. Mandar as duas nao pode virar
// uma terceira regra.
func TestConviteTemPrecedenciaSobreSenha(t *testing.T) {
	h, c := hubComConvites(t)
	cv, _ := c.Cria("x", "6h", "Grugs", t0)
	s, err := h.Join(&fakeSender{}, "senha-de-admin", "esperto", "", cv.Codigo, t0)
	if err != nil {
		t.Fatal(err)
	}
	if s.Papel != PapelConvidado {
		t.Fatalf("apresentou convite, entra como convidado. veio %q", s.Papel)
	}
}
