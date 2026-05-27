// shows-api is the HTTP server deployed to AKS at shows.romaine.life.
// It owns the playlist state and serves the local mpv-driving client.
package main

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"net/http"
	"os"
	"os/signal"
	"syscall"
	"time"

	"github.com/nelsong6/shows/internal/api"
	"github.com/nelsong6/shows/internal/auth"
	"github.com/nelsong6/shows/internal/store"
)

func main() {
	logger := slog.New(slog.NewJSONHandler(os.Stdout, nil))
	slog.SetDefault(logger)

	if err := run(); err != nil {
		slog.Error("shows-api exited", "err", err)
		os.Exit(1)
	}
}

func run() error {
	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGTERM, syscall.SIGINT)
	defer stop()

	cosmosEndpoint := os.Getenv("COSMOS_ENDPOINT")
	if cosmosEndpoint == "" {
		return errors.New("COSMOS_ENDPOINT is required")
	}
	cosmosDatabase := os.Getenv("COSMOS_DATABASE")
	if cosmosDatabase == "" {
		return errors.New("COSMOS_DATABASE is required")
	}
	port := os.Getenv("PORT")
	if port == "" {
		port = "8080"
	}

	// Cosmos client. Containers are pre-provisioned by tofu — no
	// startup schema management needed (Cosmos has no schema). Auth
	// resolves via DefaultAzureCredential against the projected
	// workload-identity token.
	st, err := store.New(ctx, cosmosEndpoint, cosmosDatabase)
	if err != nil {
		return fmt.Errorf("store: %w", err)
	}
	slog.Info("cosmos store ready", "endpoint", cosmosEndpoint, "database", cosmosDatabase)

	verifier, err := auth.FromEnv(ctx)
	if err != nil {
		return fmt.Errorf("auth verifier: %w", err)
	}
	slog.Info("auth verifier ready")

	srv := &api.Server{Store: st, Verifier: verifier}
	httpSrv := &http.Server{
		Addr:              ":" + port,
		Handler:           srv.Router(),
		ReadHeaderTimeout: 10 * time.Second,
	}

	errCh := make(chan error, 1)
	go func() {
		slog.Info("listening", "addr", httpSrv.Addr)
		if err := httpSrv.ListenAndServe(); err != nil && !errors.Is(err, http.ErrServerClosed) {
			errCh <- err
		}
		close(errCh)
	}()

	select {
	case err := <-errCh:
		return err
	case <-ctx.Done():
		slog.Info("shutdown signal received")
	}

	shutdownCtx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()
	if err := httpSrv.Shutdown(shutdownCtx); err != nil {
		return fmt.Errorf("shutdown: %w", err)
	}
	slog.Info("shutdown complete")
	return nil
}
