# Feature Contract: Round Selection + Advance

The round-robin playback loop is implemented by the **desktop engine** (`desktop-qt/shows/engine.py` over the local SQLite replica in `shows/replica.py`, driven by `shows/runner.py`), syncing to the durable origin with `POST /sync`. It began as a pair of server endpoints — `GET …/next-round` + `POST …/advance` — removed in the offline-first migration; the invariants are unchanged, only their home moved (from server SQL to the client engine). This document is the contract that engine satisfies. Implementations must preserve every invariant below; new behavior must extend this contract, not contradict it.

Advance *timing* — when an episode becomes watched — is governed by [Advance timing (per-episode)](#advance-timing-per-episode) below.

The shape comes from glimmung's pattern of [feature contracts as durable artifacts](https://github.com/nelsong6/glimmung/blob/main/docs/feature-contracts) — a future agent (or future-you) should be able to reason about the system from this doc without re-reading the implementation.

## Invariants

### I1. Deterministic round order

For the same set of `(show.id, first-unwatched relative_path, show.root_path)` tuples across the active shows, `engine.next_round` returns the same ordering every call.

Mechanism: each candidate's sort key is

```
uint32(first 4 hex chars of SHA-256(UTF-8(root_path + "\" + relative_path)))
```

Ties (vanishingly rare with a 32-bit key over ~40 shows) break by ascending `episode_id`.

This is the same hash function `play_ordered_show.ps1` used in the legacy PowerShell (`shows/ordering.py` reproduces it bit-for-bit). The order is a pure function of the current unwatched set, so two calls against the same replica state are byte-identical — advances change that state per-episode (see A1).

### I2. Advance is atomic per call

A single advance (`engine.advance`, persisted by `replica.advance` in one SQLite transaction) either:

- Marks every supplied `(show_id, episode_id)` watched, appends a `watch_history` row per newly-watched episode, and tombstones any show whose queue is now empty — **or**
- Leaves the replica unchanged.

There is no partial-success state. The per-show fan-out is committed as one transaction; a failure rolls back with no observable mutation.

### I3. Advance is idempotent on re-played episodes

Advancing an already-watched `(show_id, episode_id)` is a no-op for that entry — `engine.advance` skips any episode whose `watched_at` is already set — so the returned newly-watched count reflects only freshly-watched episodes. Re-applying the same advance is therefore safe: a sync replayed after a crash, or the same episode reached twice, changes nothing the second time.

### I4. Tombstoned shows do not appear in subsequent rounds

`engine.next_round` skips shows whose `removed_at` is set (`replica.active_shows` selects `WHERE removed_at IS NULL`). Once a show is tombstoned by an advance, the next round excludes it. The show row + its episodes + its `watch_history` rows remain queryable forever — only the active-set filter changes.

### I5. New episodes appended to an active show appear in the next round

Appending episodes — `replica.add_episodes`, or a folder rescan via `shows/scan.py` — gives each new episode `position = max(position) + 1`. Since the round picks the lowest-`position` episode with `watched_at IS NULL`, newly-appended episodes only become "next" after all earlier-position unwatched episodes are watched. New episodes never jump the queue.

### I6. Round size never exceeds the count of active shows

One episode per active show per round. This is structural, not a soft limit — `engine.next_round` takes exactly one `first_unwatched(show.episodes)` per active show. Two episodes from the same show in one round would violate the round-robin meaning of the feature.

### I7. Advance accepts any non-empty subset of the round (per-episode skip)

An advance need not cover the *entire* round — a single `(show_id, episode_id)` is valid, and each entry is marked watched independently (the count reflects only the newly-watched, per I3). Advancing one entry is the **per-episode skip**: "I'm done with this one, move that show forward now," without waiting for the rest of the round. On the desktop this is the `n` control (`runner.skip`), which marks the current episode watched immediately and jumps mpv forward. (Natural advance — A1 — is the same one-entry operation, triggered by an episode's EOF rather than by the user.)

The skip marks the episode **watched** (a `watch_history` row is written; the show offers its next-lowest-position unwatched episode next). To move past an episode *without* counting it watched, use defer (below).

## Failure modes

Offline-first: the runner plays entirely off the replica and never blocks on the network; sync is best-effort.

- **Empty playlist (no active shows)**: `engine.next_round` returns `[]`; the runner treats it as drained and parks until shutdown.
- **Origin unreachable (network/server down)**: playback continues off the replica. `get_library` (pull) and `post_sync` (push) are best-effort — the `Syncer` flips its `online` flag on each attempt's success/failure (no polling), and queued local changes stay `dirty` until a later push lands.
- **Token expiry**: the apiclient refreshes once on a `401` (`oauth.ensure_token`) and retries in place; playback is unaffected.
- **Crash mid-round**: per-episode advances are already persisted to the replica (A1), so on restart the round is recomputed from that state and resumes at the next unwatched episode. Advances not yet pushed remain `dirty` and sync on the next push.

## Defer: swap a show's next pick

Defer (`runner.defer`, the `d` control) re-rolls **one** show's next-up episode without marking anything watched — "not this one right now, show me a different one." `engine.defer` moves the named episode to the back of that show's own queue (`position = max(position) + 1`); `watched_at` is untouched and **no `watch_history` row is written**.

Invariants:

- **D1. Defer changes only the named show.** Other shows' next picks are unaffected; round order (I1) is simply recomputed from the new positions.
- **D2. Defer never marks watched.** The deferred episode stays unwatched; it resurfaces as the show's pick once all its earlier-position unwatched episodes are watched or deferred past it. A show is never tombstoned by a defer.
- **D3. Defer targets an unwatched episode.** Deferring an already-watched or unknown episode is a no-op (`engine.defer` returns `False`; the runner logs it and does nothing). This keeps defer distinct from advance.

Defer is the **without-counting-it-watched** counterpart to the per-episode skip in I7.

## Previous: step back without watching

Previous (`runner.previous`, the `p` control) steps mpv back to the prior entry in the current round — pure navigation, the backward counterpart to the `n` skip. It issues `playlist-prev` *weak* (going back at the first entry is a no-op rather than ending playback); nothing is marked watched and no `watch_history` row is written.

Invariants:

- **P1. Previous never marks watched.** Stepping back is navigation only. If the revisited episode was already watched, replaying it to its natural end is a no-op by I3 — so back-and-forth navigation can neither double-advance nor skip an unwatched episode, and the round boundary (detected from playlist exhaustion, not an end-file tally) is unaffected.
- **P2. A replayed episode restarts from the beginning.** `replica.advance` clears `resume_pos` when it marks an episode watched, so stepping back to a finished episode plays it from the start rather than its prior resume point.

Previous is navigation; skip (I7) and defer (D1–D3) are the operations that change watched/queue state.

## Cross-playlist rounds

Single-playlist is the primary path; cross-playlist is purely additive — set `SHOWS_PLAYLISTS=a,b,c` to interleave several playlists in one rotation.

`engine.next_round` over the **union** of active shows across the named playlists returns one episode per active show, ordered by the same SHA-256-of-path key as I1 over the merged candidate set. Each entry carries its own `playlist`. Drained or unknown playlists contribute nothing; if every named playlist is drained the round is `[]`.

Invariants:

- **X1. Cross-playlist order is the single-playlist order over the union.** Hashing is per absolute path, so a cross-playlist round is identical to sorting the concatenation of each playlist's candidates — playlist membership never affects an episode's key.
- **X2. Advance stays per-show.** Each entry's advance touches only its own show (per-episode, A1); there is no cross-playlist transaction, and re-applying any entry is safe by I3.

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

## Observability

**Server** (`internal/api/metrics.go`) — the dumb origin only sees sync traffic now; the round/advance/defer counters left with the server-side round engine.

| Metric | Type | What |
|---|---|---|
| `shows_request_duration_seconds` | histogram | HTTP request duration, per `(method, path, status)`. |
| `shows_synced_records_total` | counter | Cumulative records (shows + episodes + history) accepted via `/sync`. |

The PodMonitor at `k8s/templates/podmonitor.yaml` exposes these to the kube-prometheus-stack.

**Desktop** — the engine's live state is the control server's `GET /status` (current round, now-playing/up-next, playback, and sync online/pending), with `/health` + `/shows`; logs go to stdout. See `desktop-qt/README.md`.
