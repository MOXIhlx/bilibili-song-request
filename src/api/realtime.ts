/**
 * 与内嵌 axum 服务器之间的实时通道。
 *
 * 设计要点：
 *  - 自动重连，退避为 500ms * 2^n，上限 10s，带随机抖动避免多客户端同时重连。
 *  - 心跳：每 20s 发送 `{"type":"ping"}`，服务器回 `{"type":"pong"}`；
 *    45s 没收到任何消息就主动断开重连（OBS 浏览器源被切走时可能静默丢包）。
 *  - 页面隐藏（OBS 场景切换）时不主动断开，只暂停心跳，避免恢复时重连风暴。
 */
import { parseWsMessage, wsUrl } from '@/api'
import type { WsMessage } from '@/types'

export interface RealtimeClientOptions {
  /** 收到消息时回调。 */
  onMessage: (msg: WsMessage) => void
  /** 连接状态变化回调，用于 UI 显示小圆点。 */
  onStatusChange?: (connected: boolean) => void
  /** 心跳间隔（毫秒）。 */
  heartbeatMs?: number
  /** 静默超时（毫秒）。 */
  idleTimeoutMs?: number
}

export interface RealtimeClient {
  /** 主动连接（幂等）。 */
  connect: () => void
  /** 关闭并停止重连。 */
  disconnect: () => void
  /** 当前是否已连接。 */
  isConnected: () => boolean
  /** 立即重连（例如用户点了「重试」）。 */
  reconnectNow: () => void
}

const MAX_BACKOFF_MS = 10_000

export function createRealtimeClient(options: RealtimeClientOptions): RealtimeClient {
  const heartbeatMs = options.heartbeatMs ?? 20_000
  const idleTimeoutMs = options.idleTimeoutMs ?? 45_000

  let socket: WebSocket | null = null
  let attempt = 0
  let closedByUser = false
  let reconnectTimer: number | undefined
  let heartbeatTimer: number | undefined
  let lastMessageAt = 0

  function clearTimers(): void {
    if (reconnectTimer !== undefined) window.clearTimeout(reconnectTimer)
    if (heartbeatTimer !== undefined) window.clearInterval(heartbeatTimer)
    reconnectTimer = undefined
    heartbeatTimer = undefined
  }

  function scheduleReconnect(): void {
    if (closedByUser) return
    const backoff = Math.min(500 * 2 ** attempt, MAX_BACKOFF_MS)
    const jitter = Math.random() * 250
    attempt += 1
    reconnectTimer = window.setTimeout(() => connect(), backoff + jitter)
  }

  function startHeartbeat(): void {
    if (heartbeatTimer !== undefined) window.clearInterval(heartbeatTimer)
    heartbeatTimer = window.setInterval(() => {
      if (!socket || socket.readyState !== WebSocket.OPEN) return
      if (Date.now() - lastMessageAt > idleTimeoutMs) {
        // 静默过久：认为链路已死，主动关闭触发重连。
        socket.close(4000, 'idle timeout')
        return
      }
      socket.send(JSON.stringify({ type: 'ping', at: new Date().toISOString() }))
    }, heartbeatMs)
  }

  function connect(): void {
    if (closedByUser) return
    if (socket && (socket.readyState === WebSocket.OPEN || socket.readyState === WebSocket.CONNECTING)) {
      return
    }
    clearTimers()

    const url = wsUrl('/ws')
    /*
     * 连接上带一个**来源标识**。
     *
     * ## 为什么要带
     * 排查「界面反复掉线」时，服务端日志只有一句 `WS 客户端已连接/已断开`，
     * 无法区分是**哪个页面**连的。而本项目有**三个**入口都会建这条连接：
     *
     * ```text
     * src/main.ts       Tauri 桌面窗口（index.html）
     * src/dashboard.ts  浏览器里打开的控制台（dashboard.html）
     * src/panel.ts      OBS 浏览器源 / 面板页（panel.html）
     * ```
     *
     * 它们各自独立连一次，所以服务端会看到**多条**连接；实测一个干净启动
     * 的实例就有两条。没有标识时根本分不清"是谁在反复断"。
     */
    const source =
      typeof document !== 'undefined' && document.body?.classList.contains('bsr-panel-body')
        ? 'panel'
        : 'window'
    /*
     * 再带上 User-Agent。
     *
     * `src` 只能区分"前端自己是谁"，区分不了"这是哪个程序里的页面"。
     * 实测用它定位到一个**不带 src、也不带 UA** 的连接（不是当前版本的
     * 前端代码）每 2 分 45 秒断一次，正是它让界面状态反复变红。
     * URL 编码后再拼，避免 UA 里的分号/空格破坏查询串。
     */
    const ua = typeof navigator !== 'undefined' ? navigator.userAgent : ''
    const tagged =
      `${url}${url.includes('?') ? '&' : '?'}src=${source}` +
      (ua ? `&ua=${encodeURIComponent(ua)}` : '')
    try {
      socket = new WebSocket(tagged)
    } catch {
      scheduleReconnect()
      return
    }

    socket.onopen = () => {
      attempt = 0
      lastMessageAt = Date.now()
      options.onStatusChange?.(true)
      startHeartbeat()
    }

    socket.onmessage = (ev) => {
      lastMessageAt = Date.now()
      if (typeof ev.data !== 'string') return
      const msg = parseWsMessage(ev.data)
      if (msg) options.onMessage(msg)
    }

    socket.onerror = () => {
      // 具体原因由 onclose 统一处理，这里不重复排程。
      // 但错误事件本身有信息量（尤其握手阶段），记下来便于排查。
      console.warn('[realtime] WebSocket error 事件')
    }

    /*
     * 断线时记录 close code / reason。
     *
     * ## 为什么要记
     * 曾经出现过「界面每几十秒断一次」的问题，但日志里只有服务端的
     * 「WS 客户端已断开」——**看不出是谁先关的**。WebSocket 的 close code
     * 正是用来区分这个的：
     *
     * ```text
     * 1000 正常关闭（我方主动 disconnect）
     * 1001 对端离开（服务端/中间层关的）
     * 1006 异常断开（没有 close 帧，通常是网络层或进程被杀）
     * 4000 我们自己的「idle timeout」（见 startHeartbeat）
     * ```
     *
     * 排查这类问题时，先看这个 code 能省掉大量猜测。
     */
    socket.onclose = (ev) => {
      console.warn(
        `[realtime] 连接关闭 code=${ev.code} reason=${ev.reason || '(空)'} wasClean=${ev.wasClean}`,
      )
      options.onStatusChange?.(false)
      socket = null
      scheduleReconnect()
    }
  }

  return {
    connect,
    disconnect() {
      closedByUser = true
      clearTimers()
      socket?.close(1000, 'client disconnect')
      socket = null
      options.onStatusChange?.(false)
    },
    isConnected: () => socket?.readyState === WebSocket.OPEN,
    reconnectNow() {
      closedByUser = false
      attempt = 0
      socket?.close(1000, 'manual reconnect')
      socket = null
      connect()
    },
  }
}
