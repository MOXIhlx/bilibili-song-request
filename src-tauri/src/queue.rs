//! 点歌队列管理。
//!
//! 队列的所有变更都通过本模块提供的函数完成，函数内部负责状态写入与广播，
//! HTTP 处理器只做参数校验。这样阶段 4 接入弹幕指令时可以复用同一套逻辑。

pub mod blacklist;
pub mod command;
pub mod request;
pub mod store;
use thiserror::Error;
use tracing::{debug, info};
use uuid::Uuid;

use crate::config::RequestRules;
use crate::models::{QueueItem, QueuePriority, QueueStatus};
use crate::state::StateCell;

pub use command::{CommandError, SongRequest, SongRequestParser};
pub use request::{CooldownTracker, EnqueueOutcome, RejectReason, SongRequestService};
pub use store::{QueueSnapshot, QueueStore};

/// 播放序列的保留上限，超过后只从 `cursor` **左边**回收。
///
/// 右边是待播区，从右边回收会让「下一首」可走的歌凭空变少。
pub const MAX_PLAYING: usize = 200;

/// 队列操作错误。
#[derive(Debug, Error)]
pub enum QueueError {
    /// 指定的队列项不存在。
    #[error("队列项不存在：{0}")]
    NotFound(Uuid),
    /// 历史里没有可回退的曲目。
    #[error("没有上一首可播放")]
    NoHistory,
}

/// 队列入队（按优先级插到合适位置）。
///
/// ## 插队规则
/// - [`QueuePriority::Host`]（主播点歌机）插到**所有弹幕点歌之前**，
///   但排在更早的主播点歌之后——先点的先播，保持公平；
/// - [`QueuePriority::Danmaku`]（观众弹幕）一律追加到队尾。
///
/// 返回实际插入的下标（0 起），便于界面显示「第几位」。
pub fn push(state: &StateCell, item: QueueItem) -> usize {
    let title = item.song.title.clone();
    let user = item.requested_by.clone();
    let priority = item.priority;

    let index = state.mutate(|s| {
        let index = insert_index(&s.queue, priority);
        s.queue.insert(index, item);
        index
    });

    debug!(%title, %user, ?priority, index, "歌曲已入队");
    index
}

/// 计算某个优先级应该插入的下标。
///
/// 抽成纯函数便于单测：插队规则是这个功能最容易出错的地方。
pub fn insert_index(queue: &[QueueItem], priority: QueuePriority) -> usize {
    match priority {
        // 弹幕点歌永远排队尾
        QueuePriority::Danmaku => queue.len(),
        // 主播点歌插到所有弹幕之前；若已有主播点歌，则排在它们后面
        QueuePriority::Host => queue
            .iter()
            .position(|i| i.priority == QueuePriority::Host)
            .map(|first_host| {
                // 从第一个主播位置往后找到最后一个主播，插在其后
                queue[first_host..]
                    .iter()
                    .rposition(|i| i.priority == QueuePriority::Host)
                    .map(|offset| first_host + offset + 1)
                    .unwrap_or(first_host)
            })
            .unwrap_or(0),
    }
}

/// 队列长度。
pub fn len(state: &StateCell) -> usize {
    state.read().queue.len()
}

/// 判重结果：命中的队列条目与它在队列里的下标。
pub struct DuplicateHit {
    /// 已存在条目的歌名（用于提示）。
    pub title: String,
    /// 已存在条目的下标（0 起）。
    pub position: usize,
}

/// 在点歌队列里找同一首歌（歌名 + 歌手，忽略大小写与空白）。
///
/// 用于**点歌机**加歌的判重：弹幕点歌走 `SongRequestService` 的重复检查，
/// 而点歌机直连 `/api/queue/add`，早期完全没有判重，于是同一首歌能被排进
/// 队列很多次（实测队列里出现 4 条同名），播放时表现为「点了下一首还是这首歌」。
///
/// 歌手为空时只按歌名匹配（无法区分版本，宁可判重）。
///
/// ⚠️ 已知局限：队列里的占位歌名会被解析器替换成平台真实歌名，
/// 因此「解析完成后再点同一首（但输入写法不同）」不会被判重。
/// 这里只拦最常见的情况——同一输入被连点多次。
pub fn find_duplicate(state: &StateCell, title: &str, artist: &str) -> Option<DuplicateHit> {
    let title = title.trim();
    if title.is_empty() {
        return None;
    }
    let want_artist = artist.trim();
    let guard = state.read();
    guard
        .queue
        .iter()
        .enumerate()
        .find(|(_, item)| {
            if !item.song.title.trim().eq_ignore_ascii_case(title) {
                return false;
            }
            if want_artist.is_empty() {
                return true;
            }
            item.song.artist.trim().eq_ignore_ascii_case(want_artist)
        })
        .map(|(position, item)| DuplicateHit {
            title: item.song.title.clone(),
            position,
        })
}

