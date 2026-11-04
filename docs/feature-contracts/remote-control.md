# Desktop remote-control boundary

The independent [shows-remote client and SSH agent](https://forgejo.romaine.life/nelsong6/shows-remote)
control an already running desktop. The desktop remains the sole owner of
playback, the authoritative NAS database, round transitions and GPU captures.
Client UI, transport, packaging and installation belong to the remote repository.

## Protocol ownership

- The desktop-owned `shows-control` crate defines protocol 1, typed commands,
  bounded messages, SSE parsing, identity validation and preview encoding. The
  remote consumes an immutable revision; release SHAs do not have to match.
- `GET /remote/info` identifies `shows-desktop` and its supported protocol before
  any controls. A missing, stopped or different listener fails explicitly.
- The listener is loopback-only at `127.0.0.1:45831`; `SHOWS_CONTROL_PORT` selects
  an explicit nonzero port for isolated development. There is no LAN listener.
- `POST /remote/control` accepts typed playback and fullscreen commands, requires
  JSON, and rejects browser Origin headers. It exposes no filesystem browsing,
  shell execution, library import or other window actions.
- The independent SSH agent runs from its independently installed native remote
  channel. The desktop creates no agent command, client PATH entry or client binary.

## Playback and acknowledgements

- Skip, previous, defer and show navigation use the same runner and
  [round invariants](round-and-advance.md) as the overlay. NAS transactions and
  SQLite commit ordering remain authoritative.
- Play and pause set an explicit state. The target checks mpv results and observed
  pause/volume values. An unloaded episode fails; no action succeeds as a no-op.
- Seek acknowledges mpv command acceptance; navigation acknowledges the runner
  result. Live SSE status establishes the resulting selected episode and position.
- Fullscreen posts the toggle only to the controlled desktop window. A missing
  window or failed post returns an error; `window_fullscreen` in SSE confirms the
  resulting state. It requires no loaded episode and changes no playback state.
- Commands are never replayed automatically. A lost acknowledgement leaves the
  outcome unknown; clients inspect live status before repeating a command.
- `GET /status/stream` is the live authoritative status feed, including database
  revision, target alerts and 30-second idle comments. Closing a client or agent
  leaves playback running.

## C6. Bounded video preview

`GET /remote/preview` samples the already rendered GPU video surface, downscaling
to 160 by 90 before CPU readback. It opens no extra decoder or file and changes
no playback or NAS state. The loopback-only route rejects browser Origin headers.
Successful captures are shared for 250 ms; concurrent captures fail explicitly
rather than queuing. Window replies time out after 500 ms. Hidden or minimized
windows report unavailability instead of returning stale video.

Rows preserve the displayed video's top-to-bottom and left-to-right orientation.
The D3D-shared surface is already in display order and must not be reversed.
Portable conversion tests and the documented real GPU orientation smoke prove
this boundary; desktop HTTP tests prove rejection, caching and no playback or
NAS mutation. The remote's client contract owns request cadence, cancellation,
terminal aspect ratio and visible stale/error states.