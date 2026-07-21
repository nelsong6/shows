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
- **P4. Reproducible identity.** Stable builds use a source revision; dev builds
  add a publication timestamp so repeated builds of one revision remain
  immutable.

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
