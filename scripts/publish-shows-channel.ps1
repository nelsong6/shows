param(
  [ValidateSet("stable", "dev")]
  [string]$Channel = "dev",
  [string]$RepoRoot = (Split-Path -Parent $PSScriptRoot),
  [string]$NasRoot = "S:\shows-app",
  [string]$Release,
  [string]$LibmpvPath = "D:\Downloads\shows\libmpv-2.dll",
  [string]$Version,
  [switch]$SkipBuild,
  [switch]$WaitForRelease,
  [string[]]$InstallTargets = @()
)

$ErrorActionPreference = "Stop"

$launcher = Join-Path $PSScriptRoot "launch-shows.ps1"
$installer = Join-Path $PSScriptRoot "install-shows-launcher.ps1"
$download = $null

if ($Channel -eq "stable") {
  # Stable is the bundle CI built and released, never a local build.
  if ($Version -or $SkipBuild) { throw "-Version and -SkipBuild apply to dev; stable publishes a GitHub release" }
  if ($WaitForRelease) {
    if (-not $Release) {
      $releaseSha = (gh api repos/nelsong6/shows/commits/main --jq .sha).Trim()
      if ($LASTEXITCODE -ne 0 -or $releaseSha -notmatch '^[0-9a-f]{40}$') { throw 'Could not identify main for release publication' }
      $Release = 'desktop-' + $releaseSha.Substring(0,7)
    }
    $deadline = (Get-Date).AddMinutes(20)
    while ($true) {
      $published = gh release view $Release --repo nelsong6/shows --json assets 2>$null
      if ($LASTEXITCODE -eq 0 -and (($published | ConvertFrom-Json).assets | Where-Object { $_.name -eq 'shows-desktop-windows-amd64.zip' -and $_.state -eq 'uploaded' })) { break }
      if ((Get-Date) -gt $deadline) { throw "Timed out waiting for $Release" }
      if ($releaseSha) {
        $runs = gh api "repos/nelsong6/shows/actions/runs?head_sha=$releaseSha" | ConvertFrom-Json
        $build = $runs.workflow_runs | Where-Object name -eq 'Build Desktop' | Select-Object -First 1
        if ($build.conclusion -in @('failure','cancelled','timed_out','action_required')) { throw "Release build $($build.id) ended with $($build.conclusion)" }
      }
      Write-Host "Waiting for CI release $Release"
      Start-Sleep -Seconds 10
    }
  } elseif (-not $Release) {
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
  $remoteExe = Join-Path $download "bundle\shows-remote.exe"
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
  $remoteExe = Join-Path $workspace "target\release\shows-remote.exe"
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
    cargo build --release -p shows-desktop -p shows-remote
    if ($LASTEXITCODE -ne 0) { throw "Desktop build failed" }
  } finally {
    Remove-Item Env:SHOWS_BUILD_SHA -ErrorAction SilentlyContinue
    Remove-Item Env:SHOWS_UPDATE_CHANNEL -ErrorAction SilentlyContinue
    Pop-Location
  }
}

foreach ($required in @($exe, $remoteExe, $LibmpvPath, $launcher, $installer)) {
  if (-not (Test-Path -LiteralPath $required -PathType Leaf)) { throw "Missing required file: $required" }
}

$versions = Join-Path $NasRoot "versions"
$channels = Join-Path $NasRoot "channels"
$stagingRoot = Join-Path $NasRoot ".staging"
New-Item -ItemType Directory -Force -Path $versions, $channels, $stagingRoot | Out-Null

$stage = Join-Path $stagingRoot ([guid]::NewGuid().ToString("N"))
$destination = Join-Path $versions $Version
if (Test-Path -LiteralPath $destination) {
  $existing = Get-Content -Raw -LiteralPath (Join-Path $destination 'manifest.json') | ConvertFrom-Json
  $sources = @{'shows-desktop.exe'=$exe; 'shows-remote.exe'=$remoteExe; 'libmpv-2.dll'=$LibmpvPath}
  if (@($existing.files).Count -ne 3) { throw 'Previously published version has an incomplete manifest' }
  foreach ($name in $sources.Keys) {
    $file = @($existing.files | Where-Object name -eq $name)
    if ($file.Count -ne 1) { throw "Previously published version is missing $name" }
    $installed = Join-Path $destination $file.name
    if ((Get-FileHash -Algorithm SHA256 -LiteralPath $installed).Hash.ToLowerInvariant() -ne $file.sha256) { throw "Previously published version is damaged: $installed" }
    if ((Get-FileHash -Algorithm SHA256 -LiteralPath $sources[$name]).Hash.ToLowerInvariant() -ne $file.sha256) { throw "Release content differs from the immutable publication: $name" }
  }
  $current = Get-Content -Raw -LiteralPath (Join-Path $channels "$Channel.json") | ConvertFrom-Json
  if ($current.version -ne $Version) { throw 'Version exists but is not the selected channel; refusing to change an existing publication' }
  foreach ($target in $InstallTargets) {
    $installNasRoot = $NasRoot
    if ($installNasRoot -like 'S:\*') { $installNasRoot = Join-Path (Get-PSDrive S).DisplayRoot $installNasRoot.Substring(3) }
    & (Join-Path $PSScriptRoot 'install-shows-update.ps1') -Target $target -Channel $Channel -NasRoot $installNasRoot
  }
  if ($download) {
    $resolvedDownload = [IO.Path]::GetFullPath($download)
    if (-not $resolvedDownload.StartsWith([IO.Path]::GetFullPath([IO.Path]::GetTempPath()), [StringComparison]::OrdinalIgnoreCase) -or (Split-Path -Leaf $resolvedDownload) -notlike 'shows-release-*') { throw 'Unsafe release cleanup path' }
    Remove-Item -Recurse -Force -LiteralPath $resolvedDownload
  }
  Write-Host "Shows $Version is already verified on $Channel"
  return
}
New-Item -ItemType Directory -Path $stage | Out-Null

try {
  Copy-Item -LiteralPath $exe -Destination (Join-Path $stage "shows-desktop.exe")
  Copy-Item -LiteralPath $remoteExe -Destination (Join-Path $stage "shows-remote.exe")
  Copy-Item -LiteralPath $LibmpvPath -Destination (Join-Path $stage "libmpv-2.dll")

  $files = @()
  foreach ($name in @("shows-desktop.exe", "shows-remote.exe", "libmpv-2.dll")) {
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
foreach ($target in $InstallTargets) {
  $installNasRoot = $NasRoot
  if ($installNasRoot -like 'S:\*') { $installNasRoot = Join-Path (Get-PSDrive S).DisplayRoot $installNasRoot.Substring(3) }
  & (Join-Path $PSScriptRoot 'install-shows-update.ps1') -Target $target -Channel $Channel -NasRoot $installNasRoot
}
