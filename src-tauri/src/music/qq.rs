//! QQ 音乐适配器。
//!
//! 接口：
//!  - 搜索：`POST https://u.y.qq.com/cgi-bin/musicu.fcg`（`DoSearchForQQMusicDesktop`）
//!  - 播放地址 / 歌词：见下文说明（尚未实现）
//!
//! Cookie 存储：见 [`super::secrets::KEYRING_ENTRY_QQ_COOKIE`]。
//!
//! ⚠️ 关于旧的 `c.y.qq.com/soso/fcgi-bin/client_search_cp`：实测已失效，
//! 直接返回 `HTTP 500`，因此改用官方 `musicu.fcg` 接口。

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::models::{MusicPlatform, Song};

use super::netease::value_to_id;
use super::{MusicAdapter, MusicError};

/// QQ 音乐搜索接口。
const SEARCH_URL: &str = "https://u.y.qq.com/cgi-bin/musicu.fcg";

/// QQ 音乐歌词接口（**必须带 Referer: y.qq.com**，否则 403）。
const LYRICS_URL: &str = "https://c.y.qq.com/lyric/fcgi-bin/fcg_query_lyric_new.fcg";

/// QQ 音乐歌单详情（老式 fcg 网关，同样需要 Referer）。
const PLAYLIST_URL: &str = "https://c.y.qq.com/qzone/fcg-bin/fcg_ucc_getcdinfo_byids_cp.fcg";

/// QQ 音乐适配器。
pub struct QqAdapter {
    cookie: Option<String>,
    client: reqwest::Client,
}

impl QqAdapter {
    /// 用可选 Cookie 创建适配器。

    /// 统一的 `musicu.fcg` POST（带 cookie 与 Referer）。
    async fn post_musicu(&self, payload: &Value) -> Result<String, MusicError> {
        let request = self
            .decorate(self.client.post(SEARCH_URL))
            .header("Content-Type", "application/json")
            .header("Referer", "https://y.qq.com/")
            .body(payload.to_string());
        let response = request
            .send()
            .await
            .map_err(|e| MusicError::Network(e.to_string()))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| MusicError::Network(e.to_string()))?;
        if !status.is_success() {
            return Err(MusicError::Network(format!(
                "QQ音乐接口 HTTP {status}：{}",
                super::netease::snippet(&text, 200)
            )));
        }
        Ok(text)
    }

    /// 列出当前登录用户的歌单（阶段 10b）。
    ///
    /// ## 接口
    /// `POST musicu.fcg`，module `music.musicasset.PlaylistBaseRead`，
    /// method `GetPlaylistByUin`，`param.uin` 传**数字 QQ 号**。
    ///
    /// ⚠️ 这个 module 名是实测试出来的，网上流传的几个老名字都不通：
    ///  - `music.web_srf_diss.FcgiGetDiss` → `code=500003`（模块不存在）
    ///  - `music.songlist.SonglistRead`    → 同样 500003
    ///  - `music.musicasset.PlaylistBaseRead` + `GetPlaylistBase` → 40000（方法名错）
    ///
    /// 返回的 `v_playlist[]` 里，「我喜欢」的 `dirId` 固定是 `201`。
    pub async fn user_playlists(&self) -> Result<Vec<super::PlaylistInfo>, MusicError> {
        let uin = self
            .login_uin()
            .ok_or_else(|| MusicError::NotLoggedIn("QQ音乐（无法从 Cookie 解析出 uin）".to_string()))?;

        let payload = json!({
            "comm": { "ct": 24, "cv": 0 },
            "req_0": {
                "module": "music.musicasset.PlaylistBaseRead",
                "method": "GetPlaylistByUin",
                "param": { "uin": uin }
            }
        });
        let text = self.post_musicu(&payload).await?;
        parse_user_playlists(&text)
    }

    /// 读取某个歌单的全部曲目（阶段 10b）。
    ///
    /// `disstid` 用 [`super::PlaylistInfo::id`]（即 QQ 的 `tid`）。
    ///
    /// ⚠️ `song_begin=0` + `song_num=1000`：默认只回前若干首，
    /// 歌单大了会**静默截断**，用户会以为"导入少了一半"。
    pub async fn playlist_tracks(&self, disstid: &str) -> Result<Vec<Song>, MusicError> {
        let uin = self.login_uin().unwrap_or_default();
        let url = format!(
            "{PLAYLIST_URL}?type=1&utf8=1&onlysong=0&disstid={disstid}&format=json&g_tk=5381&loginUin={uin}&hostUin=0&inCharset=utf8&outCharset=utf-8&notice=0&platform=yqq.json&needNewCode=0&song_begin=0&song_num=1000"
        );
        let request = self
            .decorate(self.client.get(&url))
            .header("Referer", "https://y.qq.com/");
        let response = request
            .send()
            .await
            .map_err(|e| MusicError::Network(e.to_string()))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| MusicError::Network(e.to_string()))?;
        if !status.is_success() {
            return Err(MusicError::Network(format!(
                "QQ音乐歌单详情 HTTP {status}：{}",
                super::netease::snippet(&text, 200)
            )));
        }
        parse_playlist_tracks(&text)
    }

    /// 从 Cookie 里解析出数字 QQ 号（uin/luin/wxuin）。
    ///
    /// 歌单接口要求 `uin` 是**纯数字**，而 Cookie 里的值常带 `o` 前缀
    /// （`o0123456789`），不去掉会被判为未登录。
    pub fn login_uin(&self) -> Option<String> {
        let cookie = self.cookie.as_deref()?;
        for part in cookie.split(';') {
            let mut kv = part.trim().splitn(2, '=');
            let (Some(key), Some(value)) = (kv.next(), kv.next()) else {
                continue;
            };
            if matches!(key.trim(), "uin" | "luin" | "wxuin") {
                let cleaned = value.trim().trim_start_matches('o');
                if !cleaned.is_empty() && cleaned.chars().all(|c| c.is_ascii_digit()) {
                    return Some(cleaned.to_string());
                }
            }
        }
        None
    }
    pub fn new(cookie: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36",
            )
            .build()
            .unwrap_or_default();
        Self { cookie, client }
    }

    /// 是否已登录（QQ 音乐以 `qqmusic_uin` / `qm_keyst` 判定）。
    pub fn is_logged_in(&self) -> bool {
        self.cookie
            .as_deref()
            .map(|c| c.contains("qm_keyst") || c.contains("qqmusic_key"))
            .unwrap_or(false)
    }

    /// 底层 HTTP 客户端。
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// 给请求加上 QQ 音乐需要的头与 Cookie。
    fn decorate(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let request = request
            .header("Referer", "https://y.qq.com/")
            .header("Origin", "https://y.qq.com");
        match &self.cookie {
            Some(cookie) if !cookie.is_empty() => request.header("Cookie", cookie),
            _ => request,
        }
    }
}

