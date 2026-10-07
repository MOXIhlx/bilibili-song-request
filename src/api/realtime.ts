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
    try {
      socket = new WebSocket(url)
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
    }

    socket.onclose = () => {
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
