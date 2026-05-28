package main

import (
	"context"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"os"
	"path/filepath"
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

	// hostHWND is the Wails top-level window handle, captured at startup.
	// libmpv embeds its render surface as a child of it; we re-raise that
	// child above the WebView2 sibling at the start of each round (see
	// OnRound) so the video is actually visible and not buried behind the
	// React chrome. Zero when the host window couldn't be located.
	hostHWND uintptr

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

// initLogFile opens %APPDATA%\shows\shows.log for append and returns a
// logger that writes there + best-effort to stderr. Also sets it as the
// package-default for callers that use slog directly. File handle lives
// for the process lifetime — no rotation, but at one-JSON-line-per-event
// this is a few MB per week of continuous playback.
//
// We use a tolerant multi-writer (not io.MultiWriter) because Wails ships
// the GUI subsystem on Windows, which leaves os.Stderr attached to a dead
// handle. io.MultiWriter short-circuits on the first writer's error, so
// writing to (stderr, file) would error on stderr and never reach the
// file. tolerantMultiWriter writes to every target and discards their
// individual errors — the file is the source of truth; stderr is just
// "nice to have" for wails-dev console launches.
func initLogFile() (*slog.Logger, string, error) {
	dir, err := os.UserConfigDir()
	if err != nil {
		return nil, "", err
	}
	cfgDir := filepath.Join(dir, "shows")
	if err := os.MkdirAll(cfgDir, 0o700); err != nil {
		return nil, "", err
	}
	path := filepath.Join(cfgDir, "shows.log")
	f, err := os.OpenFile(path, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o600)
	if err != nil {
		return nil, "", err
	}
	w := tolerantMultiWriter{f, os.Stderr}
	logger := slog.New(slog.NewJSONHandler(w, nil))
	slog.SetDefault(logger)
	return logger, path, nil
}

// tolerantMultiWriter fans Writes out to every wrapped writer and never
// surfaces an individual writer's error — the slog handler only sees
// success if at least one write succeeded. Order matters: the first
// writer is the "primary" (the file); secondary writers (stderr) being
// dead is silently OK.
type tolerantMultiWriter []io.Writer

func (m tolerantMultiWriter) Write(p []byte) (int, error) {
	var firstErr error
	anySuccess := false
	for _, w := range m {
		if n, err := w.Write(p); err == nil && n == len(p) {
			anySuccess = true
		} else if firstErr == nil {
			firstErr = err
		}
	}
	if anySuccess {
		return len(p), nil
	}
	return 0, firstErr
}

// startup is called by Wails when the window is created. We grab the
// host HWND, hand it to libmpv (so mpv embeds its render surface as a
// child of the Wails window), then spawn the runner goroutine that
// authenticates and drives the round-robin loop forever.
func (a *App) startup(ctx context.Context) {
	a.ctx = ctx

	// Tee the logger to a file under the per-user config dir alongside
	// token.json. mpv covers the WebView2 surface during playback so
	// the React status panel isn't visible — log persistence is the
	// only way to reconstruct "what went wrong" after the fact, and
	// the startDebugServer endpoint below is the only way to inspect
	// "what's going on right now" without staring at the window.
	if logger, logPath, err := initLogFile(); err != nil {
		a.logger.Warn("log file init failed; logging only to stderr", "err", err)
	} else {
		a.logger = logger
		a.logger.Info("logging to file", "path", logPath)
	}

	if err := a.startDebugServer(ctx); err != nil {
		a.logger.Warn("debug server failed to start", "err", err)
	}

	hwnd, err := win32.WaitForWindow(windowTitle, 2*time.Second)
	if err != nil {
		a.logger.Warn("could not locate host window — mpv will open its own", "err", err)
		hwnd = 0
	}
	a.hostHWND = hwnd

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
	a.setStatus("auth", "signing in via auth.romaine.life")
	openInBrowser := func(url string) error {
		wruntime.BrowserOpenURL(a.ctx, url)
		return nil
	}
	tok, err := oauth.EnsureToken(ctx, oauth.Config{Opener: openInBrowser})
	if err != nil {
		a.setStatus("error", fmt.Sprintf("auth: %v", err))
		a.logger.Error("oauth.EnsureToken", "err", err)
		return
	}

	client := apiclient.New("", tok.Token)
	// Token refresh on 401: re-runs the sign-in flow. If the cached
	// token is still valid, EnsureToken short-circuits silently; if
	// not, the browser opens for sign-in again.
	client.RefreshToken = func() (string, error) {
		fresh, err := oauth.EnsureToken(ctx, oauth.Config{Opener: openInBrowser})
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

			// Lift libmpv's render window above the WebView2 chrome.
			// Both are children of the host window; WebView2 wins the
			// z-order by default, so without this the video plays
			// (audible) but stays hidden behind the React UI. Re-raised
			// every round in case WebView2 reclaimed the top on focus.
			if a.hostHWND != 0 {
				if mpvHWND := win32.RaiseChildByClass(a.hostHWND, "mpv"); mpvHWND == 0 {
					a.logger.Warn("could not find mpv render window to raise; video may be hidden behind chrome")
				}
			}
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
