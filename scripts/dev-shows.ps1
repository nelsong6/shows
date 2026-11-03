#Requires -Version 7.0
[CmdletBinding(PositionalBinding = $false)]
param(
  [Parameter(Position=0)][ValidateSet('ui','desktop','remote','test','stop','status','ship')][string]$Mode = 'ui',
  [string]$Target = 'upstairspc',
  [string]$Name = 'shows-preview',
  [string]$RepoRoot,
  [Parameter(ValueFromRemainingArguments=$true)][string[]]$RemoteArgs
)
$ErrorActionPreference = 'Stop'
if (-not $RepoRoot) {
  $checkout = & git.exe rev-parse --show-toplevel 2>$null
  if ($LASTEXITCODE -eq 0 -and (Test-Path -LiteralPath (Join-Path $checkout 'desktop-rs\desktop\Cargo.toml'))) { $RepoRoot = $checkout }
  else { $RepoRoot = Split-Path -Parent $PSScriptRoot }
}
$workspace = Join-Path $RepoRoot 'desktop-rs'
$frontend = Join-Path $workspace 'frontend'
$cache = if ($env:SHOWS_DEV_TARGET_DIR) { $env:SHOWS_DEV_TARGET_DIR } else { 'D:\build\shows\cargo' }

if ($Mode -eq 'ship') {
  & (Join-Path $PSScriptRoot 'publish-shows-channel.ps1') -Channel stable -RepoRoot $RepoRoot -WaitForRelease -InstallTargets @('local', $Target)
  return
}
if ($Mode -eq 'stop') {
  $entries = & devctl.exe list $Name -Json | ConvertFrom-Json
  $entry = @($entries)[0]
  if ($entry.port -and (Get-Process -Name shows-desktop -ErrorAction SilentlyContinue)) {
    try {
      $health = Invoke-RestMethod "http://127.0.0.1:$($entry.port)/__devctl/health" -TimeoutSec 2
      if ($health.nativeDesktop) {
        Invoke-RestMethod "http://127.0.0.1:$($entry.port)/window/close" -Method Post -TimeoutSec 3 | Out-Null
        $deadline = (Get-Date).AddSeconds(5)
        while ((Get-Date) -lt $deadline) {
          try { Invoke-RestMethod "http://127.0.0.1:$($entry.port)/remote/info" -TimeoutSec 1 | Out-Null }
          catch { break }
          Start-Sleep -Milliseconds 50
        }
      }
    } catch { Write-Warning $_.Exception.Message }
  }
  & devctl.exe stop $Name
  exit $LASTEXITCODE
}
if ($Mode -eq 'status') { & devctl.exe status $Name; exit $LASTEXITCODE }
if ($Mode -in @('ui','desktop') -and -not (Test-Path -LiteralPath (Join-Path $frontend 'dev-server.mjs'))) { throw 'This worktree predates the fast development loop; update it from main first' }
if ($Mode -eq 'ui') {
  if ($Target -eq 'local') {
    $info = Invoke-RestMethod 'http://127.0.0.1:45831/remote/info' -TimeoutSec 2
    if ($info.application -ne 'shows-desktop' -or $info.protocol -ne 1) { throw 'Open the current Shows desktop on this PC first' }
  } else {
    $statePath = Join-Path $env:LOCALAPPDATA 'shows\launcher-state-stable.json'
    $installed = Get-Content -Raw -LiteralPath $statePath | ConvertFrom-Json
    if ($installed.current -notmatch '^[A-Za-z0-9._-]+$') { throw 'Invalid installed Shows version' }
    $client = Join-Path $env:LOCALAPPDATA ('shows\versions\' + $installed.current + '\shows-remote.exe')
    & $client $Target status --json | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "Open Shows on $Target before starting its live preview" }
  }
}
if ($Mode -eq 'desktop' -and (Get-Process -Name shows-desktop -ErrorAction SilentlyContinue)) {
  $owned = & devctl.exe list $Name -Json | ConvertFrom-Json
  $owned = @($owned)[0]
  if (-not $owned.port) { throw 'A separate Shows player is running; leave it running and use ui mode, or close it before native development' }
  $health = Invoke-RestMethod "http://127.0.0.1:$($owned.port)/__devctl/health" -TimeoutSec 2
  if (-not $health.nativeDesktop -or [IO.Path]::GetFullPath($health.repo_dir) -ne [IO.Path]::GetFullPath($workspace)) { throw 'The running player belongs to a different environment' }
  & $PSCommandPath stop -Name $Name -RepoRoot $RepoRoot
  if ($LASTEXITCODE -ne 0) { throw 'Could not stop the owned native preview before rebuilding' }
}
$previousEnvironment = @{}
foreach ($variable in @('CARGO_TARGET_DIR','SHOWS_DEV_TARGET','SHOWS_DEV_DESKTOP_EXE')) { $previousEnvironment[$variable] = [Environment]::GetEnvironmentVariable($variable, 'Process') }
try {
[Environment]::SetEnvironmentVariable('SHOWS_DEV_DESKTOP_EXE', $null, 'Process')
if ($Mode -in @('remote','desktop','test')) {
  $env:CARGO_TARGET_DIR = $cache
  $arguments = @('--manifest-path', (Join-Path $workspace 'Cargo.toml'), '-j', '4', '--locked')
  $started = [Diagnostics.Stopwatch]::StartNew()
  if ($Mode -eq 'test') { & cargo.exe test @arguments --workspace; exit $LASTEXITCODE }
  $package = if ($Mode -eq 'remote') { 'shows-remote' } else { 'shows-desktop' }
  & cargo.exe build @arguments -p $package
  if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
  Write-Host ("Incremental {0} build: {1:N2}s" -f $package, $started.Elapsed.TotalSeconds)
  if ($Mode -eq 'remote') {
    & (Join-Path $cache 'debug\shows-remote.exe') $Target @RemoteArgs
    exit $LASTEXITCODE
  }
  $Target = 'local'
  if (Get-Process -Name shows-desktop -ErrorAction SilentlyContinue) { throw 'Close Shows on this PC before starting its native desktop preview; ui mode can control the existing instance' }
  $env:SHOWS_DEV_DESKTOP_EXE = Join-Path $cache 'debug\shows-desktop.exe'
  $installedState = Join-Path $env:LOCALAPPDATA 'shows\launcher-state-stable.json'
  if (-not (Test-Path -LiteralPath $installedState)) { throw 'Install the current Shows bundle once to provide the libmpv runtime for native development' }
  $installed = Get-Content -Raw -LiteralPath $installedState | ConvertFrom-Json
  if ($installed.current -notmatch '^[A-Za-z0-9._-]+$') { throw 'Invalid installed Shows version' }
  $runtime = Join-Path $env:LOCALAPPDATA ('shows\versions\' + $installed.current + '\libmpv-2.dll')
  Copy-Item -LiteralPath $runtime -Destination (Join-Path $cache 'debug\libmpv-2.dll') -Force
}

if (-not (Test-Path -LiteralPath (Join-Path $frontend 'node_modules\vite\bin\vite.js'))) {
  Push-Location $frontend
  try { & npm.cmd ci; if ($LASTEXITCODE -ne 0) { throw 'Frontend dependency install failed' } }
  finally { Pop-Location }
}
$env:SHOWS_DEV_TARGET = $Target
& devctl.exe clean $Name | Out-Null
& devctl.exe up frontend -Name $Name -Cwd $frontend -Project shows
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
} finally {
  foreach ($variable in $previousEnvironment.Keys) { [Environment]::SetEnvironmentVariable($variable, $previousEnvironment[$variable], 'Process') }
}
