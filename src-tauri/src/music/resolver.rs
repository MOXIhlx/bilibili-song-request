//! 点歌后的曲目解析：把「歌名+歌手」变成真实曲目信息。
//!
//! ## 为什么是异步两段式
//! 弹幕点歌只给出文本。若在弹幕处理路径上同步调用音乐平台，会出现两个问题：
//!  1. 网络往返（几百毫秒到几秒）会拖住弹幕消费，导致后续弹幕积压；
//!  2. 平台抖动或风控会让点歌整体失败。
//!
//! 因此队列条目**先以占位形态入队并立即广播**（观众马上看到「已入队」），
//! 再由独立任务搜索并回填真实曲目信息（`SongSource::Pending → Resolved/Failed`）。
//! 界面上表现为「解析中…」→ 变成真实歌名/歌手/时长。
//!
//! 阶段 6 取播放地址时若仍是 `Pending`，会先等待解析完成。

use std::sync::Arc;

use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::event::HubEvent;
use crate::models::{MusicPlatform, Song, SongSource};
use crate::music::{MusicAdapter, MusicError, MusicService};
use crate::queue::SongRequest;
use crate::state::StateCell;

/// 解析请求：一个队列条目待回填的搜索条件。
#[derive(Debug, Clone)]
pub struct ResolveJob {
    /// 队列条目 ID。
    pub item_id: Uuid,
    /// 搜索关键词（`歌名 歌手`）。
    pub keyword: String,
    /// 原始歌名（搜索失败时保留）。
    pub title: String,
    /// 原始歌手。
    pub artist: String,
}

impl ResolveJob {
    /// 由点歌请求与队列条目 ID 构造。
    pub fn new(item_id: Uuid, request: &SongRequest) -> Self {
        Self {
            item_id,
            keyword: request.keyword(),
            title: request.title.clone(),
            artist: request.artist.clone(),
        }
    }
}

/// 解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveOutcome {
    /// 成功解析出真实曲目。
    Resolved {
        /// 平台。
        platform: MusicPlatform,
        /// 歌名（平台返回的规范名）。
        title: String,
        /// 歌手。
        artist: String,
    },
    /// 搜索失败，已把原因写入队列条目。
    Failed {
        /// 失败原因（可直接展示）。
        reason: String,
    },
    /// 队列条目已不在（被删除/已播放），无需回填。
    Gone,
}

/// 解析器：串行消费解析任务。
///
/// 之所以串行（单任务 + 队列）：非官方接口对并发敏感，
/// 并发搜索很容易触发风控；点歌场景对延迟不敏感，串行完全够用。
///
/// ⚠️ 注意「自持发送端」问题：`SongResolver` 自己持有 `tx`，
/// 因此接收端 `rx` 永远不会因为发送端全部析构而关闭。
/// 退出必须显式调用 [`SongResolver::shutdown`]。
pub struct SongResolver {
    /// 队列/状态。
    state: StateCell,
    /// 音乐服务。
    music: Arc<MusicService>,
    /// 任务接收端。
    rx: tokio::sync::mpsc::UnboundedReceiver<ResolveJob>,
    /// 任务发送端（用于派生 [`ResolveSender`]）。
    tx: tokio::sync::mpsc::UnboundedSender<ResolveJob>,
    /// 后台循环句柄（`spawn` 之后才有值）。
    task: Option<tokio::task::JoinHandle<()>>,
}

impl SongResolver {
    /// 创建解析器与投递句柄。
    pub fn new(state: StateCell, music: Arc<MusicService>) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            state,
            music,
            rx,
            tx,
            task: None,
        }
    }

    /// 投递句柄（可在任意位置克隆使用）。
    pub fn sender(&self) -> ResolveSender {
        ResolveSender { tx: self.tx.clone() }
    }

    /// 启动后台循环并记录句柄，使 [`SongResolver::shutdown`] 能停止它。
    pub fn spawn(mut self) -> ResolveSender {
        let sender = self.sender();
        // 把循环所需的字段取出来交给独立任务：这样 `self` 仍保留接收端的所有权判定
        // 与任务句柄，`shutdown` 才能取消它。
        let state = self.state.clone();
        let music = Arc::clone(&self.music);
        let rx = std::mem::replace(&mut self.rx, tokio::sync::mpsc::unbounded_channel().1);

        let handle = tokio::spawn(async move {
            if let Err(err) = resolve_loop(state, music, rx).await {
                warn!(error = %err, "曲目解析器异常退出");
            }
        });
        self.task = Some(handle);
        sender
    }

    /// 停止后台循环（丢弃接收端并中止任务）。
    pub async fn shutdown(&mut self) {
        if let Some(handle) = self.task.take() {
            handle.abort();
            let _ = handle.await;
            info!("点歌曲目解析器已停止");
        }
    }

    /// 后台循环：直到 [`SongResolver::shutdown`] 被调用。
    pub async fn run_loop(&mut self) -> anyhow::Result<()> {
        let state = self.state.clone();
        let music = Arc::clone(&self.music);
        // 直接消费字段：把接收端移出 `self`（保留占位通道），循环结束后不再使用。
        let rx = std::mem::replace(&mut self.rx, tokio::sync::mpsc::unbounded_channel().1);
        resolve_loop(state, music, rx).await
    }

    /// 兼容入口：等价于 [`SongResolver::run_loop`]（消费 `self`）。
    pub async fn run(mut self) -> anyhow::Result<()> {
        self.run_loop().await
    }
}

