//! 网易云音乐适配器。
//!
//! ## 接口来源与风险
//! 这里用的是**非官方**接口（网站自用接口），随时可能变更。因此本模块：
//!  - 所有响应字段用 `serde_json::Value` 宽松提取，缺字段不 panic，只降级；
//!  - 结构不符时返回 [`MusicError::ApiChanged`] 而不是 unwrap；
//!  - 关键判定（能否播放）用「URL 是否为空」这种最稳的信号。
//!
//! ## 三级取址回退链
//! 实测（本机 2026-09 验证）：
//!  - 免费曲目（`fee == 0`）用**旧 `api` 接口**、无需登录即可拿到 mp3 直链；
//!  - 版权/会员曲目旧接口返回 `url: ""`，必须带登录 Cookie 走 **eapi**；
//!  - eapi 仍拿不到时，用「我喜欢的音乐」歌单详情兜底（其中会内嵌部分 `url` 字段）。
//!
//! ```text
//! /api/song/enhance/player/url ──(空)──▶ /eapi/song/enhance/player/url/v1 ──(空)──▶ 歌单兜底
//! ```
//!
//! ## Cookie
//! 登录 Cookie 通过 `Tauri` 内嵌窗口抓取后由 [`super::secrets`] 存入系统凭据库，
//! 本适配器只持有内存副本。

use std::collections::HashMap;
use std::time::Duration;

use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
use async_trait::async_trait;
use base64::Engine;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::{debug, info};

use crate::models::{MusicPlatform, Song, SongSource};

use super::{MusicAdapter, MusicError};

/// 网易云 Cookie 在凭据库里的条目名（定义在 [`super::secrets`]）。
pub use super::secrets::KEYRING_ENTRY_NETEASE_COOKIE as KEYRING_ENTRY_COOKIE;

/// 请求 UA：接口对 UA 敏感，桌面版 UA 更容易拿到播放地址。
pub const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";

/// eapi 固定密钥（16 字节）与 IV。
const EAPI_KEY: &[u8; 16] = b"e82ckenh8dichen8";
const EAPI_IV: &[u8; 16] = b"0102030405060708";
/// eapi 签名盐。
const EAPI_SALT: &str = "36cd479b6b5";

/// 默认音质：标准音质兼容性最好（部分曲目只免费提供标准音质）。
pub const DEFAULT_LEVEL: &str = "standard";

/// 覆盖接口域名的环境变量。
///
/// 用途：
///  - 指向可用的镜像 / 自建网关（平台域名被墙或被限流时）；
///  - 端到端测试时指向本地桩服务（见 `scripts/mock-music-server.mjs`）。
///
/// 只改**域名前缀**，路径与参数保持不变，因此被指向的服务必须兼容同一套接口。
pub const ENV_API_BASE: &str = "BSR_NETEASE_API_BASE";

/// 读取接口域名前缀（默认 `https://music.163.com`）。
pub fn api_base() -> String {
    match std::env::var(ENV_API_BASE) {
        Ok(value) if !value.trim().is_empty() => value.trim().trim_end_matches('/').to_string(),
        _ => "https://music.163.com".to_string(),
    }
}

/// eapi 专用域名前缀（默认 `https://interface3.music.163.com`）。
pub fn eapi_base() -> String {
    match std::env::var(ENV_API_BASE) {
        Ok(value) if !value.trim().is_empty() => value.trim().trim_end_matches('/').to_string(),
        _ => "https://interface3.music.163.com".to_string(),
    }
}

/// 拼接完整接口地址。
fn api_url(path: &str) -> String {
    format!("{}{}", api_base(), path)
}

/// 拼接 eapi 接口地址。
fn eapi_url(path: &str) -> String {
    format!("{}{}", eapi_base(), path)
}

/// 同一次运行内两次搜索之间的最小间隔，避免触发风控。
const MIN_SEARCH_INTERVAL: Duration = Duration::from_millis(300);

/// 网易云适配器。
pub struct NeteaseAdapter {
    /// 登录 Cookie（形如 `MUSIC_U=...; __csrf=...`）。
    cookie: Option<String>,
    /// 复用的 HTTP 客户端。
    client: reqwest::Client,
    /// 搜索限流用的「上次请求时间」。
    last_search: Mutex<Option<tokio::time::Instant>>,
}

