<script setup lang="ts">
/**
 * 背景图裁剪 / 旋转 / 缩放 / 镜像编辑器。
 *
 * ## 用 cropperjs 2.x（Web Components 架构）
 * v2 与 v1 的 API 完全不同：元素由包在导入时**自动注册**（`<cropper-canvas>`、
 * `<cropper-image>`、`<cropper-selection>` 等，命名空间固定为 `cropper`），
 * 所以这里用声明式写法，再通过 `getCropperSelection()` 拿到选区做导出。
 *
 * ## 关键行为：裁剪框**可以拖出图片边界**
 * 这是明确的需求（想要留白 / 做画布式排版）。`$toCanvas()` 只把选区内
 * **图片实际覆盖到**的部分画出来，所以越界区域天然是透明的。
 * 也正因如此导出**必须用 PNG**——JPEG 会把透明区压成黑块。
 *
 * ## 变换是「烘焙」进新图的
 * 旋转 / 缩放 / 镜像都发生在 canvas 上，导出的是最终像素。
 * 好处：OBS 里所见即所得，样式里只存图片路径，不存在「样式存了变换但
 * 图片被换掉」导致对不上的问题。
 *
 * ## 另存而不是覆盖
 * 由父组件调用 `saveEditedBackground()`，后端会生成 `原名-编辑.png`
 * 并自动回避重名，**原图始终保留**。
 */
import { computed, onBeforeUnmount, onMounted, ref, shallowRef } from 'vue'
/*
 * ⚠️ 这个 import **只为副作用**：cropperjs 在模块加载时把自己那 8 个自定义元素
 * 注册到 `customElements`（`CropperCanvas.$define()` 等）。
 *
 * 早期这里写的是 `import Cropper from 'cropperjs'` + `new Cropper(el)`，改成纯
 * 声明式写法后 `Cropper` 不再被引用，于是 **Vite 把这个 import 整个摇掉了**——
 * 后果是模板里的 `<cropper-selection>` 永远不会升级，`$toCanvas` 不存在，
 * 编辑器打开后选区尺寸全是 0、点「另存」也导不出东西。
 * 所以这一行必须保留，且不能改成 `import type`。
 */
import 'cropperjs'
import type { CropperSelection } from 'cropperjs'

const props = defineProps<{
  /** 要编辑的图片地址（可直接喂给 <cropper-image src>）。 */
  src: string
  /** 源文件名，用于派生「原名-编辑.png」。 */
  name: string
}>()

const emit = defineEmits<{
  /** 已生成编辑结果，交给父组件上传。 */
  (e: 'save', blob: Blob): void
  (e: 'cancel'): void
}>()

/*
 * 元素引用。
 *
 * ⚠️ v2 **不需要** `new Cropper(element)`：那套 API 只接受**原生**
 * `<img>` / `<canvas>`，传 `cropper-image`（自定义元素）会被拒绝：
 *   「The first argument is required and must be an <img> or <canvas> element.」
 *
 * v2 的正确用法是纯声明式——导入包时元素就自动升级注册好了，
 * 直接在 DOM 上取 `cropper-selection` / `cropper-image` 调它们的方法即可。
 * 所以这里用 `shallowRef` 存**元素本身**，不存 Cropper 实例。
 */
const selectionEl = shallowRef<CropperSelection | null>(null)
const imageEl = shallowRef<HTMLElement | null>(null)
const busy = ref(false)
const error = ref('')

/** 预览用的输出尺寸（跟随选区，实时更新）。 */
const outWidth = ref(0)
const outHeight = ref(0)
/** 预计导出的 PNG 大小（字节），只在点击「另存」后才知道真实值。 */
const lastSavedBytes = ref(0)

/** 当前宽高比（0 = 自由）。 */
const aspect = ref(0)

const ASPECTS: Array<{ value: number; label: string }> = [
  { value: 0, label: '自由' },
  { value: 16 / 9, label: '16:9' },
  { value: 9 / 16, label: '9:16' },
  { value: 4 / 3, label: '4:3' },
  { value: 1, label: '1:1' },
]

/** 取当前裁剪选区元素。 */
function selection(): CropperSelection | null {
  return selectionEl.value
}

/** 刷新「输出尺寸」提示。 */
function refreshSize(): void {
  const sel = selection()
  if (!sel) return
  outWidth.value = Math.round(sel.width)
  outHeight.value = Math.round(sel.height)
}

