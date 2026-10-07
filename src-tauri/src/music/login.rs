//! 音乐平台登录态的 Cookie 判定与拼接。
//!
//! ## 为什么单独一层
//! 内嵌登录窗口拿到的是一组 `(名字, 值)`。判定「是否登录成功」以及
//! 「拼成请求头用的 Cookie 串」看起来简单，但踩过好几个坑，所以抽成
//! **纯函数**放进库里，方便单测覆盖（`main.rs` 里的代码测不到）：
//!
//! 1. **候选名不是一个**：各平台不同登录方式写的 Cookie 名不一样
//!    （网易云 `MUSIC_U`，QQ 音乐 `qm_keyst` / `qqmusic_key` / `uin`）。
//!    只认一个名字会出现「明明登录成功，程序说没登录」。
//! 2. **值可能带引号**：WebView2 返回的 value 可能形如 `"abc"`，
//!    直接拼进 Cookie 头会变成 `MUSIC_U="abc"`，服务端会认为令牌非法。
//! 3. **匿名令牌不能当登录态**：网易云的 `MUSIC_A` 是匿名访问令牌，
//!    未登录也可能存在，只有在没有 `MUSIC_U` 时才作为兜底。
//! 4. **要过滤无关键**：登录页会写一堆统计/调试 Cookie，全带上会让
//!    请求头过长，某些接口会直接拒绝。

use crate::models::MusicPlatform;

/// 登录态 Cookie 的候选名（按可靠性从高到低）。
pub fn login_cookie_names(platform: MusicPlatform) -> &'static [&'static str] {
    match platform {
        MusicPlatform::Netease => &["MUSIC_U"],
        MusicPlatform::Qq => &["qm_keyst", "qqmusic_key", "qqmusic_uin", "uin"],
    }
}

/// **兜底**令牌名：不足以证明「已登录」，但能证明「会话建立过」。
///
/// 网易云的 `MUSIC_A` 就是这种：它是匿名访问令牌，未登录也会存在，
/// 所以**不能**单独用它判定登录成功——否则界面会显示「已登录」，
/// 而实际取播放地址时仍然只能拿到免费曲目，用户会以为登录功能坏了。
/// 它的价值只在于：万一平台某天改用 `MUSIC_A` 承载登录态，诊断日志能提示我们。
pub fn fallback_cookie_names(platform: MusicPlatform) -> &'static [&'static str] {
    match platform {
        MusicPlatform::Netease => &["MUSIC_A"],
        MusicPlatform::Qq => &[],
    }
}

/// 用于诊断日志的域名提示（只影响日志文案）。
pub fn cookie_host_hint(platform: MusicPlatform) -> &'static str {
    match platform {
        MusicPlatform::Netease => "163.com",
        MusicPlatform::Qq => "qq.com",
    }
}

/// 不应带进请求头的 Cookie 名（登录页的统计/调试/一次性令牌）。
///
/// 注意 `qrsig` 是 QQ 扫码登录的一次性签名，扫码完成后就没用了，
/// 带上它不仅无益，还可能让服务端认为会话状态不一致。
const SKIP_COOKIE_NAMES: &[&str] = &["qrsig", "ptcz", "RK", "pac_uid"];

/// 请求头 Cookie 串的最大长度。
///
/// 为什么需要限制：Windows 凭据库（Credential Manager）单条凭据的密码
/// 上限是 **2560 字节**，而不是 2560 个字符——密码按 **UTF-16** 存储，
/// 所以实际只能放约 1280 个字符，再算上凭据元数据，余量更小。
/// 超了会写入失败并退化成明文文件（实测踩到：登录窗口里累积了 70 个
/// Cookie，拼出来 4142 字节，缩到 1721 字节仍然超限）。
///
/// 另外，非官方接口**不需要**那么多 Cookie：音乐平台自己的网页请求
/// 也只带关键的几个。带全部反而会把「另一个账号/另一个平台」的令牌
/// 一起送过去（登录窗口里常常残留着上一次登录的 `uin`、`wxopenid` 等），
/// 服务端可能因此判定为无效会话。
const MAX_COOKIE_LEN: usize = 1000;

