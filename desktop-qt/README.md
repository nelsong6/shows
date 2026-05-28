# shows-desktop (Qt)

The desktop client rebuilt on **PySide6 + libmpv's render API**, replacing the
Go/Wails build in [`../desktop`](../desktop). One composited window: mpv video
on the bottom layer, a transparent web overlay drawing the chrome on top — the
[Jellyfin Media Player](https://github.com/jellyfin/jellyfin-media-player)
architecture. Runs against [shows.romaine.life](https://shows.romaine.life) for
round-robin scheduling, watch history, and library state.

## Why this exists

The Go build embedded mpv as a child HWND via `--wid`. On Windows a transparent
WebView2 composites only over its **host window's background**, never over a
**sibling** child HWND — so the web chrome could never sit on top of playing
video. (A spike confirmed this directly; see PR #16.) The fix is to stop using a
separate video window at all: mpv renders into an OpenGL FBO that the Qt scene
graph composites, with a transparent `WebEngineView` layered above it in the
same QML tree. Qt owns the compositing, so the overlay reliably draws over live
video.

## Run

```powershell
pip install PySide6 python-mpv httpx
python main.py
```

`main.py` locates `libmpv-2.dll` at startup, in order: `$SHOWS_LIBMPV_DIR`, the
Go build's bundled DLL (`../desktop/build/bin`), then a scoop mpv install
(`%USERPROFILE%\scoop\apps\mpv\current`). On a machine with none of those, point
`SHOWS_LIBMPV_DIR` at any folder containing `libmpv-2.dll` (or `mpv-2.dll`).

First launch opens a browser at auth.romaine.life for the normal Microsoft/Google
sign-in; the resulting user JWT caches at `%APPDATA%\shows\token.json` and
refreshes silently on 401. The cache is shared with the Go build — copy that file
to a new machine to skip the browser login.

## Layout

```
desktop-qt/
  main.py                # entry: GL setup, QML window, runner wiring
  shows/
    mpv_item.py          # MpvItem(QQuickFramebufferObject): mpv render API → FBO
    webserver.py         # ControlServer: serves overlay + /status /pause /skip
    oauth.py             # auth.romaine.life PKCE + loopback user-login, cached token
    apiclient.py         # shows.romaine.life HTTP client with 401 refresh
    runner.py            # round-robin runner: fetch → queue → wait → advance
    player.py            # python-mpv handle wrapper (play/pause/skip/show_text)
    ordering.py          # SHA-256 path ordering, bit-identical to the server
```

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

**The overlay is full-viewport, not a bottom bar** (`shows/webserver.py`,
`OVERLAY_HTML`). A `position:fixed; bottom:0` element never composited over live
video; a `position:fixed; inset:0` container does. So the page fills the
viewport and the control bar is pushed to the **top** strip via flex `order:-1`.
The page is injected with `loadHtml(html, baseUrl)` (not `url:`) — a `url:`-loaded
page only composited its top band over actively-rendering video, while
`loadHtml` composites the whole viewport.

**Control surface, not QWebChannel** (`shows/webserver.py`): a localhost
`ThreadingHTTPServer` serves the overlay and a `GET /status` + `POST /pause` +
`POST /skip` surface that the overlay polls **same-origin** (the WebEngineView's
`baseUrl` is the control server). QWebChannel was abandoned: under PySide6 a
`QWebChannel` can't be assigned to a QML `WebEngineView`'s `QQmlWebChannel`-typed
`webChannel` property, and `registerObject` isn't QML-callable. The HTTP surface
also doubles as the external debug endpoint (like the Go build's `/status`).

**Auth** (`shows/oauth.py`): the same RFC 8252 user-login flow as the Go build —
binds `127.0.0.1:0`, opens the browser at `/api/auth/cli/user-login`, receives a
one-time `?code=` on the loopback, and POSTs it + the PKCE `code_verifier` to
`/api/auth/cli/user-token`. The JWT never travels through the browser. Token
cache carries a `version` field (`CACHE_VERSION`) so the shape can evolve.

**Playback loop** (`shows/runner.py`): identical contract to the Go runner —
`GET …/next-round` → queue all paths into mpv → wait for N end-of-file events →
`POST …/advance` → `playlist-clear` → loop, with retry-and-backoff on transient
failures and a token refresh + retry on 401. Ordering (`shows/ordering.py`)
reproduces the server's SHA-256-of-path scheme bit-for-bit.

## Status

Core works end-to-end: cached auth → runner → mpv video + a live overlay
(phase + current show, working pause/skip). Remaining (tracked on GitHub):

- The HTML overlay is a **placeholder**. The next step swaps in the built React
  frontend (`../desktop/frontend`) served from the control server over the same
  `/status` + `/pause` + `/skip` surface, with a transparent root.
- **PyInstaller packaging + CI**, then retire the Go [`../desktop`](../desktop)
  once this reaches parity.
