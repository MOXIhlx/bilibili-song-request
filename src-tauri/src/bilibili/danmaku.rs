//! 弹幕 WebSocket 客户端。
//!
//! ## 连接生命周期
//! ```text
//! 创建会话(v2/app/start) ──▶ 建立 WS ──▶ 发送 auth_body ──▶ 等待鉴权回执
//!        ▲                                                        │
//!        └────────── 指数退避重连（1s→60s）◀── 断线/超时/鉴权失败 ◀──┘
//! ```
//!
//! 连接建立后：
//!  - 每 `heartbeat_interval` 秒发送一次心跳（平台要求 30 秒内必须有一次）；
//!  - 解析 `LIVE_OPEN_PLATFORM_DM` 得到弹幕，推给 [`DanmakuBroadcaster`]；
//!  - 长时间收不到任何消息（默认 90 秒）判定链路已死，主动重连。
//!
//! ## 可测试性
//! 真实的 WS 无法在 CI 里连（也没有身份码），因此把「建立连接」抽象成
//! [`DanmakuTransport`]。生产实现是 [`TungsteniteTransport`]，
//! 测试实现是 [`tests::MockTransport`]，两者共用同一套上层状态机与重连逻辑。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use super::auth::{self, AuthConfig, BilibiliError, StartSession};
use crate::models::{BilibiliPhase, Danmaku};
use crate::state::StateCell;

/// 平台推送的鉴权结果命令。
pub const CMD_AUTH: &str = "LIVE_OPEN_PLATFORM_AUTH";
/// 平台推送的心跳命令（收到后回一个 pong）。
pub const CMD_HEARTBEAT: &str = "LIVE_OPEN_PLATFORM_HEARTBEAT";
/// 平台推送的弹幕命令。
pub const CMD_DANMAKU: &str = "LIVE_OPEN_PLATFORM_DM";
/// 平台推送的礼物命令（阶段 3 只记录，供后续扩展点歌）。
pub const CMD_GIFT: &str = "LIVE_OPEN_PLATFORM_SEND_GIFT";
/// 平台推送的醒目留言命令。
pub const CMD_SUPER_CHAT: &str = "LIVE_OPEN_PLATFORM_SUPER_CHAT";

/// 心跳帧命令名（客户端 → 服务端）。
pub const CMD_HEARTBEAT_PONG: &str = "LIVE_OPEN_PLATFORM_HEARTBEAT";
/// 客户端心跳/鉴权的发送间隔上限（平台要求 30 秒内必须发一次）。
pub const MAX_HEARTBEAT_SECS: u64 = 25;
/// 判定链路静默失效的阈值（秒）。
pub const IDLE_TIMEOUT_SECS: u64 = 90;
/// 重连初始退避（毫秒）。
pub const BACKOFF_INITIAL_MS: u64 = 1_000;
/// 重连最大退避（毫秒）。
pub const BACKOFF_MAX_MS: u64 = 60_000;
/// 单次连接内允许的「无有效消息」轮次上限，超过则强制重建会话，避免同一 token 死循环。
pub const MAX_STALE_ROUNDS: u32 = 5;

/// 弹幕客户端连接状态。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DanmakuStatus {
    /// 未启动
    Idle,
    /// 正在建立连接
    Connecting,
    /// 已认证并接收消息
    Connected,
    /// 断线，正在退避重连
    Reconnecting,
    /// 已停止
    Stopped,
}

impl DanmakuStatus {
    /// 是否处于「已连接」状态（用于前端展示）。
    pub fn is_connected(&self) -> bool {
        matches!(self, DanmakuStatus::Connected)
    }
}

/// 一条弹幕（平台报文的解析结果）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DanmakuMessage {
    /// 发送者昵称。
    #[serde(default, alias = "uname")]
    pub user: String,
    /// 发送者 UID。
    #[serde(default, alias = "uid")]
    pub uid: Option<Value>,
    /// 弹幕文本。
    #[serde(default, alias = "msg")]
    pub text: String,
    /// 粉丝牌等级。
    #[serde(default, alias = "fans_medal_level")]
    pub fans_medal_level: u32,
    /// 用户等级。
    #[serde(default, alias = "user_level")]
    pub user_level: u32,
    /// 平台时间戳（秒）。
    #[serde(default)]
    pub timestamp: Option<i64>,
}

impl DanmakuMessage {
    /// 转换为内部 [`Danmaku`] 模型。
    pub fn into_danmaku(self) -> Danmaku {
        let at = self
            .timestamp
            .and_then(|ts| Utc.timestamp_opt(ts, 0).single())
            .unwrap_or_else(Utc::now);
        Danmaku {
            user: if self.user.is_empty() {
                "未知用户".to_string()
            } else {
                self.user
            },
            uid: value_to_string(self.uid),
            text: self.text.trim().to_string(),
            at,
            fans_medal_level: self.fans_medal_level,
            user_level: self.user_level,
            // 真实平台弹幕永远不是「主播注入」，由 simulate 接口另行置位
            from_host: false,
        }
    }
}

