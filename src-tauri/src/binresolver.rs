//! 外部二进制（sidecar）定位。
//!
//! Tauri 打包后 sidecar 会随安装包分发，但**开发期**通常用系统已安装的 mpv。
//! 因此查找顺序设计为「先精确、后宽松」：
//!
//!  1. 环境变量 `BSR_MPV_PATH`（排障 / 指定特殊构建）
//!  2. exe 同级的 `mpv.exe` 与 `binaries/` 子目录（便携版布局）
//!  3. exe 向上 1~4 级里常见的开发布局：
//!     - `src-tauri/binaries/mpv-<target-triple>.exe`（Tauri sidecar 命名）
//!     - `src-tauri/binaries/mpv.exe`
//!     - `<仓库根>/mpv.exe`
//!  4. 系统安装位置（本机实测：winget 的 `shinchiro.mpv` 会装到这里）
//!  5. `PATH` 里的 `mpv`
//!
//! 找不到时返回一个**自解释的错误**，列出所有尝试过的路径，
//! 让用户一眼看出该把 mpv 放哪。

use std::path::{Path, PathBuf};

/// 覆盖 mpv 路径的环境变量。
pub const ENV_MPV_PATH: &str = "BSR_MPV_PATH";

/// Tauri sidecar 的目标三元组（编译期由 tauri-build 注入）。
pub fn target_triple() -> &'static str {
    option_env!("TAURI_ENV_TARGET_TRIPLE").unwrap_or("x86_64-pc-windows-msvc")
}

/// 平台上的可执行文件后缀。
pub fn exe_suffix() -> &'static str {
    if cfg!(windows) {
        ".exe"
    } else {
        ""
    }
}

/// 定位结果。
#[derive(Debug, Clone)]
pub struct Located {
    /// 绝对路径。
    pub path: PathBuf,
    /// 人类可读的来源说明（写日志用）。
    pub source: String,
}

/// 查找 mpv 可执行文件。
pub fn find_mpv() -> Result<Located, String> {
    let mut tried: Vec<PathBuf> = Vec::new();

    // 1) 环境变量
    if let Ok(raw) = std::env::var(ENV_MPV_PATH) {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            let path = PathBuf::from(trimmed);
            if path.is_file() {
                return Ok(Located {
                    path,
                    source: format!("环境变量 {ENV_MPV_PATH}"),
                });
            }
            tried.push(path);
        }
    }

    // 2)/3) 以 exe 位置为基准
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for candidate in exe_relative_candidates(dir) {
                if candidate.is_file() {
                    return Ok(Located {
                        source: "exe 同目录或上层目录".to_string(),
                        path: candidate,
                    });
                }
                tried.push(candidate);
            }
        }
    }

    // 4) 系统安装位置
    for candidate in system_candidates() {
        if candidate.is_file() {
            return Ok(Located {
                source: "系统安装位置".to_string(),
                path: candidate,
            });
        }
        tried.push(candidate);
    }

    // 5) PATH
    if let Some(path) = find_in_path("mpv") {
        return Ok(Located {
            path,
            source: "PATH".to_string(),
        });
    }

    Err(format!(
        "未找到 mpv 可执行文件。请任选一种方式：\n\
         1. 安装 mpv（例如 `winget install shinchiro.mpv`）；\n\
         2. 把 mpv.exe 放到程序同目录；\n\
         3. 把 sidecar 放到 src-tauri/binaries/mpv-{}.exe；\n\
         4. 设置环境变量 {}=<mpv.exe 的完整路径>。\n\
         已尝试的路径：{}",
        target_triple(),
        ENV_MPV_PATH,
        tried
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("; ")
    ))
}

/// 以 exe 目录为基准的候选路径。
///
/// 覆盖的摆放方式（实测最常见的三种）：
///  - 把 `mpv.exe` 直接放在程序目录（或任意上层目录）
///  - Tauri sidecar 布局：`binaries\mpv-<target-triple>.exe` / `binaries\mpv.exe`
///  - **解压出来的整个 mpv 发行包**：`mpv\mpv.exe`
///    （官方/shinchiro 的 7z 解压后就是这种：外层一个 `mpv\` 目录，
///     里面有 `mpv.exe`、`d3dcompiler_43.dll`、`mpv-register.bat` 等）
fn exe_relative_candidates(exe_dir: &Path) -> Vec<PathBuf> {
    let suffix = exe_suffix();
    let mut candidates = Vec::new();
    let mut base: Option<&Path> = Some(exe_dir);

    for _ in 0..5 {
        let Some(current) = base else { break };
        candidates.push(current.join(format!("mpv{suffix}")));
        candidates.push(
            current.join("binaries").join(format!(
                "mpv-{}{suffix}",
                target_triple()
            )),
        );
        candidates.push(current.join("binaries").join(format!("mpv{suffix}")));
        // 解压出来的发行包：`mpv\mpv.exe`（以及少数包里多一层 installer\）
        candidates.push(current.join("mpv").join(format!("mpv{suffix}")));
        candidates.push(
            current
                .join("mpv")
                .join("installer")
                .join(format!("mpv{suffix}")),
        );
        // 有些第三方打包把可执行文件放 bin\
        candidates.push(current.join("bin").join(format!("mpv{suffix}")));
        base = current.parent();
    }
    candidates
}

