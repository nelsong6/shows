# New Computer Setup

## NAS Mapping

Map the NAS file share as `S:` by hostname, never by IP:

```powershell
New-PSDrive -Name S -PSProvider FileSystem -Root "\\NelsonNAS\files" -Persist
```

A mapping to a literal IP dies if the NAS's address ever moves: `net use`
reports `S:` as `Unavailable`, and the app refuses to start because it cannot
open `S:\shows.db`.

The library expects show roots under `S:\Group-Nelson` unless a show row points
elsewhere.

## Local Paths

- Versioned app installs: `%LOCALAPPDATA%\shows\versions\<version>`
- Local watching cache: `D:\Downloads\Watching`
- Release log: `%APPDATA%\shows\shows.log`

If `SHOWS_LOCAL_WATCHING_DIR` is set, it overrides `D:\Downloads\Watching`.

## Install the launcher

After a channel has been published to `S:\shows-app`, run:

```powershell
pwsh -ExecutionPolicy Bypass -File `
  S:\shows-app\install-shows-launcher.ps1 `
  -Channel stable
```

Use `-Channel dev` on computers that should follow locally published development
builds. The shortcut checks the channel manifest before every launch, verifies
the bundle hashes, installs it into a new versioned local directory, retains the
previous version for rollback, and starts the local executable.

Shows remote is installed separately from its own signed release on both the
controller and target PC. It exposes `shows-remote` and its SSH agent at
`%LOCALAPPDATA%\shows-remote\channels\stable`. The desktop installer does not
install or update it. See [Shows Remote](shows-remote.md).

## Publish a build

Advance the stable channel to CI's latest release (or name one with
`-Release desktop-<sha>`); nothing is compiled locally:

```powershell
.\scripts\publish-shows-channel.ps1 -Channel stable
```

Publish a local development build to the dev channel:

```powershell
.\scripts\publish-shows-channel.ps1 -Channel dev
```

A channel manifest is replaced only after the complete versioned bundle has
reached the NAS.

## Database health check

The app opens `S:\shows.db` directly. If the drive or database is unavailable,
startup fails rather than reading or writing an independently authoritative
local copy. `SHOWS_SHARED_DB` overrides the path for development and tests.

## File Cache Check

On a healthy round, `file_sync.cached` should match the current round size and
`missing` / `failed` should be zero after the first copy pass. If the app reports
missing files, verify the source path under `S:\Group-Nelson` and use the round
repair controls only for genuinely bad current-round entries.

## Development setup

PowerShell 7, Rust, Node, devctl and an installed Shows bundle are required.
Install the development command from the default-branch checkout:

```powershell
.\scripts\install-shows-launcher.ps1 -DevelopmentRepo D:\repos\shows
```

`shows-dev` selects the current Shows worktree when invoked inside one. See
[Developing Shows](development.md) for previews, warm builds and one-step shipping.
