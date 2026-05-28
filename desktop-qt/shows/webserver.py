"""Localhost control server backing the web overlay. Replaces QWebChannel
(which is a morass to wire into a QML WebEngineView under PySide6) with a
plain same-origin HTTP surface — the React overlay bundle is served from
here, polls /status and /shows, and POSTs /pause and /skip. Also doubles as
the external debug endpoint.
"""

from __future__ import annotations

import dataclasses
import http.server
import json
import logging
import mimetypes
import os
import threading
from typing import Callable, Optional

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
    backs GET /shows; it's called on the HTTP handler thread, so it must be
    thread-safe (the apiclient is).
    """

    def __init__(
        self,
        dist_dir: str,
        shows_provider: Optional[Callable[[], list]] = None,
    ):
        if not dist_dir or not os.path.isdir(dist_dir):
            raise FileNotFoundError(
                f"overlay bundle not found at {dist_dir!r}; build it with "
                "`npm run build` in frontend/"
            )
        self._status = {"phase": "initializing", "message": "starting up", "playlist": "nelson", "round": []}
        self._lock = threading.Lock()
        self._player: Optional[Player] = None
        self._httpd: Optional[http.server.HTTPServer] = None
        self.port = 0
        self._dist = dist_dir
        self._shows_provider = shows_provider

    def set_player(self, player: Player) -> None:
        self._player = player

    def set_shows_provider(self, fn: Callable[[], list]) -> None:
        self._shows_provider = fn

    def push(self, **kw) -> None:
        with self._lock:
            self._status.update({k: _jsonable(v) for k, v in kw.items()})

    def _status_json(self) -> bytes:
        with self._lock:
            return json.dumps(self._status).encode("utf-8")

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
                elif self.path == "/health":
                    self._send(200, b"ok")
                else:
                    served = srv._static_file(self.path)
                    if served is not None:
                        self._send(200, served[0], served[1])
                    else:
                        self._send(404, b"not found")

            def do_POST(self):
                if self.path == "/pause" and srv._player:
                    srv._player.toggle_pause()
                    self._send(204)
                elif self.path == "/skip" and srv._player:
                    srv._player.skip()
                    self._send(204)
                else:
                    self._send(404, b"not found")

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
