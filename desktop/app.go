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
	client *apiclient.Client
	cancel context.CancelFunc

	statusMu sync.RWMutex
	status   Status
}

// Status is the snapshot the frontend polls (or subscribes to via
// Wails events) to render itself.
type Status struct {
	Phase       string                   `json:"phase"` // initializing|auth|fetching|playing|drained|error
	Message     string                   `json:"message"`
	Playlist    string                   `json:"playlist"`
	Round       []apiclient.RoundEntry   `json:"round,omitempty"`
	LastAdvance *apiclient.AdvanceResult `json:"last_advance,omitempty"`
}

func NewApp() *App {
	return &App{
		logger: slog.New(slog.NewJSONHandler(os.Stderr, nil)),
		status: Status{Phase: "initializing", Message: "starting up", Playlist: defaultPlaylist},
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
	// Token refresh on 401: re-runs the device flow. If the cached
	// token is still valid, EnsureToken short-circuits without
	// prompting the user; if not, the browser opens for re-approval.
	client.RefreshToken = func() (string, error) {
		fresh, err := oauth.EnsureToken(ctx, oauth.Config{
			Info: oauth.RequesterInfo{
				WhereHappening: fmt.Sprintf("shows-desktop on %s (refresh)", host),
				IntendedUse:    "play episodes via shows.romaine.life",
				MiscIdentifier: "couch",
			},
			Opener: func(url string) error {
				wruntime.BrowserOpenURL(a.ctx, url)
				return nil
			},
		})
		if err != nil {
			return "", err
		}
		return fresh.Token, nil
	}
	a.mu.Lock()
	a.client = client
	a.mu.Unlock()

	r := &playlist.Runner{
		Client:   client,
		Player:   a.player,
		Playlist: defaultPlaylist,
		Logger:   a.logger,
		OnRound: func(round []apiclient.RoundEntry) {
			a.statusMu.Lock()
			a.status.Phase = "playing"
			a.status.Message = fmt.Sprintf("round of %d", len(round))
			a.status.Round = round
			snapshot := a.status
			a.statusMu.Unlock()
			wruntime.EventsEmit(a.ctx, "status", snapshot)
		},
		OnAdvance: func(res *apiclient.AdvanceResult) {
			a.statusMu.Lock()
			a.status.LastAdvance = res
			snapshot := a.status
			a.statusMu.Unlock()
			wruntime.EventsEmit(a.ctx, "status", snapshot)
		},
		OnDrained: func() {
			a.setStatus("drained", "every show in this playlist is finished")
		},
	}

	a.setStatus("fetching", "asking shows.romaine.life for the next round")
	if err := r.Run(ctx); err != nil && !errors.Is(err, context.Canceled) {
		a.setStatus("error", err.Error())
		a.logger.Error("playlist runner exited", "err", err)
	}
}

// ─── frontend bindings ──────────────────────────────────────────────

// GetStatus returns the current state snapshot. Wails generates the
// matching TS type at frontend/wailsjs/go/main/App.d.ts.
func (a *App) GetStatus() Status {
	a.statusMu.RLock()
	defer a.statusMu.RUnlock()
	return a.status
}

// ListShows returns the active shows in the configured playlist.
// Errors propagate as JS Promise rejections; the frontend renders the
// empty-state when len(shows) == 0.
func (a *App) ListShows() ([]apiclient.Show, error) {
	a.mu.Lock()
	c := a.client
	a.mu.Unlock()
	if c == nil {
		return nil, errors.New("not authenticated yet")
	}
	return c.ListActiveShows(a.ctx, defaultPlaylist)
}

func (a *App) setStatus(phase, message string) {
	a.statusMu.Lock()
	a.status.Phase = phase
	a.status.Message = message
	snapshot := a.status
	a.statusMu.Unlock()
	wruntime.EventsEmit(a.ctx, "status", snapshot)
}