#[async_trait]
impl MusicAdapter for QqAdapter {
    fn platform(&self) -> MusicPlatform {
        MusicPlatform::Qq
    }

    async fn search(&self, keyword: &str) -> Result<Vec<Song>, MusicError> {
        let keyword = keyword.trim();
        if keyword.is_empty() {
            return Ok(Vec::new());
        }

        // 官方接口的请求体格式固定：comm + req 两层包装
        let payload = json!({
            "comm": { "ct": "19", "cv": "1859", "uin": "0" },
            "req": {
                "method": "DoSearchForQQMusicDesktop",
                "module": "music.search.SearchCgiService",
                "param": {
                    "num_per_page": "20",
                    "page_num": "1",
                    "query": keyword,
                    "search_type": "0",
                }
            }
        });

        let request = self
            .client
            .post(SEARCH_URL)
            .header("Content-Type", "application/json")
            .body(payload.to_string());
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
            return Err(MusicError::Network(format!(
                "QQ音乐搜索接口 HTTP {status}：{}",
                super::netease::snippet(&text, 200)
            )));
        }

        parse_search_response(&text)
    }

    /// QQ 音乐播放地址（`vkey` 换直链）。
    async fn get_play_url(&self, song_id: &str) -> Result<String, MusicError> {
        self.get_play_info(song_id).await.map(|info| info.url)
    }



    /// QQ 音乐歌词。
    ///
    /// ## 接口说明
    /// `GET https://c.y.qq.com/lyric/fcgi-bin/fcg_query_lyric_new.fcg`
    ///
    /// ⚠️ **必须带 `Referer: https://y.qq.com/`**：这个接口是老式 fcg 网关，
    /// 不带 Referer 会直接返回 `HTTP 403` 或一段非 JSON 的错误页
    /// （和搜索用的 `musicu.fcg` 不同，那个不需要）。
    ///
    /// 返回体形如：
    /// ```json
    /// { "retcode": 0, "lyric": "<base64 的 LRC>", "trans": "<base64 的翻译 LRC>" }
    /// ```
    ///
    /// 这里把 base64 解开并返回 LRC 文本；`trans` 存在时**合并**进来
    /// （面板的 `parseLrc` 会按时间轴匹配，翻译行会自然跟在原词后面）。
    async fn get_lyrics(&self, song_id: &str) -> Result<String, MusicError> {
        let url = format!(
            "{LYRICS_URL}?songmid={}&format=json&nobase64=0&g_tk=5381&loginUin=0&hostUin=0&inCharset=utf8&outCharset=utf-8&notice=0&platform=yqq.json&needNewCode=0",
            song_id
        );
        let request = self
            .decorate(self.client.get(&url))
            .header("Referer", "https://y.qq.com/")
            .header("Accept", "application/json");

        let response = request
            .send()
            .await
            .map_err(|e| MusicError::Network(e.to_string()))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| MusicError::Network(e.to_string()))?;
        if !status.is_success() {
            return Err(MusicError::Network(format!(
                "QQ音乐歌词接口 HTTP {status}：{}",
                super::netease::snippet(&text, 200)
            )));
        }
        parse_lyrics_response(&text)
    }
}

