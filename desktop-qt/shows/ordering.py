"""Deterministic-random round ordering — Python port of
internal/ordering/ordering.go. Must produce bit-identical order_values to
the Go server (and the legacy play_ordered_show.ps1) so the desktop and
server agree on round order.

Contract: hash the absolute path with SHA-256, take the first four hex
chars of the digest, parse as a uint32. Sort ascending by that value,
ties broken by episode_id. See docs/feature-contracts/round-and-advance.md.
"""

from __future__ import annotations

import hashlib
from dataclasses import dataclass

# Backslash always — the hash input must match the legacy Windows paths
# regardless of the OS this runs on. NOT os.path.join.
PATH_SEPARATOR = "\\"


def join_path(root_path: str, relative_path: str) -> str:
    r = root_path.rstrip("\\/")
    p = relative_path.lstrip("\\/")
    return r + PATH_SEPARATOR + p


def order_value(absolute_path: str) -> int:
    digest = hashlib.sha256(absolute_path.encode("utf-8")).hexdigest()
    return int(digest[:4], 16)


@dataclass
class Candidate:
    episode_id: str
    show_id: str
    root_path: str
    relative_path: str


@dataclass
class Ordered:
    episode_id: str
    show_id: str
    absolute_path: str
    order_value: int


def sort_round(candidates: list[Candidate]) -> list[Ordered]:
    out = [
        Ordered(
            episode_id=c.episode_id,
            show_id=c.show_id,
            absolute_path=(abs_ := join_path(c.root_path, c.relative_path)),
            order_value=order_value(abs_),
        )
        for c in candidates
    ]
    out.sort(key=lambda o: (o.order_value, o.episode_id))
    return out
