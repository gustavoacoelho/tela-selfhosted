package main

import (
	"crypto/rand"
	"encoding/hex"
	"sync/atomic"
)

// Sender é como o hub fala com um cliente. O hub não sabe o que é WebSocket.
type Sender interface {
	Send(env Envelope)
}

// Papel diz com que credencial a pessoa entrou, e por consequência o que ela
// pode fazer. Não é enfeite de interface: `PapelConvidado` é verificado no
// download, no autorizador do MediaMTX e ao começar a transmitir.
type Papel string

const (
	PapelCasa      Papel = "casa"
	PapelAdmin     Papel = "admin"
	PapelConvidado Papel = "convidado"
)

type Session struct {
	ID     string
	Nick   string
	Token  string
	Versao string
	Papel  Papel
	// Código do convite por onde entrou, vazio para quem não é convidado.
	// É o que permite revogar um convite e derrubar exatamente quem entrou
	// por ele, sem tocar em mais ninguém.
	Convite string
	sender  Sender

	pingMs   atomic.Int64
	lastPing atomic.Int64 // timestamp do ping em voo, em ms

	// Latência de rede de verdade, medida pelo próprio navegador do cliente
	// na camada ICE, e o quanto a thread principal dele estava atrasada.
	//
	// Existem porque `pingMs` mistura as duas coisas: ele é ida e volta pelo
	// WebSocket, mas quem responde é o JavaScript, então um navegador ocupado
	// decodificando três vídeos aparece como se tivesse conexão ruim. Foi o
	// que aconteceu na prática — alguém no mesmo estado marcando 247ms
	// enquanto outro em outro país marcava 185.
	//
	// Zero significa "o cliente não informou": versão antiga da página, ou
	// ninguém transmitindo (sem transmissão não há conexão WebRTC de onde
	// tirar a medida).
	rttMs   atomic.Int64
	travaMs atomic.Int64
	// Ver `PongData.Religacoes`. Acumulados desde que a página abriu, então o
	// que interessa é a diferença entre dois instantes — e é assim que o log
	// os trata.
	religacoes      atomic.Int64
	quadrosPerdidos atomic.Int64

	sharing  string
	watching map[string]bool
}

func randomID(nBytes int) string {
	b := make([]byte, nBytes)
	if _, err := rand.Read(b); err != nil {
		panic(err) // crypto/rand falhando é inrecuperável
	}
	return hex.EncodeToString(b)
}

func newSession(sender Sender, nick, versao string, papel Papel, convite string) *Session {
	return &Session{
		ID:       randomID(8),
		Nick:     nick,
		Token:    randomID(16),
		Versao:   versao,
		Papel:    papel,
		Convite:  convite,
		sender:   sender,
		watching: map[string]bool{},
	}
}

func (s *Session) snapshot() Participant {
	w := make([]string, 0, len(s.watching))
	for id := range s.watching {
		w = append(w, id)
	}
	return Participant{
		ID:              s.ID,
		Nick:            s.Nick,
		Versao:          s.Versao,
		Papel:           string(s.Papel),
		PingMs:          s.pingMs.Load(),
		RttMs:           s.rttMs.Load(),
		Religacoes:      s.religacoes.Load(),
		QuadrosPerdidos: s.quadrosPerdidos.Load(),
		TravaMs:         s.travaMs.Load(),
		Sharing:         s.sharing,
		Watching:        w,
	}
}
