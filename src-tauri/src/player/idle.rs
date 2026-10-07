//! 空闲歌单（阶段 10a）。
//!
//! ## 它解决什么问题
//! 直播开始前、观众还没来、或者点歌队列播完的空档里，直播间是安静的。
//! 这个模块让主播预先准备一份自己的歌单，**队列空时自动顶上**，
//! 有人点歌就切回点歌队列。
//!
//! ## 与点歌队列的关系
//! | | 点歌队列 `queue` | 空闲歌单 `idle` |
//! |---|---|---|
//! | 来源 | 弹幕 / 主播点歌机 | 主播自己准备 |
//! | 名额限制 | 受弹幕上限约束 | **不受限制** |
//! | 播放模式 | `play_mode` | `idle_mode`（独立） |
//! | 优先级 | **高于**空闲歌单 | 仅在队列空时播 |
//!
//! 两个模式刻意分开：点歌队列用「顺序」符合"先来先唱"，
//! 空闲歌单常设成随机或列表循环。
//!
//! ## 选取逻辑为什么抽成纯函数
//! `idle_next_index` / `shuffle_pick` 是纯函数，不碰锁也不碰 IO，
//! 这样"随机不重复上一首""列表循环回到开头""顺序播完就停"这些边界
//! 都能直接单测，而不用起一个播放器。

use crate::models::{IdleMode, QueueItem};

/// 取下一首要播的空闲歌曲下标。
///
/// 返回 `None` 表示**这一轮空闲歌单结束了**（只有顺序播放会这样）。
///
/// - `cursor` 是当前播到第几首（上一首播完后的下一个位置）
/// - `last_index` 是刚刚播过的那首的下标，用于随机模式避免立刻重复
pub fn idle_next_index(
    mode: IdleMode,
    len: usize,
    cursor: usize,
    last_index: Option<usize>,
    random: u64,
) -> Option<usize> {
    if len == 0 {
        return None;
    }
    match mode {
        IdleMode::Sequential => {
            // 播到末尾就停，等下一次主动触发（比如再加歌）
            if cursor >= len {
                None
            } else {
                Some(cursor)
            }
        }
        IdleMode::LoopAll => Some(cursor % len),
        IdleMode::LoopOne => Some(last_index.unwrap_or(0).min(len - 1)),
        IdleMode::Shuffle => Some(shuffle_pick(len, last_index, random)),
    }
}

/// 随机取一个下标，**尽量不等于** `last_index`。
///
/// 单曲数只有 1 首时无法避免重复（只有这一首），此时直接返回 0。
/// 多首时用取模偏移的方式：在 `len - 1` 个"其它"里挑，
/// 这样不会出现"随机到同一首"的突兀感，也不需要循环重试。
pub fn shuffle_pick(len: usize, last_index: Option<usize>, random: u64) -> usize {
    if len <= 1 {
        return 0;
    }
    let pick = (random % (len as u64 - 1)) as usize;
    match last_index {
        // 把候选集合看成"去掉上一首之后的 len-1 首"，
        // 下标 >= last 时整体后移一位即可映射回原下标
        Some(last) if last < len => {
            if pick >= last {
                pick + 1
            } else {
                pick
            }
        }
        _ => (random % len as u64) as usize,
    }
}

/// 判断给定的条目是否在空闲歌单里（按 id）。
pub fn contains(idle: &[QueueItem], id: uuid::Uuid) -> bool {
    idle.iter().any(|item| item.id == id)
}

