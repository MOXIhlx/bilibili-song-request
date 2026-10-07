//! 搜索结果打分与挑选。
//!
//! ## 为什么需要它
//! 点歌的关键词形如 `歌名 歌手`，但平台搜索**只保证返回一个排好序的列表**，
//! 官方排序偏向热度，并不保证原唱在前。实测点「晴天 周杰伦」时
//! 首条是「晴天(深情版) / Lucky小爱」这类翻唱/改编版本，
//! 于是直接取第一条就会**点原唱却放翻唱**。
//!
//! 这里在本地对候选做一次可解释的打分：歌名匹配越紧、歌手命中越好分越高；
//! 标题/歌手里出现翻唱、伴奏、DJ 版等特征则扣分。挑选时取最高分，
//! 并给出「是否精确命中」的判断，供上层决定要不要去另一个平台再搜一轮。
//!
//! 打分完全是纯函数，便于用单测锁住行为，也方便以后调整权重。

use crate::models::Song;

/// 一个候选的打分结果。
#[derive(Debug, Clone)]
pub struct ScoredSong {
    /// 原始候选。
    pub song: Song,
    /// 总分（越大越好）。
    pub score: i32,
    /// 歌名与歌手**都**精确命中（上层据此判断是否需要跨平台再搜）。
    pub exact: bool,
}

/// 歌名/歌手都用这个字符集归一化后再比较，避免因为空格、全半角、大小写、
/// 常见标点差异而漏判。
///
/// 抽成 `pub` 是为了让**黑名单匹配**复用同一套规则——
/// 两处各写一份迟早会不一致（例如一边去了括号、另一边没去）。
pub fn normalize(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        // 全角转半角（ASCII 可见区）
        let ch = if ('\u{FF01}'..='\u{FF5E}').contains(&ch) {
            char::from_u32(ch as u32 - 0xFEE0).unwrap_or(ch)
        } else if ch == '\u{3000}' {
            ' '
        } else {
            ch
        };
        let lower = ch.to_ascii_lowercase();
        // 丢掉空白与常见分隔/装饰标点：判定「同一首歌」时它们没有信息量
        if lower.is_whitespace()
            || matches!(
                lower,
                '-' | '_'
                    | '.'
                    | ','
                    | '!'
                    | '?'
                    | ':'
                    | ';'
                    | '\''
                    | '"'
                    | '('
                    | ')'
                    | '['
                    | ']'
                    | '【'
                    | '】'
                    | '（'
                    | '）'
                    | '《'
                    | '》'
                    | '·'
                    | '~'
                    | '&'
                    | '/'
                    | '\\'
                    | '+'
                    | '*'
            )
        {
            continue;
        }
        out.push(lower);
    }
    out
}

/// 归一化后的「歌名核心」：去掉常见版本后缀，便于判断是否同一首歌。
///
/// 例如 `晴天(深情版)` → `晴天`，`稻香 - Live` → `稻香`。
/// 只用于**歌名匹配**，不影响翻唱扣分（扣分看的是原始标题）。
fn title_core(title: &str) -> String {
    let lowered = title.to_lowercase();

    // 取最早出现的「版本说明起点」（按字符索引，避免切坏多字节字符）
    let mut cut: Option<usize> = None;
    let mut consider = |byte_index: usize| {
        cut = Some(match cut {
            Some(existing) => existing.min(byte_index),
            None => byte_index,
        });
    };

    // ① 括号类后缀：第一个左括号之后都是版本说明
    for marker in ['(', '（', '[', '【'] {
        if let Some((index, _)) = lowered.char_indices().find(|(_, c)| *c == marker) {
            consider(index);
        }
    }

    // ② 破折号后缀：要求出现在中后段（前面至少 2 个字），
    //    否则会误伤 `M-1`、`X-Ray` 这类本身带连字符的正常歌名
    for marker in [" - ", " – ", "-", "－", "—"] {
        let mut search_from = 0usize;
        while let Some(relative) = lowered[search_from..].find(marker) {
            let index = search_from + relative;
            let prefix_chars = lowered[..index].chars().count();
            if prefix_chars >= 2 {
                consider(index);
                break;
            }
            search_from = index + marker.len();
            if search_from >= lowered.len() {
                break;
            }
        }
    }

    let end = cut.unwrap_or(lowered.len());
    normalize(&lowered[..end])
}

