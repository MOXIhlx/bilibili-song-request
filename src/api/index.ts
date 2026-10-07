/**
 * 内嵌 axum 服务器的访问层。
 *
 * 地址解析顺序：
 *  1. `window.__BSR_SERVER__`（Tauri 启动时注入，端口可能与默认值不同）
 *  2. `VITE_BSR_API`（构建期环境变量，便于远程调试）
 *  3. 同源（浏览器 / OBS 直接访问 127.0.0.1:17777 时，用相对路径最稳）
 */
import type {
  AppState,
  BilibiliPhase,
  BlacklistEntry,
  Config,
  IdleMode,
  IdleResponse,
  PlaylistsResponse,
  MusicLoginResult,
  MusicPlatform,
  MusicSearchResponse,
  MusicStatus,
  PlayUrlResponse,
  PlatformStatus,
  QueueItem,
  RequestRecord,
  Song,
  StoredIn,
  WsMessage,
} from '@/types'

/**
 * 服务器基地址，`''` 表示同源相对路径。
 *
 * ## ⚠️ 注入的是 `base`，不是 `host`/`port`
 * 服务端注入的是 `window.__BSR_SERVER__ = { base: "http://127.0.0.1:17777" }`。
 * 早期这里只判断 `injected?.port`（**没有这个字段**），于是直接落到 `''`，
 * `API_BASE` 成了空串。对「OBS 直连面板」没影响（同源相对路径可用），
 * 但在**桌面窗口的内嵌预览**里（页面在 `tauri://localhost`，跨源）会出问题：
 * 背景图 `/bg/x.jpg` 被解析成 `tauri://localhost/bg/x.jpg` → **必然 404**，
 * 用户看到的就是「背景图没生效」。
 * 现在优先用注入的 `base`，并保留 `host`/`port` 写法做兼容。
 */
function resolveBase(): string {
  const injected = window.__BSR_SERVER__
  if (injected) {
    if (injected.base) return injected.base.replace(/\/$/, '')
    if (injected.port) return `http://${injected.host}:${injected.port}`
  }

  const fromEnv = import.meta.env.VITE_BSR_API as string | undefined
  if (fromEnv) return fromEnv.replace(/\/$/, '')

  // 浏览器直接打开 127.0.0.1:17777 时使用同源；vite dev 由 proxy 转发。
  return ''
}

export const API_BASE = resolveBase()

/** 拼接 API 路径。 */
export function apiUrl(path: string): string {
  return `${API_BASE}${path.startsWith('/') ? path : `/${path}`}`
}

/** WS 地址（自动在 http/https 之间切换协议）。 */
export function wsUrl(path = '/ws'): string {
  if (API_BASE) {
    return `${API_BASE.replace(/^http/, 'ws')}${path}`
  }
  const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
  return `${proto}//${window.location.host}${path}`
}

/** 统一错误类型，便于 UI 区分网络错误与业务错误。 */
export class ApiError extends Error {
  constructor(
    message: string,
    readonly status?: number,
  ) {
    super(message)
    this.name = 'ApiError'
  }
}

/**
 * 「操作成功但没有内容可做」的提示（后端 200 + `{"notice": ...}`）。
 *
 * 典型场景：已经在第一首，再点「上一首」。这是**正常边界**，
 * 不该显示成「请求 … 失败」。
 */
export class ApiNotice extends Error {
  constructor(readonly notice: string) {
    super(notice)
    this.name = 'ApiNotice'
  }
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  let res: Response
  try {
    // 只有带 body 的请求才声明 JSON：空 body + JSON 声明会被 axum 判成 400。
    const hasBody = init?.body != null
    res = await fetch(apiUrl(path), {
      headers: hasBody ? { 'Content-Type': 'application/json' } : {},
      ...init,
    })
  } catch (err) {
    throw new ApiError(`无法连接本地服务 (${apiUrl(path)})：${(err as Error).message}`)
  }
  if (!res.ok) {
    throw new ApiError(`请求 ${path} 失败`, res.status)
  }
  if (res.status === 204) return undefined as T
  const data = (await res.json()) as unknown
  // 后端用 `{"notice": ...}` 表达「没有内容可做」，交给调用方决定怎么提示
  if (data && typeof data === 'object' && 'notice' in data) {
    throw new ApiNotice(String((data as { notice: unknown }).notice))
  }
  return data as T
}

