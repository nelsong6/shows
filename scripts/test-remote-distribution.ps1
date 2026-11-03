param(
  [string]$RepoRoot = (Split-Path -Parent $PSScriptRoot),
  [string]$RemoteExe = (Join-Path $PSScriptRoot '..\desktop-rs\target\release\shows-remote.exe')
)

$ErrorActionPreference = 'Stop'
$tempBase = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
$fixture = Join-Path $tempBase ('shows-remote-test-' + [guid]::NewGuid().ToString('N'))
$release = Join-Path $fixture 'repo\desktop-rs\target\release'
$nas = Join-Path $fixture 'nas'
$install = Join-Path $fixture 'installed'
New-Item -ItemType Directory -Path $release -Force | Out-Null
try {
  # The smoke test never starts the desktop or touches a real channel/database.
  Copy-Item -LiteralPath $RemoteExe -Destination (Join-Path $release 'shows-remote.exe')
  Copy-Item -LiteralPath $RemoteExe -Destination (Join-Path $release 'shows-desktop.exe')
  $runtime = Join-Path $fixture 'libmpv-2.dll'
  [IO.File]::WriteAllText($runtime, 'runtime fixture')
  & (Join-Path $RepoRoot 'scripts\publish-shows-channel.ps1') -Channel dev -RepoRoot (Join-Path $fixture 'repo') -NasRoot $nas -LibmpvPath $runtime -Version smoke -SkipBuild
  $manifest = Get-Content -Raw -LiteralPath (Join-Path $nas 'channels\dev.json') | ConvertFrom-Json
  if (@($manifest.files).Count -ne 3) { throw 'The bundle must contain desktop, remote and runtime' }
  $launcher = Join-Path $RepoRoot 'scripts\launch-shows.ps1'
  $output = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $launcher -Channel dev -NasRoot $nas -InstallRoot $install -App remote --version 2>&1
  if ($LASTEXITCODE -ne 0 -or "$output" -notmatch 'shows-remote 0\.1\.0') { throw "Terminal launcher did not forward --version: $output" }
  $output = & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $launcher -Channel dev -NasRoot $nas -InstallRoot $install -App remote --json 2>&1
  if ($LASTEXITCODE -eq 0 -or "$output" -notmatch 'needs a terminal') { throw "Terminal launcher lost failure exit status or arguments: $output" }
  $state = Get-Content -Raw -LiteralPath (Join-Path $install 'launcher-state-dev.json') | ConvertFrom-Json
  if ($state.current -ne 'smoke') { throw 'Verified version was not selected' }
  foreach ($file in $manifest.files) {
    $installed = Join-Path $install ('versions\smoke\' + $file.name)
    if ((Get-FileHash -Algorithm SHA256 -LiteralPath $installed).Hash.ToLowerInvariant() -ne $file.sha256) { throw 'Installed hash differs from manifest' }
  }
  Write-Host 'Remote distribution smoke passed: complete manifest, verified install, argument forwarding and exit status'
} finally {
  $resolved = [IO.Path]::GetFullPath($fixture)
  if (-not $resolved.StartsWith($tempBase, [StringComparison]::OrdinalIgnoreCase) -or (Split-Path -Leaf $resolved) -notlike 'shows-remote-test-*') { throw 'Unsafe test cleanup path' }
  Remove-Item -LiteralPath $resolved -Recurse -Force
}
