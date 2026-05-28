"""Local SQLite replica — the desktop's working copy of the library under the
offline-first design. All reads/writes for playback happen here; the server is
a durable origin we sync with (git-style: local changes are marked `dirty` and
pushed at smart moments; pulls reconcile last-write-wins by `updated_at`).

The engine (engine.py) holds the domain logic over plain dataclasses; this layer
loads rows into those dataclasses, runs the engine, and persists the results —
marking touched rows dirty and bumping updated_at.

Concurrency: one connection guarded by a lock, since the runner thread, the
control-server thread, and the Qt thread all touch it. Volume is tiny.
"""

from __future__ import annotations

import sqlite3
import threading
import uuid
from datetime import datetime, timezone
from typing import Optional

from . import engine
from .engine import Episode, Show

_SCHEMA = """
CREATE TABLE IF NOT EXISTS shows (
  id         TEXT PRIMARY KEY,
  playlist   TEXT NOT NULL,
  name       TEXT NOT NULL,
  root_path  TEXT NOT NULL,
  date_added TEXT,
  removed_at TEXT,
  updated_at TEXT NOT NULL,
  dirty      INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS episodes (
  id            TEXT PRIMARY KEY,
  show_id       TEXT NOT NULL,
  relative_path TEXT NOT NULL,
  position      INTEGER NOT NULL,
  watched_at    TEXT,
  resume_pos    REAL,
  updated_at    TEXT NOT NULL,
  dirty         INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_episodes_show ON episodes(show_id);
CREATE TABLE IF NOT EXISTS watch_history (
  id            TEXT PRIMARY KEY,
  show_id       TEXT NOT NULL,
  episode_id    TEXT NOT NULL,
  relative_path TEXT,
  played_at     TEXT NOT NULL,
  dirty         INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);
"""


def _now() -> str:
    return datetime.now(timezone.utc).isoformat()


def _dt(ts: str) -> datetime:
    return datetime.fromisoformat(ts.replace("Z", "+00:00"))


def _newer(a: Optional[str], b: Optional[str]) -> bool:
    """True if timestamp a is strictly newer than b (None == oldest)."""
    if a is None:
        return False
    if b is None:
        return True
    return _dt(a) > _dt(b)