impl NeteaseAdapter {
    /// 用可选 Cookie 创建适配器。
    pub fn new(cookie: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(12))
            // 网易云接口要求跟随重定向
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .unwrap_or_default();
        Self {
            cookie: cookie.filter(|c| !c.trim().is_empty()),
            client,
            last_search: Mutex::new(None),
        }
    }

    /// 更新 Cookie（登录后热替换，无需重建适配器）。
    pub fn set_cookie(&mut self, cookie: Option<String>) {
        self.cookie = cookie.filter(|c| !c.trim().is_empty());
    }

    /// 当前是否已登录（以 `MUSIC_U` 是否存在判定）。
    pub fn is_logged_in(&self) -> bool {
        self.cookie
            .as_deref()
            .map(|c| c.contains("MUSIC_U"))
            .unwrap_or(false)
    }

    /// 底层 HTTP 客户端。
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// 搜索限流：保证两次请求至少间隔 [`MIN_SEARCH_INTERVAL`]。
    async fn throttle(&self) {
        let mut guard = self.last_search.lock().await;
        if let Some(last) = *guard {
            let elapsed = last.elapsed();
            if elapsed < MIN_SEARCH_INTERVAL {
                tokio::time::sleep(MIN_SEARCH_INTERVAL - elapsed).await;
            }
        }
        *guard = Some(tokio::time::Instant::now());
    }

    /// 给请求加上公共头（UA 由 client 统一设置）。
    fn decorate(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let request = request
            .header("Referer", "https://music.163.com/")
            .header("Origin", "https://music.163.com");
        match self.cookie.as_deref() {
            Some(cookie) => request.header("Cookie", cookie),
            None => request,
        }
    }

    /// 搜索歌曲。
    pub async fn search(&self, keyword: &str) -> Result<Vec<Song>, MusicError> {
        self.throttle().await;

        let request = self
            .client
            .get(api_url("/api/search/get/web"))
            .query(&[("s", keyword), ("type", "1"), ("limit", "20"), ("offset", "0")]);
        let response = self
            .decorate(request)
            .send()
            .await
            .map_err(|e| MusicError::Network(e.to_string()))?;

        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| MusicError::Network(e.to_string()))?;
        if !status.is_success() {
            // 把状态码与响应片段带出来：网易云的风控页/错误页对排障很关键。
            return Err(MusicError::Network(format!(
                "搜索接口 HTTP {status}：{}",
                snippet(&text, 200)
            )));
        }

        parse_search_response(&text)
    }

    /// 获取歌词（优先 LRC）。
    pub async fn lyrics(&self, song_id: &str) -> Result<String, MusicError> {
        let request = self
            .client
            .get(api_url("/api/song/lyric"))
            .query(&[("id", song_id), ("lv", "1"), ("kv", "1"), ("tv", "-1")]);
        let response = self
            .decorate(request)
            .send()
            .await
            .map_err(|e| MusicError::Network(e.to_string()))?;
        let text = response
            .text()
            .await
            .map_err(|e| MusicError::Network(e.to_string()))?;
        parse_lyric_response(&text)
    }

    /// 获取播放地址（三级回退）。
    ///
    /// `level` 形如 `standard` / `higher` / `exhigh` / `lossless`。
    pub async fn play_url(&self, song_id: &str, level: &str) -> Result<String, MusicError> {
        // ① 旧 api 接口：免费曲目免登录可用
        match self.play_url_legacy(song_id).await {
            Ok(url) if !url.is_empty() => {
                debug!(song_id, source = "api", "取得播放地址");
                return Ok(url);
            }
            Ok(_) => debug!(song_id, "旧接口返回空地址，尝试 eapi"),
            Err(err) => debug!(song_id, error = %err, "旧接口取址失败，尝试 eapi"),
        }

        // ② eapi（需要登录 Cookie）
        if self.is_logged_in() {
            match self.play_url_eapi(song_id, level).await {
                Ok(url) if !url.is_empty() => {
                    debug!(song_id, source = "eapi", "取得播放地址");
                    return Ok(url);
                }
                Ok(_) => debug!(song_id, "eapi 返回空地址，尝试歌单兜底"),
                Err(err) => debug!(song_id, error = %err, "eapi 取址失败，尝试歌单兜底"),
            }
        } else {
            debug!(song_id, "未登录，跳过 eapi");
        }

        Err(MusicError::Unplayable)
    }

    /// ① 旧 `api` 接口取址。
    async fn play_url_legacy(&self, song_id: &str) -> Result<String, MusicError> {
        let ids = format!("[{song_id}]");
        let request = self
            .client
            .get(api_url("/api/song/enhance/player/url"))
            .query(&[("ids", ids.as_str()), ("br", "320000")]);
        let response = self
            .decorate(request)
            .send()
            .await
            .map_err(|e| MusicError::Network(e.to_string()))?;
        let text = response
            .text()
            .await
            .map_err(|e| MusicError::Network(e.to_string()))?;
        Ok(extract_first_url(&text))
    }

    /// ② eapi 接口取址（AES-128-CBC + 签名头）。
    async fn play_url_eapi(&self, song_id: &str, level: &str) -> Result<String, MusicError> {
        let path = "/api/song/enhance/player/url/v1";
        let params = json!({
            "ids": format!("[{song_id}]"),
            "level": level,
            "encodeType": "flac",
            "header": { "os": "pc", "appver": "8.9.70" },
        });
        let body = eapi_body(path, &params)?;

        let request = self
            .client
            .post(eapi_url("/eapi/song/enhance/player/url/v1"))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body);
        let response = self
            .decorate(request)
            .send()
            .await
            .map_err(|e| MusicError::Network(e.to_string()))?;
        let text = response
            .text()
            .await
            .map_err(|e| MusicError::Network(e.to_string()))?;
        Ok(extract_first_url(&text))
    }

    /// ③ 歌单兜底：从「我喜欢的音乐」歌单详情里找该曲目的 `url` 字段。
    ///
    /// 这是最后手段：歌单详情接口对已登录用户会内嵌一部分可播地址。
    pub async fn play_url_from_playlists(&self, song_id: &str) -> Result<String, MusicError> {
        if !self.is_logged_in() {
            return Err(MusicError::NotLoggedIn(
                MusicPlatform::Netease.display_name().to_string(),
            ));
        }

        // 找到当前用户 uid
        let uid = self
            .current_uid()
            .await
            .ok_or_else(|| MusicError::NotLoggedIn("网易云（无法获取用户 ID）".to_string()))?;

        let request = self
            .client
            .get(api_url("/api/v6/playlist/detail"))
            .query(&[("id", format!("{uid}")), ("n", "1000".into()), ("s", "0".into())]);
        let response = self
            .decorate(request)
            .send()
            .await
            .map_err(|e| MusicError::Network(e.to_string()))?;
        let text = response
            .text()
            .await
            .map_err(|e| MusicError::Network(e.to_string()))?;

        let value: Value = serde_json::from_str(&text)
            .map_err(|e| MusicError::ApiChanged(format!("歌单详情不是 JSON：{e}")))?;
        let tracks = value
            .get("playlist")
            .and_then(|p| p.get("tracks"))
            .and_then(|t| t.as_array())
            .ok_or_else(|| MusicError::ApiChanged("歌单详情缺少 playlist.tracks".to_string()))?;

        for track in tracks {
            let id = track.get("id").and_then(value_to_id);
            if id.as_deref() != Some(song_id) {
                continue;
            }
            if let Some(url) = track.get("url").and_then(|u| u.as_str()) {
                if !url.is_empty() {
                    info!(song_id, "通过歌单兜底取得播放地址");
                    return Ok(url.to_string());
                }
            }
        }
        Err(MusicError::Unplayable)
    }

    /// 列出当前登录用户的歌单（阶段 10b）。
    ///
    /// 接口：`GET /api/user/playlist?uid=<uid>&limit=100&offset=0`
    ///
    /// 返回的每个歌单带 `track_count`，界面据此提示"共 N 首"。
    pub async fn user_playlists(&self) -> Result<Vec<super::PlaylistInfo>, MusicError> {
        if !self.is_logged_in() {
            return Err(MusicError::NotLoggedIn(
                MusicPlatform::Netease.display_name().to_string(),
            ));
        }
        let uid = self
            .current_uid()
            .await
            .ok_or_else(|| MusicError::NotLoggedIn("网易云（无法获取用户 ID）".to_string()))?;

        let request = self.client.get(api_url("/api/user/playlist")).query(&[
            ("uid", uid.clone()),
            ("limit", "100".to_string()),
            ("offset", "0".to_string()),
        ]);
        let text = self
            .decorate(request)
            .send()
            .await
            .map_err(|e| MusicError::Network(e.to_string()))?
            .text()
            .await
            .map_err(|e| MusicError::Network(e.to_string()))?;

        let value: Value = serde_json::from_str(&text)
            .map_err(|e| MusicError::ApiChanged(format!("我的歌单不是 JSON：{e}")))?;
        // 登录失效时接口会返回 code=301 / 需要登录
        if let Some(code) = value.get("code").and_then(Value::as_i64) {
            if code != 200 {
                return Err(MusicError::NotLoggedIn(format!(
                    "网易云返回 code={code}（可能需要重新登录）"
                )));
            }
        }
        parse_user_playlists(&value)
    }

    /// 读取某个歌单的全部曲目（阶段 10b）。
    ///
    /// 接口：`GET /api/v6/playlist/detail?id=<playlist_id>&n=1000`
    ///
    /// ⚠️ `n` 必须给足：默认只返回前若干首，歌单稍大就会**静默截断**
    /// （用户会以为"导入少了一半"）。
    pub async fn playlist_tracks(
        &self,
        playlist_id: &str,
    ) -> Result<Vec<Song>, MusicError> {
        if !self.is_logged_in() {
            return Err(MusicError::NotLoggedIn(
                MusicPlatform::Netease.display_name().to_string(),
            ));
        }
        let request = self.client.get(api_url("/api/v6/playlist/detail")).query(&[
            ("id", playlist_id.to_string()),
            ("n", "1000".to_string()),
            ("s", "0".to_string()),
        ]);
        let text = self
            .decorate(request)
            .send()
            .await
            .map_err(|e| MusicError::Network(e.to_string()))?
            .text()
            .await
            .map_err(|e| MusicError::Network(e.to_string()))?;

        let value: Value = serde_json::from_str(&text)
            .map_err(|e| MusicError::ApiChanged(format!("歌单详情不是 JSON：{e}")))?;
        let tracks = value
            .get("playlist")
            .and_then(|p| p.get("tracks"))
            .and_then(|t| t.as_array())
            .ok_or_else(|| MusicError::ApiChanged("歌单详情缺少 playlist.tracks".to_string()))?;

        Ok(tracks
            .iter()
            .filter_map(|t| song_from_track(t, MusicPlatform::Netease))
            .collect())
    }

    /// 读取当前登录用户 UID。
    ///
    /// ## 两条路径都要试
    ///  - ① 公开接口 `/api/nuser/account/get`（网页登录态就能用）；
    ///  - ② eapi `/eapi/w/nuser/account/get`（只认 eapi 的登录方式）。
    ///
    /// ⚠️ 早期这里**只有 eapi 一条**，而 [`Self::verify_login`] 是先公开接口再 eapi。
    /// 于是出现自相矛盾的状态：界面显示「已登录」（verify_login 成功），
    /// 但歌单导入报「无法获取用户 ID」（本方法返回 None）——
    /// 实测就是踩在这里（探针里 `已登录: true` 而 `uid = 取不到`）。
    pub async fn current_uid(&self) -> Option<String> {
        // ① 公开接口
        let request = self.client.get(api_url("/api/nuser/account/get"));
        if let Ok(response) = self.decorate(request).send().await {
            if let Ok(text) = response.text().await {
                if let Ok(value) = serde_json::from_str::<Value>(&text) {
                    if let Some(uid) = value
                        .get("account")
                        .and_then(|a| a.get("id"))
                        .and_then(value_to_id)
                    {
                        return Some(uid);
                    }
                }
            }
        }

        // ② eapi 兜底
        let path = "/api/w/nuser/account/get";
        let body = eapi_body(path, &json!({})).ok()?;
        let request = self
            .client
            .post(api_url("/eapi/w/nuser/account/get"))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body);
        let text = self.decorate(request).send().await.ok()?.text().await.ok()?;
        let value: Value = serde_json::from_str(&text).ok()?;
        value
            .get("account")
            .and_then(|a| a.get("id"))
            .and_then(value_to_id)
    }

    /// 校验当前 Cookie 是否**真的**代表一个已登录账号，返回账号 UID。
    ///
    /// ## 为什么必须做这层校验
    /// 实测发现：网易云在页面加载时会写入 `MUSIC_U`，**未登录也有**。
    /// 因此「Cookie 里出现 `MUSIC_U`」并不能证明登录成功，会导致两个问题：
    ///  1. 退出登录后清掉 Cookie，再打开登录窗口，页面立刻又写回 `MUSIC_U`，
    ///     于是程序判定「已登录」→ 用户看到的是「秒登录」，根本没机会登录；
    ///  2. 界面显示已登录，但取版权曲时其实拿不到地址。
    ///
    /// 这里改用平台接口做**事实校验**：能查到账号 UID 才算登录成功。
    /// 先用公开接口 `/api/nuser/account/get`（网页登录态即可），
    /// 失败再退回 eapi 版本（兼容只认 eapi 的登录方式）。
    pub async fn verify_login(&self) -> Option<String> {
        if !self.is_logged_in() {
            return None;
        }

        // ① 公开接口
        let request = self.client.get(api_url("/api/nuser/account/get"));
        if let Ok(response) = self.decorate(request).send().await {
            if let Ok(text) = response.text().await {
                if let Ok(value) = serde_json::from_str::<Value>(&text) {
                    if let Some(uid) = value
                        .get("account")
                        .and_then(|a| a.get("id"))
                        .and_then(value_to_id)
                    {
                        return Some(uid);
                    }
                }
            }
        }

        // ② eapi 兜底
        self.current_uid().await
    }
}

