# Rebuild + restart the song-request app, with hard verification at every step.
#
# Why this script exists: hand-typed command chains repeatedly fooled me --
# `cargo build | Out-Null` swallowed compile failures, and `Copy-Item` failed
# silently when the old process still held the exe, so I ended up testing a
# STALE binary. Every step below verifies itself:
#   1. kill processes and wait for the file lock to clear
#   2. delete the old exe (so a skipped rebuild cannot be copied)
#   3. compile, check the exit code (output kept, not swallowed)
#   4. after copying, compare SHA256 source vs destination
#   5. start, poll /health, and confirm the running process path
#
# Usage: powershell -NoProfile -ExecutionPolicy Bypass -File scripts\rebuild-and-restart.ps1 [-Debug] [-Port 9333]
# NOTE: keep this file ASCII-only. Windows PowerShell 5.1 reads .ps1 as ANSI
# (GBK on zh-CN), so UTF-8 text without a BOM gets mangled and breaks parsing.

param(
    [switch]$Debug,
    [int]$Port = 9333
)

$ErrorActionPreference = 'Continue'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

function Fail($msg) {
    Write-Host "  [FAIL] $msg" -ForegroundColor Red
    exit 1
}

# -- 1. stop processes, wait for the lock to clear -------------------------
Write-Host 'Stopping old processes...'
Get-Process -Name 'bilibili-song-request', 'mpv' -ErrorAction SilentlyContinue |
    ForEach-Object { Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue }

$waited = 0
while ((Get-Process -Name 'bilibili-song-request' -ErrorAction SilentlyContinue) -and $waited -lt 30) {
    Start-Sleep -Seconds 1
    $waited++
}
if (Get-Process -Name 'bilibili-song-request' -ErrorAction SilentlyContinue) {
    Fail 'process still running; cannot replace the exe safely'
}
Write-Host "  stopped (waited ${waited}s)"

# -- 2. delete old artefacts ----------------------------------------------
$profileDir = if ($Debug) { 'debug' } else { 'release' }
$target = Join-Path $root "src-tauri\target\$profileDir\bilibili-song-request.exe"
$shipped = Join-Path $root 'bilibili-song-request.exe'

foreach ($p in @($target, $shipped)) {
    if (Test-Path $p) { Remove-Item $p -Force }
}
Write-Host '  removed old exe files'

# -- 3. compile -----------------------------------------------------------
$buildArgs = @('build', '--manifest-path', 'src-tauri\Cargo.toml')
if (-not $Debug) { $buildArgs += '--release' }
Write-Host "Building ($profileDir)..."

# Build log goes to a fixed path inside the project.
# NOTE: do NOT put it under target/ -- that directory may not exist after a
# clean, and Out-File then fails with DirectoryNotFoundException.
$buildLog = $root + '\build-output.log'
if (-not $buildLog) { $buildLog = '.\build-output.log' }

& cargo @buildArgs 2>&1 | Out-File -Encoding utf8 -FilePath $buildLog
$buildExit = $LASTEXITCODE
if ($null -eq $buildExit) {
    if (-not (Test-Path $target)) { $buildExit = 1 } else { $buildExit = 0 }
}
if ($buildExit -ne 0) {
    if ($buildLog -and (Test-Path $buildLog)) {
        Get-Content -Path $buildLog -Encoding utf8 |
            Select-String -Pattern '^error' | Select-Object -First 10 | ForEach-Object { "    $($_.Line)" }
    }
    Fail "cargo build failed (exit=$buildExit); log at $buildLog"
}
if (-not (Test-Path $target)) { Fail 'build succeeded but produced no exe' }
Write-Host "  built at $((Get-Item $target).LastWriteTime.ToString('HH:mm:ss'))"

# -- 4. copy + verify hash ------------------------------------------------
Copy-Item $target $shipped -Force
$srcHash = (Get-FileHash $target -Algorithm SHA256).Hash
$dstHash = (Get-FileHash $shipped -Algorithm SHA256).Hash
if ($srcHash -ne $dstHash) {
    Fail "hash mismatch after copy ($($srcHash.Substring(0,12)) vs $($dstHash.Substring(0,12)))"
}
$sizeMb = [Math]::Round((Get-Item $shipped).Length / 1MB, 2)
Write-Host "  installed root exe (SHA256 $($srcHash.Substring(0,12))..., ${sizeMb}MB)"

# -- 5. start + health check ---------------------------------------------
$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=$Port"
Start-Process -FilePath $shipped

$ready = $false
for ($i = 0; $i -lt 30; $i++) {
    Start-Sleep -Seconds 1
    try {
        $r = Invoke-WebRequest -Uri 'http://127.0.0.1:17777/health' -TimeoutSec 2 -UseBasicParsing
        if ($r.StatusCode -eq 200) { $ready = $true; break }
    } catch {
        # not up yet
    }
}
if (-not $ready) { Fail '/health not ready within 30s' }

$proc = Get-Process -Name 'bilibili-song-request' -ErrorAction SilentlyContinue | Select-Object -First 1
if ($proc -and $proc.Path -and ($proc.Path -ne $shipped)) {
    Fail "running process is not the freshly built exe: $($proc.Path)"
}
Write-Host '  [OK] started and healthy' -ForegroundColor Green
