<script setup lang="ts">
/**
 * 全局唯一的导航条。
 *
 * ## 为什么要有这个组件
 * 早期导航分散在**两处**：
 *  - 顶栏：`控制台 / 设置`（页级路由）
 *  - 页面内：`直播与播放 / 点歌队列 / 空闲歌单 / 点歌日志 / 黑名单`（面板内标签）
 *
 * 结果是同一个功能出现两次，而且会出现自相矛盾的链接——在设置页里点「基础」
 * 会跳回控制台。用户反馈「上面不要再留路由了，直接统一放在标签栏这里」。
 *
 * 现在只有这一条：
 *
 * ```
 * [直播与播放][点歌队列][空闲歌单][点歌日志] │ [黑名单]   [设置]      [刷新状态]
 * ```
 *
 * 左边一组是**面板内标签**（靠 `v-model` 切换，不换路由）；右边是**页级路由**。
 * `控制台` 不单独占一格——点左边任一标签就等于回控制台。
 *
 * ## 用法
 * ```vue
 * <!-- 控制台：受控标签 + 刷新 -->
 * <NavTabs v-model="tab" :counts="{ queue: 3, idle: 26 }" :refreshing="store.loading"
 *          @refresh="store.refresh()" />
 * <!-- 设置页：没有面板内标签，传 null -->
 * <NavTabs :model-value="null" />
 * ```
 */
import { computed } from 'vue'
import { RouterLink, useRoute } from 'vue-router'

/** 面板内标签的键（与 `Dashboard.vue` 的 `TabKey` 保持一致）。 */
export type NavTabKey = 'live' | 'queue' | 'idle' | 'logs' | 'blacklist'

const props = withDefaults(
  defineProps<{
    /** 当前选中的面板内标签；`null` = 不在控制台（例如设置页）。 */
    modelValue?: NavTabKey | null
    /** 刷新按钮的忙碌状态。 */
    refreshing?: boolean
    /** 各标签的计数；0 或未提供则不显示徽章。 */
    counts?: Partial<Record<NavTabKey, number>>
    /** 是否显示「刷新状态」按钮（不监听 `refresh` 事件时自动隐藏）。 */
    showRefresh?: boolean
  }>(),
  {
    modelValue: null,
    refreshing: false,
    counts: undefined,
    showRefresh: false,
  },
)

const emit = defineEmits<{
  (e: 'update:modelValue', value: NavTabKey): void
  (e: 'refresh'): void
}>()

const route = useRoute()

/** 是否在控制台（决定左边那组标签是否高亮）。 */
const onDashboard = computed(() => route.path.startsWith('/dashboard'))

/** 面板内标签清单。 */
const TABS: Array<{ key: NavTabKey; label: string }> = [
  { key: 'live', label: '直播与播放' },
  { key: 'queue', label: '点歌队列' },
  { key: 'idle', label: '空闲歌单' },
  { key: 'logs', label: '点歌日志' },
  { key: 'blacklist', label: '黑名单' },
]

/** 某个标签上的计数（0 或未提供就不显示徽章）。 */
function countOf(key: NavTabKey): number | null {
  const n = props.counts?.[key]
  return typeof n === 'number' && n > 0 ? n : null
}

/**
 * 配置入口（与三个子标签一一对应）。
 *
 * 顺序按「改得多 → 改得少」：基础设置（规则/平台）→ OBS 面板（外观/地址）
 * → 背景图库（上传素材）。
 */
const SETTINGS: Array<{ path: string; label: string }> = [
  { path: '/settings', label: '设置' },
  { path: '/settings/obs', label: 'OBS 面板' },
  { path: '/settings/background', label: '背景图库' },
]
</script>

<template>
  <div class="nav-tabs">
    <!-- 左：面板内标签（只在控制台里有意义） -->
    <div class="nav-group">
      <RouterLink
        v-for="t in TABS"
        :key="t.key"
        :to="`/dashboard?tab=${t.key}`"
        class="nav-item"
        :class="{ 'is-active': onDashboard && modelValue === t.key }"
        @click="emit('update:modelValue', t.key)"
      >
        {{ t.label }}
        <span v-if="countOf(t.key) !== null" class="badge">{{ countOf(t.key) }}</span>
      </RouterLink>
    </div>

    <span class="nav-sep" aria-hidden="true" />

    <!--
      右：配置。三个入口**直接铺开**，不做二级菜单。
      它们原本藏在设置页内部的子标签里，首页底部另外还留了两个链接
      （「OBS 面板设置 →」「预览综合面板」）——位置低、几乎看不见。
      用户反馈「为什么一定要在主页底部留两个，就不能直接把 OBS 面板跟设置一样
      也放在上面吗」。现在统一到这一条导航里，底部的链接全部删除。
    -->
    <div class="nav-group">
      <RouterLink
        v-for="s in SETTINGS"
        :key="s.path"
        :to="s.path"
        class="nav-item"
        :class="{ 'is-active': route.path === s.path }"
      >
        {{ s.label }}
      </RouterLink>
    </div>

    <button
      v-if="showRefresh"
      class="nav-item nav-refresh"
      :disabled="refreshing"
      @click="emit('refresh')"
    >
      刷新状态
    </button>
  </div>
</template>

<style scoped>
.nav-tabs {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 6px;
  padding: 10px 20px;
  border-bottom: 1px solid var(--bsr-border);
  background: var(--bsr-bg-elevated);
}

.nav-group {
  display: flex;
  align-items: center;
  gap: 6px;
  flex-wrap: wrap;
}

/* 两组之间的细分隔线：弱化，只表示"这里换了一类" */
.nav-sep {
  width: 1px;
  height: 20px;
  margin: 0 6px;
  background: var(--bsr-border);
}

.nav-item {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 5px 12px;
  border: 1px solid transparent;
  border-radius: 999px;
  background: transparent;
  color: var(--bsr-muted);
  font-size: 13px;
  text-decoration: none;
  cursor: pointer;
  transition: background 0.12s ease, color 0.12s ease, border-color 0.12s ease;
}

.nav-item:hover {
  background: var(--bsr-accent-soft);
  color: var(--bsr-fg);
}

.nav-item.is-active {
  border-color: var(--bsr-accent);
  background: var(--bsr-accent-soft);
  color: var(--bsr-accent);
  font-weight: 600;
}

/* 计数徽章：比标签文字轻，避免读成"点歌队列0"一个词 */
.badge {
  padding: 0 6px;
  border-radius: 999px;
  background: var(--bsr-accent-soft);
  color: var(--bsr-accent);
  font-size: 11px;
  line-height: 16px;
  font-weight: 500;
}

.nav-refresh {
  margin-left: auto;
}
</style>
