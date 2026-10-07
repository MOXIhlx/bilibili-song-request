/**
 * 独立控制台入口，由内嵌 axum 以 `/dashboard` 提供给普通浏览器
 * （例如主播在手机或第二台电脑上打开，不影响 exe 窗口）。
 */
import { createApp } from 'vue'
import { createPinia } from 'pinia'
import Dashboard from '@/views/Dashboard.vue'
import { useAppStore } from '@/stores/app'
import '@/styles/base.css'

document.body.classList.add('bsr-dashboard-body')

const app = createApp(Dashboard)
app.use(createPinia())
app.mount('#app')

const store = useAppStore()
void store.bootstrap()
