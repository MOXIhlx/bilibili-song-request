//! 音乐平台服务：Cookie 生命周期 + 跨平台搜索/取址/歌词。
//!
//! ## 为什么适配器要懒加载
//! 适配器需要登录 Cookie，而 Cookie 存在系统凭据库里。如果在进程启动时就构造，
//! 就会出现「用户还没登录、适配器已固定为匿名」的问题。因此这里在**首次调用时**
//! 构造并缓存适配器；登录成功后调用 [`MusicService::reload`] 重建，立即生效。
//!
//! ## 平台选择
//! 默认网易云；QQ 音乐在 [`MusicService::search_with_fallback`] 里作为备选平台。

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::models::{MusicPlatform, Song};

use super::netease::NeteaseAdapter;
use super::qq::QqAdapter;
use super::scoring;
use super::secrets::{SecretStore, StoredIn, KEYRING_ENTRY_NETEASE_COOKIE, KEYRING_ENTRY_QQ_COOKIE};
use super::{LoginAware, MusicAdapter, MusicError};
use crate::config::{PickPolicy, SearchPlatform};

/// 标称时长低于这个秒数的候选，视为可疑的「片段」（试听/铃声/剪辑）。
///
/// 取 120 秒是因为正常流行歌曲极少短于 2 分钟，而实测的试听片段是 95 秒。
/// 这个判断只用于**在多个候选间做取舍**：如果原平台候选被判定为片段，
/// 就去另一个平台试着找更长的版本。
const SHORT_CLIP_SECS: u64 = 120;

/// 跨平台兜底时最多尝试几个候选（按分数从高到低）。
///
/// 设上限是为了避免「整页候选都取不到地址」时打太多请求。
const MAX_FALLBACK_CANDIDATES: usize = 5;

/// 跨平台兜底的最低分数门槛。
///
/// ## 为什么是 35 而不是更高
/// 打分里「歌名精确」= +100，「歌手不符」= -60，
/// 所以「歌名完全正确但歌手是别人翻唱」只有 40 分。
///
/// 实测：点「青花瓷 周杰伦」时，QQ 只给试听，而网易云上
/// **周杰伦原唱没有任何可播条目**（搜索结果里最高分的是 95 秒片段，
/// 其余全是「歌手=Jay / 刘芳 / 阿杰」的翻唱，分数都是 40）。
///
/// 用户的策略是「优先保证完整时长」，所以这里必须让 40 分通过，
/// 否则就会像一开始那样——明明有 201 秒的完整版，却因为"歌手不是周杰伦"
/// 而拒绝，最后连一首歌都播不了。
///
/// 35 这个值仍能挡住**歌名都不对**的候选（那类分数是负的）。
const MIN_FALLBACK_SCORE: i32 = 35;

/// 候选是否像一个「短片」（用于在兜底时跳过）。
///
/// `duration == 0` 表示平台没给时长，无法判断，按「不是短片」处理，
/// 免得因为缺元数据就把它跳过。
fn looks_like_short_clip(song: &Song) -> bool {
    song.duration > 0 && song.duration < SHORT_CLIP_SECS
}

/// 原平台取址结果经过策略判断后，**是否还需要去另一个平台找长版本**。
///
/// 抽成纯函数是为了能离线、确定性地验证策略分支：
/// 真实 `play_url_for` 依赖各家平台的版权状态（同一首歌今天给完整版、
/// 明天可能只给试听），靠线上观察无法稳定复现两种分支。
///
/// 返回 `true` 表示应当继续尝试另一个平台。
fn should_try_other_platform(policy: PickPolicy, origin: Option<&(String, bool)>, song: &Song) -> bool {
    match policy {
        // 优先原唱：只要原平台给得出地址就接受，不再换平台。
        // 原平台完全没有才必须去另一个平台（否则彻底播不了）。
        PickPolicy::PreferArtist => origin.is_none(),
        // 优先完整时长：原平台没地址，或只有试听/可疑短片时，都要去找长版本。
        PickPolicy::PreferFullLength => match origin {
            None => true,
            Some((_, preview)) => *preview || looks_like_short_clip(song),
        },
    }
}

/// 单个平台的登录/可用状态（给界面用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformStatus {
    /// 平台标识。
    pub platform: MusicPlatform,
    /// 展示名。
    pub display_name: String,
    /// 是否已保存 Cookie 并判定为已登录。
    pub logged_in: bool,
    /// Cookie 存放位置（未保存时为 None）。
    pub stored_in: Option<StoredIn>,
}

