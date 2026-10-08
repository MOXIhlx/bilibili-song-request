<script setup lang="ts">
/**
 * OBS 面板配置页（独立路由 `/panels`）。
 *
 * ## 为什么不放在控制台里
 * 这些内容（四个面板地址 + 样式参数 + 预览）挤在「直播与播放」标签页里
 * 会把真正日常要看的弹幕/队列区挤下去，控制台显得很拥挤。
 * 拆成独立页面后，控制台保留「一眼看状态」的信息，
 * 面板相关的配置集中到这里一次配好。
 *
 * ## 这里改的到底是什么
 * 两个层次的东西，刻意放在一起以便对照：
 *  1. **默认样式**（写入 `config.panel`，落盘）——决定 `/panel` 的默认外观；
 *  2. **URL 参数**（只拼在地址里）——OBS 浏览器源各自可以覆盖默认值，
 *     例如歌词页单独放大字号，而不影响其它源。
 *
 * 地址里的参数始终以「默认样式」为起点，因此两者不会互相矛盾。
 */
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import {
  apiUrl,
  deleteBackground,
  downloadBackground,
  getBackgrounds,
  getConfig,
  renameBackground,
  uploadBackground,
  type BackgroundItem,
} from '@/api'
import { clonePlain } from '@/composables/panelParams'
import { showMessage } from '@/stores/message'
import { useAppStore } from '@/stores/app'
import type { PanelStyleConfig } from '@/types'

const store = useAppStore()

/** 本地编辑副本（保存后写回 config）。 */
const draft = ref<PanelStyleConfig | null>(null)
const saving = ref(false)
const saved = ref(false)

/** 面板地址支持的参数名。 */
type ParamName =
  | 'fontSize'
  | 'color'
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

/** 每个参数的预设选项；`presets` 为空表示自由输入（如颜色、图片地址）。 */
interface ParamSpec {
  label: string
  /** 下拉里的预设值；第一项固定是「默认」。 */
  presets: Array<{ value: string; label: string }>
  hint?: string
}

/** 参数表：决定下拉框里能选什么。 */
const PARAM_SPECS: Record<ParamName, ParamSpec> = {
  fontSize: {
    label: '字号 fontSize',
    presets: [
      { value: '12', label: '12 px' },
      { value: '14', label: '14 px' },
      { value: '16', label: '16 px（默认）' },
      { value: '20', label: '20 px' },
      { value: '24', label: '24 px' },
      { value: '28', label: '28 px' },
      { value: '32', label: '32 px' },
      { value: '40', label: '40 px' },
      { value: '48', label: '48 px' },
    ],
    hint: '会乘以每个面板自己的「字号倍率」',
  },
  color: { label: '进度条色 color', presets: [] },
  fg: { label: '字体色 fg', presets: [] },
  surface: {
    label: '卡片底色 surface',
    presets: [
      { value: 'transparent', label: 'transparent（不要底色）' },
      { value: '#ffffffb3', label: '#ffffffb3 淡白（配粉白主题）' },
      { value: '#ffeef5cc', label: '#ffeef5cc 淡粉' },
      { value: '#00000033', label: '#00000033 淡黑' },
      { value: '#ffffff1a', label: '#ffffff1a 淡白（深底用）' },
      { value: '#0f172acc', label: '#0f172acc 深蓝' },
    ],
  },
  track: {
    label: '进度条轨道 track',
    presets: [
      { value: 'transparent', label: 'transparent（不要轨道）' },
      { value: '#ffffff33', label: '#ffffff33' },
      { value: '#00000055', label: '#00000055' },
    ],
  },
  limit: {
    label: '队列条数 limit',
    presets: [
      { value: '3', label: '3 条' },
      { value: '5', label: '5 条' },
      { value: '8', label: '8 条（默认）' },
      { value: '12', label: '12 条' },
      { value: '20', label: '20 条' },
      { value: '0', label: '0 = 全部' },
    ],
  },
  scale: {
    label: '缩放 scale',
    presets: [
      { value: '0.8', label: '0.8' },
      { value: '1', label: '1（默认）' },
      { value: '1.2', label: '1.2' },
      { value: '1.5', label: '1.5' },
      { value: '2', label: '2' },
    ],
  },
  layout: {
    label: '布局 layout',
    presets: [
      { value: 'list', label: 'list 竖向堆叠' },
      { value: 'compact', label: 'compact 精简单行' },
      { value: 'lyrics', label: 'lyrics 歌词为主' },
      { value: 'wide', label: 'wide 左歌曲/右歌词' },
    ],
  },
  theme: {
    label: '主题 theme',
    presets: [
      { value: 'dark', label: 'dark' },
      { value: 'light', label: 'light' },
    ],
  },
  bg: {
    label: '背景 bg',
    presets: [
      { value: 'transparent', label: 'transparent（OBS 用）' },
      { value: 'solid', label: 'solid' },
    ],
  },
  showLyrics: {
    label: '显示歌词 showLyrics',
    presets: [
      { value: 'true', label: 'true' },
      { value: 'false', label: 'false' },
    ],
  },
  bgImage: {
    label: '背景图 bgImage',
    presets: [],
    hint: '填 /bg/xxx.png 或 https://...',
  },
}

