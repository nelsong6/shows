# Desktop distribution

The launcher and NAS channel publisher keep installed desktop builds converged
without running an executable from the network share.

## Publishing invariants

- **P1. Immutable versions.** A version is published once under
  `shows-app/versions/<version>` and is never overwritten.
- **P2. Complete before visible.** The publisher stages the executable and
  libmpv runtime, records their SHA-256 hashes and lengths, then moves the whole
  bundle into `versions`. Only afterward does it replace the channel manifest.
- **P3. Explicit channels.** `stable` and `dev` have independent manifests.
  Computers opt into one channel through their shortcut.
- **P4. Reproducible identity.** Stable publishes the bundle CI attached to a
  numbered `v0.1.N` Forgejo release (pipeline-templates' release-version step),
  versioned by that number, so the stable channel only ever carries a build CI
  produced from `main`. The executable is stamped with the same number. Dev builds
  are local, never numbered, and named `dev-<publication time>` so repeated builds
  remain immutable.
- **P5. Signed Windows binaries.** CI signs the desktop executable and libmpv with the
  existing home code-signing identity, verifies Authenticode and its pinned
  thumbprint, then archives the signed files. Manifests hash the signed bytes.

## Installation invariants

- **I1. Verify before selection.** Every file is copied into a staging directory
  and checked against the manifest before the version becomes selectable.
- **I2. Never replace a running binary.** Versions install into distinct local
  directories under `%LOCALAPPDATA%\shows\versions`.
- **I3. Atomic selection.** Launcher state is replaced only after verification
  and installation succeed.
- **I4. Recoverable failure.** If NAS access, copying, or verification fails and
  a current version exists, the launcher starts that version. A first install
  with no valid version fails explicitly.
- **I5. Rollback retained.** Launcher state records both `current` and `previous`;
  the previous version directory is not deleted during an update.
- **I6. Local execution.** The selected executable starts from its local version
  directory so video startup does not depend on executing binaries over SMB.
- **I7. Independent terminal client.** Desktop bundles contain only
  `shows-desktop.exe` and `libmpv-2.dll`. The remote is built, signed and installed
  by [its own repository](https://forgejo.romaine.life/nelsong6/shows-remote).
  Desktop installation removes the retired `agents/<channel>.cmd` entry point
  and its remote PATH entry; it never installs a client or agent. The new client
  and player interoperate through protocol 1 despite independent release versions.

## Automated updates

`publish-shows-channel.ps1 -WaitForRelease -InstallTargets local,upstairspc`
waits for the exact CI release of main before publication, then updates the
named PCs through existing SSH. An existing publication is reused only after
its files match the downloaded artifact and it is still the selected channel.
The installer uses PowerShell 7; no Windows PowerShell 5.1 launcher is retained.
A running desktop closes through its resume-saving control, restarts in the
interactive user session and regains its pause state. A closed app stays closed.
See [development-lifecycle.md](development-lifecycle.md).