/// 解析歌词接口响应（抽出成自由函数便于单测）。
pub fn parse_lyrics_response(body: &str) -> Result<String, MusicError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| MusicError::Network(format!("QQ音乐歌词响应不是合法 JSON：{e}")))?;

    // 有些情况下 retcode 是字符串，两种都容忍
    let retcode = value
        .get("retcode")
        .and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
        })
        .unwrap_or(0);
    if retcode != 0 {
        // 歌词拿不到不影响播放，所以这里用 Network 而不是 Unplayable：
        // 上层对歌词失败本来就只记一条日志，不会中断播放。
        return Err(MusicError::Network(format!(
            "QQ音乐歌词接口返回 retcode={retcode}"
        )));
    }

    let lyric = value
        .get("lyric")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if lyric.is_empty() {
        // 纯音乐 / 无版权曲目：返回空歌词而不是错误，
        // 面板会显示「（暂无歌词）」而不是「获取歌词失败」。
        return Ok(String::new());
    }

    let decoded = decode_base64(lyric)?;
    let translated = value
        .get("trans")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| decode_base64(s).ok())
        .unwrap_or_default();

    if translated.trim().is_empty() {
        return Ok(decoded);
    }
    Ok(merge_translation(&decoded, &translated))
}

/// 解析 QQ「我的歌单」响应（阶段 10b）。
///
/// 结构：`req_0.data.v_playlist[]`，每项字段见下。
/// 抽成纯函数便于单测（真实账号的歌单会变，结构约定不会）。
pub fn parse_user_playlists(body: &str) -> Result<Vec<super::PlaylistInfo>, MusicError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| MusicError::ApiChanged(format!("QQ音乐我的歌单不是 JSON：{e}")))?;

    let req = value.get("req_0").unwrap_or(&Value::Null);
    if let Some(code) = req.get("code").and_then(Value::as_i64) {
        if code != 0 {
            return Err(MusicError::NotLoggedIn(format!(
                "QQ音乐返回 code={code}（登录可能已失效）"
            )));
        }
    }

    let list = req
        .get("data")
        .and_then(|d| d.get("v_playlist"))
        .and_then(|p| p.as_array())
        .ok_or_else(|| MusicError::ApiChanged("我的歌单缺少 req_0.data.v_playlist".to_string()))?;

    Ok(list
        .iter()
        .filter_map(|item| {
            // `tid` 是歌单详情要用的 ID；`dirId` 是分类目录（201 = 我喜欢）
            let id = item.get("tid").and_then(value_to_id)?;
            let name = item.get("dirName").and_then(|n| n.as_str())?.to_string();
            let track_count = item
                .get("songNum")
                .and_then(|c| c.as_u64())
                .unwrap_or(0) as usize;
            let cover_url = item
                .get("picUrl")
                .and_then(|c| c.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let creator = item
                .get("nick")
                .and_then(|n| n.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            // dirId=201 是「我喜欢」
            let special = item
                .get("dirId")
                .and_then(|d| d.as_u64())
                .is_some_and(|d| d == 201);
            Some(super::PlaylistInfo {
                id,
                name,
                track_count,
                cover_url,
                creator,
                platform: MusicPlatform::Qq,
                special,
            })
        })
        .collect())
}

/// 解析 QQ 歌单详情响应。
///
/// 结构：`cdlist[0].songlist[]`，曲目字段是**搜索那套**
/// （`songmid`/`songname`/`singer[].name`/`interval`），
/// 因此复用 [`song_from_search_item`]。
pub fn parse_playlist_tracks(body: &str) -> Result<Vec<Song>, MusicError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| MusicError::ApiChanged(format!("QQ音乐歌单详情不是 JSON：{e}")))?;

    let songs = value
        .get("cdlist")
        .and_then(|c| c.as_array())
        .and_then(|list| list.first())
        .and_then(|first| first.get("songlist"))
        .and_then(|s| s.as_array())
        .ok_or_else(|| MusicError::ApiChanged("歌单详情缺少 cdlist[0].songlist".to_string()))?;

    Ok(songs.iter().filter_map(song_from_playlist_item).collect())
}

