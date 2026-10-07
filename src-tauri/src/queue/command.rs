//! 点歌指令解析。
//!
//! 默认正则：`^点歌\s+(.+?)(?:\s+(.+))?$`
//!  - `点歌 晴天`        → 歌名 = 晴天，歌手 = 空
//!  - `点歌 晴天 周杰伦` → 歌名 = 晴天，歌手 = 周杰伦
//!
//! 正则可在配置里修改（`rules.command_regex`），因此本模块把「解析」与「匹配」分开：
//! [`SongRequestParser::new`] 负责编译校验，[`SongRequestParser::parse`] 只做匹配。
//!
//! 阶段 4 会在此基础上补充：冷却、队列上限、重复点歌、粉丝牌/等级限制。

use regex::Regex;
use thiserror::Error;

use crate::config::RequestRules;

/// 解析出的点歌请求（尚未匹配到具体歌曲）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SongRequest {
    /// 歌名（已 trim）。
    pub title: String,
    /// 歌手（可能为空）。
    pub artist: String,
    /// 原始弹幕文本。
    pub raw: String,
}

impl SongRequest {
    /// 用于搜索引擎的关键词：`歌名 歌手`。
    pub fn keyword(&self) -> String {
        if self.artist.is_empty() {
            self.title.clone()
        } else {
            format!("{} {}", self.title, self.artist)
        }
    }
}

/// 指令解析错误。
#[derive(Debug, Error)]
pub enum CommandError {
    /// 配置里的正则非法。
    #[error("点歌指令正则非法：{0}")]
    InvalidRegex(String),
    /// 正则编译通过但缺少捕获组。
    #[error("点歌指令正则必须包含至少一个捕获组（歌名）")]
    MissingCaptureGroup,
}

/// 点歌指令解析器。
#[derive(Debug, Clone)]
pub struct SongRequestParser {
    pattern: Regex,
}

impl SongRequestParser {
    /// 用配置中的正则构造解析器。
    pub fn from_rules(rules: &RequestRules) -> Result<Self, CommandError> {
        Self::new(&rules.command_regex)
    }

    /// 用给定正则构造解析器。
    pub fn new(pattern: &str) -> Result<Self, CommandError> {
        let regex = Regex::new(pattern).map_err(|e| CommandError::InvalidRegex(e.to_string()))?;
        if regex.captures_len() < 2 {
            return Err(CommandError::MissingCaptureGroup);
        }
        Ok(Self { pattern: regex })
    }

    /// 默认解析器（内置默认正则）。
    pub fn default_pattern() -> Self {
        Self::from_rules(&RequestRules::default()).expect("内置正则必定合法")
    }

    /// 当前使用的正则文本（界面展示与热更新对比用）。
    pub fn pattern(&self) -> String {
        self.pattern.as_str().to_string()
    }

    /// 尝试把一条弹幕解析为点歌请求；不匹配返回 `None`。
    pub fn parse(&self, text: &str) -> Option<SongRequest> {
        let text = text.trim();
        let caps = self.pattern.captures(text)?;
        let title = caps.get(1)?.as_str().trim().to_string();
        if title.is_empty() {
            return None;
        }
        let artist = caps
            .get(2)
            .map(|m| m.as_str().trim().to_string())
            .unwrap_or_default();
        Some(SongRequest {
            title,
            artist,
            raw: text.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parser() -> SongRequestParser {
        SongRequestParser::default_pattern()
    }

    #[test]
    fn parses_title_only() {
        let req = parser().parse("点歌 晴天").expect("应匹配");
        assert_eq!(req.title, "晴天");
        assert_eq!(req.artist, "");
        assert_eq!(req.keyword(), "晴天");
    }

    #[test]
    fn parses_title_and_artist() {
        let req = parser().parse("点歌 晴天 周杰伦").expect("应匹配");
        assert_eq!(req.title, "晴天");
        assert_eq!(req.artist, "周杰伦");
        assert_eq!(req.keyword(), "晴天 周杰伦");
    }

    #[test]
    fn trims_extra_spaces() {
        let req = parser().parse("  点歌   富士山下   陈奕迅  ").expect("应匹配");
        assert_eq!(req.title, "富士山下");
        assert_eq!(req.artist, "陈奕迅");
    }

    #[test]
    fn ignores_non_command_danmaku() {
        assert!(parser().parse("主播好厉害").is_none());
        assert!(parser().parse("我要点歌 晴天").is_none());
        assert!(parser().parse("点歌").is_none());
        assert!(parser().parse("点歌   ").is_none());
    }

    #[test]
    fn rejects_non_capturing_regex() {
        assert!(matches!(
            SongRequestParser::new(r"^点歌\s+.+$"),
            Err(CommandError::MissingCaptureGroup)
        ));
        assert!(matches!(
            SongRequestParser::new(r"^点歌(["),
            Err(CommandError::InvalidRegex(_))
        ));
    }
}
