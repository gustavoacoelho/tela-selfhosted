package main

import (
	"crypto/rand"
	"encoding/json"
	"errors"
	"io"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"time"
)

var (
	ErrConviteInvalido = errors.New("convite inválido ou vencido")
	ErrSemAdmin        = errors.New("convites não estão habilitados")
	ErrNaoEhAdmin      = errors.New("só o administrador pode convidar")
	ErrPrazoInvalido   = errors.New("prazo de convite inválido")
	ErrRotuloInvalido  = errors.New("rótulo inválido")
)

// AlfabetoCodigo é o mesmo usado para a senha da sala: minúsculas e dígitos,
// sem os pares que se confundem ao ditar ou transcrever (l/1, o/0).
//
// O convite viaja por link, mas o código acaba lido em voz alta de vez em
// quando ("é o link que termina em..."), e o custo de evitar isso é zero.
const AlfabetoCodigo = "abcdefghijkmnpqrstuvwxyz23456789"

// TamanhoCodigo dá ~100 bits de entropia. Adivinhar está fora de questão, o
// que é o que permite tratar o convite como o único segredo da entrada.
const TamanhoCodigo = 20

// MaxRotulo limita o texto livre que o admin escreve para lembrar quem é quem.
const MaxRotulo = 40

// PrazosValidos são os prazos que a interface oferece.
//
// Lista fechada em vez de número livre: o campo vem do cliente, e "quantas
// horas você quer" com resposta livre é um convite a 87600. A interface só
// mostra estes três, e o servidor só aceita estes três.
var PrazosValidos = map[string]time.Duration{
	"1h":  time.Hour,
	"6h":  6 * time.Hour,
	"24h": 24 * time.Hour,
}

// Convite é um passe temporário de convidado.
type Convite struct {
	Codigo    string    `json:"codigo"`
	Rotulo    string    `json:"rotulo"`
	Expira    time.Time `json:"expira"`
	CriadoEm  time.Time `json:"criadoEm"`
	CriadoPor string    `json:"criadoPor"`
	Revogado  bool      `json:"revogado"`
}

// Valido diz se o convite ainda serve para entrar.
func (c *Convite) Valido(agora time.Time) bool {
	return !c.Revogado && agora.Before(c.Expira)
}

// Convites guarda os convites e os mantém em disco.
//
// # Por que em disco
//
// O resto do Hub é todo em memória, e isso é deliberado: sessão e transmissão
// não fazem sentido depois de um restart. Convite faz. A sala reinicia a cada
// implantação — quatro vezes numa única noite —, e um convite que morre nisso
// derruba o convidado no meio do evento, sem que ele entenda por quê.
type Convites struct {
	mu       sync.Mutex
	caminho  string
	convites map[string]*Convite
}

// AbreConvites carrega os convites de `caminho`, criando o diretório se
// preciso. Arquivo ausente é o caso normal da primeira execução.
//
// Arquivo ilegível NÃO impede o serviço de subir: convite é conveniência, e
// derrubar a sala inteira por causa de um JSON corrompido trocaria um problema
// pequeno por um grande. O erro é devolvido para quem chama registrar.
func AbreConvites(caminho string) (*Convites, error) {
	c := &Convites{caminho: caminho, convites: map[string]*Convite{}}
	if err := os.MkdirAll(filepath.Dir(caminho), 0o755); err != nil {
		return c, err
	}
	dados, err := os.ReadFile(caminho)
	if errors.Is(err, os.ErrNotExist) {
		return c, nil
	}
	if err != nil {
		return c, err
	}
	var lista []*Convite
	if err := json.Unmarshal(dados, &lista); err != nil {
		return c, err
	}
	for _, cv := range lista {
		if cv.Codigo != "" {
			c.convites[cv.Codigo] = cv
		}
	}
	return c, nil
}

// Cria gera um convite novo e o grava.
func (c *Convites) Cria(rotulo, prazo, criadoPor string, agora time.Time) (*Convite, error) {
	dur, ok := PrazosValidos[prazo]
	if !ok {
		return nil, ErrPrazoInvalido
	}
	rotulo = limpaRotulo(rotulo)
	if rotulo == "" {
		return nil, ErrRotuloInvalido
	}
	codigo, err := codigoAleatorio()
	if err != nil {
		return nil, err
	}
	cv := &Convite{
		Codigo:    codigo,
		Rotulo:    rotulo,
		Expira:    agora.Add(dur),
		CriadoEm:  agora,
		CriadoPor: criadoPor,
	}

	c.mu.Lock()
	c.convites[codigo] = cv
	err = c.salvaLocked()
	c.mu.Unlock()
	return cv, err
}

