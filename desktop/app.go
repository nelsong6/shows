package main

import (
	"context"
	"fmt"
	"sync"

	"github.com/nelsong6/shows/desktop/internal/player"
)

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

// startup is called by Wails when the window is created. The context
// is captured for later use with the runtime helpers.
func (a *App) startup(ctx context.Context) {
	a.ctx = ctx
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

// PlayTestFile is the Phase 1b smoke-test entry point: lazily inits
// libmpv on first call, then asks it to play the given path. Returns
// the error string ("" on success) so the TypeScript caller can
// surface it without ad-hoc Promise rejection handling.
//
// Replaced in Phase 1c by a real PlayEpisode method that takes a
// round entry from the shows API.
func (a *App) PlayTestFile(path string) string {
	a.mu.Lock()
	defer a.mu.Unlock()

	if a.player == nil {
		p, err := player.New()
		if err != nil {
			return fmt.Sprintf("player init: %v", err)
		}
		a.player = p
	}

	if err := a.player.Play(a.ctx, path); err != nil {
		return fmt.Sprintf("play: %v", err)
	}
	return ""
}
