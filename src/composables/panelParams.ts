/**
 * 面板 URL 参数解析。
 *
 * 支持（与需求文档 4.6 一致）：
 *   theme=dark|light
 *   bg=transparent|solid
 *   color=%23ff0000
 *   fontSize=16
 *   scale=1.2
 *   limit=8
 *   showLyrics=true|false
 *   layout=list|compact|lyrics
 *
 * 未提供的参数回落到后端配置里的默认样式（GET /api/config → panel）。
 * 参数值一律容错：非法值忽略而不报错，保证 OBS 里永远能出画面。
 */
import { computed, onUnmounted, ref, type ComputedRef, type Ref } from 'vue'
import { API_BASE } from '@/api'
import type { PanelStyleConfig } from '@/types'

export interface ResolvedPanelStyle {
  theme: 'dark' | 'light'
  /** 是否透明背景（OBS 勾选透明背景时使用）。 */
  transparent: boolean
  /** 主色（进度条、正在播放标题等强调色）。 */
  color: string
  /** 字体颜色（阶段 9 新增；默认跟随主题）。 */
  fg: string | null
  /** 卡片/列表底色（阶段 9 新增；默认全透明）。 */
  surface: string
  /** 进度条轨道颜色（阶段 9 新增；默认全透明）。 */
  track: string
  /** 背景图 URL（阶段 9 新增；默认无）。 */
  bgImage: string | null
  fontSize: number
  scale: number
  limit: number
  showLyrics: boolean
  /**
   * 面板布局。
   *
   * - `list`    竖向堆叠（默认，最紧凑）
   * - `compact` 精简单行，隐藏歌手与点歌人
   * - `lyrics`  以歌词为主
   * - `wide`    宽版（阶段 7 新增）：左侧歌曲/点歌队列，右侧歌词，
   *             背景保持透明；窄窗口（< 700px）自动堆叠回纵向
   */
  layout: 'list' | 'compact' | 'lyrics' | 'wide'
}

/** 支持的布局取值（新增布局时同步这里）。 */
export const PANEL_LAYOUTS = ['list', 'compact', 'lyrics', 'wide'] as const

/**
 * 面板「专注页」。
 *
 * 一个综合面板在 OBS 里往往放不下 / 字号太小，因此拆成几个独立地址，
 * 主播按需组合成多个浏览器源。所有页面共用同一套样式实现，
 * 只是隐藏不需要的区块。
 *
 * - `all`     综合面板（默认，行为与拆分前完全一致）
 * - `play`    只显示正在播放 + 进度条 + 点歌队列
 * - `lyrics`  只显示歌词（字号放大）
 * - `danmaku` 只显示最近弹幕（字号放大）
 */
export type PanelPage = 'all' | 'play' | 'lyrics' | 'danmaku'

/** 支持的专注页取值。 */
export const PANEL_PAGES = ['all', 'play', 'lyrics', 'danmaku'] as const

/**
 * 从请求路径推断专注页。
 *
 * 路径形如 `/panel`、`/panel/play`、`/panel/lyrics`、`/panel/danmaku`。
 * 也接受查询串 `?page=lyrics`（方便不带子路径时切换）。
 *
 * ⚠️ 桌面窗口用的是 **hash 路由**（`tauri.localhost/#/panel/lyrics`），
 * 这时 `pathname` 恒为 `/`，必须从 hash 里取路径——
 * 浏览器源直连 `127.0.0.1:17777/panel/lyrics` 时则走 pathname。
 * 两者都要支持，否则桌面里切页面会一直停在综合版。
 */
export function resolvePanelPage(
  pathname: string,
  hash: string,
  params?: URLSearchParams,
): PanelPage {
  const fromQuery = params ? parseEnum(params.get('page'), PANEL_PAGES) : null
  if (fromQuery) return fromQuery

  for (const source of [hash, pathname]) {
    const match = /\/panel\/([a-z-]+)/i.exec(source)
    if (match) {
      const candidate = parseEnum(match[1], PANEL_PAGES)
      if (candidate) return candidate
    }
  }
  return 'all'
}

const DEFAULTS: ResolvedPanelStyle = {
  theme: 'dark',
  transparent: true,
  color: '#7dd3fc',
  fg: null,
  // 默认**全透明**：用户要求「只要文字 + 进度条颜色」，
  // 不要那层灰色卡片底。想要卡片感就显式给 surface 一个半透明色。
  surface: 'transparent',
  track: 'transparent',
  bgImage: null,
  fontSize: 16,
  scale: 1,
  limit: 8,
  showLyrics: true,
  layout: 'list',
}

