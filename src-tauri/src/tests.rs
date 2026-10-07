//! 跨模块集成测试：把真实的 axum 路由拉到回环端口上打一遍。
//!
//! 这些测试覆盖阶段 1 的验收点：
//!  - `/health` 可用
//!  - `/api/state`、`/api/queue`、`/api/config` 返回正确结构
//!  - 队列操作（加歌 / 上移 / 置顶 / 删除 / 清空）真的改变状态
//!  - `/ws` 能收到 hello + state 快照，并能回应 ping

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::bilibili::{DanmakuBroadcaster, DanmakuHandle};
use crate::config::Config;
use crate::models::{AppState, BilibiliState, MusicPlatform, PlayerState, Song, SongSource};
use crate::music::{MusicService, SecretStore};
use crate::server::{self, ServerCtx};
use crate::state::{EventBus, StateCell};
use crate::VERSION;

/// 测试用配置目录。
///
/// `PUT /api/config` 会真的落盘，因此测试必须写到临时目录，
/// 绝不能污染用户 `%APPDATA%` 里的真实配置。
///
/// 这里通过 [`ServerCtx::with_config_path`] 显式注入路径，
/// 而**不是**设置进程级环境变量——后者在并行测试里存在竞态。
fn test_config_path() -> std::path::PathBuf {
    // ⚠️ 每个测试用**独立目录**（而不是同一目录下的不同文件名）：
    // 点歌日志 `requests.jsonl` 与配置文件同目录，若共用目录，
    // 一个测试写的日志会被另一个测试读到，「历史日志」断言随即随机失败。
    let unique = format!(
        "bsr-test-{:?}-{}",
        std::thread::current().id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let dir = std::env::temp_dir().join(unique);
    let _ = std::fs::create_dir_all(&dir);
    dir.join("config.json")
}

/// 测试用音乐服务：凭据写到临时目录，绝不碰系统凭据库之外的用户数据目录。
fn test_music_service() -> Arc<MusicService> {
    let dir = std::env::temp_dir().join(format!(
        "bsr-test-music-{:?}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::create_dir_all(&dir);
    // file_only：测试并行运行时避开系统凭据库的同名条目争抢。
    Arc::new(MusicService::with_stores(
        SecretStore::file_only("netease.cookie", &dir),
        SecretStore::file_only("qq.cookie", &dir),
    ))
}

/// 带配置定制的测试上下文。
///
/// `tweak` 在 `ServerCtx::new` 之前修改配置，因此也是构造期唯一的修改时机
/// （`ServerCtx` 内部用 `Arc<RwLock<Config>>` 共享给点歌服务）。
fn test_ctx_with<F: FnOnce(&mut Config)>(tweak: F) -> (Arc<ServerCtx>, StateCell) {
    let mut config = Config::default();
    config.server.port = 0;
    tweak(&mut config);
    let state = StateCell::new(AppState::new(
        VERSION,
        PlayerState::default(),
        BilibiliState::default(),
    ));
    let bilibili = DanmakuHandle::new(state.clone(), DanmakuBroadcaster::default());
    // 与生产路径（server::serve）保持一致：挂上弹幕事件 → WS 广播的转发器，
    // 否则 simulate 接口注入的弹幕不会出现在 /ws 上。
    server::spawn_danmaku_forwarder(&bilibili, &state);
    let ctx = ServerCtx::with_services(
        state.clone(),
        EventBus::default(),
        config,
        bilibili,
        test_config_path(),
        test_music_service(),
    );    // 与生产路径一致：弹幕 → 点歌指令 → 队列
    server::spawn_request_consumer(&ctx);
    (ctx, state)
}

/// 启动测试服务器并返回基地址。
async fn spawn_server() -> (String, StateCell) {
    spawn_server_with(|_| {}).await
}

/// 启动带定制配置的测试服务器。
async fn spawn_server_with<F: FnOnce(&mut Config)>(tweak: F) -> (String, StateCell) {
    let (ctx, state) = test_ctx_with(tweak);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定回环端口失败");
    let port = listener.local_addr().expect("读取本地地址失败").port();
    let router = server::build_router(Arc::clone(&ctx));
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://127.0.0.1:{port}"), state)
}

/// 轮询 `/health` 直到服务器就绪。
async fn wait_ready(base: &str) {
    let client = reqwest::Client::new();
    for _ in 0..50 {
        if let Ok(resp) = client.get(format!("{base}/health")).send().await {
            if resp.status().is_success() {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    panic!("测试服务器未在 2 秒内就绪");
}

/// 轮询等待队列长度达到期望值（弹幕处理是异步的）。
async fn wait_queue_len(state: &StateCell, want: usize) {
    for _ in 0..100 {
        if state.read().queue.len() == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "队列长度未在 2 秒内变为 {want}（当前 {}）",
        state.read().queue.len()
    );
}

/// 轮询等待最近记录条数达到期望值。
async fn wait_recent_len(state: &StateCell, want: usize) {
    for _ in 0..100 {
        if state.read().stats.recent.len() >= want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "请求记录未在 2 秒内达到 {want} 条（当前 {}）",
        state.read().stats.recent.len()
    );
}

#[tokio::test]
async fn health_and_state_endpoints_work() {
    let (base, _state) = spawn_server().await;
    wait_ready(&base).await;
    let client = reqwest::Client::new();

    let health: Value = client
        .get(format!("{base}/health"))
        .send()
        .await
        .expect("health 请求失败")
        .json()
        .await
        .expect("health 不是 JSON");
    assert_eq!(health["status"], "ok");
    assert_eq!(health["version"], VERSION);

    let state: AppState = client
        .get(format!("{base}/api/state"))
        .send()
        .await
        .expect("state 请求失败")
        .json()
        .await
        .expect("state 不是 AppState");
    assert!(state.queue.is_empty());
    assert!(state.current.is_none());
    assert_eq!(state.player.volume, 80);

    let config: Config = client
        .get(format!("{base}/api/config"))
        .send()
        .await
        .expect("config 请求失败")
        .json()
        .await
        .expect("config 不是 Config");
    assert_eq!(config.server.port, 0);
    assert!(config.rules.command_regex.contains("点歌"));
}

#[tokio::test]
async fn queue_operations_mutate_state() {
    let (base, state) = spawn_server().await;
    wait_ready(&base).await;
    let client = reqwest::Client::new();

    // 加两首歌
    for (title, artist) in [("晴天", "周杰伦"), ("富士山下", "陈奕迅")] {
        let resp = client
            .post(format!("{base}/api/queue/add"))
            .json(&serde_json::json!({ "title": title, "artist": artist, "requested_by": "测试观众" }))
            .send()
            .await
            .expect("加歌请求失败");
        assert!(resp.status().is_success(), "加歌应成功：{}", resp.status());
    }
    assert_eq!(state.read().queue.len(), 2);

    let first_id = state.read().queue[0].id;
    let second_id = state.read().queue[1].id;

    // 上移第二首 → 交换顺序
    client
        .post(format!("{base}/api/queue/{second_id}/up"))
        .send()
        .await
        .expect("上移失败");
    assert_eq!(state.read().queue[0].id, second_id);

    // 置顶第一首
    client
        .post(format!("{base}/api/queue/{first_id}/top"))
        .send()
        .await
        .expect("置顶失败");
    assert_eq!(state.read().queue[0].id, first_id);

    // 删除第一首
    client
        .post(format!("{base}/api/queue/{first_id}/remove"))
        .send()
        .await
        .expect("删除失败");
    assert_eq!(state.read().queue.len(), 1);

    // 非法 ID → 400
    let bad = client
        .post(format!("{base}/api/queue/not-a-uuid/remove"))
        .send()
        .await
        .expect("非法 ID 请求失败");
    assert_eq!(bad.status(), reqwest::StatusCode::BAD_REQUEST);

    // 清空
    client
        .post(format!("{base}/api/queue/clear"))
        .send()
        .await
        .expect("清空失败");
    assert!(state.read().queue.is_empty());
}

#[tokio::test]
async fn websocket_pushes_snapshot_and_answers_ping() {
    let (base, _state) = spawn_server().await;
    wait_ready(&base).await;

    let url = base.replace("http://", "ws://") + "/ws";
    let (mut socket, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("WS 连接失败");

    // 第一帧：hello
    let hello = next_json(&mut socket).await;
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["data"]["version"], VERSION);

    // 第二帧：全量状态
    let state = next_json(&mut socket).await;
    assert_eq!(state["type"], "state");
    assert!(state["data"]["queue"].is_array());

    // 应用 ping → 期待 pong（服务器仅回执，不回包）
    socket
        .send(WsMessage::Text(r#"{"type":"ping"}"#.into()))
        .await
        .expect("发送 ping 失败");
    socket
        .send(WsMessage::Text(r#"{"type":"pong"}"#.into()))
        .await
        .expect("发送非法类型失败");

    let err = next_json(&mut socket).await;
    assert_eq!(err["type"], "error");
}

/// 读取下一帧 JSON（跳过 Ping/Pong 控制帧），5 秒超时。
async fn next_json<S>(socket: &mut S) -> Value
where
    S: StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .expect("等待 WS 帧超时")
            .expect("WS 流已结束")
            .expect("WS 帧错误");
        match msg {
            WsMessage::Text(text) => {
                return serde_json::from_str(&text).expect("WS 帧不是合法 JSON");
            }
            WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => continue,
            other => panic!("收到非预期帧：{other:?}"),
        }
    }
}

#[test]
fn app_state_serializes_with_expected_field_names() {
    // 前端 types.ts 依赖 snake_case 字段名，这里做一次守卫。
    let state = AppState::new(VERSION, PlayerState::default(), BilibiliState::default());
    let json = serde_json::to_string(&state).expect("序列化失败");
    assert!(json.contains("\"started_at\""));
    assert!(json.contains("\"play_mode\""));
    assert!(json.contains("\"bilibili\""));
    assert!(json.contains("\"volume\""));
}

#[tokio::test]
async fn bilibili_simulate_runs_full_danmaku_pipeline() {
    let (base, _state) = spawn_server().await;
    wait_ready(&base).await;

    // 先用 WS 订阅，确保模拟弹幕真的走了广播通道。
    let url = base.replace("http://", "ws://") + "/ws";
    let (mut socket, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("WS 连接失败");
    // 丢弃 hello 与首帧快照
    let _ = next_json(&mut socket).await;
    let _ = next_json(&mut socket).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/api/bilibili/simulate"))
        .json(&serde_json::json!({ "text": "点歌 晴天 周杰伦", "user": "观众甲", "uid": 9527 }))
        .send()
        .await
        .expect("模拟弹幕请求失败");
    assert!(resp.status().is_success(), "模拟接口应成功：{}", resp.status());

    // 应收到一条 danmaku 消息，且内容经过真实解析
    let frame = next_json(&mut socket).await;
    assert_eq!(frame["type"], "danmaku");
    assert_eq!(frame["data"]["user"], "观众甲");
    assert_eq!(frame["data"]["text"], "点歌 晴天 周杰伦");
    assert_eq!(frame["data"]["uid"], "9527");
}

#[tokio::test]
async fn bilibili_connect_rejects_incomplete_config() {
    let (base, _state) = spawn_server().await;
    wait_ready(&base).await;
    let client = reqwest::Client::new();

    // 配置默认为空，connect 应返回 400 并说明缺什么
    let resp = client
        .post(format!("{base}/api/bilibili/connect"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("connect 请求失败");
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: Value = resp.json().await.expect("错误响应应为 JSON");
    let message = body["error"].as_str().unwrap_or_default();
    assert!(message.contains("app_id"), "错误信息应指出缺失字段：{message}");
}

#[tokio::test]
async fn bilibili_status_and_disconnect_are_available() {
    let (base, _state) = spawn_server().await;
    wait_ready(&base).await;
    let client = reqwest::Client::new();

    let status: Value = client
        .get(format!("{base}/api/bilibili/status"))
        .send()
        .await
        .expect("status 请求失败")
        .json()
        .await
        .expect("status 应为 JSON");
    assert_eq!(status["connected"], false);
    assert_eq!(status["status"], "idle");

    let after: Value = client
        .post(format!("{base}/api/bilibili/disconnect"))
        .send()
        .await
        .expect("disconnect 请求失败")
        .json()
        .await
        .expect("disconnect 应为 JSON");
    assert_eq!(after["connected"], false);
}

// ─────────────────────────────────────────────────────────────────────────────
// 阶段 4：弹幕指令 → 点歌队列
// ─────────────────────────────────────────────────────────────────────────────

/// 注入一条模拟弹幕，按**观众**身份处理（验证冷却/上限/黑名单等限制）。
///
/// ⚠️ 必须显式传 `host: false`：`/api/bilibili/simulate` 默认按**主播权限**
/// 处理（这是控制台点歌机的定位，拥有最高权限）。若不传，
/// 冷却与弹幕名额会被绕过，这些用例就失去意义了。
async fn simulate(base: &str, text: &str, user: &str, uid: u64) -> reqwest::StatusCode {
    reqwest::Client::new()
        .post(format!("{base}/api/bilibili/simulate"))
        .json(&serde_json::json!({ "text": text, "user": user, "uid": uid, "host": false }))
        .send()
        .await
        .expect("simulate 请求失败")
        .status()
}

/// 注入一条模拟弹幕，按**主播**身份处理（点歌机/链路自测的默认行为）。
async fn simulate_as_host(base: &str, text: &str) -> reqwest::StatusCode {
    reqwest::Client::new()
        .post(format!("{base}/api/bilibili/simulate"))
        .json(&serde_json::json!({ "text": text, "host": true }))
        .send()
        .await
        .expect("simulate 请求失败")
        .status()
}

#[tokio::test]
async fn danmaku_command_enters_queue_and_broadcasts_request() {
    let (base, state) = spawn_server().await;
    wait_ready(&base).await;

    // 先订阅 /ws，验证 request 帧
    let url = base.replace("http://", "ws://") + "/ws";
    let (mut socket, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("WS 连接失败");
    let _ = next_json(&mut socket).await; // hello
    let _ = next_json(&mut socket).await; // state

    assert!(simulate(&base, "点歌 晴天 周杰伦", "观众甲", 9527)
        .await
        .is_success());
    wait_queue_len(&state, 1).await;

    let queued = state.read().queue[0].clone();
    assert_eq!(queued.song.title, "晴天");
    assert_eq!(queued.song.artist, "周杰伦");
    assert_eq!(queued.requested_by, "观众甲");
    assert_eq!(queued.requested_by_uid.as_deref(), Some("9527"));

    // /ws 上应能看到 danmaku 与 request 两种帧
    let mut saw_danmaku = false;
    let mut request_position = None;
    for _ in 0..6 {
        let frame = next_json(&mut socket).await;
        match frame["type"].as_str() {
            Some("danmaku") => saw_danmaku = true,
            Some("request") => {
                assert_eq!(frame["data"]["outcome"], "queued");
                assert_eq!(frame["data"]["title"], "晴天");
                request_position = frame["data"]["position"].as_u64();
                break;
            }
            _ => {}
        }
    }
    assert!(saw_danmaku, "应收到 danmaku 帧");
    assert_eq!(request_position, Some(1), "request 帧应带队内位置");

    // 记录里也应有一条 queued
    wait_recent_len(&state, 1).await;
    let stats: Value = reqwest::Client::new()
        .get(format!("{base}/api/requests"))
        .send()
        .await
        .expect("requests 请求失败")
        .json()
        .await
        .expect("requests 应为 JSON");
    assert_eq!(stats["recent"][0]["outcome"], "queued");
    assert_eq!(stats["recent"][0]["title"], "晴天");
}

#[tokio::test]
async fn non_command_danmaku_does_not_enter_queue() {
    let (base, state) = spawn_server().await;
    wait_ready(&base).await;

    assert!(simulate(&base, "主播好厉害", "路人", 1).await.is_success());
    // 给异步处理器一点时间，然后断言队列仍为空
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(state.read().queue.is_empty(), "普通弹幕不应入队");
    let dbg = state.read().stats.recent.clone();
    assert!(
        dbg.is_empty(),
        "普通弹幕不应产生请求记录，实际有 {} 条：{:?}",
        dbg.len(),
        dbg.iter().map(|r| (&r.user, &r.title, &r.outcome)).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn duplicate_and_cooldown_are_rejected_over_http() {
    let (base, state) = spawn_server().await;
    wait_ready(&base).await;

    // 默认规则：冷却 30 秒、不允许重复
    assert!(simulate(&base, "点歌 晴天 周杰伦", "甲", 1).await.is_success());
    wait_queue_len(&state, 1).await;

    // 同一用户立刻再点 → 冷却拒绝
    assert!(simulate(&base, "点歌 稻香", "甲", 1).await.is_success());
    wait_recent_len(&state, 2).await;
    let stats = state.read().stats.clone();
    assert_eq!(stats.recent[0].outcome, "rejected");
    assert_eq!(stats.recent[0].reason.as_deref(), Some("cooldown"));
    assert_eq!(state.read().queue.len(), 1, "冷却拒绝不应入队");

    // 另一用户点同一首 → 重复拒绝
    assert!(simulate(&base, "点歌 晴天 周杰伦", "乙", 2).await.is_success());
    wait_recent_len(&state, 3).await;
    let stats = state.read().stats.clone();
    assert_eq!(stats.recent[0].reason.as_deref(), Some("duplicate"));
    assert_eq!(state.read().queue.len(), 1);
}

#[tokio::test]
async fn queue_limit_is_enforced_over_http() {
    let (base, state) = spawn_server_with(|cfg| {
        cfg.server.port = 0;
        cfg.rules.cooldown_secs = 0; // 关掉冷却，专测上限
        cfg.rules.max_queue = 2;
        cfg.rules.allow_duplicate = true;
    })
    .await;
    wait_ready(&base).await;

    for (i, title) in ["歌一", "歌二", "歌三"].iter().enumerate() {
        assert!(simulate(&base, &format!("点歌 {title}"), "观众", i as u64 + 1)
            .await
            .is_success());
    }
    wait_recent_len(&state, 3).await;
    assert_eq!(state.read().queue.len(), 2, "超出上限的请求不应入队");
    let stats = state.read().stats.clone();
    assert_eq!(stats.recent[0].reason.as_deref(), Some("queue_full"));
}

#[tokio::test]
async fn request_regex_hot_update_applies_without_restart() {
    let (base, state) = spawn_server().await;
    wait_ready(&base).await;
    let client = reqwest::Client::new();

    // 默认正则不认「求歌」
    assert!(simulate(&base, "求歌 晴天", "甲", 1).await.is_success());
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(state.read().queue.is_empty());

    // 改配置：允许「求歌」，并关掉冷却/上限
    let mut config: Config = client
        .get(format!("{base}/api/config"))
        .send()
        .await
        .expect("读取配置失败")
        .json()
        .await
        .expect("配置应为 JSON");
    config.rules.command_regex = r"^求歌\s+(.+?)(?:\s+(.+))?$".to_string();
    config.rules.cooldown_secs = 0;
    let resp = client
        .put(format!("{base}/api/config"))
        .json(&config)
        .send()
        .await
        .expect("保存配置失败");
    assert!(resp.status().is_success(), "保存配置应成功：{}", resp.status());

    assert!(simulate(&base, "求歌 晴天 周杰伦", "甲", 1).await.is_success());
    wait_queue_len(&state, 1).await;
    assert_eq!(state.read().queue[0].song.title, "晴天");
}

// ─────────────────────────────────────────────────────────────────────────────
// 阶段 5：音乐平台适配
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn music_status_lists_both_platforms_logged_out() {
    let (base, _state) = spawn_server().await;
    wait_ready(&base).await;

    let status: Value = reqwest::Client::new()
        .get(format!("{base}/api/music/status"))
        .send()
        .await
        .expect("music status 请求失败")
        .json()
        .await
        .expect("应为 JSON");

    // 默认平台是 QQ 音乐（原唱曲库更全）；取不到直链时自动回退网易云
    assert_eq!(status["default_platform"], "qq");
    let platforms = status["platforms"].as_array().expect("应有 platforms 数组");
    assert_eq!(platforms.len(), 2);
    assert!(platforms.iter().all(|p| p["logged_in"] == false));
    assert_eq!(status["pending_resolution"], 0);
}

#[tokio::test]
async fn music_search_requires_keyword() {
    let (base, _state) = spawn_server().await;
    wait_ready(&base).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/api/music/search"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("search 请求失败");
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn music_search_builds_keyword_from_title_and_artist() {
    let (base, _state) = spawn_server().await;
    wait_ready(&base).await;

    // 只验证关键词拼接与响应结构；网络失败也应返回结构化错误而不是 500 崩溃
    // 显式指定平台，避免依赖「默认平台」的配置（默认已改为 QQ 音乐）
    let resp = reqwest::Client::new()
        .post(format!("{base}/api/music/search"))
        .json(&serde_json::json!({
            "title": "晴天", "artist": "周杰伦", "limit": 3, "platform": "netease"
        }))
        .send()
        .await
        .expect("search 请求失败");

    let status = resp.status();
    let body: Value = resp.json().await.expect("应为 JSON");
    if status.is_success() {
        assert_eq!(body["keyword"], "晴天 周杰伦");
        assert_eq!(body["platform"], "netease");
        assert!(body["results"].is_array());
    } else {
        // 无外网/风控时必须是可读错误，而不是空响应
        assert!(
            body["error"].as_str().map(|s| !s.is_empty()).unwrap_or(false),
            "失败时应有 error 字段：{body}"
        );
    }
}

#[tokio::test]
async fn saving_music_cookie_marks_logged_in_and_can_be_cleared() {
    let (base, _state) = spawn_server().await;
    wait_ready(&base).await;
    let client = reqwest::Client::new();

    let saved: Value = client
        .post(format!("{base}/api/music/cookie"))
        .json(&serde_json::json!({
            "platform": "netease",
            "cookie": "MUSIC_U=fake-token-for-test; __csrf=abc"
        }))
        .send()
        .await
        .expect("保存 Cookie 请求失败")
        .json()
        .await
        .expect("应为 JSON");
    assert_eq!(saved["logged_in"], true, "含 MUSIC_U 应判定为已登录");
    assert!(saved["message"].as_str().unwrap().contains("凭据"));

    let status: Value = client
        .get(format!("{base}/api/music/status"))
        .send()
        .await
        .expect("status 请求失败")
        .json()
        .await
        .expect("应为 JSON");
    let netease = status["platforms"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["platform"] == "netease")
        .expect("应有网易云条目");
    assert_eq!(netease["logged_in"], true);

    // 清除后恢复未登录
    client
        .post(format!("{base}/api/music/cookie/clear"))
        .json(&serde_json::json!({ "platform": "netease" }))
        .send()
        .await
        .expect("清除 Cookie 请求失败");
    let status2: Value = client
        .get(format!("{base}/api/music/status"))
        .send()
        .await
        .expect("status 请求失败")
        .json()
        .await
        .expect("应为 JSON");
    let netease2 = status2["platforms"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["platform"] == "netease")
        .unwrap();
    assert_eq!(netease2["logged_in"], false);
}

#[tokio::test]
async fn empty_cookie_is_rejected_over_http() {
    let (base, _state) = spawn_server().await;
    wait_ready(&base).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/api/music/cookie"))
        .json(&serde_json::json!({ "platform": "netease", "cookie": "   " }))
        .send()
        .await
        .expect("请求失败");
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn play_url_rejects_unresolved_queue_item() {
    let (base, state) = spawn_server().await;
    wait_ready(&base).await;

    // 入队一条占位歌曲（不接线解析器时它会保持 Pending）
    let item = crate::models::QueueItem::new(Song::placeholder("晴天", "周杰伦"), "观众", None);
    let item_id = item.id;
    state.mutate(|s| s.queue.push(item));
    assert_eq!(state.read().queue[0].song.source, SongSource::Pending);

    let resp = reqwest::Client::new()
        .post(format!("{base}/api/music/play-url"))
        .json(&serde_json::json!({ "item_id": item_id.to_string() }))
        .send()
        .await
        .expect("play-url 请求失败");
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: Value = resp.json().await.expect("应为 JSON");
    assert!(
        body["error"].as_str().unwrap().contains("尚未解析"),
        "应提示尚未解析：{}",
        body["error"]
    );
}

#[tokio::test]
async fn play_url_unknown_item_returns_404() {
    let (base, _state) = spawn_server().await;
    wait_ready(&base).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/api/music/play-url"))
        .json(&serde_json::json!({ "item_id": uuid::Uuid::new_v4().to_string() }))
        .send()
        .await
        .expect("play-url 请求失败");
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn placeholder_song_stays_pending_without_music_result() {
    // 解析失败路径：真实网络下「不存在的歌」会在几秒内被标记为 Failed。
    // 这里只验证占位歌曲的初始状态与序列化字段，避免测试依赖外网。
    let song = Song::placeholder("晴天", "周杰伦");
    assert_eq!(song.source, SongSource::Pending);
    assert!(!song.is_resolved());
    let json = serde_json::to_string(&song).expect("应可序列化");
    assert!(json.contains("\"source\":\"pending\""));
    assert!(json.contains("\"source_error\":null"));
}

#[tokio::test]
async fn default_platform_is_serialized_in_snake_case_for_frontend() {
    let (base, _state) = spawn_server().await;
    wait_ready(&base).await;
    let status: Value = reqwest::Client::new()
        .get(format!("{base}/api/music/status"))
        .send()
        .await
        .expect("请求失败")
        .json()
        .await
        .expect("应为 JSON");
    // 前端 types.ts 使用 'netease' | 'qq'
    let names: Vec<String> = status["platforms"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["platform"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"netease".to_string()));
    assert!(names.contains(&"qq".to_string()));
    assert_eq!(MusicPlatform::Netease.display_name(), "网易云音乐");
}

#[tokio::test]
async fn invalid_regex_is_rejected_by_config_endpoint() {    let (base, _state) = spawn_server().await;
    wait_ready(&base).await;
    let client = reqwest::Client::new();

    let mut config: Config = client
        .get(format!("{base}/api/config"))
        .send()
        .await
        .expect("读取配置失败")
        .json()
        .await
        .expect("配置应为 JSON");
    config.rules.command_regex = r"^点歌([".to_string();

    let resp = client
        .put(format!("{base}/api/config"))
        .json(&config)
        .send()
        .await
        .expect("保存配置失败");
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: Value = resp.json().await.expect("错误响应应为 JSON");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("正则非法"),
        "应提示正则非法：{}",
        body["error"]
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 阶段 9：黑名单 与 主播权限
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn blacklist_blocks_danmaku_but_not_host() {
    let (base, state) = spawn_server().await;
    wait_ready(&base).await;

    // 先拉黑《晴天 - 周杰伦》
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/api/blacklist"))
        .json(&serde_json::json!({ "title": "晴天", "artist": "周杰伦" }))
        .send()
        .await
        .expect("拉黑请求失败");
    assert!(resp.status().is_success(), "拉黑应成功，实际 {}", resp.status());

    // 观众点这首 -> 被拒（不是入队）
    assert!(simulate(&base, "点歌 晴天 周杰伦", "观众甲", 7)
        .await
        .is_success());
    // 等待点歌消费者处理
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let recent = state.read().stats.recent.clone();
    let rejected = recent
        .iter()
        .find(|r| r.title == "晴天")
        .expect("应有一条《晴天》的记录");
    assert_eq!(rejected.outcome, "rejected", "观众点被拉黑的歌应被拒");
    assert_eq!(rejected.reason.as_deref(), Some("blacklisted"));

    // 主播（点歌机）点这首 -> 允许入队
    assert!(simulate_as_host(&base, "点歌 晴天 周杰伦").await.is_success());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let recent = state.read().stats.recent.clone();
    assert!(
        recent
            .iter()
            .any(|r| r.title == "晴天" && r.outcome == "queued"),
        "主播应能无视黑名单，记录：{recent:?}"
    );
}

#[tokio::test]
async fn host_injection_uses_host_identity_and_priority() {
    let (base, state) = spawn_server().await;
    wait_ready(&base).await;

    // 不传 user：署名应为「主播」
    assert!(simulate_as_host(&base, "点歌 稻香 周杰伦").await.is_success());
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let guard = state.read();
    let item = guard
        .queue
        .iter()
        .chain(guard.current.iter())
        .find(|i| i.song.title == "稻香" || i.requested_by == "主播")
        .cloned();
    assert!(item.is_some(), "主播注入应产生队列条目");
    let item = item.unwrap();
    assert_eq!(item.requested_by, "主播", "点歌机注入的弹幕署名应为主播");
    assert_eq!(item.priority, crate::models::QueuePriority::Host);
}

// ── 播放列表清理与边界提示（阶段 10d）──────────────────────────────────

#[tokio::test]
async fn clear_playlist_wipes_queue_and_playing_but_keeps_idle() {
    let (base, state) = spawn_server().await;
    wait_ready(&base).await;

    // 造出「点歌队列 + 播放序列 + 空闲歌单」三者都有内容的状态
    state.mutate(|s| {
        s.queue.push(song_item("待播的"));
        s.playing.push(song_item("播过的"));
        s.playing.push(song_item("正在播的"));
        s.cursor = 1;
        s.current = s.playing.get(1).cloned();
        s.idle.push(song_item("空闲曲"));
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/api/queue/clear-playlist"))
        .send()
        .await
        .expect("请求失败");
    assert!(resp.status().is_success(), "清空播放列表应成功");

    let guard = state.read();
    assert!(guard.queue.is_empty(), "点歌队列应被清空");
    assert!(guard.playing.is_empty(), "播放序列应被清空");
    assert!(guard.current.is_none(), "当前曲目应被清空");
    assert_eq!(guard.cursor, 0);
    assert!(!guard.player.playing, "清空后不应仍标记为在播");
    assert_eq!(guard.idle.len(), 1, "空闲歌单必须保留（它是主播的曲库）");
}

#[tokio::test]
async fn previous_on_first_song_returns_notice_not_error() {
    // 已经是第一首时，「上一首」是正常边界：后端应回 200 + `{"notice": ...}`，
    // 而不是非 2xx（界面会弹「请求 … 失败」，用户以为坏了）。
    //
    // 需要真实接线的播放器才能走到该分支，因此用 `with_player` + `MockPlayer`
    // 起一个带播放器的服务器（`spawn_server` 是不带播放器的占位模式）。
    let state = StateCell::new(AppState::new(
        VERSION,
        PlayerState::default(),
        BilibiliState::default(),
    ));
    state.mutate(|s| {
        s.playing.push(song_item("唯一一首"));
        s.cursor = 0;
        s.current = s.playing.first().cloned();
    });

    let mock = Arc::new(crate::player::MockPlayer::new());
    let ctx = ServerCtx::with_player(
        state.clone(),
        EventBus::default(),
        Config::default(),
        DanmakuHandle::new(state.clone(), DanmakuBroadcaster::default()),
        test_config_path(),
        test_music_service(),
        Some(mock as Arc<dyn crate::player::PlayerBackend>),
        None,
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let router = server::build_router(Arc::clone(&ctx));
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let base = format!("http://127.0.0.1:{port}");
    wait_ready(&base).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/api/player/previous"))
        .send()
        .await
        .expect("请求失败");
    assert_eq!(resp.status().as_u16(), 200, "边界情况不应返回错误码");
    let body: Value = resp.json().await.expect("应返回 JSON");
    assert!(
        body.get("notice").is_some(),
        "应返回 notice 提示而不是 error：{body}"
    );

    // 状态不能被改动
    let guard = state.read();
    assert_eq!(guard.cursor, 0, "边界情况下游标不应移动");
    assert!(guard.current.is_some(), "边界情况下不应清空当前曲目");
}

/// 构造一条已解析的测试曲目。
fn song_item(title: &str) -> crate::models::QueueItem {
    let mut song = Song::placeholder(title, "测试歌手");
    song.platform = MusicPlatform::Qq;
    song.source = SongSource::Resolved;
    song.duration = 180;
    crate::models::QueueItem::new(song, "观众", None)
}
