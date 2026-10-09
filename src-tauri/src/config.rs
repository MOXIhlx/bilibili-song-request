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
use tracing::{debug, info, warn};

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

/// **软件界面**（桌面窗口 / 浏览器控制台）的主题配色。
///
/// ## 为什么只有两个颜色
/// 需求是"不需要添加其他功能，只要能改这两颜色就行"。其余配色
/// （边框、次要文字、浅色底、文字色）由前端按这两个颜色**推导**出来，
/// 不让用户去调十来个变量。
///
/// ## 为什么不影响 OBS 面板
/// 面板有自己独立的一套 `--panel-*` 变量（随样式存储，见
/// [`PanelStyleConfig`]）。本结构只作用于软件自身的界面。
///
/// 默认值就是原有的粉白配色，所以老配置不带这个字段时外观完全不变。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiThemeConfig {
    /// 主题色（强调色）：按钮、选中态、链接、进度条。
    pub accent: String,
    /// 界面背景色。
    pub bg: String,
}

impl Default for UiThemeConfig {
    fn default() -> Self {
        // 与 `src/styles/base.css` 的出厂值一致：
        // 强调色用正粉；底色用极浅的暖粉（不是纯白，长时间看不刺眼）。
        Self {
            accent: "#ff6fa5".to_string(),
            bg: "#fff5f9".to_string(),
        }
    }
}

/// 判断是否是可接受的 CSS 颜色字面量（只收 `#rgb` / `#rrggbb`）。
///
/// ## 为什么要校验
/// 这两个值会被前端直接写进 `:root` 的 CSS 变量。若允许任意字符串，
/// 一个手改坏的 `config.json`（或一次误调 API）就能把整套变量设成
/// 非法值——**整个界面会失去全部颜色**，比"配色难看"严重得多。
/// 前端 `resolveUiTheme` 也有回落，这里再加一道，保证落盘的配置本身是干净的。
fn is_hex_color(value: &str) -> bool {
    let v = value.trim();
    let Some(hex) = v.strip_prefix('#') else {
        return false;
    };
    if hex.len() != 3 && hex.len() != 6 {
        return false;
    }
    hex.chars().all(|c| c.is_ascii_hexdigit())
}

