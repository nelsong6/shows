# shows-desktop (Qt)

The desktop client, built on **PySide6 + libmpv's render API**. One composited
window: mpv video on the bottom layer, a transparent web overlay drawing the
chrome on top — the
[Jellyfin Media Player](https://github.com/jellyfin/jellyfin-media-player)
architecture. Runs against [shows.romaine.life](https://shows.romaine.life) for
round-robin scheduling, watch history, and library state.

## Why this architecture

An earlier Go/Wails build embedded mpv as a child HWND via `--wid`. On Windows a
transparent WebView2 composites only over its **host window's background**, never
over a **sibling** child HWND — so the web chrome could never sit on top of
playing video. (A spike confirmed this directly; see PR #16.) The fix is to stop
using a separate video window at all: mpv renders into an OpenGL FBO that the Qt
scene graph composites, with a transparent `WebEngineView` layered above it in
the same QML tree. Qt owns the compositing, so the overlay reliably draws over
live video.

## Run

```powershell
pip install -r requirements.txt
# Build the overlay (the React app under frontend/) — required; the control
# server serves this bundle and has no placeholder fallback.
pushd frontend; npm ci; npm run build; popd
python main.py
```

`main.py` locates `libmpv-2.dll` at startup, in order: the frozen-bundle root
(when packaged), `$SHOWS_LIBMPV_DIR`, then a scoop mpv install
(`%USERPROFILE%\scoop\apps\mpv\current`). On a machine with none of those, point
`SHOWS_LIBMPV_DIR` at any folder containing `libmpv-2.dll` (or `mpv-2.dll`) — e.g.
a `winget install mpv.net` ships one at `%LOCALAPPDATA%\Programs\mpv.net`.

First launch opens a browser at auth.romaine.life for the normal Microsoft/Google
sign-in; the resulting user JWT caches at `%APPDATA%\shows\token.json` and
refreshes silently on 401. Copy that file to a new machine to skip the browser
login.

## Layout

```
desktop-qt/
  main.py                # entry: GL setup, QML window, runner wiring
  shows-qt.spec          # PyInstaller onedir build (bundles libmpv + the overlay)
  requirements.txt       # runtime deps (PySide6, python-mpv, httpx)
  shows/
    mpv_item.py          # MpvItem(QQuickFramebufferObject): mpv render API → FBO
    webserver.py         # ControlServer: serves the overlay + /status /shows /pause /skip /defer
    oauth.py             # auth.romaine.life PKCE + loopback user-login, cached token
    apiclient.py         # shows.romaine.life HTTP client with 401 refresh
    runner.py            # round-robin runner: fetch → queue → wait → skip/defer → advance
    roundlogic.py        # pure helper: SHOWS_PLAYLISTS parse, no Qt/mpv
    player.py            # python-mpv handle wrapper (play/pause/skip/show_text)
    ordering.py          # SHA-256 path ordering, bit-identical to the server
```

The overlay UI is the React app under `frontend/`; its data layer
(`src/api.ts`) polls this server's `/status` + `/shows` and POSTs `/pause`,
`/skip`, and `/defer`.

## Architecture notes

**Compositing** (`shows/mpv_item.py`): `MpvItem` is a `QQuickFramebufferObject`.
Its renderer creates an `MpvRenderContext("opengl", …)` and renders each frame
into the item's FBO; the QML scene graph then composites that under the
transparent `WebEngineView`. Three things are load-bearing and were each a
separate failure mode to discover:

- The window must use the **OpenGL RHI backend**
  (`QQuickWindow.setGraphicsApi(OpenGL)`) with **`AA_ShareOpenGLContexts`**, and
  `QtWebEngineQuick.initialize()` must run before the app is constructed.
- **`flip_y=False`** in `MpvRenderContext.render`. A `QQuickFramebufferObject`'s
  FBO uses the opposite vertical convention from a plain GL widget's default
  framebuffer, so `flip_y=True` renders the video upside down.
- **Do not `import QtQuickWidgets`** anywhere in the process. Pulling it in
  silently breaks WebEngine compositing — the overlay goes black over video.
- The runner only starts after `MpvItem.renderReady` fires (emitted once when the
  render context is created). Loading a file before the context exists makes mpv
  come up with no video output (`vid=no`, `dwidth=None`).

**The overlay is full-viewport, anchored to the top** (`frontend/`,
`.overlay-root` / `.controlbar`). A
`position:fixed; bottom:0` element never composited over live video; a
`position:fixed; inset:0` container does — so the React root spans the viewport
and the always-on control bar sits in the **top** strip. The bundle's
`index.html` is injected with `loadHtml(html, baseUrl)` (not `url:`): a
`url:`-loaded page only composited its top band over actively-rendering video,
while `loadHtml` composites the whole viewport. `baseUrl` is the control server,
so the bundle's relative `./assets/*` and its `fetch('/status')` resolve
same-origin.

**Adaptive UI.** A thin control bar (phase + now-playing + pause/skip/defer) is the
only chrome shown over live video; the full dashboard (sidebar of active shows +
KPIs + round/just-finished tables) appears only when *not* playing
(auth/fetching/drained/error), where there's no video to occlude. The React root
background is transparent so video shows through the gaps.

**Control surface, not QWebChannel** (`shows/webserver.py`): a localhost
`ThreadingHTTPServer` serves the overlay and a `GET /status` + `POST /pause` +
`POST /skip` + `POST /defer` surface that the overlay polls **same-origin** (the
WebEngineView's `baseUrl` is the control server). QWebChannel was abandoned: under PySide6 a
`QWebChannel` can't be assigned to a QML `WebEngineView`'s `QQmlWebChannel`-typed
`webChannel` property, and `registerObject` isn't QML-callable. The HTTP surface
also doubles as the external debug endpoint.

**Auth** (`shows/oauth.py`): an RFC 8252 user-login flow —
binds `127.0.0.1:0`, opens the browser at `/api/auth/cli/user-login`, receives a
one-time `?code=` on the loopback, and POSTs it + the PKCE `code_verifier` to
`/api/auth/cli/user-token`. The JWT never travels through the browser. Token
cache carries a `version` field (`CACHE_VERSION`) so the shape can evolve.

**Playback loop** (`shows/runner.py`): offline-first — the desktop is the engine.
Each round is computed locally from the SQLite replica (`engine.next_round`): one
episode per active show, in the deterministic SHA-256-of-path order
(`shows/ordering.py`, bit-identical to the legacy scheme). The runner queues all
paths into mpv, then advances **per episode** — the instant a file plays to its
natural end it's marked watched in the replica (contract A1), so closing partway
through a round keeps exactly what you watched and never marks what you didn't.
The Syncer pushes those local changes to the origin (`POST /sync`) at round end
and on window close; playback never blocks on the network.

Two interactive controls arrive on the control-server thread and act on the
entry mpv is currently playing (tracked via the player's `playlist-pos`):
**skip** (`n`) marks that episode watched immediately (per-episode skip, I7) and
jumps forward; **defer** (`d`) re-rolls the show's next pick *without* marking it
watched (D1-D3) and jumps forward — its forced, non-natural end means it isn't
advanced. Set `SHOWS_PLAYLISTS=a,b,c` to round-robin across several playlists at
once; one playlist is the default.

## Package

```powershell
pip install -r requirements.txt pyinstaller
pushd ..\desktop\frontend; npm ci; npm run build; popd   # overlay must be built first
pyinstaller shows-qt.spec --noconfirm                    # -> dist/shows-qt/
```

`shows-qt.spec` is a **onedir** build (PySide6 + WebEngine don't survive
onefile's temp extraction reliably). It bundles `libmpv-2.dll` and the React
`dist` into the frozen tree; `main.py` resolves both from the PyInstaller
unpack dir when `sys.frozen`. The spec finds `libmpv-2.dll` via
`$SHOWS_LIBMPV_DIR`, a `third_party/libmpv` dir, or the `mpv.net` install.
`.github/workflows/build-desktop.yaml` runs this on a Windows runner and
publishes the zipped bundle as a GitHub Release on push to main.

## Install (latest Release)

Push to main publishes the packaged bundle as a GitHub Release (tag
`desktop-<short-sha>`, marked `--latest`). **Fetching and unpacking that build
is the assistant's chore, not the user's** — when a new build needs installing
or smoke-testing, the assistant pulls it onto the machine from the session
rather than asking the user to click through the Releases page:

```powershell
gh release download --pattern 'shows-qt-windows-amd64.zip' --dir $env:TEMP\shows-dl --clobber
Expand-Archive $env:TEMP\shows-dl\shows-qt-windows-amd64.zip -DestinationPath $env:LOCALAPPDATA\shows-qt -Force
# then launch:  & "$env:LOCALAPPDATA\shows-qt\shows-qt.exe"
```

The user's only manual step is watching the running app behave (the live Qt/mpv
window) — never the download, unpack, or launch.

## Status

Works end-to-end (verified from source and as a packaged exe): cached auth →
runner → mpv video composited under the React overlay, driven by `/status` +
`/shows`, with working pause/skip. Adaptive UI (control bar over video, full
dashboard between rounds), PyInstaller packaging, and CI are done. This is the
only desktop client — the earlier Go/Wails build has been removed.