/// 音乐服务：持有适配器与凭据。
pub struct MusicService {
    /// 网易云适配器（懒加载 + 可重建）。
    netease: RwLock<Option<Arc<NeteaseAdapter>>>,
    /// QQ 音乐适配器。
    qq: RwLock<Option<Arc<QqAdapter>>>,
    /// 网易云 Cookie 存取。
    netease_store: SecretStore,
    /// QQ 音乐 Cookie 存取。
    qq_store: SecretStore,
    /// 默认平台。
    default_platform: RwLock<MusicPlatform>,
    /// 搜索平台策略（自动 / 只用 QQ / 只用网易云）。
    search_platform: RwLock<SearchPlatform>,
}

impl MusicService {
    /// 创建服务（凭据目录用默认位置）。
    pub fn new() -> Self {
        Self::with_stores(
            SecretStore::new(KEYRING_ENTRY_NETEASE_COOKIE),
            SecretStore::new(KEYRING_ENTRY_QQ_COOKIE),
        )
    }

    /// 指定凭据存取器（测试用，避免写入真实配置目录）。
    pub fn with_stores(netease_store: SecretStore, qq_store: SecretStore) -> Self {
        Self {
            netease: RwLock::new(None),
            qq: RwLock::new(None),
            netease_store,
            qq_store,
            // 默认优先 QQ 音乐：它的原唱/正版曲库更全，且搜索接口给了更干净的结果。
            // QQ 目前只实现了搜索，取址会自动回退到网易云（见 `play_url_for`）。
            default_platform: RwLock::new(MusicPlatform::Qq),
            // 默认「自动」：先搜 QQ，没有精确命中时再搜网易云综合评分。
            search_platform: RwLock::new(SearchPlatform::Auto),
        }
    }

    /// 默认平台。
    pub async fn default_platform(&self) -> MusicPlatform {
        *self.default_platform.read().await
    }

    /// 当前的搜索平台策略。
    pub async fn search_platform(&self) -> SearchPlatform {
        *self.search_platform.read().await
    }

    /// 应用搜索平台策略（会同时调整默认平台，两者必须一致）。
    pub async fn apply_search_platform(&self, strategy: SearchPlatform) {
        *self.search_platform.write().await = strategy;
        if let Some(platform) = strategy.primary() {
            *self.default_platform.write().await = platform;
        }
    }

    /// 用一份「内存里的 Cookie」构造服务，**不落任何盘**。
    ///
    /// 用途：登录窗口抓到 Cookie 后，先拿它去平台接口验证是不是真登录账号，
    /// 验证通过才写入凭据库。校验失败时不能留下脏凭据，所以这里不能走 `save_cookie`。
    pub fn with_in_memory_cookies(netease: Option<String>, qq: Option<String>) -> Self {
        let service = Self::with_stores(
            SecretStore::file_only("memory.netease", std::env::temp_dir()),
            SecretStore::file_only("memory.qq", std::env::temp_dir()),
        );
        // 直接把适配器塞进缓存，绕开凭据读取
        *service
            .netease
            .try_write()
            .expect("新建的服务没有并发访问") = netease.map(|c| Arc::new(NeteaseAdapter::new(Some(c))));
        *service.qq.try_write().expect("新建的服务没有并发访问") =
            qq.map(|c| Arc::new(QqAdapter::new(Some(c))));
        service
    }

    /// 用内存 Cookie 校验某平台登录态，返回账号标识（不写凭据库）。
    pub fn with_in_memory_cookie_for(platform: MusicPlatform, cookie: &str) -> Self {
        match platform {
            MusicPlatform::Netease => Self::with_in_memory_cookies(Some(cookie.to_string()), None),
            MusicPlatform::Qq => Self::with_in_memory_cookies(None, Some(cookie.to_string())),
        }
    }

    /// 设置默认平台。
    pub async fn set_default_platform(&self, platform: MusicPlatform) {
        *self.default_platform.write().await = platform;
    }

    /// 取（必要时构造）网易云适配器。
    async fn netease(&self) -> Arc<NeteaseAdapter> {
        if let Some(adapter) = self.netease.read().await.as_ref() {
            return Arc::clone(adapter);
        }
        let cookie = self.netease_store.load();
        let adapter = Arc::new(NeteaseAdapter::new(cookie));
        debug!(logged_in = adapter.is_logged_in(), "构造网易云适配器");
        *self.netease.write().await = Some(Arc::clone(&adapter));
        adapter
    }

