/**
 * 通过 WebView2 的 DevTools Protocol 在应用窗口里求值。
 *
 * 用途：桌面应用里的功能（例如 Tauri 命令）无法用普通 HTTP 触发。
 * 打开 WebView2 远程调试后（环境变量
 * `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9333`），
 * 就能在这里调用 `window.__TAURI_INTERNALS__.invoke(...)` 驱动界面，
 * 把「登录」这类只能点按钮触发的能力自动化验证。
 *
 * 用法：
 *   node scripts/tauri-eval.mjs "<JS 表达式>" [端口]
 *   node scripts/tauri-eval.mjs --list [端口]
 */

const args = process.argv.slice(2)
const port = Number(args.includes('--list') ? (args[1] ?? 9333) : (args[1] ?? 9333))
const expression = args[0]

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

async function findPage() {
  for (let i = 0; i < 40; i += 1) {
    try {
      const res = await fetch(`http://127.0.0.1:${port}/json/list`)
      const list = await res.json()
      const page = list.find((t) => t.type === 'page' && t.webSocketDebuggerUrl)
      if (page) return page
    } catch {
      // 端点还没起来
    }
    await sleep(250)
  }
  throw new Error(`等待 WebView2 调试端点超时（端口 ${port}）。` +
    '请用 WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9333 启动程序。')
}

if (expression === '--list') {
  const res = await fetch(`http://127.0.0.1:${port}/json/list`)
  for (const t of await res.json()) {
    console.log(`[${t.type}] ${t.title}\n    ${t.url}`)
  }
  process.exit(0)
}

if (!expression) {
  console.error('用法: node scripts/tauri-eval.mjs "<JS>" [端口]')
  process.exit(2)
}

const page = await findPage()
console.log(`已连接: ${page.title}`)

const cdp = await new Promise((resolve, reject) => {
  const ws = new WebSocket(page.webSocketDebuggerUrl)
  let nextId = 1
  const pending = new Map()
  ws.addEventListener('open', () =>
    resolve({
      send(method, params = {}) {
        const id = nextId++
        ws.send(JSON.stringify({ id, method, params }))
        return new Promise((res, rej) => {
          pending.set(id, { res, rej })
          setTimeout(() => {
            if (pending.has(id)) {
              pending.delete(id)
              rej(new Error(`${method} 超时`))
            }
          }, 30000)
        })
      },
      close: () => ws.close(),
    }),
  )
  ws.addEventListener('message', (event) => {
    const msg = JSON.parse(event.data)
    if (msg.id && pending.has(msg.id)) {
      const { res, rej } = pending.get(msg.id)
      pending.delete(msg.id)
      if (msg.error) rej(new Error(msg.error.message))
      else res(msg.result)
    }
  })
  ws.addEventListener('error', () => reject(new Error('CDP 连接失败')))
})

await cdp.send('Runtime.enable')
const result = await cdp.send('Runtime.evaluate', {
  expression,
  returnByValue: true,
  awaitPromise: true,
})
if (result.exceptionDetails) {
  console.error('页面异常:', result.exceptionDetails.text, result.exceptionDetails.exception?.description ?? '')
  process.exit(1)
}
console.log(JSON.stringify(result.result.value, null, 2))
cdp.close()
