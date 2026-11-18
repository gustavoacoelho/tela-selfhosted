package main

import "encoding/json"

// Envelope é o formato de toda mensagem trocada no WebSocket.
type Envelope struct {
	Type string          `json:"type"`
	Data json.RawMessage `json:"data,omitempty"`
}

func NewEnvelope(typ string, data any) (Envelope, error) {
	b, err := json.Marshal(data)
	if err != nil {
		return Envelope{}, err
	}
	return Envelope{Type: typ, Data: b}, nil
}

// --- cliente -> servidor ---

type HelloData struct {
	Password string `json:"password"`
	Nick     string `json:"nick"`
	// Versão do app de quem entrou. Vazia para quem entra pelo navegador, que
	// não tem versão própria — a página é sempre a que o servidor serve.
	Versao string `json:"versao"`
	// Código de convite, quando a pessoa chega por um link de convidado.
	//
	// `hello` leva `password` OU `convite`, nunca os dois: são credenciais
	// alternativas para a mesma porta, e aceitar as duas juntas só criaria a
	// pergunta de qual vence.
	Convite string `json:"convite"`
}

type PongData struct {
	T int64 `json:"t"`
	// Ida e volta medida pelo navegador na camada ICE, em ms. Zero quando o
	// cliente não tem como medir (nenhuma transmissão aberta) ou é uma versão
	// antiga da página — campos ausentes viram zero e nada quebra.
	RttMs int64 `json:"rttMs"`
	// Atraso da thread principal do navegador, em ms.
	TravaMs int64 `json:"travaMs"`
	// Quantas vezes a página teve de religar um vídeo porque o decodificador
	// empacou — chegava dado e não virava quadro. Acumulado desde que a página
	// abriu.
	//
	// Existe porque o sintoma "a transmissão de fulano fica preta só para mim,
	// e volta quando alterno o foco" não deixava rastro nenhum: `TravaMs` mede
	// a thread principal, não decodificação. Sem este número, o relato continua
	// sendo "às vezes fica preto".
	Religacoes int64 `json:"religacoes"`
	// Quadros que o navegador decodificou e descartou. Sobe quando a máquina de
	// quem assiste não dá conta — é o outro lado da mesma pergunta.
	QuadrosPerdidos int64 `json:"quadrosPerdidos"`
}

type PublishStartData struct {
	SourceType  string         `json:"sourceType"` // "monitor" | "window"
	SourceTitle string         `json:"sourceTitle"`
	Settings    StreamSettings `json:"settings"`
}

type PublishStatsData struct {
	Kbps    int `json:"kbps"`
	FPS     int `json:"fps"`
	Dropped int `json:"dropped"`
	// Resolução do que está sendo enviado AGORA.
	//
	// Vem junto das estatísticas e não do `publish_start` porque ela MUDA: a
	// escada de qualidade troca de resolução no meio da transmissão quando a
	// conexão aperta. O valor do `publish_start` é de um instante só e
	// passaria o resto da sessão mentindo.
	//
	// Zero significa "cliente antigo não informou", e a sala cai no que veio
	// no `publish_start`.
	W int `json:"w"`
	H int `json:"h"`
}

type WatchData struct {
	StreamID string `json:"streamId"`
}

// --- servidor -> cliente ---

type WelcomeData struct {
	SelfID       string        `json:"selfId"`
	Token        string        `json:"token"`
	Participants []Participant `json:"participants"`
	Streams      []Stream      `json:"streams"`
}

type DeniedData struct {
	Reason string `json:"reason"`
	// Codigo estavel da recusa, para o cliente decidir SEM ler a frase.
	//
	// Existe porque a alternativa era casar por texto em portugues, que quebra
	// silenciosamente na primeira vez que alguem reescrever uma mensagem — e a
	// decisao que depende disto e "vale a pena tentar de novo?". Errar essa
	// resposta foi o que fez a troca de senha bloquear quem era de casa.
	//
	// Valores: "senha", "convite", "apelido", "cheia", "bloqueio", "revogado".
	// Vazio significa que a recusa e antiga e nao trouxe codigo.
	Codigo string `json:"codigo"`
}

