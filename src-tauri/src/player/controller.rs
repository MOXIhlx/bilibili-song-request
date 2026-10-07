//! 播放控制：把「队列」和「播放器」缝在一起。
//!
//! ```text
//! queue（顺序/随机/单曲循环） ──▶ 取下一首 ──▶ 音乐平台取播放地址 ──▶ mpv loadfile
//!        ▲                                                            │
//!        └──────────── PlayerEvent::Ended（mpv end-file）◀────────────┘
//! ```
//!
//! 关键约束：音频只由 mpv 输出到系统音频设备，OBS 只显示面板；条目还没解析出
//! 真实曲目（阶段 5 的异步解析）就先等解析完再取地址；取地址失败不能让队列卡死，
//! 跳过并记录原因后继续下一首。

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::config::PickPolicy;
use crate::event::HubEvent;
use crate::models::{IdleMode, QueueItem, QueuePriority, QueueStatus};
use crate::music::MusicService;
use crate::queue;
use crate::state::StateCell;

use super::{PlaybackStatus, PlayerBackend, PlayerEvent};
pub use super::lyrics::Lyrics;

/// 播放模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackMode {
    /// 顺序播放：播完出队，取队首。
    Sequential,
    /// 随机播放：从队列里随机取一首。
    Random,
    /// 单曲循环：播完继续播同一首。
    RepeatOne,
}

impl Default for PlaybackMode {
    fn default() -> Self {
        PlaybackMode::Sequential
    }
}

impl From<crate::models::PlayMode> for PlaybackMode {
    fn from(mode: crate::models::PlayMode) -> Self {
        match mode {
            crate::models::PlayMode::Sequential => PlaybackMode::Sequential,
            crate::models::PlayMode::Random => PlaybackMode::Random,
            crate::models::PlayMode::RepeatOne => PlaybackMode::RepeatOne,
        }
    }
}

impl From<PlaybackMode> for crate::models::PlayMode {
    fn from(mode: PlaybackMode) -> Self {
        match mode {
            PlaybackMode::Sequential => crate::models::PlayMode::Sequential,
            PlaybackMode::Random => crate::models::PlayMode::Random,
            PlaybackMode::RepeatOne => crate::models::PlayMode::RepeatOne,
        }
    }
}

/// 等待曲目解析完成的最长时间。
const RESOLVE_WAIT: Duration = Duration::from_secs(20);
/// 解析状态轮询间隔。
const RESOLVE_POLL: Duration = Duration::from_millis(200);
/// 连续加载失败多少次后放弃本轮推进（避免坏条目导致死循环）。
const MAX_LOAD_FAILURES: usize = 3;
/// `loadfile` 之后的宽限期：这段时间内不判定播放器「空闲」。
///
/// `loadfile` 是异步的，mpv 需要一点时间才让 `time-pos` 有值；没有这个宽限期，
/// 紧随其后的 `is_really_idle()` 会把刚加载的歌误判成过期状态并清掉。
const LOAD_GRACE: Duration = Duration::from_secs(2);

/// 决定下一首播放什么。
///
/// 抽成纯函数是为了可测试：随机模式用注入的索引模拟，避免测试依赖随机结果。
/// 返回 `(候选, 是否从队列中移除)`：顺序/随机从队列取出；单曲循环有当前曲目时
/// 返回它且不出队，当前为空时退回队列首项（否则「切到单曲循环时队列里有歌却什么都不播」）。
pub fn next_candidate(
    mode: PlaybackMode,
    current: Option<&QueueItem>,
    queue: &[QueueItem],
    random_index: usize,
) -> Option<(QueueItem, bool)> {
    match mode {
        PlaybackMode::RepeatOne => match current {
            Some(item) => Some((item.clone(), false)),
            None => queue.first().cloned().map(|item| (item, true)),
        },
        PlaybackMode::Sequential => queue.first().cloned().map(|item| (item, true)),
        PlaybackMode::Random => {
            if queue.is_empty() {
                return None;
            }
            let index = random_index % queue.len();
            queue.get(index).cloned().map(|item| (item, true))
        }
    }
}

/// 歌词拉取的等待上限。超时就当没有歌词——**绝不能**因此拖住开播。
const LYRICS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// 后台拉取并写入歌词（生产路径）。
///
/// 只持有 `state` / `music` / `lyrics` 三个共享部件，因此可以在
/// `tokio::spawn` 里安全运行，不需要把整个控制器包成 `Arc`。
async fn load_lyrics_into(
    state: StateCell,
    music: Arc<MusicService>,
    cache: Arc<std::sync::Mutex<Option<Lyrics>>>,
    item: QueueItem,
) {
    let text = match tokio::time::timeout(LYRICS_TIMEOUT, fetch_lyrics_text(&item.song, &music)).await
    {
        Ok(text) => text,
        Err(_) => {
            debug!(title = %item.song.title, "歌词拉取超时，播放不受影响");
            None
        }
    };
    apply_lyrics(&state, &cache, &item, text);
}

/// 从音乐平台取歌词原文；无结果或失败时返回 `None`。
async fn fetch_lyrics_text(
    song: &crate::models::Song,
    music: &Arc<MusicService>,
) -> Option<String> {
    if !song.is_resolved() {
        return None;
    }
    match music.lyrics(song.platform, &song.id).await {
        Ok(text) if !text.trim().is_empty() => Some(text),
        Ok(_) => None,
        Err(err) => {
            debug!(error = %err, "获取歌词失败（不影响播放）");
            None
        }
    }
}

/// 解析并写入歌词缓存与状态。
///
/// ⚠️ 后台拉取意味着「回来时可能已经换歌」：只有当这首仍是当前曲目时才写入，
/// 否则会用上一首的歌词覆盖新歌。
fn apply_lyrics(
    state: &StateCell,
    cache: &std::sync::Mutex<Option<Lyrics>>,
    item: &QueueItem,
    text: Option<String>,
) {
    {
        let guard = state.read();
        if guard.current.as_ref().map(|c| c.id) != Some(item.id) {
            debug!(title = %item.song.title, "歌词已过期（当前曲目已更换），丢弃");
            return;
        }
    }

    let parsed = text.as_deref().map(Lyrics::parse);
    let plain = text.filter(|t| !t.trim().is_empty());
    let instrumental = parsed.as_ref().map(|l| l.instrumental).unwrap_or(true);
    *cache.lock().unwrap_or_else(|e| e.into_inner()) = parsed;

    state.mutate(|s| {
        s.player.lyrics = plain.clone();
        s.player.lyric_index = None;
    });
    state.publish(HubEvent::Lyrics {
        item_id: item.id,
        title: item.song.title.clone(),
        lyrics: plain,
        instrumental,
    });
}

/// 播放模式的**单一来源**。
///
/// ⚠️ 历史坑：`AppState.play_mode` 是早期字段，现在播放实际由 `idle_mode` 驱动
/// （界面上的「播放模式」就是它）。如果各处读的字段不一致，就会出现
/// 「设了单曲循环但跳过行为不对」这类诡异现象——所以统一从这里取。
fn playback_mode_of(state: &crate::models::AppState) -> PlaybackMode {
    match state.idle_mode {
        crate::models::IdleMode::LoopOne => PlaybackMode::RepeatOne,
        crate::models::IdleMode::Shuffle => PlaybackMode::Random,
        // 顺序播放与列表循环在「下一首取哪首」上等价，差别只在播完是否回到开头
        crate::models::IdleMode::Sequential | crate::models::IdleMode::LoopAll => {
            PlaybackMode::Sequential
        }
    }
}

/// 播放控制器：持有播放器后端与音乐服务，驱动队列推进。
pub struct PlayerController {
    /// 全局状态（对前端可见的 `AppState`）。
    state: StateCell,
    /// 播放器后端（mpv 或 Mock）。
    backend: Arc<dyn PlayerBackend>,
    /// 音乐服务（取播放地址）。
    music: Arc<MusicService>,
    /// 内部播放状态。
    playback: std::sync::Mutex<PlaybackInfo>,
    /// 随机数种子（用时间推进，避免引入 rand 依赖）。
    random_counter: std::sync::atomic::AtomicU64,
    /// 播放地址覆盖（仅测试）：取地址要访问音乐平台（慢且会被风控），注入假地址后
    /// 状态机可完全离线测试。
    #[cfg(test)]
    url_override: std::sync::Mutex<Option<String>>,
    /// 歌词覆盖（仅测试，理由同上）。
    #[cfg(test)]
    lyrics_override: std::sync::Mutex<Option<String>>,
    /// 当前歌曲的解析后歌词（阶段 7）。
    lyrics: Arc<std::sync::Mutex<Option<Lyrics>>>,
    /// 点歌优先级策略提供者（每次取址时读取，便于设置改动即时生效）。
    pick_policy: Box<dyn Fn() -> PickPolicy + Send + Sync>,
    /// 点歌规则提供者（用于「播完主播点歌补充弹幕名额」时读取 `host_extra_per_play`）。
    rules: Box<dyn Fn() -> crate::config::RequestRules + Send + Sync>,
    /// 是否要忽略**下一个** `end-file` 事件。
    ///
    /// seek / 重头播放会让 mpv 发一次 `end-file`（reason=stop）。不丢弃的话
    /// `handle_ended` 会把它当成「播完了」而自动切歌——实测「重头播放」或
    /// 「上一首」之后，刚播起来的那首立刻被下一首顶掉，甚至 `current` 变空。
    expect_end_file: std::sync::atomic::AtomicBool,
    /// 是否已经真正发起过播放。
    ///
    /// ⚠️ mpv 刚连上、还没加载任何文件时会先上报一次 `pause=false`（属性初值）。
    /// 而重启后我们从快照把状态设为「暂停」，于是这个初值会被当成
    /// 「用户解除了暂停」→ 状态变成在播，但**根本没有声音、位置也不动**。
    /// 用这个开关把"开播之前的事件"全部挡掉。
    playback_started: std::sync::atomic::AtomicBool,
    /// 最近一次发起 `loadfile` 的时间（用于 [`Self::is_really_idle`] 的宽限期）。
    last_load_at: std::sync::Mutex<Option<std::time::Instant>>,
    /// 串行化「切换当前播放」（见 [`Self::try_begin_switch`]）。
    switch_lock: tokio::sync::Mutex<()>,
}

/// 内部播放信息。
#[derive(Debug, Clone, Default)]
struct PlaybackInfo {
    /// 当前加载的地址。
    url: Option<String>,
    /// 播放位置（秒）。
    position: f64,
    /// 时长（秒）。
    duration: f64,
}

impl PlayerController {
    /// 创建控制器（不自动接管事件循环，调用方需自行调用
    /// [`PlayerController::run_event_loop`]）。
    pub fn new(
        state: StateCell,
        backend: Arc<dyn PlayerBackend>,
        music: Arc<MusicService>,
    ) -> Arc<Self> {
        Self::with_policy(state, backend, music, PickPolicy::default)
    }

    /// 带「优先级策略提供者」创建控制器：用回调而不是直接存 `PickPolicy`，
    /// 设置面板改完立即生效，不需要重启或重建控制器。
    pub fn with_policy<F>(
        state: StateCell,
        backend: Arc<dyn PlayerBackend>,
        music: Arc<MusicService>,
        pick_policy: F,
    ) -> Arc<Self>
    where
        F: Fn() -> PickPolicy + Send + Sync + 'static,
    {
        Self::with_config(state, backend, music, pick_policy, crate::config::RequestRules::default)
    }

