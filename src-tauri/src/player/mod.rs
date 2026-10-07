//! 播放器抽象层。
//!
//! 上层（播放控制器、HTTP API）只依赖 [`PlayerBackend`] 这组方法；
//! 具体实现见 [`mpv`]（mpv sidecar + JSON IPC）。
//!
//! 为什么要有抽象：mpv 的 IPC 在 Windows 上是命名管道、在 macOS/Linux 上是
//! Unix socket，且测试环境不一定装了 mpv。抽象后可以注入 [`MockPlayer`]，
//! 让「队列 → 播放 → 自动下一首」的状态机能完整地离线测试。

pub mod controller;
pub mod idle;
pub mod lyrics;
pub mod mpv;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use controller::{next_candidate, PlaybackMode, PlayerController};
pub use idle::{idle_next_index, should_switch_now, shuffle_pick};
pub use lyrics::{LyricLine, Lyrics};
pub use mpv::{MpvConfig, MpvController, MpvEvent};

/// 播放器状态机（与 `models::PlayerState` 是不同概念：
/// 这里是播放器内部的粗粒度状态，`PlayerState` 是给前端展示的快照）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackStatus {
    /// 空闲（未加载任何文件）
    Idle,
    /// 播放中
    Playing,
    /// 已暂停
    Paused,
}

impl Default for PlaybackStatus {
    fn default() -> Self {
        PlaybackStatus::Idle
    }
}

/// 播放器错误。
#[derive(Debug, Error)]
pub enum PlayerError {
    /// 未找到可执行的 mpv。
    #[error("{0}")]
    BinaryNotFound(String),
    /// 无法连接 mpv 的 IPC 管道。
    #[error("无法连接 mpv IPC 管道 {pipe}：{source}")]
    IpcConnect {
        /// 管道/套接字名。
        pipe: String,
        /// 底层错误。
        source: std::io::Error,
    },
    /// 命令发送失败（播放器可能已退出）。
    #[error("向 mpv 发送命令失败：{0}")]
    Send(String),
    /// 未指定要播放的文件。
    #[error("没有可播放的歌曲：{0}")]
    NothingToPlay(String),
    /// 尚未实现。
    #[error("功能尚未实现：{0}")]
    Unimplemented(&'static str),
}

/// 播放器向后抛出的语义事件。
///
/// 与 [`MpvEvent`] 的区别：`MpvEvent` 是 mpv 的原始事件（含 property-change 细节），
/// `PlayerEvent` 是上层关心的高层信号，播放控制器依赖它驱动「自动下一首」。
#[derive(Debug, Clone, PartialEq)]
pub enum PlayerEvent {
    /// 已开始播放某个文件（`url` 为加载时的地址）。
    Started {
        /// 音频地址。
        url: String,
    },
    /// 暂停状态变化。
    Paused(bool),
    /// 播放位置（秒）。
    Position(f64),
    /// 时长（秒）。
    Duration(f64),
    /// 播放结束；播放控制器据此推进队列。`reason` 来自播放器（`eof` / `stop` / `error`）。
    Ended {
        /// 结束原因。
        reason: String,
    },
}

/// 播放器后端能力。
#[async_trait]
pub trait PlayerBackend: Send + Sync {
    /// 加载并播放一个音频地址。
    async fn load(&self, url: &str) -> Result<(), PlayerError>;

    /// 暂停 / 继续。
    async fn set_paused(&self, paused: bool) -> Result<(), PlayerError>;

    /// 停止当前播放。
    async fn stop(&self) -> Result<(), PlayerError>;

    /// 设置音量（0-100）。
    async fn set_volume(&self, volume: u8) -> Result<(), PlayerError>;

    /// 跳转到指定位置（秒）。
    async fn seek(&self, position: f64) -> Result<(), PlayerError>;

    /// 主动查询当前播放位置（秒）。
    ///
    /// 为什么需要它：mpv 只在 `time-pos` **发生变化**时推送 `property-change`，
    /// 加载网络流的头几秒可能一个事件都没有，导致进度条长时间停在 0。
    /// 控制器会在「距上次事件超过 3 秒」时调用本方法兜底。
    ///
    /// 默认返回 `None`（后端不支持查询），不影响其他实现。
    async fn current_position(&self) -> Option<f64> {
        None
    }

    /// 当前状态。
    fn status(&self) -> PlaybackStatus;

