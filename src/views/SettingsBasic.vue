<script setup lang="ts">
/**
 * 设置 · 基础（`/settings`）。
 *
 * ## 这里管什么
 * 服务器地址端口、点歌规则、搜索平台、优先级，以及排障用的链路自测。
 * 它们都是**配一次就不动**的东西，所以和「OBS 面板」「背景图库」并列成
 * 设置页的三个子标签，而不是挤在控制台的日常标签里。
 *
 * ## ⚠️ 与「空闲歌单」页重复的一个字段
 * `rules.idle_switch_policy`（有人点歌时怎么办）在两处都能改：
 * 这里是"完整设置"，空闲歌单页是"顺手就地改"。
 * 两份草稿各自独立、都以 `store.config` 为初值，保存后 store 会刷新，
 * 因此不会互相覆盖——只是改完这边不会实时反映到那边，切页时会重新取。
 */
import { computed, onMounted, ref, watch } from 'vue'
import { getConfig } from '@/api'
import { clonePlain } from '@/composables/panelParams'
import { showMessage } from '@/stores/message'
import { useAppStore } from '@/stores/app'
import NavTabs from '@/components/NavTabs.vue'
import type { Config } from '@/types'

const store = useAppStore()

/** 配置草稿，保存时整体提交。 */
const draft = ref<Config | null>(null)
const draftError = ref<string | null>(null)
const saving = ref(false)
const saved = ref(false)

/** 排障用的弹幕文本。 */
const simulateText = ref('点歌 你还在不在 梁静茹')

/** 重置为已保存的配置。 */
function resetDraft(): void {
  // 必须用 clonePlain 而不是 structuredClone：store.config 是响应式代理，
  // 后者在 WebView2 下会直接抛（本页曾因此永久停在「正在加载配置…」）
  draft.value = store.config ? clonePlain(store.config) : null
}

/**
 * 确保草稿可用。
 *
 * ⚠️ 两条路径都要有：store 已就绪就直接用；否则**自己拉一次**——
 * bootstrap 在 `App.vue` 的 `onMounted` 发起，本页可能先于它完成挂载。
 */
async function ensureDraft(): Promise<void> {
  if (draft.value) return
  if (store.config) {
    resetDraft()
    return
  }
  draftError.value = null
  try {
    const cfg = await getConfig()
    draft.value = clonePlain(cfg)
  } catch (err) {
    draftError.value = (err as Error).message
  }
}

onMounted(() => {
  void ensureDraft()
})

// store.config 稍后到位时补上
watch(() => store.config, () => {
  if (!draft.value) resetDraft()
})

/** 保存。 */
async function saveSettings(): Promise<void> {
  if (!draft.value) return
  saving.value = true
  try {
    await store.updateConfig(draft.value)
    saved.value = !store.error
    if (saved.value) showMessage('ok', '设置已保存')
  } finally {
    saving.value = false
    window.setTimeout(() => (saved.value = false), 1500)
  }
}

/** 是否已就绪。 */
const ready = computed(() => draft.value !== null)
</script>

