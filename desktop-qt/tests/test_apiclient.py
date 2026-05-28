"""Request-shaping tests for the new apiclient endpoints, driven through an
httpx MockTransport (no network, no Qt/mpv)."""

import json

import httpx

from shows.apiclient import Client, RoundEntry


def _client(handler):
    c = Client(token="tok", base_url="https://example.test")
    c._http = httpx.Client(transport=httpx.MockTransport(handler))
    return c


def test_next_round_stamps_playlist():
    def handler(req):
        assert req.url.path == "/api/playlists/nelson/next-round"
        return httpx.Response(200, json={"round": [
            {"show_id": "s1", "show_name": "One", "episode_id": "e1",
             "absolute_path": "D:\\A\\a.mkv", "order_value": 1},
        ]})

    out = _client(handler).next_round("nelson")
    assert len(out) == 1
    # The single-playlist endpoint omits playlist; the client stamps it.
    assert out[0].playlist == "nelson"


def test_next_round_multi_query_and_playlist():
    def handler(req):
        assert req.url.path == "/api/rounds"
        assert req.url.params.get("playlists") == "nelson,couple"
        return httpx.Response(200, json={"round": [
            {"show_id": "s1", "show_name": "One", "episode_id": "e1",
             "absolute_path": "p", "order_value": 1, "playlist": "couple"},
        ]})

    out = _client(handler).next_round_multi(["nelson", "couple"])
    assert out[0].playlist == "couple"


def test_advance_multi_body_shape():
    seen = {}

    def handler(req):
        assert req.url.path == "/api/rounds/advance"
        seen["body"] = json.loads(req.content)
        return httpx.Response(200, json={"advanced_count": 2, "removed_shows": []})

    entries = [
        RoundEntry("s1", "One", "e1", "p1", 1, playlist="nelson"),
        RoundEntry("s2", "Two", "e2", "p2", 2, playlist="couple"),
    ]
    res = _client(handler).advance_multi(entries)
    assert res.advanced_count == 2
    assert seen["body"] == {"entries": [
        {"playlist": "nelson", "show_id": "s1", "episode_id": "e1"},
        {"playlist": "couple", "show_id": "s2", "episode_id": "e2"},
    ]}


def test_defer_show_url_and_body():
    seen = {}

    def handler(req):
        seen["path"] = req.url.path
        seen["body"] = json.loads(req.content)
        return httpx.Response(204)

    _client(handler).defer_show("nelson", "s1", "e1")
    assert seen["path"] == "/api/playlists/nelson/defer-show"
    assert seen["body"] == {"show_id": "s1", "episode_id": "e1"}
