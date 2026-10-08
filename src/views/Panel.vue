<script setup lang="ts">
/**
 * OBS 面板视图。
 *
 * 职责边界（重要）：面板只负责「显示」，永远不播放声音。
 * 音频由 mpv 直接输出到系统音频设备，OBS 通过「桌面音频」采集。
 * 这样可以避免 OBS 浏览器源在非活动场景被节流导致音乐中断。
 *
 * 阶段 7 增强：歌词滚动（按播放位置高亮当前行）、入队/换歌动画、
 * 播放中条目高亮、URL 参数热更新。
 */
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from 'vue'
import { getConfig } from '@/api'
import { useAppStore } from '@/stores/app'
import {
  styleToCssVars,
  styleToResolved,
  usePanelPage,
  usePanelStyle,
} from '@/composables/panelParams'
import { parseLrc } from '@/composables/lrc'
import type { PanelStyleConfig } from '@/types'

const store = useAppStore()

/**
 * 面板页自己的配置副本。
 *
 * ## ⚠️ 为什么不能只读 `store.config`
 * `store.bootstrap()` 只在 **控制台**（`App.vue` 的 `onMounted`）里跑。
 * 面板页是 OBS 浏览器源直接打开的独立页面，`store.config` **永远是 null**——
 * 于是样式里的背景图、主色、字号等对面板**完全不生效**，
 * 只有 URL 参数才管用（实测：配置里 `bg_image` 有值，面板却是
 * `--panel-bg-image: none`）。
 *
 * 这里自己拉一次配置：地址里的 `?style=<id>` 从这份列表里查，
 * 查不到则回落到 `default_style_id`。
 */
const configStyles = ref<import('@/types').PanelStyleConfig[] | null>(null)
const configDefaultId = ref<string | null>(null)

onMounted(async () => {
  // 控制台里已经有配置就直接用，省一次请求
  if (store.config?.panel_styles?.length) {
    configStyles.value = store.config.panel_styles
    configDefaultId.value = store.config.default_style_id
    return
  }
  try {
    const cfg = await getConfig()
    configStyles.value = cfg.panel_styles
    configDefaultId.value = cfg.default_style_id
  } catch (err) {
    // 取不到就用内置默认样式，绝不让面板空白
    console.warn('面板读取样式列表失败，使用内置默认样式：', err)
  }
})

/** 面板样式（内部订阅 URL 变化，改 OBS 地址即时生效）。 */
const baseStyle = usePanelStyle(
  () => configStyles.value ?? store.config?.panel_styles,
  () => configDefaultId.value ?? store.config?.default_style_id,
)

/**
 * 设置页实时推送过来的**草稿**样式覆盖。
 *
 * ## 为什么需要它
 * 设置页的预览是一个指向本页的 `<iframe>`。以前改颜色必须「保存样式 → 手动
 * 点刷新」才能看到效果，因为 iframe 里的样式是从**已保存的配置**解析出来的，
 * 草稿改了它并不知道。用户反馈：「每次我修改一下都不能马上预览，都要点击保存
 * 再手动刷新，我想要只要一改变右边就直接能看到效果」。
 *
 * 现在设置页把草稿通过 `postMessage` 推过来，这里直接覆盖解析结果——
 * 保存按钮只负责把方案落盘，与"能不能看见"解耦。
 */
const previewOverride = ref<PanelStyleConfig | null>(null)

/**
 * 实时预览的**存储键**。
 *
 * ## 为什么放弃 postMessage（实测走不通）
 * 设置页里的预览是一个 iframe：父页在 `tauri.localhost`、预览在
 * `127.0.0.1:17777`，**两个源**。WebView2 会把它们放进不同的渲染进程
 * （站点隔离）。实测结果：
 * ```
 * iframe.contentWindow   → 有
 * iframe.contentDocument → null      ← 跨源隔离
 * 父 → 子 postMessage     → 无任何回音
 * 子 → 父 postMessage     → 无任何回音（双向都不通）
 * ```
 * 所以 postMessage 方案在这个架构下**根本建立不起来**，不是接线问题。
 *
 * ## localStorage 为什么可行
 * 存储按 **origin** 隔离，同时**同源页面之间会自动广播 `storage` 事件**。
 * 设置页里主预览与"桥接页"都跑在 `127.0.0.1:17777`，彼此同源；
 * 桥接页负责把父页给的草稿写进 localStorage，`storage` 事件就广播给了
 * 包括主预览在内的所有同源页面。
 */
