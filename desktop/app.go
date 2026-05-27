package main

import (
	"context"
	"fmt"
	"log"
	"sync"
	"time"

	"github.com/nelsong6/shows/desktop/internal/player"
	"github.com/nelsong6/shows/desktop/internal/win32"
)

// windowTitle must match the Title field in main.go's wails.Run
// options.App. win32.FindWindowByTitle uses it to locate the host
// HWND so libmpv can parent its render surface into it.
const windowTitle = "shows"

// App is the Wails-bound application object. Its public methods are
// auto-exposed to the TypeScript frontend at frontend/wailsjs/go/main/App.
type App struct {
	ctx context.Context

	mu     sync.Mutex
	player *player.Player
}

func NewApp() *App {
	return &App{}
}

// startup is called by Wails when the window is created. We grab the
// host HWND and hand it to libmpv so mpv embeds its render surface as
// a child of the Wails window — single window, no rogue mpv popup.
//
// The window may not be enumerable for a tick after OnStartup fires,
// so WaitForWindow polls briefly. If it never appears we log and
// continue with parentHWND=0; mpv falls back to its own window, which
// is the Phase 1b behavior.
func (a *App) startup(ctx context.Context) {
	a.ctx = ctx

	hwnd, err := win32.WaitForWindow(windowTitle, 2*time.Second)
	if err != nil {
		log.Printf("startup: could not locate host window %q: %v (mpv will open its own window)", windowTitle, err)
		hwnd = 0
	}

	p, err := player.New(hwnd)
	if err != nil {
		log.Printf("startup: player.New: %v", err)
		return
	}
	a.mu.Lock()
	a.player = p
	a.mu.Unlock()
}

// shutdown is called by Wails when the user closes the window. Tear
// down the libmpv handle so the OS process exits cleanly.
func (a *App) shutdown(ctx context.Context) {
	a.mu.Lock()
	defer a.mu.Unlock()
	if a.player != nil {
		_ = a.player.Close()
		a.player = nil
	}
}

// PlayTestFile is the Phase 1b/1c smoke-test entry point: asks libmpv
// to play the given path. Returns the error string ("" on success)
// so the TypeScript caller can surface it without ad-hoc Promise
// rejection handling.
//
// Replaced in Phase 2 by a real PlayEpisode method that takes a
// round entry from the shows API.
func (a *App) PlayTestFile(path string) string {
	a.mu.Lock()
	defer a.mu.Unlock()

	if a.player == nil {
		return "player not initialized (startup may have failed; check logs)"
	}
	if err := a.player.Play(a.ctx, path); err != nil {
		return fmt.Sprintf("play: %v", err)
	}
	return ""
}