/// 实际的任务消费循环。
///
/// 独立成自由函数：它只依赖「状态 + 音乐服务 + 接收端」，
/// 因此后台任务不需要持有整个 [`SongResolver`]（避免自持发送端导致无法退出）。
async fn resolve_loop(
    state: StateCell,
    music: Arc<MusicService>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<ResolveJob>,
) -> anyhow::Result<()> {
    info!("点歌曲目解析器已启动");
    while let Some(job) = rx.recv().await {
        let outcome = resolve_with_service(&state, &music, &job).await;
        match &outcome {
            ResolveOutcome::Resolved {
                platform,
                title,
                artist,
            } => info!(%title, %artist, ?platform, "点歌已解析为真实曲目"),
            ResolveOutcome::Failed { reason } => {
                warn!(keyword = %job.keyword, %reason, "点歌解析失败，保留用户输入")
            }
            ResolveOutcome::Gone => debug!(item_id = %job.item_id, "条目已不在队列，跳过解析"),
        }
    }
    info!("点歌曲目解析器已退出");
    Ok(())
}

/// 用 [`MusicService`] 的**跨平台**策略解析（生产路径）。
///
/// 与 [`resolve_with`] 的区别：这里走 `search_best`，会在默认平台（QQ）
/// 没有精确命中时再去另一个平台（网易云）搜一轮，并按本地打分取全局最高分。
/// 条目是否还存在于**队列、空闲歌单或当前播放项**里（阶段 10a）。
///
/// ⚠️ 四处都要查：
///  - 空闲歌单的条目也要解析（只查 `queue` 会把它们当成已移除）；
///  - `advance()` 是**先 `take_from_queue` 再 `load_and_play`**，
///    条目在解析完成前就已经搬到 `current` 了。只查队列会让解析器
///    判定「已移除」直接放弃，而播放器那边又在等解析结果——互相死等，
///    最终报「条目已从队列移除」，表现为**刚入队就被跳过/没有声音**；
///  - 阶段 10d 起播放顺序由 `playing` 唯一表达，条目可能**只在 `playing` 里**
///    （已经从 `queue` 搬走、但还没同步成 `current`）。漏查它会让解析结果
///    写不回去，界面上看到的就是「歌曲信息一直是解析中/标题是占位文本」。
pub fn item_exists(state: &StateCell, id: uuid::Uuid) -> bool {
    let guard = state.read();
    guard.queue.iter().any(|i| i.id == id)
        || guard.idle.iter().any(|i| i.id == id)
        || guard.playing.iter().any(|i| i.id == id)
        || guard.current.as_ref().is_some_and(|i| i.id == id)
}

/// 在队列 / 空闲歌单 / 播放序列 / 当前播放项里按 id 找到条目并就地修改。
///
/// 返回是否找到。这几处 id 不会重复（都是新生成的 uuid），
/// 顺序只影响可读性：队列 → 空闲歌单 → 播放序列 → 当前播放项。
fn with_item_mut<F: FnOnce(&mut crate::models::QueueItem)>(
    state: &StateCell,
    id: uuid::Uuid,
    edit: F,
) -> bool {
    let mut found = false;
    let mut edit = Some(edit);
    state.mutate(|s| {
        let target = s
            .queue
            .iter_mut()
            .find(|i| i.id == id)
            .or_else(|| s.idle.iter_mut().find(|i| i.id == id))
            .or_else(|| s.playing.iter_mut().find(|i| i.id == id))
            .or_else(|| {
                s.current
                    .as_mut()
                    .filter(|i| i.id == id)
            });
        if let Some(item) = target {
            if let Some(edit) = edit.take() {
                edit(item);
            }
            found = true;
        }
    });
    found
}

