<script setup lang="ts">
/**
 * OBS 面板配置（设置页的「OBS 面板」子标签）。
 *
 * ## 为什么不再是独立导航项
 * 它本质是**配置**（配一次就不动），却曾经和控制台并列成一级导航——
 * 结果是导航项多、用户一进来就看到一堆配置。现在并进「设置」页，
 * 作为三个子标签之一（基础 / OBS 面板 / 背景图库）。
 *
 * 旧地址 `/panels` 由路由重定向到 `/settings/obs`，书签不会失效。
 *
 * ## 这里改的到底是什么
 * 外观全部来自**命名样式**（`config.panel_styles`），地址里只写
 * `?style=<id>`。改一套样式，所有引用它的 OBS 浏览器源一起生效——
 * 这是与早期「每个源各自带一串 URL 参数」最大的区别。
 */
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { RouterLink, useRoute } from 'vue-router'
import {
  apiUrl,
  createPanelStyle,
  deleteBackground,
  deletePanelStyle,
  downloadBackground,
  getBackgrounds,
  getConfig,
  getPanelStyles,
  renameBackground,
  saveEditedBackground,
  setDefaultPanelStyle,
  updatePanelStyle,
  uploadBackground,
  type BackgroundItem,
} from '@/api'
import { clonePlain } from '@/composables/panelParams'
import BackgroundEditor from '@/components/BackgroundEditor.vue'
import { showMessage } from '@/stores/message'
import { useAppStore } from '@/stores/app'
import type { PanelStyleConfig } from '@/types'

const store = useAppStore()
const route = useRoute()

/**
 * 是否只显示「背景图库」。
 *
 * 这一个组件同时服务两个子标签（`/settings/obs` 与 `/settings/background`）：
 * 两者的数据来源、背景图列表、上传逻辑完全一样，拆成两个组件会重复一大段
 * 逻辑。差别只在**显示哪些区块**，所以用路由判断。
 */
const showBackgroundsOnly = computed(
  () => route.path === '/settings/background' || route.path.startsWith('/settings/background'),
)
/**
 * 重新加载预览 iframe。
 *
 * iframe 的 `:key` 绑在 `previewUrl` 上，所以切换面板类型时会自然重建；
 * 这个函数用于「改了样式但地址没变」时手动刷新。
 */
function reloadPreview(): void {
  previewNonce.value += 1
}

/** 预览刷新计数：只改 key，不影响地址本身。 */
const previewNonce = ref(0)

/** 本地编辑副本（正在编辑的那套样式）。 */
const draft = ref<PanelStyleConfig | null>(null)
const saving = ref(false)
const saved = ref(false)

// ── 样式列表（命名样式）─────────────────────────────────────────────────

/** 全部样式。 */
const styles = ref<PanelStyleConfig[]>([])
/** 默认样式 id（OBS 源没写 `?style=` 时用它）。 */
const defaultStyleId = ref('')
/** 新建/复制样式时的名字输入。 */
const newStyleName = ref('')
/** 新建样式对话框是否打开，以及从哪套复制（`null` = 空白）。 */
const creatingFrom = ref<string | null | undefined>(undefined)
/** 重命名目标。 */
const renamingStyle = ref<PanelStyleConfig | null>(null)
const renameValue = ref('')
/** 删除确认目标。 */
const deletingStyle = ref<PanelStyleConfig | null>(null)
/** 样式操作进行中（禁用按钮，防重复提交）。 */
const styleBusy = ref(false)

/**
 * 载入样式列表。
 *
 * ⚠️ **始终走 `GET /api/panel/styles`**，不读 `store.config` 的缓存。
 *
 * 早期版本优先用 store 里的配置来"省一次请求"，结果是：样式列表只在
 * bootstrap 那一刻正确，之后别处（另一个窗口、接口调用、本页的新建/删除
 * 之前的中间态）改了样式，本页就再也看不到——实测后端已有 2 套，
 * 界面上仍只列出 1 套，且刷新页面也不行（store 一直是旧的）。
 *
 * 样式列表本来就"随时可能变"，一次轻量 GET 换正确性很划算。
 *
 * 只在 `draft` 为空时给它挑初始值，避免重置用户正在编辑的那套
 * （可能含未保存改动）。
 */
async function loadStyles(): Promise<void> {
  const res = await getPanelStyles()
  styles.value = clonePlain(res.styles)
  defaultStyleId.value = res.default_style_id
  if (!draft.value && styles.value.length) {
    const pick =
      styles.value.find((s) => s.id === defaultStyleId.value) ?? styles.value[0]
    draft.value = clonePlain(pick)
  }
}

// ── 样式增删改 ──────────────────────────────────────────────────────────

/** 切换到另一套样式。 */
function selectStyle(style: PanelStyleConfig): void {
  draft.value = clonePlain(style)
}

/** 把后端最新的样式列表同步到本地（编辑/新建/删除后调用）。 */
async function refreshStyles(): Promise<void> {
  const res = await getPanelStyles()
  styles.value = clonePlain(res.styles)
  defaultStyleId.value = res.default_style_id
  // 编辑中的样式若已被后端改变（例如重命名），同步一次显示值
  if (draft.value) {
    const fresh = styles.value.find((s) => s.id === draft.value?.id)
    if (fresh) draft.value = clonePlain(fresh)
  }
}

/** 打开「新建样式」对话框。`from` 给出时从该样式复制，否则用出厂默认。 */
function openCreate(from: string | null): void {
  creatingFrom.value = from
  const base = from ? styles.value.find((s) => s.id === from) : null
  newStyleName.value = base ? `${base.name} 副本` : '新样式'
}

/** 确认新建样式。 */
async function confirmCreate(): Promise<void> {
  const name = newStyleName.value.trim()
  if (!name) return
  styleBusy.value = true
  try {
    const { id } = await createPanelStyle(name, creatingFrom.value ?? undefined)
    await refreshStyles()
    const created = styles.value.find((s) => s.id === id)
    if (created) draft.value = clonePlain(created)
    creatingFrom.value = undefined
    showMessage('ok', `已新建样式「${name}」`)
  } catch (err) {
    showMessage('err', `新建样式失败：${(err as Error).message}`)
  } finally {
    styleBusy.value = false
  }
}

/** 打开重命名对话框。 */
function openRenameStyle(style: PanelStyleConfig): void {
  renamingStyle.value = style
  renameValue.value = style.name
}

/** 确认重命名。 */
async function confirmRenameStyle(): Promise<void> {
  const target = renamingStyle.value
  const name = renameValue.value.trim()
  if (!target || !name) return
  styleBusy.value = true
  try {
    await updatePanelStyle({ ...clonePlain(target), name })
    await refreshStyles()
    renamingStyle.value = null
    showMessage('ok', `已改名为「${name}」`)
  } catch (err) {
    showMessage('err', `重命名失败：${(err as Error).message}`)
  } finally {
    styleBusy.value = false
  }
}

/** 把某套样式设为默认。 */
async function makeDefault(style: PanelStyleConfig): Promise<void> {
  styleBusy.value = true
  try {
    await setDefaultPanelStyle(style.id)
    await refreshStyles()
    showMessage('ok', `「${style.name}」已设为默认样式`)
  } catch (err) {
    showMessage('err', `设置默认样式失败：${(err as Error).message}`)
  } finally {
    styleBusy.value = false
  }
}

/** 确认删除样式。 */
async function confirmDeleteStyle(): Promise<void> {
  const target = deletingStyle.value
  if (!target) return
  styleBusy.value = true
  try {
    const res = await deletePanelStyle(target.id)
    await refreshStyles()
    defaultStyleId.value = res.default_style_id
    // 删掉的正是当前编辑的 → 切到默认样式
    if (draft.value?.id === target.id) {
      const next =
        styles.value.find((s) => s.id === res.default_style_id) ?? styles.value[0]
      draft.value = next ? clonePlain(next) : null
    }
    deletingStyle.value = null
    showMessage('ok', `已删除样式「${target.name}」`)
  } catch (err) {
    showMessage('err', `删除失败：${(err as Error).message}`)
  } finally {
    styleBusy.value = false
  }
}

/** 列表上的配色缩略条：主色 / 进度条色 / 卡片底色三段。 */
function styleSwatch(style: PanelStyleConfig): string {
  const bar = style.bar_color || style.color
  const surface = style.surface === 'transparent' ? '#ffffff' : style.surface
  return `linear-gradient(90deg, ${style.color} 0 34%, ${bar} 34% 67%, ${surface} 67% 100%)`
}

/** 样式摘要（主题 · 背景 · 布局）。 */
function styleSummary(style: PanelStyleConfig): string {
  const theme = style.theme === 'light' ? '浅色' : '深色'
  const bg = style.bg === 'transparent' ? '透明' : '不透明'
  const layouts: Record<string, string> = {
    list: '列表',
    compact: '精简',
    lyrics: '歌词',
    wide: '宽版',
  }
  return `${theme} · ${bg} · ${layouts[style.layout] ?? style.layout}`
}

/** 面板地址支持的参数名。 */
type ParamName =
  | 'fontSize'
  | 'color'
  /** 进度条填充色；`follow` = 跟随主色 */ 
  | 'barColor'
  | 'fg'
  | 'surface'
  | 'track'
  | 'limit'
  | 'scale'
  | 'layout'
  | 'theme'
  | 'bg'
  | 'showLyrics'
  | 'bgImage'

type PanelKey = 'all' | 'play' | 'lyrics' | 'danmaku'

/** 面板清单（启用状态与地址在别处组合）。 */
const PANEL_LIST: Array<{ key: PanelKey; label: string; desc: string }> = [
  { key: 'all', label: '综合面板', desc: '正在播放 + 队列 + 歌词（与最早的行为一致）' },
  { key: 'play', label: '播放与队列', desc: '进度条 + 正在播放 + 点歌队列' },
  { key: 'lyrics', label: '歌词', desc: '只显示歌词，适合单独占一块' },
  { key: 'danmaku', label: '最近弹幕', desc: '只显示最近弹幕' },
]