    /// 完整构造：同时提供优先级策略与点歌规则来源。
    pub fn with_config<F, R>(
        state: StateCell,
        backend: Arc<dyn PlayerBackend>,
        music: Arc<MusicService>,
        pick_policy: F,
        rules: R,
    ) -> Arc<Self>
    where
        F: Fn() -> PickPolicy + Send + Sync + 'static,
        R: Fn() -> crate::config::RequestRules + Send + Sync + 'static,
    {
        Arc::new(Self {
            state,
            backend,
            music,
            playback: std::sync::Mutex::new(PlaybackInfo::default()),
            random_counter: std::sync::atomic::AtomicU64::new(0),
            #[cfg(test)]
            url_override: std::sync::Mutex::new(None),
            #[cfg(test)]
            lyrics_override: std::sync::Mutex::new(None),
            lyrics: Arc::new(std::sync::Mutex::new(None)),
            pick_policy: Box::new(pick_policy),
            rules: Box::new(rules),
            expect_end_file: std::sync::atomic::AtomicBool::new(false),
            playback_started: std::sync::atomic::AtomicBool::new(false),
            last_load_at: std::sync::Mutex::new(None),
            switch_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// 创建控制器并自动启动事件循环与进度轮询（生产路径用这个）。
    pub fn with_event_loop(
        state: StateCell,
        backend: Arc<dyn PlayerBackend>,
        music: Arc<MusicService>,
        events: tokio::sync::broadcast::Receiver<PlayerEvent>,
    ) -> Arc<Self> {
        Self::with_event_loop_and_policy(state, backend, music, events, PickPolicy::default)
    }

    /// 同 [`PlayerController::with_event_loop`]，但可指定点歌优先级策略来源。
    pub fn with_event_loop_and_policy<F>(
        state: StateCell,
        backend: Arc<dyn PlayerBackend>,
        music: Arc<MusicService>,
        events: tokio::sync::broadcast::Receiver<PlayerEvent>,
        pick_policy: F,
    ) -> Arc<Self>
    where
        F: Fn() -> PickPolicy + Send + Sync + 'static,
    {
        Self::with_event_loop_and_config(
            state,
            backend,
            music,
            events,
            pick_policy,
            crate::config::RequestRules::default,
        )
    }

    /// 同 [`PlayerController::with_event_loop_and_policy`]，但额外提供点歌规则来源。
    pub fn with_event_loop_and_config<F, R>(
        state: StateCell,
        backend: Arc<dyn PlayerBackend>,
        music: Arc<MusicService>,
        events: tokio::sync::broadcast::Receiver<PlayerEvent>,
        pick_policy: F,
        rules: R,
    ) -> Arc<Self>
    where
        F: Fn() -> PickPolicy + Send + Sync + 'static,
        R: Fn() -> crate::config::RequestRules + Send + Sync + 'static,
    {
        let controller = Self::with_config(state, backend, music, pick_policy, rules);
        // 进度轮询同样自动接管，否则网络流前几秒进度条不动（见 run_position_poller）
        let poller = Arc::clone(&controller);
        tokio::spawn(async move {
            poller.run_position_poller(Duration::from_secs(2)).await;
        });
        let runner = Arc::clone(&controller);
        tokio::spawn(async move {
            runner.run_event_loop(events).await;
        });
        controller
    }

    /// 覆盖播放地址（仅测试：避免用例真的访问音乐平台）。
    #[cfg(test)]
    fn set_url_override(&self, url: impl Into<String>) {
        *self.url_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(url.into());
    }

    /// 覆盖歌词（仅测试）。
    #[cfg(test)]
    fn set_lyrics_override(&self, lrc: impl Into<String>) {
        *self.lyrics_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(lrc.into());
    }

    /// 当前解析后的歌词（测试与 API 用）。
    pub fn current_lyrics(&self) -> Option<Lyrics> {
        self.lyrics.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 拉取并解析当前歌曲的歌词（阶段 7）。失败只是没有歌词，只记 debug 日志。
    ///
    /// 生产路径走 [`Self::spawn_lyrics_load`]（后台拉取，不阻塞开播）。
    #[cfg(test)]
    #[allow(dead_code)]
    async fn load_lyrics_for(&self, item: &QueueItem) {
        let offline = self
            .url_override
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some();
        let injected = self
            .lyrics_override
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let text = if offline {
            injected
        } else {
            fetch_lyrics_text(&item.song, &self.music)
                .await
                .or(injected)
        };
        apply_lyrics(&self.state, &self.lyrics, item, text);
    }

    /// 从音乐平台取歌词原文；无结果或失败时返回 `None`（仅测试用）。
    #[cfg(test)]
    #[allow(dead_code)]
    async fn fetch_lyrics(&self, item: &QueueItem) -> Option<String> {
        fetch_lyrics_text(&item.song, &self.music).await
    }

    /// 播放器后端（供 API 直接调用暂停/音量等低层操作）。
    pub fn backend(&self) -> &Arc<dyn PlayerBackend> {
        &self.backend
    }

    /// 启动播放：若已有当前歌曲则继续，否则取下一首（用户点播放、队列变化后自动接续）。
    pub async fn start_next(&self) -> Result<Option<QueueItem>, String> {
        // 串行化：与 skip/previous 共用一把锁，避免两处推进同时改队列
        let Some(_guard) = self.try_begin_switch() else {
            let current = self.state.read().current.clone();
            return Ok(current);
        };
        self.start_next_inner(false).await
    }

    /// 「切换当前播放」的串行化锁。
    ///
    /// 切换流程里有长达 20 秒的等待（占位歌曲要等解析器搜索完成），期间会
    /// 让出执行器。不串行化就可能同时有多个推进在跑（`SongResolved` 自动播放、
    /// 用户点「下一首」、启动时的空闲歌单开台），实测后果是 A 先从队列取走
    /// 《夜曲》并等待解析、B 又取一次，《夜曲》既在播放又留在队列里（各一条），
    /// 之后点「下一首」取到这条同名 pending 条目，看起来就是按了没反应。
    ///
    /// 用 `try_lock` 而不是 `lock`：拿不到说明已有推进在跑，直接返回，
    /// 避免两个切换互相排队后重复切歌。
    fn try_begin_switch(&self) -> Option<tokio::sync::MutexGuard<'_, ()>> {
        match self.switch_lock.try_lock() {
            Ok(guard) => Some(guard),
            Err(_) => {
                debug!("已有切歌在进行中，忽略重复的推进请求");
                None
            }
        }
    }

    /// 播放器**实际**是否空闲（可以立刻开播下一首）。
    ///
    /// 判定以 mpv 的**真实** `time-pos` 为准：没加载文件时 mpv 返回
    /// `property unavailable` → `None`，顺带把「应用认为在播、mpv 其实没有文件」
    /// 的卡死状态清掉，让后续点歌能接上。
    ///
    /// 不直接用 `backend.status()`：那个状态是乐观维护的，[`PlayerBackend::load`]
    /// 一发出 `loadfile` 就置为 `Playing`，而网络直链打不开时 mpv 可能既不报
    /// `file-loaded` 也不报 `end-file`，状态就永远停在 `Playing`，自动播放被
    /// 永久挡住（现象：「点歌进了队列，但播放器再也没动静」）。
    ///
    /// ⚠️ 刚发出 `loadfile` 时有宽限期（见 [`LOAD_GRACE`]）：此时 `time-pos` 取不到值，
    /// 不加区分会把刚开始加载的那首误判成过期状态并清掉 `current`，让别的歌顶上来。
    pub async fn is_really_idle(&self) -> bool {
        let since_load = self
            .last_load_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map(|at| at.elapsed());
        if let Some(elapsed) = since_load {
            if elapsed < LOAD_GRACE {
                debug!(
                    elapsed_ms = elapsed.as_millis(),
                    "刚发起加载，暂不判定为空闲"
                );
                return false;
            }
        }

        match self.backend.current_position().await {
            Some(_) => false,
            None => {
                // ⚠️ 只在**确实正在播放**时才清理残留的 `current`。
                //
                // 重启后会从快照恢复 `current`（只显示、不播放），此时 mpv 里
                // 什么都没有。若在这里无条件清掉，用户看到的就是「刚打开软件，
                // 上次那首歌就消失了，点继续播放也没用」。
                let stale = {
                    let guard = self.state.read();
                    guard.current.is_some() && guard.player.playing
                };
                if stale {
                    warn!("播放器实际处于空闲，但状态里还留着当前曲目，已清理以便继续播放");
                    self.state.mutate(|s| {
                        s.current = None;
                        s.player.playing = false;
                        s.player.position = 0.0;
                    });
                }
                true
            }
        }
    }

    /// **继续播放当前曲目**（不换歌）；没有当前曲目时退回正常推进。
    ///
    /// 供 `/api/player/play`（界面上的「播放队列」）使用。它解决重启场景：
    /// 从快照恢复了「上次播放的歌」与位置，此时点播放应当**接着放这一首**、
    /// 从上次的秒数继续，而不是把它当"下一首"跳过或直接报「队列已空」。
    pub async fn resume_or_start(&self) -> Result<Option<QueueItem>, String> {
        let Some(item) = self.state.read().current.clone() else {
            // 没有当前曲目时走正常推进。**必须用 `start_next_skipping`**：
            // `start_next()` 会走 `start_next_inner(false)`，而那条路径在
            // 「`current` 存在但 mpv 空闲」（重启恢复）时会自行处理并**不推导来源标记**，
            // 于是 `current_is_idle` 一直是 false——「有人点歌立即切」永远不触发。
            return self.start_next_skipping(false, None).await;
        };

        // ⚠️ 来源标记按**当前条目实际所属的歌单**重新推导。
        //
        // 快照恢复出来的 `current_is_idle` 可能已经过时（历史版本存错过、
        // 或那次是从点歌队列切的），沿用它会让「有人点歌要不要立即让位」
        // 和空闲书签回归全部失准。
        let source_is_idle = self.item_is_from_idle(&item);
        self.mark_current_source(&item, source_is_idle);

        // ⚠️ 不能只看 `backend.status()`：mpv 后端在**刚连接、还没加载任何文件**时
        // 也会乐观地报告 `Playing`（`status()` 是内部标志，不等于真的有文件在播）。
        // 早期据此直接早退，于是重启后点「继续播放」什么都不发生。
        // 必须问 mpv 本身（`current_position()` 有值才算真的在播）。
        if !self.is_really_idle().await {
            if self.state.read().player.paused {
                self.backend
                    .set_paused(false)
                    .await
                    .map_err(|e| format!("继续播放失败：{e}"))?;
                self.sync_paused(false);
            }
            return Ok(Some(item));
        }

        let Some(_guard) = self.try_begin_switch() else {
            let current = self.state.read().current.clone();
            return Ok(current);
        };
        let resume = self.state.read().player.position;
        info!(title = %item.song.title, position = resume, "继续播放当前曲目");

        let url = self.resolve_play_url(&item).await?;
        // ⚠️ 先用**普通 load**，再延迟 seek 到起点。
        //
        // 实测 `loadfile ... start=<秒>` 会让这条路**冻结**：位置从此不再推进
        // （正常 load 则每秒前进）。所以不能用 `load_at`；而紧跟 load 的
        // `seek` 也会丢（文件未就绪），必须等一会儿再跳。
        self.backend
            .load(&url)
            .await
            .map_err(|e| format!("加载到播放器失败：{e}"))?;

        self.state.mutate(|s| {
            s.player.playing = true;
            s.player.paused = false;
            // 位置先写成起点：mpv 的位置事件要几百毫秒才来，
            // 不这样做界面会先闪一下 00:00。
            s.player.position = resume;
            s.player.duration = item.song.duration as f64;
        });

        // 歌词在**后台**拉取，绝不能挡住开播（见 `spawn_lyrics_load`）
        self.spawn_lyrics_load(item.clone());

        // 文件就绪后再跳到上次的位置（后台执行，不阻塞开播）
        if resume > 1.0 {
            self.spawn_seek_later(resume);
        }
        Ok(Some(self.state.read().current.clone().unwrap_or(item)))
    }

    /// 延迟一小段时间后把播放位置跳到 `position`（后台执行）。
    ///
    /// 只用于「继续播放」：`loadfile` 的 `start=` 参数并非对所有流都生效，
    /// 需要文件真正就绪后再 seek 一次才稳。
    fn spawn_seek_later(&self, position: f64) {
        let state = self.state.clone();
        let backend = std::sync::Arc::clone(&self.backend);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
            // mpv 报告的位置若已经接近目标（±5 秒），说明 start= 生效了，不必再跳
            if let Some(current) = backend.current_position().await {
                if (current - position).abs() <= 5.0 {
                    return;
                }
            }
            if let Err(err) = backend.seek(position).await {
                debug!(error = %err, position, "延迟恢复播放位置失败");
            } else {
                debug!(position, "已延迟恢复到指定播放位置");
                state.mutate(|s| s.player.position = position);
            }
        });
    }

    /// 在后台任务里拉取歌词（失败只是没有歌词，不影响播放）。
    ///
    /// ## 为什么要 spawn
    /// 歌词要走音乐平台的 HTTP 接口。早期 `load_and_play` **同步 await** 它，
    /// 平台不响应时整个开播流程挂死——状态已经置成「正在播放」，
    /// 但调用方永远拿不到返回值，界面表现为「点了播放没反应」。
    /// 现在只把「取歌词」放到后台，开播立刻返回。
    ///
    /// 测试里同步执行：测试用的音乐服务不联网，且用例需要「设置歌词覆盖后
    /// 立刻能读到」这一确定性行为。
    fn spawn_lyrics_load(&self, item: QueueItem) {
        // 测试里若已注入歌词，直接同步写入：测试用的音乐服务不联网，
        // 而用例需要「设置覆盖后立刻可读」的确定性行为。
        #[cfg(test)]
        {
            let offline = self
                .url_override
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some();
            let injected = self
                .lyrics_override
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if offline {
                apply_lyrics(&self.state, &self.lyrics, &item, injected);
                return;
            }
        }

        let state = self.state.clone();
        let music = std::sync::Arc::clone(&self.music);
        let cache = std::sync::Arc::clone(&self.lyrics);
        tokio::spawn(async move {
            load_lyrics_into(state, music, cache, item).await;
        });
    }

    /// 启动播放的内部实现。
    ///
    /// `force_take` 用于「主动跳过」：单曲循环下点跳过应该**换歌**，所以不能接受
    /// `next_candidate` 返回的「当前这首、不移除」结果。
    ///
    /// 内部带循环：`load_and_play` 会把取不到地址的坏条目清出队列并返回错误，
    /// 这里继续尝试下一首，用 [`MAX_LOAD_FAILURES`] 兜住「整队都是坏条目」。
    async fn start_next_inner(&self, force_take: bool) -> Result<Option<QueueItem>, String> {
        self.start_next_skipping(force_take, None).await
    }

    /// 与 [`Self::start_next_inner`] 相同，但额外告诉取歌逻辑「刚被跳过的条目」。
    ///
    /// 单曲循环下 `take_from_idle` 会按「当前播放项」判重，而跳过路径已经把
    /// 当前项从 `playing` 摘掉、`current` 也清空了，判重随即失效——于是又取回
    /// 同一首，用户看到「单曲循环下点下一首还是这首歌」。
    /// `skipped` 由调用方在**清空之前**捕获并传进来。
    async fn start_next_skipping(
        &self,
        force_take: bool,
        skipped: Option<Uuid>,
    ) -> Result<Option<QueueItem>, String> {
        // 刚发起过加载，又来了一个「启动播放」的请求（典型场景：「有人点歌立即切」与
        // 「解析完成自动播放」几乎同时触发）。不要再起一首，否则会把刚加载好的那首挤掉。
        if !force_take {
            let since_load = *self
                .last_load_at
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(at) = since_load {
                if at.elapsed() < LOAD_GRACE {
                    let existing = self.state.read().current.clone();
                    if let Some(item) = existing {
                        debug!(
                            title = %item.song.title,
                            "刚刚发起过加载，本次自动播放跳过"
                        );
                        return Ok(Some(item));
                    }
                }
            }
        }

        // 已有正在播放的歌曲：直接继续，不打断（主动跳过时除外）
        if !force_take {
            let existing = self.state.read().current.clone();
            if let Some(item) = existing {
                if self.backend.status() == PlaybackStatus::Paused {
                    let _ = self.backend.set_paused(false).await;
                    self.sync_paused(false);
                    return Ok(Some(item));
                }
                if self.backend.status() == PlaybackStatus::Playing {
                    return Ok(Some(item));
                }
                // 播放器是空的，但状态里还留着「上次播放的那首」——
                // 这正是**重启后点「继续播放」**的场景：要真正加载并**接着上次的秒数**放，
                // 否则点下去什么都不会发生（旧代码在这里直接返回，表现为"按钮无效"）。
                let resume = self.state.read().player.position;
                if resume > 1.0 && item.song.is_resolved() {
                    info!(title = %item.song.title, position = resume, "从上次的位置继续播放");
                    // ⚠️ 这里同样要重新推导来源标记：恢复出来的 `current_is_idle`
                    // 可能已经过时，而它决定「有人点歌要不要立刻让位」与空闲书签回归。
                    let source_is_idle = self.item_is_from_idle(&item);
                    self.mark_current_source(&item, source_is_idle);
                    match self.load_and_play(item.clone()).await {
                        Ok(loaded) => {
                            // seek 要放在 load 之后：mpv 在文件加载完成前不接受跳转
                            if let Err(err) = self.seek(resume).await {
                                warn!(error = %err, "恢复播放位置失败，从头播放");
                            }
                            return Ok(Some(loaded));
                        }
                        Err(err) => {
                            warn!(error = %err, "继续播放失败，按新歌处理");
                        }
                    }
                }
            }
        }

        // 记住「被主动跳过的那一首」（由调用方传入，因为它在本函数之前就被摘除了）。
        let skipped_id = skipped;

        let mut failures = 0usize;
        loop {
            let mode = self.playback_mode();
            let is_idle = self.state.read().current_is_idle;

            // ── ① 单曲循环：重播当前这首 ─────────────────────────────────
            //
            // ⚠️ 判定必须在「沿序列前进」之前，否则点「跳过」时会先 step_next 走掉，
            // 单曲循环名存实亡；但 `force_take`（用户主动跳过）必须绕过它——
            // 用户点「下一首」的意图就是换歌。
            if force_take {
                // 主动跳过一律不看单曲循环，走下面的「前进 / 取新歌」
            } else if !is_idle && mode == PlaybackMode::RepeatOne {
                if let Some(item) = queue::current_of_playing(&self.state) {
                    debug!(title = %item.song.title, "单曲循环：重播当前曲目");
                    self.load_and_play(item.clone()).await?;
                    return Ok(Some(item));
                }
            }

            // ── ② 播放序列里还有「下一首」就直接播（阶段 10d）──────────────
            //
            // 上一首/下一首都走同一条 `playing` 序列：回退之后点「下一首」会把
            // cursor 移回原来的位置，而不是去队列里抓新歌（那正是「跳到第四首」
            // 的原因）；单曲循环下主动跳过也走这里，先走已生成的序列。
            if let Some(item) = queue::step_next(&self.state) {
                debug!(title = %item.song.title, "沿播放序列前进一首");
                // ⚠️ 必须重新推导来源，**不能沿用循环开头那个 `is_idle` 快照**。
                //
                // `playing` 是混合序列：既有点歌队列的歌，也有空闲歌单的歌。
                // 早期这里直接传 `is_idle`（进入本次推进之前的来源），于是
                // "从空闲歌单沿序列走到下一首空闲歌"会被标成点歌来源，
                // 后续所有判断（有人点歌要不要立即让位、空闲书签回归）全部错位。
                let source_is_idle = self.item_is_from_idle(&item);
                self.mark_current_source(&item, source_is_idle);
                self.sync_current_from_playing();
                match self.load_and_play(item).await {
                    Ok(_) => return Ok(self.state.read().current.clone()),
                    Err(err) => {
                        warn!(error = %err, "播放序列中的曲目无法播放，继续推进");
                        failures += 1;
                        if failures >= MAX_LOAD_FAILURES {
                            self.stop_playback(QueueStatus::Skipped).await;
                            return Ok(None);
                        }
                        continue;
                    }
                }
            }

            // ── ③ 取新歌：点歌队列优先，空了才用空闲歌单 ──────────────────
            match self.take_new_song(skipped_id) {
                Some((item, from_idle)) => {
                    debug!(
                        title = %item.song.title,
                        from_idle,
                        "取到新歌，追加到播放序列"
                    );
                    // ⚠️ 必须标记来源：`take_new_song` 已经算出 `from_idle`，
                    // 早期这里只推入播放序列却**没写来源标记**，于是从空闲歌单取的歌
                    // 一直带着 `current_is_idle = false`，导致
                    // 「有人点歌立即切」永远不触发、空闲书签回归也失效。
                    let source_is_idle = from_idle || self.item_is_from_idle(&item);
                    self.mark_current_source(&item, source_is_idle);
                    queue::push_playing(&self.state, item.clone());
                    self.sync_current_from_playing();
                    match self.load_and_play(item).await {
                        Ok(_) => return Ok(self.state.read().current.clone()),
                        Err(err) => {
                            warn!(error = %err, "新歌无法播放，继续尝试下一首");
                            failures += 1;
                            if failures >= MAX_LOAD_FAILURES {
                                warn!(failures, "连续多首无法播放，停止本次推进");
                                self.stop_playback(QueueStatus::Skipped).await;
                                return Ok(None);
                            }
                            continue;
                        }
                    }
                }
                None => {
                    // 点歌队列空、空闲歌单也没得播（或顺序模式已播完）
                    self.stop_playback(QueueStatus::Played).await;
                    return Ok(None);
                }
            }
        }
    }

    /// 取一首**新歌**追加到播放序列：点歌队列优先，空了才用空闲歌单。
    /// 返回 `(条目, 是否来自空闲歌单)`；都没有则返回 `None`。
    ///
    /// 需求「记录空闲歌单当前播的曲子，当点歌队列空时继续当前播的曲子并且重头播」：
    /// 当 `idle_current` 有值、而当前不在播空闲歌单时（刚被点歌插播过），优先回到
    /// `idle_current` 那一首，而不是从 `idle_next` 往下取——否则会跳过一首。
    ///
    /// `skipped` 是刚刚被「下一首」跳过的那一条，用于让单曲循环下的跳过真的换歌。
    fn take_new_song(&self, skipped: Option<Uuid>) -> Option<(QueueItem, bool)> {
        // ① 点歌队列优先。
        //
        // ⚠️ 不能把 `RepeatOne` 直接交给 `next_candidate`：它在单曲循环下会返回
        // 「当前这首」表示"不入队、重播"，而本函数的职责是**取新歌**（调用方随后
        // 会把它追加进播放序列），返回当前这首就会造成同一条目被追加两次
        // （实测 `playing` 里出现两条同名，「下一首」于是停在原地不动）。
        // 单曲循环的"重播"由 [`Self::start_next_inner`] 的 ① 分支单独处理。
        let (mode, current, queue) = {
            let guard = self.state.read();
            let mode = match playback_mode_of(&guard) {
                PlaybackMode::Random => PlaybackMode::Random,
                _ => PlaybackMode::Sequential,
            };
            (mode, guard.current.clone(), guard.queue.clone())
        };
        if let Some((item, take)) =
            next_candidate(mode, current.as_ref(), &queue, self.next_random_index())
        {
            if take {
                self.take_from_queue(item.id);
            }
            return Some((item, false));
        }

        // ② 回到空闲歌单书签（被点歌打断过才这样）。判据用 `idle_pending_return`
        // 而不是 `!current_is_idle`：后者在「空闲歌单自然播完下一首」时也为 false，
        // 会把正常推进误判成打断，于是一直重播同一首。
        let (bookmark, pending_return) = {
            let guard = self.state.read();
            (guard.idle_current, guard.idle_pending_return)
        };
        if pending_return {
            if let Some(index) = bookmark {
                let item = {
                    let guard = self.state.read();
                    guard.idle.get(index).cloned()
                };
                if let Some(item) = item {
                    info!(
                        title = %item.song.title,
                        index,
                        "点歌队列已空：回到空闲歌单上次那首并重头播"
                    );
                    // 重头播：播完后从它的下一首继续，并清掉"待回归"标记
                    self.advance_idle_next_from(index);
                    self.state.mutate(|s| s.idle_pending_return = false);
                    return Some((item, true));
                }
            }
            // 书签失效（那首被删了）：清标记，走正常取歌
            self.state.mutate(|s| s.idle_pending_return = false);
        }

        // ③ 正常从空闲歌单取下一首（传下刚被跳过的条目，避免又取回同一首）
        self.take_from_idle(skipped)
    }

    /// 离开空闲歌单前**记下书签**，以便点歌队列播完后回到这首重头播。
    ///
    /// 存的是「当前正在播的空闲曲目在 `idle` 里的下标」，比 `idle_next`
    /// （下一次取哪首）更准确：后者已经推进过，拿它当书签会跳过正在播的这首。
    fn remember_idle_bookmark(&self) -> Option<usize> {
        let index = {
            let guard = self.state.read();
            if !guard.current_is_idle {
                return None;
            }
            guard
                .current
                .as_ref()
                .and_then(|c| guard.idle.iter().position(|i| i.id == c.id))
                .or(guard.idle_current)
        };
        if let Some(index) = index {
            self.state.mutate(|s| {
                s.idle_current = Some(index);
                s.idle_pending_return = true;
            });
            info!(index, "已记下空闲歌单书签，点歌队列播完后回来重头播");
        }
        Some(index?)
    }

    /// 把 `idle_next` 设置为「从 `index` 之后继续」。
    fn advance_idle_next_from(&self, index: usize) {
        self.state.mutate(|s| {
            let len = s.idle.len();
            if len == 0 {
                s.idle_next = 0;
                return;
            }
            s.idle_next = match s.idle_mode {
                IdleMode::LoopOne => index,
                IdleMode::Shuffle => index.wrapping_add(1),
                _ => (index + 1) % len,
            };
        });
    }

    /// 把 `current` 同步为 `playing[cursor]`（前端只读 `current`）。
    ///
    /// ⚠️ 只要 id 相同就必须整体覆盖：解析器（`music::resolver`）会把搜索结果
    /// 就地写回 `playing`，但不动 `current`。早期只在「id 不同才覆盖」，于是
    /// `current` 一直停留在解析前的占位歌曲（标题是用户输入、`song.id` 为空），
    /// 界面显示的歌名与 `playing` 里的真实曲目不一致。
    /// 所以 id 相同也要比对 `song.id`、标题等字段，任一处不同就刷新。
    fn sync_current_from_playing(&self) {
        self.state.mutate(|s| {
            let Some(item) = s.playing.get(s.cursor).cloned() else {
                return;
            };
            let stale = match s.current.as_ref() {
                None => true,
                Some(existing) => {
                    existing.id != item.id
                        || existing.song.id != item.song.id
                        || existing.song.title != item.song.title
                        || existing.song.artist != item.song.artist
                        || existing.song.source != item.song.source
                }
            };
            if stale {
                let mut item = item;
                item.status = QueueStatus::Playing;
                s.current = Some(item);
            }
        });
    }

    /// 停止播放（供「清空播放列表」调用，状态由调用方清理）。
    pub async fn stop_and_clear(&self) {
        let _ = self.backend.stop().await;
        self.playback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .url
            .take();
        *self
            .lyrics
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// 公开入口：把 `current` 刷新为 `playing[cursor]` 的最新内容。
    ///
    /// 供后台任务在曲目解析完成后调用——解析器只写 `playing`，前端只读 `current`，
    /// 不刷新就会一直显示占位歌名。
    pub fn refresh_current_from_playing(&self) {
        self.sync_current_from_playing();
        self.state.broadcast_state();
    }

    /// 推进到下一首：把当前标记为已播放/已跳过，然后加载下一首。
    ///
    /// 三个要点：`keep_display` 保留上一首的显示，等新歌加载成功再切换，
    /// 避免面板闪一下「暂时没有歌曲」；内部循环跳过取不到地址的坏条目，并用
    /// [`MAX_LOAD_FAILURES`] 兜住「全是坏条目」避免死循环；以及单曲循环必须区分
    /// 「播完」与「用户点跳过」（见下）。
    pub async fn advance(&self, skipped: bool) -> Result<Option<QueueItem>, String> {
        let status = if skipped {
            QueueStatus::Skipped
        } else {
            QueueStatus::Played
        };

        // 先把要在 await 之后用到的值全部取出来。
        // ⚠️ 读锁守卫不能跨 await，否则整个 future 不是 Send（axum handler 与
        // tokio::spawn 都要求 Send）。
        let (mode, keep_display, previous) = {
            let guard = self.state.read();
            let mode = playback_mode_of(&guard);
            let keep_display = !guard.queue.is_empty() || mode == PlaybackMode::RepeatOne;
            (mode, keep_display, guard.current.clone())
        };

        // 单曲循环下点「跳过」的语义是**换歌**：把当前这首标记为已跳过并从播放
        // 序列里摘掉，否则它会成为"下一首"的候选而原地打转
        // （实测症状：单曲循环下点跳过完全没反应）。
        if skipped && mode == PlaybackMode::RepeatOne {
            // ⚠️ 必须在摘除/清空**之前**记下它的 id：`take_from_idle` 需要它来
            // 排除"刚被跳过的那首"，否则单曲循环会把它又取回来。
            let dropped = { self.state.read().current.clone() };
            let dropped_id = dropped.as_ref().map(|i| i.id);
            if let Some(item) = dropped {
                self.remove_from_playing(item.id);
            }
            self.finish_current(QueueStatus::Skipped, false).await;
            return self.start_next_skipping(true, dropped_id).await;
        }

        self.finish_current(status, keep_display).await;

        // 单曲循环：自然播完时继续播同一首（游标不动，只是重新加载）
        if mode == PlaybackMode::RepeatOne {
            if let Some(item) = previous {
                debug!(title = %item.song.title, "单曲循环：继续播这首");
                self.load_and_play(item.clone()).await?;
                return Ok(Some(item));
            }
        }

        // 主动推进必须绕过「已有歌曲在播就继续播」的短路
        self.start_next_inner(true).await
    }

    /// 从播放序列里摘掉一条（单曲循环下"跳过当前"用）。
    ///
    /// 游标相应修正：被摘掉的位置在游标之前则前移一位，保证游标仍指向同一首歌。
    fn remove_from_playing(&self, id: Uuid) {
        self.state.mutate(|s| {
            if let Some(pos) = s.playing.iter().position(|i| i.id == id) {
                s.playing.remove(pos);
                if pos < s.cursor {
                    s.cursor -= 1;
                } else if s.cursor >= s.playing.len() {
                    s.cursor = s.playing.len().saturating_sub(1);
                }
            }
        });
    }

    /// 跳过当前（用户点击「下一首」）。
    ///
    /// 与「上一首」完全对称，都在同一条 `playing` 序列上移动 `cursor`：
    /// 右边还有歌就直接前进（回到第 2 首再点下一首会回到第 3 首），已在末尾
    /// 才去队列/空闲取新歌并追加。
    pub async fn skip(&self) -> Result<Option<QueueItem>, String> {
        // 串行化：拿不到锁说明已有切歌在跑，直接忽略这次重复点击
        let Some(_guard) = self.try_begin_switch() else {
            let current = self.state.read().current.clone();
            return Ok(current);
        };
        // 主动跳过统一走 `advance(true)`：内部先看 playing 的右边，再考虑单曲循环与新歌
        self.advance(true).await
    }

    /// 回到**上一首**（沿同一条 `playing` 序列向更早移动 `cursor`）。
    ///
    /// 只移动游标、不删除条目，所以可以反复来回点；已经在最老一首时返回 `Err`，
    /// 界面据此提示而不是静默无反应。
    ///
    /// 途中**跳过播不了的曲目**（版权受限等）：否则用户会反复撞上同一首坏歌，
    /// 每次都只看到一句报错。坏条目**保留在序列里**，只让游标绕过它——
    /// 这样「下一首」还能走回来，历史也不会被削短。
    pub async fn previous(&self) -> Result<Option<QueueItem>, String> {
        // 串行化：避免与自动播放/空闲歌单推进互相抢当前曲目
        let Some(_guard) = self.try_begin_switch() else {
            let current = self.state.read().current.clone();
            return Ok(current);
        };

        // 还原点用 id 而不是下标：失败路径里游标会移动，用旧下标可能指到别的歌上。
        let restore_id = self.state.read().current.as_ref().map(|i| i.id);
        let previous_item = self.state.read().current.clone();
        let was_idle = self.state.read().current_is_idle;
        let mut last_error: Option<String> = None;

        loop {
            let Some(item) = queue::step_previous(&self.state) else {
                // 序列已被走完：要么本来就没有上一首（提示边界），
                // 要么沿途的坏条目都被跳过（同样是"没得放"，用提示而不是报错）。
                if last_error.is_some() {
                    self.restore_after_failed_switch(restore_id, &previous_item, was_idle);
                    return Err("没有更多了（更早的曲目都播不了）".to_string());
                }
                return Err("没有更多了（已经是第一首）".to_string());
            };

            info!(title = %item.song.title, "回到上一首");
            // 游标已经在正确位置，只把 current 同步过去再加载
            self.sync_current_from_playing();

            match self.load_and_play(item.clone()).await {
                Ok(_) => return Ok(Some(item)),
                Err(err) => {
                    // 坏条目**保留在序列里**（游标继续往更早走即可），
                    // 这样「下一首」还能走回来，历史也不会被削短。
                    warn!(title = %item.song.title, %err, "上一首播不了，继续往前找");
                    last_error = Some(err);
                    if !queue::has_previous_in_playing(&self.state) {
                        self.restore_after_failed_switch(restore_id, &previous_item, was_idle);
                        // 已经翻到最早，前面的都播不了 → 提示而不是报错，
                        // 当前这首歌保持不动。
                        return Err("没有更多了（更早的曲目都播不了）".to_string());
                    }
                }
            }
        }
    }

    /// 「上一首 / 下一首」失败时把游标与 `current` 还原到最后成功播放的那一首。
    ///
    /// 不还原的话：游标停在坏条目那一格、`current` 已被清空，而
    /// `playing[cursor]` 还在——破坏「current == playing[cursor]」不变量，
    /// 界面也会显示成"跳了一首却什么都没播"。
    fn restore_after_failed_switch(
        &self,
        restore_id: Option<Uuid>,
        previous_item: &Option<QueueItem>,
        was_idle: bool,
    ) {
        self.state.mutate(|s| {
            s.cursor = restore_id
                .and_then(|id| s.playing.iter().position(|i| i.id == id))
                .unwrap_or_else(|| s.cursor.min(s.playing.len().saturating_sub(1)));
            s.current = previous_item.clone();
            s.current_is_idle = was_idle;
        });
        self.state.broadcast_state();
    }

    /// **重头播放当前歌曲**（把进度拖回 0 并解除暂停）。
    ///
    /// 与「上一首」的区别：不换歌，只是重新开始。
    pub async fn replay_current(&self) -> Result<Option<QueueItem>, String> {
        let Some(item) = self.state.read().current.clone() else {
            return Err("当前没有正在播放的歌曲".to_string());
        };
        self.seek(0.0).await?;
        info!(title = %item.song.title, "重头播放当前歌曲");
        Ok(Some(item))
    }

    /// 加载并播放一条队列条目。
    async fn load_and_play(&self, item: QueueItem) -> Result<QueueItem, String> {
        let url = match self.resolve_play_url(&item).await {
            Ok(url) => url,
            Err(reason) => {
                warn!(title = %item.song.title, %reason, "无法获取播放地址，跳过该条目");
                self.state.publish(HubEvent::SongPlayFailed {
                    item_id: item.id,
                    title: item.song.title.clone(),
                    reason: reason.clone(),
                });
                // ⚠️ 这里**刻意不把条目从 `playing` 摘掉**。
                //
                // 早期会 `drop_item` 清理坏条目，代价是**历史被销毁**：
                // 一条播不了的歌会从播放序列里消失，于是 `previous()` 再也回不到它，
                // 用户看到「上一首」变成「没有更多了（更早的曲目都播不了）」，
                // 而且序列越用越短（实测 `playing` 从 2 掉到 1）。
                //
                // 正确做法是保留条目、只让游标绕过它：`previous`/`next` 本来就
                // 会跳过加载失败的曲目。坏条目不会再被播放，因为游标不会停在那里。
                self.state.publish(HubEvent::SongPlayFailed {
                    item_id: item.id,
                    title: item.song.title.clone(),
                    reason: reason.clone(),
                });
                return Err(reason);
            }
        };

        self.backend
            .load(&url)
            .await
            .map_err(|e| format!("加载到播放器失败：{e}"))?;

        // 记录加载时刻：`is_really_idle()` 在这之后的一小段时间内
        // 不会判定「空闲」，避免把刚开始加载的这首误清掉。
        *self
            .last_load_at
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());

        {
            let mut guard = self.playback.lock().unwrap_or_else(|e| e.into_inner());
            guard.url = Some(url);
            guard.position = 0.0;
            guard.duration = item.song.duration as f64;
        }

        self.state.mutate(|s| {
            s.player.playing = true;
            s.player.paused = false;
            s.player.position = 0.0;
            s.player.duration = item.song.duration as f64;
            s.player.lyrics = None;
        });
        self.playback_started
            .store(true, std::sync::atomic::Ordering::SeqCst);
        info!(title = %item.song.title, artist = %item.song.artist, "开始播放");
        self.state.publish(HubEvent::NowPlaying {
            item_id: item.id,
            title: item.song.title.clone(),
            artist: item.song.artist.clone(),
            requested_by: item.requested_by.clone(),
            platform: item.song.platform,
            duration: item.song.duration,
        });

        // 歌词在**后台**拉取：见 `spawn_lyrics_load`，绝不能挡住开播
        self.spawn_lyrics_load(item.clone());

        Ok(item)
    }

    /// 取播放地址：必要时先等曲目解析完成。
    async fn resolve_play_url(&self, item: &QueueItem) -> Result<String, String> {
        let mut current = item.clone();

        // 占位歌曲：等解析器回填真实 ID（最多 RESOLVE_WAIT）
        if !current.song.is_resolved() {
            // 解析失败（阶段 5 已给出原因）：直接跳过，不必再等
            if current.song.source == crate::models::SongSource::Failed {
                return Err(current
                    .song
                    .source_error
                    .clone()
                    .unwrap_or_else(|| "曲目解析失败".to_string()));
            }

            info!(title = %current.song.title, "歌曲尚未解析，等待搜索完成");
            let deadline = std::time::Instant::now() + RESOLVE_WAIT;
            loop {
                tokio::time::sleep(RESOLVE_POLL).await;

                // ⚠️ 条目可能已被取进 `current`（`advance()` 先 `take_from_queue` 再
                // `load_and_play`），取新歌时又会先进 `playing`，所以队列里找不到
                // 不代表条目没了。早期只查 `queue`，导致刚入队就被跳过时报「条目已从
                // 队列移除」；漏查 `playing` 则会让解析结果写不回去（标题一直是占位文本）。
                let (fresh, still_exists) = {
                    let guard = self.state.read();
                    let from_queue = guard.queue.iter().find(|i| i.id == current.id).cloned();
                    let from_playing = guard.playing.iter().find(|i| i.id == current.id).cloned();
                    let from_current = guard
                        .current
                        .as_ref()
                        .filter(|i| i.id == current.id)
                        .cloned();
                    let exists =
                        from_queue.is_some() || from_playing.is_some() || from_current.is_some();
                    // 解析结果优先看队列（解析器回写到队列/空闲歌单）；
                    // 若已经搬到 current/playing，就看那一份
                    (
                        from_queue.or(from_playing).or(from_current),
                        exists,
                    )
                };

                match fresh {
                    Some(updated) => {
                        current = updated;
                        if current.song.is_resolved() {
                            break;
                        }
                        if current.song.source == crate::models::SongSource::Failed {
                            return Err(current
                                .song
                                .source_error
                                .clone()
                                .unwrap_or_else(|| "曲目解析失败".to_string()));
                        }
                    }
                    // 既不在队列也不在当前播放项：真的被删了
                    None if !still_exists => return Err("条目已从队列移除".to_string()),
                    None => {}
                }
                if std::time::Instant::now() >= deadline {
                    return Err(format!(
                        "等待《{}》解析超时（{} 秒）",
                        current.song.title,
                        RESOLVE_WAIT.as_secs()
                    ));
                }
            }
        }

        // 测试注入假地址：跳过对音乐平台的调用（前面的解析判定仍然生效）
        #[cfg(test)]
        if let Some(url) = self
            .url_override
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Ok(url);
        }

        self.music
            .play_url_for(&current.song, (self.pick_policy)())
            .await
            .map_err(|err| crate::music::resolver::describe_music_error(&err))
    }