/** 解析 `color=%23ff0000` / `color=red` 之类的值。 */
export function decodeColor(raw: string | null): string | null {
  if (!raw) return null
  const decoded = raw.startsWith('#') ? raw : `#${raw}`
  // 允许 #rgb / #rrggbb / #rrggbbaa
  if (/^#([0-9a-f]{3}|[0-9a-f]{4}|[0-9a-f]{6}|[0-9a-f]{8})$/i.test(decoded)) return decoded
  // 允许 CSS 关键字与 rgb()/hsl() 等函数式颜色，做一次宽松校验。
  if (/^[a-z]+$/i.test(raw)) return raw
  if (/^(rgb|rgba|hsl|hsla)\(/i.test(raw)) return raw
  return null
}

function parseNumber(raw: string | null, min: number, max: number): number | null {
  if (raw === null || raw.trim() === '') return null
  const n = Number(raw)
  if (!Number.isFinite(n)) return null
  return Math.min(Math.max(n, min), max)
}

function parseBool(raw: string | null): boolean | null {
  if (raw === null) return null
  const v = raw.trim().toLowerCase()
  if (['1', 'true', 'yes', 'on'].includes(v)) return true
  if (['0', 'false', 'no', 'off'].includes(v)) return false
  return null
}

function parseEnum<T extends string>(raw: string | null, allowed: readonly T[]): T | null {
  if (!raw) return null
  const v = raw.trim().toLowerCase() as T
  return allowed.includes(v) ? v : null
}

/** 从 URLSearchParams 解析出最终样式。 */
export function resolvePanelStyle(
  params: URLSearchParams,
  defaults?: Partial<PanelStyleConfig> | null,
): ResolvedPanelStyle {
  const style: ResolvedPanelStyle = { ...DEFAULTS }

  if (defaults) {
    style.theme = defaults.theme ?? style.theme
    style.transparent = (defaults.bg ?? 'transparent') === 'transparent'
    style.color = defaults.color ?? style.color
    style.fontSize = defaults.font_size ?? style.fontSize
    style.scale = defaults.scale ?? style.scale
    style.limit = defaults.limit ?? style.limit
    style.showLyrics = defaults.show_lyrics ?? style.showLyrics
    style.layout = defaults.layout ?? style.layout
    // 阶段 9 新增的三项（老配置里没有，用 `??` 保持向后兼容）
    style.surface = defaults.surface ?? style.surface
    style.track = defaults.track ?? style.track
    style.fg = defaults.fg ?? style.fg
    style.bgImage = defaults.bg_image ?? style.bgImage
  }

  style.theme = parseEnum(params.get('theme'), ['dark', 'light'] as const) ?? style.theme
  const bg = parseEnum(params.get('bg'), ['transparent', 'solid'] as const)
  if (bg) style.transparent = bg === 'transparent'
  style.color = decodeColor(params.get('color')) ?? style.color
  // 字体颜色：`fg` 参数（也接受 `fontColor` 这个更口语的写法）
  style.fg = decodeColor(params.get('fg') ?? params.get('fontColor')) ?? style.fg
  // 卡片底色 / 进度条轨道：允许 `none` / `transparent` 显式表示透明
  style.surface = decodeSurface(params.get('surface')) ?? style.surface
  style.track = decodeSurface(params.get('track')) ?? style.track
  // 背景图：只接受 http(s) 与站内相对路径，避免 file:// 之类被 OBS 拦
  style.bgImage = decodeBgImage(params.get('bgImage')) ?? style.bgImage
  style.fontSize = parseNumber(params.get('fontSize'), 8, 96) ?? style.fontSize
  style.scale = parseNumber(params.get('scale'), 0.2, 5) ?? style.scale
  style.limit = Math.round(parseNumber(params.get('limit'), 0, 100) ?? style.limit)
  style.showLyrics = parseBool(params.get('showLyrics')) ?? style.showLyrics
  style.layout = parseEnum(params.get('layout'), PANEL_LAYOUTS) ?? style.layout

  return style
}

/**
 * 解析「底色类」参数：颜色、或显式的 `none`/`transparent`。
 *
 * 与 [`decodeColor`] 的区别：这里额外允许 `none` / `transparent`
 * 这两种「明确不要底色」的写法，便于在地址里表达
 * 「某个源要卡片底、另一个不要」。
 */
export function decodeSurface(raw: string | null): string | null {
  if (!raw) return null
  const trimmed = raw.trim().toLowerCase()
  if (trimmed === 'none' || trimmed === 'transparent') return 'transparent'
  return decodeColor(raw)
}

/**
 * 解析背景图参数。
 *
 * 只接受 `http(s)://` 或站内相对路径（`/bg/xxx.png`）：
 * OBS 的浏览器源会拦 `file://`，让用户填本地路径只会得到一片空白，
 * 所以这里直接拒绝，避免"看起来配了但没生效"。
 */
export function decodeBgImage(raw: string | null): string | null {
  if (!raw) return null
  const value = raw.trim()
  if (value === '' || value === 'none') return null
  if (/^https?:\/\//i.test(value)) return value
  if (value.startsWith('/')) return value
  return null
}

/**
 * 把「站内相对路径」的背景图补成**绝对地址**。
 *
 * ## 为什么必须补
 * 面板可能从两个来源打开：
 *  - OBS 直连 `http://127.0.0.1:17777/panel`（同源，`/bg/x.jpg` 能取到）；
 *  - 桌面窗口里的内嵌预览 `tauri://localhost/#/panel`（**跨源**）——
 *    这时 `/bg/x.jpg` 会被解析成 `tauri://localhost/bg/x.jpg`，**必然 404**，
 *    用户看到的就是「背景图没生效」。
 *
 * 已经有了绝对地址（`http(s)://`）就原样返回。
 */
export function absoluteBackground(url: string | null, base: string): string | null {
  if (!url) return null
  if (/^https?:\/\//i.test(url)) return url
  if (!base) return url
  return `${base.replace(/\/$/, '')}${url.startsWith('/') ? url : `/${url}`}`
}

/**
 * Vue 组合式函数：从当前 location 解析样式。
 *
 * 内部订阅了 `popstate` / `hashchange`，因此**改了 OBS 浏览器源地址后样式会即时生效**，
 * 不必手动刷新页面。
 */
export function usePanelStyle(
  defaults: () => Partial<PanelStyleConfig> | null | undefined,
): ComputedRef<ResolvedPanelStyle> {
  const search = usePanelSearch()
  return computed(() =>
    resolvePanelStyle(new URLSearchParams(search.value), defaults() ?? null),
  )
}

/**
 * 响应式的「当前专注页」。
 *
 * 与 [`usePanelStyle`] 一样订阅 URL 变化，因此 OBS 里改地址后即时生效。
 */
export function usePanelPage(): ComputedRef<PanelPage> {
  const page = ref(currentPanelPage())
  const sync = () => {
    page.value = currentPanelPage()
  }
  window.addEventListener('popstate', sync)
  window.addEventListener('hashchange', sync)
  onUnmounted(() => {
    window.removeEventListener('popstate', sync)
    window.removeEventListener('hashchange', sync)
  })
  return computed(() => page.value)
}

/** 从当前地址读出专注页。 */
function currentPanelPage(): PanelPage {
  return resolvePanelPage(
    window.location.pathname,
    window.location.hash,
    new URLSearchParams(extractQuery(window.location.search + window.location.hash)),
  )
}

/**
 * 监听 URL 参数变化（阶段 7）。
 *
 * 返回响应式查询串（不含 `?`）。
 */
export function usePanelSearch(): Ref<string> {
  const read = () => extractQuery(window.location.search + window.location.hash)
  const search = ref(read())
  const sync = () => {
    search.value = read()
  }
  window.addEventListener('popstate', sync)
  window.addEventListener('hashchange', sync)
  onUnmounted(() => {
    window.removeEventListener('popstate', sync)
    window.removeEventListener('hashchange', sync)
  })
  return search
}

/** 从 `?a=1` 或 `#/panel?a=1` 里取出查询串（不含 `?`）。 */
export function extractQuery(source: string): string {
  const index = source.indexOf('?')
  return index >= 0 ? source.slice(index + 1) : ''
}

/** 把样式转成内联 CSS 变量，挂在面板根节点上。 */
export function styleToCssVars(style: ResolvedPanelStyle): Record<string, string> {
  // 字体颜色：显式给了就用它，否则跟随主题
  const fg = style.fg ?? (style.theme === 'dark' ? '#f8fafc' : '#0f172a')
  // 背景图补成绝对地址：内嵌预览是跨源的，相对路径会 404
  const bgImage = absoluteBackground(style.bgImage, API_BASE)
  return {
    '--panel-color': style.color,
    '--panel-font-size': `${style.fontSize}px`,
    '--panel-scale': String(style.scale),
    '--panel-fg': fg,
    // 卡片底色 / 进度条轨道：默认 transparent（只要文字与进度条）
    '--panel-surface': style.surface,
    '--panel-track': style.track,
    // 背景图：没有就不设（保持 none），避免生成 `url("")`
    ...(bgImage ? { '--panel-bg-image': `url("${bgImage}")` } : {}),
    '--panel-bg': style.transparent
      ? 'transparent'
      : style.theme === 'dark'
        ? 'rgba(15, 23, 42, 0.82)'
        : 'rgba(248, 250, 252, 0.86)',
    '--panel-sub':
      style.theme === 'dark' ? 'rgba(248, 250, 252, 0.62)' : 'rgba(15, 23, 42, 0.6)',
  }
}

/**
 * 深拷贝一份「纯数据」对象（配置编辑的通用工具）。
 *
 * ## 为什么不用 `structuredClone`
 * 从 Pinia store 里取出的对象是 **Vue 的响应式代理**，WebView2 下
 * `structuredClone` 会直接抛：
 *
 * ```text
 * Failed to execute 'structuredClone' on 'Window': #<Object> could not be cloned.
 * ```
 *
 * 后果很隐蔽：调用点通常写在 `try` 里或者被 `?.` 短路，
 * 界面上表现为**永久「正在加载配置…」**（因为 `draft` 始终是 null）。
 * 这个坑在设置页与 OBS 面板页各踩过一次，所以统一收在这里。
 *
 * 配置本身是纯 JSON 数据（字符串/数字/布尔/数组/对象），
 * JSON 深拷贝完全够用，而且对代理透明。
 */
export function clonePlain<T>(value: T): T {
  return JSON.parse(JSON.stringify(value)) as T
}