/// 该不该在「有人点歌」时立刻打断当前的空闲歌曲。
///
/// 条件缺一不可：
///  1. 当前播的**确实是空闲歌单**的歌（正在播点歌的歌当然不该被新点歌打断）；
///  2. 策略是「立即切」；
///  3. 点歌队列里**确实有**待播的歌（否则切过去也没东西可播）。
pub fn should_switch_now(
    policy: crate::models::IdleSwitchPolicy,
    current_is_idle: bool,
    queue_len: usize,
) -> bool {
    matches!(policy, crate::models::IdleSwitchPolicy::Immediate) && current_is_idle && queue_len > 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::IdleSwitchPolicy;

    // ── 顺序播放 ─────────────────────────────────────────────────────────

    #[test]
    fn sequential_advances_then_stops_at_end() {
        let mode = IdleMode::Sequential;
        assert_eq!(idle_next_index(mode, 3, 0, None, 0), Some(0));
        assert_eq!(idle_next_index(mode, 3, 1, Some(0), 0), Some(1));
        assert_eq!(idle_next_index(mode, 3, 2, Some(1), 0), Some(2));
        // 播完最后一首就该停
        assert_eq!(idle_next_index(mode, 3, 3, Some(2), 0), None);
    }

    #[test]
    fn empty_playlist_never_yields() {
        for mode in IdleMode::ALL {
            assert_eq!(idle_next_index(mode, 0, 0, None, 0), None, "{mode:?}");
        }
    }

    // ── 列表循环 ─────────────────────────────────────────────────────────

    #[test]
    fn loop_all_wraps_around() {
        let mode = IdleMode::LoopAll;
        assert_eq!(idle_next_index(mode, 3, 2, Some(1), 0), Some(2));
        // 越过末尾回到开头
        assert_eq!(idle_next_index(mode, 3, 3, Some(2), 0), Some(0));
        assert_eq!(idle_next_index(mode, 3, 4, Some(0), 0), Some(1));
    }

    // ── 单曲循环 ─────────────────────────────────────────────────────────

    #[test]
    fn loop_one_repeats_last_index() {
        let mode = IdleMode::LoopOne;
        assert_eq!(idle_next_index(mode, 5, 0, Some(3), 0), Some(3));
        // 没有上一首时从第一首开始
        assert_eq!(idle_next_index(mode, 5, 0, None, 0), Some(0));
        // 越界的 last_index 要被夹住，不能 panic
        assert_eq!(idle_next_index(mode, 3, 0, Some(99), 0), Some(2));
    }

    // ── 随机播放 ─────────────────────────────────────────────────────────

    #[test]
    fn shuffle_never_repeats_previous_when_multiple_songs() {
        // 这是随机模式最容易出的体验问题：连续放同一首。
        // 遍历一批随机种子，确认结果永远不等于上一首。
        for random in 0u64..200 {
            let pick = shuffle_pick(5, Some(2), random);
            assert!(pick < 5, "下标越界：{pick}");
            assert_ne!(pick, 2, "random={random} 时重复了上一首");
        }
    }

    #[test]
    fn shuffle_single_song_returns_zero() {
        // 只有一首时无法避免重复，但必须返回合法下标而不是越界
        for random in 0u64..50 {
            assert_eq!(shuffle_pick(1, Some(0), random), 0);
        }
    }

    #[test]
    fn shuffle_without_history_covers_all_slots() {
        // 没有上一首时应当能取到所有下标（不遗漏）
        let mut seen = std::collections::HashSet::new();
        for random in 0u64..100 {
            seen.insert(shuffle_pick(4, None, random));
        }
        assert_eq!(seen.len(), 4, "应覆盖全部下标，实际 {seen:?}");
    }

    #[test]
    fn shuffle_can_reach_every_other_song() {
        // 排除上一首之后，仍应能取到其余所有歌
        let mut seen = std::collections::HashSet::new();
        for random in 0u64..200 {
            seen.insert(shuffle_pick(4, Some(1), random));
        }
        assert_eq!(seen, [0usize, 2, 3].into_iter().collect());
    }

    // ── 切换策略 ─────────────────────────────────────────────────────────

    #[test]
    fn switch_now_requires_all_three_conditions() {
        // 全部满足
        assert!(should_switch_now(IdleSwitchPolicy::Immediate, true, 1));
        // 正在播点歌队列的歌 -> 不该被新点歌打断
        assert!(!should_switch_now(IdleSwitchPolicy::Immediate, false, 1));
        // 策略是「放完再切」
        assert!(!should_switch_now(IdleSwitchPolicy::AfterCurrent, true, 1));
        // 队列里其实没歌 -> 切过去也没用
        assert!(!should_switch_now(IdleSwitchPolicy::Immediate, true, 0));
    }

    #[test]
    fn idle_mode_labels_are_distinct() {
        let labels: Vec<_> = IdleMode::ALL.iter().map(|m| m.label()).collect();
        let unique: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(labels.len(), unique.len(), "模式名称不应重复：{labels:?}");
        // 需求里点名的四种模式都要在
        for want in ["顺序播放", "列表循环", "单曲循环", "随机播放"] {
            assert!(labels.contains(&want), "缺少模式：{want}");
        }
    }
}
