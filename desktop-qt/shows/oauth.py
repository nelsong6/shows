"""auth.romaine.life user-login flow — Python port of
desktop/internal/oauth/loopback.go. PKCE + loopback: open the browser at
/api/auth/cli/user-login, catch the one-time code on a localhost
listener, exchange it at /api/auth/cli/user-token for the user's JWT.

Shares the cache shape and location with the Go app
(%APPDATA%\\shows\\token.json, {version, token, expires_at}) so the two
can warm each other's cache; cache_version gates cross-generation reuse.
"""

from __future__ import annotations

import base64
import hashlib
import http.server
import json
import os
import secrets
import threading
import time
import urllib.parse
import webbrowser
from dataclasses import dataclass
from typing import Callable, Optional

import httpx

DEFAULT_AUTH_BASE_URL = "https://auth.romaine.life"
LOGIN_ENDPOINT = "/api/auth/cli/user-login"
TOKEN_ENDPOINT = "/api/auth/cli/user-token"

# Must match desktop/internal/oauth cacheVersion so a token written by
# either the Go or the Python build is accepted by the other.
CACHE_VERSION = 1


@dataclass
class Token:
    token: str
    expires_at: int
    version: int = CACHE_VERSION

    def expired(self) -> bool:
        if not self.token:
            return True
        return time.time() + 60 >= self.expires_at


def _b64url(raw: bytes) -> str:
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode("ascii")


def _pkce_pair() -> tuple[str, str]:
    verifier = _b64url(secrets.token_bytes(32))
    challenge = _b64url(hashlib.sha256(verifier.encode("ascii")).digest())
    return verifier, challenge


def cache_path() -> str:
    base = os.environ.get("APPDATA") or os.path.expanduser("~/.config")
    return os.path.join(base, "shows", "token.json")


def load_cached_token() -> Optional[Token]:
    path = cache_path()
    try:
        with open(path, "r", encoding="utf-8") as f:
            d = json.load(f)
    except FileNotFoundError:
        return None
    except (json.JSONDecodeError, OSError):
        # Corrupt cache -> treat as none; next save overwrites.
        return None
    if d.get("version") != CACHE_VERSION:
        return None
    return Token(token=d.get("token", ""), expires_at=int(d.get("expires_at", 0)),
                 version=int(d.get("version", 0)))


def save_token(tok: Token) -> None:
    path = cache_path()
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        json.dump({"version": tok.version, "token": tok.token, "expires_at": tok.expires_at}, f, indent=2)


def _build_login_url(base_url, redirect_uri, challenge, state) -> str:
    q = urllib.parse.urlencode({
        "redirect_uri": redirect_uri,
        "code_challenge": challenge,
        "code_challenge_method": "S256",
        "state": state,
    })
    return f"{base_url}{LOGIN_ENDPOINT}?{q}"


def authenticate(
    auth_base_url: str = DEFAULT_AUTH_BASE_URL,
    opener: Optional[Callable[[str], None]] = None,
    timeout_s: float = 600.0,
) -> Token:
    verifier, challenge = _pkce_pair()
    state = _b64url(secrets.token_bytes(24))

    result: dict[str, str] = {}
    done = threading.Event()

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *a):  # silence
            pass

        def do_GET(self):
            parsed = urllib.parse.urlparse(self.path)
            if parsed.path != "/callback":
                self.send_error(404)
                return
            qs = urllib.parse.parse_qs(parsed.query)
            if qs.get("state", [""])[0] != state:
                self.send_response(400); self.end_headers()
                self.wfile.write(b"state mismatch")
                result["error"] = "state mismatch on callback"
                done.set()
                return
            code = qs.get("code", [""])[0]
            if not code:
                self.send_response(400); self.end_headers()
                self.wfile.write(b"missing code")
                result["error"] = "callback missing code"
                done.set()
                return
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.end_headers()
            self.wfile.write(b"<!doctype html><body style='background:#0a0a0a;color:#eee;"
                             b"font-family:monospace;padding:32px'><h2>shows: signed in</h2>"
                             b"<p>You can close this tab.</p></body>")
            result["code"] = code
            done.set()

    srv = http.server.HTTPServer(("127.0.0.1", 0), Handler)
    port = srv.server_address[1]
    redirect_uri = f"http://127.0.0.1:{port}/callback"
    t = threading.Thread(target=srv.serve_forever, daemon=True)
    t.start()
    try:
        url = _build_login_url(auth_base_url, redirect_uri, challenge, state)
        (opener or webbrowser.open)(url)
        if not done.wait(timeout_s):
            raise TimeoutError("oauth: sign-in window timed out")
        if "error" in result:
            raise RuntimeError(f"oauth: {result['error']}")
        return _exchange_code(auth_base_url, result["code"], verifier, redirect_uri)
    finally:
        srv.shutdown()


def _exchange_code(auth_base_url, code, verifier, redirect_uri) -> Token:
    resp = httpx.post(
        f"{auth_base_url}{TOKEN_ENDPOINT}",
        json={
            "grant_type": "authorization_code",
            "code": code,
            "code_verifier": verifier,
            "redirect_uri": redirect_uri,
        },
        timeout=30.0,
    )
    data = resp.json()
    if data.get("token"):
        return Token(token=data["token"], expires_at=int(data["expires_at"]))
    err = data.get("error", f"token exchange returned {resp.status_code}")
    desc = data.get("error_description")
    raise RuntimeError(f"oauth: {err}{': ' + desc if desc else ''}")


def ensure_token(
    auth_base_url: str = DEFAULT_AUTH_BASE_URL,
    opener: Optional[Callable[[str], None]] = None,
) -> Token:
    cached = load_cached_token()
    if cached is not None and not cached.expired():
        return cached
    tok = authenticate(auth_base_url, opener)
    save_token(tok)
    return tok
