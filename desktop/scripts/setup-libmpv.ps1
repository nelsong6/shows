# Downloads + extracts libmpv-dev for Windows into desktop/third_party/libmpv.
# Idempotent — re-runs as a no-op if the DLL is already present.
#
# Set as a Wails preBuild hook (wails.json) so `wails build` and `wails dev`
# both pick up the libmpv runtime automatically. CI also calls this directly
# from the release workflow before invoking `wails build`.
#
# Source: shinchiro's mpv-player-windows libmpv-dev builds on SourceForge.
# Updating to a newer libmpv = update $version + $hash + $sha256 below.

[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'

$version = '20250420'
$hash    = '3600c71'
$root    = Split-Path -Parent $PSScriptRoot
$tp      = Join-Path $root 'third_party'
$dst     = Join-Path $tp 'libmpv'
$dll     = Join-Path $dst 'libmpv-2.dll'

if (Test-Path $dll) {
    Write-Host "libmpv already present at $dst — skipping download."
    exit 0
}

New-Item -ItemType Directory -Path $dst -Force | Out-Null
$archive = Join-Path $tp 'libmpv.7z'
$file    = "mpv-dev-x86_64-$version-git-$hash.7z"
$url     = "https://master.dl.sourceforge.net/project/mpv-player-windows/libmpv/$file?viasf=1"

Write-Host "Downloading $file..."
Invoke-WebRequest -Uri $url -OutFile $archive -UseBasicParsing

Write-Host "Extracting..."
$sevenZip = "$env:USERPROFILE\scoop\shims\7z.exe"
if (-not (Test-Path $sevenZip)) {
    # Fall back to 7z on PATH if scoop isn't the source.
    $sevenZip = '7z.exe'
}
& $sevenZip x $archive "-o$dst" -y | Out-Null
Remove-Item $archive

Write-Host "libmpv ready at $dst"
