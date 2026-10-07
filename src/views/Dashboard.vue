<script setup lang="ts">
/**
 * 主播控制台。
 *
 * 阶段 1 状态：界面骨架 + 与内嵌服务器的真实数据打通（状态 / 队列 / 配置 / WS）。
 * 播放控制按钮已经接到 API 上，但后端播放器在阶段 6 才会真正响应，
 * 因此失败时只在页面上提示错误，不阻塞界面。
 */
import { computed, onMounted, onUnmounted, reactive, ref, watch } from 'vue'
import { useAppStore } from '@/stores/app'
import { showMessage } from '@/stores/message'
import {
  addBlacklistEntry,
  addIdleSong,
  apiUrl,
  clearIdle,
  getBlacklist,
  getConfig,
  getIdle,
  getPlaylists,
  getRequestLog,
  importPlaylistToIdle,
  isDesktop,
  platformLabel,
  playIdle,
  removeBlacklistEntry,
  removeIdleSong,
  setIdleMode,
} from '@/api'
import { clonePlain } from '@/composables/panelParams'
import { ApiNotice } from '@/api'
import { EVENT_MUSIC_LOGIN } from '@/api/events'
import type {
  BlacklistEntry,
  Config,
  IdleMode,
  IdleResponse,
  MusicLoginResult,
  MusicPlatform,
  PlaylistInfo,
  QueueItem,
  RequestRecord,
  Song,
} from '@/types'

const store = useAppStore()

/** 卸载 Tauri 事件监听（仅桌面端有）。 */
let unlistenLogin: (() => void) | null = null

onMounted(async () => {
  void store.refreshMusicStatus()
  void store.refreshPlayerStatus()
  try {
    const { listen } = await import('@tauri-apps/api/event')
    unlistenLogin = await listen<MusicLoginResult>(EVENT_MUSIC_LOGIN, (event) => {
      const payload = event.payload
      loginMessage.value = payload.success
        ? `${platformLabel(payload.platform)}：${payload.message}`
        : `${platformLabel(payload.platform)}登录失败：${payload.message}`
      void store.refreshMusicStatus()
    })
  } catch {
    // 浏览器里没有 Tauri 事件系统，忽略即可
  }
})

onUnmounted(() => {
  unlistenLogin?.()
  unlistenLogin = null
})

/** 登录流程的提示文案。 */
const loginMessage = ref('')

/** 手动粘贴 Cookie 的输入。 */
const manualCookie = reactive<{ platform: MusicPlatform; value: string }>({
  platform: 'netease',
  value: '',
})

/** 某平台的登录状态。 */
function platformLoggedIn(platform: MusicPlatform): boolean {
  return store.musicStatus?.platforms.find((p) => p.platform === platform)?.logged_in ?? false
}

/** 打开内嵌登录窗口。 */
async function startMusicLogin(platform: MusicPlatform): Promise<void> {
  loginMessage.value = `正在打开${platformLabel(platform)}登录窗口…`
  await store.loginMusic(platform)
}

/** 手动保存粘贴的 Cookie。 */
async function submitManualCookie(): Promise<void> {
  const ok = await store.saveMusicCookie(manualCookie.platform, manualCookie.value)
  if (ok) {
    loginMessage.value = `${platformLabel(manualCookie.platform)}：Cookie 已保存`
    manualCookie.value = ''
  }
}

/** 曲目解析状态标签。 */
function sourceLabel(song: { source: string; source_error: string | null }): string {
  switch (song.source) {
    case 'pending':
      return '解析中…'
    case 'failed':
      return song.source_error ? `未匹配：${song.source_error}` : '未匹配'
    default:
      return ''
  }
}

/** 时长格式化。 */
function formatSongDuration(seconds: number): string {
  if (!seconds) return ''
  const m = Math.floor(seconds / 60)
  const s = seconds % 60
  return `${m}:${s.toString().padStart(2, '0')}`
}

/** 播放进度时钟（容忍小数秒）。 */
function formatClock(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds <= 0) return '0:00'
  const total = Math.floor(seconds)
  const m = Math.floor(total / 60)
  const s = total % 60
  return `${m}:${s.toString().padStart(2, '0')}`
}

const selectedPlatform = ref<MusicPlatform>('netease')

/** 手动加歌：只给文本，由后端异步搜索。 */
async function addByText(): Promise<void> {
  const title = store.searchKeyword.trim()
  if (!title) return
  await store.addSongByText(title)
  if (store.error) {
    showToast('err', store.error)
    return
  }
  showToast('ok', `已加入队列：${title}（正在后台解析曲目）`)
  store.searchKeyword = ''
}

/** 搜索结果的「加入队列」：给出明确回执，避免"点了没反应"。 */
async function addSearchResultToQueue(song: Song): Promise<void> {
  await store.addSongToQueue(song)
  if (store.error) {
    showToast('err', store.error)
  } else {
    showToast('ok', `已加入队列：${song.title}${song.artist ? ' - ' + song.artist : ''}`)
  }
}

/**
 * B 站凭据表单。
 *
 * 与 `draft`（整份配置）分开维护：这里的字段提交时会走
 * `POST /api/bilibili/connect`，由后端合并进配置文件，
 * 这样「填完直接点连接」不会丢掉其它已保存的设置。
 */
const credentials = reactive({
  app_id: '',
  access_key_id: '',
  access_key_secret: '',
  code: '',
})

/** 用已保存的配置回填表单（仅当用户还没输入时，避免覆盖正在编辑的内容）。 */
watch(
  () => store.config,
  (cfg) => {
    if (!cfg) return
    if (!credentials.app_id) credentials.app_id = cfg.bilibili.app_id
    if (!credentials.access_key_id) credentials.access_key_id = cfg.bilibili.access_key_id
    if (!credentials.access_key_secret) credentials.access_key_secret = cfg.bilibili.access_key_secret
    if (!credentials.code) credentials.code = cfg.bilibili.code
  },
  { immediate: true },
)

/** 模拟弹幕输入（无身份码时验证链路）。 */
const simulateText = ref('点歌 你还在不在 梁静茹')

/**
 * 点歌请求处理汇总。
 *
 * 之所以从 `recent` 现算而不是让后端累计：`recent` 只保留最近 50 条，
 * 累计值在长时间直播里意义不大，主播更关心「最近这批入队/被拒的比例」。
 */
const requestSummary = computed(() => {
  const recent = store.recentRequests
  const queued = recent.filter((r) => r.outcome === 'queued').length
  return { queued, rejected: recent.length - queued, total: recent.length }
})

/** 拒绝原因的中文标签。 */
const REASON_LABEL: Record<string, string> = {
  cooldown: '冷却中',
  duplicate: '重复点歌',
  queue_full: '队列已满',
  fans_medal: '粉丝牌不足',
  user_level: '等级不足',
}

function reasonLabel(reason: string | null): string {
  if (!reason) return ''
  return REASON_LABEL[reason] ?? reason
}

/** 冷却剩余时间的人类可读文本。 */
function formatCooldown(key: string): string {
  const entry = store.cooldowns.find((c) => c.key === key)
  if (!entry) return ''
  return `${entry.remaining_secs}s`
}

function formatAge(iso: string): string {
  const at = new Date(iso)
  if (Number.isNaN(at.getTime())) return ''
  const secs = Math.max(0, Math.floor((Date.now() - at.getTime()) / 1000))
  if (secs < 5) return '刚刚'
  if (secs < 60) return `${secs} 秒前`
  if (secs < 3600) return `${Math.floor(secs / 60)} 分钟前`
  return `${Math.floor(secs / 3600)} 小时前`
}

/**
 * 连接状态的**唯一数据源**。
 *
 * 为什么需要合并两个来源：
 *  - `store.bilibili` 来自 `/ws` 的状态快照，是**实时**的
 *    （连接阶段变化会立刻推过来）；
 *  - `store.bilibiliStatus` 来自 `GET /api/bilibili/status`，字段更全
 *    （带 `auto_connect`），但只在点击按钮/bootstrap 时更新。
 *
 * 早期实现让卡片只读 `bilibiliStatus`，于是后端阶段一直在变、界面却
 * 停在点击那一刻的文案——用户看到的就是「点了没反应、不知道在连还是失败」。
 * 这里以实时快照为主、REST 响应兜底，保证界面始终跟得上。
 */
const liveBilibili = computed(() => store.bilibili)
const restBilibili = computed(() => store.bilibiliStatus)

/** 连接是否已建立（两个来源任一为真即可）。 */
const isConnected = computed(
  () => liveBilibili.value?.connected === true || restBilibili.value?.connected === true,
)

/** 当前进度阶段（实时优先）。 */
const connPhase = computed(
  () => liveBilibili.value?.phase ?? restBilibili.value?.phase ?? 'idle',
)

/** 阶段说明（实时优先）。 */
const connDetailText = computed(
  () => liveBilibili.value?.detail ?? restBilibili.value?.detail ?? null,
)

/** 已尝试次数（取较大值，避免两个来源互相覆盖）。 */
const connAttempts = computed(() =>
  Math.max(liveBilibili.value?.attempts ?? 0, restBilibili.value?.attempts ?? 0),
)

/** 最近错误（实时优先）。 */
const connLastError = computed(
  () => liveBilibili.value?.last_error ?? restBilibili.value?.last_error ?? null,
)

/** 直播间 ID（实时优先）。 */
const connRoomId = computed(
  () => liveBilibili.value?.room_id ?? restBilibili.value?.room_id ?? null,
)

/** 最近更新时间（实时优先）。 */
const connUpdatedAt = computed(
  () => liveBilibili.value?.updated_at ?? restBilibili.value?.updated_at ?? null,
)

/** 状态徽标文案。 */
const bilibiliBadge = computed(() => {
  switch (connPhase.value) {
    case 'connected':
      return { text: '已连接', tone: 'ok' as const }
    case 'requesting_session':
    case 'connecting_socket':
      return { text: '连接中…', tone: 'busy' as const }
    case 'retrying':
      return { text: '重连中…', tone: 'busy' as const }
    case 'failed':
      return { text: '连接失败', tone: 'err' as const }
    case 'stopped':
      return { text: '已停止', tone: 'off' as const }
    default:
      return { text: '未连接', tone: 'off' as const }
  }
})

/** 是否处于「正在连接」的中间态。 */
const isBusyPhase = computed(() => {
  const phase = connPhase.value
  return phase === 'requesting_session' || phase === 'connecting_socket' || phase === 'retrying'
})

