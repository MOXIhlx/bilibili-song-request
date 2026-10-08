# 便携版诊断脚本（在任何一台机器上跑，输出可直接复制回贴）
#
# 用法：把本文件放到便携版目录（与 bilibili-song-request.exe 同级），
#       右键「使用 PowerShell 运行」，或在 PowerShell 里：
#         powershell -NoProfile -ExecutionPolicy Bypass -File .\诊断.ps1
#
# 它只做**只读检查**：不修改配置、不删文件、不发起点歌。
# NOTE: keep this file ASCII-only. Windows PowerShell 5.1 reads .ps1 as ANSI
# (GBK on zh-CN), so UTF-8 text without a BOM gets mangled and breaks parsing.
# Chinese output is produced via [char] codes to stay ASCII-safe.

$ErrorActionPreference = 'Continue'

# UTF-8 output so Chinese shows correctly in the console
try { [Console]::OutputEncoding = [System.Text.Encoding]::UTF8 } catch {}

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$port = 17777
$base = "http://127.0.0.1:$port"

function Line { param([string]$t) Write-Host $t }
function Head { param([string]$t) Write-Host ""; Write-Host "=== $t ===" }
function Ok   { param([string]$t) Write-Host "  [OK]   $t" }
function Bad  { param([string]$t) Write-Host "  [FAIL] $t" }
function Warn { param([string]$t) Write-Host "  [WARN] $t" }
function Info { param([string]$t) Write-Host "         $t" }

function Get-Url {
    param([string]$Path, [int]$Timeout = 8)
    try {
        $r = Invoke-WebRequest -Uri ($base + $Path) -TimeoutSec $Timeout -UseBasicParsing
        return @{ ok = $true; code = $r.StatusCode; body = $r.Content }
    } catch {
        $code = $null
        $body = $_.Exception.Message
        try {
            if ($_.Exception.Response) {
                $code = [int]$_.Exception.Response.StatusCode
                $sr = New-Object System.IO.StreamReader($_.Exception.Response.GetResponseStream())
                $body = $sr.ReadToEnd()
            }
        } catch {}
        return @{ ok = $false; code = $code; body = $body }
    }
}

Line "便携版诊断  $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')"
Line "脚本目录: $scriptDir"
Line "系统: $([System.Environment]::OSVersion.VersionString)"

# ---------------------------------------------------------------- 1. files
Head "1. portable layout"
$exe = Join-Path $scriptDir 'bilibili-song-request.exe'
if (Test-Path $exe) {
    $mb = [Math]::Round((Get-Item $exe).Length / 1MB, 2)
    Ok "exe found (${mb}MB)"
} else {
    Bad "bilibili-song-request.exe NOT found next to this script"
}
foreach ($p in @('_up_\dist\panel.html', '_up_\dist\dashboard.html', '_up_\dist\index.html')) {
    if (Test-Path (Join-Path $scriptDir $p)) { Ok $p } else { Bad "$p MISSING" }
}
$assets = Join-Path $scriptDir '_up_\dist\assets'
if (Test-Path $assets) {
    $n = (Get-ChildItem $assets -File -ErrorAction SilentlyContinue).Count
    if ($n -gt 0) { Ok "assets/ has $n files" } else { Bad "assets/ is empty" }
} else { Bad "assets/ folder MISSING" }
if (Test-Path (Join-Path $scriptDir 'mpv.exe')) { Ok 'mpv.exe next to app (will be used first)' }

# ------------------------------------------------------------ 2. webview2
Head "2. WebView2 runtime (the window needs it)"
$wvKeys = @(
    'HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}',
    'HKLM:\SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}',
    'HKCU:\SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
)
$wv = $null
foreach ($k in $wvKeys) {
    try { $v = (Get-ItemProperty -Path $k -Name pv -ErrorAction Stop).pv; if ($v) { $wv = $v; break } } catch {}
}
if ($wv) { Ok "WebView2 installed: $wv" } else { Bad "WebView2 NOT found - install it (white/blank window otherwise)" }

# -------------------------------------------------------------- 3. mpv
Head "3. mpv (the playback engine)"
$envMpv = $env:BSR_MPV_PATH
if ($envMpv) {
    if (Test-Path $envMpv) { Ok "BSR_MPV_PATH -> $envMpv" } else { Bad "BSR_MPV_PATH set but missing: $envMpv" }
} else { Info 'BSR_MPV_PATH not set (env var)' }

$mpvFound = $null
$cands = @(
    (Join-Path $scriptDir 'mpv.exe'),
    (Join-Path $scriptDir 'binaries\mpv.exe'),
    'C:\Program Files\MPV Player\mpv.exe',
    'C:\Program Files\mpv\mpv.exe',
    (Join-Path $env:LOCALAPPDATA 'Microsoft\WinGet\Links\mpv.exe')
)
foreach ($c in $cands) { if (Test-Path $c) { $mpvFound = $c; break } }
if (-not $mpvFound) {
    $cmd = Get-Command mpv -ErrorAction SilentlyContinue
    if ($cmd) { $mpvFound = $cmd.Source }
}
if ($mpvFound) {
    Ok "mpv: $mpvFound"
    try {
        $ver = (& $mpvFound --version 2>&1 | Select-Object -First 1)
        Info "version: $ver"
    } catch { Warn "found but --version failed: $($_.Exception.Message)" }
} else {
    Bad "mpv NOT found - install it: winget install shinchiro.mpv"
}

# ------------------------------------------------------------- 4. server
Head "4. embedded server / API"
$h = Get-Url '/health' 6
if ($h.ok) {
    Ok "/health -> $($h.body)"
} else {
    Bad "/health failed: $($h.body)"
    Warn "the app is probably NOT running - start bilibili-song-request.exe first"
}