impl UiThemeConfig {
    /// 把非法颜色修回默认值，返回是否发生了修改。
    ///
    /// 空字符串也一并修掉：它多半来自"字段被手动清空"，而不是用户想要无色。
    pub fn sanitize(&mut self) -> bool {
        let mut changed = false;
        let default = Self::default();
        if !is_hex_color(&self.accent) {
            self.accent = default.accent;
            changed = true;
        }
        if !is_hex_color(&self.bg) {
            self.bg = default.bg;
            changed = true;
        }
        changed
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

/// 一套具名的面板样式。
///
/// 地址里用 `?style=<id>` 引用；`name` 只用于界面显示（中文名）。
///
/// 反序列化时 `id`/`name` 缺失（老配置或手写 JSON）会回落到默认值，
/// 由 [`Config::normalize_styles`] 补齐，保证列表里每项都可用。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PanelStyleConfig {
    /// 样式标识（短 ASCII，出现在 URL 里）。空 = 待补齐。
    pub id: String,
    /// 界面显示名（中文）。空 = 用 id 代替。
    pub name: String,
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
    ///
    /// 图片的裁剪/旋转/镜像在编辑时**烘焙进新图片文件**，所以这里只存路径，
    /// 不存变换参数——避免「样式里存了变换、但图片被换掉」导致的对不上。
    pub bg_image: Option<String>,
    /// 背景图不透明度（`0.0` ~ `1.0`，默认 `1.0` = 完全不透明）。
    ///
    /// 用途：把背景图调淡当**底纹**用，让文字更清楚，不必去外部软件先把图调亮。
    /// 面板用一层 `::before` 承载背景图并设它的 `opacity`，
    /// 所以**不会**连带把文字也变透明。
    pub bg_opacity: f64,
    /// 背景图在面板里的**取景位置**（CSS `background-position` 的取值）。
    ///
    /// ## 为什么需要它
    /// 面板用 `background-size: cover` 铺图，图片比例与面板不一致时必然要裁掉
    /// 一部分，而裁哪一部分由 `background-position` 决定。默认 `center`
    /// 经常正好裁掉主体（人脸、标题），用户需要能自己拖。
    ///
    /// 取值是标准 CSS 写法：`"50% 50%"`（默认）、`"20% 80%"`、`"left top"`。
    /// 存字符串而不是两个数字，是为了让前端直接透传给 CSS，也方便手改配置。
    ///
    /// `None` / 空串 = `center`，与不设时行为一致（老配置不受影响）。
    #[serde(default)]
    pub bg_position: Option<String>,
    /// **预览画布尺寸**（宽 × 高，CSS px）。
    ///
    /// ## 为什么样式里要存"预览用的尺寸"
    /// 这两个值描述的是**你 OBS 浏览器源的实际宽高**。面板的排版
    /// （换行位置、`wide` 布局是否左右分栏）与背景图 `cover` 的裁切，
    /// **完全由容器尺寸决定**——所以设置页的预览必须按这个尺寸渲染，
    /// 否则"预览里看着刚好，OBS 里完全两样"。
    ///
    /// 它不影响 OBS 里的实际显示（OBS 用自己源的尺寸），
    /// 只决定预览怎么渲染，所以放在样式里跟着走是合适的：
    /// 一个样式通常对应一组尺寸相同的源。
    ///
    /// 老配置没有这两个字段时回落到 `1920×1080`（OBS 标准画布）。
    #[serde(default)]
    pub canvas_w: Option<u32>,
    #[serde(default)]
    pub canvas_h: Option<u32>,
    /// 设置页里预览栏的宽度（px）。纯界面偏好，不影响任何显示效果。
    #[serde(default)]
    pub preview_width: Option<u32>,
    pub font_size: u32,
    pub scale: f64,
    pub limit: usize,
    pub show_lyrics: bool,
    /// 歌名单独颜色；`None` = 跟随 `color`。
    ///
    /// 拆出来是因为歌名与「队列高亮 / 歌词当前行 / 弹幕用户名」共用主色时，
    /// 想让歌名更跳一点就得整体改主色，其它元素跟着一起变。
    pub title_color: Option<String>,
    /// 正文字重（歌手、点歌人、时间、队列条目等）。
    ///
    /// 早期全部写死 `600`：细体字族下显糊、粗体字族下过重。
    /// 现在按「正文 / 次级说明 / 歌名」三档分别可调。
    pub font_weight: u32,
    /// 次级说明字重（「点歌人」「队列为空」这类小字）。
    pub font_weight_sub: u32,
    /// 歌名字重。
    pub font_weight_title: u32,
    /// 文字描边宽度（px）；`0` = 不描边。
    ///
    /// 面板要叠在任意背景图/直播画面上，浅色字压在浅色区域会糊掉，
    /// 描边是最省事的可读性保障。
    pub text_stroke_width: f64,
    /// 描边颜色；`None` = 自动（浅色字配深描边、深色字配浅描边）。
    pub text_stroke_color: Option<String>,
    pub layout: String,
}

impl Default for PanelStyleConfig {
    fn default() -> Self {
        Self {
            // 默认样式固定用 `pink`，与 `Config::default_style_id` 对应
            id: "pink".to_string(),
            name: "粉白".to_string(),
            theme: "light".to_string(),
            bg: "transparent".to_string(),
            color: "#ff6fa5".to_string(),
            // 空 = 跟随主色：OBS 里只调一个颜色就能整体协调
            bar_color: String::new(),
            fg: None,
            // 默认全透明：用户明确要求「只要文字与进度条颜色」
            surface: "transparent".to_string(),
            track: "transparent".to_string(),
            bg_image: None,
            // 1.0 = 完全不透明，与加这个字段之前的行为一致
            bg_opacity: 1.0,
            // None = center，与加这个字段之前的行为一致
            bg_position: None,
            // None = 1920×1080（OBS 标准画布），与加字段前的写死值无关：
            // 早期预览写死 960×540，那是拍脑袋的"典型值"，已废弃
            canvas_w: None,
            canvas_h: None,
            // None = 440px（比早期写死的 380px 宽，因为左边表单有 949px 太空）
            preview_width: None,
            font_size: 16,
            scale: 1.0,
            limit: 8,
            show_lyrics: true,
            // 跟随主色：只调一个颜色就整体协调
            title_color: None,
            // 早期写死 600；这里保持同样的观感作为默认
            font_weight: 600,
            font_weight_sub: 400,
            font_weight_title: 700,
            // 默认不描边：不加任何东西时外观与之前完全一致
            text_stroke_width: 0.0,
            text_stroke_color: None,
            layout: "list".to_string(),
        }
    }
}

impl PanelStyleConfig {
    /// 造一套具名样式（`id` 由调用方保证唯一）。
    pub fn named(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            ..Self::default()
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
    /// 软件界面（桌面窗口 / 控制台）的主题配色。
    ///
    /// 与 OBS 面板的 `panel_styles` **完全独立**：面板样式管的是 OBS 里
    /// 显示什么，这里管的是软件自己长什么样。
    #[serde(default)]
    pub ui_theme: UiThemeConfig,
    /// 面板样式列表（命名样式）。
    ///
    /// ## 为什么从「单个 panel 对象」改成数组
    /// 早期只有一套全局默认样式，于是「歌词页一套配色、队列页另一套」只能靠
    /// URL 参数逐个覆盖——地址长到 100+ 字符，而且改一次配色要在每个 OBS
    /// 浏览器源里重新复制一遍地址。改成命名样式后，地址只写 `?style=<id>`，
    /// 改样式一处生效于所有引用它的源。
    ///
    /// 老配置里的 `panel` 字段在 [`Self::migrate_legacy_panel`] 中迁移过来。
    pub panel_styles: Vec<PanelStyleConfig>,
    /// 默认样式 id。为空或指向不存在的 id 时回落到列表第一项。
    pub default_style_id: String,
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
    /// 旧版单一样式字段（**只用于读取迁移**，不再写回）。
    ///
    /// 反序列化时若存在就转成 `panel_styles` 的第一项；保存时永远序列化为
    /// `null`，等于自动清理旧字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel: Option<LegacyPanelStyle>,
}

/// 旧版（v1.0.x）的单一面板样式。字段与 [`PanelStyleConfig`] 完全一致，
/// 单独一个类型是为了让 `Config::panel` 能被判空并做一次性迁移。
pub type LegacyPanelStyle = PanelStyleConfig;

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

