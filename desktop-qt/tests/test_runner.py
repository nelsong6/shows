"""Runner orchestration tests with fake client + player — the skip/defer
commands, position tracking, round-end defer-exclusion, and single-vs-cross
advance routing. Imports the runner without libmpv (player.py guards its mpv
import behind TYPE_CHECKING)."""

import threading

from shows.apiclient import AdvanceResult, APIError, RoundEntry
from shows.runner import Runner


def _entry(eid, sid="s", pl="nelson"):
    return RoundEntry(
        show_id=sid, show_name=sid, episode_id=eid,
        absolute_path=f"D:\\{eid}.mkv", order_value=0, playlist=pl,
    )


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


class FakeClient:
    def __init__(self, rounds):
        self._rounds = list(rounds)   # one list per fetch; [] once exhausted
        self.advances = []            # (playlist, [AdvanceEntry])
        self.multi_advances = []      # ([RoundEntry])
        self.defers = []              # (playlist, show_id, episode_id)
        self.defer_error = False

    def next_round(self, pl):
        return self._rounds.pop(0) if self._rounds else []

    def next_round_multi(self, pls):
        return self._rounds.pop(0) if self._rounds else []

    def advance(self, pl, entries):
        self.advances.append((pl, entries))
        return AdvanceResult()

    def advance_multi(self, entries):
        self.multi_advances.append(entries)
        return AdvanceResult()

    def defer_show(self, pl, show, ep):
        if self.defer_error:
            raise APIError("boom")
        self.defers.append((pl, show, ep))


def test_skip_advances_current_and_jumps():
    c, p = FakeClient([]), FakePlayer()
    r = Runner(c, p, ["nelson"], threading.Event())
    r._round = [_entry("e0"), _entry("e1")]
    r.set_pos(0)

    r.skip()

    assert p.skips == 1
    assert len(c.advances) == 1
    pl, entries = c.advances[0]
    assert pl == "nelson"
    assert [e.episode_id for e in entries] == ["e0"]


def test_defer_calls_defer_show_excludes_and_jumps():
    c, p = FakeClient([]), FakePlayer()
    r = Runner(c, p, ["nelson"], threading.Event())
    r._round = [_entry("e0"), _entry("e1")]
    r.set_pos(1)

    r.defer()

    assert c.defers == [("nelson", "s", "e1")]
    assert "e1" in r._deferred
    assert p.skips == 1


def test_defer_failure_leaves_unchanged_and_keeps_playing():
    c, p = FakeClient([]), FakePlayer()
    c.defer_error = True
    r = Runner(c, p, ["nelson"], threading.Event())
    r._round = [_entry("e0")]
    r.set_pos(0)

    r.defer()

    assert r._deferred == set()
    assert p.skips == 0          # didn't jump past it — defer didn't take
    assert c.advances == []      # and definitely didn't mark it watched


def test_skip_or_defer_with_no_round_is_safe():
    c, p = FakeClient([]), FakePlayer()
    r = Runner(c, p, ["nelson"], threading.Event())
    # No round in progress (_round is None).
    r.defer()
    r.skip()
    assert c.defers == []
    assert c.advances == []
    assert p.skips == 1          # skip still nudges the player locally


def test_round_end_advance_excludes_deferred():
    c = FakeClient([[_entry("e0"), _entry("e1")]])  # one round, then drained
    p = FakePlayer()
    stop = threading.Event()
    r = Runner(c, p, ["nelson"], stop, on_drained=stop.set)

    # Mid-round, the user defers the current entry (pos 0 == e0).
    def during_wait():
        r.set_pos(0)
        r.defer()

    p.on_wait = during_wait
    r._loop()

    assert c.defers == [("nelson", "s", "e0")]
    assert len(c.advances) == 1
    _, entries = c.advances[0]
    assert [e.episode_id for e in entries] == ["e1"]  # e0 was deferred, not advanced


def test_cross_playlist_round_uses_advance_multi():
    rnd = [_entry("e0", pl="nelson"), _entry("e1", pl="couple")]
    c = FakeClient([rnd])
    p = FakePlayer()
    stop = threading.Event()
    r = Runner(c, p, ["nelson", "couple"], stop, on_drained=stop.set)

    r._loop()

    assert len(c.multi_advances) == 1
    assert [e.episode_id for e in c.multi_advances[0]] == ["e0", "e1"]
    assert c.advances == []  # routed through the cross-playlist endpoint, not single
