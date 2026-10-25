param(
  [ValidateSet("stable", "dev")]
  [string]$Channel = "dev",
  [string]$RepoRoot = (Split-Path -Parent $PSScriptRoot),
  [string]$NasRoot = "S:\shows-app",
  [string]$Release,
  [string]$LibmpvPath = "D:\Downloads\shows\libmpv-2.dll",
  [string]$Version,
  [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"

$launcher = Join-Path $PSScriptRoot "launch-shows.ps1"
$installer = Join-Path $PSScriptRoot "install-shows-launcher.ps1"
$download = $null

if ($Channel -eq "stable") {
  # Stable is the bundle CI built and released, never a local build.
  if ($Version -or $SkipBuild) { throw "-Version and -SkipBuild apply to dev; stable publishes a GitHub release" }
  if (-not $Release) {
    $Release = gh release view --repo nelsong6/shows --json tagName --jq .tagName
    if ($LASTEXITCODE -ne 0 -or -not $Release) { throw "Could not determine the latest release" }
  }
  if ($Release -notmatch '^desktop-([0-9a-f]+)$') { throw "Not a desktop release tag: $Release" }
  $Version = $Matches[1]

  $download = Join-Path ([IO.Path]::GetTempPath()) "shows-release-$([guid]::NewGuid().ToString('N'))"
  gh release download $Release --repo nelsong6/shows --pattern shows-desktop-windows-amd64.zip --dir $download
  if ($LASTEXITCODE -ne 0) { throw "Could not download release $Release" }
  Expand-Archive -LiteralPath (Join-Path $download "shows-desktop-windows-amd64.zip") -DestinationPath (Join-Path $download "bundle")
  $exe = Join-Path $download "bundle\shows-desktop.exe"
  $LibmpvPath = Join-Path $download "bundle\libmpv-2.dll"
} else {
  if (-not $Version) {
    $sha = (git -C $RepoRoot rev-parse --short=12 HEAD).Trim()
    if ($LASTEXITCODE -ne 0 -or -not $sha) { throw "Could not determine git revision" }
    $Version = "$sha-$(Get-Date -Format 'yyyyMMddHHmmss')"
  }
  $workspace = Join-Path $RepoRoot "desktop-rs"
  $frontend = Join-Path $workspace "frontend"
  $exe = Join-Path $workspace "target\release\shows-desktop.exe"
}
if ($Version -notmatch '^[A-Za-z0-9._-]+$') { throw "Invalid version: $Version" }

if ($Channel -eq "dev" -and -not $SkipBuild) {
  Push-Location $frontend
  try { npm run build; if ($LASTEXITCODE -ne 0) { throw "Frontend build failed" } }
  finally { Pop-Location }

  Push-Location $workspace
  try {
    $env:SHOWS_BUILD_SHA = $Version
    $env:SHOWS_UPDATE_CHANNEL = $Channel
    cargo build --release -p shows-desktop
    if ($LASTEXITCODE -ne 0) { throw "Desktop build failed" }
  } finally {
    Remove-Item Env:SHOWS_BUILD_SHA -ErrorAction SilentlyContinue
    Remove-Item Env:SHOWS_UPDATE_CHANNEL -ErrorAction SilentlyContinue
    Pop-Location
  }
}

foreach ($required in @($exe, $LibmpvPath, $launcher, $installer)) {
  if (-not (Test-Path -LiteralPath $required -PathType Leaf)) { throw "Missing required file: $required" }
}

$versions = Join-Path $NasRoot "versions"
$channels = Join-Path $NasRoot "channels"
$stagingRoot = Join-Path $NasRoot ".staging"
New-Item -ItemType Directory -Force -Path $versions, $channels, $stagingRoot | Out-Null

$stage = Join-Path $stagingRoot ([guid]::NewGuid().ToString("N"))
$destination = Join-Path $versions $Version
if (Test-Path -LiteralPath $destination) { throw "Version already published: $destination" }
New-Item -ItemType Directory -Path $stage | Out-Null

try {
  Copy-Item -LiteralPath $exe -Destination (Join-Path $stage "shows-desktop.exe")
  Copy-Item -LiteralPath $LibmpvPath -Destination (Join-Path $stage "libmpv-2.dll")

  $files = @()
  foreach ($name in @("shows-desktop.exe", "libmpv-2.dll")) {
    $path = Join-Path $stage $name
    $files += [ordered]@{
      name = $name
      sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash.ToLowerInvariant()
      length = (Get-Item -LiteralPath $path).Length
    }
  }

  $manifest = [ordered]@{
    schema = 1
    channel = $Channel
    version = $Version
    published_at = (Get-Date).ToUniversalTime().ToString("o")
    bundle = "versions/$Version"
    files = $files
  }
  $bundleManifest = Join-Path $stage "manifest.json"
  $manifest | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $bundleManifest -Encoding utf8NoBOM

  Move-Item -LiteralPath $stage -Destination $destination

  $channelTemp = Join-Path $channels "$Channel.$([guid]::NewGuid().ToString('N')).tmp"
  $channelPath = Join-Path $channels "$Channel.json"
  $manifest | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $channelTemp -Encoding utf8NoBOM
  Move-Item -Force -LiteralPath $channelTemp -Destination $channelPath

  $launcherTemp = Join-Path $NasRoot "launch-shows.$([guid]::NewGuid().ToString('N')).tmp"
  Copy-Item -LiteralPath $launcher -Destination $launcherTemp
  Move-Item -Force -LiteralPath $launcherTemp -Destination (Join-Path $NasRoot "launch-shows.ps1")

  $installerTemp = Join-Path $NasRoot "install-shows-launcher.$([guid]::NewGuid().ToString('N')).tmp"
  Copy-Item -LiteralPath $installer -Destination $installerTemp
  Move-Item -Force -LiteralPath $installerTemp -Destination (Join-Path $NasRoot "install-shows-launcher.ps1")
} finally {
  if (Test-Path -LiteralPath $stage) { Remove-Item -Recurse -Force -LiteralPath $stage }
  if ($download) { Remove-Item -Recurse -Force -LiteralPath $download }
}

Write-Host "Published shows $Version to $Channel at $NasRoot"
