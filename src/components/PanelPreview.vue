<script setup lang="ts">
/**
 * 设置页右侧的**面板预览**。
 *
 * ## 为什么是本地渲染，而不是 iframe
 * 早期预览是一个 `<iframe>` 指向 `/panel`，两个问题都解决不了：
 *
 * **1. 改样式不能实时看到**
 * 预览在 `127.0.0.1:17777`、设置页在 `tauri.localhost`，跨源被 WebView2
 * 站点隔离到不同进程。实测：
 * ```
 * iframe.contentDocument → null
 * 父 → 子、子 → 父 postMessage → 均无回音
 * ```
 * 后来试过「桥接 iframe + localStorage 的 storage 事件广播」，也不行——
 * 因为**设置页与桥接 iframe 同样跨源**，第一跳就断了。
 *
 * **2. 预览与 OBS 不是一回事**
 * iframe 宽度只有几百像素，面板按那个视口排版，换行位置与 `wide` 布局比例
 * 都和 OBS 里不同——预览反而误导人。
 *
 * 现在改为**本地渲染 [`PanelView`]**：草稿一改立刻重渲染（没有跨源、没有
 * 延迟、不需要保存），而且与 OBS 用的是**同一份标记与样式**。
 *
 * ## 外观是怎样"放进"右栏的
 * 面板在 OBS 里通常是一整块画布，这里用两层缩放把它装进窄栏：
 *  - `PanelView` 内部按 `--panel-scale` 缩放（面板自身的字号倍率）；
 *  - 外层再用 `transform: scale(...)` 把整块等比缩到栏宽。
 * 两层都是**等比**的，所以版式与 OBS 一致，只是整体小了一号。
 */
import { computed } from 'vue'
import PanelView from '@/components/PanelView.vue'
import type { ResolvedPanelStyle } from '@/composables/panelParams'
import type { DanmakuLogEntry } from '@/stores/app'
import type {
  PlayerState,
  QueueItem,
  QueuePriority,
  QueueStatus,
  Song,
} from '@/types'

const props = defineProps<{
  /** 当前编辑中的样式（草稿，未保存也会生效）。 */
  style: ResolvedPanelStyle
  /** 面板页面尺寸（CSS px），由外层量出来传入。 */
  width: number
  height: number
  /** 预览哪一页：all / play / lyrics / danmaku。 */
  page: string
  /** 是否用真实数据（设置页里没有真实数据时用示例）。 */
  useRealData?: boolean
  /** 真实数据（`useRealData` 为真时使用）。 */
  current?: QueueItem | null
  queue?: QueueItem[]
  player?: PlayerState | null
  danmakuLog?: DanmakuLogEntry[]
}>()

// ── 示例数据 ────────────────────────────────────────────────────────────
//
// 预览要有内容才看得出效果。这里造一份"一屏能看全"的假数据：
// 一首歌 + 两条队列 + 三行歌词 + 两条弹幕。
// 刻意**不用真实队列**——主播常常在没开播时调样式，那时队列是空的，
// 预览就什么都看不到，等于没法调。

/** 造一首示例歌（`source` 用 `resolved`，面板才会显示歌手等信息）。 */
function mockSong(title: string, artist: string): Song {
  return {
    id: `mock-${title}`,
    title,
    artist,
    platform: 'netease',
    duration: 252,
    cover_url: null,
    album: null,
    source: 'resolved',
    source_error: null,
  }
}

/** 造一条示例队列项。 */
function mockItem(
  id: string,
  title: string,
  artist: string,
  user: string,
  status: QueueStatus,
  priority: QueuePriority = 'danmaku',
): QueueItem {
  return {
    id,
    song: mockSong(title, artist),
    requested_by: user,
    requested_by_uid: null,
    requested_at: new Date().toISOString(),
    status,
    priority,
  }
}

