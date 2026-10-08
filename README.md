# 弹幕点歌机（bilibili-song-request）

Windows 本地桌面应用：通过 B 站直播身份码接收实时弹幕，识别 `点歌 歌名 歌手` 指令，
管理点歌队列，调用本地 **mpv** 播放音乐，并内嵌 HTTP/WebSocket 服务器，
把可定制点歌面板以 URL 形式提供给 **OBS 浏览器源**。

```
B站直播服务器 ──WebSocket弹幕流──▶ 本地 exe（Tauri 2 + Rust）
                                      │
                        ┌─────────────┼──────────────────────┐
                        ▼             ▼                      ▼
                  弹幕监听模块   指令解析 & 队列      音乐平台适配器
                                      │                      │ 音频URL
                                      ▼                      ▼
                                内嵌 axum 服务器        mpv 子进程（JSON IPC）
                                127.0.0.1:17777              │ 系统音频输出
                                      │                      ▼
                    /panel ──▶ OBS 浏览器源（只显示，不出声）  桌面音频采集
```

**关键原则**：音频由 mpv 直接输出到系统音频设备，OBS 只显示面板。
这样可避免 OBS 浏览器源后台节流导致音乐中断。

---

## 1. 当前进度

| 阶段 | 内容 | 状态 |
|------|------|------|
| 1 | 项目骨架：Tauri 2 + Vue 3 + Vite、axum `/health` `/ws`、Dashboard / Panel 路由 | ✅ 完成 |
| 2 | 状态推送：`AppState` + WS 广播（阶段 1 已一并实现） | ✅ 完成 |
| 3 | B 站身份码接入、弹幕解析、断线重连、控制台连接界面 | ✅ 完成 |
| 4 | 点歌指令 → 队列（冷却 / 上限 / 去重 / 等级门槛、热更新规则、请求记录） | ✅ 完成 |
| 5 | 音乐平台适配（网易云搜索 / 播放地址 / 歌词、Cookie 凭据库、内嵌登录、异步曲目解析） | ✅ 完成 |
| 6 | mpv 集成（sidecar 定位、播放控制、播完自动下一首、队列持久化） | ✅ 完成 |
| 7 | OBS 面板完善（歌词滚动、动画、布局、URL 参数热更新） | ✅ 完成 |
| 8 | 打包与文档（NSIS 安装包、图标、版本一致性、mpv sidecar 脚本、便携模式、README） | ✅ 完成 |

---

## 1.1 快速开始（只想用，不开发）

### 安装

1. 下载并运行安装包（`bilibili-song-request_<版本>_x64-setup.exe`，约 36 MB），按提示安装。
   安装程序默认只装当前用户，不需要管理员权限。
   **mpv（播放引擎）已包含在安装包里**，不需要单独安装。
2. 安装 **WebView2 运行时**：Windows 11 自带；Windows 10 若缺失，安装程序会提示，
   也可自行下载（Microsoft Edge WebView2 Runtime）。

### 第一次使用