/**
 * 进度条拖动预览值。
 *
 * `null` 表示「没在拖动」，此时显示播放器的真实位置。
 * 拖动期间显示这个值可以立刻反馈用户操作，不被 2 秒一次的位置轮询拖慢。
 */
const seekPreview = ref<number | null>(null)

/** 进度条填充百分比（拖动时跟随预览值）。 */
const progressPercent = computed(() => {
  const duration = store.player?.duration ?? 0
  if (duration <= 0) return 0
  const position = seekPreview.value ?? store.player?.position ?? 0
  return Math.min(100, Math.max(0, (position / duration) * 100))
})

/** 拖动中：只更新预览。 */
function onSeekInput(event: Event): void {
  seekPreview.value = Number((event.target as HTMLInputElement).value)
}

/** 松手：真正跳转，然后清掉预览。 */
async function onSeekCommit(event: Event): Promise<void> {
  const target = Number((event.target as HTMLInputElement).value)
  seekPreview.value = null
  await store.seek(target)
}

/**
 * 连接状态卡片的内容。
 *
 * 为什么单独做这个：点「保存并连接」后真正的建连在后台异步进行
 * （HTTP 立刻返回），此前界面只会一直显示「未连接」，用户无法判断
 * 是在连、已连上还是已经失败。这里把后端上报的 `phase`/`detail`
 * 翻译成明确的标题 + 说明 + 下一步建议。
 */
const connStatus = computed(() => {
  const attempts = connAttempts.value
  const updatedAt = formatClockTime(connUpdatedAt.value)
  const busy = isBusyPhase.value
  const detail = connDetailText.value
  const lastError = connLastError.value

  switch (connPhase.value) {
    case 'requesting_session':
      return {
        tone: 'busy' as const,
        title: '正在申请身份码会话…',
        detail: detail ?? '正在请求 B 站开放平台接口（POST v2/app/start），通常 1~3 秒。',
        hint: attempts > 2 ? '已重试多次：请确认身份码是否过期、app_id 与密钥是否匹配。' : '',
        busy,
        attempts,
        updatedAt,
      }
    case 'connecting_socket':
      return {
        tone: 'busy' as const,
        title: '正在连接弹幕服务器…',
        detail: detail ?? '已拿到长链地址，正在建立 WebSocket 并完成鉴权。',
        hint: '这一步如果长期不动，通常是网络无法访问 B 站服务器。',
        busy,
        attempts,
        updatedAt,
      }
    case 'connected':
      return {
        tone: 'ok' as const,
        title: '已连接，正在接收弹幕',
        detail: connRoomId.value
          ? `直播间 ${connRoomId.value} 的弹幕正在实时推送，观众发送「点歌 歌名 歌手」即可点歌。`
          : '弹幕正在实时推送，观众发送「点歌 歌名 歌手」即可点歌。',
        hint: '',
        busy: false,
        attempts,
        updatedAt,
      }
    case 'retrying':
      return {
        tone: 'busy' as const,
        title: '连接中断，正在自动重连…',
        // 把最近一次错误放进正文：这是用户最需要看到的信息
        // （例如「接口返回空响应（HTTP 405）」直接提示了是网络拦截）
        detail: lastError
          ? `最近一次失败原因：${lastError}`
          : (detail ?? '连接已断开，程序正在按退避策略自动重试。'),
        hint:
          attempts > 2
            ? '已重试多次仍未成功：请确认身份码是否过期，并检查本机能否访问 bilibili.com（网络受限时换个网络）。'
            : '若长时间无法恢复，可点击「断开」后重新连接。',
        busy,
        attempts,
        updatedAt,
      }
    case 'stopped':
      return {
        tone: 'off' as const,
        title: '已断开',
        detail: '连接已停止。需要接收弹幕请重新点击「保存并连接」。',
        hint: '',
        busy: false,
        attempts,
        updatedAt,
      }
    case 'failed':
      return {
        tone: 'err' as const,
        title: '连接失败',
        detail: lastError ?? detail ?? '连接失败，请检查凭据与网络。',
        hint: '可先展开下方「凭据怎么获取？」核对该填什么；网络受限时换个网络再试。',
        busy: false,
        attempts,
        updatedAt,
      }
    default:
      return {
        tone: 'off' as const,
        title: '未连接',
        detail: '填入凭据后点击「保存并连接」开始接收弹幕。',
        hint: '',
        busy: false,
        attempts,
        updatedAt,
      }
  }
})

/** 按钮文案：按当前阶段给出「在做什么」而不是笼统的「处理中」。 */
const connectButtonText = computed(() => {
  if (!store.connecting) {
    return isConnected.value ? '重新连接' : '保存并连接'
  }
  switch (connPhase.value) {
    case 'requesting_session':
      return '正在申请会话…'
    case 'connecting_socket':
      return '正在连接…'
    default:
      return '正在提交…'
  }
})

/** 有连接（或正在尝试）时才允许点「断开」。 */
const canDisconnect = computed(
  () => isConnected.value || isBusyPhase.value || connPhase.value === 'failed',
)

/** 把 RFC3339 转成本地时间（失败返回空串）。 */
function formatClockTime(value: string | null | undefined): string {
  if (!value) return ''
  const date = new Date(value)
  if (Number.isNaN(date.getTime())) return ''
  return date.toLocaleTimeString('zh-CN', { hour12: false })
}

/**
 * 统一提示：走全局轻提示（见 `stores/message.ts`）。
 *
 * 不再用 `window.alert()`：它会阻塞界面直到点确定，连接状态变化这类后台事件
 * 弹窗时会打断用户操作。轻提示非阻塞、自动消失，并按 `tone` 着色。
 */
function showToast(tone: 'ok' | 'err' | 'info', text: string): void {
  showMessage(tone, text)
}

/** 「上一首」：已经是第一首时后端给提示而不是错误，这里按信息展示。 */
async function goPrevious(): Promise<void> {
  const notice = await store.previous()
  if (notice) showToast('info', notice)
  else if (store.error) showToast('err', store.error)
}

/**
 * 播放/暂停按钮的文案。
 *
 *  - 在播 → 「⏸ 暂停」
 *  - 未在播 → 「▶ 播放」（刚打开软件时从上次位置接着放）
 */
const playButtonText = computed(() => (store.player?.playing ? '⏸ 暂停' : '▶ 播放'))

const playButtonTitle = computed(() =>
  store.player?.playing
    ? '暂停'
    : '开始/继续播放（重启后会从上次的秒数接着放）',
)

/** 播放/暂停：在播就暂停，否则开始（或继续）。 */
async function togglePlayPause(): Promise<void> {
  if (store.player?.playing) {
    await store.togglePause()
    return
  }
  await store.startPlayback()
  if (store.error) showToast('err', store.error)
}

/** 有内容可清时才允许点「清空播放列表」。 */
const canClearPlaylist = computed(
  () => store.queue.length > 0 || (store.state?.playing.length ?? 0) > 0 || !!store.current,
)

async function clearPlaylist(): Promise<void> {
  await store.clearPlaylist()
  if (store.error) showToast('err', store.error)
  else showToast('ok', '播放列表已清空')
}

async function connectBilibili(): Promise<void> {
  const ok = await store.connectBilibili({ ...credentials })
  if (ok) {
    showToast('ok', '凭据已保存，正在连接…')
  } else {
    showToast('err', store.error ?? '连接失败，请查看下方状态卡片')
  }
}

async function disconnectBilibili(): Promise<void> {
  await store.disconnectBilibili()
  showToast('ok', '已断开连接')
}

/**
 * 阶段真正切换时给最终提示。
 *
 * 这里监听**实时快照**（`store.bilibili`）而不是 REST 响应，
 * 否则连接成功/失败时不会有任何提示（那正是「没反应」的来源）。
 */
watch(
  () => [connPhase.value, connLastError.value] as const,
  ([phase, lastError], previous) => {
    if (phase === previous?.[0]) return
    if (phase === 'connected') {
      showToast('ok', '已连接到直播间，可以开始接收弹幕点歌了')
    } else if (phase === 'failed' && lastError) {
      showToast('err', `连接失败：${lastError}`)
    }
  },
)

// ── 音量（阶段 9 修复）────────────────────────────────────────────────────
//
// 曾经的实现只在 `@change`（松手）时发请求，而显示值走
// `store.player.volume`——那个值要等下一次 WS 广播/轮询才回来。
// 于是用户拖动时数字**纹丝不动**，看起来就是「音量锁死在 80，调不了」。
//
// 现在：拖动过程用本地预览值即时反馈并**立即**发请求（`@input`），
// 松手后再拉一次播放器状态确认最终值。

/** 拖动中的音量预览；`null` 表示未在拖动，显示真实值。 */
const volumePreview = ref<number | null>(null)
/** 防止并发提交：松手后要等这次请求回来才允许下一次。 */
const volumeCommitting = ref(false)

/**
 * 拖动中：**只改本地预览，不发请求**。
 *
 * 早期每个 `input` 事件都发一次请求，于是：
 *  - 状态快照每 2 秒广播一次（还有每次请求的响应），多个音量值互相覆盖；
 *  - 旧的响应/广播带着较大的值回来时，预览被判为"已是真实值"而清空，
 *    滑块立刻跳回旧值、又被下一次拖动拉下来——就是「左右抽搐」。
 * 现在拖动期间预览值是唯一事实来源，谁也不能覆盖它。
 */
function onVolumeInput(event: Event): void {
  volumePreview.value = Number((event.target as HTMLInputElement).value)
}

/** 松手：提交一次音量，成功后交还给真实值。 */
async function onVolumeCommit(): Promise<void> {
  const value = volumePreview.value
  if (value === null || volumeCommitting.value) return
  volumeCommitting.value = true
  try {
    await store.changeVolume(value)
  } finally {
    volumeCommitting.value = false
    // 与真实值一致才释放预览，避免提交失败时控件跳回
    if (store.player?.volume === value) volumePreview.value = null
  }
}

// ── 单曲循环（已移除按钮，仅保留说明）────────────────────────────────────
//
// 播放模式统一在「空闲歌单」页设置：那里是唯一真正驱动播放的模式。
// 直播页曾经也放了一个「播放模式」下拉，但它只改 `state.play_mode`
// 而播放并不读它 —— 是个**死控件**，改了什么都不会发生，已删除。

/** 切换「启动时自动连接」。 */
async function toggleAutoConnect(event: Event): Promise<void> {  const checked = (event.target as HTMLInputElement).checked
  if (!store.config) return
  // 用 clonePlain 而不是 structuredClone：store.config 是 Vue 响应式代理，
  // WebView2 下 structuredClone 会抛「could not be cloned」
  const next: Config = clonePlain(store.config)
  next.bilibili.auto_connect = checked
  await store.updateConfig(next)
}

