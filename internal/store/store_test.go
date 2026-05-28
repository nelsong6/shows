package store

import (
	"testing"
	"time"
)

func ptrTime(t time.Time) *time.Time { return &t }

func TestFirstUnwatched(t *testing.T) {
	now := time.Now()

	t.Run("picks lowest position among unwatched", func(t *testing.T) {
		eps := []episodeDoc{
			{ID: "c", Position: 2},
			{ID: "a", Position: 0},
			{ID: "b", Position: 1},
		}
		got := firstUnwatched(eps)
		if got == nil || got.ID != "a" {
			t.Fatalf("firstUnwatched = %v, want episode a", got)
		}
	})

	t.Run("skips watched even at a lower position", func(t *testing.T) {
		eps := []episodeDoc{
			{ID: "a", Position: 0, WatchedAt: ptrTime(now)},
			{ID: "b", Position: 1},
		}
		got := firstUnwatched(eps)
		if got == nil || got.ID != "b" {
			t.Fatalf("firstUnwatched = %v, want episode b", got)
		}
	})

	t.Run("nil when every episode is watched", func(t *testing.T) {
		eps := []episodeDoc{
			{ID: "a", Position: 0, WatchedAt: ptrTime(now)},
			{ID: "b", Position: 1, WatchedAt: ptrTime(now)},
		}
		if got := firstUnwatched(eps); got != nil {
			t.Fatalf("firstUnwatched = %v, want nil", got)
		}
	})

	t.Run("nil when empty", func(t *testing.T) {
		if got := firstUnwatched(nil); got != nil {
			t.Fatalf("firstUnwatched = %v, want nil", got)
		}
	})
}

// Defer contract D1–D3: re-roll one show's next-up pick by bumping an
// unwatched episode to the back of its queue, never marking it watched.
func TestDeferEpisode(t *testing.T) {
	now := time.Now()

	t.Run("bumps to back, stays unwatched, next pick changes (D1/D2)", func(t *testing.T) {
		eps := []episodeDoc{
			{ID: "a", Position: 0},
			{ID: "b", Position: 1},
			{ID: "c", Position: 2},
		}
		if !deferEpisode(eps, "a") {
			t.Fatal("deferEpisode = false, want true")
		}
		if eps[0].Position != 3 {
			t.Fatalf("deferred position = %d, want max+1 = 3", eps[0].Position)
		}
		if eps[0].WatchedAt != nil {
			t.Fatal("defer marked the episode watched — must not (D2)")
		}
		if got := firstUnwatched(eps); got == nil || got.ID != "b" {
			t.Fatalf("after defer, firstUnwatched = %v, want b", got)
		}
	})

	t.Run("deferring the only unwatched keeps it the pick", func(t *testing.T) {
		eps := []episodeDoc{
			{ID: "a", Position: 0, WatchedAt: ptrTime(now)},
			{ID: "b", Position: 1},
		}
		if !deferEpisode(eps, "b") {
			t.Fatal("deferEpisode = false, want true")
		}
		if got := firstUnwatched(eps); got == nil || got.ID != "b" {
			t.Fatalf("firstUnwatched = %v, want b (still the only unwatched)", got)
		}
	})

	t.Run("already-watched episode is a no-op (D3)", func(t *testing.T) {
		eps := []episodeDoc{
			{ID: "a", Position: 0, WatchedAt: ptrTime(now)},
			{ID: "b", Position: 1},
		}
		before := eps[0].Position
		if deferEpisode(eps, "a") {
			t.Fatal("deferEpisode = true for a watched episode, want false (D3)")
		}
		if eps[0].Position != before {
			t.Fatalf("position changed on a no-op defer: %d -> %d", before, eps[0].Position)
		}
	})

	t.Run("absent episode is a no-op (D3)", func(t *testing.T) {
		eps := []episodeDoc{{ID: "a", Position: 0}}
		if deferEpisode(eps, "nope") {
			t.Fatal("deferEpisode = true for an absent episode, want false (D3)")
		}
	})
}