1. 启动程序，会出现「弹幕点歌机」窗口（控制台）。
2. **填身份码**：打开 B 站 [直播开放平台](https://open-live.bilibili.com/) 申请，
   拿到 `app_id` / `access_key_id` / `access_key_secret` / `身份码`，
   填进控制台的「B 站弹幕连接」卡片，点「保存并连接」。连接成功后状态变绿。
3. **登录音乐平台**（可选）：不登录只能播免费曲目；登录后可播版权曲。
   在「音乐平台」卡片点「登录」，在弹出的窗口里扫码/登录即可。
4. **在 OBS 里加面板**：来源 → `+` → 浏览器 → URL 填

   ```text
   http://127.0.0.1:17777/panel?layout=wide&bg=transparent&theme=dark&limit=6
   ```

   宽高建议 800 × 720，并**勾选「透明背景」**。

5. **让 OBS 能听到声音**：面板不出声是设计如此。音频由 mpv 直接输出到系统音频设备，
   请在 OBS 添加「音频输入采集 → 桌面音频」（或把 mpv 输出路由到虚拟声卡后单独采集）。

6. 让观众发送 `点歌 歌名 歌手`（例如 `点歌 晴天 周杰伦`）测试整条链路。
   没有身份码时，可以在控制台用「链路自测」注入一条模拟弹幕来验证队列与面板。

### 配置文件放在哪

默认在 `%APPDATA%\bilibili-song-request\`：

```text
config.json          应用配置（端口、弹幕凭据、点歌规则、面板样式）
queue.json           待播放队列（重启后恢复）
logs\app.log         运行日志（排查问题先看这里）
secrets\             仅在系统凭据库不可用时才出现的降级凭据文件
```

**便携模式**：设置环境变量 `BSR_CONFIG_DIR` 指向任意目录（例如 U 盘里的 `.\data`），
配置/日志/队列/密钥都会放进去，做到「整个文件夹拷走就能用」：

```powershell
$env:BSR_CONFIG_DIR = "D:\danmaku-data"
.\bilibili-song-request.exe
```

---

## 2. 环境要求

> 📖 完整的安装步骤、逐条自检命令、常见报错对照表见
> **[docs/安装环境.md](docs/安装环境.md)**（含「只想用」与「从源码构建」两条路径）。
> 下面只是速览。

| 依赖 | 版本 | 说明 |
|------|------|------|
| Windows | 10 / 11 x64 | 其他平台理论上可编译，但未验证 |
| Node.js | ≥ 18（推荐 20+） | 前端构建 |
| Rust | stable ≥ 1.77 | 后端 |
| MSVC 生成工具 | VS 2022 Build Tools | Rust 的 `link.exe` 来源 |
| WebView2 Runtime | 任意版本 | Win11 自带；Win10 需安装 |
| mpv | ≥ 0.36 | 阶段 6 才需要；打包时作为 sidecar |

安装 Rust（Windows）：

```powershell
winget install --id Rustlang.Rustup -e
# 重新打开终端后
rustc -V; cargo -V
```

若缺少 C++ 链接器，`rustup` 会提示安装：
`winget install --id Microsoft.VisualStudio.2022.BuildTools -e --override "--wait --quiet --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"`

---

## 3. 安装依赖并构建

```powershell
cd bilibili-song-request

# 前端依赖
npm install

# 前端产物（同时产出 index.html / panel.html / dashboard.html）
npm run build

# 后端编译检查
cd src-tauri
cargo check
cargo test
```

### 运行

| 目标 | 命令 | 说明 |
|------|------|------|
| 桌面应用（开发，热更新） | `npm run tauri:dev` | 同时拉起 vite(1420) 与 Tauri 窗口 |
| 只跑后端 + 网页面板（开发） | `cargo run`（在 `src-tauri/`） | 内嵌服务器起在 `127.0.0.1:17777`，前端需 `npm run build` 或设置 `BSR_DIST_DIR` |
| 打包 exe 与安装包 | `npm run tauri:build` | 产物在 `src-tauri/target/release/bundle/` |
| 只编译 exe（不出安装包） | `cargo build --release`（在 `src-tauri/`） | 产物 `src-tauri/target/release/bilibili-song-request.exe` |

### 版本号约定

三处版本必须一致：`package.json`、`src-tauri/tauri.conf.json`、`src-tauri/Cargo.toml`。
不一致会导致「安装包版本」与「程序内 `/health` 显示的版本」对不上。`npm run build`
会自动校验（不一致直接失败），也可以手动：

```powershell
npm run check:versions    # 校验
npm run fix:versions      # 以 package.json 为准自动同步另两处
```

启动后日志会打印：

```text
面板地址：http://127.0.0.1:17777/panel
控制台地址：http://127.0.0.1:17777/dashboard
```

日志文件：`%APPDATA%\bilibili-song-request\logs\app.log`（可用环境变量 `BSR_LOG=debug` 调整级别）。

---

## 4. 配置文件

位置：`%APPDATA%\bilibili-song-request\config.json`（首次启动自动生成）

```json
{
  "server": { "host": "127.0.0.1", "port": 17777 },
  "bilibili": {
    "app_id": "",
    "access_key_id": "",
    "access_key_secret": "",
    "code": "",
    "auto_connect": false
  },
  "rules": {
    "command_regex": "^点歌\\s+(.+?)(?:\\s+(.+))?$",
    "cooldown_secs": 30,
    "max_queue": 20,
    "allow_duplicate": false,
    "min_fans_medal_level": 0,
    "min_user_level": 0
  },
  "panel": {
    "theme": "dark",
    "bg": "transparent",
    "color": "#7dd3fc",
    "font_size": 16,
    "scale": 1.0,
    "limit": 8,
    "show_lyrics": true,
    "layout": "list"
  }
}
```

> 安全约定：敏感信息（B 站 `access_key_secret`、音乐平台 Cookie）计划存入
> Windows 凭据管理器（keyring），不写入 `config.json`。**当前阶段 3 仍写入配置文件**，
> keyring 迁移在阶段 5 与音乐平台 Cookie 一起做。

---

## 4.1 配置 B 站身份码（阶段 3）

四个字段来自**两个不同的地方**，很容易搞混，先看清楚再填：

| 字段 | 是什么 | 从哪里拿 |
|------|--------|----------|
| `access_key_id` | 你的**开发者凭证 ID**（= 官方文档里的 `client_id`） | 开放平台注册**个人开发者**认证后获得 |
| `access_key_secret` | 开发者**密钥**（= `app_secret`），用于给请求签名 | 同上，**只在创建时显示一次**，注意保存 |
| `app_id` | **项目 ID**，是你在平台上创建的「项目/应用」编号，**不是用户名、不是你 B 站账号**，是个纯数字 | 创作者服务中心 → **创建项目** → 得到项目 ID |
| 身份码 `code` | **主播身份码**，随机串，形如 `abcd-efgh-ijkl-mnop`，绑定到你的直播间，一次性、有时效 | 直播中心 / 开放平台的「身份码」页面生成 |

用一句话概括三者的分工：
**开发者密钥证明「你是谁」，`app_id` 说明「用哪个项目」，身份码说明「连哪个直播间」。**

步骤：

1. 打开 [B 站直播开放平台](https://open-live.bilibili.com/)，用主播账号登录，
   注册个人开发者认证 → 拿到 `access_key_id` / `access_key_secret`。
2. 进入创作者服务中心 → **创建项目** → 记下 **项目 ID（即 `app_id`）**。
3. 在「身份码」页面生成身份码（有有效期，过期需重新生成）。
4. 启动本程序 → 控制台「直播与播放」→ **B 站弹幕连接** 卡片，填入四项后点「保存并连接」。
5. 「状态」变为 **已连接** 后，观众发送的弹幕会实时出现在「最近弹幕」列表里，
   并同时通过 `/ws` 推送到 OBS 面板。

也可以直接编辑 `config.json` 的 `bilibili` 段并设置 `auto_connect: true`，启动即自动连接。

### 常见连接问题

| 现象 | 原因与处理 |
|------|-----------|
| 状态一直「重连中…」，最近错误含 `code=10001` 之类 | 身份码无效/过期，或 app_id 与密钥不匹配；重新生成身份码 |
| `缺少 app_id / access_key_id / ...` | 四个字段没填全，错误信息会指出缺哪一项 |
| **`接口返回空响应（HTTP 405）`** | 曾经常见，现已修好（接口域名错误）。若再出现，说明请求没到业务接口，被网关挡了：换网络（手机热点）、检查代理/VPN |
| `版本异常 code=4006` | 签名版本必须是 `1.0`（直播接口），不是开放平台通用的 `2.0` |
| `鉴权失败` / 连上就断（`Connection reset`） | 鉴权/心跳帧必须是**二进制协议包**（16 字节头 + op 码），不能发文本帧 |
| `响应里没有长连信息`（`wss_link`/`auth_body`） | 接口返回结构变了。程序同时支持官方文档的 `data.websocket_info.{wss_link,auth_body}` 与早期扁平的 `data.{wss_link,auth_body}`；日志里有原始报文片段 |
| 同一个身份码想开多个客户端 | 平台限制最多 5 个连接；本程序内部**只建 1 个**连接，面板与控制台共享 |
| 签名相关问题（`code=4002` 签名异常 / `4005` method 异常 / `4006` 版本异常） | 签名按官方规范生成：**6 行**待签名串（`x-bili-accesskeyid` / `content-md5` / `signature-method` / `signature-nonce` / `signature-version` / `timestamp`，**字典序**、`\n` 分隔、末行无换行），`x-bili-signature-version` 取 `2.0`。可用下面的工具逐字节核对 |

### 连接问题排查（接口与协议细节）

实现这套连接时踩了不少坑，记录下来避免以后改回去：

| 项 | 正确做法 | 踩过的坑 |
|----|---------|---------|
| 接口域名 | `https://live-open.biliapi.com` | ⚠️ **不是**文档站 `open-live.bilibili.com`！对文档站发 POST 会返回 `HTTP 405`（空响应体），看起来像签名/参数错误，其实是那个域名不接受 POST。来源：官方 demo `ws.py` 里 `host="https://live-open.biliapi.com"` |
| 签名版本 | `x-bili-signature-version: 1.0` | 开放平台**通用**文档说「如无单独说明取 2.0」，但直播这套属于"单独说明"：传 2.0 会返回 `{"code":4006,"message":"版本异常"}` |
| 待签名串 | 6 行、**字典序**、`\n` 分隔、末行无换行 | 早期只拼了 4 行（漏掉 `signature-method` / `signature-version`），签名必然错 |
| 鉴权帧 | **二进制协议包**：`packetLen(4B) \| headerLen(2B=16) \| ver(2B=0) \| op(4B=7) \| seq(4B=0) \| auth_body`，全部大端 | 早期把 `auth_body` 当**文本帧**直接发，服务端立刻 reset：`WebSocket protocol error: Connection reset without closing handshake` |
| 心跳帧 | 同结构，`op=2`，body 为空 | 早期发的是 Web 端协议的 `{"cmd":"HEARTBEAT"}` 文本帧，同样会被断开 |
| 鉴权回复 | body 是 `{"code":0}`，**没有 `cmd` 字段**，要按 op=OP_AUTH_REPLY(8) + code 判定 | 按 `cmd` 分派会永远匹配不上，状态卡在「正在连接」 |
| 弹幕推送 | `op=5`(OP_MESSAGE)；`ver=2` 时 body 是 **zlib 压缩**，解压后可能含多个子包，要递归解包 | 早期当纯文本处理，ver=2 的包全部解析失败 |

官方参考：互动玩法文档 <https://open-live.bilibili.com/document/849b924b-b421-8586-3e5e-765a72ec3840>
（页面里的 `demo-python.zip` / `demo-go.zip` 就是权威实现，遇到问题先下它对照）。

### 自查工具：打印实际请求内容

连接失败时，先用这个**离线**工具看清程序到底发了什么（不联网，安全）：

```powershell
cd src-tauri
cargo run --example bili_start_probe -- --dry-run <app_id> <access_key_id> <access_key_secret> <身份码>
```

它会打印请求体、全部请求头、以及**还原出来的待签名串**，可以和 B 站官方的
[签名验证工具](https://bilibili.apifox.cn/doc-885734) 逐行对比。
去掉 `--dry-run` 就会真的发一次请求，把 HTTP 状态与响应体原样打出来——判断
「是请求构造问题还是网络问题」时非常直接。


### 音乐平台相关

| 现象 | 原因与处理 |
|------|-----------|
| 搜索报「触发网易云风控（操作频繁）」 | 请求过密被限流（错误码 `405`）。等待 1~2 分钟再试；程序已内置 300ms 最小请求间隔，正常点歌频率不会触发 |
| 搜索报「接口已变化」 | 非官方接口调整了返回结构。日志里有原始响应片段，按需更新 `music/netease.rs` 的解析字段 |
| 「未匹配：搜索失败…」 | 该曲目确实搜不到，或当时被限流。歌名+歌手一起发能显著提高命中率 |
| 取播放地址报「该歌曲需要登录或受版权限制」 | 版权/会员曲目：先在音乐平台卡片登录，再重试 |
| 登录窗口登录后状态没变 | 程序每 0.8 秒轮询一次登录态；若超过 5 分钟未检测到会自动关窗并提示取消。也可用「手动粘贴 Cookie」 |
| 提示「已保存到受限权限的本地文件（明文）」 | 系统凭据服务不可用，已回退到本地文件。若在意明文，请修复系统凭据服务后重新登录 |

### 点歌搜索与选择逻辑

点歌不是「取搜索第一条」，而是有一套本地打分与跨平台策略：

1. **解析关键词**：`点歌 歌名 歌手` → 关键词 `歌名 歌手`（无歌手则只用歌名）；
2. **先搜默认平台**（默认 **QQ 音乐**），对候选做本地打分：
   - 歌名精确 +100；去掉版本后缀后一致 +70；包含 +45；完全不像 −80；
   - 歌手命中 +80；部分命中 +40；**指定了歌手却不符 −60**；
   - 歌手字段里堆了额外署名（如 `周杰伦. / 街道办GDC/欧阳耀莹.`）每个 −25，上限 −45；
   - 标题含「伴奏/翻唱/深情版/正式版/DJ/铃声…」等特征会扣分。
   这一步是为了解决「点原唱却放翻唱」——平台返回的第一条常常是翻唱或改编版。
3. **精确命中就直接用**（歌名无版本后缀 且 歌手对得上），省掉第二次请求；
4. **否则再搜另一个平台**，两边候选合起来按分数取最高。

### 点歌优先级（设置项）

各平台版权策略不同，会出现两难。实测案例：点周杰伦《青花瓷》时

- 非会员的 **QQ 音乐**只给 **95 秒试听**；
- **网易云**上该曲原唱没有可播条目，只有 `青花瓷 — Jay`（201 秒）这类翻唱。

设置页「点歌优先级」里二选一（保存后**立即生效**，无需重启）：

| 选项 | 行为 | 代价 |
|------|------|------|
| **优先完整时长**（默认） | 跳过试听片段，改用另一平台的长版本 | 歌手可能是翻唱 |
| **优先原唱** | 直接用原平台的试听片段 | 只能听 95 秒 |

选了「优先完整时长」时，程序会在另一平台的候选里**按分数逐个尝试**，
直到找到标称时长 ≥ 120 秒的那条——因为实测排第一的候选本身就是 95 秒片段，
而完整版排在后面（且歌手名不同、分数偏低），只取第一名会错过它。

开 QQ 音乐会员后 QQ 会直接给完整版，两者不再冲突，默认选项即可。

### 点歌队列的优先级规则

弹幕点歌与主播点歌是**两套配额**，规则如下（均可在设置页调整）：

| 项 | 默认 | 说明 |
|----|------|------|
| 弹幕点歌上限 | 7 首 | 只统计**观众弹幕**来源的待播曲目，0 = 不限 |
| 主播每首补名额 | 2 个 | 每播完一首**主播点歌**，给弹幕补充 2 个可点名额 |
| 优先级 | — | 主播通过点歌机加的歌**插到所有弹幕点歌之前**；更早加的主播歌仍排在前 |

「队列满了之后会怎样」的完整例子：

```text
上限 7、主播每首补 2

① 弹幕点满 7 首   → 第 8 首被拒（原因 queue_full）
② 主播加 2 首     → 不受上限约束，且插到队首
   队列：主播A 主播B 弹1..弹7
③ 主播A 播完      → 名额 7 → 9，弹幕可以再点
④ 弹幕再点一首    → 成功，但排在剩余的「主播B」之后
   队列：主播B 弹1..弹7 弹8
```

名额增长有**上界**：`上限 + 已播主播曲目数 × 每首补充`。
没有这个上界的话，主播反复重播同一首也会一直加名额，时间一长限制就失效了。

> 实现要点：**「已用多少」不存字段，直接数队列里 `priority=danmaku` 的条目**。
> 早期用「入队 +1 / 离队 −1」的计数器，结果离队入口太多（手动删除、取出播放、
> 加载失败、跳过）而重复扣减，实测出现「队列都空了，弹幕还是点不进去」。

### 播放历史与「上一首」

队列之外单独维护一份**播放历史**（最多 30 首，随 `queue.json` 一起落盘）：

- **上一首**：从历史里取回最近播过的那首（真正换歌）；
  历史为空时返回 409，界面提示「没有上一首」而不是静默无反应；
- **重头播放**：把当前这首的进度拖回 0 并继续（不换歌）。

⚠️ 这两个操作都会让 mpv 发一次 `end-file`（reason=stop）。
早期实现把它当成「播完了」，于是自动推进立刻把刚播起来的那首顶掉——
实测现象是「点重头播放/上一首之后，歌被换成了队列里的下一首，甚至变成空」。
现在 seek 前会置一个标记，事件循环丢弃紧随其后的那一个 `end-file`。

### 点歌日志

控制台「点歌日志」标签页记录**每一次**点歌尝试：

| 字段 | 说明 |
|------|------|
| 时间 | 请求发生时间 |
| 点歌人 | 昵称（含 UID） |
| 歌名 / 歌手 | 观众点的那首（解析前的原始文本） |
| 结果 | 成功 / 失败 |
| 拒绝原因 | 冷却中 / 重复点歌 / 队列已满 / 粉丝牌不足 / 等级不足 |

- 最多保留 **2000 条**，可按「结果」「点歌人」筛选，可**导出 CSV**（带 BOM，Excel 不乱码）；
- 完整日志走 `GET /api/requests/log`（支持 `outcome` / `user` / `limit`）；
- ⚠️ `AppState.stats.recent` 只带最近 **50** 条：它会随每次状态变化经 `/ws`
  全量广播，塞进 2000 条会让每条消息膨胀几十倍。

### OBS 面板：多地址拆分

综合面板在 OBS 里常常放不下、字号只能调小，所以提供几个**专注页**
（只渲染自己那块内容，因此字号可以放大）：

| 地址 | 内容 | 相对字号 |
|------|------|---------|
| `/panel` | 综合：正在播放 + 队列 + 歌词（与以前一致） | 基准 |
| `/panel/play` | 进度条 + 正在播放 + 点歌队列 | 标题 ×1.5、队列 ×1.25 |
| `/panel/lyrics` | 只显示歌词 | ×1.55 |
| `/panel/danmaku` | 只显示最近弹幕 | ×1.5 |

四页共用同一套样式参数（`theme`/`bg`/`color`/`fontSize`/`scale`/`limit`/`layout`）。

**这些配置集中在独立的「OBS 面板」页面**（`/panels`，桌面窗口顶部导航第二项）：
默认样式、四个地址（可逐条或一次性复制）、地址参数行、
每个专注页单独的字号倍率，以及**内嵌实时预览**。

#### 地址参数（可增删的行，不是写死的复选框）

「地址参数」区每一行 = 一个 URL 参数：

```
[参数名 ▼]  [取值 ▼ / 自定义输入]  [×]
+ 添加参数
```

- **参数名**可选：`fontSize` / `color` / `fg` / `surface` / `track` / `limit` /
  `scale` / `layout` / `theme` / `bg` / `showLyrics` / `bgImage`
- **取值**：有预设的给下拉（第一项固定「默认」= 不写进地址，回落到默认样式），
  下拉最后一项是**「自定义…」**，切到输入框后可填带透明度的 8 位色值（如 `#000000aa`）
- 改完**地址实时重算**，预览跟着变；以后加新参数只需往 `PARAM_SPECS` 加一条
- `fontSize` 会乘以该面板自己的**字号倍率**（综合面板要小、歌词页可以大）

#### 要使用哪些面板

取消勾选的面板**不出现在地址列表里**，也不生成地址——只想要歌词和播放队列时，
另外两个干脆不出现，OBS 侧也只建对应的源。预览只渲染选中的那个，
且**正在预览的面板被关闭时会自动切到第一个仍启用的**。

#### 颜色与背景

- **默认只上色两处**：字体（`fg`，留空跟随主题）与进度条（`color`）。
  卡片底色 `surface`、进度条轨道 `track` **默认全透明**——
  这就是「OBS 设了透明背景却还有一层灰底」的来源（旧版写死
  `color-mix(fg 8%)`），现在可以完全关掉。
- **背景图**：在「背景图」区上传，图片存到程序配置目录，
  面板通过 `/bg/文件名` 引用（OBS 会拦 `file://`，所以不能让用户填本地路径）。
  每个面板地址可用不同背景图（`bgImage` 参数）。

> 桌面窗口的顶部导航只有「控制台」与「OBS 面板」两项。
> 四个面板地址本身**不是**导航页——它们是给 OBS 浏览器源用的，
> 在「OBS 面板」页里预览与复制即可。

> 为什么独立成页：这些内容原本挤在控制台的「直播与播放」里，
> 会把日常要看的弹幕/队列区挤下去。现在控制台只留一个入口链接。

两个容易踩的点：

1. 桌面窗口用 hash 路由（`#/panel/lyrics`），而 OBS 直连是真实路径
   （`/panel/lyrics`），所以路径解析要同时看 hash 与 pathname；
   后端也必须**显式注册**这三个子路径，否则会落到 fallback 的 404。
2. ⚠️ **不要对 `store.config` 用 `structuredClone`**：
   它是 Vue 的响应式代理，WebView2 下会抛
   `Failed to execute 'structuredClone' on 'Window': #<Object> could not be cloned.`
   后果很隐蔽——`draft` 永远是 null，界面**永久停在「正在加载配置…」**。
   统一用 `clonePlain()`（JSON 深拷贝，对代理透明）。
   这个坑在设置页与 OBS 面板页各踩过一次。

### 搜索平台（阶段 9，设置页可选）

之前**搜不到理想结果时一定会去另一个平台找**，无法固定只用一个平台。
现在设置页有「搜索平台」三选一：

| 选项 | 行为 |
|------|------|
| **自动**（默认） | 先搜 QQ；没有「歌名 + 歌手」都对的精确命中时再搜网易云，两边候选取综合最高分 |
| **只用 QQ 音乐** | 搜不到就报搜不到，**绝不换平台** |
| **只用网易云** | 同上，绝不换到 QQ |

适用场景：开了 QQ 会员想固定听 QQ 原唱 → 选「只用 QQ」；
QQ 那边只有 95 秒试听、宁可听网易云完整版 → 选「只用网易云」。
保存后**立即生效**。

> 为什么做成三选一而不是「默认平台 + 是否跨平台」两个字段：
> 两个字段能组合出 4 种状态，很容易配出自相矛盾的组合，
> 而且界面上要解释两个下拉框的关系。

### 点歌黑名单（阶段 9）

控制台「黑名单」标签页，按**歌名 + 歌手**匹配：

| 条目 | 命中范围 |
|------|---------|
| 歌名 + 歌手都有 | 两者都匹配才命中（避免误伤同名歌） |
| 只有歌名 | 拉黑这首歌的**所有版本** |

- 命中的歌：**弹幕直接点不了**（不搜索、不入队，也不占弹幕名额），
  拒绝原因是 `blacklisted`，日志里能看到「《晴天 - 周杰伦》已被拉黑」
- **主播无视黑名单**：点歌机可以照常点这些歌
- 队在列表里每首歌都有「拉黑」按钮，从队列直接拉黑最顺手
- 接口：`GET/POST/DELETE /api/blacklist`

### 主播权限（阶段 9）

通过**点歌机**（控制台「链路自测」注入、点歌机按钮）点歌时拥有最高权限：

| 限制 | 观众弹幕 | 主播 |
|------|---------|------|
| 粉丝牌 / 用户等级门槛 | 受限 | **无视** |
| 点歌冷却 | 受限（默认 30 秒） | **无视** |
| 弹幕点歌名额 | 受限 | **无视** |
| 重复点歌 | 受限 | **无视** |
| 黑名单 | 受限 | **无视** |
| 入队位置 | 队尾 | **插到所有弹幕之前** |

- 点歌机注入的弹幕**署名统一为「主播」**
- `/api/bilibili/simulate` 默认 `host: true`；
  想模拟真实观众（验证冷却/黑名单是否生效）要显式传 `"host": false`
- ⚠️ **冷却的 `0` 会被启动时迁移恢复成 30 秒**：
  `0` 语义上是「不限冷却」，但开发验证期间容易忘了改回来，
  结果弹幕能无限刷歌。真想关掉就在设置里改成 0 再保存一次
  （迁移只在启动时跑）

### QQ 音乐歌词（阶段 9）

之前 QQ 来源的歌**一定没有歌词**（`get_lyrics` 是 `Unimplemented`），
不是 bug 而是没实现。现已补上：

- 接口 `c.y.qq.com/lyric/fcgi-bin/fcg_query_lyric_new.fcg`，
  ⚠️ **必须带 `Referer: https://y.qq.com/`**，否则 403 / 返回 HTML 错误页
- 返回的 `lyric` / `trans` 是 base64，解开后合并（面板按时间轴渲染，翻译会跟在原词后）
- 实测：青花瓷 988 字符 / 49 行，夜曲 1463 字符 / 76 行
- 纯音乐或无版权 → 返回**空歌词**而不是报错，面板显示「（暂无歌词）」

### 空闲歌单（阶段 10a）

直播空档（观众还没来、点歌队列播完）不想让直播间安静，就预先准备一份歌单：

| | 点歌队列 `queue` | 空闲歌单 `idle` |
|---|---|---|
| 来源 | 弹幕 / 主播点歌机 | 主播自己准备 |
| 名额限制 | 受弹幕上限约束 | **不受限制** |
| 播放模式 | `play_mode` | `idle_mode`（**独立**） |
| 优先级 | **高于**空闲歌单 | 仅在点歌队列空时播 |

控制台「空闲歌单」标签页：

- **播放模式**四选一：顺序播放 / 列表循环 / 单曲循环 / 随机播放
  （与点歌队列的模式独立：点歌用「顺序」符合"先来先唱"，空闲歌单常设成随机）
- 每首歌可单独**选择播放**；点歌队列里的歌也能一键「→ 空闲」
- **启动时**若没有待播内容，自动用空闲歌单开台
- **有人点歌时**两种策略（设置里选）：
  - `immediate`（默认）立即播放点的歌曲，中断当前空闲歌曲
  - `after_current` 放完当前这首空闲歌曲再播点歌

> 随机播放会**记住上一首**并避开它——否则小歌单里很容易连着放同一首。

#### 这一版顺带修掉三个既有 bug

做 10a 时踩出来的，都是**之前就存在**、只是没有触发条件：

1. **`end-file` 的 `stop` 被当成「播完」**
   `loadfile` 换文件时 mpv 会为**上一个文件**发一次 `end-file(reason=stop)`，
   早期代码把它当自然播完，于是每次换歌都**额外**推进一次队列——
   表现为「刚切到点歌的《晴天》，立刻被下一首顶替」。
   现在只认 `eof`（自然播完），`stop`/`error` 一律忽略。
   （这是同一个坑的第二次：第一次是 seek 导致「重头播放」被顶掉。）

2. **解析等待只看队列**
   `advance()` 是**先 `take_from_queue` 再 `load_and_play`**，
   而 `resolve_play_url` 却去队列里等解析结果——条目已经搬走了，
   于是报「条目已从队列移除」。现在它同时认**当前播放项**；
   解析器回写也同时覆盖 队列 / 空闲歌单 / 当前播放项。

3. **`is_really_idle()` 误判「刚发起加载」**
   `loadfile` 是异步的，mpv 需要一点时间才让 `time-pos` 有值。
   这段时间里旧逻辑会把刚开始加载的那首当成「过期状态」清掉。
   现在 `loadfile` 之后有 **2 秒宽限期**，期间不判定为空闲。

### 从收藏歌单导入（阶段 10b）

空闲歌单可以**从账号里的收藏歌单导入**，不用一首首手加：
在「空闲歌单」页选平台 → 读取收藏歌单 → 对某个歌单点「添加导入」或「覆盖导入」。

| 平台 | 接口 | 实测结果 |
|------|------|---------|
| 网易云 | `GET /api/user/playlist` + `/api/v6/playlist/detail` | 39 个歌单，最大 511 首 |
| QQ 音乐 | `musicu.fcg` module `music.musicasset.PlaylistBaseRead` / `GetPlaylistByUin` | 6 个歌单，「我喜欢」529 首 |

两种导入方式：**添加**（追加到现有空闲歌单）与**覆盖**（先清空），
这就是需求里的「添加或者覆盖到空闲歌单」。

#### 排查过程中踩到的四个坑

1. **QQ 歌单的 module 名网上流传的都是错的**
   `music.web_srf_diss.FcgiGetDiss`、`music.songlist.SonglistRead` 都返回
   `code=500003`（模块不存在）；`PlaylistBaseRead` 配 `GetPlaylistBase`
   返回 `40000`（方法名错）。唯一可用的是
   **`music.musicasset.PlaylistBaseRead` + `GetPlaylistByUin`**。

2. **QQ 歌单详情的曲目字段与搜索接口不同**
   搜索是 `mid`/`title`，歌单详情是 `songmid`/`songname`。
   我一开始复用了搜索的解析函数，结果 **529 首被静默过滤成 0 首**——
   歌单列表正常显示，点进去却是空的。裸 curl 能拿到 401376 字节 / 529 项，
   说明网络没问题、是解析把所有项都丢了。现已单独实现 `song_from_playlist_item`。

3. **QQ Cookie 里的 uin 带 `o` 前缀**
   `uin=o0853886344`，歌单接口要纯数字，必须去掉这个 `o`（保留后续数字）。

4. **网易云 `current_uid` 只有 eapi 一条路径**
   它的 `verify_login()` 是「先公开接口再 eapi」，两者不一致导致出现
   自相矛盾的状态：界面显示**已登录**，但歌单导入报「无法获取用户 ID」。
   现在 `current_uid` 也走两条路径。

> ⚠️ 导入上限默认 500 首（可传 `limit`）。「我喜欢」这类大歌单要留意：
> 全部塞进空闲歌单既能播很久，也会让界面与持久化文件变大。

### 播放控制的几个语义（阶段 10b 修正）

#### 「上一首 / 下一首没反应」的真正原因：切歌没有串行化

切换播放的流程里有**长达 20 秒的等待**（占位歌曲要等解析器搜索完成），
而在这段等待里执行器会让出。同时可能有多个推进在跑：

- `SongResolved` 触发的自动播放；
- 用户点「下一首 / 上一首」；
- 启动时的空闲歌单开台。

**实测后果**：A 先从队列取走《夜曲》并等待解析，B 也进来取了一次，
于是《夜曲》**既在播放、又留在队列里**（`current` 与 `queue` 各一条
同名但 id 不同的条目）。之后点「下一首」会先取到这条同名 pending 条目，
看起来就是**按了没反应**。

修复：给所有「切换当前播放」的入口加一把 `try_lock` 串行锁
（`switch_lock`），拿不到锁说明已有切歌在跑，直接返回而不是重复切。
覆盖 `start_next` / `skip` / `previous` / `play_idle_index` / `on_song_requested`。

> 用 `try_lock` 而不是 `lock`：后者会让两个切换排队先后执行，
> 结果是「连点 3 次下一首」真的连切 3 首——也不是用户想要的。

#### 「上一首」必须跳过当前正在播的那首

`set_current` 会把刚开播的歌写到历史最前面，所以历史第一条通常就是
**当前曲目本身**。早期直接取第一条，于是「上一首」等于「把当前这首再放一遍」。
现在 `take_previous` 会跳过当前曲目、找**更早**的那首；
历史里只剩当前这首时返回 `None`，界面提示「没有上一首」而不是重播。

#### 只有 `eof` 才算「播完」

| reason | 何时出现 | 该不该推进 |
|--------|---------|-----------|
| `eof` | 文件**自然播完** | **是** |
| `stop` | `loadfile` 换文件、`seek`、主动 `stop` | **否** |
| `error` | 解码/网络失败 | 否 |

早期把 `stop` 也当成播完，于是每次换歌都会额外推进一次队列。
但**丢弃 `stop` 时不能连 `eof` 一起丢**：早期用「下一个 end-file 一律忽略」
的标记来挡 `stop`，结果「seek 到接近结尾」时随后的**真实 `eof` 也被吞掉**，
歌曲播完后队列不再推进、**播放卡住**。现在只按 reason 区分。

#### 单曲循环只在「空闲歌单」页设置

「直播与播放」里**没有**全局单曲循环按钮，因为空闲歌单有自己的
`idle_mode`（顺序/列表循环/单曲循环/随机），两套会互相覆盖：
实测「直播与播放选单曲循环」时，空闲歌单设的顺序播放永远失效。

后端的对应处理：`advance()` / `handle_ended()` 在
`current_is_idle = true` 时**忽略点歌队列的 `play_mode`**，
交给 `take_from_idle()` 用 `idle_mode` 决定下一首。

### 播放器相关

| 现象 | 原因与处理 |
|------|-----------|
| 控制台提示「未检测到 mpv」 | 未安装或不在查找路径里。`winget install shinchiro.mpv`，或设置 `BSR_MPV_PATH` 指向 mpv.exe |
| **点歌入了队列但播放器没声音** | 已修复（两个原因）：① 网易云直链里的 `authSecret` 参数会被 CDN 以 `403 auth failed - origin failed` 拒绝，现在取址时自动去掉；② 解析完成后没有任何地方触发播放，现在解析完成会**自动开播**（正在播歌时不打断，新歌排队） |
| **进度条拖不动** | 已修复。以前进度条只是展示用的 `div`，而且后端**没有 `/api/player/seek` 路由**；现在可点击/拖动，松手才发一次请求 |
| **「跳过」按钮点了没反应** | 已修复。前端是无体 POST，后端用 `Json<T>` 解析会返回 `400 Failed to parse the request body as JSON`；现已改为可选请求体 |
| **没有「上一首」按钮** | 已补上。弹幕点歌是队列语义、没有播放历史，所以「上一首」= **回到当前曲目开头** |
| 点了播放没声音 | 确认 `/api/player/status` 的 `available` 为 true；mpv 是独立进程，声音走系统默认音频设备，不经过 OBS |
| 进度条长时间停在 0 | 直链加载慢（网络流前几秒无 `time-pos` 事件）。程序每 2 秒会主动查询一次，正常几秒后就会动 |
| 报「该歌曲需要登录或受版权限制」 | 该曲目没登录拿不到直链：先在音乐平台卡片登录 |
| 一首歌播完没有自动下一首 | 检查播放模式与队列；`reason != eof` 的 `end-file` 不会推进队列（避免切换文件时误播下一首） |
| 关闭程序后 `mpv.exe` 还在 | 只在**强制终止**进程时出现（`Drop` 不执行）。手动结束即可；正常关窗不会残留 |
| 队列重启后没了 | 检查 `%APPDATA%\bilibili-song-request\queue.json`；文件损坏时程序会按空队列启动并在日志里告警 |

### 排障工具

播放出问题时，这两个示例能直接看到内部真实状态，不用猜：

```powershell
cd src-tauri

# 直连 mpv IPC：打印真实音量 / 暂停 / 进度 / 音频轨 / 输出设备
cargo run --example mpv_inspect

# 打印 B 站接口的实际请求头与待签名串（--dry-run 只打印不联网）
cargo run --example bili_start_probe -- --dry-run <app_id> <access_key_id> <access_key_secret> <身份码>
```


### 没有身份码时如何验证面板

控制台里有一张 **链路自测** 卡片：输入一条弹幕（默认 `点歌 晴天 周杰伦`）点「注入弹幕」，
它会构造与平台报文结构完全一致的帧，走**真实的**解析 → 广播 → `/ws` → 面板链路。
等价的命令行验证：

```powershell
Invoke-RestMethod -Uri 'http://127.0.0.1:17777/api/bilibili/simulate' -Method Post `
  -ContentType 'application/json' -Body '{"text":"点歌 晴天 周杰伦","user":"观众甲","uid":9527}'
```

---

## 5. 内嵌 HTTP / WebSocket API

只监听 `127.0.0.1`，默认端口 `17777`。

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/health` | 健康检查：`{status, version, uptime_secs}` |
| GET | `/api/state` | 全量状态 JSON |
| GET | `/api/queue` | 队列数组 |
| GET | `/api/requests` | 点歌统计：`{cooldowns: [...], recent: [...]}` |
| POST | `/api/queue/add` | `{title, artist?, requested_by?}` 手动加歌 |
| POST | `/api/queue/{id}/{action}` | `action` ∈ `remove` / `top` / `up` / `down` |
| POST | `/api/queue/clear` | 清空队列 |
| GET | `/api/config` | 读取配置 |
| PUT | `/api/config` | 保存配置（写入 `config.json`） |
| GET | `/api/bilibili/status` | 弹幕连接状态：`{connected, status, room_id, last_error, auto_connect}` |
| POST | `/api/bilibili/connect` | `{app_id?, access_key_id?, access_key_secret?, code?}` 合并保存并连接 |
| POST | `/api/bilibili/disconnect` | 断开连接 |
| POST | `/api/bilibili/simulate` | `{text, user?, uid?}` 注入模拟弹幕（走真实解析链路） |
| GET | `/api/music/status` | 音乐平台状态：`{default_platform, platforms[], pending_resolution}` |
| POST | `/api/music/search` | `{keyword?\|title?+artist?, platform?, limit?}` 搜索歌曲 |
| POST | `/api/music/cookie` | `{platform, cookie}` 保存 Cookie（内嵌登录后自动调用） |
| POST | `/api/music/cookie/clear` | `{platform}` 清除 Cookie |
| POST | `/api/music/play-url` | `{item_id?\|song_id?, platform?}` 取音频直链 |
| POST | `/api/player/pause` | `{paused: bool}` 转发到 mpv |
| POST | `/api/player/play` | 开始 / 继续播放队列 |
| GET | `/api/player/status` | `{available, position, duration, volume, paused, playing, mode}` |
| POST | `/api/player/volume` | `{volume: 0-100}` |
| POST | `/api/player/skip` | 跳过当前 |
| POST | `/api/player/mode` | `{mode: "sequential"\|"random"\|"repeat_one"}` |
| GET | `/panel` | OBS 浏览器源页面 |
| GET | `/dashboard` | 浏览器控制台页面 |
| GET | `/assets/*` | 前端构建产物 |
| GET | `/ws` | 实时推送：`hello` / `state` / `danmaku` / `request` / `song` / `player` / `error` |

### WebSocket 消息格式

```json
{ "type": "hello", "data": { "version": "0.1.0", "started_at": "..." } }
{ "type": "state", "data": { "queue": [], "player": {}, "bilibili": {} } }
{ "type": "danmaku", "data": { "user": "观众", "text": "点歌 晴天 周杰伦" } }
```

---

## 6. 实时推送格式

客户端保活：发送 `{"type":"ping"}`，服务器回 `{"type":"pong"}`。

### 点歌请求事件（阶段 4）

观众的点歌指令被处理后会额外推一帧 `request`：

```json
{ "type": "request", "data": { "outcome": "queued",   "user": "观众甲", "title": "晴天", "artist": "周杰伦", "position": 1 } }
{ "type": "request", "data": { "outcome": "rejected", "user": "观众甲", "title": "稻香", "reason": "cooldown", "message": "冷却中，请 27 秒后再点" } }
```

`reason` 取值：`cooldown`（冷却中）、`duplicate`（队内已有）、`queue_full`（队列已满）、
`fans_medal`（粉丝牌等级不足）、`user_level`（用户等级不足）。

---

## 6.1 点歌规则（阶段 4）

规则在控制台「设置」页可改，**保存后立即生效**（正则会被重新编译，冷却/上限下一次判定期即生效），
无需重启进程。

| 规则 | 默认值 | 说明 |
|------|--------|------|
| `command_regex` | `^点歌\s+(.+?)(?:\s+(.+))?$` | 第一条捕获组 = 歌名，第二条 = 歌手（可空）。正则非法时后端拒绝保存并回退到内置默认正则 |
| `cooldown_secs` | 30 | 同一用户冷却；用户标识优先用 UID，弹幕不带 UID 时退回 `name:昵称` |
| `max_queue` | 20 | 待播放队列上限，`0` = 不限；**正在播放的那首不占名额** |
| `allow_duplicate` | false | 关闭时，队内同名同歌手会被拒绝；同名不同歌手视为不同版本，允许 |
| `min_fans_medal_level` | 0 | 粉丝牌等级门槛，`0` = 不限 |
| `min_user_level` | 0 | 用户等级门槛，`0` = 不限 |

被拒绝的请求不会入队，但会记录在「最近点歌请求」列表里并广播 `request` 事件，
便于主播判断是规则太严还是观众没看到提示。冷却中的用户会显示在「冷却中」列表里。

改变指令词（例如想支持「求歌 晴天」）：

1. 控制台 → 设置 → 点歌规则 → 指令正则，改成 `^求歌\s+(.+?)(?:\s+(.+))?$`
2. 保存 → 下一次弹幕即按新规则解析
3. 若正则写错（例如漏了捕获组），接口返回 400 且**不落盘**，原规则继续生效

---

## 6.2 音乐平台登录与曲目解析（阶段 5）

### 登录

点「登录」会打开一个内嵌窗口，登录成功后**自动抓取 Cookie** 并写入系统凭据管理器，无需手动复制。

| 平台 | 需要出现的 Cookie | 说明 |
|------|------------------|------|
| 网易云音乐 | `MUSIC_U` | 网页登录 / 扫码 / 手机号登录都会写它 |
| QQ 音乐 | `qm_keyst` 或 `qqmusic_key` | 也接受 `qqmusic_uin` / `uin` 作为辅助判定 |

几个实现上的注意点（都踩过坑，改代码时别退回去）：

- **登录窗口必须等创建完成再开始轮询**：`run_on_main_thread` 只是排队投递闭包，
  如果立刻轮询，第一次 `get_webview_window` 会拿到 `None`，窗口被误判成
  「用户已关闭」——表现为登录窗一闪而过。
- **抓到 Cookie ≠ 登录成功**：网易云在页面加载时就会写 `MUSIC_U`，**未登录也有**。
  因此必须再调平台接口做事实校验（`/api/nuser/account/get` 能查到账号 UID 才算登录），
  否则「退出登录 → 再点登录」会立刻被判成已登录，用户根本没机会登录。
- **退出登录必须清 WebView，且不能用 `delete_cookie`**：平台的会话 Cookie 存在
  持久化的 WebView 配置目录里，只清凭据库等于没退出。而 Tauri/wry 的
  `delete_cookie` **实测无效**（调用不报错，回读 Cookie 完全没变），
  所以改用 `clear_all_browsing_data()` 并**回读验证**清理结果。
  本应用的 WebView 只服务本地控制台与登录窗口，清空是安全的。
- **要排除「另一个平台」的 Cookie**：登录窗口用的是共享配置目录，里面往往
  同时残留着网易云与 QQ 的令牌。按长度截断时，另一个平台的大体积 Cookie
  会把预算吃光，把目标平台的登录态挤掉。
- **要限制 Cookie 长度**：Windows 凭据库单条密码上限是 **2560 字节**
  （按 UTF-16 存储，所以约 1280 字符），超了会静默退化成明文文件。
  当前上限 1000 字符，并且优先保留关键 Cookie。
- **匿名令牌不算登录**：网易云的 `MUSIC_A` 未登录也会存在，
  只用它判定会让界面显示「已登录」但实际取不到版权曲。

> Cookie 默认存入 Windows 凭据管理器（界面会显示「已保存到系统凭据管理器」）。
> 若系统凭据服务不可用或凭据超长，会退回**权限受限的本地文件（明文）**，
> 界面会明确提示存放位置——看到这个提示说明凭据是明文存的，请留意。

### 退出登录

点「退出登录」会做两件事（缺一不可）：

1. 清除凭据库/文件里保存的 Cookie；
2. **清空 WebView 的浏览数据**——平台的会话 Cookie 在持久化的 WebView 配置目录里，
   只做第 1 步等于没退出：下次点「登录」时窗口一打开就已经是登录态，
   第一次轮询立刻抓到旧 Cookie，表现为「秒登录」，你根本没机会重新登录。

> 注意：第 2 步是**清空本应用 WebView 的全部浏览数据**（Cookie + 站点存储）。
> 因为 Tauri/wry 的按条删除（`delete_cookie`）实测不可用，只能整体清。
> 本应用的 WebView 只访问本地控制台与音乐平台登录页，所以影响范围就是这些登录态；
> B 站的弹幕凭据存在系统凭据库（不走 WebView），**不会被清掉**。

控制台「直播与播放」→ **音乐平台** 卡片：

| 方式 | 操作 | 适用 |
|------|------|------|
| 内嵌登录窗口（推荐） | 点「打开登录窗口」，在弹出的窗口里扫码/账号登录。程序每 0.8 秒检测登录态 cookie，拿到后**自动**存入系统凭据库并关窗 | 桌面 exe |
| 手动粘贴 Cookie | 展开「手动粘贴 Cookie」，粘贴形如 `MUSIC_U=...; __csrf=...` 的完整串 | 浏览器里打开控制台时 |

登录成功的判定依据：网易云看 `MUSIC_U`，QQ 音乐看 `qm_keyst` / `qqmusic_key`
（详细规则与踩坑记录见上一节）。

### Cookie 存到哪里

1. **首选系统凭据库**（Windows 凭据管理器 / macOS Keychain / Linux Secret Service）；
2. 凭据服务不可用**或凭据超长**时（Windows 单条密码上限 2560 字节），回退到
   `%APPDATA%\bilibili-song-request\secrets\<条目名>.txt`，权限收紧为仅当前用户，
   但是**明文**——保存后的界面提示会明确写出实际位置
   （`/api/music/status` 的 `stored_in` 字段也会显示 `keyring` / `file`）。

条目名是持久化契约，不要随意改动（改了等于让已保存的登录失效）：

```text
music.netease.cookie
music.qq.cookie
bilibili.access_key_secret   # 阶段 5 起 B 站密钥也走这里
```

排查登录问题时，可以读 WebView 的 Cookie 库看平台到底写了什么
（需要先完全退出程序，否则数据库被独占）：

```powershell
# 程序运行中：先复制快照（被独占时用 esentutl）
esentutl /y "$env:LOCALAPPDATA\com.bilibili-song-request.desktop\EBWebView\Default\Network\Cookies" /d "$env:TEMP\ck.db"

# Cookies 是 SQLite 库，用 sqlite3 列出各站点写了哪些 Cookie 名
sqlite3 "$env:TEMP\ck.db" "select host_key, name from cookies order by host_key;"
```

### 点歌是「先入队、后解析」

弹幕只给出歌名/歌手，真实曲目信息要搜索才知道。为了不让网络往返拖住弹幕消费：

```text
弹幕「点歌 晴天 周杰伦」
   └─▶ 立即以占位条目入队并广播 request 事件（观众马上看到「已入队」）
          └─▶ 后台解析器串行搜索音乐平台
                 ├─ 命中 → 回填真实 ID/歌手/专辑/时长，广播 song 事件（outcome=resolved）
                 └─ 失败 → 保留用户输入，标记 source=failed 并给出原因（outcome=failed）
```

界面上队列里会显示「解析中…」/「未匹配：<原因>」标签；`/api/music/status` 的
`pending_resolution` 是待解析条目数。**未解析完成的条目不能取播放地址**（会返回 400）。

解析器是**串行**的：非官方接口对并发敏感，并发搜索很容易触发风控。

### 取播放地址的三级回退

实测结论（2026-09 本机验证）：

| 曲目类型 | 旧 `api` 接口（无需登录） | `eapi`（需登录） | 歌单兜底（需登录） |
|----------|--------------------------|------------------|-------------------|
| 免费曲目（`fee == 0`） | ✅ 直接返回 mp3 直链 | — | — |
| 版权 / 会员曲目 | ❌ 返回 `url: ""` | ✅ | ✅（从「我喜欢的音乐」歌单内嵌 url） |

`eapi` 需要 AES-128-CBC 加密请求体 + 签名头（`eapi_body()` 实现，已用真实请求验证返回 `code: 200`）。

---

## 6.3 播放器（mpv）与队列持久化（阶段 6）

### 音频链路

```text
队列 ──取下一首──▶ 等曲目解析完成 ──▶ 音乐平台取直链 ──▶ mpv loadfile
 ▲                                                        │
 └──────── PlayerEvent::Ended（mpv end-file: eof）◀─────────┘
```

**声音只由 mpv 输出到系统音频设备**，OBS 面板不出声（避免浏览器源被节流）。

### mpv 从哪里找

按顺序查找（第一个存在的生效）：

1. 环境变量 `BSR_MPV_PATH`（直接指定 exe 路径）
2. 配置文件 `player.mpv_binary`
3. exe 同级的 `mpv.exe`、`binaries/mpv-<target-triple>.exe`（Tauri sidecar 布局）
4. 向上 1~4 级目录里的 `mpv.exe` / `binaries/mpv*.exe`（覆盖 `cargo run` 开发场景）
5. 系统安装位置（Windows 含 `C:\Program Files\MPV Player`、winget 便携包目录）
6. `PATH`

找不到时**不会阻止程序启动**：控制台会提示「未检测到 mpv」，`/api/player/status` 的
`available` 为 `false`，播放按钮禁用，但点歌、队列、面板都照常工作。

安装 mpv（**用安装包的用户不用做这一步——mpv 已内置**；下面三种是源码构建/裸 exe 场景）：

```powershell
# ① 推荐：winget 安装完整版（含所需 DLL，最省事）
winget install shinchiro.mpv

# ② 让程序把 mpv 作为 sidecar 一起打包（当前安装包就是这么做的）
npm run fetch:mpv            # 下载并放到 src-tauri/binaries/mpv-x86_64-pc-windows-msvc.exe
npm run tauri:build          # 安装包里会带上它，安装后落在程序同级的 mpv.exe

# ③ 手动指定已有的 mpv
$env:BSR_MPV_PATH = "D:\tools\mpv\mpv.exe"
```

> 说明：`scripts/fetch-mpv.ps1` 只复制 `mpv.exe` 主程序。实测 shinchiro 的构建是
> **静态链接**的，单独一个 exe 就能跑（不需要同包的 `d3dcompiler_43.dll`），
> 所以 sidecar 方案是可靠的——安装包里带的就是它。

### 启动参数

```text
--idle=yes --no-video --keep-open=no --input-ipc-server=\\.\pipe\mpvpipe --volume=<n>
--audio-display=no --cache=yes --cache-secs=30 --network-timeout=15
--stream-lavf-o=reconnect=1,reconnect_streamed=1,reconnect_delay_max=5
```

后两组参数是为音乐平台直链准备的：直链是短时效 CDN 地址，链路抖动时默认行为会直接失败。

### 播放模式

| 模式 | 播完后的行为 | 点「跳过」的行为 |
|------|-------------|-----------------|
| 顺序 `sequential` | 从队首取下一首 | 换下一首 |
| 随机 `random` | 从队列随机取一首 | 换一首 |
| 单曲循环 `repeat_one` | 重播同一首 | **换歌**（当前这首真正出队） |

### 进度上报

正常情况下 mpv 通过 `observe_property` 推送 `time-pos`。
但实测发现：mpv 只在属性**发生变化**时推送，加载网络流的头几秒可能一个事件都没有，
界面上进度条会一直停在 0。因此控制器每 2 秒**主动查询**一次 `time-pos` 作为兜底
（每个 tick 一次 IPC 往返，开销可忽略）。

### 队列持久化

- 文件：`%APPDATA%\bilibili-song-request\queue.json`
- 内容：**待播放**条目 + 播放模式。**不保存**正在播放的那首（重启后音频流已失效，
  重新点一次更合理），也不保存已播放/已跳过的。
- 已解析的曲目信息（平台 ID/歌手/时长/专辑）会被完整保留，因此**重启后无需重新搜索**。
- 写入时机：队列内容变化后（带去抖）与退出前各写一次；读取失败/文件损坏时按空队列启动，
  不让队列文件坏了就打不开程序。

### 退出行为

窗口关闭时：保存队列 → 通知服务器 runtime 结束 → **显式关闭 mpv 子进程**。

> ⚠️ 已知限制：如果进程被**强制终止**（任务管理器「结束任务」、`Stop-Process -Force`），
> Rust 的 `Drop` 不会执行，mpv 会残留。此时手动结束 `mpv.exe` 即可；
> 正常关闭窗口不会残留（已实测验证）。

---

## 7. OBS 添加浏览器源

1. 启动本程序（exe），确认日志里出现「内嵌 HTTP/WebSocket 服务器已就绪」。
2. OBS → 来源 → `+` → **浏览器** → 名称随意（例如 `点歌面板`）。
3. URL 填：

   ```text
   http://127.0.0.1:17777/panel?bg=transparent&theme=dark&limit=8&fontSize=16
   ```

4. 宽高建议 `600 × 800`（面板按内容自适应，配合 `scale` 参数缩放）。
5. **勾选「透明背景」**（否则会看到底色）。
6. 「自定义 CSS」留空即可。
7. 声音：面板**不出声**。音频由 mpv 输出，请在 OBS 添加「音频输入采集 → 桌面音频」
   （或使用虚拟声卡把 mpv 单独路由）。

### 面板 URL 参数

| 参数 | 取值 | 说明 |
|------|------|------|
| `theme` | `dark` / `light` | 配色 |
| `bg` | `transparent` / `solid` | 背景；OBS 场景用 `transparent` |
| `color` | `%23ff0000` 或 `red` | 主色（`#` 需写成 `%23`） |
| `fontSize` | 8–96 | 基础字号（px） |
| `scale` | 0.2–5 | 整体缩放 |
| `limit` | 0–100 | 队列显示条数，`0` 表示全部 |
| `showLyrics` | `true` / `false` | 是否显示歌词区 |
| `layout` | `list` / `compact` / `lyrics` / `wide` | 布局模式，见下表 |

#### 布局对比

| layout | 形态 | 适合 |
|--------|------|------|
| `list`（默认） | 卡片式：正在播放 + 队列 + 下方歌词 | 画面角落/侧边竖条 |
| `compact` | 卡片式精简：单行队列，隐藏歌手与点歌人 | 空间很小 |
| `lyrics` | 卡片式，歌词区更高（`72vh`），队列条数靠 `limit` 控制 | 想突出歌词 |
| `wide` | **宽版**：背景透明、无卡片底色；左侧纯文字「正在播放 + 点歌队列」，右侧歌词 | 画面底部/顶部通栏（建议 **800 × 720**） |

宽版示例（OBS 浏览器源建议 800 × 720，勾选透明背景）：

```text
http://127.0.0.1:17777/panel?layout=wide&bg=transparent&theme=dark&limit=6
```

宽版细节：

- 完全透明：没有卡片背景、圆角、描边，只有文字（正文带一层淡描边阴影，
  叠在亮色画面上也能看清；`theme=light` 时不加阴影）。
- 左侧宽度约屏宽 46%（上限 430px），右侧歌词贴右边缘；两栏绝不重叠。
- 正在播放用「▶ + 主色文字」标示，而不是底色块。
- 窗口宽度不足 700px 时自动退回纵向堆叠，避免文字挤在一起。

> 之前的三种布局与旧版**完全一致**（DOM 结构与样式都没有改动），
> `layout=wide` 是并列新增的一版，可以在 OBS 里随时切换对比。

示例：

```text
http://127.0.0.1:17777/panel?layout=compact&limit=5&scale=1.2&color=%23ffb703
```

**参数可以随时改**：面板监听了 `popstate` / `hashchange`，在 OBS 里改完 URL
点确定就即时生效，不需要手动刷新浏览器源。非法参数值会被忽略并回落到配置里的默认样式，
不会出现「参数写错整块白屏」。

### 面板行为说明（阶段 7）

| 行为 | 说明 |
|------|------|
| 歌词滚动 | 后端算好「当前第几句」（`player.lyric_index`）下发，前端解析 LRC 后高亮该行并把它滚到中间；已唱过的行淡出 |
| 换歌无闪烁 | 换歌时**保留上一首的显示**，直到新歌加载成功才切换，不会闪一下「暂时没有歌曲」 |
| 播放中高亮 | 队列里正在播放的条目加底色与左侧色条，序号变成 `▶` |
| 入队动画 | 新点歌滑入、被删除的滑出、重排平滑移动；识别 `prefers-reduced-motion`，系统开了「减少动态效果」就自动关掉 |
| 未解析的条目 | 淡化显示，提示这首歌还在搜索中 |
| 歌词失败不阻塞 | 取不到歌词只是不显示歌词（面板显示「暂无歌词」），播放照常 |

> OBS 里的实际效果：仓库 `docs/` 下有 `panel-wide.png`（宽版：左文字 / 右歌词）、
> `panel-wide-light.png`（宽版浅色）、`panel-playing.png`（原卡片式布局）等参考截图。
> 这些是用 headless Chrome / CDP 抓的**真实运行界面**，不是设计稿。
>
> ⚠️ 截图小坑：`chrome --screenshot` 是在**页面加载完成那一刻**拍照，
> 2~3 秒一首的测试音频经常正好落在换歌空档，会拍出「没有歌曲」的画面而让人误以为面板坏了。
> 正确做法是先轮询到「确实有歌在播且有歌词」再截，并临时开启
> `prefers-reduced-motion: reduce` 避免拍到动画中间帧。
> 现在界面自检统一用 `scripts/tauri-eval.mjs`（通过 CDP 在应用窗口里求值）：
>
> ```powershell
> # 轮询到「正在播放」再截图（PowerShell + CDP，无需额外脚本）
> node scripts/tauri-eval.mjs "document.querySelector('.now-title').textContent" 9333
> ```

---

## 8. 目录结构

```text
bilibili-song-request/
├── src-tauri/                 # Rust 后端
│   ├── src/
│   │   ├── main.rs            # 入口：拉起 axum + Tauri 窗口
│   │   ├── lib.rs             # 模块导出
│   │   ├── server.rs          # axum 路由、WS、静态资源
│   │   ├── server/templates.rs# 页面模板与 HTML 注入
│   │   ├── webdist.rs         # dist 目录发现与页面渲染
│   │   ├── state.rs           # StateCell / EventBus / 广播
│   │   ├── config.rs          # 配置读写
│   │   ├── event.rs           # 内部事件与 WS 消息封包
│   │   ├── models.rs          # 共享数据模型
│   │   ├── bilibili/          # auth.rs / danmaku.rs
│   │   ├── music/             # MusicAdapter trait + netease / qq / secrets / service / resolver
│   │   ├── player/            # PlayerBackend trait + mpv IPC + controller（队列驱动播放）
│   │   ├── player/lyrics.rs   # LRC 解析与「当前行」定位
│   │   ├── binresolver.rs     # 外部二进制（mpv）定位
│   │   ├── shutdown.rs        # 优雅退出信号（保证 mpv 子进程被清理）
│   │   ├── queue.rs           # 队列操作
│   │   ├── queue/command.rs   # 点歌指令解析
│   │   ├── queue/request.rs   # 点歌规则引擎（冷却/上限/去重/等级）
│   │   ├── queue/store.rs     # 队列持久化（重启恢复）
│   │   └── tests.rs           # 集成测试
│   ├── examples/ipc_probe.rs  # 排障用：验证 mpv IPC 管道是否长连接
│   ├── binaries/              # mpv sidecar（见其 README）
│   └── Cargo.toml
├── src/                       # Vue 3 前端
│   ├── main.ts / panel.ts / dashboard.ts   # 三个入口
│   ├── router.ts, types.ts, env.d.ts
│   ├── api/index.ts, api/realtime.ts       # HTTP + WS 客户端
│   ├── stores/app.ts                       # Pinia 状态
│   ├── views/Dashboard.vue, views/Panel.vue
│   ├── composables/panelParams.ts          # 面板 URL 参数解析（含热更新）
│   ├── composables/lrc.ts                  # 前端 LRC 解析（与后端实现同构）
│   └── styles/base.css
├── scripts/                   # 工具脚本（不参与打包）
│   ├── check-versions.mjs     # 三处版本号一致性校验（--fix 可自动同步）
│   ├── fetch-mpv.ps1          # 下载 mpv 并按 sidecar 命名放置
│   ├── mock-music-server.mjs  # 本地桩音乐服务（网易云响应结构）
│   ├── tauri-eval.mjs         # 通过 CDP 在应用窗口里求值（界面自检）
│   └── rebuild-and-restart.ps1 # 重建 + 重启，带 SHA256/健康检查自校验
├── tests/lrc.test.ts          # 前端 LRC 解析单测（node --test）
├── docs/                      # 面板参考截图 + 图标 PNG
├── index.html / panel.html / dashboard.html
├── vite.config.ts, tsconfig.json, package.json
├── LICENSE
└── README.md
```

---

## 9. 开发提示

- **前端三个入口**：`index.html`（Tauri 窗口）、`panel.html`（OBS）、`dashboard.html`（浏览器）。
  改 `vite.config.ts` 的 `rollupOptions.input` 时注意同步。
- **类型同步**：`src/types.ts` 与 `src-tauri/src/models.rs` 必须逐字段一致。
- **调试面板样式**：浏览器直接打开 `http://127.0.0.1:17777/panel?...`，
  DevTools 里改参数刷新即可，无需重启 exe。
- **端口被占用**：程序会自动改用系统分配端口并在日志里打印真实地址；
  建议在 `config.json` 里换一个端口后重启。
- **未构建前端时**：`/panel` 会显示「前端资源尚未构建」提示页，而不是 404。
- **测试不要碰用户数据**：`PUT /api/config` 会真的落盘、Cookie 会真的进凭据库，
  因此集成测试通过 `ServerCtx::with_services` 显式注入临时配置路径与 `SecretStore::file_only`，
  **不要**用改进程环境变量或写入真实 `%APPDATA%` 的方式做隔离。
- **播放链路改动后务必跑一次真机验收**：见下面的「离线端到端验收」。

### 离线端到端验收（不依赖真实网易云）

网易云直链是短时效 CDN 地址且受版权/风控影响，无法稳定复现。把接口指向本地桩服务后，
「点歌 → 解析 → 取地址 → mpv 播放 → 进度 → 歌词 → 自动下一首 → 退出清理」全链路都能自动验证：

```powershell
# 1) 起桩服务（返回网易云的响应结构，音频指向 C:\Windows\Media 下的 wav）
node scripts/mock-music-server.mjs 18999

# 2) 另开一个终端，把接口指向桩服务并启动程序
$env:BSR_NETEASE_API_BASE = "http://127.0.0.1:18999"
.\src-tauri\target\debug\bilibili-song-request.exe

# 3) 加歌并播放（走真实解析链路）
curl.exe -X POST http://127.0.0.1:17777/api/queue/add -H "content-type: application/json" `
  -d '{\"title\":\"本地测试曲一\",\"requested_by\":\"验收\"}'
curl.exe -X POST http://127.0.0.1:17777/api/player/play

# 4) 用 CDP 确认面板状态（主题、队列、歌词、当前曲目）
node scripts/tauri-eval.mjs "document.querySelector('.now-title').textContent" 9333
```

排查 mpv 相关问题时，`src-tauri/examples/ipc_probe.rs` 可以单独验证
「interprocess + mpv 管道是否保持长连接」（曾用它定位到管道被旧实例占用的问题）：

```powershell
cd src-tauri
cargo run --example ipc_probe -- "C:\Program Files\MPV Player\mpv.exe" '\\.\pipe\bsr-probe'
```

### 环境变量

| 变量 | 作用 |
|------|------|
| `BSR_MPV_PATH` | 直接指定 `mpv.exe` 路径（优先级最高） |
| `BSR_CONFIG_DIR` | 覆盖配置目录（便携模式）：配置/日志/队列/密钥都放这里 |
| `BSR_DIST_DIR` | 指定前端构建产物目录 |
| `BSR_NETEASE_API_BASE` | 覆盖网易云接口域名前缀（镜像 / 本地桩服务）；只改域名，路径与参数不变 |
| `BSR_LOG` | 日志级别，如 `bilibili_song_request_lib::player=debug,info` |

---

## 10. 分发与打包（阶段 8）

### 产物

```powershell
npm run tauri:build
```

产物在 `src-tauri/target/release/bundle/`：

| 路径 | 说明 |
|------|------|
| `release/bilibili-song-request.exe` | 免安装绿色版 exe（约 12.6 MB） |
| `release/_up_/dist/` | 前端资源，**必须与 exe 同级保留**（绿色版靠它提供面板页） |
| `nsis/bilibili-song-request_<版本>_x64-setup.exe` | NSIS 安装包（约 3.6 MB，**自带前端**） |

> **绿色版与安装包的区别**：
> 绿色版 exe **不内嵌** OBS 面板用的前端资源——那些页面由程序自建的 HTTP 服务器
> 从磁盘读取，所以 `_up_/dist/` 必须跟着 exe 一起走（整个 `release/` 目录拷走即可）。
> 安装包则通过 `bundle.resources` 把前端一起装进去，装完是独立程序。
>
> `tauri-build` 会在编译后**自动**把 `resources` 复制到输出目录，
> 因此 `cargo build --release` 也会得到可用的 `release/_up_/dist/`，不需要手动拷贝。

### 打包时国内网络需要代理

首次打包要从 GitHub 下载 NSIS 工具链，直连会超时：

```text
Error failed to bundle project: `timeout: global`
```

给构建进程带上代理即可（工具链只下一次，缓存在 `%LOCALAPPDATA%\tauri\NSIS\`）：

```powershell
$env:HTTPS_PROXY = 'http://127.0.0.1:26561'
$env:HTTP_PROXY  = 'http://127.0.0.1:26561'
npm run tauri:build
```

`bundle.targets` 目前只配了 `nsis`。想要 MSI 可以改成
`["nsis", "msi"]`（MSI 需要 WiX，Tauri 会自动下载）。

### 图标

`src-tauri/icons/icon.ico` 是一个 **7 种尺寸**（16/24/32/48/64/128/256）的多分辨率图标，
安装包、任务栏、Alt+Tab 都会用到。`docs/icon.png` 是同一图标的 256px PNG（便于复用）。

> 之前这里是个 32×32 的占位图标，安装包缩放时会明显发虚，已替换。

### mpv 的两种分发方式

| 方式 | 做法 | 结果 |
|------|------|------|
| **作为 sidecar 一起打包（当前采用）** | `npm run fetch:mpv` 后 `npm run tauri:build`，安装包里带上 `mpv.exe` | 用户**装完开箱即用**；安装包约 36.6 MB |
| 依赖用户已安装的 mpv | 去掉 `bundle.externalBin`，装完提示用户 `winget install shinchiro.mpv` | 安装包约 3.6 MB，但要用户自己装播放引擎 |

sidecar 命名必须是 `<名称>-<目标三元组>.exe`（Windows MSVC 为
`mpv-x86_64-pc-windows-msvc.exe`），放在 `src-tauri/binaries/`；
构建时通过 `bundle.externalBin` 声明（当前**已声明**为 `["binaries/mpv"]`）。
安装后 Tauri 会把它重命名为 `mpv.exe` 放在程序同级，程序查找 mpv 时同目录优先。

> `src-tauri/binaries/mpv-*.exe` 有 **115 MB**，已在 `.gitignore` 中忽略，
> **不入库**。因此**在新机器上打包前必须先跑一次 `npm run fetch:mpv`**。
> 依赖代理下载（国内直连 GitHub 会超时）。

### 发布前检查清单

```powershell
npm run check:versions        # 三处版本一致
npm run build                 # 前端 + 类型检查
npm test                      # 前端单测
cd src-tauri; cargo test      # 后端测试（当前 317 个）
cd src-tauri; cargo check --all-targets   # 零告警
npm run fetch:mpv             # 准备 mpv sidecar（新机器必做）
npm run tauri:build           # 出安装包
```

装完包后建议手动过一遍：启动程序 → 面板能打开 → 点歌 → 有声音 → OBS 里透明背景正常。

发布版已实测过（绿色版，`BSR_CONFIG_DIR` 指向临时目录）：

```text
便携目录自动创建 config.json / logs/ / secrets/
/health               → {"status":"ok","version":"0.1.0"}
/api/state            → version=0.1.0 play_mode=sequential
/panel                → HTTP 200
/api/player/status    → available=true（自动找到系统 mpv）
```

### 已知的打包事项

- **首次启动可能被杀软误报**：未签名的自编译 exe 常见现象，加信任即可；正式分发建议代码签名。
- **端口被占用**：程序会自动改用系统分配端口并在日志打印真实地址，但 OBS 里的面板 URL
  需要同步改成那个端口。建议先在配置文件里换端口。
- **许可证**：本项目 MIT；但 mpv 是 GPLv2+ / LGPLv2.1+，
  若你把 mpv 作为 sidecar 一起分发，请自行确认许可合规（保留其许可证与来源说明）。

---

## 11. 常见问题

**Q：打开 `/panel` 显示「前端资源尚未构建」？**
执行 `npm install && npm run build` 后重启程序；或设置环境变量
`BSR_DIST_DIR` 指向 `dist` 目录。

**Q：OBS 面板有黑底 / 白底？**
OBS 浏览器源里勾选「透明背景」，并确认 URL 含 `bg=transparent`。

**Q：OBS 里有画面但没声音？**
这是设计如此。音频走 mpv → 系统音频设备，请在 OBS 添加「桌面音频」采集，
或把 mpv 输出路由到虚拟声卡后单独采集。

**Q：面板不刷新 / 歌词不动？**
先用 `node scripts/tauri-eval.mjs "document.querySelector('.now-title').textContent" 9333`
确认是「面板没收到推送」还是「播放没在推进」。
前者看浏览器控制台是否有 WS 报错（通常是端口填错），后者看 `%APPDATA%\bilibili-song-request\logs\app.log`。

**Q：`cargo check` 报找不到链接器 `link.exe`？**
安装 VS 2022 Build Tools 的「使用 C++ 的桌面开发」工作负载。

**Q：身份码能开几个连接？**
B 站限制同一身份码最多 5 个 WebSocket 连接。本程序内部只建立 1 个连接，
所有面板与控制台共享同一份数据。

**Q：日志里一直刷 `mpv 命令通道已关闭`？**
说明与 mpv 的 IPC 断了。程序启动时会先清理占着同名管道的旧实例，
正常情况不会出现；若持续出现，通常是同时开了多个本程序实例（两个进程抢 `\\.\pipe\mpvpipe`），
关掉多余的实例即可。

---

## 12. 合规声明

音乐平台接口为非官方接口，仅用于本地学习研究，请遵守各平台服务条款；
B 站弹幕接入使用官方直播开放平台身份码模式。
