// Package player is the libmpv-backed playback engine for shows-desktop.
//
// Lifecycle:
//
//	p, err := player.New(parentHWND)
//	defer p.Close()
//	p.Play(ctx, "C:\\path\\to\\file.mkv", PlayReplace)
//	for ev := range p.Events() {
//	    if ev == EventEndFile { ... }
//	}
//
// When parentHWND is non-zero, libmpv's render surface is embedded as
// a child of that HWND (the Wails host window) via the --wid option.
// The player runs a goroutine that drains mpv's event queue and fans
// events out on the Events() channel.
package player

import (
	"context"
	"errors"
	"fmt"
	"sync"

	mpv "github.com/supersonic-app/go-mpv"
)

// PlayMode mirrors the loadfile command's third arg. "replace" stops
// what's playing and starts the new file; "append-play" queues behind
// the current item, starting playback only if nothing is playing.
type PlayMode string

const (
	PlayReplace    PlayMode = "replace"
	PlayAppend     PlayMode = "append"
	PlayAppendPlay PlayMode = "append-play"
)

// Event is the subset of mpv events we surface to the playlist runner.
// We don't expose the underlying *mpv.Event because go-mpv's binding
// doesn't decode the per-event payload (end-file reason, property
// names, etc.); for round-robin orchestration the bare event-id is
// enough.
type Event int

const (
	EventNone Event = iota
	EventFileLoaded
	EventStartFile
	EventEndFile
	EventPlaybackRestart
	EventIdle
	EventShutdown
)

// Player wraps a single long-lived libmpv handle plus an event pump.
// Methods are safe for concurrent use.
type Player struct {
	mu sync.Mutex
	m  *mpv.Mpv

	events chan Event
	done   chan struct{}
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
	// so the user gets a familiar experience.
	for _, opt := range [...]struct{ k, v string }{
		{"input-default-bindings", "yes"},
		{"input-vo-keyboard", "yes"},
		{"osc", "yes"},
		{"idle", "yes"},
		{"force-window", "yes"},
		// Don't pause when reaching end of file; we want playback to
		// flow into the next queued item without manual intervention.
		// (Default behavior is already correct here; pinning it
		// guards against an mpv.conf elsewhere overriding it.)
		{"keep-open", "no"},
	} {
		if err := m.SetOptionString(opt.k, opt.v); err != nil {
			return nil, fmt.Errorf("player: set %s: %w", opt.k, err)
		}
	}

	if err := m.Initialize(); err != nil {
		return nil, fmt.Errorf("player: initialize: %w", err)
	}

	p := &Player{
		m:      m,
		events: make(chan Event, 32),
		done:   make(chan struct{}),
	}
	go p.pump()
	return p, nil
}

// Play loads a file. Returns immediately once the command is
// dispatched — the caller should consume Events() to know when
// playback transitions.
func (p *Player) Play(ctx context.Context, path string, mode PlayMode) error {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.m == nil {
		return errors.New("player: closed")
	}
	return p.m.Command([]string{"loadfile", path, string(mode)})
}

// Stop clears the current playlist. mpv stays alive in idle mode
// because of the --idle option set in New.
func (p *Player) Stop(ctx context.Context) error {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.m == nil {
		return errors.New("player: closed")
	}
	return p.m.Command([]string{"stop"})
}

// PlaylistClear removes everything from mpv's internal playlist
// except the currently playing entry. Used between rounds so the
// playlist doesn't grow unbounded over a multi-hour session.
func (p *Player) PlaylistClear(ctx context.Context) error {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.m == nil {
		return errors.New("player: closed")
	}
	return p.m.Command([]string{"playlist-clear"})
}

// ShowText renders an OSD overlay for the given duration (mpv expects
// milliseconds). Used by the runner to surface the now-playing show
// name when each round entry starts. Non-fatal — OSD failures are
// cosmetic.
func (p *Player) ShowText(ctx context.Context, text string, durationMS int) error {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.m == nil {
		return errors.New("player: closed")
	}
	return p.m.Command([]string{"show-text", text, fmt.Sprintf("%d", durationMS)})
}

// Events returns the channel of mpv lifecycle events. Closed when the
// player is Closed or mpv issues EventShutdown.
func (p *Player) Events() <-chan Event { return p.events }

// Done returns a channel that closes when the event pump exits.
func (p *Player) Done() <-chan struct{} { return p.done }

// Close terminates the libmpv handle. Safe to call multiple times.
// The event pump stops, Events() drains and closes.
func (p *Player) Close() error {
	p.mu.Lock()
	m := p.m
	p.m = nil
	p.mu.Unlock()
	if m == nil {
		return nil
	}
	m.TerminateDestroy()
	return nil
}

// pump translates mpv's blocking event queue into our typed channel.
// Runs in a dedicated goroutine; exits on EventShutdown or when the
// handle is closed under it.
func (p *Player) pump() {
	defer close(p.events)
	defer close(p.done)

	for {
		p.mu.Lock()
		m := p.m
		p.mu.Unlock()
		if m == nil {
			return
		}

		// 0.1s timeout so we can notice the handle going away promptly
		// without busy-spinning. mpv wakes us up immediately on real
		// events anyway.
		ev := m.WaitEvent(0.1)
		if ev == nil {
			continue
		}
		var out Event
		switch ev.Event_Id {
		case mpv.EVENT_NONE:
			continue
		case mpv.EVENT_FILE_LOADED:
			out = EventFileLoaded
		case mpv.EVENT_START_FILE:
			out = EventStartFile
		case mpv.EVENT_END_FILE:
			out = EventEndFile
		case mpv.EVENT_PLAYBACK_RESTART:
			out = EventPlaybackRestart
		case mpv.EVENT_IDLE:
			out = EventIdle
		case mpv.EVENT_SHUTDOWN:
			out = EventShutdown
			select {
			case p.events <- out:
			default:
			}
			return
		default:
			continue
		}
		select {
		case p.events <- out:
		default:
			// Drop if consumer isn't draining. The runner only cares
			// about end-file/file-loaded; missing a tick or seek event
			// is harmless.
		}
	}
}