const PREVIEW_STYLE_KEY = 'bsr:preview-style'

/** 应用一份草稿样式覆盖（`null` = 恢复用地址里的样式）。 */
function applyPreviewOverride(next: PanelStyleConfig | null): void {
  previewOverride.value = next
}

/**
 * 监听设置页推来的草稿样式。
 *
 * 两条路径都保留：
 *  1. `postMessage`——万一将来父子同源（例如把窗口也挂到 17777 上），
 *     这条会立刻工作，不必改代码；
 *  2. `storage` 事件——**当前架构下真正生效的那条**。
 */
function onPreviewMessage(ev: MessageEvent): void {
  if (ev.origin !== window.location.origin) return
  const data = ev.data as { type?: string; style?: PanelStyleConfig } | null
  if (!data || data.type !== 'bsr:preview-style') return
  applyPreviewOverride(data.style ?? null)
}

/** 另一个同源页面改了草稿 → `storage` 事件送达本页。 */
function onPreviewStorage(ev: StorageEvent): void {
  if (ev.key !== PREVIEW_STYLE_KEY) return
  if (!ev.newValue) {
    applyPreviewOverride(null)
    return
  }
  try {
    applyPreviewOverride(JSON.parse(ev.newValue) as PanelStyleConfig)
  } catch {
    // 内容坏了就当没有覆盖，不影响正常显示
    applyPreviewOverride(null)
  }
}

onMounted(() => {
  window.addEventListener('message', onPreviewMessage)
  window.addEventListener('storage', onPreviewStorage)
  // 挂载时先读一次：设置页可能在本页加载完成前就写好了草稿
  try {
    const raw = window.localStorage.getItem(PREVIEW_STYLE_KEY)
    if (raw) applyPreviewOverride(JSON.parse(raw) as PanelStyleConfig)
  } catch {
    // 忽略：读不到就用地址里的样式
  }
  /*
   * 主动向父页报到。
   *
   * `targetOrigin` 用 `'*'`：本页不知道父页的确切来源（桌面窗口是
   * `tauri.localhost`，浏览器里可能是别的主机名）。这条消息不含任何数据，
   * 只是个类型标记，泄露面为零。
   */
  window.parent?.postMessage({ type: 'bsr:preview-hello' }, '*')
})
onUnmounted(() => {
  window.removeEventListener('message', onPreviewMessage)
  window.removeEventListener('storage', onPreviewStorage)
})

/** 实际生效的样式：草稿覆盖优先，否则用地址里的样式。 */
const style = computed(() => {
  const o = previewOverride.value
  return o ? styleToResolved(o, baseStyle.value.scale) : baseStyle.value
})

/**
 * 当前专注页（阶段 8）。
 *
 * 综合面板在 OBS 里常常放不下、字号又太小，所以拆出几个专用地址：
 * `/panel/play`（进度条+队列）、`/panel/lyrics`（歌词）、
 * `/panel/danmaku`（最近弹幕）。`all` 是原有的综合面板，行为完全不变。
 */
const page = usePanelPage()

/** 该专注页是否要显示某个区块。 */
function shows(section: 'nowPlaying' | 'queue' | 'lyrics' | 'danmaku'): boolean {
  switch (page.value) {
    case 'play':
      return section === 'nowPlaying' || section === 'queue'
    case 'lyrics':
      return section === 'lyrics'
    case 'danmaku':
      return section === 'danmaku'
    default:
      // 综合版：原有逻辑（弹幕不在综合面板里显示）
      return section !== 'danmaku'
  }
}

const cssVars = computed(() => styleToCssVars(style.value))

/** 弹幕专注页展示条数（复用面板的 limit）。 */
const danmakuLimit = computed(() => {
  const limit = style.value.limit
  return limit > 0 ? Math.max(limit * 2, 12) : 100
})