/// 把候选的歌手字段拆成单个歌手名（已归一化）。
///
/// 平台给出的多歌手形式很杂：`周杰伦`、`周杰伦 / 费玉清`、
/// `周杰伦;费玉清`、`周杰伦 feat. 费玉清`、`周杰伦&费玉清`。
fn split_artists(artist: &str) -> Vec<String> {
    // 先把 feat. / ft. / with 这类连接词替换成统一分隔符，再按分隔符切分
    let mut unified = artist.to_string();
    for marker in ["featuring", "feat.", "feat", "ft.", "with"] {
        // 大小写不敏感替换：逐个找并替换为 '/'
        let lowered = unified.to_lowercase();
        let mut result = String::with_capacity(unified.len());
        let mut cursor = 0usize;
        while let Some(relative) = lowered[cursor..].find(marker) {
            let start = cursor + relative;
            let end = start + marker.len();
            result.push_str(&unified[cursor..start]);
            result.push('/');
            cursor = end;
        }
        result.push_str(&unified[cursor..]);
        unified = result;
    }

    unified
        .split(['/', ';', '；', '&', '、', ',', '，', '|'])
        .map(normalize)
        .filter(|part| !part.is_empty())
        .collect()
}

/// 标题里的「非原唱」特征词。命中即扣分，权重按误导程度排序。
const COVER_MARKERS: &[(&str, i32)] = &[
    ("伴奏", -45),
    ("instrumental", -45),
    ("karaoke", -45),
    ("remix", -30),
    ("dj", -30),
    ("翻唱", -40),
    ("cover", -40),
    ("深情版", -35),
    ("女声版", -25),
    ("男声版", -25),
    ("童声", -25),
    ("钢琴版", -25),
    ("吉他版", -25),
    ("纯音乐", -35),
    ("铃声", -40),
    ("铃声版", -40),
    ("片段", -35),
    ("demo", -25),
    ("live", -12),
    ("现场", -12),
    ("remaster", -5),
    ("重置", -8),
    ("高清", -5),
    ("无损", -5),
    ("完整版", -3),
    // 上传者常用的「正式版 / 抖音版 / 网络版」等措辞：
    // 这些**不是**原版发行，而是二次上传的改编/翻录版本。
    // 实测网易云里「青花瓷（正式版）」的歌手是
    // `周杰伦. / 街道办GDC/欧阳耀莹.`——典型的上传者署名，不是原唱。
    ("正式版", -30),
    ("抖音版", -35),
    ("网络版", -30),
    ("加速版", -30),
    ("慢速版", -30),
    ("完整版", -8),
    ("重制版", -20),
    ("重置版", -20),
    ("改编", -35),
    ("串烧", -30),
    ("清唱", -25),
    ("哼唱", -30),
];

/// 歌手名里的「非原唱」特征词。
const COVER_ARTIST_MARKERS: &[(&str, i32)] = &[
    ("lucky小爱", -40),
    ("翻唱", -40),
    ("cover", -30),
    ("群星", -15),
    ("合辑", -15),
    ("网络歌手", -20),
    ("佚名", -25),
];

