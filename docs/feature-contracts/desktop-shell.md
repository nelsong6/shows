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
- `window_on_top` — the window floats above others (topmost Z-order). **Nothing
  else changes**: same custom chrome, same titlebar, same hit-testing, same
  single control bar, same size and position. It is a Z-order toggle, not a
  separate window style.

There is no "mini player" / picture-in-picture mode. It was removed: it swapped
in a native window frame and a parallel control surface purely to approximate
"always on top," which `window_on_top` now does directly. Per the migration
policy, the old path (`window_pip`, `mini_pointer_inside`, `/pip`, the
`mini-controls` PiP coupling, the native-frame swap) is deleted, not kept behind
a flag.

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

`window_on_top` does not change hit-testing — it uses the windowed/maximized
rules unchanged.

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
pointer-inside signal, no second "max visible" cap timer. During `phase ==
"playing"` the control bar is hidden after `CONTROLS_IDLE_MS` of no real pointer
movement. It is kept open while any of these hold:

- the settings/dashboard panel is open,
- the pointer is down on the controls (scrubbing),
- the pointer is hovering the controls.

Real pointer movement reveals the bar and re-arms the idle timer; the pointer
leaving the window hides it. The control bar adapts to **window width**
(`compactViewport`), not to any window mode — a small window gets the compact
layout whether or not it is on top.

## Render policy

The render pump runs on a ~60fps timer: each tick renders the current mpv frame
into the shared texture, presents the video swapchain, and commits the
DirectComposition device (the WebView overlay's visual-hosting output only
appears after a device commit).

**Deferred (not yet implemented):** presenting video only on a new mpv frame
(`mpv_render_context_update`'s `FRAME` flag) to stop re-presenting an unchanged
frame while paused/idle. An attempt at this regressed first paint to a blank
window — in visual-hosting the overlay's appearance is coupled to the per-tick
present/commit in a way that needs live verification before the present can be
gated. Until that is proven on real hardware, the pump presents every tick.
