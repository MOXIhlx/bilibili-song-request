/**
 * 本地桩音乐服务：用**网易云的真实响应结构**回答两个接口，音频指向本地文件。
 *
 * 为什么需要它：网易云直链是短时效 CDN 地址且受版权/风控影响，无法在无人值守的
 * 验收里稳定复现。把 `BSR_NETEASE_API_BASE` 指向本服务后，就能完整验证：
 *
 *   点歌 → 曲目解析 → 取播放地址 → mpv 播放 → 进度推进 → 歌词滚动
 *   → 播完自动下一首 → 退出清理
 *
 * 用法：
 *   node scripts/mock-music-server.mjs [port] [audioDir]
 *
 * 覆盖的接口（路径与真实接口一致）：
 *   GET /api/search/get/web                → {"code":200,"result":{"songs":[...]}}
 *   GET /api/song/enhance/player/url        → {"code":200,"data":[{"url":"..."}]}
 *   GET /api/song/lyric                     → {"code":200,"lrc":{"lyric":"[..].."}}
 *   GET /audio/:name                        → 本地音频文件（支持 Range）
 */

import { createServer } from 'node:http'
import { readFile, stat } from 'node:fs/promises'
import { extname, join, normalize, sep } from 'node:path'

const port = Number(process.argv[2] ?? 18999)
const audioDir = normalize(process.argv[3] ?? 'C:/Windows/Media')

/** 候选音频（取目录里存在的前两个）。 */
const AUDIO_CANDIDATES = [
  'Windows Notify Calendar.wav',
  'Windows Background.wav',
  'Windows Foreground.wav',
  'Windows Notify System Generic.wav',
]

/** 桩里的「歌曲」，两首用于验证自动下一首。 */
const SONGS = [
  { id: '70001', name: '本地测试曲一', artist: '本地歌手', file: AUDIO_CANDIDATES[0] },
  { id: '70002', name: '本地测试曲二', artist: '本地歌手', file: AUDIO_CANDIDATES[1] },
]

/** 歌词（时间轴覆盖前 6 秒，方便观察高亮切换）。 */
const LYRICS = {
  '70001':
    '[00:00.00]第一句：开始唱了\n[00:01.50]第二句：跟着节奏\n[00:03.00]第三句：中间部分\n[00:04.50]第四句：快到结尾\n[00:05.50]第五句：马上结束',
  '70002':
    '[00:00.00]第二首开始\n[00:02.00]第二首第二句\n[00:04.00]第二首收尾',
}

const contentTypeOf = (name) =>
  extname(name).toLowerCase() === '.wav' ? 'audio/wav' : 'audio/mpeg'

function json(res, body) {
  const payload = Buffer.from(JSON.stringify(body), 'utf8')
  res.writeHead(200, {
    'content-type': 'application/json; charset=utf-8',
    'content-length': payload.length,
  })
  res.end(payload)
}

/** 网易云 web 搜索的响应结构（歌曲字段名与真实接口一致）。 */
function searchResponse(keyword) {
  const trimmed = String(keyword ?? '').trim()
  // 关键词能对上某一首时，把它排到最前——这样「取第一条」的解析策略
  // 会为不同点歌请求解析出不同曲目，便于验证队列与自动下一首。
  const ordered = [...SONGS].sort((a, b) => {
    const score = (song) => (trimmed.includes(song.name) ? 0 : 1)
    return score(a) - score(b)
  })
  return {
    code: 200,
    result: {
      songs: ordered.map((song) => ({
        id: Number(song.id),
        name: song.name,
        duration: 6000,
        artists: [{ id: 1, name: song.artist }],
        album: { id: 1, name: '本地测试专辑', picUrl: null },
      })),
      songCount: ordered.length,
    },
  }
}

/** 旧版取址接口的响应结构。 */
function playUrlResponse(songId) {
  const song = SONGS.find((s) => s.id === songId) ?? SONGS[0]
  return {
    code: 200,
    data: [
      {
        id: Number(song.id),
        url: `http://127.0.0.1:${port}/audio/${encodeURIComponent(song.file)}`,
        br: 320000,
        size: 0,
        fee: 0,
      },
    ],
  }
}

async function serveAudio(req, res, name) {
  const target = normalize(join(audioDir, name))
  // 防目录穿越：目标必须仍在 audioDir 内
  if (target !== audioDir && !target.startsWith(audioDir + sep)) {
    res.writeHead(403).end('forbidden')
    return
  }
  try {
    const info = await stat(target)
    const data = await readFile(target)
    const range = req.headers.range
    if (range) {
      const match = /bytes=(\d+)-(\d*)/.exec(range)
      const start = match ? Number(match[1]) : 0
      const end = match && match[2] ? Number(match[2]) : info.size - 1
      res.writeHead(206, {
        'content-type': contentTypeOf(name),
        'content-range': `bytes ${start}-${end}/${info.size}`,
        'content-length': end - start + 1,
        'accept-ranges': 'bytes',
      })
      res.end(data.subarray(start, end + 1))
      return
    }
    res.writeHead(200, {
      'content-type': contentTypeOf(name),
      'content-length': data.length,
      'accept-ranges': 'bytes',
    })
    res.end(data)
  } catch {
    res.writeHead(404).end('not found')
  }
}

const server = createServer(async (req, res) => {
  const url = new URL(req.url ?? '/', `http://127.0.0.1:${port}`)
  const path = url.pathname

  if (path === '/health') {
    json(res, { ok: true, songs: SONGS.length })
    return
  }

  if (path === '/api/search/get/web') {
    json(res, searchResponse(url.searchParams.get('s')))
    return
  }

  if (path === '/api/song/enhance/player/url') {
    const ids = (url.searchParams.get('ids') ?? '[]').replace(/[[\]"']/g, '')
    const first = ids.split(',')[0]?.trim() || SONGS[0].id
    json(res, playUrlResponse(first))
    return
  }

  if (path === '/api/song/lyric') {
    const id = url.searchParams.get('id') ?? SONGS[0].id
    json(res, {
      code: 200,
      lrc: { version: 1, lyric: LYRICS[id] ?? LYRICS[SONGS[0].id] },
      tlyric: { version: 1, lyric: '' },
    })
    return
  }

  if (path.startsWith('/audio/')) {
    await serveAudio(req, res, decodeURIComponent(path.slice('/audio/'.length)))
    return
  }

  res.writeHead(404).end('not found')
})

server.listen(port, '127.0.0.1', () => {
  console.log(`mock music server: http://127.0.0.1:${port}`)
  console.log(`audio dir: ${audioDir}`)
  for (const song of SONGS) console.log(`  ${song.id} ${song.name} -> ${song.file}`)
})
