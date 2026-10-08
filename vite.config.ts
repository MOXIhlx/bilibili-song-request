import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

/**
 * 把 import.meta.url 转成本地路径。
 *
 * 为什么不用 `node:url` 的 fileURLToPath + @types/node：
 * 前端只需要三个入口的绝对路径，自己转一次可以避免为构建脚本引入 Node 类型依赖，
 * 让 tsconfig 保持单一配置（无需 project references / composite）。
 */
function fromHere(relative: string): string {
  return fromHereUrl(new URL(relative, import.meta.url))
}

/** 把 file:// URL 转成本地文件系统路径（Windows 上会去掉开头的斜杠）。 */
function fromHereUrl(url: URL): string {
  const raw = decodeURIComponent(url.pathname).replace(/^\/([A-Za-z]:)/, '$1')
  return raw
}

/**
 * 前端构建说明
 * ---------------------------------------------------------------------------
 * 一次 `vite build` 会产出三套产物，供两个「外壳」共用：
 *
 *  1) `index.html`     —— Tauri 桌面窗口（Vue Router，含 Dashboard / Panel 路由）
 *  2) `panel.html`     —— OBS 浏览器源页面（无路由、体积极小、默认透明背景）
 *  3) `dashboard.html` —— 浏览器里打开的主播控制台页面
 *
 * 2 与 3 由内嵌 axum 服务器（默认 http://127.0.0.1:17777）提供给 OBS / 浏览器。
 * 之所以拆成独立入口而不是复用 SPA：OBS 浏览器源只需要面板，不需要把整个
 * 控制台代码加载进去；同时独立入口便于将来给面板做零依赖的极致瘦身版本。
 *
 * 输出固定为 `dist/`，每次构建**清空输出目录**。
 *
 * 为什么必须清空（`emptyOutDir: true`）：产物文件名带内容哈希，不清空的话
 * 每次 `npm run build` 都会把上一版的 `assets/*-<旧hash>.js` 留在目录里。
 * 实测连续构建三次后 `dist/assets` 累积到 18 个文件、其中 8 个是死文件——
 * 它们会被 `tauri-build` 一起复制进 `_up_/dist/`，白占体积，也让打包内容难以核对。
 *
 * 早期这里写的是 `false`（理由是"保留 dist 目录本身"），但 vite 本来就会创建
 * 输出目录，这个理由不成立。仓库里没有 `public/`，也没有别的脚本往 dist 写东西，
 * 清空不会丢任何需要保留的文件。
 */
export default defineConfig(({ mode }) => ({
  // 开发期由 vite 直接服务（/），打包后由 Tauri 的 tauri://localhost 或
  // 内嵌 axum 以相对路径加载，因此使用相对 base。
  base: './',
  plugins: [vue()],
  resolve: {
    alias: {
      '@': fromHere('./src'),
    },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    sourcemap: mode !== 'production',
    rollupOptions: {
      input: {
        main: fromHere('./index.html'),
        panel: fromHere('./panel.html'),
        dashboard: fromHere('./dashboard.html'),
      },
      output: {
        entryFileNames: 'assets/[name]-[hash].js',
        chunkFileNames: 'assets/[name]-[hash].js',
        assetFileNames: 'assets/[name]-[hash][extname]',
      },
    },
  },
  server: {
    port: 1420,
    strictPort: true,
    // 开发时前端热更新在 1420，后端 axum 在 17777，这里做同源代理，
    // 让前端代码始终使用相对路径 `/api`、`/ws`。
    proxy: {
      '/api': { target: 'http://127.0.0.1:17777', changeOrigin: true },
      '/ws': { target: 'ws://127.0.0.1:17777', ws: true },
    },
    watch: {
      // src-tauri 的变更由 tauri dev 自己监听，避免 vite 重复触发整页刷新。
      ignored: ['**/src-tauri/**'],
    },
  },
  clearScreen: false,
}))
