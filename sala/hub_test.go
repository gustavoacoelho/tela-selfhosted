package main

import (
	"encoding/json"
	"sync"
	"testing"
	"time"
)

// fakeSender captura o que o hub enviaria pela rede.
type fakeSender struct {
	mu   sync.Mutex
	sent []Envelope
}

func (f *fakeSender) Send(env Envelope) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.sent = append(f.sent, env)
}

func (f *fakeSender) types() []string {
	f.mu.Lock()
	defer f.mu.Unlock()
	out := make([]string, len(f.sent))
	for i, e := range f.sent {
		out[i] = e.Type
	}
	return out
}

func (f *fakeSender) last(typ string) (Envelope, bool) {
	f.mu.Lock()
	defer f.mu.Unlock()
	for i := len(f.sent) - 1; i >= 0; i-- {
		if f.sent[i].Type == typ {
			return f.sent[i], true
		}
	}
	return Envelope{}, false
}

func TestJoinWrongPassword(t *testing.T) {
	h := NewHub("segredo")
	s := &fakeSender{}

	if _, err := h.Join(s, "errada", "gus", "", "", time.Now()); err == nil {
		t.Fatal("esperava erro com senha errada")
	}
	if n := len(h.Snapshot().Participants); n != 0 {
		t.Fatalf("participantes = %d, esperado 0", n)
	}
}

func TestJoinAndBroadcast(t *testing.T) {
	h := NewHub("segredo")
	a, b := &fakeSender{}, &fakeSender{}

	sa, err := h.Join(a, "segredo", "gus", "", "", time.Now())
	if err != nil {
		t.Fatalf("join a: %v", err)
	}
	if _, ok := a.last("welcome"); !ok {
		t.Fatalf("a não recebeu welcome, recebeu %v", a.types())
	}

	// A entrada de b deve notificar a.
	if _, err := h.Join(b, "segredo", "ana", "", "", time.Now()); err != nil {
		t.Fatalf("join b: %v", err)
	}
	env, ok := a.last("state")
	if !ok {
		t.Fatalf("a não recebeu state após b entrar, recebeu %v", a.types())
	}
	var st StateData
	if err := json.Unmarshal(env.Data, &st); err != nil {
		t.Fatal(err)
	}
	if len(st.Participants) != 2 {
		t.Fatalf("participantes = %d, esperado 2", len(st.Participants))
	}

	// Sair reduz o estado.
	h.Leave(sa.ID)
	if n := len(h.Snapshot().Participants); n != 1 {
		t.Fatalf("após leave, participantes = %d, esperado 1", n)
	}
}

func TestNickVazioRejeitado(t *testing.T) {
	h := NewHub("segredo")
	if _, err := h.Join(&fakeSender{}, "segredo", "   ", "", "", time.Now()); err == nil {
		t.Fatal("esperava erro com nick vazio")
	}
}

func TestPingRTT(t *testing.T) {
	h := NewHub("segredo")
	f := &fakeSender{}
	s, err := h.Join(f, "segredo", "gus", "", "", time.Now())
	if err != nil {
		t.Fatal(err)
	}

	h.SendPing(nowMs())
	env, ok := f.last("ping")
	if !ok {
		t.Fatalf("não recebeu ping, recebeu %v", f.types())
	}
	var p PingData
	if err := json.Unmarshal(env.Data, &p); err != nil {
		t.Fatal(err)
	}

	// Responde 40 ms depois.
	h.OnPong(s.ID, p.T, p.T+40, 0, 0, 0, 0)

	if got := h.Snapshot().Participants[0].PingMs; got != 40 {
		t.Fatalf("pingMs = %d, esperado 40", got)
	}
}

func TestPongForaDeOrdemIgnorado(t *testing.T) {
	h := NewHub("segredo")
	s, _ := h.Join(&fakeSender{}, "segredo", "gus", "", "", time.Now())

	h.SendPing(1000)
	h.OnPong(s.ID, 1000, 1050, 0, 0, 0, 0) // válido -> 50
	h.OnPong(s.ID, 999, 9999, 0, 0, 0, 0)  // ping antigo -> deve ser ignorado

	if got := h.Snapshot().Participants[0].PingMs; got != 50 {
		t.Fatalf("pingMs = %d, esperado 50", got)
	}
}

