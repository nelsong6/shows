"""The round/advance/defer engine — the desktop's local implementation of the
round-and-advance contract (docs/feature-contracts/round-and-advance.md).

Under the offline-first design the desktop is the engine: it computes rounds and
applies advance/defer/resume against its local replica, and the server is a
durable store it syncs to. This module is the single source of that behaviour;
it is a faithful port of the Go server's pure store logic (firstUnwatched /
applyAdvance / deferEpisode / allWatched + NextEpisodes→Sort), and is locked to
the same contract by golden fixtures shared with the Go tests.

Pure: operates on plain dataclasses, no SQLite / Qt / network. The replica layer
loads rows, calls these, and persists the results.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Optional

from .ordering import Candidate, Ordered, sort_round


@dataclass
class Episode:
    id: str
    relative_path: str
    position: int
    watched_at: Optional[str] = None  # ISO-8601, or None if unwatched
    resume_pos: Optional[float] = None  # seconds into the file, or None


@dataclass
class Show:
    id: str
    playlist: str
    name: str
    root_path: str
    episodes: list[Episode] = field(default_factory=list)
    removed_at: Optional[str] = None  # tombstone (all episodes watched)
    date_added: Optional[str] = None


@dataclass
class HistoryRow:
    show_id: str
    episode_id: str
    relative_path: str
    played_at: str


def first_unwatched(episodes: list[Episode]) -> Optional[Episode]:
    """The show's next pick: the lowest-position episode that isn't watched."""
    best: Optional[Episode] = None
    for e in episodes:
        if e.watched_at is not None:
            continue
        if best is None or e.position < best.position:
            best = e
    return best


def all_watched(episodes: list[Episode]) -> bool:
    return all(e.watched_at is not None for e in episodes)


def next_round(shows: list[Show]) -> list[Ordered]:
    """One episode per active show, in deterministic round order (contract I1).

    Pass the active shows for one playlist for a single-playlist round, or the
    union across playlists for a cross-playlist round (X1) — ordering keys on the
    absolute path alone, so membership never changes an episode's place.
    """
    cands: list[Candidate] = []
    for s in shows:
        if s.removed_at is not None:
            continue
        ep = first_unwatched(s.episodes)
        if ep is None:
            continue
        cands.append(
            Candidate(
                episode_id=ep.id,
                show_id=s.id,
                root_path=s.root_path,
                relative_path=ep.relative_path,
            )
        )
    return sort_round(cands)


def advance(show: Show, episode_ids: list[str], now: str) -> tuple[list[HistoryRow], int, bool]:
    """Mark the named episodes watched on `show`, mirroring the server's
    applyAdvance. Returns (history rows, count newly watched, tombstoned?).

      - I3 (idempotent): an already-watched episode is skipped — no re-mark, no
        history row — so re-advancing is a no-op.
      - I7 (per-episode skip): episode_ids may be any subset of the round.
      - I5 (tombstone): if this drains the show's last unwatched episode, set
        removed_at and return True.

    Mutates `show` in place; the caller persists it + the returned history.
    """
    history: list[HistoryRow] = []
    advanced = 0
    for epid in episode_ids:
        for ep in show.episodes:
            if ep.id != epid:
                continue
            if ep.watched_at is not None:
                break  # I3
            ep.watched_at = now
            advanced += 1
            history.append(
                HistoryRow(
                    show_id=show.id,
                    episode_id=ep.id,
                    relative_path=ep.relative_path,
                    played_at=now,
                )
            )
            break
    removed = False
    if advanced > 0 and all_watched(show.episodes):
        show.removed_at = now
        removed = True
    return history, advanced, removed


def defer(episodes: list[Episode], episode_id: str) -> bool:
    """Re-roll a show's next pick: bump the named unwatched episode to the back
    of its queue (position = max+1) without marking it watched (contract D1-D3).
    Returns False (a no-op) if the episode is absent or already watched.
    """
    idx, max_pos = -1, 0
    for i, e in enumerate(episodes):
        if e.position > max_pos:
            max_pos = e.position
        if e.id == episode_id:
            idx = i
    if idx < 0 or episodes[idx].watched_at is not None:
        return False
    episodes[idx].position = max_pos + 1
    return True
