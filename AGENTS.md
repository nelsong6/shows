# shows

Windows desktop app that drives libmpv for deterministic round-robin TV playback. The current product has no AKS service, HTTP API, Cosmos database, or authentication flow.

## Quality timeframe

This repo follows the long-term, heavy-solution operating mode codified in `romaine-life/glimmung/docs/quality-timeframes.md`. Compatibility layers are prohibited per `romaine-life/tank-operator/docs/migration-policy.md` — `legacy`, `compatibility`, `fallback`, `temporary`, and `exception` are deletion targets, not design options.

When extending a feature documented at `docs/feature-contracts/`, name the affected contract in the PR and explain how the implementation proves the invariants still hold.

## Architecture

The Rust desktop is the engine. Round selection, advance, defer, resume, library management, and watch history run against a local SQLite replica at `%APPDATA%\shows\replica.db`.

The durable origin is a shared SQLite database on the NAS, normally `S:\shows.db`. Each computer reconciles its local replica with that database through SQLite-to-SQLite sync:

- `SHOWS_SHARED_DB` overrides the shared database path.
- `SHOWS_REPLICA` overrides the local replica path for safe throwaway runs.
- Startup pulls shared changes and pushes dirty local rows.
- Changes use last-write-wins timestamps; unsynced local rows remain dirty until a later successful push.
- Playback uses the local replica and does not wait on NAS I/O once a playable local round exists.
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
      replica.rs      local SQLite replica, mutations, dirty tracking, LWW merge
      engine.rs       pure round/advance/defer engine
      runner.rs       round-robin runner and local media cache orchestration
      sync.rs         local/shared SQLite reconciliation
      model.rs        persisted and sync row types
      update.rs       GitHub Releases update check
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
scripts/              release installation and data-audit helpers
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

Multiple computers may update the NAS database. Keep shared-database operations transactional, do not hold the local replica mutex during NAS I/O, and preserve the existing serialized sync orchestration. Failed NAS access must leave local dirty state recoverable for a later push.

Library import is in-app. Folder scanning derives backslash-joined `relative_path` values so the ordering hash remains stable.

## Desktop status and controls

The localhost control server exposes `GET /status/stream` as the React overlay's live SSE feed, `GET /status` as an on-demand snapshot, and the playback/library control routes in `desktop-rs/desktop/src/webserver.rs`. Sync status includes connectivity, pending row counts, the shared database path, and the retained startup-sync timeline.

Debug runs log to stderr. Release builds append to `%APPDATA%\shows\shows.log`.

## New computer setup

Map the NAS share as `S:`:

```powershell
New-PSDrive -Name S -PSProvider FileSystem -Root "\\192.168.50.41\files" -Persist
```

The library normally expects show roots under `S:\Group-Nelson`. See `docs/new-computer-setup.md` for sync and file-cache checks.

## Build and local testing

The frontend is embedded into the release executable. A release build is not ready for local testing until the installed executable has been replaced.

```powershell
cd desktop-rs\frontend
npm run build

cd ..
cargo test --workspace
cargo build --release -p shows-desktop
```

The normal local test install is `D:\Downloads\shows\shows-desktop.exe`. After a successful release build, close the running app and copy the new executable:

```powershell
Stop-Process -Name shows-desktop -Force -ErrorAction SilentlyContinue
Copy-Item -Force `
  .\target\release\shows-desktop.exe `
  D:\Downloads\shows\shows-desktop.exe
```

Use `scripts/test-install-release.ps1` when appropriate. Frontend assets are embedded; do not copy the frontend directory beside the executable.

## Release

`.github/workflows/build-desktop.yaml` builds on Windows, embeds the React distribution, bundles `libmpv-2.dll`, and publishes a GitHub Release. The desktop is not deployed to AKS.
