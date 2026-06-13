# New Computer Setup

## NAS Mapping

Map the NAS file share as `S:`:

```powershell
New-PSDrive -Name S -PSProvider FileSystem -Root "\\192.168.50.41\files" -Persist
```

The library expects show roots under `S:\Group-Nelson` unless a show row points
elsewhere.

## Local Paths

- App install used for local release testing: `D:\Downloads\shows\shows-desktop.exe`
- Local watching cache: `D:\Downloads\Watching`
- Token cache: `%APPDATA%\shows\token.json`
- Release log: `%APPDATA%\shows\shows.log`

If `SHOWS_LOCAL_WATCHING_DIR` is set, it overrides `D:\Downloads\Watching`.

## Sync Health Check

1. Start `D:\Downloads\shows\shows-desktop.exe`.
2. Open the overlay status panel.
3. Confirm sync is online and `sync.pending == 0` after startup settles.
4. If sync is offline, confirm `S:` exists and the status `shared_db_path` is reachable.
5. Use Sync Now after repairing the drive mapping.

## File Cache Check

On a healthy round, `file_sync.cached` should match the current round size and
`missing` / `failed` should be zero after the first copy pass. If the app reports
missing files, verify the source path under `S:\Group-Nelson` and use the round
repair controls only for genuinely bad current-round entries.