    /// 处理 mpv 的播放结束事件（自动下一首）。
    ///
    /// 返回实际播放的下一首；队列为空时返回 `None`。
    ///
    /// ⚠️ 只认 `eof`：`end-file` 的 reason 里只有 `eof`（文件自然播完）该推进，
    /// `stop`（`loadfile` 换文件、`seek`、主动 `stop`）与 `error` 都不该推进。早期把
    /// `stop` 也当成「播完」，于是每次换歌都会额外触发一次推进——刚切到点歌的《晴天》，
    /// mpv 随即为上一个文件报 `stop`，就被当成播完而切到下一首（实测「切歌成功后又被
    /// 别的歌顶替」）；seek 与重头播放是同一个坑。
    pub async fn handle_ended(&self, reason: &str) -> Result<Option<QueueItem>, String> {
        if reason != "eof" {
            debug!(reason, "忽略非自然播完的 end-file（loadfile/seek/stop 都会发）");
            return Ok(None);
        }

        // ⚠️ 单曲循环只对点歌队列生效。空闲歌单有自己的 `idle_mode`
        //（顺序/列表循环/单曲循环/随机），这里拦住会把它的模式覆盖掉——
        // 实测「直播与播放选单曲循环，空闲歌单设顺序播放」时空闲歌单永远重复同一首。
        let (is_idle, mode) = {
            let guard = self.state.read();
            (
                guard.current_is_idle,
                playback_mode_of(&guard),
            )
        };
        if !is_idle && mode == PlaybackMode::RepeatOne {
            // 单曲循环：重载同一首。
            // 先把值取出来再 await：读锁守卫不能跨 await（否则 future 不是 Send）。
            let current = self.state.read().current.clone();
            if let Some(item) = current {
                self.load_and_play(item.clone()).await?;
                return Ok(Some(item));
            }
        }

        self.advance(false).await
    }

