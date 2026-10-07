//! B 站直播开放平台：身份码启动流程（`v2/app/start`）与请求签名。
//!
//! ## 流程
//! 1. `POST https://live-open.biliapi.com/v2/app/start`，请求体 `{"code": "<身份码>", "app_id": <app_id>}`。
//!
//!    ⚠️ host 是 `live-open.biliapi.com`，**不是**文档站 `open-live.bilibili.com`
//!    （后者对 POST 返回 405）。来源：官方 demo `py-demo-new/ws.py` 里
//!    `host="https://live-open.biliapi.com"`。
//! 2. 请求头带 `x-bili-accesskeyid` / `x-bili-content-md5` / `x-bili-timestamp` /
//!    `x-bili-signature-nonce` / `x-bili-signature-method` / `x-bili-signature-version` /
//!    `Authorization`（签名后的十六进制串）。
//! 3. 响应 `data` 里给出 `wss_link`（可能多个，取第一个）、`auth_body`（**原样**作为
//!    WebSocket 首帧发送）与 `heartbeat_interval`（默认 20 秒，服务端要求 30 秒内必须发一次）。
//!
//! ## 签名算法
//! ```text
//! 待签名串（四行，顺序固定，行尾 \n 不能少）:
//!   x-bili-accesskeyid:<access_key_id>
//!   x-bili-content-md5:<body 的 MD5 十六进制小写>
//!   x-bili-timestamp:<秒级 unix 时间戳>
//!   x-bili-signature-nonce:<随机串>
//!
//! signature = HMAC-SHA256(待签名串, access_key_secret) 的十六进制小写
//! ```
//!
//! > 说明：非官方文档之外的实现细节（例如字段名拼写）可能变化，因此本模块把
//! > 「构造请求」与「解析响应」分开，响应解析对缺失字段是容错的。

use std::time::Duration;

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::config::BilibiliConfig;

/// 身份码接口的**线上 host**。
///
/// ⚠️ 这是最容易踩的坑：文档站是 `open-live.bilibili.com`，
/// 但**接口不在那个域名上**。对该域名发 POST 会得到
/// `HTTP 405 Method Not Allowed`（空响应体），看起来像签名/参数错误，
/// 实际上是那个域名只提供文档站，不接受 POST。
///
/// 真实接口域名来自官方 demo（`py-demo-new/ws.py`）：
/// ```python
/// host="https://live-open.biliapi.com")   # 开放平台 (线上环境)
/// postUrl = "%s/v2/app/start" % self.host
/// ```
pub const DEFAULT_API_HOST: &str = "https://live-open.biliapi.com";

/// 身份码启动接口 URL。
pub const START_URL: &str = "https://live-open.biliapi.com/v2/app/start";
/// 心跳接口 URL。
pub const HEARTBEAT_URL: &str = "https://live-open.biliapi.com/v2/app/heartbeat";
/// 结束接口 URL。
pub const END_URL: &str = "https://live-open.biliapi.com/v2/app/end";

/// 签名方法头固定值。
pub const SIGNATURE_METHOD: &str = "HMAC-SHA256";
/// 签名版本头。
///
/// ⚠️ 直播开放平台（身份码/互动玩法）要求 **`1.0`**，
/// 这是官方 demo 里写死的值：
/// ```python
/// "x-bili-signature-version": "1.0",
/// ```
/// 注意与开放平台通用文档区分：那份《接口签名实现标准和状态码》说
/// 「如无单独说明取 2.0」，但**直播这套接口属于"单独说明"**——
/// 传 `2.0` 会直接返回 `{"code":4006,"message":"版本异常"}`（已实测）。
pub const SIGNATURE_VERSION: &str = "1.0";
/// 开放平台通用接口的签名版本（本项目的直播接口不用它）。
pub const SIGNATURE_VERSION_GENERAL: &str = "2.0";

/// 默认心跳间隔（秒）。服务端要求 30 秒内至少一次心跳，这里取 20 秒留余量。
pub const DEFAULT_HEARTBEAT_SECS: u64 = 20;

/// keyring 服务名（Windows 凭据管理器里的「资源」名）。
pub const KEYRING_SERVICE: &str = "bilibili-song-request";
/// keyring 中 access_key_secret 的条目名。
pub const KEYRING_ENTRY_ACCESS_SECRET: &str = "bilibili.access_key_secret";