/** 当前选中的标签页。 */
type TabKey = 'live' | 'queue' | 'idle' | 'logs' | 'blacklist' | 'settings'
const tab = ref<TabKey>('live')

// ── 空闲歌单（阶段 10a）──────────────────────────────────────────────────
//
// 需求：预先准备一份自己的歌单，**点歌队列空时自动播它**；
// 有人点歌就切回点歌队列（可配置「立即切」或「放完当前这首再切」）。
//
// 为什么单独一份状态而不是从 store 里读：空闲歌单的增删是低频操作，
// 单独拉一份能让界面立刻反映结果（store 的状态要等 WS 广播回来）。

const idle = ref<IdleResponse | null>(null)
const idleError = ref<string | null>(null)
const idleBusy = ref(false)
/** 新增输入。 */
const idleForm = reactive({ title: '', artist: '' })

async function refreshIdle(): Promise<void> {
  try {
    idle.value = await getIdle()
    idleError.value = null
  } catch (err) {
    idleError.value = (err as Error).message
  }
}

async function submitIdle(): Promise<void> {
  const title = idleForm.title.trim()
  if (!title) return
  idleBusy.value = true
  try {
    idle.value = await addIdleSong({ title, artist: idleForm.artist.trim() })
    idleForm.title = ''
    idleForm.artist = ''
    idleError.value = null
  } catch (err) {
    idleError.value = (err as Error).message
  } finally {
    idleBusy.value = false
  }
}

async function removeIdle(id: string): Promise<void> {
  idleBusy.value = true
  try {
    idle.value = await removeIdleSong(id)
  } catch (err) {
    idleError.value = (err as Error).message
  } finally {
    idleBusy.value = false
  }
}

async function clearIdleList(): Promise<void> {
  idleBusy.value = true
  try {
    idle.value = await clearIdle()
  } catch (err) {
    idleError.value = (err as Error).message
  } finally {
    idleBusy.value = false
  }
}

/**
 * 切换播放模式。
 *
 * 「播放模式」现在放在播放器卡片里（唯一的控制点），它同时影响点歌队列
 * 与空闲歌单——`idle_mode` 就是播放模式本身。
 */
async function changeMode(mode: IdleMode): Promise<void> {
  idleBusy.value = true
  try {
    idle.value = await setIdleMode(mode)
    showToast('ok', `播放模式已切换为「${idleModes.value.find((m) => m.value === mode)?.label ?? mode}」`)
  } catch (err) {
    showToast('err', `切换播放模式失败：${(err as Error).message}`)
  } finally {
    idleBusy.value = false
  }
}

/** 当前播放模式（从空闲歌单接口读取，仅界面展示用）。 */
const idleMode = computed<IdleMode | null>(() => idle.value?.mode ?? null)

/** 可选播放模式（来自后端，避免前后端枚举顺序不一致）。 */
const idleModes = computed(() => idle.value?.modes ?? [])

/** 当前播放模式的可读名称。 */
const idleModeLabel = computed(
  () => idleModes.value.find((m) => m.value === idleMode.value)?.label ?? '读取中…',
)

/**
 * 点一下切换到下一个播放模式（循环）。
 *
 * 用一个按钮而不是四个并排按钮：模式是**互斥单选**，循环切换更省地方。
 */
async function cycleMode(): Promise<void> {
  const modes = idleModes.value
  if (!modes.length) return
  const at = modes.findIndex((m) => m.value === idleMode.value)
  const next = modes[(at + 1) % modes.length]
  await changeMode(next.value)
}

async function playIdleItem(id: string): Promise<void> {
  idleBusy.value = true
  try {
    await playIdle({ id })
    showToast('ok', '已开始播放这首空闲歌曲')
  } catch (err) {
    showToast('err', `播放失败：${(err as Error).message}`)
  } finally {
    idleBusy.value = false
  }
}

/** 把搜索结果加进空闲歌单（用的是已经解析好的 Song，省一次搜索）。 */
async function addSearchResultToIdle(song: Song): Promise<void> {
  idleBusy.value = true
  try {
    idle.value = await addIdleSong({ song })
    showToast('ok', `已加入空闲歌单：《${song.title}》`)
  } catch (err) {
    showToast('err', `加入失败：${(err as Error).message}`)
  } finally {
    idleBusy.value = false
  }
}

/** 把点歌队列里的一首也放进空闲歌单。 */
async function queueItemToIdle(item: QueueItem): Promise<void> {
  idleBusy.value = true
  try {
    idle.value = await addIdleSong({ song: item.song })
    showToast('ok', `已加入空闲歌单：《${item.song.title}》`)
  } catch (err) {
    showToast('err', `加入失败：${(err as Error).message}`)
  } finally {
    idleBusy.value = false
  }
}

/** 切到空闲歌单页时自动加载一次。 */
watch(tab, (next) => {
  if (next === 'idle' && !idle.value) void refreshIdle()
})

// ── 从收藏歌单导入（阶段 10b）────────────────────────────────────────────
//
// 需求：读取当前网易云 / QQ 音乐账号的收藏歌单列表，
// 让用户选择歌单**添加**或**覆盖**到空闲歌单。

const playlists = ref<PlaylistInfo[]>([])
const playlistsLoading = ref(false)
const playlistsError = ref<string | null>(null)
const playlistsLoggedIn = ref(true)
/** 选择哪个平台看歌单。 */
const playlistPlatform = ref<MusicPlatform>('qq')
/** 导入模式：追加 / 覆盖。 */
const importMode = ref<'append' | 'replace'>('append')
/** 正在导入的歌单 id（用于按钮 loading）。 */
const importingId = ref<string | null>(null)

async function loadPlaylists(): Promise<void> {
  playlistsLoading.value = true
  playlistsError.value = null
  try {
    const resp = await getPlaylists(playlistPlatform.value)
    playlists.value = resp.playlists
    playlistsLoggedIn.value = resp.logged_in
  } catch (err) {
    // 未登录是「还没做那一步」而不是故障：后端会回一条 notice，
    // 这里转成「未登录」提示，不要显示成请求报错。
    if (err instanceof ApiNotice) {
      playlistsLoggedIn.value = false
      playlistsError.value = null
      playlists.value = []
      showToast('info', err.notice)
    } else {
      playlistsError.value = (err as Error).message
      playlists.value = []
    }
  } finally {
    playlistsLoading.value = false
  }
}

async function doImport(pl: PlaylistInfo): Promise<void> {
  importingId.value = pl.id
  try {
    idle.value = await importPlaylistToIdle({
      platform: pl.platform,
      playlist_id: pl.id,
      mode: importMode.value,
    })
    showToast(
      'ok',
      importMode.value === 'replace'
        ? `已用《${pl.name}》覆盖空闲歌单（${idle.value.items.length} 首）`
        : `已把《${pl.name}》添加到空闲歌单（现有 ${idle.value.items.length} 首）`,
    )
  } catch (err) {
    showToast('err', `导入失败：${(err as Error).message}`)
  } finally {
    importingId.value = null
  }
}

// ── 黑名单（阶段 9）──────────────────────────────────────────────────────
//
// 需求：把某首歌拉黑后，**弹幕点这首歌直接不搜索、不入队**；
// 主播通过点歌机可以无视黑名单。

/** 黑名单列表（从配置里读，增删后刷新）。 */
const blacklist = ref<BlacklistEntry[]>([])
const blacklistError = ref<string | null>(null)
const blacklistBusy = ref(false)

/** 新增表单。 */
const blacklistForm = reactive({ title: '', artist: '', note: '' })

async function refreshBlacklist(): Promise<void> {
  try {
    blacklist.value = await getBlacklist()
    blacklistError.value = null
  } catch (err) {
    blacklistError.value = (err as Error).message
  }
}

async function submitBlacklist(): Promise<void> {
  const title = blacklistForm.title.trim()
  if (!title) return
  blacklistBusy.value = true
  try {
    blacklist.value = await addBlacklistEntry(title, blacklistForm.artist.trim(), blacklistForm.note.trim() || undefined)
    blacklistForm.title = ''
    blacklistForm.artist = ''
    blacklistForm.note = ''
    blacklistError.value = null
    showToast('ok', `已拉黑《${title}》，弹幕将无法点这首歌`)
  } catch (err) {
    blacklistError.value = (err as Error).message
    showToast('err', `拉黑失败：${(err as Error).message}`)
  } finally {
    blacklistBusy.value = false
  }
}

async function removeBlacklist(entry: BlacklistEntry): Promise<void> {
  blacklistBusy.value = true
  try {
    blacklist.value = await removeBlacklistEntry(entry.title, entry.artist)
    blacklistError.value = null
    showToast('ok', `已移出黑名单：《${entry.title}》`)
  } catch (err) {
    blacklistError.value = (err as Error).message
  } finally {
    blacklistBusy.value = false
  }
}

/** 一键把队列里某首歌拉黑（从队列直接操作最顺手）。 */
async function blacklistFromQueue(title: string, artist: string): Promise<void> {
  blacklistBusy.value = true
  try {
    blacklist.value = await addBlacklistEntry(title, artist)
    showToast('ok', `已拉黑《${title}》，并从现在起禁止弹幕点它`)
  } catch (err) {
    showToast('err', `拉黑失败：${(err as Error).message}`)
  } finally {
    blacklistBusy.value = false
  }
}

/** 切到黑名单页时自动加载一次。 */
watch(tab, (next) => {
  if (next === 'blacklist' && !blacklist.value.length) void refreshBlacklist()
})

// ── 点歌日志（阶段 8）────────────────────────────────────────────────────
//
// 完整日志走独立接口 `/api/requests/log`：`AppState.stats.recent` 只带最近
// 几十条（它要随每次状态变化经 WS 广播），拿不到「一整场直播」的记录。

/** 日志条目。 */
const requestLog = ref<RequestRecord[]>([])
const requestLogLoading = ref(false)
const requestLogError = ref<string | null>(null)
/** 筛选：all / queued / rejected。 */
const logOutcome = ref<'all' | 'queued' | 'rejected'>('all')
/** 筛选：点歌人（模糊匹配，前端做，便于边输入边过滤）。 */
const logUser = ref('')

async function refreshRequestLog(): Promise<void> {
  requestLogLoading.value = true
  requestLogError.value = null
  try {
    requestLog.value = await getRequestLog({
      outcome: logOutcome.value === 'all' ? undefined : logOutcome.value,
      limit: 2000,
    })
  } catch (err) {
    requestLogError.value = (err as Error).message
  } finally {
    requestLogLoading.value = false
  }
}

