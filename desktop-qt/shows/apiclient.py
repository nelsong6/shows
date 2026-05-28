"""shows.romaine.life HTTP client — Python port of
desktop/internal/apiclient/client.go. Threads a bearer JWT through every
call; on 401 it invokes a refresh hook once and retries (the in-place
token refresh the Go client does)."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Callable, Optional

import httpx

DEFAULT_BASE_URL = "https://shows.romaine.life"


@dataclass
class RoundEntry:
    show_id: str
    show_name: str
    episode_id: str
    absolute_path: str
    order_value: int


@dataclass
class Show:
    id: str
    playlist: str
    name: str
    root_path: str
    date_added: str
    removed_at: Optional[str] = None


@dataclass
class AdvanceEntry:
    show_id: str
    episode_id: str


@dataclass
class RemovedShow:
    id: str
    name: str
    date_added: str
    last_played_at: str


@dataclass
class HistoryEvent:
    episode_id: str
    relative_path: str
    played_at: str


@dataclass
class AdvanceResult:
    advanced_count: int = 0
    removed_shows: list[RemovedShow] = field(default_factory=list)


class APIError(Exception):
    pass


def _only(cls, d: dict):
    """Build a dataclass from a dict, ignoring unknown keys so a server-side
    field addition doesn't crash the client."""
    known = {f.name for f in cls.__dataclass_fields__.values()}
    return cls(**{k: v for k, v in d.items() if k in known})


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

    def next_round(self, playlist: str) -> list[RoundEntry]:
        data = self._do("GET", f"/api/playlists/{playlist}/next-round").json()
        return [_only(RoundEntry, e) for e in (data.get("round") or [])]

    def list_active_shows(self, playlist: str) -> list[Show]:
        data = self._do("GET", f"/api/playlists/{playlist}").json()
        return [_only(Show, s) for s in (data.get("shows") or [])]

    def show_history(self, show_id: str) -> list[HistoryEvent]:
        data = self._do("GET", f"/api/shows/{show_id}/history").json()
        return [_only(HistoryEvent, e) for e in (data.get("history") or [])]

    def advance(self, playlist: str, entries: list[AdvanceEntry]) -> AdvanceResult:
        if not entries:
            return AdvanceResult()
        body = {"entries": [{"show_id": e.show_id, "episode_id": e.episode_id} for e in entries]}
        data = self._do("POST", f"/api/playlists/{playlist}/advance", body).json()
        return AdvanceResult(
            advanced_count=data.get("advanced_count", 0),
            removed_shows=[_only(RemovedShow, r) for r in (data.get("removed_shows") or [])],
        )
