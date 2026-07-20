# shows

A Windows TV-show player that selects one episode from each active show, orders the round deterministically, advances watched episodes, and repeats.

## Architecture

The desktop app is the complete runtime. Every computer opens the same SQLite database on the NAS (by default `S:\shows.db`), so a committed write is immediately authoritative. There is no local database replica or background sync policy: if two apps edit the same value, the transaction that commits last wins.

```text
Windows computers
  └─ shows-desktop.exe (Rust + libmpv + WebView2/React)
       ├─ reads media from the NAS
       └─ opens S:\shows.db directly
```

SQLite serializes writers, and the app waits up to 15 seconds for a concurrent writer. A shared database revision lets an open app notice commits made by another computer and refresh its UI. A round already being played remains stable; external edits take effect on the next database read or round boundary.

The historical Go/Cosmos service remains in the repository while the infrastructure is retired, but it is not part of the desktop runtime.

## Desktop distribution

The NAS also hosts immutable desktop builds and two explicit update channels:

- `stable` for normal use
- `dev` for testing a local development build on another computer

Publish a build from the repository root:

```powershell
.\scripts\publish-shows-channel.ps1 -Channel dev
```

On each computer, install a small desktop shortcut once:

```powershell
S:\shows-app\install-shows-launcher.ps1 -Channel stable
```

The shortcut checks the selected channel, verifies the published hashes, installs the version under `%LOCALAPPDATA%\shows\versions`, and launches the local copy. Publishing updates the channel manifest only after every artifact is complete, so other computers never observe a partial release. See [desktop distribution](./docs/feature-contracts/desktop-distribution.md) and [new-computer setup](./docs/new-computer-setup.md).

## Ordering

For each round, the desktop selects the next unwatched episode from each active show, then sorts by:

```text
uint32(first 4 hex chars of SHA-256(UTF-8(root_path + "\\" + relative_path)))
```

This reproduces the predecessor PowerShell ordering exactly. See the [round-and-advance contract](./docs/feature-contracts/round-and-advance.md).

## Development

The Rust workspace is under `desktop-rs/`; its React overlay is under `desktop-rs/frontend/`. See [desktop local testing](./docs/desktop-local-testing.md) for the build and test-install workflow.
