package main

import (
	"log"
	"sort"
	"sync"
	"time"
)

// Throttle limita tentativas de login por IP. Como cada tentativa de senha
// no protocolo WS abre uma conexão nova (ws.go fecha a conexão em `denied`),
// contar falhas por IP é o único jeito de coibir força bruta na camada de
// aplicação — o Free tier da Cloudflare já não tem regra de rate limit
// sobrando para isso.
type Throttle struct {
	mu        sync.Mutex
	maxFalhas int
	janela    time.Duration
	bloqueio  time.Duration
	entradas  map[string]*entradaIP
}

type entradaIP struct {
	falhas       []time.Time // timestamps dentro da janela deslizante
	bloqueadoAte time.Time   // zero se não bloqueado
	// Apelido da última tentativa vinda deste IP.
	//
	// Sem isto a lista de bloqueios é uma coluna de números que não diz nada:
	// quem administra não tem como saber que `203.0.113.7` é o fulano. O
	// valor vem do cliente, então é tratado como texto hostil na exibição.
	ultimoNick string
}

// NewThrottle cria um limitador. maxFalhas falhas dentro de janela bloqueiam
// o IP por bloqueio.
func NewThrottle(maxFalhas int, janela, bloqueio time.Duration) *Throttle {
	return &Throttle{
		maxFalhas: maxFalhas,
		janela:    janela,
		bloqueio:  bloqueio,
		entradas:  map[string]*entradaIP{},
	}
}

// Permitido diz se o IP pode tentar autenticar agora. Se bloqueado, devolve
// também quanto tempo falta para liberar.
func (t *Throttle) Permitido(ip string, agora time.Time) (bool, time.Duration) {
	t.mu.Lock()
	defer t.mu.Unlock()

	e, ok := t.entradas[ip]
	if !ok {
		return true, 0
	}
	if !e.bloqueadoAte.IsZero() && agora.Before(e.bloqueadoAte) {
		return false, e.bloqueadoAte.Sub(agora)
	}
	return true, 0
}

// RegistraFalha contabiliza uma tentativa malsucedida vinda de `ip`. Se isso
// fizer o total dentro da janela deslizante atingir maxFalhas, bloqueia o IP.
func (t *Throttle) RegistraFalha(ip, nick string, agora time.Time) {
	t.mu.Lock()
	defer t.mu.Unlock()

	e, ok := t.entradas[ip]
	if !ok {
		e = &entradaIP{}
		t.entradas[ip] = e
	}
	if nick != "" {
		e.ultimoNick = nick
	}

	corte := agora.Add(-t.janela)
	vivas := e.falhas[:0]
	for _, f := range e.falhas {
		if f.After(corte) {
			vivas = append(vivas, f)
		}
	}
	vivas = append(vivas, agora)
	e.falhas = vivas

	if len(e.falhas) >= t.maxFalhas {
		// Só registra a TRANSIÇÃO para bloqueado. Quem já estava bloqueado e
		// continua tentando não precisa de uma linha por tentativa.
		novo := e.bloqueadoAte.IsZero() || agora.After(e.bloqueadoAte)
		e.bloqueadoAte = agora.Add(t.bloqueio)
		if novo {
			// Sem esta linha, um bloqueio é INVISÍVEL. Depois de trocar a senha
			// da sala, a pergunta "quem ficou bloqueado?" não tinha resposta
			// possível: o limitador barrava gente de casa e não deixava rastro,
			// e a própria investigação levou três tentativas para descobrir que
			// o bloqueado era quem investigava.
			// O apelido vai junto. O limitador sempre soube quem tentava, mas
			// só o mostrava no painel — e a entrada do painel some quando o
			// bloqueio expira. Com o log dizendo só o IP, "quem ficou
			// bloqueado ontem?" não tinha resposta, e um IP de datacenter
			// (VPN de um amigo) quase foi lido como gente de fora.
			quem := e.ultimoNick
			if quem == "" {
				quem = "?"
			}
			log.Printf("bloqueio: %s (%q) até %s (%d falhas em %s)",
				ip, quem, e.bloqueadoAte.Format("15:04:05"), len(e.falhas), t.janela)
		}
	}
}

// RegistraSucesso zera o histórico de falhas do IP — um login válido não
// deixa dívida para trás.
func (t *Throttle) RegistraSucesso(ip string) {
	t.mu.Lock()
	defer t.mu.Unlock()
	delete(t.entradas, ip)
}

// Limpa remove entradas cujo bloqueio já expirou (ou nunca existiu) e cuja
// última falha já saiu da janela — chamada periodicamente para o mapa não
// crescer sem limite quando um atacante varia o IP de origem.
func (t *Throttle) Limpa(agora time.Time) {
	t.mu.Lock()
	defer t.mu.Unlock()

	corte := agora.Add(-t.janela)
	for ip, e := range t.entradas {
		if !e.bloqueadoAte.IsZero() && agora.Before(e.bloqueadoAte) {
			continue // bloqueio ainda vigente, preserva
		}
		ultimaViva := false
		for _, f := range e.falhas {
			if f.After(corte) {
				ultimaViva = true
				break
			}
		}
		if !ultimaViva {
			delete(t.entradas, ip)
		}
	}
}

// temEntrada é auxiliar de teste (mesmo pacote) para inspecionar se o IP
// ainda tem estado guardado no mapa após uma Limpa.
func (t *Throttle) temEntrada(ip string) bool {
	t.mu.Lock()
	defer t.mu.Unlock()
	_, ok := t.entradas[ip]
	return ok
}

// Bloqueio é um IP barrado, como o painel do administrador o vê.
type Bloqueio struct {
	IP string `json:"ip"`
	// Apelido da última tentativa. Vazio quando ninguém informou um.
	Nick string `json:"nick"`
	// Segundos que faltam. Em segundos e não como data para o navegador não
	// precisar acertar fuso nenhum.
	FaltaSeg int64 `json:"faltaSeg"`
	Falhas   int   `json:"falhas"`
}

// Bloqueados lista quem está barrado agora, do que falta mais para o que falta
// menos.
func (t *Throttle) Bloqueados(agora time.Time) []Bloqueio {
	t.mu.Lock()
	defer t.mu.Unlock()

	var v []Bloqueio
	for ip, e := range t.entradas {
		if e.bloqueadoAte.IsZero() || !agora.Before(e.bloqueadoAte) {
			continue
		}
		v = append(v, Bloqueio{
			IP:       ip,
			Nick:     e.ultimoNick,
			FaltaSeg: int64(e.bloqueadoAte.Sub(agora).Seconds()),
			Falhas:   len(e.falhas),
		})
	}
	sort.Slice(v, func(i, j int) bool { return v[i].FaltaSeg > v[j].FaltaSeg })
	return v
}

// Desbloqueia libera um IP na hora. Devolve false se ele não estava barrado.
//
// # Por que existe
//
// Porque a alternativa era reiniciar o serviço inteiro para destravar UMA
// pessoa — o que derruba as transmissões de todo mundo. Aconteceu: um amigo
// ficou barrado e a única ferramenta disponível era o reinício.
//
// Apaga a entrada inteira, e não só a data do bloqueio: deixar as falhas
// acumuladas faria a próxima tentativa errada bloquear de novo na hora, e
// "desbloqueei e travou outra vez" não é desbloquear.
func (t *Throttle) Desbloqueia(ip string) bool {
	t.mu.Lock()
	defer t.mu.Unlock()
	e, ok := t.entradas[ip]
	if !ok || e.bloqueadoAte.IsZero() {
		return false
	}
	delete(t.entradas, ip)
	log.Printf("bloqueio removido: %s", ip)
	return true
}