/** 前端再按点歌人过滤一次（后端是精确匹配，这里要模糊）。 */
const filteredLog = computed(() => {
  const needle = logUser.value.trim().toLowerCase()
  if (!needle) return requestLog.value
  return requestLog.value.filter(
    (r) =>
      r.user.toLowerCase().includes(needle) ||
      (r.uid ?? '').toLowerCase().includes(needle),
  )
})

/** 日志汇总：成功 / 被拒条数。 */
const logSummary = computed(() => {
  const list = filteredLog.value
  let queued = 0
  for (const r of list) if (r.outcome === 'queued') queued += 1
  return { total: list.length, queued, rejected: list.length - queued }
})

// ── 日志分页 ──────────────────────────────────────────────────────────────
//
// 一整场直播可能有上千条记录，一次性铺满页面既卡又难读。
// 分页在前端做：后端已经一次给全（最多 2000 条），避免翻页时重复请求。

/** 每页条数候选。 */
const LOG_PAGE_SIZES = [20, 50, 100] as const
const logPageSize = ref<number>(LOG_PAGE_SIZES[0])
const logPage = ref(1)

const logPageCount = computed(() => Math.max(1, Math.ceil(filteredLog.value.length / logPageSize.value)))

/** 当前页的日志切片。 */
const pagedLog = computed(() => {
  const start = (logPage.value - 1) * logPageSize.value
  return filteredLog.value.slice(start, start + logPageSize.value)
})

/** 筛选条件或页大小变化时回到第一页，避免停在越界的空页上。 */
watch([logOutcome, logUser, logPageSize], () => {
  logPage.value = 1
})

/** 条目变少（例如重新拉取）导致页码越界时收敛。 */
watch(logPageCount, (count) => {
  if (logPage.value > count) logPage.value = count
})

function goLogPage(next: number): void {
  logPage.value = Math.min(Math.max(1, next), logPageCount.value)
}

/** 拒绝原因的中文说明。 */
const REJECT_LABELS: Record<string, string> = {
  cooldown: '冷却中',
  duplicate: '重复点歌',
  queue_full: '队列已满',
  fans_medal: '粉丝牌不足',
  user_level: '等级不足',
}

function rejectLabel(reason: string | null): string {
  if (!reason) return '—'
  return REJECT_LABELS[reason] ?? reason
}

/**
 * 导出为 CSV。
 *
 * 用带 BOM 的 UTF-8：Excel 打开中文 CSV 时不带 BOM 会乱码。
 */
function exportRequestLog(): void {
  const header = ['时间', '点歌人', 'UID', '歌名', '歌手', '结果', '拒绝原因']
  const rows = filteredLog.value.map((r) => [
    r.at,
    r.user,
    r.uid ?? '',
    r.title,
    r.artist,
    r.outcome === 'queued' ? '成功' : '失败',
    r.outcome === 'queued' ? '' : rejectLabel(r.reason),
  ])
  const escape = (v: string) => `"${v.replace(/"/g, '""')}"`
  const csv = [header, ...rows].map((row) => row.map(escape).join(',')).join('\r\n')
  const blob = new Blob([`\uFEFF${csv}`], { type: 'text/csv;charset=utf-8' })
  const url = URL.createObjectURL(blob)
  const a = document.createElement('a')
  a.href = url
  a.download = `点歌日志-${new Date().toISOString().slice(0, 10)}.csv`
  a.click()
  URL.revokeObjectURL(url)
}

/** 切到日志页时自动加载一次（也支持手动刷新）。 */
watch(tab, (next) => {
  if (next === 'logs' && !requestLog.value.length) void refreshRequestLog()
})

/**
 * 综合面板地址（保留一个快捷预览入口）。
 *
 * 面板的样式参数、四个专注页地址与预览已移到独立的 `Panels.vue`（`/panels`）：
 * 那些配置堆在这个标签页里会把弹幕/队列区挤下去。
 * 这里只留一个「快速预览」链接，用配置里的默认样式。
 */
const panelUrl = computed(() => apiUrl('/panel'))

/** 配置编辑副本，保存时整体提交。 */
const draft = ref<Config | null>(null)
const saved = ref(false)
/** 拉取配置是否失败（用于给出重试入口，而不是永远显示「加载中」）。 */
const draftError = ref<string | null>(null)

function resetDraft(): void {
  // ⚠️ 必须用 clonePlain：store.config 是 Vue 响应式代理，
  // WebView2 下 structuredClone 会抛「#<Object> could not be cloned」，
  // 导致 draft 永远是 null、设置页永久停在「正在加载配置…」。
  draft.value = store.config ? clonePlain(store.config) : null
}

/**
 * 确保设置表单有数据可编辑。
 *
 * ## 为什么不能只依赖 bootstrap
 * 之前这里只做「轮询等 `store.config` 被 bootstrap 填上」，
 * 一旦 bootstrap 因为任何原因没能赋值（时序、某个接口失败），
 * `draft` 就永远是 null，设置页**永久停在「正在加载配置…」**，
 * 而且没有任何重试入口——用户看到的就是「点设置什么都没有」。
 *
 * 现在改为：能直接用就用；拿不到就**自己发一次请求**，失败也要给出原因与重试按钮。
 */
async function ensureDraft(): Promise<void> {
  if (draft.value) return
  if (store.config) {
    resetDraft()
    return
  }
  draftError.value = null
  try {
    const cfg = await getConfig()
    // 来自 fetch 的普通对象，但统一走 clonePlain 以免以后改成读 store 时踩坑
    draft.value = clonePlain(cfg)
  } catch (err) {
    draftError.value = (err as Error).message
  }
}

onMounted(() => {
  void ensureDraft()
  // 播放模式（= 空闲歌单模式）现在显示在**播放器卡片**里，
  // 而它在首屏可见，因此启动时就要加载，不能等用户切到空闲歌单页。
  void refreshIdle()
})

async function saveSettings(): Promise<void> {
  if (!draft.value) return
  await store.updateConfig(draft.value)
  saved.value = !store.error
  window.setTimeout(() => (saved.value = false), 1500)
}

function formatTime(iso: string): string {
  const d = new Date(iso)
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleTimeString('zh-CN')
}
</script>

