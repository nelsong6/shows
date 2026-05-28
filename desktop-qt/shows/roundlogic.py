"""Pure round helpers — deliberately free of Qt / mpv / network imports so
they run (and are unit-tested) on a plain Python runtime, where there is no
libmpv. The runner delegates the parts that must be exactly right to here.
"""

from __future__ import annotations

from typing import Iterable, Protocol, Sequence, TypeVar


class _Entry(Protocol):
    episode_id: str


E = TypeVar("E", bound=_Entry)


def advance_entries(round_: Sequence[E], deferred: Iterable[str]) -> list[E]:
    """The entries to mark watched when a round ends: every entry except the
    ones deferred during this round.

    A deferred episode was bumped to the back of its show's queue server-side
    and left unwatched (contract D2). The runner re-sends the whole round on
    advance, so without this filter the deferred episode would be marked
    watched — silently undoing the defer. Advance is idempotent (I3), so it is
    always safe to *include* an entry; the only entries we must *exclude* are
    the deferred ones.
    """
    skip = set(deferred)
    return [e for e in round_ if e.episode_id not in skip]


def parse_playlists(raw: str, default: Sequence[str]) -> list[str]:
    """Parse the SHOWS_PLAYLISTS env value (comma-separated) into a playlist
    list, trimming blanks. Falls back to `default` when nothing is set —
    mirrors the server's parsePlaylists so a single-entry list behaves exactly
    like the single-playlist path."""
    out = [p.strip() for p in (raw or "").split(",") if p.strip()]
    return out or list(default)
