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
import { computed, nextTick, onMounted, ref, watch } from 'vue'
import { getConfig } from '@/api'
import { useAppStore } from '@/stores/app'
import { styleToCssVars, usePanelPage, usePanelStyle } from '@/composables/panelParams'
import { parseLrc } from '@/composables/lrc'

const store = useAppStore()

/**
 * 面板页自己的配置副本。
 *
 * ## ⚠️ 为什么不能只读 `store.config`
 * `store.bootstrap()` 只在 **控制台**（`App.vue` 的 `onMounted`）里跑。
 * 面板页是 OBS 浏览器源直接打开的独立页面，`store.config` **永远是 null**——
 * 于是「默认样式」里的背景图、主色、字号等对面板**完全不生效**，
 * 只有 URL 参数才管用（实测：配置里 `bg_image` 有值，面板却是
 * `--panel-bg-image: none`）。
 *
 * 这里自己拉一次配置，URL 参数仍然优先（由 `resolvePanelStyle` 决定）。
 */
const configDefaults = ref<Partial<import('@/types').PanelStyleConfig> | null>(null)

onMounted(async () => {
  if (store.config?.panel) {
    configDefaults.value = store.config.panel
    return
  }
  try {
    const cfg = await getConfig()
    configDefaults.value = cfg.panel
  } catch (err) {
    // 取不到就用内置默认值，绝不让面板空白
    console.warn('面板读取默认样式失败，使用内置默认值：', err)
  }
})

/** 面板样式（内部订阅 URL 变化，改 OBS 地址即时生效）。 */
const style = usePanelStyle(
  () => configDefaults.value ?? store.config?.panel,
)

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
  --panel-fg: #5a4450;
  --panel-sub: rgba(90, 68, 80, 0.62);
  --panel-bg: transparent;
  /* 卡片/列表底色与进度条轨道：默认全透明，只要文字与进度条颜色 */
  --panel-surface: transparent;
  --panel-track: transparent;
  --panel-font-size: 16px;
  --panel-scale: 1;
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
  font-weight: 700;
  color: var(--panel-color);
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
  background: var(--panel-color);
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

/* 正在唱的那一行：放大 + 主色 + 微微发光 */
.lyric-line.active {
  color: var(--panel-color);
  font-size: 1.08em;
  font-weight: 700;
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
  font-weight: 600;
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
  font-weight: 700;
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
    font-weight: 700;
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
