//! 启动时恢复上一次的队列。
//!
//! ## 为什么需要
//! 主播常常是「先点一堆歌 → 重启程序（更新/崩溃恢复）→ 继续播」。
//! 如果队列只存在内存里，重启就全丢了。
//!
//! ## 恢复策略
//! 只保存**待播放**的条目与播放模式：
//!  - `current`（正在播放的那首）不保存——重启后音频流已失效，重新点一次更合理；
//!  - 已播放/已跳过的不保存——它们不再有队列意义；
//!  - 「已解析」的曲目信息会被完整保留，因此重启后**无需重新搜索**。

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::models::{PlayMode, QueueItem};
use crate::state::StateCell;

/// 队列快照文件内容。
///
/// ## 版本历史
/// - v1：只有 `play_mode` + `queue`
/// - v2：新增 `history`
/// - **v3（阶段 10d 重构）**：用 `playing` + `cursor` 取代 `history`，
///   并补上 `idle` 相关字段与 `current`——用户反馈「每次重开软件
///   空闲歌单和播放器都空了」，就是因为这些字段**从来没进过快照**。
///
/// 旧快照（v1/v2）依靠 `#[serde(default)]` 正常读取；v2 的 `history`
/// 会被忽略（不再有对应字段），队列与播放模式照常恢复。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueSnapshot {
    /// 快照格式版本，便于将来迁移。
    #[serde(default = "default_version")]
    pub version: u32,
    /// 播放模式。
    #[serde(default)]
    pub play_mode: PlayMode,
    /// 待播放条目（点歌队列）。
    #[serde(default)]
    pub queue: Vec<QueueItem>,
    /// **实际播放序列**（v3 新增）。
    #[serde(default)]
    pub playing: Vec<QueueItem>,
    /// 播放序列游标（v3 新增）。
    #[serde(default)]
    pub cursor: usize,
    /// 空闲歌单（v3 新增）：重启后曲库仍在。
    #[serde(default)]
    pub idle: Vec<QueueItem>,
    /// 空闲歌单播放模式（v3 新增）。
    #[serde(default)]
    pub idle_mode: crate::models::IdleMode,
    /// 空闲歌单书签（v3 新增）：正在播的那一首。
    #[serde(default)]
    pub idle_current: Option<usize>,
    /// 空闲歌单下一次取歌位置（v3 新增）。
    #[serde(default)]
    pub idle_next: usize,
    /// 关闭时正在播放的条目（v3 新增）。
    ///
    /// ⚠️ 恢复后**只显示、不自动播放**——用户明确要求
    /// 「播放器默认是上一次播放的歌曲」，但不想一开机就出声。
    #[serde(default)]
    pub current: Option<QueueItem>,
    /// 关闭时是否在播空闲歌单（v3 新增）。
    #[serde(default)]
    pub current_is_idle: bool,
    /// 关闭时播到第几秒（v4 新增）。
    ///
    /// 需求原话：「关闭软件时记录当前正在播放的歌曲在哪一秒，
    /// 重新打开软件时点击继续播放」要能接着放。
    #[serde(default)]
    pub player_position: f64,
    /// 关闭时这首歌的总时长（秒，v4 新增）。
    ///
    /// ⚠️ 必须一起存：只存位置的话，重启后界面左侧显示 30 秒、
    /// **右侧显示 0**（时长来自 mpv，而此刻没有加载任何文件），
    /// 进度条也会被拉满——用户看到的就是「进度条不对」。
    #[serde(default)]
    pub player_duration: f64,
}

fn default_version() -> u32 {
    // v4：新增 `player_position`（关闭时播到第几秒），用于「继续播放」接着放
    4
}

impl Default for QueueSnapshot {
    fn default() -> Self {
        Self {
            version: default_version(),
            play_mode: PlayMode::default(),
            queue: Vec::new(),
            playing: Vec::new(),
            cursor: 0,
            idle: Vec::new(),
            idle_mode: crate::models::IdleMode::default(),
            idle_current: None,
            idle_next: 0,
            current: None,
            current_is_idle: false,
            player_position: 0.0,
            player_duration: 0.0,
        }
    }
}

/// 队列持久化。
pub struct QueueStore {
    /// 快照文件路径。
    path: std::path::PathBuf,
}

impl QueueStore {
    /// 用给定路径创建。
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// 默认路径：`<配置目录>/queue.json`。
    pub fn default_path() -> std::path::PathBuf {
        crate::config::Config::config_dir().join("queue.json")
    }

