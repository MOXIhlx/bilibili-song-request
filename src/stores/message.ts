import { ref } from 'vue'

/**
 * 全局轻提示（message）队列。
 *
 * 替代 `window.alert()`：alert 会**阻塞整个界面**直到点确定，连接状态变化这类
 * 后台事件弹窗时会打断用户操作；message 非阻塞、自动消失、可叠加。
 *
 * 用独立 store 而不是组件内 ref：控制台与 OBS 面板页都要能弹提示。
 */
export type MessageTone = 'ok' | 'err' | 'info' | 'warn'

export interface MessageItem {
  id: number
  tone: MessageTone
  text: string
}

/** 当前展示中的提示（新的在上）。 */
export const messages = ref<MessageItem[]>([])

let nextId = 1

/** 提示停留时长（毫秒）。错误留久一点，方便看清。 */
const DURATION: Record<MessageTone, number> = {
  ok: 2600,
  info: 3000,
  warn: 4200,
  err: 6000,
}

/** 最大同时展示条数，超出丢弃最旧的。 */
const MAX_VISIBLE = 5

/** 折叠状态的持久化键（记住上次是展开还是收起）。 */
const COLLAPSED_KEY = 'bsr.messages.collapsed'

/**
 * 提示区是否收起到右下角工具栏。
 *
 * 收起后只留一个右下角的小按钮（带未读数量），点它重新展开；
 * 这样提示不会一直占着右上角，但也不会丢失。
 */
export const messagesCollapsed = ref(readCollapsed())

function readCollapsed(): boolean {
  try {
    return window.localStorage.getItem(COLLAPSED_KEY) === '1'
  } catch {
    // 隐私模式等场景下 localStorage 不可用，默认展开
    return false
  }
}

/** 收起提示区。 */
export function collapseMessages(): void {
  messagesCollapsed.value = true
  persistCollapsed(true)
}

/** 展开提示区。 */
export function expandMessages(): void {
  messagesCollapsed.value = false
  persistCollapsed(false)
}

/** 在收起/展开之间切换。 */
export function toggleMessages(): void {
  if (messagesCollapsed.value) expandMessages()
  else collapseMessages()
}

function persistCollapsed(value: boolean): void {
  try {
    window.localStorage.setItem(COLLAPSED_KEY, value ? '1' : '0')
  } catch {
    // 存不了就只影响下次启动，不影响本次体验
  }
}

/**
 * 弹出一条提示。
 *
 * `tone` 只影响配色：`ok` 成功、`err` 失败、`info` 中性（如「没有更多了」）、
 * `warn` 警示。
 *
 * ⚠️ 收起状态下**不自动展开**：否则用户特意收起后，一条后台提示又把面板顶出来。
 * 新提示仍会计入未读，右下角按钮上会显示数量。
 */
export function showMessage(tone: MessageTone, text: string): void {
  const id = nextId++
  messages.value = [{ id, tone, text }, ...messages.value].slice(0, MAX_VISIBLE)
  window.setTimeout(() => dismissMessage(id), DURATION[tone])
}

/** 手动关闭某条。 */
export function dismissMessage(id: number): void {
  messages.value = messages.value.filter((m) => m.id !== id)
}

/** `alert` 风格：成功。 */
export const messageOk = (text: string): void => showMessage('ok', text)
/** `alert` 风格：失败。 */
export const messageErr = (text: string): void => showMessage('err', text)
/** `alert` 风格：中性信息。 */
export const messageInfo = (text: string): void => showMessage('info', text)