    /// 取（必要时构造）QQ 音乐适配器。
    async fn qq(&self) -> Arc<QqAdapter> {
        if let Some(adapter) = self.qq.read().await.as_ref() {
            return Arc::clone(adapter);
        }
        let cookie = self.qq_store.load();
        let adapter = Arc::new(QqAdapter::new(cookie));
        *self.qq.write().await = Some(Arc::clone(&adapter));
        adapter
    }

    /// 丢弃缓存的适配器，下次调用重新读取 Cookie。
    pub async fn reload(&self) {
        *self.netease.write().await = None;
        *self.qq.write().await = None;
        info!("音乐适配器已重置，将在下次调用时重新读取凭据");
    }

    /// 保存某平台的 Cookie 并重建适配器。
    ///
    /// 返回实际存放位置，供界面提示用户（凭据库 or 明文文件）。
    pub async fn save_cookie(
        &self,
        platform: MusicPlatform,
        cookie: &str,
    ) -> anyhow::Result<StoredIn> {
        let store = match platform {
            MusicPlatform::Netease => &self.netease_store,
            MusicPlatform::Qq => &self.qq_store,
        };
        let cookie = cookie.trim();
        if cookie.is_empty() {
            anyhow::bail!("Cookie 为空，无法保存");
        }
        let stored_in = store.save(cookie)?;
        self.reload().await;
        Ok(stored_in)
    }

    /// 清除某平台的 Cookie。
    pub async fn clear_cookie(&self, platform: MusicPlatform) -> anyhow::Result<()> {
        let store = match platform {
            MusicPlatform::Netease => &self.netease_store,
            MusicPlatform::Qq => &self.qq_store,
        };
        store.clear()?;
        self.reload().await;
        Ok(())
    }

    /// 校验某平台的登录态是否**真的**可用，返回账号标识。
    ///
    /// 为什么不能只看 Cookie 名：实测网易云在页面加载时会写 `MUSIC_U`，
    /// 未登录也有。只按名字判定会让「退出登录后重新打开登录窗口」
    /// 立刻被判成已登录（用户看到的是「秒登录」）。
    ///
    /// - 网易云：调 `/api/nuser/account/get` 查账号 UID，查得到才算登录。
    /// - QQ 音乐：适配器尚未实现（搜索/取址都是 `Unimplemented`），
    ///   没有可靠的校验接口，这里只能按 Cookie 名判定，返回 `Some("qq")`。
    ///   等实现 QQ 音乐适配器后应替换为真实的接口校验。
    pub async fn verify_login(&self, platform: MusicPlatform) -> Option<String> {
        match platform {
            MusicPlatform::Netease => self.netease().await.verify_login().await,
            MusicPlatform::Qq => {
                if self.qq().await.logged_in() {
                    Some("qq".to_string())
                } else {
                    None
                }
            }
        }
    }

    /// 某平台的登录状态。
    pub async fn status(&self, platform: MusicPlatform) -> PlatformStatus {
        match platform {
            MusicPlatform::Netease => {
                let adapter = self.netease().await;
                PlatformStatus {
                    platform,
                    display_name: platform.display_name().to_string(),
                    logged_in: adapter.is_logged_in(),
                    // 凭据实际存放位置（凭据库 vs 明文文件）。读取时会记录，
                    // 所以这里要先确保读过一次。
                    stored_in: self.stored_in_of(platform),
                }
            }
            MusicPlatform::Qq => {
                let adapter = self.qq().await;
                PlatformStatus {
                    platform,
                    display_name: platform.display_name().to_string(),
                    logged_in: adapter.logged_in(),
                    stored_in: self.stored_in_of(platform),
                }
            }
        }
    }

    /// 该平台凭据的实际存放位置；没有凭据时返回 `None`。
    fn stored_in_of(&self, platform: MusicPlatform) -> Option<StoredIn> {
        match platform {
            MusicPlatform::Netease => self.netease_store.stored_in(),
            MusicPlatform::Qq => self.qq_store.stored_in(),
        }
    }

    /// 所有平台的状态。
    pub async fn all_status(&self) -> Vec<PlatformStatus> {
        vec![
            self.status(MusicPlatform::Netease).await,
            self.status(MusicPlatform::Qq).await,
        ]
    }

