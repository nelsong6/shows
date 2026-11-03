[CmdletBinding(PositionalBinding = $false)]
param(
  [ValidateSet("stable", "dev")]
  [string]$Channel = "stable",
  [string]$NasRoot = "S:\shows-app",
  [string]$InstallRoot = (Join-Path $env:LOCALAPPDATA "shows"),
  [switch]$NoUpdate,
  [switch]$Wait,
  [switch]$InstallOnly
)

$ErrorActionPreference = "Stop"
$versions = Join-Path $InstallRoot "versions"
$statePath = Join-Path $InstallRoot "launcher-state-$Channel.json"
New-Item -ItemType Directory -Force -Path $versions | Out-Null

$state = if (Test-Path -LiteralPath $statePath) {
  Get-Content -Raw -LiteralPath $statePath | ConvertFrom-Json
} else { $null }

function Save-State([object]$nextState) {
  $temp = "$statePath.$([guid]::NewGuid().ToString('N')).tmp"
  [IO.File]::WriteAllText($temp, ($nextState | ConvertTo-Json -Depth 4), (New-Object Text.UTF8Encoding $false))
  Move-Item -Force -LiteralPath $temp -Destination $statePath
}

function Install-Version([object]$manifest) {
  if ($manifest.schema -ne 1) { throw "Unsupported channel manifest schema: $($manifest.schema)" }
  if ($manifest.channel -ne $Channel) { throw "Channel manifest is for $($manifest.channel), expected $Channel" }
  if ($manifest.version -notmatch '^[A-Za-z0-9._-]+$') { throw "Unsafe version in manifest" }
  $fileNames = @($manifest.files | ForEach-Object { $_.name })
  foreach ($required in @("shows-desktop.exe", "shows-remote.exe", "libmpv-2.dll")) {
    if ($required -notin $fileNames) { throw "Manifest is missing $required" }
  }

  $destination = Join-Path $versions $manifest.version
  if (-not (Test-Path -LiteralPath $destination -PathType Container)) {
    $source = Join-Path (Join-Path $NasRoot "versions") $manifest.version
    $stage = Join-Path $InstallRoot ".staging-$([guid]::NewGuid().ToString('N'))"
    New-Item -ItemType Directory -Path $stage | Out-Null
    try {
      foreach ($file in $manifest.files) {
        if ($file.name -notmatch '^[A-Za-z0-9._-]+$') { throw "Unsafe file name in manifest" }
        $sourceFile = Join-Path $source $file.name
        $targetFile = Join-Path $stage $file.name
        Copy-Item -LiteralPath $sourceFile -Destination $targetFile
        $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $targetFile).Hash.ToLowerInvariant()
        if ($actual -ne $file.sha256) { throw "Hash mismatch for $($file.name)" }
        if ((Get-Item -LiteralPath $targetFile).Length -ne $file.length) { throw "Length mismatch for $($file.name)" }
      }
      Move-Item -LiteralPath $stage -Destination $destination
    } finally {
      if (Test-Path -LiteralPath $stage) { Remove-Item -Recurse -Force -LiteralPath $stage }
    }
  }
  foreach ($file in $manifest.files) {
    $installedFile = Join-Path $destination $file.name
    if (-not (Test-Path -LiteralPath $installedFile -PathType Leaf)) { throw "Installed file is missing: $installedFile" }
    if ((Get-Item -LiteralPath $installedFile).Length -ne $file.length) { throw "Installed length mismatch for $($file.name)" }
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $installedFile).Hash.ToLowerInvariant()
    if ($actual -ne $file.sha256) { throw "Installed hash mismatch for $($file.name)" }
  }
  return $destination
}

$selectedVersion = if ($state -and $state.current) { [string]$state.current } else { $null }
if (-not $NoUpdate) {
  try {
    $manifestPath = Join-Path $NasRoot "channels\$Channel.json"
    $manifest = Get-Content -Raw -LiteralPath $manifestPath | ConvertFrom-Json
    $null = Install-Version $manifest
    if ($selectedVersion -ne $manifest.version) {
      Save-State ([ordered]@{
        schema = 1
        channel = $Channel
        current = $manifest.version
        previous = $selectedVersion
        updated_at = (Get-Date).ToUniversalTime().ToString("o")
      })
      $selectedVersion = $manifest.version
    }
  } catch {
    if (-not $selectedVersion) { throw }
    $updateMessage = "Update failed; starting installed version $selectedVersion. $($_.Exception.Message)"
    Write-Warning $updateMessage
  }
}

if (-not $selectedVersion) { throw "No installed shows version and updates are disabled or unavailable" }
if ($selectedVersion -notmatch '^[A-Za-z0-9._-]+$') { throw 'Invalid installed version' }
$selectedDir = Join-Path $versions $selectedVersion
$exe = Join-Path $selectedDir "shows-desktop.exe"
if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) { throw "Installed executable is missing: $exe" }

$agents = Join-Path $InstallRoot 'agents'
New-Item -ItemType Directory -Force -Path $agents | Out-Null
$agentTemp = Join-Path $agents "$Channel.$([guid]::NewGuid().ToString('N')).tmp"
$agentCommand = '@"%~dp0..\versions\' + $selectedVersion + '\shows-remote.exe" --agent' + "`r`n"
[IO.File]::WriteAllText($agentTemp, $agentCommand, (New-Object Text.UTF8Encoding $false))
Move-Item -Force -LiteralPath $agentTemp -Destination (Join-Path $agents "$Channel.cmd")

# Expose the verified version as a native entry point, following the home tools'
# version/current junction model. Never remove a real directory or its contents.
$channels = [IO.Path]::GetFullPath((Join-Path $InstallRoot 'channels'))
New-Item -ItemType Directory -Force -Path $channels | Out-Null
$entry = Join-Path $channels $Channel
$next = $entry + '.next'
$previous = $entry + '.previous'
foreach ($path in @($entry, $next, $previous)) {
  if (-not ([IO.Path]::GetFullPath($path)).StartsWith($channels + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) { throw 'Unsafe native entry path' }
  $item = Get-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue
  if ($item -and $item.LinkType -ne 'Junction') { throw "Refusing to replace non-junction $path" }
}
if (Test-Path -LiteralPath $previous) {
  if (-not (Test-Path -LiteralPath $entry)) { [IO.Directory]::Move($previous, $entry) }
  else { [IO.Directory]::Delete($previous) }
}
if (Test-Path -LiteralPath $next) { [IO.Directory]::Delete($next) }
$existing = Get-Item -LiteralPath $entry -Force -ErrorAction SilentlyContinue
if (-not $existing -or [IO.Path]::GetFullPath([string]$existing.Target) -ne [IO.Path]::GetFullPath($selectedDir)) {
  New-Item -ItemType Junction -Path $next -Target $selectedDir | Out-Null
  try {
    if ($existing) { [IO.Directory]::Move($entry, $previous) }
    try { [IO.Directory]::Move($next, $entry) }
    catch { if (Test-Path -LiteralPath $previous) { [IO.Directory]::Move($previous, $entry) }; throw }
    if (Test-Path -LiteralPath $previous) { [IO.Directory]::Delete($previous) }
  } finally { if (Test-Path -LiteralPath $next) { [IO.Directory]::Delete($next) } }
}
if ($InstallOnly) { return }

$process = Start-Process -FilePath $exe -WorkingDirectory $selectedDir -PassThru
if ($Wait) { $process.WaitForExit(); exit $process.ExitCode }