/// 优先保留的 Cookie 名（按重要性排序），其余按出现顺序补足。
///
/// 网易云：`MUSIC_U` 是登录态，`__csrf` 是表单/接口校验，
/// `os`/`appver`/`channel`/`_ntes_nuid` 影响接口返回的字段完整度。
/// QQ 音乐：`qm_keyst`（等于 `qqmusic_key`）是登录态，`uin` 标识账号。
const PRIORITY_COOKIE_NAMES: &[&str] = &[
    "MUSIC_U",
    "__csrf",
    "os",
    "appver",
    "channel",
    "_ntes_nuid",
    "_ntes_nnid",
    "qm_keyst",
    "qqmusic_key",
    "qqmusic_uin",
    "uin",
    "euin",
    "psrf_musickey_createtime",
    "psrf_access_token_expiresAt",
];

/// 判定用的简化 Cookie 表示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedValue {
    /// Cookie 名。
    pub name: String,
    /// Cookie 值（已去引号）。
    pub value: String,
    /// Cookie 域（可能为空）。
    pub domain: String,
}

impl NamedValue {
    /// 构造（自动去掉值两端的引号）。
    pub fn new(name: impl Into<String>, value: impl Into<String>, domain: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into().trim_matches('"').to_string(),
            domain: domain.into(),
        }
    }
}

/// 从一组 Cookie 里挑出**实际生效的登录态 Cookie 名**。
///
/// 只认 [`login_cookie_names`]；兜底令牌（如 `MUSIC_A`）**不参与判定**，
/// 否则会把「未登录」误判为「已登录」。
/// 返回 `None` 表示没找到任何登录态 Cookie。
pub fn detect_login_cookie(platform: MusicPlatform, cookies: &[NamedValue]) -> Option<&'static str> {
    login_cookie_names(platform)
        .iter()
        .copied()
        .find(|name| cookies.iter().any(|c| c.name == *name))
}

/// 与某个平台**无关**的 Cookie 名（另一个平台的令牌）。
///
/// 为什么必须排除：登录窗口用一个共享的 WebView 配置目录，里面会同时残留
/// 网易云和 QQ 音乐的令牌（实测 72 个 Cookie 里两个平台都有）。
/// 难点在于：
///  1. 请求头会变得很长 → 超出系统凭据库上限，退化成明文文件；
///  2. 把「另一个平台」的令牌一起送过去，服务端可能判定会话无效；
///  3. **截断时会把目标平台的登录态挤掉**——按长度累加时，
///     如果另一个平台的关键 Cookie 排在前面，预算就被它吃光了
///     （实测：QQ 登录时 `MUSIC_U` 在前，987 字节全给了网易云，
///     `qm_keyst` 还没轮到就被截断，导致「明明登录成功却拿不到 Cookie」）。
const NETEASE_ONLY_NAMES: &[&str] = &["MUSIC_U", "__csrf", "_ntes_nuid", "_ntes_nnid", "NMTID"];

/// 判断某 Cookie 名是否属于另一个平台（应当排除）。
fn is_foreign_cookie(platform: MusicPlatform, name: &str) -> bool {
    let looks_netease = NETEASE_ONLY_NAMES.contains(&name)
        || name.starts_with("MUSIC_")
        || name.starts_with("_ntes_")
        || name.starts_with("WM_")
        || name.starts_with("Hm_lvt_");

    let looks_qq = name.starts_with("qm_")
        || name.starts_with("psrf_")
        || name.starts_with("pt")
        || name.starts_with("qqmusic")
        || name.starts_with("wx")
        || name == "uin"
        || name == "euin"
        || name == "fqm_sessionid"
        || name == "qlogin_uid"
        || name == "tmeLoginType";

    match platform {
        MusicPlatform::Netease => looks_qq,
        // QQ 不能简单用 `uin` 判定：网易云也可能出现同名键，
        // 因此这里只排除明确的网易云专有键。
        MusicPlatform::Qq => looks_netease && !looks_qq,
    }
}

