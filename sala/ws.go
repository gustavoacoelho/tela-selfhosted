package main

import (
	"context"
	"encoding/json"
	"fmt"
	"log"
	"net/http"
	"time"

	"github.com/coder/websocket"
)

// wsSender adapta uma conexão WebSocket à interface Sender do hub.
// A fila com descarte impede que um cliente lento trave o broadcast.
type wsSender struct {
	out chan Envelope
}

func (s *wsSender) Send(env Envelope) {
	select {
	case s.out <- env:
	default:
		// Fila cheia: descarta. O próximo `state` é completo de qualquer
		// forma, então perder um intermediário não corrompe a UI.
	}
}

// PrazoSemSinal é quanto a sala espera por QUALQUER mensagem do cliente antes
// de dar a conexão por morta.
//
// Todo cliente vivo fala de três em três segundos: o servidor manda `ping` e
// tanto a página quanto o app respondem `pong` — conferido nos dois. Trinta
// segundos são dez respostas perdidas; quem chega nisso não está mais lá.
//
// É variável e não constante para o teste poder encurtá-la; em produção nunca
// se mexe nela.
var PrazoSemSinal = 30 * time.Second

func WSHandler(h *Hub, th *Throttle, origins []string) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		ip := clientIP(r)

		conn, err := websocket.Accept(w, r, &websocket.AcceptOptions{
			OriginPatterns: origins,
		})
		if err != nil {
			return
		}
		defer conn.CloseNow()

		ctx, cancel := context.WithCancel(r.Context())
		defer cancel()

		sender := &wsSender{out: make(chan Envelope, 32)}

		// Escritor
		go func() {
			for {
				select {
				case <-ctx.Done():
					return
				case env := <-sender.out:
					b, err := json.Marshal(env)
					if err != nil {
						continue
					}
					wctx, wcancel := context.WithTimeout(ctx, 5*time.Second)
					err = conn.Write(wctx, websocket.MessageText, b)
					wcancel()
					if err != nil {
						cancel()
						return
					}
				}
			}
		}()

		// Primeira mensagem tem de ser hello.
		var session *Session
		defer func() {
			if session != nil {
				h.Leave(session.ID)
			}
		}()

		for {
			// Prazo por leitura. Sem ele, cliente que some SEM fechar o TCP
			// — notebook suspendendo, Wi-Fi virando 4G, aba congelada —
			// deixa esta leitura bloqueada para sempre, o `Leave` do defer
			// nunca roda, e a sessão fica de fantasma ocupando vaga.
			//
			// Aconteceu: a sala ficou 17 dias no ar, acumulou fantasmas de
			// 199 entradas e encheu o teto de 16 participantes. A sala passou
			// a recusar gente de verdade com "sala cheia", e na lista
			// apareciam várias cópias da mesma pessoa.
			rctx, rcancel := context.WithTimeout(ctx, PrazoSemSinal)
			_, data, err := conn.Read(rctx)
			// Lido ANTES do cancel: `rcancel` troca o motivo para "cancelado"
			// e apagaria a informação de que foi o prazo que estourou.
			mudo := rctx.Err() == context.DeadlineExceeded
			rcancel()
			if err != nil {
				if mudo && session != nil {
					log.Printf("sessão encerrada por silêncio: %s (%s sem resposta)",
						session.Nick, PrazoSemSinal)
				}
				return
			}
			var env Envelope
			if err := json.Unmarshal(data, &env); err != nil {
				continue
			}

			if session == nil {
				if env.Type != "hello" {
					return
				}

				if ok, restante := th.Permitido(ip, time.Now()); !ok {
					reason := fmt.Sprintf("tentativas demais, tente novamente em %s", formatDuration(restante))
					if e, err2 := NewEnvelope("denied", DeniedData{Reason: reason, Codigo: "bloqueio"}); err2 == nil {
						b, _ := json.Marshal(e)
						wctx, wcancel := context.WithTimeout(ctx, 2*time.Second)
						_ = conn.Write(wctx, websocket.MessageText, b)
						wcancel()
					}
					return
				}

				var d HelloData
				if err := json.Unmarshal(env.Data, &d); err != nil {
					return
				}
				s, err := h.Join(sender, d.Password, d.Nick, d.Versao, d.Convite, time.Now())
				if err != nil {
					// Convite invalido conta como falha: adivinhar codigo de
					// convite e a mesma superficie de adivinhar senha, e ficava
					// de fora.
					if err == ErrSenhaIncorreta || err == ErrNickInvalido || err == ErrConviteInvalido {
						th.RegistraFalha(ip, d.Nick, time.Now())
					}
					if e, err2 := NewEnvelope("denied", DeniedData{
						Reason: err.Error(),
						Codigo: CodigoDaRecusa(err),
					}); err2 == nil {
						b, _ := json.Marshal(e)
						wctx, wcancel := context.WithTimeout(ctx, 2*time.Second)
						_ = conn.Write(wctx, websocket.MessageText, b)
						wcancel()
					}
					return
				}
				th.RegistraSucesso(ip)
				session = s
				// A versão vai no log de entrada: é o que permite responder "quem
				// ainda está na antiga" sem perguntar a ninguém.
				// O papel vai no log: "quem é de fora e por qual convite"
				// tem de ser respondível depois do evento, não só durante.
				extra := ""
				if s.Versao != "" {
					extra = " v" + s.Versao
				}
				if s.Papel == PapelConvidado {
					extra += " [convidado por " + s.Convite + "]"
				} else if s.Papel == PapelAdmin {
					extra += " [admin]"
				}
				log.Printf("entrou: %s (%s)%s", s.Nick, s.ID, extra)
				continue
			}

			switch env.Type {
			case "pong":
				var d PongData
				if json.Unmarshal(env.Data, &d) == nil {
					h.OnPong(session.ID, d.T, nowMs(), d.RttMs, d.TravaMs, d.Religacoes, d.QuadrosPerdidos)
				}

			case "publish_start":
				var d PublishStartData
				if json.Unmarshal(env.Data, &d) != nil {
					continue
				}
				// Convidado assiste, não transmite. O autorizador do MediaMTX
				// também barra, e é ele que vale de verdade; esta é a primeira
				// porta, para o pedido nem sair daqui.
				if session.Papel == PapelConvidado {
					continue
				}
				st, err := h.PublishStart(session.ID, d)
				if err != nil {
					continue
				}
				// O MediaMTX expõe os endpoints como <path>/whip e <path>/whep —
				// NÃO como /whip/<path>. Com PathPrefix="tela/", a URL final é
				// https://host/tela/<streamId>/whip
				if e, err := NewEnvelope("stream_ready", StreamReadyData{
					StreamID: st.ID,
					WhipURL:  publicBase + "/" + PathPrefix + st.ID + "/whip?token=" + session.Token,
					WhepURL:  publicBase + "/" + PathPrefix + st.ID + "/whep?token=" + session.Token,
				}); err == nil {
					sender.Send(e)
				}

			case "convite_criar":
				var d ConviteCriarData
				if json.Unmarshal(env.Data, &d) != nil {
					continue
				}
				cv, err := h.CriaConvite(session.ID, d.Rotulo, d.Prazo, time.Now())
				if err != nil {
					if e, err2 := NewEnvelope("convite_erro", DeniedData{Reason: err.Error()}); err2 == nil {
						sender.Send(e)
					}
					continue
				}
				log.Printf("convite criado por %s: %s (%s)", session.Nick, cv.Rotulo, d.Prazo)
				enviaConvites(h, session, sender)

			case "convite_revogar":
				var d ConviteRevogarData
				if json.Unmarshal(env.Data, &d) != nil {
					continue
				}
				if err := h.RevogaConvite(session.ID, d.Codigo); err != nil {
					if e, err2 := NewEnvelope("convite_erro", DeniedData{Reason: err.Error()}); err2 == nil {
						sender.Send(e)
					}
					continue
				}
				enviaConvites(h, session, sender)

			case "convites_listar":
				enviaConvites(h, session, sender)

			case "bloqueios_listar":
				enviaBloqueios(h, th, session, sender)

			case "bloqueio_remover":
				var d BloqueioRemoverData
				if json.Unmarshal(env.Data, &d) != nil {
					continue
				}
				// O papel é conferido AQUI e não dentro do Throttle: quem é
				// admin é assunto do hub, quem está barrado é assunto do
				// limitador. Misturar os dois daria a um deles conhecimento
				// que não é dele.
				if !h.EhAdmin(session.ID) {
					continue
				}
				if th.Desbloqueia(d.IP) {
					log.Printf("%s desbloqueou %s", session.Nick, d.IP)
				}
				enviaBloqueios(h, th, session, sender)

			case "publish_stop":
				h.PublishStop(session.ID)

			case "publish_stats":
				var d PublishStatsData
				if json.Unmarshal(env.Data, &d) == nil {
					h.UpdateStats(session.ID, d)
				}

			case "watch_start":
				var d WatchData
				if json.Unmarshal(env.Data, &d) == nil {
					h.WatchStart(session.ID, d.StreamID)
				}

			case "watch_stop":
				var d WatchData
				if json.Unmarshal(env.Data, &d) == nil {
					h.WatchStop(session.ID, d.StreamID)
				}
			}
		}
	})
}