/// 把 JSON 值转成字符串 UID（平台可能给数字或字符串）。
pub fn value_to_string(value: Option<Value>) -> Option<String> {
    match value {
        Some(Value::String(s)) => Some(s),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

/// 弹幕客户端向外抛出的事件。
#[derive(Debug, Clone)]
pub enum DanmakuEvent {
    /// 状态变化。
    Status(DanmakuStatus),
    /// 收到弹幕。
    Message(Danmaku),
    /// 原始报文（排障用，包含未知命令）。
    Raw(Value),
    /// 错误（不致命，通常会触发重连）。
    Error(String),
}

/// 事件广播器：把 [`DanmakuEvent`] 分发给订阅者（事件总线 / 调试面板）。
#[derive(Clone)]
pub struct DanmakuBroadcaster {
    tx: broadcast::Sender<DanmakuEvent>,
}

impl DanmakuBroadcaster {
    /// 创建广播器。
    pub fn new(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self { tx }
    }

    /// 发布事件。
    pub fn emit(&self, ev: DanmakuEvent) {
        let _ = self.tx.send(ev);
    }

    /// 订阅事件。
    pub fn subscribe(&self) -> broadcast::Receiver<DanmakuEvent> {
        self.tx.subscribe()
    }
}

impl Default for DanmakuBroadcaster {
    fn default() -> Self {
        Self::new(256)
    }
}

/// 解析平台报文中的弹幕。
///
/// `data` 既可能是对象，也可能是「被序列化成字符串的 JSON」（平台两种都出现过），
/// 因此两种形态都要支持。解析失败返回 `None` 而不是报错——弹幕流里出现
/// 未知结构很正常，不应打断连接。
pub fn parse_danmaku(data: &Value) -> Option<DanmakuMessage> {
    let object: Value = match data {
        Value::String(raw) if !raw.trim().is_empty() => serde_json::from_str(raw).ok()?,
        other => other.clone(),
    };
    serde_json::from_value(object).ok()
}

/// 从平台报文中提取命令名。
pub fn extract_command(value: &Value) -> Option<&str> {
    value.get("cmd").and_then(|c| c.as_str())
}

/// 提取报文的 `data` 字段。
pub fn extract_data(value: &Value) -> Option<&Value> {
    value.get("data")
}

/// 弹幕客户端的连接抽象。
///
/// 返回的流必须是「文本帧的收发通道」，实现方负责 TLS 与协议握手。
#[async_trait]
pub trait DanmakuTransport: Send + Sync {
    /// 建立连接。
    async fn connect(
        &self,
        url: &str,
    ) -> anyhow::Result<Box<dyn DanmakuConnection>>;
}

/// 一条已建立的连接（可收发帧）。
#[async_trait]
pub trait DanmakuConnection: Send + Sync {
    /// 发送一个**二进制**帧（开放平台协议要求：鉴权与心跳都是二进制帧）。
    async fn send_binary(&mut self, bytes: &[u8]) -> anyhow::Result<()>;
    /// 接收下一个帧；`None` 表示连接已关闭。
    ///
    /// 返回原始字节：开放平台推的是**二进制协议包**（16 字节头 + 可能 zlib 压缩的
    /// JSON body），需要由上层解包，不能当纯文本处理。
    async fn next_frame(&mut self) -> anyhow::Result<Option<Vec<u8>>>;
}

/// 开放平台协议：头长度固定 16 字节。
pub const PACKET_HEADER_LEN: u32 = 16;
/// 协议版本：0 = body 是明文；2 = body 是 zlib 压缩（可能含多个子包）。
pub const PROTO_VERSION_PLAIN: i16 = 0;
/// 协议版本：压缩。
pub const PROTO_VERSION_ZLIB: i16 = 2;
/// 操作码：客户端心跳。
pub const OP_HEARTBEAT: i32 = 2;
/// 操作码：服务器心跳回复。
pub const OP_HEARTBEAT_REPLY: i32 = 3;
/// 操作码：服务器推送的消息（弹幕等）。
pub const OP_MESSAGE: i32 = 5;
/// 操作码：客户端鉴权。
pub const OP_AUTH: i32 = 7;
/// 操作码：服务器鉴权回复。
pub const OP_AUTH_REPLY: i32 = 8;

/// 按开放平台协议打包一个数据包。
///
/// 字段全部**大端**对齐（官方 demo `proto.py`）：
/// ```text
/// Packet Length (4B, 含头) | Header Length (2B, 固定 16) | Version (2B)
/// | Operation (4B) | Sequence ID (4B, 固定 0) | Body
/// ```
pub fn build_packet(operation: i32, body: &str, version: i16) -> Vec<u8> {
    let body_bytes = body.as_bytes();
    let packet_len = PACKET_HEADER_LEN + body_bytes.len() as u32;
    let mut buf = Vec::with_capacity(packet_len as usize);
    buf.extend_from_slice(&packet_len.to_be_bytes());
    buf.extend_from_slice(&(PACKET_HEADER_LEN as u16).to_be_bytes());
    buf.extend_from_slice(&version.to_be_bytes());
    buf.extend_from_slice(&operation.to_be_bytes());
    buf.extend_from_slice(&0i32.to_be_bytes());
    buf.extend_from_slice(body_bytes);
    buf
}

/// 拆解一个开放平台协议包。
///
/// 返回 `(version, operation, body)`。头长度不合法或长度字段异常时返回 `None`
/// （网络数据不可信，宁可丢弃也不能 panic）。
pub fn parse_packet(bytes: &[u8]) -> Option<(i16, i32, &[u8])> {
    if bytes.len() < PACKET_HEADER_LEN as usize {
        return None;
    }
    let packet_len = u32::from_be_bytes(bytes[0..4].try_into().ok()?) as usize;
    let header_len = u16::from_be_bytes(bytes[4..6].try_into().ok()?) as usize;
    let version = i16::from_be_bytes(bytes[6..8].try_into().ok()?);
    let operation = i32::from_be_bytes(bytes[8..12].try_into().ok()?);

    if header_len != PACKET_HEADER_LEN as usize
        || packet_len < header_len
        || packet_len > bytes.len()
    {
        return None;
    }
    Some((version, operation, &bytes[header_len..packet_len]))
}

/// 基于 tokio-tungstenite 的生产实现。
pub struct TungsteniteTransport {
    /// 连接超时。
    pub connect_timeout: Duration,
}

impl Default for TungsteniteTransport {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
        }
    }
}

#[async_trait]
impl DanmakuTransport for TungsteniteTransport {
    async fn connect(&self, url: &str) -> anyhow::Result<Box<dyn DanmakuConnection>> {
        let (stream, _resp) =
            tokio::time::timeout(self.connect_timeout, tokio_tungstenite::connect_async(url))
                .await
                .map_err(|_| anyhow::anyhow!("连接超时（{}s）", self.connect_timeout.as_secs()))?
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(Box::new(TungsteniteConnection { stream }))
    }
}

/// tokio-tungstenite 连接包装。
struct TungsteniteConnection {
    stream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

#[async_trait]
impl DanmakuConnection for TungsteniteConnection {
    async fn send_binary(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.stream
            .send(tokio_tungstenite::tungstenite::Message::Binary(
                bytes.to_vec().into(),
            ))
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    async fn next_frame(&mut self) -> anyhow::Result<Option<Vec<u8>>> {
        loop {
            match self.stream.next().await {
                // 开放平台推二进制包；若对端发文本（理论上不會），也按字节返回
                Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(bytes))) => {
                    return Ok(Some(bytes.to_vec()));
                }
                Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                    return Ok(Some(text.as_bytes().to_vec()));
                }
                Some(Ok(tokio_tungstenite::tungstenite::Message::Ping(_)))
                | Some(Ok(tokio_tungstenite::tungstenite::Message::Pong(_)))
                | Some(Ok(tokio_tungstenite::tungstenite::Message::Frame(_))) => continue,
                Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) => return Ok(None),
                Some(Err(err)) => return Err(anyhow::anyhow!("{err}")),
                None => return Ok(None),
            }
        }
    }
}

