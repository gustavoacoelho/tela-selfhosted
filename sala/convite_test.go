package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func convitesDeTeste(t *testing.T) (*Convites, string) {
	t.Helper()
	caminho := filepath.Join(t.TempDir(), "dados", "convites.json")
	c, err := AbreConvites(caminho)
	if err != nil {
		t.Fatalf("AbreConvites: %v", err)
	}
	return c, caminho
}

// O relógio é injetado em vez de lido, como no Throttle: sem isso, testar
// vencimento exigiria dormir.
var t0 = time.Date(2026, 8, 26, 20, 0, 0, 0, time.UTC)

func TestConviteValidoEntra(t *testing.T) {
	c, _ := convitesDeTeste(t)
	cv, err := c.Cria("pessoal do trabalho", "6h", "Grugs", t0)
	if err != nil {
		t.Fatalf("Cria: %v", err)
	}
	if _, ok := c.Valida(cv.Codigo, t0.Add(time.Hour)); !ok {
		t.Fatal("convite dentro do prazo devia valer")
	}
}

func TestConviteVenceSozinho(t *testing.T) {
	c, _ := convitesDeTeste(t)
	cv, _ := c.Cria("uma noite", "1h", "Grugs", t0)
	if _, ok := c.Valida(cv.Codigo, t0.Add(59*time.Minute)); !ok {
		t.Fatal("ainda devia valer um minuto antes")
	}
	if _, ok := c.Valida(cv.Codigo, t0.Add(time.Hour)); ok {
		t.Fatal("devia ter vencido exatamente no prazo")
	}
}

func TestConviteRevogadoNaoEntra(t *testing.T) {
	c, _ := convitesDeTeste(t)
	cv, _ := c.Cria("alguém", "24h", "Grugs", t0)
	achou, err := c.Revoga(cv.Codigo)
	if err != nil || !achou {
		t.Fatalf("Revoga: achou=%v err=%v", achou, err)
	}
	if _, ok := c.Valida(cv.Codigo, t0); ok {
		t.Fatal("revogado não pode entrar, mesmo dentro do prazo")
	}
}

func TestConviteInexistenteNaoEntra(t *testing.T) {
	c, _ := convitesDeTeste(t)
	if _, ok := c.Valida("codigo-que-nunca-existiu", t0); ok {
		t.Fatal("código inventado não pode entrar")
	}
}

func TestRevogarInexistenteNaoQuebra(t *testing.T) {
	c, _ := convitesDeTeste(t)
	achou, err := c.Revoga("nada")
	if err != nil {
		t.Fatalf("erro inesperado: %v", err)
	}
	if achou {
		t.Fatal("não devia dizer que achou")
	}
}

func TestPrazoForaDaListaEhRecusado(t *testing.T) {
	c, _ := convitesDeTeste(t)
	// O prazo vem do cliente. "10 anos" não pode virar convite.
	for _, prazo := range []string{"87600h", "", "6", "6 h", "999h"} {
		if _, err := c.Cria("x", prazo, "Grugs", t0); err != ErrPrazoInvalido {
			t.Fatalf("prazo %q devia ser recusado, veio %v", prazo, err)
		}
	}
}

func TestRotuloVazioEhRecusado(t *testing.T) {
	c, _ := convitesDeTeste(t)
	if _, err := c.Cria("   ", "6h", "Grugs", t0); err != ErrRotuloInvalido {
		t.Fatalf("rótulo em branco devia ser recusado, veio %v", err)
	}
}

func TestRotuloCompridoEhCortado(t *testing.T) {
	c, _ := convitesDeTeste(t)
	cv, err := c.Cria(strings.Repeat("a", 500), "6h", "Grugs", t0)
	if err != nil {
		t.Fatalf("Cria: %v", err)
	}
	if len(cv.Rotulo) > MaxRotulo {
		t.Fatalf("rótulo não foi cortado: %d caracteres", len(cv.Rotulo))
	}
}