/** 一行风格预设的取值（只服务于「风格预设」按钮，不参与地址生成）。 */
interface ParamRow {
  /** 行 id（仅用于 v-for key）。 */
  id: number
  name: ParamName
  value: string
}

let paramRowSeq = 0

function makeRow(name: ParamName, value: string): ParamRow {
  paramRowSeq += 1
  return { id: paramRowSeq, name, value }
}

/**
 * 面板样式出厂默认值（「恢复默认外观」用它）。
 *
 * 必须与后端 `PanelStyleConfig::default()` 和 `panelParams.ts` 的
 * `FALLBACK_STYLE` 保持一致，否则「恢复默认」后会得到一套既不是出厂、
 * 也不是用户配置的样式。
 *
 * `id` / `name` 在这里只是占位：恢复默认时**不会**改动当前样式的
 * id 与显示名（那是用户的东西），只重置外观字段。
 */
const DEFAULT_PANEL = {
  id: 'pink',
  name: '粉白',
  theme: 'light' as const,
  bg: 'transparent' as const,
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
  layout: 'list' as const,
}

/*
 * 风格预设用的参数名 → 取值表。
 *
 * ⚠️ 这套东西**只服务于「风格预设」按钮**，不再对应地址参数：
 * 地址里现在只有 `?style=<id>`（见 `urlFor`），外观一律来自样式对象。
 * 之所以保留这个"参数名"的中间层，是因为预设要一次改**多项**，
 * 用一张表描述比写六行赋值清楚；它只是内存里的临时描述，不参与地址生成。
 */
const paramRows = ref<ParamRow[]>([])

/**
 * 每个面板是否启用。
 *
 * 关闭的面板**不出现在地址列表里**，也就不会生成地址——
 * 用户只想要歌词和播放队列时，另外两个干脆不出现，OBS 侧也只需建两个源。
 */
const enabledPanels = ref<Record<PanelKey, boolean>>({
  all: true,
  play: true,
  lyrics: true,
  danmaku: true,
})
const fontScale = ref<Record<string, number>>({
  all: 1,
  play: 1,
  lyrics: 1.55,
  danmaku: 1.5,
})

/**
 * 修改某个专注页的字号倍率。
 *
 * ⚠️ 用 `@input`（不是 `@change`）并且**把输入值写回 state**：
 * 早期模板写的是 `:value="fontScale[p.key]"` + `@change` 里直接赋值，
 * 结果输入框的显示由 `:value` 单向绑定控制，
 * 用户按上下箭头/输入后值会被立刻"重置"回原值，看起来就是**改不动**。
 * 现在改成受控更新：先解析、夹取范围、写回 state，再由 `:value` 反映出来。
 */
function setFontScale(key: string, raw: string): void {
  const parsed = Number(raw)
  if (!Number.isFinite(parsed)) return
  // 夹到合理范围，避免 0 或负数算出非法字号
  fontScale.value[key] = Math.min(5, Math.max(0.2, parsed))
}

/** 配置读取失败原因（给出重试入口，避免无尽「加载中」）。 */
const loadError = ref<string | null>(null)

/**
 * 深拷贝一份配置用于编辑。
 *
 * ⚠️ 用 [`clonePlain`] 而不是 `structuredClone`：`store.config` 里是 Vue 响应式
 * 代理，WebView2 下 `structuredClone` 会抛
 * `#<Object> could not be cloned`，本页曾因此永久停在「正在加载配置…」。
 */
function clonePanel(cfg: PanelStyleConfig): PanelStyleConfig {
  return clonePlain(cfg)
}

function resetDraft(): void {
  const cfg = store.config
  if (!cfg) return
  // 编辑对象 = 默认样式（多套样式的完整管理界面见后续改造）
  const style =
    cfg.panel_styles.find((s) => s.id === cfg.default_style_id) ?? cfg.panel_styles[0]
  if (!style) return
  draft.value = clonePanel(style)
}

/**
 * 拿到要编辑的面板样式。
 *
 * ⚠️ 两条路径都要有：
 *  1. `store.config` 已就绪时直接用它（正常情况，省一次请求）；
 *  2. 否则**自己发一次请求**——bootstrap 在 `App.vue` 的 `onMounted` 发起，
 *     本页可能先于它完成挂载；早期版本只读一次 store，结果永久停在
 *     「正在加载配置…」，和设置页踩过的是同一个坑。
 */
async function ensureDraft(): Promise<void> {
  loadError.value = null
  try {
    /*
     * 每次都重新拉样式列表，**不要**因为 `draft` 已存在就早退。
     *
     * 早期这里写的是 `if (draft.value) return`，后果很隐蔽：别处（或另一个
     * 窗口/接口调用）新增了样式后，本页永远看不到——实测后端已有 2 套，
     * 界面上仍只列出 1 套。样式列表本来就是"随时可能变"的东西，值得每次刷。
     */
    await loadStyles()
    if (!draft.value) {
      loadError.value = '配置里没有任何面板样式'
    }
  } catch (err) {
    loadError.value = (err as Error).message
  }
}

// 立即尝试一次；`store.config` 之后到位时也能补上
watch(() => store.config, ensureDraft, { immediate: true })

/** 默认样式是否已就绪。 */
const ready = computed(() => draft.value !== null)

// ── 颜色编辑 ─────────────────────────────────────────────────────────────

/** 主题对应的默认字体色（`fg` 为空时用它给取色器一个初值）。 */
const themeFg = computed(() => (draft.value?.theme === 'light' ? '#5a4450' : '#f2f2f2'))

/** 可上色的五项：主色、进度条、字体色、卡片底色、进度条轨道。 */
type ColorField = 'color' | 'barColor' | 'fg' | 'surface' | 'track'

/**
 * 读一个颜色项当前的值（**直接读样式对象**）。
 *
 * ## 为什么不再看「参数行」
 * 早期外观既能写在样式里、也能写在地址参数里，这里因此做成"参数优先"。
 * 但命名样式上线后，地址里**只写 `?style=<id>`**，不再携带任何外观参数——
 * 参数行已经和地址生成脱节。继续让它优先会造成实测到的那种明显不一致：
 * 列表色块与预览是绿的（来自样式），取色器却显示粉色（来自参数行残留值）。
 *
 * 所以现在唯一真相就是 `draft`（当前选中的样式）。
 */
function colorValue(field: ColorField): string {
  const cfg = draft.value
  if (!cfg) return '#000000'
  switch (field) {
    case 'color':
      return cfg.color
    case 'barColor':
      // 空 = 跟随主色；取色器上直接显示主色，符合"看起来会是什么样"
      return cfg.bar_color || cfg.color
    case 'fg':
      return cfg.fg ?? themeFg.value
    case 'surface':
      return cfg.surface === 'transparent' ? '#000000' : cfg.surface
    case 'track':
      return cfg.track === 'transparent' ? '#000000' : cfg.track
  }
}

/** 该项是否偏离了出厂默认值（界面上标出可单独恢复）。 */
function isColorSet(field: ColorField): boolean {
  const cfg = draft.value
  if (!cfg) return false
  switch (field) {
    case 'color':
      return cfg.color !== DEFAULT_PANEL.color
    case 'barColor':
      // 空 = 跟随主色，也就是"没单独设过"
      return Boolean(cfg.bar_color)
    case 'fg':
      return cfg.fg !== null
    case 'surface':
      return cfg.surface !== DEFAULT_PANEL.surface
    case 'track':
      return cfg.track !== DEFAULT_PANEL.track
  }
}

/** 把颜色写进当前样式。 */
function setColor(field: ColorField, value: string): void {
  const cfg = draft.value
  if (!cfg) return
  switch (field) {
    case 'color':
      cfg.color = value
      break
    case 'barColor':
      cfg.bar_color = value
      break
    case 'fg':
      cfg.fg = value
      break
    case 'surface':
      cfg.surface = value
      break
    case 'track':
      cfg.track = value
      break
  }
}

/**
 * 把某项恢复成出厂默认。
 *
 * 进度条色恢复成**空字符串** = 「跟随主色」，而不是一个固定颜色——
 * 这样只调主色就能整体协调，符合「拆成两个」的初衷。
 */
function clearColor(field: ColorField): void {
  const cfg = draft.value
  if (!cfg) return
  switch (field) {
    case 'color':
      cfg.color = DEFAULT_PANEL.color
      break
    case 'barColor':
      cfg.bar_color = ''
      break
    case 'fg':
      cfg.fg = null
      break
    case 'surface':
      cfg.surface = DEFAULT_PANEL.surface
      break
    case 'track':
      cfg.track = DEFAULT_PANEL.track
      break
  }
}

/** 一键把所有底色清掉：只留文字与进度条颜色。 */
function makeAllTransparent(): void {
  if (!draft.value) return
  setColor('surface', 'transparent')
  setColor('track', 'transparent')
}

// ── 一键套用风格预设 ─────────────────────────────────────────────────────

/**
 * 风格预设。
 *
 * 目的：把「主题 + 背景 + 底色 + 轨道 + 字体色 + 进度条」这六项**搭配好**
 * 一次套用，而不是让用户在参数行里逐项试。预设只写地址参数，
 * 不动默认样式（默认样式要靠「保存默认样式」按钮才落盘）。
 */
interface PanelPreset {
  key: string
  label: string
  desc: string
  /** 地址参数：值 `null` = 移除该参数（回到默认）。 */
  params: Partial<Record<ParamName, string | null>>
}

const PRESETS: PanelPreset[] = [
  {
    key: 'pink',
    label: '粉白',
    desc: '粉色调，适合浅色画面',
    params: {
      theme: 'light',
      bg: 'solid',
      color: '#ff6fa5',
      barColor: null,
      fg: '#5a4450',
      surface: '#ffffffb3',
      track: '#ffd9e6',
    },
  },
  {
    key: 'dark',
    label: '深色',
    desc: '白字 + 半透明黑底，浅背景上也能看清',
    params: {
      theme: 'dark',
      bg: 'solid',
      color: '#ff9ec4',
      barColor: null,
      fg: '#f8fafc',
      surface: '#00000073',
      track: '#ffffff33',
    },
  },
  {
    key: 'plain',
    label: '纯文字',
    desc: '全透明，只留文字与进度条',
    params: {
      theme: 'dark',
      bg: 'transparent',
      color: '#ff6fa5',
      barColor: null,
      fg: '#f8fafc',
      surface: 'transparent',
      track: 'transparent',
    },
  },
]

