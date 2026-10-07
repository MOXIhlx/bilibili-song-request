//! 点歌请求处理：把弹幕指令变成队列条目。
//!
//! 这是阶段 4 的核心，串起「弹幕 → 解析 → 规则 → 队列」：
//!
//! ```text
//! Danmaku ──▶ SongRequestParser（正则）──▶ 规则校验 ──▶ queue::push ──▶ 广播
//!                                              │
//!                                              └─▶ 拒绝理由（去重/冷却/上限/等级）
//! ```
//!
//! ## 规则说明
//! | 规则 | 配置项 | 行为 |
//! |------|--------|------|
//! | 指令正则 | `rules.command_regex` | 不匹配的弹幕直接忽略（不记录、不广播） |
//! | 冷却 | `rules.cooldown_secs` | 同一用户（按 UID，缺失时按昵称）在冷却期内不能再点 |
//! | 队列上限 | `rules.max_queue` | 0 表示不限；满则拒绝（**正在播放的歌不计入**） |
//! | 重复点歌 | `rules.allow_duplicate` | 关闭时，队内同名同歌手（或同名）会被拒绝 |
//! | 粉丝牌等级 | `rules.min_fans_medal_level` | 0 = 不限 |
//! | 用户等级 | `rules.min_user_level` | 0 = 不限 |
//!
//! ## 为什么冷却表不进 `AppState`
//! 冷却表是「实现细节」，序列化给前端没有意义；但为了让主播能看到谁在冷却中，
//! 这里额外把它投影成可序列化的 [`CooldownView`] 列表，放进 `AppState.stats`。

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::event::HubEvent;
use crate::models::{CooldownView, Danmaku, QueueItem, QueuePriority, RequestRecord, Song};
use crate::music::ResolveSender;
use crate::queue::{self, SongRequestParser};
use crate::state::StateCell;

use super::command::SongRequest;

/// 一条点歌请求的处理结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnqueueOutcome {
    /// 已入队。
    Queued {
        /// 队列条目 ID（阶段 5：用于追踪曲目解析结果）。
        item_id: uuid::Uuid,
        /// 队内位置（从 1 开始）。
        position: usize,
        /// 歌名。
        title: String,
    },
    /// 被规则拒绝。
    Rejected(RejectReason),
    /// 不是点歌指令（普通弹幕），无需任何处理。
    NotARequest,
}

/// 拒绝原因（每种都带可读说明，直接展示给主播/观众）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    /// 同一用户冷却中。
    Cooldown {
        /// 还需等待的秒数。
        remaining_secs: i64,
    },
    /// 队列已满。
    QueueFull {
        /// 当前队列上限。
        max: usize,
    },
    /// 队内已有同一首歌。
    Duplicate {
        /// 重复的歌名。
        title: String,
    },
    /// 粉丝牌等级不足。
    FansMedalTooLow {
        /// 当前等级。
        current: u32,
        /// 要求等级。
        required: u32,
    },
    /// 用户等级不足。
    UserLevelTooLow {
        /// 当前等级。
        current: u32,
        /// 要求等级。
        required: u32,
    },
    /// 这首歌被主播拉黑了（不搜索、不入队）。
    Blacklisted {
        /// 黑名单里的歌名。
        title: String,
        /// 黑名单里的歌手（可能为空 = 拉黑所有版本）。
        artist: String,
    },
}

impl RejectReason {
    /// 面向弹幕/日志的可读文本。
    pub fn message(&self) -> String {
        match self {
            RejectReason::Cooldown { remaining_secs } => {
                format!("冷却中，请 {remaining_secs} 秒后再点")
            }
            RejectReason::QueueFull { max } => format!("队列已满（{max} 首）"),
            RejectReason::Duplicate { title } => format!("《{title}》已在队列中"),
            RejectReason::FansMedalTooLow { current, required } => {
                format!("粉丝牌等级不足（{current}/{required}）")
            }
            RejectReason::UserLevelTooLow { current, required } => {
                format!("用户等级不足（{current}/{required}）")
            }
            RejectReason::Blacklisted { title, artist } => {
                if artist.trim().is_empty() {
                    format!("《{title}》已被拉黑")
                } else {
                    format!("《{title} - {artist}》已被拉黑")
                }
            }
        }
    }

    /// 机器可读的分类名，前端用于上色/图标。
    pub fn kind(&self) -> &'static str {
        match self {
            RejectReason::Cooldown { .. } => "cooldown",
            RejectReason::QueueFull { .. } => "queue_full",
            RejectReason::Duplicate { .. } => "duplicate",
            RejectReason::FansMedalTooLow { .. } => "fans_medal",
            RejectReason::UserLevelTooLow { .. } => "user_level",
            RejectReason::Blacklisted { .. } => "blacklisted",
        }
    }
}

/// 冷却记录表（内存态，不持久化）。
///
/// 之所以用 `std::sync::Mutex`：临界区只有几次哈希操作，没有 await，
/// 用 tokio 的异步锁反而会引入不必要的调度开销。
#[derive(Default)]
pub struct CooldownTracker {
    entries: Mutex<HashMap<String, DateTime<Utc>>>,
}

impl CooldownTracker {
    /// 创建空表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 用户的稳定标识：优先 UID，缺失时退回昵称。
    pub fn key_of(danmaku: &Danmaku) -> String {
        danmaku
            .uid
            .clone()
            .filter(|uid| !uid.is_empty())
            .unwrap_or_else(|| format!("name:{}", danmaku.user))
    }

    /// 查询剩余冷却秒数；`Ok(())` 表示可以点歌。
    pub fn check(&self, key: &str, cooldown_secs: u64) -> Result<(), i64> {
        if cooldown_secs == 0 {
            return Ok(());
        }
        let guard = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let Some(last) = guard.get(key) else {
            return Ok(());
        };
        let elapsed = (Utc::now() - *last).num_seconds().max(0) as u64;
        if elapsed >= cooldown_secs {
            Ok(())
        } else {
            Err((cooldown_secs - elapsed) as i64)
        }
    }

