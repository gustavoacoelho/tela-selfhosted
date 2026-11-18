package main

import (
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"sync"
	"time"
)

// DirLog é onde os registros recebidos são gravados.
const DirLog = "logs"

// MaxLogPorEnvio limita cada requisição.
//
// O app manda pedaços pequenos e frequentes; um envio grande é sinal de erro
// nosso ou de alguém abusando, e em nenhum dos casos vale gravar.
const MaxLogPorEnvio = 256 * 1024

// MaxLogPorArquivo é o teto de cada arquivo.
//
// Sem teto, um app em laço de erro encheria o disco da VM — que é a mesma VM
// do Minecraft, do n8n e de tudo mais. Ao estourar, o arquivo recomeça.
const MaxLogPorArquivo = 8 * 1024 * 1024

// naoEhNome tira do apelido tudo que não serve como nome de arquivo.
var naoEhNome = regexp.MustCompile(`[^a-zA-Z0-9._-]+`)

// arquivosDeLog protege escritas simultâneas no mesmo arquivo.
var arquivosDeLog sync.Mutex

// LogHandler recebe linhas de log dos aplicativos.
//
// Existe porque depurar na máquina dos outros por telefone não funciona: quem
// está com o problema é justamente quem não vai achar o arquivo, e reabrir o
// app depois de um travamento apagava a única prova do que houve.
func LogHandler(h *Hub) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPost {
			w.WriteHeader(http.StatusMethodNotAllowed)
			return
		}
		// Mesmo portão do download: estar na sala prova que a pessoa sabe a
		// senha. Sem isso qualquer um encheria o disco de fora.
		nick, ok := h.NickDoToken(r.URL.Query().Get("token"))
		if !ok {
			w.WriteHeader(http.StatusUnauthorized)
			return
		}

		corpo, err := io.ReadAll(io.LimitReader(r.Body, MaxLogPorEnvio+1))
		if err != nil || len(corpo) > MaxLogPorEnvio {
			w.WriteHeader(http.StatusRequestEntityTooLarge)
			return
		}
		if len(corpo) == 0 {
			w.WriteHeader(http.StatusNoContent)
			return
		}

		if err := gravaLog(nick, corpo); err != nil {
			log.Printf("log de %s: %v", nick, err)
			w.WriteHeader(http.StatusInternalServerError)
			return
		}
		w.WriteHeader(http.StatusNoContent)
	})
}

// nomeDeArquivo transforma um apelido em algo seguro para o disco.
//
// O apelido vem de quem digitou e chega a virar caminho: sem limpeza, um
// apelido com `../` escreveria fora da pasta.
func nomeDeArquivo(nick string) string {
	limpo := naoEhNome.ReplaceAllString(nick, "_")
	limpo = strings.Trim(limpo, "._-")
	if limpo == "" {
		limpo = "sem-nome"
	}
	if len(limpo) > 40 {
		limpo = limpo[:40]
	}
	return limpo
}

func gravaLog(nick string, corpo []byte) error {
	arquivosDeLog.Lock()
	defer arquivosDeLog.Unlock()

	if err := os.MkdirAll(DirLog, 0o755); err != nil {
		return err
	}
	caminho := filepath.Join(DirLog, nomeDeArquivo(nick)+".log")

	if info, err := os.Stat(caminho); err == nil && info.Size() > MaxLogPorArquivo {
		// Recomeça em vez de rodar arquivos: o interessante é sempre o fim.
		if err := os.Remove(caminho); err != nil {
			return err
		}
	}

	f, err := os.OpenFile(caminho, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
	if err != nil {
		return err
	}
	defer f.Close()

	cabecalho := fmt.Sprintf("\n===== %s =====\n", time.Now().Format(time.RFC3339))
	if _, err := f.WriteString(cabecalho); err != nil {
		return err
	}
	_, err = f.Write(corpo)
	return err
}
