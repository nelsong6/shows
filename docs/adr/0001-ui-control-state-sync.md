# ADR-0001: Sync UI controls to authoritative state via optimistic + idempotent reconciliation

## Status

Accepted — 2026-06-26.

## Context

Several overlay controls reflect state that does **not** live in the React app:
it lives in the native window / libmpv — pin (stay-on-top), pause, volume,
fullscreen, maximize. The overlay is a windowless WebView2 with no direct handle
on that state; it learns the truth only through the one-way `/status/stream`
Server-Sent Events feed. That channel:

- is **lossy and laggy** — frames are coalesced to "latest", and it makes no
  exactly-once or in-order guarantee for intermediate states;
- arrives **asynchronously while the user is still acting** (clicks, keybinds);
- can deliver a single user gesture to the host as **more than one event** at the
  native input seam.

A control built as a non-idempotent **toggle** ("flip whatever you currently
are") corrupts under all three conditions: a duplicated or reordered delivery
flips an even number of times and lands wrong; a command whose target is
*derived from the echoed state* races that echo and can invert itself. This is
not hypothetical — it produced a whole class of pin bugs (bounce-back, "enable
works but disable doesn't", apparent double-fires), which are the known failure
signature of the toggle approach. It is a standard, solved frontend problem, not
a project-specific puzzle.

## Decision

Every overlay control that mirrors authoritative server/native state uses one
pattern:

1. **Desired and observed are separate values.** Hold the user's *desired* value
   locally, independent of the *observed* value from the status stream. Never
   derive the command from the observed value.
2. **Optimistic update.** Reflect the desired value in the UI immediately, before
   confirmation, so the control feels instant.
3. **Idempotent `set` command — never a flip.** Send the desired *value* (e.g.
   `POST /stay-on-top {"on_top": true}`). Duplicated, dropped, retried, or
   reordered delivery is then harmless.
4. **Reconcile, gated by an in-flight flag.** When the stream echoes the observed
   state, converge: accept it when it equals the desired value or there is no
   pending desired; otherwise keep enforcing (re-send). One request in flight at
   a time; a newer intent supersedes the pending desired. The in-flight gate is
   what stops a single gesture from dispatching contradictory writes.

This is the optimistic-UI + reconciliation pattern with idempotent writes; the
in-flight flag is the standard guard against duplicate/contradictory dispatch.

References: [optimistic UI pattern](https://www.freecodecamp.org/news/how-to-use-the-optimistic-ui-pattern-with-the-useoptimistic-hook-in-react/),
[optimistic updates + in-flight guard](https://www.sitepoint.com/react-useoptimistic-production-patterns-for-instant-ui-updates/),
[idempotency vs toggle](https://particular.net/blog/what-does-idempotent-mean).

## Consequences

- Every such control is an instance of one template, not a bespoke design. New
  controls follow it; reviewers reject toggles for remote-state controls.
- The reference implementations **already in the tree** are `volumeSync` and
  `pauseSync` in `desktop-rs/frontend/src/App.tsx` — `{ desired, inFlight }`, a
  pump that re-sends on divergence, and a reconcile effect that protects
  `desired` from a stale echo. Read those before adding or changing a control.
- Backend control endpoints for these controls take a desired value and are
  idempotent (a request matching current state is a no-op); they do not flip
  internal state.
- **Outstanding:** the pin / stay-on-top control does **not** yet follow this
  pattern — it is a backend-owned toggle. It is to be migrated to an `onTopSync`
  mirroring `pauseSync`, with `/stay-on-top` taking `{"on_top": bool}` as an
  idempotent set. Until then it is the known-nonconformant control.

## Rejected alternative

**Backend-owned toggle** — the frontend posts a contentless command and the host
flips its own boolean. Simpler, and perfectly fine on a reliable, in-order,
exactly-once channel. This is not that channel. The toggle is non-idempotent and
corrupts under duplicate / dropped / laggy delivery — the exact defect this ADR
exists to prevent.