/** 队列展示条数：limit=0 表示全部。 */
const visibleQueue = computed(() => {
  const list = store.queue
  const limit = style.value.limit
  return limit > 0 ? list.slice(0, limit) : list
})

/** 当前播放条目 id，用于在队列里高亮。 */
const currentId = computed(() => store.current?.id ?? null)

const progressPercent = computed(() => {
  const p = store.player
  if (!p || !p.duration) return 0
  return Math.min(100, Math.max(0, (p.position / p.duration) * 100))
})

/**
 * 解析后的歌词行。
 *
 * 只在**有当前歌曲**时解析：歌曲结束后后端清空 current，但状态推送可能
 * 恰好落在「歌词已下发、当前歌曲已清空」的瞬间，导致面板出现
 * 「没有歌曲却还显示着上一首歌词」的残留（截图排查时踩到过）。
 */
const lyricLines = computed(() =>
  store.current ? parseLrc(store.player?.lyrics ?? null) : [],
)

/**
 * 当前歌词行下标：后端已算好 `lyric_index`（阶段 7）就直接用，
 * 缺失时前端按位置兜底计算一次，保证面板独立可用。
 */
const lyricIndex = computed(() => {
  const fromBackend = store.player?.lyric_index
  if (typeof fromBackend === 'number') return fromBackend
  const position = store.player?.position ?? 0
  const lines = lyricLines.value
  let found: number | null = null
  for (let i = 0; i < lines.length; i += 1) {
    if (lines[i].at <= position) found = i
    else break
  }
  return found
})

/** 歌词窗口（当前行 ±2 行）。 */
const lyricWindow = computed(() => {
  const lines = lyricLines.value
  if (!lines.length) return [] as { at: number; text: string; index: number }[]
  const center = lyricIndex.value ?? 0
  const start = Math.max(0, center - 2)
  const end = Math.min(lines.length, center + 3)
  return lines.slice(start, end).map((line, offset) => ({ ...line, index: start + offset }))
})

/** 是否存在真正的歌词文本（纯音乐时为 false）。 */
const hasLyrics = computed(() => lyricLines.value.some((l) => l.text.trim().length > 0))

/** 是否显示歌词区。 */
const showLyricsBlock = computed(() => style.value.showLyrics && style.value.layout !== 'compact')

/** 歌词容器：当前行变化时把它滚到中间。 */
const lyricBox = ref<HTMLElement | null>(null)

watch(lyricIndex, async () => {
  if (!showLyricsBlock.value) return
  await nextTick()
  const box = lyricBox.value
  if (!box) return
  const active = box.querySelector<HTMLElement>('.lyric-line.active')
  if (!active) return
  // 手算 scrollTop，避免 scrollIntoView 连带滚动整个页面
  const target = active.offsetTop - box.clientHeight / 2 + active.clientHeight / 2
  box.scrollTo({ top: Math.max(0, target), behavior: 'smooth' })
})

function formatDuration(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds <= 0) return '--:--'
  const m = Math.floor(seconds / 60)
  const s = Math.floor(seconds % 60)
  return `${m}:${s.toString().padStart(2, '0')}`
}
</script>

