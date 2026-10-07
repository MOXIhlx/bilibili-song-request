//! mpv 子进程 + JSON IPC 控制。
//!
//! ## 为什么是 mpv
//! 需求文档明确要求：**音频必须由 mpv 直接输出到系统音频设备**，OBS 只负责显示面板、
//! 通过「桌面音频」采集。这样即使 OBS 浏览器源在非活动场景被节流，音乐也不会中断。
//!
//! ## 通信方式
//! mpv 以 `--input-ipc-server=<名字>` 启动后，会创建一个本地套接字：
//!  - Windows：命名管道 `\\.\pipe\mpvpipe`
//!  - macOS / Linux：Unix domain socket（如 `/tmp/mpvpipe`）
//!
//! 我们使用 `interprocess` 提供的跨平台抽象拿到同一个套接字，再用换行分隔的
//! JSON 报文通信（每行一个 JSON 对象）。
//!
//! ## 报文格式
//! ```text
//! 请求: {"command": ["loadfile", "http://..."], "request_id": 1}
//! 属性: {"command": ["observe_property", 2, "time-pos"]}
//! 事件: {"event": "end-file", "reason": "eof"}
//! 属性变化: {"event": "property-change", "name": "time-pos", "data": 12.5}
//! ```
//!
//! !!! note "版本兼容性"
//!     本文件对 `interprocess` 的调用集中在 [`connect_stream`] 一个函数里。
//!     阶段 6 会补上单元测试；若该 crate 的 API 有变化，只需改这一处。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};
use tracing::{debug, info, warn};

use super::{PlaybackStatus, PlayerBackend, PlayerError, PlayerEvent};

/// Windows 命名管道的默认名字。
pub const DEFAULT_PIPE_NAME: &str = r"\\.\pipe\mpvpipe";

/// mpv 启动配置。
#[derive(Debug, Clone)]
pub struct MpvConfig {
    /// mpv 可执行文件路径（阶段 6 由 Tauri sidecar 解析）。
    pub binary: String,
    /// IPC 套接字/管道名。
    pub pipe_name: String,
    /// 初始音量（0-100）。
    pub volume: u8,
    /// 额外的 mpv 命令行参数。
    pub extra_args: Vec<String>,
}

impl Default for MpvConfig {
    fn default() -> Self {
        Self {
            binary: "mpv".to_string(),
            pipe_name: DEFAULT_PIPE_NAME.to_string(),
            volume: 80,
            extra_args: Vec::new(),
        }
    }
}

impl MpvConfig {
    /// 生成 mpv 命令行参数。
    ///
    /// 与需求文档一致的启动参数：
    /// `--idle=yes --no-video --input-ipc-server=\\.\pipe\mpvpipe --volume=80`
    pub fn args(&self) -> Vec<String> {
        let mut args = vec![
            "--idle=yes".to_string(),
            "--no-video".to_string(),
            "--keep-open=no".to_string(),
            format!("--input-ipc-server={}", self.pipe_name),
            format!("--volume={}", self.volume.min(100)),
            // 纯音频播放：不显示音频波形/封面窗口
            "--audio-display=no".to_string(),
            // 网络音频：加大缓存与超时容错。
            //
            // 实测背景：音乐平台的直链是短时效 CDN 地址，链路抖动时
            // mpv 默认行为会直接失败；`reconnect` 让它在读流中断时自动重连。
            "--cache=yes".to_string(),
            "--cache-secs=30".to_string(),
            "--network-timeout=15".to_string(),
            "--stream-lavf-o=reconnect=1,reconnect_streamed=1,reconnect_delay_max=5".to_string(),
            // 不打印状态行（我们通过 IPC 读状态）
            "--term-status-msg=".to_string(),
        ];
        args.extend(self.extra_args.iter().cloned());
        args
    }
}