/// `resolve_with` 保留给单适配器场景（测试与降级路径）。
async fn resolve_with_service(
    state: &StateCell,
    music: &MusicService,
    job: &ResolveJob,
) -> ResolveOutcome {
    if !item_exists(state, job.item_id) {
        return ResolveOutcome::Gone;
    }

    match music.search_best(&job.title, &job.artist).await {
        Ok((song, exact)) => {
            if !exact {
                debug!(
                    title = %song.title,
                    artist = %song.artist,
                    "没有精确匹配的原唱，采用综合评分最高的一条"
                );
            }
            apply_resolved(state, job, song)
        }
        Err(err) => mark_failed(state, job, describe_music_error(&err)),
    }
}

/// 把解析结果写回队列/空闲歌单条目并广播。
fn apply_resolved(state: &StateCell, job: &ResolveJob, song: Song) -> ResolveOutcome {
    let found = with_item_mut(state, job.item_id, |item| {
        item.song = song.clone();
        item.song.source = SongSource::Resolved;
        item.song.source_error = None;
    });

    if !found {
        return ResolveOutcome::Gone;
    }

    state.publish(HubEvent::SongResolved {
        item_id: job.item_id,
        title: song.title.clone(),
        artist: song.artist.clone(),
        platform: song.platform,
        duration: song.duration,
    });

    ResolveOutcome::Resolved {
        platform: song.platform,
        title: song.title,
        artist: song.artist,
    }
}

/// 解析任务投递句柄。
#[derive(Clone)]
pub struct ResolveSender {
    tx: tokio::sync::mpsc::UnboundedSender<ResolveJob>,
}

impl ResolveSender {
    /// 投递一个解析任务；解析器已退出时返回 false。
    pub fn send(&self, job: ResolveJob) -> bool {
        self.tx.send(job).is_ok()
    }
}

/// 用给定适配器解析并把结果写回队列条目。
///
/// 独立成函数是为了可测试：测试可以传入假的 [`MusicAdapter`]，
/// 不需要网络。
pub async fn resolve_with(
    state: &StateCell,
    adapter: &dyn MusicAdapter,
    job: &ResolveJob,
) -> ResolveOutcome {
    // 条目可能已经被删除（主播手动删了 / 已播放完），先确认还在。
    if !item_exists(state, job.item_id) {
        return ResolveOutcome::Gone;
    }

    match adapter.search(&job.keyword).await {
        Ok(songs) => {
            let Some(song) = songs.into_iter().next() else {
                return mark_failed(
                    state,
                    job,
                    format!("没有搜索到《{}》", job.keyword),
                );
            };

            let found = with_item_mut(state, job.item_id, |item| {
                item.song = song.clone();
                item.song.source = SongSource::Resolved;
                item.song.source_error = None;
            });

            if !found {
                return ResolveOutcome::Gone;
            }

            state.publish(HubEvent::SongResolved {
                item_id: job.item_id,
                title: song.title.clone(),
                artist: song.artist.clone(),
                platform: song.platform,
                duration: song.duration,
            });

            ResolveOutcome::Resolved {
                platform: song.platform,
                title: song.title,
                artist: song.artist,
            }
        }
        Err(err) => {
            let reason = describe_music_error(&err);
            mark_failed(state, job, reason)
        }
    }
}

/// 标记条目解析失败并广播。
fn mark_failed(state: &StateCell, job: &ResolveJob, reason: String) -> ResolveOutcome {
    let found = with_item_mut(state, job.item_id, |item| {
        // 保留用户输入的文本，只把状态标为失败，方便界面显示「搜索失败」。
        item.song.title = job.title.clone();
        if !job.artist.is_empty() {
            item.song.artist = job.artist.clone();
        }
        item.song.mark_failed(reason.clone());
    });

    if !found {
        return ResolveOutcome::Gone;
    }

    state.publish(HubEvent::SongResolveFailed {
        item_id: job.item_id,
        title: job.title.clone(),
        reason: reason.clone(),
    });

    ResolveOutcome::Failed { reason }
}