/** 一行参数。 */
interface ParamRow {
  /** 行 id（仅用于 v-for key，与参数名分开，允许同名多行）。 */
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
 * 参数行（用户自己增删）。
 *
 * ## 为什么不是固定几个复选框
 * 之前的实现是「8 个写死的 checkbox」，想加个 `fg`（字体色）就得改代码。
 * 现在改成可选参数名的行列表：**加一行 → 选参数名 → 选预设值（或自己填）
 * → 地址立刻重算**。以后再加新参数只需往 `PARAM_SPECS` 里加一条。
 */
const paramRows = ref<ParamRow[]>([
  makeRow('fontSize', '16'),
  makeRow('color', '#ff6fa5'),
  makeRow('surface', 'transparent'),
  makeRow('track', 'transparent'),
  makeRow('limit', '8'),
])

/** 可以新增的参数名（排除已加过的，除非允许重复）。 */
const availableParams = computed(() =>
  (Object.keys(PARAM_SPECS) as ParamName[]).map((name) => ({
    name,
    label: PARAM_SPECS[name].label,
  })),
)

function addParamRow(): void {
  // 默认加一个还没用过的参数，减少手动选择
  const used = new Set(paramRows.value.map((r) => r.name))
  const next = (Object.keys(PARAM_SPECS) as ParamName[]).find((n) => !used.has(n))
  // 初始值留空 = 「默认」（不写进地址）
  paramRows.value.push(makeRow(next ?? 'limit', ''))
}
function removeParamRow(id: number): void {
  paramRows.value = paramRows.value.filter((r) => r.id !== id)
}

/** 某个参数当前是否用下拉（有预设）而不是自由输入。 */
function hasPresets(name: ParamName): boolean {
  return PARAM_SPECS[name].presets.length > 0
}

/**
 * 标记哪些行选了「自定义…」。
 *
 * 预设下拉表达不了所有取值——比如带透明度的 `#000000aa`，
 * 它在预设列表里没有，而下拉里选不存在的值会直接变成空串
 * （用户会看到"选了等于没选"）。所以给一个「自定义…」入口切到文本框。
 */
const customRows = ref<Record<number, boolean>>({})

/** 选择预设值时的处理：选到 `__custom__` 就切到自定义输入。 */
function onPresetChange(row: ParamRow, raw: string): void {
  if (raw === '__custom__') {
    customRows.value[row.id] = true
    // 从当前值出发，方便在预设基础上微调
    row.value = row.value === '__default__' ? '' : row.value
    return
  }
  customRows.value[row.id] = false
  row.value = raw
}

/** 让某一行退出自定义模式，回到预设下拉。 */
function backToPresets(row: ParamRow): void {
  customRows.value[row.id] = false
  // 若当前值不在预设里，回到「默认」以免下拉显示空
  const spec = PARAM_SPECS[row.name]
  if (!spec.presets.some((p) => p.value === row.value)) row.value = ''
}

/** 该行是否要以自定义输入框呈现。 */
function isCustom(row: ParamRow): boolean {
  if (!hasPresets(row.name)) return true
  if (customRows.value[row.id]) return true
  // 值不在预设里（例如上次填的自定义色）也自动用输入框，避免下拉显示为空
  const spec = PARAM_SPECS[row.name]
  return row.value !== '' && row.value !== '__default__' && !spec.presets.some((p) => p.value === row.value)
}

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
  const cfg = store.config?.panel
  if (!cfg) return
  draft.value = clonePanel(cfg)
}

