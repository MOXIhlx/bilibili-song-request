//! 点歌黑名单。
//!
//! 需求：把某首歌拉黑后，**弹幕点这首歌直接不搜索、不入队**；
//! 但主播通过点歌机可以无视黑名单（用于自己点被拉黑的歌）。
//!
//! ## 为什么匹配用「歌名 + 歌手」
//! 只按歌名会误伤同名歌（比如很多歌都叫《告白气球》的翻唱/同名曲）。
//! 所以条目的语义是：
//!
//! | 条目 | 命中范围 |
//! |------|---------|
//! | 歌名 + 歌手都有 | 两者都匹配才算命中（最精确） |
//! | 只有歌名 | 只要歌名匹配就命中（用于拉黑所有版本） |
//!
//! 匹配前统一做归一化（小写、去空白、去全角括号等），
//! 与搜索打分用的是同一套 `music::scoring::normalize`，避免两处规则不一致。

use serde::{Deserialize, Serialize};

/// 一条黑名单记录。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlacklistEntry {
    /// 歌名（必填）。
    pub title: String,
    /// 歌手（可选；留空表示不限制歌手）。
    #[serde(default)]
    pub artist: String,
    /// 拉黑时间（RFC3339，仅用于界面展示与排序）。
    #[serde(default)]
    pub added_at: Option<String>,
    /// 备注（谁拉的、为什么），可选。
    #[serde(default)]
    pub note: Option<String>,
}

impl BlacklistEntry {
    /// 新建一条（自动裁剪空白）。
    pub fn new(title: impl Into<String>, artist: impl Into<String>) -> Self {
        Self {
            title: title.into().trim().to_string(),
            artist: artist.into().trim().to_string(),
            added_at: Some(chrono::Utc::now().to_rfc3339()),
            note: None,
        }
    }

    /// 条目是否有效（歌名不能为空）。
    pub fn is_valid(&self) -> bool {
        !self.title.trim().is_empty()
    }
}

/// 待匹配的点歌请求（歌名 + 歌手）。
pub struct BlacklistQuery<'a> {
    pub title: &'a str,
    pub artist: &'a str,
}

impl<'a> BlacklistQuery<'a> {
    pub fn new(title: &'a str, artist: &'a str) -> Self {
        Self { title, artist }
    }
}

/// 在列表里查找命中项。
///
/// 返回第一个命中的条目，便于界面提示「被《xx - yy》拉黑」。
pub fn find<'a>(
    list: &'a [BlacklistEntry],
    query: &BlacklistQuery<'_>,
) -> Option<&'a BlacklistEntry> {
    list.iter()
        .find(|entry| matches(entry, query))
}

/// 单条命中判断。
pub fn matches(entry: &BlacklistEntry, query: &BlacklistQuery<'_>) -> bool {
    if !entry.is_valid() {
        return false;
    }
    let entry_title = crate::music::scoring::normalize(&entry.title);
    let query_title = crate::music::scoring::normalize(query.title);
    if entry_title.is_empty() || query_title.is_empty() {
        return false;
    }
    if entry_title != query_title {
        return false;
    }

    // 没写歌手 = 拉黑这首歌的所有版本
    let entry_artist = crate::music::scoring::normalize(&entry.artist);
    if entry_artist.is_empty() {
        return true;
    }

    let query_artist = crate::music::scoring::normalize(query.artist);
    if query_artist.is_empty() {
        // 观众没写歌手，但条目限定了歌手——无法判断，保守起见**不命中**，
        // 否则会把同名但不同歌手的歌一起挡掉。
        return false;
    }

    // 歌手写成多个（`周杰伦/费玉清`）时，任意一个对上即可
    entry_artist
        .split('/')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .any(|part| query_artist.contains(part) || part.contains(&query_artist))
}

/// 从列表里移除与给定查询同歌名（+歌手若给定）的条目。
///
/// 返回移除的条数。
pub fn remove(list: &mut Vec<BlacklistEntry>, title: &str, artist: &str) -> usize {
    let query = BlacklistQuery::new(title, artist);
    let before = list.len();
    list.retain(|entry| !matches(entry, &query));
    before - list.len()
}

/// 追加一条；若已存在同样的条目则不重复添加。
///
/// 返回是否真的新增了。
pub fn add(list: &mut Vec<BlacklistEntry>, entry: BlacklistEntry) -> bool {
    if !entry.is_valid() {
        return false;
    }
    let query = BlacklistQuery::new(&entry.title, &entry.artist);
    if find(list, &query).is_some() {
        return false;
    }
    list.push(entry);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q<'a>(title: &'a str, artist: &'a str) -> BlacklistQuery<'a> {
        BlacklistQuery::new(title, artist)
    }

    #[test]
    fn exact_title_and_artist_hits() {
        let list = vec![BlacklistEntry::new("晴天", "周杰伦")];
        assert!(find(&list, &q("晴天", "周杰伦")).is_some());
    }

    #[test]
    fn same_title_other_artist_does_not_hit() {
        // 条目限定了歌手，同名不同歌手不该被挡
        let list = vec![BlacklistEntry::new("晴天", "周杰伦")];
        assert!(find(&list, &q("晴天", "某某翻唱")).is_none());
    }

    #[test]
    fn title_only_entry_blocks_all_versions() {
        let list = vec![BlacklistEntry::new("口水歌", "")];
        assert!(find(&list, &q("口水歌", "谁唱的都行")).is_some());
        assert!(find(&list, &q("口水歌", "")).is_some());
    }

    #[test]
    fn entry_with_artist_does_not_block_when_request_omits_artist() {
        // 观众只写了歌名，而条目限定了歌手 —— 保守不命中，避免误伤同名歌
        let list = vec![BlacklistEntry::new("晴天", "周杰伦")];
        assert!(find(&list, &q("晴天", "")).is_none());
    }

    #[test]
    fn normalization_ignores_case_space_and_brackets() {
        let list = vec![BlacklistEntry::new("Hello World", "Adele")];
        assert!(find(&list, &q("hello  world", "adele")).is_some());
    }

    #[test]
    fn multi_artist_entry_matches_any_part() {
        let list = vec![BlacklistEntry::new("千里之外", "周杰伦/费玉清")];
        assert!(find(&list, &q("千里之外", "费玉清")).is_some());
        assert!(find(&list, &q("千里之外", "周杰伦")).is_some());
        assert!(find(&list, &q("千里之外", "别人")).is_none());
    }

    #[test]
    fn empty_title_entry_is_invalid_and_never_hits() {
        let list = vec![BlacklistEntry::new("   ", "周杰伦")];
        assert!(!list[0].is_valid());
        assert!(find(&list, &q("任意", "周杰伦")).is_none());
    }

    #[test]
    fn add_skips_duplicates() {
        let mut list = Vec::new();
        assert!(add(&mut list, BlacklistEntry::new("晴天", "周杰伦")));
        assert!(!add(&mut list, BlacklistEntry::new("晴天", "周杰伦")));
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn add_rejects_invalid_entry() {
        let mut list = Vec::new();
        assert!(!add(&mut list, BlacklistEntry::new("", "周杰伦")));
        assert!(list.is_empty());
    }

    #[test]
    fn remove_matches_by_same_rule_and_reports_count() {
        let mut list = vec![
            BlacklistEntry::new("晴天", "周杰伦"),
            BlacklistEntry::new("稻香", "周杰伦"),
        ];
        assert_eq!(remove(&mut list, "晴天", "周杰伦"), 1);
        assert_eq!(list.len(), 1);
        // 再删一次已经没有了
        assert_eq!(remove(&mut list, "晴天", "周杰伦"), 0);
    }
}
