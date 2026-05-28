"""Runner tests for the offline-first design: the runner drives a real
in-memory replica via the engine, with a stub syncer + fake player."""

import threading

from shows.replica import Replica
from shows.runner import Runner

T0 = "2026-01-01T00:00:00Z"


class FakePlayer:
    def __init__(self):
        self.skips = 0
        self.on_wait = None  # hook to drive interactive controls mid-round

    def play(self, *a):
        pass

    def show_text(self, *a):
        pass

    def playlist_clear(self):
        pass

    def skip(self):
        self.skips += 1

    def wait_for_round(self, n, stop):
        if self.on_wait:
            self.on_wait()


class StubSyncer:
    def __init__(self):
        self.seeds = 0
        self.pushes = 0
        self.online = True

    def seed(self):
        self.seeds += 1
        return True

    def push(self):
        self.pushes += 1
        return True

    def pending(self):
        return 0


def _show(sid, playlist, eps):
    return {
        "id": sid, "playlist": playlist, "name": sid, "root_path": rf"D:\{sid}", "updated_at": T0,
        "episodes": [
            {"id": e, "relative_path": f"{e}.mkv", "position": i, "updated_at": T0}
            for i, e in enumerate(eps)
        ],
    }


def _replica(shows):
    r = Replica(":memory:")
    r.merge_shows(shows)
    return r


def _watched(r, episode_id):
    for sid in ("s1", "s2"):
        s = r.show(sid)
        if not s:
            continue
        for e in s.episodes:
            if e.id == episode_id:
                return e.watched_at
    return None


def test_skip_advances_current_locally():
    r = _replica([_show("s1", "nelson", ["a", "b"])])
    p = FakePlayer()
    runner = Runner(r, StubSyncer(), p, ["nelson"], threading.Event())
    runner._round = runner._fetch_round()
    runner.set_pos(0)
    cur = runner._current()
    runner.skip()
    assert p.skips == 1
    assert _watched(r, cur.episode_id) is not None


def test_defer_bumps_without_watching_and_excludes():
    r = _replica([_show("s1", "nelson", ["a", "b"])])
    p = FakePlayer()
    runner = Runner(r, StubSyncer(), p, ["nelson"], threading.Event())
    runner._round = runner._fetch_round()
    runner.set_pos(0)
    cur = runner._current()
    runner.defer()
    ep = next(e for e in r.show("s1").episodes if e.id == cur.episode_id)
    assert ep.watched_at is None and ep.position == 2  # bumped to max+1, not watched
    assert cur.episode_id in runner._deferred
    assert p.skips == 1


def test_no_round_is_safe():
    r = _replica([_show("s1", "nelson", ["a"])])
    p = FakePlayer()
    runner = Runner(r, StubSyncer(), p, ["nelson"], threading.Event())
    runner.defer()  # no round in progress -> no-op
    runner.skip()   # skip still nudges the player
    assert p.skips == 1
    assert r.show("s1").episodes[0].watched_at is None


def test_drained_calls_on_drained():
    r = Replica(":memory:")  # empty library
    stop = threading.Event()
    seen = {"drained": False}
    runner = Runner(
        r, StubSyncer(), FakePlayer(), ["nelson"], stop,
        on_drained=lambda: (seen.update(drained=True), stop.set()),
    )
    runner._loop()
    assert seen["drained"]


def test_round_end_advance_excludes_deferred():
    r = _replica([_show("s1", "nelson", ["a", "b"]), _show("s2", "nelson", ["c"])])
    p = FakePlayer()
    stop = threading.Event()
    runner = Runner(r, StubSyncer(), p, ["nelson"], stop, on_advance=lambda res: stop.set())
    cap = {}

    def during_wait():
        runner.set_pos(0)
        cap["deferred"] = runner._current().episode_id
        cap["round"] = [e.episode_id for e in runner._round]
        runner.defer()

    p.on_wait = during_wait
    runner._loop()

    assert _watched(r, cap["deferred"]) is None  # deferred stays unwatched
    for e in [x for x in cap["round"] if x != cap["deferred"]]:
        assert _watched(r, e) is not None  # the rest of the round advanced


def test_cross_playlist_round_spans_playlists():
    r = _replica([_show("s1", "nelson", ["a"]), _show("s2", "couple", ["b"])])
    runner = Runner(r, StubSyncer(), FakePlayer(), ["nelson", "couple"], threading.Event())
    rnd = runner._fetch_round()
    assert {e.show_id for e in rnd} == {"s1", "s2"}  # union across both playlists
