"""Syncer tests — git-style push/pull + connectivity inference, via an httpx
MockTransport client and an in-memory replica."""

import json

import httpx

from shows.apiclient import Client
from shows.replica import Replica
from shows.sync import Syncer

T0 = "2026-01-01T00:00:00Z"
LIB = [{
    "id": "s1", "playlist": "nelson", "name": "S1", "root_path": r"D:\A", "updated_at": T0,
    "episodes": [
        {"id": "a", "relative_path": "a.mkv", "position": 0, "updated_at": T0},
        {"id": "b", "relative_path": "b.mkv", "position": 1, "updated_at": T0},
    ],
}]


def _client(handler):
    c = Client(token="tok", base_url="https://x.test")
    c._http = httpx.Client(transport=httpx.MockTransport(handler))
    return c


def test_seed_pulls_into_replica():
    def h(req):
        assert req.url.path == "/api/library"
        assert req.url.params.get("playlists") == "nelson"
        return httpx.Response(200, json={"shows": LIB})

    r = Replica(":memory:")
    s = Syncer(r, _client(h), ["nelson"])
    assert s.seed() is True and s.online
    assert {sh.id for sh in r.active_shows(["nelson"])} == {"s1"}


def test_seed_failure_goes_offline():
    r = Replica(":memory:")
    s = Syncer(r, _client(lambda req: httpx.Response(500, text="boom")), ["nelson"])
    assert s.seed() is False and not s.online


def test_push_sends_dirty_stripped_then_clears():
    seen = {}

    def h(req):
        if req.url.path == "/api/library":
            return httpx.Response(200, json={"shows": LIB})
        if req.url.path == "/api/sync":
            seen["body"] = json.loads(req.content)
            return httpx.Response(204)
        return httpx.Response(404)

    r = Replica(":memory:")
    s = Syncer(r, _client(h), ["nelson"])
    s.seed()
    r.advance([("s1", "a")])  # episode a watched + a history row (s1 not drained: b remains)
    assert s.pending() == 2
    assert s.push() is True
    assert s.pending() == 0
    body = seen["body"]
    assert any(e["id"] == "a" and e.get("watched_at") for e in body["episodes"])
    assert len(body["history"]) == 1
    assert "dirty" not in body["episodes"][0]  # local-only flag stripped from the wire


def test_push_failure_keeps_changes_and_goes_offline():
    def h(req):
        if req.url.path == "/api/library":
            return httpx.Response(200, json={"shows": LIB})
        return httpx.Response(503)

    r = Replica(":memory:")
    s = Syncer(r, _client(h), ["nelson"])
    s.seed()
    r.advance([("s1", "a")])
    before = s.pending()
    assert s.push() is False and not s.online
    assert s.pending() == before  # nothing lost — stays queued for next time


def test_push_noop_when_clean_stays_online():
    s = Syncer(Replica(":memory:"), _client(lambda req: httpx.Response(200, json={"shows": LIB})), ["nelson"])
    s.seed()
    assert s.push() is True and s.online and s.pending() == 0