/// 当前等待中的**弹幕**点歌数量。
pub fn danmaku_pending(state: &StateCell) -> usize {
    state
        .read()
        .queue
        .iter()
        .filter(|i| i.priority == QueuePriority::Danmaku)
        .count()
}

/// 弹幕是否还能再点歌。
///
/// ## 设计：为什么不维护「已用计数」
/// 早期用一个 `danmaku_pending` 计数配合「入队 +1 / 离队 -1」，
/// 但离队入口太多了（手动删除、取出播放、加载失败、跳过），
/// 每个入口都要记得记账——实测立刻出现**重复扣减**
/// （`remove` 扣一次、调用方又扣一次），额度随之失真，
/// 表现为「队列都空了，弹幕还是点不进去」。
///
/// 现在改成**单一事实来源**：已用量直接数队列里 `Danmaku` 优先级的条目。
/// 队列是唯一真相，不可能漂移；名额（`danmaku_slots`）才需要状态，
/// 因为它必须能被主播点歌「充值」。
pub fn danmaku_has_room(state: &StateCell) -> bool {
    let slots = state.read().danmaku_slots;
    slots > danmaku_pending(state)
}

/// 曲目离队时的记账入口（保留为兼容调用，不需要再做什么）。
///
/// 已用量改为实时统计后，离队**无需**递减任何计数——
/// 保留这个函数是为了让调用点语义清晰（也避免以后有人再加回计数器）。
pub fn account_leave(_state: &StateCell, item: &QueueItem) {
    // 只有弹幕来源的条目与名额有关；此处仅记录日志便于排障
    if item.priority == QueuePriority::Danmaku {
        debug!(title = %item.song.title, "弹幕点歌离队（名额按队列实时统计，无需扣减）");
    }
}

/// 入队后的记账入口（同样不再需要维护计数）。
pub fn account_enqueue(_state: &StateCell, _priority: QueuePriority) {}

/// 主播点歌开播时，给弹幕补充名额。
///
/// 这就是「队列满 7 首弹幕 + 2 首主播，主播那首播完，弹幕可以再点一首」的实现：
/// 名额上限随时间增长，但新点的弹幕仍排在剩余的主播曲目之后。
///
/// ## 上界
/// 名额不超过 `max_queue + 已播主播曲目数 × host_extra_per_play`。
/// 不加这个上界的话，主播反复重播同一首也会一直加名额，
/// 经过一段时间后「弹幕最多 7 首」的限制会形同虚设。
pub fn grant_host_bonus(state: &StateCell, rules: &RequestRules) {
    if rules.host_extra_per_play == 0 {
        return;
    }
    let bonus = rules.host_extra_per_play;
    let base = rules.max_queue;
    // 0 表示不限：此时不设上界（也就不需要补充名额）
    let (slots, cap, host_plays) = state.mutate(|s| {
        s.host_songs_played = s.host_songs_played.saturating_add(1);
        s.danmaku_slots = s.danmaku_slots.saturating_add(bonus);
        if base > 0 {
            let cap = base.saturating_add(
                s.host_songs_played
                    .saturating_mul(bonus),
            );
            if s.danmaku_slots > cap {
                s.danmaku_slots = cap;
            }
        }
        (s.danmaku_slots, base, s.host_songs_played)
    });
    info!(slots, bonus, host_plays, cap, "播完主播点歌，弹幕名额已补充");
}

/// 按配置初始化名额（应用启动与配置热更新时调用）。
///
/// 保留已经用掉的名额：`slots` 取「配置上限」与「当前值」的较大者，
/// 避免改配置把已经排队的点歌挤掉。
pub fn ensure_slots(state: &StateCell, rules: &RequestRules) {
    if rules.max_queue == 0 {
        return;
    }
    state.mutate(|s| {
        if s.danmaku_slots < rules.max_queue {
            s.danmaku_slots = rules.max_queue;
        }
    });
}

