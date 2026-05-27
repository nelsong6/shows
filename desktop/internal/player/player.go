// Package player is the libmpv-backed playback engine for shows-desktop.
//
// Lifecycle:
//
//	p, err := player.New()
//	defer p.Close()
//	p.Play(ctx, "C:\\path\\to\\file.mkv")
//
// The player owns one libmpv handle for the lifetime of the process.
// mpv's own window is shown for now (Phase 1b — establish the cgo
// link); Phase 1c will reparent rendering into the Wails window via
// the render API or --wid passthrough.
package player

import (
	"context"
	"errors"
	"fmt"
	"sync"

	mpv "github.com/supersonic-app/go-mpv"
)

// Player wraps a single long-lived libmpv handle. Methods are safe for
// concurrent use — go-mpv's underlying handle is concurrency-safe and
// we serialize lifecycle operations on a mutex.
type Player struct {
	mu sync.Mutex
	m  *mpv.Mpv
}

// New creates and initializes a libmpv handle. If parentHWND is
// non-zero, mpv embeds its render surface as a child of that window
// via the --wid option — i.e. video draws inside the Wails window
// instead of mpv opening its own. Set before Initialize per
// https://mpv.io/manual/master/#options-wid (--wid is init-time only).
func New(parentHWND uintptr) (*Player, error) {
	m := mpv.Create()
	if m == nil {
		return nil, errors.New("player: mpv.Create returned nil")
	}

	if parentHWND != 0 {
		// mpv parses wid as a numeric string. 64-bit HWNDs fit in
		// uint64 since they're really truncated pointers; the
		// official manual blesses this exact "long decimal" form
		// for embedding into a native window.
		if err := m.SetOptionString("wid", fmt.Sprintf("%d", parentHWND)); err != nil {
			return nil, fmt.Errorf("player: set wid: %w", err)
		}
	}

	// Sensible defaults — match the shape mpv's own desktop builds use
	// so the user gets a familiar experience while we're still showing
	// mpv's own window in Phase 1b.
	if err := m.SetOptionString("input-default-bindings", "yes"); err != nil {
		return nil, fmt.Errorf("player: set input-default-bindings: %w", err)
	}
	if err := m.SetOptionString("input-vo-keyboard", "yes"); err != nil {
		return nil, fmt.Errorf("player: set input-vo-keyboard: %w", err)
	}
	if err := m.SetOptionString("osc", "yes"); err != nil {
		return nil, fmt.Errorf("player: set osc: %w", err)
	}
	if err := m.SetOptionString("idle", "yes"); err != nil {
		return nil, fmt.Errorf("player: set idle: %w", err)
	}
	if err := m.SetOptionString("force-window", "yes"); err != nil {
		return nil, fmt.Errorf("player: set force-window: %w", err)
	}

	if err := m.Initialize(); err != nil {
		return nil, fmt.Errorf("player: initialize: %w", err)
	}
	return &Player{m: m}, nil
}

// Play loads a file and starts playback. Returns immediately once the
// command is dispatched — the caller does NOT block waiting for end-
// of-file. Use Events() (TODO Phase 1c) to know when playback ends.
func (p *Player) Play(ctx context.Context, path string) error {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.m == nil {
		return errors.New("player: closed")
	}
	return p.m.Command([]string{"loadfile", path})
}

// Stop clears the current playlist. mpv stays alive in idle mode (the
// --idle option, set in New).
func (p *Player) Stop(ctx context.Context) error {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.m == nil {
		return errors.New("player: closed")
	}
	return p.m.Command([]string{"stop"})
}

// Close terminates the libmpv handle. Safe to call multiple times.
func (p *Player) Close() error {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.m == nil {
		return nil
	}
	p.m.TerminateDestroy()
	p.m = nil
	return nil
}
