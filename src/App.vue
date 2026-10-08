<script setup lang="ts">
/**
 * 桌面窗口外壳：左侧导航 + 内容区。
 * 顶部显示与内嵌服务器的连接状态（WS 小圆点）。
 */
import { onMounted, onUnmounted } from 'vue'
import { RouterLink, RouterView, useRoute } from 'vue-router'
import { useAppStore } from '@/stores/app'
import {
  collapseMessages,
  dismissMessage,
  expandMessages,
  messages,
  messagesCollapsed,
} from '@/stores/message'

const store = useAppStore()
const route = useRoute()

onMounted(() => {
  void store.bootstrap()
})
onUnmounted(() => {
  store.teardown()
})
</script>

<template>
  <div class="app-shell">
    <header class="app-header">
      <div class="brand">
        <span class="brand-dot" />
        <strong>墨墨弹幕点歌机</strong>
        <span class="brand-version">{{ store.state?.version ?? '…' }}</span>
      </div>

      <nav class="app-nav">
        <RouterLink to="/dashboard" :class="{ active: route.path.startsWith('/dashboard') }">
          控制台
        </RouterLink>
        <!--
          「设置」取代了原来的「OBS 面板」一级入口。
          OBS 面板本质是配置（配一次就不动），不该和控制台并列成导航项——
          那样导航项多、用户一进来就看到一堆配置。
        -->
        <RouterLink to="/settings/obs" :class="{ active: route.path.startsWith('/settings') }">
          设置
        </RouterLink>
      </nav>

      <div class="conn" :data-ok="store.connected">
        <span class="conn-dot" />
        {{ store.connected ? '实时已连接' : '实时未连接' }}
      </div>
    </header>

    <!--
      全局轻提示。替代 alert：不阻塞界面、自动消失。
      右上角的 ⤵ 把它**收起到右下角工具栏**（只留一个小按钮，带未读数量），
      点那个按钮再展开。
    -->
    <div v-if="!messagesCollapsed" class="messages" role="status" aria-live="polite">
      <div v-if="messages.length" class="messages-bar">
        <span class="messages-title">提示（{{ messages.length }}）</span>
        <button class="messages-collapse" title="收起到右下角" @click="collapseMessages()">⤵</button>
      </div>
      <TransitionGroup name="msg">
        <div v-for="m in messages" :key="m.id" class="msg" :class="m.tone">
          <span class="msg-text">{{ m.text }}</span>
          <button class="msg-close" title="关闭这条" @click="dismissMessage(m.id)">✕</button>
        </div>
      </TransitionGroup>
    </div>

    <!-- 收起后的右下角工具栏按钮（有未读时显示数量） -->
    <button
      v-else
      class="messages-dock"
      :title="messages.length ? `展开提示（${messages.length} 条）` : '展开提示'"
      @click="expandMessages()"
    >
      🔔
      <span v-if="messages.length" class="messages-dock-badge">{{ messages.length }}</span>
    </button>

    <p v-if="store.error" class="app-error">{{ store.error }}</p>

    <main class="app-body">
      <RouterView />
    </main>
  </div>
</template>

<style scoped>
/*
 * 外壳高度锁死在视口内，滚动交给 `.app-body`。
 *
 * ⚠️ 这里必须用 `height` 而不是 `min-height`，否则 `.app-body` 的
 * `overflow: auto` 形同虚设：内容会把 main 撑高（实测 2098px），
 * 实际滚动的是**文档**，于是 `.editor-aside` 的 `position: sticky`
 * 失效（右栏预览跟着页面滚走，而不是吸附）。
 */
.app-shell {
  display: flex;
  flex-direction: column;
  height: 100vh;
  background: var(--bsr-bg);
  color: var(--bsr-fg);
}

.app-header {
  display: flex;
  align-items: center;
  gap: 24px;
  padding: 12px 20px;
  border-bottom: 1px solid var(--bsr-border);
  background: var(--bsr-bg-elevated);
}

.brand {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 15px;
}

.brand-dot {
  width: 10px;
  height: 10px;
  border-radius: 50%;
  background: var(--bsr-accent);
}

.brand-version {
  font-size: 11px;
  color: var(--bsr-muted);
}

.app-nav {
  display: flex;
  gap: 6px;
  margin-left: auto;
}

.app-nav a {
  padding: 6px 14px;
  border-radius: 6px;
  color: var(--bsr-muted);
  text-decoration: none;
  font-size: 13px;
}

