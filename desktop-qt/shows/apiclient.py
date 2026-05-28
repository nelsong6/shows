"""shows.romaine.life HTTP client — the desktop's link to the durable origin
under the offline-first design. The whole conversation is two calls: get_library
(pull the library to seed/reconcile the replica) and post_sync (push locally-
changed records, last-write-wins). Threads a bearer JWT through every request; on
401 it invokes a refresh hook once and retries (the in-place token refresh the Go
client does)."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Callable, Optional
from urllib.parse import quote

import httpx

DEFAULT_BASE_URL = "https://shows.romaine.life"


@dataclass
class RoundEntry:
    show_id: str
    show_name: str
    episode_id: str
    absolute_path: str
    order_value: int
    playlist: str = ""  # the entry's playlist, carried for skip/defer routing


@dataclass
class RemovedShow:
    id: str
    name: str
    date_added: str
    last_played_at: str


@dataclass
class AdvanceResult:
    advanced_count: int = 0
    removed_shows: list[RemovedShow] = field(default_factory=list)


class APIError(Exception):
    pass


class Client:
    def __init__(
        self,
        token: str,
        base_url: str = DEFAULT_BASE_URL,
        refresh_token: Optional[Callable[[], str]] = None,
    ):
        self.base_url = base_url or DEFAULT_BASE_URL
        self.token = token
        self.refresh_token = refresh_token
        self._http = httpx.Client(timeout=30.0)

    def close(self):
        self._http.close()

    def _send(self, method, path, json_body, token):
        return self._http.request(
            method,
            self.base_url + path,
            json=json_body,
            headers={"Authorization": f"Bearer {token}"},
        )

    def _do(self, method, path, json_body=None):
        resp = self._send(method, path, json_body, self.token)
        # 401 -> refresh once, retry. Persistent 401 surfaces as APIError.
        if resp.status_code == 401 and self.refresh_token is not None:
            self.token = self.refresh_token()
            resp = self._send(method, path, json_body, self.token)
        if resp.status_code >= 300:
            raise APIError(f"{method} {path}: {resp.status_code} {resp.text.strip()}")
        return resp

    # ── offline sync (library pull + record push) ──────────────────────
    def get_library(self, playlists: list[str]) -> list[dict]:
        """Pull the full library (shows + embedded episodes, incl. removed) for
        seeding/reconciling the local replica. Returns raw dicts for
        Replica.merge_shows."""
        q = ",".join(playlists)
        data = self._do("GET", f"/api/library?playlists={quote(q)}").json()
        return data.get("shows") or []

    def post_sync(self, shows: list[dict], episodes: list[dict], history: list[dict]) -> None:
        """Push locally-changed records; the server upserts last-write-wins.
        Caller passes the replica's dirty rows (already shaped to the wire
        contract). 204 on success."""
        self._do("POST", "/api/sync",
                 {"shows": shows, "episodes": episodes, "history": history})
