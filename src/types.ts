/**
 * 前后端共享的数据模型。
 *
 * ⚠️ 这份定义必须与 `src-tauri/src/models.rs` 中的 Rust 结构体逐字段保持一致
 * （serde 默认使用 snake_case 字段名与外部标记的枚举）。任何一侧改动都要同步另一侧。
 */

/** 音乐平台。 */
export type MusicPlatform = 'netease' | 'qq'

/** 队列条目状态。 */
export type QueueStatus = 'pending' | 'playing' | 'played' | 'skipped'

/** 播放模式。 */
export type PlayMode = 'sequential' | 'random' | 'repeat_one'

/** 歌曲元信息的来源状态（阶段 5）。 */
export type SongSource = 'pending' | 'resolved' | 'failed'

/** 一首歌的元信息。 */
export interface Song {
  id: string
  title: string
  artist: string
  platform: MusicPlatform
  /** 时长（秒），未知为 0。 */
  duration: number
  cover_url: string | null
  /** 专辑名（阶段 5）。 */
  album: string | null
  /** 元信息来源状态（阶段 5）。 */
  source: SongSource
  /** 解析失败原因（阶段 5）。 */
  source_error: string | null
}

/** 点歌优先级档位。 */
export type QueuePriority = 'danmaku' | 'host'

/** 点歌队列中的一项。 */
export interface QueueItem {
  id: string
  song: Song
  requested_by: string
  /** 点歌人 UID，未知为 null。 */
  requested_by_uid: string | null
  /** RFC3339 时间戳。 */
  requested_at: string
  status: QueueStatus
  /** 优先级：主播点歌机会插到所有弹幕点歌之前。 */
  priority: QueuePriority
}

/** mpv 播放状态快照。 */
export interface PlayerState {
  playing: boolean
  paused: boolean
  /** 当前播放位置（秒）。 */
  position: number
  duration: number
  volume: number
  /** 当前歌词 LRC 原文（阶段 7）。 */
  lyrics: string | null
  /** 正在唱第几行（阶段 7）。 */
  lyric_index: number | null
}

/** 弹幕连接进度阶段（给用户看的粒度，比后端状态机更细）。 */
export type BilibiliPhase =
  | 'idle'
  | 'requesting_session'
  | 'connecting_socket'
  | 'connected'
  | 'retrying'
  | 'stopped'
  | 'failed'

/** B 站弹幕连接状态。 */
export interface BilibiliState {
  connected: boolean
  /** 直播间 ID / 身份码对应的会话信息（阶段 3 填充）。 */
  room_id: string | null
  /** 最近一次错误，用于前端提示。 */
  last_error: string | null
  /** 当前进度阶段。 */
  phase: BilibiliPhase
  /** 阶段的可读说明（中文，直接展示）。 */
  detail: string | null
  /** 已尝试次数（含重连）。 */
  attempts: number
  /** 最近一次状态变化时间（RFC3339）。 */
  updated_at: string | null
}

/** 点歌请求的处理记录（阶段 4）。 */
export interface RequestRecord {
  user: string
  uid: string | null
  title: string
  artist: string
  /** `queued` 或 `rejected`。 */
  outcome: 'queued' | 'rejected'
  /** 拒绝分类：cooldown / duplicate / queue_full / fans_medal / user_level。 */
  reason: string | null
  /** 被拒绝时的可读说明。 */
  message: string | null
  /** 入队位置（成功时）。 */
  position: number | null
  /** RFC3339 时间戳。 */
  at: string
}

/** 冷却中的用户。 */
export interface CooldownView {
  /** 用户标识（UID 或 `name:昵称`）。 */
  key: string
  remaining_secs: number
}

/** 点歌统计（阶段 4）。 */
export interface RequestStats {
  cooldowns: CooldownView[]
  /** 最近的请求记录，最新的在前。 */
  recent: RequestRecord[]
}

