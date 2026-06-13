param(
  [string]$DbPath = (Join-Path $env:APPDATA "shows\replica.db")
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path $DbPath)) {
  throw "Database not found: $DbPath"
}

$python = Get-Command python -ErrorAction SilentlyContinue
if (-not $python) {
  throw "python is required for sqlite audit"
}

$script = @'
import collections
import os
import sqlite3
import sys

db_path = sys.argv[1]
conn = sqlite3.connect(db_path)
conn.row_factory = sqlite3.Row

shows = conn.execute(
    "SELECT id, playlist, name, root_path, removed_at FROM shows ORDER BY name"
).fetchall()
episodes = conn.execute(
    "SELECT id, show_id, relative_path, watched_at FROM episodes ORDER BY show_id, position"
).fetchall()
history = conn.execute(
    "SELECT id, show_id, episode_id, relative_path FROM watch_history ORDER BY played_at"
).fetchall()
queue = conn.execute(
    "SELECT episode_id, show_id, playlist, state FROM round_queue ORDER BY play_order"
).fetchall()

show_ids = {s["id"] for s in shows}
episode_ids = {e["id"] for e in episodes}
shows_by_id = {s["id"]: s for s in shows}

def report(title, rows):
    print(f"\n== {title} ({len(rows)}) ==")
    for row in rows[:100]:
        print(row)
    if len(rows) > 100:
        print(f"... {len(rows) - 100} more")

weird_roots = []
for s in shows:
    root = s["root_path"] or ""
    if not root or root.startswith("D:\\Downloads\\Group-Nelson") or not os.path.isabs(root):
        weird_roots.append(f'{s["name"]} [{s["playlist"]}] root={root!r}')

removed_with_live = []
for s in shows:
    if not s["removed_at"]:
        continue
    live = [e for e in episodes if e["show_id"] == s["id"] and not e["watched_at"]]
    if live:
        removed_with_live.append(f'{s["name"]}: {len(live)} unwatched episode(s)')

orphan_episodes = [
    f'{e["id"]} show_id={e["show_id"]} path={e["relative_path"]}'
    for e in episodes
    if e["show_id"] not in show_ids
]
orphan_history = [
    f'{h["id"]} show_id={h["show_id"]} episode_id={h["episode_id"]} path={h["relative_path"]}'
    for h in history
    if h["show_id"] not in show_ids or h["episode_id"] not in episode_ids
]
orphan_queue = [
    f'{q["episode_id"]} show_id={q["show_id"]} playlist={q["playlist"]} state={q["state"]}'
    for q in queue
    if q["show_id"] not in show_ids or q["episode_id"] not in episode_ids
]

path_counts = collections.defaultdict(list)
for e in episodes:
    s = shows_by_id.get(e["show_id"])
    if not s:
        continue
    key = (s["root_path"].rstrip("\\/").lower(), (e["relative_path"] or "").replace("/", "\\").lower())
    path_counts[key].append((s["name"], e["id"], e["relative_path"]))
duplicate_paths = [
    f'{root}\\{rel}: ' + ", ".join(f"{name}/{eid}" for name, eid, _ in rows)
    for (root, rel), rows in path_counts.items()
    if len(rows) > 1
]

playlist_mismatch = []
for q in queue:
    s = shows_by_id.get(q["show_id"])
    if s and s["playlist"] != q["playlist"]:
        playlist_mismatch.append(
            f'{s["name"]} queue_playlist={q["playlist"]} show_playlist={s["playlist"]} episode={q["episode_id"]}'
        )

print(f"Database: {db_path}")
print(f"Shows: {len(shows)}  Episodes: {len(episodes)}  History: {len(history)}  Queue: {len(queue)}")
report("weird roots", weird_roots)
report("removed shows with live episodes", removed_with_live)
report("orphan episodes", orphan_episodes)
report("orphan history", orphan_history)
report("orphan round queue", orphan_queue)
report("duplicate absolute paths", duplicate_paths)
report("queue playlist mismatches", playlist_mismatch)
'@

$temp = New-TemporaryFile
try {
  Set-Content -LiteralPath $temp -Value $script -Encoding UTF8
  & $python.Source $temp $DbPath
} finally {
  Remove-Item -LiteralPath $temp -Force
}
