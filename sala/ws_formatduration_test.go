package main

import (
	"testing"
	"time"
)

func TestFormatDuration(t *testing.T) {
	cases := []struct {
		in   time.Duration
		want string
	}{
		{0, "instantes"},
		{-time.Second, "instantes"},
		{500 * time.Millisecond, "1 segundo"},
		{30 * time.Second, "30 segundos"},
		{59 * time.Second, "59 segundos"},
		{time.Minute, "1 minuto"},
		{90 * time.Second, "2 minutos"},
		{15 * time.Minute, "15 minutos"},
		{14*time.Minute + time.Second, "15 minutos"},
	}
	for _, c := range cases {
		if got := formatDuration(c.in); got != c.want {
			t.Errorf("formatDuration(%v) = %q, esperado %q", c.in, got, c.want)
		}
	}
}
