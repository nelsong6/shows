# shows

Per-user TV-show playlist orchestrator. Round-robin one episode from each active show, deterministic-random ordering via path hash, advance after each round.

Replaces a set of PowerShell + JSON scripts from [nelsong6/play_show](https://github.com/nelsong6/play_show).

## Components

- **`cmd/shows-api`** — HTTP API deployed to AKS at `shows.romaine.life`. Owns the playlist state, computes round ordering, records watch history. Postgres (CloudNativePG) for storage. Auth via auth.romaine.life JWTs.
- **`cmd/shows-client`** — local Windows binary. Authenticates via auth.romaine.life device flow, drives [mpv](https://mpv.io) over its JSON IPC socket, plays episodes in an infinite loop until you close it.
- **`cmd/shows-migrate`** — one-shot import from the legacy `nelson.json` + per-show JSON file layout.

## Architecture

```
PC (D:\Downloads\Group-Nelson\*.mkv)
└─ shows-client.exe
    ├─ spawns mpv as subprocess (--input-ipc-server=\\.\pipe\shows-mpv)
    ├─ JWT auth via auth.romaine.life device flow (cached at %APPDATA%\shows\token.json)
    └─ HTTPS ──► shows.romaine.life
                    └─ cmd/shows-api (AKS pod)
                        └─ CNPG Postgres (in-cluster)
```

The video files never leave the PC. The API only stores metadata: show name, root path on disk, per-episode relative paths, watch history.

## Ordering algorithm

For each round, the API selects the next unwatched episode from each active show, then sorts them by `uint32(first_4_hex_chars(sha256(utf8(root_path + "\" + relative_path))))`. The sort is deterministic, so re-fetching the round before any `advance` returns the same order — survives client restarts cleanly.

## Setup

See bootstrap notes in [CLAUDE.md](./CLAUDE.md).
