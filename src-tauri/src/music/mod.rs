//! 音乐平台适配层。
//!
//! 核心是 [`MusicAdapter`]：所有平台差异都被收敛到这三个方法后面。
//! 非官方接口随时可能变化，因此：
//!  - 上层只依赖 trait，不依赖具体平台；
//!  - 适配器内部对「接口返回结构变化」要返回 [`MusicError::ApiChanged`] 而不是 panic；
//!  - 阶段 5 先实现网易云，QQ 音乐复用同一 trait 扩展。

pub mod login;
pub mod netease;
pub mod qq;
pub mod resolver;
pub mod scoring;
pub mod secrets;
pub mod service;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::models::{MusicPlatform, Song};

pub use login::{
    build_cookie_string, cookie_host_hint, describe_failure, detect_login_cookie,
    fallback_cookie_names, is_platform_cookie, login_cookie_names, NamedValue,
};
pub use netease::NeteaseAdapter;
pub use netease::{parse_user_playlists, song_from_track};
pub use qq::QqAdapter;
pub use resolver::{ResolveJob, ResolveOutcome, ResolveSender, SongResolver};pub use scoring::{best_song, rank_songs, score_song, ScoredSong};
pub use secrets::{
    SecretStore, StoredIn, KEYRING_ENTRY_BILIBILI_SECRET, KEYRING_ENTRY_NETEASE_COOKIE,
    KEYRING_ENTRY_QQ_COOKIE, KEYRING_SERVICE,
};
pub use service::{MusicService, PlatformStatus};

/// 音乐适配错误。
#[derive(Debug, Error)]
pub enum MusicError {
    /// 网络或请求失败。
    #[error("请求失败：{0}")]
    Network(String),
    /// 未登录或 Cookie 失效。
    #[error("未登录或登录已过期，请在控制台重新登录{0}")]
    NotLoggedIn(String),
    /// 接口返回结构变化（非官方接口的常见情况）。
    #[error("接口返回结构与预期不符：{0}")]
    ApiChanged(String),
    /// 没有搜索结果。
    #[error("没有找到匹配的歌曲：{0}")]
    NotFound(String),
    /// 版权/地区限制导致无法获取播放地址。
    #[error("该歌曲无法获取播放地址（可能受版权或会员限制，登录后重试）")]
    Unplayable,
    /// 尚未实现。
    #[error("功能尚未实现：{0}")]
    Unimplemented(&'static str),
}

/// 音乐平台适配器。
///
/// 实现者需要保证：所有方法都是幂等的，且不持有跨 await 的锁。
#[async_trait]
pub trait MusicAdapter: Send + Sync {
    /// 平台标识。
    fn platform(&self) -> MusicPlatform;

    /// 关键词搜索，返回候选列表（按相关性排序）。
    async fn search(&self, keyword: &str) -> Result<Vec<Song>, MusicError>;

    /// 获取可直接交给 mpv 播放的音频地址。
    async fn get_play_url(&self, song_id: &str) -> Result<String, MusicError>;

    /// 获取歌词（LRC 文本）。
    async fn get_lyrics(&self, song_id: &str) -> Result<String, MusicError>;

    /// 搜索并返回第一条结果（上层点歌流程的默认策略）。
    async fn search_first(&self, keyword: &str) -> Result<Song, MusicError> {
        let mut list = self.search(keyword).await?;
        if list.is_empty() {
            return Err(MusicError::NotFound(keyword.to_string()));
        }
        Ok(list.remove(0))
    }
}

/// 一个收藏歌单的概要（阶段 10b：导入到空闲歌单用）。
///
/// 只带界面需要的最小字段：id、名称、曲目数、封面、创建者。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlaylistInfo {
    /// 平台内的歌单 ID。
    pub id: String,
    /// 歌单名。
    pub name: String,
    /// 曲目数（平台给的，可能略旧）。
    pub track_count: usize,
    /// 封面。
    #[serde(default)]
    pub cover_url: Option<String>,
    /// 创建者昵称。
    #[serde(default)]
    pub creator: Option<String>,
    /// 平台。
    pub platform: MusicPlatform,
    /// 是否是「我喜欢的音乐」（各平台的口径不同，界面可高亮）。
    #[serde(default)]
    pub special: bool,
}

/// 是否处于「已登录」状态（供界面展示）。
#[async_trait]
pub trait LoginAware {
    /// 平台是否已登录。
    fn logged_in(&self) -> bool;
}

impl LoginAware for NeteaseAdapter {
    fn logged_in(&self) -> bool {
        self.is_logged_in()
    }
}

impl LoginAware for QqAdapter {
    fn logged_in(&self) -> bool {
        self.is_logged_in()
    }
}