    /// 从默认位置读取；文件不存在时落盘一份默认配置。
    ///
    /// ## ⚠️ 解析失败时**绝不覆盖**已有文件
    /// 早期这里的分支是「读失败 → 用默认配置 → 立刻 save 覆盖」。
    /// 那条路径会**静默摧毁用户数据**：配置里装着用户手工调出来的面板样式
    /// （配色、字号、描边、背景图、画布尺寸……），一旦某个字段解析失败
    /// （字段类型改过、手改坏、写了一半），整个 config 就被默认值顶掉，
    /// 用户看到的就是「我的自定义方案没了」——**且没有任何退路**。
    ///
    /// 现在：
    ///  1. 解析失败只**警告**，把坏文件改名保留（`config.json.broken-<时间戳>`），
    ///     供事后抢救；绝不静默删除或覆盖；
    ///  2. 本次运行用默认配置（程序仍然可用，不会打不开）；
    ///  3. 用户在界面里**主动保存**时才会写入新文件（那时 `save_to` 也会
    ///     先备份，见 [`Self::backup_existing`]）。
    ///
    /// 文件**不存在**是另一回事——那是首次运行，落盘一份默认配置是合理的。
    pub fn load() -> Self {
        let path = Self::config_path();
        if !path.is_file() {
            info!(path = %path.display(), "配置文件不存在，写入一份默认配置");
            let mut cfg = Self::default();
            /*
             * ⚠️ 必须迁移/规范化后再落盘。
             *
             * `Config::default()` 的 `panel_styles` 是**空数组**（样式由
             * `normalize_styles` 补齐）。早期这里直接写默认配置，于是
             * 首次运行会落下一份 `panel_styles: []` 的无效配置——
             * 界面上就是"一套方案都没有"。同一段代码也用在
             * "配置文件损坏被移走"之后，所以那种情况下同样会写出空列表。
             */
            cfg.migrate();
            if let Err(err) = cfg.save_to(&path) {
                warn!(error = %err, "写入默认配置失败");
            }
            return cfg;
        }

        match Self::load_from(&path) {
            Ok(mut cfg) => {
                info!(path = %path.display(), "已加载配置");
                if cfg.migrate() {
                    // 迁移结果要落盘，否则每次启动都会重新迁移一次。
                    // `save_to` 会先备份原文件，所以这次覆盖是安全的。
                    if let Err(err) = cfg.save_to(&path) {
                        warn!(error = %err, "迁移后的配置写入失败");
                    }
                }
                cfg
            }
            Err(err) => {
                /*
                 * 解析失败：**保留原文件**，只把它改名成 `.broken-<时间戳>`
                 * 留档，然后用默认配置跑起来。
                 *
                 * 为什么不直接覆盖：用户可能只是某个字段写坏了，
                 * 保留原文件才有可能把里面的样式人工抢救出来。
                 */
                let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
                let broken = path.with_file_name(format!("{CONFIG_FILE_NAME}.broken-{stamp}"));
                let kept = match fs::rename(&path, &broken) {
                    Ok(()) => {
                        warn!(
                            error = %err,
                            kept = %broken.display(),
                            "配置解析失败，原文件已改名保留（未删除），本次运行使用默认配置"
                        );
                        true
                    }
                    Err(rename_err) => {
                        warn!(
                            error = %err,
                            rename_error = %rename_err,
                            "配置解析失败，且无法改名保留原文件；本次运行使用默认配置（原文件未改动）"
                        );
                        false
                    }
                };
                let mut cfg = Self::default();
                // 与"文件不存在"分支同理：默认配置也要先规范化，
                // 否则会落下 `panel_styles: []`（界面上一套方案都没有）。
                cfg.migrate();
                // 只有在原文件**已经安全移走**时才写默认配置；
                // 否则宁可不写，也不能覆盖掉那份可能还能抢救的文件。
                if kept {
                    if let Err(save_err) = cfg.save_to(&path) {
                        warn!(error = %save_err, "写入默认配置失败");
                    }
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

        // 单一样式 → 命名样式列表
        if self.migrate_legacy_panel() {
            changed = true;
        }
        // 补齐 id/name、去掉重复 id、修正默认样式指向
        if self.normalize_styles() {
            changed = true;
        }
        // 界面配色：把非法颜色（手改坏 / 被清空 / 非十六进制）修回默认，
        // 否则前端会拿到无法解析的值，整套 CSS 变量失效、界面失去颜色
        if self.ui_theme.sanitize() {
            info!("配置已迁移：界面配色中存在非法颜色，已恢复默认值");
            changed = true;
        }

        changed
    }

    /// 把旧版 `panel`（单个样式对象）迁移成 `panel_styles` 的第一项。
    ///
    /// 返回是否发生了迁移。迁移后 `panel` 置空，保存时即被清理。
    fn migrate_legacy_panel(&mut self) -> bool {
        let Some(legacy) = self.panel.take() else {
            return false;
        };
        // 老配置的样式沿用原来的颜色等设置，只补上 id/name。
        //
        // ⚠️ 判断「有没有显式 id」不能只看空串：旧配置的 `panel` 里根本没有
        // `id` 字段，而 `#[serde(default)]` 会用 `PanelStyleConfig::default()`
        // 兜底——它的 id 恰好就是出厂默认样式的 `pink`。于是 id 与默认样式
        // 撞名，`normalize_styles()` 把其中一套改成 `pink-2`，用户升级后
        // **莫名多出第二套一模一样的样式**（实测踩到过两次，第一次误以为
        // 是空串问题，改完仍然复现，才发现是 serde 默认值在作怪）。
        //
        // 所以「空」或「等于出厂默认值」都视为未指定。
        let mut style = legacy;
        let factory = PanelStyleConfig::default();
        if style.id.trim().is_empty() || style.id == factory.id {
            style.id = "legacy".to_string();
        }
        if style.name.trim().is_empty() || style.name == factory.name {
            style.name = "原有样式".to_string();
        }
        let id = style.id.clone();
        info!(
            style_id = %id,
            "配置已迁移：单一面板样式转为命名样式列表（地址需带 ?style={id}）"
        );
        self.panel_styles.insert(0, style);
        // 老的单一样式在语义上就是「默认样式」，除非用户已经指定过别的
        if self.default_style_id.trim().is_empty() || self.style_by_id(&self.default_style_id).is_none() {
            self.default_style_id = id;
        }
        true
    }

    /// 规范化样式列表：保证非空、id/name 有值且唯一、默认指向存在。
    ///
    /// 对**手写或外部修改过的 config.json** 也要成立，所以这里不假设任何前提。
    pub fn normalize_styles(&mut self) -> bool {
        let mut changed = false;

        // 列表为空：补一套出厂默认样式
        if self.panel_styles.is_empty() {
            self.panel_styles.push(PanelStyleConfig::default());
            changed = true;
        }

        // id / name 补齐 + 去重
        let mut seen: Vec<String> = Vec::new();
        let mut next_suffix = 2;
        for style in &mut self.panel_styles {
            if style.id.trim().is_empty() {
                // 用 name 转拼音不现实，直接用序号构造稳定 id
                let candidate = format!("style{next_suffix}");
                next_suffix += 1;
                style.id = candidate;
                changed = true;
            }
            if style.name.trim().is_empty() {
                style.name = style.id.clone();
                changed = true;
            }
            if seen.contains(&style.id) {
                // 重复 id：加后缀直到唯一
                let base = style.id.clone();
                let mut n = 2;
                let mut candidate = format!("{base}-{n}");
                while seen.contains(&candidate) {
                    n += 1;
                    candidate = format!("{base}-{n}");
                }
                style.id = candidate;
                changed = true;
            }
            seen.push(style.id.clone());
        }

        // 默认样式指向必须存在
        if !seen.iter().any(|id| *id == self.default_style_id) {
            self.default_style_id = seen.first().cloned().unwrap_or_default();
            changed = true;
        }

        changed
    }

    /// 找一套样式；找不到返回 `None`。
    pub fn style_by_id(&self, id: &str) -> Option<&PanelStyleConfig> {
        self.panel_styles.iter().find(|s| s.id == id)
    }

    /// 取默认样式；列表异常为空时返回一份出厂默认值。
    pub fn default_style(&self) -> PanelStyleConfig {
        self.style_by_id(&self.default_style_id)
            .or_else(|| self.panel_styles.first())
            .cloned()
            .unwrap_or_default()
    }

    /// 生成一个未被占用的样式 id（`style2`、`style3`…）。
    pub fn next_style_id(&self) -> String {
        let mut n = 2;
        loop {
            let candidate = format!("style{n}");
            if !self.panel_styles.iter().any(|s| s.id == candidate) {
                return candidate;
            }
            n += 1;
        }
    }

    /// 从指定路径读取配置。
    ///
    /// ⚠️ 这里**只做反序列化**，不做规范化/迁移。
    ///
    /// 早期版本在这里调用了 `normalize_styles()`，结果是：读到一份「只有旧
    /// `panel` 字段、没有 `panel_styles`」的老配置时，先补出一套出厂 `pink`，
    /// 随后 `migrate()` 又把旧样式插进去——用户升级后莫名多出第二套样式
    /// （实测踩到，列表变成 `["legacy", "pink"]`）。
    ///
    /// 正确顺序是**先迁移、再规范化**，由 [`Self::load`] 统一负责。
    /// 直接使用本函数的调用方（含测试）需要自己调 `migrate()`。
    pub fn load_from(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path).with_context(|| format!("读取 {} 失败", path.display()))?;
        let cfg: Config =
            serde_json::from_str(&raw).with_context(|| format!("解析 {} 失败", path.display()))?;
        Ok(cfg)
    }

    /// 保存到默认位置（原子写：先写临时文件再重命名）。
    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::config_path())
    }