<template>
  <div
    class="obs-panel"
    :data-theme="style.theme"
    :data-layout="style.layout"
    :data-page="page"
    :data-transparent="style.transparent"
    :style="cssVars"
  >
    <div class="panel-flow">
      <section v-if="shows('nowPlaying')" class="now-playing">
        <div class="np-label">正在播放</div>
        <Transition name="np-swap" mode="out-in">
          <div v-if="store.current" :key="store.current.id" class="np-main">
            <div class="np-title">{{ store.current.song.title }}</div>
            <div class="np-artist">
              {{ store.current.song.artist || '未知歌手' }}
              <span class="np-by">· 点歌人 {{ store.current.requested_by }}</span>
            </div>
            <div class="np-progress">
              <div class="np-progress-fill" :style="{ width: `${progressPercent}%` }" />
            </div>
            <div class="np-times">
              <span>{{ formatDuration(store.player?.position ?? 0) }}</span>
              <span>{{ formatDuration(store.player?.duration ?? 0) }}</span>
            </div>
          </div>
          <div v-else key="empty" class="np-empty">暂时没有歌曲，发送「点歌 歌名 歌手」即可点歌</div>
        </Transition>
      </section>

      <section v-if="shows('queue')" class="queue">
        <div class="queue-head">
          <span>点歌队列</span>
          <span class="queue-count">{{ store.queue.length }} 首</span>
        </div>
        <TransitionGroup v-if="visibleQueue.length" name="queue" tag="ol" class="queue-list">
          <li
            v-for="(item, index) in visibleQueue"
            :key="item.id"
            class="queue-item"
            :class="{ current: item.id === currentId, pending: item.song.source === 'pending' }"
          >
            <span class="qi-index">{{ item.id === currentId ? '▶' : index + 1 }}</span>
            <span class="qi-title">{{ item.song.title }}</span>
            <span class="qi-artist">{{ item.song.artist }}</span>
            <span class="qi-user">{{ item.requested_by }}</span>
          </li>
        </TransitionGroup>
        <div v-else class="queue-empty">队列为空</div>
      </section>
    </div>

    <section v-if="shows('lyrics') && showLyricsBlock && store.current" class="lyrics">
      <div ref="lyricBox" class="lyrics-box">
        <p v-if="!hasLyrics" class="lyrics-empty">（暂无歌词）</p>
        <template v-else>
          <p
            v-for="line in lyricWindow"
            :key="`${line.index}-${line.at}`"
            class="lyric-line"
            :class="{
              active: line.index === lyricIndex,
              passed: lyricIndex !== null && line.index < lyricIndex,
            }"
          >
            {{ line.text || '♪' }}
          </p>
        </template>
      </div>
    </section>

    <!-- 专注页：最近弹幕（/panel/danmaku） -->
    <section v-if="page === 'danmaku'" class="danmaku-panel">
      <div class="queue-head">
        <span>最近弹幕</span>
        <span class="queue-count">{{ store.danmakuLog.length }} 条</span>
      </div>
      <TransitionGroup v-if="store.danmakuLog.length" name="queue" tag="ul" class="danmaku-list">
        <li v-for="(d, i) in store.danmakuLog.slice(0, danmakuLimit)" :key="`${i}-${d.at}`">
          <span class="dm-user">{{ d.user }}</span>
          <span class="dm-text">{{ d.text }}</span>
        </li>
      </TransitionGroup>
      <div v-else class="queue-empty">还没有弹幕</div>
    </section>
  </div>
</template>

<style scoped>
/*
 * 透明背景说明：OBS 浏览器源必须勾选「透明背景」，
 * 同时这里不能给根节点设置任何不透明底色（bg=transparent 时）。
 *
 * 默认配色与桌面窗口一致（粉白少女风）：强调色为正粉，文字用带紫调的深棕，
 * 这样在浅色背景图上也有足够对比度。这些值都可以被面板样式配置覆盖。
 */
.obs-panel {
  --panel-color: #ff6fa5;
  /* 进度条填充色。空 = 跟随主色（由 styleToCssVars 解析后写入） */
  --panel-bar: #ff6fa5;
  /* 歌名颜色。空 = 跟随主色 */
  --panel-title: #ff6fa5;
  --panel-fg: #5a4450;
  --panel-sub: rgba(90, 68, 80, 0.62);
  --panel-bg: transparent;
  /* 卡片/列表底色与进度条轨道：默认全透明，只要文字与进度条颜色 */
  --panel-surface: transparent;
  --panel-track: transparent;
  --panel-font-size: 16px;
  --panel-scale: 1;
  /* 三档字重：正文 / 次级说明 / 歌名 */
  --panel-fw: 600;
  --panel-fw-sub: 400;
  --panel-fw-title: 700;
  /*
   * 文字描边。宽度 0 = 不描边（默认），此时这些声明等价于没有。
   *
   * 面板会叠在任意背景图/直播画面上，浅色字压在浅色区域会糊掉，
   * 描边是最省事的可读性保障。
   * `-webkit-text-stroke` 在 WebView2/Chromium 上可用；`paint-order: stroke fill`
   * 让描边画在文字**后面**，否则粗描边会把笔画吃掉一半。
   */
  --panel-stroke-w: 0px;
  --panel-stroke: transparent;
  /* 背景图（可选，由 bgImage 参数给出 URL） */
  --panel-bg-image: none;

  box-sizing: border-box;
  width: 100%;
  min-height: 100vh;
  padding: 14px;
  font-family: 'Microsoft YaHei', 'PingFang SC', system-ui, sans-serif;
  font-size: var(--panel-font-size);
  color: var(--panel-fg);
  background-color: var(--panel-bg);
  background-image: var(--panel-bg-image);
  background-size: cover;
  background-position: center;
  background-repeat: no-repeat;
  transform-origin: top left;
  zoom: var(--panel-scale);
}