    /// 优雅退出播放器。
    async fn shutdown(&self) -> Result<(), PlayerError>;
}

/// 空实现：不真正播放，只记录调用，用于开发与测试。
///
/// 关键点：它也能产生 [`PlayerEvent`]，因此「队列 → 播放 → 播完自动下一首」
/// 的完整链路可以完全离线测试。
pub struct MockPlayer {
    status: std::sync::Mutex<PlaybackStatus>,
    volume: std::sync::Mutex<u8>,
    log: std::sync::Mutex<Vec<String>>,
    events: tokio::sync::broadcast::Sender<PlayerEvent>,
    /// 命中这些地址时 `load` 返回失败（测试"播不了"的曲目）。
    failing_urls: std::sync::Mutex<Vec<String>>,
}

impl Default for MockPlayer {
    fn default() -> Self {
        Self::new()
    }
}

impl MockPlayer {
    /// 创建空实现。
    pub fn new() -> Self {
        let (events, _) = tokio::sync::broadcast::channel(64);
        Self {
            status: std::sync::Mutex::new(PlaybackStatus::Idle),
            volume: std::sync::Mutex::new(80),
            log: std::sync::Mutex::new(Vec::new()),
            events,
            failing_urls: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// 指定这些地址的 `load` 一律失败（模拟版权受限等播不了的情况）。
    pub fn fail_urls(&self, urls: &[&str]) {
        *self.failing_urls.lock().unwrap_or_else(|e| e.into_inner()) =
            urls.iter().map(|u| u.to_string()).collect();
    }

    /// 已记录的操作日志（测试断言用）。
    pub fn log(&self) -> Vec<String> {
        self.log.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 订阅事件（测试里用它驱动「播完」）。
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<PlayerEvent> {
        self.events.subscribe()
    }

    /// 手动推送一个事件（测试模拟 mpv 的 `end-file`）。
    pub fn emit(&self, event: PlayerEvent) {
        let _ = self.events.send(event);
    }

    /// 当前音量。
    pub fn volume(&self) -> u8 {
        *self.volume.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn record(&self, entry: impl Into<String>) {
        self.log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(entry.into());
    }
}

#[async_trait]
impl PlayerBackend for MockPlayer {
    async fn load(&self, url: &str) -> Result<(), PlayerError> {
        self.record(format!("load {url}"));
        // 测试可指定"播不了"的地址，用于验证失败路径不会破坏播放历史
        if self
            .failing_urls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|u| u == url)
        {
            self.record(format!("load-failed {url}"));
            return Err(PlayerError::Send(format!("模拟加载失败：{url}")));
        }
        *self.status.lock().unwrap_or_else(|e| e.into_inner()) = PlaybackStatus::Playing;
        let _ = self.events.send(PlayerEvent::Started {
            url: url.to_string(),
        });
        Ok(())
    }

    async fn set_paused(&self, paused: bool) -> Result<(), PlayerError> {
        self.record(format!("pause {paused}"));
        *self.status.lock().unwrap_or_else(|e| e.into_inner()) = if paused {
            PlaybackStatus::Paused
        } else {
            PlaybackStatus::Playing
        };
        let _ = self.events.send(PlayerEvent::Paused(paused));
        Ok(())
    }

    async fn stop(&self) -> Result<(), PlayerError> {
        self.record("stop");
        *self.status.lock().unwrap_or_else(|e| e.into_inner()) = PlaybackStatus::Idle;
        let _ = self.events.send(PlayerEvent::Ended {
            reason: "stop".to_string(),
        });
        Ok(())
    }

    async fn set_volume(&self, volume: u8) -> Result<(), PlayerError> {
        let volume = volume.min(100);
        self.record(format!("volume {volume}"));
        *self.volume.lock().unwrap_or_else(|e| e.into_inner()) = volume;
        Ok(())
    }

    async fn seek(&self, position: f64) -> Result<(), PlayerError> {
        self.record(format!("seek {position}"));
        let _ = self.events.send(PlayerEvent::Position(position));
        Ok(())
    }

    fn status(&self) -> PlaybackStatus {
        *self.status.lock().unwrap_or_else(|e| e.into_inner())
    }

    async fn shutdown(&self) -> Result<(), PlayerError> {
        self.record("shutdown");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_player_tracks_status_and_volume() {
        let player = MockPlayer::new();
        assert_eq!(player.status(), PlaybackStatus::Idle);
        player.load("http://example/a.mp3").await.unwrap();
        assert_eq!(player.status(), PlaybackStatus::Playing);
        player.set_paused(true).await.unwrap();
        assert_eq!(player.status(), PlaybackStatus::Paused);
        player.set_volume(150).await.unwrap();
        assert_eq!(player.volume(), 100, "音量应被裁剪到 100");
        player.stop().await.unwrap();
        assert_eq!(player.status(), PlaybackStatus::Idle);
    }

    #[tokio::test]
    async fn mock_player_emits_events() {
        let player = MockPlayer::new();
        let mut rx = player.subscribe();
        player.load("http://example/a.mp3").await.unwrap();
        assert_eq!(
            rx.recv().await.unwrap(),
            PlayerEvent::Started {
                url: "http://example/a.mp3".into()
            }
        );
        player.stop().await.unwrap();
        assert_eq!(
            rx.recv().await.unwrap(),
            PlayerEvent::Ended {
                reason: "stop".into()
            }
        );
    }

    #[test]
    fn playback_status_default_is_idle() {
        assert_eq!(PlaybackStatus::default(), PlaybackStatus::Idle);
    }
}
