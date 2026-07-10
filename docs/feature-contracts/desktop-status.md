# Desktop Status Payload Contract

The desktop control server owns the frontend status contract exposed by
`GET /status` and `GET /status/stream`. The stream sends full status snapshots
as `event: status` Server-Sent Events; the one-shot endpoint returns the same
shape.

This contract is intentionally frontend-facing. Any field used by
`desktop-rs/frontend` belongs here before it becomes a stable UI dependency.

## Required Base Fields

- `phase`: one of `initializing`, `auth`, `syncing`, `fetching`, `playing`, `drained`, `error`.
- `message`: human-readable current state.
- `playlist`: comma-separated active playlists for this runner.
- `round`: array of current round entries. Empty when no round is active.
- `round_pos`: zero-based index into `round`. Valid when `phase == "playing"` and `round` is non-empty.
- `round_id`: monotonically increasing desktop-local round identity, or `null` when no round is active.
- `window_maximized`, `window_fullscreen`, `window_on_top`: current shell state.
  `window_on_top` means the window floats above others (topmost Z-order); chrome
  and layout are otherwise identical to windowed mode. See
  [`desktop-shell.md`](desktop-shell.md) for the full shell contract.

## Round Entry

Each `round[]` item contains:

- `show_id`
- `show_name`
- `episode_id`
- `playlist`
- `order_value`
- `absolute_path`: local playback path under the watching cache.

## Playback

`playback` is present after mpv is attached:

- `time_pos`, `duration`, `percent_pos`
- `volume`
- `paused`, `core_idle`, `paused_for_cache`
- `sub_tracks`, `audio_tracks`
- `sid`, `aid`

## Sync

`sync` is present after the syncer is attached:

- `online`
- `pending`
- `pending_breakdown.shows`
- `pending_breakdown.episodes`
- `pending_breakdown.history`
- `pending_breakdown.queue`
- `last_error`
- `shared_db_path`

`pending` must equal the sum of `pending_breakdown`.

## Startup Sync Timeline

`startup_sync` is the retained, current-process account of the origin
reconciliation started at launch. It is included in every `GET /status` and
`GET /status/stream` snapshot so a frontend that connects after an early step
still receives the complete timeline.

- `state`: `running`, `succeeded`, or `degraded`. `degraded` means at least one
  launch step failed (origin reconciliation or local file-cache preparation),
  even when playback can continue from the usable local subset.
- `started_at`: RFC 3339 timestamp for the launch reconciliation.
- `finished_at`: RFC 3339 terminal timestamp, or `null` while running.
- `elapsed_ms`: total terminal duration, or `null` while running. The frontend
  derives a live elapsed value from `started_at`; the server does not poll just
  to advance a clock.
- `shared_db_path`: the configured durable-origin path.
- `playlists`: playlists included in the reconciliation.
- `events`: ordered structured progress records. Launch events already
  published are retained; a later event never rewrites an earlier one. After
  launch reaches a terminal state, later smart-moment pushes and manual syncs
  are excluded from this launch-scoped trace.

Each `events[]` record contains:

- `seq`: monotonically increasing within this process.
- `at`: RFC 3339 timestamp.
- `stage`: a stable identifier such as `startup.plan`, `local-round.load`,
  `origin.connect`, `origin.push`, `origin.pull`, `replica.merge`,
  `origin.complete`, `file-cache.check`, or `startup.complete`.
- `state`: `started`, `succeeded`, `skipped`, or `failed`.
- `message`: human-readable statement of what the desktop is doing or learned.
- `duration_ms`: completed-step duration, otherwise `null`.
- `counts`: optional `shows`, `episodes`, `history`, `queue`, and `total`.
  Whenever counts are present, `total` must equal the other four fields' sum.

A potentially blocking origin operation publishes its `started` event before
performing I/O and a terminal event afterward. This is the observability
invariant that makes a cold or unreachable shared database distinguishable from
local round loading and the media-file cache pass.

`startup.complete` is server-generated and is always the final retained event.
It is emitted only after both `origin.complete` and a terminal
`local-round.load` event have arrived, regardless of which finishes first.
Any startup-created round-queue push begins only after that local terminal
event, so it is a later smart-moment operation and cannot leave an unmatched
started event inside the frozen launch trace.

## File Sync

`file_sync` describes the most recent round file-cache pass:

- `copied`: files copied from NAS into the local watching cache.
- `cached`: round files already present locally, including cached copies used while the source is unavailable.
- `missing`: source unavailable and no local cached copy exists.
- `failed`: copy or local directory creation failed.
- `summary`: concise human-readable count summary.
- `incomplete`: true when `missing > 0 || failed > 0`.
- `problems`: capped example list for repair UI and alerts.

Each problem contains `episode_id`, `show_name`, `source_path`, `local_path`,
and `reason`.

The expected steady state after a healthy round load is `copied == 0`,
`missing == 0`, `failed == 0`, and `cached == round.length`. A value such as
`37 cached` is expected only when the active round has 37 entries and every
entry is already present in the local cache.

## Error And Alert Fields

- `error_kind`: currently `round_unplayable` for a round that produced no playable media; otherwise `null`.
- `round_blocked`: true only while a round-unplayable error is active.
- `alerts`: derived server-side from status and sync/file-sync state.

Any non-error phase patch must clear stale round error state by setting
`error_kind: null` and `round_blocked: false`.

## Manual Repair And Reload

When `round_blocked` is true and `file_sync.problems` includes bad entries, the
frontend may call `POST /round/remove-entry` for one or more `episode_id`s. The
server removes those rows from `round_queue` and pushes sync.

The frontend must then call `POST /round/reload`. Reload interrupts the player,
causing the runner to rebuild from the repaired `round_queue` without restarting
the app. Reload is deliberate; removing entries alone does not mutate the
already-loaded in-memory playlist.
