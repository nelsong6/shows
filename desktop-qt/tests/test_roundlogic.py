"""Pure-logic tests for shows.roundlogic — no Qt/mpv, so they run on a plain
Python runtime in CI."""

from shows.roundlogic import advance_entries, parse_playlists


class _Entry:
    def __init__(self, episode_id, playlist=""):
        self.episode_id = episode_id
        self.playlist = playlist


def test_advance_entries_excludes_deferred():
    # The defer-correctness invariant: a deferred episode must NOT be in the
    # round-end advance, or it'd be marked watched (undoing the defer).
    round_ = [_Entry("a"), _Entry("b"), _Entry("c")]
    got = advance_entries(round_, {"b"})
    assert [e.episode_id for e in got] == ["a", "c"]


def test_advance_entries_none_deferred_keeps_all():
    round_ = [_Entry("a"), _Entry("b")]
    assert [e.episode_id for e in advance_entries(round_, set())] == ["a", "b"]


def test_advance_entries_all_deferred_is_empty():
    round_ = [_Entry("a"), _Entry("b")]
    assert advance_entries(round_, {"a", "b"}) == []


def test_advance_entries_accepts_list_deferred():
    round_ = [_Entry("a"), _Entry("b")]
    assert [e.episode_id for e in advance_entries(round_, ["a"])] == ["b"]


def test_parse_playlists_default_when_empty():
    assert parse_playlists("", ["nelson"]) == ["nelson"]
    assert parse_playlists("   ", ["nelson"]) == ["nelson"]
    assert parse_playlists(None, ["nelson"]) == ["nelson"]


def test_parse_playlists_trims_and_drops_blanks():
    assert parse_playlists(" a , b ,, c ", ["x"]) == ["a", "b", "c"]


def test_parse_playlists_single():
    assert parse_playlists("couple", ["nelson"]) == ["couple"]