/// 弹幕客户端抽象（便于替换实现 / 测试）。
#[async_trait]
pub trait DanmakuClient: Send + Sync {
    /// 启动客户端；内部负责自动重连，直到 [`DanmakuClient::stop`] 被调用。
    async fn run(self: Arc<Self>) -> anyhow::Result<()>;

    /// 请求停止。
    fn stop(&self);

    /// 当前状态。
    fn status(&self) -> DanmakuStatus;
}

/// 真实弹幕客户端。
pub struct BilibiliDanmakuClient {
    /// 接入配置（app_id / access_key / 身份码）。
    pub auth: AuthConfig,
    /// 全局状态（用于写入连接状态与错误）。
    pub state: StateCell,
    /// 事件广播器。
    pub broadcaster: DanmakuBroadcaster,
    /// 连接抽象（生产环境为 tungstenite）。
    pub transport: Arc<dyn DanmakuTransport>,
    /// 复用的 HTTP 客户端（调用 start 接口）。
    pub http: reqwest::Client,
    /// 停止标志。
    stopped: Arc<AtomicBool>,
    /// 当前状态（供同步查询）。
    status: std::sync::Mutex<DanmakuStatus>,
    /// 本客户端所属的连接代号（见 [`crate::models::ConnectionEpoch`]）。
    epoch: crate::models::ConnectionEpoch,
}

impl BilibiliDanmakuClient {
    /// 创建客户端。
    ///
    /// `epoch` 由控制器在每次连接时分配，客户端写状态前会比对它。
    pub fn new(
        auth: AuthConfig,
        state: StateCell,
        broadcaster: DanmakuBroadcaster,
        transport: Arc<dyn DanmakuTransport>,
    ) -> Self {
        let epoch = state.read().bilibili.epoch;
        Self {
            auth,
            state,
            broadcaster,
            transport,
            http: reqwest::Client::builder()
                .user_agent("bilibili-song-request/0.1")
                .build()
                .unwrap_or_default(),
            stopped: Arc::new(AtomicBool::new(false)),
            status: std::sync::Mutex::new(DanmakuStatus::Idle),
            epoch,
        }
    }

    /// 本客户端是否仍是"当前连接"。
    ///
    /// 主动断开时若旧任务没能在超时内退出，控制器会放弃等待并开始新连接；
    /// 旧任务随后退出时若还去写状态，就会把新连接的「已连接」覆盖成「已停止」。
    /// 所有写状态的路径都必须先过这一关。
    fn is_current(&self) -> bool {
        self.state.read().bilibili.epoch == self.epoch
    }

    /// 是否已被请求停止。
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    /// 更新状态：写入内部状态 + 全局 `AppState` + 广播。
    ///
    /// 同时把「给用户看的进度阶段」写进 `AppState.bilibili.phase/detail`：
    /// 界面就是靠这两个字段告诉用户「正在申请身份码」还是「正在连弹幕服务器」。
    fn set_status(&self, next: DanmakuStatus) {
        *self.status.lock().unwrap_or_else(|e| e.into_inner()) = next.clone();
        // 已被放弃的旧任务不许回写全局状态（否则会把新连接覆盖成「已停止」）
        if !self.is_current() {
            debug!(?next, "连接已换代，忽略旧任务的status写入");
            return;
        }
        let connected = next.is_connected();
        let (phase, detail) = Self::describe_status(&next);
        self.state.mutate(|s| {
            s.bilibili.connected = connected;
            s.bilibili.phase = phase;
            s.bilibili.detail = Some(detail.to_string());
            s.bilibili.updated_at = Some(chrono::Utc::now().to_rfc3339());
            if connected {
                s.bilibili.last_error = None;
            }
        });
        self.broadcaster.emit(DanmakuEvent::Status(next));
    }

    /// 把内部状态翻译成「给用户看的阶段 + 说明」。
    fn describe_status(status: &DanmakuStatus) -> (BilibiliPhase, &'static str) {
        match status {
            DanmakuStatus::Idle => (BilibiliPhase::Idle, "尚未连接"),
            DanmakuStatus::Connecting => (
                BilibiliPhase::ConnectingSocket,
                "正在连接弹幕服务器并完成鉴权…",
            ),
            DanmakuStatus::Connected => (BilibiliPhase::Connected, "已连接，正在接收弹幕"),
            DanmakuStatus::Reconnecting => (
                BilibiliPhase::Retrying,
                "连接中断，正在自动重连（请检查身份码与网络）…",
            ),
            DanmakuStatus::Stopped => (BilibiliPhase::Stopped, "已停止"),
        }
    }

    /// 更新「正在申请身份码会话」这一更细的阶段。
    ///
    /// `Connecting` 覆盖了「申请会话」与「连 WS」两件事，
    /// 但对用户来说这是两个等待感受不同的步骤，所以单独标记。
    fn set_phase(&self, phase: BilibiliPhase, detail: &str) {
        let detail = detail.to_string();
        self.mutate_if_current(move |s| {
            s.bilibili.phase = phase;
            s.bilibili.detail = Some(detail);
            s.bilibili.updated_at = Some(chrono::Utc::now().to_rfc3339());
        });
    }

    /// 只在**本客户端仍是当前连接**时修改全局状态。
    ///
    /// 全部写状态的路径都要走这里：被放弃的旧任务绝不能再改界面状态。
    fn mutate_if_current<F>(&self, f: F)
    where
        F: FnOnce(&mut crate::models::AppState),
    {
        if !self.is_current() {
            return;
        }
        self.state.mutate(f);
    }

    /// 累加尝试次数（每次申请会话/重连都算一次），让用户知道程序还在努力。
    fn bump_attempts(&self) {
        self.mutate_if_current(|s| {
            s.bilibili.attempts = s.bilibili.attempts.saturating_add(1);
        });
    }

