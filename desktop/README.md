# shows-desktop

The local desktop app — Wails v2 + React/TS frontend + libmpv-backed playback. Runs against [shows.romaine.life](https://shows.romaine.life) for round-robin scheduling, watch history, and library state.

## Build

```powershell
scoop install mingw   # one-time, for cgo
pwsh scripts/build.ps1
```

`scripts/build.ps1` does the full pipeline: downloads `libmpv-2.dll` + headers via `scripts/setup-libmpv.ps1` (idempotent — skips if already present), wires CGO env vars to point at them, runs `wails build`, and copies the runtime DLL into `build/bin/` alongside the resulting `shows.exe`.

Output: `build/bin/shows.exe` (~11 MB) and `build/bin/libmpv-2.dll` (~110 MB, codec engine). Both are needed at runtime; ship them together.

## Dev loop

```powershell
$env:Path = "$env:USERPROFILE\scoop\apps\mingw\current\bin;$(Resolve-Path third_party/libmpv);$env:Path"
$env:CGO_CFLAGS = "-I$(Resolve-Path third_party/libmpv)\include"
$env:CGO_LDFLAGS = "-L$(Resolve-Path third_party/libmpv) -lmpv"
wails dev
```

`wails dev` hot-reloads the frontend on save and rebuilds the Go side when its files change. libmpv changes are rare enough that re-invoking the env-var setup once per terminal session is fine.

## Layout

```
desktop/
  app.go                 # Wails-bound App object; methods are exposed to the TS frontend
  main.go                # wails.Run entry; window options
  internal/
    player/              # libmpv wrapper (cgo via supersonic-app/go-mpv)
  frontend/              # Vite + React + TS
    src/App.tsx          # phase-1b smoke-test UI; replaced in phase 3
    wailsjs/             # auto-generated TS bindings for app.go methods
  scripts/
    setup-libmpv.ps1     # downloads + extracts libmpv-dev to third_party/libmpv
    build.ps1            # full reproducible build (calls setup + wails)
  third_party/           # gitignored — libmpv DLLs + headers live here per-machine
  build/                 # gitignored output
```

## Phase 1b status

What works:
- Wails app launches, shows a phase-1b smoke-test UI
- "play" button invokes libmpv with the typed path
- mpv opens its own window and plays the file

What's missing (Phase 1c):
- mpv opens a separate window. Reparenting into the Wails window via `--wid` or the libmpv render API lands in Phase 1c
- No event loop yet — once playback ends, the file just sits there. Phase 1c adds the `end-file` event subscription that the round-robin loop hangs off of.