/// B 站接入相关错误。
#[derive(Debug, Error)]
pub enum BilibiliError {
    /// 本地配置不完整。
    #[error("B 站配置不完整：缺少 {0}")]
    Incomplete(&'static str),
    /// 网络错误。
    #[error("网络请求失败：{0}")]
    Network(String),
    /// 开放平台返回错误码。
    #[error("B 站开放平台返回错误：code={code} message={message}")]
    Api {
        /// 平台错误码。
        code: i64,
        /// 平台错误信息。
        message: String,
    },
    /// 响应结构与预期不符。
    #[error("接口返回结构与预期不符：{0}")]
    ApiChanged(String),
    /// WebSocket 连接失败。
    #[error("WebSocket 连接失败：{0}")]
    WebSocket(String),
}

/// 发给 `v2/app/start` 的请求体。
#[derive(Debug, Clone, Serialize)]
pub struct StartRequestBody {
    /// 身份码。
    pub code: String,
    /// 开放平台应用 ID。
    pub app_id: String,
}

/// B 站开放平台签名所需的配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    /// 开放平台应用 ID。
    pub app_id: String,
    /// 访问密钥 ID。
    pub access_key_id: String,
    /// 访问密钥 Secret（阶段 3 从配置文件读取，阶段 3 末尾可迁移到 keyring）。
    pub access_key_secret: String,
    /// 身份码。
    pub code: String,
}

impl AuthConfig {
    /// 从应用配置构造。
    pub fn from_config(cfg: &BilibiliConfig) -> Self {
        Self {
            app_id: cfg.app_id.trim().to_string(),
            access_key_id: cfg.access_key_id.trim().to_string(),
            access_key_secret: cfg.access_key_secret.trim().to_string(),
            code: cfg.code.trim().to_string(),
        }
    }

    /// 校验是否具备发起连接的最小信息集，缺失项以中文名返回。
    pub fn validate(&self) -> Result<(), BilibiliError> {
        if self.app_id.is_empty() {
            return Err(BilibiliError::Incomplete("app_id"));
        }
        if self.access_key_id.is_empty() {
            return Err(BilibiliError::Incomplete("access_key_id"));
        }
        if self.access_key_secret.is_empty() {
            return Err(BilibiliError::Incomplete("access_key_secret"));
        }
        if self.code.is_empty() {
            return Err(BilibiliError::Incomplete("身份码 code"));
        }
        Ok(())
    }

    /// 日志用的脱敏描述（绝不打印 secret 与身份码全文）。
    pub fn redacted(&self) -> String {
        format!(
            "app_id={} access_key_id={} code={}*** secret={}",
            self.app_id,
            self.access_key_id,
            mask(&self.code),
            if self.access_key_secret.is_empty() {
                "未配置"
            } else {
                "已配置"
            }
        )
    }
}

/// 只保留前 4 个字符，其余用 `*` 代替。
fn mask(value: &str) -> String {
    let head: String = value.chars().take(4).collect();
    if value.chars().count() <= 4 {
        head
    } else {
        format!("{head}****")
    }
}

/// 一次连接所需的会话信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartSession {
    /// WebSocket 接入地址。
    pub wss_url: String,
    /// **原样**作为 WebSocket 首帧发送的鉴权体（通常是 JSON 字符串）。
    pub auth_body: String,
    /// 会话心跳间隔（秒）。
    pub heartbeat_interval: u64,
    /// 直播间 ID（若响应给出）。
    pub room_id: Option<String>,
    /// 长链心跳 ID（若响应给出，新接口用它做 HTTP 心跳）。
    pub conn_id: Option<String>,
    /// 场次 id（新版文档为 `data.game_info.game_id`）。
    ///
    /// 心跳接口 `v2/app/heartbeat` 要传它；老文档里叫 `heartbeat_id` 类字段。
    /// 未开播时可能为空字符串。
    #[serde(default)]
    pub game_id: Option<String>,
}

impl StartSession {
    /// 是否是可直接使用的会话。
    pub fn is_usable(&self) -> bool {
        !self.wss_url.is_empty() && !self.auth_body.is_empty()
    }
}