// Valida devolve o convite se ele servir para entrar agora.
func (c *Convites) Valida(codigo string, agora time.Time) (*Convite, bool) {
	c.mu.Lock()
	defer c.mu.Unlock()
	cv, ok := c.convites[codigo]
	if !ok || !cv.Valido(agora) {
		return nil, false
	}
	return cv, true
}

// Revoga invalida um convite. Devolve false se ele não existir.
func (c *Convites) Revoga(codigo string) (bool, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	cv, ok := c.convites[codigo]
	if !ok {
		return false, nil
	}
	cv.Revogado = true
	return true, c.salvaLocked()
}

// Ativos lista os convites que ainda servem, do mais novo para o mais velho.
func (c *Convites) Ativos(agora time.Time) []Convite {
	c.mu.Lock()
	defer c.mu.Unlock()
	var v []Convite
	for _, cv := range c.convites {
		if cv.Valido(agora) {
			v = append(v, *cv)
		}
	}
	sortConvites(v)
	return v
}

// Limpa descarta o que já não serve, para o arquivo não crescer para sempre.
//
// Só grava se algo saiu: a faxina roda de minuto em minuto e reescrever um
// arquivo idêntico a cada volta é desperdício.
func (c *Convites) Limpa(agora time.Time) {
	c.mu.Lock()
	defer c.mu.Unlock()
	mudou := false
	for codigo, cv := range c.convites {
		if !cv.Valido(agora) {
			delete(c.convites, codigo)
			mudou = true
		}
	}
	if mudou {
		_ = c.salvaLocked()
	}
}

// salvaLocked grava o arquivo. Exige c.mu já travado.
//
// Escreve num temporário e renomeia: `rename` no mesmo diretório é atômico, e
// sem isso um restart no meio da escrita deixaria um JSON pela metade — que na
// próxima subida viraria "nenhum convite existe" bem no meio de um evento.
func (c *Convites) salvaLocked() error {
	lista := make([]*Convite, 0, len(c.convites))
	for _, cv := range c.convites {
		lista = append(lista, cv)
	}
	dados, err := json.MarshalIndent(lista, "", "  ")
	if err != nil {
		return err
	}
	tmp, err := os.CreateTemp(filepath.Dir(c.caminho), ".convites-*.tmp")
	if err != nil {
		return err
	}
	nome := tmp.Name()
	if _, err := tmp.Write(dados); err != nil {
		tmp.Close()
		os.Remove(nome)
		return err
	}
	if err := tmp.Close(); err != nil {
		os.Remove(nome)
		return err
	}
	return os.Rename(nome, c.caminho)
}

func codigoAleatorio() (string, error) {
	b := make([]byte, TamanhoCodigo)
	if _, err := io.ReadFull(rand.Reader, b); err != nil {
		return "", err
	}
	// Módulo enviesaria: 256 não é múltiplo de 32. Aqui é, então não enviesa —
	// e a asserção existe para o dia em que alguém mexer no alfabeto.
	if 256%len(AlfabetoCodigo) != 0 {
		panic("AlfabetoCodigo precisa dividir 256 para o sorteio não enviesar")
	}
	for i, v := range b {
		b[i] = AlfabetoCodigo[int(v)%len(AlfabetoCodigo)]
	}
	return string(b), nil
}

// limpaRotulo corta espaço e tamanho. O rótulo é texto livre do admin e vai
// para a tela dele, então basta não deixar crescer sem limite.
func limpaRotulo(r string) string {
	r = strings.TrimSpace(r)
	if len(r) > MaxRotulo {
		r = strings.TrimSpace(r[:MaxRotulo])
	}
	return r
}

// sortConvites deixa o mais recente primeiro, que é a ordem em que o admin
// procura o que acabou de gerar.
func sortConvites(v []Convite) {
	sort.Slice(v, func(i, j int) bool { return v[i].CriadoEm.After(v[j].CriadoEm) })
}
