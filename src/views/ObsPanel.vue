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
import { computed, onBeforeUnmount, onMounted, onUnmounted, ref, watch } from 'vue'
import { useRoute } from 'vue-router'
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
import NavTabs from '@/components/NavTabs.vue'
import PanelPreview from '@/components/PanelPreview.vue'
import { clonePlain, styleToResolved } from '@/composables/panelParams'
import BackgroundEditor from '@/components/BackgroundEditor.vue'
import BgPositionEditor from '@/components/BgPositionEditor.vue'
import { showMessage } from '@/stores/message'
import { useAppStore } from '@/stores/app'
import type { FontScaleConfig, PanelStyleConfig } from '@/types'

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
  newStyleName.value = base ? `${base.name} 副本` : '新方案'
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
  bg_opacity: 1,
  bg_position: null,
  canvas_w: null,
  canvas_h: null,
  preview_width: null,
  font_scale: null,
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
/*
 * 每个面板地址的 `?scale=` 倍率。
 *
 * ## ⚠️ 全部保持 1，地址里就永远不会出现 `scale=`
 * 早期这里给歌词/弹幕预设了 `1.55` / `1.5`（那时想的是"歌词页该大一点"），
 * 于是生成出来的地址是 `/panel/lyrics?style=x&scale=1.55`。
 *
 * 但用户明确要求**不要用缩放来调字号**，改用「外观 → 字号」里的分项倍率。
 * 那个输入框已经删掉，若这里还留着非 1 的初值，用户就**再也改不掉它**
 * （界面里没有入口了），地址也会一直挂着一个莫名其妙的 `scale=1.55`。
 *
 * 所以全部归 1：地址恒为 `/panel/xxx?style=<id>`，与用户已有的 OBS 地址格式一致。
 * 需要临时放大某一个源时，仍可在控制台调用 `__setFontScale('lyrics', 1.3)`。
 */
const fontScale = ref<Record<string, number>>({
  all: 1,
  play: 1,
  lyrics: 1,
  danmaku: 1,
})

/**
 * 修改某个专注页的字号倍率。
 *
 * ## ⚠️ 界面入口已移除，函数**保留**用于向后兼容
 * 用户明确要求"不要用缩放来调字号"，所以「面板地址」列表里的那个
 * **字号倍率**输入框已经删掉，改用「外观 → 字号」里的分项倍率。
 *
 * 但 `?scale=` **地址参数本身不能删**：
 *  - 用户已经在 OBS 里配好的地址不能失效；
 *  - 它仍是一个有用的能力（同一个样式，某个源单独放大一点）。
 *
 * 因此 `urlFor()` 里写 `scale` 的逻辑保留：只要没人改这个 state，
 * 倍率恒为 1，地址里就**不会**出现 `scale=`，
 * 生成出来的地址与以前完全一致（例如 `/panel/play?style=style2`）。
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

/**
 * 手动设定某个面板地址的 `?scale=` 倍率。
 *
 * 界面入口已移除（用户在地址列表里看不到它了），但保留这个可调用的入口：
 * 控制台里可以直接调用，例如
 * `__setFontScale('lyrics', 1.3)` 给歌词页单独放大一点。
 *
 * 之所以不把整条链路删掉：**已有的 OBS 地址不能失效**。`?scale=` 仍然
 * 被 `usePanelStyle` 读取并应用，只是不再由界面写入。
 */
function setPanelScale(key: string, value: number): void {
  setFontScale(key, String(value))
}
/** 暴露到控制台，便于手动微调（见 `setPanelScale` 的说明）。 */
;(window as unknown as Record<string, unknown>).__setFontScale = setPanelScale

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

/** 可上色的项：歌名、主色、进度条、字体色、卡片底色、进度条轨道、描边色。 */
type ColorField =
  | 'color'
  | 'titleColor'
  | 'barColor'
  | 'fg'
  | 'surface'
  | 'track'
  | 'strokeColor'

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
    case 'titleColor':
      // 空 = 跟随主色；取色器上直接显示主色，符合"看起来会是什么样"
      return cfg.title_color || cfg.color
    case 'strokeColor':
      // 空 = 自动（按字体色算），这里给一个中性深色作为取色器初值
      return cfg.text_stroke_color || '#000000'
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
    case 'titleColor':
      return Boolean(cfg.title_color)
    case 'strokeColor':
      return Boolean(cfg.text_stroke_color)
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
    case 'titleColor':
      cfg.title_color = value
      break
    case 'strokeColor':
      cfg.text_stroke_color = value
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
    case 'titleColor':
      cfg.title_color = null
      break
    case 'strokeColor':
      cfg.text_stroke_color = null
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
/**
 * 面板预设。
 *
 * ## 字段名与样式字段一一对应
 * `theme` / `bg` / `color` / `barColor` / `fg` / `surface` / `track` 就是
 * `PanelStyleConfig` 里的字段名（`barColor` 与 `fg` 已是同形，`bar_color`
 * 用驼峰在这里对照）。这让我们可以直接把预设写进 `draft`。
 */
