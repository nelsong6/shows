# Developing Shows

Preview changes before opening a PR. Release packaging happens after iteration,
not between each edit and trying it.

From a Shows worktree, use `shows-dev` (installed with
`scripts/install-shows-launcher.ps1 -DevelopmentRepo D:\repos\shows`), or invoke
`scripts/dev-shows.ps1` directly. The command detects the current Shows worktree.

```powershell
shows-dev ui -Target upstairspc
shows-dev desktop
shows-dev test
shows-dev stop
```

## UI edits

`ui` starts Vite through the existing generic `devctl frontend` definition with
the Shows working directory and project. It prints a named localhost URL. HMR
applies source edits without rebuilding Rust, packaging, restarting the player,
or copying another executable. The page title identifies the controlled PC.

For an upstairs target, Vite owns an authenticated SSH loopback tunnel to its
running desktop. It proxies the real API and SSE stream. It starts no local
player, opens no independent database, and copies no media. Playback and library
controls affect the actual target and authoritative NAS state. Remote window
close/move/fullscreen controls are rejected: shell work uses native preview.

The preview proves its identity at `/__devctl/health`; `/api/__devctl/health`
checks the target's application/protocol and the local transport process. A dead
SSH child ends the preview process so devctl's bounded restart policy owns
recovery. `devctl list`, `devctl logs <name>` and `devctl stop <name>` remain
authoritative; no separate tunnel or dev-server process is left untracked.

## Rust edits

The terminal remote has its own [repository and development entry point](https://forgejo.romaine.life/nelsong6/shows-remote).
`desktop` builds the native
shell and starts it under the tracked Vite environment. Native previews start
paused and use the real local engine/cache; the first media cache pass may take
time. Use `ui` for menu work against a player whose files are already cached.

Debug artifacts live in `D:\build\shows\cargo` across worktrees. Override with
`SHOWS_DEV_TARGET_DIR`. Every local Cargo command uses four jobs. The command
prints rebuild time; it never runs an optimized release build for a preview.

The ASCII preview orientation also has a hardware smoke test. From `desktop-rs`,
with the installed bundle's `libmpv-2.dll` on PATH, run:

```powershell
cargo test --locked -j 4 --target-dir D:\build\shows\cargo -p shows-desktop gl::tests::preview_preserves_rendered_video_orientation -- --ignored
```

It uses a hidden GPU surface and an isolated white-top/black-bottom image,
without opening a player window or accessing the library. It requires
`WGL_NV_DX_interop2`, so CI's normal workspace tests leave it ignored; the
portable conversion test checks all four corners and terminal resizing.

Debug desktops require an explicit HTTP loopback `SHOWS_DEV_URL`, supplied by
the managed preview. They do not embed `frontend/dist`, so Rust debugging needs
no frontend build. Releases always embed assets and reject `SHOWS_DEV_URL`.
`SHOWS_DIST_DIR` and the fixed-path release-copy smoke script are retired.

`desktop` needs the current installed Shows bundle for libmpv. It gracefully
restarts its own preview before rebuilding and refuses to replace a separate
local player. The supervisor owns both Vite and its
native child. `shows-dev stop` requests a graceful native close before stopping
the environment, preserving the normal resume transaction.

## Shipping once

After local verification, open the PR. UI-only PRs run frontend checks without
Rust compilation. Rust/script PRs run debug workspace and installation checks.
Only main (or an explicit package dispatch) builds the release bundle. Build
identity is set for both optimized tests and compilation, avoiding a second
workspace recompilation just to add version metadata.

After CI passes and the PR merges:

```powershell
shows-dev ship -Target upstairspc
```

This waits for the exact main release, publishes it through the existing NAS
channel publisher, and installs it locally and on the target. It reuses existing
SSH and an on-demand interactive user task, so an update reaches the actual TV
session rather than a hidden SSH desktop. A running player closes gracefully,
resumes its selected episode and retains its pause state; a closed player stays
closed. The owned task registration is removed after the result is recorded.
Timeouts and failed releases/installations fail explicitly.

Publication retries verify the immutable existing files against the CI artifact
and selected channel before proceeding with installation. Stable still ships
only CI's main build; previews never replace stable binaries.
