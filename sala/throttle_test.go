package main

import (
	"bytes"
	"log"
	"os"
	"strings"
	"sync"
	"testing"
	"time"
)

func TestPermiteAteQuatroFalhasSeguidas(t *testing.T) {
	th := NewThrottle(5, 5*time.Minute, 15*time.Minute)
	base := time.Now()

	for i := 0; i < 4; i++ {
		ok, _ := th.Permitido("1.2.3.4", base)
		if !ok {
			t.Fatalf("falha %d: esperava permitido=true antes da 5ª falha", i+1)
		}
		th.RegistraFalha("1.2.3.4", "", base)
	}

	ok, _ := th.Permitido("1.2.3.4", base)
	if !ok {
		t.Fatal("após 4 falhas ainda deveria estar permitido")
	}
}

func TestQuintaFalhaBloqueia(t *testing.T) {
	th := NewThrottle(5, 5*time.Minute, 15*time.Minute)
	base := time.Now()

	for i := 0; i < 5; i++ {
		th.RegistraFalha("1.2.3.4", "", base)
	}

	ok, restante := th.Permitido("1.2.3.4", base)
	if ok {
		t.Fatal("após 5 falhas deveria estar bloqueado")
	}
	if restante <= 0 || restante > 15*time.Minute {
		t.Fatalf("tempo restante fora do esperado: %v", restante)
	}
}

func TestBloqueioExpiraApos15Min(t *testing.T) {
	th := NewThrottle(5, 5*time.Minute, 15*time.Minute)
	base := time.Now()

	for i := 0; i < 5; i++ {
		th.RegistraFalha("1.2.3.4", "", base)
	}

	// Ainda dentro do bloqueio.
	ok, _ := th.Permitido("1.2.3.4", base.Add(14*time.Minute))
	if ok {
		t.Fatal("aos 14min ainda deveria estar bloqueado")
	}

	// Depois do bloqueio.
	ok, _ = th.Permitido("1.2.3.4", base.Add(15*time.Minute+time.Second))
	if !ok {
		t.Fatal("após 15min+1s deveria estar liberado")
	}
}

func TestSucessoZeraContador(t *testing.T) {
	th := NewThrottle(5, 5*time.Minute, 15*time.Minute)
	base := time.Now()

	th.RegistraFalha("1.2.3.4", "", base)
	th.RegistraFalha("1.2.3.4", "", base)
	th.RegistraSucesso("1.2.3.4")

	// Mais 4 falhas depois do sucesso não deveriam bloquear (total seria 4, não 6).
	for i := 0; i < 4; i++ {
		th.RegistraFalha("1.2.3.4", "", base)
	}
	ok, _ := th.Permitido("1.2.3.4", base)
	if !ok {
		t.Fatal("sucesso deveria ter zerado o contador de falhas")
	}
}

func TestFalhasForaDaJanelaNaoAcumulam(t *testing.T) {
	th := NewThrottle(5, 5*time.Minute, 15*time.Minute)
	base := time.Now()

	// 4 falhas bem antigas, fora da janela de 5min.
	for i := 0; i < 4; i++ {
		th.RegistraFalha("1.2.3.4", "", base)
	}

	depois := base.Add(6 * time.Minute)
	// Mais 4 falhas dentro da nova janela — total "vivo" é só 4, não deveria bloquear.
	for i := 0; i < 4; i++ {
		th.RegistraFalha("1.2.3.4", "", depois)
	}

	ok, _ := th.Permitido("1.2.3.4", depois)
	if !ok {
		t.Fatal("falhas antigas fora da janela não deveriam contar para o bloqueio")
	}
}

func TestIPsDiferentesNaoInterferem(t *testing.T) {
	th := NewThrottle(5, 5*time.Minute, 15*time.Minute)
	base := time.Now()

	for i := 0; i < 5; i++ {
		th.RegistraFalha("1.1.1.1", "", base)
	}

	ok, _ := th.Permitido("1.1.1.1", base)
	if ok {
		t.Fatal("1.1.1.1 deveria estar bloqueado")
	}

	ok, _ = th.Permitido("2.2.2.2", base)
	if !ok {
		t.Fatal("2.2.2.2 não deveria ser afetado pelo bloqueio de 1.1.1.1")
	}
}

