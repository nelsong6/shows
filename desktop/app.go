package main

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"os"
	"sync"
	"time"

	wruntime "github.com/wailsapp/wails/v2/pkg/runtime"

	"github.com/nelsong6/shows/desktop/internal/apiclient"
	"github.com/nelsong6/shows/desktop/internal/oauth"
	"github.com/nelsong6/shows/desktop/internal/player"
	"github.com/nelsong6/shows/desktop/internal/playlist"
	"github.com/nelsong6/shows/desktop/internal/win32"
)

const (
	// windowTitle must match the Title field in main.go's wails.Run
	// options.App. win32.FindWindowByTitle uses it to locate the host
	// HWND so libmpv can parent its render surface into it.
	windowTitle = "shows"

	// defaultPlaylist is the only playlist we drive in v1. Multi-
	// playlist support arrives in Phase 4 alongside the keybind
	// vocabulary.
	defaultPlaylist = "nelson"
)

// App is the Wails-bound application object. Its public methods are
// auto-exposed to the TypeScript frontend at frontend/wailsjs/go/main/App.
type App struct {
	ctx    context.Context
	logger *slog.Logger

	mu     sync.Mutex
	player *player.Player
	cancel context.CancelFunc

	// status mirrors the runner state so the frontend can render
	// something useful before/during/after a round.
	statusMu sync.RWMutex
	status   Status
}

// Status is the snapshot the frontend polls (or subscribes to via
// Wails events) to render itself.
type Status struct {
	Phase        string                 `json:"phase"` // initializing|auth|fetching|playing|drained|error
	Message      string                 `json:"message"`
	Round        []apiclient.RoundEntry `json:"round,omitempty"`
	LastAdvance  *apiclient.AdvanceResult `json:"last_advance,omitempty"`
}

func NewApp() *App {
	return &App{
		logger: slog.New(slog.NewJSONHandler(os.Stderr, nil)),
		status: Status{Phase: "initializing", Message: "starting up"},
	}
}

// startup is called by Wails when the window is created. We grab the
// host HWND, hand it to libmpv (so mpv embeds its render surface as a
// child of the Wails window), then spawn the runner goroutine that
// authenticates and drives the round-robin loop forever.
func (a *App) startup(ctx context.Context) {
	a.ctx = ctx

	hwnd, err := win32.WaitForWindow(windowTitle, 2*time.Second)
	if err != nil {
		a.logger.Warn("could not locate host window — mpv will open its own", "err", err)
		hwnd = 0
	}

	p, err := player.New(hwnd)
	if err != nil {
		a.setStatus("error", fmt.Sprintf("player init: %v", err))
		a.logger.Error("player.New", "err", err)
		return
	}
	a.mu.Lock()
	a.player = p
	a.mu.Unlock()

	runnerCtx, cancel := context.WithCancel(ctx)
	a.mu.Lock()
	a.cancel = cancel
	a.mu.Unlock()

	go a.runForever(runnerCtx)
}

// shutdown is called by Wails when the user closes the window. Tear
// down the runner goroutine and the libmpv handle.
func (a *App) shutdown(ctx context.Context) {
	a.mu.Lock()
	if a.cancel != nil {
		a.cancel()
		a.cancel = nil
	}
	p := a.player
	a.player = nil
	a.mu.Unlock()
	if p != nil {
		_ = p.Close()
	}
}

// runForever does auth + runs the playlist loop. Survives transient
// errors via the runner's own backoff; fatal errors (auth refused,
// mpv shutdown) terminate the goroutine and set the status to error.
func (a *App) runForever(ctx context.Context) {
	a.setStatus("auth", "obtaining auth.romaine.life token")
	host, _ := os.Hostname()
	tok, err := oauth.EnsureToken(ctx, oauth.Config{
		Info: oauth.RequesterInfo{
			WhereHappening: fmt.Sprintf("shows-desktop on %s", host),
			IntendedUse:    "play episodes via shows.romaine.life",
			MiscIdentifier: "couch",
		},
		Opener: func(url string) error {
			wruntime.BrowserOpenURL(a.ctx, url)
			return nil
		},
	})
	if err != nil {
		a.setStatus("error", fmt.Sprintf("auth: %v", err))
		a.logger.Error("oauth.EnsureToken", "err", err)
		return
	}

	client := apiclient.New("", tok.Token)
	r := &playlist.Runner{
		Client:   client,
		Player:   a.player,
		Playlist: defaultPlaylist,
		Logger:   a.logger,
		OnRound: func(round []apiclient.RoundEntry) {
			a.statusMu.Lock()
			a.status = Status{Phase: "playing", Message: fmt.Sprintf("round of %d", len(round)), Round: round}
			a.statusMu.Unlock()
			wruntime.EventsEmit(a.ctx, "round", round)
		},
		OnAdvance: func(res *apiclient.AdvanceResult) {
			a.statusMu.Lock()
			a.status.LastAdvance = res
			a.statusMu.Unlock()
			wruntime.EventsEmit(a.ctx, "advance", res)
		},
		OnDrained: func() {
			a.setStatus("drained", "every show in this playlist is finished")
			wruntime.EventsEmit(a.ctx, "drained", nil)
		},
	}

	a.setStatus("fetching", "asking shows.romaine.life for the next round")
	if err := r.Run(ctx); err != nil && !errors.Is(err, context.Canceled) {
		a.setStatus("error", err.Error())
		a.logger.Error("playlist runner exited", "err", err)
	}
}

// GetStatus is bound for the frontend; returns the most recent
// snapshot. Phase 3 replaces polling with Wails events.
func (a *App) GetStatus() Status {
	a.statusMu.RLock()
	defer a.statusMu.RUnlock()
	return a.status
}

func (a *App) setStatus(phase, message string) {
	a.statusMu.Lock()
	a.status.Phase = phase
	a.status.Message = message
	a.statusMu.Unlock()
	wruntime.EventsEmit(a.ctx, "status", a.status)
}
