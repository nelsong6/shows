"""Round-robin playback loop — Python port of
desktop/internal/playlist/runner.go.

  IDLE    -> /next-round -> queue N -> PLAYING
  PLAYING -> N end-files -> /advance -> IDLE
  IDLE    -> empty round -> DRAINED -> exit

API errors are non-fatal (retry with capped exponential backoff); player
errors (mpv shutdown) are fatal. Runs on its own thread; lifecycle is
controlled by a threading.Event `stop`.
"""

from __future__ import annotations

import logging
import threading
from typing import Callable, Optional

from .apiclient import AdvanceEntry, AdvanceResult, APIError, Client, RoundEntry
from .player import Player, PlayerShutdown

log = logging.getLogger("shows.runner")


class Runner:
    def __init__(
        self,
        client: Client,
        player: Player,
        playlist: str,
        stop: threading.Event,
        on_round: Optional[Callable[[list[RoundEntry]], None]] = None,
        on_advance: Optional[Callable[[AdvanceResult], None]] = None,
        on_drained: Optional[Callable[[], None]] = None,
        on_error: Optional[Callable[[str], None]] = None,
    ):
        self.client = client
        self.player = player
        self.playlist = playlist
        self.stop = stop
        self.on_round = on_round
        self.on_advance = on_advance
        self.on_drained = on_drained
        self.on_error = on_error

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
                log.info("playlist drained: %s", self.playlist)
                if self.on_drained:
                    self.on_drained()
                self.stop.wait()  # park until shutdown
                return

            log.info("round queued: %d episodes", len(round_))
            self._queue_round(round_)
            if self.on_round:
                self.on_round(round_)

            self.player.wait_for_round(len(round_), self.stop)
            if self.stop.is_set():
                return

            # Bound mpv's internal playlist memory over long sessions.
            self.player.playlist_clear()

            entries = [AdvanceEntry(show_id=e.show_id, episode_id=e.episode_id) for e in round_]
            result = self._advance_with_backoff(entries)
            if result is None:
                return  # stopped
            log.info("round advanced: %d, removed %d", result.advanced_count, len(result.removed_shows))
            if self.on_advance:
                self.on_advance(result)

    def _queue_round(self, round_: list[RoundEntry]) -> None:
        for i, ep in enumerate(round_):
            self.player.play(ep.absolute_path, "replace" if i == 0 else "append-play")
            # OSD the show name as each entry loads. (file-loaded ordering
            # matches queue order; a simple index is enough.)
        # Show the first entry's name immediately.
        if round_:
            first = round_[0]
            self.player.show_text(f"{first.show_name}   (1/{len(round_)})", 4000)

    def _fetch_round_with_backoff(self) -> Optional[list[RoundEntry]]:
        backoff = 2.0
        while not self.stop.is_set():
            try:
                return self.client.next_round(self.playlist)
            except APIError as e:
                log.warning("next-round failed; retrying in %.0fs: %s", backoff, e)
                if self.stop.wait(backoff):
                    return None
                backoff = min(backoff * 2, 60.0)
        return None

    def _advance_with_backoff(self, entries: list[AdvanceEntry]) -> Optional[AdvanceResult]:
        backoff = 2.0
        while not self.stop.is_set():
            try:
                return self.client.advance(self.playlist, entries)
            except APIError as e:
                log.warning("advance failed; retrying in %.0fs: %s", backoff, e)
                if self.stop.wait(backoff):
                    return None
                backoff = min(backoff * 2, 60.0)
        return None
