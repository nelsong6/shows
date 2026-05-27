# Feature Contract: Round Selection + Advance

The two endpoints `GET /api/playlists/:name/next-round` and `POST /api/playlists/:name/advance` together implement the round-robin playback loop. This document is the contract those endpoints satisfy. Implementations must preserve every invariant below; new behavior must extend this contract, not contradict it.

The shape comes from glimmung's pattern of [feature contracts as durable artifacts](https://github.com/nelsong6/glimmung/blob/main/docs/feature-contracts) — a future agent (or future-you) should be able to reason about the system from this doc without re-reading the implementation.

## Invariants

### I1. Deterministic round order

For the same set of `(show.id, episodes[0].relative_path, show.root_path)` tuples across all active shows in the playlist, `/next-round` returns the same ordering across calls.

Mechanism: each candidate's sort key is

```
uint32(first 4 hex chars of SHA-256(UTF-8(root_path + "\" + relative_path)))
```

Ties (vanishingly rare with a 32-bit key over ~40 shows) break by ascending `EpisodeID`.

This is the same hash function `play_ordered_show.ps1` used in the legacy PowerShell. Re-fetching a round before any `/advance` returns the byte-identical list — which is what lets the client survive crashes mid-round.

### I2. Advance is atomic per call

A single `/advance` call either:

- Marks every supplied `(show_id, episode_id)` watched, appends a `watch_history` row per episode, and tombstones any show whose queue is now empty — **or**
- Returns an error and leaves the database unchanged.

There is no partial-success state. The store wraps the per-show fan-out in a single logical operation; failure surfaces back as `5xx` with no observable mutation.

### I3. Advance is idempotent on re-played episodes

`/advance` against already-watched `(show_id, episode_id)` pairs is a no-op for those entries (the `UPDATE` filters on `watched_at IS NULL`). The `advanced_count` in the response reflects only newly-watched episodes.

Practical consequence: if the desktop client crashes after `/advance` succeeds server-side but before the client sees the 200, re-issuing the same `/advance` after restart is safe.

### I4. Tombstoned shows do not appear in subsequent rounds

`/next-round` filters `WHERE removed_at IS NULL`. Once a show is tombstoned in an `/advance` call, the next `/next-round` excludes it. The show row + its episodes + its `watch_history` rows remain queryable forever — only the active-set filter changes.

### I5. New episodes appended to an active show appear in the next round

`/api/shows/:id/episodes` appends to the show's `episodes` array with `position = max(position) + 1`. Since `/next-round` selects the episode with the lowest `position` that has `watched_at IS NULL`, newly-appended episodes only become "next" after all earlier-position unwatched episodes are watched. New episodes never jump the queue.

### I6. Round size never exceeds the count of active shows

One episode per active show per round. This is structural, not a soft limit — the `NextEpisodes` query is `DISTINCT ON (s.id) ... ORDER BY s.id, e.position`. Two episodes from the same show in the same round would violate the round-robin meaning of the feature.

## Failure modes

- **Empty playlist (no active shows)**: `/next-round` returns `{"round": []}`. Clients treat this as "drained, stop trying."
- **Network blip between `/next-round` and `/advance`**: client retries `/advance` with the original entries. Per I3, double-advance is idempotent.
- **Token expiry mid-round**: client re-runs the auth flow, gets a new token, retries. Server has no notion of "round in progress" — each request is independent.
- **Server restart mid-round**: same. Server holds no state about an in-flight round; the next `/next-round` returns the same shuffle because I1 (modulo nothing being advanced in between).

## Out of scope (today)

- Per-episode `/advance` (skip-this-episode-only without playing it). Today the entire round must advance together.
- "Swap show next round" — re-rolling a single show's next-round position. Would require either weighted-shuffle parameters or a dedicated `/skip-show` endpoint.
- Cross-playlist rounds (e.g. "play one episode from nelson, one from couple, alternating"). Each playlist is independent.

These appear on the roadmap because the desktop will eventually want hotkeys for them, but they're explicitly not part of this contract today. Adding any of them is a contract amendment — write a new section here first, then implement.

## Metrics (see `internal/api/metrics.go`)

| Metric | Type | What |
|---|---|---|
| `shows_round_size` | histogram | Episodes per `/next-round` response. Bucket `0` == playlist drained. |
| `shows_advanced_episodes_total` | counter | Sum of `advanced_count` across all `/advance` calls. |
| `shows_removed_shows_total` | counter | Sum of `len(removed_shows)` across all `/advance` calls — how many shows finished. |
| `shows_request_duration_seconds` | histogram | Per `(method, path, status)`. |

The PodMonitor at `k8s/templates/podmonitor.yaml` exposes these to the kube-prometheus-stack.
