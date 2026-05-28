package main

// Localhost-only HTTP introspection for a running shows-desktop process.
//
// Why this exists: during playback, libmpv's render surface covers the
// Wails WebView2 chrome (see desktop/README.md "Architecture notes"), so
// the React status panel isn't visible. There's no way to inspect "what
// is the app actually doing right now" by looking at the window.
//
// Surface:
//   - GET /status — current Status snapshot (phase, message, playlist,
//     current round, last advance). Same struct the frontend polls via
//     Wails events.
//   - GET /health — "ok" liveness probe for scripts/automation.
//
// Binds 127.0.0.1:0 (ephemeral port). The chosen port is written to
// %APPDATA%\shows\debug-port on startup so callers discover it without
// parsing logs. No auth — localhost-only, no secrets exposed; the JWT
// and any other credentials are NOT on Status.
//
// Lifetime is tied to the Wails ctx: shutdown() cancels the ctx, which
// triggers srv.Shutdown via the goroutine below.

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strconv"
	"time"
)

// debugPortFilename is the basename written under the per-user config
// directory (UserConfigDir() / "shows"). Same parent as token.json.
const debugPortFilename = "debug-port"

func (a *App) startDebugServer(ctx context.Context) error {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return fmt.Errorf("debug: listen: %w", err)
	}
	port := listener.Addr().(*net.TCPAddr).Port

	mux := http.NewServeMux()
	mux.HandleFunc("/status", func(w http.ResponseWriter, _ *http.Request) {
		snap := a.GetStatus()
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(snap)
	})
	mux.HandleFunc("/health", func(w http.ResponseWriter, _ *http.Request) {
		_, _ = w.Write([]byte("ok"))
	})

	srv := &http.Server{
		Handler:           mux,
		ReadHeaderTimeout: 5 * time.Second,
	}
	go func() {
		if err := srv.Serve(listener); err != nil && !errors.Is(err, http.ErrServerClosed) {
			a.logger.Error("debug: serve exited", "err", err)
		}
	}()
	go func() {
		<-ctx.Done()
		shutdownCtx, cancel := context.WithTimeout(context.Background(), 1*time.Second)
		defer cancel()
		_ = srv.Shutdown(shutdownCtx)
	}()

	if err := writeDebugPort(port); err != nil {
		// Non-fatal: the server still runs on the bound port; the user
		// just has to find it via netstat or the log line below. We
		// don't want a config-dir hiccup to kill the introspection
		// surface that's supposed to help diagnose hiccups.
		a.logger.Warn("debug: failed to write port file", "err", err, "port", port)
	}
	a.logger.Info("debug: server listening", "port", port, "endpoints", "/status,/health")
	return nil
}

func writeDebugPort(port int) error {
	dir, err := os.UserConfigDir()
	if err != nil {
		return err
	}
	cfgDir := filepath.Join(dir, "shows")
	if err := os.MkdirAll(cfgDir, 0o700); err != nil {
		return err
	}
	return os.WriteFile(
		filepath.Join(cfgDir, debugPortFilename),
		[]byte(strconv.Itoa(port)),
		0o600,
	)
}
