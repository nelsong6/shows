"""Localhost control server backing the web overlay. Replaces QWebChannel
(which is a morass to wire into a QML WebEngineView under PySide6) with a
plain same-origin HTTP surface — the overlay is served from here, polls
/status, and POSTs /pause and /skip. Also doubles as the external debug
endpoint (like the Go build's /status server).
"""

from __future__ import annotations

import dataclasses
import http.server
import json
import threading
from typing import Optional

from .player import Player


def _jsonable(v):
    if dataclasses.is_dataclass(v):
        return dataclasses.asdict(v)
    if isinstance(v, list):
        return [_jsonable(x) for x in v]
    return v


OVERLAY_HTML = """<!doctype html><html><head><meta charset='utf-8'>
<style>
  body{margin:0;background:transparent;font-family:Consolas,monospace;color:#eee;overflow:hidden;}
  /* A fixed inset:0 container composites reliably over the live video
     layer (position:fixed;bottom:0 alone did not). Flex-column pushes the
     control bar to the bottom. */
  #root{position:fixed;inset:0;display:flex;flex-direction:column;pointer-events:none;}
  #spacer{flex:1;}
  /* Bar lives at the TOP: content anchored to the top of the viewport
     composites reliably over the live video layer. */
  #bar{order:-1;display:flex;align-items:center;gap:18px;padding:14px 18px;
       background:rgba(10,10,16,0.78);pointer-events:auto;}
  #now{flex:1;font-size:15px;}
  #phase{color:#8c8;text-transform:uppercase;letter-spacing:.1em;font-size:12px;min-width:72px;}
  button{background:rgba(255,255,255,.10);color:#eee;border:1px solid #666;border-radius:4px;
         padding:7px 16px;font:inherit;cursor:pointer;}
  button:hover{background:rgba(255,255,255,.20);}
</style></head>
<body>
  <div id='root'>
    <div id='spacer'></div>
    <div id='bar'>
      <span id='phase'>…</span>
      <span id='now'>—</span>
      <button id='pause'>pause / play</button>
      <button id='skip'>skip</button>
    </div>
  </div>
<script>
async function poll(){
  try{
    const s = await (await fetch('/status')).json();
    document.getElementById('phase').textContent = s.phase || '';
    const r = s.round || [];
    document.getElementById('now').textContent = r.length
      ? (r[0].show_name + '   (1/' + r.length + ')')
      : (s.message || '—');
  }catch(e){}
}
document.getElementById('pause').onclick = () => fetch('/pause', {method:'POST'});
document.getElementById('skip').onclick  = () => fetch('/skip',  {method:'POST'});
poll(); setInterval(poll, 700);
</script>
</body></html>"""


class ControlServer:
    def __init__(self):
        self._status = {"phase": "initializing", "message": "starting up", "playlist": "nelson", "round": []}
        self._lock = threading.Lock()
        self._player: Optional[Player] = None
        self._httpd: Optional[http.server.HTTPServer] = None
        self.port = 0

    def set_player(self, player: Player) -> None:
        self._player = player

    def push(self, **kw) -> None:
        with self._lock:
            self._status.update({k: _jsonable(v) for k, v in kw.items()})

    def _status_json(self) -> bytes:
        with self._lock:
            return json.dumps(self._status).encode("utf-8")

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
                    self._send(200, OVERLAY_HTML.encode("utf-8"), "text/html; charset=utf-8")
                elif self.path == "/status":
                    self._send(200, srv._status_json(), "application/json")
                elif self.path == "/health":
                    self._send(200, b"ok")
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