/// 计算待签名串。
///
/// 构造**待签名字符串**。
///
/// ## 格式（依据官方《接口签名实现标准和状态码》）
/// 抽取带 `x-bili-` 前缀的自定义 header，**按字典序**（`sort.Strings`）拼接，
/// 每行 `键:值`，行间以 `\n` 分隔，**最后一行不带尾随换行**：
///
/// ```text
/// x-bili-accesskeyid:<access_key_id>
/// x-bili-content-md5:<body 的 MD5 十六进制小写>
/// x-bili-signature-method:HMAC-SHA256
/// x-bili-signature-nonce:<随机串>
/// x-bili-signature-version:<2.0>
/// x-bili-timestamp:<秒级 unix 时间戳>
/// ```
///
/// 官方 Go demo 的 `ToSortedString` 就是对这 6 个键排序后拼接，
/// 字典序结果恰好是上面这个顺序（`accesskeyid < content-md5 <
/// signature-method < signature-nonce < signature-version < timestamp`）。
///
/// ⚠️ 早期实现只拼了 4 行（漏掉 method 与 version），会得到错误签名，
/// 服务端返回「签名异常」。这里按官方规范补齐，并用单测锁住格式。
pub fn signature_payload(
    access_key_id: &str,
    content_md5: &str,
    timestamp: i64,
    nonce: &str,
) -> String {
    signature_payload_with_version(access_key_id, content_md5, timestamp, nonce, SIGNATURE_VERSION)
}

/// 指定签名版本构造待签名串（回退旧版时用）。
pub fn signature_payload_with_version(
    access_key_id: &str,
    content_md5: &str,
    timestamp: i64,
    nonce: &str,
    version: &str,
) -> String {
    format!(
        "x-bili-accesskeyid:{access_key_id}\n\
         x-bili-content-md5:{content_md5}\n\
         x-bili-signature-method:{SIGNATURE_METHOD}\n\
         x-bili-signature-nonce:{nonce}\n\
         x-bili-signature-version:{version}\n\
         x-bili-timestamp:{timestamp}"
    )
}

/// 用 HMAC-SHA256 计算签名，返回十六进制小写字符串。
pub fn sign(payload: &str, secret: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .expect("HMAC 接受任意长度密钥，不会失败");
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// 计算请求体的 MD5（十六进制小写），用于 `x-bili-content-md5`。
pub fn content_md5(body: &str) -> String {
    let digest = md5_lite::md5(body.as_bytes());
    hex::encode(digest)
}

/// 生成签名用的随机串。
pub fn signature_nonce() -> String {
    // 32 位十六进制随机串；用 uuid v4 的两段即可满足唯一性要求。
    let a = uuid::Uuid::new_v4().simple().to_string();
    let b = uuid::Uuid::new_v4().simple().to_string();
    format!("{}{}", &a[..16], &b[..16])
}

/// 组装好的请求头（键名保持平台要求的原始大小写）。
pub fn build_headers(auth: &AuthConfig, body: &str) -> Vec<(String, String)> {
    build_headers_with_version(auth, body, SIGNATURE_VERSION)
}

/// 按指定签名版本组装请求头（便于回退旧版规范）。
pub fn build_headers_with_version(
    auth: &AuthConfig,
    body: &str,
    version: &str,
) -> Vec<(String, String)> {
    let timestamp = chrono::Utc::now().timestamp();
    let nonce = signature_nonce();
    let md5 = content_md5(body);
    let payload =
        signature_payload_with_version(&auth.access_key_id, &md5, timestamp, &nonce, version);
    let signature = sign(&payload, &auth.access_key_secret);

    vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Accept".to_string(), "application/json".to_string()),
        ("x-bili-accesskeyid".to_string(), auth.access_key_id.clone()),
        ("x-bili-content-md5".to_string(), md5),
        ("x-bili-timestamp".to_string(), timestamp.to_string()),
        ("x-bili-signature-nonce".to_string(), nonce),
        (
            "x-bili-signature-method".to_string(),
            SIGNATURE_METHOD.to_string(),
        ),
        (
            "x-bili-signature-version".to_string(),
            version.to_string(),
        ),
        ("Authorization".to_string(), signature),
    ]
}

/// 调用 `v2/app/start` 获取会话。
///
/// `client` 由调用方复用（保持连接池）；超时 10 秒。
pub async fn start_session(
    client: &reqwest::Client,
    auth: &AuthConfig,
) -> Result<StartSession, BilibiliError> {
    auth.validate()?;

    let body = serde_json::to_string(&StartRequestBody {
        code: auth.code.clone(),
        app_id: auth.app_id.clone(),
    })
    .map_err(|e| BilibiliError::ApiChanged(format!("序列化请求体失败：{e}")))?;

    let mut request = client
        .post(START_URL)
        .timeout(Duration::from_secs(10))
        .body(body.clone());
    for (key, value) in build_headers(auth, &body) {
        request = request.header(key, value);
    }

    debug!(auth = %auth.redacted(), "请求 B 站身份码启动接口");
    let response = request
        .send()
        .await
        .map_err(|e| BilibiliError::Network(e.to_string()))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| BilibiliError::Network(e.to_string()))?;

    parse_start_response(status.as_u16(), &text)
}

