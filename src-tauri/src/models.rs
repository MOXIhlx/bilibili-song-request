//! 前后端共享的数据模型。
//!
//! ⚠️ 与 `src/types.ts` 必须逐字段保持一致。serde 默认输出 snake_case 字段名与
//! 外部标记（externally tagged）枚举，前端 TS 类型即按此约定书写。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 音乐平台。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MusicPlatform {
    /// 网易云音乐
    Netease,
    /// QQ 音乐
    Qq,
}

impl MusicPlatform {
    /// 用于日志与界面展示的名称。
    pub fn display_name(self) -> &'static str {
        match self {
            MusicPlatform::Netease => "网易云音乐",
            MusicPlatform::Qq => "QQ音乐",
        }
    }
}

/// 队列条目状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QueueStatus {
    /// 排队中
    Pending,
    /// 正在播放
    Playing,
    /// 已播放完毕
    Played,
    /// 被跳过
    Skipped,
}

/// 播放模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlayMode {
    /// 顺序播放
    Sequential,
    /// 随机播放
    Random,
    /// 单曲循环
    RepeatOne,
}

impl Default for PlayMode {
    fn default() -> Self {
        PlayMode::Sequential
    }
}

/// 歌曲元信息的来源状态（阶段 5）。
///
/// 点歌是「先入队、后解析」的两段式：弹幕只给出歌名/歌手，
/// 真实曲目信息要等音乐平台搜索返回，因此需要把中间状态暴露给界面。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SongSource {
    /// 尚未搜索（刚由弹幕入队）。
    Pending,
    /// 已由音乐平台解析出真实曲目。
    Resolved,
    /// 搜索失败（无结果/网络问题），保留用户输入的文本。
    Failed,
}

impl Default for SongSource {
    fn default() -> Self {
        SongSource::Pending
    }
}

/// 一首歌的元信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Song {
    /// 平台内的歌曲 ID（网易云为数字 ID 的字符串形式）。
    pub id: String,
    pub title: String,
    pub artist: String,
    pub platform: MusicPlatform,
    /// 时长（秒），未知为 0。
    #[serde(default)]
    pub duration: u64,
    /// 封面地址，可能为空。
    #[serde(default)]
    pub cover_url: Option<String>,
    /// 专辑名（阶段 5）。
    #[serde(default)]
    pub album: Option<String>,
    /// 元信息来源状态（阶段 5）。
    #[serde(default)]
    pub source: SongSource,
    /// 解析失败时的原因（阶段 5）。
    #[serde(default)]
    pub source_error: Option<String>,
}

impl Song {
    /// 创建一首「仅知标题/歌手、平台待定」的占位歌曲。
    ///
    /// 阶段 4 收到弹幕点歌时先入队，阶段 5 由音乐适配器补齐真实 ID 与时长。
    pub fn placeholder(title: impl Into<String>, artist: impl Into<String>) -> Self {
        Self {
            id: String::new(),
            title: title.into(),
            artist: artist.into(),
            platform: MusicPlatform::Netease,
            duration: 0,
            cover_url: None,
            album: None,
            source: SongSource::Pending,
            source_error: None,
        }
    }

    /// 是否已经解析出真实曲目。
    pub fn is_resolved(&self) -> bool {
        self.source == SongSource::Resolved && !self.id.is_empty()
    }

    /// 标记解析失败并记录原因。
    pub fn mark_failed(&mut self, reason: impl Into<String>) {
        self.source = SongSource::Failed;
        self.source_error = Some(reason.into());
    }

    /// 用于日志与界面的一行描述。
    pub fn display(&self) -> String {
        if self.artist.is_empty() {
            self.title.clone()
        } else {
            format!("{} - {}", self.title, self.artist)
        }
    }
}

