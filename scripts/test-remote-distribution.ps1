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
  & $launcher -Channel dev -NasRoot $nas -InstallRoot $install -InstallOnly
  $native = Join-Path $install 'channels\dev\shows-remote.exe'
  $output = & $native --version 2>&1
  if ($LASTEXITCODE -ne 0 -or "$output" -notmatch 'shows-remote 0\.1\.0') { throw "Native entry point did not run: $output" }
  $output = & $native --json 2>&1
  if ($LASTEXITCODE -eq 0 -or "$output" -notmatch 'needs a terminal') { throw "Native entry point lost failure exit status or arguments: $output" }
  & $launcher -Channel dev -NasRoot $nas -InstallRoot $install -InstallOnly
  if ((Get-Item (Split-Path $native)).LinkType -ne 'Junction') { throw 'Native entry is not a version junction' }
  $state = Get-Content -Raw -LiteralPath (Join-Path $install 'launcher-state-dev.json') | ConvertFrom-Json
  if ($state.current -ne 'smoke') { throw 'Verified version was not selected' }
  $agent = Get-Content -Raw -LiteralPath (Join-Path $install 'agents\dev.cmd')
  if ($agent -notmatch 'versions\\smoke\\shows-remote.exe') { throw 'Native SSH entry point does not select the verified version' }
  foreach ($file in $manifest.files) {
    $installed = Join-Path $install ('versions\smoke\' + $file.name)
    if ((Get-FileHash -Algorithm SHA256 -LiteralPath $installed).Hash.ToLowerInvariant() -ne $file.sha256) { throw 'Installed hash differs from manifest' }
  }
  & (Join-Path $RepoRoot 'scripts\publish-shows-channel.ps1') -Channel dev -RepoRoot (Join-Path $fixture 'repo') -NasRoot $nas -LibmpvPath $runtime -Version smoke -SkipBuild
  # Advance the pointer to a second verified version without overwriting the old binary.
  & (Join-Path $RepoRoot 'scripts\publish-shows-channel.ps1') -Channel dev -RepoRoot (Join-Path $fixture 'repo') -NasRoot $nas -LibmpvPath $runtime -Version smoke2 -SkipBuild
  & $launcher -Channel dev -NasRoot $nas -InstallRoot $install -InstallOnly
  $state = Get-Content -Raw -LiteralPath (Join-Path $install 'launcher-state-dev.json') | ConvertFrom-Json
  if ($state.current -ne 'smoke2' -or $state.previous -ne 'smoke') { throw 'Native selection did not retain rollback version' }
  if (-not (Test-Path (Join-Path $install 'versions\smoke\shows-remote.exe'))) { throw 'Switch deleted the old version' }
  if ([IO.Path]::GetFullPath([string](Get-Item (Split-Path $native)).Target) -ne [IO.Path]::GetFullPath((Join-Path $install 'versions\smoke2'))) { throw 'Native pointer was not updated' }
  & (Join-Path $RepoRoot 'scripts\publish-shows-channel.ps1') -Channel dev -RepoRoot (Join-Path $fixture 'repo') -NasRoot $nas -LibmpvPath $runtime -Version smoke3 -SkipBuild
  [IO.File]::WriteAllText((Join-Path $nas 'versions\smoke3\shows-remote.exe'), 'corrupt fixture')
  & $launcher -Channel dev -NasRoot $nas -InstallRoot $install -InstallOnly
  $state = Get-Content -Raw -LiteralPath (Join-Path $install 'launcher-state-dev.json') | ConvertFrom-Json
  if ($state.current -ne 'smoke2' -or [IO.Path]::GetFullPath([string](Get-Item (Split-Path $native)).Target) -ne [IO.Path]::GetFullPath((Join-Path $install 'versions\smoke2'))) { throw 'Corrupt update changed the verified native selection' }
  Write-Host 'Remote distribution smoke passed: verified native binary launch, idempotent install, version switch and retained prior files'
} finally {
  $resolved = [IO.Path]::GetFullPath($fixture)
  if (-not $resolved.StartsWith($tempBase, [StringComparison]::OrdinalIgnoreCase) -or (Split-Path -Leaf $resolved) -notlike 'shows-remote-test-*') { throw 'Unsafe test cleanup path' }
  Remove-Item -LiteralPath $resolved -Recurse -Force
}
$global:LASTEXITCODE = 0
