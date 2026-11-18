package main

import (
	"errors"
	"log"
	"sort"
	"strings"
	"sync"
	"time"
)

var (
	ErrSenhaIncorreta     = errors.New("senha incorreta")
	ErrNickInvalido       = errors.New("apelido inválido")
	ErrSalaCheia          = errors.New("sala cheia")
	ErrSessaoDesconhecida = errors.New("sessão desconhecida")
)

// MaxParticipantes é folgado sobre o uso real (3-8 pessoas) e existe só
// para impedir que um cliente em loop encha a memória do serviço.
const MaxParticipantes = 16

type Hub struct {
	mu       sync.Mutex
	password string
	// Senha do administrador. VAZIA desliga os convites por completo.
	//
	// Falha fechada de propósito: uma variável esquecida no ambiente tem de
	// desligar o recurso, nunca liberá-lo. Sem admin ninguém convida, e sem
	// convite ninguém entra como convidado.
	adminPassword string
	convites      *Convites
	sessions      map[string]*Session
	streams       map[string]*Stream
}

func NewHub(password string) *Hub {
	return &Hub{
		password: password,
		sessions: map[string]*Session{},
		streams:  map[string]*Stream{},
	}
}

// HabilitaConvites liga o recurso de convidados.
//
// Separado do construtor porque a maior parte dos testes não quer nada disto,
// e porque em produção depende de duas coisas que podem faltar
// independentemente: a senha de admin e o arquivo de convites.
func (h *Hub) HabilitaConvites(adminPassword string, c *Convites) {
	h.mu.Lock()
	defer h.mu.Unlock()
	h.adminPassword = adminPassword
	h.convites = c
}

// ConvitesLigados diz se dá para convidar alguém.
func (h *Hub) ConvitesLigados() bool {
	h.mu.Lock()
	defer h.mu.Unlock()
	return h.adminPassword != "" && h.convites != nil
}

// saneiaVersao devolve uma versão exibível, ou vazio.
//
// O valor vem do cliente e vai parar na tela de todo mundo, então não pode ser
// texto livre: aceita só o que uma versão é (dígitos, letras, ponto, hífen e
// mais) e corta no tamanho de qualquer versão plausível. Entrada fora disso
// vira vazio em vez de erro — não conseguir mostrar a versão de alguém não é
// motivo para recusar a entrada dele na sala.
func saneiaVersao(v string) string {
	v = strings.TrimSpace(v)
	if len(v) > 16 {
		return ""
	}
	for _, r := range v {
		ok := (r >= '0' && r <= '9') || (r >= 'a' && r <= 'z') ||
			(r >= 'A' && r <= 'Z') || r == '.' || r == '-' || r == '+'
		if !ok {
			return ""
		}
	}
	return v
}

// autentica decide o papel a partir da credencial apresentada.
//
// `hello` traz senha OU convite. A ordem aqui importa: a senha de admin é
// testada antes da senha de casa, senão configurar as duas iguais faria o admin
// entrar como gente comum sem nenhum aviso.
func (h *Hub) autentica(password, convite string, agora time.Time) (Papel, string, error) {
	h.mu.Lock()
	adminPassword, casa, convites := h.adminPassword, h.password, h.convites
	h.mu.Unlock()

	if convite != "" {
		if convites == nil || adminPassword == "" {
			// Convites desligados: um link antigo não pode virar entrada.
			return "", "", ErrConviteInvalido
		}
		cv, ok := convites.Valida(convite, agora)
		if !ok {
			return "", "", ErrConviteInvalido
		}
		return PapelConvidado, cv.Codigo, nil
	}

	if adminPassword != "" && password == adminPassword {
		return PapelAdmin, "", nil
	}
	if password == casa {
		return PapelCasa, "", nil
	}
	return "", "", ErrSenhaIncorreta
}

func (h *Hub) Join(sender Sender, password, nick, versao, convite string, agora time.Time) (*Session, error) {
	nick = strings.TrimSpace(nick)
	if nick == "" || len(nick) > 24 {
		return nil, ErrNickInvalido
	}
	versao = saneiaVersao(versao)

	papel, codigo, err := h.autentica(password, convite, agora)
	if err != nil {
		return nil, err
	}

	h.mu.Lock()
	if len(h.sessions) >= MaxParticipantes {
		h.mu.Unlock()
		return nil, ErrSalaCheia
	}
	s := newSession(sender, nick, versao, papel, codigo)
	h.sessions[s.ID] = s
	state := h.snapshotLocked()
	h.mu.Unlock()

	if env, err := NewEnvelope("welcome", WelcomeData{
		SelfID:       s.ID,
		Token:        s.Token,
		Participants: state.Participants,
		Streams:      state.Streams,
	}); err == nil {
		sender.Send(env)
	}

	h.Broadcast()
	return s, nil
}