/// 解析 `v2/app/start` 的响应。
///
/// 拆成独立函数便于用固定报文做单元测试（不需要真的连网）。
pub fn parse_start_response(status: u16, text: &str) -> Result<StartSession, BilibiliError> {
    let trimmed = text.trim();

    // 空响应体：通常是网关/WAF 直接拒绝（例如 HTTP 405）。
    // 这时报「JSON 解析失败」会严重误导排查方向（会让人以为是接口改了字段），
    // 因此单独给出一条说明「请求根本没到业务接口」的错误。
    if trimmed.is_empty() {
        return Err(BilibiliError::ApiChanged(format!(
            "接口返回空响应（HTTP {status}）。这通常表示请求**没有到达业务接口**，\
             而是被网关/防火墙/网络策略挡掉了（常见于 HTTP 405 Method Not Allowed）。\
             请检查：① 本机能否正常访问 bilibili.com；② 是否开了代理/VPN 需要放行；\
             ③ 换个网络（如手机热点）重试。当前网络对 B 站的 POST 请求可能被拦截。"
        )));
    }

    let value: Value = serde_json::from_str(trimmed).map_err(|e| {
        BilibiliError::ApiChanged(format!(
            "响应不是合法 JSON（HTTP {status}）：{e}；报文开头：{}",
            truncate(trimmed, 200)
        ))
    })?;

    // 平台约定 code=0 表示成功。
    let code = value.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    let message = value
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();

    if code != 0 {
        if message.is_empty() && status != 200 {
            return Err(BilibiliError::Api {
                code: status as i64,
                message: format!("HTTP {status}"),
            });
        }
        return Err(BilibiliError::Api { code, message });
    }

    let data = value
        .get("data")
        .ok_or_else(|| BilibiliError::ApiChanged("响应缺少 data 字段".to_string()))?;

    // 长连信息的位置有两种形态，都要支持：
    //  ① 官方文档结构：`data.websocket_info.{wss_link, auth_body}`
    //     （`data.game_info.game_id` 是场次 id，心跳要用）
    //  ② 早期/扁平结构：`data.{wss_link, auth_body}`
    // 实测文档已更新为 ①，若只按 ② 解析会报「响应缺少 wss_link 或 auth_body」。
    let ws_info = data.get("websocket_info").unwrap_or(data);
    let game_info = data.get("game_info");

    // wss_link 既可能是数组也可能是单个字符串。
    let wss_url = match ws_info.get("wss_link") {
        Some(Value::Array(list)) => list
            .iter()
            .filter_map(|v| v.as_str())
            .next()
            .unwrap_or_default()
            .to_string(),
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    };

    let auth_body = match ws_info.get("auth_body") {
        Some(Value::String(s)) => s.clone(),
        // 少数情况下平台直接返回对象，这里再序列化回字符串。
        Some(other @ Value::Object(_)) => other.to_string(),
        _ => String::new(),
    };

    let heartbeat_interval = ws_info
        .get("heartbeat_interval")
        .or_else(|| data.get("heartbeat_interval"))
        .and_then(|v| v.as_u64())
        .filter(|v| *v > 0)
        // 平台要求 30 秒内必须心跳，配置值过大时收敛到 25 秒。
        .map(|v| v.min(25))
        .unwrap_or(DEFAULT_HEARTBEAT_SECS);

    // 场次 id：心跳接口要用它（新版文档放在 data.game_info.game_id）
    let game_id = ws_info
        .get("game_id")
        .or_else(|| game_info.and_then(|g| g.get("game_id")))
        .or_else(|| data.get("game_id"))
        .map(|v| match v {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        })
        .filter(|s| !s.is_empty());

    let room_id = data
        .get("room_id")
        .or_else(|| data.get("anchor_info").and_then(|a| a.get("room_id")))
        .map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .filter(|s| !s.is_empty() && s != "null");

    let conn_id = data
        .get("conn_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let session = StartSession {
        wss_url,
        auth_body,
        heartbeat_interval,
        room_id,
        conn_id,
        game_id,
    };

    if !session.is_usable() {
        return Err(BilibiliError::ApiChanged(format!(
            "响应里没有长连信息（既没有 data.websocket_info.wss_link/auth_body，\
             也没有 data.wss_link/auth_body）：{}",
            truncate(text, 300)
        )));
    }

    info!(
        url = %session.wss_url,
        interval = session.heartbeat_interval,
        "已获取 B 站弹幕长链会话"
    );
    Ok(session)
}