/// 点歌优先级档位。
///
/// ## 为什么需要它
/// 主播自己在点歌机上加歌时应该**优先于观众弹幕**（正在直播的人更清楚要放什么），
/// 而观众弹幕点歌有数量上限（默认 7 首），避免队列被刷爆。
///
/// 插队规则：`Host` 插到**所有 `Danmaku` 之前**，
/// 但不会插到更早入队的 `Host` 之前（先点的先播，保持公平）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QueuePriority {
    /// 观众弹幕点歌（受上限约束）。
    #[default]
    Danmaku,
    /// 主播通过点歌机添加（不受上限约束，且优先播放）。
    Host,
}

impl QueuePriority {
    /// 是否是主播（高优先级）点歌。
    pub fn is_host(&self) -> bool {
        matches!(self, QueuePriority::Host)
    }
}

/// 点歌队列中的一项。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueItem {
    /// 队列项自身的唯一 ID（与歌曲 ID 无关）。
    pub id: Uuid,
    pub song: Song,
    /// 点歌人昵称。
    pub requested_by: String,
    /// 点歌人 UID（弹幕消息不一定携带，阶段 3 起填充）。
    #[serde(default)]
    pub requested_by_uid: Option<String>,
    pub requested_at: DateTime<Utc>,
    pub status: QueueStatus,
    /// 优先级档位（老数据默认按弹幕处理，保证向后兼容）。
    #[serde(default)]
    pub priority: QueuePriority,
}

impl QueueItem {
    /// 由弹幕点歌创建队列项。
    pub fn new(song: Song, requested_by: impl Into<String>, uid: Option<String>) -> Self {
        Self::with_priority(song, requested_by, uid, QueuePriority::Danmaku)
    }

    /// 按指定优先级创建队列项。
    pub fn with_priority(
        song: Song,
        requested_by: impl Into<String>,
        uid: Option<String>,
        priority: QueuePriority,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            song,
            requested_by: requested_by.into(),
            requested_by_uid: uid,
            requested_at: Utc::now(),
            status: QueueStatus::Pending,
            priority,
        }
    }
}

/// mpv 播放状态快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerState {
    pub playing: bool,
    pub paused: bool,
    /// 当前播放位置（秒）。
    pub position: f64,
    /// 当前歌曲总时长（秒）。
    pub duration: f64,
    /// 音量 0-100。
    pub volume: u8,
    /// 当前歌词（LRC 原文）。
    #[serde(default)]
    pub lyrics: Option<String>,
    /// 当前正在唱第几行（阶段 7）；无歌词或未开始时为 `None`。
    #[serde(default)]
    pub lyric_index: Option<usize>,
}

impl Default for PlayerState {
    fn default() -> Self {
        Self {
            playing: false,
            paused: false,
            position: 0.0,
            duration: 0.0,
            volume: 80,
            lyrics: None,
            lyric_index: None,
        }
    }
}

/// B 站弹幕连接状态。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BilibiliState {
    pub connected: bool,
    /// 直播间 ID（身份码模式由 start 接口返回，阶段 3 填充）。
    #[serde(default)]
    pub room_id: Option<String>,
    /// 最近一次错误，用于界面提示。
    #[serde(default)]
    pub last_error: Option<String>,
    /// 连接进度阶段（见 [`BilibiliPhase`]）。
    ///
    /// 为什么需要它：点「保存并连接」后真正的建连在后台异步进行，
    /// HTTP 会立刻返回。没有阶段信息时界面只能一直显示「未连接」，
    /// 用户无法判断「正在连」还是「已经失败」。
    #[serde(default)]
    pub phase: BilibiliPhase,
    /// 阶段的可读说明（中文，直接展示给用户）。
    #[serde(default)]
    pub detail: Option<String>,
    /// 已尝试次数（含重连），用于让用户知道程序还在努力。
    #[serde(default)]
    pub attempts: u32,
    /// 最近一次状态变化时间（RFC3339）。
    #[serde(default)]
    pub updated_at: Option<String>,
    /// 连接代号：只增不减，用于让**已放弃的旧连接任务**不再回写状态。
    #[serde(default)]
    pub epoch: ConnectionEpoch,
}