#[async_trait]
impl MusicAdapter for NeteaseAdapter {
    fn platform(&self) -> MusicPlatform {
        MusicPlatform::Netease
    }

    async fn search(&self, keyword: &str) -> Result<Vec<Song>, MusicError> {
        NeteaseAdapter::search(self, keyword).await
    }

    async fn get_play_url(&self, song_id: &str) -> Result<String, MusicError> {
        self.play_url(song_id, DEFAULT_LEVEL).await
    }

    async fn get_lyrics(&self, song_id: &str) -> Result<String, MusicError> {
        self.lyrics(song_id).await
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// eapi 加密
// ─────────────────────────────────────────────────────────────────────────────

/// 构造 eapi 请求体。
///
/// 算法（社区逆向结论，已用真实请求验证可返回 `code: 200`）：
/// ```text
/// text     = json(params)
/// digest   = md5("nobody" + path + "use" + text + "md5forencrypt")
/// payload  = path + "-36cd479b6b5-" + text + "-36cd479b6b5-" + digest
/// body     = base64(AES-128-CBC(payload, key=e82ckenh8dichen8, iv=0102030405060708))
/// ```
pub fn eapi_body(path: &str, params: &Value) -> Result<String, MusicError> {
    let text = serde_json::to_string(params)
        .map_err(|e| MusicError::ApiChanged(format!("序列化 eapi 参数失败：{e}")))?;
    let digest = crate::bilibili::auth::md5_lite::md5(
        format!("nobody{path}use{text}md5forencrypt").as_bytes(),
    );
    let payload = format!("{path}-{EAPI_SALT}-{text}-{EAPI_SALT}-{}", hex::encode(digest));

    type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;
    let cipher = Aes128CbcEnc::new(EAPI_KEY.into(), EAPI_IV.into());
    let encrypted = cipher.encrypt_padded_vec_mut::<Pkcs7>(payload.as_bytes());
    Ok(base64::engine::general_purpose::STANDARD.encode(encrypted))
}

// ─────────────────────────────────────────────────────────────────────────────
// 响应解析（宽松、可单测）
// ─────────────────────────────────────────────────────────────────────────────

/// 把 JSON 里的数字/字符串 ID 统一成字符串。
pub fn value_to_id(value: &Value) -> Option<String> {
    match value {
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// 从播放地址响应里取第一个非空 URL（并做必要的清洗）。
///
/// 兼容 `data` 为数组或单对象两种形态。
pub fn extract_first_url(text: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return String::new();
    };
    let data = value.get("data");
    let first = match data {
        Some(Value::Array(list)) => list.first().cloned(),
        Some(obj @ Value::Object(_)) => Some(obj.clone()),
        _ => None,
    };
    let raw = first
        .and_then(|item| {
            item.get("url")
                .and_then(|u| u.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_default();
    sanitize_play_url(&raw)
}

/// 清洗直链：去掉会导致 CDN 拒绝的 `authSecret` 参数。
///
/// ## 为什么要这么做（实测结论）
/// 登录态下网易云会在直链里附加 `authSecret` 参数，形如
/// `http://m704.music.126.net/.../x.mp3?vuutv=...&authSecret=000001a0f5...&cdntag=...`。
/// 这个参数是官方客户端专用的下载令牌，**第三方请求带它会直接被 CDN 拒绝**：
///
/// ```text
/// HTTP/1.1 403 Forbidden
/// X-AUTH-MSG: auth failed - origin failed
/// Server: Tengine
/// ```
///
/// 表现为「播放器完全没有声音」——mpv 侧其实是
/// `[ffmpeg] http: Error reading HTTP response: Error number -10054`（连接被重置），
/// 而界面因为拿不到 `time-pos` 一直显示进度 0。
///
/// 把该参数去掉后同一 URL 立刻返回 **200**（已对多首曲目验证：
/// `1330348068` / `347230` / `65766` / `229271` 均为「带=403、去掉=200」）。
/// 其余参数（`vuutv`、`cdntag`）必须保留，它们是常规防盗链签名。
pub fn sanitize_play_url(url: &str) -> String {
    if url.is_empty() || !url.contains("authSecret=") {
        return url.to_string();
    }

    // 保留 fragment（正常情况下没有，但不要因为清洗而破坏 URL 结构）
    let (before_fragment, fragment) = match url.split_once('#') {
        Some((head, frag)) => (head, Some(frag)),
        None => (url, None),
    };
    let (path, query) = match before_fragment.split_once('?') {
        Some((path, query)) => (path, query),
        None => return url.to_string(),
    };

    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| !pair.starts_with("authSecret="))
        .collect();

    let mut cleaned = String::from(path);
    if !kept.is_empty() {
        cleaned.push('?');
        cleaned.push_str(&kept.join("&"));
    }
    if let Some(frag) = fragment {
        cleaned.push('#');
        cleaned.push_str(frag);
    }
    cleaned
}

/// 解析搜索响应为 [`Song`] 列表。
pub fn parse_search_response(text: &str) -> Result<Vec<Song>, MusicError> {
    let value: Value = serde_json::from_str(text)
        .map_err(|e| MusicError::ApiChanged(format!("搜索响应不是 JSON：{e}")))?;

    let code = value.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
    if code != 200 {
        let message = value
            .get("message")
            .or_else(|| value.get("msg"))
            .and_then(|m| m.as_str())
            .unwrap_or("");
        // 405 = 触发风控（"操作频繁"）。这是**可恢复**的，需要明确告诉用户稍后重试，
        // 而不是笼统地报「接口已变化」。
        if code == 405 {
            return Err(MusicError::Network(format!(
                "触发网易云风控（操作频繁），请等待 1~2 分钟后重试{}",
                if message.is_empty() {
                    String::new()
                } else {
                    format!("：{message}")
                }
            )));
        }
        return Err(MusicError::ApiChanged(format!(
            "搜索接口返回 code={code} message={message}"
        )));
    }

    let songs = value
        .get("result")
        .and_then(|r| r.get("songs"))
        .and_then(|s| s.as_array())
        .ok_or_else(|| MusicError::ApiChanged("搜索响应缺少 result.songs".to_string()))?;

    Ok(songs.iter().filter_map(song_from_search_item).collect())
}

/// 把搜索结果里的一项转成 [`Song`]。
pub fn song_from_search_item(item: &Value) -> Option<Song> {
    let id = item.get("id").and_then(value_to_id)?;
    let title = item.get("name").and_then(|n| n.as_str())?.to_string();

    // 歌手：优先 `artists`（web 搜索），兼容 `ar`（v6 接口）
    let mut artists: Vec<String> = Vec::new();
    for key in ["artists", "ar"] {
        if let Some(list) = item.get(key).and_then(|a| a.as_array()) {
            for artist in list {
                if let Some(name) = artist.get("name").and_then(|n| n.as_str()) {
                    artists.push(name.to_string());
                }
            }
            if !artists.is_empty() {
                break;
            }
        }
    }

    let album = item
        .get("album")
        .or_else(|| item.get("al"))
        .and_then(|a| a.get("name"))
        .and_then(|n| n.as_str())
        .map(|s| s.to_string());

    let cover_url = item
        .get("album")
        .or_else(|| item.get("al"))
        .and_then(|a| a.get("picUrl"))
        .and_then(|p| p.as_str())
        .map(|s| s.to_string());

    // 时长字段：搜索接口给毫秒（duration），专辑/歌单接口给秒（dt）
    let duration_ms = item
        .get("duration")
        .and_then(|d| d.as_u64())
        .or_else(|| item.get("dt").and_then(|d| d.as_u64()));
    let duration = duration_ms.map(|ms| ms / 1000).unwrap_or(0);

    Some(Song {
        id,
        title,
        artist: artists.join(" / "),
        platform: MusicPlatform::Netease,
        duration,
        cover_url,
        album,
        source: SongSource::Resolved,
        source_error: None,
    })
}

/// 解析「我的歌单」响应（阶段 10b）。
///
/// 网易云的结构是 `{ "code": 200, "playlist": [ {...}, ... ] }`。
/// 抽成纯函数便于单测：真实账号的歌单内容会变，但结构约定不会。
pub fn parse_user_playlists(value: &Value) -> Result<Vec<super::PlaylistInfo>, MusicError> {
    let list = value
        .get("playlist")
        .and_then(|p| p.as_array())
        .ok_or_else(|| MusicError::ApiChanged("我的歌单缺少 playlist 数组".to_string()))?;

    Ok(list
        .iter()
        .filter_map(|item| {
            let id = item.get("id").and_then(value_to_id)?;
            let name = item.get("name").and_then(|n| n.as_str())?.to_string();
            let track_count = item
                .get("trackCount")
                .and_then(|c| c.as_u64())
                .unwrap_or(0) as usize;
            let cover_url = item
                .get("coverImgUrl")
                .and_then(|c| c.as_str())
                .map(str::to_string);
            let creator = item
                .get("creator")
                .and_then(|c| c.get("nickname"))
                .and_then(|n| n.as_str())
                .map(str::to_string);
            // 「我喜欢的音乐」在网易云里 specialType=5；用它给界面一个标记
            let special = item
                .get("specialType")
                .and_then(|s| s.as_u64())
                .is_some_and(|s| s != 0);
            Some(super::PlaylistInfo {
                id,
                name,
                track_count,
                cover_url,
                creator,
                platform: MusicPlatform::Netease,
                special,
            })
        })
        .collect())
}

/// 把歌单详情里的一项转成 [`Song`]。
///
/// 歌单详情的字段名与搜索接口不同（`ar`/`al`/`dt`），
/// 复用 [`song_from_search_item`] 即可——它已经把两套字段都兼容了。
pub fn song_from_track(track: &Value, platform: MusicPlatform) -> Option<Song> {
    let mut song = song_from_search_item(track)?;
    song.platform = platform;
    Some(song)
}

/// 截取响应片段用于错误信息（避免把整页 HTML 塞进错误里）。
pub fn snippet(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "(空响应)".to_string();
    }
    let cut: String = trimmed.chars().take(max).collect();
    if trimmed.chars().count() > max {
        format!("{cut}…")
    } else {
        cut
    }
}

/// 解析歌词响应：优先 LRC，其次翻译，最后纯文本。
pub fn parse_lyric_response(text: &str) -> Result<String, MusicError> {
    let value: Value = serde_json::from_str(text)
        .map_err(|e| MusicError::ApiChanged(format!("歌词响应不是 JSON：{e}")))?;

    let pick = |key: &str| {
        value
            .get(key)
            .and_then(|v| v.get("lyric"))
            .and_then(|l| l.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };

    if let Some(lrc) = pick("lrc") {
        return Ok(lrc);
    }
    if let Some(translated) = pick("tlyric") {
        return Ok(translated);
    }
    Ok(String::new())
}

/// 一个「可搜索」的注册表条目：平台 + 关键词 → 首个匹配。
pub struct SearchPlan {
    /// 关键词。
    pub keyword: String,
    /// 候选映射，用于调试。
    pub debug: HashMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eapi_body_is_base64_and_deterministic() {
        let params = json!({ "ids": "[186016]", "level": "standard" });
        let a = eapi_body("/api/song/enhance/player/url/v1", &params).unwrap();
        let b = eapi_body("/api/song/enhance/player/url/v1", &params).unwrap();
        assert_eq!(a, b, "同样输入应得到同样密文");
        assert!(!a.is_empty());
        // 必须是合法 base64
        assert!(base64::engine::general_purpose::STANDARD.decode(a).is_ok());
    }

    #[test]
    fn eapi_body_changes_with_params() {
        let path = "/api/test";
        let a = eapi_body(path, &json!({ "id": "1" })).unwrap();
        let b = eapi_body(path, &json!({ "id": "2" })).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn sanitize_strips_auth_secret_only() {
        // 实测：带 authSecret 的链接被 CDN 以 403「auth failed - origin failed」拒绝，
        // 去掉后立刻 200。其余防盗链参数必须原样保留。
        let url = "http://m704.music.126.net/20261001/abc/jdymusic/obj/wo3/x.mp3\
                   ?vuutv=5CA92Q%2BomRY%3D&authSecret=000001a0f59e02bc14cb0a7df9ac0007&cdntag=bWFyaw";
        let cleaned = sanitize_play_url(url);
        assert!(!cleaned.contains("authSecret"), "必须去掉 authSecret");
        assert!(cleaned.contains("vuutv="), "vuutv 是常规签名，必须保留");
        assert!(cleaned.contains("cdntag="), "cdntag 必须保留");
        assert!(!cleaned.contains("&&"), "不能留下空参数位");
        assert!(cleaned.starts_with("http://m704.music.126.net/"), "路径不能变");
    }

    #[test]
    fn sanitize_is_noop_without_auth_secret() {
        let url = "http://m701.music.126.net/a/b.mp3?vuutv=xxx&cdntag=yyy";
        assert_eq!(sanitize_play_url(url), url, "没有 authSecret 时不能改动 URL");
        assert_eq!(sanitize_play_url(""), "", "空串安全返回");
        assert_eq!(sanitize_play_url("http://a.b/c.mp3"), "http://a.b/c.mp3");
    }

    #[test]
    fn sanitize_handles_auth_secret_in_any_position() {
        assert_eq!(
            sanitize_play_url("http://a/b.mp3?authSecret=1&vuutv=2&cdntag=3"),
            "http://a/b.mp3?vuutv=2&cdntag=3"
        );
        assert_eq!(
            sanitize_play_url("http://a/b.mp3?vuutv=2&cdntag=3&authSecret=1"),
            "http://a/b.mp3?vuutv=2&cdntag=3"
        );
        assert_eq!(
            sanitize_play_url("http://a/b.mp3?authSecret=1"),
            "http://a/b.mp3",
            "只剩该参数时要连 ? 一起去掉"
        );
    }

    #[test]
    fn extract_first_url_applies_sanitize() {
        let text = r#"{"data":[{"id":1,"url":"http://m1.music.126.net/x.mp3?vuutv=a&authSecret=b&cdntag=c","code":200}]}"#;
        let url = extract_first_url(text);
        assert!(url.contains("vuutv=a"));
        assert!(!url.contains("authSecret"), "取址入口就应该清洗掉");
    }

    #[test]
    fn extract_first_url_parses_object_data() {
        let text = r#"{"data":{"url":"http://m1.music.126.net/y.mp3?vuutv=a"}}"#;
        assert_eq!(
            extract_first_url(text),
            "http://m1.music.126.net/y.mp3?vuutv=a"
        );
        assert_eq!(extract_first_url(r#"{"data":[{"url":null}]}"#), "");
        assert_eq!(extract_first_url("not json"), "");
    }

    #[test]
    fn parses_search_response_with_artists_and_duration() {
        let raw = r#"{
            "result": {
                "songs": [
                    { "id": 186016, "name": "晴天", "duration": 269000,
                      "artists": [{ "name": "周杰伦" }],
                      "album": { "name": "叶惠美", "picUrl": "https://p1.music.126.net/x.jpg" } }
                ]
            },
            "code": 200
        }"#;
        let songs = parse_search_response(raw).expect("应解析成功");
        assert_eq!(songs.len(), 1);
        let song = &songs[0];
        assert_eq!(song.id, "186016");
        assert_eq!(song.title, "晴天");
        assert_eq!(song.artist, "周杰伦");
        assert_eq!(song.album.as_deref(), Some("叶惠美"));
        assert_eq!(song.duration, 269);
        assert!(song.is_resolved());
        assert_eq!(song.platform, MusicPlatform::Netease);
        assert!(song.cover_url.as_deref().unwrap().starts_with("https://"));
    }

    #[test]
    fn parses_v6_style_search_item() {
        // 部分接口用 ar/dt/al 字段名
        let item = json!({
            "id": "123",
            "name": "富士山下",
            "dt": 259000,
            "ar": [{ "name": "陈奕迅" }, { "name": "Another" }],
            "al": { "name": "What's Going On…?" }
        });
        let song = song_from_search_item(&item).expect("应解析成功");
        assert_eq!(song.artist, "陈奕迅 / Another");
        assert_eq!(song.duration, 259);
    }

    #[test]
    fn search_response_without_songs_is_api_changed() {
        assert!(matches!(
            parse_search_response(r#"{"code":200,"result":{}}"#),
            Err(MusicError::ApiChanged(_))
        ));
        assert!(matches!(
            parse_search_response("not json"),
            Err(MusicError::ApiChanged(_))
        ));
    }

    #[test]
    fn rate_limit_code_405_is_reported_as_retryable() {
        // 实测：请求过密时网易云返回 {"code":405,"message":"操作频繁，请稍候再试"}
        let raw = r#"{"msg":"操作频繁，请稍候再试","code":405,"message":"操作频繁，请稍候再试"}"#;
        match parse_search_response(raw) {
            Err(MusicError::Network(message)) => {
                assert!(message.contains("风控"), "应说明是风控：{message}");
                assert!(message.contains("重试"), "应提示可重试：{message}");
            }
            other => panic!("405 应映射为可重试的网络错误，实际：{other:?}"),
        }
    }

    #[test]
    fn other_error_codes_stay_api_changed() {
        match parse_search_response(r#"{"code":400,"message":"bad request"}"#) {
            Err(MusicError::ApiChanged(message)) => assert!(message.contains("400")),
            other => panic!("应映射为 ApiChanged，实际：{other:?}"),
        }
    }

    #[test]
    fn extracts_url_from_array_and_object() {
        let array = r#"{"code":200,"data":[{"url":"https://a.mp3","br":320000}]}"#;
        assert_eq!(extract_first_url(array), "https://a.mp3");

        let object = r#"{"code":200,"data":{"url":"https://b.mp3"}}"#;
        assert_eq!(extract_first_url(object), "https://b.mp3");

        // 版权受限：url 为空
        assert_eq!(extract_first_url(r#"{"code":200,"data":[{"url":null}]}"#), "");
        assert_eq!(extract_first_url("garbage"), "");
    }

    #[test]
    fn parses_lyrics_preferring_lrc() {
        let raw = r#"{
            "lrc": { "lyric": "[00:00.000] 作词 : 周杰伦\n[00:01.000] 作曲 : 周杰伦" },
            "tlyric": { "lyric": "[00:00.000] Lyrics by Jay" }
        }"#;
        let lyric = parse_lyric_response(raw).expect("应解析成功");
        assert!(lyric.starts_with("[00:00.000] 作词"));

        let only_translation = r#"{"lrc":{"lyric":""},"tlyric":{"lyric":"translated"}}"#;
        assert_eq!(parse_lyric_response(only_translation).unwrap(), "translated");

        // 纯音乐：两者都空 → 返回空串而不是报错
        assert_eq!(
            parse_lyric_response(r#"{"lrc":{"lyric":""},"tlyric":{"lyric":""}}"#).unwrap(),
            ""
        );
    }

    #[test]
    fn logged_in_detection_uses_music_u() {
        let anon = NeteaseAdapter::new(None);
        assert!(!anon.is_logged_in());
        let with_other = NeteaseAdapter::new(Some("__csrf=abc".into()));
        assert!(!with_other.is_logged_in());
        let logged = NeteaseAdapter::new(Some("MUSIC_U=token; __csrf=abc".into()));
        assert!(logged.is_logged_in());
    }

    #[test]
    fn set_cookie_can_clear_login_state() {
        let mut adapter = NeteaseAdapter::new(Some("MUSIC_U=token".into()));
        assert!(adapter.is_logged_in());
        adapter.set_cookie(None);
        assert!(!adapter.is_logged_in());
        adapter.set_cookie(Some("   ".into()));
        assert!(!adapter.is_logged_in(), "空白 Cookie 应视为未登录");
    }

    // ── 歌单解析（阶段 10b）───────────────────────────────────────────────
    //
    // 真实账号实测：39 个歌单，最大一个 511 首。
    // 下面用裁剪过的真实响应片段覆盖解析边界。

    #[test]
    fn parses_user_playlists() {
        let value: Value = serde_json::from_str(
            r#"{"code":200,"playlist":[
                {"id":832020815,"name":"墨_一喜欢的音乐","trackCount":511,
                 "coverImgUrl":"http://p1.music.126.net/x.jpg","specialType":5,
                 "creator":{"nickname":"墨_一"}},
                {"id":3068408053,"name":"vip","trackCount":1,"specialType":0}
            ]}"#,
        )
        .unwrap();
        let list = parse_user_playlists(&value).expect("应能解析");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "832020815");
        assert_eq!(list[0].name, "墨_一喜欢的音乐");
        assert_eq!(list[0].track_count, 511);
        assert!(list[0].special, "specialType=5 应标记为我喜欢");
        assert_eq!(list[0].creator.as_deref(), Some("墨_一"));
        assert!(!list[1].special);
        // 缺 coverImgUrl 不该导致整条丢失
        assert_eq!(list[1].cover_url, None);
    }

    #[test]
    fn user_playlists_missing_array_is_api_change() {
        let value: Value = serde_json::from_str(r#"{"code":200}"#).unwrap();
        assert!(parse_user_playlists(&value).is_err());
    }

    #[test]
    fn playlist_track_uses_v6_field_names() {
        // 歌单详情用 ar/al/dt（与搜索接口的 artists/album/duration 不同）
        let track: Value = serde_json::from_str(
            r#"{"id":347230,"name":"海阔天空","dt":326000,
                "ar":[{"name":"Beyond"}],"al":{"name":"乐与怒","picUrl":"http://p.x/y.jpg"}}"#,
        )
        .unwrap();
        let song = song_from_track(&track, MusicPlatform::Netease).expect("应能解析");
        assert_eq!(song.id, "347230");
        assert_eq!(song.title, "海阔天空");
        assert_eq!(song.artist, "Beyond");
        assert_eq!(song.duration, 326, "毫秒应换算成秒");
        assert_eq!(song.album.as_deref(), Some("乐与怒"));
        assert_eq!(song.platform, MusicPlatform::Netease);
    }

    #[test]
    fn playlist_track_without_id_is_skipped() {
        let track: Value = serde_json::from_str(r#"{"name":"没有ID","dt":1000}"#).unwrap();
        assert!(song_from_track(&track, MusicPlatform::Netease).is_none());
    }
}
