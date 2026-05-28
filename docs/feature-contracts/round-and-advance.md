# Feature Contract: Round Selection + Advance

The round-robin playback loop is implemented by the **desktop engine** (`desktop-qt/shows/engine.py` + `runner.py`) against its local SQLite replica, syncing to the durable origin with `POST /sync`. (It began as a pair of server endpoints — `GET …/next-round` + `POST …/advance` — since removed in the offline-first migration; the invariants are unchanged, only their home moved. The endpoint-shaped prose in some invariants below is historical.) This document is the contract that engine satisfies. Implementations must preserve every invariant below; new behavior must extend this contract, not contradict it.

Advance *timing* — when an episode becomes watched — is governed by [Advance timing (per-episode)](#advance-timing-per-episode) below.

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

### I7. Advance accepts any non-empty subset of the round (per-episode skip)

`/advance` does not require the *entire* round. The client may post any non-empty subset of `(show_id, episode_id)` entries; each is marked watched independently, and `advanced_count` reflects only the newly-watched ones (per I3). Advancing a single entry is the **per-episode skip**: "I'm done with this one, move that show forward now" without waiting for the rest of the round.

Mechanism: `Advance` groups entries by `show_id` and read-modify-writes each show doc once, so a one-entry call touches exactly one show. Because re-advancing an already-watched episode is a no-op (I3), the desktop's round-end advance (which re-sends the whole round) safely re-includes anything skipped earlier in the round.

The skip marks the episode **watched** (it gets a `watch_history` row and the show offers its next-lowest-position unwatched episode next). To move past an episode *without* counting it watched, use defer (below).

## Failure modes

- **Empty playlist (no active shows)**: `/next-round` returns `{"round": []}`. Clients treat this as "drained, stop trying."
- **Network blip between `/next-round` and `/advance`**: client retries `/advance` with the original entries. Per I3, double-advance is idempotent.
- **Token expiry mid-round**: client re-runs the auth flow, gets a new token, retries. Server has no notion of "round in progress" — each request is independent.
- **Server restart mid-round**: same. Server holds no state about an in-flight round; the next `/next-round` returns the same shuffle because I1 (modulo nothing being advanced in between).

## Defer: swap a show's next-round pick

`POST /api/playlists/:name/defer-show` with `{show_id, episode_id}` re-rolls **one** show's next-up episode without marking anything watched — "not this one right now, show me a different one." It moves the named episode to the back of that show's own queue (`position = max(position) + 1`); `watched_at` is untouched and **no `watch_history` row is written**.

Invariants:

- **D1. Defer changes only the named show.** Other shows' next-round picks are unaffected; round order (I1) is simply recomputed from the new positions.
- **D2. Defer never marks watched.** The deferred episode stays unwatched; it resurfaces as the show's pick once all its earlier-position unwatched episodes are watched or deferred past it. A show is never tombstoned by a defer.
- **D3. Defer targets an unwatched episode.** Deferring an already-watched or unknown episode is a `404` no-op (nothing is reordered). This keeps defer distinct from advance.

Defer is the **without-counting-it-watched** counterpart to the per-episode skip in I7.

## Cross-playlist rounds

The per-playlist endpoints above stay the primary path; cross-playlist is purely additive for a client that wants to interleave several playlists.

- `GET /api/rounds?playlists=a,b,c` returns one episode per active show **across all named playlists**, ordered by the same SHA-256-of-path key as I1 over the merged candidate set. Each entry carries its `playlist` so the client can route the advance back. Unknown or empty playlists contribute nothing; if every named playlist is drained the response is `{"round": []}`.
- `POST /api/rounds/advance` with `{entries:[{playlist, show_id, episode_id}, …]}` groups entries by playlist and runs the same per-playlist `Advance` (atomic per show — I2; idempotent — I3) for each, summing `advanced_count` and concatenating `removed_shows`.

Invariants:

- **X1. Cross-playlist order is the single-playlist order over the union.** Hashing is per absolute path, so a cross-playlist round is identical to sorting the concatenation of each playlist's candidates — playlist membership never affects an episode's key.
- **X2. Advance stays per-playlist-atomic.** A cross-playlist advance is N independent per-playlist advances; there is no cross-playlist transaction. A failure surfaces after some playlists may have advanced (re-issue is safe by I3).

## Advance timing (per-episode)

The invariants above govern *what* a round is and *what* an advance does. This section governs *when* an advance happens on the desktop engine.

### A1. Advance is per-episode, at each episode's natural end

The desktop queues a whole round into mpv but advances **one episode at a time**: the moment a file plays to its natural end (mpv `end-file` with reason `EOF`), exactly that episode is marked watched in the replica — then a `watch_history` row is appended and the show is tombstoned if its queue is now empty (I5). It does **not** wait for the round to finish.

Consequence: closing the app partway through a round keeps precisely the episodes you watched and loses nothing. The next round is recomputed from the updated watched-state and resumes with what you haven't seen. This *supersedes* the old "re-fetching mid-round returns the byte-identical round" reading of I1 — re-fetching now reflects the per-episode advances, which is the stronger crash-survival guarantee (you resume at the next unwatched episode rather than replaying the whole round).

### A2. Only a watched episode advances

An episode advances **only** on a natural end (A1) or an explicit skip (I7). Every other way a file can end leaves it unwatched — a non-watch is never recorded as a watch:

- **Load failure** (`end-file` reason `ERROR`): a file that can't be opened is passed over, never marked watched. If *nothing* in a round opens, the runner treats it as "media unreachable" — it surfaces an error and parks rather than re-queuing the same unplayable round (which would spin and, before this rule, error-stormed entire rounds into a falsely-watched state).
- **Defer** (D1–D3) and **closing mid-episode**: the forced or early end isn't an `EOF`, so the episode stays the show's next pick (defer also bumps its position to the back of the queue).

This is the invariant the user's model turns on ("if I finished the Simpsons but missed Malcolm, don't make me re-watch the Simpsons, and don't skip Malcolm"). Mechanism: `shows/player.py` keys the advance callback on the `EOF` reason; `shows/runner.py` advances the now-playing entry on that callback. Locked by `desktop-qt/tests/test_runner.py`.

## Out of scope (today)

- Weighted or biased shuffles (e.g. "show I haven't watched in longest first"). Order remains the pure deterministic hash.

Adding anything here is a contract amendment — write a new section above first, then implement.

## Metrics (see `internal/api/metrics.go`)

| Metric | Type | What |
|---|---|---|
| `shows_round_size` | histogram | Episodes per `/next-round` response. Bucket `0` == playlist drained. |
| `shows_advanced_episodes_total` | counter | Sum of `advanced_count` across all `/advance` calls. |
| `shows_removed_shows_total` | counter | Sum of `len(removed_shows)` across all `/advance` calls — how many shows finished. |
| `shows_deferred_episodes_total` | counter | Episodes bumped to the back of their queue via `/defer-show` (D1–D3). |
| `shows_request_duration_seconds` | histogram | Per `(method, path, status)`. |

The PodMonitor at `k8s/templates/podmonitor.yaml` exposes these to the kube-prometheus-stack.