/// 弹幕连接进度阶段。
///
/// 与 [`crate::bilibili::DanmakuStatus`] 的区别：`DanmakuStatus` 是连接器内部的
/// 状态机（idle/connecting/connected/reconnecting/stopped），
/// 而这里的阶段是**给用户看的进度条**，粒度更细（例如「正在申请身份码会话」
/// 与「正在连接弹幕服务器」都对应 connecting，但用户看到的信息完全不同）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BilibiliPhase {
    /// 未开始
    #[default]
    Idle,
    /// 正在申请身份码会话（POST v2/app/start）
    RequestingSession,
    /// 正在连接弹幕服务器并鉴权
    ConnectingSocket,
    /// 已连接，正在接收弹幕
    Connected,
    /// 断线退避中，稍后自动重连
    Retrying,
    /// 已停止（用户主动断开）
    Stopped,
    /// 失败（配置/凭据错误等需要用户处理的情况）
    Failed,
}

/// 一条点歌请求的处理记录（阶段 4）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestRecord {
    /// 点歌人昵称。
    pub user: String,
    /// 点歌人 UID。
    #[serde(default)]
    pub uid: Option<String>,
    /// 歌名。
    pub title: String,
    /// 歌手。
    #[serde(default)]
    pub artist: String,
    /// 结果：`queued` 或 `rejected`。
    pub outcome: String,
    /// 被拒绝时的分类（cooldown / duplicate / queue_full / …），成功时为空。
    #[serde(default)]
    pub reason: Option<String>,
    /// 被拒绝时的可读说明。
    #[serde(default)]
    pub message: Option<String>,
    /// 入队位置（成功时）。
    #[serde(default)]
    pub position: Option<usize>,
    /// 发生时间。
    pub at: DateTime<Utc>,
}

impl RequestRecord {
    /// 记录一次成功入队。
    pub fn queued(
        user: impl Into<String>,
        uid: Option<String>,
        request: &crate::queue::SongRequest,
        position: usize,
    ) -> Self {
        Self {
            user: user.into(),
            uid,
            title: request.title.clone(),
            artist: request.artist.clone(),
            outcome: "queued".to_string(),
            reason: None,
            message: None,
            position: Some(position),
            at: Utc::now(),
        }
    }

    /// 记录一次被拒绝的请求。
    pub fn rejected(
        user: impl Into<String>,
        uid: Option<String>,
        request: &crate::queue::SongRequest,
        reason: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            user: user.into(),
            uid,
            title: request.title.clone(),
            artist: request.artist.clone(),
            outcome: "rejected".to_string(),
            reason: Some(reason.into()),
            message: Some(message.into()),
            position: None,
            at: Utc::now(),
        }
    }

    /// 是否成功入队。
    pub fn is_queued(&self) -> bool {
        self.outcome == "queued"
    }
}

/// 冷却中的用户（阶段 4）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CooldownView {
    /// 用户标识（UID 或 `name:昵称`）。
    pub key: String,
    /// 剩余冷却秒数。
    pub remaining_secs: u64,
}

/// 点歌统计信息（阶段 4），供控制台展示。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestStats {
    /// 当前处于冷却中的用户。
    #[serde(default)]
    pub cooldowns: Vec<CooldownView>,
    /// 最近的请求记录（最新的在前，最多 50 条）。
    #[serde(default)]
    pub recent: Vec<RequestRecord>,
}

impl RequestStats {
    /// 最近记录里成功入队的条数。
    pub fn queued_count(&self) -> usize {
        self.recent.iter().filter(|r| r.is_queued()).count()
    }

    /// 最近记录里被拒绝的条数。
    pub fn rejected_count(&self) -> usize {
        self.recent.iter().filter(|r| !r.is_queued()).count()
    }
}

