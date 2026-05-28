"""Sync orchestration between the local replica and the server origin —
git-style: local-first, push at smart moments, pull to seed/reconcile,
last-write-wins.

Connectivity is *inferred*, never polled: every push/pull attempt sets `online`
(success) or clears it (failure). A failed attempt flips offline and stops; the
manual "check connectivity" action or the next natural push recovers it. The
runner never blocks on the network — playback runs entirely off the replica.
"""

from __future__ import annotations

import logging

from .apiclient import Client
from .replica import Replica

log = logging.getLogger("shows.sync")


def _wire(row: dict) -> dict:
    """A replica row shaped for the wire: drop the local-only `dirty` flag."""
    return {k: v for k, v in row.items() if k != "dirty"}


class Syncer:
    def __init__(self, replica: Replica, client: Client, playlists: list[str]):
        self.replica = replica
        self.client = client
        self.playlists = playlists
        self.online = True  # optimistic; the first failed attempt flips it

    def seed(self) -> bool:
        """Pull the library and reconcile it into the replica (initial seed and
        later pulls). Returns the resulting online state."""
        try:
            self.replica.merge_shows(self.client.get_library(self.playlists))
            self.online = True
        except Exception as e:  # noqa: BLE001 — any failure == offline, never crash playback
            log.warning("pull failed; staying on local replica: %s", e)
            self.online = False
        return self.online

    def push(self) -> bool:
        """Push dirty records and clear their dirty flags on success. No-op when
        nothing is pending. Returns the resulting online state."""
        d = self.replica.dirty()
        if not (d["shows"] or d["episodes"] or d["history"]):
            return self.online
        try:
            self.client.post_sync(
                [_wire(r) for r in d["shows"]],
                [_wire(r) for r in d["episodes"]],
                [_wire(r) for r in d["history"]],
            )
            self.replica.mark_synced("shows", [r["id"] for r in d["shows"]])
            self.replica.mark_synced("episodes", [r["id"] for r in d["episodes"]])
            self.replica.mark_synced("watch_history", [r["id"] for r in d["history"]])
            self.online = True
        except Exception as e:  # noqa: BLE001 — keep the changes queued, go offline
            log.warning("push failed; %d change(s) stay queued: %s", self.pending(), e)
            self.online = False
        return self.online

    def sync(self) -> bool:
        """Push then pull — the manual 'check connectivity' / reconcile action."""
        self.push()
        if self.online:
            self.seed()
        return self.online

    def pending(self) -> int:
        """Unpushed local changes — the git 'ahead' count."""
        p = self.replica.pending()
        return p["shows"] + p["episodes"] + p["history"]