    /// 事件循环：订阅播放器事件，驱动状态同步与自动下一首。
    pub async fn run_event_loop(
        self: Arc<Self>,
        mut events: tokio::sync::broadcast::Receiver<PlayerEvent>,
    ) {
        info!("播放事件循环已启动");
        let mut failures = 0usize;
        loop {
            match events.recv().await {
                Ok(event) => match event {
                    PlayerEvent::Position(position) => {
                        self.update_position(position);
                    }
                    PlayerEvent::Duration(duration) => {
                        self.update_duration(duration);
                    }
                    PlayerEvent::Paused(paused) => {
                        self.sync_paused(paused);
                    }
                    PlayerEvent::Started { .. } => {
                        failures = 0;
                    }
                    PlayerEvent::Ended { reason } => {
                        // seek / 重头播放 / 手动跳转会触发一次 `end-file`，那不是「播完了」，
                        // 必须丢弃，否则会自动切到下一首、把用户刚要重播的那首顶掉。
                        // ⚠️ 只丢弃 `stop`，不能连 `eof` 一起丢：早期无条件消费这个标记，
                        // 于是「seek 到接近结尾」会把随后的真实 `eof` 也吞掉，歌曲自然播完
                        // 后队列不再推进（表现为播放卡住）。
                        // 现在 `stop` 才消费标记，`eof` 一律照常处理。
                        if reason == "stop"
                            && self
                                .expect_end_file
                                .swap(false, std::sync::atomic::Ordering::SeqCst)
                        {
                            debug!(%reason, "忽略 seek/跳转产生的 end-file（非播完）");
                            continue;
                        }
                        match self.handle_ended(&reason).await {
                            Ok(_) => failures = 0,
                            Err(err) => {
                                failures += 1;
                                warn!(error = %err, failures, "推进队列失败");
                                if failures >= MAX_LOAD_FAILURES {
                                    warn!("连续多次加载失败，停止自动推进");
                                    failures = 0;
                                } else {
                                    // 继续尝试下一首
                                    let _ = self.start_next().await;
                                }
                            }
                        }
                    }
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    debug!(skipped, "播放事件落后，忽略部分中间状态");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
        info!("播放事件循环已退出");
    }

    /// 当前位置轮询：兜底 mpv 不推送 `time-pos` 的场景。
    ///
    /// mpv 只在属性变化时推送 `property-change`，加载网络流后可能几秒内完全没有
    /// `time-pos` 事件，界面进度条会一直停在 0；这里用低频轮询主动查询兜底。
    pub async fn run_position_poller(self: Arc<Self>, interval: Duration) {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let (playing, known_position) = {
                let state = self.state.read();
                (
                    state.player.playing && !state.player.paused,
                    state.player.position,
                )
            };
            if !playing {
                continue;
            }
            if let Some(position) = self.backend.current_position().await {
                // 只在前进了才更新，避免抖动
                if position > known_position {
                    self.update_position(position);
                }
            }
        }
    }

    // ── 内部状态同步 ────────────────────────────────────────────────────────

    /// 当前播放模式（从 `AppState` 读取，保证前后端一致）。
    fn playback_mode(&self) -> PlaybackMode {
        playback_mode_of(&self.state.read())
    }

    /// 取随机索引（简单递增 + 时间抖动，避免引入 rand 依赖）。
    fn next_random_index(&self) -> usize {
        let counter = self
            .random_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0);
        ((counter.wrapping_mul(2654435761).wrapping_add(nanos)) % 9973) as usize
    }

    /// 从队列中移除指定条目（取出来准备播放时调用）。
    ///
    /// ⚠️ 必须同时做名额记账：弹幕点歌离队时要递减 `danmaku_pending`，否则计数
    /// 只增不减，额度被幽灵条目永久占住，「弹幕最多 7 首」的限制随之失效
    /// （实测：队列已空但 `danmaku_pending` 还是 8，于是还能继续点）。
    fn take_from_queue(&self, id: Uuid) {
        let removed = self.state.mutate(|s| {
            s.queue
                .iter()
                .position(|i| i.id == id)
                .map(|pos| s.queue.remove(pos))
        });
        if let Some(item) = removed {
            crate::queue::account_leave(&self.state, &item);
        }
    }

    /// 某个条目是否来自**空闲歌单**（按 `QueueItem.id` 比对）。
    ///
    /// `playing` 是混合序列，条目本身没有"来源"字段，因此每次切歌都要重新推导，
    /// 不能沿用上一次的来源标记——否则沿序列走一步来源就错位了。
    fn item_is_from_idle(&self, item: &QueueItem) -> bool {
        let guard = self.state.read();
        crate::player::idle::contains(&guard.idle, item.id)
    }

    /// 标记「当前播的是空闲歌单」并做主播点歌的名额记账。
    ///
    /// 播放顺序由 `playing` + `cursor` 唯一表达（追加发生在
    /// [`Self::start_next_inner`] 取新歌时），这里只做两件事：主播点歌开播时释放
    /// 额外弹幕名额，以及标记来源（供「有人点歌要不要立刻让位」判断）。
    fn mark_current_source(&self, item: &QueueItem, from_idle: bool) {
        if !from_idle && item.priority == QueuePriority::Host {
            queue::grant_host_bonus(&self.state, &(self.rules)());
        }
        self.state.mutate(|s| s.current_is_idle = from_idle);
        // 来源标记决定「有人点歌要不要立刻让位」与空闲书签回归。
        // 出问题时（点歌不切歌 / 空闲歌曲被误判）这一行能直接看出是哪个分支写错了，
        // 所以保留在 info 级别。
        info!(title = %item.song.title, from_idle, "已标记当前曲目来源");
    }

    // ── 空闲歌单（阶段 10a）──────────────────────────────────────────────

    /// 从空闲歌单取下一首（返回条目与它的来源标记）。
    ///
    /// 两个索引分工：`idle_next` 是本次该取哪首（由
    /// [`crate::player::idle_next_index`] 决定），`idle_current` 是取到之后记下的
    /// 「重头播」书签。旧实现只有一个语义为「下次取哪首」的 `idle_cursor`，
    /// 拿它当书签会差 1，表现为回来时跳过一首。
    /// `skipped` 是刚被「下一首」跳过的那一条：单曲循环模式下必须把它也排除，
    /// 否则 `idle_next_index` 会一直返回同一首，点下一首变成"原地重播"。
    fn take_from_idle(&self, skipped: Option<Uuid>) -> Option<(QueueItem, bool)> {
        let (mode, len, next, last_index) = {
            let guard = self.state.read();
            // 「上一首」用于随机模式避免连续重复
            let last = guard
                .current
                .as_ref()
                .filter(|_| guard.current_is_idle)
                .and_then(|c| guard.idle.iter().position(|i| i.id == c.id))
                .or(guard.idle_current);
            (guard.idle_mode, guard.idle.len(), guard.idle_next, last)
        };

        // 随机源用纳秒时间戳：这里不需要密码学强度，只要不同调用之间有差异
        let random = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0);

        // ⚠️ 取到的不能是当前正在播的那一首，否则调用方会把它再追加一次到
        // `playing`，「下一首」就停在原地不动。
        //
        // 单曲循环要特别处理：`idle_next_index` 在 LoopOne 下**永远返回同一首**，
        // 所以「点下一首」必须显式突破它——用户点下一首的意图就是换歌。
        // 判据用 `QueueItem.id`（列表里那条记录本身的 id）。
        let mut index = crate::player::idle_next_index(mode, len, next, last_index, random)?;
        let break_loop_one = skipped.is_some() && mode == IdleMode::LoopOne && len > 1;
        if len > 1 {
            let current_item_id = {
                let guard = self.state.read();
                guard
                    .current
                    .as_ref()
                    .filter(|_| guard.current_is_idle)
                    .map(|c| c.id)
            };
            let blocked: Vec<Uuid> = [current_item_id, skipped].into_iter().flatten().collect();
            if break_loop_one || !blocked.is_empty() {
                let mut tries = 0;
                while tries < len {
                    let at_index = self.state.read().idle.get(index).map(|i| i.id);
                    // 单曲循环下即使取到"没被挡住"的同一首，也要往下走一位
                    let must_advance = break_loop_one && tries == 0;
                    if !must_advance && !at_index.is_some_and(|id| blocked.contains(&id)) {
                        break;
                    }
                    index = (index + 1) % len;
                    tries += 1;
                }
            }
        }
        let item = self.state.read().idle.get(index).cloned()?;

        // 记下书签（= 实际在播的这首），并按模式推进「下一次取哪首」
        self.state.mutate(|s| {
            s.idle_current = Some(index);
            s.idle_next = match mode {
                IdleMode::Shuffle => index.wrapping_add(1),
                IdleMode::LoopOne => index,
                _ => (index + 1) % s.idle.len().max(1),
            };
        });
        Some((item, true))
    }

