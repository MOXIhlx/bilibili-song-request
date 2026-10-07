/**
 * Tauri 桌面窗口入口。
 *
 * 这里挂载的是「完整 SPA」：包含 Dashboard（控制台）与 Panel（面板预览）两个路由，
 * 方便主播在 exe 窗口里直接操作与预览 OBS 面板效果。
 * OBS / 外部浏览器访问的是内嵌 axum 服务器提供的独立页面（/panel、/dashboard）。
 */
import { createApp } from 'vue'
import { createPinia } from 'pinia'
import App from '@/App.vue'
import { router } from '@/router'
import '@/styles/base.css'

const app = createApp(App)
app.use(createPinia())
app.use(router)
app.mount('#app')