/// mpv 回调事件（由事件读取任务解析后通过 [`MpvController::subscribe`] 广播）。
#[derive(Debug, Clone, PartialEq)]
pub enum MpvEvent {
    /// 新文件开始播放。
    FileLoaded,
    /// 暂停状态变化。
    PauseChanged(bool),
    /// 播放位置变化（秒）。
    Position(f64),
    /// 总时长变化（秒）。
    Duration(f64),
    /// 文件播放结束；`reason` 为 mpv 给出的原因（`eof` / `stop` / `error`）。
    EndFile { reason: String },
    /// 原始事件（未识别的事件名与字段，保留给排障）。
    Raw(Value),
}

/// 事件广播通道容量。
const EVENT_CHANNEL: usize = 64;
/// 播放状态内部编码：0=Idle 1=Playing 2=Paused。
const STATUS_IDLE: u8 = 0;
const STATUS_PLAYING: u8 = 1;
const STATUS_PAUSED: u8 = 2;

fn status_from_u8(v: u8) -> PlaybackStatus {
    match v {
        STATUS_PLAYING => PlaybackStatus::Playing,
        STATUS_PAUSED => PlaybackStatus::Paused,
        _ => PlaybackStatus::Idle,
    }
}

fn status_to_u8(v: PlaybackStatus) -> u8 {
    match v {
        PlaybackStatus::Idle => STATUS_IDLE,
        PlaybackStatus::Playing => STATUS_PLAYING,
        PlaybackStatus::Paused => STATUS_PAUSED,
    }
}

/// mpv 控制器：持有子进程与 IPC 命令通道。
pub struct MpvController {
    child: Mutex<Option<Child>>,
    tx: mpsc::UnboundedSender<Value>,
    events: broadcast::Sender<MpvEvent>,
    /// 上层语义事件（播放控制器据此推进队列）。
    player_events: broadcast::Sender<PlayerEvent>,
    status: Arc<AtomicU8>,
    /// 当前加载的地址（用于 `PlayerEvent::Started`）。
    current_url: Arc<std::sync::Mutex<Option<String>>>,
    /// 主动查询 `time-pos` 的应答表（request_id → 结果）。
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Option<f64>>>>>,
    /// 下一个 request_id。
    next_request_id: Arc<AtomicU64>,
    config: MpvConfig,
}

impl MpvController {
    /// 启动 mpv 子进程并连接 IPC。
    ///
    /// 步骤：spawn → 轮询等待管道出现（最多 40 × 100ms）→
    /// 启动事件读取任务 → observe 需要的属性。
    pub async fn spawn(config: MpvConfig) -> Result<Arc<Self>, PlayerError> {
        // 每次启动前清理可能残留的旧管道（上一次进程被强杀时会留下）。
        remove_stale_pipe(&config.pipe_name);
        // 更关键的一步：同名管道上若还挂着旧的 mpv，新进程无法创建管道，
        // 我们会连到旧进程上导致「mpv 在跑但不响应」。
        evict_previous_instance(&config.pipe_name).await;

        let child = Command::new(&config.binary)
            .args(config.args())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| PlayerError::BinaryNotFound(format!("{}（{e}）", config.binary)))?;

        // 后续还要用到 config（构造 Self、同步音量），因此提前取出需要的值。
        let initial_volume = config.volume.min(100);

        let stream = connect_stream(&config.pipe_name).await.map_err(|e| {
            PlayerError::IpcConnect {
                pipe: config.pipe_name.clone(),
                source: e,
            }
        })?;