    /// **立刻播放空闲歌单的下一首**（用于「清空点歌列表并播空闲歌单」）。
    ///
    /// 与 [`Self::take_from_idle`] 的区别：这里会真正切歌并加载。
    pub async fn play_idle_next(&self) -> Result<Option<QueueItem>, String> {
        let Some((item, _from_idle)) = self.take_from_idle(None) else {
            return Ok(None);
        };
        let Some(_guard) = self.try_begin_switch() else {
            return Err("正在切歌，请稍后再试".to_string());
        };
        self.mark_current_source(&item, true);
        queue::push_playing(&self.state, item.clone());
        self.sync_current_from_playing();
        self.load_and_play(item.clone()).await?;
        Ok(Some(item))
    }

    /// 人工选中空闲歌单里的某一首（「选择播放」）。
    pub async fn play_idle_index(&self, index: usize) -> Result<Option<QueueItem>, String> {
        let item = {
            let guard = self.state.read();
            guard.idle.get(index).cloned()
        };
        let Some(item) = item else {
            return Err(format!("空闲歌单里没有第 {} 首", index + 1));
        };
        // 串行化：手动播放同样会改 current，必须与自动推进互斥
        let Some(_guard) = self.try_begin_switch() else {
            return Err("正在切歌，请稍后再试".to_string());
        };
        // 人工选曲也要进播放序列，否则「上一首/下一首」跟它脱节
        self.mark_current_source(&item, true);
        queue::push_playing(&self.state, item.clone());
        self.state.mutate(|s| {
            // 记书签并按模式推进「下一次取哪首」
            s.idle_current = Some(index);
            s.idle_next = match s.idle_mode {
                IdleMode::LoopOne => index,
                IdleMode::Shuffle => index.wrapping_add(1),
                _ => (index + 1) % s.idle.len().max(1),
            };
        });
        self.sync_current_from_playing();
        self.load_and_play(item.clone()).await?;
        Ok(Some(item))
    }

    /// 有人点歌时调用：按配置策略决定要不要**立刻**让位给点歌队列。
    ///
    /// 返回 `true` 表示已经切过去。⚠️ 只在当前正在播空闲歌曲时才可能切换：
    /// 已经在播点歌队列的歌不该被新的点歌打断（那是插队，由队列顺序决定）。
    pub async fn on_song_requested(&self) -> bool {
        let (policy, current_is_idle, queue_len) = {
            let guard = self.state.read();
            (
                (self.rules)().idle_switch_policy,
                guard.current_is_idle,
                guard.queue.len(),
            )
        };
        if !crate::player::should_switch_now(policy, current_is_idle, queue_len) {
            return false;
        }
        // 串行化：拿不到锁说明已有切歌在跑，这次就不重复打断
        let Some(_guard) = self.try_begin_switch() else {
            return false;
        };
        // ⚠️ 必须在切走之前记下书签：切完之后 `current` 已经变成点歌，
        // 就再也找不到"刚才在播哪首空闲曲"了。
        self.remember_idle_bookmark();
        info!("有人点歌，按「立即切换」策略中断当前空闲歌曲");
        matches!(self.start_next_inner(true).await, Ok(Some(_)))
    }

    /// 结束当前项（标记状态）并清空 `current`。
    ///
    /// `current` 为空时不做任何事（失败发生在「取队首 → 加载」之间），
    /// 由调用方通过 [`PlayerController::drop_item`] 按 id 清理队列。
    /// `keep_display` 为 true 时保留 `current` 不清空：新歌加载需要几百毫秒，
    /// 提前清空会让面板在每首歌之间闪一下「暂时没有歌曲」。
    async fn finish_current(&self, status: QueueStatus, keep_display: bool) {
        if keep_display {
            // 只更新状态，不清空 current（面板继续显示上一首，直到新歌接上）
            self.state.mutate(|s| {
                if let Some(item) = s.current.as_mut() {
                    item.status = status;
                }
            });
            if let Some(item) = self.state.read().current.clone() {
                debug!(title = %item.song.title, ?status, "当前歌曲结束（等待下一首接上）");
                self.state.publish(HubEvent::SongFinished {
                    item_id: item.id,
                    title: item.song.title.clone(),
                    status,
                });
            }
            return;
        }

        let finished = {
            let mut guard = self.state.write();
            let mut finished = guard.current.take();
            if let Some(item) = finished.as_mut() {
                item.status = status;
            }
            finished
        };

        if let Some(item) = finished {
            debug!(title = %item.song.title, ?status, "当前歌曲结束");
            self.state.publish(HubEvent::SongFinished {
                item_id: item.id,
                title: item.song.title.clone(),
                status,
            });
        }
        self.sync_state(false, false, 0.0, 0.0, None);
    }

