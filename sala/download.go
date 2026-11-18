package main

import (
	"net/http"
	"os"
	"path/filepath"
	"strings"
)

// DirDownload é onde os binários do app de transmissão ficam. Fica FORA de
// `web/`, senão o FileServer da raiz os serviria sem passar por aqui.
const DirDownload = "download"

// Único arquivo servido sem token — ver `DownloadHandler`.
const ManifestoPublico = "versao.json"

// DownloadHandler entrega o app de transmissão apenas para quem já está na
// sala.
//
// O botão na página só aparece depois do login, mas isso é cosmético: sem esta
// checagem qualquer pessoa com a URL baixaria o binário. O token vem na query
// porque um `<a download>` não carrega cabeçalho.
//
// Não é segredo de estado — o app é inútil sem a senha da sala — mas manter o
// download atrás da mesma porta evita que a URL circule solta por aí.
func DownloadHandler(h *Hub) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		nome := strings.TrimPrefix(r.URL.Path, "/download/")

		// O manifesto de versão é público: o app precisa saber se há atualização
		// ANTES de entrar na sala, e ele não contém nada além de número de
		// versão, tamanho e hash. Os binários seguem exigindo token.
		if filepath.Base(nome) != ManifestoPublico {
			token := r.URL.Query().Get("token")
			if !h.TokenValido(token) {
				w.WriteHeader(http.StatusUnauthorized)
				return
			}
			// Convidado assiste pelo navegador e não leva o app.
			//
			// A página esconde o botão dele, mas esconder não é impedir: sem
			// esta linha bastaria a URL do download, que circula entre quem já
			// está na sala.
			if convidado, _ := h.EhConvidado(token); convidado {
				w.WriteHeader(http.StatusUnauthorized)
				return
			}
		}
		// `filepath.Base` corta qualquer travessia de diretório: uma requisição
		// para `../../etc/passwd` vira `passwd` e não acha nada.
		nome = filepath.Base(nome)
		if nome == "." || nome == string(filepath.Separator) {
			w.WriteHeader(http.StatusNotFound)
			return
		}

		caminho := filepath.Join(DirDownload, nome)
		f, err := os.Open(caminho)
		if err != nil {
			// Enquanto o app não foi publicado, este é o caminho normal. A
			// página faz um HEAD aqui e se rotula "em breve" sozinha.
			w.WriteHeader(http.StatusNotFound)
			return
		}
		defer f.Close()

		info, err := f.Stat()
		if err != nil || info.IsDir() {
			w.WriteHeader(http.StatusNotFound)
			return
		}

		w.Header().Set("Content-Type", "application/octet-stream")
		w.Header().Set("Content-Disposition", `attachment; filename="`+nome+`"`)
		// Sem cache: o binário é substituído a cada versão nova e ninguém deve
		// receber uma antiga por causa de proxy no caminho.
		w.Header().Set("Cache-Control", "no-store")
		http.ServeContent(w, r, nome, info.ModTime(), f)
	})
}