        // `interprocess` 的 Stream 同时实现了 AsyncRead + AsyncWrite，
        // 但 split 需要的是 futures 的 `AsyncRead`/`AsyncWrite` 语义，
        // 因此用 tokio 官方的 split 适配器（返回 (ReadHalf, WriteHalf)）。
        let (read_half, write_half) = tokio::io::split(stream);
        let (tx, rx) = mpsc::unbounded_channel::<Value>();
        let (events, _rx) = broadcast::channel(EVENT_CHANNEL);
        let (player_events, _prx) = broadcast::channel(EVENT_CHANNEL);
        let status = Arc::new(AtomicU8::new(STATUS_IDLE));
        let current_url: Arc<std::sync::Mutex<Option<String>>> =
            Arc::new(std::sync::Mutex::new(None));
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Option<f64>>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // 事件读取任务
        let read_status = Arc::clone(&status);
        let read_events = events.clone();
        let read_player_events = player_events.clone();
        let read_url = Arc::clone(&current_url);
        let read_pending = Arc::clone(&pending);
        tokio::spawn(async move {
            read_events_loop(
                read_half,
                read_status,
                read_events,
                read_player_events,
                read_url,
                read_pending,
            )
            .await;
            warn!("mpv 事件读取任务结束（IPC 已断开）");
        });

        // 命令写入任务
        tokio::spawn(async move {
            write_commands_loop(write_half, rx).await;
        });

        let controller = Arc::new(Self {
            child: Mutex::new(Some(child)),
            tx,
            events,
            player_events,
            status,
            current_url,
            pending,
            next_request_id: Arc::new(AtomicU64::new(10_000)),
            config,
        });

        // 首次同步音量与需要观察的属性。
        controller.send(json!({ "command": ["set_property", "volume", initial_volume] }));
        for (id, prop) in [(1u32, "pause"), (2, "time-pos"), (3, "duration"), (4, "eof-reached")] {
            controller.send(json!({ "command": ["observe_property", id, prop] }));
        }

        info!(pipe = %controller.config.pipe_name, "mpv 已启动并连接 IPC");
        Ok(controller)
    }

    /// 发送一条原始 IPC 命令（不等待响应）。
    pub fn send(&self, value: Value) {
        if self.tx.send(value).is_err() {
            warn!("mpv 命令通道已关闭");
        }
    }

    /// 发送命令并附带 request_id，便于日志追踪。
    pub fn send_command(&self, command: Value, request_id: u64) {
        self.send(json!({ "command": command, "request_id": request_id }));
    }

    /// 订阅 mpv 原始事件。
    pub fn subscribe(&self) -> broadcast::Receiver<MpvEvent> {
        self.events.subscribe()
    }

    /// 订阅上层语义事件（播放控制器使用）。
    pub fn subscribe_player_events(&self) -> broadcast::Receiver<PlayerEvent> {
        self.player_events.subscribe()
    }

    /// 主动查询某个数值型属性。
    ///
    /// 通过 `request_id` 关联应答，最多等待 1 秒。
    pub async fn get_property_f64(&self, name: &str) -> Option<f64> {
        let id = self
            .next_request_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        self.send(json!({ "command": ["get_property", name], "request_id": id }));

        match tokio::time::timeout(Duration::from_secs(1), rx).await {
            Ok(Ok(value)) => value,
            Ok(Err(_)) => None,
            Err(_) => {
                // 超时后清理应答表，避免泄漏
                self.pending.lock().await.remove(&id);
                None
            }
        }
    }

    /// 记录当前加载的地址。
    fn set_current_url(&self, url: Option<String>) {
        *self.current_url.lock().unwrap_or_else(|e| e.into_inner()) = url;
    }

    /// mpv 启动参数（用于日志与排障）。
    pub fn config(&self) -> &MpvConfig {
        &self.config
    }

    /// 加载文件。
    ///
    /// ⚠️ 这里**刻意不带** `loadfile` 的 `start=<秒>` 参数：实测加上之后
    /// 这条路会「冻结」——位置从此不再推进（普通 load 每秒都会前进）。
    /// 需要从中间播放时，由调用方在文件就绪后显式 `seek`。
    async fn load_inner(&self, url: &str) -> Result<(), PlayerError> {
        self.set_current_url(Some(url.to_string()));
        self.send(json!({ "command": ["loadfile", url, "replace"] }));
        self.status.store(STATUS_PLAYING, Ordering::Relaxed);
        // 某些网络流不会触发 start-file，这里主动广播一次开始事件，
        // 让播放控制器立刻更新「正在播放」而不必等 mpv 回调。
        let _ = self.player_events.send(PlayerEvent::Started {
            url: url.to_string(),
        });
        Ok(())
    }
}