<template>
  <div class="dashboard">
    <div class="tabs">
      <button :class="{ active: tab === 'live' }" @click="tab = 'live'">直播与播放</button>
      <button :class="{ active: tab === 'queue' }" @click="tab = 'queue'">
        点歌队列 <span class="badge">{{ store.queue.length }}</span>
      </button>
      <button :class="{ active: tab === 'idle' }" @click="tab = 'idle'">
        空闲歌单 <span class="badge">{{ idle?.items.length ?? 0 }}</span>
      </button>
      <button :class="{ active: tab === 'logs' }" @click="tab = 'logs'">点歌日志</button>
      <button :class="{ active: tab === 'blacklist' }" @click="tab = 'blacklist'">
        黑名单 <span v-if="blacklist.length" class="badge">{{ blacklist.length }}</span>
      </button>
      <button :class="{ active: tab === 'settings' }" @click="tab = 'settings'">设置</button>
      <button class="ghost" :disabled="store.loading" @click="store.refresh()">刷新状态</button>
    </div>

    <!-- ── 直播与播放 ───────────────────────────────────────────── -->
    <section v-if="tab === 'live'" class="grid">
      <article class="card">
        <h3>B 站弹幕连接</h3>
        <div class="fields">
          <label>app_id
            <input v-model="credentials.app_id" placeholder="项目 ID（纯数字，不是用户名）" />
          </label>
          <label>access_key_id <input v-model="credentials.access_key_id" placeholder="访问密钥 ID" /></label>
          <label class="wide">access_key_secret
            <input v-model="credentials.access_key_secret" type="password" placeholder="访问密钥 Secret" />
          </label>
          <label class="wide">身份码 code
            <input v-model="credentials.code" placeholder="形如 xxxx-xxxx-xxxx-xxxx" />
          </label>
        </div>

        <div class="controls">
          <button :disabled="store.connecting" @click="connectBilibili()">
            <span v-if="store.connecting" class="spinner" aria-hidden="true" />
            {{ connectButtonText }}
          </button>
          <button
            class="ghost"
            :disabled="store.connecting || !canDisconnect"
            @click="disconnectBilibili()"
          >
            断开
          </button>
          <label class="check inline">
            <input
              type="checkbox"
              :checked="store.config?.bilibili.auto_connect ?? false"
              @change="toggleAutoConnect($event)"
            />
            启动时自动连接
          </label>
        </div>

        <!-- 连接进度：让用户始终知道「在连 / 连上了 / 失败了」 -->
        <div class="conn-status" :class="connStatus.tone" role="status" aria-live="polite">
          <div class="conn-head">
            <span class="conn-dot" :class="connStatus.tone" />
            <strong>{{ connStatus.title }}</strong>
            <span v-if="connStatus.busy" class="spinner" aria-hidden="true" />
            <span v-if="connStatus.attempts > 1" class="conn-attempts">
              第 {{ connStatus.attempts }} 次尝试
            </span>
          </div>
          <p class="conn-detail">{{ connStatus.detail }}</p>
          <p v-if="connStatus.hint" class="conn-hint">{{ connStatus.hint }}</p>
          <p v-if="connStatus.updatedAt" class="conn-time">最后更新：{{ connStatus.updatedAt }}</p>
        </div>

        <dl class="kv" style="margin-top: 12px">
          <dt>状态</dt>
          <dd :class="bilibiliBadge.tone === 'ok' ? 'ok' : bilibiliBadge.tone === 'busy' ? 'busy' : 'off'">
            {{ bilibiliBadge.text }}
          </dd>
          <dt>直播间</dt>
          <dd>{{ connRoomId ?? '—' }}</dd>
          <dt>最近错误</dt>
          <dd class="err">{{ connLastError ?? '—' }}</dd>
        </dl>

      </article>

      <article class="card">
        <h3>音乐平台</h3>
        <div class="platform-row">
          <div v-for="platform in (['netease', 'qq'] as MusicPlatform[])" :key="platform" class="platform">
            <div class="platform-head">
              <span class="platform-name">{{ platformLabel(platform) }}</span>
              <span class="platform-badge" :class="platformLoggedIn(platform) ? 'ok' : 'off'">
                {{ platformLoggedIn(platform) ? '已登录' : '未登录' }}
              </span>
            </div>
            <div class="controls">
              <button :disabled="!isDesktop()" @click="startMusicLogin(platform)">
                {{ platformLoggedIn(platform) ? '重新登录' : '打开登录窗口' }}
              </button>
              <button
                class="ghost"
                :disabled="!platformLoggedIn(platform)"
                @click="store.clearMusicCookie(platform)"
              >
                退出登录
              </button>
            </div>
          </div>
        </div>

        <!-- 非桌面环境（OBS/浏览器）无法开登录窗口，用这里手动填 Cookie -->
        <details class="help">
          <summary>手动粘贴 Cookie</summary>
          <div class="fields">
            <label>平台
              <select v-model="manualCookie.platform">
                <option value="netease">网易云音乐</option>
                <option value="qq">QQ 音乐</option>
              </select>
            </label>
            <label class="wide">Cookie
              <input v-model="manualCookie.value" placeholder="MUSIC_U=...; __csrf=..." />
            </label>
          </div>
          <div class="controls">
            <button :disabled="!manualCookie.value.trim()" @click="submitManualCookie()">保存 Cookie</button>
          </div>
        </details>

        <h4>搜索并加歌</h4>
        <div class="fields">
          <label class="wide">关键词
            <input
              v-model="store.searchKeyword"
              placeholder="你还在不在 梁静茹"
              @keyup.enter="store.searchMusic()"
            />
          </label>
          <label>平台
            <select v-model="selectedPlatform">
              <option value="netease">网易云音乐</option>
              <option value="qq">QQ 音乐</option>
            </select>
          </label>
        </div>
        <div class="controls">
          <button :disabled="store.searching" @click="store.searchMusic()">
            {{ store.searching ? '搜索中…' : '搜索' }}
          </button>
          <button class="ghost" :disabled="!store.searchKeyword.trim()" @click="addByText()">
            直接入队（异步搜索）
          </button>
        </div>

        <ul v-if="store.searchResults.length" class="search-list">
          <li v-for="song in store.searchResults" :key="song.id">
            <span class="sl-title">{{ song.title }}</span>
            <span class="sl-artist">{{ song.artist }}</span>
            <span class="sl-meta">
              {{ song.album || '' }}<template v-if="formatSongDuration(song.duration)"> · {{ formatSongDuration(song.duration) }}</template>
            </span>
            <button
              class="ghost"
              title="加入空闲歌单（用已解析的曲目，省一次搜索）"
              :disabled="idleBusy"
              @click="addSearchResultToIdle(song)"
            >
              → 空闲
            </button>
            <button class="sl-add" @click="addSearchResultToQueue(song)">加入队列</button>
          </li>
        </ul>
        <p v-else-if="store.searchKeyword && !store.searching" class="empty">
          没有结果。若提示未登录，请先登录；若提示接口已变化，说明平台接口调整了。
        </p>

      </article>

      <article class="card">
        <h3>正在播放</h3>
        <div class="now" :class="{ 'now-idle': !store.current }">
          <div class="now-title">{{ store.current?.song.title ?? '当前没有播放中的歌曲' }}</div>
          <div class="now-artist">
            {{ store.current?.song.artist ?? (store.queue.length ? '队列里还有待播歌曲' : '点歌或播放队列后开始') }}
          </div>
          <div class="now-meta">
            <template v-if="store.current">
              点歌人：{{ store.current.requested_by }} · {{ formatTime(store.current.requested_at) }}
            </template>
            <template v-else>播放器已就绪</template>
          </div>
          <!--
            进度条做成真正的 <input type="range">：之前只是一个展示用的 div，
            既不能点击也不能拖动。拖动只更新本地预览，松手才真正 seek。
            没有歌曲时**保留组件本身**（置灰禁用），避免整块控件消失导致布局跳动。
          -->
          <input
            class="progress-range"
            type="range"
            min="0"
            :max="Math.max(1, Math.floor(store.player?.duration ?? 0))"
            step="1"
            :value="seekPreview ?? Math.floor(store.player?.position ?? 0)"
            :disabled="!store.current"
            :style="{ '--seek-progress': `${progressPercent}%` }"
            @input="onSeekInput"
            @change="onSeekCommit"
          />
          <div class="progress-times">
            <span>{{ formatClock(store.current ? (seekPreview ?? store.player?.position ?? 0) : 0) }}</span>
            <span>{{ formatClock(store.current ? (store.player?.duration ?? 0) : 0) }}</span>
          </div>
          <div class="controls">
            <button :disabled="!store.current" title="回到上一首" @click="goPrevious()">⏮ 上一首</button>
            <button :disabled="!store.current" title="当前歌曲重头播放" @click="store.replay()">↺ 重头</button>
            <!--
              这一个按钮同时承担「开始播放」与「暂停/继续」：
              没有在播时点它是"开始"（重启后会接着上次的秒数）；
              在播时点它是"暂停/继续"。之前这里有两个按钮（含一个只显示
              「正在播放」的），看起来重复且无用，已合并。
            -->
            <button
              :disabled="(!store.current && !store.queue.length) || !store.playerAvailable"
              :title="playButtonTitle"
              @click="togglePlayPause()"
            >
              {{ playButtonText }}
            </button>
            <button title="播放下一首（队列空时接空闲歌单）" @click="store.skip()">⏭ 下一首</button>
          </div>
        </div>

        <!-- 播放模式：一个按钮循环切换（顺序 → 列表循环 → 单曲循环 → 随机）-->
        <div class="controls" style="margin-top: 12px">
          <button
            :disabled="idleBusy || !idleModes.length"
            title="点击切换播放模式"
            @click="cycleMode()"
          >
            🔁 播放模式：{{ idleModeLabel }}
          </button>
        </div>

        <label class="volume">
          <!-- 固定宽度：否则个位数/两位数/100 三种宽度会把右边的进度条左右推挤 -->
          <span class="vol-label">音量 {{ volumePreview ?? store.player?.volume ?? 80 }}</span>
          <input
            type="range"
            min="0"
            max="100"
            :value="volumePreview ?? store.player?.volume ?? 80"
            @input="onVolumeInput"
            @change="onVolumeCommit"
          />
        </label>
      </article>

      <article class="card">
        <h3>最近弹幕</h3>
        <ul v-if="store.danmakuLog.length" class="danmaku">
          <li v-for="(d, i) in store.danmakuLog.slice(0, 12)" :key="i">
            <span class="dm-user">{{ d.user }}</span>
            <span class="dm-text">{{ d.text }}</span>
          </li>
        </ul>
        <p v-else class="empty">暂无弹幕。连接 B 站身份码后这里会实时滚动，也可以用上面的「链路自测」注入。</p>
      </article>

      <article class="card">
        <h3>最近点歌请求</h3>
        <div class="req-summary">
          <span>近 {{ requestSummary.total }} 条：</span>
          <span class="ok">入队 {{ requestSummary.queued }}</span>
          <span class="bad">拒绝 {{ requestSummary.rejected }}</span>
        </div>
        <ul v-if="store.recentRequests.length" class="req-list">
          <li
            v-for="(item, index) in store.recentRequests.slice(0, 12)"
            :key="`${item.at}-${index}`"
            :class="item.outcome === 'queued' ? 'req-ok' : 'req-bad'"
          >
            <span class="req-user">{{ item.user }}</span>
            <span class="req-title">
              {{ item.title }}<template v-if="item.artist"> · {{ item.artist }}</template>
            </span>
            <span v-if="item.outcome === 'queued'" class="req-tag ok">第 {{ item.position }} 位</span>
            <span v-else class="req-tag bad" :title="item.message ?? ''">
              {{ reasonLabel(item.reason) }}
            </span>
            <span class="req-time">{{ formatAge(item.at) }}</span>
          </li>
        </ul>
        <p v-else class="empty">还没有点歌请求。观众发送「点歌 歌名 歌手」后会出现在这里。</p>

        <template v-if="store.cooldowns.length">
          <h4>冷却中（{{ store.cooldowns.length }}）</h4>
          <ul class="cooldown-list">
            <li v-for="c in store.cooldowns" :key="c.key">
              <span class="cd-key">{{ c.key }}</span>
              <span class="cd-time">{{ formatCooldown(c.key) }}</span>
            </li>
          </ul>
        </template>
      </article>

      <!--
        链路自测放在**最底部**：它只是排障工具，放在顶部会挤掉真正要看的内容
        （用户反馈「在下面看不清顶部的报错提示」，顶部越少干扰越好）。
      -->

      <article class="card">
        <h3>链路自测（排障用）</h3>
        <div class="fields">
          <label class="wide">弹幕文本
            <input
              v-model="simulateText"
              placeholder="点歌 你还在不在 梁静茹"
              @keyup.enter="store.simulateDanmaku(simulateText)"
            />
          </label>
        </div>
        <div class="controls">
          <button @click="store.simulateDanmaku(simulateText)">注入弹幕</button>
          <button class="ghost" @click="store.simulateDanmaku('点歌 你还在不在', '测试观众')">
            注入非点歌弹幕
          </button>
        </div>
      </article>

      <!-- 面板配置入口：卡片已移除（内容都在「OBS 面板」页），只留一个链接 -->
      <p class="panel-entry">
        <RouterLink class="link" to="/panels">OBS 面板设置 →</RouterLink>
        <a class="link" :href="panelUrl" target="_blank" rel="noreferrer">预览综合面板</a>
      </p>
    </section>

    <!-- ── 点歌队列 ─────────────────────────────────────────────── -->
    <section v-else-if="tab === 'queue'" class="card">
      <div class="card-head">
        <h3>点歌队列（{{ store.queue.length }}）</h3>
        <div class="controls">
          <button :disabled="!store.queue.length" title="只清空点歌队列" @click="store.clearAll()">
            清空点歌队列
          </button>
          <button
            class="primary"
            :disabled="!canClearPlaylist"
            title="清空点歌队列与播放列表（保留空闲歌单）"
            @click="clearPlaylist"
          >
            清空播放列表
          </button>
          <button
            :disabled="!store.queue.length"
            title="清空点歌队列并立即播放空闲歌单"
            @click="store.clearAndPlayIdle()"
          >
            清空并播空闲歌单
          </button>
          <button :disabled="!store.current" @click="store.skip()">跳过当前</button>
        </div>
      </div>

      <table v-if="store.queue.length" class="queue-table">
        <thead>
          <tr>
            <th>#</th>
            <th>歌曲</th>
            <th>歌手</th>
            <th>专辑 / 时长</th>
            <th>点歌人</th>
            <th>时间</th>
            <th>操作</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="(item, index) in store.queue" :key="item.id">
            <td>{{ index + 1 }}</td>
            <td>
              {{ item.song.title }}
              <span v-if="item.priority === 'host'" class="prio-tag host" title="主播点歌（优先播放）">
                主播
              </span>
              <span v-if="sourceLabel(item.song)" class="src-tag" :class="item.song.source">
                {{ sourceLabel(item.song) }}
              </span>
            </td>
            <td>{{ item.song.artist }}</td>
            <td class="dim">
              <template v-if="item.song.album">{{ item.song.album }}</template>
              <template v-if="formatSongDuration(item.song.duration)">
                <template v-if="item.song.album"> · </template>{{ formatSongDuration(item.song.duration) }}
              </template>
              <template v-if="!item.song.album && !item.song.duration">—</template>
            </td>
            <td>{{ item.requested_by }}</td>
            <td>{{ formatTime(item.requested_at) }}</td>
            <td class="row-actions">
              <button @click="store.topItem(item.id)">置顶</button>
              <button @click="store.moveUp(item.id)">上移</button>
              <button @click="store.moveDown(item.id)">下移</button>
              <button
                v-if="item.song.source === 'resolved'"
                :title="'取播放地址（' + item.song.id + '）'"
                @click="store.fetchPlayUrl(item.song.id, item.song.platform)"
              >
                取地址
              </button>
              <button
                class="ghost"
                title="把这首也放进空闲歌单"
                :disabled="idleBusy"
                @click="queueItemToIdle(item)"
              >
                → 空闲
              </button>
              <button
                class="danger"
                :title="'拉黑《' + item.song.title + '》，之后弹幕不能再点这首歌'"
                :disabled="blacklistBusy"
                @click="blacklistFromQueue(item.song.title, item.song.artist)"
              >
                拉黑
              </button>
              <button class="danger" @click="store.removeItem(item.id)">删除</button>
            </td>
          </tr>
        </tbody>
      </table>
      <p v-if="store.lastPlayUrl" class="play-url">
        <strong>播放地址：</strong><code>{{ store.lastPlayUrl }}</code>
      </p>
      <p v-else class="empty">队列为空。连接弹幕后，观众发送「点歌 歌名 歌手」即可自动入队。</p>
    </section>

    <!-- ── 空闲歌单（阶段 10a）──────────────────────────────────── -->
    <section v-else-if="tab === 'idle'" class="card">
      <div class="card-head">
        <h3>空闲歌单（{{ idle?.items.length ?? 0 }}）</h3>
        <div class="controls">
          <button class="ghost" :disabled="idleBusy" @click="refreshIdle()">刷新</button>
          <button
            :disabled="idleBusy || !idle?.items.length"
            @click="idle && idle.items.length && playIdleItem(idle.items[0].id)"
          >
            从第一首开始播
          </button>
          <button class="danger" :disabled="idleBusy || !idle?.items.length" @click="clearIdleList()">
            清空
          </button>
        </div>
      </div>
      <!-- 有人点歌时怎么切 -->
      <h4>有人点歌时</h4>
      <div v-if="draft" class="fields">
        <label class="wide">切换策略
          <select v-model="draft.rules.idle_switch_policy">
            <option value="immediate">立即播放点的歌曲（中断当前空闲歌曲）</option>
            <option value="after_current">放完当前这首空闲歌曲再播点歌</option>
          </select>
        </label>
      </div>
      <!-- 加歌 -->
      <h4>加歌</h4>
      <div class="fields">
        <label>歌名
          <input v-model="idleForm.title" placeholder="例如 晴天" @keyup.enter="submitIdle()" />
        </label>
        <label>歌手（可留空）
          <input v-model="idleForm.artist" placeholder="例如 周杰伦" @keyup.enter="submitIdle()" />
        </label>
      </div>
      <div class="controls">
        <button :disabled="!idleForm.title.trim() || idleBusy" @click="submitIdle()">加入空闲歌单</button>
      </div>
      <p v-if="idleError" class="err">{{ idleError }}</p>

      <!-- ── 从收藏歌单导入（阶段 10b）──────────────────────────── -->
      <h4>从收藏歌单导入</h4>
      <div class="fields">
        <label>平台
          <select v-model="playlistPlatform" @change="loadPlaylists()">
            <option value="qq">QQ 音乐</option>
            <option value="netease">网易云音乐</option>
          </select>
        </label>
        <label>导入方式
          <select v-model="importMode">
            <option value="append">添加到现有歌单后面</option>
            <option value="replace">覆盖（先清空空闲歌单）</option>
          </select>
        </label>
      </div>
      <div class="controls">
        <button class="ghost" :disabled="playlistsLoading" @click="loadPlaylists()">
          {{ playlistsLoading ? '读取中…' : '读取我的收藏歌单' }}
        </button>
      </div>
      <p v-if="playlistsError" class="err">{{ playlistsError }}</p>
      <ul v-if="playlists.length" class="playlist-list">
        <li v-for="pl in playlists" :key="pl.id">
          <div class="pl-meta">
            <strong>{{ pl.name }}</strong>
            <span v-if="pl.special" class="req-tag ok">我喜欢</span>
            <span class="dim">{{ pl.track_count }} 首</span>
            <span v-if="pl.creator" class="dim">· {{ pl.creator }}</span>
          </div>
          <button
            :disabled="importingId !== null"
            @click="doImport(pl)"
          >
            {{ importingId === pl.id ? '导入中…' : (importMode === 'replace' ? '覆盖导入' : '添加导入') }}
          </button>
        </li>
      </ul>
      <!-- 列表 -->
      <table v-if="idle?.items.length" class="queue-table">
        <thead>
          <tr>
            <th>#</th>
            <th>歌名</th>
            <th>歌手</th>
            <th>状态</th>
            <th>时长</th>
            <th>操作</th>
          </tr>
        </thead>
        <tbody>
          <tr
            v-for="(item, index) in idle.items"
            :key="item.id"
            :class="{ 'row-current': store.current?.id === item.id }"
          >
            <td>
              <span v-if="store.current?.id === item.id">▶</span>
              <span v-else>{{ index + 1 }}</span>
            </td>
            <td>{{ item.song.title }}</td>
            <td>{{ item.song.artist || '—' }}</td>
            <td>
              <span v-if="item.song.source === 'resolved'" class="req-tag ok">已解析</span>
              <span v-else-if="item.song.source === 'failed'" class="req-tag bad" :title="item.song.source_error ?? ''">
                解析失败
              </span>
              <span v-else class="req-tag">解析中</span>
            </td>
            <td class="dim">{{ formatSongDuration(item.song.duration) || '—' }}</td>
            <td class="row-actions">
              <button :disabled="idleBusy" @click="playIdleItem(item.id)">播放</button>
              <button class="danger" :disabled="idleBusy" @click="removeIdle(item.id)">移除</button>
            </td>
          </tr>
        </tbody>
      </table>
      <p v-else class="empty">空闲歌单是空的。加上几首，点歌队列播完就会自动接着放。</p>
    </section>

    <!-- ── 点歌日志（阶段 8）────────────────────────────────────── -->
    <section v-else-if="tab === 'logs'" class="card">
      <div class="card-head">
        <h3>点歌日志</h3>
        <div class="controls">
          <button class="ghost" :disabled="requestLogLoading" @click="refreshRequestLog()">
            {{ requestLogLoading ? '加载中…' : '刷新' }}
          </button>
          <button class="ghost" :disabled="!filteredLog.length" @click="exportRequestLog()">
            导出 CSV
          </button>
        </div>
      </div>

      <div class="fields">
        <label>结果
          <select v-model="logOutcome" @change="refreshRequestLog()">
            <option value="all">全部</option>
            <option value="queued">成功</option>
            <option value="rejected">被拒绝</option>
          </select>
        </label>
        <label>点歌人
          <input v-model="logUser" placeholder="按昵称或 UID 过滤" />
        </label>
      </div>

      <p v-if="requestLogError" class="err">读取日志失败：{{ requestLogError }}</p>

      <table v-if="filteredLog.length" class="queue-table log-table">
        <thead>
          <tr>
            <th>时间</th>
            <th>点歌人</th>
            <th>歌名</th>
            <th>歌手</th>
            <th>结果</th>
            <th>拒绝原因</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="(r, i) in pagedLog" :key="`${r.at}-${i}`">
            <td class="dim">{{ formatTime(r.at) }}</td>
            <td>{{ r.user }}</td>
            <td>{{ r.title }}</td>
            <td>{{ r.artist || '—' }}</td>
            <td>
              <span class="outcome-tag" :class="r.outcome">
                {{ r.outcome === 'queued' ? '成功' : '失败' }}
              </span>
            </td>
            <td class="dim">
              {{ r.outcome === 'queued' ? '—' : rejectLabel(r.reason) }}
              <template v-if="r.outcome !== 'queued' && r.message"> · {{ r.message }}</template>
            </td>
          </tr>
        </tbody>
      </table>
      <p v-else-if="!requestLogLoading" class="empty">还没有点歌记录。</p>

      <!-- 分页：一整场直播可能上千条，一次铺满既卡又难读 -->
      <div v-if="filteredLog.length" class="pager">
        <span class="dim">
          第 {{ logPage }} / {{ logPageCount }} 页 · 共 {{ logSummary.total }} 条
        </span>
        <div class="controls">
          <select v-model.number="logPageSize" title="每页条数">
            <option v-for="size in LOG_PAGE_SIZES" :key="size" :value="size">每页 {{ size }} 条</option>
          </select>
          <button :disabled="logPage <= 1" @click="goLogPage(1)">首页</button>
          <button :disabled="logPage <= 1" @click="goLogPage(logPage - 1)">上一页</button>
          <button :disabled="logPage >= logPageCount" @click="goLogPage(logPage + 1)">下一页</button>
          <button :disabled="logPage >= logPageCount" @click="goLogPage(logPageCount)">末页</button>
        </div>
      </div>
    </section>

    <!-- ── 黑名单（阶段 9）──────────────────────────────────────── -->
    <section v-else-if="tab === 'blacklist'" class="card">
      <div class="card-head">
        <h3>点歌黑名单（{{ blacklist.length }}）</h3>
        <div class="controls">
          <button class="ghost" :disabled="blacklistBusy" @click="refreshBlacklist()">刷新</button>
        </div>
      </div>
      <div class="fields">
        <label>歌名
          <input v-model="blacklistForm.title" placeholder="例如 晴天" @keyup.enter="submitBlacklist()" />
        </label>
        <label>歌手（可留空）
          <input v-model="blacklistForm.artist" placeholder="留空 = 所有版本" @keyup.enter="submitBlacklist()" />
        </label>
        <label>备注（可选）
          <input v-model="blacklistForm.note" placeholder="为什么拉黑" @keyup.enter="submitBlacklist()" />
        </label>
      </div>
      <div class="controls">
        <button :disabled="!blacklistForm.title.trim() || blacklistBusy" @click="submitBlacklist()">
          加入黑名单
        </button>
      </div>
      <p v-if="blacklistError" class="err">{{ blacklistError }}</p>

      <table v-if="blacklist.length" class="queue-table">
        <thead>
          <tr>
            <th>#</th>
            <th>歌名</th>
            <th>歌手</th>
            <th>备注</th>
            <th>加入时间</th>
            <th>操作</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="(entry, index) in blacklist" :key="`${entry.title}-${entry.artist}`">
            <td>{{ index + 1 }}</td>
            <td>{{ entry.title }}</td>
            <td>{{ entry.artist || '（所有版本）' }}</td>
            <td class="dim">{{ entry.note || '—' }}</td>
            <td class="dim">{{ entry.added_at ? formatTime(entry.added_at) : '—' }}</td>
            <td>
              <button class="ghost" :disabled="blacklistBusy" @click="removeBlacklist(entry)">移出</button>
            </td>
          </tr>
        </tbody>
      </table>
      <p v-else class="empty">黑名单是空的。</p>
    </section>

    <!-- ── 设置 ────────────────────────────────────────────────── -->
    <section v-else class="card">
      <div class="card-head">
        <h3>设置</h3>
        <div class="controls">
          <button class="ghost" @click="resetDraft()">重置</button>
          <button :disabled="!draft" @click="saveSettings()">{{ saved ? '已保存' : '保存' }}</button>
        </div>
      </div>

      <template v-if="draft">        <h4>内嵌服务器</h4>
        <div class="fields">
          <label>监听地址 <input v-model="draft.server.host" /></label>
          <label>端口 <input type="number" v-model.number="draft.server.port" /></label>
        </div>
        <h4>点歌规则</h4>
        <div class="fields">
          <label class="wide">指令正则 <input v-model="draft.rules.command_regex" /></label>
          <label>冷却（秒）<input type="number" v-model.number="draft.rules.cooldown_secs" /></label>
          <label>弹幕点歌上限
            <input type="number" v-model.number="draft.rules.max_queue" />
          </label>
          <label>主播每首补名额
            <input type="number" v-model.number="draft.rules.host_extra_per_play" />
          </label>
          <label class="check"><input type="checkbox" v-model="draft.rules.allow_duplicate" /> 允许重复点歌</label>
          <label>粉丝牌等级下限<input type="number" v-model.number="draft.rules.min_fans_medal_level" /></label>
          <label>用户等级下限<input type="number" v-model.number="draft.rules.min_user_level" /></label>
        </div>
        <h4>搜索平台</h4>
        <div class="fields">
          <label class="wide">用哪个平台搜索
            <select v-model="draft.rules.search_platform">
              <option value="auto">自动（QQ 优先，必要时用网易云）</option>
              <option value="qq">只用 QQ 音乐</option>
              <option value="netease">只用网易云音乐</option>
            </select>
          </label>
        </div>
        <h4>点歌优先级</h4>
        <div class="fields">
          <label class="wide">搜不到精确原唱时
            <select v-model="draft.rules.pick_policy">
              <option value="prefer_full_length">优先完整时长（推荐）</option>
              <option value="prefer_artist">优先原唱</option>
            </select>
          </label>
        </div>
        <h4>面板默认样式</h4>
        <div class="controls">
          <RouterLink class="link" to="/panels">打开 OBS 面板配置 →</RouterLink>
        </div>
      </template>
      <!-- 拿不到配置时给出明确原因与重试，绝不无尽「加载中」 -->
      <div v-else-if="draftError" class="empty">
        <p class="err">读取配置失败：{{ draftError }}</p>
        <button class="ghost" @click="ensureDraft()">重试</button>
      </div>
      <p v-else class="empty">正在加载配置…</p>
    </section>
  </div>
