//! 配置文件读写与默认值。
//!
//! 落盘位置：`%APPDATA%/bilibili-song-request/config.json`
//! （由 `directories::BaseDirs` 解析；非 Windows 平台落到对应规范目录）。
//!
//! 注意用 `BaseDirs` 而不是 `ProjectDirs`：后者会把
//! `com/bilibili-song-request/bilibili-song-request` 拼成嵌套目录，
//! 得到 `%APPDATA%\bilibili-song-request\bilibili-song-request\config\`，
//! 与本项目的目录约定不符。
//!
//! 安全约定：**敏感凭据不写这里**。B 站 access_key_secret 与音乐平台 Cookie
//! 走系统凭据库（keyring），详见 `bilibili::auth` 与 `music` 模块的阶段 3/5 实现。

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// 应用目录名（配置文件与日志所在文件夹）。
pub const APP_DIR_NAME: &str = "bilibili-song-request";

/// 覆盖配置目录的环境变量（便携模式）。
pub const ENV_CONFIG_DIR: &str = "BSR_CONFIG_DIR";
/// 配置文件名。
pub const CONFIG_FILE_NAME: &str = "config.json";

/// 内嵌 HTTP 服务器配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// 监听地址。**只允许回环地址**，不要暴露到公网（见需求文档「风险与注意事项」）。
    pub host: String,
    /// 监听端口，OBS 浏览器源地址为 `http://127.0.0.1:<port>/panel`。
    pub port: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 17777,
        }
    }
}

/// B 站直播开放平台（身份码模式）配置。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BilibiliConfig {
    /// 开放平台应用 ID。
    pub app_id: String,
    /// 访问密钥 ID。
    pub access_key_id: String,
    /// 访问密钥 Secret。
    ///
    /// 安全提示：该字段会随配置文件明文落盘（阶段 1 简化处理）。
    /// 阶段 3 会改为「优先从 keyring 读取，配置文件仅留空占位」。
    pub access_key_secret: String,
    /// 身份码，形如 `xxxx-xxxx-xxxx-xxxx`。B 站同一身份码最多 5 个连接，
    /// 因此本程序内部只建立一个连接，面板通过本地 WS 复用数据。
    pub code: String,
    /// 启动后是否自动连接（阶段 3 生效）。
    pub auto_connect: bool,
}

/// 点歌规则。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RequestRules {
    /// 指令正则，默认 `^点歌\s+(.+?)(?:\s+(.+))?$`。
    pub command_regex: String,
    /// 同一用户冷却秒数（0 = 不限制）。
    pub cooldown_secs: u64,
    /// **弹幕点歌**的等待上限（0 = 不限）。
    ///
    /// 只统计弹幕来源（[`crate::models::QueuePriority::Danmaku`]）的待播曲目；
    /// 主播自己通过点歌机添加的不受这个上限约束。
    pub max_queue: usize,
    /// 每播完一首**主播点歌**，给弹幕补充多少个可点名额。
    ///
    /// 这样「队列满 7 首弹幕 + 2 首主播，队首播完后弹幕能再点一首」成立；
    /// 而新点的弹幕仍排在主播曲目**之后**（由优先级插队规则保证）。
    pub host_extra_per_play: usize,
    /// 是否允许同一首歌重复点。
    pub allow_duplicate: bool,
    /// 预留：最低粉丝牌等级（0 = 不限）。
    pub min_fans_medal_level: u32,
    /// 预留：最低用户等级（0 = 不限）。
    pub min_user_level: u32,
    /// 点歌优先级策略（默认平台搜不到精确原唱时怎么选）。
    pub pick_policy: PickPolicy,
    /// 搜索用哪个平台（自动 / 只用 QQ / 只用网易云）。
    pub search_platform: SearchPlatform,
    /// 有人点歌时，正在播的空闲歌曲要不要立刻让位（阶段 10a）。
    pub idle_switch_policy: crate::models::IdleSwitchPolicy,
}

/// 搜索平台策略。
///
/// ## 为什么是一个三选一，而不是「默认平台 + 是否跨平台」两个字段
/// 两个字段能组合出 4 种状态，其中「默认 QQ 但不跨平台」和
/// 「默认 QQ 且跨平台」对用户来说只差一个复选框，很容易配出自相矛盾的组合。
/// 三选一把意图直接写清楚，界面也只需要一个下拉框。
///
/// 用户的实际诉求（原话）："我无法主动切换到只在网易云搜索或者只在 QQ 音乐搜索"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SearchPlatform {
    /// 自动：先搜 QQ，没有**精确命中**（歌名+歌手都对上）时再搜网易云，
    /// 两边候选放一起取综合最高分。
    ///
    /// 这是默认值：既用得上 QQ 更全的原唱曲库，
    /// 又不会在 QQ 没版权时干等。
    #[default]
    Auto,
    /// 只用 QQ 音乐：搜不到就报搜不到，**绝不换到网易云**。
    ///
    /// 适合「开了 QQ 会员、只想听 QQ 原唱」。
    Qq,
    /// 只用网易云：搜不到就报搜不到，**绝不换到 QQ**。
    ///
    /// 适合「QQ 那边只有 95 秒试听，宁可听网易云的完整版」。
    Netease,
}

