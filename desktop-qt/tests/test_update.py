"""Update-check tests — mocked GitHub response + injected build SHA, no network."""

import httpx

import shows.update as update


def _mock_get(monkeypatch, *, tag="desktop-newsha", url="https://example/r", status=200):
    def fake_get(u, **kw):
        return httpx.Response(
            status,
            json={"tag_name": tag, "html_url": url},
            request=httpx.Request("GET", update.RELEASES_LATEST),
        )
    monkeypatch.setattr(update.httpx, "get", fake_get)


def test_no_embedded_sha_skips(monkeypatch):
    monkeypatch.setattr(update, "current_sha", lambda: None)
    assert update.check() is None  # dev/source run — nothing to compare


def test_update_available(monkeypatch):
    monkeypatch.setattr(update, "current_sha", lambda: "old1234")
    _mock_get(monkeypatch, tag="desktop-new5678", url="https://gh/rel")
    assert update.check() == {"latest": "new5678", "current": "old1234", "url": "https://gh/rel"}


def test_up_to_date_returns_none(monkeypatch):
    monkeypatch.setattr(update, "current_sha", lambda: "same999")
    _mock_get(monkeypatch, tag="desktop-same999")
    assert update.check() is None


def test_network_failure_is_silent(monkeypatch):
    monkeypatch.setattr(update, "current_sha", lambda: "x")

    def boom(u, **kw):
        raise httpx.ConnectError("offline")

    monkeypatch.setattr(update.httpx, "get", boom)
    assert update.check() is None
