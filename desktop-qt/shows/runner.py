"""Round-robin playback loop — Python port of
desktop/internal/playlist/runner.go.

  IDLE    -> /next-round -> queue N -> PLAYING
  PLAYING -> N end-files -> /advance -> IDLE
  IDLE    -> empty round -> DRAINED -> exit

One playlist by default; with several it fetches a cross-playlist round
(/api/rounds, contract X1) and routes each advance back by playlist (X2).

Interactive controls arrive on another thread (the control server), so the
current round + position are held under a lock:

  skip(current)  -> mark this episode watched now (per-episode advance, I7)
                    and jump mpv to the next entry. Idempotent with the
                    round-end advance, so it's also crash-resilient.
  defer(current) -> re-roll this show's next-round pick (/defer-show, D1-D3)
                    WITHOUT marking it watched, and jump to the next entry.
                    Deferred entries are excluded from the round-end advance
                    (see roundlogic.advance_entries) so the defer isn't undone.

API errors are non-fatal (retry with capped exponential backoff); player
errors (mpv shutdown) are fatal. Runs on its own thread; lifecycle is
controlled by a threading.Event `stop`.
"""

from __future__ import annotations

import logging
import threading
from typing import Callable, Optional

from . import roundlogic
from .apiclient import AdvanceEntry, AdvanceResult, APIError, Client, RoundEntry
from .player import Player, PlayerShutdown

log = logging.getLogger("shows.runner")


class Runner:
    def __init__(
        self,
        client: Client,
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
        self.client = client
        self.player = player
        self.playlists = playlists
        self.stop = stop
        self.on_round = on_round
        self.on_advance = on_advance
        self.on_drained = on_drained
        self.on_error = on_error

        # Interactive state, shared with the control-server thread.
        self._lock = threading.Lock()
        self._round: Optional[list[RoundEntry]] = None
        self._pos = 0
        self._deferred: set[str] = set()

    def run(self) -> None:
        try:
            self._loop()
        except PlayerShutdown as e:
            log.info("runner exiting: %s", e)
        except Exception as e:  # noqa: BLE001 — surface to the UI
            log.exception("runner crashed")
            if self.on_error:
                self.on_error(str(e))

    def _loop(self) -> None:
        while not self.stop.is_set():
            round_ = self._fetch_round_with_backoff()
            if round_ is None:
                return  # stopped
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

            log.info("round queued: %d episodes", len(round_))
            self._queue_round(round_)
            if self.on_round:
                self.on_round(round_)

            self.player.wait_for_round(len(round_), self.stop)
            if self.stop.is_set():
                return

            # Bound mpv's internal playlist memory over long sessions.
            self.player.playlist_clear()

            # Advance everything except what was deferred this round (D2): a
            # deferred episode must stay unwatched.
            with self._lock:
                entries = roundlogic.advance_entries(self._round or [], self._deferred)
                self._round = None
            result = self._advance_with_backoff(entries)
            if result is None:
                return  # stopped
            log.info("round advanced: %d, removed %d", result.advanced_count, len(result.removed_shows))
            if self.on_advance:
                self.on_advance(result)

    # ── interactive controls (called from the control-server thread) ──────
    def set_pos(self, i: int) -> None:
        """Report which queued entry mpv is now playing (its playlist-pos), so
        skip/defer act on the right episode."""
        with self._lock:
            self._pos = i

    def _current(self) -> Optional[RoundEntry]:
        with self._lock:
            r = self._round
            if r and 0 <= self._pos < len(r):
                return r[self._pos]
        return None

    def skip(self) -> None:
        """Skip the current episode: jump mpv forward now, and mark the episode
        watched immediately (per-episode advance, I7). The jump is instant; the
        advance is best-effort — the round-end advance re-sends it idempotently
        if this call fails."""
        cur = self._current()
        self.player.skip()
        if cur is None:
            return
        try:
            self._advance([cur])
        except APIError as e:
            log.warning("skip advance failed (round end will retry): %s", e)

    def defer(self) -> None:
        """Defer the current show's pick: re-roll it to a different episode next
        round (/defer-show, D1-D3) without marking it watched, then jump mpv
        forward. Excluded from the round-end advance so the defer holds. If the
        server defer fails, leave the episode playing rather than guess."""
        cur = self._current()
        if cur is None:
            return
        try:
            self.client.defer_show(cur.playlist or self.playlists[0], cur.show_id, cur.episode_id)
        except APIError as e:
            log.warning("defer failed; leaving %r playing: %s", cur.show_name, e)
            return
        with self._lock:
            self._deferred.add(cur.episode_id)
        self.player.skip()

    # ── round queue / fetch / advance ─────────────────────────────────────
    def _queue_round(self, round_: list[RoundEntry]) -> None:
        for i, ep in enumerate(round_):
            self.player.play(ep.absolute_path, "replace" if i == 0 else "append-play")
        # Show the first entry's name immediately.
        if round_:
            first = round_[0]
            self.player.show_text(f"{first.show_name}   (1/{len(round_)})", 4000)

    def _fetch_round(self) -> list[RoundEntry]:
        if len(self.playlists) == 1:
            return self.client.next_round(self.playlists[0])
        return self.client.next_round_multi(self.playlists)

    def _advance(self, entries: list[RoundEntry]) -> AdvanceResult:
        """Route the advance: the single-playlist endpoint when there's one
        playlist (the primary path), the cross-playlist endpoint otherwise."""
        if len(self.playlists) == 1:
            ae = [AdvanceEntry(show_id=e.show_id, episode_id=e.episode_id) for e in entries]
            return self.client.advance(self.playlists[0], ae)
        return self.client.advance_multi(entries)

    def _fetch_round_with_backoff(self) -> Optional[list[RoundEntry]]:
        backoff = 2.0
        while not self.stop.is_set():
            try:
                return self._fetch_round()
            except APIError as e:
                log.warning("next-round failed; retrying in %.0fs: %s", backoff, e)
                if self.stop.wait(backoff):
                    return None
                backoff = min(backoff * 2, 60.0)
        return None

    def _advance_with_backoff(self, entries: list[RoundEntry]) -> Optional[AdvanceResult]:
        backoff = 2.0
        while not self.stop.is_set():
            try:
                return self._advance(entries)
            except APIError as e:
                log.warning("advance failed; retrying in %.0fs: %s", backoff, e)
                if self.stop.wait(backoff):
                    return None
                backoff = min(backoff * 2, 60.0)
        return None