impl SearchPlatform {
    /// 用户可读的名称。
    pub fn label(&self) -> &'static str {
        match self {
            SearchPlatform::Auto => "自动（QQ 优先，必要时用网易云）",
            SearchPlatform::Qq => "只用 QQ 音乐",
            SearchPlatform::Netease => "只用网易云音乐",
        }
    }

    /// 首选平台。
    pub fn primary(&self) -> Option<crate::models::MusicPlatform> {
        match self {
            SearchPlatform::Auto | SearchPlatform::Qq => Some(crate::models::MusicPlatform::Qq),
            SearchPlatform::Netease => Some(crate::models::MusicPlatform::Netease),
        }
    }

    /// 是否允许回退到另一个平台。
    pub fn allows_fallback(&self) -> bool {
        matches!(self, SearchPlatform::Auto)
    }
}

/// 点歌的「原唱 vs 完整版」优先级策略。
///
/// ## 为什么需要这个开关
/// 各平台的版权策略不同，实测会出现这种两难：
///  - QQ 音乐（**非会员**）遇到版权曲目只给 **95 秒试听**；
///  - 网易云上该曲的**原唱没有任何可播条目**，只有翻唱/上传者版本（但是完整时长）。
///
/// 于是「点周杰伦《青花瓷》」时只能二选一：
///  - [`PickPolicy::PreferArtist`]：保住原唱（周杰伦），但只有 95 秒；
///  - [`PickPolicy::PreferFullLength`]：保住完整时长（约 4 分钟），但歌手是翻唱。
///
/// 哪种更好取决于主播偏好与账号是否开了会员，所以做成设置项而不是写死。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PickPolicy {
    /// 优先保证**播放完整时长**：宁可换平台的翻唱，也不放 95 秒试听。
    ///
    /// 这是默认值：直播间里「歌突然断在 95 秒」的体验通常比「听到翻唱」更糟。
    #[default]
    PreferFullLength,
    /// 优先保证**原唱**：宁可只放试听片段，也不换平台听翻唱。
    PreferArtist,
}

impl Default for RequestRules {
    fn default() -> Self {
        Self {
            command_regex: r"^点歌\s+(.+?)(?:\s+(.+))?$".to_string(),
            cooldown_secs: 30,
            // 弹幕最多同时在等 7 首
            max_queue: 7,
            // 主播每播完一首，给弹幕补 2 个名额
            host_extra_per_play: 2,
            allow_duplicate: false,
            min_fans_medal_level: 0,
            min_user_level: 0,
            pick_policy: PickPolicy::default(),
            search_platform: SearchPlatform::default(),
            idle_switch_policy: crate::models::IdleSwitchPolicy::default(),
        }
    }
}

/// mpv 播放器配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PlayerConfig {
    /// mpv 可执行文件路径；留空表示自动查找（见 `binresolver`）。
    pub mpv_binary: String,
    /// IPC 管道名（Windows 形如 `\\.\pipe\mpvpipe`）。
    pub pipe_name: String,
    /// 初始音量 0-100。
    pub volume: u8,
    /// 启动时自动拉起 mpv。
    pub auto_start: bool,
    /// 附加 mpv 命令行参数。
    pub extra_args: Vec<String>,
}

impl Default for PlayerConfig {
    fn default() -> Self {
        Self {
            mpv_binary: String::new(),
            pipe_name: crate::player::mpv::DEFAULT_PIPE_NAME.to_string(),
            volume: 80,
            auto_start: true,
            extra_args: Vec::new(),
        }
    }
}

/// 面板默认样式，可被 `/panel?...` 的 URL 参数覆盖。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PanelStyleConfig {
    pub theme: String,
    pub bg: String,
    /// 主色：歌曲名、队列高亮、歌词当前行、弹幕用户名等**强调**文字与装饰。
    ///
    /// 早期它同时控制进度条，导致「只想改进度条颜色，结果整个面板都变了」，
    /// 于是进度条被拆到 [`Self::bar_color`]。
    pub color: String,
    /// 进度条填充色。空字符串 = 跟随 `color`（默认），便于只改一处就整体协调。
    pub bar_color: String,
    /// 字体颜色；`None` = 跟随主题（阶段 9）。
    pub fg: Option<String>,
    /// 卡片/列表底色；默认 `transparent` = 只要文字不要底色（阶段 9）。
    pub surface: String,
    /// 进度条轨道颜色；默认 `transparent`（阶段 9）。
    pub track: String,
    /// 背景图 URL；只允许 `http(s)://` 或站内路径 `/bg/xxx`（阶段 9）。
    pub bg_image: Option<String>,
    pub font_size: u32,
    pub scale: f64,
    pub limit: usize,
    pub show_lyrics: bool,
    pub layout: String,
}