/** 当前套用的是哪个预设（参数与预设完全一致时高亮）。 */
const activePreset = computed(() => {
  const current = new Map(paramRows.value.map((r) => [r.name, r.value.trim()]))
  return (
    PRESETS.find((p) =>
      Object.entries(p.params).every(([name, value]) => {
        const cur = current.get(name as ParamName) ?? ''
        if (value === null) return cur === '' || cur === '__default__'
        return cur === value
      }),
    )?.key ?? null
  )
})

/** 套用预设：把这些参数写进地址参数行（`null` 表示移除）。 */
function applyPreset(preset: PanelPreset): void {
  for (const [name, value] of Object.entries(preset.params)) {
    const paramName = name as ParamName
    const rows = paramRows.value.filter((r) => r.name === paramName)
    if (value === null) {
      paramRows.value = paramRows.value.filter((r) => r.name !== paramName)
      continue
    }
    if (rows.length) {
      rows[0].value = value
    } else {
      paramRows.value.push(makeRow(paramName, value))
    }
  }
}

/** 最近一次套用的预设提示（1.5 秒后消失）。 */
const presetHint = ref('')
let presetTimer: ReturnType<typeof setTimeout> | null = null
function applyPresetWithHint(preset: PanelPreset): void {
  applyPreset(preset)
  presetHint.value = `已套用「${preset.label}」`
  if (presetTimer) clearTimeout(presetTimer)
  presetTimer = setTimeout(() => (presetHint.value = ''), 1500)
}

/** 一键恢复出厂样式：清空所有地址参数 + 默认样式回到出厂值。 */
function resetAllStyles(): void {
  paramRows.value = []
  if (draft.value) {
    // 背景图是用户上传的资产，不该被「恢复默认」清掉
    draft.value = { ...DEFAULT_PANEL, bg_image: draft.value.bg_image }
  }
  presetHint.value = '已恢复默认样式'
  if (presetTimer) clearTimeout(presetTimer)
  presetTimer = setTimeout(() => (presetHint.value = ''), 1500)
}

/** 颜色项的界面元数据（顺序即界面顺序：最常调的排前面）。 */
const COLOR_ITEMS: Array<{ field: ColorField; label: string; hint: string }> = [
  { field: 'color', label: '主色', hint: '歌曲名、队列高亮、歌词当前行' },
  { field: 'barColor', label: '进度条色', hint: '留空即跟随主色' },
  { field: 'fg', label: '字体色', hint: '歌手、点歌人、时间等正文' },
  { field: 'surface', label: '卡片底色', hint: 'transparent = 不要底色' },
  { field: 'track', label: '进度条轨道', hint: 'transparent = 不要轨道' },
]

/**
 * 把「布局与尺寸」里的下拉/输入同步到地址参数，同时更新默认样式草稿。
 *
 * 与 [`setColor`] 的差别：这几个不是颜色，但同样需要「地址 + 默认样式」双写，
 * 否则用户在界面上改了字号却发现地址里没变（早期就是这样，只能去参数行手改）。
 */
function syncStyleParam(name: ParamName, value: string): void {
  const rows = paramRows.value.filter((r) => r.name === name)
  if (rows.length) {
    rows[0].value = value
  } else {
    paramRows.value.push(makeRow(name, value))
  }
}

// ── 字体色对比度提示 ─────────────────────────────────────────────────────