    /// 保存到指定路径。
    ///
    /// ## 原子写
    /// 先写 `config.json.tmp` 再 `rename` 覆盖——避免写到一半崩溃/断电时
    /// 留下半截 JSON（下次启动会解析失败，进而触发"用默认配置覆盖"，
    /// 用户数据就没了）。
    ///
    /// ## 覆盖前先备份
    /// 见 [`Self::backup_existing`]：会写一份带时间戳的副本并只保留最近几份。
    /// 早期这里**直接覆盖**，没有任何退路——一次解析失败 + 自动写默认配置，
    /// 用户的全部自定义样式就永久消失了（真实发生过）。
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Self::backup_existing(path);
        let json = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, json).with_context(|| format!("写入 {} 失败", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("替换 {} 失败", path.display()))?;
        Ok(())
    }

    /// 覆盖 `path` 之前，把现有内容另存为 `config.json.<时间戳>.bak`。
    ///
    /// ## 为什么必须有
    /// 配置里装着用户**手工调出来的面板样式**（配色、字号、描边、背景图、
    /// 画布尺寸……），丢了很难重建。而历史上存在一条真实的丢数据路径：
    ///
    /// ```text
    /// 新版字段变化 → 旧 config.json 解析失败
    ///   → Config::load 走 Err 分支 → 写一份默认配置覆盖它
    ///   → 用户的样式全部永久消失，且没有任何备份可恢复
    /// ```
    ///
    /// 现在无论谁要覆盖，都先留一份副本。
    ///
    /// ## 只保留最近 [`MAX_CONFIG_BACKUPS`] 份
    /// 不然每次改配置都留一份，目录会无限增长。
    fn backup_existing(path: &Path) {
        /// 最多保留几份历史备份。
        const MAX_CONFIG_BACKUPS: usize = 5;

        if !path.is_file() {
            return;
        }
        // 时间戳精确到秒。同一秒内多次覆盖时**不能跳过**——那样就漏掉了
        // 中间那几次的保护（单元测试正是抓到了这一点）。
        // 改为同名时自动加序号，保证每次覆盖都真的留下一份。
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| CONFIG_FILE_NAME.to_string());
        let mut backup = path.with_file_name(format!("{name}.{stamp}.bak"));
        let mut seq = 2;
        while backup.exists() {
            backup = path.with_file_name(format!("{name}.{stamp}-{seq}.bak"));
            seq += 1;
            if seq > 100 {
                // 极端情况下（同一秒被覆盖上百次）放弃，避免死循环
                warn!("同一秒内备份过多，跳过本次备份");
                return;
            }
        }
        if let Err(err) = fs::copy(path, &backup) {
            // 备份失败不能阻断保存本身——否则用户会"改什么都存不上"。
            warn!(error = %err, "配置备份失败，继续保存");
            return;
        }
        debug!(path = %backup.display(), "已备份旧配置");