/** 健康检查。 */
export function getHealth(): Promise<{ status: string; version: string; uptime_secs: number }> {
  return request('/health')
}

/** 拉取完整状态。 */
export function getState(): Promise<AppState> {
  return request('/api/state')
}

/** 拉取队列。 */
export function getQueue(): Promise<QueueItem[]> {
  return request('/api/queue')
}

/** 拉取配置。 */
export function getConfig(): Promise<Config> {
  return request('/api/config')
}

/** 保存配置。 */
export function saveConfig(config: Config): Promise<Config> {
  return request('/api/config', { method: 'PUT', body: JSON.stringify(config) })
}

/** 队列操作名称，与后端 POST /api/queue/:id/:action 对应。 */
export type QueueAction = 'remove' | 'top' | 'up' | 'down'

/** 对某个队列项执行操作。 */
export function queueAction(id: string, action: QueueAction): Promise<AppState> {
  return request(`/api/queue/${encodeURIComponent(id)}/${action}`, { method: 'POST' })
}

/** 清空队列。 */
export function clearQueue(): Promise<AppState> {
  return request('/api/queue/clear', { method: 'POST' })
}

/** 清空播放列表（点歌队列 + 播放序列），不影响空闲歌单。 */
export function clearPlaylist(): Promise<AppState> {
  return request('/api/queue/clear-playlist', { method: 'POST' })
}

/**
 * 一键清空点歌列表**并开始播空闲歌单**（阶段 10d）。
 *
 * 与 `clearQueue` 的区别：清空后会立刻按空闲歌单出歌
 * （被点歌打断过则回到原来那首重头播），用于直播中"清场"。
 */
export function clearQueueAndPlayIdle(): Promise<AppState> {
  return request('/api/queue/clear-and-play-idle', { method: 'POST' })
}

/** 跳过当前歌曲（队列语义里的「下一首」）。 */
export function skipCurrent(): Promise<AppState> {
  return request('/api/player/skip', { method: 'POST' })
}

/** 回到播放序列里的上一首（真正换歌，不是重播）。 */
export function previousTrack(): Promise<AppState> {
  return request('/api/player/previous', { method: 'POST' })
}

/** 重头播放当前歌曲（进度归零并继续）。 */
export function replayCurrent(): Promise<AppState> {
  return request('/api/player/replay', { method: 'POST' })
}

/** 跳转到指定位置（秒）。 */
export function seekTo(position: number): Promise<AppState> {
  return request('/api/player/seek', {
    method: 'POST',
    body: JSON.stringify({ position }),
  })
}

/** 播放 / 暂停。 */
export function setPaused(paused: boolean): Promise<AppState> {
  return request('/api/player/pause', { method: 'POST', body: JSON.stringify({ paused }) })
}

/** 设置音量（0-100）。 */
export function setVolume(volume: number): Promise<AppState> {
  return request('/api/player/volume', { method: 'POST', body: JSON.stringify({ volume }) })
}

/** 设置播放模式。 */
export function setPlayMode(mode: AppState['play_mode']): Promise<AppState> {
  return request('/api/player/mode', { method: 'POST', body: JSON.stringify({ mode }) })
}

/** 开始 / 继续播放队列（阶段 6）。 */
export function playerPlay(): Promise<AppState> {
  return request('/api/player/play', { method: 'POST' })
}

/** 播放器状态（含 mpv 是否可用）。 */
export interface PlayerStatus {
  available: boolean
  position: number
  duration: number
  volume: number
  paused: boolean
  playing: boolean
  mode: AppState['play_mode']
}

/** 查询播放器状态。 */
export function getPlayerStatus(): Promise<PlayerStatus> {
  return request('/api/player/status')
}

// ─────────────────────────────────────────────────────────────────────────────
// B 站弹幕连接
// ─────────────────────────────────────────────────────────────────────────────

