/**
 * 前端 LRC 解析（阶段 7）。
 *
 * 为什么前端也要一份：后端已经算好「当前行下标」并通过 `lyric_index` 下发，
 * 但面板还要**显示**歌词文本本身。让后端把整段解析结果也塞进每一帧状态里
 * 会明显增加 WS 流量（歌词几百行 × 每帧），因此：
 *  - 后端只下发 LRC 原文 + 当前行下标（已在用）；
 *  - 前端解析一次（computed 缓存），只在歌词变化时重算。
 *
 * 解析规则与后端 `player/lyrics.rs` 保持一致（同样忽略元信息行、
 * 支持一行多时间标签）。Rust 侧有完整单测；这里是等价的精简实现。
 */

export interface LrcLine {
  /** 起始时间（秒）。 */
  at: number
  text: string
}

/** 解析 `mm:ss.xx` / `hh:mm:ss.xx`；非法返回 null。 */
function parseTimestamp(inside: string): number | null {
  const parts = inside.split(':')
  if (parts.length !== 2 && parts.length !== 3) return null
  let seconds = 0
  for (let i = 0; i < parts.length; i += 1) {
    const value = Number(parts[i].trim())
    if (!Number.isFinite(value) || value < 0) return null
    const weight = parts.length - i === 1 ? 1 : parts.length - i === 2 ? 60 : 3600
    seconds += value * weight
  }
  return seconds
}

/** 剥离一行里的所有时间标签，返回时间列表与剩余文本。 */
function splitTimeTags(line: string): { times: number[]; text: string } {
  const times: number[] = []
  let cursor = 0

  while (cursor < line.length) {
    while (cursor < line.length && /\s/.test(line[cursor])) cursor += 1
    if (line[cursor] !== '[') break
    const close = line.indexOf(']', cursor)
    if (close < 0) break
    const seconds = parseTimestamp(line.slice(cursor + 1, close))
    if (seconds === null) {
      // 元信息标签（[ar:...]）→ 整行不产生歌词
      return { times: [], text: '' }
    }
    times.push(seconds)
    cursor = close + 1
  }

  return { times, text: line.slice(cursor) }
}

/** 解析 LRC 文本为按时间升序的行。 */
export function parseLrc(raw: string | null | undefined): LrcLine[] {
  if (!raw) return []
  const lines: LrcLine[] = []

  for (const rawLine of raw.split(/\r?\n/)) {
    const trimmed = rawLine.trim()
    if (!trimmed) continue
    const { times, text } = splitTimeTags(trimmed)
    if (!times.length) continue
    const value = text.trim()
    for (const at of times) lines.push({ at, text: value })
  }

  lines.sort((a, b) => a.at - b.at)

  // 去掉紧邻的重复行（同一时间同一文本）
  return lines.filter((line, index) => {
    const previous = lines[index - 1]
    if (!previous) return true
    return !(Math.abs(previous.at - line.at) < 0.001 && previous.text === line.text)
  })
}