if ($h.ok) {
    $st = Get-Url '/api/state' 8
    if ($st.ok) {
        $s = $st.body | ConvertFrom-Json
        Ok "/api/state ok"
        Info ("version      : " + $s.version)
        Info ("current song : " + $(if ($s.current) { $s.current.song.title } else { '(none)' }))
        Info ("player       : playing=" + $s.player.playing + " paused=" + $s.player.paused + " pos=" + [Math]::Round($s.player.position, 1))
        Info ("queue        : " + $s.queue.Count + "   idle: " + $s.idle.Count)
    } else { Bad "/api/state failed: $($st.body)" }

    $ps = Get-Url '/api/player/status' 8
    if ($ps.ok) {
        Ok "/api/player/status -> $($ps.body)"
        try {
            $pj = $ps.body | ConvertFrom-Json
            if (-not $pj.available) { Bad "player NOT available -> mpv was not found by the app" }
        } catch {}
    } else { Bad "/api/player/status failed: $($ps.body)" }

    $ms = Get-Url '/api/music/status' 10
    if ($ms.ok) {
        Ok "/api/music/status ok"
        try {
            $mj = $ms.body | ConvertFrom-Json
            foreach ($p in $mj.platforms) {
                Info ("  " + $p.platform + " logged_in=" + $p.logged_in + " stored_in=" + $p.stored_in)
            }
        } catch {}
    } else { Bad "/api/music/status failed: $($ms.body)" }

    $bs = Get-Url '/api/bilibili/status' 8
    if ($bs.ok) {
        Ok "/api/bilibili/status ok"
        try {
            $bj = $bs.body | ConvertFrom-Json
            Info ("  phase=" + $bj.phase + " connected=" + $bj.connected + " room=" + $bj.room_id)
            if ($bj.last_error) { Info ("  last_error=" + $bj.last_error) }
        } catch {}
    } else { Bad "/api/bilibili/status failed: $($bs.body)" }

    # pages served by the app (this is what OBS loads)
    Head "5. panel pages"
    foreach ($p in @('/panel', '/dashboard')) {
        $r = Get-Url $p 10
        if ($r.ok) {
            $len = $r.body.Length
            $isHint = $r.body.Contains([char]0x524D + [char]0x7AEF + [char]0x8D44 + [char]0x6E90 + [char]0x5C1A + [char]0x672A + [char]0x6784 + [char]0x5EFA)
            if ($isHint) { Bad "$p -> 'frontend not built' hint page (dist missing?)" }
            else { Ok "$p -> HTTP $($r.code), $len chars" }
        } else { Bad "$p failed: $($r.body)" }
    }

    # real fetch of one asset
    $panel = Get-Url '/panel' 10
    if ($panel.ok) {
        $m = [regex]::Match($panel.body, 'assets/([A-Za-z0-9_\-\.]+\.js)')
        if ($m.Success) {
            $asset = '/assets/' + $m.Groups[1].Value
            $a = Get-Url $asset 10
            if ($a.ok) { Ok "$asset -> HTTP $($a.code)" } else { Bad "$asset failed: $($a.body)" }
        } else { Warn 'could not find an asset reference in /panel html' }
    }
}

# ------------------------------------------------------- 6. port / proxy
Head "6. port and proxy"
$listen = Get-NetTCPConnection -State Listen -LocalPort $port -ErrorAction SilentlyContinue
if ($listen) { Ok "port $port is listening" } else { Warn "port $port not listening" }
$sysProxy = $null
try { $sysProxy = (Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Internet Settings' -ErrorAction SilentlyContinue).ProxyServer } catch {}
if ($sysProxy) { Info "system proxy: $sysProxy" }
foreach ($v in @('HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY')) {
    $val = [Environment]::GetEnvironmentVariable($v)
    if ($val) { Info "$v = $val" }
}

# --------------------------------------------------------- 7. app logs
Head "7. app log (last errors)"
$logDir = Join-Path $env:APPDATA 'bilibili-song-request\logs'
$log = Join-Path $logDir 'app.log'
if (Test-Path $log) {
    Ok "log: $log  ($([Math]::Round((Get-Item $log).Length / 1KB, 1))KB)"
    Line "  --- last WARN/ERROR lines ---"
    Get-Content $log -Encoding utf8 |
        Where-Object { $_ -match 'WARN|ERROR' } |
        Select-Object -Last 15 | ForEach-Object { "    $($_ -replace '^.*?Z\s+', '')" }
    Line "  --- player / mpv related ---"
    Get-Content $log -Encoding utf8 |
        Where-Object { $_ -match 'mpv|play|audio' } |
        Select-Object -Last 12 | ForEach-Object { "    $($_ -replace '^.*?Z\s+', '')" }
} else {
    Warn "no log at $log  (portable mode with BSR_CONFIG_DIR set? check that folder)"
}
if ($env:BSR_CONFIG_DIR) { Info "BSR_CONFIG_DIR = $($env:BSR_CONFIG_DIR)" }

Head "done"
Line "Copy everything above and send it back."

# ---- pause so a double-clicked window does not vanish ----------------------
# When launched by right-click -> "Run with PowerShell", Windows opens a
# temporary console and closes it the moment the script exits, so the report
# would flash by unseen. Keep it open on the console host, but never block in
# an automated/non-interactive host (`powershell -File` from a script or CI).
$interactive = $true
try {
    if ($Host.Name -ne 'ConsoleHost') { $interactive = $false }
    if ([Console]::IsInputRedirected) { $interactive = $false }
} catch {
    $interactive = $false
}
if ($interactive) {
    Line ""
    Line "---- press Enter to close ----"
    try { [void](Read-Host) } catch {}
}
