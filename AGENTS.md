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

## New computer setup

Map the NAS share as `S:`:

```powershell
New-PSDrive -Name S -PSProvider FileSystem -Root "\\192.168.50.41\files" -Persist
```

The library normally expects show roots under `S:\Group-Nelson`. See `docs/new-computer-setup.md` for launcher, channel publishing, database, and file-cache checks.

## Build and local testing

The frontend is embedded into the release executable. Use the NAS publisher for cross-computer testing:

```powershell
cd desktop-rs\frontend
npm run build

cd ..
cargo test --workspace
cargo build --release -p shows-desktop
..\scripts\publish-shows-channel.ps1 -Channel dev
```

The normal local test install is `D:\Downloads\shows\shows-desktop.exe`. After a successful release build, close the running app and copy the new executable:

```powershell
Stop-Process -Name shows-desktop -Force -ErrorAction SilentlyContinue
Copy-Item -Force `
  .\target\release\shows-desktop.exe `
  D:\Downloads\shows\shows-desktop.exe
```

Use `scripts/test-install-release.ps1` for the legacy fixed-path smoke test when appropriate. The normal cross-computer path is the versioned launcher governed by `docs/feature-contracts/desktop-distribution.md`.

## Release

`.github/workflows/build-desktop.yaml` builds on Windows, embeds the React distribution, bundles `libmpv-2.dll`, and publishes a GitHub Release. The desktop is not deployed to AKS.