#[async_trait]
impl PlayerBackend for MpvController {
    async fn load(&self, url: &str) -> Result<(), PlayerError> {
        self.load_inner(url).await
    }

    async fn set_paused(&self, paused: bool) -> Result<(), PlayerError> {
        self.send(json!({ "command": ["set_property", "pause", paused] }));
        self.status.store(
            if paused { STATUS_PAUSED } else { STATUS_PLAYING },
            Ordering::Relaxed,
        );
        Ok(())
    }

    async fn stop(&self) -> Result<(), PlayerError> {
        self.send(json!({ "command": ["stop"] }));
        self.status.store(STATUS_IDLE, Ordering::Relaxed);
        Ok(())
    }

    async fn set_volume(&self, volume: u8) -> Result<(), PlayerError> {
        self.send(json!({ "command": ["set_property", "volume", volume.min(100)] }));
        Ok(())
    }

    async fn seek(&self, position: f64) -> Result<(), PlayerError> {
        self.send(json!({ "command": ["seek", position, "absolute"] }));
        Ok(())
    }

    fn status(&self) -> PlaybackStatus {
        status_from_u8(self.status.load(Ordering::Relaxed))
    }

    async fn current_position(&self) -> Option<f64> {
        self.get_property_f64("time-pos").await
    }

    async fn shutdown(&self) -> Result<(), PlayerError> {
        self.send(json!({ "command": ["quit"] }));
        let mut guard = self.child.lock().await;
        if let Some(mut child) = guard.take() {
            let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
        }
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// IPC 底层工具
// ─────────────────────────────────────────────────────────────────────────────

/// 用于收发 mpv JSON 报文的双向流。
///
/// `interprocess` 在 Windows 上是命名管道、在 Unix 上是 Unix domain socket，
/// 但对外暴露同一个类型，因此这里不需要按平台分支。
type MpvStream = interprocess::local_socket::tokio::Stream;

/// 连接 mpv 的 IPC 套接字（跨平台）。
///
/// Windows 下 mpv 使用命名管道 `\\.\pipe\mpvpipe`，Unix 下是 socket 文件路径。
/// `interprocess` 用同一套 `Name` 表达两者，这里按名字形态选择命名空间：
///  - `\\.\pipe\...`（以及 Unix 绝对路径）→ 文件系统名字
///  - 其他短名字 → 平台命名空间（Windows 的 `GenericNamespaced`）
async fn connect_stream(pipe_name: &str) -> std::io::Result<MpvStream> {
    use interprocess::local_socket::traits::tokio::Stream as _;
    use interprocess::local_socket::{GenericFilePath, ToFsName};
    #[cfg(windows)]
    use interprocess::local_socket::{GenericNamespaced, ToNsName};
    use std::path::Path;

    /// 最多轮询 4 秒等 mpv 把管道建好。
    const MAX_ATTEMPTS: u32 = 40;

    let mut last_err: Option<std::io::Error> = None;
    for attempt in 0..MAX_ATTEMPTS {
        #[cfg(windows)]
        let name = if pipe_name.starts_with(r"\\.\pipe\") {
            Path::new(pipe_name).to_fs_name::<GenericFilePath>()?
        } else {
            pipe_name.to_ns_name::<GenericNamespaced>()?
        };
        #[cfg(not(windows))]
        let name = Path::new(pipe_name).to_fs_name::<GenericFilePath>()?;

        match MpvStream::connect(name).await {
            Ok(stream) => {
                debug!(attempt, "已连接 mpv IPC");
                return Ok(stream);
            }
            Err(err) => {
                last_err = Some(err);
                // mpv 从进程启动到创建管道需要几十毫秒，采用 100ms 轮询。
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::other("连接 mpv IPC 超时")))
}

/// 清理上一次异常退出遗留的管道/socket 文件。
fn remove_stale_pipe(pipe_name: &str) {
    #[cfg(unix)]
    {
        if pipe_name.starts_with('/') && std::path::Path::new(pipe_name).exists() {
            let _ = std::fs::remove_file(pipe_name);
        }
    }
    #[cfg(windows)]
    {
        // Windows 命名管道没有「文件」可删，见 `evict_previous_instance`。
        let _ = pipe_name;
    }
}

/// 把占着同名 IPC 管道的旧 mpv 实例赶走。
///
/// ## 为什么必须做
/// 我们的进程被**强制终止**时（任务管理器 / `Stop-Process -Force`），`Drop` 不执行，
/// mpv 子进程会留下来继续占着 `\\.\pipe\mpvpipe`。此时新启动的 mpv **无法**创建同名
/// 管道，而我们的客户端会连到**上一个进程的残留**上——表现为「mpv 在跑但完全不响应」，
/// 日志里全是 `mpv 命令通道已关闭`。（这是实测踩到的坑。）
///
/// 做法：先尝试连一下管道；连得上说明有旧实例，发 `quit` 让它自己退出（最多等 3 秒）。
/// 连不上或清理失败都不影响后续启动。
async fn evict_previous_instance(pipe_name: &str) {
    let Ok(stream) = connect_stream_once(pipe_name).await else {
        return; // 没有旧实例（最常见的情况）
    };

    warn!(pipe = %pipe_name, "检测到残留的 mpv 实例，正在请求它退出");
    let (read_half, mut write_half) = tokio::io::split(stream);
    let quit = "{\"command\":[\"quit\"]}\n";
    if write_half.write_all(quit.as_bytes()).await.is_err() {
        // 写不进去说明其实已经死了，直接返回
        return;
    }
    let _ = write_half.flush().await;

    // 等对端关闭（读到 EOF）或超时
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => continue,
            }
        }
    })
    .await;

    // 让出一点时间给管道真正释放
    tokio::time::sleep(Duration::from_millis(200)).await;
    debug!(pipe = %pipe_name, "残留实例清理流程结束");
}