/**
 * 拿到面板默认样式。
 *
 * ⚠️ 两条路径都要有：
 *  1. `store.config` 已就绪时直接用它（正常情况，省一次请求）；
 *  2. 否则**自己发一次请求**——bootstrap 在 `App.vue` 的 `onMounted` 发起，
 *     本页可能先于它完成挂载；早期版本只读一次 store，结果永久停在
 *     「正在加载配置…」，和设置页踩过的是同一个坑。
 */
async function ensureDraft(): Promise<void> {
  if (draft.value) return
  if (store.config?.panel) {
    resetDraft()
    return
  }
  loadError.value = null
  try {
    const cfg = await getConfig()
    draft.value = clonePanel(cfg.panel)
  } catch (err) {
    loadError.value = (err as Error).message
  }
}

// 立即尝试一次；`store.config` 之后到位时也能补上
watch(() => store.config, ensureDraft, { immediate: true })

/** 默认样式是否已就绪。 */
const ready = computed(() => draft.value !== null)

// ── 颜色编辑（阶段 9）────────────────────────────────────────────────────

/** 主题对应的默认字体色（`fg` 为空时用它给取色器一个初值）。 */
const themeFg = computed(() => (draft.value?.theme === 'light' ? '#5a4450' : '#f2f2f2'))

/** 取色器回调：把颜色写进对应字段。 */
function setColor(field: 'fg' | 'color' | 'surface' | 'track', event: Event): void {
  if (!draft.value) return
  const value = (event.target as HTMLInputElement).value
  if (field === 'fg') draft.value.fg = value
  else draft.value[field] = value
}

/** 一键把所有底色清掉：只留文字与进度条颜色。 */
function makeAllTransparent(): void {
  if (!draft.value) return
  draft.value.surface = 'transparent'
  draft.value.track = 'transparent'
}

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
    draft.value = clonePlain(cfg.panel)
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

/** 保存默认样式到配置。 */
async function save(): Promise<void> {
  if (!draft.value || !store.config) return
  saving.value = true
  // 同样要避开 structuredClone（store.config 是响应式代理）
  const next = clonePlain(store.config)
  next.panel = clonePlain(draft.value)
  await store.updateConfig(next)
  saving.value = false
  saved.value = !store.error
  window.setTimeout(() => (saved.value = false), 1500)
}

/**
 * 生成某个专注页的地址。
 *
 * 参数来自用户自己维护的**参数行**（见 `paramRows`），
 * 外加该页专属的字号倍率。用户增删参数行，这里立刻跟着变，
 * 面板页再通过 `usePanelStyle` 实时生效——即"选完直接改地址"。
 */
