package main

import (
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func mandaLog(t *testing.T, h *Hub, token, corpo string) *httptest.ResponseRecorder {
	t.Helper()
	rec := httptest.NewRecorder()
	req := httptest.NewRequest(http.MethodPost, "/log?token="+token, strings.NewReader(corpo))
	LogHandler(h).ServeHTTP(rec, req)
	return rec
}

// Muda para um diretório temporário: `gravaLog` escreve relativo ao diretório
// de trabalho, e um teste não pode sujar o repositório.
func emPastaTemporaria(t *testing.T) {
	t.Helper()
	antigo, err := os.Getwd()
	if err != nil {
		t.Fatal(err)
	}
	if err := os.Chdir(t.TempDir()); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.Chdir(antigo) })
}

func TestLogExigeTokenDeSessao(t *testing.T) {
	emPastaTemporaria(t)
	h := NewHub("segredo")

	if code := mandaLog(t, h, "", "linha").Code; code != http.StatusUnauthorized {
		t.Fatalf("sem token: code = %d, esperado 401", code)
	}
	if code := mandaLog(t, h, "inventado", "linha").Code; code != http.StatusUnauthorized {
		t.Fatalf("token inválido: code = %d, esperado 401", code)
	}
	if _, err := os.Stat(DirLog); !os.IsNotExist(err) {
		t.Fatal("não devia ter criado a pasta de logs para um token inválido")
	}
}

func TestLogGravaComONomeDaPessoa(t *testing.T) {
	emPastaTemporaria(t)
	h := NewHub("segredo")
	s, err := h.Join(&fakeSender{}, "segredo", "fael", "", "", time.Now())
	if err != nil {
		t.Fatal(err)
	}

	if code := mandaLog(t, h, s.Token, "primeira leva\n").Code; code != http.StatusNoContent {
		t.Fatalf("code = %d, esperado 204", code)
	}
	if code := mandaLog(t, h, s.Token, "segunda leva\n").Code; code != http.StatusNoContent {
		t.Fatalf("code = %d, esperado 204", code)
	}

	b, err := os.ReadFile(filepath.Join(DirLog, "fael.log"))
	if err != nil {
		t.Fatal(err)
	}
	texto := string(b)
	// Acrescenta, não sobrescreve: o segundo envio não pode apagar o primeiro.
	if !strings.Contains(texto, "primeira leva") || !strings.Contains(texto, "segunda leva") {
		t.Fatalf("as duas levas tinham de estar no arquivo: %q", texto)
	}
}

// O apelido vem de quem digitou e vira nome de arquivo. Sem limpeza, um
// apelido com travessia escreveria fora da pasta de logs.
func TestApelidoNaoEscapaDaPasta(t *testing.T) {
	for _, caso := range []struct{ nick, esperado string }{
		// O `Trim` come os pontos e traços das pontas, então sobra só o miolo.
		{"../../etc/passwd", "etc_passwd"},
		{`C:\Windows\x`, "C_Windows_x"},
		{"...", "sem-nome"},
		{"", "sem-nome"},
		{"fael", "fael"},
		{"Zé Ninguém", "Z_Ningu_m"},
	} {
		got := nomeDeArquivo(caso.nick)
		if got != caso.esperado {
			t.Errorf("nomeDeArquivo(%q) = %q, esperado %q", caso.nick, got, caso.esperado)
		}
		if strings.ContainsAny(got, `/\`) || strings.Contains(got, "..") {
			t.Errorf("nomeDeArquivo(%q) = %q ainda escapa", caso.nick, got)
		}
	}
}

func TestLogGrandeDemaisEhRecusado(t *testing.T) {
	emPastaTemporaria(t)
	h := NewHub("segredo")
	s, _ := h.Join(&fakeSender{}, "segredo", "gus", "", "", time.Now())

	grande := strings.Repeat("x", MaxLogPorEnvio+1)
	if code := mandaLog(t, h, s.Token, grande).Code; code != http.StatusRequestEntityTooLarge {
		t.Fatalf("code = %d, esperado 413", code)
	}
}
