/// <reference types="vite/client" />

/**
 * Tauri 启动时注入的内嵌服务器地址；浏览器 / OBS 访问时不存在，走默认值。
 *
 * ⚠️ 三个字段都要声明：`main.rs`（Tauri 窗口）注入 `host`/`port`，
 * `server/templates.rs`（HTTP 服务）现在同时注入 `base` 与 `host`/`port`。
 * 前端优先用 `base`。
 */
interface Window {
  __BSR_SERVER__?: {
    /** 内嵌服务器基地址，如 `http://127.0.0.1:17777`（推荐用它）。 */
    base?: string
    host?: string
    port?: number
  }
}

declare module '*.vue' {
  import type { DefineComponent } from 'vue'
  const component: DefineComponent<Record<string, unknown>, Record<string, unknown>, unknown>
  export default component
}
