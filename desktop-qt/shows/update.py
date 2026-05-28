"""Best-effort 'is a newer build out?' check, run once at launch.

The repo is public, so GitHub's releases API needs no auth. This never raises
into playback: any problem (offline, rate-limited, or a dev/source run with no
embedded SHA) just returns None and the overlay shows no banner.

The build SHA is stamped into the bundle at package time as `shows/_build.py`
(see .github/workflows/build-desktop.yaml); a source checkout has no such file,
so dev runs are treated as "version unknown" and skip the check.
"""

from __future__ import annotations

import logging
from typing import Optional

import httpx

log = logging.getLogger("shows.update")

RELEASES_LATEST = "https://api.github.com/repos/nelsong6/shows/releases/latest"
_TAG_PREFIX = "desktop-"  # release tags are desktop-<short-sha> (build-desktop.yaml)


def current_sha() -> Optional[str]:
    """The short SHA this bundle was built from, or None on a source/dev run."""
    try:
        from shows import _build  # type: ignore  # written into the bundle at build time
    except Exception:
        return None
    sha = (getattr(_build, "SHA", "") or "").strip()
    return sha or None


def check(timeout_s: float = 8.0) -> Optional[dict]:
    """Compare this build to GitHub's latest release. Returns
    {latest, current, url} when a different (newer) release exists, else None."""
    cur = current_sha()
    if not cur:
        return None  # dev build — nothing meaningful to compare against
    try:
        r = httpx.get(RELEASES_LATEST, timeout=timeout_s,
                      headers={"Accept": "application/vnd.github+json"})
        r.raise_for_status()
        data = r.json()
    except Exception as e:  # noqa: BLE001 — best-effort; offline/rate-limited is fine
        log.info("update check skipped: %s", e)
        return None
    tag = data.get("tag_name") or ""
    latest = tag[len(_TAG_PREFIX):] if tag.startswith(_TAG_PREFIX) else tag
    if latest and latest != cur:
        return {"latest": latest, "current": cur, "url": data.get("html_url") or ""}
    return None
