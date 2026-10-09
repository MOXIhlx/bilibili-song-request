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
 * ## 关于"字太小"
 * 面板字号是固定 px，而预览被等比缩放，所以字**按同一比例**变小——
 * 也就是说：**预览里字小 = 真实 OBS 源里字也小**，这本身是真实情况，
 * 不是 bug（想让它变大应当调「字号」，而不是让预览骗人）。
 *
 * 为了让细节能看清，这里**不做任何缩放**，只按固定的虚拟画布
 * （`CANVAS_W × CANVAS_H`）渲染出内容；**缩放到栏宽、或按 1:1 显示并滚动，
 * 都由父组件用 CSS 处理**。这样两种模式的取舍留在外层，组件本身保持"如实渲染"。
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
  /** 预览哪一页：all / play / lyrics / danmaku。 */
  page: string
  /**
   * **虚拟画布尺寸**（CSS px）。
   *
   * ## 这就是"预览像不像 OBS"的关键
   * 面板的排版（换行位置、`wide` 是否左右分栏、``cover`` 背景图裁掉哪一块）
   * **完全由容器尺寸决定**。OBS 浏览器源的宽高是多少，面板就该按多少渲染。
   *
   * 早期这里**写死 960×540**（我拍的"典型 16:9"），结果用户的实际源是
   * **400×600 竖版**：预览里的换行与背景裁切和 OBS 里完全对不上，
   * 而且他怎么调都调不准。现在尺寸由调用方传入（设置页里可填、可存）。
   */
  canvasW: number
  canvasH: number
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
 * 画布尺寸的兜底值。
 *
 * 只在调用方没传（或传了非法值）时用。`1920×1080` 是 OBS 的标准画布尺寸，
 * 也是绝大多数人的实际情况。
 */
const FALLBACK_W = 1920
const FALLBACK_H = 1080

const canvasW = computed(() => (props.canvasW > 0 ? props.canvasW : FALLBACK_W))
const canvasH = computed(() => (props.canvasH > 0 ? props.canvasH : FALLBACK_H))
</script>

<template>
  <!--
    固定虚拟画布：宽高由调用方给出，**组件自己不做任何缩放**。
    缩放比由父组件通过 CSS 变量 `--pv-scale` 给出（默认 1 = 1:1 实际大小）。
    这样组件始终"如实渲染"，不会出现"预览里看着刚好、OBS 里差一半"。
  -->
  <div
    class="panel-preview-stage"
    :style="{ width: `${canvasW}px`, height: `${canvasH}px` }"
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
</template>

<style scoped>
/*
 * 画布本体。用 `zoom` 而不是 `transform: scale`：
 *  - `zoom` 会**改变布局尺寸**，父容器用 `overflow: auto` 就能自然地
 *    "该滚动就滚动"，不需要手工补偿缩放后的高度；
 *  - `transform` 不改变布局尺寸，缩放后元素仍占原尺寸，必须手工补高度。
 */
.panel-preview-stage {
  zoom: var(--pv-scale, 1);
}

/* 面板根节点默认 `min-height: 100vh`，在预览里会撑成整个视口高，必须压回画布高 */
.panel-preview-stage :deep(.obs-panel) {
  min-height: 100%;
}
</style>
