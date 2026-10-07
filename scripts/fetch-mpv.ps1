<#
.SYNOPSIS
    下载 mpv 并放置成 Tauri sidecar 要求的文件名。

.DESCRIPTION
    背景：Tauri 的 `bundle.externalBin` 要求外部二进制按
    `<名称>-<目标三元组>.exe` 命名（Windows MSVC 下即
    `mpv-x86_64-pc-windows-msvc.exe`），构建时它会被一起打进安装包，
    运行时可通过 `binaries/` 里的同名文件定位。

    本项目对 mpv 采取**可选**策略：找不到 mpv 也能正常启动（只是没有声音）。
    因此这个脚本是可选的——你也可以直接 `winget install shinchiro.mpv`
    让程序从系统路径找到 mpv。

.PARAMETER Version
    mpv 版本，默认 `latest`（跟随 shinchiro 的 latest 构建）。

.PARAMETER Force
    目标文件已存在时也重新下载。

.EXAMPLE
    pwsh -File scripts/fetch-mpv.ps1
    pwsh -File scripts/fetch-mpv.ps1 -Version v0.41.0 -Force

.NOTES
    下载源：https://github.com/shinchiro/mpv-winbuild-cmake/releases
    许可：mpv 为 GPLv2+ / LGPLv2.1+，分发时请自行确认许可合规。
#>
[CmdletBinding()]
param(
    [string]$Version = 'latest',
    [switch]$Force
)

$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$binDir = Join-Path $repoRoot 'src-tauri\binaries'
$target = 'x86_64-pc-windows-msvc'
$destName = "mpv-$target.exe"
$dest = Join-Path $binDir $destName

if ((Test-Path $dest) -and -not $Force) {
    Write-Host "已存在，跳过：$dest" -ForegroundColor Yellow
    Write-Host "（需要重新下载请加 -Force）"
    exit 0
}

New-Item -ItemType Directory -Force -Path $binDir | Out-Null

# ── 解析下载地址 ────────────────────────────────────────────────────────────
$apiBase = 'https://api.github.com/repos/shinchiro/mpv-winbuild-cmake/releases'
if ($Version -eq 'latest') {
    $releaseUrl = "$apiBase/latest"
} else {
    $releaseUrl = "$apiBase/tags/$Version"
}

Write-Host "查询 release：$releaseUrl"
$headers = @{ 'User-Agent' = 'bilibili-song-request-fetch-mpv' }
$release = Invoke-RestMethod -Uri $releaseUrl -Headers $headers -TimeoutSec 60
Write-Host "版本：$($release.tag_name)  发布时间：$($release.published_at)"

# 选 x86_64 的 7z 包（体积最小；若没有则退回 zip）
$asset = $release.assets |
    Where-Object { $_.name -match '^mpv-x86_64-.*\.7z$' } |
    Select-Object -First 1
if (-not $asset) {
    $asset = $release.assets |
        Where-Object { $_.name -match '^mpv-x86_64-.*\.zip$' } |
        Select-Object -First 1
}
if (-not $asset) {
    Write-Error "该 release 里没有找到 x86_64 的 7z/zip 包。可用资源：`n$($release.assets.name -join "`n")"
}

$tmpDir = Join-Path ([System.IO.Path]::GetTempPath()) ("bsr-mpv-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Force -Path $tmpDir | Out-Null
$archive = Join-Path $tmpDir $asset.name

try {
    Write-Host "下载：$($asset.name)（$([Math]::Round($asset.size / 1MB, 1)) MB）"
    $progressBackup = $ProgressPreference
    $ProgressPreference = 'SilentlyContinue'   # 关掉进度条，否则下载明显变慢
    Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $archive -Headers $headers -TimeoutSec 600
    $ProgressPreference = $progressBackup

    # ── 解压 ────────────────────────────────────────────────────────────────
    Write-Host '解压…'
    if ($archive.EndsWith('.zip')) {
        Expand-Archive -Path $archive -DestinationPath $tmpDir -Force
    } else {
        # 7z 优先用系统已装的 7z，其次用 tar（Win10 1803+ 自带 bsdtar，支持 7z）
        # 注意：不用 `?.Source`——Windows PowerShell 5.1 不支持空条件运算符。
        $sevenZip = $null
        $cmd = Get-Command 7z.exe -ErrorAction SilentlyContinue
        if ($cmd) { $sevenZip = $cmd.Source }
        if (-not $sevenZip) {
            $candidates = @(
                "$env:ProgramFiles\7-Zip\7z.exe",
                "${env:ProgramFiles(x86)}\7-Zip\7z.exe"
            )
            $sevenZip = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
        }
        if ($sevenZip) {
            & $sevenZip x $archive "-o$tmpDir" -y | Out-Null
        } else {
            Write-Host '未找到 7z，改用系统自带 tar 解压'
            & tar -xf $archive -C $tmpDir
        }
    }

    $mpvExe = Get-ChildItem -Path $tmpDir -Recurse -Filter 'mpv.exe' | Select-Object -First 1
    if (-not $mpvExe) {
        Write-Error "解压后没有找到 mpv.exe（目录内容：$((Get-ChildItem $tmpDir -Recurse -File | Select-Object -First 20).Name -join ', ')）"
    }

    # 只复制主程序：动态库（libmpv / d3dcompiler 等）与 mpv.exe 同级，
    # sidecar 只需一个可执行文件；若遇到缺少 DLL 的报错，请改用
    # `winget install shinchiro.mpv`（完整安装包）并设置 BSR_MPV_PATH。
    Copy-Item $mpvExe.FullName $dest -Force

    $size = [Math]::Round((Get-Item $dest).Length / 1MB, 1)
    Write-Host "完成：$dest（$size MB）" -ForegroundColor Green
    Write-Host ''
    Write-Host '下一步：' -ForegroundColor Cyan
    Write-Host '  1) 构建发布版：npm run tauri build'
    Write-Host '  2) 安装包会把该 sidecar 一起打包；运行时程序优先使用它。'
    Write-Host '  3) 若 mpv 启动报缺少 DLL，请改用 winget 安装完整版并设置 BSR_MPV_PATH。'
} finally {
    Remove-Item -Recurse -Force $tmpDir -ErrorAction SilentlyContinue
}
