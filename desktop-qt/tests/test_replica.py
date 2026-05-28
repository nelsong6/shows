"""Tests for the local SQLite replica — seed/reconcile, engine-backed mutations,
dirty tracking, and last-write-wins merge. Uses an in-memory DB."""

from shows.engine import first_unwatched, next_round
from shows.replica import Replica

OLD = "2025-01-01T00:00:00Z"
T0 = "2026-01-01T00:00:00Z"
NEW = "2027-01-01T00:00:00Z"


def _seed(r):
    r.merge_shows([
        {"id": "s1", "playlist": "nelson", "name": "S1", "root_path": r"D:\A", "updated_at": T0,
         "episodes": [
             {"id": "a", "relative_path": "a.mkv", "position": 0, "updated_at": T0},
             {"id": "b", "relative_path": "b.mkv", "position": 1, "updated_at": T0},
         ]},
        {"id": "s2", "playlist": "nelson", "name": "S2", "root_path": r"D:\B", "updated_at": T0,
         "episodes": [{"id": "c", "relative_path": "c.mkv", "position": 0, "updated_at": T0}]},
    ])


def test_seed_and_next_round():
    r = Replica(":memory:")
    _seed(r)
    shows = r.active_shows(["nelson"])
    assert {s.id for s in shows} == {"s1", "s2"}
    rnd = next_round(shows)
    assert {o.show_id for o in rnd} == {"s1", "s2"}
    assert next(o for o in rnd if o.show_id == "s1").episode_id == "a"  # lowest unwatched


def test_advance_persists_marks_dirty_and_advances_pick():
    r = Replica(":memory:")
    _seed(r)
    n, removed = r.advance([("s1", "a")])
    assert n == 1 and removed == []
    s1 = r.show("s1")
    assert next(e for e in s1.episodes if e.id == "a").watched_at is not None
    assert next(o for o in next_round(r.active_shows(["nelson"])) if o.show_id == "s1").episode_id == "b"
    p = r.pending()
    assert p["episodes"] >= 1 and p["history"] == 1


def test_advance_tombstones_drained_show():
    r = Replica(":memory:")
    _seed(r)
    n, removed = r.advance([("s2", "c")])
    assert n == 1 and removed == ["s2"]
    assert r.show("s2").removed_at is not None
    assert {s.id for s in r.active_shows(["nelson"])} == {"s1"}


def test_defer_persists_position_and_changes_pick():
    r = Replica(":memory:")
    _seed(r)
    assert r.defer("s1", "a") is True
    s1 = r.show("s1")
    assert next(e for e in s1.episodes if e.id == "a").position == 2  # max(0,1)+1
    assert first_unwatched(s1.episodes).id == "b"
    r.advance([("s1", "b")])
    assert r.defer("s1", "b") is False  # now watched -> no-op


def test_set_resume_persists():
    r = Replica(":memory:")
    _seed(r)
    r.set_resume("a", 123.5)
    assert r.resume_pos("a") == 123.5


def test_merge_lww_keeps_dirty_local_but_takes_newer_clean():
    r = Replica(":memory:")
    _seed(r)
    r.advance([("s1", "a")])  # episode a now dirty (locally watched)
    # An OLDER server view must NOT clobber the dirty local episode...
    r.merge_shows([{"id": "s1", "playlist": "nelson", "name": "S1", "root_path": r"D:\A", "updated_at": T0,
                    "episodes": [{"id": "a", "relative_path": "OLD.mkv", "position": 0, "updated_at": OLD}]}])
    assert next(e for e in r.show("s1").episodes if e.id == "a").relative_path == "a.mkv"
    # ...but a NEWER server update to the (clean) show row does win.
    r.merge_shows([{"id": "s1", "playlist": "nelson", "name": "S1NEW", "root_path": r"D:\A", "updated_at": NEW,
                    "episodes": []}])
    assert r.show("s1").name == "S1NEW"


def test_mark_synced_clears_pending():
    r = Replica(":memory:")
    _seed(r)
    r.advance([("s1", "a")])
    d = r.dirty()
    r.mark_synced("episodes", [e["id"] for e in d["episodes"]])
    r.mark_synced("watch_history", [h["id"] for h in d["history"]])
    p = r.pending()
    assert p["episodes"] == 0 and p["history"] == 0
