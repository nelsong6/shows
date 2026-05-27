# One-shot build for shows-desktop. Wires up cgo to libmpv, runs the
# Wails toolchain, and copies the libmpv runtime DLL alongside the
# resulting .exe so it loads on a fresh machine without PATH fiddling.
#
# Idempotent. Re-runnable. Honors a few env vars for CI:
#   $env:SHOWS_SKIP_LIBMPV_SETUP = '1' — assume third_party/libmpv/ is
#       already populated (CI restores it from cache).
#   $env:SHOWS_MINGW_BIN = '<path>'  — override the mingw bin dir if
#       it's not at scoop's default location.
#
# Local dev: just run `pwsh scripts/build.ps1` from anywhere; absolute
# paths inside this script are derived from $PSScriptRoot.

[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# --- 1. libmpv runtime + headers ---------------------------------
if ($env:SHOWS_SKIP_LIBMPV_SETUP -ne '1') {
    & "$PSScriptRoot\setup-libmpv.ps1"
}

$lib = Join-Path $root 'third_party\libmpv'
if (-not (Test-Path "$lib\libmpv-2.dll")) {
    throw "libmpv-2.dll missing at $lib — setup-libmpv.ps1 did not produce it"
}

# --- 2. cgo toolchain + flags ------------------------------------
$mingwBin = if ($env:SHOWS_MINGW_BIN) {
    $env:SHOWS_MINGW_BIN
} else {
    "$env:USERPROFILE\scoop\apps\mingw\current\bin"
}
if (-not (Test-Path "$mingwBin\gcc.exe")) {
    throw "MinGW gcc not found at $mingwBin\gcc.exe — install via `scoop install mingw` or set `$env:SHOWS_MINGW_BIN"
}

# libmpv on PATH for the bindings-generation step (wails runs the
# freshly-built exe briefly to introspect Go methods; that fails with
# STATUS_DLL_NOT_FOUND if libmpv-2.dll isn't reachable).
$env:Path        = "$mingwBin;$lib;$env:Path"
$env:CGO_CFLAGS  = "-I$lib\include"
$env:CGO_LDFLAGS = "-L$lib -lmpv"

Write-Host ""
Write-Host "Building shows-desktop ($lib)"
Write-Host "  CGO_CFLAGS  = $env:CGO_CFLAGS"
Write-Host "  CGO_LDFLAGS = $env:CGO_LDFLAGS"
Write-Host ""

# --- 3. wails build ----------------------------------------------
wails build
if ($LASTEXITCODE -ne 0) {
    throw "wails build failed (exit $LASTEXITCODE)"
}

# --- 4. bundle libmpv-2.dll alongside the exe --------------------
$exe = Join-Path $root 'build\bin\shows.exe'
if (-not (Test-Path $exe)) {
    throw "build/bin/shows.exe missing after wails build"
}
Copy-Item "$lib\libmpv-2.dll" (Join-Path $root 'build\bin\') -Force

Write-Host ""
Write-Host "shows.exe + libmpv-2.dll at $($root)\build\bin"
Get-ChildItem (Join-Path $root 'build\bin') |
    Select-Object Name, @{n='size_MB'; e={[math]::Round($_.Length / 1MB, 1)}}