<template>
  <div class="settings-basic">
    <!--
      统一导航（`components/NavTabs.vue`）：左边是控制台的面板内标签，
      右边是「设置」。设置页自己没有面板内标签，所以 `model-value` 传 null。
      顶栏不再有页级导航——早期两处重复，会出现「在设置页点『基础』跳回控制台」
      这种自相矛盾的链接。
    -->
    <NavTabs :model-value="null" />

    <div class="page-head">
      <h2>基础设置</h2>
      <div class="controls">
        <button class="ghost" :disabled="!ready" @click="resetDraft()">重置</button>
        <button :disabled="!ready || saving" @click="saveSettings()">
          {{ saving ? '保存中…' : saved ? '已保存' : '保存' }}
        </button>
      </div>
    </div>

    <section class="card">
      <p v-if="draftError" class="err">读取配置失败：{{ draftError }}</p>
      <button v-if="draftError" class="ghost" @click="ensureDraft()">重试</button>
      <p v-else-if="!ready" class="empty">正在加载配置…</p>

      <template v-else-if="draft">
        <h4>内嵌服务器</h4>
        <div class="fields">
          <label>监听地址 <input v-model="draft.server.host" /></label>
          <label>端口 <input type="number" v-model.number="draft.server.port" /></label>
        </div>
        <p class="dim">
          改端口后要重启程序才生效。OBS 浏览器源地址会随之变成
          <code>http://&lt;监听地址&gt;:&lt;端口&gt;/panel?style=&lt;样式id&gt;</code>。
        </p>

        <h4>点歌规则</h4>
        <div class="fields">
          <label class="wide">指令正则 <input v-model="draft.rules.command_regex" /></label>
          <label>冷却（秒）<input type="number" v-model.number="draft.rules.cooldown_secs" /></label>
          <label>弹幕点歌上限
            <input type="number" v-model.number="draft.rules.max_queue" />
          </label>
          <label>主播每首补名额
            <input type="number" v-model.number="draft.rules.host_extra_per_play" />
          </label>
          <label class="check">
            <input type="checkbox" v-model="draft.rules.allow_duplicate" /> 允许重复点歌
          </label>
          <label>粉丝牌等级下限
            <input type="number" v-model.number="draft.rules.min_fans_medal_level" />
          </label>
          <label>用户等级下限
            <input type="number" v-model.number="draft.rules.min_user_level" />
          </label>
        </div>

        <h4>有人点歌时（空闲歌单）</h4>
        <div class="fields">
          <label class="wide">切换策略
            <select v-model="draft.rules.idle_switch_policy">
              <option value="immediate">立即播放点的歌曲（中断当前空闲歌曲）</option>
              <option value="after_current">放完当前这首空闲歌曲再播点歌</option>
            </select>
          </label>
        </div>

        <h4>搜索平台</h4>
        <div class="fields">
          <label class="wide">用哪个平台搜索
            <select v-model="draft.rules.search_platform">
              <option value="auto">自动（QQ 优先，必要时用网易云）</option>
              <option value="qq">只用 QQ 音乐</option>
              <option value="netease">只用网易云音乐</option>
            </select>
          </label>
        </div>

        <h4>点歌优先级</h4>
        <div class="fields">
          <label class="wide">搜不到精确原唱时
            <select v-model="draft.rules.pick_policy">
              <option value="prefer_full_length">优先完整时长（推荐）</option>
              <option value="prefer_artist">优先原唱</option>
            </select>
          </label>
        </div>

        <h4>链路自测（排障用）</h4>
        <p class="dim">
          注入一条弹幕文本，走完整的解析 → 搜索 → 入队链路，用来验证程序本身是否正常。
          观众的真实弹幕不受影响。
        </p>
        <div class="fields">
          <label class="wide">弹幕文本
            <input
              v-model="simulateText"
              placeholder="点歌 你还在不在 梁静茹"
              @keyup.enter="store.simulateDanmaku(simulateText)"
            />
          </label>
        </div>
        <div class="controls">
          <button @click="store.simulateDanmaku(simulateText)">注入弹幕</button>
          <button class="ghost" @click="store.simulateDanmaku('点歌 你还在不在', '测试观众')">
            注入非点歌弹幕
          </button>
        </div>
      </template>
    </section>
  </div>
</template>

<style scoped>
.settings-basic {
  display: flex;
  flex-direction: column;
  gap: 14px;
}

.page-head {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: 16px;
  flex-wrap: wrap;
}

.page-head h2 {
  margin: 0;
  font-size: 18px;
}

.card {
  padding: 14px 16px;
  border: 1px solid var(--bsr-border);
  border-radius: 10px;
  background: var(--bsr-bg-elevated);
}

.card h4 {
  margin: 16px 0 0;
  font-size: 13px;
  color: var(--bsr-fg);
}

.card h4:first-of-type {
  margin-top: 0;
}

/*
 * 表单网格用 `auto-fill`：`auto-fit` 会合并空轨道并拉伸剩余轨道，
 * 于是「字段越少每个越宽」（实测 2 个字段时列宽 545px、4 个字段时 264px）。
 */
.fields {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(200px, 1fr));
  gap: 10px 14px;
  margin-top: 10px;
}

.fields label {
  display: flex;
  flex-direction: column;
  gap: 4px;
  font-size: 12px;
  color: var(--bsr-muted);
}

.fields label.check {
  flex-direction: row;
  align-items: center;
  gap: 6px;
}

.fields label.wide {
  grid-column: 1 / -1;
}

.fields input,
.fields select {
  padding: 5px 8px;
  border: 1px solid var(--bsr-border);
  border-radius: 6px;
  background: var(--bsr-bg);
  color: var(--bsr-fg);
  font-size: 13px;
  max-width: 260px;
}

.fields input[type='number'] {
  max-width: 130px;
}

.fields label.check input {
  max-width: none;
  accent-color: var(--bsr-accent);
}

.controls {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
  margin-top: 10px;
}

.controls button {
  padding: 5px 12px;
  border: 1px solid var(--bsr-accent);
  border-radius: 6px;
  background: var(--bsr-accent);
  color: #fff;
  font-size: 13px;
  cursor: pointer;
}

.controls button.ghost {
  border-color: var(--bsr-border);
  background: var(--bsr-bg-elevated);
  color: var(--bsr-fg);
}

.controls button:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}

.dim {
  margin: 8px 0 0;
  font-size: 12px;
  line-height: 1.5;
  color: var(--bsr-muted);
}

.dim code {
  padding: 1px 5px;
  border-radius: 4px;
  background: var(--bsr-accent-soft);
  font-size: 11.5px;
}

.err {
  margin: 0 0 8px;
  color: var(--bsr-danger);
  font-size: 13px;
}

.empty {
  margin: 0;
  color: var(--bsr-muted);
  font-size: 13px;
}
</style>
