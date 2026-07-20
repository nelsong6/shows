param(
  [ValidateSet("stable", "dev")]
  [string]$Channel = "stable",
  [string]$NasRoot = "S:\shows-app",
  [string]$InstallRoot = (Join-Path $env:LOCALAPPDATA "shows"),
  [switch]$NoUpdate,
  [switch]$Wait
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
  $nextState | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $temp -Encoding utf8NoBOM
  Move-Item -Force -LiteralPath $temp -Destination $statePath
}

function Install-Version([object]$manifest) {
  if ($manifest.schema -ne 1) { throw "Unsupported channel manifest schema: $($manifest.schema)" }
  if ($manifest.channel -ne $Channel) { throw "Channel manifest is for $($manifest.channel), expected $Channel" }
  if ($manifest.version -notmatch '^[A-Za-z0-9._-]+$') { throw "Unsafe version in manifest" }
  $fileNames = @($manifest.files | ForEach-Object { $_.name })
  foreach ($required in @("shows-desktop.exe", "libmpv-2.dll")) {
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
    Write-Warning "Update failed; starting installed version $selectedVersion. $($_.Exception.Message)"
  }
}

if (-not $selectedVersion) { throw "No installed shows version and updates are disabled or unavailable" }
$selectedDir = Join-Path $versions $selectedVersion
$exe = Join-Path $selectedDir "shows-desktop.exe"
if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) { throw "Installed executable is missing: $exe" }

$process = Start-Process -FilePath $exe -WorkingDirectory $selectedDir -PassThru
if ($Wait) { $process.WaitForExit(); exit $process.ExitCode }