/// 某 Cookie 是否属于目标平台（判断依据：Cookie 的域）。
///
/// 与 [`is_foreign_cookie`]（按**名字**猜）不同，这里的依据是**域**，
/// 用于「退出登录时清掉该平台在 WebView 里的 Cookie」：
/// 名字判断容易误伤（网易云登录页也会写 `uin`、`RK` 等 QQ 域常见的键），
/// 而域是可靠的——`music.163.com` 的 Cookie 不可能属于 QQ 音乐。
///
/// 传入的 `domain` 应当是 Cookie 实际生效的域（WebView 返回的 `domain`）。
pub fn is_platform_cookie(platform: MusicPlatform, domain: &str) -> bool {
    let domain = domain.trim_start_matches('.').to_ascii_lowercase();
    match platform {
        MusicPlatform::Netease => {
            // 网易云会同时用 163.com 及其子域，以及自己的一些辅助域
            domain.ends_with("163.com")
                || domain.ends_with("126.net")
                || domain.ends_with("127.net")
                || domain.ends_with("music.163.com")
        }
        MusicPlatform::Qq => {
            domain.ends_with("qq.com")
                || domain.ends_with("qqmusic.qq.com")
                || domain.ends_with("y.qq.com")
                || domain.ends_with("tme.com")
        }
    }
}

/// 拼接请求头用的 Cookie 串。
///
/// 找不到登录态 Cookie 时返回 `None`；成功时 `Some((登录态名, Cookie 串))`。
///
/// 处理顺序：过滤无用名 → 排除另一个平台的令牌 → 同名去重（保留第一个）
/// → 优先项前置 → 按长度截断到 [`MAX_COOKIE_LEN`]。
pub fn build_cookie_string(
    platform: MusicPlatform,
    cookies: &[NamedValue],
) -> Option<(&'static str, String)> {
    let login_name = detect_login_cookie(platform, cookies)?;

    // 过滤 + 去重（同名 Cookie 在多个域下会重复出现）
    let mut seen = Vec::new();
    let mut usable: Vec<&NamedValue> = Vec::new();
    for cookie in cookies {
        if cookie.name.is_empty()
            || SKIP_COOKIE_NAMES.contains(&cookie.name.as_str())
            || is_foreign_cookie(platform, &cookie.name)
            || seen.contains(&cookie.name)
        {
            continue;
        }
        seen.push(cookie.name.clone());
        usable.push(cookie);
    }

    // 优先项前置，其余保持原顺序
    usable.sort_by_key(|c| {
        PRIORITY_COOKIE_NAMES
            .iter()
            .position(|name| *name == c.name)
            .unwrap_or(PRIORITY_COOKIE_NAMES.len())
    });

    // 逐条累加，超出上限就停（登录态 Cookie 一定在优先项里，不会丢）
    let mut parts: Vec<String> = Vec::new();
    let mut length = 0usize;
    let usable_len = usable.len();
    for cookie in usable {
        let piece = format!("{}={}", cookie.name, cookie.value);
        if length + piece.len() + 2 > MAX_COOKIE_LEN && !parts.is_empty() {
            break;
        }
        length += piece.len() + 2;
        parts.push(piece);
    }

    let joined = parts.join("; ");
    // 双保险：截断后必须仍含有登录态键值对。
    // 这里留下线索很关键：上层只看到「检测到候选名却拿不到 Cookie」时，
    // 靠这条日志才能判断是「被截断」「值为空」还是「被别的平台挤掉」。
    if joined.is_empty() || !joined.contains(&format!("{login_name}=")) {
        tracing::warn!(
            ?platform,
            login_name,
            kept = parts.len(),
            total = usable_len,
            joined_len = joined.len(),
            "拼接 Cookie 后缺少登录态键值对，已放弃"
        );
        return None;
    }
    Some((login_name, joined))
}

