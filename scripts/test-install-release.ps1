param(
  [string]$RepoRoot = "D:\repos\shows",
  [string]$InstallPath = "D:\Downloads\shows\shows-desktop.exe",
  [switch]$NoRestart,
  [switch]$SkipSmoke
)

$ErrorActionPreference = "Stop"

$frontend = Join-Path $RepoRoot "desktop-rs\frontend"
$workspace = Join-Path $RepoRoot "desktop-rs"
$builtExe = Join-Path $workspace "target\release\shows-desktop.exe"
$logPath = Join-Path $env:APPDATA "shows\shows.log"
$cargo = Get-Command cargo -ErrorAction SilentlyContinue
if (-not $cargo) {
  $cargoPath = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"
  if (Test-Path $cargoPath) {
    $cargo = @{ Source = $cargoPath }
  } else {
    throw "cargo was not found on PATH or at $cargoPath"
  }
}

Push-Location $frontend
try {
  npm run build
} finally {
  Pop-Location
}

Push-Location $workspace
try {
  & $cargo.Source build --release -p shows-desktop
} finally {
  Pop-Location
}

Get-Process shows-desktop -ErrorAction SilentlyContinue | Stop-Process -Force

$installDir = Split-Path -Parent $InstallPath
New-Item -ItemType Directory -Force -Path $installDir | Out-Null

$copyDeadline = (Get-Date).AddSeconds(20)
while ($true) {
  try {
    Copy-Item -Force $builtExe $InstallPath
    break
  } catch {
    if ((Get-Date) -ge $copyDeadline) {
      throw
    }
    Start-Sleep -Milliseconds 500
  }
}

if ($NoRestart) {
  Write-Host "Copied $builtExe to $InstallPath"
  return
}

$startLineCount = 0
if (Test-Path $logPath) {
  $startLineCount = (Get-Content $logPath).Count
}

$appProcess = Start-Process -FilePath $InstallPath -WorkingDirectory $installDir -PassThru

$deadline = (Get-Date).AddSeconds(30)
$controlUrl = $null
while ((Get-Date) -lt $deadline -and -not $controlUrl) {
  Start-Sleep -Milliseconds 500
  if (-not (Test-Path $logPath)) {
    continue
  }
  $newLines = Get-Content $logPath | Select-Object -Skip $startLineCount
  $line = $newLines | Where-Object { $_ -match "control server on (http://127\.0\.0\.1:\d+/)" } | Select-Object -Last 1
  if ($line -and $line -match "control server on (http://127\.0\.0\.1:\d+/)") {
    $controlUrl = $Matches[1]
  }
  if (-not $controlUrl -and $appProcess -and -not $appProcess.HasExited) {
    $listener = Get-NetTCPConnection -OwningProcess $appProcess.Id -State Listen -ErrorAction SilentlyContinue |
      Where-Object { $_.LocalAddress -eq "127.0.0.1" } |
      Select-Object -First 1
    if ($listener) {
      $controlUrl = "http://127.0.0.1:$($listener.LocalPort)/"
    }
  }
}

if (-not $controlUrl) {
  throw "Started app, but could not discover control server URL in $logPath"
}

function Get-DesktopStatus {
  Invoke-RestMethod -Method Get -Uri ($controlUrl + "status")
}

function Invoke-Control {
  param([string]$Path)
  $result = Invoke-RestMethod -Method Post -Uri ($controlUrl + $Path)
  if ($null -ne $result.ok -and -not $result.ok) {
    throw "$Path failed: $($result.status) $($result.message)"
  }
  $result
}

function Wait-StatusField {
  param(
    [string]$Field,
    [object]$Expected,
    [string]$Description
  )

  $deadline = (Get-Date).AddSeconds(8)
  while ((Get-Date) -lt $deadline) {
    $status = Get-DesktopStatus
    if ($status.$Field -eq $Expected) {
      return
    }
    Start-Sleep -Milliseconds 250
  }
  $last = Get-DesktopStatus
  throw "Timed out waiting for $Description. Last $Field=$($last.$Field)"
}

$status = Get-DesktopStatus
if (-not $status.phase) {
  throw "Control server responded, but /status did not include phase"
}
if ($null -eq $status.round_pos) {
  throw "Control server responded, but /status did not include round_pos"
}

Invoke-Control "pause?state=true" | Out-Null

if (-not $SkipSmoke) {
  Invoke-Control "stay-on-top" | Out-Null
  Wait-StatusField "window_on_top" $true "stay on top to turn on"
  Invoke-Control "stay-on-top" | Out-Null
  Wait-StatusField "window_on_top" $false "stay on top to turn off"

  $beforeMaximize = [bool](Get-DesktopStatus).window_maximized
  Invoke-Control "window/maximize" | Out-Null
  Wait-StatusField "window_maximized" (-not $beforeMaximize) "maximize to toggle"
  Invoke-Control "window/maximize" | Out-Null
  Wait-StatusField "window_maximized" $beforeMaximize "maximize to toggle back"
}

Invoke-Control "pause?state=true" | Out-Null
if ($SkipSmoke) {
  Write-Host "Built, copied, restarted, and paused $InstallPath at $controlUrl"
} else {
  Write-Host "Built, copied, restarted, smoked, and paused $InstallPath at $controlUrl"
}