/** 弹幕连接状态（对应后端 BilibiliStatusResponse）。 */
export interface BilibiliStatus {
  connected: boolean
  /** idle / connecting / connected / reconnecting / stopped */
  status: string
  room_id: string | null
  last_error: string | null
  auto_connect: boolean
  /** 给用户看的进度阶段。 */
  phase: BilibiliPhase
  /** 阶段说明（中文）。 */
  detail: string | null
  /** 已尝试次数（含重连）。 */
  attempts: number
  /** 最近一次状态变化时间。 */
  updated_at: string | null
}

/** 连接时可只提交变更的字段，后端会合并进已保存的配置。 */
export interface ConnectPayload {
  app_id?: string
  access_key_id?: string
  access_key_secret?: string
  code?: string
}

/** 查询弹幕连接状态。 */
export function getBilibiliStatus(): Promise<BilibiliStatus> {
  return request('/api/bilibili/status')
}

/** 保存凭据并发起连接。 */
export function connectBilibili(payload: ConnectPayload): Promise<BilibiliStatus> {
  return request('/api/bilibili/connect', { method: 'POST', body: JSON.stringify(payload) })
}

/** 断开连接。 */
export function disconnectBilibili(): Promise<BilibiliStatus> {
  return request('/api/bilibili/disconnect', { method: 'POST' })
}

/** 手动加歌：只给文本时后端会异步搜索真实曲目。 */
export function addSong(
  title: string,
  artist?: string,
  requestedBy?: string,
): Promise<AppState> {
  return request('/api/queue/add', {
    method: 'POST',
    body: JSON.stringify({ title, artist, requested_by: requestedBy }),
  })
}

/** 手动加歌：直接使用搜索结果（已解析，跳过搜索）。 */
export function addResolvedSong(song: Song, requestedBy?: string): Promise<AppState> {
  return request('/api/queue/add', {
    method: 'POST',
    body: JSON.stringify({ song, requested_by: requestedBy }),
  })
}

/**
 * 注入一条模拟弹幕，走真实的解析 + 广播链路。
 * 用于在没有身份码 / 未开播时验证面板链路。
 */
export function simulateDanmaku(text: string, user?: string): Promise<AppState> {
  return request('/api/bilibili/simulate', {
    method: 'POST',
    body: JSON.stringify({ text, user }),
  })
}

// ─────────────────────────────────────────────────────────────────────────────
// 音乐平台（阶段 5）
// ─────────────────────────────────────────────────────────────────────────────

/** 查询音乐平台登录状态与待解析条目数。 */
export function getMusicStatus(): Promise<MusicStatus> {
  return request('/api/music/status')
}

/** 搜索歌曲。 */
export function searchMusic(params: {
  keyword?: string
  title?: string
  artist?: string
  platform?: MusicPlatform
  limit?: number
}): Promise<MusicSearchResponse> {
  return request('/api/music/search', { method: 'POST', body: JSON.stringify(params) })
}

/** 保存 Cookie（内嵌登录窗口抓取后调用，也可手动粘贴）。 */
export function saveMusicCookie(
  platform: MusicPlatform,
  cookie: string,
): Promise<{ platform: MusicPlatform; stored_in: StoredIn; message: string; logged_in: boolean }> {
  return request('/api/music/cookie', {
    method: 'POST',
    body: JSON.stringify({ platform, cookie }),
  })
}

/** 清除某平台 Cookie。 */
export function clearMusicCookie(platform: MusicPlatform): Promise<MusicStatus> {
  return request('/api/music/cookie/clear', {
    method: 'POST',
    body: JSON.stringify({ platform }),
  })
}

/** 取播放地址（队列条目或指定曲目）。 */
export function getPlayUrl(params: {
  item_id?: string
  song_id?: string
  platform?: MusicPlatform
}): Promise<PlayUrlResponse> {
  return request('/api/music/play-url', { method: 'POST', body: JSON.stringify(params) })
}

/** 是否运行在 Tauri 桌面窗口里（决定能否用内嵌登录窗口）。 */
export function isDesktop(): boolean {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window
}