/// 把 QQ 歌单详情里的一项转成 [`Song`]。
///
/// ## ⚠️ 为什么不能复用 `song_from_search_item`
/// 歌单详情的字段名与搜索接口**完全不同**：
///
/// | 含义 | 搜索接口 | 歌单详情 |
/// |------|---------|---------|
/// | ID | `mid` | `songmid` |
/// | 歌名 | `title` | `songname` |
/// | 歌手 | `singer[]` | `singer[]` |
/// | 时长 | `interval` | `interval` |
///
/// 早期直接复用搜索用的解析（它要求 `id` + `name`），
/// 结果 **529 首全被过滤成 0 首**——歌单列表能显示、点进去却是空的。
/// 实测排查过程：裸 curl 拿到 401376 字节 / 529 项，
/// 但适配器返回 0 首，说明网络没问题、是解析把所有项都丢掉了。
pub fn song_from_playlist_item(item: &Value) -> Option<Song> {
    let id = item.get("songmid").and_then(|v| v.as_str())?.to_string();
    let title = item.get("songname").and_then(|v| v.as_str())?.to_string();

    // 歌手：`singer` 是 `[{name: "..."}]`，多歌手用 ` / ` 连接
    let mut artists: Vec<String> = Vec::new();
    if let Some(list) = item.get("singer").and_then(|s| s.as_array()) {
        for artist in list {
            if let Some(name) = artist.get("name").and_then(|n| n.as_str()) {
                artists.push(name.to_string());
            }
        }
    }

    let album = item
        .get("albumname")
        .and_then(|a| a.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    // 歌单接口不给封面，用 albummid 按 QQ 的固定格式拼
    let cover_url = item
        .get("albummid")
        .and_then(|a| a.as_str())
        .filter(|s| !s.is_empty())
        .map(|mid| format!("https://y.qq.com/music/photo_new/T002R300x300M000{mid}.jpg"));

    let duration = item.get("interval").and_then(|d| d.as_u64()).unwrap_or(0);

    Some(Song {
        id,
        title,
        artist: artists.join(" / "),
        platform: MusicPlatform::Qq,
        duration,
        cover_url,
        album,
        source: crate::models::SongSource::Resolved,
        source_error: None,
    })
}

/// 解码歌词接口返回的 base64（标准字母表，带 `=` 填充）。
///
/// 自己实现而不是引一个新依赖：只需要解码，且输入一定是合法 base64。
fn decode_base64(input: &str) -> Result<String, MusicError> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lookup = [255u8; 256];
    for (i, &ch) in TABLE.iter().enumerate() {
        lookup[ch as usize] = i as u8;
    }

    let mut out: Vec<u8> = Vec::with_capacity(input.len() / 4 * 3);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for ch in input.bytes() {
        if ch == b'=' {
            break;
        }
        // 跳过换行等空白
        let value = lookup[ch as usize];
        if value == 255 {
            if ch.is_ascii_whitespace() {
                continue;
            }
            return Err(MusicError::Network(format!(
                "QQ音乐歌词 base64 含非法字符：{}",
                ch as char
            )));
        }
        buffer = (buffer << 6) | value as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }

    String::from_utf8(out).map_err(|e| MusicError::Network(format!("QQ音乐歌词不是合法 UTF-8：{e}")))
}

/// 把翻译行按时间戳并入原文歌词。
///
/// 面板的 `parseLrc` 是按 `[mm:ss.xx]` 时间轴解析的，
/// 所以只要把翻译行原样插进去（时间戳相同），就会显示在同一时刻。
fn merge_translation(original: &str, translated: &str) -> String {
    let mut merged = original.trim_end().to_string();
    merged.push('\n');
    merged.push_str(translated.trim());
    merged
}

impl QqAdapter {
    /// QQ 音乐取址（带「是否试听片段」标记）。
    ///
    /// ## 为什么单独一个方法
    /// 非会员账号遇到版权曲目时，QQ 会返回一段 95 秒左右的**试听**而不是完整版
    /// （实测周杰伦《青花瓷》：`purl` 有值但 `result=104003`）。
    /// 这个信息在 `trait MusicAdapter` 的 `get_play_url` 里表达不了
    /// （它只能返回一个字符串），所以额外提供本方法，
    /// 由 [`crate::music::service::MusicService::play_url_for`] 用它决定是否换平台。
    pub async fn get_play_info(&self, song_id: &str) -> Result<QqPlayInfo, MusicError> {
        let guid = format!("10000{}", stable_guid());
        let payload = json!({
            "req_0": {
                "module": "vkey.GetVkeyServer",
                "method": "CgiGetVkey",
                "param": {
                    "guid": guid,
                    "songmid": [song_id],
                    "songtype": [0],
                    "uin": "0",
                    "loginflag": 1,
                    "platform": "20"
                }
            },
            "comm": { "uin": 0, "format": "json", "ct": 24, "cv": 0 }
        });

        let request = self
            .client
            .post(SEARCH_URL)
            .header("Content-Type", "application/json")
            .body(payload.to_string());
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
            return Err(MusicError::Network(format!(
                "QQ音乐取址接口 HTTP {status}：{}",
                super::netease::snippet(&text, 200)
            )));
        }

        parse_play_info_response(&text)
    }
}

