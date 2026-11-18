package main

import (
	"context"
	"log"
	"net/http"
	"os"
	"os/signal"
	"strings"
	"syscall"
	"time"
)

// publicBase é a URL pública usada para montar os endpoints WHIP/WHEP
// entregues ao cliente. Vem do ambiente.
var publicBase string

func env(key, def string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return def
}

func main() {
	password := os.Getenv("TELA_PASSWORD")
	if password == "" {
		log.Fatal("TELA_PASSWORD não definida")
	}
	publicBase = strings.TrimSuffix(env("TELA_PUBLIC_BASE", "http://localhost:8090"), "/")
	addr := env("TELA_ADDR", ":8090")
	origins := strings.Split(env("TELA_ORIGINS", "localhost:8090"), ",")

	hub := NewHub(password)

	// Convites são opcionais e falham FECHADO: sem senha de admin o recurso
	// simplesmente não existe. Variável esquecida no ambiente tem de desligar
	// convidados, nunca liberá-los.
	if adminPassword := os.Getenv("TELA_ADMIN_PASSWORD"); adminPassword != "" {
		caminho := env("TELA_CONVITES", "dados/convites.json")
		convites, err := AbreConvites(caminho)
		if err != nil {
			// Não é fatal: convite é conveniência, entrar na sala é o serviço.
			// Derrubar a sala inteira por um JSON corrompido trocaria um
			// problema pequeno por um grande.
			log.Printf("convites: não consegui ler %s (%v) — começando vazio", caminho, err)
		}
		hub.HabilitaConvites(adminPassword, convites)
		log.Printf("convites habilitados (%s)", caminho)
	} else {
		log.Print("convites desabilitados: TELA_ADMIN_PASSWORD não definida")
	}

	done := make(chan struct{})
	hub.StartPinger(3*time.Second, done)

	// 5 falhas em 5min bloqueiam o IP por 15min. Cada tentativa de senha
	// exige uma conexão WS nova (ws.go fecha em `denied`), então contar
	// falhas por IP é o controle certo — o Free tier da Cloudflare já não
	// tem regra de rate limit sobrando para isto.
	throttle := NewThrottle(5, 5*time.Minute, 15*time.Minute)
	go func() {
		tk := time.NewTicker(5 * time.Minute)
		defer tk.Stop()
		for {
			select {
			case <-done:
				return
			case <-tk.C:
				agora := time.Now()
				throttle.Limpa(agora)
				hub.LimpaConvites(agora)
			}
		}
	}()

	mux := http.NewServeMux()
	mux.Handle("/ws", WSHandler(hub, throttle, origins))
	mux.Handle("/internal/auth", AuthHandler(hub))
	mux.HandleFunc("/healthz", func(w http.ResponseWriter, _ *http.Request) {
		w.Write([]byte("ok"))
	})
	mux.Handle("/download/", DownloadHandler(hub))
	mux.Handle("/log", LogHandler(hub))
	// O link de convite serve a MESMA página; o código sai da URL no
	// JavaScript. Uma página só em vez de duas, e o convidado vê exatamente a
	// sala que os outros veem.
	mux.Handle("/c/", http.StripPrefix("/c/", http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		http.ServeFile(w, r, "web/index.html")
	})))
	mux.Handle("/", http.FileServer(http.Dir("web")))

	srv := &http.Server{Addr: addr, Handler: mux}

	go func() {
		log.Printf("sala ouvindo em %s (base pública %s)", addr, publicBase)
		if err := srv.ListenAndServe(); err != nil && err != http.ErrServerClosed {
			log.Fatal(err)
		}
	}()

	sig := make(chan os.Signal, 1)
	signal.Notify(sig, syscall.SIGINT, syscall.SIGTERM)
	<-sig
	close(done)

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	_ = srv.Shutdown(ctx)
	log.Println("sala encerrada")
}