/** 后端全量状态，对应 GET /api/state。 */
export interface AppState {
  version: string
  /** 服务器启动时间（RFC3339）。 */
  started_at: string
  play_mode: PlayMode
  /** 点歌队列（待播的新歌）。真正决定播放顺序的是 `playing`。 */
  queue: QueueItem[]
  current: QueueItem | null
  player: PlayerState
  bilibili: BilibiliState
  stats: RequestStats
  /**
   * **实际播放序列**（阶段 10d 重构）。
   *
   * 「上一首 / 下一首」都在这条序列上移动 `cursor`，不再各走各的列表。
   * 已播部分在 `cursor` 左边、待播部分在右边；新歌只追加到末尾，
   * **不移动** `cursor`。
   */
  playing: QueueItem[]
  /** 当前正在播放在 `playing` 中的下标。 */
  cursor: number
  /**
   * 弹幕还能点几首（上限；每播完一首主播点歌会补充）。
   *
   * 「已用多少」由前端自行从 `queue` 里数 `priority === 'danmaku'` 得出——
   * 后端刻意不存这个计数，避免多个离队入口各扣一次导致漂移。
   */
  danmaku_slots: number
  /** 空闲歌单（阶段 10a）：点歌队列空时自动播它。 */
  idle: QueueItem[]
  /** 空闲歌单的播放模式（与点歌队列的 `play_mode` 独立）。 */
  idle_mode: IdleMode
  /**
   * 空闲歌单**书签**：正在播（或最近播过）那一首的下标。
   *
   * 被点歌打断后，点歌播完会回到这首**重头播**。
   * 注意它与 `idle_next` 差 1，不能混用。
   */
  idle_current: number | null
  /** 空闲歌单**下一次取歌**的下标。 */
  idle_next: number
  /** 空闲歌单是否被点歌打断、等待回去重头播。 */
  idle_pending_return: boolean
  /** 当前正在播的是不是空闲歌单的歌。 */
  current_is_idle: boolean
}

/** 服务器配置。 */
export interface ServerConfig {
  host: string
  port: number
}

/** B 站直播开放平台配置（身份码模式）。 */
export interface BilibiliConfig {
  app_id: string
  access_key_id: string
  access_key_secret: string
  /** 身份码，形如 `xxxx-xxxx-xxxx-xxxx`。 */
  code: string
  /** exe 内部只建立一个连接，此项仅用于展示 B 站侧限制。 */
  auto_connect: boolean
}

/** 点歌优先级策略（默认平台搜不到精确原唱时怎么选）。 */
export type PickPolicy =
  /** 优先完整时长：宁可换平台听翻唱，也不放 95 秒试听。 */
  | 'prefer_full_length'
  /** 优先原唱：宁可只放试听片段，也不换平台。 */
  | 'prefer_artist'

/** 点歌规则。 */
export interface RequestRules {
  /** 指令正则，默认 `^点歌\s+(.+?)(?:\s+(.+))?$`。 */
  command_regex: string
  /** 同一用户冷却秒数。 */
  cooldown_secs: number
  /** 队列上限，0 表示不限。 */
  max_queue: number
  /** 每播完一首主播点歌，给弹幕补充多少个可点名额。 */
  host_extra_per_play: number
  /** 是否允许同一首歌重复点。 */
  allow_duplicate: boolean
  /** 预留：最低粉丝牌等级，0 表示不限制。 */
  min_fans_medal_level: number
  /** 预留：最低用户等级，0 表示不限制。 */
  min_user_level: number
  /**
   * 点歌优先级策略。
   *
   * 各平台版权策略不同，会出现「原唱只有 95 秒试听 vs 翻唱是完整版」的两难，
   * 由这个开关决定取舍（见后端 `PickPolicy`）。
   */
  pick_policy: PickPolicy
  /**
   * 搜索用哪个平台（阶段 9）。
   *
   * - `auto`    先搜 QQ，没有精确命中时再搜网易云综合评分（默认）
   * - `qq`      只用 QQ，搜不到就报搜不到
   * - `netease` 只用网易云，搜不到就报搜不到
   */
  search_platform: SearchPlatform
  /** 有人点歌时，正在播的空闲歌曲要不要立刻让位（阶段 10a）。 */
  idle_switch_policy: IdleSwitchPolicy
}

/** 搜索平台策略（阶段 9）。 */
export type SearchPlatform = 'auto' | 'qq' | 'netease'

/** 空闲歌单的播放模式（阶段 10a）。 */
export type IdleMode = 'sequential' | 'loop_all' | 'loop_one' | 'shuffle'

/** 有人点歌时，正在播的空闲歌曲要不要让位（阶段 10a）。 */
export type IdleSwitchPolicy = 'immediate' | 'after_current'

/** `GET /api/idle` 响应。 */
export interface IdleResponse {
  items: QueueItem[]
  mode: IdleMode
  /** 书签：正在播（或最近播过）那一首的下标。 */
  current: number | null
  /** 下一次取歌的下标。 */
  next: number
  current_is_idle: boolean
  modes: Array<{ value: IdleMode; label: string }>
}