/*
 * 字重与描边：**统一在根节点上给默认值**。
 *
 * 这样面板里所有文字都自动带上这两项，不必在十几个选择器里各写一遍；
 * 需要区分的（歌名、次级小字）在下面单独覆盖即可。
 *
 * `paint-order: stroke fill` 很关键：默认描边是画在**填充之后**，
 * 粗描边会把笔画吃掉一半，字看起来又细又脏；改成先描边后填充就正常了。
 */
.obs-panel,
.obs-panel * {
  font-weight: var(--panel-fw);
  -webkit-text-stroke: var(--panel-stroke-w) var(--panel-stroke);
  paint-order: stroke fill;
}

/* 歌名：单独的颜色与字重 */
.obs-panel .np-title,
.obs-panel .qi-title {
  font-weight: var(--panel-fw-title);
}

/* 次级说明（「点歌人」「队列为空」「正在播放」标签、时间等）用细一档 */
.obs-panel .np-label,
.obs-panel .np-artist,
.obs-panel .np-by,
.obs-panel .np-time,
.obs-panel .queue-empty,
.obs-panel .np-lyric,
.obs-panel .dm-user,
.obs-panel .dm-text {
  font-weight: var(--panel-fw-sub);
}

/*
 * 「透明背景」只清掉**底色**，不要动背景图。
 *
 * ⚠️ 这里曾经写 `background: transparent`——那是简写属性，会把
 * `background-image` 一并重置成 none。结果是「上传了背景图、地址里也带了
 * bgImage 参数，画面却看不到图」，因为透明模式恰好是默认值。
 */
.obs-panel[data-transparent='true'] {
  background-color: transparent;
}

/*
 * 卡片底色（阶段 9 新增 `--panel-surface`）。
 *
 * 之前这里写死了 `color-mix(fg 8%, transparent)`，于是即使 OBS 勾了
 * 「透明背景」，画面上仍然叠着一层灰底——用户明确说不需要，
 * 只要文字 + 进度条颜色。
 *
 * 现在默认 `transparent`（真正只剩文字与进度条）；
 * 想要卡片感的话，把「主题色/卡片底色」参数设成 `#00000033` 这类值即可。
 */
.now-playing,
.queue {
  border-radius: 10px;
  padding: 12px 14px;
  background: var(--panel-surface, transparent);
}

.now-playing {
  border-left: 4px solid var(--panel-color);
  margin-bottom: 12px;
}

.np-label {
  font-size: 0.72em;
  letter-spacing: 0.14em;
  color: var(--panel-sub);
}

.np-title {
  margin-top: 4px;
  font-size: 1.35em;
  /* 歌名有独立的颜色变量：`--panel-title` 没单独设时由 JS 解析成主色 */
  color: var(--panel-title, var(--panel-color));
}

.np-artist {
  margin-top: 2px;
  font-size: 0.9em;
  color: var(--panel-sub);
}

.np-by {
  margin-left: 4px;
}

.np-progress {
  margin-top: 10px;
  height: 5px;
  border-radius: 999px;
  /*
   * 进度条轨道。用户要求「只要进度条的颜色」，所以轨道默认**完全透明**，
   * 由 `--panel-track` 控制；想要可见的底槽就把它设成 #ffffff33 之类。
   */
  background: var(--panel-track, transparent);
  overflow: hidden;
}

.np-progress-fill {
  height: 100%;
  /* 进度条填充色：`--panel-bar` 由 barColor 决定，为空时已在 JS 里解析成主色 */
  background: var(--panel-bar, var(--panel-color));
  /* 0.5s 线性过渡：进度每秒推进一次也不会跳格 */
  transition: width 0.5s linear;
}

