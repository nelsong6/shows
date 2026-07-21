param(
  [ValidateSet("stable", "dev")]
  [string]$Channel = "stable",
  [string]$NasRoot = "S:\shows-app",
  [string]$ShortcutPath = (Join-Path ([Environment]::GetFolderPath("Desktop")) "Shows.lnk")
)

$ErrorActionPreference = "Stop"
$launcher = Join-Path $NasRoot "launch-shows.ps1"
if (-not (Test-Path -LiteralPath $launcher -PathType Leaf)) { throw "Launcher not found: $launcher" }

$shell = New-Object -ComObject WScript.Shell
$shortcut = $shell.CreateShortcut($ShortcutPath)
$shortcut.TargetPath = (Get-Command powershell.exe).Source
$shortcut.Arguments = "-NoProfile -ExecutionPolicy Bypass -File `"$launcher`" -Channel $Channel -NasRoot `"$NasRoot`""
$shortcut.WorkingDirectory = $NasRoot
$shortcut.IconLocation = "shell32.dll,137"
$shortcut.Save()

Write-Host "Installed $Channel Shows launcher at $ShortcutPath"
