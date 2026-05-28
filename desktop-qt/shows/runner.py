"""Round-robin playback loop — offline-first.

The desktop is the engine: each round is computed locally from the SQLite
replica (engine.next_round), advances/defers are applied to the replica, and the
Syncer pushes those changes to the server at smart moments. Playback never
blocks on the network — the loop runs entirely off the replica.

  IDLE    -> next round from replica -> queue N -> PLAYING
  PLAYING -> each episode's natural end advances it (local); round drains -> IDLE
  IDLE    -> empty round -> DRAINED -> park

Advance is per-episode: an episode is marked watched the instant it plays to its
natural end (contract A1). A file that fails to load, or one you close/skip/defer
before it finishes, never reaches that point, so a non-watch is never recorded as
a watch. The replica updates locally on each finish; the origin is synced at
round end + on window close.

Interactive controls (control-server thread) act on the current entry under a
lock:
  skip  -> mark watched now (local advance, I7), push, jump forward.
  defer -> bump the show's pick (local defer, D1-D3, NOT watched), push, jump
           forward; its non-natural end means it isn't advanced.
"""

from __future__ import annotations

import logging
import threading
from typing import Callable, Optional

from . import engine
from .apiclient import AdvanceResult, RemovedShow, RoundEntry
from .player import Player, PlayerShutdown
from .replica import Replica
from .sync import Syncer

log = logging.getLogger("shows.runner")


