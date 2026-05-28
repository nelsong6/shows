"""Pure-logic tests for shows.roundlogic — no Qt/mpv, so they run on a plain
Python runtime in CI."""

from shows.roundlogic import parse_playlists


def test_parse_playlists_default_when_empty():
    assert parse_playlists("", ["nelson"]) == ["nelson"]
    assert parse_playlists("   ", ["nelson"]) == ["nelson"]
    assert parse_playlists(None, ["nelson"]) == ["nelson"]


def test_parse_playlists_trims_and_drops_blanks():
    assert parse_playlists(" a , b ,, c ", ["x"]) == ["a", "b", "c"]


def test_parse_playlists_single():
    assert parse_playlists("couple", ["nelson"]) == ["couple"]
