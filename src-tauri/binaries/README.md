# mpv sidecar 放置目录

Tauri 打包时可以把这里的 mpv 可执行文件作为 **sidecar** 打进安装包，运行时由
`src-tauri/src/player/mpv.rs`（`binresolver`）解析真实路径并作为子进程启动。

## 为什么需要单独放一个 mpv.exe

mpv 不是 Rust 依赖，无法由 Cargo 管理，只能以二进制形式随包分发。

## 一键获取（推荐）

```powershell
# 在仓库根目录执行
npm run fetch:mpv

# 指定版本 / 强制重下
npm run fetch:mpv -- -Version v0.41.0 -Force
```

脚本（`scripts/fetch-mpv.ps1`）会从
<https://github.com/shinchiro/mpv-winbuild-cmake/releases> 下载 x86_64 包，
解压出 `mpv.exe` 并按下面的命名规则放到本目录。

## 命名规则（Tauri 2 sidecar 约定）

Tauri 通过 `tauri.conf.json` 的 `bundle.externalBin` 引用 sidecar，
文件名必须带上目标三元组后缀：

```text
src-tauri/binaries/mpv-x86_64-pc-windows-msvc.exe
```

对应 `tauri.conf.json` 需要增加：

```json
{
  "bundle": {
    "externalBin": ["binaries/mpv"]
  }
}
```

> ⚠️ 当前 `tauri.conf.json` **没有**声明 `externalBin`：
> 项目默认依赖用户自行安装的 mpv（`winget install shinchiro.mpv`），
> 这样安装包体积小、也不会引入 mpv 的 GPL 分发义务。
> 需要「零配置分发」时再加上这一行并重新打包。

目标三元组可用 `rustc -vV | Select-String host` 确认。

## 手动获取

1. 打开 <https://github.com/shinchiro/mpv-winbuild-cmake/releases>（或
   <https://sourceforge.net/projects/mpv-player-windows/files/release/>）
2. 下载 `mpv-x86_64-*.7z`，解压出 `mpv.exe`
3. 重命名为 `mpv-x86_64-pc-windows-msvc.exe` 放到本目录

## 找不到 mpv 时程序怎么办

**不影响启动**：控制台提示「未检测到 mpv」，`/api/player/status` 的 `available`
为 `false`，播放按钮禁用；点歌、队列、面板都照常工作。
也可以临时指向任意位置的 mpv：

```powershell
$env:BSR_MPV_PATH = "D:\tools\mpv\mpv.exe"
```

> 本目录中的 `.exe` 与 `.dll` 已被 `.gitignore` 忽略，避免把几十 MB 的二进制提交进仓库。