/**
 * 打开内嵌登录窗口（仅桌面端可用）。
 *
 * 登录成功后后端会抓取 Cookie 存入凭据库，并通过 `music-login-result` 事件通知；
 * 这里返回 Promise 只表示「窗口已打开」。
 */
export async function openMusicLogin(platform: MusicPlatform): Promise<void> {
  if (!isDesktop()) {
    throw new ApiError('内嵌登录窗口仅在桌面程序里可用，请在 exe 窗口中操作')
  }
  const { invoke } = await import('@tauri-apps/api/core')
  await invoke('open_music_login', { platform })
}

/** 桌面端命令返回的登录结果。 */
export interface DesktopLoginState {
  platform: MusicPlatform
  success: boolean
  message: string
  logged_in: boolean
}

/** 桌面端清除 Cookie。 */
export async function desktopClearMusicCookie(platform: MusicPlatform): Promise<void> {
  const { invoke } = await import('@tauri-apps/api/core')
  await invoke('clear_music_cookie', { platform })
}

/** 平台展示名（前端本地兜底，避免依赖后端返回）。 */
export function platformLabel(platform: MusicPlatform): string {
  return platform === 'netease' ? '网易云音乐' : 'QQ 音乐'
}

// ── 空闲歌单（阶段 10a）──────────────────────────────────────────────────

/** 读取空闲歌单。 */
export function getIdle(): Promise<IdleResponse> {
  return request('/api/idle')
}

/** 往空闲歌单加歌（给 `song` 表示已解析，否则由后端搜索）。 */
export function addIdleSong(payload: {
  title?: string
  artist?: string
  song?: Song
}): Promise<IdleResponse> {
  return request('/api/idle', { method: 'POST', body: JSON.stringify(payload) })
}

/** 从空闲歌单移除。 */
export function removeIdleSong(id: string): Promise<IdleResponse> {
  return request('/api/idle', { method: 'DELETE', body: JSON.stringify({ id }) })
}

/** 清空空闲歌单。 */
export function clearIdle(): Promise<IdleResponse> {
  return request('/api/idle/clear', { method: 'POST' })
}

/** 切换空闲歌单播放模式。 */
export function setIdleMode(mode: IdleMode): Promise<IdleResponse> {
  return request('/api/idle/mode', { method: 'POST', body: JSON.stringify({ mode }) })
}

/** 立即播放空闲歌单里的一首（按 id 或下标；都不给则播下一首）。 */
export function playIdle(payload: { id?: string; index?: number }): Promise<AppState> {
  return request('/api/idle/play', { method: 'POST', body: JSON.stringify(payload) })
}

/** 列出某个平台的收藏歌单（阶段 10b）。 */
export function getPlaylists(platform: MusicPlatform): Promise<PlaylistsResponse> {
  return request(`/api/music/playlists?platform=${platform}`)
}

/** 把收藏歌单导入空闲歌单（`mode` = append / replace）。 */
export function importPlaylistToIdle(payload: {
  platform: MusicPlatform
  playlist_id: string
  mode?: 'append' | 'replace'
  limit?: number
}): Promise<IdleResponse> {
  return request('/api/idle/import', { method: 'POST', body: JSON.stringify(payload) })
}

/** 一张已上传的背景图。 */
export interface BackgroundItem {
  url: string
  name: string
  size: number
  modified?: number | null
}

/** 列出已上传的面板背景图（阶段 10b 补充：界面要做选择 + 预览）。 */
export function getBackgrounds(): Promise<BackgroundItem[]> {
  return request('/api/panel/background')
}

/** 给已上传的背景图改名（扩展名沿用原图）。 */
export function renameBackground(from: string, to: string): Promise<BackgroundItem[]> {
  return request('/api/panel/background/rename', {
    method: 'POST',
    body: JSON.stringify({ from, to }),
  })
}

/** 删除已上传的背景图。 */
export function deleteBackground(name: string): Promise<BackgroundItem[]> {
  return request('/api/panel/background/delete', {
    method: 'POST',
    body: JSON.stringify({ name }),
  })
}