    /// 按平台取适配器（返回 trait 对象）。
    pub async fn adapter(&self, platform: MusicPlatform) -> Arc<dyn MusicAdapter> {
        match platform {
            MusicPlatform::Netease => self.netease().await as Arc<dyn MusicAdapter>,
            MusicPlatform::Qq => self.qq().await as Arc<dyn MusicAdapter>,
        }
    }

    /// 跨平台搜索并挑选最佳匹配。
    ///
    /// ## 策略（按用户要求实现）
    /// 1. 先在**默认平台**（默认 QQ 音乐）搜索，用本地打分挑最高分；
    /// 2. 如果结果**精确命中**（歌名与歌手都对得上，且标题没有版本后缀），
    ///    直接采用，不再查另一个平台；
    /// 3. 否则（没搜到 / 歌手不符 / 只给了歌名导致歌手可能是翻唱）再去
    ///    **另一个平台**搜一轮，两边候选放在一起按分数取最高。
    ///
    /// 这样既能用 QQ 的曲库（原唱通常更全），又不会在 QQ 没有版权时干等，
    /// 而且「点原唱却放翻唱」的问题被本地打分挡掉了。
    ///
    /// 返回选中的歌曲，以及它的来源平台与是否精确命中（供界面提示）。
    pub async fn search_best(
        &self,
        title: &str,
        artist: &str,
    ) -> Result<(Song, bool), MusicError> {
        let keyword = if artist.trim().is_empty() {
            title.trim().to_string()
        } else {
            format!("{} {}", title.trim(), artist.trim())
        };
        if keyword.is_empty() {
            return Err(MusicError::NotFound("(空关键词)".to_string()));
        }

        let primary = self.default_platform().await;
        // 是否允许在默认平台没精确命中时换平台（见 `SearchPlatform`）
        let allow_fallback = self.search_platform().await.allows_fallback();
        let secondary = match primary {
            MusicPlatform::Netease => MusicPlatform::Qq,
            MusicPlatform::Qq => MusicPlatform::Netease,
        };

        // ① 默认平台
        let mut candidates: Vec<(MusicPlatform, Vec<Song>)> = Vec::new();
        let mut primary_error: Option<MusicError> = None;
        match self.search(primary, &keyword).await {
            Ok(list) if !list.is_empty() => {
                // 精确命中就直接用，省掉第二次网络请求
                if let Some(best) = scoring::best_song(&list, title, artist) {
                    if best.exact {
                        debug!(
                            platform = ?primary,
                            title = %best.song.title,
                            artist = %best.song.artist,
                            "默认平台已精确命中，无需跨平台搜索"
                        );
                        return Ok((best.song, true));
                    }
                }
                candidates.push((primary, list));
            }
            Ok(_) => debug!(platform = ?primary, "默认平台无结果"),
            Err(err) => {
                debug!(platform = ?primary, error = %err, "默认平台搜索失败");
                primary_error = Some(err);
            }
        }

        // ①.5 「只用某个平台」策略：**绝不换平台**。
        //
        // 为什么需要：自动策略会在没有精确命中时去另一个平台找更高分，
        // 这有时不是用户想要的——比如开会员后就想固定用 QQ 听原唱，
        // 或者想固定用网易云（完整版更多）。这时搜不到就该报搜不到，
        // 而不是"偷偷"换成另一个平台的版本。
        if !allow_fallback {
            if candidates.is_empty() {
                return Err(primary_error
                    .unwrap_or_else(|| MusicError::NotFound(keyword.clone())));
            }
            let list = candidates
                .into_iter()
                .next()
                .map(|(_, list)| list)
                .unwrap_or_default();
            let best = scoring::best_song(&list, title, artist)
                .ok_or(MusicError::NotFound(keyword))?;
            debug!(
                platform = ?primary,
                title = %best.song.title,
                score = best.score,
                "固定平台策略：只用默认平台，不跨平台"
            );
            return Ok((best.song, best.exact));
        }

        // ② 备选平台
        match self.search(secondary, &keyword).await {
            Ok(list) if !list.is_empty() => candidates.push((secondary, list)),
            Ok(_) => debug!(platform = ?secondary, "备选平台同样无结果"),
            Err(err) => {
                debug!(platform = ?secondary, error = %err, "备选平台搜索失败");
                // 两个平台都失败时，把默认平台的错误抛出去（更有代表性）
                if candidates.is_empty() {
                    return Err(primary_error.unwrap_or(err));
                }
            }
        }

        if candidates.is_empty() {
            return Err(MusicError::NotFound(keyword));
        }

        // ③ 汇总评分：把两边的候选放一起比，取全局最高分
        let mut best: Option<scoring::ScoredSong> = None;
        for (_platform, list) in candidates {
            if let Some(candidate) = scoring::best_song(&list, title, artist) {
                let better = match &best {
                    Some(current) => candidate.score > current.score,
                    None => true,
                };
                if better {
                    best = Some(candidate);
                }
            }
        }

        let best = best.ok_or(MusicError::NotFound(keyword))?;
        debug!(
            platform = ?best.song.platform,
            title = %best.song.title,
            artist = %best.song.artist,
            score = best.score,
            exact = best.exact,
            "跨平台搜索选出最佳匹配"
        );
        Ok((best.song, best.exact))
    }