// formatDuration arredonda para cima em minutos (ou segundos, se for menos
// de um minuto) para a mensagem de bloqueio não expor granularidade
// desnecessária.
func formatDuration(d time.Duration) string {
	if d <= 0 {
		return "instantes"
	}
	if d < time.Minute {
		s := int(d.Round(time.Second) / time.Second)
		if s <= 0 {
			s = 1
		}
		if s == 1 {
			return "1 segundo"
		}
		return fmt.Sprintf("%d segundos", s)
	}
	m := int((d + time.Minute - 1) / time.Minute)
	if m == 1 {
		return "1 minuto"
	}
	return fmt.Sprintf("%d minutos", m)
}

// enviaConvites manda a lista de convites ativos para o admin.
//
// Erro aqui é silencioso de propósito: quem não é admin simplesmente não
// recebe lista. Responder "você não é admin" a quem nem pediu direito só
// contaria a existência do recurso para quem não deveria vê-lo.
func enviaConvites(h *Hub, session *Session, sender Sender) {
	lista, err := h.ListaConvites(session.ID, time.Now())
	if err != nil {
		return
	}
	if e, err := NewEnvelope("convites", ConvitesData{Convites: lista}); err == nil {
		sender.Send(e)
	}
}

// enviaBloqueios manda a lista de IPs barrados para o administrador.
//
// Silencioso para quem não é admin, pelo mesmo motivo de `enviaConvites`:
// responder "você não é admin" contaria a existência do recurso a quem não
// deveria vê-lo.
func enviaBloqueios(h *Hub, th *Throttle, session *Session, sender Sender) {
	if !h.EhAdmin(session.ID) {
		return
	}
	if e, err := NewEnvelope("bloqueios", BloqueiosData{
		Bloqueios: th.Bloqueados(time.Now()),
	}); err == nil {
		sender.Send(e)
	}
}
