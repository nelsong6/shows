# shows

Windows desktop app that drives libmpv for deterministic round-robin TV playback. The current product has no AKS service, HTTP API, Cosmos database, or authentication flow.

## Quality timeframe

This repo follows the long-term, heavy-solution operating mode codified in `romaine-life/glimmung/docs/quality-timeframes.md`. Compatibility layers are prohibited per `romaine-life/tank-operator/docs/migration-policy.md` — `legacy`, `compatibility`, `fallback`, `temporary`, and `exception` are deletion targets, not design options.

When extending a feature documented at `docs/feature-contracts/`, name the affected contract in the PR and explain how the implementation proves the invariants still hold.

## Architecture

The Rust desktop is the engine. Round selection, advance, defer, resume, library management, and watch history run directly against the authoritative SQLite database on the NAS, normally `S:\shows.db`:

- `SHOWS_SHARED_DB` overrides the shared database path.
- There is no local database replica, dirty-row preference, or manual sync.
- SQLite commit order is authoritative when multiple computers write.
- Database triggers increment a shared revision; running apps watch it so externally committed edits refresh their status consumers.
- The current round's media files are copied from the NAS to `D:\Downloads\Watching` before playback. `SHOWS_LOCAL_WATCHING_DIR` overrides that cache path.

There is no Cosmos or web-API source of truth in the current implementation.

## Layout

```text
desktop-rs/           Cargo workspace
  core/               shows-core; pure Rust, #![forbid(unsafe_code)]
    src/
      ordering.rs     SHA-256 path ordering
      roundlogic.rs   round helpers and playlist parsing
      scan.rs         folder scanning and episode discovery
      replica.rs      authoritative SQLite schema, reads, and transactions
      engine.rs       pure round/advance/defer engine
      runner.rs       round-robin runner and local media cache orchestration
      model.rs        persisted row types
      update.rs       Forgejo Releases update check
  control/            shows-control; typed loopback/SSH control protocol and client
  remote/             shows-remote; terminal remote and SSH agent, no database or player
  desktop/            shows-desktop; Windows shell and the only unsafe code
    src/
      main.rs         process wiring and shared/local database configuration
      compositor.rs   DirectComposition window and WebView2 overlay
      gl.rs           libmpv OpenGL to shared D3D texture
      mpv.rs          dynamically loaded libmpv FFI
      player.rs       player wrapper and event pump
      webserver.rs    localhost overlay/status/control server
  frontend/           Vite, React, and TypeScript overlay
docs/
  feature-contracts/  durable behavior and UI contracts
scripts/              NAS channel publisher, launcher, release install, and audits
```

The root Go module and stale README descriptions are historical residue; do not infer a currently deployed Go API from them. Verify architecture against `desktop-rs/` and the feature contracts.

## Ordering invariant

The deterministic round order is computed by `desktop-rs/core/src/ordering.rs`:

```text
hash := SHA-256(UTF-8 bytes of: root_path + "\" + relative_path)
order_value := uint32(first 4 hex chars of hash, parsed as base 16)
shows in round are sorted by order_value ascending
```

This reproduces the legacy PowerShell ordering bit-for-bit. See `docs/feature-contracts/round-and-advance.md`; changes to round selection, advance, defer, previous, resume, queue persistence, or startup behavior must preserve that contract.

## Shared database and concurrency

Multiple computers may update the NAS database. Keep multi-statement mutations transactional and preserve last-commit-wins behavior. The local media cache is disposable and must never become a database authority. If the database is unreachable, fail explicitly rather than creating divergent local state.

Library import is in-app. Folder scanning derives backslash-joined `relative_path` values so the ordering hash remains stable.

## Desktop status and controls

The localhost control server exposes `GET /status/stream` as the React overlay's live SSE feed, `GET /status` as an on-demand snapshot, and the playback/library control routes in `desktop-rs/desktop/src/webserver.rs`. Database status includes the authoritative path and shared revision.

Debug runs log to stderr. Release builds append to `%APPDATA%\shows\shows.log`.

The control listener is loopback-only at `127.0.0.1:45831` (`SHOWS_CONTROL_PORT`
overrides the port). `shows-remote` controls an already running desktop over
existing Windows OpenSSH, using the installed target agent and the same SSE
status feed. See `docs/feature-contracts/remote-control.md`.

## New computer setup

Map the NAS share as `S:` by hostname:

```powershell
New-PSDrive -Name S -PSProvider FileSystem -Root "\\NelsonNAS\files" -Persist
```

The library normally expects show roots under `S:\Group-Nelson`. See `docs/new-computer-setup.md` for launcher, channel publishing, database, and file-cache checks.

## Build and local testing

Use the fast development loop before opening a PR. See
`docs/development.md` and `docs/feature-contracts/development-lifecycle.md`.

```powershell
.\scripts\dev-shows.ps1 ui -Target upstairspc
.\scripts\dev-shows.ps1 remote -Target upstairspc status
.\scripts\dev-shows.ps1 desktop
.\scripts\dev-shows.ps1 test
.\scripts\dev-shows.ps1 stop
```

UI previews use tracked Vite HMR against the real engine and SSE feed; they do
not need a Rust build, a local player, or another database. Native previews
start paused and require Vite; debug builds do not embed frontend assets.
`SHOWS_DIST_DIR` and the fixed-path release-copy smoke script are retired.

Keep debug output warm across worktrees in `D:\build\shows\cargo` (override
`SHOWS_DEV_TARGET_DIR`). Local Cargo builds use at most four jobs. Release
compilation is not an edit-preview step on nelsonpc.

PRs run the relevant frontend/debug tests; main produces the optimized, embedded
release bundle. After CI and merge, publish and install the exact main release
on this PC and the TV in one operation:

```powershell
.\scripts\dev-shows.ps1 ship -Target upstairspc
```

The update command preserves a running TV's selected episode and pause state,
uses its interactive Windows session, and leaves a closed app closed. Stable
remains the hash-verified CI artifact; preview code never replaces it.

## Release

`.forgejo/workflows/build-desktop.yaml` builds on Windows, embeds the React distribution, bundles `libmpv-2.dll`, and publishes a Forgejo Release. The desktop is not deployed to AKS.