    /// 在指定平台搜索。
    pub async fn search(
        &self,
        platform: MusicPlatform,
        keyword: &str,
    ) -> Result<Vec<Song>, MusicError> {
        let keyword = keyword.trim();
        if keyword.is_empty() {
            return Err(MusicError::NotFound("(空关键词)".to_string()));
        }
        self.adapter(platform).await.search(keyword).await
    }

    /// 在默认平台搜索，返回第一条匹配。
    pub async fn search_first(&self, keyword: &str) -> Result<Song, MusicError> {
        let platform = self.default_platform().await;
        self.search_first_on(platform, keyword).await
    }

    /// 在指定平台搜索，返回第一条匹配。
    pub async fn search_first_on(
        &self,
        platform: MusicPlatform,
        keyword: &str,
    ) -> Result<Song, MusicError> {
        let mut list = self.search(platform, keyword).await?;
        if list.is_empty() {
            return Err(MusicError::NotFound(keyword.trim().to_string()));
        }
        Ok(list.remove(0))
    }

    /// 跨平台搜索：默认平台失败时尝试另一个平台。
    pub async fn search_with_fallback(&self, keyword: &str) -> Result<(MusicPlatform, Song), MusicError> {
        let primary = self.default_platform().await;
        match self.search_first_on(primary, keyword).await {
            Ok(song) => return Ok((primary, song)),
            Err(err) => debug!(error = %err, platform = ?primary, "默认平台搜索失败，尝试备选平台"),
        }

        let secondary = match primary {
            MusicPlatform::Netease => MusicPlatform::Qq,
            MusicPlatform::Qq => MusicPlatform::Netease,
        };
        match self.search_first_on(secondary, keyword).await {
            Ok(song) => Ok((secondary, song)),
            Err(err) => {
                warn!(error = %err, "备选平台搜索同样失败");
                Err(err)
            }
        }
    }

    /// 取播放地址。
    pub async fn play_url(&self, platform: MusicPlatform, song_id: &str) -> Result<String, MusicError> {
        let adapter = self.adapter(platform).await;
        match adapter.get_play_url(song_id).await {
            Ok(url) => Ok(url),
            Err(err @ MusicError::Unplayable) => {
                // 网易云多一条兜底路径：从用户歌单里找内嵌 url
                if platform == MusicPlatform::Netease {
                    let netease = self.netease_typed().await;
                    if let Ok(url) = netease.play_url_from_playlists(song_id).await {
                        return Ok(url);
                    }
                }
                Err(err)
            }
            Err(err) => Err(err),
        }
    }