/** 一个收藏歌单的概要（阶段 10b）。 */
export interface PlaylistInfo {
  id: string
  name: string
  track_count: number
  cover_url?: string | null
  creator?: string | null
  platform: MusicPlatform
  /** 是否是「我喜欢 / 我喜欢的音乐」。 */
  special: boolean
}

/** `GET /api/music/playlists` 响应。 */
export interface PlaylistsResponse {
  platform: MusicPlatform
  playlists: PlaylistInfo[]
  logged_in: boolean
}

/** 一条黑名单记录。 */
export interface BlacklistEntry {
  /** 歌名。 */
  title: string
  /** 歌手（空 = 拉黑这首歌的所有版本）。 */
  artist: string
  /** 加入时间（RFC3339）。 */
  added_at?: string | null
  /** 备注。 */
  note?: string | null
}

/**
 * 一套**具名**的面板样式。
 *
 * 地址里用 `?style=<id>` 引用（`id` 是短 ASCII），`name` 只用于界面显示。
 *
 * ## 为什么是数组而不是单个对象
 * 早期全局只有一套默认样式，于是「歌词页一套配色、队列页另一套」只能靠
 * URL 参数逐个覆盖，地址长到 100+ 字符，改一次配色还要在每个 OBS 源里
 * 重新复制地址。命名样式让地址只写 `?style=<id>`，改一处即可全局生效。
 */
export interface PanelStyleConfig {
  /** 样式标识（短 ASCII，出现在 URL 里）。 */
  id: string
  /** 界面显示名（中文）。 */
  name: string
  theme: 'dark' | 'light'
  bg: 'transparent' | 'solid'
  /**
   * 主色：歌曲名、队列高亮、歌词当前行、弹幕用户名等**强调**文字与装饰。
   *
   * 早期它同时控制进度条，导致「只想改进度条颜色，结果整个面板都变了」，
   * 于是进度条拆到了 `bar_color`。
   */
  color: string
  /**
   * 进度条填充色。
   *
   * **空字符串 = 跟随主色**（默认）。这样只想整体协调时只改 `color` 一处；
   * 想让进度条单独跳色时再单独设它。
   */
  bar_color: string
  /**
   * 字体颜色。`null` = 跟随主题。
   *
   * 与 `color` 的分工：`color` 是**强调色**（歌曲名、队列高亮等），
   * `fg` 是**普通正文颜色**（歌手、点歌人、时间、队列条目等）。
   *
   * ⚠️ 主题为 `dark` 时应给浅色，为 `light` 时应给深色；
   * 给反了会让文字与背景融在一起（实测 dark + `#000000` 时正文几乎不可见）。
   */
  fg: string | null
  /**
   * 卡片/列表底色。默认 `transparent` = 完全不要底色。
   *
   * 之前这里是写死的 `color-mix(fg 8%, transparent)`，
   * 于是 OBS 勾了透明背景也仍有一层灰底。
   */
  surface: string
  /** 进度条轨道颜色。默认 `transparent`。 */
  track: string
  /**
   * 背景图。`null` = 无背景图。
   *
   * 只接受 `http(s)://` 或站内路径（如 `/bg/xxx.png`）——
   * OBS 会拦 `file://`，所以本地绝对路径在这里没有意义。
   * 图片由「设置 → OBS 面板 → 背景图库」上传，经 `/bg/` 提供。
   *
   * 图片的裁剪/旋转/镜像在编辑时**烘焙进新图片文件**，所以这里只存路径、
   * 不存变换参数——避免「样式里存了变换、但图片被换掉」导致对不上。
   */
  bg_image: string | null
  font_size: number
  scale: number
  limit: number
  show_lyrics: boolean
  /**
   * 歌名（正在播放的曲名）单独颜色。`null` = 跟随主色 `color`。
   *
   * 拆出来是因为歌名和「队列高亮 / 歌词当前行 / 弹幕用户名」共用主色时，
   * 想让歌名更跳一点就得整体改主色，其他元素也跟着变。
   */
  title_color: string | null
  /**
   * 正文字重（歌手、点歌人、时间、队列条目等）。
   *
   * 早期全部写死 `600`，细体字族下显得糊、粗体字族下过重。现在按
   * "正文 / 次级说明 / 歌名" 三档分别可调。
   */
  font_weight: number
  /** 次级说明字重（「点歌人」「队列为空」这类小字）。 */
  font_weight_sub: number
  /** 歌名字重。 */
  font_weight_title: number
  /**
   * 文字描边宽度（px）。`0` = 不描边。
   *
   * 面板要叠在任意背景图/直播画面上，浅色字压在浅色区域会糊掉；
   * 描边是最省事的可读性保障。实现用 `-webkit-text-stroke`（WebView2 支持），
   * 再加一层同色 `text-shadow` 让描边更实。
   */
  text_stroke_width: number
  /** 描边颜色。`null` = 自动（浅色字用深描边、深色字用浅描边）。 */
  text_stroke_color: string | null
  /**
   * 面板布局：
   *  - `list`    竖向堆叠（默认）
   *  - `compact` 精简单行
   *  - `lyrics`  以歌词为主
   *  - `wide`    宽版：左侧歌曲/队列文字，右侧歌词（阶段 7）
   */
  layout: 'list' | 'compact' | 'lyrics' | 'wide'
}