func (h *Hub) Leave(sessionID string) {
	h.mu.Lock()
	s, ok := h.sessions[sessionID]
	if ok {
		if s.sharing != "" {
			delete(h.streams, s.sharing)
		}
		delete(h.sessions, sessionID)
	}
	h.mu.Unlock()

	if ok {
		h.Broadcast()
	}
}

// Snapshot devolve o estado atual, ordenado de forma estável para que a UI
// não fique reordenando a lista a cada broadcast.
func (h *Hub) Snapshot() StateData {
	h.mu.Lock()
	defer h.mu.Unlock()
	return h.snapshotLocked()
}

func (h *Hub) snapshotLocked() StateData {
	ps := make([]Participant, 0, len(h.sessions))
	for _, s := range h.sessions {
		ps = append(ps, s.snapshot())
	}
	sort.Slice(ps, func(i, j int) bool { return ps[i].ID < ps[j].ID })

	ss := make([]Stream, 0, len(h.streams))
	for _, st := range h.streams {
		ss = append(ss, *st)
	}
	sort.Slice(ss, func(i, j int) bool { return ss[i].ID < ss[j].ID })

	return StateData{Participants: ps, Streams: ss}
}

// Broadcast envia o estado atual para todo mundo na sala.
func (h *Hub) Broadcast() {
	state := h.Snapshot()
	env, err := NewEnvelope("state", state)
	if err != nil {
		return
	}

	h.mu.Lock()
	senders := make([]Sender, 0, len(h.sessions))
	for _, s := range h.sessions {
		senders = append(senders, s.sender)
	}
	h.mu.Unlock()

	// Fora do lock: um Send lento não pode travar a sala inteira.
	for _, s := range senders {
		s.Send(env)
	}
}

func nowMs() int64 { return time.Now().UnixMilli() }

// SendPing dispara um ping carimbado para todos. O parâmetro `t` é injetável
// para o teste não depender do relógio real.
func (h *Hub) SendPing(t int64) {
	env, err := NewEnvelope("ping", PingData{T: t})
	if err != nil {
		return
	}

	h.mu.Lock()
	type target struct {
		s      *Session
		sender Sender
	}
	targets := make([]target, 0, len(h.sessions))
	for _, s := range h.sessions {
		s.lastPing.Store(t)
		targets = append(targets, target{s, s.sender})
	}
	h.mu.Unlock()

	for _, tg := range targets {
		tg.sender.Send(env)
	}
}

// OnPong registra o RTT. `echoed` é o carimbo devolvido pelo cliente e
// `arrived` é quando a resposta chegou. Pongs que não correspondem ao ping
// em voo são descartados — senão um cliente atrasado reporta RTT absurdo.
//
// `rtt` e `trava` vêm medidos pelo cliente e são guardados como chegaram:
// nada aqui pode verificá-los, então o teto existe só para um cliente
// defeituoso não pintar a lista com número absurdo.
func (h *Hub) OnPong(sessionID string, echoed, arrived, rtt, trava, religacoes, perdidos int64) {
	h.mu.Lock()
	s, ok := h.sessions[sessionID]
	h.mu.Unlock()
	if !ok {
		return
	}
	if s.lastPing.Load() != echoed {
		return
	}
	ida := arrived - echoed
	if ida < 0 {
		ida = 0
	}
	s.pingMs.Store(ida)
	s.rttMs.Store(saneia(rtt))
	s.travaMs.Store(saneia(trava))

	// Religação de vídeo é rara e significa alguma coisa: o decodificador de
	// quem assiste empacou e a página teve de reatar a fonte. Só o AUMENTO vai
	// para o log — o valor é acumulado, e repeti-lo a cada ping encheria o
	// registro sem dizer nada de novo.
	if antes := s.religacoes.Swap(saneia(religacoes)); saneia(religacoes) > antes {
		log.Printf("%s: %d religação(ões) de vídeo (total %d)",
			s.Nick, saneia(religacoes)-antes, saneia(religacoes))
	}
	s.quadrosPerdidos.Store(saneia(perdidos))
}

// Teto para número vindo do cliente. Uma hora é absurdo para qualquer uma das
// duas medidas, e negativo não existe.
func saneia(v int64) int64 {
	if v < 0 || v > 3_600_000 {
		return 0
	}
	return v
}