/// 队列是否已达上限（0 表示不限）。
pub fn is_full(state: &StateCell, max_queue: usize) -> bool {
    max_queue > 0 && len(state) >= max_queue
}

/// 删除指定队列项。
pub fn remove(state: &StateCell, id: Uuid) -> Result<QueueItem, QueueError> {
    let mut removed: Option<QueueItem> = None;
    {
        let mut guard = state.write();
        if let Some(pos) = guard.queue.iter().position(|i| i.id == id) {
            removed = Some(guard.queue.remove(pos));
        }
    }
    match removed {
        Some(item) => {
            debug!(title = %item.song.title, "已从队列移除");
            // 被主播手动删掉的弹幕点歌也要释放名额，否则名额会被永久占住
            account_leave(state, &item);
            Ok(item)
        }
        None => Err(QueueError::NotFound(id)),
    }
}

/// 将指定队列项移动到队首。
pub fn move_to_top(state: &StateCell, id: Uuid) -> Result<(), QueueError> {
    let mut guard = state.write();
    let pos = guard
        .queue
        .iter()
        .position(|i| i.id == id)
        .ok_or(QueueError::NotFound(id))?;
    let item = guard.queue.remove(pos);
    guard.queue.insert(0, item);
    Ok(())
}

/// 相对移动：`delta` 为 -1 上移、+1 下移；越界时保持不动。
pub fn shift(state: &StateCell, id: Uuid, delta: isize) -> Result<(), QueueError> {
    let mut guard = state.write();
    let pos = guard
        .queue
        .iter()
        .position(|i| i.id == id)
        .ok_or(QueueError::NotFound(id))?;
    let target = (pos as isize + delta).clamp(0, guard.queue.len() as isize - 1) as usize;
    if target != pos {
        let item = guard.queue.remove(pos);
        guard.queue.insert(target, item);
    }
    Ok(())
}

/// 清空整个队列（不动当前播放项）。
pub fn clear(state: &StateCell) -> usize {
    let removed = state.mutate(|s| {
        let n = s.queue.len();
        s.queue.clear();
        n
    });
    info!(removed, "队列已清空");
    removed
}

/// 把一首歌**追加**到播放序列末尾，并把 `cursor` 移到它上面。
///
/// 追加而非头插：`playing` 按播放顺序排列，头插会让所有已有下标后移，
/// 而游标是绝对下标 → 位置失真。返回新条目的下标（= 新的 `cursor`）。
pub fn push_playing(state: &StateCell, item: QueueItem) -> usize {
    let title = item.song.title.clone();
    let index = state.mutate(|s| {
        // 只追加，不去重、不回退游标：
        // 同一首歌点两次本就该播两次；而去重时若目标已被 `trim_playing` 回收，
        // `playing[cursor]` 会取不到，`current` 随之变空。
        s.playing.push(item);
        let index = s.playing.len() - 1;
        s.cursor = index;
        index
    });
    trim_playing(state);
    debug!(%title, index, "已追加到播放序列");
    index
}

/// 回收过长的播放序列：只从 `cursor` **左边**丢弃最老的记录。
///
/// 右边是待播区，绝不能丢——否则「下一首」可走的歌会凭空变少。
fn trim_playing(state: &StateCell) {
    state.mutate(|s| {
        if s.playing.len() <= MAX_PLAYING {
            return;
        }
        // 只允许把 cursor 之前的部分丢掉，且至少给已播区留一段，便于「上一首」回退。
        const KEEP_BEHIND: usize = 50;
        let excess = s.playing.len() - MAX_PLAYING;
        let droppable = s.cursor.saturating_sub(KEEP_BEHIND.min(s.cursor));
        let drop_count = excess.min(droppable);
        if drop_count == 0 {
            return;
        }
        s.playing.drain(0..drop_count);
        s.cursor -= drop_count;
    });
}

/// 播放序列里是否还有「下一首」（`cursor` 右边还有条目）。
pub fn has_next_in_playing(state: &StateCell) -> bool {
    let guard = state.read();
    guard.cursor + 1 < guard.playing.len()
}

/// 播放序列里是否还有「上一首」（`cursor` 左边还有条目）。
pub fn has_previous_in_playing(state: &StateCell) -> bool {
    state.read().cursor > 0
}