/// 对单个候选打分。
///
/// `want_title` / `want_artist` 是用户点歌时给出的歌名与歌手（歌手可为空）。
pub fn score_song(song: &Song, want_title: &str, want_artist: &str) -> ScoredSong {
    let want_title_norm = normalize(want_title);
    let want_title_core = title_core(want_title);
    let have_title_norm = normalize(&song.title);
    let have_title_core = title_core(&song.title);

    let mut score = 0i32;

    // ── 歌名匹配 ──────────────────────────────────────────────────────────
    let title_exact = !want_title_norm.is_empty() && have_title_norm == want_title_norm;
    let core_exact =
        !want_title_core.is_empty() && have_title_core == want_title_core;
    let title_contains = !want_title_core.is_empty() && have_title_core.contains(&want_title_core);
    let want_contains_have =
        !want_title_core.is_empty() && want_title_core.contains(&have_title_core) && !have_title_core.is_empty();

    if title_exact {
        // 完全一致的歌名（没有任何版本后缀）
        score += 100;
    } else if core_exact {
        // 去掉版本后缀后一致：还是这首歌，但有后缀说明是某个版本
        score += 70;
    } else if title_contains {
        score += 45;
    } else if want_contains_have {
        score += 30;
    } else {
        // 歌名都不像，基本不是用户要的
        score -= 80;
    }

    // ── 歌手匹配 ──────────────────────────────────────────────────────────
    let want_artist_norm = normalize(want_artist);
    let have_artists = split_artists(&song.artist);
    let artist_specified = !want_artist_norm.is_empty();

    let want_artist_parts = split_artists(want_artist);
    let artist_exact = artist_specified
        && (have_artists.contains(&want_artist_norm)
            || want_artist_parts
                .iter()
                .any(|p| have_artists.contains(p)));

    if artist_specified {
        if artist_exact {
            score += 80;
            // 「精确」但在歌手字段里塞了一堆别人的名字 → 多半是上传者署名
            // （实测：`周杰伦. / 街道办GDC/欧阳耀莹.`）。
            // 用户只点了「周杰伦」，这里却挂着三个名字，属于强负面信号。
            let extras = have_artists
                .iter()
                .filter(|a| {
                    !a.is_empty()
                        && **a != want_artist_norm
                        && !want_artist_parts.iter().any(|p| p == *a)
                })
                .count();
            if extras > 0 {
                // 每多一个额外署名扣一档，最多扣 45，避免直接否决合法合唱
                score -= (extras as i32 * 25).min(45);
            }
        } else if have_artists
            .iter()
            .any(|a| !a.is_empty() && (a.contains(&want_artist_norm) || want_artist_norm.contains(a)))
        {
            // 部分包含（例如「周杰伦」vs「周杰伦乐队」）
            score += 40;
        } else {
            // 用户明确指定了歌手却没匹配上：这是最强的负面信号
            score -= 60;
        }
    }

    // ── 翻唱/伴奏等特征扣分 ───────────────────────────────────────────────
    let title_lower = song.title.to_lowercase();
    for (marker, penalty) in COVER_MARKERS {
        if title_lower.contains(marker) {
            score += penalty;
        }
    }
    let artist_lower = song.artist.to_lowercase();
    for (marker, penalty) in COVER_ARTIST_MARKERS {
        if artist_lower.contains(marker) {
            score += penalty;
        }
    }

    // 「精确命中」= 歌名没有版本后缀 且 歌手对得上。
    // 上层用它决定是否还需要去另一个平台再搜一轮。
    let exact = artist_specified && core_exact && title_exact && artist_exact;

    ScoredSong {
        song: song.clone(),
        score,
        exact,
    }
}

/// 对候选列表打分并按分数降序排列（同分保持平台原顺序，即稳定排序）。
pub fn rank_songs(songs: &[Song], want_title: &str, want_artist: &str) -> Vec<ScoredSong> {
    let mut scored: Vec<ScoredSong> = songs
        .iter()
        .map(|s| score_song(s, want_title, want_artist))
        .collect();
    // 稳定排序：分数相同时不改变平台给的相对顺序
    scored.sort_by(|a, b| b.score.cmp(&a.score));
    scored
}

