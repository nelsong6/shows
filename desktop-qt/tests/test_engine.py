"""Contract tests for the local engine — ported from the Go server's
internal/store store_test.go vectors so the Python port is provably faithful to
the round-and-advance contract. (Post-F1 the server engine is gone, so these
become the engine's regression lock.)"""

from shows.engine import (
    Episode,
    Show,
    advance,
    all_watched,
    defer,
    first_unwatched,
    next_round,
)

NOW = "2026-05-28T12:00:00Z"
EARLIER = "2026-05-28T11:00:00Z"


def ep(eid, pos, watched=None):
    return Episode(id=eid, relative_path=f"{eid}.mkv", position=pos, watched_at=watched)


# ── first_unwatched ────────────────────────────────────────────────────
def test_first_unwatched_lowest_position():
    eps = [ep("c", 2), ep("a", 0), ep("b", 1)]
    assert first_unwatched(eps).id == "a"


def test_first_unwatched_skips_watched_even_lower():
    eps = [ep("a", 0, NOW), ep("b", 1)]
    assert first_unwatched(eps).id == "b"


def test_first_unwatched_none_when_all_watched_or_empty():
    assert first_unwatched([ep("a", 0, NOW), ep("b", 1, NOW)]) is None
    assert first_unwatched([]) is None


def test_all_watched():
    assert not all_watched([ep("a", 0), ep("b", 1, NOW)])
    assert all_watched([ep("a", 0, NOW)])


# ── advance (I3 / I5 / I7) ─────────────────────────────────────────────
def test_advance_subset_only_named(  ):
    s = Show("s1", "nelson", "S1", r"D:\A", [ep("a", 0), ep("b", 1)])
    history, n, removed = advance(s, ["a"], NOW)
    assert n == 1 and not removed
    assert s.episodes[0].watched_at == NOW
    assert s.episodes[1].watched_at is None
    assert [h.episode_id for h in history] == ["a"]


def test_advance_idempotent():
    s = Show("s1", "nelson", "S1", r"D:\A", [ep("a", 0, EARLIER), ep("b", 1)])
    history, n, removed = advance(s, ["a"], NOW)
    assert n == 0 and not removed and history == []
    assert s.episodes[0].watched_at == EARLIER  # not overwritten


def test_advance_tombstones_when_drained():
    s = Show("s1", "nelson", "S1", r"D:\A", [ep("a", 0, EARLIER), ep("b", 1)])
    _, n, removed = advance(s, ["b"], NOW)
    assert n == 1 and removed and s.removed_at == NOW


def test_advance_whole_round_drains():
    s = Show("s1", "nelson", "S1", r"D:\A", [ep("a", 0), ep("b", 1)])
    history, n, removed = advance(s, ["a", "b"], NOW)
    assert n == 2 and removed and len(history) == 2


def test_advance_unknown_id_noop():
    s = Show("s1", "nelson", "S1", r"D:\A", [ep("a", 0)])
    _, n, removed = advance(s, ["nope"], NOW)
    assert n == 0 and not removed and s.episodes[0].watched_at is None


# ── defer (D1-D3) ──────────────────────────────────────────────────────
def test_defer_bumps_and_changes_pick():
    eps = [ep("a", 0), ep("b", 1), ep("c", 2)]
    assert defer(eps, "a") is True
    assert eps[0].position == 3 and eps[0].watched_at is None
    assert first_unwatched(eps).id == "b"


def test_defer_watched_or_absent_is_noop():
    eps = [ep("a", 0, NOW), ep("b", 1)]
    assert defer(eps, "a") is False  # watched
    assert defer(eps, "nope") is False  # absent


# ── next_round (I1 selection + ordering, X1 cross-playlist) ─────────────
def test_next_round_one_per_active_show_skips_drained_and_removed():
    shows = [
        Show("s1", "nelson", "S1", r"D:\A", [ep("a", 0), ep("b", 1, NOW)]),
        Show("s2", "nelson", "S2", r"D:\B", [ep("c", 0, NOW)]),          # drained
        Show("s3", "nelson", "S3", r"D:\C", [ep("d", 0)], removed_at=NOW),  # tombstoned
        Show("s4", "couple", "S4", r"D:\D", [ep("e", 0)]),                # other playlist
    ]
    out = next_round(shows)
    picked = {o.show_id for o in out}
    assert picked == {"s1", "s4"}  # s1's "a" (b is watched), s4's "e"; s2 drained, s3 removed


def test_next_round_deterministic_ascending_union():
    shows = [
        Show("s1", "nelson", "S1", r"D:\A", [ep("a", 0)]),
        Show("s2", "couple", "S2", r"D:\B", [ep("b", 0)]),
        Show("s3", "nelson", "S3", r"D:\C", [ep("c", 0)]),
    ]
    out = next_round(shows)
    vals = [o.order_value for o in out]
    assert vals == sorted(vals)  # round order is ascending by the hash key