/// 后端全量状态，对应 `GET /api/state` 与 WS 的 `state` 消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppState {
    /// 应用版本号。
    pub version: String,
    /// 服务器启动时间，前端用于展示运行时长。
    pub started_at: DateTime<Utc>,
    pub play_mode: PlayMode,
    /// 点歌队列（待播的**新歌**，会被消费）。
    ///
    /// 这是「还没进入播放序列」的点歌请求。真正决定播放顺序的是
    /// [`Self::playing`]；本列表只回答"下一首新歌该从哪取"。
    pub queue: Vec<QueueItem>,
    #[serde(default)]
    pub current: Option<QueueItem>,
    pub player: PlayerState,
    pub bilibili: BilibiliState,
    /// 点歌统计（阶段 4）。
    #[serde(default)]
    pub stats: RequestStats,
    /// **实际播放序列**：按播放顺序排列，「上一首 / 下一首」都在这条上移动 `cursor`。
    ///
    /// ```text
    ///     已播区          当前        待播区
    /// ┌──┬──┬──┬────────┬──┬──┬──┐
    /// │0 │1 │2 │   3    │4 │5 │6 │
    /// └──┴──┴──┴────────┴──┴──┴──┘
    ///              ↑ cursor
    /// ```
    ///
    /// 新歌只**追加**到末尾、不移动 `cursor`，因此「回到第 2 首再点下一首」
    /// 必然回到第 3 首。（早期上一首走 `history`、下一首走 `queue`，
    /// 两条列表互不相干，才会跳错。）
    #[serde(default)]
    pub playing: Vec<QueueItem>,
    /// 当前正在播放在 [`Self::playing`] 中的下标。
    #[serde(default)]
    pub cursor: usize,
    /// 弹幕可用的名额上限（初始 `rules.max_queue`，只增不减）。
    ///
    /// 每播完一首主播点歌就补充 `rules.host_extra_per_play` 个。
    /// 这里只存上限，「已用多少」由 [`crate::queue::danmaku_pending`] 实时数出。
    #[serde(default)]
    pub danmaku_slots: usize,
    /// 累计播过多少首主播点歌，用于给 `danmaku_slots` 设增长上界。
    #[serde(default)]
    pub host_songs_played: usize,
    /// 空闲歌单：点歌队列为空时自动从这里出歌，有人点歌就切回点歌队列。
    ///
    /// 与 `queue` 分开存：它是主播自己的曲库，不受弹幕名额限制。
    #[serde(default)]
    pub idle: Vec<QueueItem>,
    /// 空闲歌单的播放模式（与点歌队列的 `play_mode` 独立）。
    #[serde(default)]
    pub idle_mode: IdleMode,
    /// 空闲歌单**书签**：正在播（或最近播过）的那一首的下标。
    ///
    /// 被点歌打断后，点歌播完会回到这首重头播。
    /// 与 [`Self::idle_next`] 语义差 1（书签是"正在播"、`idle_next` 是"下次取"），
    /// 混用会表现为"回来时跳过一首"。
    #[serde(default)]
    pub idle_current: Option<usize>,
    /// 空闲歌单下一次取歌的下标。
    #[serde(default)]
    pub idle_next: usize,
    /// 空闲歌单是否**被点歌打断**、等待点歌队列播完后回去重头播。
    ///
    /// 不能改用 `!current_is_idle` 判断：恢复播放时该值会被重置为 false，
    /// 会把"正常推进"误判成"被打断"，表现为一直重播同一首。
    #[serde(default)]
    pub idle_pending_return: bool,
    /// 当前正在播放的是不是空闲歌单的歌（决定有人点歌时要不要立刻让位）。
    #[serde(default)]
    pub current_is_idle: bool,
}

/// B 站弹幕连接代号（避免被"已放弃的旧任务"回写状态）。
///
/// 每次 `connect()` 都会 +1；客户端拿着创建时的代号，写状态前先比对——
/// 不是当前代号就直接丢弃。没有这道闸门时会出现：
/// 主动断开会等旧任务退出，等不到（超时 3 秒）就放弃等待并开始新连接，
/// 但旧任务随后退出时**又写了一次「已停止」**，把新连接的「已连接」覆盖掉，
/// 界面表现为「刚显示已连接，过一会儿自己断了」。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionEpoch(pub u64);