func TestLimpaRemoveEntradaVelhaEMantemBloqueioVigente(t *testing.T) {
	th := NewThrottle(5, 5*time.Minute, 15*time.Minute)
	base := time.Now()

	// IP com poucas falhas antigas — deve ser varrido pela limpeza.
	th.RegistraFalha("3.3.3.3", "", base)

	// IP bloqueado — não deve ser removido enquanto o bloqueio ainda vale.
	for i := 0; i < 5; i++ {
		th.RegistraFalha("4.4.4.4", "", base)
	}

	// Momento bem depois: janela de falhas de 3.3.3.3 já expirou, mas o
	// bloqueio de 4.4.4.4 (15min) ainda está de pé aos 10min.
	momento := base.Add(10 * time.Minute)
	th.Limpa(momento)

	if th.temEntrada("3.3.3.3") {
		t.Fatal("entrada velha de 3.3.3.3 deveria ter sido limpa")
	}
	if !th.temEntrada("4.4.4.4") {
		t.Fatal("entrada de 4.4.4.4, ainda bloqueada, não deveria ter sido limpa")
	}

	ok, _ := th.Permitido("4.4.4.4", momento)
	if ok {
		t.Fatal("4.4.4.4 ainda deveria estar bloqueado aos 10min")
	}
}

func TestRegistraFalhaConcorrente(t *testing.T) {
	th := NewThrottle(5, 5*time.Minute, 15*time.Minute)
	base := time.Now()

	var wg sync.WaitGroup
	for i := 0; i < 50; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			th.RegistraFalha("5.5.5.5", "", base)
		}()
	}
	wg.Wait()

	// Só verificamos que não houve corrida (rodar com -race) e que o estado
	// é consistente: IP deve estar bloqueado após 50 falhas.
	ok, _ := th.Permitido("5.5.5.5", base)
	if ok {
		t.Fatal("após 50 falhas concorrentes o IP deveria estar bloqueado")
	}
}

// Um bloqueio invisível não é auditável: depois da troca de senha, "quem ficou
// bloqueado?" não tinha resposta. O log é a resposta, e ele só pode aparecer na
// transição — uma linha por tentativa de quem já está bloqueado seria ruído.
func TestBloqueioRegistraUmaVezSo(t *testing.T) {
	var buf bytes.Buffer
	log.SetOutput(&buf)
	t.Cleanup(func() { log.SetOutput(os.Stderr) })

	th := NewThrottle(3, 5*time.Minute, 15*time.Minute)
	agora := time.Date(2026, 8, 26, 3, 0, 0, 0, time.UTC)
	for i := 0; i < 8; i++ {
		th.RegistraFalha("1.2.3.4", "", agora.Add(time.Duration(i)*time.Second))
	}

	n := strings.Count(buf.String(), "bloqueio: 1.2.3.4")
	if n != 1 {
		t.Fatalf("esperava 1 linha de bloqueio, veio %d:\n%s", n, buf.String())
	}
}

// Bloqueio novo depois de o anterior expirar merece linha nova.
func TestBloqueioNovoDepoisDeExpirarRegistraDeNovo(t *testing.T) {
	var buf bytes.Buffer
	log.SetOutput(&buf)
	t.Cleanup(func() { log.SetOutput(os.Stderr) })

	th := NewThrottle(2, time.Minute, 5*time.Minute)
	agora := time.Date(2026, 8, 26, 3, 0, 0, 0, time.UTC)
	th.RegistraFalha("9.9.9.9", "", agora)
	th.RegistraFalha("9.9.9.9", "", agora)

	depois := agora.Add(10 * time.Minute)
	th.RegistraFalha("9.9.9.9", "", depois)
	th.RegistraFalha("9.9.9.9", "", depois)

	if n := strings.Count(buf.String(), "bloqueio: 9.9.9.9"); n != 2 {
		t.Fatalf("esperava 2 linhas, veio %d:\n%s", n, buf.String())
	}
}

// O log do bloqueio tem de dizer QUEM, nao so o IP.
//
// Sem o apelido, a pergunta "quem ficou bloqueado?" dependia de o bloqueio
// ainda estar vivo no painel. Um amigo usando VPN apareceu como IP de
// datacenter e quase foi tratado como estranho batendo na porta.
func TestBloqueioRegistraOApelido(t *testing.T) {
	var buf bytes.Buffer
	log.SetOutput(&buf)
	t.Cleanup(func() { log.SetOutput(os.Stderr) })

	th := NewThrottle(3, 5*time.Minute, 15*time.Minute)
	agora := time.Date(2026, 9, 18, 3, 0, 0, 0, time.UTC)
	for i := 0; i < 3; i++ {
		th.RegistraFalha("203.0.113.7", "convidado curioso", agora.Add(time.Duration(i)*time.Second))
	}
	if !strings.Contains(buf.String(), `"convidado curioso"`) {
		t.Fatalf("o log do bloqueio nao disse quem era: %s", buf.String())
	}
}