/// 把适配器错误转成给用户看的短句。
pub fn describe_music_error(err: &MusicError) -> String {
    match err {
        MusicError::NotFound(keyword) => format!("没有搜索到《{keyword}》"),
        // 风控类的 Network 错误会把原因写在消息里（例如「触发网易云风控…请稍后重试」），
        // 这类信息对主播很有用，直接透出而不是替换成笼统文案。
        MusicError::Network(detail) if !detail.trim().is_empty() => {
            format!("搜索失败：{}", crate::music::netease::snippet(detail, 120))
        }
        MusicError::Network(_) => "搜索失败（网络异常），可稍后重试".to_string(),
        MusicError::ApiChanged(_) => "音乐平台接口已变化，搜索失败".to_string(),
        MusicError::NotLoggedIn(_) => "需要登录音乐平台账号".to_string(),
        MusicError::Unplayable => "该歌曲无法播放（版权限制）".to_string(),
        MusicError::Unimplemented(_) => "该平台暂未实现搜索".to_string(),
    }
}

/// 把广播错误转成日志文本（供测试与排障）。
pub fn describe_recv_error(err: &RecvError) -> String {
    match err {
        RecvError::Lagged(n) => format!("解析队列落后 {n} 条"),
        RecvError::Closed => "解析队列已关闭".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AppState, BilibiliState, PlayerState, QueueItem, Song};
    use crate::music::MusicError;
    use crate::VERSION;
    use async_trait::async_trait;
    use std::sync::Mutex;

    fn state_with_item(title: &str, artist: &str) -> (StateCell, Uuid) {
        let state = StateCell::new(AppState::new(
            VERSION,
            PlayerState::default(),
            BilibiliState::default(),
        ));
        let item = QueueItem::new(Song::placeholder(title, artist), "观众甲", None);
        let id = item.id;
        state.mutate(|s| s.queue.push(item));
        (state, id)
    }

    /// 可控的假适配器。
    struct FakeAdapter {
        result: Mutex<Result<Vec<Song>, MusicError>>,
    }

    impl FakeAdapter {
        fn ok(songs: Vec<Song>) -> Self {
            Self {
                result: Mutex::new(Ok(songs)),
            }
        }
        fn err(err: MusicError) -> Self {
            Self {
                result: Mutex::new(Err(err)),
            }
        }
    }

    #[async_trait]
    impl MusicAdapter for FakeAdapter {
        fn platform(&self) -> MusicPlatform {
            MusicPlatform::Netease
        }

        async fn search(&self, _keyword: &str) -> Result<Vec<Song>, MusicError> {
            let guard = self.result.lock().unwrap();
            match &*guard {
                Ok(songs) => Ok(songs.clone()),
                Err(MusicError::NotFound(k)) => Err(MusicError::NotFound(k.clone())),
                Err(MusicError::Network(m)) => Err(MusicError::Network(m.clone())),
                Err(MusicError::ApiChanged(m)) => Err(MusicError::ApiChanged(m.clone())),
                Err(MusicError::NotLoggedIn(m)) => Err(MusicError::NotLoggedIn(m.clone())),
                Err(MusicError::Unplayable) => Err(MusicError::Unplayable),
                Err(MusicError::Unimplemented(m)) => Err(MusicError::Unimplemented(m)),
            }
        }

        async fn get_play_url(&self, _song_id: &str) -> Result<String, MusicError> {
            Err(MusicError::Unplayable)
        }

        async fn get_lyrics(&self, _song_id: &str) -> Result<String, MusicError> {
            Ok(String::new())
        }
    }

    fn job_for(id: Uuid, title: &str, artist: &str) -> ResolveJob {
        ResolveJob {
            item_id: id,
            keyword: if artist.is_empty() {
                title.to_string()
            } else {
                format!("{title} {artist}")
            },
            title: title.to_string(),
            artist: artist.to_string(),
        }
    }

    #[tokio::test]
    async fn resolves_placeholder_into_real_song() {
        let (state, id) = state_with_item("晴天", "周杰伦");
        let mut song = Song::placeholder("晴天", "周杰伦");
        song.id = "186016".into();
        song.duration = 269;
        song.album = Some("叶惠美".into());
        song.source = SongSource::Resolved;
        let adapter = FakeAdapter::ok(vec![song]);

        let outcome = resolve_with(&state, &adapter, &job_for(id, "晴天", "周杰伦")).await;
        assert!(matches!(outcome, ResolveOutcome::Resolved { .. }));

        let guard = state.read();
        let item = &guard.queue[0];
        assert_eq!(item.song.id, "186016");
        assert_eq!(item.song.duration, 269);
        assert!(item.song.is_resolved());
        assert!(item.song.source_error.is_none());
    }

    #[tokio::test]
    async fn empty_result_marks_failed_and_keeps_user_text() {
        let (state, id) = state_with_item("不存在的歌", "某人");
        let adapter = FakeAdapter::ok(vec![]);

        let outcome = resolve_with(&state, &adapter, &job_for(id, "不存在的歌", "某人")).await;
        match outcome {
            ResolveOutcome::Failed { reason } => assert!(reason.contains("不存在的歌")),
            other => panic!("应失败，实际：{other:?}"),
        }

        let guard = state.read();
        let item = &guard.queue[0];
        assert_eq!(item.song.title, "不存在的歌");
        assert_eq!(item.song.artist, "某人");
        assert_eq!(item.song.source, SongSource::Failed);
        assert!(item.song.source_error.is_some());
    }

    #[tokio::test]
    async fn network_error_marks_failed_with_readable_reason() {
        let (state, id) = state_with_item("晴天", "周杰伦");
        let adapter = FakeAdapter::err(MusicError::Network("connection timeout".into()));
        let outcome = resolve_with(&state, &adapter, &job_for(id, "晴天", "周杰伦")).await;
        match outcome {
            ResolveOutcome::Failed { reason } => {
                // 具体原因会被透出（风控/超时等对主播有诊断价值）
                assert!(reason.contains("搜索失败"), "应可读：{reason}");
                assert!(reason.contains("timeout"), "应保留原始原因：{reason}");
            }
            other => panic!("应失败，实际：{other:?}"),
        }
        assert_eq!(state.read().queue[0].song.source, SongSource::Failed);
    }

    #[tokio::test]
    async fn removed_item_is_reported_as_gone() {
        let (state, id) = state_with_item("晴天", "周杰伦");
        state.mutate(|s| s.queue.clear());
        let adapter = FakeAdapter::ok(vec![Song::placeholder("晴天", "周杰伦")]);
        let outcome = resolve_with(&state, &adapter, &job_for(id, "晴天", "周杰伦")).await;
        assert_eq!(outcome, ResolveOutcome::Gone);
    }

    #[tokio::test]
    async fn resolver_loop_stops_on_shutdown() {
        // 只验证生命周期契约（不投递任务，避免依赖外部接口状态）。
        // 解析逻辑本身由 `resolve_with` + 假适配器的用例覆盖。
        let (state, _id) = state_with_item("富士山下", "陈奕迅");
        let dir = std::env::temp_dir().join(format!(
            "bsr-resolver-{:?}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let music = Arc::new(MusicService::with_stores(
            crate::music::SecretStore::file_only("netease.cookie", &dir),
            crate::music::SecretStore::file_only("qq.cookie", &dir),
        ));
        let mut resolver = SongResolver::new(state.clone(), Arc::clone(&music));
        let sender = resolver.sender();
        // 投递句柄可用（解析器尚未停止）
        assert!(sender.send(job_for(_id, "富士山下", "陈奕迅")));

        // 注意：解析器自己持有发送端，接收端不会因外部 senders 析构而关闭，
        // 因此退出必须显式 shutdown。
        resolver.shutdown().await;
    }

    #[test]
    fn sender_reports_failure_after_receiver_dropped() {
        let (state, _id) = state_with_item("晴天", "周杰伦");
        let dir = std::env::temp_dir().join(format!(
            "bsr-resolver-drop-{:?}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let music = Arc::new(MusicService::with_stores(
            crate::music::SecretStore::file_only("netease.cookie", &dir),
            crate::music::SecretStore::file_only("qq.cookie", &dir),
        ));
        let resolver = SongResolver::new(state, music);
        let sender = resolver.sender();
        drop(resolver); // 接收端被丢弃
        assert!(
            !sender.send(ResolveJob {
                item_id: Uuid::new_v4(),
                keyword: "x".into(),
                title: "x".into(),
                artist: String::new(),
            }),
            "接收端不存在时 send 应返回 false，供调用方降级"
        );
    }

    #[test]
    fn error_messages_are_user_friendly() {
        assert!(describe_music_error(&MusicError::NotFound("x".into())).contains("没有搜索到"));
        assert!(describe_music_error(&MusicError::Network("x".into())).contains("搜索失败"));
        assert!(describe_music_error(&MusicError::ApiChanged("x".into())).contains("接口已变化"));
        assert!(describe_music_error(&MusicError::Unplayable).contains("版权"));
        assert!(describe_music_error(&MusicError::NotLoggedIn("".into())).contains("登录"));
        // 空白细节不应产生「搜索失败：」这种残缺消息
        assert_eq!(
            describe_music_error(&MusicError::Network("   ".into())),
            "搜索失败（网络异常），可稍后重试"
        );
    }

    #[test]
    fn recv_error_describes_are_stable() {
        assert!(describe_recv_error(&RecvError::Lagged(3)).contains('3'));
        assert!(describe_recv_error(&RecvError::Closed).contains("关闭"));
    }
}