    /// 用默认路径创建。
    pub fn with_default_path() -> Self {
        Self::new(Self::default_path())
    }

    /// 快照文件路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 从状态里采集快照。
    ///
    /// 必须包含**所有影响播放状态的字段**：漏掉任何一项，重启后就会丢失
    /// （早期漏了空闲歌单与当前曲目，表现为「播放器是空的」）。
    pub fn snapshot_of(&self, state: &StateCell) -> QueueSnapshot {
        let guard = state.read();
        QueueSnapshot {
            version: default_version(),
            play_mode: guard.play_mode,
            queue: guard.queue.clone(),
            playing: guard.playing.clone(),
            cursor: guard.cursor,
            idle: guard.idle.clone(),
            idle_mode: guard.idle_mode,
            idle_current: guard.idle_current,
            idle_next: guard.idle_next,
            current: guard.current.clone(),
            current_is_idle: guard.current_is_idle,
            player_position: guard.player.position,
            player_duration: guard.player.duration,
        }
    }

    /// 保存当前队列。
    pub fn save(&self, state: &StateCell) -> Result<()> {
        let snapshot = self.snapshot_of(state);
        self.save_snapshot(&snapshot)
    }

    /// 保存给定快照（原子写）。
    pub fn save_snapshot(&self, snapshot: &QueueSnapshot) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建目录 {} 失败", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(snapshot)?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json).with_context(|| format!("写入 {} 失败", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("替换 {} 失败", self.path.display()))?;
        debug!(path = %self.path.display(), items = snapshot.queue.len(), "队列已持久化");
        Ok(())
    }

    /// 读取快照；文件不存在时返回默认空快照。
    pub fn load_snapshot(&self) -> Result<QueueSnapshot> {
        match std::fs::read_to_string(&self.path) {
            Ok(raw) => {
                let snapshot: QueueSnapshot = serde_json::from_str(&raw)
                    .with_context(|| format!("解析 {} 失败", self.path.display()))?;
                Ok(snapshot)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(QueueSnapshot::default()),
            Err(err) => Err(err).with_context(|| format!("读取 {} 失败", self.path.display())),
        }
    }

    /// 恢复播放状态（队列、播放序列、空闲歌单、当前曲目）。
    ///
    /// 返回恢复的点歌队列条目数。文件损坏时只记录警告并继续，不让程序打不开。
    ///
    /// `current` 恢复后**只用于显示、不自动播放**：一开机就出声会吓人，
    /// 因此 `player.playing` 保持 false，由主播自己按播放。
    pub fn restore(&self, state: &StateCell) -> usize {
        let snapshot = match self.load_snapshot() {
            Ok(snapshot) => snapshot,
            Err(err) => {
                warn!(error = %err, "队列快照读取失败，按空队列启动");
                return 0;
            }
        };

        let count = snapshot.queue.len();
        let idle_count = snapshot.idle.len();
        let playing_count = snapshot.playing.len();
        if count == 0 && idle_count == 0 && playing_count == 0 && snapshot.current.is_none() {
            return 0;
        }

        state.mutate(|s| {
            s.play_mode = snapshot.play_mode;
            s.queue = snapshot.queue.clone();
            s.playing = snapshot.playing.clone();
            // 游标夹取，防止手改过文件或旧快照导致越界
            s.cursor = snapshot.cursor.min(s.playing.len().saturating_sub(1));
            s.idle = snapshot.idle.clone();
            s.idle_mode = snapshot.idle_mode;
            s.idle_current = snapshot.idle_current.filter(|i| *i < s.idle.len());
            s.idle_next = snapshot.idle_next.min(s.idle.len());
            s.current = snapshot.current.clone();
            s.current_is_idle = snapshot.current_is_idle;
            // 只显示不播放，并且**默认处于暂停**：用户明确要求
            // 「每次打开软件都把播放器的播放状态设为暂停」，
            // 这样点「开始播放」时语义清晰（继续/从头），也不会一开机就出声。
            s.player.playing = false;
            s.player.paused = true;
            s.player.position = snapshot.player_position.max(0.0);
            // 时长也要恢复，否则界面右侧显示 0、进度条被拉满
            s.player.duration = snapshot.player_duration.max(0.0);
        });
        info!(
            count,
            idle = idle_count,
            playing = playing_count,
            has_current = snapshot.current.is_some(),
            position = snapshot.player_position,
            path = %self.path.display(),
            "已恢复上次的播放状态"
        );
        count
    }

    /// 删除快照文件（清空队列时用）。
    pub fn clear(&self) -> Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).with_context(|| format!("删除 {} 失败", self.path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AppState, BilibiliState, PlayerState, Song, SongSource};
    use crate::VERSION;

    fn temp_store(tag: &str) -> QueueStore {
        let dir = std::env::temp_dir().join(format!(
            "bsr-queue-{tag}-{:?}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        QueueStore::new(dir.join("queue.json"))
    }

    fn state() -> StateCell {
        StateCell::new(AppState::new(
            VERSION,
            PlayerState::default(),
            BilibiliState::default(),
        ))
    }

    fn resolved_item(title: &str) -> QueueItem {
        let mut song = Song::placeholder(title, "歌手");
        song.id = "999".into();
        song.source = SongSource::Resolved;
        QueueItem::new(song, "观众甲", Some("1".into()))
    }

    #[test]
    fn save_then_restore_roundtrip() {
        let store = temp_store("roundtrip");
        let source = state();
        source.mutate(|s| {
            s.play_mode = PlayMode::Random;
            s.queue.push(resolved_item("晴天"));
            s.queue.push(resolved_item("富士山下"));
        });

        store.save(&source).expect("保存应成功");
        assert!(store.path().is_file());

        let target = state();
        let restored = store.restore(&target);
        assert_eq!(restored, 2);

        let guard = target.read();
        assert_eq!(guard.play_mode, PlayMode::Random);
        assert_eq!(guard.queue.len(), 2);
        assert_eq!(guard.queue[0].song.title, "晴天");
        // 已解析信息被完整保留，重启后无需重新搜索
        assert!(guard.queue[0].song.is_resolved());
        assert_eq!(guard.queue[0].requested_by, "观众甲");
        assert!(guard.current.is_none(), "不恢复正在播放项");
    }

    #[test]
    fn missing_file_restores_zero_items() {
        let store = temp_store("missing");
        let target = state();
        assert_eq!(store.restore(&target), 0);
        assert!(target.read().queue.is_empty());
    }

    #[test]
    fn corrupted_file_does_not_panic() {
        let store = temp_store("corrupt");
        if let Some(parent) = store.path().parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(store.path(), "{ this is not json").unwrap();

        let target = state();
        // 损坏时按空队列启动，而不是让程序崩溃
        assert_eq!(store.restore(&target), 0);
        assert!(target.read().queue.is_empty());
    }

    #[test]
    fn clear_is_idempotent() {
        let store = temp_store("clear");
        store.clear().unwrap();
        store.clear().unwrap();
        let source = state();
        source.mutate(|s| s.queue.push(resolved_item("A")));
        store.save(&source).unwrap();
        store.clear().unwrap();
        assert!(!store.path().is_file());
    }

    #[test]
    fn snapshot_includes_current_for_display_after_restart() {
        // v3：`current` 现在**要**写进快照——用户要求「播放器默认是上一次播放的歌曲」。
        // 但恢复后只显示、不自动播放（见 `restore`）。
        let store = temp_store("pending");
        let source = state();
        source.mutate(|s| {
            let item = resolved_item("正在播的");
            s.current = Some(item);
            s.queue.push(resolved_item("待播的"));
        });
        let snapshot = store.snapshot_of(&source);
        assert_eq!(snapshot.queue.len(), 1);
        assert_eq!(snapshot.queue[0].song.title, "待播的");
        assert_eq!(
            snapshot.current.as_ref().map(|i| i.song.title.clone()),
            Some("正在播的".to_string()),
            "当前曲目应进快照以便重启后显示"
        );
    }

    #[test]
    fn playing_sequence_and_cursor_survive_restart() {
        // v3 核心：播放序列与游标必须落盘，否则重启后「上一首/下一首」失忆
        let store = temp_store("playing");
        let source = state();
        for title in ["A", "B", "C"] {
            crate::queue::push_playing(&source, resolved_item(title));
        }
        crate::queue::step_previous(&source); // cursor = 1 (B)

        store.save(&source).expect("保存应成功");

        let target = state();
        store.restore(&target);
        let guard = target.read();
        let titles: Vec<_> = guard
            .playing
            .iter()
            .map(|i| i.song.title.clone())
            .collect();
        assert_eq!(titles, vec!["A", "B", "C"], "播放序列应被恢复");
        assert_eq!(guard.cursor, 1, "游标应指向重启前那一首");
        assert!(!guard.player.playing, "恢复后只显示、不自动播放");
    }

    #[test]
    fn idle_playlist_survives_restart() {
        // v3：空闲歌单以前**完全没进快照**，重启即丢
        let store = temp_store("idle");
        let source = state();
        source.mutate(|s| {
            s.idle.push(resolved_item("夜曲"));
            s.idle.push(resolved_item("稻香"));
            s.idle_mode = crate::models::IdleMode::LoopAll;
            s.idle_current = Some(1);
            s.idle_next = 0;
        });

        store.save(&source).expect("保存应成功");

        let target = state();
        store.restore(&target);
        let guard = target.read();
        assert_eq!(guard.idle.len(), 2, "空闲歌单应被恢复");
        assert_eq!(guard.idle[0].song.title, "夜曲");
        assert_eq!(guard.idle_mode, crate::models::IdleMode::LoopAll);
        assert_eq!(guard.idle_current, Some(1), "书签应被恢复");
        assert_eq!(guard.idle_next, 0);
    }

    #[test]
    fn player_position_and_duration_survive_restart() {
        // 用户报告：重启后进度条左侧是上次秒数、**右侧却是 0**，进度条被拉满。
        // 根因是只存了位置没存时长（时长来自 mpv，重启时没有加载文件）。
        let store = temp_store("pos-dur");
        let source = state();
        crate::queue::push_playing(&source, resolved_item("听到一半的歌"));
        source.mutate(|s| {
            s.player.position = 30.5;
            s.player.duration = 193.0;
        });
        store.save(&source).expect("保存应成功");

        let target = state();
        store.restore(&target);
        let guard = target.read();
        assert_eq!(guard.player.position, 30.5, "播放位置应被恢复");
        assert_eq!(guard.player.duration, 193.0, "总时长也要恢复，否则右侧显示 0");
    }

    #[test]
    fn restore_clamps_out_of_range_cursor() {
        // 手改过快照文件时游标可能越界，恢复必须夹取而不是 panic
        let store = temp_store("clamp");
        let source = state();
        crate::queue::push_playing(&source, resolved_item("唯一"));
        source.mutate(|s| s.cursor = 999);
        store.save(&source).unwrap();

        let target = state();
        store.restore(&target);
        let guard = target.read();
        assert!(guard.cursor < guard.playing.len().max(1), "游标应被夹取");
    }

    #[test]
    fn danmaku_pending_is_derived_from_queue() {
        // 「已用多少名额」是**派生量**：直接数队列里的 Danmaku 条目，
        // 不存字段、也就不会漂移（早期用计数器时曾因重复扣减而失真）
        let store = temp_store("pending-count");
        let source = state();
        let mut danmaku = resolved_item("弹幕点的");
        danmaku.priority = crate::models::QueuePriority::Danmaku;
        let mut host = resolved_item("主播点的");
        host.priority = crate::models::QueuePriority::Host;
        source.mutate(|s| {
            s.queue.push(danmaku);
            s.queue.push(host);
        });
        store.save(&source).unwrap();

        let target = state();
        store.restore(&target);
        assert_eq!(
            crate::queue::danmaku_pending(&target),
            1,
            "应只统计弹幕来源"
        );
    }

    #[test]
    fn v1_and_v2_snapshots_still_load() {
        // 向后兼容：v1（无 history）、v2（有 history）都必须能读，
        // 不能因为新增 `playing`/`idle` 字段就把用户的旧文件变成"打不开"。
        for (name, legacy) in [
            ("v1", r#"{"version":1,"play_mode":"sequential","queue":[]}"#),
            (
                "v2",
                r#"{"version":2,"play_mode":"random","queue":[],"history":[]}"#,
            ),
        ] {
            let store = temp_store(&format!("compat-{name}"));
            if let Some(parent) = store.path().parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(store.path(), legacy).unwrap();

            let target = state();
            // 空队列 + 空空闲歌单 → 恢复 0 条，但不报错
            assert_eq!(store.restore(&target), 0, "{name} 应能正常读取");
            let guard = target.read();
            assert!(guard.playing.is_empty());
            assert!(guard.idle.is_empty());
        }
    }
}