</template>

<style scoped>
.dashboard {
  display: flex;
  flex-direction: column;
  gap: 16px;
}

.tabs {
  display: flex;
  gap: 8px;
  align-items: center;
}

.tabs button,
.controls button,
.row-actions button {
  padding: 6px 12px;
  border: 1px solid var(--bsr-border);
  border-radius: 6px;
  background: var(--bsr-bg-elevated);
  color: var(--bsr-fg);
  font-size: 13px;
  cursor: pointer;
}

.tabs button.active {
  border-color: var(--bsr-accent);
  background: var(--bsr-accent-soft);
}

.tabs button.ghost {
  margin-left: auto;
}

button:disabled {
  opacity: 0.45;
  cursor: not-allowed;
}

button.danger {
  border-color: var(--bsr-danger);
  color: var(--bsr-danger);
}

.badge {
  display: inline-block;
  min-width: 18px;
  margin-left: 4px;
  padding: 0 5px;
  border-radius: 999px;
  background: var(--bsr-accent-soft);
  font-size: 11px;
}

.grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(320px, 1fr));
  gap: 16px;
}

.panel-entry {
  grid-column: 1 / -1;
  display: flex;
  gap: 16px;
  flex-wrap: wrap;
  margin: 0;
  padding: 0 2px;
}

