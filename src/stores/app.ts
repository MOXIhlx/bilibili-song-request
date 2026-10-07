/**
 * 全局状态（Pinia）。
 *
 * 单一数据源策略：后端是唯一真源，前端 store 只是它的一份镜像。
 * 所有写操作都通过 HTTP API 提交，成功后立刻用返回的 AppState 覆盖本地，
 * 再由 WS 推送做最终一致（避免前端做乐观更新导致队列顺序错乱）。
 */
import { defineStore } from 'pinia'
import { computed, ref } from 'vue'
import * as api from '@/api'
import { ApiNotice } from '@/api'
import { createRealtimeClient, type RealtimeClient } from '@/api/realtime'
import type {
  AppState,
  Config,
  CooldownView,
  MusicPlatform,
  MusicStatus,
  PlayerEvent,
  QueueItem,
  RequestEvent,
  RequestRecord,
  Song,
  SongEvent,
  WsMessage,
} from '@/types'

/** 最近的弹幕记录（阶段 3 起由后端推送填充）。 */
export interface DanmakuLogEntry {
  user: string
  uid: string | null
  text: string
  at: string
}

export const useAppStore = defineStore('app', () => {
  const state = ref<AppState | null>(null)
  const config = ref<Config | null>(null)
  const connected = ref(false)
  const loading = ref(false)
  const error = ref<string | null>(null)
  const danmakuLog = ref<DanmakuLogEntry[]>([])
  /** 点歌请求的实时事件流（阶段 4），最新的在前。 */
  const requestLog = ref<RequestEvent[]>([])
  /** 曲目解析事件流（阶段 5），最新的在前。 */
  const songLog = ref<SongEvent[]>([])
  /** 播放事件流（阶段 6），最新的在前。 */
  const playerLog = ref<PlayerEvent[]>([])
  /** mpv 是否可用（未安装时为 false）。 */
  const playerAvailable = ref(false)
  /** 音乐平台状态（阶段 5）。 */
  const musicStatus = ref<MusicStatus | null>(null)
  /** 音乐搜索关键词与结果。 */
  const searchKeyword = ref('')
  const searchResults = ref<Song[]>([])
  const searching = ref(false)
  /** 最近一次取到的播放地址（排障用；实际播放由 mpv 完成）。 */
  const lastPlayUrl = ref<string | null>(null)
  /** B 站弹幕连接状态（后端状态机快照）。 */
  const bilibiliStatus = ref<api.BilibiliStatus | null>(null)
  /** 是否正在发起连接（用于按钮 loading）。 */
  const connecting = ref(false)

  let client: RealtimeClient | null = null

  const queue = computed<QueueItem[]>(() => state.value?.queue ?? [])
  const current = computed(() => state.value?.current ?? null)
  const player = computed(() => state.value?.player ?? null)
  const bilibili = computed(() => state.value?.bilibili ?? null)
  /** 最近的请求记录（由后端维护在 `AppState.stats.recent`）。 */
  const recentRequests = computed<RequestRecord[]>(() => state.value?.stats?.recent ?? [])
  /** 当前处于冷却中的用户。 */
  const cooldowns = computed<CooldownView[]>(() => state.value?.stats?.cooldowns ?? [])

  function applyState(next: AppState): void {
    state.value = next
  }

  function handleMessage(msg: WsMessage): void {
    switch (msg.type) {
      case 'state':
        applyState(msg.data)
        break
      case 'connection':
        if (state.value) state.value = { ...state.value, bilibili: msg.data }
        break
      case 'danmaku':
        danmakuLog.value = [msg.data, ...danmakuLog.value].slice(0, 200)
        break
      case 'request':
        requestLog.value = [msg.data, ...requestLog.value].slice(0, 100)
        break
      case 'song':
        songLog.value = [msg.data, ...songLog.value].slice(0, 100)
        break
      case 'player':
        playerLog.value = [msg.data, ...playerLog.value].slice(0, 100)
        break
      case 'hello':
      case 'pong':
        break
      case 'error':
        error.value = msg.data.message
        break
    }
  }

  /** 首次加载：并行拉取状态与配置，然后建立实时连接。 */
  async function bootstrap(): Promise<void> {
    loading.value = true
    error.value = null
    try {
      const [s, c, b] = await Promise.all([
        api.getState(),
        api.getConfig(),
        // 弹幕状态查询失败不应阻塞界面（例如后端降级模式）。
        api.getBilibiliStatus().catch(() => null),
      ])
      applyState(s)
      config.value = c
      bilibiliStatus.value = b
      // 音乐状态同样容错
      musicStatus.value = await api.getMusicStatus().catch(() => null)
    } catch (err) {
      error.value = (err as Error).message
    } finally {
      loading.value = false
    }

    if (!client) {
      client = createRealtimeClient({
        onMessage: handleMessage,
        onStatusChange: (ok) => {
          connected.value = ok
          // 实时通道重新连上后刷新一次弹幕连接状态，避免错过状态变化。
          if (ok) void refreshBilibiliStatus()
        },
      })
    }
    client.connect()
  }

  function teardown(): void {
    client?.disconnect()
    client = null
    connected.value = false
  }

  /**
   * 包一层：统一错误处理 + 用返回值刷新状态。
   *
   * 返回 `string` 表示后端给出了「没有内容可做」的**提示**（而非错误），
   * 由调用方决定怎么展示（例如弹一条信息提示）。
   */
  async function run(action: () => Promise<AppState>): Promise<string | void> {
    error.value = null
    try {
      applyState(await action())
    } catch (err) {
      if (err instanceof ApiNotice) return err.notice
      error.value = (err as Error).message
    }
  }

  async function refresh(): Promise<void> {
    await run(() => api.getState())
  }

  async function removeItem(id: string): Promise<void> {
    await run(() => api.queueAction(id, 'remove'))
  }

  async function topItem(id: string): Promise<void> {
    await run(() => api.queueAction(id, 'top'))
  }

  async function moveUp(id: string): Promise<void> {
    await run(() => api.queueAction(id, 'up'))
  }

  async function moveDown(id: string): Promise<void> {
    await run(() => api.queueAction(id, 'down'))
  }

  async function clearAll(): Promise<void> {
    await run(() => api.clearQueue())
  }

  /** 清空播放列表（点歌队列 + 播放序列），保留空闲歌单。 */
  async function clearPlaylist(): Promise<void> {
    await run(() => api.clearPlaylist())
  }

  /**
   * 一键清空点歌列表并开始播空闲歌单（阶段 10d）。
   *
   * 直播中"清场"用：清掉还没播的点歌，立刻接着放主播自己的空闲歌单。
   */
  async function clearAndPlayIdle(): Promise<void> {
    await run(() => api.clearQueueAndPlayIdle())
  }

  async function skip(): Promise<void> {
    await run(() => api.skipCurrent())
  }

  /** 回到播放序列里的上一首（真正换歌）。 */
  async function previous(): Promise<string | void> {
    return run(() => api.previousTrack())
  }

  /** 重头播放当前歌曲（进度归零）。 */
  async function replay(): Promise<void> {
    await run(() => api.replayCurrent())
  }

  /** 拖动进度条跳转。 */
  async function seek(position: number): Promise<void> {
    await run(() => api.seekTo(position))
  }

  async function togglePause(): Promise<void> {
    const paused = !(player.value?.paused ?? false)
    await run(() => api.setPaused(paused))
  }

  async function changeVolume(volume: number): Promise<void> {
    await run(() => api.setVolume(Math.round(volume)))
  }

  async function changePlayMode(mode: AppState['play_mode']): Promise<void> {
    await run(() => api.setPlayMode(mode))
  }

  async function updateConfig(next: Config): Promise<void> {
    error.value = null
    try {
      config.value = await api.saveConfig(next)
    } catch (err) {
      error.value = (err as Error).message
    }
  }

  /** 刷新 B 站弹幕连接状态。 */
  async function refreshBilibiliStatus(): Promise<void> {
    try {
      bilibiliStatus.value = await api.getBilibiliStatus()
    } catch {
      // 状态查询失败时保持上一次的值，由顶部的实时连接状态提示用户。
    }
  }

  /** 保存凭据并发起弹幕连接。 */
  async function connectBilibili(payload: api.ConnectPayload): Promise<boolean> {
    connecting.value = true
    error.value = null
    try {
      bilibiliStatus.value = await api.connectBilibili(payload)
      // 凭据已被后端落盘，同步本地配置，避免设置页显示旧值。
      config.value = await api.getConfig()
      return true
    } catch (err) {
      error.value = (err as Error).message
      await refreshBilibiliStatus()
      return false
    } finally {
      connecting.value = false
    }
  }

  /** 断开弹幕连接。 */
  async function disconnectBilibili(): Promise<void> {
    connecting.value = true
    error.value = null
    try {
      bilibiliStatus.value = await api.disconnectBilibili()
    } catch (err) {
      error.value = (err as Error).message
    } finally {
      connecting.value = false
    }
  }

  /**
   * 注入一条模拟弹幕（走真实解析与广播链路）。
   * 用于没有身份码 / 未开播时验证面板。
   */
  async function simulateDanmaku(text: string, user?: string): Promise<void> {
    error.value = null
    try {
      applyState(await api.simulateDanmaku(text, user))
    } catch (err) {
      error.value = (err as Error).message
    }
  }

  // ── 音乐平台（阶段 5）────────────────────────────────────────────────────

  /** 刷新音乐平台状态。 */
  async function refreshMusicStatus(): Promise<void> {
    try {
      musicStatus.value = await api.getMusicStatus()
    } catch {
      // 保持上一次的值，顶部实时连接状态会提示网络问题。
    }
  }

  /** 刷新播放器可用性（mpv 是否接线）。 */
  async function refreshPlayerStatus(): Promise<void> {
    try {
      playerAvailable.value = (await api.getPlayerStatus()).available
    } catch {
      playerAvailable.value = false
    }
  }

  /** 开始 / 继续播放队列。 */
  async function startPlayback(): Promise<void> {
    error.value = null
    try {
      applyState(await api.playerPlay())
    } catch (err) {
      error.value = (err as Error).message
    }
  }

  /** 打开内嵌登录窗口（桌面端）或提示用户。 */
  async function loginMusic(platform: 'netease' | 'qq'): Promise<void> {
    error.value = null
    try {
      await api.openMusicLogin(platform)
    } catch (err) {
      error.value = (err as Error).message
    }
  }

  /** 手动保存 Cookie（浏览器端 / 复制粘贴场景）。 */
  async function saveMusicCookie(platform: 'netease' | 'qq', cookie: string): Promise<boolean> {
    error.value = null
    try {
      await api.saveMusicCookie(platform, cookie)
      await refreshMusicStatus()
      return true
    } catch (err) {
      error.value = (err as Error).message
      return false
    }
  }

  /**
   * 退出某平台登录。
   *
   * 必须走**两步**：
   *  1. `clearMusicCookie`（REST）：清掉凭据库/文件里保存的 Cookie；
   *  2. `desktopClearMusicCookie`（Tauri 命令）：清掉 WebView 里该平台的 Cookie。
   *
   * 第 2 步不能省：登录窗口用的是持久化 WebView 配置目录，平台自己的会话
   * 也在里面。只清凭据库的话，下次点「登录」时窗口一打开就已是登录态，
   * 第一次轮询立刻抓到旧 Cookie → 表现为「退出后再登录，秒登录」。
   */
  async function clearMusicCookie(platform: 'netease' | 'qq'): Promise<void> {
    error.value = null
    try {
      musicStatus.value = await api.clearMusicCookie(platform)
    } catch (err) {
      error.value = (err as Error).message
      return
    }
    // 清 WebView Cookie：浏览器环境下没有该命令，忽略失败即可
    try {
      await api.desktopClearMusicCookie(platform as MusicPlatform)
    } catch {
      // 非桌面端（浏览器直接打开控制台）会失败，这里不影响凭据已清除
    }
  }

  /** 搜索歌曲。 */
  async function searchMusic(keyword?: string): Promise<void> {
    const query = (keyword ?? searchKeyword.value).trim()
    if (!query) {
      searchResults.value = []
      return
    }
    searchKeyword.value = query
    searching.value = true
    error.value = null
    try {
      const response = await api.searchMusic({ keyword: query, limit: 10 })
      searchResults.value = response.results
    } catch (err) {
      searchResults.value = []
      error.value = (err as Error).message
    } finally {
      searching.value = false
    }
  }

  /** 手动把搜索结果加进队列（已解析，跳过搜索）。 */
  async function addSongToQueue(song: Song, requestedBy = '手动添加'): Promise<void> {
    error.value = null
    try {
      applyState(await api.addResolvedSong(song, requestedBy))
    } catch (err) {
      error.value = (err as Error).message
    }
  }

  /** 取播放地址（排障用，阶段 6 由 mpv 直接消费）。 */
  async function fetchPlayUrl(songId: string, platform: 'netease' | 'qq'): Promise<void> {
    error.value = null
    try {
      const response = await api.getPlayUrl({ song_id: songId, platform })
      lastPlayUrl.value = response.url
    } catch (err) {
      lastPlayUrl.value = null
      error.value = (err as Error).message
    }
  }

  /** 手动按文本加歌：入队后由后端异步搜索真实曲目。 */
  async function addSongByText(title: string, artist?: string): Promise<void> {
    error.value = null
    try {
      applyState(await api.addSong(title, artist))
    } catch (err) {
      error.value = (err as Error).message
    }
  }

  return {
    state,
    config,
    connected,
    loading,
    error,
    danmakuLog,
    requestLog,
    songLog,
    playerLog,
    playerAvailable,
    queue,
    current,
    player,
    bilibili,
    recentRequests,
    cooldowns,
    bilibiliStatus,
    musicStatus,
    searchKeyword,
    searchResults,
    searching,
    lastPlayUrl,
    connecting,
    bootstrap,
    teardown,
    refresh,
    removeItem,
    topItem,
    moveUp,
    moveDown,
    clearAll,
    clearPlaylist,
    clearAndPlayIdle,
    skip,
    previous,
    replay,
    seek,    togglePause,
    changeVolume,
    changePlayMode,
    updateConfig,
    refreshBilibiliStatus,
    connectBilibili,
    disconnectBilibili,
    simulateDanmaku,
    refreshMusicStatus,
    refreshPlayerStatus,
    startPlayback,
    loginMusic,
    saveMusicCookie,
    clearMusicCookie,
    searchMusic,
    addSongToQueue,
    addSongByText,
    fetchPlayUrl,
  }
})
