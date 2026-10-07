/**
 * 独立面板入口，由内嵌 axum 以 `/panel`（及 `/assets/*`）提供给 OBS 浏览器源。
 *
 * 与 Tauri 窗口里的 Panel.vue 是同一个组件，区别只在于：
 *  - 没有 vue-router，URL 参数直接从 location.search 读取
 *  - body 强制透明背景
 */
import { createApp } from 'vue'
import { createPinia } from 'pinia'
import Panel from '@/views/Panel.vue'
import { useAppStore } from '@/stores/app'
import '@/styles/base.css'

document.body.classList.add('bsr-panel-body')

const app = createApp(Panel)
app.use(createPinia())
app.mount('#panel')

// 挂载后拉取状态并建立实时连接。
const store = useAppStore()
void store.bootstrap()