/// 尝试连接一次（不重试），用于探测旧实例。
async fn connect_stream_once(pipe_name: &str) -> std::io::Result<MpvStream> {
    use interprocess::local_socket::traits::tokio::Stream as _;
    use interprocess::local_socket::{GenericFilePath, ToFsName};
    #[cfg(windows)]
    use interprocess::local_socket::{GenericNamespaced, ToNsName};
    use std::path::Path;

    #[cfg(windows)]
    let name = if pipe_name.starts_with(r"\\.\pipe\") {
        Path::new(pipe_name).to_fs_name::<GenericFilePath>()?
    } else {
        pipe_name.to_ns_name::<GenericNamespaced>()?
    };
    #[cfg(not(windows))]
    let name = Path::new(pipe_name).to_fs_name::<GenericFilePath>()?;

    MpvStream::connect(name).await
}

/// 命令写入循环：把 mpsc 收到的 JSON 逐行写入 IPC。
async fn write_commands_loop<W>(mut writer: W, mut rx: mpsc::UnboundedReceiver<Value>)
where
    W: tokio::io::AsyncWrite + Unpin,
{
    while let Some(value) = rx.recv().await {
        let mut line = value.to_string();
        line.push('\n');
        if let Err(err) = writer.write_all(line.as_bytes()).await {
            // 管道断开（例如被强杀后残留的旧实例）：必须留下日志，
            // 否则上层只会看到「命令通道已关闭」，无法判断根因。
            warn!(error = %err, command = %value, "写入 mpv IPC 失败，命令通道即将关闭");
            return;
        }
        if let Err(err) = writer.flush().await {
            warn!(error = %err, "flush mpv IPC 失败，命令通道即将关闭");
            return;
        }
    }
    debug!("mpv 命令写入任务结束（发送端已全部释放）");
}

