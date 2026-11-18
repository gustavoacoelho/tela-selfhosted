package main

import (
	"encoding/json"
	"net/http"
	"net/url"
	"strings"
)

// PathPrefix é o namespace único da sala dentro do MediaMTX.
const PathPrefix = "tela/"

// authRequest é o corpo que o MediaMTX envia ao autorizador externo.
// Só os campos que usamos estão declarados.
type authRequest struct {
	Action   string `json:"action"` // publish | read | playback | api | metrics | pprof
	Path     string `json:"path"`
	Query    string `json:"query"`
	Protocol string `json:"protocol"`
	IP       string `json:"ip"`
}

// AuthHandler responde ao webhook de autorização do MediaMTX.
// 200 libera, 401 nega. Qualquer dúvida resulta em negar.
func AuthHandler(h *Hub) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var req authRequest
		if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
			w.WriteHeader(http.StatusUnauthorized)
			return
		}

		// Ações administrativas nunca passam por aqui.
		if req.Action != "publish" && req.Action != "read" {
			w.WriteHeader(http.StatusUnauthorized)
			return
		}
		if !strings.HasPrefix(req.Path, PathPrefix) {
			w.WriteHeader(http.StatusUnauthorized)
			return
		}
		streamID := strings.TrimPrefix(req.Path, PathPrefix)
		if streamID == "" || strings.Contains(streamID, "/") {
			w.WriteHeader(http.StatusUnauthorized)
			return
		}

		token := tokenFromQuery(req.Query)
		if token == "" {
			w.WriteHeader(http.StatusUnauthorized)
			return
		}

		ok := false
		switch req.Action {
		case "publish":
			// Convidado nunca publica. Esta é a barreira que vale: o MediaMTX
			// só aceita a transmissão se ESTE handler disser sim, então nem um
			// cliente adulterado contorna.
			if convidado, _ := h.EhConvidado(token); convidado {
				w.WriteHeader(http.StatusUnauthorized)
				return
			}
			// Só publica no path que o hub reservou para esta sessão.
			ok = h.StreamOwnedBy(token, streamID)
		case "read":
			// Qualquer pessoa autenticada na sala pode assistir qualquer tela.
			ok = h.TokenValido(token)
		}

		if !ok {
			w.WriteHeader(http.StatusUnauthorized)
			return
		}
		w.WriteHeader(http.StatusOK)
	})
}

func tokenFromQuery(q string) string {
	v, err := url.ParseQuery(q)
	if err != nil {
		return ""
	}
	return v.Get("token")
}
