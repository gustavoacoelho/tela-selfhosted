package main

import (
	"encoding/json"
	"testing"
)

func TestDecodeHello(t *testing.T) {
	raw := []byte(`{"type":"hello","data":{"password":"segredo","nick":"gus"}}`)

	var env Envelope
	if err := json.Unmarshal(raw, &env); err != nil {
		t.Fatalf("unmarshal envelope: %v", err)
	}
	if env.Type != "hello" {
		t.Fatalf("tipo = %q, esperado hello", env.Type)
	}

	var h HelloData
	if err := json.Unmarshal(env.Data, &h); err != nil {
		t.Fatalf("unmarshal hello: %v", err)
	}
	if h.Password != "segredo" || h.Nick != "gus" {
		t.Fatalf("hello = %+v", h)
	}
}

func TestEncodeState(t *testing.T) {
	env, err := NewEnvelope("state", StateData{
		Participants: []Participant{{
			ID: "a", Nick: "gus", Versao: "0.7.5", Papel: "casa",
			// `pingMs` é ida e volta pelo WebSocket, com o atraso do
			// JavaScript de quem responde embutido; `rttMs` é a latência de
			// rede que o navegador mede na camada ICE; `travaMs` é o quanto a
			// thread principal dele estava atrasada. São três números
			// diferentes de propósito — o primeiro é aproximadamente a soma
			// dos outros dois, e confundi-los foi o que fez alguém no mesmo
			// estado parecer ter conexão pior que alguém em outro país.
			PingMs: 210, RttMs: 22, TravaMs: 180,
			Watching: []string{},
		}},
		Streams: []Stream{},
	})
	if err != nil {
		t.Fatalf("NewEnvelope: %v", err)
	}
	b, _ := json.Marshal(env)
	got := string(b)
	want := `{"type":"state","data":{"participants":[{"id":"a","nick":"gus","versao":"0.7.5","papel":"casa","pingMs":210,"rttMs":22,"travaMs":180,"religacoes":0,"quadrosPerdidos":0,"sharing":"","watching":[]}],"streams":[]}}`
	if got != want {
		t.Fatalf("\n got: %s\nwant: %s", got, want)
	}
}
