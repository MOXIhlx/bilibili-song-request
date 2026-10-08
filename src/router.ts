/**
 * 路由表。
 *
 * 使用 hash 历史：Tauri 打包后页面通过 tauri://localhost/ 加载，
 * 没有服务端 rewrite，hash 模式可以避免刷新 404。
 *
 * ## 结构：控制台 / 设置 两个一级入口
 * OBS 面板配置**不再是一级导航**——它是配置，并进「设置」页的子标签。
 * 早期它和控制台并列，导致导航项过多、且用户一进来就看到一堆配置项。
 */
import { createRouter, createWebHashHistory, type RouteRecordRaw } from 'vue-router'
import Dashboard from '@/views/Dashboard.vue'
import Panel from '@/views/Panel.vue'
import ObsPanel from '@/views/ObsPanel.vue'

const routes: RouteRecordRaw[] = [
  { path: '/', redirect: '/dashboard' },
  {
    path: '/dashboard',
    name: 'dashboard',
    component: Dashboard,
    meta: { title: '主播控制台' },
  },
  // ── 设置（子标签）──────────────────────────────────────────────────────
  // 三个子标签共用同一个「设置」导航项，靠路径段区分：
  //   /settings            → 基础（服务器、点歌规则、搜索平台）
  //   /settings/obs        → OBS 面板（样式、面板地址、双栏预览）
  //   /settings/background → 背景图库（网格 + 裁剪编辑器）
  //
  // 「基础」暂时仍由控制台的「设置」标签承载（见 Dashboard.vue），
  // 所以 `/settings` 先跳到控制台设置标签，避免出现两个都能改配置的入口。
  { path: '/settings', redirect: '/dashboard' },
  {
    path: '/settings/obs',
    name: 'settings-obs',
    component: ObsPanel,
    meta: { title: '设置 · OBS 面板' },
  },
  {
    path: '/settings/background',
    name: 'settings-background',
    component: ObsPanel,
    meta: { title: '设置 · 背景图库' },
  },
  // 旧地址兼容：`/panels` 曾是独立导航项，书签与文档里可能都还有
  { path: '/panels', redirect: '/settings/obs' },
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
