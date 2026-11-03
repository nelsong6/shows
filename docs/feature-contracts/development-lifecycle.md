# Development lifecycle

- **D1. Preview precedes release.** UI edits use tracked Vite HMR. Debug Rust
  builds use a shared incremental cache and at most four jobs. Neither preview
  path compiles an optimized release, publishes a channel, or waits for CI.
- **D2. Real state stays authoritative.** Browser previews proxy the selected
  running engine over local HTTP or existing SSH. They do not run another player,
  open SQLite, or hold a replica. Native previews run the ordinary local engine
  paused; round/advance/defer/resume transactions remain those in
  [round-and-advance.md](round-and-advance.md).
- **D3. Processes have owners.** devctl owns the frontend, SSH transport and any
  native preview. Health proves frontend worktree/configuration and actual target
  reachability. Stopping a native preview requests its normal close first.
- **D4. Development cannot replace release UI.** Debug builds require an explicit
  HTTP loopback Vite URL and do not compile embedded frontend assets. Release
  builds embed their own assets and reject development URLs. The filesystem
  `SHOWS_DIST_DIR` path is removed.
- **D5. Checks match the change.** Frontend-only PRs compile/test the frontend.
  Rust or script changes also run workspace/debug installation checks. Optimized
  tests, packaging and publication run on main once iteration is complete.
- **D6. Shipping is one operation.** The existing publisher waits for the exact
  main release and can install verified bundles on named PCs. It preserves
  [desktop-distribution.md](desktop-distribution.md) immutable files, hashes,
  channels and local execution. Retrying an existing publication requires the
  same artifact hashes and selected channel.
- **D7. Updates preserve the viewing session.** A running desktop closes via its
  normal resume-saving control and restarts in the interactive user session.
  Pause state is restored after loading. A stopped desktop is not started. SSH,
  release and installer failures are explicit and bounded; the installer issues
  no new credentials and removes its owned task registration on completion.

Verification includes a live source edit reaching Vite without a frontend PID
change, a real target status stream through SSH, warm Rust build timings,
debug/release URL tests, channel install/retry checks, and an actual interactive
TV update retaining its episode and paused state.
