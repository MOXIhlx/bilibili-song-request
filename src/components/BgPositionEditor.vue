<script setup lang="ts">
/**
 * 背景图**取景位置**编辑器。
 *
 * ## 解决什么问题
 * 面板用 `background-size: cover` 铺背景图：图片比例与面板不一致时，
 * **必然要裁掉一部分**，而裁哪一部分由 `background-position` 决定。
 * 默认 `center` 经常正好裁掉主体（人脸、标题），用户需要能自己拖——这就是
 * 这个组件的全部职责。
 *
 * ## 交互
 * 直接**拖动图片**：往哪边拖，就露出那一侧的更多内容（与在手机相册里
 * 平移一张被裁过的图是同一直觉）。
 *
 * ## 坐标数学
 * `background-position: X% Y%` 的语义是「图片上 X% 的那个点，对齐到容器的
 * X% 位置」。设容器 `cw×ch`、图片按 cover 铺成 `iw×ih`（`iw ≥ cw` 且
 * `ih ≥ ch`），则不重复铺图时可滑动的范围是：
 *
 * ```
 * 横向可滑 = iw - cw        纵向可滑 = ih - ch
 * ```
 *
 * CSS 的百分比写法**正好**是「可滑范围的百分比」（端点对齐），所以：
 *
 * ```
 * 横向偏移（px） = -(X% / 100) × (iw - cw)
 * ```
 *
 * 反过来，用户拖了 `dx` 像素后新的百分比就是：
 *
 * ```
 * X% = clamp(0, 100, 旧的 X% - dx / (iw - cw) × 100)
 * ```
 *
 * 当图片比容器还小时（`iw - cw ≤ 0`）该轴无法滑动，百分比无意义，
 * 直接固定 50%。
 */
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'

const props = defineProps<{
  /** 背景图地址（已补成绝对地址的传入由调用方负责）。 */
  src: string
  /** 容器的宽高比（面板画布的 `w / h`，例如 16/9）。 */
  aspect: number
  /** 当前取景位置，CSS 写法（如 `"50% 50%"`）。空 = center。 */
  position: string
}>()

const emit = defineEmits<{
  /** 拖动中与松手后都会触发，父组件可即时预览。 */
  (e: 'update', value: string): void
}>()

/** 取景框元素，用于量可用宽度。 */
const frameEl = ref<HTMLElement | null>(null)
/** 图片自然尺寸。 */
const natural = ref({ w: 0, h: 0 })
const imgEl = ref<HTMLImageElement | null>(null)

/** 拖动中的临时位置（`null` = 用 props 的值）。 */
const dragging = ref<{ x: number; y: number } | null>(null)

/**
 * 解析 `"50% 50%"` / `"left top"` 成两个百分比数字。
 *
 * 只支持百分比与 `center`——拖动界面本来也只产出百分比，
 * 关键字写法（`left` 等）无法参与增量计算，遇到时按 50% 处理。
 */
function parsePosition(value: string): { x: number; y: number } {
  const parts = (value || '').trim().split(/\s+/).filter(Boolean)
  if (!parts.length) return { x: 50, y: 50 }
  const read = (raw: string | undefined): number => {
    if (!raw) return 50
    const m = /^(-?[\d.]+)%$/.exec(raw)
    if (m) {
      const n = Number(m[1])
      return Number.isFinite(n) ? Math.min(100, Math.max(0, n)) : 50
    }
    // 关键字：按 CSS 语义映射
    switch (raw.toLowerCase()) {
      case 'left':
      case 'top':
        return 0
      case 'right':
      case 'bottom':
        return 100
      default:
        return 50
    }
  }
  if (parts.length === 1) return { x: read(parts[0]), y: 50 }
  return { x: read(parts[0]), y: read(parts[1]) }
}

/** 当前显示的百分比。 */
const shown = computed(() => dragging.value ?? parsePosition(props.position))

/** 取景框实际尺寸（由可用宽度 + `aspect` 算出）。 */
const frameWidth = ref(480)
const frameHeight = computed(() => Math.round(frameWidth.value / (props.aspect || 16 / 9)))

/**
 * 计算 cover 铺图后图片的显示尺寸。
 *
 * cover = 等比放大到**同时覆盖**容器的宽和高，取两者所需缩放比的较大值。
 */
const imageBox = computed(() => {
  const { w: nw, h: nh } = natural.value
  const cw = frameWidth.value
  const ch = frameHeight.value
  if (!nw || !nh || !cw || !ch) return { w: cw, h: ch }
  const scale = Math.max(cw / nw, ch / nh)
  return { w: nw * scale, h: nh * scale }
})