onMounted(async () => {
  if (error.value) return
  /*
   * 等元素升级完成。
   *
   * ⚠️ 这里**不能吞掉异常**：如果 `customElements.whenDefined` 永远不 resolve
   * （例如上面那个副作用 import 被 tree-shake 掉，元素压根没注册），
   * 静默失败会让编辑器"看起来能开"、但选区尺寸全是 0、导出为空，
   * 排查时完全看不出原因。所以加超时并明确报错。
   */
  try {
    await Promise.race([
      Promise.all([
        customElements.whenDefined('cropper-selection'),
        customElements.whenDefined('cropper-image'),
      ]),
      new Promise((_, reject) =>
        setTimeout(() => reject(new Error('cropperjs 自定义元素未注册（超时 3 秒）')), 3000),
      ),
    ])
  } catch (err) {
    error.value = `编辑器初始化失败：${(err as Error).message}`
    return
  }
  // 等本地 object URL 就绪（见 localSrc 的说明）
  for (let i = 0; i < 40 && !localSrc.value; i++) {
    if (error.value) return
    await new Promise((r) => setTimeout(r, 50))
  }
  loading.value = false
  /*
   * ⚠️ 必须用 DOM 查询，**不能**用模板 ref：
   * 这些是自定义元素（Web Component），Vue 的 `ref` 给到的是组件代理而不是
   * 真正的元素，调用 `$toCanvas()` 之类的方法会失败。实测踩到过。
   */
  const sel = document.querySelector('.stage cropper-selection') as CropperSelection | null
  const img = document.querySelector('.stage cropper-image') as HTMLElement | null
  if (!sel || !img) {
    error.value = '编辑器初始化失败：找不到裁剪组件'
    return
  }
  selectionEl.value = sel
  imageEl.value = img

  // 选区可移动 / 可缩放 / 可键盘微调；**不限制在图片内**（越界即透明）
  sel.movable = true
  sel.resizable = true
  sel.keyboard = true

  /*
   * 等图片解码 + 等一帧布局，然后**用像素显式设定**选区。
   *
   * ⚠️ 模板里写 `width="80%"` 不够：cropperjs 会把 `sel.width` 原样保留成
   * 字符串 `"80%"`，而 `$toCanvas()` / `$center()` 需要的是**数字**。
   * 实测症状：打开编辑器选区只有 80px 高、`输出尺寸` 显示 `0 × 0`。
   * 所以这里量出 canvas 的实际像素，按 80% 算好再 `$change()`。
   */
  const waitImage = new Promise<void>((resolve) => {
    if ((img as HTMLImageElement).complete) {
      resolve()
      return
    }
    img.addEventListener('load', () => resolve(), { once: true })
    img.addEventListener('error', () => resolve(), { once: true })
  })
  await waitImage
  await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)))

  const canvasBox = document.querySelector('.stage cropper-canvas')?.getBoundingClientRect()
  const imgBox = img.getBoundingClientRect()
  if (canvasBox?.width && canvasBox?.height) {
    // 以图片在画布里的可见范围为准，取 80% 并居中
    const baseW = imgBox.width || canvasBox.width
    const baseH = imgBox.height || canvasBox.height
    const w = Math.max(20, Math.round(baseW * 0.8))
    const h = Math.max(20, Math.round(baseH * 0.8))
    const x = Math.round((canvasBox.width - w) / 2)
    const y = Math.round((canvasBox.height - h) / 2)
    sel.$change(x, y, w, h)
  } else {
    sel.$center()
  }
  refreshSize()
})

onBeforeUnmount(() => {
  selectionEl.value = null
  imageEl.value = null
})

/** 旋转 90°（正数顺时针）。 */
function rotate(deg: number): void {
  (imageEl.value as unknown as { $rotate?: (d: number) => void })?.$rotate?.(deg)
  refreshSize()
}

/** 缩放图片。 */
function zoom(factor: number): void {
  (imageEl.value as unknown as { $zoom?: (f: number) => void })?.$zoom?.(factor)
  refreshSize()
}