/**
 * 下载背景图到本地。
 *
 * 浏览器源要的是 HTTP 地址，但用户可能想留一份原图（换机器/重装后恢复），
 * 所以提供一个"另存为"入口。这里直接触发浏览器下载，不做额外打包。
 */
export function downloadBackground(url: string, name: string): void {
  const a = document.createElement('a')
  a.href = apiUrl(url)
  a.download = name
  a.rel = 'noreferrer'
  document.body.appendChild(a)
  a.click()
  document.body.removeChild(a)
}

/**
 * 上传面板背景图（阶段 9）。
 *
 * 图片由后端存到程序配置目录，返回可直接给 OBS 用的站内地址（`/bg/xxx`）。
 * 为什么不转 base64 塞进 URL：大图会把地址撑到几千字符，OBS 可能拒绝。
 */
export async function uploadBackground(file: File): Promise<string> {  const res = await fetch(apiUrl('/api/panel/background'), {
    method: 'POST',
    headers: { 'Content-Type': file.type || 'application/octet-stream' },
    body: file,
  })
  if (!res.ok) {
    // 后端的错误信息是可读中文，优先透出
    let detail = `HTTP ${res.status}`
    try {
      const body = (await res.json()) as { error?: string }
      if (body.error) detail = body.error
    } catch {
      // 非 JSON，保持 HTTP 码
    }
    throw new ApiError(`上传背景图失败：${detail}`, res.status)
  }
  const data = (await res.json()) as { url?: string }
  if (!data.url) throw new ApiError('上传成功但后端未返回图片地址')
  return data.url
}

/**
 * 读取黑名单（阶段 9）。
 *
 * 命中的歌**弹幕点不了**（不搜索不入队），主播用点歌机可以无视。
 */
export function getBlacklist(): Promise<BlacklistEntry[]> {
  return request('/api/blacklist')
}

/** 加入黑名单（返回更新后的列表）。 */
export function addBlacklistEntry(
  title: string,
  artist = '',
  note?: string,
): Promise<BlacklistEntry[]> {
  return request('/api/blacklist', {
    method: 'POST',
    body: JSON.stringify({ title, artist, note }),
  })
}

/** 移出黑名单（返回更新后的列表）。 */
export function removeBlacklistEntry(title: string, artist = ''): Promise<BlacklistEntry[]> {
  return request('/api/blacklist', {
    method: 'DELETE',
    body: JSON.stringify({ title, artist }),
  })
}

/** 点歌日志筛选条件。 */
export interface RequestLogFilters {
  /** 只看 `queued`（成功）或 `rejected`（被拒）。 */
  outcome?: 'queued' | 'rejected'
  /** 按昵称或 UID 精确匹配。 */
  user?: string
  /** 最多返回多少条（后端上限 2000）。 */
  limit?: number
}

/**
 * 拉取**完整**点歌日志（阶段 8）。
 *
 * 与 `AppState.stats.recent` 的区别：那份只带最近几十条，
 * 因为它会随每次状态变化经 `/ws` 全量广播；
 * 完整日志（含时间/点歌人/歌名/歌手/结果/拒绝原因）由本接口按需拉取。
 */
export function getRequestLog(filters: RequestLogFilters = {}): Promise<RequestRecord[]> {
  const params = new URLSearchParams()
  if (filters.outcome) params.set('outcome', filters.outcome)
  if (filters.user) params.set('user', filters.user)
  if (filters.limit) params.set('limit', String(filters.limit))
  const query = params.toString()
  return request(`/api/requests/log${query ? `?${query}` : ''}`)
}

/** 重新导出便于组件直接使用类型。 */
export type { MusicLoginResult, MusicSearchResponse, MusicStatus, PlatformStatus, Song }

/**
 * 解析 WS 文本帧；解析失败返回 null（忽略脏数据而不是抛错打断连接）。
 */
export function parseWsMessage(raw: string): WsMessage | null {
  try {
    const parsed = JSON.parse(raw) as WsMessage
    if (parsed && typeof parsed === 'object' && 'type' in parsed) return parsed
    return null
  } catch {
    return null
  }
}