class Replica:
    def __init__(self, path: str):
        self._lock = threading.Lock()
        self._db = sqlite3.connect(path, check_same_thread=False)
        self._db.row_factory = sqlite3.Row
        self._db.executescript(_SCHEMA)
        self._db.commit()

    def close(self) -> None:
        with self._lock:
            self._db.close()

    # ── reads ──────────────────────────────────────────────────────────
    def _load_show(self, row: sqlite3.Row) -> Show:
        eps = [
            Episode(
                id=e["id"],
                relative_path=e["relative_path"],
                position=e["position"],
                watched_at=e["watched_at"],
                resume_pos=e["resume_pos"],
            )
            for e in self._db.execute(
                "SELECT * FROM episodes WHERE show_id=? ORDER BY position", (row["id"],)
            )
        ]
        return Show(
            id=row["id"],
            playlist=row["playlist"],
            name=row["name"],
            root_path=row["root_path"],
            episodes=eps,
            removed_at=row["removed_at"],
            date_added=row["date_added"],
        )

    def _show_locked(self, show_id: str) -> Optional[Show]:
        r = self._db.execute("SELECT * FROM shows WHERE id=?", (show_id,)).fetchone()
        return self._load_show(r) if r else None

    def active_shows(self, playlists: list[str]) -> list[Show]:
        """Non-removed shows in the given playlists, with episodes — the input to
        engine.next_round (pass several playlists for a cross-playlist round)."""
        if not playlists:
            return []
        marks = ",".join("?" * len(playlists))
        with self._lock:
            rows = self._db.execute(
                f"SELECT * FROM shows WHERE removed_at IS NULL AND playlist IN ({marks})",
                playlists,
            ).fetchall()
            return [self._load_show(r) for r in rows]

    def show(self, show_id: str) -> Optional[Show]:
        with self._lock:
            return self._show_locked(show_id)

    def resume_pos(self, episode_id: str) -> Optional[float]:
        with self._lock:
            r = self._db.execute(
                "SELECT resume_pos FROM episodes WHERE id=?", (episode_id,)
            ).fetchone()
            return r["resume_pos"] if r else None

    def reveal(self, show_id: str) -> dict:
        """The 'just finished' payload for a tombstoned show — name, date added,
        and last play time (from watch_history) — for the overlay reveal."""
        with self._lock:
            s = self._db.execute(
                "SELECT name, date_added FROM shows WHERE id=?", (show_id,)
            ).fetchone()
            h = self._db.execute(
                "SELECT MAX(played_at) AS last FROM watch_history WHERE show_id=?", (show_id,)
            ).fetchone()
            return {
                "id": show_id,
                "name": s["name"] if s else "",
                "date_added": (s["date_added"] if s else None) or "",
                "last_played_at": (h["last"] if h and h["last"] else None) or "",
            }

    def overlay_shows(self, playlists: list[str]) -> list[dict]:
        """Active shows for the dashboard sidebar (no episodes), as plain dicts —
        offline-capable replacement for the old GET /api/playlists call."""
        return [
            {
                "id": s.id, "playlist": s.playlist, "name": s.name,
                "root_path": s.root_path, "date_added": s.date_added or "",
                "removed_at": s.removed_at,
            }
            for s in self.active_shows(playlists)  # acquires the lock itself
        ]

    def show_history(self, show_id: str) -> list[dict]:
        """Watch-history rows for a show, oldest first — backs the overlay's
        per-show history view from the local replica."""
        with self._lock:
            rows = self._db.execute(
                "SELECT episode_id, relative_path, played_at FROM watch_history"
                " WHERE show_id=? ORDER BY played_at",
                (show_id,),
            ).fetchall()
            return [dict(r) for r in rows]

    # ── mutations (local-first: mark dirty + bump updated_at) ───────────
    def advance(self, entries: list[tuple[str, str]], now: Optional[str] = None) -> tuple[int, list[str]]:
        """Mark (show_id, episode_id) pairs watched via the engine; persist
        watched_at/removed_at, append history. Returns (newly-watched, removed show ids)."""
        now = now or _now()
        by_show: dict[str, list[str]] = {}
        for sid, eid in entries:
            by_show.setdefault(sid, []).append(eid)
        advanced_total = 0
        removed: list[str] = []
        with self._lock:
            for sid, eids in by_show.items():
                sh = self._show_locked(sid)
                if sh is None:
                    continue
                history, n, tombstoned = engine.advance(sh, eids, now)
                if n == 0:
                    continue
                for h in history:
                    self._db.execute(
                        "UPDATE episodes SET watched_at=?, updated_at=?, dirty=1 WHERE id=?",
                        (now, now, h.episode_id),
                    )
                    self._db.execute(
                        "INSERT INTO watch_history(id,show_id,episode_id,relative_path,played_at,dirty)"
                        " VALUES(?,?,?,?,?,1)",
                        (str(uuid.uuid4()), h.show_id, h.episode_id, h.relative_path, h.played_at),
                    )
                if tombstoned:
                    self._db.execute(
                        "UPDATE shows SET removed_at=?, updated_at=?, dirty=1 WHERE id=?",
                        (now, now, sid),
                    )
                    removed.append(sid)
                advanced_total += n
            self._db.commit()
        return advanced_total, removed

    def defer(self, show_id: str, episode_id: str, now: Optional[str] = None) -> bool:
        now = now or _now()
        with self._lock:
            sh = self._show_locked(show_id)
            if sh is None or not engine.defer(sh.episodes, episode_id):
                return False
            ep = next(e for e in sh.episodes if e.id == episode_id)
            self._db.execute(
                "UPDATE episodes SET position=?, updated_at=?, dirty=1 WHERE id=?",
                (ep.position, now, episode_id),
            )
            self._db.commit()
            return True

    def set_resume(self, episode_id: str, pos: Optional[float], now: Optional[str] = None) -> None:
        now = now or _now()
        with self._lock:
            self._db.execute(
                "UPDATE episodes SET resume_pos=?, updated_at=?, dirty=1 WHERE id=?",
                (pos, now, episode_id),
            )
            self._db.commit()

    # ── library management (local-first; syncs up like any change) ──────
    def create_show(self, playlist: str, name: str, root_path: str,
                    episodes: list[str], now: Optional[str] = None) -> str:
        """Create a show + its episode queue from a scanned file list. Returns
        the new show id; rows are dirty so the next sync creates them upstream
        (SyncUpsert creates unknown shows/episodes)."""
        now = now or _now()
        sid = str(uuid.uuid4())
        with self._lock:
            self._db.execute(
                "INSERT INTO shows(id,playlist,name,root_path,date_added,removed_at,updated_at,dirty)"
                " VALUES(?,?,?,?,?,NULL,?,1)",
                (sid, playlist, name, root_path, now, now),
            )
            for i, rel in enumerate(episodes):
                self._db.execute(
                    "INSERT INTO episodes(id,show_id,relative_path,position,watched_at,resume_pos,updated_at,dirty)"
                    " VALUES(?,?,?,?,NULL,NULL,?,1)",
                    (str(uuid.uuid4()), sid, rel, i, now),
                )
            self._db.commit()
        return sid

    def update_show(self, show_id: str, *, name: Optional[str] = None,
                    root_path: Optional[str] = None, playlist: Optional[str] = None,
                    now: Optional[str] = None) -> bool:
        now = now or _now()
        sets, vals = [], []
        if name is not None:
            sets.append("name=?"); vals.append(name)
        if root_path is not None:
            sets.append("root_path=?"); vals.append(root_path)
        if playlist is not None:
            sets.append("playlist=?"); vals.append(playlist)
        if not sets:
            return False
        sets += ["updated_at=?", "dirty=1"]
        vals += [now, show_id]
        with self._lock:
            cur = self._db.execute(f"UPDATE shows SET {', '.join(sets)} WHERE id=?", vals)
            self._db.commit()
            return cur.rowcount > 0

    def remove_show(self, show_id: str, now: Optional[str] = None) -> bool:
        """Tombstone a show (removes it from rotation). Soft-delete — the engine
        skips removed shows; the tombstone syncs up."""
        now = now or _now()
        with self._lock:
            cur = self._db.execute(
                "UPDATE shows SET removed_at=?, updated_at=?, dirty=1 WHERE id=?",
                (now, now, show_id),
            )
            self._db.commit()
            return cur.rowcount > 0

    def add_episodes(self, show_id: str, rels: list[str], now: Optional[str] = None) -> int:
        """Append new episodes to a show, positions continuing from max+1 (the
        new-episode-detection path). Returns how many were added."""
        if not rels:
            return 0
        now = now or _now()
        with self._lock:
            rows = self._db.execute(
                "SELECT position FROM episodes WHERE show_id=?", (show_id,)
            ).fetchall()
            start = max((r["position"] for r in rows), default=-1) + 1
            for i, rel in enumerate(rels):
                self._db.execute(
                    "INSERT INTO episodes(id,show_id,relative_path,position,watched_at,resume_pos,updated_at,dirty)"
                    " VALUES(?,?,?,?,NULL,NULL,?,1)",
                    (str(uuid.uuid4()), show_id, rel, start + i, now),
                )
            self._db.commit()
            return len(rels)

    def episode_paths(self, show_id: str) -> set[str]:
        """Relative paths already known for a show — for diffing a rescan."""
        with self._lock:
            rows = self._db.execute(
                "SELECT relative_path FROM episodes WHERE show_id=?", (show_id,)
            ).fetchall()
            return {r["relative_path"] for r in rows}

    # ── seed / reconcile (pull): upsert last-write-wins ─────────────────
    def merge_shows(self, shows: list[dict]) -> None:
        """Upsert shows + episodes pulled from the server. Last-write-wins by
        updated_at; a locally-dirty row is kept (local unsynced change wins until
        it's pushed — git-style). Used for the initial seed and later pulls.

        Each show dict: id, playlist, name, root_path, date_added, removed_at,
        updated_at, episodes:[{id, relative_path, position, watched_at,
        resume_pos, updated_at}].
        """
        with self._lock:
            for s in shows:
                cur = self._db.execute(
                    "SELECT updated_at, dirty FROM shows WHERE id=?", (s["id"],)
                ).fetchone()
                if cur is None:
                    self._db.execute(
                        "INSERT INTO shows(id,playlist,name,root_path,date_added,removed_at,updated_at,dirty)"
                        " VALUES(?,?,?,?,?,?,?,0)",
                        (s["id"], s["playlist"], s["name"], s["root_path"],
                         s.get("date_added"), s.get("removed_at"), s["updated_at"]),
                    )
                elif not cur["dirty"] and _newer(s["updated_at"], cur["updated_at"]):
                    self._db.execute(
                        "UPDATE shows SET playlist=?,name=?,root_path=?,date_added=?,removed_at=?,updated_at=?,dirty=0"
                        " WHERE id=?",
                        (s["playlist"], s["name"], s["root_path"], s.get("date_added"),
                         s.get("removed_at"), s["updated_at"], s["id"]),
                    )
                for e in s.get("episodes", []):
                    self._merge_episode_locked(s["id"], e)
            self._db.commit()

    def _merge_episode_locked(self, show_id: str, e: dict) -> None:
        cur = self._db.execute(
            "SELECT updated_at, dirty FROM episodes WHERE id=?", (e["id"],)
        ).fetchone()
        if cur is None:
            self._db.execute(
                "INSERT INTO episodes(id,show_id,relative_path,position,watched_at,resume_pos,updated_at,dirty)"
                " VALUES(?,?,?,?,?,?,?,0)",
                (e["id"], show_id, e["relative_path"], e["position"],
                 e.get("watched_at"), e.get("resume_pos"), e["updated_at"]),
            )
        elif not cur["dirty"] and _newer(e["updated_at"], cur["updated_at"]):
            self._db.execute(
                "UPDATE episodes SET relative_path=?,position=?,watched_at=?,resume_pos=?,updated_at=?,dirty=0"
                " WHERE id=?",
                (e["relative_path"], e["position"], e.get("watched_at"),
                 e.get("resume_pos"), e["updated_at"], e["id"]),
            )

    # ── sync push: dirty rows out, then clear ───────────────────────────
    def pending(self) -> dict:
        """Count of unpushed local changes — the git 'ahead' number."""
        with self._lock:
            return {
                "shows": self._count("shows"),
                "episodes": self._count("episodes"),
                "history": self._count("watch_history"),
            }

    def _count(self, table: str) -> int:
        return self._db.execute(
            f"SELECT COUNT(*) AS n FROM {table} WHERE dirty=1"
        ).fetchone()["n"]

    def dirty(self) -> dict:
        """The records to push upstream."""
        with self._lock:
            return {
                "shows": [dict(r) for r in self._db.execute("SELECT * FROM shows WHERE dirty=1")],
                "episodes": [dict(r) for r in self._db.execute("SELECT * FROM episodes WHERE dirty=1")],
                "history": [dict(r) for r in self._db.execute("SELECT * FROM watch_history WHERE dirty=1")],
            }

    def mark_synced(self, table: str, ids: list[str]) -> None:
        """Clear dirty on rows confirmed pushed."""
        if not ids:
            return
        marks = ",".join("?" * len(ids))
        with self._lock:
            self._db.execute(f"UPDATE {table} SET dirty=0 WHERE id IN ({marks})", ids)
            self._db.commit()