/// 解析 QQ 音乐搜索响应。
///
/// 响应结构：`req.data.body.song.list[]`，每项含 `name`（歌名）、
/// `singer[].name`（歌手）、`mid`（歌曲标识）、`interval`（时长，秒）、
/// `album.name` / `album.pmid`（专辑与封面）。
pub fn parse_search_response(text: &str) -> Result<Vec<Song>, MusicError> {
    let value: Value = serde_json::from_str(text)
        .map_err(|e| MusicError::ApiChanged(format!("QQ音乐搜索响应不是 JSON：{e}")))?;

    // 外层与内层都有 code，任一非 0 都视为失败
    let outer_code = value.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
    if outer_code != 0 {
        return Err(MusicError::ApiChanged(format!(
            "QQ音乐搜索外层 code={outer_code}"
        )));
    }
    let req = value.get("req").unwrap_or(&Value::Null);
    let inner_code = req.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
    if inner_code != 0 {
        let message = req
            .get("data")
            .and_then(|d| d.get("msg"))
            .and_then(|m| m.as_str())
            .unwrap_or("未知错误");
        return Err(MusicError::ApiChanged(format!(
            "QQ音乐搜索失败 code={inner_code} message={message}"
        )));
    }

    let list = req
        .get("data")
        .and_then(|d| d.get("body"))
        .and_then(|b| b.get("song"))
        .and_then(|s| s.get("list"))
        .and_then(|l| l.as_array())
        .ok_or_else(|| MusicError::ApiChanged("QQ音乐搜索响应缺少 req.data.body.song.list".to_string()))?;

    let songs: Vec<Song> = list.iter().filter_map(song_from_search_item).collect();
    Ok(songs)
}

/// 把 QQ 音乐搜索结果里的一项转成 [`Song`]。
fn song_from_search_item(item: &Value) -> Option<Song> {
    // `mid` 是 QQ 音乐的稳定歌曲标识；`id` 是数字 id（可能缺失）
    let mid = item
        .get("mid")
        .and_then(|m| m.as_str())
        .filter(|m| !m.is_empty())
        .map(|m| m.to_string())
        .or_else(|| {
            item.get("songmid")
                .and_then(|m| m.as_str())
                .map(|m| m.to_string())
        })
        .or_else(|| {
            item.get("id")
                .and_then(|i| i.as_i64())
                .map(|i| i.to_string())
        })?;

    let title = item.get("name").and_then(|n| n.as_str())?.to_string();
    if title.is_empty() {
        return None;
    }

    // 歌手：`singer` 是数组（桌面接口），兼容 `singer[].name` 与字符串
    let mut artists: Vec<String> = Vec::new();
    match item.get("singer") {
        Some(Value::Array(list)) => {
            for singer in list {
                if let Some(name) = singer.get("name").and_then(|n| n.as_str()) {
                    if !name.is_empty() {
                        artists.push(name.to_string());
                    }
                }
            }
        }
        Some(Value::String(s)) if !s.is_empty() => artists.push(s.clone()),
        _ => {}
    }

    let album = item
        .get("album")
        .and_then(|a| a.get("name"))
        .and_then(|n| n.as_str())
        .filter(|n| !n.is_empty())
        .map(|n| n.to_string());

    // 封面：QQ 的 album 有 pmid，可拼出封面地址
    let cover_url = item
        .get("album")
        .and_then(|a| a.get("pmid"))
        .and_then(|p| p.as_str())
        .filter(|p| !p.is_empty())
        .map(|pmid| format!("https://y.qq.com/music/photo_new/T002R300x300M000{pmid}.jpg"));

    // `interval` 是秒
    let duration = item
        .get("interval")
        .and_then(|i| i.as_u64())
        .or_else(|| {
            item.get("interval")
                .and_then(|i| i.as_str())
                .and_then(|s| s.parse::<u64>().ok())
        })
        .unwrap_or(0);

    Some(Song {
        id: mid,
        title,
        artist: artists.join(" / "),
        platform: MusicPlatform::Qq,
        duration,
        cover_url,
        album,
        source: Default::default(),
        source_error: None,
    })
}

/// 生成一个稳定的数字 guid。
///
/// QQ 接口要求 `guid` 是数字串；用固定值即可（官方客户端也是固定 guid）。
/// 这里取进程内的固定随机值，避免每次请求都变导致 CDN 缓存失效。
fn stable_guid() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static GUID: AtomicU32 = AtomicU32::new(0);

    let existing = GUID.load(Ordering::Relaxed);
    if existing != 0 {
        return existing;
    }

    // 用时间戳派生一个 8 位数字，避免与其它客户端撞车
    let derived = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0))
        % 100_000_000;
    let derived = derived.max(10_000_000);

    // 注意：`compare_exchange` 失败时 `Err` 里带的是**当前实际值**，
    // 必须用它作为返回值；否则并发下会各自返回不同的 guid（曾经导致测试失败）。
    match GUID.compare_exchange(0, derived, Ordering::Relaxed, Ordering::Relaxed) {
        Ok(_) => derived,
        Err(actual) => actual,
    }
}

/// QQ 取址结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QqPlayInfo {
    /// 可播放直链。
    pub url: String,
    /// 是否只是**试听片段**。
    ///
    /// 非会员账号遇到版权曲目时，QQ 会返回一段 95 秒左右的试听而不是完整版
    /// （实测周杰伦《青花瓷》：`purl` 有值、`result=104003`、实际时长 95 秒）。
    /// 上层据此决定要不要去另一个平台找完整版。
    pub preview: bool,
}

