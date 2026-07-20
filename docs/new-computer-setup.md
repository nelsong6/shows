# New Computer Setup

## NAS Mapping

Map the NAS file share as `S:`:

```powershell
New-PSDrive -Name S -PSProvider FileSystem -Root "\\192.168.50.41\files" -Persist
```

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
powershell -ExecutionPolicy Bypass -File `
  S:\shows-app\install-shows-launcher.ps1 `
  -Channel stable
```

Use `-Channel dev` on computers that should follow locally published development
builds. The shortcut checks the channel manifest before every launch, verifies
the bundle hashes, installs it into a new versioned local directory, retains the
previous version for rollback, and starts the local executable.

## Publish a development build

From the repository computer:

```powershell
.\scripts\publish-shows-channel.ps1 -Channel dev
```

Use `-Channel stable` to advance the stable NAS channel. A channel manifest is
replaced only after the complete versioned bundle has reached the NAS.

## Database health check

The app opens `S:\shows.db` directly. If the drive or database is unavailable,
startup fails rather than reading or writing an independently authoritative
local copy. `SHOWS_SHARED_DB` overrides the path for development and tests.

## File Cache Check

On a healthy round, `file_sync.cached` should match the current round size and
`missing` / `failed` should be zero after the first copy pass. If the app reports
missing files, verify the source path under `S:\Group-Nelson` and use the round
repair controls only for genuinely bad current-round entries.