// Sobreviver a restart é o motivo de isto estar em disco.
func TestConvitesSobrevivemAoRestart(t *testing.T) {
	c, caminho := convitesDeTeste(t)
	cv, _ := c.Cria("pessoal", "24h", "Grugs", t0)

	outro, err := AbreConvites(caminho)
	if err != nil {
		t.Fatalf("reabrir: %v", err)
	}
	if _, ok := outro.Valida(cv.Codigo, t0.Add(time.Hour)); !ok {
		t.Fatal("o convite não sobreviveu ao restart — era o motivo de gravar em disco")
	}
}

func TestRevogacaoSobreviveAoRestart(t *testing.T) {
	c, caminho := convitesDeTeste(t)
	cv, _ := c.Cria("pessoal", "24h", "Grugs", t0)
	c.Revoga(cv.Codigo)

	outro, _ := AbreConvites(caminho)
	if _, ok := outro.Valida(cv.Codigo, t0); ok {
		t.Fatal("revogação não pode ser desfeita por um restart")
	}
}

// Arquivo corrompido não pode derrubar a sala: convite é conveniência, entrar
// na sala é o serviço.
func TestArquivoCorrompidoNaoDerrubaOServico(t *testing.T) {
	caminho := filepath.Join(t.TempDir(), "convites.json")
	if err := os.WriteFile(caminho, []byte("{ isto não é json"), 0o644); err != nil {
		t.Fatal(err)
	}
	c, err := AbreConvites(caminho)
	if err == nil {
		t.Fatal("o erro devia ser devolvido para quem chama registrar")
	}
	if c == nil {
		t.Fatal("mesmo com erro tem de vir um store utilizável")
	}
	// E continua funcionando dali para a frente.
	if _, err := c.Cria("novo", "6h", "Grugs", t0); err != nil {
		t.Fatalf("devia dar para criar convite mesmo assim: %v", err)
	}
}

func TestAtivosOmiteVencidoERevogado(t *testing.T) {
	c, _ := convitesDeTeste(t)
	vivo, _ := c.Cria("vivo", "24h", "Grugs", t0)
	curto, _ := c.Cria("curto", "1h", "Grugs", t0)
	morto, _ := c.Cria("morto", "24h", "Grugs", t0)
	c.Revoga(morto.Codigo)

	depois := t0.Add(2 * time.Hour)
	ativos := c.Ativos(depois)
	if len(ativos) != 1 || ativos[0].Codigo != vivo.Codigo {
		t.Fatalf("esperava só o convite vivo, veio %d: %+v", len(ativos), ativos)
	}
	_ = curto
}

func TestLimpaDescartaOQueNaoServeMais(t *testing.T) {
	c, caminho := convitesDeTeste(t)
	curto, _ := c.Cria("curto", "1h", "Grugs", t0)
	longo, _ := c.Cria("longo", "24h", "Grugs", t0)

	c.Limpa(t0.Add(2 * time.Hour))

	outro, _ := AbreConvites(caminho)
	if _, ok := outro.Valida(longo.Codigo, t0.Add(2*time.Hour)); !ok {
		t.Fatal("a faxina levou um convite que ainda valia")
	}
	if _, existe := outro.convites[curto.Codigo]; existe {
		t.Fatal("o vencido devia ter saído do arquivo")
	}
}

// Dois códigos iguais dariam a duas pessoas o mesmo passe.
func TestCodigosNaoSeRepetem(t *testing.T) {
	c, _ := convitesDeTeste(t)
	vistos := map[string]bool{}
	for i := 0; i < 200; i++ {
		cv, err := c.Cria("x", "6h", "Grugs", t0)
		if err != nil {
			t.Fatal(err)
		}
		if vistos[cv.Codigo] {
			t.Fatalf("código repetido: %s", cv.Codigo)
		}
		vistos[cv.Codigo] = true
		if len(cv.Codigo) != TamanhoCodigo {
			t.Fatalf("tamanho errado: %d", len(cv.Codigo))
		}
		for _, r := range cv.Codigo {
			if !strings.ContainsRune(AlfabetoCodigo, r) {
				t.Fatalf("caractere fora do alfabeto: %q", r)
			}
		}
	}
}
