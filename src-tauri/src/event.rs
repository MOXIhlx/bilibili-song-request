//! 内部事件与对前端广播的消息封包。

use crate::models::{AppState, BilibiliState, Danmaku};

/// 进程内部事件（阶段 3 起由弹幕模块推入总线）。
#[derive(Debug, Clone)]
pub enum Event {
    /// B 站连接状态变化。
    Connection(BilibiliState),
    /// 收到一条弹幕。
    Danmaku(Danmaku),
    /// 应用状态整体变化（由 `StateCell::write` 自动发出）。
    AppState(Box<AppState>),
    /// 普通日志，透传给前端便于排障。
    Log(String),
}

/// 发往 WebSocket 客户端的消息。
///
/// 为保持「状态与弹幕分通道」的清晰结构，这里不直接复用 `Event`，
/// 而是显式定义 wire format（`type` 字段 + `data` 字段）。
#[derive(Debug, Clone)]
pub enum HubEvent {
    /// 全量状态快照。
    State(Box<AppState>),
    /// 弹幕消息。
    Danmaku(Danmaku),
    /// 连接状态变化。
    Connection(BilibiliState),
    /// 点歌成功入队（阶段 4）。
    RequestQueued {
        /// 点歌人。
        user: String,
        /// 歌名。
        title: String,
        /// 歌手。
        artist: String,
        /// 队内位置（从 1 开始）。
        position: usize,
    },
    /// 点歌被规则拒绝（阶段 4）。
    RequestRejected {
        /// 点歌人。
        user: String,
        /// 歌名。
        title: String,
        /// 拒绝分类（cooldown / duplicate / queue_full / fans_medal / user_level）。
        reason: String,
        /// 可读说明。
        message: String,
    },
    /// 队列条目已解析出真实曲目（阶段 5）。
    SongResolved {
        /// 队列条目 ID。
        item_id: uuid::Uuid,
        /// 平台返回的歌名。
        title: String,
        /// 歌手。
        artist: String,
        /// 平台。
        platform: crate::models::MusicPlatform,
        /// 时长（秒）。
        duration: u64,
    },
    /// 队列条目解析失败（阶段 5）。
    SongResolveFailed {
        /// 队列条目 ID。
        item_id: uuid::Uuid,
        /// 用户输入的原始歌名。
        title: String,
        /// 失败原因。
        reason: String,
    },
    /// 开始播放某首歌曲（阶段 6）。
    NowPlaying {
        /// 队列条目 ID。
        item_id: uuid::Uuid,
        /// 歌名。
        title: String,
        /// 歌手。
        artist: String,
        /// 点歌人。
        requested_by: String,
        /// 平台。
        platform: crate::models::MusicPlatform,
        /// 时长（秒）。
        duration: u64,
    },
    /// 某首歌曲播放结束（阶段 6）。
    SongFinished {
        /// 队列条目 ID。
        item_id: uuid::Uuid,
        /// 歌名。
        title: String,
        /// 结束时的状态（played / skipped）。
        status: crate::models::QueueStatus,
    },
    /// 取播放地址失败，该条目被跳过（阶段 6）。
    SongPlayFailed {
        /// 队列条目 ID。
        item_id: uuid::Uuid,
        /// 歌名。
        title: String,
        /// 失败原因。
        reason: String,
    },
    /// 已获取当前歌曲的歌词（阶段 7）。
    Lyrics {
        /// 队列条目 ID。
        item_id: uuid::Uuid,
        /// 歌名。
        title: String,
        /// LRC 原文；纯音乐或无歌词时为 `None`。
        lyrics: Option<String>,
        /// 是否纯音乐（无歌词行）。
        instrumental: bool,
    },
    /// 服务器问候，客户端连接后立即发送。
    Hello { version: String, started_at: String },
    /// 前端消息处理失败时的错误回执。
    Error(String),
}

impl HubEvent {
    /// 转换为 WS 文本帧。序列化失败时回退为一个 error 消息，避免连接被打断。
    pub fn to_text(&self) -> String {
        let value = match self {
            HubEvent::State(state) => serde_json::json!({ "type": "state", "data": state }),
            HubEvent::Danmaku(d) => serde_json::json!({ "type": "danmaku", "data": d }),
            HubEvent::Connection(c) => serde_json::json!({ "type": "connection", "data": c }),
            HubEvent::RequestQueued {
                user,
                title,
                artist,
                position,
            } => serde_json::json!({
                "type": "request",
                "data": {
                    "outcome": "queued",
                    "user": user,
                    "title": title,
                    "artist": artist,
                    "position": position,
                }
            }),
            HubEvent::RequestRejected {
                user,
                title,
                reason,
                message,
            } => serde_json::json!({
                "type": "request",
                "data": {
                    "outcome": "rejected",
                    "user": user,
                    "title": title,
                    "reason": reason,
                    "message": message,
                }
            }),
            HubEvent::Hello {
                version,
                started_at,
            } => serde_json::json!({
                "type": "hello",
                "data": { "version": version, "started_at": started_at }
            }),
            HubEvent::SongResolved {
                item_id,
                title,
                artist,
                platform,
                duration,
            } => serde_json::json!({
                "type": "song",
                "data": {
                    "outcome": "resolved",
                    "item_id": item_id,
                    "title": title,
                    "artist": artist,
                    "platform": platform,
                    "duration": duration,
                }
            }),
            HubEvent::SongResolveFailed {
                item_id,
                title,
                reason,
            } => serde_json::json!({
                "type": "song",
                "data": {
                    "outcome": "failed",
                    "item_id": item_id,
                    "title": title,
                    "reason": reason,
                }
            }),
            HubEvent::NowPlaying {
                item_id,
                title,
                artist,
                requested_by,
                platform,
                duration,
            } => serde_json::json!({
                "type": "player",
                "data": {
                    "event": "started",
                    "item_id": item_id,
                    "title": title,
                    "artist": artist,
                    "requested_by": requested_by,
                    "platform": platform,
                    "duration": duration,
                }
            }),
            HubEvent::SongFinished {
                item_id,
                title,
                status,
            } => serde_json::json!({
                "type": "player",
                "data": {
                    "event": "finished",
                    "item_id": item_id,
                    "title": title,
                    "status": status,
                }
            }),
            HubEvent::SongPlayFailed {
                item_id,
                title,
                reason,
            } => serde_json::json!({
                "type": "player",
                "data": {
                    "event": "failed",
                    "item_id": item_id,
                    "title": title,
                    "reason": reason,
                }
            }),
            HubEvent::Lyrics {
                item_id,
                title,
                lyrics,
                instrumental,
            } => serde_json::json!({
                "type": "lyrics",
                "data": {
                    "item_id": item_id,
                    "title": title,
                    "lyrics": lyrics,
                    "instrumental": instrumental,
                }
            }),
            HubEvent::Error(msg) => serde_json::json!({ "type": "error", "data": { "message": msg } }),
        };
        serde_json::to_string(&value)
            .unwrap_or_else(|e| format!(r#"{{"type":"error","data":{{"message":"序列化失败: {e}"}}}}"#))
    }
}