/** 把 `#rgb` / `#rrggbb` 解析成 0~1 的亮度；解析不了返回 null。 */
function luminance(color: string): number | null {
  const hex = color.trim().replace(/^#/, '')
  const full =
    hex.length === 3
      ? hex
          .split('')
          .map((c) => c + c)
          .join('')
      : hex.slice(0, 6)
  if (!/^[0-9a-f]{6}$/i.test(full)) return null
  const r = parseInt(full.slice(0, 2), 16) / 255
  const g = parseInt(full.slice(2, 4), 16) / 255
  const b = parseInt(full.slice(4, 6), 16) / 255
  return 0.2126 * r + 0.7152 * g + 0.0722 * b
}

/**
 * 字体色与主题是否**明显冲突**（深色主题配深字、浅色主题配浅字）。
 *
 * 实测踩过：theme=dark + fg=#000000 时，面板上除歌曲名外的文字几乎看不见，
 * 而用户很难意识到是「主题和字体色不搭」。这里给一条明确提示。
 */
const fgConflict = computed(() => {
  if (!draft.value || !draft.value.fg) return false
  // 背景是透明时按主题判断：深色主题意味着画面底下大概率是暗的
  const lum = luminance(draft.value.fg)
  if (lum === null) return false
  const dark = draft.value.theme === 'dark'
  return dark ? lum < 0.35 : lum > 0.7
})

// ── 每个面板的字号倍率 ───────────────────────────────────────────────────

// ── 背景图（阶段 9）──────────────────────────────────────────────────────

/** 手动填写的图片地址。 */
const bgImageInput = ref('')
/** 上传/应用的结果提示。 */

/** 已上传的图片列表（下拉框用）。 */
const backgrounds = ref<BackgroundItem[]>([])
const backgroundsLoading = ref(false)

/**
 * 是否把背景图写进生成的面板地址。
 *
 * 默认开启：用户上传/选中背景图后，期望「地址里就带着它」，
 * 而不是再去「地址参数」里手动加一条 `bgImage`——
 * 早期就是这样，导致**选了背景图但地址里没有、面板上看不到效果**。
 */
const includeBgInUrls = ref(true)

/** 把 `/bg/x.jpg` 这类相对地址补成绝对地址（预览用，避免跨源 404）。 */
function absoluteBg(url: string | null): string {
  if (!url) return ''
  if (/^https?:\/\//i.test(url)) return url
  // `apiUrl('')` 在 OBS 直连时是空串（同源），所以只做最小拼接，
  // 并且**去掉重复斜杠**——否则会出现 `127.0.0.1:17777//bg/...`
  const base = apiUrl('').replace(/\/$/, '')
  const path = url.startsWith('/') ? url : `/${url}`
  return `${base}${path}`
}

/** 人类可读的文件大小。 */
function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`
}

/** 读取已上传的图片列表。 */
async function loadBackgrounds(): Promise<void> {
  backgroundsLoading.value = true
  try {
    backgrounds.value = await getBackgrounds()
  } catch {
    backgrounds.value = []
  } finally {
    backgroundsLoading.value = false
  }
}

// ── 已上传图片的管理（重命名 / 下载 / 删除）──────────────────────────────

const bgBusy = ref(false)

// ── 图片预览遮罩层 ──────────────────────────────────────────────────────

/** 当前正在预览的图片（`null` = 未开启）。 */
const preview = ref<BackgroundItem | null>(null)
/** 缩放倍率（1 = 原始大小）。 */
const previewZoom = ref(1)

/** 当前预览图在列表里的下标（用于左右切换与 "n / m" 显示）。 */
const previewIndex = computed(() =>
  preview.value ? backgrounds.value.findIndex((b) => b.url === preview.value?.url) : -1,
)

/** 打开某张图的预览。 */
function openPreview(bg: BackgroundItem): void {
  preview.value = bg
  previewZoom.value = 1
}

// ── 背景图编辑器（裁剪 / 旋转 / 缩放 / 镜像）────────────────────────────

/** 正在编辑的图片；`null` = 编辑器关闭。 */
const editing = ref<BackgroundItem | null>(null)
/** 编辑结果上传中。 */
const savingEdit = ref(false)

/** 打开编辑器。 */
function openEditor(bg: BackgroundItem): void {
  editing.value = bg
}

/** 关闭编辑器。 */
function closeEditor(): void {
  editing.value = null
}

/**
 * 保存编辑结果（另存为新图）。
 *
 * 后端会生成 `原名-编辑.png` 并**回避重名**，所以原图一定还在。
 * 保存成功后自动把新图设为当前样式的背景图——用户点「另存」的意图
 * 通常就是"用这张改好的"，再让他手动选一次没必要。
 */
async function saveEdited(blob: Blob): Promise<void> {
  const src = editing.value
  if (!src) return
  savingEdit.value = true
  try {
    const { url, name } = await saveEditedBackground(blob, src.name)
    backgrounds.value = await getBackgrounds()
    if (draft.value) draft.value.bg_image = url
    // 提示里带上大小：PNG 无损，大图可能几 MB，让用户心里有数
    const mb = (blob.size / 1024 / 1024).toFixed(2)
    showMessage('ok', `已另存为 ${name}（${mb} MB），并设为当前样式的背景图`)
    closeEditor()
  } catch (err) {
    showMessage('err', `保存编辑结果失败：${(err as Error).message}`)
  } finally {
    savingEdit.value = false
  }
}

/** 把某张图设为当前样式的背景图。 */
function pickBackground(bg: BackgroundItem): void {
  if (!draft.value) return
  draft.value.bg_image = bg.url
  showMessage('ok', `已选择 ${bg.name}，记得保存样式`)
}

/** 关闭预览。 */
function closePreview(): void {
  preview.value = null
  previewZoom.value = 1
}

/** 左右切换上/下一张（循环）。 */
function stepPreview(direction: number): void {
  const list = backgrounds.value
  if (!preview.value || list.length < 2) return
  const next = (previewIndex.value + direction + list.length) % list.length
  preview.value = list[next]
  previewZoom.value = 1
}

/** 调整缩放（限制在 10%–800%，避免缩到看不见或放太大卡顿）。 */
function zoomPreview(delta: number): void {
  const next = Math.round((previewZoom.value + delta) * 100) / 100
  previewZoom.value = Math.min(8, Math.max(0.1, next))
}

/** 滚轮缩放：向上放大、向下缩小。 */
function onPreviewWheel(event: WheelEvent): void {
  zoomPreview(event.deltaY < 0 ? 0.15 : -0.15)
}

/** 键盘：Esc 关闭、← → 切图。 */
function onPreviewKey(event: KeyboardEvent): void {
  if (!preview.value) return
  if (event.key === 'Escape') closePreview()
  else if (event.key === 'ArrowLeft') stepPreview(-1)
  else if (event.key === 'ArrowRight') stepPreview(1)
}

onMounted(() => window.addEventListener('keydown', onPreviewKey))
onUnmounted(() => window.removeEventListener('keydown', onPreviewKey))

// ── 页内确认 / 输入弹层（替代 window.prompt / window.confirm）───────────
//
// 系统弹窗会阻塞整个界面；这里改成页内弹层，与预览遮罩同一套视觉。

/** 待确认删除的图片。 */
const pendingDelete = ref<BackgroundItem | null>(null)
/** 重命名弹层状态（`null` = 未打开）。 */
const renameTarget = ref<BackgroundItem | null>(null)
const renameInput = ref('')

/** 打开重命名弹层（扩展名沿用原图，输入框只编辑基础名）。 */
function openRename(bg: BackgroundItem): void {
  renameTarget.value = bg
  renameInput.value = bg.name.replace(/\.[^.]+$/, '')
}

/** 确认重命名。 */
async function confirmRename(): Promise<void> {
  const bg = renameTarget.value
  if (!bg) return
  const base = bg.name.replace(/\.[^.]+$/, '')
  const next = renameInput.value.trim()
  if (!next || next === base) {
    renameTarget.value = null
    return
  }
  bgBusy.value = true
  try {
    backgrounds.value = await renameBackground(bg.name, next)
    // 若默认样式正引用它，后端已同步改好地址；这里刷新一份，避免界面还显示旧路径
    await refreshPanelConfig()
    showMessage('ok', `已重命名为 ${next}.${bg.name.split('.').pop() ?? ''}`)
  } catch (err) {
    showMessage('err', `重命名失败：${(err as Error).message}`)
  } finally {
    bgBusy.value = false
    renameTarget.value = null
  }
}

/** 重新拉取配置并只更新面板样式部分（`draft` 只装 panel）。 */
async function refreshPanelConfig(): Promise<void> {
  try {
    const cfg = await getConfig()
    store.config = clonePlain(cfg)
    // 重命名/删除背景图后，后端可能已同步改过样式里的 bg_image，
    // 这里重新取默认样式，避免界面还显示旧路径
    const style =
      cfg.panel_styles.find((s) => s.id === cfg.default_style_id) ?? cfg.panel_styles[0]
    if (style) draft.value = clonePlain(style)
  } catch {
    // 拉取失败不影响主流程，界面保留旧值
  }
}

/** 下载到本地（换机器/重装后可以直接再上传）。 */
function downloadBg(bg: BackgroundItem): void {
  downloadBackground(bg.url, bg.name)
  showMessage('ok', `已开始下载 ${bg.name}`)
}

/** 删除：先弹页内确认，确认后若默认样式正引用它，后端会一并清空引用。 */
async function confirmDelete(): Promise<void> {
  const bg = pendingDelete.value
  if (!bg) return
  bgBusy.value = true
  try {
    backgrounds.value = await deleteBackground(bg.name)
    await refreshPanelConfig()
    showMessage('ok', `已删除 ${bg.name}`)
  } catch (err) {
    showMessage('err', `删除失败：${(err as Error).message}`)
  } finally {
    bgBusy.value = false
    pendingDelete.value = null
  }
}

/** 从下拉里选了一张已有图片。 */
function onPickExisting(): void {
  showMessage('ok', draft.value?.bg_image ? '已选择，记得保存默认样式' : '已取消背景图（保存后生效）')
}

onMounted(() => {
  void loadBackgrounds()
})

/** 把手动填写的地址套到默认样式上。 */
function applyBgImageInput(): void {
  if (!draft.value) return
  const value = bgImageInput.value.trim()
  if (!value) {
    draft.value.bg_image = null
    showMessage('ok', '已清除背景图')
    return
  }
  if (!/^https?:\/\//i.test(value) && !value.startsWith('/')) {
    // 明确拒绝本地路径：OBS 会拦 file://，配了也不会生效
    showMessage('err', '只接受 http(s):// 或站内路径（如 /bg/xxx.png）')
    return
  }
  draft.value.bg_image = value
  showMessage('ok', '已应用，记得保存默认样式')
}

function clearBackground(): void {
  if (!draft.value) return
  draft.value.bg_image = null
  bgImageInput.value = ''
  showMessage('ok', '已清除背景图（保存后生效）')
}

/**
 * 上传背景图。
 *
 * 走 `POST /api/panel/background`：图片存进程序配置目录，
 * 返回可由 OBS 访问的 `/bg/<name>` 地址。
 */
async function onUploadBackground(event: Event): Promise<void> {
  const input = event.target as HTMLInputElement
  const file = input.files?.[0]
  if (!file) return
  showMessage('info', '上传中…')
  try {
    const url = await uploadBackground(file)
    if (draft.value) draft.value.bg_image = url
    showMessage('ok', `上传成功：${url}`)
    // 列表里立刻能看到刚传的这张
    await loadBackgrounds()
  } catch (err) {
    showMessage('err', `上传失败：${(err as Error).message}`)
  } finally {
    // 允许重复选择同一个文件
    input.value = ''
  }
}

/**
 * 保存当前编辑的样式。
 *
 * 用专门的 `PUT /api/panel/styles`（只改这一套），而不是 `PUT /api/config`
 * 整份配置——后者会把本地草稿里的**所有**样式一起覆盖，若另一个窗口同时
 * 改过别的样式就会被静默回滚。
 */
async function save(): Promise<void> {
  if (!draft.value) return
  saving.value = true
  try {
    await updatePanelStyle(clonePlain(draft.value))
    await refreshStyles()
    saved.value = true
    showMessage('ok', `样式「${draft.value.name}」已保存`)
  } catch (err) {
    showMessage('err', `保存失败：${(err as Error).message}`)
    saved.value = false
  } finally {
    saving.value = false
    window.setTimeout(() => (saved.value = false), 1500)
  }
}

/**
 * 生成某个专注页的地址。
 *
 * ## 只有两个参数
 *  - `style=<id>`：用哪套命名样式（外观全在样式里，不再逐个写参数）
 *  - `scale=<倍率>`：**可选的**字号倍率，只影响这一个源
 *
 * 早期这里会把样式字段全拼进地址（100~135 字符），用户既读不懂、也没法
 * 整体换配色（得逐源重复制）。现在改样式只需在样式编辑器里改一处。
 */
function urlFor(page: PanelKey): string {
  const cfg = draft.value
  if (!cfg) return ''
  const params = new URLSearchParams()
  params.set('style', cfg.id)

  // 字号倍率：仅当不是 1 时写进地址，避免无意义的 `scale=1` 把地址弄长
  const multiplier = fontScale.value[page] ?? 1
  if (Number.isFinite(multiplier) && multiplier !== 1) {
    params.set('scale', String(multiplier))
  }

  const path = page === 'all' ? '/panel' : `/panel/${page}`
  return `${apiUrl(path)}?${params.toString()}`
}

/** 四个面板，附带用途说明。 */
const pages = computed(() =>
  PANEL_LIST.filter((p) => enabledPanels.value[p.key] !== false).map((p) => ({
    ...p,
    url: urlFor(p.key),
  })),
)

/**
 * 当前预览的面板。
 *
 * 关闭某个面板时（`enabled` 变 false）要自动切到第一个仍启用的，
 * 否则预览会停在一个已被隐藏的面板地址上。
 */
const previewKey = ref<PanelKey>('play')
const previewUrl = computed(() => {
  const list = pages.value
  if (!list.length) return ''
  const found = list.find((p) => p.key === previewKey.value)
  return (found ?? list[0]).url
})

/** 预览标题（跟随实际预览的面板，而不是用户点过但已关闭的那个）。 */
const previewLabel = computed(() => {
  const list = pages.value
  if (!list.length) return '（未启用任何面板）'
  const found = list.find((p) => p.key === previewKey.value)
  return (found ?? list[0]).label
})

/** 关闭中的面板被预览时，自动切走。 */
watch(pages, (list) => {
  if (!list.length) return
  if (!list.some((p) => p.key === previewKey.value)) {
    previewKey.value = list[0].key
  }
})

const copiedKey = ref<string | null>(null)
async function copyUrl(url: string, key: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(url)
    copiedKey.value = key
    window.setTimeout(() => {
      if (copiedKey.value === key) copiedKey.value = null
    }, 1500)
  } catch {
    copiedKey.value = null
  }
}

/** 一次性复制全部地址（OBS 里要建多个源，逐条复制很烦）。 */
async function copyAll(): Promise<void> {
  const text = pages.value.map((p) => `${p.label}\t${p.url}`).join('\n')
  try {
    await navigator.clipboard.writeText(text)
    copiedKey.value = '__all__'
    window.setTimeout(() => {
      if (copiedKey.value === '__all__') copiedKey.value = null
    }, 1500)
  } catch {
    copiedKey.value = null
  }
}
</script>

<template>
  <div class="panels-page">
    <!--
      设置页的子标签导航。
      「背景图库」指向同一组件的另一个路由（/settings/background），
      由 `route.path` 决定是否只显示背景图部分——见下方 `showBackgroundsOnly`。
      「基础」暂时落在控制台的设置标签（避免出现两个改配置的入口）。
    -->
    <nav class="sub-tabs">
      <RouterLink to="/dashboard">基础</RouterLink>
      <RouterLink to="/settings/obs">OBS 面板</RouterLink>
      <RouterLink to="/settings/background">背景图库</RouterLink>
    </nav>

    <div class="page-head">
      <div>
        <h2>{{ showBackgroundsOnly ? '背景图库' : 'OBS 面板样式与地址' }}</h2>
      </div>
      <div v-if="!showBackgroundsOnly" class="controls">
        <button class="ghost" :disabled="!ready" @click="resetDraft()">重置</button>
        <button :disabled="!ready || saving" @click="save()">
          {{ saving ? '保存中…' : saved ? '已保存' : '保存样式' }}
        </button>
      </div>
    </div>

    <p v-if="store.error" class="err">{{ store.error }}</p>
    <div v-if="loadError" class="empty">
      <p class="err">读取面板配置失败：{{ loadError }}</p>
      <button class="ghost" @click="ensureDraft()">重试</button>
    </div>
    <p v-else-if="!ready" class="empty">正在加载配置…</p>

    <template v-if="ready && draft">
      <!--
        双栏布局：左边调样式，右边**吸附**预览。
        这是针对「上面调样式还得滚轮往下滑看效果」的直接解法——
        无论左边滚到哪里，预览始终可见（`position: sticky`）。
        窄屏（< 1080px）自动回落到单栏，预览回到内容下方。
      -->
      <div class="editor-layout">
        <div class="editor-main">
      <!-- ── 样式列表（命名样式）────────────────────────────────────── -->
      <section class="card">
        <div class="card-head">
          <h3>样式</h3>
          <div class="controls">
            <button class="ghost" :disabled="styleBusy" @click="openCreate(null)">+ 新建空白</button>
            <button class="ghost" :disabled="styleBusy" @click="openCreate(draft.id)">
              从当前复制
            </button>
          </div>
        </div>
        <p class="dim">
          每套样式是一整套外观。面板地址里只写 <code>?style=&lt;id&gt;</code>，
          所以在这里改一处，所有用这套样式的 OBS 浏览器源一起生效。
        </p>
        <ul class="style-list">
          <li
            v-for="s in styles"
            :key="s.id"
            class="style-row"
            :class="{ active: s.id === draft.id }"
          >
            <button class="style-pick" @click="selectStyle(s)">
              <span class="style-swatch" :style="{ background: styleSwatch(s) }" />
              <span class="style-info">
                <span class="style-name">
                  {{ s.name }}
                  <span v-if="s.id === defaultStyleId" class="tag-default">默认</span>
                </span>
                <span class="dim">{{ styleSummary(s) }} · <code>{{ s.id }}</code></span>
              </span>
            </button>
            <div class="style-ops">
              <button
                v-if="s.id !== defaultStyleId"
                class="ghost"
                :disabled="styleBusy"
                title="没写 ?style= 的源会用它"
                @click="makeDefault(s)"
              >
                设为默认
              </button>
              <button class="ghost" :disabled="styleBusy" @click="openRenameStyle(s)">重命名</button>
              <button
                class="danger"
                :disabled="styleBusy || styles.length <= 1"
                :title="styles.length <= 1 ? '至少要保留一套样式' : '删除这套样式'"
                @click="deletingStyle = s"
              >
                删除
              </button>
            </div>
          </li>
        </ul>
      </section>

      <!-- ── 外观：预设 + 颜色 ───────────────────────────────────── -->
      <section class="card">
        <div class="card-head">
          <h3>外观</h3>
          <div class="controls">
            <span v-if="presetHint" class="dim">{{ presetHint }}</span>
            <button class="ghost" @click="resetAllStyles()">恢复默认外观</button>
          </div>
        </div>

        <!-- 风格预设：一次把主题/背景/底色/轨道/字体色/进度条搭配好 -->
        <div class="preset-row">
          <button
            v-for="p in PRESETS"
            :key="p.key"
            class="preset"
            :class="{ active: activePreset === p.key }"
            :title="p.desc"
            @click="applyPresetWithHint(p)"
          >
            <span class="preset-chip" :data-preset="p.key" />
            <strong>{{ p.label }}</strong>
            <span class="dim">{{ p.desc }}</span>
          </button>
        </div>

        <h4>颜色</h4>
        <div class="color-grid">
          <label v-for="c in COLOR_ITEMS" :key="c.field" class="color-item">
            <span class="color-item-head">
              {{ c.label }}
              <button
                v-if="isColorSet(c.field)"
                class="mini"
                title="恢复这一项"
                @click="clearColor(c.field)"
              >✕</button>
            </span>
            <span class="color-row">
              <input
                type="color"
                :value="colorValue(c.field)"
                @input="setColor(c.field, ($event.target as HTMLInputElement).value)"
              />
              <span class="mono">{{ colorValue(c.field) }}</span>
            </span>
            <span class="dim">{{ c.hint }}</span>
          </label>
        </div>
        <p v-if="fgConflict" class="warn">
          ⚠️ 当前是<strong>{{ draft.theme === 'dark' ? '深色' : '浅色' }}</strong>主题，
          但字体色偏{{ draft.theme === 'dark' ? '暗' : '亮' }}，文字可能看不清。
          深色主题建议用浅色字，浅色主题建议用深色字。
        </p>

        <!-- 字体色单独给一行「用主题推荐色」，避免手调出看不见的组合 -->
        <div class="controls">
          <button class="ghost" @click="setColor('fg', themeFg)">字体色用主题推荐值（{{ themeFg }}）</button>
          <button class="ghost" @click="makeAllTransparent()">全部透明（只留文字与进度条）</button>
        </div>
      </section>

      <!-- ── 布局与尺寸 ───────────────────────────────────────────── -->
      <section class="card">
        <h3>布局与尺寸</h3>
        <div class="fields">
          <label>主题
            <select v-model="draft.theme" @change="syncStyleParam('theme', draft.theme)">
              <option value="dark">dark（深色）</option>
              <option value="light">light（浅色）</option>
            </select>
          </label>
          <label>背景
            <select v-model="draft.bg" @change="syncStyleParam('bg', draft.bg)">
              <option value="transparent">transparent（OBS 用这个）</option>
              <option value="solid">solid（不透明）</option>
            </select>
          </label>
          <label>布局
            <select v-model="draft.layout" @change="syncStyleParam('layout', draft.layout)">
              <option value="list">list（竖向堆叠）</option>
              <option value="compact">compact（精简单行）</option>
              <option value="lyrics">lyrics（歌词为主）</option>
              <option value="wide">wide（左歌曲 / 右歌词）</option>
            </select>
          </label>
          <label>字号
            <input
              type="number"
              v-model.number="draft.font_size"
              @change="syncStyleParam('fontSize', String(draft.font_size))"
            />
          </label>
          <label>队列条数
            <input
              type="number"
              v-model.number="draft.limit"
              @change="syncStyleParam('limit', String(draft.limit))"
            />
          </label>
          <label class="check">
            <input
              type="checkbox"
              v-model="draft.show_lyrics"
              @change="syncStyleParam('showLyrics', draft.show_lyrics ? 'true' : 'false')"
            /> 显示歌词
          </label>
        </div>
        <p class="dim">
          这些会同时写进面板地址与默认样式。地址里的值优先，所以每个 OBS 浏览器源可以各调各的。
        </p>
      </section>

      <!-- ── 背景图（阶段 9，阶段 10b 改为下拉选择 + 预览）────────── -->
      <section class="card">
        <div class="card-head">
          <h3>背景图</h3>
          <div class="controls">
            <button
              v-if="draft.bg_image"
              :class="{ active: includeBgInUrls }"
              :title="includeBgInUrls ? '地址里已带 bgImage 参数' : '把背景图写进面板地址'"
              @click="includeBgInUrls = !includeBgInUrls"
            >
              {{ includeBgInUrls ? '✓ 地址已带背景图' : '让地址带上背景图' }}
            </button>
          </div>
        </div>
        <div class="fields">
          <label class="wide">从已上传的图片里选
            <select v-model="draft.bg_image" @change="onPickExisting">
              <option :value="null">（不使用背景图）</option>
              <option v-for="bg in backgrounds" :key="bg.url" :value="bg.url">
                {{ bg.name }}（{{ formatSize(bg.size) }}）
              </option>
            </select>
          </label>
          <label class="wide">上传新图片
            <input type="file" accept="image/*" @change="onUploadBackground" />
          </label>
          <label class="wide">或填写图片地址
            <input v-model="bgImageInput" placeholder="https://... 或 /bg/xxx.png" />
          </label>
        </div>
        <div class="controls">
          <button class="ghost" :disabled="backgroundsLoading" @click="loadBackgrounds()">
            {{ backgroundsLoading ? '读取中…' : '刷新图片列表' }}
          </button>
          <button :disabled="!bgImageInput.trim()" @click="applyBgImageInput()">应用地址</button>
          <button class="ghost" :disabled="!draft.bg_image" @click="clearBackground()">清除背景图</button>
        </div>
        <!--
          背景图网格：像文件夹里看图片一样，每张都有缩略图。
          早期是「文件名 / 大小 / 操作」表格，缩略图只在卡片里显示一张，
          要认出是哪张图只能逐张点开预览——实测反馈就是"看不清"。
        -->
        <h4>图片库</h4>
        <div v-if="backgrounds.length" class="bg-grid">
          <div
            v-for="bg in backgrounds"
            :key="bg.url"
            class="bg-cell"
            :class="{ active: draft.bg_image === bg.url }"
          >
            <button
              class="bg-thumb"
              :title="`${bg.name} · 点击编辑`"
              :style="{ backgroundImage: `url(${absoluteBg(bg.url)})` }"
              @click="openEditor(bg)"
            >
              <!-- 正在被当前样式使用：给个角标，避免误删 -->
              <span v-if="draft.bg_image === bg.url" class="bg-badge">在用</span>
            </button>
            <div class="bg-meta">
              <span class="bg-name" :title="bg.name">{{ bg.name }}</span>
              <span class="dim">{{ formatSize(bg.size) }}</span>
            </div>
            <div class="bg-actions">
              <button class="ghost" title="裁剪 / 旋转 / 缩放 / 镜像" @click="openEditor(bg)">编辑</button>
              <button
                class="ghost"
                title="设为当前样式的背景图"
                :disabled="draft.bg_image === bg.url"
                @click="pickBackground(bg)"
              >
                用它
              </button>
              <button class="ghost" title="预览大图" @click="openPreview(bg)">预览</button>
              <button class="ghost" :disabled="bgBusy" @click="openRename(bg)">重命名</button>
              <button class="ghost" @click="downloadBg(bg)">下载</button>
              <button class="danger" :disabled="bgBusy" @click="pendingDelete = bg">删除</button>
            </div>
          </div>
        </div>
        <p v-else-if="!backgroundsLoading" class="empty">
          还没有图片。用上面的「上传新图片」加一张，或填写外部图片地址。
        </p>
        <!-- 预览：让用户确定图片真的能加载（相对路径在跨源预览里曾 404） -->
        <div v-if="draft.bg_image" class="bg-preview-wrap">
          <div class="bg-preview" :style="{ backgroundImage: `url(${absoluteBg(draft.bg_image)})` }" />
        </div>
      </section>

      <!-- ── 地址与参数 ───────────────────────────────────────────── -->
      <section class="card">
        <div class="card-head">
          <h3>面板地址</h3>
          <div class="controls">
            <button class="ghost" @click="copyAll()">
              {{ copiedKey === '__all__' ? '已复制全部' : '复制全部' }}
            </button>
          </div>
        </div>
        <ul class="url-list">
          <li v-for="p in pages" :key="p.key">
            <div class="url-meta">
              <strong>{{ p.label }}</strong>
              <span class="dim">{{ p.desc }}</span>
            </div>
            <code class="url">{{ p.url }}</code>
            <div class="controls">
              <button class="ghost" @click="copyUrl(p.url, p.key)">
                {{ copiedKey === p.key ? '已复制' : '复制' }}
              </button>
              <a class="link" :href="p.url" target="_blank" rel="noreferrer">新窗口打开</a>
              <button class="ghost" @click="previewKey = p.key">在下方预览</button>
              <label class="inline-num">
                字号倍率
                <input
                  type="number"
                  step="0.05"
                  min="0.2"
                  max="5"
                  :value="fontScale[p.key]"
                  @input="setFontScale(p.key, ($event.target as HTMLInputElement).value)"
                />
              </label>
            </div>
          </li>
        </ul>
      </section>

      <!--
        这里曾有一个「高级：地址参数」编辑器（手写 ?color=/?theme= 之类的覆盖）。
        命名样式上线后，面板地址**只写 `?style=<id>`**，不再携带任何外观参数——
        那个编辑器已经和地址生成脱节：改它不会影响任何地址，却会让人以为改了有用
        （实测还造成过"列表色块是绿的、取色器却显示粉色"的不一致）。
        所以整块移除。要改外观请直接改上面的样式，然后「保存样式」。
      -->

      <!-- ── 面板启停（阶段 9）────────────────────────────────────── -->
      <section class="card">
        <h3>要使用哪些面板</h3>
        <p class="dim">
          只影响下面地址列表里显示哪几个。OBS 里想要哪个源都可以直接加，
          这里不打勾也不会被禁用。
        </p>
        <div class="fields">
          <label v-for="p in PANEL_LIST" :key="p.key" class="check">
            <input type="checkbox" v-model="enabledPanels[p.key]" />
            {{ p.label }}
            <span class="dim">（{{ p.desc }}）</span>
          </label>
        </div>
      </section>
        </div>
        <!-- /.editor-main -->

        <!-- ── 预览（吸附右栏）──────────────────────────────────────── -->
        <aside class="editor-aside">
          <section class="card preview-card">
            <div class="card-head">
              <h3>预览 · {{ previewLabel }}</h3>
              <div class="controls">
                <button class="ghost" title="重新加载预览" @click="reloadPreview()">刷新</button>
              </div>
            </div>
            <!-- 面板类型切换：一行放下，避免和预览抢高度 -->
            <div class="preview-tabs">
              <button
                v-for="p in pages"
                :key="p.key"
                class="ghost"
                :class="{ active: previewKey === p.key }"
                @click="previewKey = p.key"
              >
                {{ p.label }}
              </button>
            </div>
            <div class="preview-frame" :class="{ transparent: draft.bg === 'transparent' }">
              <iframe :key="`${previewUrl}#${previewNonce}`" :src="previewUrl" title="面板预览" />
            </div>
            <p class="dim preview-hint">
              预览实时跟随左边改动的样式。改颜色时不会整页重载，只推变量。
            </p>
          </section>
        </aside>
      </div>
      <!-- /.editor-layout -->
    </template>

    <!--
      图片预览遮罩层。
      - 点击遮罩空白处 / 右上角 ✕ / Esc 关闭
      - ← → 切换上下一张
      - 滚轮或 + − 缩放，双击复位
    -->
    <div
      v-if="preview"
      class="lightbox"
      role="dialog"
      aria-modal="true"
      @click.self="closePreview()"
      @wheel.prevent="onPreviewWheel($event)"
    >
      <button class="lightbox-close" title="关闭预览（Esc）" @click="closePreview()">✕</button>

      <div class="lightbox-name">
        {{ preview.name }}
        <span class="lightbox-count">{{ previewIndex + 1 }} / {{ backgrounds.length }}</span>
      </div>

      <button
        v-if="backgrounds.length > 1"
        class="lightbox-nav prev"
        title="上一张（←）"
        @click.stop="stepPreview(-1)"
      >
        ‹
      </button>

      <img
        class="lightbox-img"
        :src="absoluteBg(preview.url)"
        :alt="preview.name"
        :style="{ transform: `scale(${previewZoom})` }"
        :class="{ zoomed: previewZoom > 1 }"
        @click.stop
        @dblclick.stop="previewZoom = 1"
      />

      <button
        v-if="backgrounds.length > 1"
        class="lightbox-nav next"
        title="下一张（→）"
        @click.stop="stepPreview(1)"
      >
        ›
      </button>

      <div class="lightbox-zoom" @click.stop>
        <button class="ghost" title="缩小（滚轮向下）" @click="zoomPreview(-0.25)">−</button>
        <span>{{ Math.round(previewZoom * 100) }}%</span>
        <button class="ghost" title="放大（滚轮向上）" @click="zoomPreview(0.25)">＋</button>
        <button class="ghost" @click="previewZoom = 1">复位</button>
      </div>
    </div>

    <!-- 重命名弹层（替代 window.prompt） -->
    <div v-if="renameTarget" class="modal-mask" @click.self="renameTarget = null">
      <div class="modal">
        <h4>重命名图片</h4>
        <div class="fields">
          <label class="wide">新名称（扩展名保持不变）
            <input
              v-model="renameInput"
              autofocus
              @keyup.enter="confirmRename()"
              @keyup.esc="renameTarget = null"
            />
          </label>
        </div>
        <p class="modal-note">原文件：{{ renameTarget.name }}</p>
        <div class="controls">
          <button :disabled="bgBusy" @click="confirmRename()">确定</button>
          <button class="ghost" @click="renameTarget = null">取消</button>
        </div>
      </div>
    </div>

    <!-- 删除确认弹层（替代 window.confirm） -->
    <div v-if="pendingDelete" class="modal-mask" @click.self="pendingDelete = null">
      <div class="modal">
        <h4>删除图片</h4>
        <p class="modal-note">
          确定删除 <b>{{ pendingDelete.name }}</b> ？引用它的面板会失去背景图。
        </p>
        <div class="controls">
          <button class="danger" :disabled="bgBusy" @click="confirmDelete()">确定删除</button>
          <button class="ghost" @click="pendingDelete = null">取消</button>
        </div>
      </div>
    </div>

    <!-- 新建样式 -->
    <div v-if="creatingFrom !== undefined" class="modal-mask" @click.self="creatingFrom = undefined">
      <div class="modal">
        <h4>新建样式</h4>
        <p class="modal-note">
          <template v-if="creatingFrom">
            从「{{ styles.find((s) => s.id === creatingFrom)?.name ?? creatingFrom }}」复制一份，
            之后两套各自独立。
          </template>
          <template v-else>建立一套出厂默认外观的空白样式。</template>
        </p>
        <label class="modal-field">
          样式名
          <input
            v-model="newStyleName"
            maxlength="24"
            autofocus
            @keyup.enter="confirmCreate()"
            @keyup.esc="creatingFrom = undefined"
          />
        </label>
        <div class="controls">
          <button :disabled="styleBusy || !newStyleName.trim()" @click="confirmCreate()">
            {{ styleBusy ? '创建中…' : '创建' }}
          </button>
          <button class="ghost" @click="creatingFrom = undefined">取消</button>
        </div>
      </div>
    </div>

    <!-- 重命名样式 -->
    <div v-if="renamingStyle" class="modal-mask" @click.self="renamingStyle = null">
      <div class="modal">
        <h4>重命名样式</h4>
        <label class="modal-field">
          样式名
          <input
            v-model="renameValue"
            maxlength="24"
            autofocus
            @keyup.enter="confirmRenameStyle()"
            @keyup.esc="renamingStyle = null"
          />
        </label>
        <p class="modal-note">
          只改显示名；地址里用的 id <code>{{ renamingStyle.id }}</code> 不变，
          因此 OBS 里的源不受影响。
        </p>
        <div class="controls">
          <button :disabled="styleBusy || !renameValue.trim()" @click="confirmRenameStyle()">确定</button>
          <button class="ghost" @click="renamingStyle = null">取消</button>
        </div>
      </div>
    </div>

    <!-- 删除样式确认 -->
    <div v-if="deletingStyle" class="modal-mask" @click.self="deletingStyle = null">
      <div class="modal">
        <h4>删除样式</h4>
        <p class="modal-note">
          确定删除 <b>{{ deletingStyle.name }}</b> ？
          <template v-if="deletingStyle.id === defaultStyleId">
            它当前是**默认样式**，删除后会自动切到列表里的第一套。
          </template>
          引用它的 OBS 浏览器源会回落到默认样式，不会报错。
        </p>
        <div class="controls">
          <button class="danger" :disabled="styleBusy" @click="confirmDeleteStyle()">确定删除</button>
          <button class="ghost" @click="deletingStyle = null">取消</button>
        </div>
      </div>
    </div>

    <!--
      背景图裁剪编辑器。
      变换（旋转/缩放/镜像）与裁剪都**烘焙**进导出的 PNG，另存为新图，
      原图保留；保存成功后自动设为当前样式的背景图。
    -->
    <BackgroundEditor
      v-if="editing"
      :src="absoluteBg(editing.url)"
      :name="editing.name"
      @save="saveEdited"
      @cancel="closeEditor"
    />
  </div>
</template>

<style scoped>
.panels-page {
  display: flex;
  flex-direction: column;
  gap: 14px;
}

/*
 * 设置页的子标签。
 * 与主标签（控制台里的那些）刻意做成不同样式：这是**页内**导航，
 * 视觉上更轻，避免和顶部主导航混淆。
 */
.sub-tabs {
  display: flex;
  gap: 6px;
  padding-bottom: 10px;
  border-bottom: 1px solid var(--bsr-border);
}

.sub-tabs a {
  padding: 5px 14px;
  border: 1px solid transparent;
  border-radius: 999px;
  color: var(--bsr-muted);
  font-size: 13px;
  text-decoration: none;
  transition: background 0.12s ease, color 0.12s ease;
}

.sub-tabs a:hover {
  background: var(--bsr-accent-soft);
  color: var(--bsr-fg);
}

.sub-tabs a.router-link-active {
  border-color: var(--bsr-accent);
  background: var(--bsr-accent-soft);
  color: var(--bsr-accent);
  font-weight: 600;
}

/* ── 双栏：左编辑 / 右吸附预览 ─────────────────────────────────────────── */

.editor-layout {
  display: grid;
  /* 右栏固定 380px：够看清面板比例，又不至于把左侧表单挤窄 */
  grid-template-columns: minmax(0, 1fr) 380px;
  gap: 16px;
  align-items: start;
}

.editor-main {
  display: flex;
  flex-direction: column;
  gap: 14px;
  min-width: 0;
}

.editor-aside {
  /*
   * 吸附：这是「调样式还得滚下去看」的解法。
   * top 取一个略大于顶栏的值，避免贴住顶部导航。
   */
  position: sticky;
  top: 12px;
}

.preview-card {
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.preview-tabs {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}

.preview-tabs button.active {
  border-color: var(--bsr-accent);
  color: var(--bsr-accent);
  background: var(--bsr-accent-soft);
}

.preview-hint {
  margin: 0;
  font-size: 11px;
  line-height: 1.4;
}

.preview-frame {
  /* 需求指定 400px：能看清宽版/列表布局的比例 */
  height: 400px;
}

/*
 * 窄屏回落单栏：预览回到内容下方（且不再吸附，
 * 否则在小窗口里会盖住表单）。
 */
@media (max-width: 1080px) {
  .editor-layout {
    grid-template-columns: minmax(0, 1fr);
  }

  .editor-aside {
    position: static;
  }
}

/* ── 样式列表 ─────────────────────────────────────────────────────────── */

.style-list {
  display: flex;
  flex-direction: column;
  gap: 8px;
  margin: 10px 0 0;
  padding: 0;
  list-style: none;
}

.style-row {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 8px 10px;
  border: 1px solid var(--bsr-border);
  border-radius: 10px;
  background: var(--bsr-bg-elevated);
  transition: border-color 0.12s ease, box-shadow 0.12s ease;
}

.style-row.active {
  border-color: var(--bsr-accent);
  box-shadow: 0 0 0 2px var(--bsr-accent-soft);
}

/* 点击整行切换编辑对象 */
.style-pick {
  display: flex;
  flex: 1;
  align-items: center;
  gap: 10px;
  min-width: 0;
  padding: 0;
  border: 0;
  background: transparent;
  color: inherit;
  text-align: left;
  cursor: pointer;
}

.style-swatch {
  flex: none;
  width: 46px;
  height: 26px;
  border: 1px solid var(--bsr-border);
  border-radius: 6px;
}

.style-info {
  display: flex;
  flex-direction: column;
  gap: 1px;
  min-width: 0;
}

.style-name {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 13px;
  color: var(--bsr-fg);
}

.style-info .dim {
  font-size: 11.5px;
}

.style-info code {
  font-size: 11px;
  color: var(--bsr-muted);
}

.tag-default {
  padding: 0 6px;
  border-radius: 999px;
  background: var(--bsr-accent-soft);
  color: var(--bsr-accent);
  font-size: 10px;
  line-height: 16px;
}

.style-ops {
  display: flex;
  flex: none;
  gap: 6px;
}

.style-ops button {
  padding: 4px 10px;
  font-size: 12px;
}

.page-head {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: 16px;
  flex-wrap: wrap;
}

.page-head h2 {
  margin: 0;
  font-size: 18px;
}

.card {
  padding: 14px 16px;
  border: 1px solid var(--bsr-border);
  border-radius: 10px;
  background: var(--bsr-bg-elevated, transparent);
}

.card h3 {
  margin: 0 0 6px;
  font-size: 15px;
}

.card-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  flex-wrap: wrap;
}

.hint,
.empty {
  margin: 6px 0 0;
  font-size: 12px;
  color: var(--bsr-muted);
  line-height: 1.7;
}

/*
 * 💡 悬浮提示：长说明默认收起成一个图标，悬停/聚焦时展开。
 * 与 Dashboard.vue 里的同名样式保持一致（两边都是 scoped）。
 *.err {
  margin: 6px 0 0;
  font-size: 13px;
  color: var(--bsr-danger);
}

/* ── 背景图网格（像文件夹里看图片）────────────────────────────────────── */

.bg-grid {
  display: grid;
  /* 每格约 150px 宽，窗口越宽放得越多 */
  grid-template-columns: repeat(auto-fill, minmax(150px, 1fr));
  gap: 12px;
  margin-top: 10px;
}

.bg-cell {
  display: flex;
  flex-direction: column;
  gap: 6px;
  padding: 8px;
  border: 1px solid var(--bsr-border);
  border-radius: 10px;
  background: var(--bsr-bg-elevated);
  transition: border-color 0.12s ease, box-shadow 0.12s ease;
}

/* 当前样式正在用这张图 */
.bg-cell.active {
  border-color: var(--bsr-accent);
  box-shadow: 0 0 0 2px var(--bsr-accent-soft);
}

/* 缩略图：正方形裁切，点击进入编辑器 */
.bg-thumb {
  position: relative;
  width: 100%;
  aspect-ratio: 1 / 1;
  padding: 0;
  border: 1px solid var(--bsr-border);
  border-radius: 8px;
  /* 棋盘底：透明区域一眼可见；图片覆盖在上面 */
  background-color: #f6e9ef;
  background-image:
    repeating-conic-gradient(rgb(255 111 165 / 10%) 0% 25%, transparent 0% 50%);
  background-size: 16px 16px, cover;
  background-position: 0 0, center;
  background-repeat: repeat, no-repeat;
  cursor: pointer;
  overflow: hidden;
}

.bg-thumb:hover {
  border-color: var(--bsr-accent);
}

.bg-badge {
  position: absolute;
  top: 5px;
  left: 5px;
  padding: 1px 6px;
  border-radius: 999px;
  background: var(--bsr-accent);
  color: #fff;
  font-size: 10px;
  line-height: 16px;
}

.bg-meta {
  display: flex;
  flex-direction: column;
  gap: 1px;
  min-width: 0;
  font-size: 12px;
}

.bg-name {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--bsr-fg);
}

.bg-meta .dim {
  font-size: 11px;
}

.bg-actions {
  display: flex;
  flex-wrap: wrap;
  gap: 4px;
}

.bg-actions button {
  padding: 3px 8px;
  font-size: 11.5px;
}

/* ── 图片预览遮罩层 ─────────────────────────────────────────────────── */
.lightbox {
  position: fixed;
  inset: 0;
  z-index: 200;
  display: flex;
  align-items: center;
  justify-content: center;
  background: rgb(0 0 0 / 78%);
  overflow: hidden;
}

.lightbox-img {
  max-width: 90vw;
  max-height: 84vh;
  object-fit: contain;
  transition: transform 0.12s ease-out;
  transform-origin: center center;
  user-select: none;
}

/* 放大后允许拖出可视区（配合 overflow hidden 形成"平移"效果） */
.lightbox-img.zoomed {
  cursor: grab;
}

.lightbox-close {
  position: absolute;
  top: 18px;
  right: 22px;
  width: 38px;
  height: 38px;
  border: 0;
  border-radius: 50%;
  background: rgb(255 255 255 / 14%);
  color: #fff;
  font-size: 18px;
  line-height: 1;
  cursor: pointer;
}

.lightbox-close:hover {
  background: rgb(255 255 255 / 26%);
}

.lightbox-name {
  position: absolute;
  top: 24px;
  left: 26px;
  color: #fff;
  font-size: 13px;
  display: flex;
  gap: 10px;
  align-items: baseline;
}

.lightbox-count {
  color: rgb(255 255 255 / 62%);
  font-size: 12px;
}

.lightbox-nav {
  position: absolute;
  top: 50%;
  transform: translateY(-50%);
  width: 46px;
  height: 64px;
  border: 0;
  border-radius: 8px;
  background: rgb(255 255 255 / 12%);
  color: #fff;
  font-size: 30px;
  line-height: 1;
  cursor: pointer;
}

.lightbox-nav:hover {
  background: rgb(255 255 255 / 24%);
}

.lightbox-nav.prev {
  left: 22px;
}

.lightbox-nav.next {
  right: 22px;
}

.lightbox-zoom {
  position: absolute;
  bottom: 24px;
  left: 50%;
  transform: translateX(-50%);
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 6px 12px;
  border-radius: 999px;
  background: rgb(0 0 0 / 55%);
  color: #fff;
  font-size: 12px;
}

.lightbox-zoom span {
  min-width: 4em;
  text-align: center;
  font-variant-numeric: tabular-nums;
}

/* ── 页内弹层（重命名 / 删除确认，替代系统 prompt / confirm）────────── */
.modal-mask {
  position: fixed;
  inset: 0;
  z-index: 240;
  display: flex;
  align-items: center;
  justify-content: center;
  background: rgb(0 0 0 / 52%);
}

.modal {
  width: min(420px, calc(100vw - 40px));
  padding: 18px 20px;
  border: 1px solid var(--bsr-border);
  border-radius: 10px;
  background: var(--bsr-bg-elevated);
  box-shadow: 0 12px 32px rgb(0 0 0 / 32%);
}

.modal h4 {
  margin: 0 0 12px;
  font-size: 14px;
}

.modal-note {
  margin: 10px 0 14px;
  font-size: 12.5px;
  color: var(--bsr-muted);
  line-height: 1.6;
  word-break: break-all;
}

.modal .controls {
  justify-content: flex-end;
}

.hint b {
  color: var(--bsr-fg);
}

/*
 * 表单字段网格。
 *
 * ⚠️ 用 `auto-fill` 而不是 `auto-fit`：`auto-fit` 会合并空轨道并拉伸剩余轨道，
 * 于是「字段越少每个越宽」（实测 2 个字段时列宽 545px、4 个字段时 264px）。
 * 详见 Dashboard.vue 里同一处的说明。
 */
.fields {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(200px, 1fr));
  gap: 10px 14px;
  margin-top: 10px;
}