.card {
  padding: 16px;
  border: 1px solid var(--bsr-border);
  border-radius: 10px;
  background: var(--bsr-bg-elevated);
}

.card h3 {
  margin: 0 0 10px;
  font-size: 15px;
}

.card h4 {
  margin: 18px 0 8px;
  font-size: 13px;
  color: var(--bsr-muted);
}

.card-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
}

.hint,
.empty {
  margin: 8px 0 0;
  font-size: 12px;
  color: var(--bsr-muted);
  line-height: 1.6;
}

/* 设置页里的说明性列表（比 .hint 更易读的逐条解释） */
.hint-list {
  margin: 6px 0 0;
  padding-left: 20px;
  font-size: 12px;
  color: var(--bsr-muted);
  line-height: 1.7;
}

.hint-list li + li {
  margin-top: 4px;
}

.hint-list b,
.hint b {
  color: var(--bsr-fg);
}

.kv {
  display: grid;
  grid-template-columns: 84px 1fr;
  gap: 6px 12px;
  margin: 0;
  font-size: 13px;
}

.kv dt {
  color: var(--bsr-muted);
}

.kv dd {
  margin: 0;
}

.kv dd.ok {
  color: var(--bsr-success);
}

.kv dd.off {
  color: var(--bsr-muted);
}

.kv dd.busy {
  color: var(--bsr-accent);
}

.kv dd.err {
  color: var(--bsr-danger);
  word-break: break-all;
}

/* 「启动时自动连接」等行内复选框 */
.check.inline {
  display: flex;
  flex-direction: row;
  align-items: center;
  gap: 6px;
  font-size: 12px;
  color: var(--bsr-muted);
  cursor: pointer;
}

.help {
  margin-top: 12px;
  font-size: 12px;
  color: var(--bsr-muted);
}

.help summary {
  cursor: pointer;
  color: var(--bsr-accent);
}

.help p {
  margin: 8px 0 0;
  line-height: 1.7;
}

/* ── 点歌请求列表（阶段 4）────────────────────────────────────────────── */
.req-summary {
  display: flex;
  gap: 10px;
  font-size: 12px;
  color: var(--bsr-muted);
}

.req-summary .ok {
  color: var(--bsr-success);
}

.req-summary .bad {
  color: var(--bsr-danger);
}

.req-list,
.cooldown-list {
  margin: 10px 0 0;
  padding: 0;
  list-style: none;
  display: flex;
  flex-direction: column;
  gap: 5px;
  max-height: 260px;
  overflow: auto;
}

.req-list li {
  display: grid;
  grid-template-columns: 5.5em 1fr auto auto;
  gap: 8px;
  align-items: baseline;
  padding: 5px 8px;
  border-radius: 6px;
  background: var(--bsr-bg);
  border-left: 3px solid var(--bsr-border);
  font-size: 13px;
}

.req-list li.req-ok {
  border-left-color: var(--bsr-success);
}

.req-list li.req-bad {
  border-left-color: var(--bsr-danger);
}

.req-user {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--bsr-accent);
}

.req-title {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.req-tag {
  font-size: 11px;
  padding: 1px 6px;
  border-radius: 999px;
  white-space: nowrap;
}

.req-tag.ok {
  background: color-mix(in srgb, var(--bsr-success) 20%, transparent);
  color: var(--bsr-success);
}

.req-tag.bad {
  background: color-mix(in srgb, var(--bsr-danger) 20%, transparent);
  color: var(--bsr-danger);
}

.req-time {
  font-size: 11px;
  color: var(--bsr-muted);
  white-space: nowrap;
}

.cooldown-list li {
  display: flex;
  justify-content: space-between;
  font-size: 12px;
  color: var(--bsr-muted);
  padding: 3px 8px;
  border-radius: 4px;
  background: var(--bsr-bg);
}

.cd-time {
  color: var(--bsr-accent);
}

/* ── 音乐平台卡片（阶段 5）────────────────────────────────────────────── */
.platform-row {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(200px, 1fr));
  gap: 12px;
  margin-bottom: 8px;
}

.platform {
  padding: 10px;
  border: 1px solid var(--bsr-border);
  border-radius: 8px;
  background: var(--bsr-bg);
}

.platform-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-bottom: 8px;
}

.platform-name {
  font-size: 13px;
}

.platform-badge {
  font-size: 11px;
  padding: 1px 8px;
  border-radius: 999px;
}

.platform-badge.ok {
  background: color-mix(in srgb, var(--bsr-success) 20%, transparent);
  color: var(--bsr-success);
}

.platform-badge.off {
  background: color-mix(in srgb, var(--bsr-muted) 20%, transparent);
  color: var(--bsr-muted);
}

.hint.warn {
  color: var(--bsr-danger);
}

.search-list {
  margin: 10px 0 0;
  padding: 0;
  list-style: none;
  display: flex;
  flex-direction: column;
  gap: 5px;
  max-height: 260px;
  overflow: auto;
}

.search-list li {
  display: grid;
  grid-template-columns: 1fr auto;
  grid-template-areas: 'title add' 'meta add';
  gap: 2px 10px;
  align-items: center;
  padding: 6px 8px;
  border-radius: 6px;
  background: var(--bsr-bg);
  font-size: 13px;
}

.sl-title {
  grid-area: title;
}