/// 解析 `CgiGetVkey` 响应，取出可播放直链与「是否试听片段」。
///
/// 成功时 `req_0.data.midurlinfo[0].purl` 形如
/// `C400...m4a?vkey=...&guid=...`，需要拼上 `sip` 里的 CDN 域名。
///
/// 三种结果：
///  - `purl` 为空 → [`MusicError::Unplayable`]（无版权，交由上层换平台）；
///  - `result` 非 0（如 `104003`）→ 可播放但**只是试听**；
///  - 其它 → 完整版。
pub fn parse_play_info_response(text: &str) -> Result<QqPlayInfo, MusicError> {
    let value: Value = serde_json::from_str(text)
        .map_err(|e| MusicError::ApiChanged(format!("QQ音乐取址响应不是 JSON：{e}")))?;

    let req = value.get("req_0").unwrap_or(&Value::Null);
    let code = req.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
    if code != 0 {
        return Err(MusicError::ApiChanged(format!(
            "QQ音乐取址失败 code={code}"
        )));
    }

    let data = req.get("data").unwrap_or(&Value::Null);
    let first = data
        .get("midurlinfo")
        .and_then(|m| m.as_array())
        .and_then(|list| list.first());

    let purl = first
        .and_then(|item| item.get("purl"))
        .and_then(|p| p.as_str())
        .unwrap_or("");

    if purl.is_empty() {
        // 版权/会员限制到连试听都没有，交由上层跨平台兜底
        return Err(MusicError::Unplayable);
    }

    // result 非 0 表示该曲目受限制（104003 = 需要会员，第三方只给试听）
    let result_code = first
        .and_then(|item| item.get("result"))
        .and_then(|r| r.as_i64())
        .unwrap_or(0);
    let preview = result_code != 0;

    let url = if purl.starts_with("http://") || purl.starts_with("https://") {
        purl.to_string()
    } else {
        let sip = data
            .get("sip")
            .and_then(|s| s.as_array())
            .and_then(|list| list.iter().find_map(|v| v.as_str()))
            .unwrap_or("http://aqqmusic.tc.qq.com/");
        format!("{sip}{purl}")
    };

    Ok(QqPlayInfo { url, preview })
}