.np-times {
  display: flex;
  justify-content: space-between;
  margin-top: 4px;
  font-size: 0.75em;
  color: var(--panel-sub);
}

.np-empty {
  margin-top: 6px;
  font-size: 0.95em;
  color: var(--panel-sub);
}

/*
 * 歌词卡片。
 *
 * ⚠️ 这里曾经漏改：`.now-playing` / `.queue` 都换成了 `--panel-surface`，
 * 唯独 `.lyrics` 还留着一层写死的 `color-mix(fg 8%, transparent)`，
 * 于是「卡片底色设为 transparent」时**歌词面板仍有一块灰底**。
 * 现在三处统一受 `--panel-surface` 控制。
 */
.lyrics {
  margin-bottom: 12px;
  padding: 10px 14px;
  border-radius: 10px;
  background: var(--panel-surface, transparent);
}

/* 只显示约 5 行，靠滚动把当前行带到中间 */
.lyrics-box {
  max-height: 32vh;
  overflow: hidden;
  scroll-behavior: smooth;
}

.lyric-line {
  margin: 0;
  padding: 2px 0;
  line-height: 1.6;
  font-size: 0.92em;
  text-align: center;
  color: var(--panel-sub);
  opacity: 0.72;
  transform: scale(0.98);
  transition:
    color 0.25s ease,
    transform 0.25s ease,
    opacity 0.25s ease;
}

/* 已经唱过的行进一步淡出，形成视觉层次 */
.lyric-line.passed {
  opacity: 0.45;
}

/* 正在唱的那一行：放大 + 主色 + 微微发光（字重跟歌名同档） */
.lyric-line.active {
  color: var(--panel-color);
  font-size: 1.08em;
  font-weight: var(--panel-fw-title);
  opacity: 1;
  transform: scale(1);
  text-shadow: 0 1px 6px color-mix(in srgb, var(--panel-color) 45%, transparent);
}

.lyrics-empty {
  margin: 0;
  text-align: center;
  font-size: 0.9em;
  color: var(--panel-sub);
}

/* ── 专注页（阶段 8）───────────────────────────────────────────────────────
 *
 * 拆页的初衷：综合面板在 OBS 里放不下，字号只能调得很小。
 * 每个专注页只渲染它需要的内容，因此可以把字放大。
 * 所有规则都限定在 [data-page=...] 下，**不影响原有布局样式**。
 */

/* 弹幕页：最近弹幕 */
.danmaku-panel {
  margin-top: 4px;
}

