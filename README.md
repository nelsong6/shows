# shows

Per-user TV-show playlist orchestrator. Round-robin one episode from each active show, deterministic-random ordering via path hash, advance after each round, repeat forever.

Replaces a set of PowerShell + JSON scripts from [nelsong6/play_show](https://github.com/nelsong6/play_show).

## Components

- **`cmd/shows-api`** — HTTP API deployed to AKS at `shows.romaine.life`. Owns playlist state, computes round ordering, records watch history. Backed by Cosmos DB on the shared `infra-cosmos-serverless` account. JWT auth via auth.romaine.life. Prometheus-instrumented.
- **`desktop-rs/`** — Rust + libmpv desktop app (located at `D:\Downloads\shows\shows-desktop.exe`). One DirectComposition window — mpv video renders under a transparent WebView2/React overlay. First launch opens a browser for the normal auth.romaine.life sign-in; the resulting user JWT caches at `%APPDATA%\shows\token.json`. Runs forever until you close it.

## Architecture

```
PC (D:\Downloads\Group-Nelson\*.mkv)
└─ desktop-rs\ → D:\Downloads\shows\shows-desktop.exe   (Rust + WebView2 + libmpv, one composition window)
    ├─ mpv video composited under a transparent React overlay (DirectComposition)
    ├─ Microsoft/Google sign-in via auth.romaine.life (PKCE + loopback)
    │  └─ Token cached at %APPDATA%\shows\token.json
    └─ HTTPS ──► shows.romaine.life
                    └─ cmd/shows-api (AKS pod)
                        └─ Cosmos: infra-cosmos-serverless / dbs/shows
                            ├─ shows (one doc per show, episodes embedded)
                            └─ watch_history (append-only)
```

The video files never leave the PC. The API only stores metadata: show name, root path on disk, per-episode relative paths, watch history.

## Ordering algorithm

For each round, the API selects the next unwatched episode from each active show, then sorts them by

```
uint32(first 4 hex chars of SHA-256(UTF-8(root_path + "\" + relative_path)))
```

This bit-for-bit reproduces the PowerShell `Get-FileHash -InputStream` + `SubString(0,4)` + `[uint32]` cast from the legacy `play_ordered_show.ps1`. The sort is deterministic, so re-fetching a round before any `advance` returns the same order — survives client restarts cleanly, preserves resume-where-you-left-off when migrating from the legacy scripts.

Full contract: [docs/feature-contracts/round-and-advance.md](./docs/feature-contracts/round-and-advance.md).

## Setup

See [CLAUDE.md](./CLAUDE.md) for the per-component layout, build process, and bootstrap order.

## Local Development & Testing

To build and test changes locally on Windows:

1. **Build the React Frontend**:
   ```bash
   cd desktop-rs/frontend
   npm run build
   ```
2. **Build the Desktop App** (which embeds the compiled React frontend):
   ```bash
   cd desktop-rs
   cargo build --release -p shows-desktop
   ```
3. **Deploy for Local Testing**:
   - Close the running app (it is safe to close anytime; watched progress is saved per-episode and resumes cleanly on restart).
   - Copy the newly compiled binary from `desktop-rs/target/release/shows-desktop.exe` to `D:\Downloads\shows\shows-desktop.exe` to swap the code over.
   - *Note: Since the frontend assets are now embedded directly in the binary, you no longer need to copy the `frontend/` folder to the target directory for release testing.*
