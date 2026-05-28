"""Request-shaping tests for the apiclient — driven through an httpx
MockTransport (no network, no Qt/mpv). The live surface is get_library (pull) +
post_sync (push), plus the 401 refresh-and-retry."""

import json

import httpx

from shows.apiclient import APIError, Client


def _client(handler, **kw):
    c = Client(token="tok", base_url="https://example.test", **kw)
    c._http = httpx.Client(transport=httpx.MockTransport(handler))
    return c


def test_get_library_query_and_parse():
    def handler(req):
        assert req.method == "GET"
        assert req.url.path == "/api/library"
        assert req.url.params.get("playlists") == "nelson,couple"
        assert req.headers["Authorization"] == "Bearer tok"
        return httpx.Response(200, json={"shows": [{"id": "s1"}, {"id": "s2"}]})

    out = _client(handler).get_library(["nelson", "couple"])
    assert [s["id"] for s in out] == ["s1", "s2"]


def test_get_library_missing_key_is_empty():
    out = _client(lambda req: httpx.Response(200, json={})).get_library(["nelson"])
    assert out == []


def test_post_sync_body_shape():
    seen = {}

    def handler(req):
        assert req.method == "POST"
        assert req.url.path == "/api/sync"
        seen["body"] = json.loads(req.content)
        return httpx.Response(204)

    _client(handler).post_sync(
        shows=[{"id": "s1"}], episodes=[{"id": "e1"}], history=[{"id": "h1"}]
    )
    assert seen["body"] == {
        "shows": [{"id": "s1"}],
        "episodes": [{"id": "e1"}],
        "history": [{"id": "h1"}],
    }


def test_401_refreshes_token_once_and_retries():
    calls = {"n": 0}

    def handler(req):
        calls["n"] += 1
        if calls["n"] == 1:
            assert req.headers["Authorization"] == "Bearer tok"
            return httpx.Response(401, json={"error": "expired"})
        assert req.headers["Authorization"] == "Bearer tok2"  # refreshed
        return httpx.Response(200, json={"shows": []})

    c = _client(handler, refresh_token=lambda: "tok2")
    c.get_library(["nelson"])
    assert calls["n"] == 2 and c.token == "tok2"


def test_persistent_error_raises_apierror():
    c = _client(lambda req: httpx.Response(500, text="boom"))
    try:
        c.get_library(["nelson"])
        assert False, "expected APIError"
    except APIError as e:
        assert "500" in str(e)