.danmaku-list {
  margin: 0;
  padding: 0;
  list-style: none;
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.danmaku-list li {
  line-height: 1.5;
  /* 长弹幕换行而不是被截断 */
  overflow-wrap: anywhere;
}

.danmaku-list .dm-user {
  margin-right: 10px;
  color: var(--panel-color);
  font-weight: var(--panel-fw-title);
}

.danmaku-list .dm-text {
  color: var(--panel-fg);
}

/* 弹幕页字号放大：内容少，可以放心做大 */
.obs-panel[data-page='danmaku'] .danmaku-list {
  font-size: 1.5em;
}

.obs-panel[data-page='danmaku'] .queue-head {
  font-size: 1.15em;
}

/* 歌词页字号放大（专注页只有歌词，整屏给它） */
.obs-panel[data-page='lyrics'] .lyrics-box {
  font-size: 1.55em;
  line-height: 2;
}

.obs-panel[data-page='lyrics'] .lyric-line {
  padding: 0.22em 0;
}

/* 播放页：进度条与队列放大，正在播放的标题更醒目 */
.obs-panel[data-page='play'] .np-title {
  font-size: 1.5em;
}

.obs-panel[data-page='play'] .np-artist,
.obs-panel[data-page='play'] .np-by {
  font-size: 1.1em;
}

.obs-panel[data-page='play'] .np-progress {
  height: 10px;
}

.obs-panel[data-page='play'] .np-times {
  font-size: 1em;
}

.obs-panel[data-page='play'] .queue-list {
  font-size: 1.25em;
}

.obs-panel[data-page='play'] .queue-item {
  padding: 0.32em 0;
}

.obs-panel[data-page='play'] .queue-head {
  font-size: 1.15em;
}

.queue-head {
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  font-size: 0.8em;
  letter-spacing: 0.1em;
  color: var(--panel-sub);
}

.queue-count {
  letter-spacing: 0;
}

.queue-list {
  margin: 8px 0 0;
  padding: 0;
  list-style: none;
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.queue-item {
  display: grid;
  grid-template-columns: 1.6em 1fr auto auto;
  gap: 8px;
  align-items: baseline;
  padding: 6px 8px;
  border-radius: 6px;
  /* 列表项底色同样受 `--panel-surface` 控制（默认透明，只要文字） */
  background: transparent;
  transition: background 0.3s ease;
}

/* 正在播放的条目：左侧色条 + 可选底色，一眼看出播到哪了 */
.queue-item.current {
  background: var(--panel-surface, transparent);
  box-shadow: inset 3px 0 0 var(--panel-color);
}

/* 还在解析曲目的条目：淡化提示「还没准备好」 */
.queue-item.pending .qi-title {
  opacity: 0.7;
}

.qi-index {
  color: var(--panel-color);
  font-weight: var(--panel-fw-title);
}

.qi-title {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.qi-artist,
.qi-user {
  font-size: 0.82em;
  color: var(--panel-sub);
  white-space: nowrap;
}

/* compact 布局：单行、隐藏歌手与点歌人，适合小尺寸叠加 */
.obs-panel[data-layout='compact'] .queue-item {
  grid-template-columns: 1.6em 1fr;
  padding: 3px 6px;
}

.obs-panel[data-layout='compact'] .qi-artist,
.obs-panel[data-layout='compact'] .qi-user {
  display: none;
}

.obs-panel[data-layout='compact'] .np-title {
  font-size: 1.15em;
}

.obs-panel[data-layout='compact'] .lyrics-box {
  max-height: 18vh;
}

.queue-empty {
  margin-top: 8px;
  font-size: 0.85em;
  color: var(--panel-sub);
}

/* ── 动画 ─────────────────────────────────────────────────────────────── */

/* 队列项进入 / 离开 / 重排 */
.queue-enter-active {
  transition:
    opacity 0.3s ease,
    transform 0.3s ease;
}

.queue-leave-active {
  transition:
    opacity 0.25s ease,
    transform 0.25s ease;
  position: absolute;
}

.queue-enter-from {
  opacity: 0;
  transform: translateY(-8px);
}

.queue-leave-to {
  opacity: 0;
  transform: translateX(12px);
}

.queue-move {
  transition: transform 0.3s ease;
}

/* 换歌时当前歌曲区块淡入淡出 */
.np-swap-enter-active,
.np-swap-leave-active {
  transition:
    opacity 0.3s ease,
    transform 0.3s ease;
}

.np-swap-enter-from {
  opacity: 0;
  transform: translateY(6px);
}

.np-swap-leave-to {
  opacity: 0;
  transform: translateY(-6px);
}

/* 尊重系统「减少动态效果」：动画全部关闭，避免晕动症用户不适 */
@media (prefers-reduced-motion: reduce) {
  .queue-enter-active,
  .queue-leave-active,
  .queue-move,
  .np-swap-enter-active,
  .np-swap-leave-active,
  .lyric-line,
  .np-progress-fill {
    transition: none;
  }
}

/* ═══════════════════════════════════════════════════════════════════════════
 * 宽版布局 layout=wide（阶段 7 新增）
 *
 * 设计目标（主播要求）：
 *   - 背景完全透明，只有文字，没有卡片底色；
 *   - 左侧：正在播放 + 点歌队列（纯文字）；
 *   - 右侧：歌词。
 *
 * 实现方式：`.panel-flow` 默认 `display: contents`（不产生盒子），
 * 因此在 list / compact / lyrics 布局下 DOM 结构与**视觉完全不变**；
 * 只有 wide 才把 `.panel-flow`（左栏）与 `.lyrics`（右栏）绝对定位成两列。
 *
 * 所有规则都限定在 `[data-layout='wide']` 下，不影响原有三种布局。
 * ═══════════════════════════════════════════════════════════════════════════ */

/* 默认不产生盒子：保证旧布局的排版与 DOM 展开结果一致 */
.panel-flow {
  display: contents;
}

/* ── 窄窗口回退：不足 700px 时退回纵向堆叠，避免文字挤在一起 ────────────── */
@media (max-width: 699px) {
  .obs-panel[data-layout='wide'] {
    display: flex;
    flex-direction: column;
    gap: 12px;
  }

  .obs-panel[data-layout='wide'] .panel-flow {
    display: flex;
    flex-direction: column;
    gap: 12px;
  }
}

@media (min-width: 700px) {
  /* 宽版：两列绝对定位，互不挤压 */
  .obs-panel[data-layout='wide'] {
    position: relative;
    min-height: 100vh;
  }

  /* 左侧：歌曲 + 队列，纯文字 */
  .obs-panel[data-layout='wide'] .panel-flow {
    display: block;
    position: absolute;
    top: 14px;
    left: 14px;
    width: 46%;
    max-width: 430px;
  }

  /* 右侧：歌词，贴着右边 */
  .obs-panel[data-layout='wide'] .lyrics {
    position: absolute;
    top: 14px;
    right: 14px;
    width: 46%;
    max-width: 430px;
    margin: 0;
    padding: 0;
    background: transparent;
    backdrop-filter: none;
    border-radius: 0;
  }

  /* 背景透明：去掉所有卡片底色与描边，只留文字 */
  .obs-panel[data-layout='wide'] .now-playing,
  .obs-panel[data-layout='wide'] .queue {
    background: transparent;
    backdrop-filter: none;
    border-radius: 0;
    padding: 0;
    margin: 0;
  }

  .obs-panel[data-layout='wide'] .queue {
    margin-top: 16px;
  }

  /* 左侧不再需要「正在播放」的左侧色条（纯文字更干净） */
  .obs-panel[data-layout='wide'] .now-playing {
    border-left: none;
  }

  /* 正文加一层淡淡的描边阴影：透明背景叠在亮色画面上也能看清 */
  .obs-panel[data-layout='wide'] {
    text-shadow: 0 1px 3px rgba(0, 0, 0, 0.85);
  }

  /*
   * 浅色主题取消描边：它是「白字 + 白描边」，
   * 在浅背景上只会糊成一团（实测就是这个效果），
   * 强行加阴影反而更差，因此交给宿主背景保证对比度。
   */
  .obs-panel[data-layout='wide'][data-theme='light'] {
    text-shadow: none;
  }

  /* 宽版里的次要列提高一点对比度：纯文字排版下太淡会看不清 */
  .obs-panel[data-layout='wide'] .qi-artist,
  .obs-panel[data-layout='wide'] .qi-user {
    color: var(--panel-fg);
    opacity: 0.72;
  }

  .obs-panel[data-layout='wide'] .queue-head,
  .obs-panel[data-layout='wide'] .np-label {
    opacity: 0.8;
  }

  /* 队列条目在宽版下是纯文字行，去掉底色与高亮块，
     改用「▶ + 主色文字」表示正在播放 */
  .obs-panel[data-layout='wide'] .queue-item {
    background: transparent;
    box-shadow: none;
    padding: 2px 0;
    border-radius: 0;
    grid-template-columns: 1.4em minmax(0, 1fr) minmax(0, 0.8fr) minmax(0, 0.7fr);
    gap: 6px;
  }

  .obs-panel[data-layout='wide'] .queue-item.current {
    background: transparent;
    box-shadow: none;
  }

  .obs-panel[data-layout='wide'] .queue-item.current .qi-title {
    color: var(--panel-color);
    font-weight: var(--panel-fw-title);
  }

  .obs-panel[data-layout='wide'] .queue-item.current .qi-index {
    color: var(--panel-color);
  }

  /* 歌词区在右侧居中显示，高度按视口留白 */
  .obs-panel[data-layout='wide'] .lyrics-box {
    max-height: 72vh;
    text-align: right;
  }

  .obs-panel[data-layout='wide'] .lyric-line {
    text-align: right;
  }

  /* 进度条收窄，避免在文字列里显得突兀 */
  .obs-panel[data-layout='wide'] .np-progress {
    max-width: 100%;
  }
}
</style>
