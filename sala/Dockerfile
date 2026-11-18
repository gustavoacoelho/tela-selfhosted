FROM golang:1.23-alpine AS build
WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download
COPY . .
RUN CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o /out/sala .

FROM alpine:3.20
RUN adduser -D -u 10001 sala
WORKDIR /app
COPY --from=build /out/sala /app/sala
COPY web /app/web
# Pastas que viram volume: precisam ser do usuário do container.
RUN mkdir -p /app/dados /app/logs /app/download && chown sala:sala /app/dados /app/logs
USER sala
EXPOSE 8090
ENTRYPOINT ["/app/sala"]