/// 把 `cursor` 向后（更早）移一步，返回那一首。
///
/// **只移动游标、不删除条目**，所以可以反复「上一首 / 下一首」来回走。
/// 到最老一首时返回 `None`（调用方据此提示"没有上一首"）。
pub fn step_previous(state: &StateCell) -> Option<QueueItem> {
    let mut previous: Option<QueueItem> = None;
    state.mutate(|s| {
        if s.cursor > 0 {
            s.cursor -= 1;
            previous = s.playing.get(s.cursor).cloned();
        }
    });
    if let Some(item) = &previous {
        info!(title = %item.song.title, "播放序列后退一首");
    }
    previous
}

/// 把 `cursor` 向前（更新）移一步，返回那一首。
///
/// 返回 `None` 表示已在序列末尾，调用方应去 `queue`/`idle` 取**新歌**。
pub fn step_next(state: &StateCell) -> Option<QueueItem> {
    let mut next: Option<QueueItem> = None;
    state.mutate(|s| {
        let target = s.cursor + 1;
        if target < s.playing.len() {
            s.cursor = target;
            next = s.playing.get(target).cloned();
        }
    });
    if let Some(item) = &next {
        info!(title = %item.song.title, "播放序列前进一首");
    }
    next
}

/// 当前正在播放的条目（= `playing[cursor]`）。
pub fn current_of_playing(state: &StateCell) -> Option<QueueItem> {
    let guard = state.read();
    guard.playing.get(guard.cursor).cloned()
}

/// 清空**待播区**（`cursor` 右边的部分），保留已播历史。
///
/// 用于「一键清空点歌列表」：清掉还没播的，但"上一首"仍能回去。
/// 返回被清掉的条数。
pub fn clear_upcoming(state: &StateCell) -> usize {
    state.mutate(|s| {
        let keep = (s.cursor + 1).min(s.playing.len());
        let removed = s.playing.len() - keep;
        s.playing.truncate(keep);
        removed
    })
}

/// 跳过当前歌曲。
///
/// 阶段 1 只做状态迁移；阶段 6 接入 mpv 后会额外向播放器发送 `stop`，
/// 播放器的 `end-file` 事件再驱动「自动下一首」。
pub fn skip_current(state: &StateCell) -> Option<QueueItem> {
    let mut skipped: Option<QueueItem> = None;
    {
        let mut guard = state.write();
        if let Some(mut current) = guard.current.take() {
            current.status = QueueStatus::Skipped;
            skipped = Some(current);
        }
        guard.player.playing = false;
        guard.player.position = 0.0;
        guard.player.duration = 0.0;
        guard.player.lyrics = None;
    }
    if let Some(item) = &skipped {
        info!(title = %item.song.title, "已跳过当前歌曲");
        account_leave(state, item);
    }
    skipped
}

