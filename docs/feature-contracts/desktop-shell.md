# Desktop Shell Contract

The desktop is one DirectComposition window: libmpv renders video into a
composition visual, and a **windowless** (composition-hosted) WebView2 overlay —
the React UI — is layered on top. Because the overlay has no HWND of its own, the
Win32 host forwards pointer input into it and owns hit-testing, cursor, and
window state. This contract fixes the behavior at that seam so the window *feels*
like a native video player.

The shell state lives in `desktop-rs/desktop/src/compositor.rs`; the overlay
behavior lives in `desktop-rs/frontend/src/App.tsx`. Field changes are mirrored
in [`desktop-status.md`](desktop-status.md).

## Window modes

Three orthogonal flags, surfaced in status:

- `window_maximized` — standard maximize/restore.
- `window_fullscreen` — borderless fullscreen on the current monitor. Hides the
  custom titlebar; the video fills the client area.
- `window_on_top` ("pin mode") — the window floats above others (topmost
  Z-order) **and hides the custom titlebar** so the video fills the window and a
  compact floating player reclaims the space. It is still not a separate window
  style — no native frame swap. Because there is no titlebar to grab, the bare
  video surface is the drag handle (see Pin-mode dragging). Exit via the pin
  button in the control bar or `i`, which restores the titlebar.

  `POST /stay-on-top` is an **idempotent set**, not a flip: the body carries the
  desired state (`{"on_top": bool}`). A request matching the current state is a
  no-op. This is necessary but **NOT sufficient** to fix the pin button — see
  [Pin toggle: one click, one command](#pin-toggle-one-click-one-command), which
  is the hard-won part. Read it before touching the pin logic.

There is no "mini player" / picture-in-picture mode. It was removed: it swapped
in a native window frame and a parallel control surface purely to approximate
"always on top," which `window_on_top` now does directly. Per the migration
policy, the old path (`window_pip`, `mini_pointer_inside`, `/pip`, the
`mini-controls` PiP coupling, the native-frame swap) is deleted, not kept behind
a flag.

## Pin toggle: one click, one command

`window_on_top` is harder than the other remote-state controls (pause, volume)
for one reason: **it is echoed only on change.** Pause and volume ride every mpv
heartbeat, so a missed reconcile self-heals on the next frame; `window_on_top` is
emitted only when `WM_SET_STAY_ON_TOP` actually flips `is_on_top`, so a stranded
optimistic value would *never* recover. ADR-0001's pattern is necessary but not
sufficient here. The additional invariants:

1. **One optimistic value for intent.** `pinned`/`onTopRef` (overlay) is the
   single source for both the button highlight *and* the direction of the next
   toggle. It updates immediately on click (instant feedback — the lag was what
   made the button feel like it "didn't take"). It is never read from
   `status.window_on_top`.
2. **Roll back on a lost command.** If a `/stay-on-top` POST fails, the overlay
   resets the optimistic value to the host-confirmed `hostOnTopRef`. Otherwise
   the next click derives the wrong direction and hits the host's idempotent
   no-op — a silent dead toggle.
3. **The drag handle gates on host truth, never on intent.** Exactly one drag
   affordance must exist at all times: the native caption when not pinned, the
   overlay surface-drag when pinned — never zero, never both. Both the host
   (`nc_hit has_titlebar = !is_on_top`) and the overlay (`surfaceDragProps` gate,
   titlebar render condition) key off the *same* host-confirmed value
   (`status.window_on_top` / `hostOnTopRef`), so they can never disagree about
   whether a caption exists. A failed command that desynced intent from host
   truth must not be able to suppress the surface drag while the caption is also
   gone.
4. **The host re-asserts `is_on_top` after every transition that can change
   Z-order.** Exiting fullscreen restores a pre-pin exstyle that lacks
   `WS_EX_TOPMOST`, so the fullscreen-exit path re-applies `HWND_TOPMOST` from the
   live `is_on_top`. Pin entry clears `WS_MAXIMIZE` (`SW_RESTORE`) before the
   aspect snap so the floating window keeps working resize borders. The
   invariant: **whenever fullscreen/maximize changes, real Z-order and the
   emitted `window_on_top` are reconciled back to `is_on_top`.**

## Chrome geometry — one source of truth, DPI-correct

The custom titlebar's dimensions are defined **once**, in logical (96-DPI)
pixels, and both sides scale them by the window DPI so the draggable/clickable
regions line up exactly with what the overlay renders:

| Constant | Logical px | Win32 (`compositor.rs`) | CSS (`App.css`) |
| --- | --- | --- | --- |
| Titlebar height | 32 | `titlebar_height_px` (`× dpi/96`) | `.titlebar { height: 32px }` |
| Window-button cluster width | 138 | `nc_hit` buttons zone (`× dpi/96`) | `.titlebar-actions { width: 138px }` |
| Resize border | 6 | `nc_hit` edge band (`× dpi/96`) | n/a (synthesized by hit-test) |

The previous bug was a flat `32`/`140` in the hit-test compared against physical
pixels while the overlay rendered DPI-scaled: above 100% DPI the bottom strip of
the visible titlebar stopped dragging and the button zone drifted. The invariant
now is: **any pixel the user sees as titlebar drags; any pixel they see as a
window button clicks; at every DPI.**

## Hit-testing (`nc_hit`)

A pure function of `(client_x, client_y, width, height, dpi, is_maximized)`
returns the `HT*` code, so it is unit-tested at 96/120/144 DPI:

- **Not maximized:** the outer resize-border band → `HTTOP*`/`HTBOTTOM*`/
  `HTLEFT`/`HTRIGHT` (and corners). Within the titlebar height, left of the
  button cluster → `HTCAPTION` (drag). Everything else → `HTCLIENT`.
- **Maximized:** no resize band. Titlebar-minus-buttons → `HTCAPTION`; rest →
  `HTCLIENT`.
- **Fullscreen:** entire window → `HTCLIENT`.

In **pin mode** the titlebar is hidden, so `nc_hit` is called with
`has_titlebar = false`: there is no caption strip (the whole interior is
`HTCLIENT`, forwarded to the overlay), but the resize borders still work.

## Pin-mode dragging

With no titlebar there is no native caption to grab, so the window is moved by
dragging the bare video surface. The overlay owns this decision because it knows
the DOM: a left press on a non-control surface that then travels past a small
threshold (so plain clicks and double-clicks still work) calls
`POST /window/begin-drag`, and the host starts the standard move loop
(`ReleaseCapture` + `WM_NCLBUTTONDOWN HTCAPTION`). Controls stay clickable —
they are normal `HTCLIENT` and the overlay never treats a press on them as a
drag.

## Pin-mode aspect lock

Pin mode is a floating player, so wasted screen space (letterbox bars) is the
enemy. The window is therefore locked to the video's display aspect ratio
**only while on top**:

- **On entering** pin mode the titlebar is removed, so the old shape (sized for
  video + titlebar) would letterbox the video. The window snaps to the video
  aspect — top-left and width kept, height derived — so no space is wasted.
- **While resizing** (`WM_SIZING`) the proposed window rect is constrained to the
  video aspect by `constrain_rect_aspect`: a side edge drives its own axis and
  derives the other; a corner derives height from width and pins the dragged
  corner so the fixed corner stays put. The handler returns `TRUE` so the OS
  reads the adjusted rect back.

Because pin mode has no non-client frame, the window rect equals the
client/video rect, so locking the window shape locks the visible video shape
directly. The ratio comes from mpv's `video-params/aspect` (`GlVideo::video_aspect`);
with no video loaded yet there is nothing to lock, so resizing is unconstrained
until the first frame's params are known. Windowed mode keeps free resizing.

## Input forwarding

- Client-area mouse messages are forwarded to the overlay via `SendMouseInput`
  (the `COREWEBVIEW2_MOUSE_EVENT_KIND` ids equal the `WM_*` ids).
- Non-client moves over the titlebar are forwarded as `MOVE` so overlay buttons
  still show hover.
- `LEAVE` is forwarded only when the pointer actually leaves the window
  (`is_cursor_in_window` is false), so moving between the client area and the
  caption does not flicker hover state.
- A same-position `WM_MOUSEMOVE` (synthesized by keyboard/media input) does not
  reveal the idle-hidden cursor; only real displacement does (`CursorMotion`).

## Controls visibility — single owner

The overlay owns exactly one visible/hidden state and one idle timer. No native
pointer-inside signal, no second "max visible" cap timer. Only while the video is
**actively advancing** (`phase == "playing"` **and** `playback.paused` is false)
is the control bar hidden after `CONTROLS_IDLE_MS` of no real pointer movement.
`phase` stays `"playing"` across a pause (it tracks the round, not playback), so
the paused check is required — otherwise the bar and cursor would hide while
paused, leaving the controls invisible and unclickable. It is kept open (auto-
hide disabled) while any of these hold:

- the video is paused,
- the settings/dashboard panel is open,
- the pointer is down on the controls (scrubbing),
- the pointer is hovering the controls.

The now-playing HUD and current queue marker are bound to the active round entry
(`phase == "playing"`, `round`, `round_pos`), not to whether the playback clock
is advancing. Pause and mute may annotate the label, but they do not clear the
current episode or disable round controls such as previous, skip, and defer.

Real pointer movement reveals the bar and re-arms the idle timer; the pointer
leaving the window hides it. The control bar adapts to **window width**
(`compactViewport`), not to any window mode — a small window gets the compact
layout whether or not it is on top.

## Render policy

The render pump runs on a ~60fps timer: each tick renders the current mpv frame
into the shared texture, presents the video swapchain, and commits the
DirectComposition device (the WebView overlay's visual-hosting output only
appears after a device commit).

The video visual carries a sub-unity opacity effect (0.99) so DWM **always
composites** it and never promotes it to a hardware overlay / independent-flip
plane. This is deliberate: the promote/demote churn at focus edges re-trained the
DSC/VRR display link and caused a monitor flash on alt-tab. It assumes an SDR
swapchain — forcing composition also defeats HDR passthrough, so the effect must
be gated off if HDR is ever enabled. See ADR-0002.

**Deferred (not yet implemented):** presenting video only on a new mpv frame
(`mpv_render_context_update`'s `FRAME` flag) to stop re-presenting an unchanged
frame while paused/idle. An attempt at this regressed first paint to a blank
window — in visual-hosting the overlay's appearance is coupled to the per-tick
present/commit in a way that needs live verification before the present can be
gated. Until that is proven on real hardware, the pump presents every tick.
