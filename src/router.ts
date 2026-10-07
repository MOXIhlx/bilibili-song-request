/**
 * 路由表。
 *
 * 使用 hash 历史：Tauri 打包后页面通过 tauri://localhost/ 加载，
 * 没有服务端 rewrite，hash 模式可以避免刷新 404。
 */
import { createRouter, createWebHashHistory, type RouteRecordRaw } from 'vue-router'
import Dashboard from '@/views/Dashboard.vue'
import Panel from '@/views/Panel.vue'
import Panels from '@/views/Panels.vue'

const routes: RouteRecordRaw[] = [
  { path: '/', redirect: '/dashboard' },
  {
    path: '/dashboard',
    name: 'dashboard',
    component: Dashboard,
    meta: { title: '主播控制台' },
  },
  {
    // 独立的「OBS 面板配置」页：样式 + 四个地址 + 预览。
    // 从控制台拆出来的原因：这些内容挤在「直播与播放」里，
    // 会把日常要看的弹幕/队列区挤下去。
    path: '/panels',
    name: 'panels',
    component: Panels,
    meta: { title: 'OBS 面板配置' },
  },
  {
    path: '/panel',
    name: 'panel',
    component: Panel,
    meta: { title: 'OBS 面板' },
  },
  // ── 专注页（阶段 8）────────────────────────────────────────────────────
  // 同一个组件，靠路径决定渲染哪些区块（见 `resolvePanelPage`）。
  // 拆页的意义：综合面板在 OBS 里放不下，拆分后每页可以把字放大。
  {
    path: '/panel/play',
    name: 'panel-play',
    component: Panel,
    meta: { title: 'OBS 面板 · 播放与队列' },
  },
  {
    path: '/panel/lyrics',
    name: 'panel-lyrics',
    component: Panel,
    meta: { title: 'OBS 面板 · 歌词' },
  },
  {
    path: '/panel/danmaku',
    name: 'panel-danmaku',
    component: Panel,
    meta: { title: 'OBS 面板 · 最近弹幕' },
  },
  { path: '/:pathMatch(.*)*', redirect: '/dashboard' },
]

export const router = createRouter({
  history: createWebHashHistory(),
  routes,
})
