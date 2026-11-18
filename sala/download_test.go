package main

import (
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"testing"
	"time"
)

func pedeDownload(t *testing.T, h *Hub, caminho string) *httptest.ResponseRecorder {
	t.Helper()
	rec := httptest.NewRecorder()
	req := httptest.NewRequest(http.MethodGet, caminho, nil)
	DownloadHandler(h).ServeHTTP(rec, req)
	return rec
}

// Cria `download/tela.exe` no diretório de trabalho do teste e devolve o
// conteúdo esperado.
func criaBinarioFalso(t *testing.T) []byte {
	t.Helper()
	dir := t.TempDir()
	antigo, err := os.Getwd()
	if err != nil {
		t.Fatal(err)
	}
	if err := os.Chdir(dir); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.Chdir(antigo) })

	if err := os.Mkdir(DirDownload, 0o755); err != nil {
		t.Fatal(err)
	}
	conteudo := []byte("MZ binario falso")
	if err := os.WriteFile(filepath.Join(DirDownload, "tela.exe"), conteudo, 0o644); err != nil {
		t.Fatal(err)
	}
	return conteudo
}

func TestDownloadExigeToken(t *testing.T) {
	criaBinarioFalso(t)
	h := NewHub("segredo")

	if code := pedeDownload(t, h, "/download/tela.exe").Code; code != http.StatusUnauthorized {
		t.Fatalf("sem token: code = %d, esperado 401", code)
	}
	if code := pedeDownload(t, h, "/download/tela.exe?token=inventado").Code; code != http.StatusUnauthorized {
		t.Fatalf("token inválido: code = %d, esperado 401", code)
	}
}

func TestDownloadComTokenValido(t *testing.T) {
	esperado := criaBinarioFalso(t)
	h := NewHub("segredo")
	s, err := h.Join(&fakeSender{}, "segredo", "gus", "", "", time.Now())
	if err != nil {
		t.Fatal(err)
	}

	rec := pedeDownload(t, h, "/download/tela.exe?token="+s.Token)
	if rec.Code != http.StatusOK {
		t.Fatalf("code = %d, esperado 200", rec.Code)
	}
	if got := rec.Body.String(); got != string(esperado) {
		t.Fatalf("corpo = %q, esperado %q", got, esperado)
	}
	if cc := rec.Header().Get("Cache-Control"); cc != "no-store" {
		t.Fatalf("Cache-Control = %q, esperado no-store", cc)
	}
}

func TestDownloadArquivoInexistente(t *testing.T) {
	criaBinarioFalso(t)
	h := NewHub("segredo")
	s, _ := h.Join(&fakeSender{}, "segredo", "gus", "", "", time.Now())

	if code := pedeDownload(t, h, "/download/naoexiste.exe?token="+s.Token).Code; code != http.StatusNotFound {
		t.Fatalf("code = %d, esperado 404", code)
	}
}

// Travessia de diretório: mesmo autenticado, ninguém deve conseguir ler fora
// da pasta de download.
func TestDownloadNaoEscapaDoDiretorio(t *testing.T) {
	criaBinarioFalso(t)
	if err := os.WriteFile("segredo.txt", []byte("nao deveria vazar"), 0o644); err != nil {
		t.Fatal(err)
	}
	h := NewHub("segredo")
	s, _ := h.Join(&fakeSender{}, "segredo", "gus", "", "", time.Now())

	for _, caminho := range []string{
		"/download/../segredo.txt",
		"/download/..%2Fsegredo.txt",
		"/download/subdir/../../segredo.txt",
	} {
		rec := pedeDownload(t, h, caminho+"?token="+s.Token)
		if rec.Code == http.StatusOK {
			t.Errorf("%s vazou conteúdo: %q", caminho, rec.Body.String())
		}
	}
}

// O manifesto de versão precisa ser legível sem token: o app consulta antes de
// entrar na sala. Os binários, não.
func TestManifestoEhPublicoMasBinarioNao(t *testing.T) {
	criaBinarioFalso(t)
	if err := os.WriteFile(filepath.Join(DirDownload, ManifestoPublico),
		[]byte(`{"versao":"0.1.0"}`), 0o644); err != nil {
		t.Fatal(err)
	}
	h := NewHub("segredo")

	if code := pedeDownload(t, h, "/download/"+ManifestoPublico).Code; code != http.StatusOK {
		t.Fatalf("manifesto sem token: code = %d, esperado 200", code)
	}
	if code := pedeDownload(t, h, "/download/tela.exe").Code; code != http.StatusUnauthorized {
		t.Fatalf("binário sem token: code = %d, esperado 401", code)
	}
	// A isenção vale só para o nome exato, não para qualquer coisa parecida.
	if code := pedeDownload(t, h, "/download/versao.json.exe").Code; code != http.StatusUnauthorized {
		t.Fatalf("nome parecido: code = %d, esperado 401", code)
	}
}