    /// 记录一次成功点歌。
    pub fn touch(&self, key: &str) {
        let mut guard = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        guard.insert(key.to_string(), Utc::now());
    }

    /// 清理已经过期的记录，避免长期运行时无界增长。
    pub fn prune(&self, cooldown_secs: u64) {
        let mut guard = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let now = Utc::now();
        guard.retain(|_, at| {
            let elapsed = (now - *at).num_seconds().max(0) as u64;
            elapsed < cooldown_secs
        });
    }

    /// 导出给前端展示的冷却列表（按剩余时间升序）。
    pub fn view(&self, cooldown_secs: u64) -> Vec<CooldownView> {
        if cooldown_secs == 0 {
            return Vec::new();
        }
        let guard = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let now = Utc::now();
        let mut list: Vec<CooldownView> = guard
            .iter()
            .filter_map(|(key, at)| {
                let elapsed = (now - *at).num_seconds().max(0) as u64;
                let remaining = cooldown_secs.saturating_sub(elapsed);
                if remaining == 0 {
                    return None;
                }
                Some(CooldownView {
                    key: key.clone(),
                    remaining_secs: remaining,
                })
            })
            .collect();
        list.sort_by_key(|c| c.remaining_secs);
        list
    }

    /// 当前记录条数。
    pub fn len(&self) -> usize {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 判断队内是否已有同一首歌。
///
/// 匹配规则：标题必须一致（忽略大小写与首尾空白）；
/// 若新请求带了歌手，则要求队内条的歌手也一致；否则只比标题。
pub fn is_duplicate(queue_items: &[QueueItem], request: &SongRequest) -> bool {
    let want_title = request.title.trim().to_lowercase();
    let want_artist = request.artist.trim().to_lowercase();
    queue_items.iter().any(|item| {
        let title = item.song.title.trim().to_lowercase();
        if title != want_title {
            return false;
        }
        if want_artist.is_empty() {
            return true;
        }
        item.song.artist.trim().to_lowercase() == want_artist
    })
}

/// 点歌请求服务。
pub struct SongRequestService {
    /// 指令解析器（来自 `rules.command_regex`）。
    ///
    /// 用 `RwLock` 包裹是为了支持**热更新**：`PUT /api/config` 改了正则后，
    /// 下一秒的弹幕就应该按新规则解析，不需要重启进程。
    parser: std::sync::RwLock<SongRequestParser>,
    /// 全局状态。
    state: StateCell,
    /// 配置快照提供者（每次处理时读取，便于热更新规则）。
    config: Box<dyn Fn() -> Config + Send + Sync>,
    /// 冷却表。
    cooldowns: CooldownTracker,
    /// 最近请求记录（环形缓冲，最多 [`MAX_RECENT`] 条）。
    recent: Mutex<Vec<RequestRecord>>,
    /// 点歌日志的落盘路径（JSONL）。`None` = 不落盘（测试用）。
    ///
    /// ## 为什么单独用一个文件
    /// 需求原话「点歌日志也要一直记录着」。日志是**只追加**的流水，
    /// 和 `queue.json`（整体重写的状态快照）语义不同：
    /// 用 JSONL 追加写，崩溃时最多丢最后一行，不会把整份日志写坏。
    log_path: Option<std::path::PathBuf>,
    /// 曲目解析任务投递句柄（阶段 5）。未接线时为 `None`，此时保留占位歌曲。
    resolver: Option<ResolveSender>,
}

/// 远端保留的请求记录上限。
///
/// 需求是「记录**所有**点歌日志」，所以这个值取得比较大：
/// 一条记录只有几十字节，2000 条也不过百来 KB，
/// 但足够主播回看一整场直播的点歌情况。
pub const MAX_RECENT: usize = 2000;

impl SongRequestService {
    /// 用解析器、状态与配置来源创建服务。
    ///
    /// 解析器在启动时编译一次；若配置里的正则被改坏，调用方应回退到默认解析器
    /// （见 [`SongRequestService::from_config`]）。
    pub fn new<F>(parser: SongRequestParser, state: StateCell, config: F) -> Self
    where
        F: Fn() -> Config + Send + Sync + 'static,
    {
        Self {
            parser: std::sync::RwLock::new(parser),
            state,
            config: Box::new(config),
            cooldowns: CooldownTracker::new(),
            recent: Mutex::new(Vec::new()),
            log_path: None,
            resolver: None,
        }
    }

    /// 指定点歌日志的落盘路径，并从磁盘读回已有日志（链式调用）。
    ///
    /// ⚠️ 读完必须立刻 `sync_stats()`：否则 `AppState.stats.recent` 仍是空的，
    /// 界面首屏「最近点歌」什么都不显示——要等到**下一次有人点歌**
    /// （那时才触发同步）才把历史记录一起刷出来。用户看到的就是
    /// 「必须新增一条才能刷新出之前的记录」。
    pub fn with_log_path(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        let path = path.into();
        self.log_path = Some(path.clone());
        self.load_log(&path);
        self.sync_stats();
        self
    }

    /// 从 JSONL 文件读回历史日志。文件不存在或个别行损坏都只警告，不影响启动。
    ///
    /// 文件按时间正序追加（旧的在前），内存里要求最新在前，因此读完要反转。
    fn load_log(&self, path: &std::path::Path) {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                debug!(path = %path.display(), "点歌日志尚不存在，从空日志开始");
                return;
            }
            Err(err) => {
                warn!(error = %err, path = %path.display(), "点歌日志读取失败，按空日志开始");
                return;
            }
        };

        let mut records: Vec<RequestRecord> = Vec::new();
        let mut broken = 0usize;
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<RequestRecord>(line) {
                Ok(record) => records.push(record),
                // 单行损坏（例如上次写到一半断电）不应让整份日志报废
                Err(_) => broken += 1,
            }
        }
        // 文件是旧→新，内存要新→旧
        records.reverse();
        records.truncate(MAX_RECENT);
        let count = records.len();
        *self.recent.lock().unwrap_or_else(|e| e.into_inner()) = records;
        info!(count, broken, path = %path.display(), "已恢复历史点歌日志");
    }

    /// 把一条记录**追加**写入 JSONL 日志（失败只警告，不影响点歌本身）。
    fn append_log(&self, record: &RequestRecord) {
        let Some(path) = &self.log_path else {
            return;
        };
        if let Some(parent) = path.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                warn!(error = %err, "创建日志目录失败");
                return;
            }
        }
        let line = match serde_json::to_string(record) {
            Ok(line) => line,
            Err(err) => {
                warn!(error = %err, "点歌日志序列化失败");
                return;
            }
        };
        use std::io::Write;
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            Ok(mut file) => {
                if let Err(err) = writeln!(file, "{line}") {
                    warn!(error = %err, "点歌日志写入失败");
                }
            }
            Err(err) => warn!(error = %err, path = %path.display(), "打开点歌日志失败"),
        }
    }

    /// 接上曲目解析器（阶段 5）。
    ///
    /// 接线后，成功入队的占位歌曲会被投递给解析器，
    /// 由后台任务搜索音乐平台并回填真实曲目信息。
    pub fn with_resolver(mut self, resolver: ResolveSender) -> Self {
        self.resolver = Some(resolver);
        self
    }

    /// 提交一个解析任务（供手动加歌等非弹幕路径使用）。
    ///
    /// 返回 `false` 表示解析器不可用（此时歌曲会保留占位状态）。
    pub fn enqueue_resolution(&self, job: crate::music::ResolveJob) -> bool {
        match &self.resolver {
            Some(sender) => sender.send(job),
            None => false,
        }
    }

    /// 从配置构造：正则非法时回退到内置默认正则并记录警告。
    pub fn from_config<F>(state: StateCell, config: F) -> Self
    where
        F: Fn() -> Config + Send + Sync + 'static,
    {
        let rules = config().rules;
        let parser = match SongRequestParser::from_rules(&rules) {
            Ok(parser) => parser,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    regex = %rules.command_regex,
                    "配置里的点歌正则非法，回退到内置默认正则"
                );
                SongRequestParser::default_pattern()
            }
        };
        // 按配置初始化弹幕可用名额。
        //
        // 必须在这里做：`AppState` 的默认 `danmaku_slots` 是 0，
        // 不初始化就会把所有弹幕点歌都判成「队列已满」。
        queue::ensure_slots(&state, &rules);
        Self::new(parser, state, config)
    }

    /// 当前使用的正则（测试与界面展示用）。
    pub fn pattern(&self) -> String {
        self.parser
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .pattern()
    }

    /// 替换解析器（配置里正则变更后调用，立即生效）。
    pub fn set_parser(&self, parser: SongRequestParser) {
        *self.parser.write().unwrap_or_else(|e| e.into_inner()) = parser;
    }

    /// 处理一条弹幕（观众来源）。
    ///
    /// 只有真正匹配指令的弹幕才会产生副作用（冷却记录、入队、广播）。
    pub fn handle(&self, danmaku: &Danmaku) -> EnqueueOutcome {
        self.handle_with_priority(danmaku, QueuePriority::Danmaku)
    }

    /// 处理一条弹幕，并指定它的**来源优先级**。
    ///
    /// `QueuePriority::Host` 表示这条消息来自主播的点歌机
    /// （例如控制台的「链路自测」注入）——按需求它拥有最高权限：
    ///
    /// | 限制 | 观众弹幕 | 主播 |
    /// |------|---------|------|
    /// | 粉丝牌 / 等级门槛 | 受限制 | **无视** |
    /// | 点歌冷却 | 受限制 | **无视** |
    /// | 弹幕点歌名额 | 受限制 | **无视** |
    /// | 重复点歌 | 受限制 | **无视** |
    /// | 黑名单 | 受影响 | **无视** |
    /// | 插队位置 | 队尾 | **插到所有弹幕之前** |
    pub fn handle_with_priority(&self, danmaku: &Danmaku, priority: QueuePriority) -> EnqueueOutcome {
        let request = {
            let parser = self.parser.read().unwrap_or_else(|e| e.into_inner());
            parser.parse(&danmaku.text)
        };
        let Some(request) = request else {
            // 普通聊天弹幕：完全忽略，不占用队列也不写记录。
            return EnqueueOutcome::NotARequest;
        };

        let config = (self.config)();
        let rules = &config.rules;
        let is_host = priority == QueuePriority::Host;

        // 黑名单：直接不搜也不入队（按需求「直接就不搜索」）。
        // 主播可无视，用于自己点被拉黑的歌。
        if !is_host {
            let query =
                crate::queue::blacklist::BlacklistQuery::new(&request.title, &request.artist);
            if let Some(entry) = crate::queue::blacklist::find(&config.blacklist, &query) {
                return self.reject(
                    danmaku,
                    &request,
                    RejectReason::Blacklisted {
                        title: entry.title.clone(),
                        artist: entry.artist.clone(),
                    },
                );
            }
        }

        // ── 规则校验（主播全部跳过）────────────────────────────────────────
        if !is_host {
            if rules.min_fans_medal_level > 0 && danmaku.fans_medal_level < rules.min_fans_medal_level {
                return self.reject(
                    danmaku,
                    &request,
                    RejectReason::FansMedalTooLow {
                        current: danmaku.fans_medal_level,
                        required: rules.min_fans_medal_level,
                    },
                );
            }
            if rules.min_user_level > 0 && danmaku.user_level < rules.min_user_level {
                return self.reject(
                    danmaku,
                    &request,
                    RejectReason::UserLevelTooLow {
                        current: danmaku.user_level,
                        required: rules.min_user_level,
                    },
                );
            }
        }

        let key = CooldownTracker::key_of(danmaku);
        if !is_host {
            if let Err(remaining) = self.cooldowns.check(&key, rules.cooldown_secs) {
                return self.reject(danmaku, &request, RejectReason::Cooldown {
                    remaining_secs: remaining,
                });
            }
        }

        // 弹幕点歌的名额限制：只统计**弹幕来源**的待播曲目
        // （主播通过点歌机加的不占这个名额，见 `rules.max_queue` 的说明）。
        //
        // 名额不是固定的：每播完一首主播点歌会由 `queue::grant_host_bonus` 补充，
        // 所以「队列满了之后主播那几首播掉，弹幕还能再点」是自然成立的。
        if !is_host && rules.max_queue > 0 && !queue::danmaku_has_room(&self.state) {
            return self.reject(
                danmaku,
                &request,
                RejectReason::QueueFull {
                    max: rules.max_queue,
                },
            );
        }

        if !is_host && !rules.allow_duplicate {
            let queue_snapshot = self.state.read().queue.clone();
            if is_duplicate(&queue_snapshot, &request) {
                return self.reject(
                    danmaku,
                    &request,
                    RejectReason::Duplicate {
                        title: request.title.clone(),
                    },
                );
            }
        }

        // ── 入队 ────────────────────────────────────────────────────────────
        let song = Song::placeholder(request.title.clone(), request.artist.clone());
        let item = QueueItem::with_priority(
            song,
            danmaku.user.clone(),
            danmaku.uid.clone().filter(|uid| !uid.is_empty()),
            priority,
        );
        let item_id = item.id;
        queue::push(&self.state, item);
        queue::account_enqueue(&self.state, priority);

        if !is_host {
            self.cooldowns.touch(&key);
            self.cooldowns.prune(rules.cooldown_secs);
        }

        let position = self
            .state
            .read()
            .queue
            .iter()
            .position(|i| i.id == item_id)
            .map(|idx| idx + 1)
            .unwrap_or(0);

        info!(
            user = %danmaku.user,
            title = %request.title,
            artist = %request.artist,
            position,
            "点歌入队"
        );

        self.record(RequestRecord::queued(
            &danmaku.user,
            danmaku.uid.clone(),
            &request,
            position,
        ));
        self.state.publish(HubEvent::RequestQueued {
            user: danmaku.user.clone(),
            title: request.title.clone(),
            artist: request.artist.clone(),
            position,
        });
        self.sync_stats();

        // 阶段 5：把占位歌曲交给后台解析器搜索真实曲目。
        // 投递失败（解析器已退出）不影响入队结果，界面会保留「解析中」状态。
        if let Some(sender) = &self.resolver {
            let job = crate::music::ResolveJob::new(item_id, &request);
            if !sender.send(job) {
                debug!(title = %request.title, "曲目解析器不可用，保留占位歌曲");
            }
        }

        EnqueueOutcome::Queued {
            item_id,
            position,
            title: request.title,
        }
    }

    /// 统一处理拒绝路径：写日志、记状态、广播。
    fn reject(&self, danmaku: &Danmaku, request: &SongRequest, reason: RejectReason) -> EnqueueOutcome {
        debug!(
            user = %danmaku.user,
            title = %request.title,
            reason = %reason.message(),
            "点歌被拒绝"
        );
        self.record(RequestRecord::rejected(
            &danmaku.user,
            danmaku.uid.clone(),
            request,
            reason.kind(),
            reason.message(),
        ));
        self.state.publish(HubEvent::RequestRejected {
            user: danmaku.user.clone(),
            title: request.title.clone(),
            reason: reason.kind().to_string(),
            message: reason.message(),
        });
        self.sync_stats();
        EnqueueOutcome::Rejected(reason)
    }

    /// 追加一条请求记录（保留最近 [`MAX_RECENT`] 条，最新的在前）。
    ///
    /// 同时**追加落盘**——用户明确要求「点歌日志也要一直记录着」，
    /// 只在内存里的话重启就清零了。
    fn record(&self, record: RequestRecord) {
        self.append_log(&record);
        let mut guard = self.recent.lock().unwrap_or_else(|e| e.into_inner());
        guard.insert(0, record);
        guard.truncate(MAX_RECENT);
    }

    /// 最近请求记录快照。
    pub fn recent(&self) -> Vec<RequestRecord> {
        self.recent.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 把冷却与记录投影到 `AppState.stats`，让前端能展示。
    ///
    /// ⚠️ 只同步最近 [`MAX_RECENT_IN_STATE`] 条，**不是全部**。
    /// `AppState` 会在每次状态变化时通过 `/ws` 全量广播，
    /// 把 2000 条记录塞进去会让每条消息都变大几十倍。
    /// 完整的点歌日志走独立接口 `GET /api/requests/log`（前端按需拉取）。
    fn sync_stats(&self) {
        let cooldown_secs = (self.config)().rules.cooldown_secs;
        let cooldowns = self.cooldowns.view(cooldown_secs);
        let mut recent = self.recent();
        recent.truncate(MAX_RECENT_IN_STATE);
        self.state.mutate(|s| {
            s.stats.cooldowns = cooldowns;
            s.stats.recent = recent;
        });
    }
}