// StartPinger dispara pings periódicos até `done` fechar.
func (h *Hub) StartPinger(every time.Duration, done <-chan struct{}) {
	go func() {
		tk := time.NewTicker(every)
		defer tk.Stop()
		for {
			select {
			case <-done:
				return
			case <-tk.C:
				h.SendPing(nowMs())
				h.Broadcast()
			}
		}
	}()
}

func (h *Hub) PublishStart(sessionID string, d PublishStartData) (Stream, error) {
	h.mu.Lock()
	s, ok := h.sessions[sessionID]
	if !ok {
		h.mu.Unlock()
		return Stream{}, ErrSessaoDesconhecida
	}
	// Republicar substitui: uma pessoa transmite no máximo uma tela.
	if s.sharing != "" {
		delete(h.streams, s.sharing)
	}
	st := &Stream{
		ID:          randomID(8),
		OwnerID:     s.ID,
		SourceType:  d.SourceType,
		SourceTitle: d.SourceTitle,
		Settings:    d.Settings,
	}
	h.streams[st.ID] = st
	s.sharing = st.ID
	out := *st
	h.mu.Unlock()

	h.Broadcast()
	return out, nil
}

func (h *Hub) PublishStop(sessionID string) {
	h.mu.Lock()
	s, ok := h.sessions[sessionID]
	if ok && s.sharing != "" {
		delete(h.streams, s.sharing)
		s.sharing = ""
	}
	h.mu.Unlock()

	if ok {
		h.Broadcast()
	}
}

// UpdateStats grava a telemetria de envio vinda do -progress do ffmpeg.
// Não faz broadcast: chega a cada segundo e o pinger já republica o estado.
func (h *Hub) UpdateStats(sessionID string, d PublishStatsData) {
	h.mu.Lock()
	defer h.mu.Unlock()
	s, ok := h.sessions[sessionID]
	if !ok || s.sharing == "" {
		return
	}
	st, ok := h.streams[s.sharing]
	if !ok {
		return
	}
	// Resolução zero é cliente antigo que não a informa: fica o que veio no
	// publish_start, que é o melhor palpite disponível.
	w, hh := d.W, d.H
	if w <= 0 || hh <= 0 {
		w, hh = st.Settings.W, st.Settings.H
	}
	st.Stats = StreamStats{Kbps: d.Kbps, FPS: d.FPS, Dropped: d.Dropped, W: w, H: hh}
}

func (h *Hub) WatchStart(sessionID, streamID string) {
	h.mu.Lock()
	s, ok := h.sessions[sessionID]
	if ok {
		if _, exists := h.streams[streamID]; exists {
			s.watching[streamID] = true
		}
	}
	h.mu.Unlock()
	if ok {
		h.Broadcast()
	}
}

func (h *Hub) WatchStop(sessionID, streamID string) {
	h.mu.Lock()
	s, ok := h.sessions[sessionID]
	if ok {
		delete(s.watching, streamID)
	}
	h.mu.Unlock()
	if ok {
		h.Broadcast()
	}
}

// StreamOwnedBy diz se `streamID` pertence à sessão do `token`. Usado pelo
// autorizador do MediaMTX para impedir que alguém publique no path de outro.
func (h *Hub) StreamOwnedBy(token, streamID string) bool {
	h.mu.Lock()
	defer h.mu.Unlock()
	for _, s := range h.sessions {
		if s.Token == token {
			return s.sharing == streamID
		}
	}
	return false
}

// TokenValido diz se existe sessão ativa com este token.
func (h *Hub) TokenValido(token string) bool {
	_, ok := h.NickDoToken(token)
	return ok
}

// NickDoToken devolve o apelido da sessão dona do token.
//
// O apelido é o que dá nome ao arquivo de log de cada pessoa: sem ele, os
// registros de todo mundo chegariam misturados e sem dono.
func (h *Hub) NickDoToken(token string) (string, bool) {
	h.mu.Lock()
	defer h.mu.Unlock()
	for _, s := range h.sessions {
		if s.Token == token {
			return s.Nick, true
		}
	}
	return "", false
}

// CriaConvite gera um convite. Só admin.
func (h *Hub) CriaConvite(sessionID, rotulo, prazo string, agora time.Time) (*Convite, error) {
	h.mu.Lock()
	s, ok := h.sessions[sessionID]
	convites, adminPassword := h.convites, h.adminPassword
	h.mu.Unlock()

	if !ok {
		return nil, ErrSessaoDesconhecida
	}
	if convites == nil || adminPassword == "" {
		return nil, ErrSemAdmin
	}
	if s.Papel != PapelAdmin {
		return nil, ErrNaoEhAdmin
	}
	return convites.Cria(rotulo, prazo, s.Nick, agora)
}