.app-nav a.active,
.app-nav a:hover {
  background: var(--bsr-accent-soft);
  color: var(--bsr-fg);
}

.conn {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 12px;
  color: var(--bsr-muted);
}

.conn-dot {
  width: 8px;
  height: 8px;
  border-radius: 50%;
  background: var(--bsr-danger);
}

.conn[data-ok='true'] .conn-dot {
  background: var(--bsr-success);
}

.app-error {
  margin: 0;
  padding: 8px 20px;
  background: color-mix(in srgb, var(--bsr-danger) 18%, transparent);
  color: var(--bsr-danger);
  font-size: 13px;
}

/*
 * 内容区：**唯一的滚动容器**。
 *
 * `min-height: 0` 是必须的——flex 子项默认 `min-height: auto`，不加它
 * 内容会把本元素撑高（而不是产生滚动），`overflow: auto` 就成了摆设，
 * 连带 `.editor-aside` 的 sticky 吸附失效。
 */
.app-body {
  flex: 1;
  min-height: 0;
  padding: 20px;
  overflow: auto;
}

/*
 * 轻提示区域：固定在右上角，不占布局、不阻塞点击（只拦截自身区域）。
 */
.messages {
  position: fixed;
  top: 64px;
  right: 20px;
  z-index: 300;
  display: flex;
  flex-direction: column;
  gap: 8px;
  width: min(360px, calc(100vw - 40px));
  pointer-events: none;
}

/* 提示区抬头：只有存在提示时才显示，右侧是「收起到右下角」按钮 */
.messages-bar {
  pointer-events: auto;
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 0 2px;
  font-size: 11.5px;
  color: var(--bsr-muted);
}

.messages-collapse {
  border: 1px solid var(--bsr-border);
  border-radius: 6px;
  background: var(--bsr-bg-elevated);
  color: var(--bsr-muted);
  font-size: 12px;
  line-height: 1;
  padding: 3px 7px;
  cursor: pointer;
}

.messages-collapse:hover {
  color: var(--bsr-fg);
  border-color: var(--bsr-accent);
}

/*
 * 收起后的右下角工具栏按钮。
 * 固定在右下角、始终可见，未读数量以角标显示。
 */
.messages-dock {
  position: fixed;
  right: 20px;
  bottom: 20px;
  z-index: 300;
  /* 未读角标相对本按钮定位 */
  display: flex;
  align-items: center;
  justify-content: center;
  width: 42px;
  height: 42px;
  border: 1px solid var(--bsr-border);
  border-radius: 50%;
  background: var(--bsr-bg-elevated);
  /* 淡粉阴影：深色阴影在浅粉底上会显脏 */
  box-shadow: 0 6px 18px rgb(255 111 165 / 22%);
  font-size: 17px;
  line-height: 1;
  cursor: pointer;
}

.messages-dock:hover {
  border-color: var(--bsr-accent);
}

.messages-dock-badge {
  position: absolute;
  top: -4px;
  right: -4px;
  min-width: 18px;
  height: 18px;
  padding: 0 4px;
  border-radius: 9px;
  background: var(--bsr-danger);
  color: #fff;
  font-size: 11px;
  line-height: 18px;
  text-align: center;
}

.msg {
  pointer-events: auto;
  display: flex;
  align-items: flex-start;
  gap: 10px;
  padding: 10px 12px;
  border: 1px solid var(--bsr-border);
  border-left-width: 3px;
  border-radius: 8px;
  background: var(--bsr-bg-elevated);
  /* 同上：用粉色阴影而不是黑色 */
  box-shadow: 0 6px 18px rgb(255 111 165 / 18%);
  font-size: 13px;
  line-height: 1.5;
}

.msg-text {
  flex: 1;
  word-break: break-word;
}

.msg-close {
  flex: none;
  border: 0;
  background: transparent;
  color: var(--bsr-muted);
  font-size: 12px;
  line-height: 1.4;
  cursor: pointer;
  padding: 0 2px;
}

.msg-close:hover {
  color: var(--bsr-fg);
}

.msg.ok {
  border-left-color: var(--bsr-success);
}

.msg.err {
  border-left-color: var(--bsr-danger);
}

.msg.warn {
  border-left-color: var(--bsr-warning);
}

.msg.info {
  border-left-color: var(--bsr-accent);
}

.msg-enter-active,
.msg-leave-active {
  transition: opacity 0.18s ease, transform 0.18s ease;
}

.msg-enter-from,
.msg-leave-to {
  opacity: 0;
  transform: translateX(16px);
}
</style>
