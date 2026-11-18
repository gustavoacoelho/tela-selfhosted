package main

import (
	"net/http"
	"testing"
)

func TestClientIPUsaCFConnectingIP(t *testing.T) {
	r, _ := http.NewRequest("GET", "/", nil)
	r.Header.Set("CF-Connecting-IP", "9.9.9.9")
	r.Header.Set("X-Forwarded-For", "8.8.8.8, 1.1.1.1")
	r.RemoteAddr = "127.0.0.1:5555"

	if got := clientIP(r); got != "9.9.9.9" {
		t.Fatalf("clientIP = %q, esperado 9.9.9.9", got)
	}
}

func TestClientIPUsaXForwardedForQuandoSemCF(t *testing.T) {
	r, _ := http.NewRequest("GET", "/", nil)
	r.Header.Set("X-Forwarded-For", "8.8.8.8, 1.1.1.1")
	r.RemoteAddr = "127.0.0.1:5555"

	if got := clientIP(r); got != "8.8.8.8" {
		t.Fatalf("clientIP = %q, esperado 8.8.8.8", got)
	}
}

func TestClientIPXForwardedForComEspacos(t *testing.T) {
	r, _ := http.NewRequest("GET", "/", nil)
	r.Header.Set("X-Forwarded-For", "  8.8.8.8  ,1.1.1.1")
	r.RemoteAddr = "127.0.0.1:5555"

	if got := clientIP(r); got != "8.8.8.8" {
		t.Fatalf("clientIP = %q, esperado 8.8.8.8", got)
	}
}

func TestClientIPFallbackRemoteAddrComPorta(t *testing.T) {
	r, _ := http.NewRequest("GET", "/", nil)
	r.RemoteAddr = "1.2.3.4:56789"

	if got := clientIP(r); got != "1.2.3.4" {
		t.Fatalf("clientIP = %q, esperado 1.2.3.4", got)
	}
}

func TestClientIPFallbackRemoteAddrIPv6ComPorta(t *testing.T) {
	r, _ := http.NewRequest("GET", "/", nil)
	r.RemoteAddr = "[::1]:56789"

	if got := clientIP(r); got != "::1" {
		t.Fatalf("clientIP = %q, esperado ::1", got)
	}
}

func TestClientIPFallbackRemoteAddrSemPorta(t *testing.T) {
	r, _ := http.NewRequest("GET", "/", nil)
	r.RemoteAddr = "1.2.3.4"

	if got := clientIP(r); got != "1.2.3.4" {
		t.Fatalf("clientIP = %q, esperado 1.2.3.4", got)
	}
}