    /// 停止播放并清空当前项（队列播空 / 加载失败 / 用户清空队列时）。
    async fn stop_playback(&self, status: QueueStatus) {
        self.finish_current(status, false).await;
        let _ = self.backend.stop().await;
        self.sync_state(false, false, 0.0, 0.0, None);
    }

    /// 同步播放位置到 `AppState`。
    fn update_position(&self, position: f64) {
        {
            let mut guard = self.playback.lock().unwrap_or_else(|e| e.into_inner());
            guard.position = position;
        }
        // 阶段 7：顺便算出「当前唱到第几行」，面板据此高亮与滚动。
        let lyric_index = self
            .lyrics
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|lyrics| lyrics.index_at(position));

        self.state.mutate(|s| {
            s.player.position = position;
            s.player.lyric_index = lyric_index;
        });
    }

    /// 同步时长到 `AppState`。
    fn update_duration(&self, duration: f64) {
        {
            let mut guard = self.playback.lock().unwrap_or_else(|e| e.into_inner());
            guard.duration = duration;
        }
        // 播放器报出的时长比搜索结果更准（网络流可能略有差异），只要大于 0 就采用。
        if duration > 0.0 {
            self.state.mutate(|s| {
                s.player.duration = duration;
                if let Some(current) = s.current.as_mut() {
                    current.song.duration = duration.round() as u64;
                }
            });
        }
    }

    /// 同步暂停状态。
    ///
    /// ⚠️ 只动 `paused`，不要把「从没播过」的曲目变成"在播"：早期实现是
    /// `playing = !paused && current.is_some()`，但 mpv 刚启动、什么文件都没加载时
    /// 也会上报一次 `pause=false`（属性初值）。于是「重启后从快照恢复了 `current`
    /// （只显示、不播放）→ mpv 发来 `Paused(false)` → `playing` 被置成 true」，
    /// 界面显示「正在播放」但实际没有声音。
    ///
    /// 因此用旧状态区分：旧值 `paused == true` 说明之前是真暂停，解除暂停即恢复播放；
    /// 旧值 `paused == false && playing == false` 说明从未开始播放（例如刚从快照恢复），
    /// 此时的 `pause=false` 只是 mpv 初值，忽略。
    fn sync_paused(&self, paused: bool) {
        // ⚠️ 还没真正开播时，mpv 的 pause 事件不反映用户意图。
        //
        // mpv 刚连上、还没加载文件就会上报一次 `pause=false`（属性初值）。
        // 而重启后我们把状态恢复成「暂停」，于是这个初值会被当成
        // 「用户解除了暂停」→ 界面显示在播，实际没有声音、位置也不动。
        if !self
            .playback_started
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            debug!(paused, "尚未开播，忽略 mpv 的 pause 事件");
            return;
        }
        self.state.mutate(|s| {
            let was_paused = s.player.paused;
            let was_playing = s.player.playing;
            s.player.paused = paused;
            if was_paused {
                // 真暂停被解除 → 恢复播放；继续暂停 → 仍未在播
                s.player.playing = !paused;
            } else if was_playing && paused {
                s.player.playing = false;
            }
            // 其余情况（从未播放 + pause=false）保持 playing 不变
        });
    }

    /// 跳转到指定位置（秒）。用于前端拖动进度条。
    ///
    /// 除了转发给 mpv，还要立刻把位置写进状态并广播：位置轮询器最长 2 秒才刷新
    /// 一次，不这样做界面会在拖动后「弹回」旧位置，看起来像拖动失败。
    pub async fn seek(&self, position: f64) -> Result<(), String> {
        let target = position.max(0.0);
        // 标记：这次 seek 会引发一个 end-file，事件循环要丢弃它。
        // 必须在发送 seek 之前置位，否则事件可能先于我们返回就到达。
        self.expect_end_file
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.backend
            .seek(target)
            .await
            .map_err(|e| format!("跳转失败：{e}"))?;

        // 复用统一的位置同步（顺带重算歌词高亮行），再解除暂停：
        // 拖动进度条视为「想继续听」。
        self.update_position(target);
        if let Err(err) = self.backend.set_paused(false).await {
            debug!(error = %err, "跳转后恢复播放失败（可能本来就没暂停）");
        }
        self.sync_paused(false);
        Ok(())
    }

    /// 一次性同步播放状态（停止播放时用）。
    ///
    /// 同时清空 `current` 对应的歌词缓存：没有播放中的歌曲时不应还对外报告
    /// 「正在唱第几行」（面板会显示上一次的高亮行）。
    fn sync_state(
        &self,
        playing: bool,
        paused: bool,
        position: f64,
        duration: f64,
        lyrics: Option<String>,
    ) {
        if !playing {
            *self.lyrics.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
        self.state.mutate(|s| {
            s.player.playing = playing;
            s.player.paused = paused;
            s.player.position = position;
            s.player.duration = duration;
            s.player.lyrics = lyrics;
            if !playing {
                s.player.lyric_index = None;
            }
        });
    }

    /// 当前播放进度（测试与 API 用）。
    pub fn progress(&self) -> (f64, f64) {
        let guard = self.playback.lock().unwrap_or_else(|e| e.into_inner());
        (guard.position, guard.duration)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        AppState, BilibiliState, MusicPlatform, PlayerState, Song, SongSource,
    };
    use crate::music::SecretStore;
    use crate::player::MockPlayer;
    use crate::VERSION;

    fn state() -> StateCell {
        StateCell::new(AppState::new(
            VERSION,
            PlayerState::default(),
            BilibiliState::default(),
        ))
    }

    fn music_service(tag: &str) -> Arc<MusicService> {
        let dir = std::env::temp_dir().join(format!(
            "bsr-player-{tag}-{:?}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::create_dir_all(&dir);
        Arc::new(MusicService::with_stores(
            SecretStore::file_only("netease.cookie", &dir),
            SecretStore::file_only("qq.cookie", &dir),
        ))
    }

    fn resolved_item(title: &str) -> QueueItem {
        let mut song = Song::placeholder(title, "测试歌手");
        song.id = "123456".into();
        song.platform = MusicPlatform::Netease;
        song.duration = 200;
        song.source = SongSource::Resolved;
        QueueItem::new(song, "观众甲", None)
    }

    /// 构造一个「地址已被 mock」的控制器（真实取地址会访问音乐平台，既慢又会被风控）。
    fn controller(state: &StateCell, backend: &Arc<MockPlayer>, tag: &str) -> Arc<PlayerController> {
        controller_with_rules(state, backend, tag, crate::config::RequestRules::default())
    }

    /// 同上，但注入指定的点歌规则（用于验证「立即切换 / 播完再切」）。
    fn controller_with_rules(
        state: &StateCell,
        backend: &Arc<MockPlayer>,
        tag: &str,
        rules: crate::config::RequestRules,
    ) -> Arc<PlayerController> {
        let controller = PlayerController::with_config(
            state.clone(),
            backend.clone() as Arc<dyn PlayerBackend>,
            music_service(tag),
            PickPolicy::default,
            move || rules.clone(),
        );
        controller.set_url_override("http://mock.local/song.mp3");
        controller
    }

    #[test]
    fn next_candidate_sequential_takes_first_and_removes() {
        let queue = vec![resolved_item("A"), resolved_item("B")];
        let (item, take) = next_candidate(PlaybackMode::Sequential, None, &queue, 0).unwrap();
        assert_eq!(item.song.title, "A");
        assert!(take, "顺序模式应从队列取出");
    }

    #[test]
    fn next_candidate_random_uses_index() {
        let queue = vec![resolved_item("A"), resolved_item("B"), resolved_item("C")];
        let (item, take) = next_candidate(PlaybackMode::Random, None, &queue, 4).unwrap();
        assert_eq!(item.song.title, "B", "4 % 3 == 1");
        assert!(take);
    }

    #[test]
    fn next_candidate_repeat_one_keeps_current() {
        let current = resolved_item("A");
        let queue = vec![resolved_item("B")];
        let (item, take) =
            next_candidate(PlaybackMode::RepeatOne, Some(&current), &queue, 0).unwrap();
        assert_eq!(item.song.title, "A");
        assert!(!take, "单曲循环不应从队列取出");
    }

    #[test]
    fn next_candidate_repeat_one_falls_back_to_queue_without_current() {
        // 切到单曲循环时还没有当前曲目：必须从队列取一首开始播，否则队列里有歌却什么都不播。
        let queue = vec![resolved_item("A"), resolved_item("B")];
        let (item, take) = next_candidate(PlaybackMode::RepeatOne, None, &queue, 0).unwrap();
        assert_eq!(item.song.title, "A");
        assert!(take, "无当前曲目时应从队列取出");
    }

    #[test]
    fn next_candidate_returns_none_on_empty_queue() {
        assert!(next_candidate(PlaybackMode::Sequential, None, &[], 0).is_none());
        assert!(next_candidate(PlaybackMode::Random, None, &[], 0).is_none());
        // 单曲循环、队列为空、当前也为空 → 没得播
        assert!(next_candidate(PlaybackMode::RepeatOne, None, &[], 0).is_none());
        // 单曲循环、队列为空但有当前曲目 → 继续循环这一首
        let current = resolved_item("A");
        let (item, take) = next_candidate(PlaybackMode::RepeatOne, Some(&current), &[], 0).unwrap();
        assert_eq!(item.song.title, "A");
        assert!(!take);
    }

    #[test]
    fn play_mode_conversions_roundtrip() {
        for mode in [
            PlaybackMode::Sequential,
            PlaybackMode::Random,
            PlaybackMode::RepeatOne,
        ] {
            let model: crate::models::PlayMode = mode.into();
            assert_eq!(PlaybackMode::from(model), mode);
        }
    }

    #[tokio::test]
    async fn start_next_marks_item_playing_and_loads_backend() {
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let controller = controller(&state, &backend, "start");

        let item = resolved_item("晴天");
        state.mutate(|s| s.queue.push(item.clone()));

        let played = controller.start_next().await.expect("应播放成功").unwrap();
        assert_eq!(played.id, item.id);

        let guard = state.read();
        assert_eq!(guard.current.as_ref().unwrap().id, item.id);
        assert_eq!(guard.current.as_ref().unwrap().status, QueueStatus::Playing);
        assert!(guard.queue.is_empty(), "顺序模式应从队列移出");
        assert!(guard.player.playing);
        assert_eq!(backend.status(), PlaybackStatus::Playing);
    }

    #[tokio::test]
    async fn empty_queue_stops_playback() {
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let controller = controller(&state, &backend, "empty");

        let result = controller.start_next().await.expect("空队列也应成功");
        assert!(result.is_none());
        assert!(!state.read().player.playing);
        assert_eq!(backend.status(), PlaybackStatus::Idle);
    }

    #[tokio::test]
    async fn unresolved_song_fails_over_to_next_item() {
        // 已标记失败的条目会被清出队列并继续尝试下一首；这里只有这一个坏条目，
        // 所以最终没有可播的歌（`Ok(None)`），但失败原因要通过事件推给前端。
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let controller = controller(&state, &backend, "unresolved");

        let mut song = Song::placeholder("搜不到的歌", "");
        song.mark_failed("没有搜索到《搜不到的歌》");
        let item = QueueItem::new(song, "观众", None);
        state.mutate(|s| s.queue.push(item));

        let result = controller.start_next().await;
        assert!(result.is_ok(), "坏条目应被跳过而不是让整个调用失败");
        assert!(result.unwrap().is_none(), "没有可播的歌时返回 None");
        assert!(state.read().queue.is_empty(), "失败条目应被移除，避免卡住队列");
        assert!(state.read().current.is_none(), "失败条目不应成为当前播放项");
        assert!(!state.read().player.playing);
    }

    #[tokio::test]
    async fn unresolved_song_falls_through_to_the_next_playable_one() {
        // 队首坏、第二首正常：应自动跳过坏条目播出第二首
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let controller = controller(&state, &backend, "failover");

        let mut broken = Song::placeholder("搜不到的歌", "");
        broken.mark_failed("没有搜索到《搜不到的歌》");
        state.mutate(|s| {
            s.queue.push(QueueItem::new(broken, "观众", None));
            s.queue.push(resolved_item("能播的歌"));
        });

        let played = controller.start_next().await.expect("应成功").expect("应有歌");
        assert_eq!(played.song.title, "能播的歌");
        assert_eq!(
            state.read().current.as_ref().unwrap().song.title,
            "能播的歌"
        );
        assert!(
            state.read().queue.is_empty(),
            "坏条目与已播放条目都应离开队列"
        );
    }

    #[tokio::test]
    async fn all_items_broken_stops_within_failure_budget() {
        // 整队都是坏条目：不能死循环，且要停在最大失败次数内
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let controller = controller(&state, &backend, "allbroken");

        state.mutate(|s| {
            for i in 0..10 {
                let mut song = Song::placeholder("坏歌", "");
                song.mark_failed(format!("没有搜索到《坏歌{i}》"));
                s.queue.push(QueueItem::new(song, "观众", None));
            }
        });

        let result = controller.start_next().await;
        assert!(result.is_ok(), "应正常结束而不是 panic/死循环");
        assert!(result.unwrap().is_none());
        assert!(
            state.read().queue.len() > 0,
            "失败次数用尽后剩余条目留在队列里（主播可手动清理）"
        );
        assert!(state.read().current.is_none());
    }

    #[tokio::test]
    async fn skip_advances_to_next_song() {
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let controller = controller(&state, &backend, "skip");

        state.mutate(|s| {
            s.queue.push(resolved_item("第一首"));
            s.queue.push(resolved_item("第二首"));
        });

        let first = controller.start_next().await.unwrap().unwrap();
        assert_eq!(first.song.title, "第一首");

        let next = controller.skip().await.unwrap().unwrap();
        assert_eq!(next.song.title, "第二首");
        assert_eq!(state.read().current.as_ref().unwrap().song.title, "第二首");
    }

    #[tokio::test]
    async fn skip_in_repeat_one_really_changes_song() {
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let controller = controller(&state, &backend, "repeat-skip");
        state.mutate(|s| {
            s.idle_mode = IdleMode::LoopOne;
            s.queue.push(resolved_item("第一首"));
            s.queue.push(resolved_item("第二首"));
        });

        let first = controller.start_next().await.unwrap().unwrap();
        assert_eq!(first.song.title, "第一首");

        // 单曲循环下点跳过应换歌，而不是重播同一首
        let next = controller.skip().await.unwrap();
        assert!(
            next.is_none() || next.as_ref().unwrap().song.title != "第一首",
            "单曲循环下跳过不应重播同一首"
        );
        assert_eq!(
            state.read().current.as_ref().unwrap().song.title,
            "第二首",
            "应切到队列里的下一首"
        );
    }

    #[tokio::test]
    async fn ended_event_repeats_song_in_repeat_one() {
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let controller = controller(&state, &backend, "repeat");
        state.mutate(|s| {
            // 播放模式的实际来源是 `idle_mode`（界面上的「播放模式」就是它）
            s.idle_mode = IdleMode::LoopOne;
            s.queue.push(resolved_item("循环曲"));
        });
        controller.start_next().await.unwrap().unwrap();

        let again = controller.handle_ended("eof").await.unwrap().unwrap();
        assert_eq!(again.song.title, "循环曲", "单曲循环应重播同一首");
        assert_eq!(state.read().current.as_ref().unwrap().song.title, "循环曲");
    }

    #[tokio::test]
    async fn ended_event_advances_queue_in_sequential_mode() {
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let controller = controller(&state, &backend, "seq");
        state.mutate(|s| {
            s.queue.push(resolved_item("A"));
            s.queue.push(resolved_item("B"));
        });

        controller.start_next().await.unwrap().unwrap();
        let next = controller.handle_ended("eof").await.unwrap().unwrap();
        assert_eq!(next.song.title, "B");
        assert_eq!(state.read().current.as_ref().unwrap().song.title, "B");
    }

    #[tokio::test]
    async fn non_eof_end_file_does_not_advance() {
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let controller = controller(&state, &backend, "noneof");
        state.mutate(|s| s.queue.push(resolved_item("A")));
        controller.start_next().await.unwrap().unwrap();

        // loadfile 切换文件时 mpv 也会发 end-file(reason=stop/error)，不应推进队列
        let result = controller.handle_ended("error").await.unwrap();
        assert!(result.is_none());
        assert_eq!(state.read().current.as_ref().unwrap().song.title, "A");
    }

    #[tokio::test]
    async fn event_loop_tracks_position_and_duration() {
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let controller = controller(&state, &backend, "events");
        state.mutate(|s| s.queue.push(resolved_item("A")));

        let rx = backend.subscribe();
        let controller_clone = Arc::clone(&controller);
        let handle = tokio::spawn(controller_clone.run_event_loop(rx));

        controller.start_next().await.unwrap().unwrap();
        backend.emit(PlayerEvent::Position(12.5));
        backend.emit(PlayerEvent::Duration(240.0));
        backend.emit(PlayerEvent::Paused(true));

        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if state.read().player.position > 12.0 {
                break;
            }
        }
        let guard = state.read();
        assert!(
            (guard.player.position - 12.5).abs() < 0.01,
            "位置应同步：{}",
            guard.player.position
        );
        assert!(
            (guard.player.duration - 240.0).abs() < 0.01,
            "时长应同步：{}",
            guard.player.duration
        );
        assert!(guard.player.paused);
        drop(guard);

        handle.abort();
    }

    #[tokio::test]
    async fn event_loop_auto_advances_on_end() {
        // 端到端：播完 → 自动下一首（用 MockPlayer 的 Ended 事件驱动）
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let controller = controller(&state, &backend, "auto");
        state.mutate(|s| {
            s.queue.push(resolved_item("A"));
            s.queue.push(resolved_item("B"));
        });

        let rx = backend.subscribe();
        let controller_clone = Arc::clone(&controller);
        let handle = tokio::spawn(controller_clone.run_event_loop(rx));

        controller.start_next().await.unwrap().unwrap();
        assert_eq!(state.read().current.as_ref().unwrap().song.title, "A");

        // 模拟 mpv 播完第一首
        backend.emit(PlayerEvent::Ended {
            reason: "eof".into(),
        });

        for _ in 0..80 {
            tokio::time::sleep(Duration::from_millis(25)).await;
            if state.read().current.as_ref().map(|i| i.song.title.clone())
                == Some("B".to_string())
            {
                break;
            }
        }
        assert_eq!(
            state.read().current.as_ref().unwrap().song.title,
            "B",
            "播完应自动切到下一首"
        );
        handle.abort();
    }

    #[tokio::test]
    async fn playing_a_song_loads_lyrics_and_tracks_current_line() {
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let controller = controller(&state, &backend, "lyrics");
        controller.set_lyrics_override(
            "[00:00.00]第一句\n[00:05.00]第二句\n[00:10.00]第三句",
        );
        state.mutate(|s| s.queue.push(resolved_item("带歌词的歌")));

        controller.start_next().await.unwrap().unwrap();

        // 歌词原文与解析结果都应就绪
        let lyrics = controller.current_lyrics().expect("应有歌词");
        assert_eq!(lyrics.lines.len(), 3);
        assert!(state.read().player.lyrics.as_deref().unwrap().contains("第二句"));

        // 位置推进 → 当前行跟着变（直接调用内部同步，等价于事件循环收到 Position）
        controller.update_position(6.0);
        assert_eq!(state.read().player.lyric_index, Some(1), "6 秒应落在第二句");

        controller.update_position(11.0);
        assert_eq!(state.read().player.lyric_index, Some(2));

        // 超出末尾保持最后一行
        controller.update_position(999.0);
        assert_eq!(state.read().player.lyric_index, Some(2));
    }

    #[tokio::test]
    async fn no_lyrics_keeps_index_none() {
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let controller = controller(&state, &backend, "nolyrics");
        state.mutate(|s| s.queue.push(resolved_item("纯音乐")));

        controller.start_next().await.unwrap().unwrap();
        assert!(controller.current_lyrics().is_none());
        assert!(state.read().player.lyrics.is_none());
        controller.update_position(30.0);
        assert_eq!(
            state.read().player.lyric_index,
            None,
            "没有歌词时不应产生当前行"
        );
    }

    #[tokio::test]
    async fn finishing_a_song_clears_lyrics_state() {
        // 面板依赖「没有 current 就不显示歌词」。曾经出现过
        // 「歌曲已结束但仍显示上一首歌词高亮」的残留，这里锁住该行为。
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let controller = controller(&state, &backend, "clearlyrics");
        controller.set_lyrics_override("[00:00.00]第一句\n[00:02.00]第二句");
        state.mutate(|s| s.queue.push(resolved_item("有歌词的歌")));

        controller.start_next().await.unwrap().unwrap();
        controller.update_position(2.5);
        assert_eq!(state.read().player.lyric_index, Some(1));

        // 播完且队列为空 → 停止播放
        let next = controller.handle_ended("eof").await.unwrap();
        assert!(next.is_none(), "队列为空时不应有下一首");

        let guard = state.read();
        assert!(guard.current.is_none());
        assert!(guard.player.lyrics.is_none(), "停止后应清空歌词文本");
        assert_eq!(guard.player.lyric_index, None, "停止后应清空当前行");
        drop(guard);
        assert!(controller.current_lyrics().is_none(), "内部歌词缓存也应清空");
    }

    #[tokio::test]
    async fn progress_reports_loaded_position() {
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let controller = controller(&state, &backend, "progress");
        state.mutate(|s| s.queue.push(resolved_item("A")));
        controller.start_next().await.unwrap().unwrap();
        let (position, duration) = controller.progress();
        assert_eq!(position, 0.0);
        assert_eq!(duration, 200.0, "初始时长来自歌曲元信息");
    }

    #[tokio::test]
    async fn paused_song_resumes_instead_of_switching() {
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let controller = controller(&state, &backend, "resume");
        state.mutate(|s| {
            s.queue.push(resolved_item("A"));
            s.queue.push(resolved_item("B"));
        });
        controller.start_next().await.unwrap().unwrap();
        backend.set_paused(true).await.unwrap();

        // 再次 start_next 应该是「继续播放」而不是换歌
        let same = controller.start_next().await.unwrap().unwrap();
        assert_eq!(same.song.title, "A");
        assert!(!state.read().player.paused);
    }

    // ── 上一首/下一首统一序列（阶段 10c）────────────────────────────────

    #[tokio::test]
    async fn previous_and_next_walk_the_same_history_sequence() {
        // 核心回归：下一首 A→B，再上一首应回到 A，再下一首又回到 B——因为两者遍历
        // 的是同一条序列，而不是一个走历史、一个走队列。
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let controller = controller(&state, &backend, "seq");
        state.mutate(|s| {
            s.queue.push(resolved_item("A"));
            s.queue.push(resolved_item("B"));
        });

        let a = controller.start_next().await.unwrap().unwrap();
        assert_eq!(a.song.title, "A");
        let b = controller.skip().await.unwrap().unwrap();
        assert_eq!(b.song.title, "B");
        let back = controller.previous().await.unwrap().unwrap();
        assert_eq!(back.song.title, "A");
        // 下一首 → 又能回到 B（序列里 A 之后就是 B）
        let fwd = controller.skip().await.unwrap().unwrap();
        assert_eq!(fwd.song.title, "B");
    }

    #[tokio::test]
    async fn previous_then_next_does_not_lose_position() {
        // 播 A、B、C，走到 C 后连点两次「上一首」回到 A，再点「下一首」应回到 B
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let controller = controller(&state, &backend, "pos");
        for title in ["A", "B", "C"] {
            state.mutate(|s| s.queue.push(resolved_item(title)));
        }
        controller.start_next().await.unwrap(); // A
        controller.skip().await.unwrap(); // B
        controller.skip().await.unwrap(); // C

        let prev1 = controller.previous().await.unwrap().unwrap();
        assert_eq!(prev1.song.title, "B");
        let prev2 = controller.previous().await.unwrap().unwrap();
        assert_eq!(prev2.song.title, "A");

        let next = controller.skip().await.unwrap().unwrap();
        assert_eq!(next.song.title, "B", "上一首两次后，下一首应回到 B");
    }

    // ── 重启后「只显示、不出声」（阶段 10d 回归）──────────────────────────

    #[tokio::test]
    async fn stray_paused_event_does_not_mark_restored_song_as_playing() {
        // 真实 bug：mpv 刚启动、还没加载任何文件时会先上报一次 `pause=false`（属性
        // 初值）。旧实现 `playing = !paused && current.is_some()` 于是把从快照恢复的
        // 当前曲目标成正在播放——界面显示「正在播放」，实际一点声音都没有。
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let controller = controller(&state, &backend, "restore-playing");

        // 模拟「从快照恢复」：有 current，但没有在播
        state.mutate(|s| {
            s.current = Some(resolved_item("上次听的歌"));
            s.player.playing = false;
            s.player.paused = false;
        });

        // mpv 发来 pause=false
        controller.sync_paused(false);

        let guard = state.read();
        assert!(
            !guard.player.playing,
            "恢复后收到 pause=false 不应被当成「正在播放」"
        );
        assert!(!guard.player.paused);
    }

    #[tokio::test]
    async fn paused_event_still_toggles_while_actually_playing() {
        // 反向保证：真的在播时，暂停事件仍要正常工作
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let controller = controller(&state, &backend, "pause-toggle");
        state.mutate(|s| s.queue.push(resolved_item("A")));
        controller.start_next().await.unwrap().unwrap();
        assert!(state.read().player.playing);

        controller.sync_paused(true);
        assert!(state.read().player.paused);
        assert!(!state.read().player.playing, "暂停后不应仍标记为在播");

        controller.sync_paused(false);
        assert!(!state.read().player.paused);
        assert!(state.read().player.playing, "恢复后应重新标记为在播");
    }

    /// 不变量：只要有 `current`，它就必须等于 `playing[cursor]`。
    ///
    /// 这个不变量一旦破坏，界面就会出现「正在播放但显示空」之类的怪象——真实 bug 是
    /// `push_playing` 按 id 去重并回退游标，而那条已被回收，于是 `playing[cursor]`
    /// 取不到，`current` 就空了。
    fn assert_current_matches_cursor(state: &StateCell) {
        let guard = state.read();
        if guard.current.is_none() {
            return;
        }
        let at_cursor = guard.playing.get(guard.cursor);
        assert!(
            at_cursor.is_some(),
            "cursor({}) 越界，但 current 有值（playing.len={}）",
            guard.cursor,
            guard.playing.len()
        );
        assert_eq!(
            guard.current.as_ref().map(|i| i.id),
            at_cursor.map(|i| i.id),
            "current 必须等于 playing[cursor]"
        );
    }

    #[tokio::test]
    async fn clear_and_play_idle_keeps_current_consistent() {
        // 「清空并播空闲歌单」后，current 必须与 playing[cursor] 一致
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let controller = controller(&state, &backend, "clear-idle");
        state.mutate(|s| {
            s.idle.push(resolved_item("空闲甲"));
            s.idle.push(resolved_item("空闲乙"));
            s.queue.push(resolved_item("点歌甲"));
        });
        // 先播一首点歌
        controller.start_next().await.unwrap().unwrap();
        assert_current_matches_cursor(&state);

        let played = controller.play_idle_next().await.unwrap().unwrap();
        assert_eq!(played.song.title, "空闲甲");
        assert_current_matches_cursor(&state);
        assert!(state.read().current_is_idle);
    }

    #[tokio::test]
    async fn playing_idle_twice_never_repeats_the_current_song() {
        // 空闲歌单只有两首时连续取三次，不能出现连着两次同一首
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let controller = controller(&state, &backend, "idle-norepeat");
        state.mutate(|s| {
            s.idle.push(resolved_item("空闲甲"));
            s.idle.push(resolved_item("空闲乙"));
        });

        let first = controller.play_idle_next().await.unwrap().unwrap();
        let second = controller.play_idle_next().await.unwrap().unwrap();
        // 按队列项 id 比较：测试工厂里 `song.id` 是常量，用它判重会误判成"同一首"
        assert_ne!(
            first.id, second.id,
            "连续两次取空闲歌不应取到同一条记录"
        );
        assert_eq!(first.song.title, "空闲甲");
        assert_eq!(second.song.title, "空闲乙", "第二首应是列表里的下一首");
        assert_current_matches_cursor(&state);
    }

    #[tokio::test]
    async fn previous_failure_restores_cursor_and_current() {
        // 真实故障：上一首版权受限播不了，`load_and_play` 失败后游标留在坏条目上、
        // current 被清空 → 不变量被破坏，接口报 409。现在必须还原到最后成功播放的那首。
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let controller = controller(&state, &backend, "prev-fail");

        // 前一首标成解析失败（模拟版权受限），后一首正常
        let mut broken = Song::placeholder("播不了的歌", "");
        broken.mark_failed("该歌曲无法播放（版权限制）");
        state.mutate(|s| {
            s.playing.push(QueueItem::new(broken, "观众", None));
            s.playing.push(resolved_item("正常歌"));
            s.cursor = 1;
            s.current = s.playing.get(1).cloned();
        });

        let result = controller.previous().await;
        assert!(result.is_err(), "播不了时应返回错误而不是假装成功");

        // 状态必须还原：游标回到正常歌，current 仍是正常歌
        let guard = state.read();
        assert_eq!(guard.cursor, 1, "失败后游标应还原");
        assert_eq!(
            guard.current.as_ref().map(|i| i.song.title.clone()),
            Some("正常歌".to_string()),
            "失败后 current 不应被清空"
        );
        assert_current_matches_cursor(&state);
    }

    #[tokio::test]
    async fn skip_in_repeat_one_advances_through_idle_playlist() {
        // 用户报告：把播放模式设为「单曲循环」后点「下一首」无效，
        // 只会回到当前这首歌。
        //
        // 根因是跳过路径把当前项从 `playing` 摘掉了，而 `take_from_idle`
        // 当时只按「current」判重 → 又取回同一首。现在把"刚跳过的条目"也排除。
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let controller = controller(&state, &backend, "repeat-idle-skip");
        state.mutate(|s| {
            s.idle_mode = IdleMode::LoopOne; // 单曲循环
            s.idle.push(resolved_item("空闲甲"));
            s.idle.push(resolved_item("空闲乙"));
        });

        let first = controller.play_idle_next().await.unwrap().unwrap();
        assert_eq!(first.song.title, "空闲甲");

        // 单曲循环下点「下一首」应当**换歌**（不是重播甲）
        let next = controller.skip().await.unwrap().unwrap();
        assert_ne!(
            next.id, first.id,
            "单曲循环下点下一首不应回到同一首（实际又播了 {}）",
            next.song.title
        );
    }

    #[tokio::test]
    async fn previous_keeps_history_when_a_song_is_unplayable() {
        // 用户报告「上一首怎么又坏了」。
        //
        // 根因：加载失败时会把条目从 `playing` **摘掉**，历史因此被销毁——
        // `previous` 再也回不到它，且序列越用越短（实测 playing 从 2 掉到 1）。
        // 现在失败只让游标绕过，条目保留。
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let controller = controller(&state, &backend, "prev-keep-history");
        state.mutate(|s| {
            s.queue.push(resolved_item("甲"));
            s.queue.push(resolved_item("乙"));
        });

        let first = controller.start_next().await.unwrap().unwrap();
        assert_eq!(first.song.title, "甲");
        let second = controller.skip().await.unwrap().unwrap();
        assert_eq!(second.song.title, "乙");
        let before = state.read().playing.len();
        assert_eq!(before, 2, "两首都应在播放序列里");

        // 让「甲」变得播不了（模拟版权受限），再点上一首：序列不应被削短
        controller.set_url_override("http://mock.local/broken.mp3");
        backend.fail_urls(&["http://mock.local/broken.mp3"]);
        let _ = controller.previous().await;

        let after = state.read().playing.len();
        assert!(
            after >= 2,
            "播不了的曲目不应把历史摘掉（之前 {before} 条，现在 {after} 条）"
        );
        assert!(
            state.read().playing.iter().any(|i| i.song.title == "甲"),
            "「甲」应仍在播放序列里，否则永远回不去"
        );
    }

    #[tokio::test]
    async fn walking_idle_playlist_keeps_current_is_idle_true() {
        // 用户报告「点歌马上就切歌了」+「点歌列表一直是空的」。
        //
        // 根因：`playing` 是**混合序列**（点歌 + 空闲歌单），但沿序列前进时
        // 来源标记沿用了上一次的快照，于是"从空闲歌单走到下一首空闲歌"
        // 被标成点歌来源 → `current_is_idle` 变 false：
        //  - 「立即切歌」策略永远不触发（用户调成等待也没用，因为压根没走到那条分支）；
        //  - 空闲书签回归失效。
        // 现在改为**按条目是否属于空闲歌单**重新推导。
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let controller = controller(&state, &backend, "idle-source");
        state.mutate(|s| {
            // 空闲歌单里放两首（id 由 idle 条目决定）
            s.idle.push(resolved_item("空闲甲"));
            s.idle.push(resolved_item("空闲乙"));
        });

        // 播空闲第一首 → 来源应为空闲
        let first = controller.play_idle_next().await.unwrap().unwrap();
        assert!(state.read().current_is_idle, "播放空闲歌单的歌应标记为空闲来源");

        // 沿序列前进到下一首（此时会先取新的空闲歌，走 ③ 分支）
        let second = controller.skip().await.unwrap().unwrap();
        assert_ne!(second.id, first.id, "「下一首」应换歌");
        assert!(
            state.read().current_is_idle,
            "沿序列前进到空闲歌单的曲目后，current_is_idle 仍应为 true（实际为 false）"
        );
    }

    #[tokio::test]
    async fn immediately_lets_requested_song_cut_in_when_policy_is_immediate() {
        // 与上一条配套：来源标记正确后，「立即切换」才可能生效。
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let mut rules = crate::config::RequestRules::default();
        rules.idle_switch_policy = crate::models::IdleSwitchPolicy::Immediate;
        let controller = controller_with_rules(&state, &backend, "immediate-cut", rules);
        state.mutate(|s| s.idle.push(resolved_item("空闲曲")));

        controller.play_idle_next().await.unwrap().unwrap();
        assert!(state.read().current_is_idle);

        state.mutate(|s| s.queue.push(resolved_item("点歌曲")));
        let switched = controller.on_song_requested().await;
        assert!(switched, "策略为「立即切换」且正在播空闲歌时应打断");
        assert_eq!(
            state.read().current.as_ref().map(|i| i.song.title.clone()),
            Some("点歌曲".to_string())
        );
    }

    #[tokio::test]
    async fn after_current_policy_does_not_cut_in() {
        // 策略为「播完再切」时，点歌只入队，不打断当前空闲歌曲。
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let mut rules = crate::config::RequestRules::default();
        rules.idle_switch_policy = crate::models::IdleSwitchPolicy::AfterCurrent;
        let controller = controller_with_rules(&state, &backend, "after-current", rules);
        state.mutate(|s| s.idle.push(resolved_item("空闲曲")));

        controller.play_idle_next().await.unwrap().unwrap();
        state.mutate(|s| s.queue.push(resolved_item("点歌曲")));

        let switched = controller.on_song_requested().await;
        assert!(!switched, "「播完再切」不应打断当前空闲歌曲");
        assert_eq!(
            state.read().current.as_ref().map(|i| i.song.title.clone()),
            Some("空闲曲".to_string()),
            "当前仍应是空闲歌曲"
        );
        assert_eq!(state.read().queue.len(), 1, "点歌应留在队列里等待");
    }

    // ── 用户报告的确切场景（阶段 10d 回归）──────────────────────────────

    #[tokio::test]
    async fn user_scenario_next_next_prev_next_returns_to_third() {
        // 用户原话：下一首两次播到第三首，点一次上一首回到第二首，再点下一首
        // 播放的却是第四首。这里逐步断言，锁死修复后的行为：必须是第三首。
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let controller = controller(&state, &backend, "user-scenario");
        for title in ["第一首", "第二首", "第三首", "第四首", "第五首"] {
            state.mutate(|s| s.queue.push(resolved_item(title)));
        }

        // ① 播放第一首
        let first = controller.start_next().await.unwrap().unwrap();
        assert_eq!(first.song.title, "第一首");

        // ② 下一首 → 第二首
        let second = controller.skip().await.unwrap().unwrap();
        assert_eq!(second.song.title, "第二首");

        // ③ 下一首 → 第三首
        let third = controller.skip().await.unwrap().unwrap();
        assert_eq!(third.song.title, "第三首");

        // ④ 上一首 → 回到第二首
        let back = controller.previous().await.unwrap().unwrap();
        assert_eq!(back.song.title, "第二首");

        // ⑤ 下一首必须回到第三首（而不是第四首）
        let fwd = controller.skip().await.unwrap().unwrap();
        assert_eq!(
            fwd.song.title, "第三首",
            "回到第二首后点下一首应回到第三首，不能跳到第四首"
        );

        let fourth = controller.skip().await.unwrap().unwrap();
        assert_eq!(fourth.song.title, "第四首");
    }

    #[tokio::test]
    async fn new_request_during_backtrack_does_not_change_next() {
        // 回到第二首之后队列里又插进来一首新歌，「下一首」仍必须是第三首
        // （新歌追加到末尾、不移动游标）。
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let controller = controller(&state, &backend, "backtrack-new");
        for title in ["A", "B", "C"] {
            state.mutate(|s| s.queue.push(resolved_item(title)));
        }
        controller.start_next().await.unwrap(); // A
        controller.skip().await.unwrap(); // B
        controller.skip().await.unwrap(); // C
        controller.previous().await.unwrap(); // 回到 B

        // 新歌进队（只影响 queue，不该影响已生成的播放序列）
        state.mutate(|s| s.queue.push(resolved_item("新来的")));

        let next = controller.skip().await.unwrap().unwrap();
        assert_eq!(next.song.title, "C", "新歌入队不应改变已回退位置的前进目标");
    }

    #[tokio::test]
    async fn playing_sequence_has_no_duplicates() {
        // 曾经出现过同一条目被追加两次（`playing` 里两条同名），
        // 导致「下一首」停在原地不动。这里锁死"不会重复追加"。
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let controller = controller(&state, &backend, "dedup");
        for title in ["A", "B"] {
            state.mutate(|s| s.queue.push(resolved_item(title)));
        }
        controller.start_next().await.unwrap();
        controller.skip().await.unwrap();
        controller.skip().await.unwrap(); // 末尾再点一次

        let guard = state.read();
        let mut titles: Vec<_> = guard
            .playing
            .iter()
            .map(|i| i.song.title.clone())
            .collect();
        let before = titles.len();
        titles.sort();
        titles.dedup();
        assert_eq!(titles.len(), before, "播放序列里不应出现重复条目");
    }

    #[tokio::test]
    async fn idle_resumes_the_same_song_from_the_start_after_requests() {
        // 空闲歌单在播第 2 首 → 有人点歌打断 → 点歌播完必须回到第 2 首重头播，
        // 而不是跳到第 3 首。
        let state = state();
        let backend = Arc::new(MockPlayer::new());
        let controller = controller(&state, &backend, "idle-resume");
        state.mutate(|s| {
            s.idle.push(resolved_item("空闲1"));
            s.idle.push(resolved_item("空闲2"));
            s.idle.push(resolved_item("空闲3"));
        });
        // 直接播空闲第 2 首
        let second = controller.play_idle_index(1).await.unwrap().unwrap();
        assert_eq!(second.song.title, "空闲2");

        // 有人点歌打断
        state.mutate(|s| s.queue.push(resolved_item("点的歌")));
        controller.on_song_requested().await;

        // 点歌播完后应回到「空闲2」重头播
        let resumed = controller.skip().await.unwrap().unwrap();
        assert_eq!(
            resumed.song.title, "空闲2",
            "点歌队列空后应回到被打断的那首空闲曲重头播"
        );
    }
}
