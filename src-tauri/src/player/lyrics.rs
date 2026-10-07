//! LRC 歌词解析与「当前行」定位。
//!
//! ## 为什么放在后端
//! 解析逻辑要能被单元测试覆盖，也要能被「当前播放位置」驱动。放在 Rust 侧后：
//!  - 面板与控制台共用同一份解析结果，避免两份实现；
//!  - 测试不需要启动浏览器；
//!  - 时间轴计算（哪一行正在唱）是纯函数，容易测。
//!
//! ## LRC 格式要点
//! ```text
//! [00:12.34]第一行
//! [01:05.678][02:10.00]重复的时间标签（同一句会出现多次）
//! [ar:周杰伦]        ← 元信息标签，无时间戳
//! [00:30.00]         ← 纯时间戳行（空歌词，作为间隔）
//! ```
//! 需要处理：多位小数秒、一行多时间标签、无时间戳的元信息行、空行。

use serde::{Deserialize, Serialize};

/// 一行歌词。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LyricLine {
    /// 起始时间（秒）。
    pub at: f64,
    /// 歌词文本。
    pub text: String,
}

/// 解析后的歌词。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Lyrics {
    /// 按时间升序排列的歌词行。
    pub lines: Vec<LyricLine>,
    /// 是否为纯音乐（没有任何歌词行）。
    pub instrumental: bool,
}

impl Lyrics {
    /// 从 LRC 文本解析。
    ///
    /// 无法解析的行会被忽略（而不是报错）——歌词格式五花八门，
    /// 面板宁可少显示一行，也不能因为一行脏数据整段不显示。
    pub fn parse(raw: &str) -> Self {
        let mut lines: Vec<LyricLine> = Vec::new();

        for line in raw.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let (times, text) = split_time_tags(trimmed);
            if times.is_empty() {
                // 没有时间标签（元信息 [ar:...] 或纯文本）→ 忽略
                continue;
            }
            let text = text.trim().to_string();
            // 纯时间戳行（空歌词）保留为占位，让「当前行」在两段之间不会乱跳
            for at in times {
                lines.push(LyricLine {
                    at,
                    text: text.clone(),
                });
            }
        }

        lines.sort_by(|a, b| a.at.partial_cmp(&b.at).unwrap_or(std::cmp::Ordering::Equal));

        // 去掉紧邻的重复行（同一时间同一文本）
        lines.dedup_by(|a, b| (a.at - b.at).abs() < 0.001 && a.text == b.text);

        let has_text = lines.iter().any(|l| !l.text.trim().is_empty());
        Self {
            instrumental: !has_text,
            lines,
        }
    }

    /// 是否为空歌词。
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// 定位当前行下标；`position` 早于第一行时返回 `None`。
    pub fn index_at(&self, position: f64) -> Option<usize> {
        if self.lines.is_empty() {
            return None;
        }
        // 线性扫描足够：一首歌最多几百行，且每帧只调用一次。
        let mut found = None;
        for (index, line) in self.lines.iter().enumerate() {
            if line.at <= position + f64::EPSILON {
                found = Some(index);
            } else {
                break;
            }
        }
        found
    }

    /// 取当前行附近的若干行（用于面板滚动显示）。
    ///
    /// 返回 `(current_index, 片段)`，片段长度不超过 `context * 2 + 1`。
    pub fn window(&self, position: f64, context: usize) -> (Option<usize>, Vec<LyricLine>) {
        let Some(current) = self.index_at(position) else {
            // 还没开始：显示开头几行
            let take = context * 2 + 1;
            return (None, self.lines.iter().take(take).cloned().collect());
        };

        let start = current.saturating_sub(context);
        let end = (current + context + 1).min(self.lines.len());
        (Some(current), self.lines[start..end].to_vec())
    }
}

