# Terminal remote control

`shows-remote` is a separate console executable shipped with the desktop. Its
interactive UI and one-shot commands control an already running Shows instance.
The companion never initializes libmpv, opens SQLite, starts a desktop, or owns a
round. The target owns playback and all authoritative NAS transactions.

## Transport and identity

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
  commands, requires JSON, and rejects browser Origin headers. It exposes no
  library import, filesystem browsing, or shell/window actions.

## Playback and acknowledgement

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

## Distribution and evidence

The executable belongs to the same immutable, hash-verified channel bundle as
`shows-desktop.exe` and `libmpv-2.dll`; see
[desktop-distribution.md](desktop-distribution.md). The existing installer adds
a local launcher and `shows-remote` command to PATH.

CI covers existing round/SQLite invariants, HTTP identity and rejection, bounded
messages, SSE parsing, CLI-to-agent subprocesses, native desktop route failures,
terminal render/state behavior and channel installation. Cross-computer checks
use released executables and restore playback state after play/pause checks.