interface PanelPreset {
  key: string
  label: string
  desc: string
  /** 样式字段的值；`null` = 回到"跟随主色/自动"（对应后端空串）。 */
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

/**
 * `ParamName` → `PanelStyleConfig` 字段名的对照。
 *
 * 大部分同名，只有 `barColor` 在样式里叫 `bar_color`（后端是 snake_case）。
 */
const PARAM_TO_FIELD: Partial<Record<ParamName, keyof PanelStyleConfig>> = {
  barColor: 'bar_color',
}

/**
 * 当前套用的是哪个预设（样式与预设完全一致时高亮）。
 *
 * ⚠️ 原来这里比的是 `paramRows`——那套地址参数已是死代码，
 * 所以高亮**永远不会亮**。现在改成比 `draft` 的真实字段。
 */
const activePreset = computed(() => {
  const d = draft.value
  if (!d) return null
  return (
    PRESETS.find((p) =>
      Object.entries(p.params).every(([name, value]) => {
        const field = PARAM_TO_FIELD[name as ParamName] ?? (name as keyof PanelStyleConfig)
        const cur = String(d[field] ?? '').trim()
        // null 表示"跟随主色/自动"，后端用空串表示
        if (value === null) return cur === ''
        return cur === value
      }),
    )?.key ?? null
  )
})

/**
 * 套用预设：直接写进**当前样式草稿**。
 *
 * ## ⚠️ 这里曾经是个真 bug
 * 早期这个函数只往 `paramRows` 里写——那是**旧 URL 参数系统**的遗留物。
 * 自从样式改成命名样式（地址里只带 `?style=<id>`）之后，
 * **已经没有任何代码读 `paramRows` 了**，所以点预设只是改了一个没人看的
 * 数组，界面上毫无变化；而按钮还会弹「已套用「深色」」的提示，
 * 让这个 bug 更具欺骗性（用户反馈「不知道有什么用，似乎没有效果」）。
 *
 * 现在直接改 `draft`，因此：
 *  - 右侧预览**立刻**跟着变（本地渲染，见 `PanelPreview.vue`）；
 *  - 点「保存样式」后落盘，所有引用这套样式的 OBS 源一起生效。
 */
function applyPreset(preset: PanelPreset): void {
  if (!draft.value) return
  for (const [name, value] of Object.entries(preset.params)) {
    const field = PARAM_TO_FIELD[name as ParamName] ?? (name as keyof PanelStyleConfig)
    // null → 空串，表示"跟随主色/自动"（与后端约定一致）
    const next = value === null ? '' : value
    // 只赋值字符串字段；这里用到的字段在样式里都是 string 类型
    ;(draft.value as unknown as Record<string, unknown>)[field] = next
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

/**
 * 可选的 CSS 字重档位。
 *
 * 只列 100 的整数倍——CSS 虽然接受任意数值，但字体文件通常只有这几档，
 * 中间值会被浏览器吸附到最近一档，界面显示与实际渲染就会对不上。
 */
const WEIGHTS: Array<{ value: number; label: string }> = [
  { value: 300, label: '细 300' },
  { value: 400, label: '常规 400' },
  { value: 500, label: '中 500' },
  { value: 600, label: '半粗 600' },
  { value: 700, label: '粗 700' },
  { value: 800, label: '特粗 800' },
  { value: 900, label: '极粗 900' },
]

/** 恢复默认字重与描边。 */
function resetTypography(): void {
  if (!draft.value) return
  draft.value.font_weight = DEFAULT_PANEL.font_weight
  draft.value.font_weight_sub = DEFAULT_PANEL.font_weight_sub
  draft.value.font_weight_title = DEFAULT_PANEL.font_weight_title
  draft.value.text_stroke_width = DEFAULT_PANEL.text_stroke_width
  draft.value.text_stroke_color = null
}

/** 一键设描边宽度（保留当前颜色，没设过就走"自动"）。 */
function applyStrokePreset(width: number): void {
  if (!draft.value) return
  draft.value.text_stroke_width = width
}

/**
 * 颜色项的界面元数据。
 *
 * ## 顺序即界面顺序
 * 按「常调程度」排，**主色第一**——它一处改全局（队列高亮、歌词当前行、
 * 弹幕用户名），是最常动的；早期它排在第二，反而把「歌名颜色」摆在首位。
 *
 * 之后的次序按用途分组：
 *   主色 → 文字三兄弟（字体色 / 歌名 / 描边，都跟可读性有关）
 *        → 进度条两项 → 卡片底色（最少动）
 *
 * ## `hint` 不再渲染在项下面
 * 原来每个颜色项下面都有一行小字说明，纵向把整块拉得很高、也显得杂乱。
 * 现在改成**悬停提示**（`title`），信息保留但不占版面。
 */
/**
 * 各元素的**字号倍率**项的界面元数据。
 *
 * ## 为什么用"倍率"而不是直接填 px
 * 面板的字号是一套**相对比例**（歌名比正文大、正文比点歌人小……）。
 * 若每项都填绝对 px，改「基础字号」时它们不会跟着变，比例就散了。
 * 用倍率则：基础字号一改，全部按同一比例缩放，相对关系保持。
 *
 * `def` 必须与 `PanelView.vue` 里各选择器的兜底值一致，
 * 否则"没设过"与"点一下默认"会渲染出两个样子。
 */
const FONT_SCALE_ITEMS: Array<{
  field: keyof FontScaleConfig
  label: string
  def: number
  hint: string
}> = [
  { field: 'title', label: '歌名', def: 1.35, hint: '正在播放的曲名' },
  { field: 'body', label: '正文', def: 0.9, hint: '歌手、点歌人、时间等' },
  { field: 'lyrics', label: '歌词', def: 0.92, hint: '歌词行（当前行会再亮一点、大一点）' },
  { field: 'queue', label: '队列', def: 1.0, hint: '点歌队列里的曲目' },
  { field: 'danmaku', label: '弹幕', def: 1.5, hint: '「最近弹幕」页里的弹幕文本' },
]

/**
 * 读某项的字号倍率（没设过就用默认比例）。
 *
 * `font_scale` 整块为空时返回该字段的默认值，所以界面一打开显示的就是
 * **当前真实生效的比例**，而不是空白让用户猜。
 */
function fontScaleValue(field: keyof FontScaleConfig): number {
  const s = draft.value?.font_scale
  const v = s ? Number(s[field]) : NaN
  const def = FONT_SCALE_ITEMS.find((i) => i.field === field)?.def ?? 1
  return Number.isFinite(v) && v > 0 ? v : def
}

/**
 * 设定某项的字号倍率。
 *
 * 会把整块 `font_scale` 补齐（后端字段是一整个对象，不能只写一项），
 * 并把值夹在 0.3~6——0 会让文字消失、过大则把面板撑爆，
 * 两者都会让用户以为"面板坏了"。
 */
function setFontScaleOf(field: keyof FontScaleConfig, raw: string): void {
  if (!draft.value) return
  const parsed = Number(raw)
  if (!Number.isFinite(parsed)) return
  const value = Math.min(6, Math.max(0.3, Math.round(parsed * 100) / 100))
  const current = draft.value.font_scale
  const next: FontScaleConfig = {
    title: current?.title ?? 1.35,
    body: current?.body ?? 0.9,
    lyrics: current?.lyrics ?? 0.92,
    queue: current?.queue ?? 1.0,
    danmaku: current?.danmaku ?? 1.5,
  }
  next[field] = value
  draft.value.font_scale = next
}

/** 把全部字号倍率恢复成默认比例（`null` = 用面板写死的比例）。 */
function resetFontScale(): void {
  if (!draft.value) return
  draft.value.font_scale = null
}

const COLOR_ITEMS: Array<{ field: ColorField; label: string; hint: string }> = [
  { field: 'color', label: '主色', hint: '队列高亮、歌词当前行、弹幕用户名。改这一处即可换整体色调' },
  { field: 'fg', label: '字体色', hint: '歌手、点歌人、时间等正文颜色' },
  { field: 'titleColor', label: '歌名颜色', hint: '留空即跟随主色，只影响歌名' },
  { field: 'strokeColor', label: '描边颜色', hint: '留空即按字体色自动选深浅' },
  { field: 'barColor', label: '进度条色', hint: '留空即跟随主色' },
  { field: 'track', label: '进度条轨道', hint: '留空或 transparent = 不要轨道' },
  { field: 'surface', label: '卡片底色', hint: '留空或 transparent = 不要底色，直接叠在画面上' },
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

/*
 * 这里原本有一个 `includeBgInUrls` 开关（"是否把背景图写进面板地址"），
 * 连同界面上的「✓ 地址已带背景图」按钮。**已删除**，原因：
 *
 *  - 它是个**无效控件**：有声明、有 @click 切换，但**没有任何地方读它**，
 *    `urlFor()` 只写 `style=<id>` 与可选的 `scale`。点它只会让按钮文字
 *    在两个状态间跳，地址一点变化都没有；
 *  - 它描述的功能在命名样式体系下**已不存在**：背景图现在是样式的
 *    `bg_image` 字段，地址里只带 `style=<id>`，背景图自然跟着样式走，
 *    根本不存在"要不要写进地址"这个选择。
 */

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

// ── 背景图取景（拖动决定显示哪一块）──────────────────────────────────────

/**
 * 正在取景的图片；`null` = 关闭。
 *
 * ## 与「编辑」的分工
 * 「编辑」是**改图片本身**（裁剪/旋转/镜像后另存为新文件）；
 * 「取景」只改**样式里的 `bg_position`**——不动图片，之后还能随时再调。
 *
 * 两者都需要，因为它们的取舍不同：想把主体真正裁掉、减小文件用「编辑」；
 * 只想让面板显示图片的某个区域、又不想多存一份文件用「取景」。
 */
const panningBg = ref<BackgroundItem | null>(null)

function openPanEditor(bg: BackgroundItem): void {
  panningBg.value = bg
  // 若这张图还不是当前样式的背景图，顺手设上——否则调完位置看不到效果
  if (draft.value && draft.value.bg_image !== bg.url) {
    pickBackground(bg)
  }
}

function closePanEditor(): void {
  panningBg.value = null
}

/** 拖动时即时写进草稿（预览会立刻跟着变）。 */
function setBgPosition(value: string): void {
  if (!draft.value) return
  draft.value.bg_position = value
}

/**
 * 取景框用的比例 = 当前画布比例。
 *
 * ⚠️ 必须与画布尺寸一致（`previewCanvasW / previewCanvasH`）：
 * `cover` 的裁切完全由容器比例决定，取景框若用别的比例，
 * 用户在这里对好的位置到 OBS 里就会变样。
 *
 * 早期这里写死 `16/9`——在画布尺寸可配置之前还算对，
 * 现在必须跟着画布走。
 */
const panAspect = computed(() => previewCanvasW.value / previewCanvasH.value)

/**
 * 取景弹窗里那块"实际效果"预览的缩放比。
 *
 * 面板按 960 宽渲染，这一栏只有约 300px。取 `0.26` 让 960 → 250px，
 * 正好整幅放进去（留出内边距与边框），用户能一眼看到**完整**面板，
 * 而不是被裁掉右半边的半张图。
 *
 * 这一块只是辅助确认；精确对位靠左边的取景框。
 */
const panLiveScale = 0.26

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

/** 设置背景图不透明度（只影响背景图层，不影响文字）。 */
function setBgOpacity(raw: string): void {
  const n = Number(raw)
  if (!draft.value || !Number.isFinite(n)) return
  draft.value.bg_opacity = Math.min(Math.max(n, 0), 1)
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
 * 否则预览会停在一个已被隐藏的面板上。
 */
const previewKey = ref<PanelKey>('play')

// ── 当前背景图的等比预览 ────────────────────────────────────────────────

/** 当前背景图的宽高比（`width / height`），量不到时回落到 16:9。 */
const bgPreviewRatio = ref('16 / 9')

/**
 * 预览框的**实际像素尺寸**（在 JS 里算好，不靠 CSS 自动推导）。
 *
 * ## 为什么不交给 CSS
 * 试过两种纯 CSS 方案，都不成立：
 *  - `aspect-ratio: 850/1323` + `max-height: 320px` → 实测渲染成 1191×320
 *    （比例 3.7），因为高度被夹住后浏览器不再按比例回算宽度；
 *  - `padding-top: 155%` 撑高 + `max-height` → 高度被夹住了，但**宽度仍拉满**，
 *    得到 1191×1856 的巨块。
 *
 * 比例本来就是已知的（`bgPreviewRatio`），直接算最稳：
 * 高度封顶 320px，宽度按比例反算，再限制不超过容器宽度。
 * 容器宽度取不到时先给一个保守值，等 `onMounted` 量到后再刷新。
 */
const bgPreviewMaxW = ref(560)

/** 量一下预览所在卡片的可用宽度，避免小窗口下溢出。 */
const bgPreviewWrapEl = ref<HTMLElement | null>(null)
let bgPreviewRO: ResizeObserver | null = null
onMounted(() => {
  const el = bgPreviewWrapEl.value?.parentElement
  if (!el) return
  const sync = () => {
    const w = el.clientWidth
    if (w > 0) bgPreviewMaxW.value = w
  }
  sync()
  bgPreviewRO = new ResizeObserver(sync)
  bgPreviewRO.observe(el)
})
onBeforeUnmount(() => bgPreviewRO?.disconnect())

const bgPreviewBox = computed(() => {
  const [w, h] = bgPreviewRatio.value.split('/').map((v) => Number(v.trim()))
  const ratio = w > 0 && h > 0 ? w / h : 16 / 9
  const MAX_H = 320
  let height = MAX_H
  let width = height * ratio
  const maxW = bgPreviewMaxW.value
  if (width > maxW) {
    width = maxW
    height = width / ratio
  }
  return { w: Math.round(width), h: Math.round(height) }
})

/**
 * 量当前背景图的真实比例。
 *
 * 有比例才能让预览「等比显示整张图」而不是固定高度裁一条。
 * 用 `new Image()` 加载同一地址来量——与模板里的背景图各自加载，
 * 但浏览器会命中同一份缓存，不会真的多下一次。
 */
watch(
  () => draft.value?.bg_image,
  (url) => {
    if (!url) return
    const probe = new Image()
    probe.onload = () => {
      if (probe.naturalWidth > 0 && probe.naturalHeight > 0) {
        bgPreviewRatio.value = `${probe.naturalWidth} / ${probe.naturalHeight}`
      }
    }
    probe.src = absoluteBg(url)
  },
  { immediate: true },
)

/** 预览标题（跟随实际预览的面板，而不是用户点过但已关闭的那个）。 */
const previewLabel = computed(() => {
  const list = pages.value
  if (!list.length) return '（未启用任何面板）'
  const found = list.find((p) => p.key === previewKey.value)
  return (found ?? list[0]).label
})

// ── 预览（本地渲染，不用 iframe）──────────────────────────────────────

/**
 * 预览**可用空间**。
 *
 * ## ⚠️ 必须量一个"不受缩放影响"的元素
 * 早期这里量的就是 `.preview-frame` 本身，而那个元素的宽高是由**算出来的
 * 缩放比**设的 —— 于是形成循环依赖：量到高 240 → 算出比例 0.4 →
 * 把高度设成 240 → 再量还是 240，比例永远卡在 0.4 上，画布再小也不会放大。
 *
 * 现在改成量一个纯占位、尺寸只由栏宽决定的包装层，
 * 并把**限高**交给 CSS 的 `max-height`（不参与比例计算）。
 */
const previewBoxEl = ref<HTMLElement | null>(null)
const previewBoxSize = ref({ w: 0, h: 0 })

let previewRO: ResizeObserver | null = null

/**
 * 量可用宽度。
 *
 * ## ⚠️ 必须用 `watch` 而不是 `onMounted`
 * 这个元素在 `v-if="ready && draft"` 里面，而配置是**异步**加载的——
 * 组件挂载那一刻它还**不存在**，`previewBoxEl.value` 是 `null`，
 * 于是 `onMounted` 里直接 `return`，ResizeObserver 从来没被挂上，
 * `previewBoxSize` 永远是 `{ w: 0 }`。
 *
 * 后果很隐蔽：`previewScale` 每次都走兜底分支返回写死的 `0.4`，
 * 画布再小也不会放大（实测 400×600 本该是 1.0，却一直是 0.4）。
 * 而 1920×1080 恰好正确值是 0.4，所以**看起来一切正常**，掩盖了这个 bug。
 *
 * `flush: 'post'` 保证在 DOM 更新后再量。
 */
watch(
  previewBoxEl,
  (el, _old, onCleanup) => {
    previewRO?.disconnect()
    previewRO = null
    if (!el) return
    const sync = () => {
      previewBoxSize.value = { w: el.clientWidth, h: el.clientHeight }
    }
    sync()
    previewRO = new ResizeObserver(sync)
    previewRO.observe(el)
    onCleanup(() => {
      previewRO?.disconnect()
      previewRO = null
    })
  },
  { flush: 'post' },
)

onBeforeUnmount(() => {
  previewRO?.disconnect()
})

/**
 * 「1:1 实际大小」开关。
 *
 * ## 为什么需要它
 * 预览栏通常只有三四百像素宽，等比缩略后字很小。**但那正是真实 OBS 源里
 * 的样子**——把预览的字放大只会让它骗人（预览里看着刚好，OBS 里小一半）。
 *
 * 所以默认走等比（版式真实），另给一个 1:1 档：按画布真实尺寸渲染、
 * 超出容器就滚动。这样"看得清"与"等于 OBS"可以同时成立。
 *
 * ## ⚠️ 这个开关**不持久化**，每次进入页面都回到等比档
 * 早期它是普通 ref，用户点过一次 1:1 之后就一直停在那一档——而 1:1 下
 * 画面远超预览栏宽，于是"预览只有半张图、还带横向滚动条"，
 * 看起来像功能坏了（用户就这么反馈过）。现在进入页面时显式复位。
 */
const previewActualSize = ref(false)

/**
 * 预览画布尺寸，来自当前样式（`draft.canvas_*`）。
 *
 * ## 为什么这是"预览像不像 OBS"的关键
 * 面板的排版（换行、`wide` 是否左右分栏）与背景图 `cover` 的裁切，
 * **完全由容器尺寸决定**。所以这里必须等于**你 OBS 浏览器源的实际宽高**。
 *
 * 早期写死 `960×540`，而用户的实际源是 `400×600` 竖版——预览怎么调都对不上。
 */
const previewCanvasW = computed(() => {
  const v = Number(draft.value?.canvas_w)
  return Number.isFinite(v) && v > 0 ? v : 1920
})
const previewCanvasH = computed(() => {
  const v = Number(draft.value?.canvas_h)
  return Number.isFinite(v) && v > 0 ? v : 1080
})

/** 常用画布尺寸，一键切换（`400×600` 是竖版源的典型值）。 */
const CANVAS_PRESETS: Array<{ w: number; h: number; label: string }> = [
  { w: 1920, h: 1080, label: '1920×1080' },
  { w: 1280, h: 720, label: '1280×720' },
  { w: 960, h: 540, label: '960×540' },
  { w: 400, h: 600, label: '400×600' },
  { w: 600, h: 400, label: '600×400' },
]

/** 设定画布尺寸（限制在合理范围内，避免手滑输入 0 或超大值）。 */
function setCanvasSize(w: number, h: number): void {
  if (!draft.value) return
  const cw = Math.min(7680, Math.max(120, Math.round(w)))
  const ch = Math.min(4320, Math.max(120, Math.round(h)))
  draft.value.canvas_w = cw
  draft.value.canvas_h = ch
}

/** 当前是否正好是某个预设。 */
function isCanvasPreset(w: number, h: number): boolean {
  return previewCanvasW.value === w && previewCanvasH.value === h
}

// ── 预览栏宽度：可拖动 ───────────────────────────────────────────────────

/**
 * 预览栏宽度（px）。
 *
 * 早期是 CSS 里写死的 `380px`，而左边表单有 949px——**大部分是空的**，
 * 预览却被挤得看不清。现在可拖，并跟随草稿持久化。
 */
const asideWidth = computed({
  get: () => {
    const v = Number(draft.value?.preview_width)
    return Number.isFinite(v) && v > 0 ? v : 440
  },
  set: (v: number) => {
    if (!draft.value) return
    draft.value.preview_width = Math.min(900, Math.max(280, Math.round(v)))
  },
})

/** 分隔条是否正在拖动（拖动时禁用过渡、改变光标）。 */
const resizingAside = ref(false)

function startAsideResize(ev: PointerEvent): void {
  resizingAside.value = true
  const startX = ev.clientX
  const startW = asideWidth.value
  // 分隔条在预览栏**左侧**，向左拖 = 预览变宽，所以用减法
  const onMove = (e: PointerEvent) => {
    asideWidth.value = startW - (e.clientX - startX)
  }
  const onUp = () => {
    resizingAside.value = false
    window.removeEventListener('pointermove', onMove)
    window.removeEventListener('pointerup', onUp)
  }
  window.addEventListener('pointermove', onMove)
  window.addEventListener('pointerup', onUp)
  ev.preventDefault()
}

/**
 * 「放大预览」整页模式：预览占满整个内容区，表单暂时让位。
 *
 * 这是唯一能让**大画布（如 1920×1080）也按 1:1 看**的办法——
 * 侧栏再宽也放不下。
 */
const previewExpanded = ref(false)

// ── 右下角浮动操作（重置 / 保存）────────────────────────────────────────

/**
 * 页头元素，用于观察它是否还在视野里。
 *
 * 用 `ref` + `watch`（`flush: 'post'`）而不是 `onMounted`：页头在
 * `v-if` 之类的条件里，挂载那一刻可能还不存在（本项目已因这个坑栽过一次，
 * 见 `previewBoxEl` 的说明）。
 */
const pageHeadEl = ref<HTMLElement | null>(null)

/**
 * 页头是否**完全**在视野内。
 *
 * 初始 `true`：页面刚打开时页头就在顶部，浮动按钮不该出现。
 */
const headVisible = ref(true)

/**
 * 是否显示右下角浮动操作。
 *
 * ## 为什么用 `IntersectionObserver` 而不是比较 `scrollTop`
 * 页头的高度会变（标题换行、按钮换行、窗口宽度变化），写死一个阈值
 * 必然在某处不准。观察元素本身则自动适应所有情况。
 *
 * `threshold: 0` + 判断 `isIntersecting`：**只要页头有一点点离开视野**
 * 就显示浮动按钮，不会有"顶部按钮已经被切掉一半、浮动按钮却还没出来"的空档。
 *
 * 滚动容器是 `.app-body`（不是视口），所以必须把 `root` 明确指过去——
 * 否则观察的是视口交叉，而页头一直在视口里（只是被容器裁掉了），永远不触发。
 */
const showFloatingActions = computed(
  () => ready.value && !!draft.value && !showBackgroundsOnly.value && !headVisible.value,
)

let headObserver: IntersectionObserver | null = null

watch(
  [pageHeadEl, () => showBackgroundsOnly.value],
  ([el, backgroundsOnly], _old, onCleanup) => {
    headObserver?.disconnect()
    headObserver = null
    // 背景图库页没有这两个操作，不需要观察
    if (!el || backgroundsOnly) {
      headVisible.value = true
      return
    }
    const root = el.closest('.app-body')
    headObserver = new IntersectionObserver(
      (entries) => {
        const entry = entries[entries.length - 1]
        if (entry) headVisible.value = entry.isIntersecting
      },
      { root: root instanceof Element ? root : null, threshold: 0 },
    )
    headObserver.observe(el)
    onCleanup(() => {
      headObserver?.disconnect()
      headObserver = null
    })
  },
  { flush: 'post' },
)

/*
 * 「1:1 实际大小」开关**每次进入页面都复位到等比档**。
 *
 * ## 为什么必须复位
 * 早期它是个普通 ref，用户点过一次 1:1 之后就**一直停在那一档**。
 * 而 1:1 下画面远超预览栏宽，于是表现为"预览只有半张图、还带横向滚动条"，
 * 看起来像功能坏了（用户就是这么反馈的）。
 *
 * 它只是临时查看手段、不是偏好设置，所以不持久化。
 * 这里不能直接写 `previewActualSize.value = false`——那样在 setup 阶段
 * 赋值没有意义（本来就是 false），必须在挂载时执行才能覆盖"上次离开时的状态"。
 */
onMounted(() => {
  previewActualSize.value = false
})

/**
 * 等比档的缩放比；1:1 档恒为 1。
 *
 * 只按**可用宽度**算（不按高度）——高度那一路会和"高度由缩放决定"互相
 * 牵扯成循环，见上面 `previewBoxEl` 的说明。特别高的预览由 CSS 的
 * `max-height` + 滚动兜住，不影响比例。
 */
const previewScale = computed(() => {
  if (previewActualSize.value) return 1
  const w = previewBoxSize.value.w
  if (!w) return 0.4
  return Math.min(w / previewCanvasW.value, 1)
})

/** 缩放后画布的实际显示尺寸。 */
const previewDisplayW = computed(() => Math.max(1, Math.round(previewCanvasW.value * previewScale.value)))
const previewDisplayH = computed(() => Math.max(1, Math.round(previewCanvasH.value * previewScale.value)))

/**
 * 预览用的**已解析样式**——直接来自当前草稿。
 *
 * ## 这就是"实时预览"
 * 因为是本地渲染，草稿一变这个 computed 就重算，面板立刻重绘。
 * 没有 iframe、没有跨源消息、没有存储中转，也不需要先保存。
 * （早期用 iframe 时这三条路都试过，全部因为跨源隔离而失败，
 * 详见 `PanelPreview.vue` 顶部说明。）
 */
const previewResolvedStyle = computed(() =>
  draft.value
    ? styleToResolved(draft.value, null)
    : styleToResolved(DEFAULT_PANEL as PanelStyleConfig, null),
)



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
    <!-- 统一导航：左边控制台标签，右边「设置」（顶栏已不再有页级导航） -->
    <NavTabs :model-value="null" />

    <div ref="pageHeadEl" class="page-head">
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
      <div
        class="editor-layout"
        :class="{ 'is-expanded': previewExpanded, 'is-resizing': resizingAside }"
        :style="{ '--aside-w': `${asideWidth}px` }"
      >
        <div class="editor-main">
      <template v-if="!showBackgroundsOnly">
      <!-- ── 样式列表（命名样式）────────────────────────────────────── -->
      <section class="card">
        <div class="card-head">
          <h3>样式方案</h3>
          <div class="controls">
            <button class="ghost" :disabled="styleBusy" @click="openCreate(null)">+ 新建空白</button>
            <button class="ghost" :disabled="styleBusy" @click="openCreate(draft.id)">
              从当前复制
            </button>
          </div>
        </div>
        <!-- 说明只留一句；"为什么只写 ?style=" 这类设计理由不占版面 -->
        <p class="dim">自定义方案预设
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
                  <span v-if="s.id === defaultStyleId" class="tag-default">使用中</span>
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
                使用方案
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

        <h4>画布</h4>
        <!--
          画布尺寸 = **你 OBS 浏览器源的实际宽高**。
          面板排版（换行、wide 分栏）与背景图 cover 的裁切完全由容器尺寸决定，
          所以这个值必须对得上，否则"预览里看着刚好、OBS 里完全两样"。
          它**不影响 OBS 的实际显示**，只决定预览按多大渲染。
        -->
        <div class="canvas-row">
          <input
            type="number"
            class="canvas-num"
            :value="previewCanvasW"
            min="120"
            max="7680"
            title="你 OBS 浏览器源的宽度"
            @change="setCanvasSize(Number(($event.target as HTMLInputElement).value), previewCanvasH)"
          />
          <span class="canvas-x">×</span>
          <input
            type="number"
            class="canvas-num"
            :value="previewCanvasH"
            min="120"
            max="4320"
            title="你 OBS 浏览器源的高度"
            @change="setCanvasSize(previewCanvasW, Number(($event.target as HTMLInputElement).value))"
          />
          <div class="canvas-presets">
            <button
              v-for="c in CANVAS_PRESETS"
              :key="c.label"
              class="ghost mini-preset"
              :class="{ active: isCanvasPreset(c.w, c.h) }"
              :title="`设为 ${c.label}`"
              @click="setCanvasSize(c.w, c.h)"
            >
              {{ c.label }}
            </button>
          </div>
        </div>

        <!--
          字号：基础字号 + 各元素的相对倍率。
          ⚠️ 必须放在「颜色」**之前**——它比颜色更常调，
          而且改完能立刻在右侧预览里看到，是这一页最常用的两处之一。
        -->
        <h4>字号</h4>
        <div class="fields">
          <label>基础字号
            <input
              type="number"
              min="8"
              max="72"
              v-model.number="draft.font_size"
              title="整套面板的字号基数；下面各项都是它的倍数"
              @change="syncStyleParam('fontSize', String(draft.font_size))"
            />
          </label>
          <label
            v-for="f in FONT_SCALE_ITEMS"
            :key="f.field"
            :title="`${f.hint}。倍数相对「基础字号」，默认 ${f.def}`"
          >
            {{ f.label }}
            <input
              type="number"
              step="0.05"
              min="0.3"
              max="6"
              :value="fontScaleValue(f.field)"
              @input="setFontScaleOf(f.field, ($event.target as HTMLInputElement).value)"
            />
          </label>
          <label class="check">
            <button class="ghost" title="把各元素字号恢复成默认比例" @click="resetFontScale()">
              恢复默认比例
            </button>
          </label>
        </div>

        <h4>颜色</h4>
        <div class="color-grid">
          <!--
            颜色项。
            `hint` 走 `title`（悬停提示），**不再渲在项下面**——每个项下面挂一行
            小字会把整块拉得很高、也显得杂乱。悬停即可看到同样的信息。
          -->
          <label v-for="c in COLOR_ITEMS" :key="c.field" class="color-item" :title="c.hint">
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
          </label>
        </div>
        <p v-if="fgConflict" class="warn">
          ⚠️ 当前是<strong>{{ draft.theme === 'dark' ? '深色' : '浅色' }}</strong>主题，
          但字体色偏{{ draft.theme === 'dark' ? '暗' : '亮' }}，文字可能看不清。
          深色主题建议用浅色字，浅色主题建议用深色字。
        </p>

        <!--
          快捷操作。按钮文案刻意只用 3~4 个字——原来分别是
          「字体色用主题推荐值（#f2f2f2）」和「全部透明（只留文字与进度条）」，
          把色值和整句解释都塞进了按钮，一眼看不出是干什么的。
          解释改到 title（悬停可见）。
        -->
        <div class="controls">
          <button
            class="ghost"
            :title="`把字体色设为当前主题的推荐值（${themeFg}），避免手调出看不清的组合`"
            @click="setColor('fg', themeFg)"
          >
            推荐字体色
          </button>
          <button
            class="ghost"
            title="把卡片底色与进度条轨道都设为透明，只留下文字和进度条"
            @click="makeAllTransparent()"
          >
            全部透明
          </button>
        </div>
      </section>

      <!-- ── 文字：字重 + 描边 ─────────────────────────────────────── -->
      <section class="card">
        <div class="card-head">
          <h3>文字</h3>
          <div class="controls">
            <button class="ghost" :disabled="!ready" @click="resetTypography()">恢复默认</button>
          </div>
        </div>
        <p class="dim">叠在画面上时加描边更清楚；宽度 0 即不描边。</p>

        <h4>字重</h4>
        <div class="fields">
          <label>歌名
            <select v-model.number="draft.font_weight_title" @change="syncStyleParam('fontSize', String(draft.font_size))">
              <option v-for="w in WEIGHTS" :key="w.value" :value="w.value">{{ w.label }}</option>
            </select>
          </label>
          <label>正文（歌手、队列、歌词）
            <select v-model.number="draft.font_weight" @change="syncStyleParam('fontSize', String(draft.font_size))">
              <option v-for="w in WEIGHTS" :key="w.value" :value="w.value">{{ w.label }}</option>
            </select>
          </label>
          <label>小字（点歌人、时间）
            <select v-model.number="draft.font_weight_sub" @change="syncStyleParam('fontSize', String(draft.font_size))">
              <option v-for="w in WEIGHTS" :key="w.value" :value="w.value">{{ w.label }}</option>
            </select>
          </label>
        </div>

        <h4>描边</h4>
        <div class="fields">
          <label>宽度
            <input
              type="number"
              min="0"
              max="6"
              step="0.5"
              v-model.number="draft.text_stroke_width"
              @change="syncStyleParam('fontSize', String(draft.font_size))"
            />
          </label>
          <label>颜色
            <span class="color-row">
              <input
                type="color"
                :value="draft.text_stroke_color ?? '#000000'"
                @input="setColor('strokeColor', ($event.target as HTMLInputElement).value)"
              />
              <span class="mono">{{ draft.text_stroke_color ?? '自动' }}</span>
              <button
                v-if="draft.text_stroke_color"
                class="mini"
                title="改回自动（按字体色选）"
                @click="clearColor('strokeColor')"
              >✕</button>
            </span>
          </label>
        </div>
        <div class="controls">
          <button class="ghost" @click="applyStrokePreset(1.5)">细描边 1.5px</button>
          <button class="ghost" @click="applyStrokePreset(3)">粗描边 3px</button>
          <button class="ghost" @click="draft.text_stroke_width = 0">不要描边</button>
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
          <label
            class="check"
            :title="
              draft.layout === 'compact'
                ? '精简单行布局本身不显示歌词，这一项当前不起作用'
                : '关掉后面板上整个歌词区都不渲染'
            "
          >
            <input
              type="checkbox"
              v-model="draft.show_lyrics"
              :disabled="draft.layout === 'compact'"
              @change="syncStyleParam('showLyrics', draft.show_lyrics ? 'true' : 'false')"
            /> 显示歌词
            <!--
              精简单行布局下这个开关**必然无效**——`PanelView.vue` 里判定是
              `showLyrics && layout !== 'compact'`。不提示的话用户会以为功能坏了
              （用户反馈「我不知道这个显示歌词有什么用」正是这个原因）。
            -->
            <span v-if="draft.layout === 'compact'" class="dim">（精简单行不显示歌词）</span>
          </label>
        </div>
      </section>

      </template>
      <!-- ── 背景图（阶段 9，阶段 10b 改为下拉选择 + 预览）────────── -->
      <section class="card">
        <div class="card-head">
          <h3>背景图</h3>
          <div class="controls">
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
        <h4>不透明度</h4>
        <div class="opacity-row">
          <input
            type="range"
            min="0"
            max="1"
            step="0.05"
            :value="draft.bg_opacity"
            :disabled="!draft.bg_image"
            @input="setBgOpacity(($event.target as HTMLInputElement).value)"
          />
          <span class="mono">{{ Math.round(draft.bg_opacity * 100) }}%</span>
          <button
            v-if="draft.bg_opacity !== 1"
            class="mini"
            title="恢复 100%"
            @click="setBgOpacity('1')"
          >✕</button>
        </div>
        <p v-if="!draft.bg_image" class="dim">（当前没有选背景图）</p>

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
              :title="`${bg.name} · 点击使用这张图`"
              :style="{ backgroundImage: `url(${absoluteBg(bg.url)})` }"
              @click="pickBackground(bg)"
            >
              <!-- 正在被当前样式使用：给个角标，避免误删 -->
              <span v-if="draft.bg_image === bg.url" class="bg-badge">正在使用</span>
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
                使用
              </button>
              <button
                class="ghost"
                title="拖动图片，决定背景图显示哪一块（cover 会裁掉一部分，默认居中常裁到主体）"
                @click="openPanEditor(bg)"
              >
                取景
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
        <div v-if="draft.bg_image" ref="bgPreviewWrapEl" class="bg-preview-wrap">
          <div
            class="bg-preview"
            :style="{
              width: `${bgPreviewBox.w}px`,
              height: `${bgPreviewBox.h}px`,
              '--bg-preview-image': `url(${absoluteBg(draft.bg_image)})`,
            }"
          />
        </div>
      </section>

      <template v-if="!showBackgroundsOnly">
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
            </div>
          </li>
        </ul>
      </section>

      </template>
      <!--
        这里曾有一个「高级：地址参数」编辑器（手写 ?color=/?theme= 之类的覆盖）。
        命名样式上线后，面板地址**只写 `?style=<id>`**，不再携带任何外观参数——
        那个编辑器已经和地址生成脱节：改它不会影响任何地址，却会让人以为改了有用
        （实测还造成过"列表色块是绿的、取色器却显示粉色"的不一致）。
        所以整块移除。要改外观请直接改上面的样式，然后「保存样式」。
      -->

      <template v-if="!showBackgroundsOnly">
      <!-- ── 面板启停（阶段 9）────────────────────────────────────── -->
      <section class="card">
        <h3>要使用哪些面板</h3>
        <p class="dim">只影响下面列出哪几个地址，不会禁用任何面板。</p>
        <div class="fields">
          <label v-for="p in PANEL_LIST" :key="p.key" class="check">
            <input type="checkbox" v-model="enabledPanels[p.key]" />
            {{ p.label }}
            <span class="dim">（{{ p.desc }}）</span>
          </label>
        </div>
      </section>
      </template>
        </div>
        <!-- /.editor-main -->

        <!--
          ── 预览（吸附右栏）──────────────────────────────────────────
          ⚠️ 只在「OBS 面板」子标签显示。
          「背景图库」页只管上传/裁剪/删除图片，那里放一份面板预览没有意义，
          还会把图片网格挤窄。这与需求一致：双栏吸附预览属于 OBS 面板页。
        -->
        <!--
          拖动分隔条调整预览栏宽度。
          早期这里是 CSS 写死的 380px，而左边表单有 949px（大部分是空的），
          预览被挤得看不清。宽度跟随草稿持久化。
        -->
        <div
          v-if="!showBackgroundsOnly && !previewExpanded"
          class="aside-resizer"
          role="separator"
          aria-orientation="vertical"
          title="拖动调整预览栏宽度"
          @pointerdown="startAsideResize"
        />

        <aside v-if="!showBackgroundsOnly" class="editor-aside">
          <section class="card preview-card">
            <div class="card-head">
              <h3>预览 · {{ previewLabel }}</h3>
              <div class="controls">
                <button
                  class="ghost"
                  :title="
                    previewActualSize
                      ? '切回等比缩放（整块都看得见，版式仍与 OBS 一致）'
                      : `按 ${previewCanvasW}×${previewCanvasH} 真实尺寸显示，超出部分可滚动——这是 OBS 里的实际大小`
                  "
                  @click="previewActualSize = !previewActualSize"
                >
                  {{ previewActualSize ? '等比缩放' : '1:1 实际大小' }}
                </button>
                <button
                  class="ghost"
                  :title="
                    previewExpanded
                      ? '收回侧栏（恢复表单与预览并排）'
                      : '预览占满整个内容区，大画布也能按 1:1 看'
                  "
                  @click="previewExpanded = !previewExpanded"
                >
                  {{ previewExpanded ? '收回' : '放大' }}
                </button>
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
            <!--
              预览是**本地渲染**的（不再用 iframe）：
              iframe 与设置页跨源，既做不到"改样式立刻可见"，
              宽度也和一个真实的 OBS 画布差太远。见 `PanelPreview.vue`。
            -->
            <div ref="previewBoxEl" class="preview-box">
              <div
                class="preview-frame"
                :class="{ 'is-actual': previewActualSize }"
                :style="{
                  '--pv-scale': String(previewScale),
                  width: `${previewDisplayW}px`,
                  height: `${previewDisplayH}px`,
                }"
              >
                <PanelPreview
                  :style="previewResolvedStyle"
                  :page="previewKey === 'all' ? 'all' : previewKey"
                  :canvas-w="previewCanvasW"
                  :canvas-h="previewCanvasH"
                />
              </div>
            </div>
            <p class="dim preview-hint">
              <template v-if="previewActualSize">
                1:1 实际大小，可滚动——<strong>这就是 OBS 里的真实尺寸</strong>。
              </template>
              <template v-else>
                等比缩放，版式与 OBS 一致；字偏小属正常（= 真实源里的字大小）。
              </template>
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
          <template v-else>建立一套默认外观的空白样式。</template>
        </p>
        <label class="modal-field">
          方案名
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
          方案名
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
        <h4>删除方案</h4>
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
      右下角浮动的「重置 / 保存样式」。
      页头不吸顶了，所以滚到下面改完样式时靠它操作，不必滚回顶部。

      ## 为什么只在页头滚出视野后才出现
      一直飘着会挡住右下角的内容（尤其背景图库的缩略图和右栏预览底部）。
      顶部本来就有同样的两个按钮，**看不到它们时才需要这个替补**。
      用 `IntersectionObserver` 观察页头，而不是比较 `scrollTop`——
      这样窗口大小变化、内容增减都自动适应，不必维护一个阈值。

      只在「OBS 面板」子标签显示（背景图库页没有样式可存）。
    -->
    <Transition name="float-fade">
      <div v-if="showFloatingActions" class="floating-actions">
        <button
          class="floating-btn"
          :disabled="!ready"
          title="放弃改动，恢复成已保存的样式"
          @click="resetDraft()"
        >
          重置
        </button>
        <button
          class="floating-btn is-primary"
          :class="{ 'is-saved': saved }"
          :disabled="!ready || saving"
          title="保存当前样式（改完样式不用滚回顶部）"
          @click="save()"
        >
          {{ saving ? '保存中…' : saved ? '✓ 已保存' : '保存样式' }}
        </button>
      </div>
    </Transition>

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

    <!--
      背景图取景。只改样式里的 `bg_position`，**不动图片文件**——
      与上面那个"改图片本身"的编辑器分工不同。见 `BgPositionEditor.vue`。
    -->
    <div v-if="panningBg" class="pan-mask" role="dialog" aria-modal="true">
      <div class="pan-dialog">
        <header>
          <div>
            <h3>调整背景图取景</h3>
            <p class="dim">
              按住图片拖动，决定面板显示图片的哪一块。框内即面板实际看到的区域。
            </p>
          </div>
          <div class="controls">
            <button class="ghost" title="把取景位置复位到图片正中" @click="setBgPosition('50% 50%')">
              回到正中
            </button>
            <button class="ghost" @click="closePanEditor()">完成</button>
          </div>
        </header>

        <div class="pan-body">
          <div class="pan-preview-col">
            <BgPositionEditor
              :src="absoluteBg(panningBg.url)"
              :aspect="panAspect"
              :position="draft?.bg_position || '50% 50%'"
              @update="setBgPosition"
            />
            <p class="dim mono-line">取景位置：{{ draft?.bg_position || '50% 50%' }}</p>
          </div>

          <div class="pan-side">
            <h4>面板里的实际效果</h4>
            <p class="dim">这一块与 OBS 里显示的一致。</p>
            <div class="pan-live" :style="{ '--pv-scale': String(panLiveScale) }">
              <PanelPreview
                :style="previewResolvedStyle"
                :page="previewKey === 'all' ? 'all' : previewKey"
                :canvas-w="previewCanvasW"
                :canvas-h="previewCanvasH"
              />
            </div>
          </div>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
/* ── 背景图取景弹窗 ──────────────────────────────────────────────────── */

.pan-mask {
  position: fixed;
  inset: 0;
  z-index: 80;
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 24px;
  background: rgb(90 68 80 / 45%);
  backdrop-filter: blur(2px);
}

.pan-dialog {
  width: min(860px, 100%);
  max-height: 100%;
  overflow: auto;
  padding: 16px 18px;
  border: 1px solid var(--bsr-border);
  border-radius: 12px;
  background: var(--bsr-bg-elevated);
  box-shadow: 0 18px 48px rgb(90 68 80 / 24%);
}

.pan-dialog header {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: 16px;
  flex-wrap: wrap;
  margin-bottom: 14px;
}

.pan-dialog h3 {
  margin: 0;
  font-size: 15px;
}

.pan-dialog h4 {
  margin: 0 0 6px;
  font-size: 13px;
}

/* 左边取景框 / 右边实际效果 */
.pan-body {
  display: grid;
  grid-template-columns: minmax(0, 2fr) minmax(0, 1fr);
  gap: 18px;
  align-items: start;
}

@media (max-width: 780px) {
  .pan-body {
    grid-template-columns: minmax(0, 1fr);
  }
}

.pan-preview-col {
  min-width: 0;
}

.mono-line {
  font-family: ui-monospace, 'Cascadia Mono', Consolas, monospace;
}

/*
 * 「面板里的实际效果」。
 * 面板按 960 宽渲染，这一栏放不下，所以用固定的小缩放比装进去并裁掉溢出——
 * 它只是辅助确认，精确对位靠左边的取景框。
 */
.pan-live {
  overflow: hidden;
  height: 170px;
  border: 1px solid var(--bsr-border);
  border-radius: 8px;
  background: #f6e9ef;
}

.panels-page {
  margin-top: 20px;
  display: flex;
  flex-direction: column;
  gap: 14px;
}

/* ── 双栏：左编辑 / 右吸附预览 ─────────────────────────────────────────── */

.editor-layout {
  display: grid;
  /*
   * 三列：表单 | 拖动条 | 预览。
   *
   * 右栏宽度由 `--aside-w` 给出（可拖动，跟随草稿持久化）。
   * 早期写死 `380px`，而左边表单实测有 949px——**大部分是空的**，
   * 预览却被挤得看不清，用户反馈"看着好难受"。
   */
  grid-template-columns: minmax(0, 1fr) 8px var(--aside-w, 440px);
  gap: 16px;
  align-items: start;
}

/* 拖动中：禁用过渡与文本选中，避免拖起来"粘手" */
.editor-layout.is-resizing {
  user-select: none;
  cursor: col-resize;
}

/*
 * 「放大预览」整页模式：预览占满整个内容区，表单暂时让位。
 * 这是唯一能让**大画布（如 1920×1080）也按 1:1 看**的办法——
 * 侧栏再宽也放不下。
 */
.editor-layout.is-expanded {
  grid-template-columns: minmax(0, 1fr);
}

.editor-layout.is-expanded .editor-main {
  display: none;
}

/*
 * 预览栏宽度拖动条。
 * 视觉上是一条很轻的竖线，hover/拖动时才明显——不抢注意力，
 * 但鼠标移上去能看出"这里可以拖"（`cursor: col-resize`）。
 */
.aside-resizer {
  align-self: stretch;
  min-height: 240px;
  border-radius: 999px;
  background: transparent;
  cursor: col-resize;
  transition: background 0.12s ease;
}

.aside-resizer:hover,
.editor-layout.is-resizing .aside-resizer {
  background: var(--bsr-accent-soft);
}

/*
 * 「背景图库」页没有右栏预览，多出来的两列会把图片网格挤窄 —— 改成单栏。
 * 用 `:has()` 按"有没有右栏"来决定，比再传一个 class 更不容易忘记同步。
 */
.editor-layout:not(:has(.editor-aside)) {
  grid-template-columns: minmax(0, 1fr);
}

/*
 * 画布尺寸行：`画布 [1920] × [1080]  [预设…]`
 *
 * 它决定预览按多大渲染，也就决定"预览像不像 OBS"。
 */
.canvas-row {
  display: flex;
  align-items: center;
  gap: 6px;
  flex-wrap: wrap;
  font-size: 12px;
}

.canvas-label {
  color: var(--bsr-muted);
}

.canvas-num {
  width: 68px;
  padding: 3px 6px;
  border: 1px solid var(--bsr-border);
  border-radius: 6px;
  background: var(--bsr-bg-elevated);
  color: var(--bsr-fg);
  font-size: 12px;
}

.canvas-x {
  color: var(--bsr-muted);
}

.canvas-presets {
  display: flex;
  gap: 4px;
  flex-wrap: wrap;
}

/*
 * 预设小按钮。基础样式必须写全——`.preview-card` 不在 `.controls` 里，
 * 不写就退回浏览器默认外观（这个坑在本文件踩过好几次）。
 */
.mini-preset {
  padding: 2px 7px;
  border: 1px solid var(--bsr-border);
  border-radius: 6px;
  background: var(--bsr-bg-elevated);
  color: var(--bsr-muted);
  font-size: 11px;
  line-height: 1.6;
  cursor: pointer;
  transition: border-color 0.12s ease, background 0.12s ease, color 0.12s ease;
}

.mini-preset:hover {
  border-color: var(--bsr-accent);
  color: var(--bsr-fg);
}

.mini-preset.active {
  border-color: var(--bsr-accent);
  background: var(--bsr-accent-soft);
  color: var(--bsr-accent);
}

.editor-main {
  display: flex;
  flex-direction: column;
  gap: 14px;
  min-width: 0;
}

.editor-aside {
  /*
   * 吸附：这是「调样式还得滚下去看」的解法——调样式时预览始终可见。
   *
   * `top` **只等于导航高度**：页头已经不吸顶了（见上面 `.page-head` 的说明），
   * 所以右栏直接贴在导航条下面即可。
   *
   * ⚠️ 实测关系：sticky 的 `top: X` 最终停在 `45 + X`（45 是滚动容器
   * 客户区上边）。导航 `top:0` 停 45、高 50、底 95；这里 `top: 51` 停在 96，
   * 正好贴住导航底且留 1px 缝。
   *
   * 早期写成 `calc(导航 + 页头)`，公式本身没错，但因为页头在文档流里本就
   * 在右栏上方 36px，**两者必然不同步吸附**，滚动时会"晃一下"。
   */
  position: sticky;
  top: var(--bsr-nav-h, 51px);
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

/*
 * 预览上方的面板切换按钮。
 *
 * ⚠️ 以前这里**一条基础样式都没写**（只有下面那条 `.active`），所以按钮
 * 退回浏览器默认外观（灰底立体边框），和整套粉白主题明显不搭。
 * 根因是老问题：本文件里按钮的基础样式都挂在 `.controls button` 这类
 * 选择器上，不在其中的按钮就会"裸奔"。这里补齐。
 */
.preview-tabs button {
  padding: 3px 10px;
  border: 1px solid var(--bsr-border);
  border-radius: 6px;
  background: var(--bsr-bg-elevated);
  color: var(--bsr-fg);
  font-size: 11.5px;
  line-height: 1.6;
  cursor: pointer;
  transition: border-color 0.12s ease, background 0.12s ease, color 0.12s ease;
}

.preview-tabs button:hover {
  border-color: var(--bsr-accent);
  background: var(--bsr-accent-soft);
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

/*
 * 双栏区的预览框高度。
 *
 * ⚠️ 从固定 `400px` 改成 **16:9 等比**。
 * 固定高度配栏宽（380px）会得到一个 380×400 的近方形窗口，而 OBS 画布是
 * 1920×1080——把 16:9 的内容塞进近方形窗口，用户看到的就是「预览比例不对」。
 * 现在宽度由栏宽决定、高度按 16:9 自动算，比例与 OBS 一致。
 */
/*
 * 预览容器（两栏布局的右栏）。
 *
 * 高度由内联样式给出：
 *  - 等比档：按容器宽度 × 9/16 算，正好放下缩放后的画布；
 *  - 1:1 档：给一个固定高度（420px），由内容自己滚动。
 *
 * 这里**不写 aspect-ratio**——它只在等比档成立，1:1 档会把容器压变形。
 */
/*
 * 预览框尺寸由内联样式给出（= 画布 × 缩放比）。
 * 这里只负责边框、裁切与底纹。
 *
 * ⚠️ 不要再写 `width: 100%`：等比档下宽度正好等于缩放后的画布宽，
 * 1:1 档下则可能超出栏宽（那时才需要滚动）。
 */
/*
 * 预览可用空间。只是个占位/量尺寸的容器：宽度等于卡片内容宽，
 * 高度不限（因此不会反过来影响缩放比）。
 */
.preview-box {
  width: 100%;
  min-width: 0;
}

.preview-frame {
  max-width: 100%;
  /* 限高：特别高的画布（如 400×900）不会把卡片撑破，超出部分可滚动 */
  max-height: 70vh;
  border: 1px solid var(--bsr-border);
  border-radius: 8px;
  /*
   * ⚠️ 基础态必须是 hidden，**不能**是 auto。
   *
   * 等比档下预览框的宽度正好等于「缩放后的画布宽」，而边框 + 可能出现的
   * 纵向滚动条会让 clientWidth 比它小十几个像素。若这里用 `auto`，
   * 就会因为「内容宽 400 > 客户区 383」判出一个**横向滚动条**，
   * 而那条滚动条又进一步吃掉宽度——实测 400 宽的画布只显示出 383，
   * 看着像内容溢出了，其实面板内部一个溢出元素都没有。
   *
   * 只有 1:1 档确实需要滚动，那里单独开（见 `.is-actual`）。
   */
  overflow: hidden;
  background: #f6e9ef;
}

/*
 * 1:1 实际大小：画布按 960×540 原尺寸渲染，容器放不下就滚动。
 *
 * 用 `overflow: auto` 而不是缩放——这样量到的字号/换行**与 OBS 完全一致**，
 * 是唯一"绝对可信"的预览模式。
 */
/* 1:1 档：画布可能比栏宽大得多，必须能滚动 */
.preview-frame.is-actual {
  overflow: auto;
  /* 棋盘格：透明背景是否生效一眼可见（对应 OBS 里勾选"透明背景"） */
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

/*
 * 窄屏回落单栏：预览回到内容下方（且不再吸附，
 * 否则在小窗口里会盖住表单）。
 */
@media (max-width: 1080px) {
  .editor-layout,
  .editor-layout.is-expanded {
    grid-template-columns: minmax(0, 1fr);
  }

  .editor-aside {
    position: static;
  }

  /* 单栏时没有左右可分，拖动条没有意义 */
  .aside-resizer {
    display: none;
  }

  /* 整页模式在单栏下本来就是单栏，恢复显示表单，否则会"什么都看不到" */
  .editor-layout.is-expanded .editor-main {
    display: flex;
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

/*
 * 样式列表右侧的操作按钮。
 *
 * ⚠️ 这里原来**只写了 padding 与字号**，没有边框/圆角/底色——于是按钮退回
 * 浏览器默认外观。表现很有迷惑性：`.danger`（删除）因为全局有规则看着正常，
 * 而 `.ghost`（使用方案 / 重命名）却带着一条突兀的默认边框，
 * 同一个列表里的按钮**看起来不像一套**。
 *
 * 这是本文件反复出现的同一个坑：按钮的基础样式散落在 `.controls button`
 * 之类的选择器上，不在其中的就"裸奔"。这里把基础样式补全。
 */
.style-ops button {
  padding: 4px 10px;
  border: 1px solid var(--bsr-border);
  border-radius: 6px;
  background: var(--bsr-bg-elevated);
  color: var(--bsr-fg);
  font-size: 12px;
  line-height: 1.5;
  cursor: pointer;
  transition: border-color 0.12s ease, background 0.12s ease, color 0.12s ease;
}

.style-ops button:hover:not(:disabled) {
  border-color: var(--bsr-accent);
  background: var(--bsr-accent-soft);
}

.style-ops button:disabled {
  opacity: 0.45;
  cursor: not-allowed;
}

/* 删除按钮：危险色，与其它操作区分，降低误点 */
.style-ops button.danger {
  border-color: var(--bsr-danger);
  background: transparent;
  color: var(--bsr-danger);
}

.style-ops button.danger:hover:not(:disabled) {
  background: color-mix(in srgb, var(--bsr-danger) 12%, transparent);
  color: var(--bsr-danger);
}

/*
 * 页头（标题 + 「重置 / 保存样式」）。
 *
 * ## ⚠️ 这里**故意不吸顶**
 * 吸顶试过两版都有问题：
 *
 * **第一版**：`position: sticky` + **不透明底色**。滚轮往下时下面的卡片从它
 * 底下经过被整块盖住——用户反馈「这一条挡住我往下滚动的界面」。
 *
 * **第二版**：改成半透明 + 模糊后不挡了，但仍然"晃"。实测原因不在样式，
 * 而在**结构与文档流**：
 *
 * ```text
 * sticky 的 top 值 X → 最终停在 (45 + X)，45 是滚动容器的客户区上边
 * 导航 top:0   → 停 45，高 50，底 95
 * 页头 top:51  → 停 96          （贴导航底，正确）
 * 右栏 top:87  → 停 132         （贴页头底，正确）
 * ```
 *
 * 每个值单独看都对，但**吸附时机不同**：页头在 `scrollTop≈36` 吸住，
 * 右栏要到 `≈56`。中间那 20px 里页头已固定、右栏还在继续上滑，
 * 于是「晃动一下才吸附」。
 *
 * 根因是**文档流**:右栏本来就在页头下方 36px，页头吸住后它必须再往上走
 * 36px 才能贴到页头底——所以它**必然晚吸附**，调 `top` 只能缓解、不能消除。
 *
 * 结论：要让两者同步，只能**少吸一层**。这里选择保留右栏吸附（调样式时
 * 预览始终可见，那是它存在的意义），页头改为**随内容滚走**；
 * 「保存样式」改用右下角浮动按钮，任何时候都能点到。
 */
.page-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 16px;
  flex-wrap: wrap;
  margin: 0 0 4px;
  padding: 6px 0;
}

.page-head h2 {
  margin: 0;
  font-size: 15px;
}

/*
 * 右下角浮动的「重置 / 保存样式」。
 *
 * 页头不再吸顶后，「滚到下面改完样式还要滚回顶部点保存」成了新问题——
 * 这组按钮解决它。
 *
 * ## 只在页头滚出视野时出现
 * 一直飘着会挡住右下角内容（尤其缩略图与右栏预览底部）。顶部本来就有
 * 同样的按钮，**看不到时才需要替补**——由 `IntersectionObserver` 控制
 * `v-if`（见脚本里的 `headVisible`），不是靠滚动阈值判断。
 *
 * `z-index` 高于右栏预览（20）与导航（30）：它是操作入口，不能被盖住。
 */
.floating-actions {
  position: fixed;
  right: 20px;
  bottom: 20px;
  z-index: 60;
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 6px;
  border: 1px solid var(--bsr-border);
  border-radius: 999px;
  /* 半透明白底 + 模糊：叠在面板预览上时也不至于糊成一片 */
  background: color-mix(in srgb, var(--bsr-bg-elevated) 88%, transparent);
  backdrop-filter: blur(6px);
  box-shadow: 0 8px 26px rgb(90 68 80 / 18%);
}

/*
 * 浮动按钮的基础样式。
 * ⚠️ 必须写全（边框/圆角/底色）——本项目多次出现按钮"裸奔"成浏览器默认外观，
 * 根因都是基础样式挂在别的选择器上、这里的按钮不在其中。
 */
.floating-btn {
  padding: 7px 14px;
  border: 1px solid var(--bsr-border);
  border-radius: 999px;
  background: var(--bsr-bg-elevated);
  color: var(--bsr-fg);
  font-size: 12.5px;
  line-height: 1.4;
  cursor: pointer;
  transition: transform 0.12s ease, box-shadow 0.12s ease, opacity 0.12s ease,
    background 0.12s ease, border-color 0.12s ease;
}

.floating-btn:hover:not(:disabled) {
  border-color: var(--bsr-accent);
  background: var(--bsr-accent-soft);
}

.floating-btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}

/* 主操作（保存）：实心主题色，一眼能分辨 */
.floating-btn.is-primary {
  border-color: var(--bsr-accent);
  background: var(--bsr-accent);
  color: #fff;
  font-weight: 600;
}

.floating-btn.is-primary:hover:not(:disabled) {
  background: var(--bsr-accent);
  filter: brightness(1.06);
}

/* 保存成功时短暂变绿，给一个不打扰的反馈 */
.floating-btn.is-primary.is-saved {
  border-color: var(--bsr-success);
  background: var(--bsr-success);
}

/* 出现/消失：轻微上移淡入，避免突然弹出 */
.float-fade-enter-active,
.float-fade-leave-active {
  transition: opacity 0.16s ease, transform 0.16s ease;
}

.float-fade-enter-from,
.float-fade-leave-to {
  opacity: 0;
  transform: translateY(8px);
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
  background-color: #f6e9ef;
  /* 图片**独占**一层背景（棋盘格在下面的伪元素里） */
  background-size: cover;
  background-position: center;
  background-repeat: no-repeat;
  cursor: pointer;
  overflow: hidden;
}

/*
 * 棋盘格放在伪元素上，**和图片分成两层**。
 *
 * ⚠️ 早期两者挤在同一个 `background` 简写里（两层，各带 repeat/size），
 * 而模板用行内 `backgroundImage` 只覆盖了**一层**——层数对不上号，
 * 第二层的 `cover` / `no-repeat` 就落空了，图片按默认 `repeat` 平铺，
 * 表现为缩略图里人物被重复成一格一格（用户报的「不要平铺开」）。
 *
 * 拆成两层后，行内样式只碰图片那一层，互不干扰。
 */
.bg-thumb::before {
  content: '';
  position: absolute;
  inset: 0;
  background-image: repeating-conic-gradient(rgb(255 111 165 / 10%) 0% 25%, transparent 0% 50%);
  background-size: 16px 16px;
  pointer-events: none;
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

/*
 * 缩略图下的操作按钮。
 *
 * ⚠️ 早期这里只写了 padding 与字号，**没有边框/圆角/底色**——本文件里按钮的
 * 基础样式都挂在 `.controls button` / `.tabs button` 这类选择器上，而
 * `.bg-actions` 不在 `.controls` 里，于是这些按钮退回浏览器默认外观
 * （灰底立体边框），和整套粉白主题明显不搭。
 *
 * 现在把基础样式补在这里，观感与 `.controls button` 保持一致。
 */
.bg-actions button {
  padding: 3px 9px;
  border: 1px solid var(--bsr-border);
  border-radius: 6px;
  background: var(--bsr-bg-elevated);
  color: var(--bsr-fg);
  font-size: 11.5px;
  line-height: 1.5;
  cursor: pointer;
  transition: border-color 0.12s ease, background 0.12s ease, color 0.12s ease;
}

.bg-actions button:hover:not(:disabled) {
  border-color: var(--bsr-accent);
  background: var(--bsr-accent-soft);
}

.bg-actions button:disabled {
  opacity: 0.45;
  cursor: not-allowed;
}

/* 删除用危险色，与其它按钮区分，降低误点概率 */
.bg-actions button.danger {
  border-color: var(--bsr-danger);
  background: transparent;
  color: var(--bsr-danger);
}

.bg-actions button.danger:hover:not(:disabled) {
  background: color-mix(in srgb, var(--bsr-danger) 12%, transparent);
  color: var(--bsr-danger);
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

/* 不透明度一行：滑块 + 百分比 + 恢复按钮 */
.opacity-row {
  display: flex;
  align-items: center;
  gap: 10px;
  margin-top: 10px;
  max-width: 360px;
}

.opacity-row input[type='range'] {
  flex: 1;
  min-width: 0;
  accent-color: var(--bsr-accent);
}

.opacity-row .mono {
  flex: none;
  min-width: 42px;
  text-align: right;
  font-size: 12px;
  color: var(--bsr-muted);
}

.bg-preview-wrap {
  margin-top: 10px;
}

/*
 * 当前背景图的**等比预览**。
 *
 * ⚠️ 早期是 `height: 140px` + `background-size: cover`——那是一根"取样条"：
 * 只显示图片中段一条、左右撑满，看起来就是被拉长了。
 *
 * 现在改为「等比显示整张图」：用 `padding-top` 百分比撑出与图片一致的高宽比
 * （百分比 padding 是相对**宽度**计算的，所以 `高/宽 × 100%` 就能得到正确比例），
 * 再用 `max-height` 封顶竖图。**不用 `aspect-ratio`**：它和 `max-height`
 * 一起用时实测会被压成 1191×320（比例 3.7，而不是图片真实的 0.64），
 * 因为高度被夹住后浏览器不再按比例回算宽度。
 */
.bg-preview-wrap {
  margin-top: 10px;
}

.bg-preview {
  position: relative;
  /*
   * 宽度用 `fit-content` + `max-width` 而不是 `100%`：
   * padding 撑高时高度已定，若宽度也拉满就会得到「宽 × 比例高」的巨块
   * （实测竖图 1191×1856）。加上 `max-height` 后，这里让它按高度约束
   * **等比缩小**，再居中显示。
   */
  width: fit-content;
  max-width: 100%;
  margin: 0 auto;
  /* 高度由内联的 padding-top 按图片比例给出 */
  max-height: 320px;
  overflow: hidden;
  border: 1px solid var(--bsr-border);
  border-radius: 8px;
  background-color: #f6e9ef;
  /* 图片加载失败时不会「看起来像没生效」，这里给个浅色底 */
  box-shadow: inset 0 0 0 1px rgba(255, 255, 255, 0.6);
}

/* 图片层：绝对定位铺满，`contain` 保证完整显示且不变形 */
.bg-preview::after {
  content: '';
  position: absolute;
  inset: 0;
  background-image: var(--bg-preview-image, none);
  background-size: contain;
  background-position: center;
  background-repeat: no-repeat;
}

/* 预览框：棋盘底纹以便看清「透明背景」是否生效 */
.preview-frame {
  /*
   * 高度由双栏区的 `.preview-frame { height: 560px }` 决定（需求指定 400px，
   * 但改成按 16:9 等比显示后需要更高才能放下缩放过后的画布）。
   * 这里只负责边框与底色，不要再写 height——否则两处会互相覆盖。
   */
  border: 1px solid var(--bsr-border);
  border-radius: 8px;
  overflow: hidden;
  background: #f6e9ef;
}

/*
 * 预览舞台：由 `PanelPreview.vue` 内部负责（它用 `zoom: var(--pv-scale)`）。
 * 这里只保留 `.transparent` 的棋盘格——
 * 面板自身的 `bg` 是 transparent 时，预览必须有可见底纹，
 * 否则"透明"和"忘了设背景"看起来一样。
 */
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

/*
 * 桥接 iframe：只做消息中转，必须**完全不可见且不参与布局**。
 * 用 1×1 + opacity 0 而不是 display:none —— 后者会让部分浏览器
 * 延迟甚至跳过加载，桥接就永远不就绪。
 */
.preview-bridge {
  position: absolute;
  width: 1px;
  height: 1px;
  border: 0;
  opacity: 0;
  pointer-events: none;
}

.preview-stage iframe {
  /* 尺寸由 `.preview-stage` 的内联样式给定（虚拟画布），这里不要再写宽高 */
  display: block;
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
