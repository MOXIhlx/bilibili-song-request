//! 应用库入口。
//!
//! 三层结构：
//!  - `models`  —— 与前端共享的数据模型（serde 序列化）
//!  - `state`   —— 全局状态 `AppState` + 广播总线 `EventBus`
//!  - `server`  —— axum HTTP/WebSocket 服务器（给 OBS 面板与浏览器控制台用）
//!
//! `main.rs` 只负责拉起 axum 与 Tauri 窗口，业务逻辑都从这里暴露。

pub mod bilibili;
pub mod binresolver;
pub mod config;
pub mod event;
pub mod models;
pub mod music;
pub mod player;
pub mod queue;
pub mod server;
pub mod shutdown;
pub mod state;
pub mod webdist;

#[cfg(test)]
mod tests;

/// crate 版本号，来源于 Cargo.toml，用于 /health 与前端展示。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
