"""Pure round helpers — deliberately free of Qt / mpv / network imports so
they run (and are unit-tested) on a plain Python runtime, where there is no
libmpv. The runner delegates the parts that must be exactly right to here.
"""

from __future__ import annotations

from typing import Sequence


def parse_playlists(raw: str, default: Sequence[str]) -> list[str]:
    """Parse the SHOWS_PLAYLISTS env value (comma-separated) into a playlist
    list, trimming blanks. Falls back to `default` when nothing is set —
    mirrors the server's parsePlaylists so a single-entry list behaves exactly
    like the single-playlist path."""
    out = [p.strip() for p in (raw or "").split(",") if p.strip()]
    return out or list(default)
