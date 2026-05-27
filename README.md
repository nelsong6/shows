# shows

Per-user TV-show playlist orchestrator. Round-robin one episode from each active show, deterministic-random ordering via path hash, advance after each round, repeat forever.

Replaces a set of PowerShell + JSON scripts from [nelsong6/play_show](https://github.com/nelsong6/play_show).

## Components

- **`cmd/shows-api`** — HTTP API deployed to AKS at `shows.romaine.life`. Owns playlist state, computes round ordering, records watch history. Backed by Cosmos DB on the shared `infra-cosmos-serverless` account. JWT auth via auth.romaine.life. Prometheus-instrumented.
- **`desktop/`** — Wails v2 + React/TS app embedding libmpv via cgo. Single window — mpv renders inside the Wails host via `--wid`. First launch opens a browser for the normal auth.romaine.life Microsoft/Google sign-in; the resulting user JWT caches at `%APPDATA%\shows\token.json`. Runs forever until you close it.
- **`cmd/shows-migrate`** — one-shot CLI that imports the legacy `nelson.json` + per-show JSONs into the API. Deleted in a future phase when the desktop grows an in-app import surface.

## Architecture

```
PC (D:\Downloads\Group-Nelson\*.mkv)
└─ desktop\build\bin\shows.exe       (Wails host, libmpv embedded via cgo)
    ├─ mpv parented into the Wails window via --wid
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
