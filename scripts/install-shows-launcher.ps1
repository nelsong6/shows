param(
  [ValidateSet("stable", "dev")]
  [string]$Channel = "stable",
  [string]$NasRoot = "S:\shows-app",
  [string]$ShortcutPath = (Join-Path ([Environment]::GetFolderPath("Desktop")) "Shows.lnk")
)

$ErrorActionPreference = "Stop"
$launcher = Join-Path $NasRoot "launch-shows.ps1"
if (-not (Test-Path -LiteralPath $launcher -PathType Leaf)) { throw "Launcher not found: $launcher" }

$installRoot = Join-Path $env:LOCALAPPDATA "shows"
$bin = Join-Path $installRoot "bin"
New-Item -ItemType Directory -Force -Path $bin | Out-Null
$localLauncher = Join-Path $installRoot "launch-shows.ps1"
Copy-Item -LiteralPath $launcher -Destination $localLauncher -Force
$remoteCommand = '@powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File "' + $localLauncher + '" -Channel ' + $Channel + ' -NasRoot "' + $NasRoot + '" -App remote %*'
[IO.File]::WriteAllText((Join-Path $bin "shows-remote.cmd"), $remoteCommand + "`r`n", [Text.Encoding]::ASCII)
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($bin -notin ($userPath -split ';')) {
  [Environment]::SetEnvironmentVariable("Path", ($bin + ';' + $userPath), "User")
}
if ($bin -notin ($env:Path -split ';')) { $env:Path = $bin + ';' + $env:Path }

$shell = New-Object -ComObject WScript.Shell
$shortcut = $shell.CreateShortcut($ShortcutPath)
$shortcut.TargetPath = (Get-Command powershell.exe).Source
$shortcut.Arguments = "-NoProfile -ExecutionPolicy Bypass -File `"$localLauncher`" -Channel $Channel -NasRoot `"$NasRoot`""
$shortcut.WorkingDirectory = $installRoot
$shortcut.IconLocation = "shell32.dll,137"
$shortcut.Save()

Write-Host "Installed $Channel Shows launcher at $ShortcutPath"
Write-Host "Installed shows-remote terminal command at $bin"