.fields label {
  display: flex;
  flex-direction: column;
  gap: 4px;
  font-size: 12px;
  color: var(--bsr-muted);
}

.fields label.check {
  flex-direction: row;
  align-items: center;
  gap: 6px;
}

.fields input,
.fields select {
  padding: 5px 8px;
  border: 1px solid var(--bsr-border);
  border-radius: 6px;
  background: var(--bsr-bg);
  color: var(--bsr-fg);
  font-size: 13px;
  /* 数字框不该占满整列 */
  max-width: 260px;
}

.fields input[type='number'] {
  max-width: 130px;
}

.fields label.check input {
  max-width: none;
}

/* ── 风格预设 ─────────────────────────────────────────────────────────── */

.preset-row {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(150px, 1fr));
  gap: 10px;
  margin-top: 10px;
}

.preset {
  display: flex;
  flex-direction: column;
  align-items: flex-start;
  gap: 3px;
  padding: 10px 12px;
  border: 1px solid var(--bsr-border);
  border-radius: 10px;
  background: var(--bsr-bg-elevated);
  color: var(--bsr-fg);
  text-align: left;
  cursor: pointer;
  transition: border-color 0.12s ease, box-shadow 0.12s ease, transform 0.12s ease;
}

.preset:hover {
  transform: translateY(-1px);
  box-shadow: 0 4px 12px rgb(255 111 165 / 18%);
}