.sl-artist {
  grid-area: meta;
  font-size: 11px;
  color: var(--bsr-accent);
}

.sl-meta {
  grid-area: meta;
  justify-self: end;
  font-size: 11px;
  color: var(--bsr-muted);
}

.sl-add {
  grid-area: add;
  padding: 4px 10px;
  border: 1px solid var(--bsr-border);
  border-radius: 6px;
  background: var(--bsr-bg-elevated);
  color: var(--bsr-fg);
  font-size: 12px;
  cursor: pointer;
}

/* 队列里的解析状态标签 */
.src-tag {
  display: inline-block;
  margin-left: 6px;
  padding: 0 6px;
  border-radius: 999px;
  font-size: 10px;
  vertical-align: middle;
}

.src-tag.pending {
  background: color-mix(in srgb, var(--bsr-accent) 20%, transparent);
  color: var(--bsr-accent);
}

.src-tag.failed {
  background: color-mix(in srgb, var(--bsr-danger) 20%, transparent);
  color: var(--bsr-danger);
}

/* 优先级标签：主播点歌会插到所有弹幕之前，用醒目颜色标出来 */
.prio-tag {
  display: inline-block;
  margin-left: 6px;
  padding: 0 6px;
  border-radius: 999px;
  font-size: 10px;
  vertical-align: middle;
}

.prio-tag.host {
  background: color-mix(in srgb, #fbbf24 24%, transparent);
  color: #fbbf24;
  font-weight: 600;
}

.queue-table td.dim {
  font-size: 12px;
  color: var(--bsr-muted);
}

.play-url {
  margin: 12px 0 0;
  font-size: 12px;
  color: var(--bsr-muted);
}

.play-url code {
  display: inline-block;
  max-width: 100%;
  padding: 4px 8px;
  border-radius: 6px;
  background: var(--bsr-bg);
  border: 1px solid var(--bsr-border);
  word-break: break-all;
  color: var(--bsr-fg);
}

.now-title {
  font-size: 17px;
  font-weight: 700;
}

.now-artist,
.now-meta {
  font-size: 12px;
  color: var(--bsr-muted);
}

.progress {
  height: 6px;
  margin: 10px 0;
  border-radius: 999px;
  background: var(--bsr-border);
  overflow: hidden;
}

.progress-fill {
  height: 100%;
  background: var(--bsr-accent);
  transition: width 0.4s linear;
}

/* ── 可点击 / 可拖动的进度条 ───────────────────────────────────────────── */

.progress-range {
  -webkit-appearance: none;
  appearance: none;
  width: 100%;
  /* 轨道本身很细，但把控件做高，扩大点击热区（细条很难点中） */
  height: 18px;
  margin: 8px 0 0;
  background: transparent;
  cursor: pointer;
}

.progress-range:disabled {
  cursor: default;
  opacity: 0.5;
}

/* 轨道用渐变画出「已播放」部分，百分比由 --seek-progress 传入 */
.progress-range::-webkit-slider-runnable-track {
  height: 6px;
  border-radius: 999px;
  background: linear-gradient(
    to right,
    var(--bsr-accent) 0%,
    var(--bsr-accent) var(--seek-progress, 0%),
    var(--bsr-border) var(--seek-progress, 0%),
    var(--bsr-border) 100%
  );
}

.progress-range::-webkit-slider-thumb {
  -webkit-appearance: none;
  appearance: none;
  width: 14px;
  height: 14px;
  margin-top: -4px; /* 让圆点居中于 6px 轨道 */
  border-radius: 50%;
  background: var(--bsr-fg);
  border: 2px solid var(--bsr-accent);
  transition: transform 0.12s ease;
}

.progress-range:hover::-webkit-slider-thumb,
.progress-range:focus::-webkit-slider-thumb {
  transform: scale(1.15);
}

.progress-times {
  display: flex;
  justify-content: space-between;
  font-size: 11px;
  color: var(--bsr-muted);
}

.controls {
  display: flex;
  gap: 8px;
  align-items: center;
  flex-wrap: wrap;
}

/* ── 连接进度卡片 ──────────────────────────────────────────────────────── */

.conn-status {
  margin-top: 12px;
  padding: 10px 12px;
  border-radius: 8px;
  border-left: 3px solid var(--bsr-muted);
  background: rgba(148, 163, 184, 0.08);
  font-size: 13px;
}

.conn-status.ok {
  border-left-color: #34d399;
  background: rgba(52, 211, 153, 0.1);
}

.conn-status.busy {
  border-left-color: #fbbf24;
  background: rgba(251, 191, 36, 0.1);
}

.conn-status.err {
  border-left-color: #f87171;
  background: rgba(248, 113, 113, 0.1);
}

.conn-head {
  display: flex;
  align-items: center;
  gap: 8px;
}

.conn-dot {
  width: 8px;
  height: 8px;
  border-radius: 50%;
  background: var(--bsr-muted);
  flex: none;
}

.conn-dot.ok {
  background: #34d399;
}

.conn-dot.busy {
  background: #fbbf24;
  animation: pulse 1.2s ease-in-out infinite;
}

.conn-dot.err {
  background: #f87171;
}

@keyframes pulse {
  0%,
  100% {
    opacity: 1;
    transform: scale(1);
  }
  50% {
    opacity: 0.45;
    transform: scale(0.8);
  }
}

.conn-attempts {
  margin-left: auto;
  font-size: 11px;
  color: var(--bsr-muted);
}

.conn-detail {
  margin: 6px 0 0;
  color: var(--bsr-fg);
  line-height: 1.5;
}

.conn-hint {
  margin: 4px 0 0;
  font-size: 12px;
  color: var(--bsr-muted);
  line-height: 1.5;
}

.conn-time {
  margin: 4px 0 0;
  font-size: 11px;
  color: var(--bsr-muted);
}

/* 按钮里的小转圈：明确「正在处理」 */
.spinner {
  display: inline-block;
  width: 12px;
  height: 12px;
  margin-right: 6px;
  vertical-align: -1px;
  border: 2px solid currentColor;
  border-top-color: transparent;
  border-radius: 50%;
  animation: spin 0.7s linear infinite;
}

@keyframes spin {
  to {
    transform: rotate(360deg);
  }
}

/* 没有在播歌曲时：控件保留但整体压暗，避免布局跳动 */
.now-idle .now-title {
  opacity: 0.72;
}

.now-idle .now-artist,
.now-idle .now-meta,
.now-idle .progress-times {
  opacity: 0.55;
}

.pager {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  flex-wrap: wrap;
  margin-top: 10px;
}

.pager .controls {
  margin-top: 0;
}

@media (prefers-reduced-motion: reduce) {
  .conn-dot.busy,
  .spinner {
    animation: none;
  }
}

.muted {
  color: var(--bsr-muted);
}

.volume {
  display: flex;
  align-items: center;
  gap: 10px;
  margin-top: 14px;
  font-size: 12px;
  color: var(--bsr-muted);
}

/*
 * 音量文本固定宽度。
 *
 * 用户报告「拖到 10 以下进度条会左右抽搐」——真因就在这里：
 * 「音量 8」「音量 42」「音量 100」三种文本宽度不同，而进度条紧跟其后，
 * 于是数字每变一次、进度条就被推挤一次。
 * 用等宽数字 + 固定宽度彻底消除位移。
 */
.vol-label {
  display: inline-block;
  width: 5.4em;
  font-variant-numeric: tabular-nums;
  white-space: nowrap;
}

.volume input[type='range'] {
  flex: 1;
  min-width: 0;
}

.danmaku {
  margin: 0;
  padding: 0;
  list-style: none;
  display: flex;
  flex-direction: column;
  gap: 5px;
  font-size: 13px;
  max-height: 240px;
  overflow: auto;
}

.dm-user {
  margin-right: 8px;
  color: var(--bsr-accent);
}

.dm-text {
  color: var(--bsr-fg);
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

/* 专注页地址列表 */
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

/* ── 点歌日志（阶段 8）─────────────────────────────────────────────────── */

.log-table td {
  vertical-align: top;
}

/* 结果标签：成功/失败一眼可辨 */
.outcome-tag {
  display: inline-block;
  padding: 1px 8px;
  border-radius: 999px;
  font-size: 11px;
  white-space: nowrap;
}

.outcome-tag.queued {
  background: color-mix(in srgb, #34d399 22%, transparent);
  color: #34d399;
}

.outcome-tag.rejected {
  background: color-mix(in srgb, var(--bsr-danger) 22%, transparent);
  color: var(--bsr-danger);
}

.log-table .err,
.log-table td .err {
  color: var(--bsr-danger);
}

/* ── 收藏歌单导入列表（阶段 10b）───────────────────────────────────────── */

.playlist-list {
  margin: 10px 0 0;
  padding: 0;
  list-style: none;
  display: flex;
  flex-direction: column;
  gap: 6px;
  /* 歌单可能有几十个，限高滚动避免把整页撑长 */
  max-height: 320px;
  overflow-y: auto;
}

.playlist-list li {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 10px;
  padding: 7px 10px;
  border: 1px solid var(--bsr-border);
  border-radius: 8px;
}

.playlist-list .pl-meta {
  display: flex;
  align-items: baseline;
  gap: 8px;
  flex-wrap: wrap;
  min-width: 0;
}

.playlist-list .pl-meta strong {
  font-size: 13px;
}

.playlist-list button {
  flex: 0 0 auto;
  padding: 5px 12px;
  border: 1px solid var(--bsr-accent);
  border-radius: 6px;
  background: transparent;
  color: var(--bsr-fg);
  font-size: 12px;
  cursor: pointer;
}

.playlist-list button:disabled {
  opacity: 0.5;
  cursor: default;
}

.link {
  font-size: 13px;
  color: var(--bsr-accent);
}

.queue-table {
  width: 100%;
  margin-top: 12px;
  border-collapse: collapse;
  font-size: 13px;
}

.queue-table th,
.queue-table td {
  padding: 8px 10px;
  border-bottom: 1px solid var(--bsr-border);
  text-align: left;
}

.queue-table th {
  color: var(--bsr-muted);
  font-weight: 500;
}

.row-actions {
  display: flex;
  gap: 6px;
}

.fields {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(210px, 1fr));
  gap: 10px 16px;
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
  gap: 8px;
}

.fields label.wide {
  grid-column: 1 / -1;
}

.fields input,
.fields select {
  padding: 6px 8px;
  border: 1px solid var(--bsr-border);
  border-radius: 6px;
  background: var(--bsr-bg);
  color: var(--bsr-fg);
  font-size: 13px;
}
</style>