/// 系统安装位置候选。
fn system_candidates() -> Vec<PathBuf> {
    let suffix = exe_suffix();
    let mut candidates = Vec::new();

    #[cfg(windows)]
    {
        // winget 的 shinchiro.mpv 实测安装目录
        candidates.push(PathBuf::from(format!(
            r"C:\Program Files\MPV Player\mpv{suffix}"
        )));
        candidates.push(PathBuf::from(format!(
            r"C:\Program Files (x86)\MPV Player\mpv{suffix}"
        )));
        candidates.push(PathBuf::from(format!(r"C:\Program Files\mpv\mpv{suffix}")));
        candidates.push(PathBuf::from(format!(r"C:\mpv\mpv{suffix}")));
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            candidates.push(PathBuf::from(&local).join("Programs").join("mpv").join(format!("mpv{suffix}")));
            candidates.push(PathBuf::from(&local).join("mpv").join(format!("mpv{suffix}")));
            // winget 便携包目录（包名含版本，用通配扫描）
            let packages = PathBuf::from(&local).join("Microsoft").join("WinGet").join("Packages");
            if packages.is_dir() {
                if let Ok(entries) = std::fs::read_dir(&packages) {
                    for entry in entries.flatten() {
                        let name = entry.file_name().to_string_lossy().to_lowercase();
                        if name.contains("mpv") {
                            candidates.push(entry.path().join(format!("mpv{suffix}")));
                        }
                    }
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        candidates.push(PathBuf::from("/opt/homebrew/bin/mpv"));
        candidates.push(PathBuf::from("/usr/local/bin/mpv"));
        candidates.push(PathBuf::from("/Applications/mpv.app/Contents/MacOS/mpv"));
    }

    #[cfg(target_os = "linux")]
    {
        candidates.push(PathBuf::from("/usr/bin/mpv"));
        candidates.push(PathBuf::from("/usr/local/bin/mpv"));
        candidates.push(PathBuf::from("/snap/bin/mpv"));
    }

    candidates
}

/// 在 PATH 中查找可执行文件。
pub fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let suffix = exe_suffix();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(format!("{name}{suffix}"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_include_sidecar_naming() {
        let dir = PathBuf::from("/tmp/bsr-exe");
        let candidates = exe_relative_candidates(&dir);
        let expected = dir
            .join("binaries")
            .join(format!("mpv-{}{}", target_triple(), exe_suffix()));
        assert!(
            candidates.contains(&expected),
            "应包含 Tauri sidecar 命名 {expected:?}"
        );
        assert!(candidates.iter().any(|p| p.ends_with(format!("mpv{}", exe_suffix()))));
    }

    #[test]
    fn candidates_find_extracted_mpv_folder() {
        // 用户实测的摆放：把官方/shinchiro 的 mpv 7z 解压到程序目录旁边，
        // 于是同级同时出现 `mpv\`（文件夹，内含 mpv.exe）与可能重复的其它文件。
        // 早期只探 `current\mpv.exe` 与 `current\binaries\...`，**不探 `current\mpv\mpv.exe`**，
        // 导致"明明装了 mpv 却报未检测到"。
        let dir = PathBuf::from("/app");
        let candidates = exe_relative_candidates(&dir);
        let in_subfolder = dir.join("mpv").join(format!("mpv{}", exe_suffix()));
        assert!(
            candidates.contains(&in_subfolder),
            "应包含解压包布局 {in_subfolder:?}"
        );
        // 少数包多一层 installer\
        let nested = dir
            .join("mpv")
            .join("installer")
            .join(format!("mpv{}", exe_suffix()));
        assert!(candidates.contains(&nested), "应包含 mpv/installer/ 布局");
        // 以及 bin\ 布局
        let in_bin = dir.join("bin").join(format!("mpv{}", exe_suffix()));
        assert!(candidates.contains(&in_bin), "应包含 bin/ 布局");
    }

    #[test]
    fn candidates_walk_up_several_levels() {
        let dir = PathBuf::from("/repo/src-tauri/target/debug");
        let candidates = exe_relative_candidates(&dir);
        // 应该包含仓库根下的 binaries/mpv-<triple>
        let repo_sidecar = PathBuf::from("/repo/src-tauri/binaries")
            .join(format!("mpv-{}{}", target_triple(), exe_suffix()));
        assert!(
            candidates.contains(&repo_sidecar),
            "应向上找到 src-tauri/binaries"
        );
    }

    #[test]
    fn system_candidates_are_non_empty() {
        assert!(!system_candidates().is_empty());
    }

    #[test]
    fn missing_binary_error_lists_attempts_and_hints() {
        // 用一个必然不存在的环境变量值触发失败路径
        let previous = std::env::var(ENV_MPV_PATH).ok();
        std::env::set_var(ENV_MPV_PATH, "/definitely/not/here/mpv");
        let result = find_mpv();
        match previous {
            Some(value) => std::env::set_var(ENV_MPV_PATH, value),
            None => std::env::remove_var(ENV_MPV_PATH),
        }
        // 本机装了 mpv 时可能真的找到，因此只断言「找到了就一定是文件，没找到就带提示」
        match result {
            Ok(located) => assert!(located.path.is_file()),
            Err(message) => {
                assert!(message.contains("未找到 mpv"));
                assert!(message.contains(ENV_MPV_PATH));
            }
        }
    }

    #[test]
    fn find_in_path_locates_a_known_executable() {
        // Windows 上一定有 cmd.exe；Unix 上一定有 sh
        let name = if cfg!(windows) { "cmd" } else { "sh" };
        assert!(find_in_path(name).is_some(), "应从 PATH 找到 {name}");
        assert!(find_in_path("definitely-not-a-real-binary-xyz").is_none());
    }
}
