/**
 * 软件界面（桌面窗口 / 浏览器控制台）的主题配色。
 *
 * ## 只有两个可调颜色
 * 需求是"不需要添加其他功能，只要能改这两颜色就行"：
 *  - `accent` 主题色（强调色）
 *  - `bg` 界面背景色
 *
 * ## 其余颜色是**推导**出来的，不是让用户去调
 * `base.css` 里原本有十个 `--bsr-*` 变量，其中四个是为粉白配色**手工调过**
 * 的固定值。只给用户两个颜色的话，必须自动推导其余部分，否则暗色背景会
 * 立刻出现"深色字压深色底"：
 *
 * | 变量 | 推导方式 |
 * |---|---|
 * | `--bsr-bg` | 用户选的颜色 |
 * | `--bsr-accent` | 用户选的颜色 |
 * | `--bsr-accent-soft` | 主题色 14% 透明（选中态的浅底） |
 * | `--bsr-border` | 背景色与主题色 22% 混合 |
 * | `--bsr-bg-elevated` | 背景偏白/偏黑一点点（卡片底） |
 * | `--bsr-fg` | **按背景亮度取深浅**：浅底配深字、深底配浅字 |
 * | `--bsr-muted` | `--bsr-fg` 淡化到 62% |
 *
 * 文字色按亮度自动取，是这里最关键的一条——它保证无论用户选多深/多浅的
 * 背景，界面始终可读。
 *
 * ## ⚠️ 只作用于软件界面，不影响 OBS 面板
 * OBS 面板用的是另一套完全独立的 `--panel-*` 变量（随面板样式存储），
 * 所以改这里**不会**改变 OBS 里的显示效果。
 */
import type { UiThemeConfig } from '@/types'

/** 出厂配色（与 `base.css` 的默认值一致）。 */
export const DEFAULT_UI_THEME: UiThemeConfig = {
  accent: '#ff6fa5',
  bg: '#fff5f9',
}

/** 解析后的界面配色（可直接写成 CSS 变量）。 */
export interface ResolvedUiTheme {
  accent: string
  accentSoft: string
  bg: string
  bgElevated: string
  fg: string
  muted: string
  border: string
}

interface Rgb {
  r: number
  g: number
  b: number
}

/** 解析 `#rgb` / `#rrggbb`；失败返回 null。 */
function parseHex(color: string): Rgb | null {
  const hex = (color || '').trim().replace(/^#/, '')
  const full =
    hex.length === 3
      ? hex
          .split('')
          .map((c) => c + c)
          .join('')
      : hex.slice(0, 6)
  if (!/^[0-9a-f]{6}$/i.test(full)) return null
  return {
    r: parseInt(full.slice(0, 2), 16),
    g: parseInt(full.slice(2, 4), 16),
    b: parseInt(full.slice(4, 6), 16),
  }
}

function toHex({ r, g, b }: Rgb): string {
  const h = (v: number) =>
    Math.max(0, Math.min(255, Math.round(v))).toString(16).padStart(2, '0')
  return `#${h(r)}${h(g)}${h(b)}`
}

/** 感知亮度（0=黑，1=白）。用人眼敏感的权重，不是简单平均。 */
function luminance({ r, g, b }: Rgb): number {
  return (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255
}

/** 两色线性混合，`t` = 第二种颜色的占比。 */
function mix(a: Rgb, b: Rgb, t: number): Rgb {
  return {
    r: a.r + (b.r - a.r) * t,
    g: a.g + (b.g - a.g) * t,
    b: a.b + (b.b - a.b) * t,
  }
}

const WHITE: Rgb = { r: 255, g: 255, b: 255 }
const BLACK: Rgb = { r: 0, g: 0, b: 0 }

/**
 * 把两个用户颜色推导成完整的一套界面配色。
 *
 * 任何一项解析失败都回落到出厂值，**绝不产出非法 CSS**（否则整个界面会
 * 失去全部颜色，比配色难看严重得多）。
 */
export function resolveUiTheme(theme: UiThemeConfig | null | undefined): ResolvedUiTheme {
  const accentRgb = parseHex(theme?.accent ?? '') ?? parseHex(DEFAULT_UI_THEME.accent)!
  const bgRgb = parseHex(theme?.bg ?? '') ?? parseHex(DEFAULT_UI_THEME.bg)!

  // 背景越暗，文字越白；用 0.55 作分界，避免中间亮度时两边都不够对比
  const dark = luminance(bgRgb) < 0.55

  const fgRgb = dark ? mix(WHITE, bgRgb, 0.08) : mix(BLACK, bgRgb, 0.28)
  const mutedRgb = mix(fgRgb, bgRgb, 0.38)

  // 卡片底：浅色背景往白走（奶油层次感），深色背景往亮走一点点
  const elevatedRgb = dark ? mix(bgRgb, WHITE, 0.08) : mix(bgRgb, WHITE, 0.72)

  // 边框：背景混进主题色，既能看出边界，又和主题呼应
  const borderRgb = mix(bgRgb, accentRgb, dark ? 0.3 : 0.22)

  return {
    accent: toHex(accentRgb),
    accentSoft: `rgba(${Math.round(accentRgb.r)}, ${Math.round(accentRgb.g)}, ${Math.round(accentRgb.b)}, 0.14)`,
    bg: toHex(bgRgb),
    bgElevated: toHex(elevatedRgb),
    fg: toHex(fgRgb),
    muted: toHex(mutedRgb),
    border: toHex(borderRgb),
  }
}

/**
 * 把配色写成 `:root` 上的 CSS 变量。
 *
 * 只覆盖 `base.css` 里那十来个 `--bsr-*`——各视图都通过 `var()` 引用它们，
 * 所以"所有按钮和界面跟随"是自动成立的，不需要逐个组件改。
 */
export function applyUiTheme(theme: UiThemeConfig | null | undefined): ResolvedUiTheme {
  const resolved = resolveUiTheme(theme)
  if (typeof document === 'undefined') return resolved
  const root = document.documentElement
  root.style.setProperty('--bsr-accent', resolved.accent)
  root.style.setProperty('--bsr-accent-soft', resolved.accentSoft)
  root.style.setProperty('--bsr-bg', resolved.bg)
  root.style.setProperty('--bsr-bg-elevated', resolved.bgElevated)
  root.style.setProperty('--bsr-fg', resolved.fg)
  root.style.setProperty('--bsr-muted', resolved.muted)
  root.style.setProperty('--bsr-border', resolved.border)
  // 让浏览器原生控件（滚动条、下拉箭头、取色器）也跟着切换明暗
  root.style.setProperty('color-scheme', isDark(theme) ? 'dark' : 'light')
  return resolved
}

/** 背景是否偏暗（给 `color-scheme` 用）。 */
export function isDark(theme: UiThemeConfig | null | undefined): boolean {
  const bgRgb = parseHex(theme?.bg ?? '') ?? parseHex(DEFAULT_UI_THEME.bg)!
  return luminance(bgRgb) < 0.55
}
