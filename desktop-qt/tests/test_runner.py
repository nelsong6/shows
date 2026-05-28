"""Runner tests for the offline-first design: the runner drives a real
in-memory replica via the engine, with a stub syncer + fake player.

Playback is simulated by calling the runner's mpv-thread callbacks directly:
`on_file_loaded()` (a queued entry opened) then `on_natural_end()` (it played to
its natural end). An entry that never opens — a failed load, or one you close
before reaching — simply gets neither call, and so is never advanced."""

import threading

from shows.replica import Replica
from shows.runner import Runner

T0 = "2026-01-01T00:00:00Z"


class FakePlayer:
    def __init__(self):
        self.skips = 0
        self.on_wait = None  # hook to drive playback/controls during the round
        self.seeked = None   # last seek_absolute target
        self._time = None    # what time_pos() returns

    def play(self, *a):
        pass

    def show_text(self, *a):
        pass

    def playlist_clear(self):
        pass

    def skip(self):
        self.skips += 1

    def time_pos(self):
        return self._time

    def seek_absolute(self, seconds):
        self.seeked = seconds

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


def _play(runner, i):
    """Simulate mpv opening queued entry i and playing it to its natural end."""
    runner.set_pos(i)
    runner.on_file_loaded()
    runner.on_natural_end()


# ── interactive controls ───────────────────────────────────────────────

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


def test_defer_bumps_without_watching():
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


# ── per-episode advance (the "watch what's next" model) ─────────────────

def test_each_finished_episode_is_marked_watched():
    r = _replica([_show("s1", "nelson", ["a"]), _show("s2", "nelson", ["b"])])
    p = FakePlayer()
    stop = threading.Event()
    runner = Runner(r, StubSyncer(), p, ["nelson"], stop)

    def during_wait():
        for i in range(len(runner._round)):
            _play(runner, i)
        stop.set()

    p.on_wait = during_wait
    runner._loop()
    assert _watched(r, "a") is not None
    assert _watched(r, "b") is not None


def test_unfinished_episode_is_not_watched():
    # Watch the Simpsons to the end, "turn off" before Malcolm plays: Simpsons is
    # done; Malcolm wasn't watched, so it stays your next pick.
    r = _replica([_show("s1", "nelson", ["simpsons"]), _show("s2", "nelson", ["malcolm"])])
    p = FakePlayer()
    stop = threading.Event()
    runner = Runner(r, StubSyncer(), p, ["nelson"], stop)

    def during_wait():
        for i, e in enumerate(runner._round):
            if e.show_id == "s1":      # only the Simpsons entry plays through
                _play(runner, i)
        stop.set()

    p.on_wait = during_wait
    runner._loop()
    assert _watched(r, "simpsons") is not None  # finished -> watched
    assert _watched(r, "malcolm") is None       # missed -> stays unwatched


def test_failed_load_is_not_watched():
    # A file that fails to open fires no file-loaded and no natural end, so the
    # runner never advances it (the load-failure invariant, A2).
    r = _replica([_show("s1", "nelson", ["a"]), _show("s2", "nelson", ["b"])])
    p = FakePlayer()
    stop = threading.Event()
    runner = Runner(r, StubSyncer(), p, ["nelson"], stop)

    def during_wait():
        for i, e in enumerate(runner._round):
            if e.show_id == "s1":      # s1 plays; s2's file fails to load
                _play(runner, i)
        stop.set()

    p.on_wait = during_wait
    runner._loop()
    assert _watched(r, "a") is not None
    assert _watched(r, "b") is None


def test_deferred_episode_is_not_advanced_on_finish():
    # A deferred episode must never be marked watched even if a natural-end
    # arrives for it (D2 + the on_natural_end deferred guard).
    r = _replica([_show("s1", "nelson", ["a", "b"])])
    p = FakePlayer()
    runner = Runner(r, StubSyncer(), p, ["nelson"], threading.Event())
    runner._round = runner._fetch_round()
    runner.set_pos(0)
    runner.on_file_loaded()
    cur = runner._current()
    runner.defer()              # bumps + marks deferred + player.skip()
    runner.on_natural_end()     # a stray EOF for the deferred entry -> no-op
    ep = next(e for e in r.show("s1").episodes if e.id == cur.episode_id)
    assert ep.watched_at is None and ep.position == 2  # not watched; bumped


def test_round_with_no_playable_media_parks():
    # Whole round unreachable: nothing opens, so nothing is watched and the runner
    # parks instead of re-queuing the same broken round forever.
    r = _replica([_show("s1", "nelson", ["a"]), _show("s2", "nelson", ["b"])])
    p = FakePlayer()  # on_wait is None -> wait_for_round opens nothing
    stop = threading.Event()
    errors, advances = [], []
    runner = Runner(
        r, StubSyncer(), p, ["nelson"], stop,
        on_advance=lambda res: advances.append(res),
        on_error=lambda e: (errors.append(e), stop.set()),
    )
    runner._loop()
    assert errors and "no playable media" in errors[0]
    assert not advances                                       # nothing finished
    assert _watched(r, "a") is None and _watched(r, "b") is None


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


def test_cross_playlist_round_spans_playlists():
    r = _replica([_show("s1", "nelson", ["a"]), _show("s2", "couple", ["b"])])
    runner = Runner(r, StubSyncer(), FakePlayer(), ["nelson", "couple"], threading.Event())
    rnd = runner._fetch_round()
    assert {e.show_id for e in rnd} == {"s1", "s2"}  # union across both playlists


# ── resume ──────────────────────────────────────────────────────────────

def test_save_resume_persists_position():
    r = _replica([_show("s1", "nelson", ["a", "b"])])
    p = FakePlayer()
    p._time = 100.0
    runner = Runner(r, StubSyncer(), p, ["nelson"], threading.Event())
    runner._round = runner._fetch_round()
    runner.set_pos(0)
    cur = runner._current()
    runner.save_resume()
    assert r.resume_pos(cur.episode_id) == 100.0


def test_on_file_loaded_seeks_to_resume():
    r = _replica([_show("s1", "nelson", ["a", "b"])])
    p = FakePlayer()
    runner = Runner(r, StubSyncer(), p, ["nelson"], threading.Event())
    runner._round = runner._fetch_round()
    runner.set_pos(0)
    cur = runner._current()
    r.set_resume(cur.episode_id, 200.0)
    runner.on_file_loaded()
    assert p.seeked == 200.0
    assert runner._playing is cur  # now-playing tracked for per-episode advance
