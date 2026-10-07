//! B 站直播弹幕接入。
//!
//! 整体链路（阶段 3 已实现）：
//!
//! ```text
//! auth::start_session  ──POST /v2/app/start（HMAC-SHA256 签名）──▶ wss_link + auth_body
//!        │
//!        ▼
//! danmaku::BilibiliDanmakuClient  ──WS──▶ 鉴权帧 → 心跳 → LIVE_OPEN_PLATFORM_DM
//!        │                                        │
//!        │ DanmakuBroadcaster                     │
//!        ▼                                        ▼
//! controller::DanmakuHandle  ──事件转发──▶ EventBus ──▶ StateCell 广播 ──▶ /ws ──▶ 面板
//! ```
//!
//! ⚠️ 身份码限制：同一身份码最多 5 个并发连接。本程序内部只建立 **1 个** 连接
//! （由 [`controller::DanmakuHandle`] 强制保证），面板与控制台通过本地 `/ws` 复用数据。

pub mod auth;
pub mod controller;
pub mod danmaku;

pub use auth::{
    build_headers, content_md5, parse_start_response, sign, signature_payload, AuthConfig,
    BilibiliError, StartSession, START_URL,
};
pub use controller::DanmakuHandle;
pub use danmaku::{
    BilibiliDanmakuClient, DanmakuBroadcaster, DanmakuClient, DanmakuEvent, DanmakuMessage,
    DanmakuStatus, NoopDanmakuClient, TungsteniteTransport,
};