/// 从一行里剥离所有 `[mm:ss.xx]` 时间标签，返回时间列表与剩余文本。
///
/// 实现说明：用**字节索引**扫描而不是 `char_indices` 手动跳过。
/// 后者很容易在「跳过 `[..]`」时多消费一个字符——这正是初版把每行首字吃掉的原因。
fn split_time_tags(line: &str) -> (Vec<f64>, String) {
    let bytes = line.as_bytes();
    let mut times = Vec::new();
    let mut cursor = 0usize;

    while cursor < bytes.len() {
        // 跳过空白
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() || bytes[cursor] != b'[' {
            break;
        }
        // 找匹配的 ']'
        let Some(close_rel) = line[cursor..].find(']') else {
            break;
        };
        let close = cursor + close_rel;
        let inside = &line[cursor + 1..close];
        if let Some(seconds) = parse_timestamp(inside) {
            times.push(seconds);
            cursor = close + 1;
        } else {
            // 不是时间标签（例如 [ar:...]）→ 是元信息，整行不产生歌词
            return (Vec::new(), String::new());
        }
    }

    let rest = line[cursor..].to_string();
    (times, rest)
}

/// 解析 `mm:ss.xx` / `mm:ss` / `hh:mm:ss.xx` 形式的时间戳。
fn parse_timestamp(inside: &str) -> Option<f64> {
    // 元信息标签带冒号但前缀非数字（如 ar:）
    let parts: Vec<&str> = inside.split(':').collect();
    match parts.len() {
        2 | 3 => {}
        _ => return None,
    }

    let mut seconds = 0f64;
    for (index, part) in parts.iter().enumerate() {
        let value: f64 = part.trim().parse().ok()?;
        if value.is_sign_negative() {
            return None;
        }
        // 最后一段是秒（可带小数），前面的依次是分、时
        let weight = match parts.len() - index {
            1 => 1.0,
            2 => 60.0,
            _ => 3600.0,
        };
        seconds += value * weight;
    }
    Some(seconds)
}

