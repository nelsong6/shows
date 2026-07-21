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

## Database

`database` identifies the state authority:

- `path`: configured NAS SQLite path.
- `authoritative`: always `true`.
- `revision`: monotonically increasing shared change counter.

`sync` and `startup_sync` are absent. A revision change is broadcast through the
normal status stream so the frontend can refresh library reads after another
computer commits an edit.

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
- `alerts`: derived server-side from status and file-sync state.

Any non-error phase patch must clear stale round error state by setting
`error_kind: null` and `round_blocked: false`.

## Manual Repair And Reload

When `round_blocked` is true and `file_sync.problems` includes bad entries, the
frontend may call `POST /round/remove-entry` for one or more `episode_id`s. The
server removes those rows from the authoritative `round_queue`.

The frontend must then call `POST /round/reload`. Reload interrupts the player,
causing the runner to rebuild from the repaired `round_queue` without restarting
the app. Reload is deliberate; removing entries alone does not mutate the
already-loaded in-memory playlist.
