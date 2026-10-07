/**
 * LRC 解析的单元测试（node --test）。
 *
 * 与后端 `src-tauri/src/player/lyrics.rs` 的测试同构：
 * 两份实现必须给出**一致**的结果，否则面板高亮位置会和后端算出的
 * `lyric_index` 对不上。这里的用例刻意与 Rust 侧保持相同样本。
 */
import { strict as assert } from 'node:assert'
import { test } from 'node:test'

import { parseLrc } from '../src/composables/lrc.ts'

const SAMPLE = `[ar:周杰伦]
[ti:晴天]
[00:00.000] 作词 : 周杰伦
[00:01.500] 作曲 : 周杰伦
[00:05.25]故事的小黄花
[00:10.00][00:20.00]从出生那年就飘着
[00:15.00]童年的荡秋千
`

test('解析歌词行并忽略元信息', () => {
  const lines = parseLrc(SAMPLE)
  assert.equal(lines.length, 6)
  assert.equal(lines[0].text, '作词 : 周杰伦')
  assert.equal(lines[0].at, 0)
  assert.equal(lines[2].at, 5.25)
})

test('按时间升序排列', () => {
  const times = parseLrc(SAMPLE).map((l) => l.at)
  const sorted = [...times].sort((a, b) => a - b)
  assert.deepEqual(times, sorted)
})

test('一行多时间标签会展开成多行', () => {
  const matches = parseLrc(SAMPLE).filter((l) => l.text === '从出生那年就飘着')
  assert.equal(matches.length, 2)
  assert.equal(matches[0].at, 10)
  assert.equal(matches[1].at, 20)
})

test('纯音乐 / 脏数据返回空', () => {
  assert.deepEqual(parseLrc(''), [])
  assert.deepEqual(parseLrc(null), [])
  assert.deepEqual(parseLrc('纯音乐，请欣赏'), [])
  assert.deepEqual(parseLrc('[ar:某人]\n[ti:歌名]'), [])
})

test('支持小时级时间戳', () => {
  const lines = parseLrc('[01:02:03.50]长音频')
  assert.equal(lines.length, 1)
  assert.equal(lines[0].at, 3600 + 120 + 3.5)
})

test('相邻重复行会被去重', () => {
  const lines = parseLrc('[00:10.00]同一句\n[00:10.00]同一句\n[00:11.00]下一句')
  assert.equal(lines.length, 2)
})

test('CRLF 换行也能解析', () => {
  const lines = parseLrc('[00:01.00]第一句\r\n[00:02.00]第二句\r\n')
  assert.equal(lines.length, 2)
  assert.equal(lines[1].text, '第二句')
})

test('保留占位空行以避免当前行乱跳', () => {
  const lines = parseLrc('[00:10.00]\n[00:20.00]有歌词')
  assert.equal(lines.length, 2)
  assert.equal(lines[0].text, '')
})

test('支持小时级时间戳的边界值', () => {
  assert.deepEqual(parseLrc('[00:00.00]零秒'), [{ at: 0, text: '零秒' }])
  assert.deepEqual(parseLrc('[99:59.99]接近上限'), [{ at: 5999.99, text: '接近上限' }])
})

test('歌词窗口计算：当前行前后各两行', async () => {
  // 与 Panel.vue 中 lyricWindow 的算法保持一致（前后各 2 行）
  const lines = parseLrc(
    ['[00:01.00]A', '[00:02.00]B', '[00:03.00]C', '[00:04.00]D', '[00:05.00]E', '[00:06.00]F']
      .join('\n'),
  )
  const windowAt = (position: number, context = 2) => {
    let current = -1
    for (let i = 0; i < lines.length; i += 1) {
      if (lines[i].at <= position) current = i
      else break
    }
    const start = Math.max(0, current - context)
    const end = Math.min(lines.length, current + context + 1)
    return lines.slice(start, end).map((l) => l.text)
  }

  assert.deepEqual(windowAt(0), ['A', 'B'], '还没开始时显示开头几行（第一行 1s 起）')
  assert.deepEqual(windowAt(3), ['A', 'B', 'C', 'D', 'E'], '中间时前后各两行')
  assert.deepEqual(windowAt(6), ['D', 'E', 'F'], '末尾时不越界')
})
