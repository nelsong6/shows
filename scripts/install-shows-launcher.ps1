param(
  [ValidateSet("stable", "dev")]
  [string]$Channel = "stable",
  [string]$NasRoot = "S:\shows-app",
  [string]$ShortcutPath = (Join-Path ([Environment]::GetFolderPath("Desktop")) "Shows.lnk"),
  [string]$DevelopmentRepo
)

$ErrorActionPreference = "Stop"
$launcher = Join-Path $NasRoot "launch-shows.ps1"
if (-not (Test-Path -LiteralPath $launcher -PathType Leaf)) { throw "Launcher not found: $launcher" }

$installRoot = Join-Path $env:LOCALAPPDATA "shows"
$bin = Join-Path $env:USERPROFILE "bin"
New-Item -ItemType Directory -Force -Path $installRoot, $bin | Out-Null
$localLauncher = Join-Path $installRoot "launch-shows.ps1"
Copy-Item -LiteralPath $launcher -Destination $localLauncher -Force
$powerShell = (Get-Command pwsh.exe -ErrorAction Stop).Source
& $localLauncher -Channel $Channel -NasRoot $NasRoot -InstallOnly
$nativeBin = Join-Path $installRoot "channels\$Channel"
$oldCommand = Join-Path $bin 'shows-remote.cmd'
if (Test-Path -LiteralPath $oldCommand -PathType Leaf) { Remove-Item -LiteralPath $oldCommand -Force }
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
[Environment]::SetEnvironmentVariable('Path', (($userPath -split ';' | Where-Object { $_ -and $_ -ine $nativeBin }) -join ';'), 'User')
$env:Path = ($env:Path -split ';' | Where-Object { $_ -and $_ -ine $nativeBin }) -join ';'

$shell = New-Object -ComObject WScript.Shell
$shortcut = $shell.CreateShortcut($ShortcutPath)
$shortcut.TargetPath = $powerShell
$shortcut.Arguments = "-NoProfile -ExecutionPolicy Bypass -File `"$localLauncher`" -Channel $Channel -NasRoot `"$NasRoot`""
$shortcut.WorkingDirectory = $installRoot
$shortcut.IconLocation = "shell32.dll,137"
$shortcut.Save()

Write-Host "Installed $Channel Shows launcher at $ShortcutPath"
if ($DevelopmentRepo) {
  $developmentScript = Join-Path ([IO.Path]::GetFullPath($DevelopmentRepo)) 'scripts\dev-shows.ps1'
  if (-not (Test-Path -LiteralPath $developmentScript)) { throw "Development entry point is missing: $developmentScript" }
  $command = '@"' + $powerShell + '" -NoLogo -NoProfile -File "' + $developmentScript + '" %*' + "`r`n"
  [IO.File]::WriteAllText((Join-Path $bin 'shows-dev.cmd'), $command, [Text.Encoding]::ASCII)
  Write-Host "Installed shows-dev from $DevelopmentRepo"
}