// RevogaConvite invalida um convite e DERRUBA quem entrou por ele.
//
// Derrubar é o ponto. Sem isso, revogar durante um evento não faz nada com
// quem já está dentro — e é exatamente durante o evento que se revoga.
func (h *Hub) RevogaConvite(sessionID, codigo string) error {
	h.mu.Lock()
	s, ok := h.sessions[sessionID]
	convites, adminPassword := h.convites, h.adminPassword
	h.mu.Unlock()

	if !ok {
		return ErrSessaoDesconhecida
	}
	if convites == nil || adminPassword == "" {
		return ErrSemAdmin
	}
	if s.Papel != PapelAdmin {
		return ErrNaoEhAdmin
	}
	if _, err := convites.Revoga(codigo); err != nil {
		return err
	}

	// Quem entrou por este convite sai. Coleta sob o mutex e avisa fora dele:
	// `Send` fala com a rede e segurar o mutex do hub durante isso travaria a
	// sala inteira para todo mundo.
	h.mu.Lock()
	var derrubados []*Session
	for id, sess := range h.sessions {
		if sess.Papel == PapelConvidado && sess.Convite == codigo {
			derrubados = append(derrubados, sess)
			if sess.sharing != "" {
				delete(h.streams, sess.sharing)
			}
			delete(h.sessions, id)
		}
	}
	h.mu.Unlock()

	for _, sess := range derrubados {
		if env, err := NewEnvelope("denied", DeniedData{
			Reason: "o convite foi revogado",
			Codigo: "revogado",
		}); err == nil {
			sess.sender.Send(env)
		}
	}
	if len(derrubados) > 0 {
		log.Printf("convite %s revogado: %d convidado(s) derrubado(s)", codigo, len(derrubados))
		h.Broadcast()
	}
	return nil
}

// ListaConvites devolve os convites ativos e quantas pessoas estão usando cada
// um. Só admin.
func (h *Hub) ListaConvites(sessionID string, agora time.Time) ([]ConviteInfo, error) {
	h.mu.Lock()
	s, ok := h.sessions[sessionID]
	convites, adminPassword := h.convites, h.adminPassword
	h.mu.Unlock()

	if !ok {
		return nil, ErrSessaoDesconhecida
	}
	if convites == nil || adminPassword == "" {
		return nil, ErrSemAdmin
	}
	if s.Papel != PapelAdmin {
		return nil, ErrNaoEhAdmin
	}

	h.mu.Lock()
	emUso := map[string]int{}
	for _, sess := range h.sessions {
		if sess.Papel == PapelConvidado {
			emUso[sess.Convite]++
		}
	}
	h.mu.Unlock()

	ativos := convites.Ativos(agora)
	out := make([]ConviteInfo, 0, len(ativos))
	for _, cv := range ativos {
		out = append(out, ConviteInfo{
			Codigo:   cv.Codigo,
			Rotulo:   cv.Rotulo,
			Link:     publicBase + "/c/" + cv.Codigo,
			ExpiraEm: int64(cv.Expira.Sub(agora).Seconds()),
			EmUso:    emUso[cv.Codigo],
		})
	}
	return out, nil
}

// EhConvidado diz se o token pertence a uma sessão de convidado.
//
// Um só lugar respondendo isso, usado pelo download e pelo autorizador do
// MediaMTX: duas cópias da mesma regra divergem, e a que divergir vira o
// buraco. Token desconhecido devolve `false, false` — quem chama trata.
func (h *Hub) EhConvidado(token string) (convidado bool, existe bool) {
	h.mu.Lock()
	defer h.mu.Unlock()
	for _, s := range h.sessions {
		if s.Token == token {
			return s.Papel == PapelConvidado, true
		}
	}
	return false, false
}

// LimpaConvites descarta convites que já não servem. Sem efeito se os convites
// estiverem desligados.
func (h *Hub) LimpaConvites(agora time.Time) {
	h.mu.Lock()
	c := h.convites
	h.mu.Unlock()
	if c != nil {
		c.Limpa(agora)
	}
}

// EhAdmin diz se a sessão tem papel de administrador.
//
// Existe para quem está fora do hub (o limitador de tentativas, por exemplo)
// poder conferir permissão sem precisar conhecer sessão nem papel.
func (h *Hub) EhAdmin(sessionID string) bool {
	h.mu.Lock()
	defer h.mu.Unlock()
	s, ok := h.sessions[sessionID]
	return ok && s.Papel == PapelAdmin
}