/// 生成失败时给用户看的诊断文案（说明看到了什么、期望什么）。
pub fn describe_failure(platform: MusicPlatform, cookies: &[NamedValue]) -> String {
    if cookies.is_empty() {
        return "未捕获到任何 Cookie：可能是登录窗口被过早关闭，或页面还没加载完成。\
                请重新点击登录，等页面出现登录框后再操作。"
            .to_string();
    }

    let names = cookies
        .iter()
        .map(|c| c.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");

    // 兜底令牌单独提示：它存在但登录态不存在，说明用户「到过登录页但没真正登录完成」
    let fallback_hit = fallback_cookie_names(platform)
        .iter()
        .filter(|name| cookies.iter().any(|c| c.name == **name))
        .copied()
        .collect::<Vec<_>>();

    let extra = if fallback_hit.is_empty() {
        String::new()
    } else {
        format!(
            "（注意：检测到 {}，但这是匿名令牌，不代表已登录）",
            fallback_hit.join(" / ")
        )
    };

    format!(
        "已捕获 {} 个 Cookie（{}），但没有找到 {} 的登录态 Cookie（期望之一：{}）。\
         可能的原因：还没真正登录完成（例如只打开了登录页/只扫了码但没确认），\
         或者该平台改用了新的 Cookie 名。{extra}",
        cookies.len(),
        names,
        platform.display_name(),
        login_cookie_names(platform).join(" / ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nv(pairs: &[(&str, &str)]) -> Vec<NamedValue> {
        pairs
            .iter()
            .map(|(n, v)| NamedValue::new(*n, *v, ".music.163.com"))
            .collect()
    }

    #[test]
    fn netease_music_u_is_the_login_cookie() {
        let cookies = nv(&[("MUSIC_U", "token123"), ("__csrf", "abc")]);
        let (name, joined) = build_cookie_string(MusicPlatform::Netease, &cookies).unwrap();
        assert_eq!(name, "MUSIC_U");
        assert!(joined.contains("MUSIC_U=token123"));
        assert!(joined.contains("__csrf=abc"));
    }

    #[test]
    fn anonymous_token_is_not_mistaken_for_login() {
        // 只有匿名令牌（MUSIC_A）时**不能**判定为已登录
        let only_anon = nv(&[("MUSIC_A", "anon")]);
        assert!(
            detect_login_cookie(MusicPlatform::Netease, &only_anon).is_none(),
            "匿名令牌不足以证明已登录"
        );
        assert!(build_cookie_string(MusicPlatform::Netease, &only_anon).is_none());

        // 两者都在时必须选 MUSIC_U
        let both = nv(&[("MUSIC_A", "anon"), ("MUSIC_U", "real")]);
        assert_eq!(
            detect_login_cookie(MusicPlatform::Netease, &both),
            Some("MUSIC_U")
        );
    }

    #[test]
    fn failure_message_points_out_anonymous_token() {
        // 用户「到过登录页但没登录完成」时，文案要能说清为什么不算登录
        let msg = describe_failure(MusicPlatform::Netease, &nv(&[("MUSIC_A", "anon")]));
        assert!(msg.contains("MUSIC_A"), "应指出看到了匿名令牌");
        assert!(msg.contains("匿名令牌"), "应说明它不代表已登录");
        assert!(msg.contains("MUSIC_U"), "应列出期望的登录态名");
    }

    #[test]
    fn quotes_are_stripped_from_values() {
        let cookies = nv(&[("MUSIC_U", "\"quoted-token\"")]);
        let (_, joined) = build_cookie_string(MusicPlatform::Netease, &cookies).unwrap();
        assert_eq!(joined, "MUSIC_U=quoted-token");
        assert!(!joined.contains('"'), "Cookie 值不应带引号");
    }

    #[test]
    fn qq_accepts_any_of_the_known_names() {
        for key in ["qm_keyst", "qqmusic_key", "qqmusic_uin", "uin"] {
            let cookies = vec![NamedValue::new(key, "v", ".qq.com")];
            let found = detect_login_cookie(MusicPlatform::Qq, &cookies);
            assert_eq!(found, Some(key), "{key} 应被识别为登录态");
        }
    }

    #[test]
    fn qq_prefers_the_primary_key() {
        let cookies = vec![
            NamedValue::new("uin", "12345", ".qq.com"),
            NamedValue::new("qm_keyst", "key", ".qq.com"),
        ];
        assert_eq!(
            detect_login_cookie(MusicPlatform::Qq, &cookies),
            Some("qm_keyst")
        );
    }

    #[test]
    fn noisy_cookies_are_filtered_out() {
        let cookies = nv(&[
            ("MUSIC_U", "t"),
            ("qrsig", "oneshot"),
            ("ptcz", "stat"),
            ("RK", "stat"),
            ("pac_uid", "stat"),
        ]);
        let (_, joined) = build_cookie_string(MusicPlatform::Netease, &cookies).unwrap();
        assert_eq!(joined, "MUSIC_U=t");
    }

    #[test]
    fn duplicate_names_are_deduplicated() {
        // 同名 Cookie 会在多个域下重复出现（登录窗口里很常见）
        let cookies = vec![
            NamedValue::new("MUSIC_U", "first", ".music.163.com"),
            NamedValue::new("MUSIC_U", "second", ".163.com"),
            NamedValue::new("__csrf", "csrf", ".music.163.com"),
        ];
        let (_, joined) = build_cookie_string(MusicPlatform::Netease, &cookies).unwrap();
        assert_eq!(joined.matches("MUSIC_U=").count(), 1, "同名只保留一次");
        assert!(joined.contains("MUSIC_U=first"), "保留最先出现的值");
    }

    #[test]
    fn long_cookie_set_is_truncated_but_keeps_login_token() {
        // 模拟实测场景：登录窗口里累积了 70 个 Cookie，拼出来超过 4KB
        let mut cookies = vec![NamedValue::new("MUSIC_U", "token", ".music.163.com")];
        for i in 0..70 {
            cookies.push(NamedValue::new(
                format!("stat_cookie_{i}"),
                "x".repeat(120),
                ".music.163.com",
            ));
        }
        let (name, joined) = build_cookie_string(MusicPlatform::Netease, &cookies).unwrap();
        assert_eq!(name, "MUSIC_U");
        assert!(
            joined.len() <= MAX_COOKIE_LEN,
            "应截断到上限内，实际 {}",
            joined.len()
        );
        assert!(joined.contains("MUSIC_U=token"), "登录态 Cookie 不能被截掉");
        // Windows 凭据库上限 2560 **字节**，密码按 UTF-16 存储 → 字符数要减半再留余量
        assert!(
            joined.encode_utf16().count() * 2 < 2560,
            "必须能存进系统凭据库（UTF-16 字节数 {}）",
            joined.encode_utf16().count() * 2
        );
    }

    #[test]
    fn priority_cookies_come_first() {
        let cookies = nv(&[
            ("Hm_lvt_abc", "1"),
            ("gdxidpyhxdE", "2"),
            ("__csrf", "csrf-value"),
            ("MUSIC_U", "token"),
        ]);
        let (_, joined) = build_cookie_string(MusicPlatform::Netease, &cookies).unwrap();
        let mut parts = joined.split("; ");
        assert!(parts.next().unwrap().starts_with("MUSIC_U="), "登录态应排第一");
        assert!(parts.next().unwrap().starts_with("__csrf="), "csrf 排第二");
    }

    #[test]
    fn foreign_platform_cookies_are_excluded_from_qq_login() {
        // 实测场景：登录窗口是共享配置目录，里面同时有网易云与 QQ 的令牌。
        // 网易云的大体积 Cookie 会把长度预算吃光，导致 QQ 的登录态被截掉。
        let mut cookies = vec![
            NamedValue::new("MUSIC_U", "x".repeat(900), ".music.163.com"),
            NamedValue::new("__csrf", "csrf", ".music.163.com"),
            NamedValue::new("qm_keyst", "qq-key", ".qq.com"),
            NamedValue::new("qqmusic_uin", "12345", ".qq.com"),
        ];
        // 再加一堆网易云的噪声
        for i in 0..20 {
            cookies.push(NamedValue::new(
                format!("_ntes_nuid_{i}"),
                "y".repeat(50),
                ".163.com",
            ));
        }

        let (login_name, joined) = build_cookie_string(MusicPlatform::Qq, &cookies).unwrap();
        assert_eq!(login_name, "qm_keyst");
        assert!(joined.contains("qm_keyst=qq-key"), "QQ 登录态必须保留");
        assert!(!joined.contains("MUSIC_U"), "不能把网易云令牌发给 QQ 音乐");
        assert!(!joined.contains("__csrf"), "网易云 csrf 也应排除");
    }

    #[test]
    fn platform_domain_matching_is_reliable() {
        // 网易云：163.com 及其子域
        for domain in [".music.163.com", "music.163.com", ".163.com", ".126.net"] {
            assert!(
                is_platform_cookie(MusicPlatform::Netease, domain),
                "{domain} 应属于网易云"
            );
        }
        // QQ 音乐：qq.com 及其子域
        for domain in [".qq.com", "y.qq.com", ".qqmusic.qq.com"] {
            assert!(
                is_platform_cookie(MusicPlatform::Qq, domain),
                "{domain} 应属于 QQ 音乐"
            );
        }
        // 交叉验证：不能互相误判
        assert!(!is_platform_cookie(MusicPlatform::Qq, ".music.163.com"));
        assert!(!is_platform_cookie(MusicPlatform::Netease, ".y.qq.com"));
        // 无关域不应命中
        assert!(!is_platform_cookie(MusicPlatform::Netease, ".bilibili.com"));
        assert!(!is_platform_cookie(MusicPlatform::Qq, "tauri.localhost"));
    }

    #[test]
    fn foreign_platform_cookies_are_excluded_from_netease_login() {
        let cookies = vec![
            NamedValue::new("MUSIC_U", "token", ".music.163.com"),
            NamedValue::new("qm_keyst", "qq-key", ".qq.com"),
            NamedValue::new("psrf_qqunionid", "union", ".qq.com"),
            NamedValue::new("uin", "12345", ".qq.com"),
        ];
        let (_, joined) = build_cookie_string(MusicPlatform::Netease, &cookies).unwrap();
        assert!(joined.contains("MUSIC_U=token"));
        assert!(!joined.contains("qm_keyst"), "不能把 QQ 令牌发给网易云");
        assert!(!joined.contains("psrf_"), "QQ 的 psrf_ 系列也应排除");
    }

    #[test]
    fn empty_cookie_set_yields_none() {
        assert!(build_cookie_string(MusicPlatform::Netease, &[]).is_none());
        assert!(build_cookie_string(MusicPlatform::Qq, &[]).is_none());
    }

    #[test]
    fn failure_message_explains_what_was_seen() {
        let nothing = describe_failure(MusicPlatform::Netease, &[]);
        assert!(nothing.contains("未捕获到任何 Cookie"));

        let some = describe_failure(MusicPlatform::Netease, &nv(&[("__csrf", "x"), ("NMTID", "y")]));
        assert!(some.contains("__csrf"), "应列出实际看到的 Cookie 名");
        assert!(some.contains("MUSIC_U"), "应列出期望的 Cookie 名");
    }

    #[test]
    fn cookie_names_are_non_empty_for_both_platforms() {
        for platform in [MusicPlatform::Netease, MusicPlatform::Qq] {
            assert!(!login_cookie_names(platform).is_empty());
            assert!(!cookie_host_hint(platform).is_empty());
        }
    }
}