/// 截断过长文本用于日志。
pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max).collect();
    format!("{cut}…")
}

/// 记录一次启动失败（供上层写状态用）。
pub fn describe_start_error(err: &BilibiliError) -> String {
    warn!(error = %err, "获取弹幕长链会话失败");
    err.to_string()
}

/// 极简 MD5 实现。
///
/// 为什么不加 `md5` crate：`x-bili-content-md5` 只要求标准 MD5，
/// 这里只需要 80 行纯 Rust 实现，避免为一个头字段引入额外依赖树。
/// 实现按 RFC 1321，已用测试向量校验（见 tests）。
pub mod md5_lite {
    /// 计算 MD5 摘要（16 字节）。
    pub fn md5(input: &[u8]) -> [u8; 16] {
        const S: [u32; 64] = [
            7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20,
            5, 9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23,
            6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
        ];
        const K: [u32; 64] = [
            0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
            0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
            0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
            0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
            0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
            0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
            0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
            0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
            0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
            0xeb86d391,
        ];

        let mut msg = input.to_vec();
        let bit_len = (input.len() as u64).wrapping_mul(8);
        msg.push(0x80);
        while msg.len() % 64 != 56 {
            msg.push(0);
        }
        msg.extend_from_slice(&bit_len.to_le_bytes());

        let mut a0: u32 = 0x67452301;
        let mut b0: u32 = 0xefcdab89;
        let mut c0: u32 = 0x98badcfe;
        let mut d0: u32 = 0x10325476;

        for chunk in msg.chunks_exact(64) {
            let mut m = [0u32; 16];
            for (i, word) in m.iter_mut().enumerate() {
                let mut bytes = [0u8; 4];
                bytes.copy_from_slice(&chunk[i * 4..i * 4 + 4]);
                *word = u32::from_le_bytes(bytes);
            }

            let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
            for i in 0..64 {
                let (f, g) = match i {
                    0..=15 => ((b & c) | (!b & d), i),
                    16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                    32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                    _ => (c ^ (b | !d), (7 * i) % 16),
                };
                let tmp = d;
                d = c;
                c = b;
                let sum = a
                    .wrapping_add(f)
                    .wrapping_add(K[i])
                    .wrapping_add(m[g]);
                b = b.wrapping_add(sum.rotate_left(S[i]));
                a = tmp;
            }

            a0 = a0.wrapping_add(a);
            b0 = b0.wrapping_add(b);
            c0 = c0.wrapping_add(c);
            d0 = d0.wrapping_add(d);
        }

        let mut out = [0u8; 16];
        out[0..4].copy_from_slice(&a0.to_le_bytes());
        out[4..8].copy_from_slice(&b0.to_le_bytes());
        out[8..12].copy_from_slice(&c0.to_le_bytes());
        out[12..16].copy_from_slice(&d0.to_le_bytes());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_matches_rfc1321_vectors() {
        assert_eq!(content_md5(""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(content_md5("abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            content_md5("message digest"),
            "f96b697d7cb7938d525a2f31aaf161d0"
        );
        assert_eq!(
            content_md5("12345678901234567890123456789012345678901234567890123456789012345678901234567890"),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
    }

    #[test]
    fn signature_payload_matches_official_demo() {
        // 与官方 demo（py-demo-new/ws.py）的 sign() 一致：
        // 6 个 x-bili- 头按**字典序**拼接，`\n` 分隔，末行不带换行。
        // 值取自 demo：signature-version 固定 "1.0"。
        let payload = signature_payload("AKID", "MD5SUM", 1700000000, "NONCE");
        assert_eq!(
            payload,
            "x-bili-accesskeyid:AKID\n\
             x-bili-content-md5:MD5SUM\n\
             x-bili-signature-method:HMAC-SHA256\n\
             x-bili-signature-nonce:NONCE\n\
             x-bili-signature-version:1.0\n\
             x-bili-timestamp:1700000000"
        );
        // 不能有尾随换行（demo 里是 rstrio("\n")）
        assert!(!payload.ends_with('\n'));
        // 必须是 6 行
        assert_eq!(payload.lines().count(), 6);
    }

    #[test]
    fn signature_payload_lines_are_lexicographically_sorted() {
        // 官方 Go demo 用 sort.Strings 排序，这里验证我们的顺序与之一致
        let payload = signature_payload("k", "m", 1, "n");
        let keys: Vec<&str> = payload
            .lines()
            .map(|l| l.split(':').next().unwrap())
            .collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(keys, sorted, "待签名串必须按字典序排列");
    }

    #[test]
    fn legacy_general_version_can_be_selected() {
        // 开放平台通用接口用 2.0；保留该能力以便与直播接口区分
        let payload =
            signature_payload_with_version("AKID", "MD5SUM", 1700000000, "NONCE", "2.0");
        assert!(payload.contains("x-bili-signature-version:2.0"));
        assert_eq!(payload.lines().count(), 6);
    }

    #[test]
    fn headers_use_signature_version_1_0() {
        // 直播开放平台要求 1.0（官方 demo 写死）。传 2.0 会返回 code=4006 版本异常。
        let auth = AuthConfig {
            app_id: "1".into(),
            access_key_id: "AKID".into(),
            access_key_secret: "SECRET".into(),
            code: "CODE".into(),
        };
        let headers = build_headers(&auth, "{}");
        let version = headers
            .iter()
            .find(|(k, _)| k == "x-bili-signature-version")
            .map(|(_, v)| v.as_str());
        assert_eq!(version, Some("1.0"), "直播接口必须用 1.0");
    }

    #[test]
    fn start_url_uses_the_real_api_host() {
        // 回归测试：曾经误用文档站域名 open-live.bilibili.com，
        // 导致所有请求都被 405 拒绝（请求根本没到业务接口）。
        assert!(
            START_URL.starts_with("https://live-open.biliapi.com"),
            "接口域名必须是 live-open.biliapi.com，实际：{START_URL}"
        );
        assert!(
            !START_URL.contains("open-live.bilibili.com"),
            "不能使用文档站域名作为接口地址"
        );
    }

    #[test]
    fn parses_nested_websocket_info_response() {
        // 官方文档结构：长连信息在 data.websocket_info 下，场次 id 在 data.game_info 下
        let text = r#"{
            "code": 0,
            "message": "ok",
            "data": {
                "game_info": { "game_id": "game-abc" },
                "websocket_info": {
                    "auth_body": "{\"key\":\"token\"}",
                    "wss_link": ["wss://broadcastlv.chat.bilibili.com:443/sub"]
                },
                "anchor_info": { "room_id": 12345 }
            }
        }"#;
        let session = parse_start_response(200, text).expect("官方结构应能解析");
        assert_eq!(session.wss_url, "wss://broadcastlv.chat.bilibili.com:443/sub");
        assert_eq!(session.auth_body, r#"{"key":"token"}"#);
        assert_eq!(session.game_id.as_deref(), Some("game-abc"));
        assert_eq!(session.room_id.as_deref(), Some("12345"));
    }

    #[test]
    fn parses_flat_response_too() {
        // 兼容早期扁平结构（长连字段直接在 data 下）
        let text = r#"{
            "code": 0,
            "data": {
                "wss_link": "wss://flat/ws",
                "auth_body": "{\"k\":1}",
                "heartbeat_interval": 15
            }
        }"#;
        let session = parse_start_response(200, text).expect("扁平结构也要支持");
        assert_eq!(session.wss_url, "wss://flat/ws");
        assert_eq!(session.heartbeat_interval, 15);
    }

    #[test]
    fn hmac_signature_is_hex_lowercase_and_stable() {
        // 与 `printf 'x' | openssl dgst -sha256 -hmac 'k'` 的等价性由长度与确定性保证：
        // 这里校验格式、确定性与密钥敏感性。
        let a = sign("payload", "secret");
        let b = sign("payload", "secret");
        let c = sign("payload", "secret2");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase()));
    }

    #[test]
    fn headers_contain_all_required_fields() {
        let auth = AuthConfig {
            app_id: "123".into(),
            access_key_id: "AKID".into(),
            access_key_secret: "SECRET".into(),
            code: "abcd-efgh-ijkl-mnop".into(),
        };
        let headers = build_headers(&auth, r#"{"code":"x","app_id":"123"}"#);
        let names: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
        for required in [
            "x-bili-accesskeyid",
            "x-bili-content-md5",
            "x-bili-timestamp",
            "x-bili-signature-nonce",
            "x-bili-signature-method",
            "x-bili-signature-version",
            "Authorization",
        ] {
            assert!(names.contains(&required), "缺少请求头 {required}");
        }
        // 签名必须与同样输入重算一致
        let md5 = &headers.iter().find(|(k, _)| k == "x-bili-content-md5").unwrap().1;
        let ts: i64 = headers
            .iter()
            .find(|(k, _)| k == "x-bili-timestamp")
            .unwrap()
            .1
            .parse()
            .unwrap();
        let nonce = &headers
            .iter()
            .find(|(k, _)| k == "x-bili-signature-nonce")
            .unwrap()
            .1;
        let expected = sign(&signature_payload("AKID", md5, ts, nonce), "SECRET");
        assert_eq!(
            headers.iter().find(|(k, _)| k == "Authorization").unwrap().1,
            expected
        );
    }

    #[test]
    fn parses_successful_start_response() {
        let raw = r#"{
            "code": 0,
            "message": "ok",
            "data": {
                "wss_link": ["wss://broadcastlv.net/x", "wss://backup.net/y"],
                "auth_body": "{\"key\":\"token\"}",
                "heartbeat_interval": 30,
                "room_id": 12345,
                "conn_id": "conn-abc"
            }
        }"#;
        let session = parse_start_response(200, raw).expect("应解析成功");
        assert_eq!(session.wss_url, "wss://broadcastlv.net/x");
        assert_eq!(session.auth_body, r#"{"key":"token"}"#);
        // 平台要求 30 秒内心跳，配置 30 会被收敛到 25 以内
        assert!(session.heartbeat_interval <= 25);
        assert_eq!(session.room_id.as_deref(), Some("12345"));
        assert_eq!(session.conn_id.as_deref(), Some("conn-abc"));
        assert!(session.is_usable());
    }

    #[test]
    fn accepts_string_wss_link() {
        let raw = r#"{"code":0,"data":{"wss_link":"wss://only.net/a","auth_body":"{}"}}"#;
        let session = parse_start_response(200, raw).expect("应解析成功");
        assert_eq!(session.wss_url, "wss://only.net/a");
        assert_eq!(session.heartbeat_interval, DEFAULT_HEARTBEAT_SECS);
    }

    #[test]
    fn surfaces_platform_error_code() {
        let raw = r#"{"code":10001,"message":"invalid code"}"#;
        match parse_start_response(200, raw) {
            Err(BilibiliError::Api { code, message }) => {
                assert_eq!(code, 10001);
                assert_eq!(message, "invalid code");
            }
            other => panic!("应返回 Api 错误，实际：{other:?}"),
        }
    }

    #[test]
    fn reports_missing_fields_as_api_changed() {
        let raw = r#"{"code":0,"data":{"room_id":"1"}}"#;
        assert!(matches!(
            parse_start_response(200, raw),
            Err(BilibiliError::ApiChanged(_))
        ));
        assert!(matches!(
            parse_start_response(500, "not json"),
            Err(BilibiliError::ApiChanged(_))
        ));
    }

    #[test]
    fn validate_reports_first_missing_field() {
        let mut auth = AuthConfig {
            app_id: String::new(),
            access_key_id: "a".into(),
            access_key_secret: "b".into(),
            code: "c".into(),
        };
        assert!(matches!(
            auth.validate(),
            Err(BilibiliError::Incomplete("app_id"))
        ));
        auth.app_id = "1".into();
        auth.code.clear();
        assert!(matches!(
            auth.validate(),
            Err(BilibiliError::Incomplete(_))
        ));
        auth.code = "code".into();
        assert!(auth.validate().is_ok());
    }

    #[test]
    fn redacted_hides_secrets() {
        let auth = AuthConfig {
            app_id: "app".into(),
            access_key_id: "AKID".into(),
            access_key_secret: "SUPER-SECRET-VALUE".into(),
            code: "1234-5678-9012-3456".into(),
        };
        let text = auth.redacted();
        assert!(!text.contains("SUPER-SECRET-VALUE"));
        assert!(!text.contains("5678"));
        assert!(text.contains("1234"));
    }
}