/** 图片在显示层里的内联样式：尺寸 + 偏移。 */
const imageStyle = computed(() => {
  const { w, h } = imageBox.value
  const cw = frameWidth.value
  const ch = frameHeight.value
  const { x, y } = shown.value
  // 百分比 → 负偏移（见文件头的推导）
  const dx = -((x / 100) * (w - cw))
  const dy = -((y / 100) * (h - ch))
  return {
    width: `${w}px`,
    height: `${h}px`,
    transform: `translate(${dx}px, ${dy}px)`,
  }
})

/** 该轴能否滑动（图片比容器大才有余量）。 */
const canPanX = computed(() => imageBox.value.w - frameWidth.value > 1)
const canPanY = computed(() => imageBox.value.h - frameHeight.value > 1)

/** 拖动状态。 */
let panning = false
let last = { x: 0, y: 0 }

function onPointerDown(ev: PointerEvent): void {
  if (!canPanX.value && !canPanY.value) return
  panning = true
  last = { x: ev.clientX, y: ev.clientY }
  dragging.value = { ...shown.value }
  ;(ev.target as HTMLElement).setPointerCapture?.(ev.pointerId)
  ev.preventDefault()
}

function onPointerMove(ev: PointerEvent): void {
  if (!panning) return
  const { w, h } = imageBox.value
  const cw = frameWidth.value
  const ch = frameHeight.value
  const slackX = w - cw
  const slackY = h - ch
  const cur = dragging.value ?? parsePosition(props.position)
  const next = { ...cur }
  if (slackX > 1) {
    // 往右拖 → 露出左侧更多 → 百分比减小
    next.x = cur.x - ((ev.clientX - last.x) / slackX) * 100
  }
  if (slackY > 1) {
    next.y = cur.y - ((ev.clientY - last.y) / slackY) * 100
  }
  next.x = Math.min(100, Math.max(0, next.x))
  next.y = Math.min(100, Math.max(0, next.y))
  dragging.value = next
  last = { x: ev.clientX, y: ev.clientY }
  emit('update', `${next.x.toFixed(1)}% ${next.y.toFixed(1)}%`)
}

function onPointerUp(): void {
  if (!panning) return
  panning = false
  const v = dragging.value
  if (v) emit('update', `${v.x.toFixed(1)}% ${v.y.toFixed(1)}%`)
  dragging.value = null
}

/** 量取景框宽度（窗口变化时跟随）。 */
let ro: ResizeObserver | null = null
onMounted(() => {
  const el = frameEl.value
  if (el) {
    const sync = () => {
      const w = el.clientWidth
      if (w > 0) frameWidth.value = w
    }
    sync()
    ro = new ResizeObserver(sync)
    ro.observe(el)
  }
  const img = new Image()
  img.onload = () => {
    natural.value = { w: img.naturalWidth, h: img.naturalHeight }
  }
  img.src = props.src
  imgEl.value = img
})
onBeforeUnmount(() => ro?.disconnect())

/** 把取景位置复位到正中。 */
function resetCenter(): void {
  dragging.value = null
  emit('update', '50% 50%')
}

defineExpose({ resetCenter })
</script>

<template>
  <div ref="frameEl" class="pan-frame" :style="{ height: `${frameHeight}px` }">
    <img
      class="pan-image"
      :src="src"
      :style="imageStyle"
      :class="{ 'is-pannable': canPanX || canPanY }"
      alt=""
      draggable="false"
      @pointerdown="onPointerDown"
      @pointermove="onPointerMove"
      @pointerup="onPointerUp"
      @pointercancel="onPointerUp"
    />
  </div>
</template>

<style scoped>
/*
 * 取景框：与面板画布同比例，`overflow: hidden` 才能看出"被裁掉的部分"。
 * 棋盘格底纹让图片未覆盖到的区域（理论上 cover 下不会有）一眼可见。
 */
.pan-frame {
  position: relative;
  width: 100%;
  overflow: hidden;
  border: 1px solid var(--bsr-border);
  border-radius: 8px;
  background-color: #f6e9ef;
  /* 拖图时不要选中/滚动 */
  touch-action: none;
  user-select: none;
}

.pan-image {
  position: absolute;
  top: 0;
  left: 0;
  /* 尺寸与偏移都由内联样式给出；用 transform 走 GPU，拖动才不卡 */
  will-change: transform;
}

.pan-image.is-pannable {
  cursor: grab;
}

.pan-image.is-pannable:active {
  cursor: grabbing;
}
</style>