/// 把 lrc 文本规范成面板可直接使用的 JSON（供 API 使用）。
pub fn to_json(lines: &Lyrics) -> String {
    serde_json::to_string(lines).unwrap_or_else(|_| "{\"lines\":[],\"instrumental\":false}".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
[ar:周杰伦]
[ti:晴天]
[00:00.000] 作词 : 周杰伦
[00:01.500] 作曲 : 周杰伦
[00:05.25]故事的小黄花
[00:10.00][00:20.00]从出生那年就飘着
[00:15.00]童年的荡秋千
";

    #[test]
    fn parses_lines_and_ignores_metadata() {
        let lyrics = Lyrics::parse(SAMPLE);
        assert!(!lyrics.instrumental);
        // 元信息行被忽略；[00:10][00:20] 展开成两行
        assert_eq!(lyrics.lines.len(), 6);
        assert_eq!(lyrics.lines[0].text, "作词 : 周杰伦");
        assert!((lyrics.lines[0].at - 0.0).abs() < 1e-6);
        assert!((lyrics.lines[1].at - 1.5).abs() < 1e-6);
        assert!((lyrics.lines[2].at - 5.25).abs() < 1e-6);
    }

    #[test]
    fn lines_are_sorted_by_time() {
        let lyrics = Lyrics::parse(SAMPLE);
        let times: Vec<f64> = lyrics.lines.iter().map(|l| l.at).collect();
        let mut sorted = times.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(times, sorted, "行必须按时间升序");
    }

    #[test]
    fn multi_tag_line_expands_into_two_lines() {
        let lyrics = Lyrics::parse(SAMPLE);
        let matches: Vec<&LyricLine> = lyrics
            .lines
            .iter()
            .filter(|l| l.text == "从出生那年就飘着")
            .collect();
        assert_eq!(matches.len(), 2);
        assert!((matches[0].at - 10.0).abs() < 1e-6);
        assert!((matches[1].at - 20.0).abs() < 1e-6);
    }

    #[test]
    fn instrumentals_and_garbage_yield_empty() {
        assert!(Lyrics::parse("").is_empty());
        assert!(Lyrics::parse("纯音乐，请欣赏").instrumental);
        assert!(Lyrics::parse("[ar:某人]\n[ti:歌名]").instrumental);
        // 只有时间戳没有文本 → 视为纯音乐
        let only_offsets = Lyrics::parse("[00:10.00]\n[00:20.00]");
        assert!(only_offsets.instrumental);
        assert_eq!(only_offsets.lines.len(), 2, "占位行要保留，避免当前行乱跳");
    }

    #[test]
    fn index_at_finds_current_line() {
        let lyrics = Lyrics::parse(SAMPLE);
        // 时间轴：0.0 / 1.5 / 5.25 / 10 / 15 / 20
        assert_eq!(lyrics.index_at(-1.0), None, "早于第一行时无当前行");
        assert_eq!(lyrics.index_at(0.0), Some(0));
        assert_eq!(lyrics.index_at(3.0), Some(1));
        assert_eq!(lyrics.index_at(5.25), Some(2));
        assert_eq!(lyrics.index_at(12.0), Some(3));
        assert_eq!(lyrics.index_at(100.0), Some(5), "超出末尾保持最后一行");
    }

    #[test]
    fn window_centers_on_current_line() {
        let lyrics = Lyrics::parse(SAMPLE);
        // 时间轴：0.0 作词 / 1.5 作曲 / 5.25 故事的小黄花 / 10 从出生那年就飘着 / 15 童年的荡秋千 / 20 从出生那年就飘着
        let (current, window) = lyrics.window(12.0, 1);
        assert_eq!(current, Some(3), "12 秒时应停在 10 秒那一行");
        assert_eq!(window.len(), 3, "context=1 应给出前后各一行");
        assert_eq!(window[0].text, "故事的小黄花", "窗口应从当前行前一行开始");
        assert_eq!(window[1].text, "从出生那年就飘着");
        assert_eq!(window[2].text, "童年的荡秋千");

        // 开头附近：当前行在第 0 行，窗口只能向下取（不越过开头）
        let (current, window) = lyrics.window(0.0, 2);
        assert_eq!(current, Some(0));
        assert_eq!(window.len(), 3, "开头处向下取 min(全部剩余, context*2+1)");
        assert_eq!(window[0].at, 0.0);
        assert_eq!(window[2].at, 5.25);
    }

    #[test]
    fn window_before_start_shows_head() {
        let lyrics = Lyrics::parse(SAMPLE);
        let (current, window) = lyrics.window(-5.0, 1);
        assert_eq!(current, None);
        assert_eq!(window.len(), 3);
        assert_eq!(window[0].text, "作词 : 周杰伦", "未开始时显示开头");
    }

    #[test]
    fn parses_hour_scale_timestamps() {
        let lyrics = Lyrics::parse("[01:02:03.50]长音频");
        assert_eq!(lyrics.lines.len(), 1);
        let expected = 3600.0 + 2.0 * 60.0 + 3.5;
        assert!((lyrics.lines[0].at - expected).abs() < 1e-6);
    }

    #[test]
    fn duplicate_adjacent_lines_are_deduped() {
        let lyrics = Lyrics::parse("[00:10.00]同一句\n[00:10.00]同一句\n[00:11.00]下一句");
        assert_eq!(lyrics.lines.len(), 2);
    }

    #[test]
    fn rejects_negative_or_broken_timestamps() {
        assert!(parse_timestamp("-1:00").is_none());
        assert!(parse_timestamp("ab:cd").is_none());
        assert!(parse_timestamp("ar:周杰伦").is_none());
        assert!(parse_timestamp("00:10.00").is_some());
    }

    #[test]
    fn to_json_roundtrips() {
        let lyrics = Lyrics::parse(SAMPLE);
        let json = to_json(&lyrics);
        let parsed: Lyrics = serde_json::from_str(&json).expect("应为合法 JSON");
        assert_eq!(parsed, lyrics);
    }
}