/** 镜像：翻转图片在 x / y 方向的缩放符号。 */
function flip(axis: 'x' | 'y'): void {
  const img = imageEl.value as unknown as {
    $scaleX?: number
    $scaleY?: number
    $scale?: (x: number, y: number) => void
  } | null
  if (!img?.$scale) return
  const sx = img.$scaleX ?? 1
  const sy = img.$scaleY ?? 1
  img.$scale(axis === 'x' ? -sx : sx, axis === 'y' ? -sy : sy)
  refreshSize()
}

/** 重置到初始状态。 */
function reset(): void {
  const img = imageEl.value as unknown as {
    $resetTransform?: () => void
    $center?: (size?: string) => void
  } | null
  img?.$resetTransform?.()
  img?.$center?.('contain')
  selection()?.$reset?.()
  refreshSize()
}

/** 切换宽高比。 */
function setAspect(value: number): void {
  aspect.value = value
  const sel = selection()
  if (!sel) return
  sel.aspectRatio = value
  refreshSize()
}

/** 导出并交给父组件保存。 */
async function save(): Promise<void> {
  const sel = selection()
  if (!sel) {
    error.value = '没有可导出的选区'
    return
  }
  busy.value = true
  error.value = ''
  try {
    const canvas = await sel.$toCanvas()
    if (!canvas.width || !canvas.height) {
      throw new Error('选区为空，请先框出要保留的区域')
    }
    const blob = await new Promise<Blob | null>((resolve) => {
      // PNG 无损：越界区域必须是透明，JPEG 会变成黑块
      canvas.toBlob((b) => resolve(b), 'image/png')
    })
    if (!blob) throw new Error('生成 PNG 失败')
    lastSavedBytes.value = blob.size
    emit('save', blob)
  } catch (err) {
    error.value = (err as Error).message
  } finally {
    busy.value = false
  }
}

/**
 * 实际喂给 `<cropper-image>` 的地址。
 *
 * ## ⚠️ 必须先用 blob 转成本地 object URL
 * 图片由内嵌服务器提供（`http://127.0.0.1:17777/bg/x.png`），而桌面窗口的
 * 页面来源是 `http://tauri.localhost`——**跨源**。跨源图片画进 canvas 会
 * **污染（taint）**它，随后 `toBlob()` 直接抛：
 *   「Tainted canvases may not be exported.」
 * 于是「另存为新图」永远失败。
 *
 * 先把图片 fetch 成 Blob、再 `URL.createObjectURL` 得到同源地址，
 * canvas 就不会被污染，导出正常。顺带也避开了 OBS/WebView 对跨源图片的
 * 其它限制。组件卸载时要 `revokeObjectURL` 释放。
 */
const localSrc = ref('')
const loading = ref(true)

onMounted(async () => {
  try {
    const res = await fetch(props.src, { mode: 'cors' })
    if (!res.ok) throw new Error(`HTTP ${res.status}`)
    const blob = await res.blob()
    localSrc.value = URL.createObjectURL(blob)
  } catch (err) {
    error.value = `读取图片失败：${(err as Error).message}`
    loading.value = false
  }
})

onBeforeUnmount(() => {
  if (localSrc.value.startsWith('blob:')) URL.revokeObjectURL(localSrc.value)
})

/** 人类可读的大小。 */
const sizeHint = computed(() => {
  if (lastSavedBytes.value) {
    return `${(lastSavedBytes.value / 1024 / 1024).toFixed(2)} MB`
  }
  return '点击「另存为新图」后可知'
})
</script>