impl ConnectionEpoch {
    /// 下一个代号。
    pub fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

/// 空闲歌单的播放模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IdleMode {
    /// 顺序播放：放完最后一首就停。
    #[default]
    Sequential,
    /// 列表循环：放完最后一首回到第一首。
    LoopAll,
    /// 单曲循环：一直重播当前这首。
    LoopOne,
    /// 随机播放：每次随机取一首（且尽量不重复上一首）。
    Shuffle,
}

impl IdleMode {
    /// 面向界面的名称。
    pub fn label(&self) -> &'static str {
        match self {
            IdleMode::Sequential => "顺序播放",
            IdleMode::LoopAll => "列表循环",
            IdleMode::LoopOne => "单曲循环",
            IdleMode::Shuffle => "随机播放",
        }
    }

    /// 全部取值（界面下拉用，避免前后端枚举顺序不一致）。
    pub const ALL: [IdleMode; 4] = [
        IdleMode::Sequential,
        IdleMode::LoopAll,
        IdleMode::LoopOne,
        IdleMode::Shuffle,
    ];
}

/// 有人点歌时，正在播放的空闲歌曲要不要让位。
///
/// 需求原话："给一个选项，选择 ①当有人点歌时，立即播放点的歌曲，
/// 选择② 即使有人点歌 也把当前正在播放的这一首空闲歌曲播放完毕后再播放点歌歌曲"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IdleSwitchPolicy {
    /// 立即切到点歌（空闲歌曲被中断）。
    #[default]
    Immediate,
    /// 放完当前这首空闲歌曲再切。
    AfterCurrent,
}

impl IdleSwitchPolicy {
    /// 面向界面的名称。
    pub fn label(&self) -> &'static str {
        match self {
            IdleSwitchPolicy::Immediate => "立即播放点的歌曲",
            IdleSwitchPolicy::AfterCurrent => "放完当前这首再切",
        }
    }
}

impl AppState {
    /// 构造初始状态。
    pub fn new(version: impl Into<String>, player: PlayerState, bilibili: BilibiliState) -> Self {
        Self {
            version: version.into(),
            started_at: Utc::now(),
            play_mode: PlayMode::default(),
            queue: Vec::new(),
            current: None,
            player,
            bilibili,
            stats: RequestStats::default(),
            playing: Vec::new(),
            cursor: 0,
            danmaku_slots: 0,
            host_songs_played: 0,
            idle: Vec::new(),
            idle_mode: IdleMode::default(),
            idle_current: None,
            idle_next: 0,
            idle_pending_return: false,
            current_is_idle: false,
        }
    }
}

/// 一条弹幕。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Danmaku {
    /// 发送者昵称。
    pub user: String,
    /// 发送者 UID（可能未知）。
    #[serde(default)]
    pub uid: Option<String>,
    /// 弹幕文本。
    pub text: String,
    /// 收到时间。
    pub at: DateTime<Utc>,
    /// 粉丝牌等级（预留，阶段 3 起填充）。
    #[serde(default)]
    pub fans_medal_level: u32,
    /// 用户等级（预留）。
    #[serde(default)]
    pub user_level: u32,
    /// 是否来自**主播的点歌机**（而不是观众弹幕）。
    ///
    /// 置位后点歌处理会按 [`QueuePriority::Host`] 处理：
    /// 无视冷却、弹幕名额、重复限制与黑名单，并插到所有弹幕之前。
    ///
    /// 为什么放在弹幕上而不是另开一条事件类型：
    /// 注入的弹幕仍要经过同一条广播链路（面板要显示、日志要记录），
    /// 若另开类型就要在每个订阅方各加一个分支，容易漏。
    #[serde(default)]
    pub from_host: bool,
}
