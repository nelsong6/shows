// Package playlist runs the round-robin playback loop against
// shows.romaine.life. It's the brain of the desktop app: pulls a
// round from the API, queues every episode into libmpv, waits for
// the round to drain, calls /advance, repeats forever.
//
// State machine:
//
//	IDLE     →  /next-round  →  queue N episodes  →  PLAYING
//	PLAYING  →  N×EndFile    →  /advance          →  IDLE (next round)
//	IDLE     →  empty round  →  DRAINED
//	DRAINED  →  ctx.Done     →  exit
//
// Errors at the API surface are non-fatal — we log + retry with
// backoff. Errors at the player surface are fatal because they
// usually mean mpv died.
package playlist

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"time"

	"github.com/nelsong6/shows/desktop/internal/apiclient"
	"github.com/nelsong6/shows/desktop/internal/player"
)

// Runner is constructed once per session and drives playback for the
// lifetime of the window. Cancel ctx to stop.
type Runner struct {
	Client   *apiclient.Client
	Player   *player.Player
	Playlist string
	Logger   *slog.Logger

	// OnRound, if non-nil, is invoked when a fresh round is queued.
	// Lets the frontend mirror what's playing in its own UI.
	OnRound func([]apiclient.RoundEntry)

	// OnAdvance fires after a successful /advance. Carries the
	// per-show "took N days to watch" reveal payload.
	OnAdvance func(*apiclient.AdvanceResult)

	// OnDrained fires once when /next-round returns empty. The
	// runner stops trying after that.
	OnDrained func()
}

// Run drives the loop until ctx cancels, the player shuts down, or
// the playlist drains.
func (r *Runner) Run(ctx context.Context) error {
	log := r.Logger
	if log == nil {
		log = slog.Default()
	}

	for {
		round, err := r.fetchRoundWithBackoff(ctx)
		if err != nil {
			return err
		}
		if len(round) == 0 {
			log.Info("playlist drained", "playlist", r.Playlist)
			if r.OnDrained != nil {
				r.OnDrained()
			}
			<-ctx.Done()
			return ctx.Err()
		}

		log.Info("round queued", "playlist", r.Playlist, "episodes", len(round))
		if err := r.queueRound(ctx, round); err != nil {
			return fmt.Errorf("queue round: %w", err)
		}
		if r.OnRound != nil {
			r.OnRound(round)
		}

		if err := r.waitRound(ctx, round); err != nil {
			return err
		}

		// Reclaim the just-played playlist entries so mpv's internal
		// playlist doesn't grow unbounded over a multi-hour session.
		// Keeps the currently playing entry (which is the first of
		// the next round once we re-queue).
		_ = r.Player.PlaylistClear(ctx)

		entries := make([]apiclient.AdvanceEntry, len(round))
		for i, ep := range round {
			entries[i] = apiclient.AdvanceEntry{
				ShowID:    ep.ShowID,
				EpisodeID: ep.EpisodeID,
			}
		}
		result, err := r.advanceWithRetry(ctx, entries)
		if err != nil {
			return fmt.Errorf("advance: %w", err)
		}
		log.Info("round advanced", "advanced", result.AdvancedCount, "removed_shows", len(result.RemovedShows))
		if r.OnAdvance != nil {
			r.OnAdvance(result)
		}
	}
}

func (r *Runner) queueRound(ctx context.Context, round []apiclient.RoundEntry) error {
	for i, ep := range round {
		mode := player.PlayAppendPlay
		if i == 0 {
			mode = player.PlayReplace
		}
		if err := r.Player.Play(ctx, ep.AbsolutePath, mode); err != nil {
			return fmt.Errorf("loadfile %s: %w", ep.AbsolutePath, err)
		}
	}
	return nil
}

// waitRound blocks until `n` EndFile events have been received from
// libmpv. While waiting, it watches for FileLoaded events too and
// uses them to display the now-playing show name as an OSD overlay —
// the user knows which show in the round-robin is currently playing
// without having to alt-tab to the library sidebar.
//
// FileLoaded fires once per loaded entry in order, so a simple
// counter into round[] gives us the right show name.
func (r *Runner) waitRound(ctx context.Context, round []apiclient.RoundEntry) error {
	ends := 0
	fileIdx := 0
	n := len(round)
	for ends < n {
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-r.Player.Done():
			return errors.New("mpv exited")
		case ev, ok := <-r.Player.Events():
			if !ok {
				return errors.New("mpv events channel closed")
			}
			switch ev {
			case player.EventFileLoaded:
				if fileIdx < n {
					entry := round[fileIdx]
					text := fmt.Sprintf("%s   (%d/%d)", entry.ShowName, fileIdx+1, n)
					_ = r.Player.ShowText(ctx, text, 4000)
					fileIdx++
				}
			case player.EventEndFile:
				ends++
			case player.EventShutdown:
				return errors.New("mpv shutdown event")
			}
		}
	}
	return nil
}

// fetchRoundWithBackoff retries /next-round on transient failures.
// Exponential backoff capped at 60s — covers brief shows-api outages,
// network blips, AKS pod restarts, etc. ctx cancellation aborts.
func (r *Runner) fetchRoundWithBackoff(ctx context.Context) ([]apiclient.RoundEntry, error) {
	backoff := 2 * time.Second
	for {
		round, err := r.Client.NextRound(ctx, r.Playlist)
		if err == nil {
			return round, nil
		}
		if ctx.Err() != nil {
			return nil, ctx.Err()
		}
		(r.Logger).Warn("next-round failed; retrying", "err", err, "backoff", backoff)
		select {
		case <-ctx.Done():
			return nil, ctx.Err()
		case <-time.After(backoff):
		}
		backoff *= 2
		if backoff > 60*time.Second {
			backoff = 60 * time.Second
		}
	}
}

// advanceWithRetry does the same backoff dance for /advance. Loss-
// less: an advance dropped here results in the same round being
// re-fetched and re-played on next iteration (idempotent), so we err
// on the side of retrying generously.
func (r *Runner) advanceWithRetry(ctx context.Context, entries []apiclient.AdvanceEntry) (*apiclient.AdvanceResult, error) {
	backoff := 2 * time.Second
	for {
		result, err := r.Client.Advance(ctx, r.Playlist, entries)
		if err == nil {
			return result, nil
		}
		if ctx.Err() != nil {
			return nil, ctx.Err()
		}
		(r.Logger).Warn("advance failed; retrying", "err", err, "backoff", backoff)
		select {
		case <-ctx.Done():
			return nil, ctx.Err()
		case <-time.After(backoff):
		}
		backoff *= 2
		if backoff > 60*time.Second {
			backoff = 60 * time.Second
		}
	}
}