const MOCK_CURRENT = mockItem('mock-current', '示例歌曲', '示例歌手', '示例观众', 'playing')
const MOCK_QUEUE: QueueItem[] = [
  MOCK_CURRENT,
  mockItem('mock-2', '队列第二首', '另一位歌手', '观众B', 'pending'),
  mockItem('mock-3', '队列第三首', '第三位歌手', '观众C', 'pending', 'host'),
]

const MOCK_PLAYER: PlayerState = {
  playing: true,
  paused: false,
  position: 40,
  duration: 252,
  volume: 52,
  lyrics:
    '[00:00.00]第一行示例歌词\n[00:20.00]第二行示例歌词\n' +
    '[00:40.00]正在唱的这行\n[01:00.00]下一行示例歌词',
  lyric_index: 2,
}

const MOCK_DANMAKU: DanmakuLogEntry[] = [
  { user: '观众A', uid: null, text: '点歌 晴天 周杰伦', at: new Date().toISOString() },
  { user: '观众B', uid: null, text: '这首好听', at: new Date().toISOString() },
]

/** 实际交给面板的数据。 */
const current = computed(() => (props.useRealData ? (props.current ?? null) : MOCK_CURRENT))
const queue = computed(() => (props.useRealData ? (props.queue ?? []) : MOCK_QUEUE))
const player = computed(() => (props.useRealData ? (props.player ?? null) : MOCK_PLAYER))
const danmakuLog = computed(() =>
  props.useRealData ? (props.danmakuLog ?? []) : MOCK_DANMAKU,
)

/**
 * 面板的虚拟画布尺寸。
 *
 * 与 OBS 里的典型画布接近，这样版式（换行、`wide` 分栏）才有参考价值；
 * 再整体等比缩到栏宽，所以缩放不会让文字比例失真。
 */
const CANVAS_W = 960
const CANVAS_H = 540

/** 把虚拟画布等比缩放到栏宽。 */
const scale = computed(() => {
  if (!props.width || !props.height) return 1
  return Math.min(props.width / CANVAS_W, props.height / CANVAS_H)
})
</script>

<template>
  <div
    class="panel-preview"
    :style="{ width: `${width}px`, height: `${height}px` }"
  >
    <div
      class="panel-preview-stage"
      :style="{
        width: `${CANVAS_W}px`,
        height: `${CANVAS_H}px`,
        transform: `scale(${scale})`,
        transformOrigin: 'top left',
      }"
    >
      <PanelView
        :resolved="style"
        :page="page"
        :current="current"
        :queue="queue"
        :player="player"
        :danmaku-log="danmakuLog"
      />
    </div>
  </div>
</template>

<style scoped>
/*
 * 预览容器：固定尺寸 + 裁掉溢出。
 * 里面那块是 960×540 的虚拟画布，被 `transform` 缩放后仍占据原尺寸，
 * 所以必须 `overflow: hidden`，否则会撑破右栏。
 */
.panel-preview {
  position: relative;
  overflow: hidden;
  border: 1px solid var(--bsr-border);
  border-radius: 8px;
  /* 棋盘格：透明背景是否生效一眼可见（与 OBS 里勾选"透明背景"对应） */
  background-color: #f6e9ef;
  background-image:
    linear-gradient(45deg, rgba(255, 111, 165, 0.12) 25%, transparent 25%),
    linear-gradient(-45deg, rgba(255, 111, 165, 0.12) 25%, transparent 25%),
    linear-gradient(45deg, transparent 75%, rgba(255, 111, 165, 0.12) 75%),
    linear-gradient(-45deg, transparent 75%, rgba(255, 111, 165, 0.12) 75%);
  background-size: 18px 18px;
  background-position:
    0 0,
    0 9px,
    9px -9px,
    -9px 0;
}

.panel-preview-stage {
  /* 缩放后仍在左上角对齐 */
  will-change: transform;
}

/*
 * 面板根节点默认 `min-height: 100vh`——在预览里那会撑成整个视口高，
 * 把布局顶乱。这里强制等于虚拟画布高度。
 */
.panel-preview-stage :deep(.obs-panel) {
  min-height: 100%;
}
</style>