/// 随 `AppState` 广播的记录条数（界面顶部列表够用即可）。
pub const MAX_RECENT_IN_STATE: usize = 50;

/// 展示用的时间差（`刚刚` / `N 秒前` / `N 分钟前`）。
pub fn humanize_age(at: DateTime<Utc>) -> String {
    let secs = (Utc::now() - at).num_seconds().max(0);
    if secs < 5 {
        "刚刚".to_string()
    } else if secs < 60 {
        format!("{secs} 秒前")
    } else if secs < 3600 {
        format!("{} 分钟前", secs / 60)
    } else {
        let hours = ChronoDuration::seconds(secs).num_hours();
        format!("{hours} 小时前")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AppState, BilibiliState, PlayerState};
    use crate::VERSION;
    use chrono::Utc;
    use std::sync::Arc;

    fn state() -> StateCell {
        StateCell::new(AppState::new(
            VERSION,
            PlayerState::default(),
            BilibiliState::default(),
        ))
    }

    fn danmaku(user: &str, uid: Option<&str>, text: &str) -> Danmaku {
        Danmaku {
            user: user.to_string(),
            uid: uid.map(|u| u.to_string()),
            text: text.to_string(),
            at: Utc::now(),
            fans_medal_level: 0,
            user_level: 0,
            from_host: false,
        }
    }

    /// 主播来源的弹幕（走 `handle_with_priority` 时应无视各种限制）。
    fn host_danmaku(user: &str, uid: Option<&str>, text: &str) -> Danmaku {
        Danmaku {
            from_host: true,
            ..danmaku(user, uid, text)
        }
    }

    /// 用固定规则构造服务。
    fn service_with(rules: crate::config::RequestRules) -> (SongRequestService, StateCell) {
        let state = state();
        let mut config = Config::default();
        config.rules = rules;
        let config = Arc::new(config);
        let provider = {
            let config = Arc::clone(&config);
            move || (*config).clone()
        };
        let service = SongRequestService::from_config(state.clone(), provider);
        (service, state)
    }

    fn default_rules() -> crate::config::RequestRules {
        // 测试默认关掉冷却，避免用例之间互相影响
        crate::config::RequestRules {
            cooldown_secs: 0,
            ..crate::config::RequestRules::default()
        }
    }

    /// 用完整配置构造服务（黑名单用例需要）。
    fn service_with_config(config: Config) -> (SongRequestService, StateCell) {
        let state = state();
        let config = Arc::new(config);
        let provider = {
            let config = Arc::clone(&config);
            move || (*config).clone()
        };
        let service = SongRequestService::from_config(state.clone(), provider);
        (service, state)
    }

    // ── 黑名单（阶段 9）──────────────────────────────────────────────────

    #[test]
    fn blacklisted_song_is_rejected_for_danmaku() {
        let mut config = Config::default();
        config.rules = default_rules();
        config.blacklist = vec![crate::queue::blacklist::BlacklistEntry::new(
            "晴天", "周杰伦",
        )];
        let (service, state) = service_with_config(config);

        let outcome = service.handle(&danmaku("甲", Some("1"), "点歌 晴天 周杰伦"));
        match outcome {
            EnqueueOutcome::Rejected(RejectReason::Blacklisted { title, artist }) => {
                assert_eq!(title, "晴天");
                assert_eq!(artist, "周杰伦");
            }
            other => panic!("应因黑名单被拒，实际：{other:?}"),
        }
        assert!(state.read().queue.is_empty(), "被拉黑的歌不应入队");
    }

    #[test]
    fn blacklist_rejection_reason_is_readable() {
        let msg = RejectReason::Blacklisted {
            title: "晴天".into(),
            artist: "周杰伦".into(),
        }
        .message();
        assert!(msg.contains("晴天") && msg.contains("周杰伦"), "实际：{msg}");
        assert_eq!(RejectReason::Blacklisted { title: "a".into(), artist: "b".into() }.kind(), "blacklisted");
    }

    #[test]
    fn host_can_request_blacklisted_song() {
        // 需求："通过主播点歌机可以进行无视黑名单"
        let mut config = Config::default();
        config.rules = default_rules();
        config.blacklist = vec![crate::queue::blacklist::BlacklistEntry::new(
            "晴天", "周杰伦",
        )];
        let (service, state) = service_with_config(config);

        assert!(matches!(
            service.handle_with_priority(
                &host_danmaku("主播", Some("9"), "点歌 晴天 周杰伦"),
                QueuePriority::Host
            ),
            EnqueueOutcome::Queued { .. }
        ));
        assert_eq!(state.read().queue.len(), 1);
    }

    #[test]
    fn blacklist_is_song_specific_not_global() {
        let mut config = Config::default();
        config.rules = default_rules();
        config.blacklist = vec![crate::queue::blacklist::BlacklistEntry::new(
            "晴天", "周杰伦",
        )];
        let (service, _state) = service_with_config(config);

        // 别的歌照常能点
        assert!(matches!(
            service.handle(&danmaku("甲", Some("1"), "点歌 稻香 周杰伦")),
            EnqueueOutcome::Queued { .. }
        ));
    }

    // ── 主播权限（阶段 9）────────────────────────────────────────────────
    //
    // 需求原话："当我用点歌机点歌时（非弹幕），拥有最高权限，不要冷却……
    // 通过主播点歌机可以进行无视黑名单和点歌cd"。

    #[test]
    fn host_bypasses_cooldown() {
        let rules = crate::config::RequestRules {
            cooldown_secs: 600,
            ..default_rules()
        };
        let (service, _state) = service_with(rules);

        // 观众点一首，进入冷却
        assert!(matches!(
            service.handle(&danmaku("观众甲", Some("1"), "点歌 晴天 周杰伦")),
            EnqueueOutcome::Queued { .. }
        ));
        // 同一个观众再来一次：被冷却挡住
        assert!(matches!(
            service.handle(&danmaku("观众甲", Some("1"), "点歌 稻香 周杰伦")),
            EnqueueOutcome::Rejected(RejectReason::Cooldown { .. })
        ));
        // 主播即使同名同 UID 也不受冷却影响
        assert!(matches!(
            service.handle_with_priority(
                &host_danmaku("观众甲", Some("1"), "点歌 稻香 周杰伦"),
                QueuePriority::Host
            ),
            EnqueueOutcome::Queued { .. }
        ));
    }

    #[test]
    fn host_bypasses_danmaku_quota() {
        // 弹幕名额设为 1：第一首弹幕占满，第二首应被拒
        let rules = crate::config::RequestRules {
            max_queue: 1,
            host_extra_per_play: 0,
            ..default_rules()
        };
        let (service, _state) = service_with(rules);
        assert!(matches!(
            service.handle(&danmaku("甲", Some("1"), "点歌 歌一")),
            EnqueueOutcome::Queued { .. }
        ));
        assert!(matches!(
            service.handle(&danmaku("乙", Some("2"), "点歌 歌二")),
            EnqueueOutcome::Rejected(RejectReason::QueueFull { .. })
        ));
        // 主播不受名额限制
        assert!(matches!(
            service.handle_with_priority(
                &host_danmaku("主播", Some("9"), "点歌 歌三"),
                QueuePriority::Host
            ),
            EnqueueOutcome::Queued { .. }
        ));
    }

    #[test]
    fn host_bypasses_duplicate_check() {
        let (service, _state) = service_with(default_rules()); // allow_duplicate = false
        assert!(matches!(
            service.handle(&danmaku("甲", Some("1"), "点歌 晴天 周杰伦")),
            EnqueueOutcome::Queued { .. }
        ));
        // 观众重复点 -> 拒
        assert!(matches!(
            service.handle(&danmaku("乙", Some("2"), "点歌 晴天 周杰伦")),
            EnqueueOutcome::Rejected(RejectReason::Duplicate { .. })
        ));
        // 主播重复点 -> 允许
        assert!(matches!(
            service.handle_with_priority(
                &host_danmaku("主播", Some("9"), "点歌 晴天 周杰伦"),
                QueuePriority::Host
            ),
            EnqueueOutcome::Queued { .. }
        ));
    }

    #[test]
    fn host_entry_is_marked_host_priority_and_jumps_ahead() {
        let (service, state) = service_with(default_rules());
        assert!(matches!(
            service.handle(&danmaku("观众甲", Some("1"), "点歌 弹幕歌")),
            EnqueueOutcome::Queued { .. }
        ));
        assert!(matches!(
            service.handle_with_priority(
                &host_danmaku("主播", Some("9"), "点歌 主播歌"),
                QueuePriority::Host
            ),
            EnqueueOutcome::Queued { .. }
        ));

        let queue = state.read().queue.clone();
        assert_eq!(queue.len(), 2);
        // 主播的歌插到所有弹幕之前
        assert_eq!(queue[0].song.title, "主播歌");
        assert_eq!(queue[0].priority, QueuePriority::Host);
        assert_eq!(queue[1].song.title, "弹幕歌");
        assert_eq!(queue[1].priority, QueuePriority::Danmaku);
    }

    #[test]
    fn host_bypasses_user_level_and_fans_medal() {
        let rules = crate::config::RequestRules {
            min_fans_medal_level: 5,
            min_user_level: 10,
            ..default_rules()
        };
        let (service, _state) = service_with(rules);

        // 观众不满足门槛 -> 拒
        assert!(matches!(
            service.handle(&danmaku("甲", Some("1"), "点歌 晴天")),
            EnqueueOutcome::Rejected(RejectReason::FansMedalTooLow { .. })
        ));
        // 主播无视门槛
        assert!(matches!(
            service.handle_with_priority(
                &host_danmaku("主播", Some("9"), "点歌 晴天"),
                QueuePriority::Host
            ),
            EnqueueOutcome::Queued { .. }
        ));
    }

    #[test]
    fn queues_simple_request() {
        let (service, state) = service_with(default_rules());
        let outcome = service.handle(&danmaku("观众甲", Some("1"), "点歌 晴天 周杰伦"));
        match outcome {
            EnqueueOutcome::Queued {
                item_id,
                position,
                ref title,
            } => {
                assert_eq!(position, 1);
                assert_eq!(title, "晴天");
                assert_eq!(state.read().queue[0].id, item_id, "返回的 ID 应指向入队条目");
            }
            other => panic!("应入队，实际：{other:?}"),
        }
        let guard = state.read();
        assert_eq!(guard.queue.len(), 1);
        assert_eq!(guard.queue[0].song.title, "晴天");
        assert_eq!(guard.queue[0].song.artist, "周杰伦");
        assert_eq!(guard.queue[0].requested_by, "观众甲");
        assert_eq!(guard.queue[0].requested_by_uid.as_deref(), Some("1"));
        // 刚入队时是占位歌曲，等待解析器回填
        assert_eq!(guard.queue[0].song.source, crate::models::SongSource::Pending);
        assert_eq!(guard.stats.recent.len(), 1);
        assert_eq!(guard.stats.recent[0].outcome, "queued");
    }

    #[test]
    fn ignores_non_command_danmaku() {
        let (service, state) = service_with(default_rules());
        assert_eq!(
            service.handle(&danmaku("观众", Some("1"), "主播好厉害")),
            EnqueueOutcome::NotARequest
        );
        assert!(state.read().queue.is_empty());
        // 普通弹幕不写记录
        assert!(state.read().stats.recent.is_empty());
    }

    #[test]
    fn title_only_request_has_empty_artist() {
        let (service, state) = service_with(default_rules());
        let outcome = service.handle(&danmaku("观众甲", Some("1"), "点歌 晴天"));
        assert!(matches!(outcome, EnqueueOutcome::Queued { .. }));
        assert_eq!(state.read().queue[0].song.artist, "");
    }

    #[test]
    fn cooldown_blocks_second_request_from_same_user() {
        let mut rules = default_rules();
        rules.cooldown_secs = 60;
        let (service, state) = service_with(rules);

        assert!(matches!(
            service.handle(&danmaku("观众甲", Some("1"), "点歌 晴天")),
            EnqueueOutcome::Queued { .. }
        ));
        let outcome = service.handle(&danmaku("观众甲", Some("1"), "点歌 富士山下"));
        match outcome {
            EnqueueOutcome::Rejected(RejectReason::Cooldown { remaining_secs }) => {
                assert!(remaining_secs > 0 && remaining_secs <= 60);
            }
            other => panic!("应因冷却被拒绝，实际：{other:?}"),
        }
        assert_eq!(state.read().queue.len(), 1, "被拒绝的请求不应入队");
        assert_eq!(state.read().stats.cooldowns.len(), 1);

        // 另一个用户不受影响
        assert!(matches!(
            service.handle(&danmaku("观众乙", Some("2"), "点歌 富士山下")),
            EnqueueOutcome::Queued { .. }
        ));
    }

    #[test]
    fn cooldown_uses_nickname_when_uid_missing() {
        let mut rules = default_rules();
        rules.cooldown_secs = 30;
        let (service, _state) = service_with(rules);
        assert!(matches!(
            service.handle(&danmaku("匿名", None, "点歌 晴天")),
            EnqueueOutcome::Queued { .. }
        ));
        assert!(matches!(
            service.handle(&danmaku("匿名", None, "点歌 稻香")),
            EnqueueOutcome::Rejected(RejectReason::Cooldown { .. })
        ));
    }

    #[test]
    fn queue_limit_rejects_when_full() {
        let mut rules = default_rules();
        rules.max_queue = 2;
        rules.allow_duplicate = true;
        let (service, state) = service_with(rules);

        assert!(matches!(
            service.handle(&danmaku("A", Some("1"), "点歌 歌一")),
            EnqueueOutcome::Queued { .. }
        ));
        assert!(matches!(
            service.handle(&danmaku("B", Some("2"), "点歌 歌二")),
            EnqueueOutcome::Queued { .. }
        ));
        assert_eq!(
            service.handle(&danmaku("C", Some("3"), "点歌 歌三")),
            EnqueueOutcome::Rejected(RejectReason::QueueFull { max: 2 })
        );
        assert_eq!(state.read().queue.len(), 2);
    }

    #[test]
    fn duplicate_rejected_unless_allowed() {
        let (service, state) = service_with(default_rules());
        assert!(matches!(
            service.handle(&danmaku("A", Some("1"), "点歌 晴天 周杰伦")),
            EnqueueOutcome::Queued { .. }
        ));
        // 同名同歌手 → 拒绝
        match service.handle(&danmaku("B", Some("2"), "点歌 晴天 周杰伦")) {
            EnqueueOutcome::Rejected(RejectReason::Duplicate { title }) => assert_eq!(title, "晴天"),
            other => panic!("应判为重复，实际：{other:?}"),
        }
        // 同名不同歌手 → 允许（视为不同版本）
        assert!(matches!(
            service.handle(&danmaku("C", Some("3"), "点歌 晴天 张信哲")),
            EnqueueOutcome::Queued { .. }
        ));
        assert_eq!(state.read().queue.len(), 2);

        // 打开重复开关后同名同歌手可再点
        let mut rules = default_rules();
        rules.allow_duplicate = true;
        let (service2, _state2) = service_with(rules);
        assert!(matches!(
            service2.handle(&danmaku("A", Some("1"), "点歌 晴天 周杰伦")),
            EnqueueOutcome::Queued { .. }
        ));
        assert!(matches!(
            service2.handle(&danmaku("B", Some("2"), "点歌 晴天 周杰伦")),
            EnqueueOutcome::Queued { .. }
        ));
    }

    #[test]
    fn duplicate_check_ignores_case_and_spaces() {
        let queue = vec![QueueItem::new(
            Song::placeholder("  QingTian ", "Jay"),
            "A",
            None,
        )];
        let request = SongRequest {
            title: "qingtian".into(),
            artist: "jay".into(),
            raw: String::new(),
        };
        assert!(is_duplicate(&queue, &request));

        let other_artist = SongRequest {
            title: "qingtian".into(),
            artist: "other".into(),
            raw: String::new(),
        };
        assert!(!is_duplicate(&queue, &other_artist));
    }

    #[test]
    fn level_requirements_are_enforced() {
        let mut rules = default_rules();
        rules.min_fans_medal_level = 5;
        rules.min_user_level = 10;
        let (service, state) = service_with(rules);

        let mut dm = danmaku("A", Some("1"), "点歌 晴天");
        dm.fans_medal_level = 3;
        dm.user_level = 20;
        assert_eq!(
            service.handle(&dm),
            EnqueueOutcome::Rejected(RejectReason::FansMedalTooLow {
                current: 3,
                required: 5
            })
        );

        let mut dm2 = danmaku("A", Some("1"), "点歌 晴天");
        dm2.fans_medal_level = 6;
        dm2.user_level = 2;
        assert_eq!(
            service.handle(&dm2),
            EnqueueOutcome::Rejected(RejectReason::UserLevelTooLow {
                current: 2,
                required: 10
            })
        );

        let mut dm3 = danmaku("A", Some("1"), "点歌 晴天");
        dm3.fans_medal_level = 6;
        dm3.user_level = 11;
        assert!(matches!(
            service.handle(&dm3),
            EnqueueOutcome::Queued { .. }
        ));
        assert_eq!(state.read().queue.len(), 1);
    }

    #[test]
    fn rejection_reasons_have_kind_and_message() {
        let cases = [
            RejectReason::Cooldown { remaining_secs: 12 },
            RejectReason::QueueFull { max: 20 },
            RejectReason::Duplicate {
                title: "晴天".into(),
            },
            RejectReason::FansMedalTooLow {
                current: 1,
                required: 5,
            },
            RejectReason::UserLevelTooLow {
                current: 1,
                required: 10,
            },
        ];
        for reason in cases {
            assert!(!reason.message().is_empty());
            assert!(!reason.kind().is_empty());
        }
    }

    #[test]
    fn records_are_newest_first_and_capped() {
        let mut rules = default_rules();
        rules.allow_duplicate = true;
        rules.cooldown_secs = 0;
        let (service, state) = service_with(rules);
        // 只多打一屏就够验证「环形缓冲会截断」，
        // 不必真的灌满 MAX_RECENT（2000 条在调试构建下偏慢）
        let total = MAX_RECENT_IN_STATE + 10;
        for i in 0..total {
            service.handle(&danmaku("A", Some("1"), &format!("点歌 歌{i}")));
        }
        let recent = service.recent();
        assert_eq!(recent.len(), total, "完整日志应保留全部记录");
        // 最新的在最前
        assert_eq!(recent[0].title, format!("歌{}", total - 1));
        // 广播用的 stats 只带最近一批，避免 WS 消息膨胀
        assert_eq!(state.read().stats.recent.len(), MAX_RECENT_IN_STATE);
    }

    #[test]
    fn full_log_keeps_far_more_than_broadcast_slice() {
        // 「记录所有点歌日志」的关键：`recent()` 的容量远大于 `stats.recent`。
        // 用常量大小关系断言，避免真的写入上千条拖慢测试。
        assert!(
            MAX_RECENT > MAX_RECENT_IN_STATE * 10,
            "完整日志上限应显著大于广播切片：{MAX_RECENT} vs {MAX_RECENT_IN_STATE}"
        );
    }

    #[test]
    fn hot_swapping_parser_takes_effect_immediately() {
        let (service, state) = service_with(default_rules());
        // 默认正则不认「求歌」
        assert_eq!(
            service.handle(&danmaku("A", Some("1"), "求歌 晴天")),
            EnqueueOutcome::NotARequest
        );

        // 换成自定义正则后立刻生效
        let parser = SongRequestParser::new(r"^求歌\s+(.+?)(?:\s+(.+))?$").expect("正则应合法");
        service.set_parser(parser);
        assert_eq!(service.pattern(), r"^求歌\s+(.+?)(?:\s+(.+))?$");
        assert!(matches!(
            service.handle(&danmaku("A", Some("1"), "求歌 晴天 周杰伦")),
            EnqueueOutcome::Queued { .. }
        ));
        assert_eq!(state.read().queue[0].song.title, "晴天");
    }

    #[test]
    fn invalid_configured_regex_falls_back_to_default() {
        let state = state();
        let mut config = Config::default();
        config.rules.command_regex = "^点歌([".to_string();
        let config = Arc::new(config);
        let provider = {
            let config = Arc::clone(&config);
            move || (*config).clone()
        };
        let service = SongRequestService::from_config(state.clone(), provider);
        assert!(matches!(
            service.handle(&danmaku("A", Some("1"), "点歌 晴天")),
            EnqueueOutcome::Queued { .. }
        ));
    }

    // ── 点歌日志落盘（阶段 10d）──────────────────────────────────────────

    /// 建一个独占的临时日志路径，避免测试之间互相污染。
    fn temp_log(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bsr-request-log-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("requests.jsonl")
    }

    #[test]
    fn request_log_is_appended_and_reloaded() {
        // 用户诉求「点歌日志也要一直记录着」：写盘后新建服务应能读回
        let path = temp_log("roundtrip");
        let state = state();
        let mut config = Config::default();
        // 关掉冷却与名额限制，方便连续记录多条
        config.rules.cooldown_secs = 0;
        config.rules.max_queue = 0;
        let config = Arc::new(config);
        let provider = {
            let config = Arc::clone(&config);
            move || (*config).clone()
        };

        let service = SongRequestService::from_config(state.clone(), provider.clone())
            .with_log_path(path.clone());
        service.handle(&danmaku("A", Some("1"), "点歌 晴天"));
        service.handle(&danmaku("B", Some("2"), "点歌 稻香"));

        // 新建一个服务（模拟重启），应从磁盘读回
        let reloaded = SongRequestService::from_config(state.clone(), provider)
            .with_log_path(path.clone());
        let recent = reloaded.recent();
        assert_eq!(recent.len(), 2, "重启后应恢复历史日志");
        // 内存里最新在前
        assert_eq!(recent[0].title, "稻香");
        assert_eq!(recent[1].title, "晴天");
    }

    #[test]
    fn corrupt_log_line_does_not_discard_the_rest() {
        // 上次写到一半断电会留下半行 JSON；不能因此把整份日志报废
        let path = temp_log("corrupt");
        std::fs::write(
            &path,
            r#"{"user":"A","title":"晴天","artist":"","outcome":"queued","at":"2026-01-01T00:00:00Z"}
{"user":"B","title":"半行
{"user":"C","title":"稻香","artist":"","outcome":"queued","at":"2026-01-01T00:01:00Z"}
"#,
        )
        .unwrap();

        let state = state();
        let config = Arc::new(Config::default());
        let provider = {
            let config = Arc::clone(&config);
            move || (*config).clone()
        };
        let service = SongRequestService::from_config(state, provider).with_log_path(path);
        let recent = service.recent();
        assert_eq!(recent.len(), 2, "损坏行应被跳过，其余记录仍要保留");
        assert_eq!(recent[0].title, "稻香");
        assert_eq!(recent[1].title, "晴天");
    }

    #[test]
    fn missing_log_file_starts_empty_without_error() {
        let path = temp_log("missing").join("nested").join("requests.jsonl");
        let state = state();
        let config = Arc::new(Config::default());
        let provider = {
            let config = Arc::clone(&config);
            move || (*config).clone()
        };
        let service = SongRequestService::from_config(state, provider).with_log_path(path);
        assert!(service.recent().is_empty());
    }

    #[test]
    fn cooldown_tracker_prunes_expired_entries() {
        let tracker = CooldownTracker::new();
        tracker.touch("a");
        tracker.touch("b");
        assert_eq!(tracker.len(), 2);
        // cooldown 为 0 时全部视为过期
        tracker.prune(0);
        assert!(tracker.is_empty());
    }

    #[test]
    fn humanize_age_formats_recent_times() {
        assert_eq!(humanize_age(Utc::now()), "刚刚");
        assert!(humanize_age(Utc::now() - ChronoDuration::seconds(30)).contains("秒前"));
        assert!(humanize_age(Utc::now() - ChronoDuration::minutes(5)).contains("分钟前"));
        assert!(humanize_age(Utc::now() - ChronoDuration::hours(3)).contains("小时前"));
    }
}