// CodigoDaRecusa traduz um erro de entrada no codigo que vai para o cliente.
func CodigoDaRecusa(err error) string {
	switch err {
	case ErrSenhaIncorreta:
		return "senha"
	case ErrConviteInvalido:
		return "convite"
	case ErrNickInvalido:
		return "apelido"
	case ErrSalaCheia:
		return "cheia"
	}
	return ""
}

type StateData struct {
	Participants []Participant `json:"participants"`
	Streams      []Stream      `json:"streams"`
}

type StreamReadyData struct {
	StreamID string `json:"streamId"`
	WhipURL  string `json:"whipUrl"`
	WhepURL  string `json:"whepUrl"`
}

type PingData struct {
	T int64 `json:"t"`
}

// --- modelos ---

type Participant struct {
	ID   string `json:"id"`
	Nick string `json:"nick"`
	// Versão do app, vazia para quem está pelo navegador. Ver `HelloData`.
	Versao string `json:"versao"`
	// "casa", "admin" ou "convidado". A interface etiqueta o convidado na
	// lista: saber quem é de fora sem ter de lembrar quem chamou.
	Papel   string `json:"papel"`
	PingMs  int64  `json:"pingMs"`
	RttMs   int64  `json:"rttMs"`
	TravaMs int64  `json:"travaMs"`
	// Ver `PongData.Religacoes`.
	Religacoes      int64    `json:"religacoes"`
	QuadrosPerdidos int64    `json:"quadrosPerdidos"`
	Sharing         string   `json:"sharing"` // streamId, ou "" se não transmite
	Watching        []string `json:"watching"`
}

type StreamSettings struct {
	W    int `json:"w"`
	H    int `json:"h"`
	FPS  int `json:"fps"`
	Kbps int `json:"kbps"`
}

type StreamStats struct {
	Kbps    int `json:"kbps"`
	FPS     int `json:"fps"`
	Dropped int `json:"dropped"`
	W       int `json:"w"`
	H       int `json:"h"`
}

type Stream struct {
	ID          string         `json:"id"`
	OwnerID     string         `json:"ownerId"`
	SourceType  string         `json:"sourceType"`
	SourceTitle string         `json:"sourceTitle"`
	Settings    StreamSettings `json:"settings"`
	Stats       StreamStats    `json:"stats"`
}

// ConviteInfo é um convite como o painel do admin o vê.
//
// Leva o link pronto em vez do código: quem está na tela quer copiar e colar no
// WhatsApp, não montar URL. `ExpiraEm` vai em segundos restantes e não como
// data, para o navegador não precisar acertar fuso nenhum.
type ConviteInfo struct {
	Codigo   string `json:"codigo"`
	Rotulo   string `json:"rotulo"`
	Link     string `json:"link"`
	ExpiraEm int64  `json:"expiraEm"`
	EmUso    int    `json:"emUso"`
}

// ConviteCriarData é o pedido de criação vindo do admin.
type ConviteCriarData struct {
	Rotulo string `json:"rotulo"`
	Prazo  string `json:"prazo"`
}

// ConviteRevogarData é o pedido de revogação.
type ConviteRevogarData struct {
	Codigo string `json:"codigo"`
}

// ConvitesData é a lista devolvida ao painel.
type ConvitesData struct {
	Convites []ConviteInfo `json:"convites"`
}

// BloqueioRemoverData é o pedido de desbloqueio vindo do administrador.
type BloqueioRemoverData struct {
	IP string `json:"ip"`
}

// BloqueiosData é a lista de IPs barrados devolvida ao painel.
type BloqueiosData struct {
	Bloqueios []Bloqueio `json:"bloqueios"`
}