<template>
  <div class="editor-mask" @click.self="emit('cancel')">
    <div class="editor">
      <header>
        <strong>编辑背景图 · {{ props.name || '未命名' }}</strong>
        <button class="ghost" @click="emit('cancel')">✕</button>
      </header>

      <div class="toolbar">
        <button class="ghost" @click="rotate(-90)">↶ 左转 90°</button>
        <button class="ghost" @click="rotate(90)">↷ 右转 90°</button>
        <button class="ghost" @click="flip('x')">⇋ 水平镜像</button>
        <button class="ghost" @click="flip('y')">⇅ 垂直镜像</button>
        <button class="ghost" @click="zoom(1.1)">＋ 放大</button>
        <button class="ghost" @click="zoom(0.9)">－ 缩小</button>
        <span class="sep" />
        <span class="dim">宽高比</span>
        <button
          v-for="a in ASPECTS"
          :key="a.label"
          class="ghost"
          :class="{ active: aspect === a.value }"
          @click="setAspect(a.value)"
        >
          {{ a.label }}
        </button>
        <span class="sep" />
        <button class="ghost" @click="reset">恢复原状</button>
      </div>

      <!--
        cropperjs v2 的声明式结构：canvas 负责交互、image 是要编辑的图、
        selection 是裁剪框。选区**可以拖到图片外**，越界处导出为透明。
      -->
      <div class="stage">
        <cropper-canvas background>
          <cropper-image
            v-if="localSrc"
            :src="localSrc"
            alt="背景图"
            rotatable
            scalable
            translatable
          />
          <cropper-shade hidden />
          <cropper-handle action="move" plain />
          <!--
            尺寸在 onMounted 里用**像素**显式设定（见那里的说明）：
            模板上的 `width="80%"` 会以字符串形式留在 `sel.width` 里，
            `$toCanvas()` 需要数字。这里给一组兜底值，避免闪一下 0 尺寸。
          -->
          <cropper-selection
            width="400"
            height="300"
            movable
            resizable
            outlined
            precise
          >
            <cropper-grid role="grid" bordered covered />
            <cropper-crosshair centered />
            <cropper-handle action="move" theme-color="rgba(255, 111, 165, 0.35)" />
            <cropper-handle action="n-resize" />
            <cropper-handle action="e-resize" />
            <cropper-handle action="s-resize" />
            <cropper-handle action="w-resize" />
            <cropper-handle action="ne-resize" />
            <cropper-handle action="nw-resize" />
            <cropper-handle action="se-resize" />
            <cropper-handle action="sw-resize" />
          </cropper-selection>
        </cropper-canvas>
      </div>

      <p v-if="error" class="err">{{ error }}</p>

      <footer>
        <span class="dim">
          输出 {{ outWidth }} × {{ outHeight }} px · PNG 大小 {{ sizeHint }}
          <br />
          裁剪框可以拖出图片边界，超出的部分会变成透明。
        </span>
        <div class="actions">
          <button class="ghost" @click="emit('cancel')">取消</button>
          <button :disabled="busy" @click="save()">
            {{ busy ? '生成中…' : '另存为新图' }}
          </button>
        </div>
      </footer>
    </div>
  </div>
</template>

<style scoped>
.editor-mask {
  position: fixed;
  inset: 0;
  z-index: 60;
  display: flex;
  align-items: center;
  justify-content: center;
  background: rgb(90 68 80 / 45%);
  padding: 20px;
}

.editor {
  display: flex;
  flex-direction: column;
  gap: 10px;
  width: min(960px, 96vw);
  max-height: 92vh;
  padding: 14px 16px 16px;
  border: 1px solid var(--bsr-border);
  border-radius: 12px;
  background: var(--bsr-bg-elevated);
  box-shadow: 0 18px 48px rgb(255 111 165 / 28%);
}

header {
  display: flex;
  align-items: center;
  justify-content: space-between;
}

.toolbar {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 6px;
}

.toolbar .sep {
  width: 1px;
  height: 18px;
  background: var(--bsr-border);
  margin: 0 4px;
}

.toolbar button.active {
  border-color: var(--bsr-accent);
  color: var(--bsr-accent);
}

.stage {
  flex: 1;
  min-height: 320px;
  border: 1px solid var(--bsr-border);
  border-radius: 8px;
  overflow: hidden;
  /* 深一点的底：越界区域的透明一眼可见 */
  background:
    repeating-conic-gradient(rgb(255 111 165 / 10%) 0% 25%, transparent 0% 50%) 50% / 18px 18px,
    #f6e9ef;
}

/* cropper 元素默认不带尺寸，必须显式撑满舞台 */
.stage :deep(cropper-canvas) {
  width: 100%;
  height: 100%;
  display: block;
}

.stage :deep(cropper-image) {
  max-width: 100%;
  max-height: 100%;
}

footer {
  display: flex;
  align-items: flex-end;
  justify-content: space-between;
  gap: 12px;
  font-size: 12px;
  line-height: 1.5;
  color: var(--bsr-muted);
}

.actions {
  display: flex;
  gap: 8px;
  flex: none;
}

.err {
  margin: 0;
  color: var(--bsr-danger);
  font-size: 12px;
}
</style>
