/**
 * 面板 URL 参数解析。
 *
 * ## 现在的模型：地址只写「用哪套样式」
 *
 * ```
 * /panel/play?style=pink               ← 常用形态
 * /panel/lyrics?style=ghost&scale=1.3  ← 只额外覆盖「这个源」的字号倍率
 * ```
 *
 * 外观（主题、颜色、卡片底色、进度条、背景图、字号、条数、布局）全部来自
 * **命名样式**，由 `GET /api/config` 的 `panel_styles` 提供，地址里只引用 id。
 *
 * ## 为什么从「13 个参数」改成这样
 * 早期每个字段都能写在地址里，于是：
 *  - 地址长到 100~135 字符，无法阅读；
 *  - 想整体换配色，得在每个 OBS 浏览器源里重新复制一遍地址；
 *  - 用户分不清「哪些参数是我特意改的、哪些只是默认值被写出来了」。
 *
 * 现在改样式只需在「设置 → OBS 面板」里改一处，所有引用它的源一起变。
 *
 * ## `scale` 为什么留在地址里
 * 它是「这个源的特殊需求」（歌词页放大、队列页不用），而不是「这套配色的
 * 属性」。把它塞进样式会逼用户为同一套配色存两份样式。
 *
 * ## 容错
 * 未知样式 id、非法倍率一律回落到默认样式，保证 OBS 里**永远能出画面**。
 */
import { computed, onUnmounted, ref, type ComputedRef, type Ref } from 'vue'
import { API_BASE } from '@/api'
import type { PanelStyleConfig } from '@/types'