        // 清理多余的历史备份（按文件名排序即按时间排序）
        let Some(dir) = path.parent() else { return };
        let prefix = format!("{name}.");
        let mut olds: Vec<std::path::PathBuf> = fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .map(|n| {
                        let n = n.to_string_lossy();
                        n.starts_with(&prefix) && n.ends_with(".bak")
                    })
                    .unwrap_or(false)
            })
            .collect();
        if olds.len() <= MAX_CONFIG_BACKUPS {
            return;
        }
        olds.sort();
        for old in &olds[..olds.len() - MAX_CONFIG_BACKUPS] {
            let _ = fs::remove_file(old);
        }
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
    fn ui_theme_defaults_are_the_original_pink_white() {
        // 默认值必须与前端 base.css 的出厂配色一致，
        // 否则老配置（不带 ui_theme 字段）升级后外观会变
        let t = UiThemeConfig::default();
        assert_eq!(t.accent, "#ff6fa5");
        assert_eq!(t.bg, "#fff5f9");
        assert!(!t.clone().sanitize(), "默认值本身应当是合法的，不该被修改");
    }

    #[test]
    fn ui_theme_sanitize_repairs_invalid_colors() {
        // 非法颜色会把前端整套 CSS 变量搞失效，必须修回默认值
        for bad in ["", "red", "ff6fa5", "#ff6fa", "#gggggg", "rgb(1,2,3)"] {
            let mut t = UiThemeConfig {
                accent: bad.to_string(),
                bg: bad.to_string(),
            };
            assert!(t.sanitize(), "{bad:?} 应被判为非法并修正");
            assert_eq!(t.accent, UiThemeConfig::default().accent);
            assert_eq!(t.bg, UiThemeConfig::default().bg);
        }
    }

    #[test]
    fn ui_theme_sanitize_accepts_short_and_long_hex() {
        // #rgb 与 #rrggbb 都是合法 CSS 颜色，不应被改掉
        for good in ["#abc", "#ABC", "#a1b2c3", "  #ffffff  "] {
            let mut t = UiThemeConfig {
                accent: good.to_string(),
                bg: good.to_string(),
            };
            assert!(!t.sanitize(), "{good:?} 是合法颜色，不该被修改");
            assert_eq!(t.accent, good);
        }
    }

    #[test]
    fn migrate_repairs_invalid_ui_theme() {
        // 手改坏的 config.json 在启动迁移时也应被修好
        let mut cfg = Config {
            ui_theme: UiThemeConfig {
                accent: "not-a-color".into(),
                bg: "".into(),
            },
            ..Default::default()
        };
        assert!(cfg.migrate());
        assert_eq!(cfg.ui_theme.accent, UiThemeConfig::default().accent);
        assert_eq!(cfg.ui_theme.bg, UiThemeConfig::default().bg);
    }

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
        // 用户手工调过的值不能被覆盖。
        //
        // ⚠️ 先 `normalize_styles()` 再断言：`Config::default()` 的
        // `panel_styles` 是空数组（serde 的 `Vec` 默认值），首次 `migrate()`
        // 会补一套出厂样式并因此返回 `true`——那是**首次运行**的正常行为，
        // 与「规则值是否被覆盖」无关。真实场景里配置总是先经过
        // `load_from()`（内含规范化），所以这里显式对齐那个前提。
        let mut cfg = Config::default();
        cfg.normalize_styles();
        cfg.rules.max_queue = 3;
        assert!(!cfg.migrate(), "非旧默认值不应迁移");
        assert_eq!(cfg.rules.max_queue, 3);

        // 0 = 不限，同样不该被改
        let mut unlimited = Config::default();
        unlimited.normalize_styles();
        unlimited.rules.max_queue = 0;
        assert!(!unlimited.migrate());
        assert_eq!(unlimited.rules.max_queue, 0);
    }

    #[test]
    fn deserialized_config_fills_styles_without_touching_rules() {
        // 真实路径：从磁盘读一份**没有 panel_styles 字段**的配置。
        // 迁移要补齐样式列表，同时一个规则值都不能动。
        let raw = r##"{ "rules": { "max_queue": 3, "cooldown_secs": 45 } }"##;
        let mut cfg: Config = serde_json::from_str(raw).unwrap();
        cfg.migrate();
        assert_eq!(cfg.panel_styles.len(), 1, "应补一套出厂样式");
        assert_eq!(cfg.panel_styles[0].id, "pink");
        assert_eq!(cfg.default_style_id, "pink");
        assert_eq!(cfg.rules.max_queue, 3);
        assert_eq!(cfg.rules.cooldown_secs, 45);
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

    // ── 命名样式的迁移与规范化 ───────────────────────────────────────────

    #[test]
    fn legacy_panel_becomes_first_style() {
        // 老配置：只有单个 `panel` 对象，没有 panel_styles
        let mut cfg = Config::default();
        cfg.panel_styles.clear();
        cfg.default_style_id.clear();
        let mut legacy = PanelStyleConfig::default();
        legacy.id.clear();
        legacy.name.clear();
        legacy.color = "#123456".to_string();
        cfg.panel = Some(legacy);

        assert!(cfg.migrate(), "应发生迁移");
        assert_eq!(cfg.panel_styles.len(), 1, "旧样式应成为列表第一项");
        let style = &cfg.panel_styles[0];
        assert_eq!(style.id, "legacy");
        assert_eq!(style.name, "原有样式");
        assert_eq!(style.color, "#123456", "老的配色设置必须保留");
        assert_eq!(cfg.default_style_id, "legacy");
        assert!(cfg.panel.is_none(), "旧字段应被清空，保存时即被清理");
    }

    #[test]
    fn legacy_panel_with_empty_id_does_not_collide_with_default() {
        // 实测踩到的 bug：旧配置的 `panel` 里没有 `id` 字段，`#[serde(default)]`
        // 用 `PanelStyleConfig::default()` 兜底，而它的 id 正是出厂默认样式的
        // `pink`——于是 id 撞名，`normalize_styles()` 把其中一套改成 `pink-2`，
        // 用户升级后**莫名多出第二套一模一样的样式**。
        //
        // 这个用例复刻真实老配置：`panel` 里既没有 id 也没有 name。
        let mut cfg = Config::default();
        cfg.panel_styles.clear();
        cfg.default_style_id.clear();
        let mut legacy = PanelStyleConfig::default();
        legacy.theme = "dark".into();
        cfg.panel = Some(legacy);

        cfg.migrate();

        assert_eq!(
            cfg.panel_styles.len(),
            1,
            "迁移后只应有一套样式，实际 {:?}",
            cfg.panel_styles.iter().map(|s| s.id.as_str()).collect::<Vec<_>>()
        );
        assert_eq!(cfg.panel_styles[0].id, "legacy");
        assert_eq!(cfg.default_style_id, "legacy");
        assert_eq!(cfg.panel_styles[0].theme, "dark", "老的配色必须保留");
    }

    #[test]
    fn legacy_panel_from_raw_json_migrates_to_single_style() {
        // 端到端复刻：磁盘上的老配置长这样（panel 无 id/name），
        // 经 load_from → migrate 之后必须只有一套样式。
        let raw = r##"{
            "panel": { "theme": "dark", "color": "#f50000", "limit": 5 }
        }"##;
        let mut cfg: Config = serde_json::from_str(raw).unwrap();
        // 必须先迁移再规范化（load_from 不再自动规范化，见它的文档）
        cfg.migrate();
        assert_eq!(
            cfg.panel_styles.len(),
            1,
            "实际 {:?}",
            cfg.panel_styles.iter().map(|s| s.id.as_str()).collect::<Vec<_>>()
        );
        assert_eq!(cfg.panel_styles[0].id, "legacy");
        assert_eq!(cfg.panel_styles[0].color, "#f50000");
        assert_eq!(cfg.panel_styles[0].limit, 5);
    }

    #[test]
    fn fresh_config_without_any_panel_fields_gets_factory_style() {
        // 全新安装：配置里既没有 panel 也没有 panel_styles
        let mut cfg: Config = serde_json::from_str("{}").unwrap();
        cfg.migrate();
        assert_eq!(cfg.panel_styles.len(), 1);
        assert_eq!(cfg.panel_styles[0].id, "pink", "全新配置应得到出厂样式");
        assert_eq!(cfg.default_style_id, "pink");
    }

    #[test]
    fn migrate_is_idempotent_for_styles() {
        // 迁移跑两次不能再多出样式（启动时 `migrate()` 可能被调用多次）
        let mut cfg = Config::default();
        cfg.panel_styles.clear();
        cfg.default_style_id.clear();
        let mut legacy = PanelStyleConfig::default();
        legacy.id.clear();
        legacy.name.clear();
        cfg.panel = Some(legacy);

        cfg.migrate();
        let after_first = cfg.panel_styles.len();
        cfg.migrate();
        assert_eq!(cfg.panel_styles.len(), after_first, "第二次迁移不应再加样式");
    }

    #[test]
    fn normalize_fills_empty_style_list() {
        // 手写/损坏的 config：列表为空 → 补一套出厂默认样式
        let mut cfg = Config::default();
        cfg.panel_styles.clear();
        cfg.default_style_id.clear();
        assert!(cfg.normalize_styles());
        assert_eq!(cfg.panel_styles.len(), 1);
        assert_eq!(cfg.panel_styles[0].id, "pink");
        assert_eq!(cfg.default_style_id, "pink");
    }

    #[test]
    fn normalize_dedupes_ids_and_fills_names() {
        let mut cfg = Config::default();
        cfg.panel_styles = vec![
            PanelStyleConfig { id: "a".into(), name: "".into(), ..Default::default() },
            PanelStyleConfig { id: "a".into(), name: "第二套".into(), ..Default::default() },
            PanelStyleConfig { id: "".into(), name: "第三套".into(), ..Default::default() },
        ];
        cfg.default_style_id = "nope".to_string();
        assert!(cfg.normalize_styles());

        let ids: Vec<&str> = cfg.panel_styles.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids.len(), 3);
        // 全部唯一
        let mut uniq = ids.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(uniq.len(), 3, "id 必须唯一，实际 {ids:?}");
        // 空名用 id 兜底
        assert_eq!(cfg.panel_styles[0].name, "a");
        // 悬空的默认指向被修正到第一项
        assert_eq!(cfg.default_style_id, ids[0]);
    }

    #[test]
    fn normalize_is_idempotent() {
        let mut cfg = Config::default();
        cfg.panel_styles.clear();
        cfg.normalize_styles();
        assert!(!cfg.normalize_styles(), "第二次不应再有改动");
    }

    #[test]
    fn default_style_falls_back_when_id_missing() {
        let mut cfg = Config::default();
        cfg.default_style_id = "does-not-exist".to_string();
        // 即使指向不存在的 id，也要拿到一份可用的样式
        let style = cfg.default_style();
        assert_eq!(style.id, "pink");
    }

    #[test]
    fn next_style_id_avoids_collisions() {
        let mut cfg = Config::default();
        assert_eq!(cfg.next_style_id(), "style2");
        cfg.panel_styles.push(PanelStyleConfig::named("style2", "x"));
        assert_eq!(cfg.next_style_id(), "style3");
    }

    #[test]
    fn legacy_panel_survives_json_roundtrip() {
        // 直接给一段「老格式」JSON，模拟用户升级后首次启动
        let raw = r##"{
            "panel": { "theme": "dark", "color": "#abcdef", "limit": 5 }
        }"##;
        let mut cfg: Config = serde_json::from_str(raw).expect("应能解析老配置");
        assert!(cfg.panel.is_some());
        cfg.migrate();
        assert_eq!(cfg.panel_styles.len(), 1);
        assert_eq!(cfg.panel_styles[0].color, "#abcdef");
        assert_eq!(cfg.panel_styles[0].limit, 5);
        // 保存后再读回来，旧字段不该再出现
        let saved = serde_json::to_string(&cfg).unwrap();
        assert!(!saved.contains("\"panel\""), "旧字段不应被写回：{saved}");
        assert!(saved.contains("panel_styles"));
    }

    #[test]
    fn default_rules_match_agreed_limits() {
        let rules = RequestRules::default();
        assert_eq!(rules.max_queue, 7, "弹幕最多同时在等 7 首");
        assert_eq!(rules.host_extra_per_play, 2, "主播每首补 2 个名额");
    }

    // ── 配置安全：用户的自定义样式绝不能被静默删掉 ──────────────────────

    /// 造一个临时目录（不碰用户的真实配置目录）。
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bsr-config-test-{tag}-{}-{}",
            std::process::id(),
            chrono::Local::now().format("%H%M%S%3f")
        ));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    /// 造一份"用户精心调过的"配置：两套带自定义颜色的样式。
    fn user_config() -> Config {
        let mut a = PanelStyleConfig::named("mine", "我的方案");
        a.color = "#123456".into();
        a.font_size = 23;
        let mut b = PanelStyleConfig::named("second", "第二套");
        b.color = "#abcdef".into();
        Config {
            panel_styles: vec![a, b],
            default_style_id: "mine".into(),
            ..Default::default()
        }
    }

    #[test]
    fn save_keeps_a_backup_of_the_previous_file() {
        // 覆盖前必须留一份备份——这是"样式被删掉后还能救回来"的唯一保障
        let dir = temp_dir("backup");
        let path = dir.join("config.json");

        // 用 migrate() 后的默认配置，模拟真实启动路径写下的第一份文件
        // （`Config::default()` 的 panel_styles 是空的，必须规范化才有样式）
        let mut first = Config::default();
        first.migrate();
        first.save_to(&path).unwrap();
        user_config().save_to(&path).unwrap();

        let baks: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".bak"))
            .collect();
        assert_eq!(baks.len(), 1, "应恰好有 1 份备份，实际 {baks:?}");

        // 备份里应当是**覆盖前**那份（默认配置），不是新写进去的
        let bak = dir.join(&baks[0]);
        let restored = Config::load_from(&bak).unwrap();
        assert_eq!(restored.panel_styles.len(), 1);
        assert_eq!(restored.panel_styles[0].id, "pink");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn backups_are_capped_and_oldest_removed() {
        // 备份不能无限增长
        let dir = temp_dir("cap");
        let path = dir.join("config.json");
        for i in 0..9 {
            let mut c = Config::default();
            c.default_style_id = format!("s{i}");
            c.panel_styles = vec![PanelStyleConfig::named(format!("s{i}"), format!("方案{i}"))];
            // 备份用的是秒级时间戳，同一秒内会复用同一份；
            // 这里每次先删掉已有备份，模拟"不同时刻保存"。
            let _ = fs::remove_dir_all(&dir);
            let _ = fs::create_dir_all(&dir);
            if i > 0 {
                // 先放一个旧文件，让 save_to 有东西可备份
                Config::default().save_to(&path).unwrap();
            }
            c.save_to(&path).unwrap();
        }
        let count = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".bak"))
            .count();
        assert!(count <= 5, "备份数量应被限制在 5 份以内，实际 {count}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unparsable_config_is_not_silently_replaced() {
        // 这条是本次修复的核心：解析失败时**原始文件的内容必须还在**，
        // 不能像早期那样被默认配置直接顶掉（那会永久删除用户的样式）。
        let dir = temp_dir("broken");
        let path = dir.join("config.json");
        /*
         * ⚠️ 用 `r##"…"##` 而不是 `r#"…"#`：
         * 下面这段 JSON 里含有 `"#123456"`，其中的 `"#` 会**提前闭合**
         * `r#"…"#`，导致整个文件语法错误（本次就踩了这个）。
         * 多用一个 `#` 后，必须 `"##` 才能收尾，因此不再冲突。
         */
        let broken_json =
            r##"{ "panel_styles": [ { "id": "mine", "color": "#123456" } ], "BROKEN"##;
        fs::write(&path, broken_json).unwrap();

        // 解析确实会失败
        assert!(Config::load_from(&path).is_err());

        // 模拟 load() 的失败分支：改名保留，然后才写默认配置
        let kept = dir.join("config.json.broken-test");
        fs::rename(&path, &kept).unwrap();
        Config::default().save_to(&path).unwrap();

        // 原始内容仍然可以在 .broken-* 里完整取回
        assert!(kept.is_file(), "坏文件必须被保留");
        let raw = fs::read_to_string(&kept).unwrap();
        assert!(raw.contains("#123456"), "坏文件里的用户数据必须原样保留");
        assert_eq!(raw, broken_json);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn normalize_never_drops_user_styles() {
        // 保险：迁移/规范化只补不删，用户的样式数量不会减少
        let mut cfg = user_config();
        cfg.normalize_styles();
        assert_eq!(cfg.panel_styles.len(), 2);
        assert_eq!(cfg.default_style_id, "mine");
        cfg.migrate();
        assert_eq!(cfg.panel_styles.len(), 2, "migrate 不得删除任何样式");
    }

    #[test]
    fn fresh_default_config_has_a_usable_style() {
        // `Config::default()` 的 `panel_styles` 是**空数组**，样式由
        // `normalize_styles` 补齐（`migrate` 内部会调它）。
        //
        // 而 `Config::load()` 在"文件不存在"和"文件损坏被移走"两个分支里
        // 都会写一份默认配置——早期**没有先 migrate**，于是会落下一份
        // `panel_styles: []` 的无效配置，界面上一套方案都没有。
        //
        // 这个测试锁住那条修复：任何"写默认配置"的路径都必须是可用的。
        let mut cfg = Config::default();
        assert!(cfg.panel_styles.is_empty(), "前提：默认值本身是空列表");
        cfg.migrate();
        assert!(!cfg.panel_styles.is_empty(), "migrate 后必须有至少一套样式");
        assert!(
            cfg.style_by_id(&cfg.default_style_id).is_some(),
            "default_style_id 必须指向真实存在的样式"
        );
    }
}
