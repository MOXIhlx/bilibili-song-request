//! 前端静态资源发现与页面渲染。
//!
//! 内嵌 axum 需要向 OBS / 浏览器提供 `/panel` 与 `/dashboard` 两个页面。
//! 这些资源来自 `npm run build` 产出的 `dist/`，但运行期不能假设当前工作目录，
//! 因此按以下顺序查找（第一个存在的胜出）：
//!
//!  1. 环境变量 `BSR_DIST_DIR`（调试用，优先级最高）
//!  2. exe 同级的 `dist/`
//!  3. exe 上一级的 `dist/`（打包安装后常见位置）
//!  4. exe 向上 3~4 级的 `dist/`
//!     —— 覆盖 `cargo run` 的开发场景：`src-tauri/target/debug/x.exe`
//!        向上 3 级到 `src-tauri/`、4 级到仓库根，而 `dist/` 在仓库根。
//!  5. 当前工作目录与其父目录下的 `dist/`
//!
//! 找不到时返回一个自解释的提示页，而不是 404 —— 主播看到提示比看到空白好。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::server::templates::{error_page, inject_server_hint};

/// 全局缓存的 dist 目录查找结果。
static DIST_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();

/// 从可执行文件出发向上探测的层数（含自身）。
const EXE_ANCESTOR_LEVELS: usize = 5;

/// 查找 dist 目录（结果缓存，进程内只探测一次）。
pub fn dist_dir() -> Option<&'static PathBuf> {
    DIST_DIR
        .get_or_init(|| {
            let mut candidates: Vec<PathBuf> = Vec::new();

            if let Ok(dir) = std::env::var("BSR_DIST_DIR") {
                if !dir.trim().is_empty() {
                    candidates.push(PathBuf::from(dir));
                }
            }

            // exe 所在目录及其向上 4 级父目录
            if let Ok(exe) = std::env::current_exe() {
                let mut base: Option<&Path> = exe.parent();
                for _ in 0..EXE_ANCESTOR_LEVELS {
                    let Some(current) = base else { break };
                    candidates.push(current.join("dist"));
                    base = current.parent();
                }
            }

            if let Ok(cwd) = std::env::current_dir() {
                candidates.push(cwd.join("dist"));
                if let Some(parent) = cwd.parent() {
                    candidates.push(parent.join("dist"));
                }
            }

            candidates
                .into_iter()
                .find(|p| p.join("panel.html").is_file() && p.join("dashboard.html").is_file())
        })
        .as_ref()
}

/// 资源目录是否就绪。
pub fn is_ready() -> bool {
    dist_dir().is_some()
}

/// dist 目录路径（未就绪时返回 None）。
pub fn dist_path() -> Option<&'static Path> {
    dist_dir().map(|p| p.as_path())
}

/// 读取 dist 下的某个构建产物页面。
fn read_page(file_name: &str) -> Option<String> {
    let dir = dist_dir()?;
    std::fs::read_to_string(dir.join(file_name)).ok()
}

/// 渲染 OBS 面板页；`server_hint` 为注入到 `window.__BSR_SERVER__` 的地址。
pub fn render_panel(server_hint: Option<&str>) -> String {
    match read_page("panel.html") {
        Some(raw) => inject_server_hint(
            &rewrite_assets(&raw),
            "弹幕点歌机 · OBS 面板",
            server_hint,
        ),
        None => not_built_page(),
    }
}

/// 渲染浏览器控制台页。
pub fn render_dashboard(server_hint: Option<&str>) -> String {
    match read_page("dashboard.html") {
        Some(raw) => inject_server_hint(
            &rewrite_assets(&raw),
            "弹幕点歌机 · 控制台",
            server_hint,
        ),
        None => not_built_page(),
    }
}

/// 把相对资源路径重写为绝对路径，确保 `/panel` 这种无尾斜杠的 URL 也能正确加载资源。
fn rewrite_assets(html: &str) -> String {
    html.replace("src=\"./assets/", "src=\"/assets/")
        .replace("href=\"./assets/", "href=\"/assets/")
        .replace("src=\"./src/", "src=\"/src/")
        .replace("href=\"./src/", "href=\"/src/")
}

/// 未构建前端时的提示页。
fn not_built_page() -> String {
    error_page(
        "前端资源尚未构建",
        r#"<p>内嵌服务器没有找到 <code>dist/</code> 目录，因此无法提供面板页面。</p>
<p>请在项目根目录执行：</p>
<pre>npm install
npm run build</pre>
<p>然后重启本程序。开发期也可以设置环境变量 <code>BSR_DIST_DIR</code> 指向 dist 目录。</p>"#,
    )
}

#[cfg(test)]
mod tests {
    use super::rewrite_assets;

    #[test]
    fn rewrites_relative_asset_paths() {
        let html = r#"<script type="module" src="./assets/panel-abc.js"></script>
<link rel="stylesheet" href="./assets/index-def.css" />"#;
        let out = rewrite_assets(html);
        assert!(out.contains("src=\"/assets/panel-abc.js\""));
        assert!(out.contains("href=\"/assets/index-def.css\""));
        assert!(!out.contains("./assets/"));
    }
}
