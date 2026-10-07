//! Tauri 构建脚本。
//!
//! 它负责把 `tauri.conf.json` 转成编译期常量、生成 capability 权限、校验图标等。
//! 没有它 `tauri::generate_context!()` 无法编译。
fn main() {
    tauri_build::build()
}
