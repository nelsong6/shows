# ADR-0003: Revert the force-composed video-plane opacity (supersedes ADR-0002)

## Status

Accepted — 2026-06-27. Supersedes [ADR-0002](0002-force-composed-video-plane.md).

## Context

ADR-0002 applied a sub-unity DirectComposition opacity effect (0.99) to the video
visual to force it onto the DWM composition path, on the theory that present-mode
promote/demote churn ("Composed: Flip" ↔ "Hardware Composed: Independent Flip")
at focus edges was re-training the DSC/VRR link and causing the alt-tab monitor
flash. It was verified with PresentMon showing 100% Composed across alt-tabs.

Two things invalidated it:

1. **It was verified in the wrong mode.** The flash actually occurs when shows is
   **pinned** (always-on-top, titlebar hidden, video filling the client area).
   PresentMon in pinned mode shows shows is *already* stably `Composed: Flip` yet
   the flash still happens — so present-mode churn of shows' *own* plane is not the
   trigger, and pinning the plane to Composed changes nothing for the real bug.
   The earlier "fix confirmed" capture was unpinned, where the titlebar offset
   alone (video not client-filling) made the surface ineligible for independent
   flip — the opacity effect was not the cause of that result. The flash is further
   shown to be uptime-correlated and to occur with no game present, pointing at a
   topmost-overlay plane/VRR re-arbitration the app cannot pin from its own visual.

2. **It caused a visible regression.** A 0.99 opacity over the
   `WS_EX_NOREDIRECTIONBITMAP` (transparent) host window lets ~1% of the desktop
   behind the window bleed through the video — perceptible and not acceptable for a
   video player.

So the effect was pure downside: no benefit for the actual (pinned) bug, a lost
independent-flip fast path when unpinned, and visible translucency.

## Decision

Remove the opacity effect entirely. The video visual hosts the swapchain with no
DComp effect, as before ADR-0002. The video is fully opaque again.

The monitor-flash investigation continues via observability (continuous PresentMon
+ app event log + a Ctrl+Alt+F marker + a resource monitor), not via a speculative
rendering change. If a force-composed lever is ever needed for a *confirmed* cause,
the invisible near-identity color-matrix variant (not constant opacity) is the
route — but only against evidence, and gated off for HDR.

## Consequences

- Video is opaque; no background bleed-through.
- The video may again be granted independent flip when eligible (e.g. unpinned,
  client-filling). That is acceptable: it is not the confirmed cause of the flash.
- ADR-0002's SDR/HDR coupling caveat no longer applies (no effect to gate).

## Rejected alternative

**Keep ADR-0002 but switch 0.99 opacity → near-identity color-matrix** (invisible,
still force-composed). Rejected for now: it would preserve a fix that does not
address the actual pinned-mode flash, so it adds cost (no independent flip) for no
demonstrated benefit. Revisit only if observability confirms a cause that a
force-composed video plane actually fixes.