.preset.active {
  border-color: var(--bsr-accent);
  box-shadow: 0 0 0 2px var(--bsr-accent-soft);
}

.preset strong {
  font-size: 13px;
}

.preset .dim {
  font-size: 11px;
  line-height: 1.35;
}

/* 预设小色块：一眼看出这个预设的配色 */
.preset-chip {
  width: 100%;
  height: 8px;
  border-radius: 999px;
  margin-bottom: 4px;
}

.preset-chip[data-preset='pink'] {
  background: linear-gradient(90deg, #ff6fa5, #ffd9e6, #ffffff);
}

.preset-chip[data-preset='dark'] {
  background: linear-gradient(90deg, #ff9ec4, #00000073, #333333);
}

.preset-chip[data-preset='plain'] {
  background: linear-gradient(90deg, #ff6fa5, transparent 70%), repeating-linear-gradient(45deg, #ffd9e6 0 4px, transparent 4px 8px);
}

/* ── 颜色控件 ─────────────────────────────────────────────────────────── */

.color-grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(210px, 1fr));
  gap: 12px 16px;
  margin-top: 10px;
}

.color-item {
  display: flex;
  flex-direction: column;
  gap: 4px;
  font-size: 12px;
  color: var(--bsr-muted);
}

.color-item-head {
  display: flex;
  align-items: center;
  gap: 6px;
}

.color-item .mono {
  font-family: ui-monospace, Consolas, monospace;
  font-size: 12px;
  color: var(--bsr-fg);
}

.color-item .dim {
  font-size: 11px;
}

/* 清除单项的小按钮 */
button.mini {
  padding: 0 5px;
  border: 1px solid var(--bsr-border);
  border-radius: 999px;
  background: transparent;
  color: var(--bsr-muted);
  font-size: 10px;
  line-height: 16px;
  cursor: pointer;
}

button.mini:hover {
  border-color: var(--bsr-danger);
  color: var(--bsr-danger);
}

/* 对比度提示 */
p.warn {
  margin: 10px 0 0;
  padding: 8px 10px;
  border-left: 3px solid var(--bsr-warning);
  border-radius: 6px;
  background: color-mix(in srgb, var(--bsr-warning) 14%, transparent);
  font-size: 12px;
  line-height: 1.5;
  color: var(--bsr-fg);
}

.controls {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}

.controls button {
  padding: 5px 12px;
  border: 1px solid var(--bsr-accent);
  border-radius: 6px;
  background: var(--bsr-accent-soft, transparent);
  color: var(--bsr-fg);
  font-size: 12px;
  cursor: pointer;
}

.controls button.ghost {
  border-color: var(--bsr-border);
  background: transparent;
  color: var(--bsr-muted);
}

.controls button.ghost.active {
  color: var(--bsr-fg);
  border-color: var(--bsr-accent);
}

.controls button:disabled {
  opacity: 0.5;
  cursor: default;
}

.link {
  font-size: 12px;
  color: var(--bsr-accent);
}

.inline-num {
  display: flex;
  align-items: center;
  gap: 4px;
  font-size: 12px;
  color: var(--bsr-muted);
}

.inline-num input {
  width: 62px;
  padding: 4px 6px;
  border: 1px solid var(--bsr-border);
  border-radius: 6px;
  background: var(--bsr-bg);
  color: var(--bsr-fg);
}

.url-list {
  margin: 10px 0 0;
  padding: 0;
  list-style: none;
  display: flex;
  flex-direction: column;
  gap: 12px;
}

.url-list li {
  padding: 10px 12px;
  border: 1px solid var(--bsr-border);
  border-radius: 8px;
}

.url-meta {
  display: flex;
  align-items: baseline;
  gap: 8px;
  flex-wrap: wrap;
}

.url-meta strong {
  font-size: 13px;
}

.url-meta .dim {
  font-size: 12px;
  color: var(--bsr-muted);
}

.url {
  display: block;
  margin: 6px 0 10px;
  padding: 8px 10px;
  border-radius: 6px;
  background: var(--bsr-bg);
  border: 1px solid var(--bsr-border);
  font-size: 12px;
  word-break: break-all;
}

/* ── 参数行编辑器（阶段 9）────────────────────────────────────────────── */

.param-list {
  margin: 10px 0 0;
  padding: 0;
  list-style: none;
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.param-list li {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
  padding: 8px 10px;
  border: 1px solid var(--bsr-border);
  border-radius: 8px;
}

.param-list select,
.param-list input {
  padding: 5px 8px;
  border: 1px solid var(--bsr-border);
  border-radius: 6px;
  background: var(--bsr-bg);
  color: var(--bsr-fg);
  font-size: 13px;
}

.param-list select:first-child {
  min-width: 170px;
}

.param-list input[type='text'],
.param-list input:not([type]) {
  min-width: 140px;
}

.param-hint {
  font-size: 11px;
  color: var(--bsr-muted);
}

.color-row {
  display: inline-flex;
  align-items: center;
  gap: 6px;
}

.color-row input[type='color'] {
  width: 34px;
  height: 30px;
  padding: 2px;
  cursor: pointer;
}

.color-row input[type='text'],
.color-row input:not([type]) {
  min-width: 130px;
}

/* ── 背景图预览（阶段 10b）────────────────────────────────────────────── */

.bg-preview-wrap {
  margin-top: 10px;
}

.bg-preview {
  height: 140px;
  border: 1px solid var(--bsr-border);
  border-radius: 8px;
  /* 浅粉灰底：图片未覆盖到的区域一眼可见，且与粉白主题协调 */
  background-color: #f6e9ef;
  background-size: cover;
  background-position: center;
  background-repeat: no-repeat;
  /* 图片加载失败时不会「看起来像没生效」，这里给个浅色底 */
  box-shadow: inset 0 0 0 1px rgba(255, 255, 255, 0.6);
}

/* 预览框：棋盘底纹以便看清「透明背景」是否生效 */
.preview-frame {
  /*
   * 高度由双栏区的 `.preview-frame { height: 400px }` 决定（需求指定）。
   * 这里只负责边框与底色，不要再写 height——否则两处会互相覆盖。
   */
  border: 1px solid var(--bsr-border);
  border-radius: 8px;
  overflow: hidden;
  background: #f6e9ef;
}

.preview-frame.transparent {
  /* 棋盘格用浅粉灰：既是"透明"的通用视觉约定，又和粉白主题协调 */
  background-image:
    linear-gradient(45deg, rgba(255, 111, 165, 0.12) 25%, transparent 25%),
    linear-gradient(-45deg, rgba(255, 111, 165, 0.12) 25%, transparent 25%),
    linear-gradient(45deg, transparent 75%, rgba(255, 111, 165, 0.12) 75%),
    linear-gradient(-45deg, transparent 75%, rgba(255, 111, 165, 0.12) 75%);
  background-size: 18px 18px;
  background-position:
    0 0,
    0 9px,
    9px -9px,
    -9px 0;
}

.preview-frame iframe {
  width: 100%;
  height: 100%;
  border: 0;
}

/* 小窗口里预览矮一些，避免占掉整个视口 */
@media (max-width: 720px) {
  .preview-frame {
    height: 280px;
  }
}
</style>