    /// 是否已经重试过（用于把文案改成「可能需要几秒」）。
    fn attempts_hint_slow(&self) -> bool {
        self.state.read().bilibili.attempts > 1
    }

    /// 写入错误信息（同时置为未连接）。
    fn set_error(&self, message: impl Into<String>) {
        let message = message.into();
        warn!(%message, "B 站弹幕连接异常");
        if !self.is_current() {
            debug!(%message, "连接已换代，忽略旧任务的错误写入");
            return;
        }
        let for_state = message.clone();
        self.state.mutate(move |s| {
            s.bilibili.connected = false;
            s.bilibili.last_error = Some(for_state.clone());
            s.bilibili.phase = BilibiliPhase::Failed;
            s.bilibili.detail = Some(for_state);
            s.bilibili.updated_at = Some(chrono::Utc::now().to_rfc3339());
        });
        self.broadcaster.emit(DanmakuEvent::Error(message));
    }

    /// 写入直播间 ID。
    fn set_room_id(&self, room_id: Option<String>) {
        if let Some(room) = room_id {
            self.mutate_if_current(move |s| s.bilibili.room_id = Some(room));
        }
    }

    /// 主循环：会话 → 连接 → 断线 → 退避 → 重连。
    async fn run_loop(self: Arc<Self>) -> anyhow::Result<()> {
        let mut backoff = BACKOFF_INITIAL_MS;
        let mut stale_rounds: u32 = 0;
        let mut session: Option<StartSession> = None;

        self.set_status(DanmakuStatus::Connecting);

        loop {
            if self.is_stopped() {
                break;
            }

            // 1) 没有会话或会话已失效时，重新申请。
            if session.is_none() {
                self.bump_attempts();
                self.set_phase(
                    BilibiliPhase::RequestingSession,
                    if self.attempts_hint_slow() {
                        "正在申请身份码会话（可能需要几秒，请稍候）…"
                    } else {
                        "正在申请身份码会话…"
                    },
                );
                match auth::start_session(&self.http, &self.auth).await {
                    Ok(new_session) => {
                        self.set_room_id(new_session.room_id.clone());
                        self.set_phase(
                            BilibiliPhase::ConnectingSocket,
                            "已获取会话，正在连接弹幕服务器…",
                        );
                        session = Some(new_session);
                    }
                    Err(err) => {
                        self.set_error(err.to_string());
                        self.set_status(DanmakuStatus::Reconnecting);
                        if !self.sleep_backoff(&mut backoff).await {
                            break;
                        }
                        continue;
                    }
                }
            }

            let current = session.clone().expect("会话已在上面初始化");
            let url = current.wss_url.clone();

            // 2) 建立连接并进入消息循环。
            self.set_status(DanmakuStatus::Connecting);
            match self.run_connection(&current).await {
                Ok(rounds) => {
                    // 收到了消息说明链路可用，重置退避。
                    backoff = BACKOFF_INITIAL_MS;
                    stale_rounds = if rounds > 0 { 0 } else { stale_rounds + 1 };
                }
                Err(err) => {
                    stale_rounds += 1;
                    self.set_error(format!("{err}"));
                }
            }

            if self.is_stopped() {
                break;
            }

            // 3) 连续多轮没有正常消息：丢弃会话，重新申请（避免复用已失效的 token）。
            if stale_rounds >= MAX_STALE_ROUNDS {
                warn!(stale_rounds, "连续多轮无有效消息，重新申请长链会话");
                session = None;
                stale_rounds = 0;
            } else if session
                .as_ref()
                .map(|s| s.wss_url != url)
                .unwrap_or(false)
            {
                session = None;
            }

            self.set_status(DanmakuStatus::Reconnecting);
            if !self.sleep_backoff(&mut backoff).await {
                break;
            }
        }

        self.set_status(DanmakuStatus::Stopped);
        info!("B 站弹幕客户端已退出");
        Ok(())
    }