/// 从候选里挑最好的一个。
pub fn best_song(songs: &[Song], want_title: &str, want_artist: &str) -> Option<ScoredSong> {
    rank_songs(songs, want_title, want_artist).into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::MusicPlatform;

    fn song(title: &str, artist: &str) -> Song {
        Song {
            id: title.to_string(),
            title: title.to_string(),
            artist: artist.to_string(),
            platform: MusicPlatform::Netease,
            duration: 200,
            cover_url: None,
            album: None,
            source: Default::default(),
            source_error: None,
        }
    }

    #[test]
    fn normalize_strips_width_space_and_punctuation() {
        assert_eq!(normalize("周杰伦"), normalize("周 杰 伦"));
        assert_eq!(normalize("晴天"), normalize("晴天"));
        // 全角字母数字转半角
        assert_eq!(normalize("ＡＢＣ"), "abc");
        assert_eq!(normalize("A-B"), "ab");
    }

    #[test]
    fn title_core_drops_version_suffix() {
        assert_eq!(title_core("晴天"), "晴天");
        assert_eq!(title_core("晴天(深情版)"), "晴天");
        assert_eq!(title_core("晴天（深情版）"), "晴天");
        assert_eq!(title_core("稻香 - Live"), "稻香");
        assert_eq!(title_core("Hotel California"), "hotelcalifornia");
        // 短名字里的连字符不能被当成版本分隔（避免误伤）
        assert_eq!(title_core("M-1"), "m1");
    }

    #[test]
    fn split_artists_handles_common_separators() {
        assert_eq!(split_artists("周杰伦"), vec!["周杰伦"]);
        assert_eq!(split_artists("周杰伦 / 费玉清"), vec!["周杰伦", "费玉清"]);
        assert_eq!(split_artists("周杰伦&费玉清"), vec!["周杰伦", "费玉清"]);
        assert_eq!(split_artists("周杰伦;费玉清"), vec!["周杰伦", "费玉清"]);
        let feat = split_artists("周杰伦 feat. 费玉清");
        assert!(feat.contains(&"周杰伦".to_string()), "实际：{feat:?}");
        assert!(feat.contains(&"费玉清".to_string()), "实际：{feat:?}");
    }

    /// 这是本次修复的核心回归：翻唱版不能盖过原唱。
    #[test]
    fn original_beats_cover_when_artist_specified() {
        let candidates = vec![
            song("晴天(深情版)", "Lucky小爱"),
            song("晴天", "周杰伦"),
        ];
        let best = best_song(&candidates, "晴天", "周杰伦").expect("应有结果");
        assert_eq!(best.song.artist, "周杰伦", "应选原唱而不是翻唱");
        assert!(best.exact, "原唱应被判定为精确命中");
    }

    /// 平台把翻唱排在前面时的真实回归场景。
    #[test]
    fn cover_listed_first_still_loses() {
        let candidates = vec![
            song("稻香(深情版)", "Lucky小爱"),
            song("稻香", "周杰伦"),
            song("稻香 (女声版)", "某翻唱歌手"),
        ];
        let best = best_song(&candidates, "稻香", "周杰伦").expect("应有结果");
        assert_eq!(best.song.artist, "周杰伦");
        assert_eq!(best.song.title, "稻香");
    }

    #[test]
    fn without_artist_prefers_clean_title() {
        // 用户只给歌名：应优先没有版本后缀的那条
        let candidates = vec![song("晴天(深情版)", "Lucky小爱"), song("晴天", "周杰伦")];
        let best = best_song(&candidates, "晴天", "").expect("应有结果");
        assert_eq!(best.song.title, "晴天");
        // 没指定歌手时 exact 恒为 false（上层据此继续跨平台搜索）
        assert!(!best.exact);
    }

    #[test]
    fn accompaniment_ranks_last() {
        let candidates = vec![
            song("晴天", "周杰伦"),
            song("晴天(伴奏)", "周杰伦"),
        ];
        let ranked = rank_songs(&candidates, "晴天", "周杰伦");
        assert_eq!(ranked[0].song.title, "晴天", "伴奏必须排在原版之后");
    }

    #[test]
    fn wrong_artist_scores_much_lower() {
        let right = score_song(&song("晴天", "周杰伦"), "晴天", "周杰伦");
        let wrong = score_song(&song("晴天", "别人"), "晴天", "周杰伦");
        assert!(
            right.score > wrong.score + 100,
            "歌手不符应有明显分差：right={} wrong={}",
            right.score,
            wrong.score
        );
        assert!(!wrong.exact);
    }

    #[test]
    fn unrelated_title_scores_negative() {
        let unrelated = score_song(&song("完全不同的歌", "某某"), "晴天", "周杰伦");
        assert!(unrelated.score < 0, "歌名不符应为负分：{}", unrelated.score);
        assert!(!unrelated.exact);
    }

    #[test]
    fn multi_artist_match_counts() {
        // 平台把合作歌手写成「周杰伦 / 费玉清」，用户只点周杰伦也应算命中
        let s = score_song(&song("千里之外", "周杰伦 / 费玉清"), "千里之外", "周杰伦");
        assert!(s.exact, "多歌手里包含目标歌手应判定命中");
    }

    #[test]
    fn ranking_is_stable_for_equal_scores() {
        // 同分时保持平台原顺序，避免结果在不同请求间抖动
        let candidates = vec![song("晴天", "周杰伦"), song("晴天", "周杰伦")];
        let ranked = rank_songs(&candidates, "晴天", "周杰伦");
        assert_eq!(ranked[0].song.id, "晴天");
        assert_eq!(ranked[1].song.id, "晴天");
    }

    /// 真实线上回归：网易云的「青花瓷（正式版）」
    /// 歌手字段写成 `周杰伦. / 街道办GDC/欧阳耀莹.`（上传者署名），
    /// 不能因为里面含「周杰伦」就当成原唱。
    #[test]
    fn uploader_credit_string_does_not_beat_clean_original() {
        let polluted = song("青花瓷（正式版）", "周杰伦. / 街道办GDC/欧阳耀莹.");
        let clean = song("青花瓷", "周杰伦");

        let polluted_score = score_song(&polluted, "青花瓷", "周杰伦");
        let clean_score = score_song(&clean, "青花瓷", "周杰伦");

        assert!(
            clean_score.score > polluted_score.score,
            "干净原唱必须高于上传者署名版本：clean={} polluted={}",
            clean_score.score,
            polluted_score.score
        );
        assert!(clean_score.exact, "干净原唱应判定为精确命中");
        assert!(
            !polluted_score.exact,
            "带版本后缀的「正式版」不应判定为精确命中"
        );
    }

    /// 该场景下必须能选出原唱，而不是被排在首位的上传版本带偏。
    #[test]
    fn picks_original_over_uploader_remix_in_list() {
        let candidates = vec![
            song("青花瓷（正式版）", "周杰伦. / 街道办GDC/欧阳耀莹."),
            song("青花瓷", "周杰伦"),
            song("青花瓷 (伴奏)", "周杰伦"),
        ];
        let best = best_song(&candidates, "青花瓷", "周杰伦").expect("应有结果");
        assert_eq!(best.song.title, "青花瓷");
        assert_eq!(best.song.artist, "周杰伦");
        assert!(best.exact);
    }

    /// 合法合唱不能因为「有额外歌手」被误杀。
    #[test]
    fn legitimate_duet_is_not_penalized_as_uploader_credit() {
        let duet = score_song(
            &song("千里之外", "周杰伦 / 费玉清"),
            "千里之外",
            "周杰伦",
        );
        // 额外 1 个歌手扣 25，但仍是高分并通过精确判定
        assert!(duet.exact, "合法合唱应判定命中");
        assert!(duet.score >= 100, "合法合唱分数不应过低：{}", duet.score);
    }
}