/// 事件读取循环：逐行解析 mpv 的 JSON 报文并广播。
async fn read_events_loop<R>(
    reader: R,
    status: Arc<AtomicU8>,
    events: broadcast::Sender<MpvEvent>,
    player_events: broadcast::Sender<PlayerEvent>,
    current_url: Arc<std::sync::Mutex<Option<String>>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Option<f64>>>>>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    debug!(%line, "mpv 返回了非 JSON 行");
                    continue;
                };

                // 先看是不是主动查询的应答（get_property 的 request_id 回执）
                resolve_pending(&value, &pending).await;

                // 再走事件/属性变化
                if let Some(ev) = translate_event(&value, &status) {
                    // 同步翻译成上层语义事件
                    if let Some(player_event) = to_player_event(&ev, &current_url) {
                        let _ = player_events.send(player_event);
                    }
                    let _ = events.send(ev);
                }
            }
            Ok(None) => break,
            Err(err) => {
                warn!(error = %err, "读取 mpv IPC 失败");
                break;
            }
        }
    }

    // 连接断开：把所有等待中的查询一并释放，避免调用方等到超时。
    let mut guard = pending.lock().await;
    for (_, sender) in guard.drain() {
        let _ = sender.send(None);
    }
}

/// 处理 `get_property` 的应答：把结果回填给等待中的调用方。
async fn resolve_pending(
    value: &Value,
    pending: &Arc<Mutex<HashMap<u64, oneshot::Sender<Option<f64>>>>>,
) {
    let Some(request_id) = value.get("request_id").and_then(|v| v.as_u64()) else {
        return;
    };
    let sender = pending.lock().await.remove(&request_id);
    if let Some(sender) = sender {
        // `error` 为 success 时 data 才是有效值；属性不可用（例如未加载文件）时 data 为 null。
        let result = value
            .get("data")
            .and_then(|d| d.as_f64())
            .filter(|v| v.is_finite());
        let _ = sender.send(result);
    }
}

/// 把 mpv 原始事件翻译成上层语义事件；不需要上报的返回 `None`。
fn to_player_event(
    event: &MpvEvent,
    current_url: &Arc<std::sync::Mutex<Option<String>>>,
) -> Option<PlayerEvent> {
    match event {
        MpvEvent::FileLoaded => {
            let url = current_url
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .unwrap_or_default();
            Some(PlayerEvent::Started { url })
        }
        MpvEvent::PauseChanged(paused) => Some(PlayerEvent::Paused(*paused)),
        MpvEvent::Position(position) => Some(PlayerEvent::Position(*position)),
        MpvEvent::Duration(duration) => Some(PlayerEvent::Duration(*duration)),
        MpvEvent::EndFile { reason } => Some(PlayerEvent::Ended {
            reason: reason.clone(),
        }),
        // 原始事件只用于排障，不上报给控制器
        MpvEvent::Raw(_) => None,
    }
}

/// 把 mpv 的原始报文翻译为 [`MpvEvent`]，并顺带维护播放状态。
fn translate_event(value: &Value, status: &Arc<AtomicU8>) -> Option<MpvEvent> {
    let event = value.get("event").and_then(|e| e.as_str())?;
    match event {
        "start-file" | "file-loaded" => {
            status.store(STATUS_PLAYING, Ordering::Relaxed);
            Some(MpvEvent::FileLoaded)
        }
        "playback-restart" => {
            if status.load(Ordering::Relaxed) != STATUS_PAUSED {
                status.store(STATUS_PLAYING, Ordering::Relaxed);
            }
            None
        }
        "end-file" => {
            status.store(STATUS_IDLE, Ordering::Relaxed);
            let reason = value
                .get("reason")
                .and_then(|r| r.as_str())
                .unwrap_or("unknown")
                .to_string();
            Some(MpvEvent::EndFile { reason })
        }
        "property-change" => {
            let name = value.get("name").and_then(|n| n.as_str()).unwrap_or_default();
            let data = value.get("data");
            match name {
                "pause" => {
                    let paused = data.and_then(|d| d.as_bool()).unwrap_or(false);
                    status.store(
                        if paused { STATUS_PAUSED } else { STATUS_PLAYING },
                        Ordering::Relaxed,
                    );
                    Some(MpvEvent::PauseChanged(paused))
                }
                "time-pos" => data
                    .and_then(|d| d.as_f64())
                    .map(MpvEvent::Position)
                    .or(Some(MpvEvent::Raw(value.clone()))),
                "duration" => data
                    .and_then(|d| d.as_f64())
                    .map(MpvEvent::Duration)
                    .or(Some(MpvEvent::Raw(value.clone()))),
                _ => None,
            }
        }
        _ => Some(MpvEvent::Raw(value.clone())),
    }
}

