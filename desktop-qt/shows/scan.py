"""Scan a show's directory for episode files.

The desktop is the only thing that can see the media (the AKS server has no
filesystem), so adding a show and detecting new episodes both happen here, then
sync up. Relative paths use backslashes to match the ordering hash — see
shows.ordering and docs/feature-contracts/round-and-advance.md.
"""

from __future__ import annotations

import os
import re

VIDEO_EXTS = {
    ".mp4", ".mkv", ".avi", ".mov", ".m4v", ".webm",
    ".wmv", ".flv", ".mpg", ".mpeg", ".ts", ".m2ts", ".ogv",
}


def _natural_key(s: str):
    """Sort key so S01E02 comes before S01E10 (digit runs compared as ints)."""
    return [int(t) if t.isdigit() else t.lower() for t in re.split(r"(\d+)", s)]


def scan_episodes(root_path: str) -> list[str]:
    """Backslash-joined relative paths of video files under root_path,
    recursively, natural-sorted. Non-video files are ignored."""
    out: list[str] = []
    for dirpath, _dirs, files in os.walk(root_path):
        for f in files:
            if os.path.splitext(f)[1].lower() in VIDEO_EXTS:
                rel = os.path.relpath(os.path.join(dirpath, f), root_path)
                out.append(rel.replace("/", "\\"))
    out.sort(key=_natural_key)
    return out