    /// 取播放地址，按配置的 [`PickPolicy`] 策略跨平台选择。
    ///
    /// ## 两种策略的差别（实测场景）
    /// 点周杰伦《青花瓷》时：
    ///  - QQ（非会员）只给 95 秒试听；
    ///  - 网易云搜不到周杰伦原唱，只有 `青花瓷 — Jay`（201 秒）这类翻唱。
    ///
    /// - [`PickPolicy::PreferFullLength`]（默认）：跳过 95 秒片段，
    ///   换成另一平台的长版本 → 能听完整首歌，但歌手是翻唱；
    /// - [`PickPolicy::PreferArtist`]：直接用原平台的试听 → 保住原唱，
    ///   但只能听 95 秒。
    ///
    /// 做成设置项而不是写死，是因为哪种更好取决于主播偏好，
    /// 以及账号是否开了会员（开了会员后 QQ 直接给完整版，两者不再冲突）。
    pub async fn play_url_for(
        &self,
        song: &Song,
        policy: PickPolicy,
    ) -> Result<String, MusicError> {
        // ① 原平台
        let origin = self.try_play_for_platform(song.platform, song).await;

        // 原平台的结果是否可以直接用？
        //
        // 两种策略在这里分道扬镳（判断逻辑抽在 `should_try_other_platform`，可离线测试）：
        //  - 优先原唱：只要能播就用，不再换平台；
        //  - 优先完整时长：只有「完整版」才直接用，试听/短片要去别处找。
        if !should_try_other_platform(policy, origin.as_ref(), song) {
            return match origin {
                Some((url, preview)) => {
                    if preview {
                        debug!("策略=优先原唱：采用原平台试听片段");
                    }
                    Ok(url)
                }
                None => Err(MusicError::Unplayable),
            };
        }

        if policy == PickPolicy::PreferFullLength {
            match &origin {
                Some((_, preview)) => debug!(
                    platform = ?song.platform,
                    preview,
                    duration = song.duration,
                    "策略=优先完整版：原平台结果可能是试听/短片，尝试另一个平台"
                ),
                None => debug!(platform = ?song.platform, "原平台取址失败，尝试另一个平台"),
            }
        } else {
            debug!(platform = ?song.platform, "原平台没有可用地址，尝试另一个平台");
        }

        // ② 另一个平台：能找到长版本就用它
        match self.play_url_from_other(song).await {
            Ok(url) => Ok(url),
            // ③ 另一个平台也没有长版本，退回原平台结果（可能是试听）
            Err(_) => match origin {
                Some((url, preview)) => {
                    if preview {
                        debug!("两处都只有试听片段，采用原平台试听");
                    }
                    Ok(url)
                }
                None => Err(MusicError::Unplayable),
            },
        }
    }

    /// 在**另一个**平台找同一首歌并取址（找不到长版本则报错）。
    ///
    /// 会按分数顺序逐个试，直到拿到一个「不是短片」的候选：
    /// 实测网易云搜「青花瓷 周杰伦」时，分数最高的是 95 秒片段，
    /// 完整版因为在第二位、且歌手名是「Jay」（分数 40），
    /// 只取第一名就会错过它。
    async fn play_url_from_other(&self, song: &Song) -> Result<String, MusicError> {
        let other = match song.platform {
            MusicPlatform::Netease => MusicPlatform::Qq,
            MusicPlatform::Qq => MusicPlatform::Netease,
        };

        let keyword = if song.artist.trim().is_empty() {
            song.title.clone()
        } else {
            format!("{} {}", song.title, song.artist)
        };

        let list = match self.search(other, &keyword).await {
            Ok(list) if !list.is_empty() => list,
            Ok(_) => return Err(MusicError::Unplayable),
            Err(err) => {
                debug!(error = %err, "另一个平台搜索失败");
                return Err(MusicError::Unplayable);
            }
        };

        let ranked = scoring::rank_songs(&list, &song.title, &song.artist);
        for candidate in ranked.iter().take(MAX_FALLBACK_CANDIDATES) {
            // 分数太低说明不是同一首歌，跳过它但**继续看后面的候选**。
            //
            // 这里必须用 `continue` 而不是 `break`：实测网易云搜「青花瓷 周杰伦」时，
            // 分数最高的是片段（95 秒，被下面的短片判断跳过），
            // 而完整版「青花瓷 — Jay」因为歌手名不是「周杰伦」分数偏低——
            // 一旦 `break` 就再也没有机会选中它，用户只能听 95 秒或者干脆播不了。
            if candidate.score < MIN_FALLBACK_SCORE {
                debug!(
                    title = %candidate.song.title,
                    artist = %candidate.song.artist,
                    score = candidate.score,
                    "另一个平台候选分数偏低，跳过"
                );
                continue;
            }
            // 候选本身标称时长过短 → 多半也是片段，跳过
            if looks_like_short_clip(&candidate.song) {
                debug!(
                    title = %candidate.song.title,
                    duration = candidate.song.duration,
                    "另一个平台该候选标称时长过短，跳过"
                );
                continue;
            }
            if let Ok(url) = self.play_url(other, &candidate.song.id).await {
                if !url.is_empty() {
                    info!(
                        from = ?song.platform,
                        to = ?candidate.song.platform,
                        title = %candidate.song.title,
                        artist = %candidate.song.artist,
                        score = candidate.score,
                        duration = candidate.song.duration,
                        "已改用另一个平台的完整版"
                    );
                    return Ok(url);
                }
            }
        }

        Err(MusicError::Unplayable)
    }