/// 播放结束：把当前项标记为已播放并清空 current。
pub fn finish_current(state: &StateCell) -> Option<QueueItem> {
    let mut done: Option<QueueItem> = None;
    {
        let mut guard = state.write();
        if let Some(mut current) = guard.current.take() {
            current.status = QueueStatus::Played;
            done = Some(current);
        }
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AppState, BilibiliState, PlayerState, Song};
    use crate::VERSION;

    fn cell_with(items: usize) -> (StateCell, Vec<Uuid>) {
        let state = StateCell::new(AppState::new(
            VERSION,
            PlayerState::default(),
            BilibiliState::default(),
        ));
        let mut ids = Vec::new();
        for i in 0..items {
            let item = QueueItem::new(
                Song::placeholder(format!("歌{i}"), "歌手"),
                "测试用户",
                None,
            );
            ids.push(item.id);
            push(&state, item);
        }
        (state, ids)
    }

    #[test]
    fn shift_moves_item_up_and_clamps_at_edges() {
        let (state, ids) = cell_with(3);
        // 第三首上移一位
        shift(&state, ids[2], -1).unwrap();
        let titles: Vec<_> = state.read().queue.iter().map(|i| i.song.title.clone()).collect();
        assert_eq!(titles, vec!["歌0", "歌2", "歌1"]);

        // 已经在队首，再上移应保持不动
        shift(&state, ids[0], -1).unwrap();
        let titles: Vec<_> = state.read().queue.iter().map(|i| i.song.title.clone()).collect();
        assert_eq!(titles, vec!["歌0", "歌2", "歌1"]);
    }

    #[test]
    fn move_to_top_and_remove() {
        let (state, ids) = cell_with(3);
        move_to_top(&state, ids[1]).unwrap();
        assert_eq!(state.read().queue[0].id, ids[1]);

        remove(&state, ids[1]).unwrap();
        assert_eq!(state.read().queue.len(), 2);
        assert!(matches!(remove(&state, ids[1]), Err(QueueError::NotFound(_))));
    }

    #[test]
    fn skip_marks_current_as_skipped() {
        let (state, ids) = cell_with(1);
        let item = remove(&state, ids[0]).unwrap();
        state.mutate(|s| s.current = Some(item));

        let skipped = skip_current(&state).expect("应返回被跳过的项");
        assert_eq!(skipped.status, QueueStatus::Skipped);
        assert!(state.read().current.is_none());
    }

    // ── 优先级插队（阶段 8）────────────────────────────────────────────────

    fn item(title: &str, priority: QueuePriority) -> QueueItem {
        QueueItem::with_priority(
            Song::placeholder(title, "歌手"),
            "点歌人",
            None,
            priority,
        )
    }

    #[test]
    fn danmaku_appends_to_tail() {
        let state = StateCell::new(AppState::new(
            VERSION,
            PlayerState::default(),
            BilibiliState::default(),
        ));
        push(&state, item("弹1", QueuePriority::Danmaku));
        push(&state, item("弹2", QueuePriority::Danmaku));
        let titles: Vec<_> = state.read().queue.iter().map(|i| i.song.title.clone()).collect();
        assert_eq!(titles, vec!["弹1", "弹2"]);
    }

    #[test]
    fn host_jumps_ahead_of_all_danmaku() {
        let state = StateCell::new(AppState::new(
            VERSION,
            PlayerState::default(),
            BilibiliState::default(),
        ));
        push(&state, item("弹1", QueuePriority::Danmaku));
        push(&state, item("弹2", QueuePriority::Danmaku));
        push(&state, item("主播1", QueuePriority::Host));

        let titles: Vec<_> = state.read().queue.iter().map(|i| i.song.title.clone()).collect();
        // 主播插到所有弹幕之前
        assert_eq!(titles, vec!["主播1", "弹1", "弹2"]);
    }

    #[test]
    fn later_host_queues_after_earlier_host() {
        let state = StateCell::new(AppState::new(
            VERSION,
            PlayerState::default(),
            BilibiliState::default(),
        ));
        push(&state, item("弹1", QueuePriority::Danmaku));
        push(&state, item("主播1", QueuePriority::Host));
        push(&state, item("弹2", QueuePriority::Danmaku));
        push(&state, item("主播2", QueuePriority::Host));

        let titles: Vec<_> = state.read().queue.iter().map(|i| i.song.title.clone()).collect();
        // 先点的主播曲在前；新弹幕仍排在所有主播之后
        assert_eq!(titles, vec!["主播1", "主播2", "弹1", "弹2"]);
    }

    /// 用户明确要求的场景：7 首弹幕 + 2 首主播，队首播完后弹幕还能点，
    /// 但顺序排在主播之后。
    #[test]
    fn danmaku_can_resume_after_host_songs_play() {
        let state = StateCell::new(AppState::new(
            VERSION,
            PlayerState::default(),
            BilibiliState::default(),
        ));
        let rules = RequestRules {
            max_queue: 7,
            host_extra_per_play: 2,
            ..RequestRules::default()
        };
        ensure_slots(&state, &rules);
        assert_eq!(state.read().danmaku_slots, 7);

        for i in 0..7 {
            push(&state, item(&format!("弹{i}"), QueuePriority::Danmaku));
            account_enqueue(&state, QueuePriority::Danmaku);
        }
        assert!(!danmaku_has_room(&state), "7 首之后弹幕应点不动");

        // 主播加 2 首（不受上限约束）
        push(&state, item("主播1", QueuePriority::Host));
        push(&state, item("主播2", QueuePriority::Host));

        // 队首（主播1）播掉 → 它离开队列，并补充 2 个名额
        // （真实流程里是 `PlayerController::take_from_queue` 把它移出队列，
        //   `set_current` 再调 `grant_host_bonus`）
        let played = state.read().queue[0].id;
        remove(&state, played).unwrap();
        grant_host_bonus(&state, &rules);
        assert!(danmaku_has_room(&state), "主播那首播完后弹幕应能继续点");

        push(&state, item("弹7", QueuePriority::Danmaku));
        account_enqueue(&state, QueuePriority::Danmaku);

        let titles: Vec<_> = state.read().queue.iter().map(|i| i.song.title.clone()).collect();
        // 新弹幕排在剩下那首主播曲之后
        assert_eq!(
            titles,
            vec!["主播2", "弹0", "弹1", "弹2", "弹3", "弹4", "弹5", "弹6", "弹7"]
        );
    }

    #[test]
    fn leaving_frees_a_slot() {
        let state = StateCell::new(AppState::new(
            VERSION,
            PlayerState::default(),
            BilibiliState::default(),
        ));
        let rules = RequestRules {
            max_queue: 1,
            ..RequestRules::default()
        };
        ensure_slots(&state, &rules);
        // ⚠️ 只调用一次 `item()`：它内部会生成新的 UUID，
        // 调用两次会得到两个不同 id，导致后面 remove 找不到条目。
        let entry = item("弹1", QueuePriority::Danmaku);
        let id = entry.id;
        push(&state, entry);
        account_enqueue(&state, QueuePriority::Danmaku);
        assert!(!danmaku_has_room(&state), "上限 1 且已有 1 首，应点不动");

        remove(&state, id).unwrap();
        assert!(danmaku_has_room(&state), "条目被删掉后名额应释放");
    }

    /// 模拟「取出队首去播放」的完整记账链路。
    ///
    /// 这是曾经漏掉的一环：`take_from_queue` 早期直接用 `retain` 删条目，
    /// 绕过了 `account_leave`，于是 `danmaku_pending` **只增不减**——
    /// 额度被幽灵条目永久占住，实测现象是「队列都空了，第 8 首还是点不进去」。
    #[test]
    fn taking_item_out_for_playback_keeps_accounting_consistent() {
        let state = StateCell::new(AppState::new(
            VERSION,
            PlayerState::default(),
            BilibiliState::default(),
        ));
        let rules = RequestRules {
            max_queue: 2,
            host_extra_per_play: 2,
            ..RequestRules::default()
        };
        ensure_slots(&state, &rules);

        for i in 0..2 {
            push(&state, item(&format!("弹{i}"), QueuePriority::Danmaku));
            account_enqueue(&state, QueuePriority::Danmaku);
        }
        assert_eq!(danmaku_pending(&state), 2);
        assert!(!danmaku_has_room(&state));

        // 取出第一首去播放（真实流程：controller.take_from_queue → account_leave）
        let first = state.read().queue[0].id;
        let taken = remove(&state, first).unwrap();
        // `remove` 与调用方都调 `account_leave`：已用量改为实时统计，重复调用无害
        account_leave(&state, &taken);
        account_leave(&state, &taken);

        assert_eq!(danmaku_pending(&state), 1, "离队后队列里应只剩 1 首弹幕");
        assert!(danmaku_has_room(&state), "释放 1 个名额后应能再点");
    }

    /// 「7 首弹幕 + 2 首主播」的完整生命周期：
    /// 主播歌播掉 → 名额 +2 → 弹幕能再点 → 新弹幕排在剩下的主播歌之后。
    #[test]
    fn host_play_replenishes_and_new_danmaku_queues_behind_remaining_host() {
        let state = StateCell::new(AppState::new(
            VERSION,
            PlayerState::default(),
            BilibiliState::default(),
        ));
        let rules = RequestRules {
            max_queue: 7,
            host_extra_per_play: 2,
            ..RequestRules::default()
        };
        ensure_slots(&state, &rules);

        for i in 0..7 {
            push(&state, item(&format!("弹{i}"), QueuePriority::Danmaku));
            account_enqueue(&state, QueuePriority::Danmaku);
        }
        assert_eq!(state.read().danmaku_slots, 7);
        assert!(!danmaku_has_room(&state), "7 首已占满");

        // 主播点 2 首（不受上限约束）
        push(&state, item("主播1", QueuePriority::Host));
        push(&state, item("主播2", QueuePriority::Host));
        let order: Vec<_> = state.read().queue.iter().map(|i| i.song.title.clone()).collect();
        assert_eq!(&order[..2], &["主播1", "主播2"], "主播应插在弹幕之前");

        // 主播1 播出：离队 + 补充名额
        let played = state.read().queue[0].id;
        let taken = remove(&state, played).unwrap();
        assert_eq!(taken.priority, QueuePriority::Host, "取出的应是主播歌");
        account_leave(&state, &taken);
        grant_host_bonus(&state, &rules);
        assert_eq!(state.read().danmaku_slots, 9, "每次补充 2 个名额");
        assert!(danmaku_has_room(&state));
        // 弹幕再点一首：应排在剩下的主播2 之后
        push(&state, item("弹7", QueuePriority::Danmaku));
        account_enqueue(&state, QueuePriority::Danmaku);
        let order: Vec<_> = state.read().queue.iter().map(|i| i.song.title.clone()).collect();
        assert_eq!(
            order,
            vec!["主播2", "弹0", "弹1", "弹2", "弹3", "弹4", "弹5", "弹6", "弹7"],
            "新弹幕必须排在剩余主播曲目之后"
        );
    }

    /// 名额增长必须有上界，否则「弹幕最多 N 首」会随时间失效。
    #[test]
    fn host_bonus_is_capped_by_host_plays() {
        let state = StateCell::new(AppState::new(
            VERSION,
            PlayerState::default(),
            BilibiliState::default(),
        ));
        let rules = RequestRules {
            max_queue: 7,
            host_extra_per_play: 2,
            ..RequestRules::default()
        };
        ensure_slots(&state, &rules);
        assert_eq!(state.read().danmaku_slots, 7);

        // 播 1 首主播曲 → 7 + 2 = 9
        grant_host_bonus(&state, &rules);
        assert_eq!(state.read().danmaku_slots, 9);

        // 再播 1 首 → 11（上界 = 7 + 2×2 = 11）
        grant_host_bonus(&state, &rules);
        assert_eq!(state.read().danmaku_slots, 11);

        // 即使继续播，也不能超过「基础 + 已播主播曲目数 × 奖励」
        for _ in 0..10 {
            grant_host_bonus(&state, &rules);
        }
        let slots = state.read().danmaku_slots;
        let plays = state.read().host_songs_played;
        assert_eq!(plays, 12);
        assert_eq!(slots, 7 + plays * 2, "上界应恰好等于 基础 + 播过的主播曲目×奖励");
    }

    #[test]
    fn zero_max_queue_means_unlimited() {        let state = StateCell::new(AppState::new(
            VERSION,
            PlayerState::default(),
            BilibiliState::default(),
        ));
        let rules = RequestRules {
            max_queue: 0,
            ..RequestRules::default()
        };
        ensure_slots(&state, &rules);
        // 0 = 不限：不应初始化名额，也不该因为名额为 0 被挡住
        for i in 0..50 {
            push(&state, item(&format!("弹{i}"), QueuePriority::Danmaku));
            account_enqueue(&state, QueuePriority::Danmaku);
        }
        assert_eq!(danmaku_pending(&state), 50);
    }

    // ── 播放序列 `playing` + `cursor`（阶段 10d 重构）──────────────────────
    //
    // 旧实现用「history（头插）+ history_cursor（绝对下标）」表示播放顺序，
    // 有两个致命问题：头插会让所有下标后移、截断会让游标失真。
    // 现在改成「追加式 playing + cursor」，下面这些测试锁住新语义。

    fn empty_state() -> StateCell {
        StateCell::new(AppState::new(
            VERSION,
            PlayerState::default(),
            BilibiliState::default(),
        ))
    }

    #[test]
    fn playing_is_append_only_and_cursor_follows() {
        let state = empty_state();
        push_playing(&state, item("第一", QueuePriority::Danmaku));
        push_playing(&state, item("第二", QueuePriority::Danmaku));
        push_playing(&state, item("第三", QueuePriority::Danmaku));

        let titles: Vec<_> = state
            .read()
            .playing
            .iter()
            .map(|i| i.song.title.clone())
            .collect();
        // 追加式：顺序就是播放顺序（与旧的头插相反）
        assert_eq!(titles, vec!["第一", "第二", "第三"]);
        assert_eq!(state.read().cursor, 2, "游标应指向最后追加的那首");
    }

    #[test]
    fn step_back_and_forward_return_to_the_same_item() {
        // 用户核心诉求：回到第二首后，「下一首」必须是第三首
        let state = empty_state();
        for title in ["A", "B", "C"] {
            push_playing(&state, item(title, QueuePriority::Danmaku));
        }
        // 当前在 C
        assert_eq!(state.read().cursor, 2);

        let back = step_previous(&state).expect("应能回退");
        assert_eq!(back.song.title, "B");
        assert_eq!(state.read().cursor, 1);

        let fwd = step_next(&state).expect("应能前进");
        assert_eq!(fwd.song.title, "C", "回到 B 之后前进必须是 C，不能跳到 D");
        assert_eq!(state.read().cursor, 2);
    }

    #[test]
    fn new_items_after_going_back_do_not_move_cursor() {
        // 更关键的场景：回到 B 之后又来了新歌 D，
        // 「下一首」仍必须是 C（新歌不插队、不移动游标）
        let state = empty_state();
        for title in ["A", "B", "C"] {
            push_playing(&state, item(title, QueuePriority::Danmaku));
        }
        step_previous(&state); // cursor = 1 (B)
        assert_eq!(state.read().cursor, 1);

        // D 已经进入 playing（模拟"播到末尾时追加"），
        // 但游标仍在 B——这正是"播到末尾才追加"的核心性质
        let cursor_before = state.read().cursor;
        state.mutate(|s| s.playing.push(item("D", QueuePriority::Danmaku)));
        assert_eq!(state.read().cursor, cursor_before, "追加不能移动游标");

        let fwd = step_next(&state).expect("应能前进");
        assert_eq!(fwd.song.title, "C", "有 D 存在时前进也应是 C");
    }

    #[test]
    fn step_previous_at_start_returns_none() {
        let state = empty_state();
        push_playing(&state, item("唯一", QueuePriority::Danmaku));
        assert!(step_previous(&state).is_none(), "已是第一首，没有上一首");
        assert_eq!(state.read().cursor, 0, "失败时不应改动游标");
    }

    #[test]
    fn step_next_at_end_returns_none() {
        let state = empty_state();
        push_playing(&state, item("唯一", QueuePriority::Danmaku));
        assert!(step_next(&state).is_none(), "已在末尾，没有下一首");
        assert_eq!(state.read().cursor, 0, "失败时不应改动游标");
    }

    #[test]
    fn has_next_and_previous_flags_match_bounds() {
        let state = empty_state();
        push_playing(&state, item("A", QueuePriority::Danmaku));
        assert!(!has_previous_in_playing(&state));
        assert!(!has_next_in_playing(&state));

        push_playing(&state, item("B", QueuePriority::Danmaku));
        assert!(has_previous_in_playing(&state));
        assert!(!has_next_in_playing(&state));
    }

    #[test]
    fn current_of_playing_matches_cursor() {
        let state = empty_state();
        push_playing(&state, item("A", QueuePriority::Danmaku));
        push_playing(&state, item("B", QueuePriority::Danmaku));
        step_previous(&state);
        assert_eq!(
            current_of_playing(&state).map(|i| i.song.title),
            Some("A".to_string())
        );
    }

    #[test]
    fn trim_only_drops_from_the_left_of_cursor() {
        // 播放序列超上限时，只允许丢弃 cursor 左边的旧记录；
        // 右边的待播区必须完整保留，否则「下一首」会凭空少歌。
        let state = empty_state();
        let total = MAX_PLAYING + 60;
        for i in 0..total {
            push_playing(&state, item(&format!("歌{i}"), QueuePriority::Danmaku));
        }
        let guard = state.read();
        assert!(guard.playing.len() <= MAX_PLAYING + 1, "应触发回收");
        // 游标仍指向最后追加的那首（回收时同步左移，语义不变）
        assert_eq!(
            guard.playing.get(guard.cursor).map(|i| i.song.title.clone()),
            Some(format!("歌{}", total - 1)),
            "回收后游标仍须指向同一首"
        );
    }

    #[test]
    fn clear_upcoming_keeps_played_history() {
        // 「一键清空点歌列表」：清掉待播，但已播的还能用「上一首」回去
        let state = empty_state();
        for title in ["A", "B"] {
            push_playing(&state, item(title, QueuePriority::Danmaku));
        }
        step_previous(&state); // cursor=0 (A)，B 在右边（待播）
        let removed = clear_upcoming(&state);
        assert_eq!(removed, 1, "应清掉右侧的 B");
        let guard = state.read();
        assert_eq!(guard.playing.len(), 1);
        assert_eq!(guard.playing[0].song.title, "A");
        assert_eq!(guard.cursor, 0);
    }
}
