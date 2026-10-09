# Rebuild the release binary and sync web assets into the run directories.
#
# NOTE: This file is intentionally ASCII-only.
# Windows PowerShell 5.1 reads .ps1 files as ANSI (GBK on this machine), so any
# non-ASCII text inside a string literal gets mangled into invalid tokens and
# the whole script fails to parse. Keep comments/messages in English here.
#
# ---------------------------------------------------------------- why this exists
# "Rebuilt but the app still behaves the old way" has happened several times.
# The cause was always that the FRONTEND output was not synced:
#
#   * `npm run build` writes dist/ at the repo root;
#   * at runtime the Rust binary probes for dist/ in a fixed order
#     (see src-tauri/src/webdist.rs). The `_up_/dist` next to the exe is the
#     installer layout, and it is what the portable build actually serves;
#   * previously only the exe was copied by hand, so the old frontend kept
#     being served. The symptom is very misleading: fresh exe timestamp and a
#     healthy /health response, but zero UI change.
#   * worse, Copy-Item only adds/overwrites and never deletes, so old
#     `Panel-<hash>.css` files piled up in `_up_/dist/assets`
#     (measured: 235 files, 10 different Panel css versions present at once).
#
# So this script REPLACES `_up_/dist` wholesale (delete then copy) rather than
# copying incrementally.
#
# ---------------------------------------------------------------- usage
# powershell -NoProfile -ExecutionPolicy Bypass -File scripts/build-release.ps1
#
# Options:
#   -SkipNpmBuild    do not run `npm run build`
#   -SkipCargoBuild  do not run `cargo build --release`
[CmdletBinding()]
param(
  [switch]$SkipNpmBuild,
  [switch]$SkipCargoBuild
)

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

function Step($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }
function Ok($msg) { Write-Host "    $msg" -ForegroundColor Green }

# 1) Frontend build
if (-not $SkipNpmBuild) {
  Step 'Frontend build (npm run build)'
  & npm run build
  if ($LASTEXITCODE -ne 0) { throw "npm run build failed (exit $LASTEXITCODE)" }
  Ok 'dist/ updated'
} else {
  Step 'Skip frontend build'
}

# 2) Stop running instances.
#    Windows will not let us overwrite a running exe, and WebView2 child
#    processes hold handles too, so they are killed as well.
Step 'Stop running instances'
Get-Process -Name 'bilibili-song-request', 'mpv' -ErrorAction SilentlyContinue |
  Stop-Process -Force -ErrorAction SilentlyContinue
Get-CimInstance Win32_Process -Filter "Name='msedgewebview2.exe'" -ErrorAction SilentlyContinue |
  Where-Object { $_.CommandLine -like '*song-request*' } |
  ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }

# 3) Backend build
$exe = Join-Path $root 'src-tauri\target\release\bilibili-song-request.exe'
if (Test-Path $exe) {
  for ($i = 0; $i -lt 10; $i++) {
    Start-Sleep -Seconds 2
    try {
      $fs = [System.IO.File]::Open($exe, 'Open', 'ReadWrite', 'None')
      $fs.Close()
      break
    } catch {
      if ($i -eq 9) { throw "exe is still locked, cannot rebuild: $exe" }
    }
  }
  Ok 'exe is writable'
}

if (-not $SkipCargoBuild) {
  Step 'Backend build (cargo build --release)'
  Push-Location 'src-tauri'
  try {
    & cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit $LASTEXITCODE)" }
  } finally {
    Pop-Location
  }
  Ok 'release binary updated'
} else {
  Step 'Skip backend build'
}

# 4) Sync into the portable directory.
#    Layout (verified, do not change): `<portable>/_up_/dist`, ONE level of _up_.
#    That is what `resources: ["../dist"]` in tauri.conf.json produces.
Step 'Sync into dist-portable'
$portable = Join-Path $root 'dist-portable'
New-Item -ItemType Directory -Path $portable -Force | Out-Null
if (Test-Path $exe) {
  Copy-Item $exe (Join-Path $portable 'bilibili-song-request.exe') -Force
}

$upDir = Join-Path $portable '_up_'
$upDist = Join-Path $upDir 'dist'
Remove-Item $upDir -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Path $upDir -Force | Out-Null
Copy-Item (Join-Path $root 'dist') $upDist -Recurse -Force

$assetCount = (Get-ChildItem (Join-Path $upDist 'assets') -File | Measure-Object).Count
Ok "exe + dist synced ($assetCount files in assets)"

# 5) Also refresh target/release/_up_/dist so running the exe straight from
#    target/release uses the new frontend too (this exact trap was hit before).
Step 'Sync into target/release'
$relUp = Join-Path $root 'src-tauri\target\release\_up_'
Remove-Item $relUp -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Path $relUp -Force | Out-Null
Copy-Item (Join-Path $root 'dist') (Join-Path $relUp 'dist') -Recurse -Force
Ok 'target/release/_up_/dist synced'

Write-Host ''
Write-Host 'Done. To start the portable build:' -ForegroundColor Cyan
Write-Host "  $portable\bilibili-song-request.exe"