    /// 向某个平台取址，返回 `(地址, 是否试听片段)`；取不到返回 `None`。
    /// 只有 QQ 有「试听」概念（非会员 + 版权曲目），其它平台恒为完整版。
    async fn try_play_for_platform(
        &self,
        platform: MusicPlatform,
        song: &Song,
    ) -> Option<(String, bool)> {
        if platform == MusicPlatform::Qq {
            let qq = self.qq_typed().await;
            match qq.get_play_info(&song.id).await {
                Ok(info) if !info.url.is_empty() => return Some((info.url, info.preview)),
                Ok(_) => debug!("QQ音乐返回空地址"),
                Err(err) => debug!(error = %err, "QQ音乐取址失败"),
            }
            return None;
        }

        // 其它平台：按 id 取址（含网易云歌单兜底）
        match self.play_url(platform, &song.id).await {
            Ok(url) if !url.is_empty() => Some((url, false)),
            Ok(_) => {
                debug!(?platform, "平台返回空地址");
                None
            }
            Err(err) => {
                debug!(?platform, error = %err, "取址失败");
                None
            }
        }
    }

    /// 取歌词。
    pub async fn lyrics(&self, platform: MusicPlatform, song_id: &str) -> Result<String, MusicError> {
        self.adapter(platform).await.get_lyrics(song_id).await
    }

    /// 取具体类型的网易云适配器（内部使用，避免 trait 对象丢失特有能力）。
    async fn netease_typed(&self) -> Arc<NeteaseAdapter> {
        self.netease().await
    }

    /// 取具体类型的 QQ 适配器（需要它的 `get_play_info`：能报告「是否试听片段」）。
    async fn qq_typed(&self) -> Arc<QqAdapter> {
        self.qq().await
    }

    // ── 收藏歌单（阶段 10b）──────────────────────────────────────────────

    /// 列出某个平台的收藏歌单。
    pub async fn user_playlists(
        &self,
        platform: MusicPlatform,
    ) -> Result<Vec<super::PlaylistInfo>, MusicError> {
        match platform {
            MusicPlatform::Netease => self.netease_typed().await.user_playlists().await,
            MusicPlatform::Qq => self.qq_typed().await.user_playlists().await,
        }
    }

    /// 读取某个平台的歌单曲目。
    pub async fn playlist_tracks(
        &self,
        platform: MusicPlatform,
        playlist_id: &str,
    ) -> Result<Vec<Song>, MusicError> {
        match platform {
            MusicPlatform::Netease => {
                self.netease_typed().await.playlist_tracks(playlist_id).await
            }
            MusicPlatform::Qq => self.qq_typed().await.playlist_tracks(playlist_id).await,
        }
    }
}

impl Default for MusicService {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bsr-music-{tag}-{:?}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn service(tag: &str) -> MusicService {
        let dir = temp_dir(tag);
        // 用 file_only 彻底避开系统凭据库：并行用例共享同名条目会互相覆盖。
        MusicService::with_stores(
            SecretStore::file_only(KEYRING_ENTRY_NETEASE_COOKIE, &dir),
            SecretStore::file_only(KEYRING_ENTRY_QQ_COOKIE, &dir),
        )
    }

    #[tokio::test]
    async fn default_platform_is_qq() {
        // 默认优先 QQ 音乐（原唱曲库更全）；取不到直链时会回退网易云
        let service = service("default");
        assert_eq!(service.default_platform().await, MusicPlatform::Qq);
        service.set_default_platform(MusicPlatform::Netease).await;
        assert_eq!(service.default_platform().await, MusicPlatform::Netease);
    }

    // ── 点歌优先级策略（离线确定性测试）────────────────────────────────────
    //
    // 为什么必须离线测：真实取址依赖各家平台的版权状态——同一首歌
    // 今天可能给完整版、明天只给试听（实测 QQ 对同一个 songmid
    // 有时返回 purl 为空 + result=104003，有时返回完整直链）。
    // 靠线上观察无法稳定复现两种分支，所以把决策逻辑抽成
    // `should_try_other_platform` 并在这里逐条锁住。

    fn song_with_duration(duration: u64) -> Song {
        Song {
            id: "x".into(),
            title: "青花瓷".into(),
            artist: "周杰伦".into(),
            platform: MusicPlatform::Qq,
            duration,
            cover_url: None,
            album: None,
            source: Default::default(),
            source_error: None,
        }
    }

