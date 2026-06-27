# ADR-0002: Force the video plane to DWM composition to stop the focus-change display re-sync

## Status

Accepted — 2026-06-26. Empirically validated at the present-mode layer (see
Decision); end-to-end mechanism confidence ~0.65, with long-run real-use
confirmation ongoing because the symptom is intermittent.

## Context

On a 4K@144 DisplayPort link with DSC compression and VRR (49–144 Hz), the
monitor briefly blanks / re-syncs (a "flash") when the user alt-tabs to or from
the desktop app. The app is the **only** program on the affected machine that
triggers it; no GPU TDR/driver reset occurs, so the event is a display-link
reconfiguration, not a crash. System-side mitigations (disabling MPO, VRR/G-Sync,
or changing resolution) were ruled out as unacceptable: only this one app
misbehaves, so the fix must be in the app.

The app renders libmpv video into a flip-model composition swapchain hosted by a
DirectComposition `bottom` visual, with a transparent WebView2 overlay in a
sibling `web` visual on top. That video surface is borderless, client-filling,
opaque, and flip-model — i.e. it meets **all** the criteria DWM uses to grant a
hardware overlay plane ("Hardware Composed: Independent Flip" / MPO). Browsers
and other windowed apps on the machine do not meet all of them at once, which
explains why this app is the unique trigger.

PresentMon (`--v1_metrics`, `PresentMode` column) confirmed the behavior
objectively: while the app is foreground it runs as **Hardware Composed:
Independent Flip**, and it falls back to **Composed: Flip** when it loses
foreground. DWM thus *promotes* the surface to a dedicated scanout plane on focus
gain and *demotes* it on focus loss. That per-focus-edge change to the scanout
plane topology is the candidate trigger for the DSC/VRR link reconfiguration —
the flash. (The first two links of that chain are documented; that a plane
retopology *by itself* re-trains the link is a strong inference, not documented
end-to-end — hence the ~0.65 confidence.)

## Decision

Make the video surface ineligible for the hardware-overlay / independent-flip
scanout path so DWM **always** composites it, removing the promote/demote churn
at focus edges. This is done with a DirectComposition opacity effect just below
unity on the `bottom` (video) visual, set once at compositor creation:

```rust
let video_fx = dcomp.CreateEffectGroup()?;   // IDCompositionDevice::CreateEffectGroup
video_fx.SetOpacity2(0.99)?;                  // IDCompositionEffectGroup::SetOpacity2
bottom.SetEffect(&video_fx)?;                 // IDCompositionVisual::SetEffect
```

A sub-unity opacity forces DWM to alpha-blend the visual. A multiplane-overlay
plane supports only `OPAQUE` or *per-pixel* `ALPHABLEND` — there is **no
constant/scalar plane-alpha mode** — so a uniform 0.99 over opaque video cannot
be expressed as an overlay and DWM must composite. The lever is decisive, not a
half-measure.

Verification: with the effect applied and the window confirmed foreground,
PresentMon showed **592/592** frames `Composed: Flip` (zero Independent Flip),
and a 45 s real-use capture across alt-tabs showed **1104/1104** `Composed: Flip`
with **0** present-mode transitions — versus a baseline that was predominantly
Independent Flip while foreground. The promotion churn is eliminated.

## Consequences

- The video plane is permanently composited by DWM; it never takes the
  independent-flip fast path. The cost is a per-frame compose (slightly more
  GPU/power and no direct VRR scanout for the video). For a video player this is
  an acceptable trade for a stable display link.
- The effect is scoped to the `bottom` video visual only. The WebView2 overlay
  is a sibling visual and is **not** dimmed.
- **SDR-coupled (the one trap to remember).** Forcing composition is the same
  path that disables HDR passthrough. The swapchain is currently 8-bit
  `B8G8R8A8_UNORM` (SDR), so there is no live regression. If HDR / 10-bit is ever
  enabled, this effect must be **gated off** for the HDR swapchain/colorspace or
  it will clamp/tonemap and lose HDR passthrough.
- The 0.99 opacity costs ~1% luminance. It is imperceptible and band-free, but if
  ever objectionable it can be replaced by a near-identity color-matrix effect
  (`IDCompositionDevice3::CreateColorMatrixEffect`, RGB diagonal `255/256` — not
  strict identity, which DWM may elide as a no-op and re-promote). Re-verify with
  PresentMon after any such change.
- Any future change near presentation should be graded with the PresentMon
  `PresentMode` method above, not by eye — the symptom is intermittent.

## Rejected alternatives

- **System-side mitigations** (disable MPO/VRR, change resolution) — rejected by
  constraint: only this app misbehaves, so the fix belongs in the app.
- **Tearing / sync-interval-0 presentation** (`DXGI_PRESENT_ALLOW_TEARING`,
  `Present(0, …)`) — tried as an experiment and **refuted**: a rigorous PresentMon
  run still showed present-mode transitions. It targets direct-flip
  `CreateSwapChainForHwnd` swapchains, not a DWM-composited composition swapchain.
- **Focus-coupled candidates** — deferring the `WM_SETFOCUS → MoveFocus` overlay
  handoff (#1) and gating the per-tick `dcomp.Commit()` to change-only (#2). Held
  in reserve: they only become relevant if the flash persists *despite* the
  present mode being pinned to Composed (which would refute the mechanism here).
  Not applied, to keep one variable at a time.
