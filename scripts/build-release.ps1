# Rebuild the release binary and sync web assets into the run directories.
#
# NOTE: This file is intentionally ASCII-only.
# Windows PowerShell 5.1 reads .ps1 files as ANSI (GBK on this machine), so any
# non-ASCII text inside a string literal gets mangled into invalid tokens and
# the whole script fails to parse. Keep comments/messages in English here.
#
# ---------------------------------------------------------------- why this exists
# "Rebuilt but the app still behaves the old way" has happened repeatedly, and
# there are THREE independent causes. All of them must be handled:
#
# (1) THE FRONTEND IS EMBEDDED INTO THE EXE BY THE TAURI BUILD.
#     `cargo build --release` does NOT re-embed it. The frontend that the app
#     actually serves comes from whatever `npm run tauri build` last baked into
#     the binary. Measured proof:
#         exe contains "main-hD3H5WBu" -> True
#         exe contains "main-CNG95QjQ" -> False   (the fresh dist on disk)
#     So a `cargo build` "succeeds" while serving a frontend from hours ago.
#     => Use `npm run tauri:build`. It rebuilds AND re-embeds.
#
# (2) THE WEBVIEW HTTP CACHE KEEPS OLD ASSET URLS.
#     WebView2 caches `/assets/main-<hash>.js` by URL. If a file is later
#     removed it can still be served from cache, so the UI shows stale code
#     even though the server-side file is correct.
#     => Clear Cache / Code Cache / GPUCache (see step 3). User data
#        (Local Storage, History, Preferences) is deliberately left alone.
#
# (3) `_up_/dist` WAS NEVER SYNCED, AND Copy-Item NEVER DELETES.
#     `npm run build` writes dist/ at the repo root, while the runtime probes
#     `_up_/dist` next to the exe (installer layout, see webdist.rs).
#     Copy-Item only adds/overwrites, so old `Panel-<hash>.css` piles up:
#     measured 235 files with 10 different Panel css versions present at once.
#     => Replace `_up_/dist` wholesale (delete then copy).
#
# ---------------------------------------------------------------- usage
# powershell -NoProfile -ExecutionPolicy Bypass -File scripts/build-release.ps1
#
# Options:
#   -SkipFrontendBuild  do not run `npm run tauri:build` (reuse the current exe)
[CmdletBinding()]
param(
  [switch]$SkipFrontendBuild
)

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

function Step($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }
function Ok($msg) { Write-Host "    $msg" -ForegroundColor Green }

# 1) Stop every process that can hold the exe or the webview cache.
Step 'Stop running instances'
Get-Process -Name 'bilibili-song-request', 'mpv' -ErrorAction SilentlyContinue |
  Stop-Process -Force -ErrorAction SilentlyContinue
for ($i = 0; $i -lt 10; $i++) {
  $wv = Get-Process -Name 'msedgewebview2' -ErrorAction SilentlyContinue
  if (-not $wv) { break }
  $wv | Stop-Process -Force -ErrorAction SilentlyContinue
  Start-Sleep -Seconds 2
}
Ok 'all instances stopped'

# 2) Build. `npm run tauri:build` also embeds the frontend into the exe
#    (see cause (1) above) - `cargo build` alone is NOT enough.
if (-not $SkipFrontendBuild) {
  Step 'Build (npm run tauri:build) - rebuilds frontend AND re-embeds it'
  & npm run tauri:build
  if ($LASTEXITCODE -ne 0) { throw "npm run tauri:build failed (exit $LASTEXITCODE)" }
  Ok 'bundle built and frontend embedded'
} else {
  Step 'Skip build (reusing current exe)'
}

$exe = Join-Path $root 'src-tauri\target\release\bilibili-song-request.exe'
if (-not (Test-Path $exe)) { throw "exe not found: $exe" }

# 3) Clear the WebView2 HTTP / code cache (cause (2)).
#    Only cache dirs are removed - user data stays untouched.
Step 'Clear WebView2 caches'
$wvRoot = Join-Path $env:LOCALAPPDATA 'com.bilibili-song-request.desktop\EBWebView\Default'
foreach ($name in @('Cache', 'Code Cache', 'GPUCache')) {
  $p = Join-Path $wvRoot $name
  if (Test-Path $p) {
    Remove-Item $p -Recurse -Force -ErrorAction SilentlyContinue
    Ok "cleared $name"
  }
}

# 4) Sync exe + dist into the portable directory (cause (3)).
#    Layout (verified, do not change): `<portable>/_up_/dist`, ONE level of _up_.
Step 'Sync into dist-portable'
$portable = Join-Path $root 'dist-portable'
New-Item -ItemType Directory -Path $portable -Force | Out-Null
Copy-Item $exe (Join-Path $portable 'bilibili-song-request.exe') -Force

$upDir = Join-Path $portable '_up_'
$upDist = Join-Path $upDir 'dist'
Remove-Item $upDir -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Path $upDir -Force | Out-Null
Copy-Item (Join-Path $root 'dist') $upDist -Recurse -Force
$assetCount = (Get-ChildItem (Join-Path $upDist 'assets') -File | Measure-Object).Count
Ok "exe + dist synced ($assetCount files in assets)"

# 5) Also refresh target/release/_up_/dist so running the exe straight from
#    target/release uses the new frontend too.
Step 'Sync into target/release'
$relUp = Join-Path $root 'src-tauri\target\release\_up_'
Remove-Item $relUp -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Path $relUp -Force | Out-Null
Copy-Item (Join-Path $root 'dist') (Join-Path $relUp 'dist') -Recurse -Force
Ok 'target/release/_up_/dist synced'

# 6) Assert that the exe really carries the current frontend.
#    This is the check that would have caught cause (1) immediately.
Step 'Verify embedded frontend matches dist/'
$mainJs = (Get-ChildItem (Join-Path $root 'dist\assets') -Filter 'main-*.js' |
  Select-Object -First 1).BaseName
if ($mainJs) {
  $bytes = [System.IO.File]::ReadAllBytes($exe)
  $ascii = [System.Text.Encoding]::ASCII.GetString($bytes)
  if ($ascii.Contains($mainJs)) {
    Ok "OK: exe embeds $mainJs"
  } else {
    throw "MISMATCH: exe does not embed $mainJs - the frontend was NOT re-embedded. Did the build really run?"
  }
}

Write-Host ''
Write-Host 'Done. To start the portable build:' -ForegroundColor Cyan
Write-Host "  $portable\bilibili-song-request.exe"