impl Default for PanelStyleConfig {
    fn default() -> Self {
        Self {
            theme: "dark".to_string(),
            bg: "transparent".to_string(),
            color: "#ff6fa5".to_string(),
            // 空 = 跟随主色：OBS 里只调一个颜色就能整体协调
            bar_color: String::new(),
            fg: None,
            // 默认全透明：用户明确要求「只要文字与进度条颜色」
            surface: "transparent".to_string(),
            track: "transparent".to_string(),
            bg_image: None,
            font_size: 16,
            scale: 1.0,
            limit: 8,
            show_lyrics: true,
            layout: "list".to_string(),
        }
    }
}

/// 应用总体配置。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub bilibili: BilibiliConfig,
    pub rules: RequestRules,
    pub panel: PanelStyleConfig,
    pub player: PlayerConfig,
    /// 点歌黑名单：命中的歌**弹幕直接点不了**，主播可无视。
    #[serde(default)]
    pub blacklist: Vec<crate::queue::blacklist::BlacklistEntry>,
    /// 点歌队列的播放模式（阶段 10d：持久化）。
    ///
    /// ## 为什么放进 config 而不是只留在队列快照里
    /// 用户反馈「每次重开软件播放模式都被重置」——因为 `POST /api/player/mode`
    /// 只改了内存状态、从不落盘；而队列快照只在这个值**恰好伴随队列变化**时
    /// 才被顺带写入。放进 config 后，改模式就等于改配置，走同一套落盘路径。
    #[serde(default)]
    pub play_mode: crate::models::PlayMode,
}

impl Config {
    /// 应用配置目录，必要时创建。
    ///
    /// Windows → `%APPDATA%\bilibili-song-request`；
    /// macOS → `~/Library/Application Support/bilibili-song-request`；
    /// Linux → `$XDG_CONFIG_HOME/bilibili-song-request`。
    ///
    /// 配置目录。
    ///
    /// 优先级：
    ///  1. 环境变量 `BSR_CONFIG_DIR`（便携模式 / 测试隔离）——
    ///     把配置、日志、队列、密钥文件都放到指定目录，
    ///     实现「U 盘里放一个文件夹就能跑」；
    ///  2. 平台规范目录：
    ///     Windows → `%APPDATA%\bilibili-song-request`；
    ///     macOS → `~/Library/Application Support/bilibili-song-request`；
    ///     Linux → `$XDG_CONFIG_HOME/bilibili-song-request`。
    ///
    /// 需要在**代码里**写到别处时（例如集成测试），请用 [`Config::save_to`] 或
    /// [`crate::server::ServerCtx::with_config_path`]，不要改这里的返回值。
    ///
    /// 关于线程安全：该函数在进程启动早期被调用一次（`Config::load`），
    /// 之后路径会随 `Config` 一起传递，因此读环境变量不会造成竞态。
    pub fn config_dir() -> PathBuf {
        let dir = match std::env::var(ENV_CONFIG_DIR) {
            Ok(value) if !value.trim().is_empty() => PathBuf::from(value.trim()),
            _ => directories::BaseDirs::new()
                .map(|base| base.config_dir().join(APP_DIR_NAME))
                .unwrap_or_else(|| PathBuf::from(".")),
        };
        let _ = fs::create_dir_all(&dir);
        dir
    }

    /// 配置文件完整路径。
    pub fn config_path() -> PathBuf {
        Self::config_dir().join(CONFIG_FILE_NAME)
    }

    /// 从默认位置读取；文件不存在或损坏时回落到默认配置并落盘一份。
    pub fn load() -> Self {
        let path = Self::config_path();
        match Self::load_from(&path) {
            Ok(mut cfg) => {
                info!(path = %path.display(), "已加载配置");
                if cfg.migrate() {
                    // 迁移结果要落盘，否则每次启动都会重新迁移一次
                    if let Err(err) = cfg.save_to(&path) {
                        warn!(error = %err, "迁移后的配置写入失败");
                    }
                }
                cfg
            }
            Err(err) => {
                warn!(path = %path.display(), error = %err, "读取配置失败，使用默认配置");
                let cfg = Self::default();
                if let Err(save_err) = cfg.save_to(&path) {
                    warn!(error = %save_err, "写入默认配置失败");
                }
                cfg
            }
        }
    }

