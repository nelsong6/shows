"""Localhost control server backing the web overlay. Replaces QWebChannel
(which is a morass to wire into a QML WebEngineView under PySide6) with a
plain same-origin HTTP surface — the React overlay bundle is served from
here, polls /status and /shows, and POSTs /pause, /skip, and /defer. Also
doubles as the external debug endpoint.
"""

from __future__ import annotations

import dataclasses
import http.server
import json
import logging
import mimetypes
import os
import threading
import urllib.parse
from typing import Callable, Optional

from . import scan
from .player import Player

log = logging.getLogger("shows.webserver")


def _jsonable(v):
    if dataclasses.is_dataclass(v):
        return dataclasses.asdict(v)
    if isinstance(v, list):
        return [_jsonable(x) for x in v]
    return v


class ControlServer:
    """Serves the React overlay bundle + a same-origin control surface.

    `dist_dir` is the built overlay (frontend/dist) and is required — there
    is no placeholder fallback; build the frontend first. `shows_provider`
    backs GET /shows and `history_provider` backs GET /history?show=<id>;
    both are called on the HTTP handler thread, so they must be thread-safe
    (the apiclient is).
    """

    def __init__(
        self,
        dist_dir: str,
        shows_provider: Optional[Callable[[], list]] = None,
        history_provider: Optional[Callable[[str], list]] = None,
        stats_provider: Optional[Callable[[], dict]] = None,
    ):
        if not dist_dir or not os.path.isdir(dist_dir):
            raise FileNotFoundError(
                f"overlay bundle not found at {dist_dir!r}; build it with "
                "`npm run build` in frontend/"
            )
        self._status = {"phase": "initializing", "message": "starting up",
                        "playlist": "nelson", "round": [], "round_pos": 0}
        self._lock = threading.Lock()
        self._player: Optional[Player] = None
        self._on_skip: Optional[Callable[[], None]] = None
        self._on_defer: Optional[Callable[[], None]] = None
        self._on_fullscreen: Optional[Callable[[], None]] = None
        self._syncer = None  # set after the runner is built; backs /sync-now + status
        self._replica = None  # set for /library/* management endpoints
        self._httpd: Optional[http.server.HTTPServer] = None
        self.port = 0
        self._dist = dist_dir
        self._shows_provider = shows_provider
        self._history_provider = history_provider
        self._stats_provider = stats_provider

    def set_player(self, player: Player) -> None:
        self._player = player

    def set_command_handlers(
        self,
        skip: Optional[Callable[[], None]] = None,
        defer: Optional[Callable[[], None]] = None,
        fullscreen: Optional[Callable[[], None]] = None,
    ) -> None:
        """Wire POST /skip, /defer, and /fullscreen. Set after the runner exists
        (it's built once mpv's render context is ready). `fullscreen` toggles the
        Qt window; it must marshal onto the Qt thread itself (see main.py)."""
        if skip is not None:
            self._on_skip = skip
        if defer is not None:
            self._on_defer = defer
        if fullscreen is not None:
            self._on_fullscreen = fullscreen

    def set_syncer(self, syncer) -> None:
        """Wire the Syncer so /status reports online/pending and POST /sync-now
        triggers a manual reconcile (the 'check connectivity' button)."""
        self._syncer = syncer

    def set_library(self, replica) -> None:
        """Wire the replica for the /library/* management endpoints (add show by
        scanning a directory, remove/edit, rescan for new episodes)."""
        self._replica = replica

    def set_shows_provider(self, fn: Callable[[], list]) -> None:
        self._shows_provider = fn

    def set_history_provider(self, fn: Callable[[str], list]) -> None:
        self._history_provider = fn

    def push(self, **kw) -> None:
        with self._lock:
            self._status.update({k: _jsonable(v) for k, v in kw.items()})

    def _status_json(self) -> bytes:
        with self._lock:
            status = dict(self._status)
        # Live playback state (position/duration/volume/tracks) for the scrub
        # bar + menus — read fresh from mpv on each poll, merged under "playback".
        if self._player is not None:
            try:
                status["playback"] = self._player.playback_state()
            except Exception as e:  # noqa: BLE001 — keep /status serving
                log.warning("playback_state failed: %s", e)
        # Sync state for the offline indicator: online + unpushed-change count.
        if self._syncer is not None:
            status["sync"] = {"online": self._syncer.online, "pending": self._syncer.pending()}
        return json.dumps(status).encode("utf-8")

    def index_html(self) -> bytes:
        """The overlay document — the built React index.html. main.py feeds
        this to the QML WebEngineView's loadHtml() with the control server as
        baseUrl, so the bundle's relative ./assets/* and its fetch('/status')
        etc. all resolve same-origin to this server."""
        with open(os.path.join(self._dist, "index.html"), "rb") as f:
            return f.read()

    def _static_file(self, rel: str) -> Optional[tuple[bytes, str]]:
        """Read a file from the dist dir, guarding against traversal.
        Returns (bytes, content-type) or None if absent/out of bounds."""
        rel = rel.lstrip("/").split("?", 1)[0]
        full = os.path.normpath(os.path.join(self._dist, rel))
        if os.path.commonpath([full, self._dist]) != os.path.normpath(self._dist):
            return None
        try:
            with open(full, "rb") as f:
                data = f.read()
        except OSError:
            return None
        ctype = mimetypes.guess_type(full)[0] or "application/octet-stream"
        return data, ctype

    def start(self) -> int:
        srv = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def _send(self, code, body=b"", ctype="text/plain"):
                self.send_response(code)
                self.send_header("Content-Type", ctype)
                self.send_header("Content-Length", str(len(body)))
                self.send_header("Cache-Control", "no-store")
                self.end_headers()
                if body:
                    self.wfile.write(body)

            def do_GET(self):
                if self.path == "/" or self.path.startswith("/index"):
                    self._send(200, srv.index_html(), "text/html; charset=utf-8")
                elif self.path == "/status":
                    self._send(200, srv._status_json(), "application/json")
                elif self.path == "/shows":
                    self._send(200, srv._shows_json(), "application/json")
                elif self.path == "/stats":
                    self._send(200, srv._stats_json(), "application/json")
                elif self.path.startswith("/history"):
                    q = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)
                    self._send(200, srv._history_json(q.get("show", [""])[0]), "application/json")
                elif self.path == "/health":
                    self._send(200, b"ok")
                else:
                    served = srv._static_file(self.path)
                    if served is not None:
                        self._send(200, served[0], served[1])
                    else:
                        self._send(404, b"not found")

            def _json_body(self):
                try:
                    n = int(self.headers.get("Content-Length", 0) or 0)
                    raw = self.rfile.read(n) if n else b""
                    return json.loads(raw) if raw else {}
                except Exception:
                    return {}

            def do_POST(self):
                p = srv._player
                if self.path == "/pause" and p:
                    p.toggle_pause()
                    self._send(204)
                elif self.path == "/sync-now" and srv._syncer is not None:
                    # Manual "check connectivity"/reconcile. Run off-thread so the
                    # response is instant; the overlay sees the result via /status.
                    threading.Thread(target=srv._syncer.sync, name="sync-now", daemon=True).start()
                    self._send(204)
                elif self.path == "/skip" and srv._on_skip:
                    srv._on_skip()
                    self._send(204)
                elif self.path == "/defer" and srv._on_defer:
                    srv._on_defer()
                    self._send(204)
                elif self.path == "/fullscreen" and srv._on_fullscreen:
                    srv._on_fullscreen()
                    self._send(204)
                elif self.path == "/seek" and p:
                    b = self._json_body()
                    if "percent" in b:
                        p.seek_percent(float(b["percent"]))
                    elif "seconds" in b:
                        p.seek_relative(float(b["seconds"]))
                    self._send(204)
                elif self.path == "/volume" and p:
                    b = self._json_body()
                    if "volume" in b:
                        p.set_volume(float(b["volume"]))
                    self._send(204)
                elif self.path == "/sub" and p:
                    p.set_sub(self._json_body().get("sid", "no"))
                    self._send(204)
                elif self.path == "/audio" and p:
                    b = self._json_body()
                    if "aid" in b:
                        p.set_audio(b["aid"])
                    self._send(204)
                elif self.path == "/library/add" and srv._replica is not None:
                    self._library_add()
                elif self.path == "/library/remove" and srv._replica is not None:
                    srv._replica.remove_show(self._json_body().get("show_id", ""))
                    self._push()
                    self._send(204)
                elif self.path == "/library/update" and srv._replica is not None:
                    b = self._json_body()
                    srv._replica.update_show(
                        b.get("show_id", ""), name=b.get("name"),
                        root_path=b.get("root_path"), playlist=b.get("playlist"),
                    )
                    self._push()
                    self._send(204)
                elif self.path == "/library/rescan" and srv._replica is not None:
                    self._library_rescan()
                else:
                    self._send(404, b"not found")

            def _push(self):
                if srv._syncer is not None:
                    srv._syncer.push()

            def _library_add(self):
                b = self._json_body()
                name = (b.get("name") or "").strip()
                root = (b.get("root_path") or "").strip()
                playlist = (b.get("playlist") or "nelson").strip()
                if not name or not root:
                    self._send(400, b'{"error":"name and root_path are required"}', "application/json")
                    return
                eps = scan.scan_episodes(root)
                if not eps:
                    self._send(400, b'{"error":"no video files found under root_path"}', "application/json")
                    return
                sid = srv._replica.create_show(playlist, name, root, eps)
                self._push()
                self._send(200, json.dumps({"id": sid, "episodes": len(eps)}).encode("utf-8"), "application/json")

            def _library_rescan(self):
                sid = self._json_body().get("show_id", "")
                sh = srv._replica.show(sid)
                added = 0
                if sh is not None:
                    known = srv._replica.episode_paths(sid)
                    new = [f for f in scan.scan_episodes(sh.root_path) if f not in known]
                    added = srv._replica.add_episodes(sid, new)
                    if added:
                        self._push()
                self._send(200, json.dumps({"added": added}).encode("utf-8"), "application/json")

        self._httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.port = self._httpd.server_address[1]
        threading.Thread(target=self._httpd.serve_forever, name="control-server", daemon=True).start()
        return self.port

    def _shows_json(self) -> bytes:
        if self._shows_provider is None:
            return b"[]"
        try:
            shows = self._shows_provider()
        except Exception as e:  # noqa: BLE001 — surface as empty, log it
            log.warning("shows provider failed: %s", e)
            return b"[]"
        return json.dumps([_jsonable(s) for s in shows]).encode("utf-8")

    def _stats_json(self) -> bytes:
        if self._stats_provider is None:
            return b"{}"
        try:
            return json.dumps(self._stats_provider()).encode("utf-8")
        except Exception as e:  # noqa: BLE001 — surface as empty, log it
            log.warning("stats provider failed: %s", e)
            return b"{}"

    def _history_json(self, show_id: str) -> bytes:
        if self._history_provider is None or not show_id:
            return b"[]"
        try:
            events = self._history_provider(show_id)
        except Exception as e:  # noqa: BLE001 — surface as empty, log it
            log.warning("history provider failed: %s", e)
            return b"[]"
        return json.dumps([_jsonable(ev) for ev in events]).encode("utf-8")