class Runner:
    def __init__(
        self,
        replica: Replica,
        syncer: Syncer,
        player: Player,
        playlists: list[str],
        stop: threading.Event,
        on_round: Optional[Callable[[list[RoundEntry]], None]] = None,
        on_advance: Optional[Callable[[AdvanceResult], None]] = None,
        on_drained: Optional[Callable[[], None]] = None,
        on_error: Optional[Callable[[str], None]] = None,
    ):
        if not playlists:
            raise ValueError("runner needs at least one playlist")
        self.replica = replica
        self.syncer = syncer
        self.player = player
        self.playlists = playlists
        self.stop = stop
        self.on_round = on_round
        self.on_advance = on_advance
        self.on_drained = on_drained
        self.on_error = on_error

        self._lock = threading.Lock()
        self._round: Optional[list[RoundEntry]] = None
        self._pos = 0
        self._deferred: set[str] = set()
        self._playing: Optional[RoundEntry] = None  # entry mpv is playing now
        self._loaded_any = False  # did any entry open this round? (media-reachable guard)

    def run(self) -> None:
        try:
            self.syncer.seed()  # pull/reconcile once before the first local round
            self._loop()
        except PlayerShutdown as e:
            log.info("runner exiting: %s", e)
        except Exception as e:  # noqa: BLE001 — surface to the UI
            log.exception("runner crashed")
            if self.on_error:
                self.on_error(str(e))

    def _loop(self) -> None:
        while not self.stop.is_set():
            round_ = self._fetch_round()
            if not round_:
                log.info("playlists drained: %s", ",".join(self.playlists))
                if self.on_drained:
                    self.on_drained()
                self.stop.wait()  # park until shutdown
                return

            with self._lock:
                self._round = round_
                self._pos = 0
                self._deferred = set()
                self._playing = None
                self._loaded_any = False

            log.info("round queued: %d episodes", len(round_))
            self._queue_round(round_)
            if self.on_round:
                self.on_round(round_)

            # Episodes advance one-by-one as they finish (on_natural_end); this
            # just blocks until every queued entry has ended (by any reason).
            self.player.wait_for_round(len(round_), self.stop)
            if self.stop.is_set():
                return
            self.player.playlist_clear()

            with self._lock:
                loaded_any = self._loaded_any
                self._round = None

            if not loaded_any:
                # Nothing in the round opened — the library files are unreachable
                # (e.g. the media drive isn't mounted). Re-fetching would queue the
                # same unplayable round and spin, so surface it and park instead.
                log.error("round produced no playable media; parking until restart")
                if self.on_error:
                    self.on_error("no playable media — check that the show files are reachable")
                self.stop.wait()
                return

            # Flush this round's per-episode advances to the origin (smart moment).
            self.syncer.push()

    # ── interactive controls (control-server thread) ──────────────────────
    def set_pos(self, i: int) -> None:
        """Report which queued entry mpv is now playing (its playlist-pos)."""
        with self._lock:
            self._pos = i

    def _current(self) -> Optional[RoundEntry]:
        with self._lock:
            r = self._round
            if r and 0 <= self._pos < len(r):
                return r[self._pos]
        return None

    def skip(self) -> None:
        """Skip the current episode: jump forward now and mark it watched (the
        per-episode skip, I7). Local; pushed right away since it's a user action."""
        cur = self._current()
        self.player.skip()
        if cur is not None:
            self._apply_advance([cur])
            self.syncer.push()

    def defer(self) -> None:
        """Defer the current show's pick: bump it locally (D1-D3, not watched),
        push, and jump forward. The forced jump ends the file non-naturally, so
        it never advances (and the _deferred guard backs that up)."""
        cur = self._current()
        if cur is None:
            return
        if not self.replica.defer(cur.show_id, cur.episode_id):
            log.warning("defer no-op for %r", cur.show_name)
            return
        with self._lock:
            self._deferred.add(cur.episode_id)
        self.syncer.push()
        self.player.skip()

    # ── playback callbacks (mpv event thread; keep fast + local) ──────────
    def on_file_loaded(self) -> None:
        """A queued file just opened — mark it the now-playing entry (so its
        natural end advances exactly it), note the round has reachable media, and
        restore its saved resume position."""
        cur = self._current()
        if cur is None:
            return
        with self._lock:
            self._playing = cur
            self._loaded_any = True
        pos = self.replica.resume_pos(cur.episode_id)
        if pos and pos > 1.0:  # ignore zero / the very start
            self.player.seek_absolute(pos)

    def on_natural_end(self) -> None:
        """The now-playing episode reached its natural end — the user watched it,
        so mark it watched now (per-episode advance, A1). Only a real EOF lands
        here: a load failure, skip, or defer ends the file another way, so an
        unwatched episode is never advanced. Local-only; synced at round end and
        on window close."""
        with self._lock:
            cur = self._playing
            self._playing = None
            if cur is None or cur.episode_id in self._deferred:
                return
        result = self._apply_advance([cur])
        if self.on_advance:
            self.on_advance(result)

    def save_resume(self) -> None:
        """Persist the current episode's position to the replica (local; pushed
        on the next sync). Called periodically and on window close."""
        cur = self._current()
        if cur is None:
            return
        pos = self.player.time_pos()
        if pos is not None and pos > 1.0:
            self.replica.set_resume(cur.episode_id, float(pos))

    # ── round build / advance (all local; sync is best-effort) ────────────
    def _fetch_round(self) -> list[RoundEntry]:
        shows = self.replica.active_shows(self.playlists)
        ordered = engine.next_round(shows)
        name_by = {s.id: s.name for s in shows}
        pl_by = {s.id: s.playlist for s in shows}
        return [
            RoundEntry(
                show_id=o.show_id,
                show_name=name_by.get(o.show_id, ""),
                episode_id=o.episode_id,
                absolute_path=o.absolute_path,
                order_value=o.order_value,
                playlist=pl_by.get(o.show_id, ""),
            )
            for o in ordered
        ]

    def _apply_advance(self, entries: list[RoundEntry]) -> AdvanceResult:
        """Mark entries watched in the local replica + build the reveal for any
        show this finished. No network — callers push at their own smart moment."""
        advanced, removed_ids = self.replica.advance(
            [(e.show_id, e.episode_id) for e in entries]
        )
        removed = [RemovedShow(**self.replica.reveal(sid)) for sid in removed_ids]
        return AdvanceResult(advanced_count=advanced, removed_shows=removed)

    def _queue_round(self, round_: list[RoundEntry]) -> None:
        for i, ep in enumerate(round_):
            self.player.play(ep.absolute_path, "replace" if i == 0 else "append-play")
        if round_:
            first = round_[0]
            self.player.show_text(f"{first.show_name}   (1/{len(round_)})", 4000)