func TestPublishStartStop(t *testing.T) {
	h := NewHub("segredo")
	f := &fakeSender{}
	s, _ := h.Join(f, "segredo", "gus", "", "", time.Now())

	st, err := h.PublishStart(s.ID, PublishStartData{
		SourceType:  "window",
		SourceTitle: "Elden Ring",
		Settings:    StreamSettings{W: 1920, H: 1080, FPS: 60, Kbps: 12000},
	})
	if err != nil {
		t.Fatalf("PublishStart: %v", err)
	}
	if st.ID == "" || st.OwnerID != s.ID {
		t.Fatalf("stream = %+v", st)
	}

	snap := h.Snapshot()
	if len(snap.Streams) != 1 {
		t.Fatalf("streams = %d, esperado 1", len(snap.Streams))
	}
	if snap.Participants[0].Sharing != st.ID {
		t.Fatalf("sharing = %q, esperado %q", snap.Participants[0].Sharing, st.ID)
	}

	// Segunda chamada substitui o stream anterior em vez de duplicar.
	st2, err := h.PublishStart(s.ID, PublishStartData{SourceType: "monitor"})
	if err != nil {
		t.Fatal(err)
	}
	if n := len(h.Snapshot().Streams); n != 1 {
		t.Fatalf("streams após republish = %d, esperado 1", n)
	}
	if st2.ID == st.ID {
		t.Fatal("republish deveria gerar novo streamId")
	}

	h.PublishStop(s.ID)
	if n := len(h.Snapshot().Streams); n != 0 {
		t.Fatalf("streams após stop = %d, esperado 0", n)
	}
}

func TestStatsAtualizaStream(t *testing.T) {
	h := NewHub("segredo")
	s, _ := h.Join(&fakeSender{}, "segredo", "gus", "", "", time.Now())
	st, _ := h.PublishStart(s.ID, PublishStartData{SourceType: "monitor"})

	h.UpdateStats(s.ID, PublishStatsData{Kbps: 7800, FPS: 59, Dropped: 3})

	got := h.Snapshot().Streams[0]
	if got.ID != st.ID || got.Stats.Kbps != 7800 || got.Stats.FPS != 59 || got.Stats.Dropped != 3 {
		t.Fatalf("stats = %+v", got.Stats)
	}
}

func TestLeaveRemoveStream(t *testing.T) {
	h := NewHub("segredo")
	s, _ := h.Join(&fakeSender{}, "segredo", "gus", "", "", time.Now())
	h.PublishStart(s.ID, PublishStartData{SourceType: "monitor"})

	h.Leave(s.ID)
	if n := len(h.Snapshot().Streams); n != 0 {
		t.Fatalf("streams órfãos = %d, esperado 0", n)
	}
}

func TestWatchTracking(t *testing.T) {
	h := NewHub("segredo")
	a, _ := h.Join(&fakeSender{}, "segredo", "gus", "", "", time.Now())
	b, _ := h.Join(&fakeSender{}, "segredo", "ana", "", "", time.Now())
	st, _ := h.PublishStart(a.ID, PublishStartData{SourceType: "monitor"})

	h.WatchStart(b.ID, st.ID)
	for _, p := range h.Snapshot().Participants {
		if p.ID == b.ID && len(p.Watching) != 1 {
			t.Fatalf("watching de b = %v, esperado 1 item", p.Watching)
		}
	}

	h.WatchStop(b.ID, st.ID)
	for _, p := range h.Snapshot().Participants {
		if p.ID == b.ID && len(p.Watching) != 0 {
			t.Fatalf("watching de b = %v, esperado vazio", p.Watching)
		}
	}
}

// O cliente informa a latência de rede de verdade, medida por ele na camada
// ICE. Ela precisa chegar inteira no snapshot: é o número que a sala mostra,
// e o `pingMs` ao lado dela mede outra coisa (ida e volta pelo WebSocket, com
// o atraso do JavaScript de quem responde embutido).
func TestOnPongGuardaRttEArrasto(t *testing.T) {
	h := NewHub("x")
	s, _ := h.Join(&fakeSender{}, "x", "leo", "", "", time.Now())
	h.SendPing(1000)
	h.OnPong(s.ID, 1000, 1250, 22, 180, 0, 0)

	p := acha(h.Snapshot().Participants, s.ID)
	if p.PingMs != 250 {
		t.Fatalf("pingMs: quero 250, veio %d", p.PingMs)
	}
	if p.RttMs != 22 {
		t.Fatalf("rttMs: quero 22, veio %d", p.RttMs)
	}
	if p.TravaMs != 180 {
		t.Fatalf("travaMs: quero 180, veio %d", p.TravaMs)
	}
}

