# Terminal remote control

`shows-remote` is a separate console executable shipped with the desktop. Its
interactive UI and one-shot commands control an already running Shows instance.
The companion never initializes libmpv, opens SQLite, starts a desktop, or owns a
round. The target owns playback and all authoritative NAS transactions.

## Transport and identity

- **T0. Choose an instance.** Opening the app without a target checks the shared
  home machine inventory and displays an fzt instance picker. It includes this
  PC, explains unavailable computers, supports refresh, and never starts a player.
  Explicit CLI targets skip discovery; one-shot commands never silently choose
  a computer. The at-menu entry launches the generic app without a target.

- **T1. HTTP stays local.** The desktop listens on `127.0.0.1:45831`.
  `SHOWS_CONTROL_PORT` may select another nonzero port for isolated tests or a
  configured installation; desktop and agent must agree. The native client
  rejects non-loopback HTTP addresses and does not follow redirects.
- **T2. Existing SSH access.** Windows OpenSSH uses batch authentication, strict
  known-host verification, connect timeout and keepalives. Targets are computer
  hostnames or SSH aliases, containing letters, digits, dots and hyphens, starting
  with a letter or digit. Shows issues no credentials.
- **T3. Commands are data.** The validated channel selects a fixed remote startup
  native entry point. Commands travel as bounded JSON lines on SSH stdin, never
  interpolated shell text. After verifying a version, the launcher writes
  `agents/<channel>.cmd` beside `launcher-state-<channel>.json`, selecting that
  version's executable. SSH starts it directly without a PowerShell pipeline
  or NAS access, preserving protocol stdin/stdout and Unicode.
- **T4. Identity precedes controls.** `GET /remote/info` must identify
  `shows-desktop` and protocol 1. A missing, stopped, or different listener fails
  explicitly. Every new connection checks identity.
- **T5. Narrow native surface.** `POST /remote/control` accepts typed playback
  commands and fullscreen toggle, requires JSON, and rejects browser Origin
  headers. It exposes no library import, filesystem browsing, shell execution,
  or other window actions.

## Playback and acknowledgement

- **C0. Discoverable actions.** `/` opens the fzt command palette, advertised on
  screen. It provides playback and current-round show selection when controls
  are available, plus reconnect, change instance, and quit. Cancel executes
  nothing. Switching closes the previous agent, leaves its player unchanged,
  and returns to instance selection. Status queued while the palette is open
  is consumed before another playback command is accepted.
  **Toggle fullscreen** (`f`) remains available while connected and idle even
  without a loaded episode. It acts only on the controlled desktop window.
  Success acknowledges posting the window's toggle message; the authoritative
  `window_fullscreen` SSE field confirms the resulting state. Missing windows
  or failed UI-message posting return errors; toggles are never replayed.

- **C1. Existing round semantics.** Skip, previous, defer and show navigation use
  the overlay's runner. [round-and-advance.md](round-and-advance.md) invariants
  I7, D1–D3, P1–P2 and M1–M3 remain unchanged. Name selection resolves uniquely
  within the target's current round. Ambiguous or absent names issue no play.
- **C2. Explicit play/pause.** They set a state, never toggle implicitly. The
  target checks mpv's result and observed pause/volume values before success.
  Seek reports mpv command acceptance; navigation reports the runner result.
  Subsequent live status establishes which episode loaded. No loaded episode
  means failure, never a successful no-op.
- **C3. No automatic command replay.** Requests have a bounded response wait.
  A lost acknowledgement means the outcome may be unknown; inspect status before
  repeating a command, especially skip, defer, previous and relative seek.
- **C4. Target-owned live state.** The interactive UI uses `/status/stream`, not
  snapshot polling. Playback fields update only on target snapshots. Target
  alerts, waits, request failures and disconnects remain visible.
- **C5. Disconnect disables controls.** Initial replies and commands time out
  after 12 seconds. SSH keepalives detect broken peers. No status or SSE heartbeat
  activity for 65 seconds disconnects the UI; idle SSE comments arrive every 30
  seconds. `r` explicitly reconnects with a fresh agent and replays no command.
  Quitting tears down the owned SSH child and leaves TV playback running.

- **C6. Opt-in video view.** `v` and the fzt palette switch between the round
  list and an ASCII video preview. The target alone reads its already-rendered
  GPU video surface, downscaling to 160×90 before CPU readback. No extra decoder,
  filesystem capture, NAS read or playback mutation is introduced. The native
  loopback-only `GET /remote/preview` route rejects browser Origin headers;
  the SSH agent carries a bounded `preview` read through the existing protocol.
  Successful captures are shared across clients for 250 ms, and concurrent
  capture requests fail explicitly instead of queuing. The UI has at most one
  read outstanding, requests no more than four per second, backs off on errors,
  and stops requesting when the view is off or the palette owns the terminal.
  The UI hides frames after two seconds, reports loading/unavailable/disconnected
  states, and keeps playback results separate from preview results. Hidden or
  minimized target windows report unavailability instead of stale video. All
  waits for a window reply time out after 500 ms; no request is
  replayed as a playback action. Terminal sizing preserves the source surface's
  aspect ratio and validates dimensions and printable ASCII before rendering.
  Preview rows preserve the displayed video's top-to-bottom and left-to-right
  orientation. The D3D-shared surface is already in display row order; ASCII
  conversion must not reverse it.

## Distribution and evidence

The executable belongs to the same immutable, hash-verified channel bundle as
`shows-desktop.exe` and `libmpv-2.dll`; see
[desktop-distribution.md](desktop-distribution.md). The existing installer adds
a local launcher and `shows-remote` command to PATH.

CI covers existing round/SQLite invariants, HTTP identity and rejection, bounded
messages, SSE parsing, CLI-to-agent subprocesses, native desktop route failures,
terminal render/state behavior and channel installation. Cross-computer checks
use released executables and restore playback state after play/pause checks.