    /// 退避等待；返回 `false` 表示期间被请求停止。每次等待后退避翻倍（上限 60 秒）。
    async fn sleep_backoff(&self, backoff: &mut u64) -> bool {
        let wait = *backoff;
        debug!(wait_ms = wait, "退避等待后重连");
        let stopped = Arc::clone(&self.stopped);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(wait)) => {}
            _ = async move {
                while !stopped.load(Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            } => { return false; }
        }
        *backoff = (*backoff * 2).min(BACKOFF_MAX_MS);
        !self.is_stopped()
    }

    /// 建立一次连接并处理消息，直到断开。
    ///
    /// 返回值为「处理过的有效消息条数」，用于判断链路是否真的在工作。
    async fn run_connection(&self, session: &StartSession) -> anyhow::Result<u64> {
        let mut connection = self.transport.connect(&session.wss_url).await?;
        debug!(url = %session.wss_url, "WS 已连接，发送鉴权帧");

        // ⚠️ 鉴权帧必须是**二进制协议包**（headerLen=16, op=OP_AUTH, body=auth_body），
        // 不能把 auth_body 当普通文本帧发出去——
        // 那样服务端拿到的是裸 JSON，会立刻 reset 连接
        // （表现为 `WebSocket protocol error: Connection reset without closing handshake`）。
        let auth_frame = build_packet(OP_AUTH, &session.auth_body, PROTO_VERSION_PLAIN);
        connection.send_binary(&auth_frame).await?;

        let interval = session.heartbeat_interval.clamp(1, MAX_HEARTBEAT_SECS);
        let mut ticker = tokio::time::interval(Duration::from_secs(interval));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // 第一次 tick 立即触发，跳过它（鉴权帧刚发过，不必马上再发心跳）。
        ticker.tick().await;

        let idle_deadline = tokio::time::sleep(Duration::from_secs(IDLE_TIMEOUT_SECS));
        tokio::pin!(idle_deadline);

        let mut handled: u64 = 0;

        loop {
            if self.is_stopped() {
                return Ok(handled);
            }

            tokio::select! {
                frame = connection.next_frame() => {
                    match frame? {
                        Some(bytes) => {
                            idle_deadline.as_mut().reset(
                                tokio::time::Instant::now() + Duration::from_secs(IDLE_TIMEOUT_SECS),
                            );
                            handled += self.handle_protocol_frame(&bytes);
                        }
                        None => {
                            info!("WS 连接已由对端关闭");
                            return Ok(handled);
                        }
                    }
                }
                _ = ticker.tick() => {
                    // 心跳同样是二进制包：op=OP_HEARTBEAT，body 为空。
                    let heartbeat = build_packet(OP_HEARTBEAT, "", PROTO_VERSION_PLAIN);
                    connection.send_binary(&heartbeat).await?;
                }
                _ = &mut idle_deadline => {
                    warn!(secs = IDLE_TIMEOUT_SECS, "长时间未收到任何消息，主动重连");
                    return Ok(handled);
                }
                // ⚠️ 必须能被「停止」打断。
                //
                // 早期循环只阻塞在 `next_frame()` 上，`stop()` 设的标志要等到
                // 下一次收到帧（或 90 秒空闲超时）才有机会被检查——于是
                // 「主动断开」等 3 秒没等到就放弃，旧任务却在后台继续跑、
                // 继续重连，退出时还把状态写成「已停止」，把新连接覆盖掉。
                _ = async {
                    while !self.is_stopped() {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                } => {
                    debug!("收到停止请求，立即结束连接循环");
                    return Ok(handled);
                }
            }
        }
    }

    /// 解包一个协议帧并处理其中的消息，返回有效消息条数。
    ///
    /// 处理三种情况：
    ///  - `ver=0`：body 就是明文 JSON；
    ///  - `ver=2`：body 是 zlib 压缩的数据，解压后可能含有**多个**子包（递归处理）；
    ///  - 其它 op（心跳回复等）：只记日志。
    fn handle_protocol_frame(&self, bytes: &[u8]) -> u64 {
        let Some((version, operation, body)) = parse_packet(bytes) else {
            debug!(len = bytes.len(), "无法解析协议包（头长度或长度字段异常）");
            return 0;
        };

        match operation {
            OP_AUTH_REPLY => {
                // ⚠️ 开放平台的鉴权回复 body 是 `{"code":0}`，**没有 `cmd` 字段**，
                // 因此不能丢给按 cmd 分派的 `handle_frame`（那样永远匹配不上，
                // 状态会一直停在「正在连接」）。这里直接按 code 判定。
                let text = String::from_utf8_lossy(body).to_string();
                let code = serde_json::from_str::<Value>(&text)
                    .ok()
                    .and_then(|v| v.get("code").and_then(|c| c.as_i64()))
                    .unwrap_or(-1);
                if code == 0 {
                    info!("B 站弹幕鉴权成功");
                    self.set_status(DanmakuStatus::Connected);
                    return 1;
                }
                self.set_error(format!("鉴权失败：code={code}，响应={}", auth::truncate(&text, 200)));
                return 0;
            }
            OP_HEARTBEAT_REPLY => {
                debug!("收到心跳回复");
                return 0;
            }
            OP_MESSAGE => {}
            other => {
                debug!(operation = other, "收到未处理的操作码");
                return 0;
            }
        }

        // OP_MESSAGE：可能是明文，也可能是 zlib 压缩的多包
        if version == PROTO_VERSION_ZLIB {
            match inflate(body) {
                Some(decompressed) => {
                    // 解压后可能是若干完整子包，逐个递归解包
                    let mut total = 0u64;
                    let mut offset = 0usize;
                    while offset + PACKET_HEADER_LEN as usize <= decompressed.len() {
                        let Some((_, _, _)) = parse_packet(&decompressed[offset..]) else {
                            break;
                        };
                        let packet_len =
                            u32::from_be_bytes(decompressed[offset..offset + 4].try_into().unwrap())
                                as usize;
                        if packet_len == 0 || offset + packet_len > decompressed.len() {
                            break;
                        }
                        total += self.handle_protocol_frame(&decompressed[offset..offset + packet_len]);
                        offset += packet_len;
                    }
                    return total;
                }
                None => {
                    debug!(len = body.len(), "zlib 解压失败，丢弃该帧");
                    return 0;
                }
            }
        }

        let text = String::from_utf8_lossy(body).to_string();
        if self.handle_frame(&text) {
            1
        } else {
            0
        }
    }

    /// 处理一帧文本报文；返回是否是一条有效业务消息。
    fn handle_frame(&self, text: &str) -> bool {
        let value: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(err) => {
                debug!(error = %err, raw = %auth::truncate(text, 200), "收到非 JSON 报文");
                return false;
            }
        };

        self.broadcaster.emit(DanmakuEvent::Raw(value.clone()));

        match extract_command(&value) {
            Some(CMD_DANMAKU) => {
                let Some(data) = extract_data(&value) else {
                    return false;
                };
                match parse_danmaku(data) {
                    Some(message) => {
                        let danmaku = message.into_danmaku();
                        debug!(user = %danmaku.user, text = %danmaku.text, "收到弹幕");
                        self.broadcaster.emit(DanmakuEvent::Message(danmaku));
                        true
                    }
                    None => {
                        warn!(raw = %auth::truncate(text, 300), "弹幕报文解析失败");
                        false
                    }
                }
            }
            Some(CMD_AUTH) => {
                let code = extract_data(&value)
                    .and_then(|d| d.get("code"))
                    .and_then(|c| c.as_i64())
                    .unwrap_or(0);
                if code == 0 {
                    info!("B 站弹幕鉴权成功");
                    self.set_status(DanmakuStatus::Connected);
                } else {
                    self.set_error(format!("鉴权失败：code={code}"));
                }
                false
            }
            Some(CMD_HEARTBEAT) => {
                // 平台心跳，回一个 pong 即可（真正的发送在下一次 tick，这里只记录）。
                debug!("收到平台心跳");
                false
            }
            Some(other) => {
                debug!(cmd = other, "收到未处理的消息类型");
                false
            }
            None => false,
        }
    }
}

#[async_trait]
impl DanmakuClient for BilibiliDanmakuClient {
    async fn run(self: Arc<Self>) -> anyhow::Result<()> {
        let this = Arc::clone(&self);
        // 主循环放在单独任务里，`run` 本身可被 await 也支持后台 spawn。
        this.run_loop().await
    }

    fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
        self.set_status(DanmakuStatus::Stopped);
        info!("已请求停止 B 站弹幕客户端");
    }

    fn status(&self) -> DanmakuStatus {
        self.status.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// 解压 zlib 数据（B 站 ver=2 的包体）。
///
/// 失败返回 `None`：网络数据不可信，遇到坏包应丢弃并继续，不能 panic。
pub fn inflate(data: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut decoder = flate2::read::ZlibDecoder::new(data);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).ok()?;
    Some(out)
}

/// 演示/测试用的空实现：不连接任何服务，只维护状态。
///
/// 用于「没有身份码也想验证前端链路」的场景。
pub struct NoopDanmakuClient {
    status: std::sync::Mutex<DanmakuStatus>,
    broadcaster: DanmakuBroadcaster,
}

impl NoopDanmakuClient {
    /// 创建空实现。
    pub fn new(broadcaster: DanmakuBroadcaster) -> Self {
        Self {
            status: std::sync::Mutex::new(DanmakuStatus::Idle),
            broadcaster,
        }
    }

    /// 更新状态并广播。
    fn set_status(&self, next: DanmakuStatus) {
        *self.status.lock().unwrap_or_else(|e| e.into_inner()) = next.clone();
        self.broadcaster.emit(DanmakuEvent::Status(next));
    }
}

#[async_trait]
impl DanmakuClient for NoopDanmakuClient {
    async fn run(self: Arc<Self>) -> anyhow::Result<()> {
        self.set_status(DanmakuStatus::Connecting);
        info!("弹幕客户端为占位实现（未配置身份码）");
        self.set_status(DanmakuStatus::Idle);
        Ok(())
    }

    fn stop(&self) {
        self.set_status(DanmakuStatus::Stopped);
    }

