$ErrorActionPreference='Stop'
$tempBase=[IO.Path]::GetFullPath([IO.Path]::GetTempPath())
$fixture=Join-Path $tempBase ('shows-desktop-distribution-' + [guid]::NewGuid().ToString('N'))
$repo=Join-Path $fixture 'repo'
$release=Join-Path $repo 'desktop-rs\target\release'
$nas=Join-Path $fixture 'nas'
$install=Join-Path $fixture 'installed'
New-Item -ItemType Directory -Path $release | Out-Null
try {
  Copy-Item -LiteralPath (Join-Path $env:SystemRoot 'System32\where.exe') -Destination (Join-Path $release 'shows-desktop.exe')
  $runtime=Join-Path $fixture 'libmpv-2.dll'
  [IO.File]::WriteAllText($runtime,'runtime fixture')
  & "$PSScriptRoot/publish-shows-channel.ps1" -Channel dev -RepoRoot $repo -NasRoot $nas -LibmpvPath $runtime -Version first -SkipBuild
  $manifest=Get-Content -Raw (Join-Path $nas 'channels\dev.json') | ConvertFrom-Json
  if (@($manifest.files).Count -ne 2 -or 'shows-remote.exe' -in @($manifest.files.name)) { throw 'Desktop publication still bundles a remote' }
  New-Item -ItemType Directory -Path (Join-Path $install 'agents') -Force | Out-Null
  [IO.File]::WriteAllText((Join-Path $install 'agents\dev.cmd'),'retired fixture')
  & "$PSScriptRoot/launch-shows.ps1" -Channel dev -NasRoot $nas -InstallRoot $install -InstallOnly
  if (Test-Path (Join-Path $install 'agents\dev.cmd')) { throw 'Desktop installer retained the retired agent entry' }
  if (Test-Path (Join-Path $install 'channels\dev\shows-remote.exe')) { throw 'Desktop installer still exposes a remote' }
  & "$PSScriptRoot/publish-shows-channel.ps1" -Channel dev -RepoRoot $repo -NasRoot $nas -LibmpvPath $runtime -Version second -SkipBuild
  & "$PSScriptRoot/launch-shows.ps1" -Channel dev -NasRoot $nas -InstallRoot $install -InstallOnly
  $state=Get-Content -Raw (Join-Path $install 'launcher-state-dev.json') | ConvertFrom-Json
  if ($state.current -ne 'second' -or $state.previous -ne 'first') { throw 'Desktop-only channel switch failed' }
  foreach ($file in $manifest.files) {
    $installed=Join-Path $install ('versions\second\' + $file.name)
    if ((Get-FileHash -Algorithm SHA256 -LiteralPath $installed).Hash.ToLowerInvariant() -ne $file.sha256) { throw 'Desktop installation hash differs' }
  }
  Write-Host 'Desktop-only distribution passed: player/runtime, verified native selection and retired agent removal'
} finally {
  $resolved=[IO.Path]::GetFullPath($fixture)
  if (-not $resolved.StartsWith($tempBase,[StringComparison]::OrdinalIgnoreCase) -or (Split-Path -Leaf $resolved) -notlike 'shows-desktop-distribution-*') { throw 'Unsafe desktop fixture cleanup path' }
  Remove-Item -LiteralPath $resolved -Recurse -Force
}
$global:LASTEXITCODE=0