function urlFor(page: PanelKey): string {
  const cfg = draft.value
  if (!cfg) return ''
  const params = new URLSearchParams()

  for (const row of paramRows.value) {
    const spec = PARAM_SPECS[row.name]
    if (!spec) continue
    const raw = row.value.trim()
    // 「默认」= 不写进地址，由面板回落到后端保存的默认样式
    if (raw === '' || raw === '__default__') continue
    // 字号按该页倍率换算（这是唯一会随面板变化的参数）
    if (row.name === 'fontSize') {
      const base = Number(raw)
      if (!Number.isFinite(base)) continue
      const multiplier = fontScale.value[page] ?? 1
      params.set('fontSize', String(Math.round(base * multiplier)))
      continue
    }
    params.set(row.name, raw)
  }

  // 背景图：默认**自动**写进地址。
  //
  // 早期只有「地址参数」里手动加 `bgImage` 才会带上，于是用户上传了背景图
  // 却发现面板上没效果（默认样式里的 bg_image 又不被面板读取）。
  // 现在只要选了背景图且开关打开，地址里就带上它。
  // 注意：面板会把这个相对路径补成绝对地址（内嵌预览是跨源的）。
  if (includeBgInUrls.value && cfg.bg_image) {
    params.set('bgImage', cfg.bg_image)
  }

  const path = page === 'all' ? '/panel' : `/panel/${page}`
  const query = params.toString()
  return query ? `${apiUrl(path)}?${query}` : apiUrl(path)
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
    <div class="page-head">
      <div>
        <h2>OBS 面板地址</h2>
      </div>
      <div class="controls">
        <button class="ghost" :disabled="!ready" @click="resetDraft()">重置</button>
        <button :disabled="!ready || saving" @click="save()">
          {{ saving ? '保存中…' : saved ? '已保存' : '保存默认样式' }}
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
      <!-- ── 默认样式 ─────────────────────────────────────────────── -->
      <section class="card">
        <h3>默认样式</h3>
        <div class="fields">
          <label>主题
            <select v-model="draft.theme">
              <option value="dark">dark（深色）</option>
              <option value="light">light（浅色）</option>
            </select>
          </label>
          <label>背景
            <select v-model="draft.bg">
              <option value="transparent">transparent（OBS 用这个）</option>
              <option value="solid">solid（不透明）</option>
            </select>
          </label>
          <label>布局
            <select v-model="draft.layout">
              <option value="list">list（竖向堆叠）</option>
              <option value="compact">compact（精简单行）</option>
              <option value="lyrics">lyrics（歌词为主）</option>
              <option value="wide">wide（左歌曲 / 右歌词）</option>
            </select>
          </label>
          <label>字号 <input type="number" v-model.number="draft.font_size" /></label>
          <label>缩放 <input type="number" step="0.1" v-model.number="draft.scale" /></label>
          <label>队列条数 <input type="number" v-model.number="draft.limit" /></label>
          <label class="check">
            <input type="checkbox" v-model="draft.show_lyrics" /> 显示歌词
          </label>
        </div>

        <h4>颜色</h4>
        <div class="fields">
          <label>字体颜色
            <span class="color-row">
              <input type="color" :value="draft.fg ?? themeFg" @input="setColor('fg', $event)" />
              <input v-model="draft.fg" placeholder="留空 = 跟随主题" />
            </span>
          </label>
          <label>进度条颜色（强调色）
            <span class="color-row">
              <input type="color" :value="draft.color" @input="setColor('color', $event)" />
              <input v-model="draft.color" placeholder="#ff6fa5" />
            </span>
          </label>
          <label>卡片底色
            <span class="color-row">
              <input
                type="color"
                :value="draft.surface === 'transparent' ? '#000000' : draft.surface"
                @input="setColor('surface', $event)"
              />
              <input v-model="draft.surface" placeholder="transparent = 不要底色" />
            </span>
          </label>
          <label>进度条轨道
            <span class="color-row">
              <input
                type="color"
                :value="draft.track === 'transparent' ? '#000000' : draft.track"
                @input="setColor('track', $event)"
              />
              <input v-model="draft.track" placeholder="transparent = 不要轨道" />
            </span>
          </label>
        </div>
        <div class="controls">
          <button class="ghost" @click="makeAllTransparent()">全部透明（只留文字与进度条）</button>
        </div>
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
        <!-- 已上传图片的管理：重命名 / 下载 / 删除 -->
        <h4>图片管理</h4>
        <table v-if="backgrounds.length" class="bg-table">
          <thead>
            <tr>
              <th>文件名</th>
              <th>大小</th>
              <th>操作</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="bg in backgrounds" :key="bg.url">
              <td class="dim">{{ bg.name }}</td>
              <td class="dim">{{ formatSize(bg.size) }}</td>
              <td>
                <div class="controls">
                  <button class="ghost" @click="openPreview(bg)">预览</button>
                  <button class="ghost" :disabled="bgBusy" @click="openRename(bg)">重命名</button>
                  <button class="ghost" @click="downloadBg(bg)">下载</button>
                  <button class="danger" :disabled="bgBusy" @click="pendingDelete = bg">删除</button>
                </div>
              </td>
            </tr>
          </tbody>
        </table>
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

      <!-- ── 参数行（阶段 9）─────────────────────────────────────── -->
      <section class="card">
        <div class="card-head">
          <h3>地址参数</h3>
          <div class="controls">
            <button class="ghost" @click="addParamRow()">+ 添加参数</button>
          </div>
        </div>
        <ul v-if="paramRows.length" class="param-list">
          <li v-for="row in paramRows" :key="row.id">
            <select v-model="row.name">
              <option v-for="p in availableParams" :key="p.name" :value="p.name">
                {{ p.label }}
              </option>
            </select>

            <!--
              有预设的参数：默认给下拉（第一项「默认」+「自定义…」）；
              选了自定义、或当前值不在预设里，就切成输入框。
            -->
            <template v-if="hasPresets(row.name) && !isCustom(row)">
              <select :value="row.value" @change="onPresetChange(row, ($event.target as HTMLSelectElement).value)">
                <option value="__default__">默认</option>
                <option v-for="opt in PARAM_SPECS[row.name].presets" :key="opt.value" :value="opt.value">
                  {{ opt.label }}
                </option>
                <option value="__custom__">自定义…</option>
              </select>
            </template>
            <template v-else>
              <!-- 颜色类：取色器 + 文本框（文本框里可写带透明度的 8 位色值） -->
              <span
                v-if="row.name === 'color' || row.name === 'fg' || row.name === 'surface' || row.name === 'track'"
                class="color-row"
              >
                <input
                  type="color"
                  :value="row.value && /^#[0-9a-f]{6}$/i.test(row.value.slice(0, 7)) ? row.value.slice(0, 7) : '#000000'"
                  @input="row.value = ($event.target as HTMLInputElement).value"
                />
                <input v-model="row.value" :placeholder="PARAM_SPECS[row.name].hint ?? '留空 = 默认'" />
              </span>
              <input v-else v-model="row.value" :placeholder="PARAM_SPECS[row.name].hint ?? '留空 = 默认'" />
              <button
                v-if="hasPresets(row.name)"
                class="ghost"
                title="回到预设选项"
                @click="backToPresets(row)"
              >
                用预设
              </button>
            </template>

            <span v-if="PARAM_SPECS[row.name].hint" class="param-hint">
              {{ PARAM_SPECS[row.name].hint }}
            </span>

            <button class="ghost" title="删除这一行" @click="removeParamRow(row.id)">×</button>
          </li>
        </ul>
        <p v-else class="empty">没有参数——地址将完全使用上面保存的默认样式。</p>
      </section>

      <!-- ── 面板启停（阶段 9）────────────────────────────────────── -->
      <section class="card">
        <h3>要使用哪些面板</h3>
        <div class="fields">
          <label v-for="p in PANEL_LIST" :key="p.key" class="check">
            <input type="checkbox" v-model="enabledPanels[p.key]" />
            {{ p.label }}
            <span class="dim">（{{ p.desc }}）</span>
          </label>
        </div>
      </section>

      <!-- ── 预览 ─────────────────────────────────────────────────── -->
      <section class="card">
        <div class="card-head">
          <h3>预览：{{ previewLabel }}</h3>
          <div class="controls">
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
        </div>
        <div class="preview-frame" :class="{ transparent: draft.bg === 'transparent' }">
          <iframe :src="previewUrl" title="面板预览" />
        </div>
      </section>
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
  </div>
</template>

<style scoped>
.panels-page {
  display: flex;
  flex-direction: column;
  gap: 14px;
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

/* 已上传图片的管理表格 */
.bg-table {
  width: 100%;
  border-collapse: collapse;
  margin-top: 8px;
  font-size: 12.5px;
}

.bg-table th,
.bg-table td {
  padding: 6px 8px;
  border-bottom: 1px solid var(--bsr-border);
  text-align: left;
  vertical-align: middle;
}

.bg-table th {
  color: var(--bsr-muted);
  font-weight: 500;
}

.bg-table .controls {
  margin-top: 0;
  gap: 6px;
}

.bg-table .controls button {
  padding: 3px 8px;
  font-size: 12px;
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

.fields {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
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
  margin-top: 10px;
  height: 480px;
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

/* 非桌面窗口里 iframe 可能被 CSP 拦，这里给一句提示性兜底样式 */
@media (max-width: 720px) {
  .preview-frame {
    height: 320px;
  }
}
</style>