    fn status(&self) -> DanmakuStatus {
        self.status.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// 由弹幕文本构造 [`Danmaku`] 的小工具（供测试与模拟接口使用）。
pub fn make_danmaku(
    user: impl Into<String>,
    uid: Option<String>,
    text: impl Into<String>,
    at: DateTime<Utc>,
) -> Danmaku {
    Danmaku {
        user: user.into(),
        uid,
        text: text.into(),
        at,
        fans_medal_level: 0,
        user_level: 0,
        from_host: false,
    }
}

/// 构造一条标准弹幕报文（供模拟接口与测试使用）。
pub fn sample_danmaku_frame(user: &str, uid: u64, text: &str) -> Value {
    serde_json::json!({
        "cmd": CMD_DANMAKU,
        "data": {
            "uname": user,
            "uid": uid,
            "msg": text,
            "fans_medal_level": 5,
            "user_level": 12,
            "timestamp": Utc::now().timestamp()
        }
    })
}

/// 把 [`BilibiliError`] 转成前端可读文本（保留平台错误码）。
pub fn describe_error(err: &BilibiliError) -> String {
    err.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AppState, BilibiliState, PlayerState};
    use crate::VERSION;
    use std::sync::Mutex;

    #[test]
    fn parses_object_data() {
        let frame = sample_danmaku_frame("观众甲", 9527, "点歌 晴天 周杰伦");
        assert_eq!(extract_command(&frame), Some(CMD_DANMAKU));
        let message = parse_danmaku(extract_data(&frame).unwrap()).expect("应解析成功");
        assert_eq!(message.user, "观众甲");
        assert_eq!(message.text, "点歌 晴天 周杰伦");
        assert_eq!(message.uid.as_ref().and_then(|v| v.as_u64()), Some(9527));
        assert_eq!(message.fans_medal_level, 5);
        assert_eq!(message.user_level, 12);
    }

    #[test]
    fn parses_stringified_json_data() {
        // 平台有时把 data 再序列化成字符串
        let data = Value::String(
            r#"{"uname":"观众乙","uid":"1001","msg":"点歌 富士山下","fans_medal_level":0,"user_level":3}"#
                .to_string(),
        );
        let message = parse_danmaku(&data).expect("应解析成功");
        assert_eq!(message.user, "观众乙");
        let danmaku = message.into_danmaku();
        assert_eq!(danmaku.uid.as_deref(), Some("1001"));
        assert_eq!(danmaku.text, "点歌 富士山下");
    }

    #[test]
    fn returns_none_for_unknown_shape() {
        assert!(parse_danmaku(&Value::String("not json".into())).is_none());
        assert!(parse_danmaku(&serde_json::json!(123)).is_none());
    }

    #[test]
    fn auth_packet_has_correct_header() {
        // 官方协议：packetLen(4) | headerLen=16(2) | ver=0(2) | op=7(4) | seq=0(4) | body
        let body = r#"{"roomid":1,"protover":2}"#;
        let frame = build_packet(OP_AUTH, body, PROTO_VERSION_PLAIN);
        assert_eq!(
            frame.len(),
            PACKET_HEADER_LEN as usize + body.len(),
            "总长度 = 头 + body"
        );
        let packet_len = u32::from_be_bytes(frame[0..4].try_into().unwrap());
        assert_eq!(packet_len as usize, frame.len(), "长度字段必须等于实际长度");
        assert_eq!(
            u16::from_be_bytes(frame[4..6].try_into().unwrap()),
            16,
            "头长度固定 16"
        );
        assert_eq!(i16::from_be_bytes(frame[6..8].try_into().unwrap()), 0);
        assert_eq!(
            i32::from_be_bytes(frame[8..12].try_into().unwrap()),
            OP_AUTH,
            "鉴权包的 op 必须是 7"
        );
        assert_eq!(
            i32::from_be_bytes(frame[12..16].try_into().unwrap()),
            0,
            "seq 固定 0"
        );
        assert_eq!(&frame[16..], body.as_bytes());
    }

    #[test]
    fn heartbeat_packet_is_16_bytes_with_empty_body() {
        let frame = build_packet(OP_HEARTBEAT, "", PROTO_VERSION_PLAIN);
        assert_eq!(frame.len(), 16, "空 body 的心跳包只有头");
        assert_eq!(
            i32::from_be_bytes(frame[8..12].try_into().unwrap()),
            OP_HEARTBEAT
        );
    }

    #[test]
    fn parse_packet_roundtrip_and_rejects_garbage() {
        let frame = build_packet(OP_MESSAGE, r#"{"cmd":"DANMU_MSG"}"#, PROTO_VERSION_PLAIN);
        let (version, op, body) = parse_packet(&frame).expect("应能解包");
        assert_eq!(version, 0);
        assert_eq!(op, OP_MESSAGE);
        assert_eq!(body, br#"{"cmd":"DANMU_MSG"}"#);

        // 网络数据不可信：不足 16 字节、头长度异常、长度字段越界都要安全返回 None
        assert!(parse_packet(&[0u8; 8]).is_none());
        let mut bad_header = frame.clone();
        bad_header[4..6].copy_from_slice(&8u16.to_be_bytes());
        assert!(parse_packet(&bad_header).is_none());
        let mut too_long = frame.clone();
        too_long[0..4].copy_from_slice(&9999u32.to_be_bytes());
        assert!(parse_packet(&too_long).is_none());
    }

    #[test]
    fn inflate_decodes_zlib_payload() {
        use flate2::write::ZlibEncoder;
        use std::io::Write;

        let original = br#"{"cmd":"DANMU_MSG"}"#.to_vec();
        let mut encoder = ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&original).unwrap();
        let compressed = encoder.finish().unwrap();

        assert_eq!(inflate(&compressed).unwrap(), original, "应能还原原文");
        assert!(inflate(b"not zlib at all").is_none(), "坏包必须安全返回 None");
    }

    /// 模拟传输：把预设的协议包按顺序吐出，并记录客户端发出的帧。
    struct MockTransport {
        frames: Mutex<Vec<Vec<u8>>>,
        sent: Arc<Mutex<Vec<String>>>,
    }

    struct MockConnection {
        frames: Arc<Mutex<Vec<Vec<u8>>>>,
        sent: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl DanmakuConnection for MockConnection {
        async fn send_binary(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
            // 记录成可读形式，便于断言 op 与 body
            let (_, op, body) = parse_packet(bytes).expect("测试里发出的包应当合法");
            let text = String::from_utf8_lossy(body).to_string();
            self.sent
                .lock()
                .unwrap()
                .push(format!("op={op};body={text}"));
            Ok(())
        }

        async fn next_frame(&mut self) -> anyhow::Result<Option<Vec<u8>>> {
            let mut frames = self.frames.lock().unwrap();
            if frames.is_empty() {
                return Ok(None);
            }
            // 预置的帧按协议包解析后把 body 交给上层；
            // 这里直接返回原始字节，让 `handle_protocol_frame` 走完整解包路径。
            Ok(Some(frames.remove(0)))
        }
    }

    #[async_trait]
    impl DanmakuTransport for MockTransport {
        async fn connect(&self, _url: &str) -> anyhow::Result<Box<dyn DanmakuConnection>> {
            Ok(Box::new(MockConnection {
                frames: Arc::new(Mutex::new(
                    self.frames.lock().unwrap().drain(..).collect(),
                )),
                sent: Arc::clone(&self.sent),
            }))
        }
    }

    fn test_state() -> StateCell {
        StateCell::new(AppState::new(
            VERSION,
            PlayerState::default(),
            BilibiliState::default(),
        ))
    }

    fn test_auth() -> AuthConfig {
        AuthConfig {
            app_id: "1".into(),
            access_key_id: "k".into(),
            access_key_secret: "s".into(),
            code: "c".into(),
        }
    }

    /// 测试用的会话（只需要 wss 地址与鉴权体）。
    fn session_for_test() -> StartSession {
        StartSession {
            room_id: Some("1".into()),
            wss_url: "wss://example.invalid/sub".into(),
            auth_body: "{}".into(),
            heartbeat_interval: 20,
            conn_id: None,
            game_id: None,
        }
    }

    #[tokio::test]
    async fn stop_interrupts_a_blocked_read() {
        // 用户报告「显示已连接过了一会自己断掉」。
        //
        // 根因之一：连接循环只阻塞在 `next_frame()` 上，`stop()` 设的标志要等到
        // 下一帧（或 90 秒空闲超时）才被检查——于是「主动断开」等 3 秒没等到就放弃，
        // 旧任务却在后台继续跑，退出时又把状态写成「已停止」，覆盖掉新连接。
        // 现在停止请求能立即打断连接循环。
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        struct HangingConnection {
            dropped: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl DanmakuConnection for HangingConnection {
            async fn send_binary(&mut self, _data: &[u8]) -> anyhow::Result<()> {
                Ok(())
            }
            async fn next_frame(&mut self) -> anyhow::Result<Option<Vec<u8>>> {
                // 永远不会返回：模拟对端静默（连接假死）
                std::future::pending::<()>().await;
                Ok(None)
            }
        }

        impl Drop for HangingConnection {
            fn drop(&mut self) {
                self.dropped.fetch_add(1, AtomicOrdering::SeqCst);
            }
        }

        struct HangingTransport {
            dropped: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl DanmakuTransport for HangingTransport {
            async fn connect(&self, _url: &str) -> anyhow::Result<Box<dyn DanmakuConnection>> {
                Ok(Box::new(HangingConnection {
                    dropped: Arc::clone(&self.dropped),
                }))
            }
        }

        let dropped = Arc::new(AtomicUsize::new(0));
        let client = Arc::new(BilibiliDanmakuClient::new(
            test_auth(),
            test_state(),
            DanmakuBroadcaster::default(),
            Arc::new(HangingTransport {
                dropped: Arc::clone(&dropped),
            }),
        ));

        let runner = Arc::clone(&client);
        let handle = tokio::spawn(async move { runner.run_connection(&session_for_test()).await });

        // 让连接循环真正进入 select
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!handle.is_finished(), "循环应正阻塞在读帧上");

        client.stop();
        let finished = tokio::time::timeout(Duration::from_secs(2), handle).await;
        assert!(
            finished.is_ok(),
            "stop() 应立即打断阻塞的读帧（旧实现要等 90 秒空闲超时）"
        );
        assert_eq!(
            dropped.load(AtomicOrdering::SeqCst),
            1,
            "连接应被释放"
        );
    }

    #[tokio::test]
    async fn stale_client_cannot_overwrite_connection_state() {
        // 被放弃的旧任务绝不能再改界面状态：代号一变，它的写入全部作废。
        let state = test_state();
        let client = BilibiliDanmakuClient::new(
            test_auth(),
            state.clone(),
            DanmakuBroadcaster::default(),
            Arc::new(TungsteniteTransport::default()),
        );
        assert!(client.is_current(), "刚创建时应是当前连接");

        // 模拟控制器开始新连接：代号 +1
        state.mutate(|s| {
            s.bilibili.epoch = s.bilibili.epoch.next();
            s.bilibili.connected = true;
            s.bilibili.phase = BilibiliPhase::Connected;
        });

        // 旧任务尝试写「已停止」/错误 —— 都应被丢弃
        client.set_status(DanmakuStatus::Stopped);
        client.set_error("旧任务的错误");
        assert!(
            state.read().bilibili.connected,
            "旧任务的 Stopped 不应把新连接改回未连接"
        );
        assert_eq!(
            state.read().bilibili.phase,
            BilibiliPhase::Connected,
            "旧任务不应覆盖新连接的阶段"
        );
        assert!(
            state.read().bilibili.last_error.is_none(),
            "旧任务的错误不应写进新连接的状态"
        );
    }

    #[tokio::test]
    async fn handle_frame_emits_danmaku_and_updates_status() {
        let state = test_state();
        let broadcaster = DanmakuBroadcaster::default();
        let mut rx = broadcaster.subscribe();

        let client = BilibiliDanmakuClient::new(
            AuthConfig {
                app_id: "1".into(),
                access_key_id: "k".into(),
                access_key_secret: "s".into(),
                code: "c".into(),
            },
            state.clone(),
            broadcaster.clone(),
            Arc::new(TungsteniteTransport::default()),
        );

        // 鉴权成功
        let auth_frame = serde_json::json!({ "cmd": CMD_AUTH, "data": { "code": 0 } }).to_string();
        assert!(!client.handle_frame(&auth_frame));
        assert!(client.status().is_connected());
        assert!(state.read().bilibili.connected);

        // 弹幕
        let dm = sample_danmaku_frame("观众甲", 9527, "点歌 晴天 周杰伦").to_string();
        assert!(client.handle_frame(&dm));

        // 依次应收到：Status(Connected) → Raw(auth) → Status? → Raw(dm) → Message
        let mut got_message = false;
        while let Ok(ev) = rx.try_recv() {
            if let DanmakuEvent::Message(d) = ev {
                assert_eq!(d.text, "点歌 晴天 周杰伦");
                assert_eq!(d.user, "观众甲");
                got_message = true;
            }
        }
        assert!(got_message, "应广播出弹幕消息");
    }

    #[tokio::test]
    async fn auth_failure_records_error() {
        let state = test_state();
        let broadcaster = DanmakuBroadcaster::default();
        let client = BilibiliDanmakuClient::new(
            AuthConfig {
                app_id: "1".into(),
                access_key_id: "k".into(),
                access_key_secret: "s".into(),
                code: "c".into(),
            },
            state.clone(),
            broadcaster,
            Arc::new(TungsteniteTransport::default()),
        );

        let frame = serde_json::json!({ "cmd": CMD_AUTH, "data": { "code": 10001 } }).to_string();
        client.handle_frame(&frame);

        let guard = state.read();
        assert!(!guard.bilibili.connected);
        assert!(guard
            .bilibili
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("鉴权失败"));
    }

    #[tokio::test]
    async fn run_connection_sends_auth_then_handles_frames() {
        let state = test_state();
        let broadcaster = DanmakuBroadcaster::default();
        // 预置帧必须是**协议包**：服务端推的是 OP_AUTH_REPLY / OP_MESSAGE 二进制包。
        // 注意鉴权回复的 body 就只是 `{"code":0}`，**没有 cmd 字段**（与 Web 端协议不同）。
        let frames = vec![
            build_packet(OP_AUTH_REPLY, r#"{"code":0}"#, PROTO_VERSION_PLAIN),
            build_packet(
                OP_MESSAGE,
                &sample_danmaku_frame("观众乙", 1, "点歌 富士山下 陈奕迅").to_string(),
                PROTO_VERSION_PLAIN,
            ),
        ];
        let sent = Arc::new(Mutex::new(Vec::new()));
        let transport = Arc::new(MockTransport {
            frames: Mutex::new(frames),
            sent: Arc::clone(&sent),
        });
        let client = BilibiliDanmakuClient::new(
            AuthConfig {
                app_id: "1".into(),
                access_key_id: "k".into(),
                access_key_secret: "s".into(),
                code: "c".into(),
            },
            state.clone(),
            broadcaster.clone(),
            transport,
        );

        let mut rx = broadcaster.subscribe();
        let session = StartSession {
            wss_url: "wss://mock/ws".into(),
            auth_body: r#"{"key":"token"}"#.into(),
            heartbeat_interval: 20,
            room_id: Some("42".into()),
            conn_id: None,
            game_id: Some("game-1".into()),
        };

        let handled = client.run_connection(&session).await.expect("应正常结束");
        // 1 条鉴权回复 + 1 条弹幕消息
        assert_eq!(handled, 2, "鉴权回复与弹幕消息各计 1 条");
        assert!(state.read().bilibili.connected);

        // 首帧必须是 OP_AUTH 的**二进制协议包**，body 原样等于 auth_body。
        // （早期实现直接把 auth_body 当文本帧发，会被服务端立刻 reset。）
        let sent_frames = sent.lock().unwrap().clone();
        assert_eq!(
            sent_frames.first().map(String::as_str),
            Some(r#"op=7;body={"key":"token"}"#),
            "鉴权帧必须是 op=7 的协议包"
        );

        let mut texts = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let DanmakuEvent::Message(d) = ev {
                texts.push(d.text);
            }
        }
        assert_eq!(texts, vec!["点歌 富士山下 陈奕迅".to_string()]);
    }

    #[tokio::test]
    async fn stop_marks_status_and_is_idempotent() {
        let state = test_state();
        let broadcaster = DanmakuBroadcaster::default();
        let client = BilibiliDanmakuClient::new(
            AuthConfig {
                app_id: "1".into(),
                access_key_id: "k".into(),
                access_key_secret: "s".into(),
                code: "c".into(),
            },
            state,
            broadcaster,
            Arc::new(TungsteniteTransport::default()),
        );
        assert!(!client.is_stopped());
        client.stop();
        client.stop();
        assert!(client.is_stopped());
        assert_eq!(client.status(), DanmakuStatus::Stopped);
    }
}
