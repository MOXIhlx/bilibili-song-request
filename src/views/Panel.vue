<script setup lang="ts">
/**
 * OBS 面板页（路由 `/panel`、`/panel/play|lyrics|danmaku`）。
 *
 * ## 职责
 * 这个文件只做三件事：
 *  1. 取配置里的样式列表，按地址里的 `?style=<id>` 解析出**生效样式**
 *     （找不到就回落默认样式，再回落列表第一项）；
 *  2. 从 store 拿真实数据（当前歌曲 / 队列 / 播放器 / 弹幕）；
 *  3. 把两者交给 [`PanelView`] 渲染。
 *
 * ## 渲染为什么在别处
 * 同一套面板还要在**设置页右侧当预览**用，而且必须长得一模一样。
 * 早期预览是 `<iframe>` 指向本页，结果两个问题都解决不了：
 * 改样式不能实时看到（跨源、`postMessage` 与 `localStorage` 两条路都不通）、
 * 预览与 OBS 排版不一致（iframe 视口宽度差太多）。
 * 抽成 [`PanelView`] 之后，预览本地渲染，两边共用同一份标记与样式。
 */
import { computed, onMounted, onUnmounted, ref } from 'vue'
import PanelView from '@/components/PanelView.vue'
import { getConfig } from '@/api'
import { useAppStore } from '@/stores/app'
import { styleToResolved, usePanelPage, usePanelStyle } from '@/composables/panelParams'
import type { PanelStyleConfig } from '@/types'

const store = useAppStore()

/**
 * 当前专注页（阶段 8）。
 *
 * 综合面板在 OBS 里常常放不下、字号又太小，所以拆出几个专用地址：
 * `/panel/play`（进度条+队列）、`/panel/lyrics`（歌词）、
 * `/panel/danmaku`（最近弹幕）。`all` 是原有的综合面板，行为完全不变。
 */
const page = usePanelPage()

/**
 * 样式列表与默认 id。
 *
 * 自己拉一次配置：地址里的 `?style=<id>` 要从这份列表里查，
 * 查不到则回落到 `default_style_id`。
 */
const configStyles = ref<PanelStyleConfig[] | null>(null)
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

/** 按地址解析出的样式（内部订阅 URL 变化，改 OBS 地址即时生效）。 */
const baseStyle = usePanelStyle(
  () => configStyles.value ?? store.config?.panel_styles,
  () => configDefaultId.value ?? store.config?.default_style_id,
)

/**
 * 实时预览推来的**草稿**样式覆盖。
 *
 * ## 现状：暂时用不上，但保留
 * 设置页的预览已经改成**本地渲染**（见 `PanelPreview.vue`），不再依赖
 * 这个通道，所以正常情况下这里始终是 `null`。
 * 保留它有两个理由：
 *  1. 如果将来还用 iframe 预览（例如浏览器里打开控制台），这条能直接生效；
 *  2. 交付的安装包里可能残留旧设置页，收到消息时不至于没反应。
 */
const previewOverride = ref<PanelStyleConfig | null>(null)

const PREVIEW_STYLE_KEY = 'bsr:preview-style'

/** 应用一份草稿样式覆盖（`null` = 恢复用地址里的样式）。 */
function applyPreviewOverride(next: import('@/types').PanelStyleConfig | null): void {
  previewOverride.value = next
}

/** 同源页面（设置页的桥接 iframe）改了草稿 → `storage` 事件送达本页。 */
function onPreviewStorage(ev: StorageEvent): void {
  if (ev.key !== PREVIEW_STYLE_KEY) return
  if (!ev.newValue) {
    applyPreviewOverride(null)
    return
  }
  try {
    applyPreviewOverride(JSON.parse(ev.newValue) as PanelStyleConfig)
  } catch {
    applyPreviewOverride(null)
  }
}

onMounted(() => window.addEventListener('storage', onPreviewStorage))
onUnmounted(() => window.removeEventListener('storage', onPreviewStorage))

/** 实际生效的样式：草稿覆盖优先，否则用地址里的样式。 */
const style = computed(() => {
  const o = previewOverride.value
  return o ? styleToResolved(o, baseStyle.value.scale) : baseStyle.value
})
</script>

<template>
  <PanelView
    :resolved="style"
    :page="page"
    :current="store.current"
    :queue="store.queue"
    :player="store.player"
    :danmaku-log="store.danmakuLog"
  />
</template>
