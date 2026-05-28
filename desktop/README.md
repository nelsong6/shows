# shows-desktop

The local desktop app — Wails v2 + React/TS frontend + libmpv via cgo. Single window: mpv parents into the Wails host via `--wid`, so video plays inside the same window the React chrome lives in. Runs against [shows.romaine.life](https://shows.romaine.life) for round-robin scheduling, watch history, and library state.

## Build

```powershell
scoop install mingw                                              # one-time, for cgo
powershell -ExecutionPolicy Bypass -File scripts\build.ps1
```

`scripts/build.ps1` does the full pipeline: runs `scripts/setup-libmpv.ps1` (idempotent — downloads `libmpv-2.dll` + headers from shinchiro's SourceForge builds, skips if already present), wires `CGO_CFLAGS` / `CGO_LDFLAGS` to point at them, runs `wails build`, and copies the runtime DLL into `build/bin/` alongside the resulting `shows.exe`.

Output: `build\bin\shows.exe` (~12 MB) and `build\bin\libmpv-2.dll` (~110 MB, codec engine). Both are needed at runtime; ship them together.

## Run

```powershell
build\bin\shows.exe
```

First launch opens a browser tab at auth.romaine.life for the normal Microsoft/Google sign-in; the resulting user JWT caches at `%APPDATA%\shows\token.json` and refreshes silently on 401. Closing the window terminates everything.

## Dev loop

```powershell
$lib = (Resolve-Path third_party\libmpv).Path
$env:Path        = "$env:USERPROFILE\scoop\apps\mingw\current\bin;$lib;$env:Path"
$env:CGO_CFLAGS  = "-I$lib\include"
$env:CGO_LDFLAGS = "-L$lib -lmpv"
wails dev
```

`wails dev` hot-reloads the frontend on save and rebuilds the Go side when its files change. libmpv setup is per-terminal — re-export the three env vars after a shell restart.

## Layout

```
desktop/
  app.go                 # Wails App; methods auto-exposed to the TS frontend
  debug.go               # localhost /status + /health introspection server
  main.go                # wails.Run entry; window options
  internal/
    player/              # libmpv cgo wrapper (supersonic-app/go-mpv)
    win32/               # HWND lookup so libmpv embeds into the Wails window
    oauth/               # user-login flow against auth.romaine.life (PKCE + loopback)
    apiclient/           # shows.romaine.life HTTP client with 401 refresh
    playlist/            # round-robin runner: fetch → queue → wait → advance
  frontend/              # Vite + React + TS
    src/
      App.tsx            # sidebar + status panel + KPI strip
      App.css            # layout
      design-tokens.css  # vendored from glimmung/design-system/colors_and_type.css
      style.css          # base resets + console-plate button vocabulary
    wailsjs/             # auto-generated TS bindings for app.go methods
  scripts/
    setup-libmpv.ps1     # downloads + extracts libmpv-dev to third_party/libmpv
    build.ps1            # full reproducible build
  third_party/           # gitignored; libmpv DLLs + headers (per-machine)
  build/                 # gitignored output
```

## Architecture notes

**Single window:** when the Wails host window is created, `internal/win32.WaitForWindow` locates its HWND by title `"shows"` and passes it to libmpv as the `wid` option (init-time only, set before `mpv.Initialize`). mpv then embeds its render surface as a child of that window — one taskbar entry, one alt-tab target.

**During playback, mpv's child window covers the WebView2 chrome.** The React tree is visible at auth time, between rounds, and on drain. Layering chrome on top of the video via the libmpv render API into a `<canvas>` is a future phase; not a blocker for the durable-app shape.

**Auth:** RFC 8252 user-login flow against `auth.romaine.life` — `internal/oauth` binds `127.0.0.1:0`, opens the browser at `/api/auth/cli/user-login?redirect_uri=...&code_challenge=S256(verifier)&state=...` via `runtime.BrowserOpenURL`. If the user has no session cookie, auth.romaine.life bounces them through Microsoft/Google sign-in and returns. Once signed in, the server redirects to the loopback with a one-time `?code=...`; the desktop POSTs that + `code_verifier` to `/api/auth/cli/user-token` and gets the user's JWT in the response. The JWT never travels through the browser — no token in URL, no token in browser history.

**Playback loop** (`internal/playlist/Runner.Run`):

1. `GET /api/playlists/nelson/next-round` → ordered list of N episode absolute paths.
2. Queue them all with `loadfile path replace` for the first, `loadfile path append-play` for the rest. mpv plays seamlessly between them.
3. On each `EVENT_FILE_LOADED`, `show-text` the next show name as an OSD overlay for 4s — visible round position without alt-tabbing.
4. Wait for N `EVENT_END_FILE` events.
5. `POST /api/playlists/nelson/advance` with `entries=[{show_id, episode_id}, …]`.
6. `playlist-clear` to bound mpv's internal playlist memory over multi-hour sessions.
7. Loop.

Retries-with-backoff on transient `/next-round` and `/advance` failures. 401s trigger a token refresh and one retry.

## Inspecting a running instance

During playback mpv covers the WebView2 chrome, so the React status panel isn't visible. Two surfaces let you inspect state from outside:

- **`%APPDATA%\shows\shows.log`** — slog JSON, one event per line (auth state transitions, round fetches, runner errors). Append-only; no rotation.
- **`http://127.0.0.1:<port>/status`** — current Status snapshot as JSON (`phase`, `message`, `playlist`, current `round`, `last_advance`). The port is bound ephemerally and written to `%APPDATA%\shows\debug-port` on launch. Also exposes `/health` returning `ok`.

From PowerShell:

```powershell
$port = Get-Content "$env:APPDATA\shows\debug-port"
iwr "http://127.0.0.1:$port/status" -UseBasicParsing | Select-Object -ExpandProperty Content
Get-Content "$env:APPDATA\shows\shows.log" -Tail 20
```

Localhost-only, no auth — the surface exposes nothing that isn't already in the React frontend's status events. The JWT and other secrets stay in `%APPDATA%\shows\token.json`.