/// 兼容旧的「只要一个直链」调用方式。
pub fn parse_play_url_response(text: &str) -> Result<String, MusicError> {
    parse_play_info_response(text).map(|info| info.url)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 歌词解析（阶段 9）─────────────────────────────────────────────────
    //
    // 真实接口实测可用（青花瓷 988 字符 / 49 行，夜曲 1463 字符 / 76 行），
    // 这里用构造的响应覆盖边界：base64 解码、翻译合并、无歌词、retcode 非 0。

    /// `[00:00.02]测试` 的 base64。
    const LRC_B64: &str = "WzAwOjAwLjAyXea1i+ivlQ==";

    #[test]
    fn decodes_base64_lyric() {
        let body = format!(r#"{{"retcode":0,"lyric":"{LRC_B64}"}}"#);
        let lrc = parse_lyrics_response(&body).expect("应能解析");
        assert_eq!(lrc, "[00:00.02]测试");
    }

    #[test]
    fn merges_translation_after_original() {
        // 翻译同样是 base64
        let trans_b64 = "W3RyYW5zbGF0aW9uXQ=="; // "[translation]"
        let body = format!(r#"{{"retcode":0,"lyric":"{LRC_B64}","trans":"{trans_b64}"}}"#);
        let lrc = parse_lyrics_response(&body).expect("应能解析");
        assert!(lrc.contains("[00:00.02]测试"));
        assert!(lrc.contains("[translation]"));
        // 原文在前，翻译在后
        let pos_original = lrc.find("测试").unwrap();
        let pos_trans = lrc.find("[translation]").unwrap();
        assert!(pos_original < pos_trans);
    }

    #[test]
    fn empty_lyric_returns_empty_string_not_error() {
        // 纯音乐 / 无版权：应当返回空串，让面板显示「暂无歌词」而不是报错
        let body = r#"{"retcode":0,"lyric":""}"#;
        assert_eq!(parse_lyrics_response(body).unwrap(), "");
    }

    #[test]
    fn nonzero_retcode_is_error() {
        let body = r#"{"retcode":-1901,"lyric":""}"#;
        assert!(parse_lyrics_response(body).is_err());
    }

    #[test]
    fn retcode_as_string_is_tolerated() {
        // 实测该接口偶尔把 retcode 序列化成字符串
        let body = format!(r#"{{"retcode":"0","lyric":"{LRC_B64}"}}"#);
        assert!(parse_lyrics_response(&body).is_ok());
    }

    #[test]
    fn non_json_body_is_error_with_readable_message() {
        // 不带 Referer 时接口会返回 HTML 错误页，必须给出可读原因
        let err = parse_lyrics_response("<html>403</html>").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("合法 JSON"), "实际：{msg}");
    }

    #[test]
    fn base64_decoder_handles_padding_and_whitespace() {
        // 带换行的 base64（接口偶尔会折行）
        let decoded = decode_base64("WzAwOjAw\nLjAyXQ==").unwrap();
        assert_eq!(decoded, "[00:00.02]");
    }

    #[test]
    fn base64_decoder_rejects_invalid_char() {
        assert!(decode_base64("****").is_err());
    }

    /// 真实响应裁剪出来的片段（保留结构，去掉无关字段）。
    const SAMPLE: &str = r#"{
      "code": 0,
      "req": {
        "code": 0,
        "data": {
          "body": {
            "song": {
              "list": [
                {
                  "id": 97773,
                  "mid": "0039MnYb0qxYhV",
                  "name": "晴天",
                  "interval": 269,
                  "singer": [{ "id": 4558, "name": "周杰伦" }],
                  "album": { "id": 8220, "mid": "000MkMni19ClKG", "pmid": "000MkMni19ClKG", "name": "叶惠美" }
                },
                {
                  "mid": "004Z8Ihr0JIu5s",
                  "name": "晴天 (深情版)",
                  "interval": 240,
                  "singer": [{ "name": "Lucky小爱" }],
                  "album": { "name": "晴天(深情版)" }
                }
              ]
            }
          }
        }
      }
    }"#;

    #[test]
    fn parses_qq_search_response() {
        let songs = parse_search_response(SAMPLE).expect("应解析成功");
        assert_eq!(songs.len(), 2);

        let first = &songs[0];
        assert_eq!(first.title, "晴天");
        assert_eq!(first.artist, "周杰伦");
        assert_eq!(first.id, "0039MnYb0qxYhV");
        assert_eq!(first.platform, MusicPlatform::Qq);
        assert_eq!(first.duration, 269);
        assert_eq!(first.album.as_deref(), Some("叶惠美"));
        assert!(
            first
                .cover_url
                .as_deref()
                .unwrap_or_default()
                .contains("000MkMni19ClKG"),
            "封面应能由 pmid 拼出"
        );
    }

    #[test]
    fn joins_multiple_singers() {
        let raw = r#"{"code":0,"req":{"code":0,"data":{"body":{"song":{"list":[
            {"mid":"x","name":"千里之外","interval":240,
             "singer":[{"name":"周杰伦"},{"name":"费玉清"}]}
        ]}}}}}"#;
        let songs = parse_search_response(raw).expect("应解析成功");
        assert_eq!(songs[0].artist, "周杰伦 / 费玉清");
    }

    #[test]
    fn reports_api_errors_instead_of_panicking() {
        // 内层 code 非 0：要把 message 带出来
        let raw = r#"{"code":0,"req":{"code":1000,"data":{"msg":"参数错误"}}}"#;
        let err = parse_search_response(raw).expect_err("应报错");
        assert!(format!("{err}").contains("1000"), "错误里应含 code");
        // 不是 JSON
        assert!(parse_search_response("<html>500</html>").is_err());
    }

    #[test]
    fn tolerates_missing_list() {
        let raw = r#"{"code":0,"req":{"code":0,"data":{"body":{}}}}"#;
        assert!(parse_search_response(raw).is_err(), "缺 list 应报结构变化");
    }

    #[test]
    fn builds_play_url_from_vkey_response() {
        // 成功形态：purl 需要拼上 sip 里的 CDN 域名
        let raw = r#"{"code":0,"req_0":{"code":0,"data":{
            "sip":["http://aqqmusic.tc.qq.com/"],
            "midurlinfo":[{"purl":"C400abc.m4a?vkey=XYZ&guid=10000","filename":"C400abc.m4a","result":0}]
        }}}"#;
        let url = parse_play_url_response(raw).expect("应成功");
        assert_eq!(url, "http://aqqmusic.tc.qq.com/C400abc.m4a?vkey=XYZ&guid=10000");
    }

    #[test]
    fn empty_purl_means_unplayable_not_api_change() {
        // 实测周杰伦等版权曲目：purl 为空、result=104003。
        // 这必须报 Unplayable（触发跨平台兜底），而不是 ApiChanged（会当成接口坏了）。
        let raw = r#"{"code":0,"req_0":{"code":0,"data":{
            "sip":["http://aqqmusic.tc.qq.com/"],
            "midurlinfo":[{"purl":"","filename":"C400abc.m4a","result":104003}]
        }}}"#;
        let err = parse_play_url_response(raw).expect_err("应报不可播放");
        assert!(
            matches!(err, MusicError::Unplayable),
            "应为 Unplayable 以便回退，实际：{err:?}"
        );
    }

    #[test]
    fn already_absolute_purl_is_kept() {
        let raw = r#"{"code":0,"req_0":{"code":0,"data":{
            "midurlinfo":[{"purl":"https://cdn.example/x.m4a?vkey=1"}]
        }}}"#;
        assert_eq!(
            parse_play_url_response(raw).unwrap(),
            "https://cdn.example/x.m4a?vkey=1"
        );
    }

    #[test]
    fn vkey_api_error_is_reported() {
        let raw = r#"{"code":0,"req_0":{"code":-1000,"data":{}}}"#;
        assert!(parse_play_url_response(raw).is_err());
        assert!(parse_play_url_response("not json").is_err());
    }

    #[test]
    fn stable_guid_is_consistent() {
        let a = stable_guid();
        let b = stable_guid();
        assert_eq!(a, b, "同一进程内 guid 必须稳定");
        assert!(a >= 10_000_000, "guid 应为 8 位以上数字：{a}");
    }
}


    // ── 歌单解析（阶段 10b）─────────────────────────────────────────────
    //
    // 真实账号实测：QQ「我的歌单」6 个、「我喜欢」529 首。
    // 下面用裁剪过的真实响应片段覆盖解析边界。

    #[test]
    fn parses_qq_user_playlists() {
        let body = r#"{"code":0,"req_0":{"code":0,"data":{"total":2,"v_playlist":[
            {"dirId":201,"dirName":"我喜欢","tid":2068522257,"songNum":529,
             "picUrl":"http://y.gtimg.cn/x.jpg","nick":"_"},
            {"dirId":0,"dirName":"直播歌单","tid":9370940892,"songNum":98}
        ]}}}"#;
        let list = parse_user_playlists(body).expect("应能解析");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "2068522257");
        assert_eq!(list[0].name, "我喜欢");
        assert_eq!(list[0].track_count, 529);
        assert!(list[0].special, "dirId=201 应标记为我喜欢");
        assert_eq!(list[0].creator.as_deref(), Some("_"));
        assert!(!list[1].special, "dirId=0 不是我喜欢");
    }

    #[test]
    fn qq_playlist_error_code_is_reported() {
        // 登录失效 / 模块不通时返回非 0 code，必须报错而不是当成空列表
        let body = r#"{"code":0,"req_0":{"code":500003,"subcode":860100001}}"#;
        assert!(parse_user_playlists(body).is_err());
    }

    #[test]
    fn parses_qq_playlist_tracks_with_playlist_field_names() {
        // 关键回归：歌单详情用 songmid/songname/singer/interval，
        // 与搜索接口的 mid/title 不同。曾经因为按搜索字段解析而全部丢失。
        let body = r#"{"code":0,"cdlist":[{"disstid":"2068522257","songlist":[
            {"songmid":"000c3ohn2KPrcl","songname":"朋友","interval":312,
             "albummid":"001fNHEf1SFEFN","albumname":"朋友",
             "singer":[{"id":96,"mid":"003NThQh3ujqIo","name":"周华健"}]},
            {"songmid":"003aAYrm3GE0Ac","songname":"不得不爱","interval":281,
             "singer":[{"name":"潘玮柏"},{"name":"弦子"}]}
        ]}]}"#;
        let songs = parse_playlist_tracks(body).expect("应能解析");
        assert_eq!(songs.len(), 2, "两首都应被解析出来（不能因字段名不同而丢失）");

        assert_eq!(songs[0].id, "000c3ohn2KPrcl");
        assert_eq!(songs[0].title, "朋友");
        assert_eq!(songs[0].artist, "周华健");
        assert_eq!(songs[0].duration, 312);
        assert_eq!(songs[0].platform, MusicPlatform::Qq);
        // 封面由 albummid 拼出
        assert!(songs[0]
            .cover_url
            .as_deref()
            .unwrap_or_default()
            .contains("001fNHEf1SFEFN"));

        // 多歌手用 ` / ` 连接
        assert_eq!(songs[1].artist, "潘玮柏 / 弦子");
    }

    #[test]
    fn playlist_track_without_songmid_is_skipped_not_panicking() {
        // 有些条目可能是本地文件/无版权占位，没有 songmid，必须安全跳过
        let body = r#"{"code":0,"cdlist":[{"songlist":[
            {"songname":"没有ID的歌","interval":100},
            {"songmid":"ok1","songname":"正常","interval":200,"singer":[{"name":"甲"}]}
        ]}]}"#;
        let songs = parse_playlist_tracks(body).expect("不应因为坏条目报错");
        assert_eq!(songs.len(), 1);
        assert_eq!(songs[0].title, "正常");
    }

    #[test]
    fn playlist_detail_missing_songlist_is_api_change() {
        let body = r#"{"code":0,"cdlist":[]}"#;
        assert!(parse_playlist_tracks(body).is_err());
    }

    #[test]
    fn login_uin_strips_leading_o_prefix() {
        // QQ Cookie 里的 uin 常带 `o` 前缀（如 `o0853886344`）。
        // 只去掉这个 `o`，**保留后续数字**：实测这种值能正常调歌单接口
        // （真实 cookie 就是 `o` 后面还带一个 0）。
        let adapter = QqAdapter::new(Some("uin=o0853886344; qqmusic_key=abc".to_string()));
        assert_eq!(adapter.login_uin().as_deref(), Some("0853886344"));

        let adapter2 = QqAdapter::new(Some("luin=123456; other=1".to_string()));
        assert_eq!(adapter2.login_uin().as_deref(), Some("123456"));

        // 没有 uin / 非数字：返回 None 而不是乱给一个值
        assert!(QqAdapter::new(Some("qqmusic_key=abc".to_string()))
            .login_uin()
            .is_none());
        assert!(QqAdapter::new(None).login_uin().is_none());
    }