    /// 把旧版本配置迁移到当前语义，返回是否发生了改动。
    ///
    /// `serde(default)` 只能补**缺失字段**，补不了「字段存在但默认值已经变了」
    /// 的情况——这正是 `max_queue` 的问题：它的默认值从 20 改成 7
    /// （弹幕点歌上限），而老配置文件里写着 20，直接读进来就还是 20，
    /// 用户会以为新规则没生效。
    ///
    /// 这里只在值**恰好等于旧默认值**时替换，避免把用户手工调过的值覆盖掉。
    pub fn migrate(&mut self) -> bool {
        /// `max_queue` 的旧默认值（弹幕上限引入之前）。
        const OLD_DEFAULT_MAX_QUEUE: usize = 20;

        let mut changed = false;

        if self.rules.max_queue == OLD_DEFAULT_MAX_QUEUE {
            self.rules.max_queue = RequestRules::default().max_queue;
            // 旧配置里没有这个概念，补上默认值
            if self.rules.host_extra_per_play == 0 {
                self.rules.host_extra_per_play = RequestRules::default().host_extra_per_play;
            }
            info!(
                max_queue = self.rules.max_queue,
                host_extra = self.rules.host_extra_per_play,
                "配置已迁移：弹幕点歌上限从旧默认值 20 调整为 7（主播点歌每首补 2 个名额）"
            );
            changed = true;
        }

        // 点歌冷却：把 `0` 恢复成默认的 30 秒。
        //
        // 为什么需要这一步：`0` 在语义上是「不限冷却」，看起来像用户有意关掉的；
        // 但开发/验证期间为了连续点歌会被临时设成 0，之后忘了改回来，
        // 结果**弹幕可以无限刷歌**——这在直播间是灾难性的。
        // 默认 30 秒是需求明确要求的（"你给弹幕的点歌cd设置默认为30秒"），
        // 所以这里把 0 视作「没配过」恢复默认；真想关掉冷却的话，
        // 保存一次配置（PUT /api/config）即可，迁移只在启动时跑一次。
        if self.rules.cooldown_secs == 0 {
            self.rules.cooldown_secs = RequestRules::default().cooldown_secs;
            info!(
                cooldown_secs = self.rules.cooldown_secs,
                "配置已迁移：点歌冷却从 0 恢复为默认 30 秒（如需完全关闭，请在设置里改成 0 并保存）"
            );
            changed = true;
        }

        changed
    }

    /// 从指定路径读取配置。
    pub fn load_from(path: &Path) -> Result<Self> {        let raw = fs::read_to_string(path).with_context(|| format!("读取 {} 失败", path.display()))?;
        let cfg: Config =
            serde_json::from_str(&raw).with_context(|| format!("解析 {} 失败", path.display()))?;
        Ok(cfg)
    }

    /// 保存到默认位置（原子写：先写临时文件再重命名）。
    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::config_path())
    }

    /// 保存到指定路径。
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, json).with_context(|| format!("写入 {} 失败", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("替换 {} 失败", path.display()))?;
        Ok(())
    }

    /// 生成 `http://host:port` 形式的基础地址。
    pub fn base_url(&self) -> String {
        format!("http://{}:{}", self.server.host, self.server.port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_bumps_legacy_default_max_queue() {
        // 老配置里 max_queue 是旧默认值 20 → 应迁移成新默认 7
        let mut cfg = Config::default();
        cfg.rules.max_queue = 20;
        cfg.rules.host_extra_per_play = 0;
        assert!(cfg.migrate(), "应发生迁移");
        assert_eq!(cfg.rules.max_queue, 7);
        assert_eq!(cfg.rules.host_extra_per_play, 2);
    }

    #[test]
    fn migrate_keeps_user_customised_values() {
        // 用户手工调过的值不能被覆盖
        let mut cfg = Config::default();
        cfg.rules.max_queue = 3;
        assert!(!cfg.migrate(), "非旧默认值不应迁移");
        assert_eq!(cfg.rules.max_queue, 3);

        // 0 = 不限，同样不该被改
        let mut unlimited = Config::default();
        unlimited.rules.max_queue = 0;
        assert!(!unlimited.migrate());
        assert_eq!(unlimited.rules.max_queue, 0);
    }

    #[test]
    fn migrate_is_idempotent() {
        let mut cfg = Config::default();
        cfg.rules.max_queue = 20;
        assert!(cfg.migrate());
        // 再跑一次不应再改动
        assert!(!cfg.migrate());
        assert_eq!(cfg.rules.max_queue, 7);
    }

    #[test]
    fn default_rules_match_agreed_limits() {
        let rules = RequestRules::default();
        assert_eq!(rules.max_queue, 7, "弹幕最多同时在等 7 首");
        assert_eq!(rules.host_extra_per_play, 2, "主播每首补 2 个名额");
    }
}