// Advance contract I3/I5/I7, exercised through the pure core applyAdvance.
func TestApplyAdvance(t *testing.T) {
	now := time.Date(2026, 5, 28, 12, 0, 0, 0, time.UTC)

	t.Run("subset of round advances only the named episode (I7)", func(t *testing.T) {
		d := &showDoc{ID: "s1", Episodes: []episodeDoc{
			{ID: "a", Position: 0},
			{ID: "b", Position: 1},
		}}
		history, advanced, removed := applyAdvance(d, []string{"a"}, now)
		if advanced != 1 {
			t.Fatalf("advanced = %d, want 1", advanced)
		}
		if removed {
			t.Fatal("removed = true, want false (b still unwatched)")
		}
		if d.Episodes[0].WatchedAt == nil {
			t.Fatal("episode a not marked watched")
		}
		if d.Episodes[1].WatchedAt != nil {
			t.Fatal("episode b wrongly marked watched")
		}
		if len(history) != 1 || history[0].EpisodeID != "a" {
			t.Fatalf("history = %+v, want one row for a", history)
		}
	})

	t.Run("re-advancing a watched episode is a no-op (I3)", func(t *testing.T) {
		earlier := now.Add(-time.Hour)
		d := &showDoc{ID: "s1", Episodes: []episodeDoc{
			{ID: "a", Position: 0, WatchedAt: ptrTime(earlier)},
			{ID: "b", Position: 1},
		}}
		history, advanced, removed := applyAdvance(d, []string{"a"}, now)
		if advanced != 0 {
			t.Fatalf("advanced = %d, want 0 (already watched)", advanced)
		}
		if len(history) != 0 {
			t.Fatalf("history len = %d, want 0", len(history))
		}
		if removed {
			t.Fatal("removed = true, want false")
		}
		if !d.Episodes[0].WatchedAt.Equal(earlier) {
			t.Fatal("re-advance overwrote the original watched_at")
		}
	})

	t.Run("draining the last unwatched episode tombstones (I5)", func(t *testing.T) {
		d := &showDoc{ID: "s1", Episodes: []episodeDoc{
			{ID: "a", Position: 0, WatchedAt: ptrTime(now.Add(-time.Hour))},
			{ID: "b", Position: 1},
		}}
		_, advanced, removed := applyAdvance(d, []string{"b"}, now)
		if advanced != 1 {
			t.Fatalf("advanced = %d, want 1", advanced)
		}
		if !removed {
			t.Fatal("removed = false, want true (show drained)")
		}
		if d.RemovedAt == nil {
			t.Fatal("RemovedAt not set on tombstone")
		}
	})

	t.Run("advancing the whole round at once drains and tombstones", func(t *testing.T) {
		d := &showDoc{ID: "s1", Episodes: []episodeDoc{
			{ID: "a", Position: 0},
			{ID: "b", Position: 1},
		}}
		history, advanced, removed := applyAdvance(d, []string{"a", "b"}, now)
		if advanced != 2 {
			t.Fatalf("advanced = %d, want 2", advanced)
		}
		if len(history) != 2 {
			t.Fatalf("history len = %d, want 2", len(history))
		}
		if !removed || d.RemovedAt == nil {
			t.Fatal("show with all episodes advanced was not tombstoned")
		}
	})

	t.Run("unknown episode id advances nothing", func(t *testing.T) {
		d := &showDoc{ID: "s1", Episodes: []episodeDoc{{ID: "a", Position: 0}}}
		_, advanced, removed := applyAdvance(d, []string{"nope"}, now)
		if advanced != 0 || removed {
			t.Fatalf("advanced=%d removed=%v, want 0/false", advanced, removed)
		}
		if d.Episodes[0].WatchedAt != nil {
			t.Fatal("an unrelated episode was marked watched")
		}
	})
}

func TestAllWatched(t *testing.T) {
	now := time.Now()
	if allWatched([]episodeDoc{{ID: "a"}, {ID: "b", WatchedAt: ptrTime(now)}}) {
		t.Fatal("allWatched = true with one episode still unwatched")
	}
	if !allWatched([]episodeDoc{{ID: "a", WatchedAt: ptrTime(now)}}) {
		t.Fatal("allWatched = false when every episode is watched")
	}
}
