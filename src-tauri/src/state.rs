//! 全局状态与广播总线。
//!
//! 设计要点：
//!  - **唯一真源**：`AppState` 只保存在 `StateCell` 里，所有写入都经过它，
//!    写完后自动把新的快照广播给所有 WS 客户端，避免各处手写「记得推送」。
//!  - **无锁读 + 短锁写**：使用 `std::sync::RwLock`；写入只做内存变更，
//!    绝不在持锁期间做 await（这正是标准库锁优于 tokio 锁的场合）。
//!  - **总线容量**：广播通道按 256 条缓冲；订阅者跟不上时收到 `Lagged`，
//!    此时服务器会补发一次全量快照（见 `server::ws`），保证前端最终一致。

use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::config::Config;
use crate::event::{Event, HubEvent};
use crate::models::AppState;

/// 广播通道容量。状态 + 弹幕混合推送，256 足够覆盖瞬时突发。
pub const BROADCAST_CAPACITY: usize = 256;

/// 应用全局状态单元。
#[derive(Clone)]
pub struct StateCell {
    inner: Arc<RwLock<AppState>>,
    tx: broadcast::Sender<HubEvent>,
}

impl StateCell {
    /// 用初始状态创建状态单元与配套的广播通道。
    pub fn new(initial: AppState) -> Self {
        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            inner: Arc::new(RwLock::new(initial)),
            tx,
        }
    }

    /// 只读访问（守卫不可跨 await 持有）。
    pub fn read(&self) -> RwLockReadGuard<'_, AppState> {
        self.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    /// 克隆一份完整快照，用于序列化返回给 HTTP / WS 客户端。
    pub fn snapshot(&self) -> AppState {
        self.read().clone()
    }

    /// 可变访问。⚠️ 不要跨 await 持有守卫；如需在持锁期间推送，请用 [`StateCell::mutate`]。
    pub fn write(&self) -> RwLockWriteGuard<'_, AppState> {
        self.inner.write().unwrap_or_else(|e| e.into_inner())
    }

    /// 在闭包内修改状态，随后自动广播全量快照。
    pub fn mutate<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut AppState) -> R,
    {
        let result = {
            let mut guard = self.write();
            f(&mut guard)
        };
        self.broadcast_state();
        result
    }

    /// 广播当前状态快照。
    pub fn broadcast_state(&self) {
        let snapshot = Box::new(self.snapshot());
        self.publish(HubEvent::State(snapshot));
    }

    /// 订阅实时消息（每个 WS 连接一个订阅者）。
    pub fn subscribe(&self) -> broadcast::Receiver<HubEvent> {
        self.tx.subscribe()
    }

    /// 直接推送一条 hub 消息。
    pub fn publish(&self, ev: HubEvent) {
        // 没有订阅者时会返回 Err，属于正常情况（例如 exe 刚启动、OBS 还没打开）。
        if let Err(err) = self.tx.send(ev) {
            debug!(error = %err, "当前没有 WS 订阅者，消息被丢弃");
        }
    }
}

/// 内部事件总线（弹幕、连接状态等）。
///
/// 之所以与 `StateCell` 分开：状态是「可查询的快照」，事件是「一次性的通知」，
/// 两者生命周期与订阅者语义不同，混在一起会让 WS 层难以区分补发策略。
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

impl EventBus {
    /// 创建事件总线。
    pub fn new(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self { tx }
    }

    /// 发布内部事件；无订阅者时静默丢弃。
    pub fn emit(&self, ev: Event) {
        if self.tx.send(ev).is_err() {
            debug!("事件总线无订阅者");
        }
    }

    /// 订阅内部事件。
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(128)
    }
}

/// 服务器启动后返回给调用方的句柄。
pub struct ServerHandles {
    /// 实际监听端口（配置为 0 时由系统分配，这里回填真实端口）。
    pub port: u16,
    /// 全局状态。
    pub state: StateCell,
    /// 内部事件总线。
    pub events: EventBus,
    /// 弹幕连接控制器；服务器没起来时（降级模式）为 `None`。
    pub danmaku: Option<crate::bilibili::DanmakuHandle>,
    /// 音乐平台服务（阶段 5）；降级模式下为 `None`。
    pub music: Option<std::sync::Arc<crate::music::MusicService>>,
    /// 播放控制器（阶段 6）；未找到 mpv 时为 `None`。
    pub player: Option<std::sync::Arc<crate::player::PlayerController>>,
    /// 后端任务句柄；Tauri 退出时无需显式 join。
    /// 降级模式（服务器没起来）下为 `None`。
    pub join: Option<tokio::task::JoinHandle<()>>,
}

impl ServerHandles {
    /// 当前配置下的面板地址（用于打印到日志与前端展示）。
    pub fn panel_url(&self) -> String {
        format!("http://127.0.0.1:{}/panel", self.port)
    }

    /// 控制台地址。
    pub fn dashboard_url(&self) -> String {
        format!("http://127.0.0.1:{}/dashboard", self.port)
    }

    /// 触发一次状态广播（外部改动 `AppState` 后调用）。
    pub fn notify(&self) {
        self.state.broadcast_state();
    }
}

/// 便捷函数：把配置里的错误信息写进 `BilibiliState`，同时广播。
pub fn report_bilibili_error(state: &StateCell, message: impl Into<String>) {
    let msg = message.into();
    warn!(%msg, "B 站连接异常");
    state.mutate(|s| {
        s.bilibili.connected = false;
        s.bilibili.last_error = Some(msg.clone());
    });
}

/// 便捷函数：写入配置（配置本身不属于 `AppState`，由调用方持有）。
pub fn log_config_summary(cfg: &Config) {
    debug!(
        base_url = %cfg.base_url(),
        auto_connect = cfg.bilibili.auto_connect,
        "配置摘要"
    );
}
