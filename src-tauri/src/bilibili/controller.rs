//! 弹幕客户端生命周期控制器。
//!
//! 为什么单独一层：`axum` 的 HTTP 处理器需要「连接 / 断开」两个动作，
//! 而 [`BilibiliDanmakuClient`] 的 `run` 是一个长驻任务。
//! 控制器负责持有任务句柄、保证同一时刻只有一个连接，并提供停止信号。
//!
//! ⚠️ 身份码限制：B 站同一身份码最多 5 个并发连接。本控制器**强制单连接**
//! （再次 `connect` 会先停掉旧连接），面板与控制台通过本地 `/ws` 复用数据。

use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use super::danmaku::{
    BilibiliDanmakuClient, DanmakuBroadcaster, DanmakuClient, DanmakuStatus, TungsteniteTransport,
};
use super::auth::AuthConfig;
use crate::state::StateCell;

/// 当前弹幕任务的内部状态。
struct RunningTask {
    /// 任务句柄。
    join: JoinHandle<()>,
    /// 中止句柄：`join` 会被 `await` 消耗掉，超时后要中止就得靠它。
    abort_handle: tokio::task::AbortHandle,
    /// 客户端引用（用于请求停止与查询状态）。
    client: Arc<BilibiliDanmakuClient>,
}

/// 弹幕连接控制器（可克隆，内部共享）。
#[derive(Clone)]
pub struct DanmakuHandle {
    current: Arc<Mutex<Option<RunningTask>>>,
    broadcaster: DanmakuBroadcaster,
    state: StateCell,
}

impl DanmakuHandle {
    /// 创建控制器。
    pub fn new(state: StateCell, broadcaster: DanmakuBroadcaster) -> Self {
        Self {
            current: Arc::new(Mutex::new(None)),
            broadcaster,
            state,
        }
    }

    /// 当前连接状态。
    pub async fn status(&self) -> DanmakuStatus {
        let guard = self.current.lock().await;
        match guard.as_ref() {
            Some(task) => task.client.status(),
            None => DanmakuStatus::Idle,
        }
    }

    /// 是否已连接。
    pub async fn is_connected(&self) -> bool {
        self.status().await.is_connected()
    }

    /// 建立连接（若已有连接会先停止）。
    ///
    /// 返回 `Err` 仅表示「无法启动任务」（例如配置校验失败），
    /// 真实的鉴权/网络错误会异步写入 `AppState.bilibili.last_error`。
    pub async fn connect(&self, auth: AuthConfig) -> anyhow::Result<()> {
        // 配置不完整时立刻失败，并把失败原因写进状态。
        //
        // ⚠️ 不能只 `return Err(...)`：那样状态会停留在上一次的值
        // （例如「正在申请会话…」），界面看起来像还在连，
        // 用户完全不知道是因为少填了字段而根本没开始。
        if let Err(err) = auth.validate() {
            self.state.mutate(|s| {
                s.bilibili.connected = false;
                s.bilibili.phase = crate::models::BilibiliPhase::Failed;
                s.bilibili.detail = Some(err.to_string());
                s.bilibili.last_error = Some(err.to_string());
                s.bilibili.updated_at = Some(chrono::Utc::now().to_rfc3339());
            });
            return Err(err.into());
        }

        // 先停旧连接，保证单连接不变量。
        self.disconnect().await;

        // 分配新代号：此后旧任务的任何状态写入都会被丢弃（见 `ConnectionEpoch`）。
        // 这一步必须在 `disconnect()` **之后**：disconnect 自己会把状态落到「已停止」。
        let epoch = self.state.mutate(|s| {
            let next = s.bilibili.epoch.next();
            s.bilibili.epoch = next;
            next
        });

        let client = Arc::new(BilibiliDanmakuClient::new(
            auth,
            self.state.clone(),
            self.broadcaster.clone(),
            Arc::new(TungsteniteTransport::default()),
        ));

        // 连接前先清空上一次的错误，并把进度重置到「正在申请会话」。
        // 用户点完「保存并连接」必须立刻看到反馈，而不是停在旧状态上。
        self.state.mutate(|s| {
            s.bilibili.last_error = None;
            s.bilibili.connected = false;
            s.bilibili.phase = crate::models::BilibiliPhase::RequestingSession;
            s.bilibili.detail = Some("正在申请身份码会话…".to_string());
            s.bilibili.attempts = 0;
            s.bilibili.updated_at = Some(chrono::Utc::now().to_rfc3339());
        });

        let runner = Arc::clone(&client);
        let join = tokio::spawn(async move {
            if let Err(err) = runner.run().await {
                warn!(error = %err, "弹幕客户端任务异常结束");
            }
        });
        let abort_handle = join.abort_handle();

        info!(?epoch, "B 站弹幕连接任务已启动（单连接模式）");
        *self.current.lock().await = Some(RunningTask {
            join,
            abort_handle,
            client,
        });
        Ok(())
    }

    /// 主动断开连接。
    ///
    /// ⚠️ 旧任务退出可能很慢（卡在一次 WS 读或退避睡眠里），因此这里**必须**做两件事：
    ///  1. 超时后 `abort()` 真正中止它，而不是只"放弃等待"——
    ///     否则它会继续跑、继续重连，与新连接抢同一条身份码会话；
    ///  2. 断开前先让代号失效（由 `connect` 负责递增），旧任务此后写状态会被丢弃。
    pub async fn disconnect(&self) {
        let mut guard = self.current.lock().await;
        if let Some(task) = guard.take() {
            task.client.stop();
            // 给任务 3 秒体面退出；超时则强制中止，避免拖住 HTTP 响应。
            if tokio::time::timeout(std::time::Duration::from_secs(3), task.join)
                .await
                .is_err()
            {
                warn!("弹幕任务未在 3 秒内退出，强制中止");
                task.abort_handle.abort();
            }
        }
        // 明确落到「已停止」：否则界面会停在最后一个中间阶段
        // （例如「正在连接…」），让人以为还在尝试。
        self.state.mutate(|s| {
            s.bilibili.connected = false;
            s.bilibili.phase = crate::models::BilibiliPhase::Stopped;
            s.bilibili.detail = Some("已断开连接".to_string());
            s.bilibili.updated_at = Some(chrono::Utc::now().to_rfc3339());
        });
    }

    /// 把一条弹幕事件注入广播器（用于 `simulate` 接口与测试）。
    pub fn emit(&self, event: super::danmaku::DanmakuEvent) {
        self.broadcaster.emit(event);
    }

    /// 事件广播器（供事件转发任务订阅）。
    pub fn broadcaster(&self) -> &DanmakuBroadcaster {
        &self.broadcaster
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AppState, BilibiliState, PlayerState};
    use crate::VERSION;

    fn state() -> StateCell {
        StateCell::new(AppState::new(
            VERSION,
            PlayerState::default(),
            BilibiliState::default(),
        ))
    }

    #[tokio::test]
    async fn reject_incomplete_config() {
        let handle = DanmakuHandle::new(state(), DanmakuBroadcaster::default());
        let auth = AuthConfig {
            app_id: String::new(),
            access_key_id: "k".into(),
            access_key_secret: "s".into(),
            code: "c".into(),
        };
        assert!(handle.connect(auth).await.is_err());
        assert_eq!(handle.status().await, DanmakuStatus::Idle);
    }

    #[tokio::test]
    async fn disconnect_is_idempotent() {
        let handle = DanmakuHandle::new(state(), DanmakuBroadcaster::default());
        handle.disconnect().await;
        handle.disconnect().await;
        assert_eq!(handle.status().await, DanmakuStatus::Idle);
    }
}