/** 应用配置，落盘于 %APPDATA%/bilibili-song-request/config.json。 */
export interface Config {
  server: ServerConfig
  bilibili: BilibiliConfig
  rules: RequestRules
  /** 命名面板样式列表（至少一套）。 */
  panel_styles: PanelStyleConfig[]
  /** 默认样式 id；指向不存在时面板回落到列表第一项。 */
  default_style_id: string
  /** 点歌黑名单（阶段 9）。 */
  blacklist: BlacklistEntry[]
}

/** WS /ws 推送的消息封包。 */
export type WsMessage =
  | { type: 'state'; data: AppState }
  | { type: 'danmaku'; data: { user: string; uid: string | null; text: string; at: string } }
  | { type: 'connection'; data: BilibiliState }
  | { type: 'request'; data: RequestEvent }
  | { type: 'song'; data: SongEvent }
  | { type: 'player'; data: PlayerEvent }
  | { type: 'lyrics'; data: LyricsEvent }
  | { type: 'hello'; data: { version: string; started_at: string } }
  | { type: 'pong'; data: { at: string } }
  | { type: 'error'; data: { message: string } }

/** 歌词事件（阶段 7）。 */
export interface LyricsEvent {
  item_id: string
  title: string
  /** LRC 原文；无歌词时为 null。 */
  lyrics: string | null
  /** 是否纯音乐。 */
  instrumental: boolean
}

/** 播放事件（阶段 6）。 */
export interface PlayerEvent {
  event: 'started' | 'finished' | 'failed'
  item_id: string
  title: string
  /** started 时携带。 */
  artist?: string
  requested_by?: string
  platform?: MusicPlatform
  duration?: number
  /** finished 时的结束状态。 */
  status?: QueueStatus
  /** failed 时的原因。 */
  reason?: string
}

/** 曲目解析事件（阶段 5）。 */
export interface SongEvent {
  outcome: 'resolved' | 'failed'
  item_id: string
  title: string
  /** 解析成功时携带。 */
  artist?: string
  platform?: MusicPlatform
  duration?: number
  /** 解析失败时携带。 */
  reason?: string
}

// ─────────────────────────────────────────────────────────────────────────────
// 音乐平台（阶段 5）
// ─────────────────────────────────────────────────────────────────────────────

/** 凭据存放位置。 */
export type StoredIn = 'keyring' | 'file'

/** 单个平台的登录状态。 */
export interface PlatformStatus {
  platform: MusicPlatform
  display_name: string
  logged_in: boolean
  stored_in: StoredIn | null
}

/** `GET /api/music/status`。 */
export interface MusicStatus {
  default_platform: MusicPlatform
  platforms: PlatformStatus[]
  /** 队列中尚未解析完成的条目数。 */
  pending_resolution: number
}

/** `POST /api/music/search` 响应。 */
export interface MusicSearchResponse {
  platform: MusicPlatform
  keyword: string
  results: Song[]
}

/** `POST /api/music/play-url` 响应。 */
export interface PlayUrlResponse {
  song_id: string
  platform: MusicPlatform
  url: string
  title: string
  artist: string
}

/** 登录结果（Tauri 事件 / 命令返回）。 */
export interface MusicLoginResult {
  platform: MusicPlatform
  success: boolean
  message: string
  logged_in: boolean
}

/** 点歌请求的实时事件（阶段 4）。 */
export interface RequestEvent {
  outcome: 'queued' | 'rejected'
  user: string
  title: string
  /** 成功入队时的队内位置。 */
  position?: number
  /** 入队成功时携带的歌手。 */
  artist?: string
  /** 被拒绝时的分类与说明。 */
  reason?: string
  message?: string
}
