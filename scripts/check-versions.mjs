/**
 * 版本号一致性检查（阶段 8）。
 *
 * 三处版本必须一致，否则会出现很难查的问题：
 *   - `package.json`          → 前端 `npm version` 与 Tauri CLI 读取
 *   - `src-tauri/tauri.conf.json` → 安装包版本、`generate_context!` 注入的版本
 *   - `src-tauri/Cargo.toml`  → `env!("CARGO_PKG_VERSION")`（`/health` 与界面展示）
 *
 * 不一致时安装包版本与「关于」里显示的版本会不一样，用户排查问题时会被误导。
 *
 * 用法：
 *   node scripts/check-versions.mjs          # 校验，不一致则退出码 1
 *   node scripts/check-versions.mjs --fix    # 以 package.json 为准同步另外两处
 */
import { readFileSync, writeFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = join(dirname(fileURLToPath(import.meta.url)), '..')
const pkgPath = join(root, 'package.json')
const tauriPath = join(root, 'src-tauri', 'tauri.conf.json')
const cargoPath = join(root, 'src-tauri', 'Cargo.toml')

const read = (path) => readFileSync(path, 'utf8')

/** package.json 的 version 是唯一权威来源。 */
const authoritative = JSON.parse(read(pkgPath)).version
if (!authoritative || !/^\d+\.\d+\.\d+/.test(authoritative)) {
  console.error(`package.json 的 version 看起来不合法：${authoritative}`)
  process.exit(1)
}

/** 从 tauri.conf.json 粗读 version（保留原文件格式，不用 JSON.parse 回写）。 */
function readTauriVersion() {
  const match = /"version"\s*:\s*"([^"]+)"/.exec(read(tauriPath))
  return match ? match[1] : null
}

/** 从 Cargo.toml 的 [package] 段读 version（避免误读依赖的 version）。 */
function readCargoVersion() {
  const lines = read(cargoPath).split(/\r?\n/)
  let inPackage = false
  for (const line of lines) {
    if (/^\[/.test(line)) {
      inPackage = line.trim() === '[package]'
      continue
    }
    if (inPackage) {
      const match = /^version\s*=\s*"([^"]+)"/.exec(line)
      if (match) return match[1]
    }
  }
  return null
}

const found = [
  ['package.json', authoritative, pkgPath],
  ['src-tauri/tauri.conf.json', readTauriVersion(), tauriPath],
  ['src-tauri/Cargo.toml', readCargoVersion(), cargoPath],
]

const mismatched = found.filter(([, version]) => version !== authoritative)

if (mismatched.length === 0) {
  console.log(`版本一致：${authoritative}`)
  process.exit(0)
}

if (!process.argv.includes('--fix')) {
  console.error(`版本不一致（以 package.json 的 ${authoritative} 为准）：`)
  for (const [name, version] of found) {
    console.error(`  ${name.padEnd(28)} ${version ?? '(未找到)'}`)
  }
  console.error('\n执行 `node scripts/check-versions.mjs --fix` 自动同步。')
  process.exit(1)
}

// ── 同步：只替换版本那一行，不动文件其它内容与格式 ──────────────────────────
for (const [name, version, path] of mismatched) {
  const before = read(path)
  const after = before
    .replace(/(^\s*"version"\s*:\s*")[^"]+(")/m, `$1${authoritative}$2`)
    .replace(/(?<=\[package\][\s\S]*?^version\s*=\s*")[^"]+(?=")/m, authoritative)
  if (before === after) {
    console.error(`无法自动同步 ${name}，请手动修改。`)
    process.exit(1)
  }
  writeFileSync(path, after)
  console.log(`已同步 ${name}：${version ?? '(未找到)'} → ${authoritative}`)
}
