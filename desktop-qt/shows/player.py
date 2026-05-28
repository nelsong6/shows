"""Thin wrapper over a python-mpv handle exposing the queue / wait /
clear / OSD operations the runner needs — Python analog of
desktop/internal/player. The mpv handle itself is owned by the QML
MpvItem; this wraps it for the runner thread.

mpv fires event callbacks on its own thread; we keep an end-file counter
under a condition variable that the runner thread blocks on.
"""

from __future__ import annotations

import threading
from typing import TYPE_CHECKING, Callable, Optional

if TYPE_CHECKING:
    # Only needed for the `mpv.MPV` annotation, which `from __future__ import
    # annotations` keeps as a string. Importing libmpv at module load would
    # otherwise force every importer (incl. the runner tests) to have the DLL.
    import mpv


class PlayerShutdown(Exception):
    """Raised when mpv emits a shutdown event (window closed)."""


class Player:
    def __init__(self, handle: mpv.MPV, on_pos: Optional[Callable[[int], None]] = None):
        self._m = handle
        self._cv = threading.Condition()
        self._end_files = 0
        self._shutdown = False

        @handle.event_callback("end-file")
        def _on_end(_ev):
            with self._cv:
                self._end_files += 1
                self._cv.notify_all()

        @handle.event_callback("shutdown")
        def _on_shutdown(_ev):
            with self._cv:
                self._shutdown = True
                self._cv.notify_all()

        # Keep refs so python-mpv's weakref-based callback registry doesn't
        # drop them.
        self._cbs = (_on_end, _on_shutdown)

        # Report the current playlist index (which queued entry is playing) so
        # the overlay can show "now playing / up next" and the runner knows
        # which episode skip/defer act on. Fires on mpv's thread; the callback
        # must be thread-safe. Always registered; set_on_pos can (re)point it
        # after construction, since the runner that consumes it is built later.
        self._on_pos = on_pos

        def _pos_handler(_name, value):
            cb = self._on_pos
            if cb is not None and value is not None:
                cb(int(value))

        handle.observe_property("playlist-pos", _pos_handler)
        self._pos_handler = _pos_handler  # keep ref alive

    def set_on_pos(self, fn: Callable[[int], None]) -> None:
        """(Re)point the playlist-pos callback. Used by main.py to fan position
        updates out to both the overlay status and the runner once both exist."""
        self._on_pos = fn

    # ── commands ──────────────────────────────────────────────────────
    def play(self, path: str, mode: str) -> None:
        # mode: "replace" for the first entry, "append-play" for the rest.
        self._m.loadfile(path, mode)

    def playlist_clear(self) -> None:
        try:
            self._m.command("playlist-clear")
        except Exception:
            pass

    def show_text(self, text: str, duration_ms: int) -> None:
        try:
            self._m.command("show-text", text, str(duration_ms))
        except Exception:
            pass

    def set_pause(self, paused: bool) -> None:
        self._m.pause = paused

    def toggle_pause(self) -> None:
        self._m.pause = not self._m.pause

    def skip(self) -> None:
        # Force-advance to the next queued entry (the runner counts the
        # resulting end-file like a natural one).
        try:
            self._m.command("playlist-next", "force")
        except Exception:
            pass

    # ── round synchronization ─────────────────────────────────────────
    def wait_for_round(self, n: int, stop: threading.Event) -> None:
        """Block until `n` more end-file events arrive. Raises
        PlayerShutdown if mpv closes; returns early (raising) if stopped."""
        with self._cv:
            target = self._end_files + n
            while self._end_files < target:
                if self._shutdown:
                    raise PlayerShutdown("mpv shutdown event")
                if stop.is_set():
                    raise PlayerShutdown("runner stopped")
                self._cv.wait(timeout=0.5)