/// 供事件翻译使用的状态编码（暴露给测试）。
pub fn encode_status(status: PlaybackStatus) -> u8 {
    status_to_u8(status)
}

/// 从编码还原状态（暴露给测试）。
pub fn decode_status(value: u8) -> PlaybackStatus {
    status_from_u8(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_args_match_requirement() {
        let cfg = MpvConfig::default();
        let args = cfg.args();
        assert!(args.contains(&"--idle=yes".to_string()));
        assert!(args.contains(&"--no-video".to_string()));
        assert!(args
            .iter()
            .any(|a| a.starts_with("--input-ipc-server=")));
        assert!(args.contains(&"--volume=80".to_string()));
    }

    #[test]
    fn translates_end_file_event() {
        let status = Arc::new(AtomicU8::new(STATUS_PLAYING));
        let raw = json!({ "event": "end-file", "reason": "eof" });
        let ev = translate_event(&raw, &status).expect("应产生事件");
        assert_eq!(ev, MpvEvent::EndFile { reason: "eof".into() });
        assert_eq!(status.load(Ordering::Relaxed), STATUS_IDLE);
    }

    #[test]
    fn translates_pause_property_change() {
        let status = Arc::new(AtomicU8::new(STATUS_PLAYING));
        let raw = json!({ "event": "property-change", "name": "pause", "data": true });
        assert_eq!(
            translate_event(&raw, &status),
            Some(MpvEvent::PauseChanged(true))
        );
        assert_eq!(status.load(Ordering::Relaxed), STATUS_PAUSED);
    }

    #[test]
    fn status_roundtrip() {
        for s in [
            PlaybackStatus::Idle,
            PlaybackStatus::Playing,
            PlaybackStatus::Paused,
        ] {
            assert_eq!(decode_status(encode_status(s)), s);
        }
    }

    #[test]
    fn translates_events_into_player_events() {
        let url: Arc<std::sync::Mutex<Option<String>>> =
            Arc::new(std::sync::Mutex::new(Some("http://example/a.mp3".into())));

        assert_eq!(
            to_player_event(&MpvEvent::FileLoaded, &url),
            Some(PlayerEvent::Started {
                url: "http://example/a.mp3".into()
            })
        );
        assert_eq!(
            to_player_event(&MpvEvent::PauseChanged(true), &url),
            Some(PlayerEvent::Paused(true))
        );
        assert_eq!(
            to_player_event(&MpvEvent::Position(12.5), &url),
            Some(PlayerEvent::Position(12.5))
        );
        assert_eq!(
            to_player_event(&MpvEvent::Duration(269.0), &url),
            Some(PlayerEvent::Duration(269.0))
        );
        assert_eq!(
            to_player_event(
                &MpvEvent::EndFile {
                    reason: "eof".into()
                },
                &url
            ),
            Some(PlayerEvent::Ended {
                reason: "eof".into()
            })
        );
        // 原始事件不上报
        assert_eq!(to_player_event(&MpvEvent::Raw(json!({})), &url), None);
    }

    #[test]
    fn default_args_include_common_audio_flags() {
        let args = MpvConfig::default().args();
        assert!(args.contains(&"--keep-open=no".to_string()));
        assert!(args.iter().any(|a| a.starts_with("--cache")));
    }
}