/** 面板实际生效的样式（命名样式 + 地址里的倍率覆盖）。 */
export interface ResolvedPanelStyle {
  /** 生效的样式 id（回落过后的）。 */
  styleId: string
  /** 样式显示名（界面提示用）。 */
  styleName: string
  theme: 'dark' | 'light'
  /** 是否透明背景（OBS 勾选透明背景时使用）。 */
  transparent: boolean
  /** 主色（歌曲名、队列高亮、歌词当前行等强调色）。 */
  color: string
  /**
   * 进度条填充色。**空字符串 = 跟随 [`color`]**。
   *
   * 拆出来的原因：早期 `color` 一个变量管 10 处（含进度条），
   * 用户「只想改进度条颜色」却发现整个面板都变色了。
   */
  barColor: string
  /** 字体颜色（`null` = 跟随主题）。 */
  fg: string | null
  /** 卡片/列表底色（默认全透明）。 */
  surface: string
  /** 进度条轨道颜色（默认全透明）。 */
  track: string
  /** 背景图 URL（默认无）。 */
  bgImage: string | null
  fontSize: number
  /** 字号倍率（**只来自地址**，每个源可以不同）。 */
  scale: number
  limit: number
  showLyrics: boolean
  /** 歌名单独颜色（`null` = 跟随 `color`）。 */
  titleColor: string | null
  /** 正文字重。 */
  fontWeight: number
  /** 次级说明字重（「点歌人」「队列为空」这类小字）。 */
  fontWeightSub: number
  /** 歌名字重。 */
  fontWeightTitle: number
  /** 文字描边宽度（px）；`0` = 不描边。 */
  textStrokeWidth: number
  /** 文字描边颜色（`null` = 按字体色自动选）。 */
  textStrokeColor: string | null
  /**
   * 面板布局。
   *
   * - `list`    竖向堆叠（默认，最紧凑）
   * - `compact` 精简单行，隐藏歌手与点歌人
   * - `lyrics`  以歌词为主
   * - `wide`    宽版：左侧歌曲/点歌队列，右侧歌词
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
 * - `all`     综合面板（默认）
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

/** 出厂默认样式（后端 `PanelStyleConfig::default()` 的镜像）。 */
export const FALLBACK_STYLE: PanelStyleConfig = {
  id: 'pink',
  name: '粉白',
  theme: 'light',
  bg: 'transparent',
  color: '#ff6fa5',
  bar_color: '',
  fg: null,
  surface: 'transparent',
  track: 'transparent',
  bg_image: null,
  font_size: 16,
  scale: 1,
  limit: 8,
  show_lyrics: true,
  title_color: null,
  font_weight: 600,
  font_weight_sub: 400,
  font_weight_title: 700,
  text_stroke_width: 0,
  text_stroke_color: null,
  layout: 'list',
}

/** 把一套命名样式转成「生效样式」。 */
export function styleToResolved(
  style: PanelStyleConfig,
  scaleOverride?: number | null,
): ResolvedPanelStyle {
  const layout = (PANEL_LAYOUTS as readonly string[]).includes(style.layout)
    ? (style.layout as ResolvedPanelStyle['layout'])
    : 'list'
  return {
    styleId: style.id,
    styleName: style.name || style.id,
    theme: style.theme === 'dark' ? 'dark' : 'light',
    transparent: (style.bg ?? 'transparent') === 'transparent',
    color: style.color,
    barColor: style.bar_color ?? '',
    fg: style.fg,
    surface: style.surface,
    track: style.track,
    bgImage: style.bg_image,
    fontSize: style.font_size,
    // 倍率只来自地址；样式里的 scale 作为「没有地址参数时的默认倍率」
    scale: scaleOverride ?? style.scale ?? 1,
    limit: style.limit,
    showLyrics: style.show_lyrics,
    titleColor: style.title_color ?? null,
    /*
     * 字重与描边都要给**兜底值**。
     *
     * 老配置里没有这些字段（后端 `#[serde(default)]` 会补上默认值，
     * 但前端也可能从别处拿到不完整的对象——例如 `FALLBACK_STYLE`）。
     * 不给兜底会让 `font-weight: undefined` 直接失效、文字粗细全乱。
     */
    fontWeight: clampWeight(style.font_weight, 600),
    fontWeightSub: clampWeight(style.font_weight_sub, 400),
    fontWeightTitle: clampWeight(style.font_weight_title, 700),
    textStrokeWidth: Number.isFinite(style.text_stroke_width)
      ? Math.min(Math.max(style.text_stroke_width, 0), 6)
      : 0,
    textStrokeColor: style.text_stroke_color ?? null,
    layout,
  }
}

/**
 * 把字重限制到 CSS 实际支持的档位。
 *
 * 只接受 100 的整数倍且落在 100~900——中间值浏览器会自己取整，
 * 但显式规范化能让"界面显示的档位"与"实际渲染"完全一致。
 */
function clampWeight(raw: number | undefined, fallback: number): number {
  const n = Number(raw)
  if (!Number.isFinite(n) || n <= 0) return fallback
  const stepped = Math.round(n / 100) * 100
  return Math.min(Math.max(stepped, 100), 900)
}

function parseNumber(raw: string | null, min: number, max: number): number | null {
  if (raw === null || raw.trim() === '') return null
  const n = Number(raw)
  if (!Number.isFinite(n)) return null
  return Math.min(Math.max(n, min), max)
}

function parseEnum<T extends string>(raw: string | null, allowed: readonly T[]): T | null {
  if (!raw) return null
  const v = raw.trim().toLowerCase() as T
  return allowed.includes(v) ? v : null
}

/**
 * 从 URLSearchParams 解析出最终样式。
 *
 * @param params    地址里的查询参数
 * @param styles    可用样式列表（来自 `GET /api/config`）
 * @param fallbackId 默认样式 id（地址没给 `style` 或给的 id 不存在时用它）
 */
export function resolvePanelStyle(
  params: URLSearchParams,
  styles: PanelStyleConfig[] | null | undefined,
  fallbackId?: string | null,
): ResolvedPanelStyle {
  const list = styles?.length ? styles : [FALLBACK_STYLE]

  // 地址指定的样式优先；不存在就回落默认 id；默认 id 也不存在就用第一套。
  const wanted = params.get('style')?.trim()
  const byWanted = wanted ? list.find((s) => s.id === wanted) : undefined
  const byDefault = fallbackId ? list.find((s) => s.id === fallbackId) : undefined
  const style = byWanted ?? byDefault ?? list[0]

  // 倍率只从地址取；没给就沿用样式自己的 scale
  const scale = parseNumber(params.get('scale'), 0.2, 5)

  return styleToResolved(style, scale)
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

/** 把样式转成内联 CSS 变量，挂在面板根节点上。 */
export function styleToCssVars(style: ResolvedPanelStyle): Record<string, string> {
  // 字体颜色：显式给了就用它，否则跟随主题（与桌面端的粉白主题保持一致）
  const fg = style.fg ?? (style.theme === 'dark' ? '#f8fafc' : '#5a4450')
  // 背景图补成绝对地址：内嵌预览是跨源的，相对路径会 404
  const bgImage = absoluteBackground(style.bgImage, API_BASE)

  /*
   * 描边颜色：显式给了就用它，否则**按字体色自动选**——
   * 浅色字配深描边、深色字配浅描边。这样用户只调宽度就能得到可读的效果，
   * 不必自己去想描边该用什么颜色。
   */
  const stroke = style.textStrokeColor ?? (isLight(fg) ? 'rgba(0,0,0,0.55)' : 'rgba(255,255,255,0.65)')
  const strokeW = Math.max(0, style.textStrokeWidth)

  return {
    '--panel-color': style.color,
    // 进度条填充：没单独设就跟随主色（`color` 变量本身不能自引用，所以在这里解析）
    '--panel-bar': style.barColor || style.color,
    // 歌名颜色：没单独设就跟随主色
    '--panel-title': style.titleColor || style.color,
    '--panel-font-size': `${style.fontSize}px`,
    '--panel-scale': String(style.scale),
    '--panel-fg': fg,
    // 三档字重：正文 / 次级说明 / 歌名
    '--panel-fw': String(style.fontWeight),
    '--panel-fw-sub': String(style.fontWeightSub),
    '--panel-fw-title': String(style.fontWeightTitle),
    /*
     * 文字描边。宽度为 0 时给 `0` 而不是省略变量——面板 CSS 里用
     * `-webkit-text-stroke: var(--panel-stroke-w) var(--panel-stroke)`
     * 统一书写，宽度 0 就等于不描边，省掉一堆分支。
     */
    '--panel-stroke-w': `${strokeW}px`,
    '--panel-stroke': stroke,
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
    /*
     * 次要文字色（歌手、点歌人、时间、队列条目、歌词非当前行…）。
     *
     * ⚠️ 早期这里**只按主题推导**，完全无视用户设的 `fg`，于是
     * 「字体色」这个设置实测几乎不生效：把 fg 从 #000000 改成 #ffffff，
     * 面板上除标题外的文字颜色纹丝不动。
     * 现在用 color-mix 从 `fg` 派生：保留「次要文字更淡」的层次，
     * 同时让字体色真正作用到所有正文上。
     */
    '--panel-sub': `color-mix(in srgb, ${fg} 62%, transparent)`,
  }
}

/**
 * 判断一个颜色是否偏亮。
 *
 * 只处理十六进制（面板里用户能输入的颜色都是取色器给的 `#rrggbb`）；
 * 其它形式（`rgba(...)`、颜色关键字）一律当作"深色"处理——宁可给浅描边，
 * 也不要给一个和背景同色的描边导致完全看不见。
 */
function isLight(color: string): boolean {
  const m = /^#?([0-9a-f]{6})$/i.exec(color.trim())
  if (!m) return false
  const n = parseInt(m[1], 16)
  const r = (n >> 16) & 0xff
  const g = (n >> 8) & 0xff
  const b = n & 0xff
  // 感知亮度（Rec. 601），阈值取 0.6 偏向"浅色字"判定，符合深色面板更常见的场景
  return (0.299 * r + 0.587 * g + 0.114 * b) / 255 > 0.6
}

/** 从 `?a=1` 或 `#/panel?a=1` 里取出查询串（不含 `?`）。 */
export function extractQuery(source: string): string {
  const index = source.indexOf('?')
  return index >= 0 ? source.slice(index + 1) : ''
}

function readSearch(): string {
  if (typeof window === 'undefined') return ''
  const fromHash = extractQuery(window.location.hash)
  return fromHash || extractQuery(window.location.search)
}

/** 响应式的查询串（订阅 `popstate` / `hashchange`，OBS 改地址即时生效）。 */
export function usePanelSearch(): Ref<string> {
  const search = ref(readSearch())
  if (typeof window !== 'undefined') {
    const sync = (): void => {
      search.value = readSearch()
    }
    window.addEventListener('popstate', sync)
    window.addEventListener('hashchange', sync)
    onUnmounted(() => {
      window.removeEventListener('popstate', sync)
      window.removeEventListener('hashchange', sync)
    })
  }
  return search
}

/**
 * Vue 组合式函数：从当前 location 解析样式。
 *
 * 内部订阅了 `popstate` / `hashchange`，因此**改了 OBS 浏览器源地址后样式会即时生效**，
 * 不必手动刷新页面。
 */
export function usePanelStyle(
  styles: () => PanelStyleConfig[] | null | undefined,
  fallbackId: () => string | null | undefined,
): ComputedRef<ResolvedPanelStyle> {
  const search = usePanelSearch()
  return computed(() =>
    resolvePanelStyle(new URLSearchParams(search.value), styles(), fallbackId()),
  )
}

/**
 * 响应式的「当前专注页」。
 *
 * 与 [`usePanelStyle`] 一样订阅 URL 变化，因此 OBS 里改地址后即时生效。
 */
export function usePanelPage(): ComputedRef<PanelPage> {
  const search = usePanelSearch()
  return computed(() => {
    if (typeof window === 'undefined') return 'all'
    return resolvePanelPage(
      window.location.pathname,
      window.location.hash,
      new URLSearchParams(search.value),
    )
  })
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
 * 界面上表现为**永久「正在加载配置…」**（因为 `draft` 始终为 null）。
 * 这个坑在设置页与 OBS 面板页各踩过一次，所以统一收在这里。
 *
 * 配置本身是纯 JSON 数据（字符串/数字/布尔/数组/对象），
 * JSON 深拷贝完全够用，而且对代理透明。
 */
export function clonePlain<T>(value: T): T {
  return JSON.parse(JSON.stringify(value)) as T
}