// Cliente antigo não manda os campos novos. Não pode virar lixo na lista.
func TestOnPongSemOsCamposNovos(t *testing.T) {
	h := NewHub("x")
	s, _ := h.Join(&fakeSender{}, "x", "antigo", "", "", time.Now())
	h.SendPing(1000)
	h.OnPong(s.ID, 1000, 1030, 0, 0, 0, 0)

	p := acha(h.Snapshot().Participants, s.ID)
	if p.RttMs != 0 || p.TravaMs != 0 {
		t.Fatalf("quero zero nos dois, veio rtt=%d trava=%d", p.RttMs, p.TravaMs)
	}
}

// Número absurdo vindo do cliente não pode ir para a tela: não temos como
// verificar o que ele manda, então existe um teto.
func TestOnPongRecusaNumeroAbsurdo(t *testing.T) {
	h := NewHub("x")
	s, _ := h.Join(&fakeSender{}, "x", "quebrado", "", "", time.Now())
	h.SendPing(1000)
	h.OnPong(s.ID, 1000, 1030, -5, 999_999_999, 0, 0)

	p := acha(h.Snapshot().Participants, s.ID)
	if p.RttMs != 0 || p.TravaMs != 0 {
		t.Fatalf("quero zero nos dois, veio rtt=%d trava=%d", p.RttMs, p.TravaMs)
	}
}

func acha(ps []Participant, id string) Participant {
	for _, p := range ps {
		if p.ID == id {
			return p
		}
	}
	return Participant{}
}

// Religação de vídeo tem de chegar ao estado, e o valor absurdo tem de ser
// aparado como os outros.
//
// O número existe por causa de um sintoma que não deixava rastro: a
// transmissão de alguém fica preta só para uma pessoa e volta quando ela
// alterna o modo foco. Sem isto registrado, o relato continua sendo "às vezes
// fica preto".
func TestOnPongGuardaReligacoes(t *testing.T) {
	h := NewHub("x")
	s, _ := h.Join(&fakeSender{}, "x", "leo", "", "", time.Now())
	h.SendPing(1000)
	h.OnPong(s.ID, 1000, 1010, 20, 5, 3, 42)

	p := acha(h.Snapshot().Participants, s.ID)
	if p.ID != s.ID {
		t.Fatal("participante sumiu")
	}
	if p.Religacoes != 3 {
		t.Errorf("religações = %d, queria 3", p.Religacoes)
	}
	if p.QuadrosPerdidos != 42 {
		t.Errorf("quadros perdidos = %d, queria 42", p.QuadrosPerdidos)
	}

	// Cliente defeituoso não pode pintar a lista com número absurdo.
	h.SendPing(2000)
	h.OnPong(s.ID, 2000, 2010, 20, 5, -1, 9_999_999_999)
	p = acha(h.Snapshot().Participants, s.ID)
	if p.Religacoes != 0 || p.QuadrosPerdidos != 0 {
		t.Errorf("valores absurdos passaram: %d / %d", p.Religacoes, p.QuadrosPerdidos)
	}
}

// A versão vem do cliente e vai para a tela de todo mundo, então tem de ser
// tratada como entrada hostil e não como enfeite.
func TestSaneiaVersao(t *testing.T) {
	casos := []struct {
		nome    string
		entrada string
		quer    string
	}{
		{"versão normal", "0.7.5", "0.7.5"},
		{"pré-lançamento", "1.0.0-beta+3", "1.0.0-beta+3"},
		{"espaço em volta", "  0.7.5  ", "0.7.5"},
		{"vazia", "", ""},
		// Quem entra pelo navegador não manda versão; quem manda lixo não
		// derruba a página de ninguém.
		{"marcação", "<b>x</b>", ""},
		{"aspas e script", `"><script>alert(1)</script>`, ""},
		{"barra", "0.7.5/../..", ""},
		{"comprida demais", "0.0.0.0.0.0.0.0.0.0", ""},
	}
	for _, c := range casos {
		t.Run(c.nome, func(t *testing.T) {
			if got := saneiaVersao(c.entrada); got != c.quer {
				t.Fatalf("saneiaVersao(%q) = %q, queria %q", c.entrada, got, c.quer)
			}
		})
	}
}

// Versão inválida não pode impedir alguém de entrar: mostrar a versão é
// conveniência, entrar na sala é o serviço.
func TestVersaoInvalidaNaoImpedeEntrada(t *testing.T) {
	h := NewHub("senha")
	s, err := h.Join(&fakeSender{}, "senha", "gus", "<script>", "", time.Now())
	if err != nil {
		t.Fatalf("Join recusou por causa da versão: %v", err)
	}
	if s.Versao != "" {
		t.Fatalf("versão inválida foi guardada: %q", s.Versao)
	}
}
