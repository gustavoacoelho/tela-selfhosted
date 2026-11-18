package main

import (
	"testing"
	"time"
)

var tb = time.Date(2026, 8, 28, 0, 0, 0, 0, time.UTC)

func bloqueia(th *Throttle, ip, nick string, quantas int, agora time.Time) {
	for i := 0; i < quantas; i++ {
		th.RegistraFalha(ip, nick, agora.Add(time.Duration(i)*time.Second))
	}
}

func TestDesbloqueiaLiberaNaHora(t *testing.T) {
	th := NewThrottle(5, 5*time.Minute, 15*time.Minute)
	bloqueia(th, "1.2.3.4", "convidado curioso", 5, tb)

	if ok, _ := th.Permitido("1.2.3.4", tb); ok {
		t.Fatal("devia estar bloqueado")
	}
	if !th.Desbloqueia("1.2.3.4") {
		t.Fatal("Desbloqueia devia ter achado o bloqueio")
	}
	if ok, _ := th.Permitido("1.2.3.4", tb); !ok {
		t.Fatal("depois de desbloquear tem de poder tentar")
	}
}

// O caso que tornaria o botao inutil.
//
// Se o desbloqueio apagasse so a data e deixasse as falhas acumuladas, a
// PRIMEIRA tentativa seguinte ja recolocaria o IP no teto e a pessoa travaria
// de novo na hora. "Desbloqueei e travou outra vez" nao e desbloquear.
func TestDesbloqueiaZeraOHistoricoDeFalhas(t *testing.T) {
	th := NewThrottle(5, 5*time.Minute, 15*time.Minute)
	bloqueia(th, "1.2.3.4", "alguem", 5, tb)
	th.Desbloqueia("1.2.3.4")

	// Uma tentativa errada depois do desbloqueio nao pode bloquear de novo.
	th.RegistraFalha("1.2.3.4", "alguem", tb.Add(time.Minute))
	if ok, _ := th.Permitido("1.2.3.4", tb.Add(time.Minute)); !ok {
		t.Fatal("uma unica falha nao pode rebloquear: o historico devia ter sido zerado")
	}
}

func TestDesbloquearQuemNaoEstaBloqueadoNaoQuebra(t *testing.T) {
	th := NewThrottle(5, 5*time.Minute, 15*time.Minute)
	if th.Desbloqueia("9.9.9.9") {
		t.Fatal("nao devia dizer que achou")
	}
	// IP com falhas mas ainda sem bloqueio tambem nao conta.
	bloqueia(th, "8.8.8.8", "x", 2, tb)
	if th.Desbloqueia("8.8.8.8") {
		t.Fatal("2 falhas nao e bloqueio")
	}
}

func TestDesbloquearUmNaoLiberaOsOutros(t *testing.T) {
	th := NewThrottle(5, 5*time.Minute, 15*time.Minute)
	bloqueia(th, "1.1.1.1", "a", 5, tb)
	bloqueia(th, "2.2.2.2", "b", 5, tb)

	th.Desbloqueia("1.1.1.1")

	if ok, _ := th.Permitido("1.1.1.1", tb); !ok {
		t.Fatal("1.1.1.1 devia ter sido liberado")
	}
	if ok, _ := th.Permitido("2.2.2.2", tb); ok {
		t.Fatal("2.2.2.2 nao devia ter sido tocado")
	}
}

// A lista precisa dizer QUEM e, senao e uma coluna de numeros que nao ajuda
// ninguem a decidir.
func TestListaDeBloqueadosTrazApelidoETempo(t *testing.T) {
	th := NewThrottle(5, 5*time.Minute, 15*time.Minute)
	bloqueia(th, "1.2.3.4", "convidado curioso", 5, tb)

	lista := th.Bloqueados(tb.Add(time.Minute))
	if len(lista) != 1 {
		t.Fatalf("esperava 1 bloqueado, veio %d", len(lista))
	}
	b := lista[0]
	if b.IP != "1.2.3.4" {
		t.Fatalf("ip errado: %q", b.IP)
	}
	if b.Nick != "convidado curioso" {
		t.Fatalf("sem o apelido a lista nao ajuda a decidir: %q", b.Nick)
	}
	if b.Falhas != 5 {
		t.Fatalf("falhas: %d", b.Falhas)
	}
	// Bloqueio de 15 min, um minuto ja passou.
	if b.FaltaSeg < 13*60 || b.FaltaSeg > 15*60 {
		t.Fatalf("tempo restante fora do esperado: %ds", b.FaltaSeg)
	}
}

func TestListaOmiteQuemNaoEstaBloqueado(t *testing.T) {
	th := NewThrottle(5, 5*time.Minute, 15*time.Minute)
	bloqueia(th, "1.1.1.1", "bloqueado", 5, tb)
	bloqueia(th, "2.2.2.2", "so tropecou", 2, tb)

	lista := th.Bloqueados(tb)
	if len(lista) != 1 || lista[0].IP != "1.1.1.1" {
		t.Fatalf("esperava so o bloqueado, veio %+v", lista)
	}

	// E some sozinho quando o prazo passa.
	if l := th.Bloqueados(tb.Add(16 * time.Minute)); len(l) != 0 {
		t.Fatalf("bloqueio vencido nao devia aparecer: %+v", l)
	}
}