    #[test]
    fn short_clip_detection_uses_metadata_duration() {
        assert!(looks_like_short_clip(&song_with_duration(95)), "95 秒是试听片段");
        assert!(!looks_like_short_clip(&song_with_duration(239)), "完整版不算短片");
        // 时长未知（0）时不应判定为短片，否则会误跳过正常候选
        assert!(!looks_like_short_clip(&song_with_duration(0)));
    }

    #[test]
    fn prefer_artist_keeps_preview_instead_of_switching() {
        let song = song_with_duration(95);
        let origin = Some(("http://qq/preview.m4a".to_string(), true));

        // 优先原唱：即使只是试听，也不换平台
        assert!(
            !should_try_other_platform(PickPolicy::PreferArtist, origin.as_ref(), &song),
            "优先原唱时不应换平台"
        );
        // 原平台完全没有地址时才必须换（否则彻底播不了）
        assert!(
            should_try_other_platform(PickPolicy::PreferArtist, None, &song),
            "原平台无地址时必须去另一个平台尝试"
        );
    }

    #[test]
    fn prefer_full_length_switches_away_from_preview() {
        let song = song_with_duration(95);
        let preview = Some(("http://qq/preview.m4a".to_string(), true));

        // 优先完整时长：标记为试听 → 换平台
        assert!(
            should_try_other_platform(PickPolicy::PreferFullLength, preview.as_ref(), &song),
            "试听片段应触发换平台"
        );

        // 元数据时长过短（95 秒）也应触发换平台，即使平台没标记试听
        let unmarked = Some(("http://qq/x.m4a".to_string(), false));
        assert!(
            should_try_other_platform(PickPolicy::PreferFullLength, unmarked.as_ref(), &song),
            "标称时长过短应触发换平台"
        );
    }

    #[test]
    fn prefer_full_length_keeps_good_full_version() {
        let song = song_with_duration(239);
        let full = Some(("http://qq/full.m4a".to_string(), false));

        // 完整版 + 时长正常 → 直接用，不再多跑一次搜索
        assert!(
            !should_try_other_platform(PickPolicy::PreferFullLength, full.as_ref(), &song),
            "原平台已是完整版时不应换平台"
        );
        // 时长未知也不换（缺元数据不等于短片）
        let unknown = song_with_duration(0);
        assert!(
            !should_try_other_platform(PickPolicy::PreferFullLength, full.as_ref(), &unknown),
            "时长未知时不应因为缺元数据就换平台"
        );
    }

    #[test]
    fn both_policies_switch_when_origin_has_nothing() {
        let song = song_with_duration(239);
        for policy in [PickPolicy::PreferArtist, PickPolicy::PreferFullLength] {
            assert!(
                should_try_other_platform(policy, None, &song),
                "{policy:?} 在原平台无地址时必须换平台"
            );
        }
    }

    #[tokio::test]
    async fn status_reports_logged_out_without_cookie() {
        let service = service("status");
        let statuses = service.all_status().await;
        assert_eq!(statuses.len(), 2);
        assert!(statuses.iter().all(|s| !s.logged_in));
        assert_eq!(statuses[0].display_name, "网易云音乐");
    }

    #[tokio::test]
    async fn saving_cookie_marks_platform_logged_in() {
        let service = service("save");
        assert!(!service.status(MusicPlatform::Netease).await.logged_in);
        service
            .save_cookie(MusicPlatform::Netease, "MUSIC_U=abc; __csrf=xyz")
            .await
            .expect("保存应成功");
        let status = service.status(MusicPlatform::Netease).await;
        assert!(status.logged_in, "保存 MUSIC_U 后应判定为已登录");
    }

    #[tokio::test]
    async fn empty_cookie_is_rejected() {
        let service = service("empty");
        assert!(service.save_cookie(MusicPlatform::Netease, "   ").await.is_err());
    }

    #[tokio::test]
    async fn clearing_cookie_resets_login() {
        let service = service("clear");
        service
            .save_cookie(MusicPlatform::Netease, "MUSIC_U=abc")
            .await
            .unwrap();
        assert!(service.status(MusicPlatform::Netease).await.logged_in);
        service.clear_cookie(MusicPlatform::Netease).await.unwrap();
        assert!(!service.status(MusicPlatform::Netease).await.logged_in);
    }

    #[tokio::test]
    async fn blank_keyword_is_rejected_before_network() {
        let service = service("blank");
        match service.search(MusicPlatform::Netease, "   ").await {
            Err(MusicError::NotFound(_)) => {}
            other => panic!("应返回 NotFound，实际：{other:?}"),
        }
    }
}
