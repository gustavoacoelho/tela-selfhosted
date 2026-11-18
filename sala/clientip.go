package main

import (
	"net"
	"net/http"
	"strings"
)

// clientIP resolve o IP do cliente por trás de Cloudflare + Caddy.
// Ordem de precedência:
//  1. CF-Connecting-IP — posto pela Cloudflare.
//  2. Primeiro IP de X-Forwarded-For — posto pelo Caddy.
//  3. r.RemoteAddr sem a porta, como último recurso.
//
// ATENÇÃO AO PUBLICAR ATRÁS DA CLOUDFLARE: o CF-Connecting-IP só é confiável
// se a origem aceitar conexão apenas da Cloudflare. Se o servidor também
// responde por conexão direta, um cliente pode pular o proxy e forjar o
// header, ganhando um balde de rate limit novo a cada tentativa.
//
// O conserto tem de ser no Caddy, não aqui: r.RemoteAddr sempre é o IP do
// container do Caddy, então este código não consegue distinguir uma requisição
// vinda da Cloudflare de uma direta. Caminho: configurar `trusted_proxies` com
// as faixas da Cloudflare, remover o CF-Connecting-IP recebido do cliente e
// repassar {client_ip} num header próprio — ou fechar a 443 da origem para
// tudo que não seja faixa da Cloudflare.
//
// Enquanto isso não é feito, o Throttle protege contra força bruta ingênua
// (script batendo no domínio público), não contra quem conhece a origem.
func clientIP(r *http.Request) string {
	if cf := strings.TrimSpace(r.Header.Get("CF-Connecting-IP")); cf != "" {
		return cf
	}

	if xff := r.Header.Get("X-Forwarded-For"); xff != "" {
		first := strings.SplitN(xff, ",", 2)[0]
		if ip := strings.TrimSpace(first); ip != "" {
			return ip
		}
	}

	if host, _, err := net.SplitHostPort(r.RemoteAddr); err == nil {
		return host
	}
	return r.RemoteAddr
}
