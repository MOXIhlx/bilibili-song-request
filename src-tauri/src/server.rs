//! 内嵌 axum HTTP / WebSocket 服务器。
//!
//! 路由分组：`/health`、`/api/state`、`/api/queue/*`、`/api/idle/*`、`/api/music/*`、
//! `/api/bilibili/*`、`/api/config`、`/api/player/*`、`/api/panel/*`、`/api/blacklist`；
//! 页面 `/`、`/panel`（含 `/panel/play|lyrics|danmaku`）、`/dashboard`；
//! 静态资源 `/assets/*`、`/bg/{name}`；实时推送 `/ws`。
//!
//! 安全：默认只监听 `127.0.0.1`。CORS 允许任意来源，因为服务本身不可从外部访问，
//! 而 Tauri 的 WebView 来源是 `tauri://localhost`，必须放行。

pub mod templates;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path as AxumPath, Query, State, WebSocketUpgrade};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;
// 只为拿 `oneshot`：fallback 里要对同一个 router 重新分发一次请求
use tower::ServiceExt as _;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;
use tracing::{debug, error, info, warn};

use crate::bilibili::danmaku::DanmakuEvent;
use crate::bilibili::{AuthConfig, DanmakuHandle};
use crate::config::Config;
use crate::event::HubEvent;
use crate::models::AppState;
use crate::music::{MusicService, SongResolver};
use crate::queue;
use crate::queue::blacklist::BlacklistEntry;
use crate::queue::{EnqueueOutcome, SongRequestService};
use crate::state::{EventBus, StateCell};
use crate::webdist;

/// 服务器共享上下文。
pub struct ServerCtx {
    /// 全局状态（含 WS 广播通道）。
    pub state: StateCell,
    /// 内部事件总线。
    pub events: EventBus,
    /// 当前生效的配置；保存配置时整体替换。
    /// 用 `Arc` 包裹使点歌服务能通过闭包读到最新规则。
    pub config: Arc<std::sync::RwLock<Config>>,
    /// B 站弹幕连接控制器（单连接）。
    pub bilibili: DanmakuHandle,
    /// 点歌请求服务（弹幕指令 → 队列）。
    pub requests: Arc<SongRequestService>,
    /// 音乐平台服务（阶段 5）。
    pub music: Arc<MusicService>,
    /// 播放控制器（阶段 6）；未接线播放器时为 `None`（占位模式）。
    pub player: Option<Arc<crate::player::PlayerController>>,
    /// 配置文件落盘路径。
    ///
    /// 显式传入而不是用 `Config::save()` 的全局路径，测试才能隔离、不污染真实配置。
    pub config_path: std::path::PathBuf,
}

impl ServerCtx {
    /// 创建上下文（配置写到默认位置）。
    pub fn new(
        state: StateCell,
        events: EventBus,
        config: Config,
        bilibili: DanmakuHandle,
    ) -> Arc<Self> {
        Self::with_config_path(state, events, config, bilibili, Config::config_path())
    }

    /// 创建上下文并指定配置落盘路径（测试用）。
    ///
    /// 音乐服务在此创建并挂上曲目解析器，使占位歌曲能被异步解析为真实曲目。
    pub fn with_config_path(
        state: StateCell,
        events: EventBus,
        config: Config,
        bilibili: DanmakuHandle,
        config_path: std::path::PathBuf,
    ) -> Arc<Self> {
        Self::with_services(
            state,
            events,
            config,
            bilibili,
            config_path,
            Arc::new(MusicService::new()),
        )
    }

    /// 创建上下文并注入音乐服务（测试可注入隔离的凭据目录）。
    /// 播放器未接线，播放类 API 走「占位」分支（只改状态）。
    pub fn with_services(
        state: StateCell,
        events: EventBus,
        config: Config,
        bilibili: DanmakuHandle,
        config_path: std::path::PathBuf,
        music: Arc<MusicService>,
    ) -> Arc<Self> {
        Self::with_player(state, events, config, bilibili, config_path, music, None, None)
    }

    /// 创建上下文并注入播放器后端（生产路径：mpv）。
    ///
    /// 传入 `player_events` 时自动接管播放事件循环（进度同步 + 自动下一首）。
    #[allow(clippy::too_many_arguments)]
    pub fn with_player(
        state: StateCell,
        events: EventBus,
        config: Config,
        bilibili: DanmakuHandle,
        config_path: std::path::PathBuf,
        music: Arc<MusicService>,
        backend: Option<Arc<dyn crate::player::PlayerBackend>>,
        player_events: Option<tokio::sync::broadcast::Receiver<crate::player::PlayerEvent>>,
    ) -> Arc<Self> {
        let shared_config = Arc::new(std::sync::RwLock::new(config));

        // 曲目解析器：单独任务串行消费搜索请求。
        // 用 `spawn` 而不是 `tokio::spawn(resolver.run())`：解析器自己持有发送端，
        // 接收端不会因外部发送端析构而关闭，必须由句柄显式管理生命周期。
        let resolve_sender = SongResolver::new(state.clone(), Arc::clone(&music)).spawn();

        let requests = {
            let shared = Arc::clone(&shared_config);
            // 点歌日志放在**配置文件同目录**，而不是硬编码用户配置目录：
            // 否则测试（用临时 config_path）会读写用户真实的 requests.jsonl，
            // 既污染用户数据、又让「历史日志」相关的断言随机失败。
            let log_path = config_path
                .parent()
                .map(|dir| dir.join("requests.jsonl"))
                .unwrap_or_else(|| crate::config::Config::config_dir().join("requests.jsonl"));
            SongRequestService::from_config(state.clone(), move || {
                shared.read().unwrap_or_else(|e| e.into_inner()).clone()
            })
            .with_log_path(log_path)
            .with_resolver(resolve_sender)
        };

        let player = backend.map(|backend| {
            // 每次取址/开播时读取当前配置：优先级策略与点歌规则改完立即生效。
            let shared = Arc::clone(&shared_config);
            let policy = {
                let shared = Arc::clone(&shared);
                move || shared.read().unwrap_or_else(|e| e.into_inner()).rules.pick_policy
            };
            let rules = move || shared.read().unwrap_or_else(|e| e.into_inner()).rules.clone();
            match player_events {
                Some(receiver) => crate::player::PlayerController::with_event_loop_and_config(
                    state.clone(),
                    backend,
                    Arc::clone(&music),
                    receiver,
                    policy,
                    rules,
                ),
                None => crate::player::PlayerController::with_config(
                    state.clone(),
                    backend,
                    Arc::clone(&music),
                    policy,
                    rules,
                ),
            }
        });

        Arc::new(Self {
            state,
            events,
            config: shared_config,
            bilibili,
            requests: Arc::new(requests),
            music,
            player,
            config_path,
        })
    }

    /// 读取配置副本。
    pub fn config(&self) -> Config {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// 启动内嵌服务器。
///
/// 端口取自 `config.server.port`；传入 `0` 时由系统分配（便于测试），
/// 返回值中的 `port` 是实际监听端口。
pub async fn serve(
    state: StateCell,
    events: EventBus,
    config: Config,
    backend: Option<Arc<dyn crate::player::PlayerBackend>>,
    player_events: Option<tokio::sync::broadcast::Receiver<crate::player::PlayerEvent>>,
) -> anyhow::Result<(
    u16,
    Arc<ServerCtx>,
    DanmakuHandle,
    Arc<MusicService>,
    Option<Arc<crate::player::PlayerController>>,
    JoinHandle<()>,
)> {
    let port = config.server.port;
    let host = config.server.host.clone();
    let addr = format!("{host}:{port}");

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| anyhow::anyhow!("内嵌服务器无法绑定 {addr}：{e}"))?;
    let actual_port = listener.local_addr()?.port();

    // 弹幕事件 → hub 广播（面板 / 控制台的 /ws）
    let bilibili = DanmakuHandle::new(state.clone(), crate::bilibili::DanmakuBroadcaster::default());
    spawn_danmaku_forwarder(&bilibili, &state);

    let ctx = ServerCtx::with_player(
        state,
        events,
        config,
        bilibili.clone(),
        Config::config_path(),
        Arc::new(MusicService::new()),
        backend,
        player_events,
    );
    // 弹幕 → 点歌指令 → 队列（阶段 4）
    spawn_request_consumer(&ctx);
    // 曲目解析完成 → 自动开播（「点歌即播」闭环）
    spawn_autoplay_consumer(&ctx);
    // 启动时没有待播内容 → 用空闲歌单开台（阶段 10a）
    spawn_idle_bootstrap(&ctx);
    let router = build_router(Arc::clone(&ctx));

    info!(%addr, actual_port, "内嵌 HTTP/WebSocket 服务器已就绪");

    let shutdown_player = ctx.player.clone();
    let handle = tokio::spawn(async move {
        // 收到退出信号时显式关闭播放器。
        //
        // 不能只依赖 `kill_on_drop`：进程被强制终止时 Drop 不会执行，会留下孤立的 mpv。
        tokio::select! {
            result = axum::serve(listener, router) => {
                if let Err(err) = result {
                    error!(error = %err, "内嵌服务器异常退出");
                }
            }
            _ = crate::shutdown::wait() => {
                info!("收到退出信号，正在关闭播放器");
                if let Some(player) = shutdown_player.as_ref() {
                    if let Err(err) = player.backend().shutdown().await {
                        warn!(error = %err, "关闭播放器时出错");
                    }
                }
            }
        }
    });

    let music = Arc::clone(&ctx.music);
    let player = ctx.player.clone();
    Ok((actual_port, ctx, bilibili, music, player, handle))
}

/// 消费弹幕事件，把点歌指令变成队列条目。
///
/// 独立任务：队列写入（锁 + 规则判断）不应阻塞弹幕的读取与转发。
pub fn spawn_request_consumer(ctx: &Arc<ServerCtx>) {
    let mut rx = ctx.bilibili.broadcaster().subscribe();
    let requests = Arc::clone(&ctx.requests);
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(DanmakuEvent::Message(danmaku)) => {
                    // 注入的弹幕带 `from_host` 标记，按主播权限处理；真实观众走观众规则。
                    let priority = if danmaku.from_host {
                        crate::models::QueuePriority::Host
                    } else {
                        crate::models::QueuePriority::Danmaku
                    };
                    // 返回值已包含日志与广播（在 SongRequestService 内部完成）。
                    match requests.handle_with_priority(&danmaku, priority) {
                        EnqueueOutcome::NotARequest => {}
                        EnqueueOutcome::Queued { .. } => {}
                        EnqueueOutcome::Rejected(_) => {}
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!(skipped, "点歌处理器落后于弹幕流，已丢弃部分弹幕");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// 启动时如果没有任何待播内容，就用空闲歌单开台。
///
/// 「队列空了播空闲歌单」的逻辑挂在 `advance()` 里，而它只在上一首播完时被调用；
/// 程序启动时队列为空、也没有「播完」事件，空闲歌单不会自己开始，所以这里补一次检查。
/// 等几秒让 mpv 与解析器就绪，仅当「当前没歌 + 点歌队列空 + 空闲歌单有可播曲目」时开台。
pub fn spawn_idle_bootstrap(ctx: &Arc<ServerCtx>) {
    let Some(player) = ctx.player.clone() else {
        return;
    };
    let state = ctx.state.clone();
    tokio::spawn(async move {
        // 给 mpv 与解析器一点启动时间，也避开启动时的状态恢复
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;

        let (has_current, queue_len, idle_len, ready) = {
            let guard = state.read();
            (
                guard.current.is_some(),
                guard.queue.len(),
                guard.idle.len(),
                // 空闲歌单里至少要有一首能播的（已解析或还在解析中）
                guard
                    .idle
                    .iter()
                    .any(|i| i.song.source != crate::models::SongSource::Failed),
            )
        };
        if has_current || queue_len > 0 || idle_len == 0 || !ready {
            return;
        }

        info!(idle_len, "启动时无待播内容，用空闲歌单开台");
        match player.start_next().await {
            Ok(Some(item)) => info!(title = %item.song.title, "空闲歌单已开始播放"),
            Ok(None) => debug!("空闲歌单没有可播的曲目"),
            Err(err) => warn!(error = %err, "空闲歌单开台失败"),
        }
    });
}

/// 曲目解析完成后自动开播（点歌即播闭环）。
///
/// `HubEvent::SongResolved` 原本没有任何订阅者，每首点歌都只躺在队列里，
/// 必须手动点「播放」才会出声，表现就是「点歌显示了，但播放器没有声音」。
///
/// 职责很小：只在当前空闲时启动下一首；正在播歌时不打断，新歌排队等待。
pub fn spawn_autoplay_consumer(ctx: &Arc<ServerCtx>) {
    let Some(player) = ctx.player.clone() else {
        // 没有播放器（未装 mpv）时不需要自动播放
        return;
    };
    // 订阅需要 `'static`：把状态单元克隆进任务，而不是借用 `ctx`。
    let state = ctx.state.clone();
    let mut rx = state.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                // 有人点歌入队：此时才可能在「空闲歌曲」与「点歌队列」之间切换。
                // 必须用入队事件而不是解析完成事件——解析要几百毫秒到几秒，
                // 届时空闲那首可能已播完、空闲歌单已顶上下一首，「放完再切」的策略会失效。
                Ok(HubEvent::RequestQueued { .. }) => {
                    // `on_song_requested` 只在「在播空闲歌曲」+「策略=立即切」时才真的打断。
                    if player.on_song_requested().await {
                        info!("已按配置切到点歌队列");
                    }
                }
                Ok(HubEvent::SongResolved { item_id, title, .. }) => {
                    // 解析器把真实曲目就地写回 `playing`（而不是 `current`），前端只读 `current`。
                    // 这里补一次同步，否则界面会一直显示解析前的占位歌名。
                    player.refresh_current_from_playing();

                    // 用 mpv 的真实状态判断空闲；乐观的 `status()` 在直链打不开时会卡在 Playing。
                    if !player.is_really_idle().await {
                        debug!(%title, "已有曲目在播，新解析完成的歌曲排队等待");
                        continue;
                    }
                    info!(%title, item_id = %item_id, "解析完成，自动开始播放");
                    match player.start_next().await {
                        Ok(Some(item)) => {
                            info!(title = %item.song.title, "自动播放已开始");
                        }
                        Ok(None) => debug!(%title, "队列已空，无需自动播放"),
                        Err(err) => warn!(%title, error = %err, "自动播放失败"),
                    }
                }
                // 状态事件量很大，这里不关心
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    debug!(skipped, "自动播放消费者落后于事件流");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// 把弹幕客户端产生的事件转发到 WS 广播通道。
/// 状态变化时额外补发一次全量快照，让面板立刻更新连接状态与错误信息。
pub fn spawn_danmaku_forwarder(handle: &DanmakuHandle, state: &StateCell) {
    let mut rx = handle.broadcaster().subscribe();
    let state = state.clone();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => match event {
                    DanmakuEvent::Message(danmaku) => {
                        state.publish(HubEvent::Danmaku(danmaku));
                    }
                    DanmakuEvent::Status(status) => {
                        debug!(?status, "弹幕连接状态变化");
                        state.broadcast_state();
                    }
                    DanmakuEvent::Error(message) => {
                        warn!(%message, "弹幕连接错误");
                        state.broadcast_state();
                    }
                    // 原始报文只用于排障，不进 WS（避免面板被刷爆）。
                    DanmakuEvent::Raw(_) => {}
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!(skipped, "弹幕事件转发落后，补发状态快照");
                    state.broadcast_state();
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// 组装路由。
pub fn build_router(ctx: Arc<ServerCtx>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let mut router: Router<Arc<ServerCtx>> = Router::new()
        .route("/", get(root_page))
        .route("/health", get(health))
        .route("/api/state", get(api_state))
        .route("/api/queue", get(api_queue))
        .route("/api/requests", get(api_requests))
        .route("/api/requests/log", get(api_requests_log))
        .route("/api/queue/clear", post(api_queue_clear))
        .route(
            "/api/queue/clear-playlist",
            post(api_queue_clear_playlist),
        )
        .route(
            "/api/queue/clear-and-play-idle",
            post(api_queue_clear_and_play_idle),
        )
        .route("/api/queue/add", post(api_queue_add))
        .route("/api/queue/{id}/{action}", post(api_queue_action))
        .route("/api/bilibili/connect", post(api_bilibili_connect))
        .route("/api/bilibili/disconnect", post(api_bilibili_disconnect))
        .route("/api/bilibili/status", get(api_bilibili_status))
        .route("/api/bilibili/simulate", post(api_bilibili_simulate))
        .route("/api/music/status", get(api_music_status))
        .route("/api/music/search", post(api_music_search))
        .route("/api/music/cookie", post(api_music_cookie))
        .route("/api/music/cookie/clear", post(api_music_cookie_clear))
        .route("/api/music/play-url", post(api_music_play_url))
        .route(
            "/api/idle",
            get(api_idle_get).post(api_idle_add).delete(api_idle_remove),
        )
        .route("/api/idle/mode", post(api_idle_mode))
        .route("/api/idle/play", post(api_idle_play))
        .route("/api/idle/clear", post(api_idle_clear))
        .route("/api/idle/import", post(api_idle_import))
        .route("/api/music/playlists", get(api_music_playlists))
        .route("/api/config", get(api_config_get).put(api_config_put))
        .route("/api/player/pause", post(api_player_pause))
        .route("/api/player/play", post(api_player_play))
        .route("/api/player/status", get(api_player_status))
        .route("/api/player/volume", post(api_player_volume))
        .route("/api/player/skip", post(api_player_skip))
        .route("/api/player/seek", post(api_player_seek))
        .route("/api/player/previous", post(api_player_previous))
        .route("/api/player/replay", post(api_player_replay))
        .route("/api/player/mode", post(api_player_mode))
        .route(
            "/api/panel/background",
            post(api_panel_background).get(api_panel_backgrounds),
        )
        .route(
            "/api/panel/background/rename",
            post(api_panel_background_rename),
        )
        .route(
            "/api/panel/background/delete",
            post(api_panel_background_delete),
        )
        .route("/bg/{name}", get(bg_file))
        .route(
            "/api/blacklist",
            get(api_blacklist)
                .post(api_blacklist_add)
                .delete(api_blacklist_remove),
        )
        .route("/panel", get(page_panel))
        // 专注页：与 `/panel` 共用 SPA 外壳。
        // 必须显式注册，否则 OBS 直连 `/panel/lyrics` 会落到 fallback 的 404。
        .route("/panel/play", get(page_panel))
        .route("/panel/lyrics", get(page_panel))
        .route("/panel/danmaku", get(page_panel))
        .route("/dashboard", get(page_dashboard))
        .route("/ws", get(ws_handler))
        .fallback(not_found)
        .layer(cors)
        .layer(SetResponseHeaderLayer::overriding(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ));

    // 前端构建产物；dist 不存在时退化为提示页而不是 500。
    if let Some(dist) = webdist::dist_path() {
        router = router.nest_service("/assets", ServeDir::new(dist.join("assets")));
        info!(dir = %dist.display(), "已挂载前端静态资源");
    } else {
        warn!("未找到 dist 目录，/panel 与 /dashboard 将显示构建提示页");
        router = router.route("/assets/{*path}", get(assets_missing));
    }

    // 尾斜杠容错：`/api/player/skip/` 这类请求会落到 fallback。
    //
    // axum 的路由是精确匹配的，尾斜杠不命中 `/api/player/skip`，于是直接 404；
    // 前端只要拼了一处尾斜杠，报错就只是「请求 /api/player/skip/ 失败」，极难定位。
    //
    // 不能用 `NormalizePathLayer` / `Router::layer` 中间件：实测（带 debug 日志确认）
    // `Router::layer` 的中间件在路由匹配之后才执行，路径虽被改写为 `/api/player/skip`，
    // 匹配早已完成，响应依旧是 404；而 `axum::serve` 需要 make-service，
    // 普通 `ServiceBuilder` 产物也不满足约束。
    //
    // 因此 fallback 是唯一能真正重新分发的地方：修剪尾斜杠后用同一个 router 再 `oneshot`。
    let router = router.with_state(Arc::clone(&ctx));
    let redispatch = router.clone();
    router.fallback(move |req: axum::extract::Request| {
        let router = redispatch.clone();
        async move {
            let path = req.uri().path().to_string();
            if path.len() > 1 && path.ends_with('/') {
                let trimmed = path.trim_end_matches('/');
                let new_path = if trimmed.is_empty() { "/" } else { trimmed };
                let path_and_query = match req.uri().query() {
                    Some(q) => format!("{new_path}?{q}"),
                    None => new_path.to_string(),
                };
                if let Ok(pq) = path_and_query.parse::<axum::http::uri::PathAndQuery>() {
                    let mut parts = req.uri().clone().into_parts();
                    parts.path_and_query = Some(pq);
                    if let Ok(uri) = axum::http::Uri::from_parts(parts) {
                        let (mut parts, body) = req.into_parts();
                        parts.uri = uri;
                        debug!(from = %path, to = %new_path, "尾斜杠已归一化，重新分发");
                        let rebuilt = axum::http::Request::from_parts(parts, body);
                        // 再匹配一次；仍然 404 才真的返回 404
                        return match router.oneshot(rebuilt).await {
                            Ok(response) => response,
                            Err(never) => match never {},
                        };
                    }
                }
            }
            not_found().await
        }
    })
}

// ──────────────────────────── 基础页面 ────────────────────────────

/// 根路径：给出可用地址列表，避免主播打开 `http://127.0.0.1:17777/` 看到 404。
async fn root_page(State(ctx): State<Arc<ServerCtx>>) -> Html<String> {
    let cfg = ctx.config();
    let panel = format!("{}/panel?bg=transparent&theme=dark", cfg.base_url());
    let dashboard = format!("{}/dashboard", cfg.base_url());
    Html(templates::error_page(
        "弹幕点歌机本地服务运行中",
        &format!(
            r#"<p>OBS 浏览器源地址（复制到 OBS 的「浏览器源」）：</p>
<pre>{panel}</pre>
<p>主播控制台（浏览器打开）：</p>
<pre>{dashboard}</pre>
<p>健康检查：<code>/health</code>　实时推送：<code>/ws</code></p>"#
        ),
    ))
}

/// 健康检查。
async fn health(State(ctx): State<Arc<ServerCtx>>) -> Json<HealthResponse> {
    let state = ctx.state.read();
    let uptime = (chrono::Utc::now() - state.started_at).num_seconds().max(0);
    Json(HealthResponse {
        status: "ok",
        version: state.version.clone(),
        uptime_secs: uptime,
    })
}

/// `/health` 响应体。
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    /// 固定为 `ok`。
    pub status: &'static str,
    /// 应用版本。
    pub version: String,
    /// 运行时长（秒）。
    pub uptime_secs: i64,
}

/// OBS 面板页。
async fn page_panel(State(ctx): State<Arc<ServerCtx>>) -> Html<String> {
    let base = ctx.config().base_url();
    Html(webdist::render_panel(Some(&base)))
}

/// 浏览器控制台页。
async fn page_dashboard(State(ctx): State<Arc<ServerCtx>>) -> Html<String> {
    let base = ctx.config().base_url();
    Html(webdist::render_dashboard(Some(&base)))
}

/// 未构建前端时的静态资源占位响应。
async fn assets_missing() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [("content-type", "text/plain; charset=utf-8")],
        "前端资源尚未构建：请在项目根目录执行 npm run build",
    )
        .into_response()
}

/// 兜底 404。
async fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Html(templates::error_page(
            "404 未找到",
            "<p>可用页面：<code>/panel</code>、<code>/dashboard</code>、<code>/health</code>、<code>/api/state</code>、<code>/ws</code></p>",
        )),
    )
        .into_response()
}

// ──────────────────────────── 音乐平台 API ────────────────────────────

/// `GET /api/music/status` 响应。
#[derive(Debug, Serialize)]
pub struct MusicStatusResponse {
    /// 默认平台。
    pub default_platform: crate::models::MusicPlatform,
    /// 各平台登录状态。
    pub platforms: Vec<crate::music::PlatformStatus>,
    /// 队列中尚未解析完成的条目数。
    pub pending_resolution: usize,
}

/// `GET /api/music/status`
async fn api_music_status(State(ctx): State<Arc<ServerCtx>>) -> Json<MusicStatusResponse> {
    let pending = ctx
        .state
        .read()
        .queue
        .iter()
        .filter(|item| !item.song.is_resolved())
        .count();
    Json(MusicStatusResponse {
        default_platform: ctx.music.default_platform().await,
        platforms: ctx.music.all_status().await,
        pending_resolution: pending,
    })
}

/// `POST /api/music/search` 请求体。
#[derive(Debug, Deserialize)]
pub struct MusicSearchRequest {
    /// 关键词；为空时可用 `title` + `artist` 拼接。
    #[serde(default)]
    pub keyword: Option<String>,
    /// 歌名。
    #[serde(default)]
    pub title: Option<String>,
    /// 歌手。
    #[serde(default)]
    pub artist: Option<String>,
    /// 指定平台，默认取服务默认平台。
    #[serde(default)]
    pub platform: Option<crate::models::MusicPlatform>,
    /// 返回条数上限，默认 10。
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `POST /api/music/search` 响应。
#[derive(Debug, Serialize)]
pub struct MusicSearchResponse {
    /// 实际使用的平台。
    pub platform: crate::models::MusicPlatform,
    /// 实际使用的关键词。
    pub keyword: String,
    /// 搜索结果。
    pub results: Vec<crate::models::Song>,
}

/// `POST /api/music/search`
async fn api_music_search(
    State(ctx): State<Arc<ServerCtx>>,
    Json(body): Json<MusicSearchRequest>,
) -> Result<Json<MusicSearchResponse>, ApiError> {
    let keyword = match body.keyword {
        Some(k) if !k.trim().is_empty() => k.trim().to_string(),
        _ => {
            let title = body.title.unwrap_or_default();
            let artist = body.artist.unwrap_or_default();
            let joined = format!("{} {}", title.trim(), artist.trim());
            joined.trim().to_string()
        }
    };
    if keyword.is_empty() {
        return Err(ApiError::bad_request("请提供 keyword 或 title"));
    }

    let platform = match body.platform {
        Some(p) => p,
        None => ctx.music.default_platform().await,
    };
    let limit = body.limit.unwrap_or(10).clamp(1, 50);

    let results = ctx
        .music
        .search(platform, &keyword)
        .await
        .map_err(music_error_to_api)?;

    Ok(Json(MusicSearchResponse {
        platform,
        keyword,
        results: results.into_iter().take(limit).collect(),
    }))
}

/// `POST /api/music/cookie` 请求体。
#[derive(Debug, Deserialize)]
pub struct MusicCookieRequest {
    /// 平台。
    pub platform: crate::models::MusicPlatform,
    /// 完整 Cookie 串（例如 `MUSIC_U=...; __csrf=...`）。
    pub cookie: String,
}

/// `POST /api/music/cookie` 响应。
#[derive(Debug, Serialize)]
pub struct MusicCookieResponse {
    /// 平台。
    pub platform: crate::models::MusicPlatform,
    /// 存放位置（凭据库 / 受限文件）。
    pub stored_in: crate::music::StoredIn,
    /// 给用户看的说明。
    pub message: String,
    /// 保存后是否判定为已登录。
    pub logged_in: bool,
}

/// `POST /api/music/cookie` —— 保存 Cookie（由内嵌登录窗口抓取后调用）。
async fn api_music_cookie(
    State(ctx): State<Arc<ServerCtx>>,
    Json(body): Json<MusicCookieRequest>,
) -> Result<Json<MusicCookieResponse>, ApiError> {
    let stored_in = ctx
        .music
        .save_cookie(body.platform, &body.cookie)
        .await
        .map_err(|e| ApiError::bad_request(format!("保存 Cookie 失败：{e}")))?;
    let status = ctx.music.status(body.platform).await;

    info!(
        platform = ?body.platform,
        stored_in = ?stored_in,
        logged_in = status.logged_in,
        "音乐平台 Cookie 已保存"
    );

    Ok(Json(MusicCookieResponse {
        platform: body.platform,
        stored_in,
        message: stored_in.describe().to_string(),
        logged_in: status.logged_in,
    }))
}

/// `POST /api/music/cookie/clear` 请求体。
#[derive(Debug, Deserialize)]
pub struct MusicClearRequest {
    /// 平台。
    pub platform: crate::models::MusicPlatform,
}

/// `POST /api/music/cookie/clear`
async fn api_music_cookie_clear(
    State(ctx): State<Arc<ServerCtx>>,
    Json(body): Json<MusicClearRequest>,
) -> Result<Json<MusicStatusResponse>, ApiError> {
    ctx.music
        .clear_cookie(body.platform)
        .await
        .map_err(|e| ApiError::internal(format!("清除 Cookie 失败：{e}")))?;
    Ok(Json(api_music_status(State(ctx)).await.0))
}

/// `POST /api/music/play-url` 请求体。
#[derive(Debug, Deserialize)]
pub struct PlayUrlRequest {
    /// 队列条目 ID 或歌曲 ID（二选一）。
    #[serde(default)]
    pub item_id: Option<uuid::Uuid>,
    /// 歌曲 ID（直接指定平台曲目时使用）。
    #[serde(default)]
    pub song_id: Option<String>,
    /// 平台，默认从队列条目取。
    #[serde(default)]
    pub platform: Option<crate::models::MusicPlatform>,
}

/// `POST /api/music/play-url` 响应。
#[derive(Debug, Serialize)]
pub struct PlayUrlResponse {
    /// 歌曲 ID。
    pub song_id: String,
    /// 平台。
    pub platform: crate::models::MusicPlatform,
    /// 可直接播放的地址。
    pub url: String,
    /// 歌名（便于前端提示）。
    pub title: String,
    /// 歌手。
    pub artist: String,
}

/// `POST /api/music/play-url` —— 取音频直链（阶段 6 的 mpv 会用它）。
async fn api_music_play_url(
    State(ctx): State<Arc<ServerCtx>>,
    Json(body): Json<PlayUrlRequest>,
) -> Result<Json<PlayUrlResponse>, ApiError> {
    // 从队列条目解析歌曲信息
    let (song_id, platform, title, artist) = if let Some(item_id) = body.item_id {
        let item = ctx
            .state
            .read()
            .queue
            .iter()
            .find(|i| i.id == item_id)
            .cloned()
            .ok_or_else(|| ApiError::not_found(format!("队列项不存在：{item_id}")))?;
        if !item.song.is_resolved() {
            return Err(ApiError::bad_request(format!(
                "《{}》尚未解析出曲目信息，请稍后重试",
                item.song.title
            )));
        }
        (
            item.song.id.clone(),
            body.platform.unwrap_or(item.song.platform),
            item.song.title.clone(),
            item.song.artist.clone(),
        )
    } else if let Some(song_id) = body.song_id {
        let platform = body.platform.unwrap_or(crate::models::MusicPlatform::Netease);
        (song_id, platform, String::new(), String::new())
    } else {
        return Err(ApiError::bad_request("请提供 item_id 或 song_id"));
    };

    let url = ctx
        .music
        .play_url(platform, &song_id)
        .await
        .map_err(music_error_to_api)?;

    Ok(Json(PlayUrlResponse {
        song_id,
        platform,
        url,
        title,
        artist,
    }))
}

/// 音乐错误 → HTTP 错误。
fn music_error_to_api(err: crate::music::MusicError) -> ApiError {
    use crate::music::MusicError;
    match err {
        // 未登录：409 更贴切（状态问题而非参数问题）
        MusicError::NotLoggedIn(platform) => {
            ApiError::conflict(format!("未登录{platform}，请先在控制台登录"))
        }
        MusicError::NotFound(keyword) => ApiError::not_found(format!("没有搜索到《{keyword}》")),
        MusicError::Unplayable => {
            ApiError::conflict("该歌曲需要登录或受版权限制，无法获取播放地址".to_string())
        }
        MusicError::Network(message) => ApiError::bad_gateway(format!("请求音乐平台失败：{message}")),
        MusicError::ApiChanged(message) => {
            ApiError::bad_gateway(format!("音乐平台接口已变化：{message}"))
        }
        MusicError::Unimplemented(what) => ApiError::not_implemented(what.to_string()),
    }
}

// ──────────────────────────── B 站弹幕连接 API ────────────────────────────

/// `POST /api/bilibili/connect` 请求体；字段全部可选，未传的沿用已保存配置。
#[derive(Debug, Default, Deserialize)]
pub struct ConnectRequest {
    /// 开放平台应用 ID。
    #[serde(default)]
    pub app_id: Option<String>,
    /// 访问密钥 ID。
    #[serde(default)]
    pub access_key_id: Option<String>,
    /// 访问密钥 Secret。
    #[serde(default)]
    pub access_key_secret: Option<String>,
    /// 身份码。
    #[serde(default)]
    pub code: Option<String>,
}

/// `POST /api/bilibili/connect` 响应体。
#[derive(Debug, Serialize)]
pub struct BilibiliStatusResponse {
    /// 当前是否已连接。
    pub connected: bool,
    /// 状态机名称（idle / connecting / connected / reconnecting / stopped）。
    pub status: String,
    /// 直播间 ID（若已知）。
    pub room_id: Option<String>,
    /// 最近错误。
    pub last_error: Option<String>,
    /// 是否已开启自动连接。
    pub auto_connect: bool,
    /// 给用户看的进度阶段（idle / requesting_session / connecting_socket / connected / retrying / stopped / failed）。
    pub phase: crate::models::BilibiliPhase,
    /// 阶段说明（中文，直接展示）。
    pub detail: Option<String>,
    /// 已尝试次数（含重连）。
    pub attempts: u32,
    /// 最近一次状态变化时间（RFC3339）。
    pub updated_at: Option<String>,
}

/// `GET /api/bilibili/status`（以及 connect / disconnect 的返回）。
async fn api_bilibili_status(State(ctx): State<Arc<ServerCtx>>) -> Json<BilibiliStatusResponse> {
    Json(bilibili_status(&ctx).await)
}

/// 汇总当前弹幕连接状态。
async fn bilibili_status(ctx: &ServerCtx) -> BilibiliStatusResponse {
    let status = ctx.bilibili.status().await;
    let (room_id, last_error, auto_connect, phase, detail, attempts, updated_at) = {
        let state = ctx.state.read();
        (
            state.bilibili.room_id.clone(),
            state.bilibili.last_error.clone(),
            ctx.config().bilibili.auto_connect,
            state.bilibili.phase,
            state.bilibili.detail.clone(),
            state.bilibili.attempts,
            state.bilibili.updated_at.clone(),
        )
    };
    BilibiliStatusResponse {
        connected: status.is_connected(),
        status: serde_json::to_value(&status)
            .ok()
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| format!("{status:?}").to_lowercase()),
        room_id,
        last_error,
        auto_connect,
        phase,
        detail,
        attempts,
        updated_at,
    }
}

/// `POST /api/bilibili/connect`
/// 把请求体给出的字段合并并落盘，再用完整配置发起连接。
async fn api_bilibili_connect(
    State(ctx): State<Arc<ServerCtx>>,
    Json(body): Json<ConnectRequest>,
) -> Result<Json<BilibiliStatusResponse>, ApiError> {
    // 1) 合并并保存配置
    let merged = {
        let mut cfg = ctx.config();
        if let Some(v) = body.app_id {
            cfg.bilibili.app_id = v.trim().to_string();
        }
        if let Some(v) = body.access_key_id {
            cfg.bilibili.access_key_id = v.trim().to_string();
        }
        if let Some(v) = body.access_key_secret {
            cfg.bilibili.access_key_secret = v.trim().to_string();
        }
        if let Some(v) = body.code {
            cfg.bilibili.code = v.trim().to_string();
        }
        cfg
    };

    merged
        .save_to(&ctx.config_path)
        .map_err(|e| ApiError::internal(format!("保存配置失败：{e}")))?;
    *ctx.config.write().unwrap_or_else(|e| e.into_inner()) = merged.clone();

    // 2) 发起连接
    let auth = AuthConfig::from_config(&merged.bilibili);
    if let Err(err) = ctx.bilibili.connect(auth).await {
        // 配置不完整等本地错误直接回 400，前端能立刻提示该填哪一项。
        return Err(ApiError::bad_request(err.to_string()));
    }

    // 让界面立刻看到「连接中」而不是旧的错误信息。
    ctx.state.broadcast_state();
    Ok(Json(bilibili_status(&ctx).await))
}

/// `POST /api/bilibili/disconnect`
async fn api_bilibili_disconnect(
    State(ctx): State<Arc<ServerCtx>>,
) -> Json<BilibiliStatusResponse> {
    ctx.bilibili.disconnect().await;
    ctx.state.broadcast_state();
    Json(bilibili_status(&ctx).await)
}

/// `POST /api/bilibili/simulate` 请求体；用于在没有身份码 / 直播间时验证整条弹幕链路。
#[derive(Debug, Deserialize)]
pub struct SimulateRequest {
    /// 弹幕文本，例如 `点歌 晴天 周杰伦`。
    pub text: String,
    /// 发送者昵称，默认 `主播`。
    #[serde(default)]
    pub user: Option<String>,
    /// 发送者 UID。
    #[serde(default)]
    pub uid: Option<u64>,
    /// 是否按主播权限处理（默认 true）。
    ///
    /// 默认最高权限：无视冷却、名额、重复限制与黑名单，并插到所有弹幕之前；传 `false` 可模拟观众。
    #[serde(default = "default_true")]
    pub host: bool,
}

fn default_true() -> bool {
    true
}

/// `POST /api/bilibili/simulate`
///
/// 构造与平台结构一致的弹幕报文，走真实的解析与广播路径，因此可用来验证解析逻辑。
async fn api_bilibili_simulate(
    State(ctx): State<Arc<ServerCtx>>,
    Json(body): Json<SimulateRequest>,
) -> Result<Json<AppState>, ApiError> {
    let text = body.text.trim();
    if text.is_empty() {
        return Err(ApiError::bad_request("弹幕文本不能为空"));
    }
    // 需求：通过点歌机注入的弹幕，署名统一是主播
    let user = body.user.unwrap_or_else(|| "主播".to_string());
    let uid = body.uid.unwrap_or(1);

    let frame = crate::bilibili::danmaku::sample_danmaku_frame(&user, uid, text);
    let data = frame
        .get("data")
        .cloned()
        .ok_or_else(|| ApiError::internal("模拟报文缺少 data 字段"))?;
    let mut message = crate::bilibili::danmaku::parse_danmaku(&data)
        .ok_or_else(|| ApiError::internal("模拟报文解析失败"))?
        .into_danmaku();
    // 标记来源：消费端据此决定按主播还是观众规则处理
    message.from_host = body.host;

    info!(
        user = %message.user,
        text = %message.text,
        host = body.host,
        "注入模拟弹幕"
    );
    // 走**同一条**广播链路：面板能看到这条弹幕，点歌消费者会按优先级入队
    ctx.bilibili.emit(DanmakuEvent::Message(message));
    Ok(Json(ctx.state.snapshot()))
}

// ──────────────────────────── 状态 / 队列 API ────────────────────────────

/// `GET /api/state`
async fn api_state(State(ctx): State<Arc<ServerCtx>>) -> Json<AppState> {
    Json(ctx.state.snapshot())
}

/// `GET /api/queue`
async fn api_queue(State(ctx): State<Arc<ServerCtx>>) -> Json<Vec<crate::models::QueueItem>> {
    Json(ctx.state.read().queue.clone())
}

/// `GET /api/panel/backgrounds` —— 列出已上传的背景图。
///
/// 供界面「选择已上传图片」的下拉框 + 预览使用，免得用户每次重新选文件。
async fn api_panel_backgrounds() -> Json<Vec<BackgroundItem>> {
    Json(list_backgrounds())
}

/// 一张已上传的背景图。
#[derive(Debug, Serialize)]
pub struct BackgroundItem {
    /// 面板可直接引用的地址（`/bg/xxx`）。
    pub url: String,
    /// 文件名。
    pub name: String,
    /// 字节数。
    pub size: u64,
    /// 修改时间（Unix 秒）。
    pub modified: Option<u64>,
}

/// `POST /api/panel/background/rename` 请求体。
#[derive(Debug, Deserialize)]
pub struct BackgroundRenameRequest {
    /// 原文件名（或 `/bg/xxx` 形式）。
    pub from: String,
    /// 新文件名（不含扩展名；扩展名沿用原图）。
    pub to: String,
}

/// `POST /api/panel/background/rename` —— 给已上传的背景图改名。
///
/// 只允许改**基础名**（扩展名沿用原文件），并复用 `bg_file` 的安全校验，
/// 因此不可能通过改名做目录穿越。
async fn api_panel_background_rename(
    State(ctx): State<Arc<ServerCtx>>,
    Json(body): Json<BackgroundRenameRequest>,
) -> Result<Json<Vec<BackgroundItem>>, ApiError> {
    let dir = crate::config::Config::config_dir().join("backgrounds");
    let from_name = safe_bg_name(&body.from).ok_or_else(|| ApiError::bad_request("文件名不合法"))?;
    let new_base: String = body
        .to
        .trim()
        .chars()
        .filter(|c| !matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        .collect();
    if new_base.is_empty() {
        return Err(ApiError::bad_request("新名称不能为空"));
    }
    let ext = std::path::Path::new(&from_name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("jpg")
        .to_string();
    let to_name = format!("{new_base}.{ext}");
    if to_name == from_name {
        return Err(ApiError::notice("名称没有变化"));
    }

    let from_path = dir.join(&from_name);
    let to_path = dir.join(&to_name);
    if !from_path.is_file() {
        return Err(ApiError::not_found(format!("找不到图片 {from_name}")));
    }
    if to_path.exists() {
        return Err(ApiError::bad_request(format!("已存在同名图片 {to_name}")));
    }
    std::fs::rename(&from_path, &to_path)
        .map_err(|e| ApiError::internal(format!("重命名失败：{e}")))?;

    // 如果默认样式正引用这张图，同步改掉，否则重启后面板会 404
    let old_url = format!("/bg/{from_name}");
    let new_url = format!("/bg/{to_name}");
    let mut next = ctx.config();
    if next.panel.bg_image.as_deref() == Some(old_url.as_str()) {
        next.panel.bg_image = Some(new_url.clone());
        save_config(ctx.as_ref(), &next)?;
        info!(from = %from_name, to = %to_name, "背景图已重命名并同步默认样式");
    } else {
        info!(from = %from_name, to = %to_name, "背景图已重命名");
    }
    Ok(Json(list_backgrounds()))
}

/// `DELETE /api/panel/background` 请求体。
#[derive(Debug, Deserialize)]
pub struct BackgroundDeleteRequest {
    /// 文件名（或 `/bg/xxx` 形式）。
    pub name: String,
}

/// `DELETE /api/panel/background` —— 删除已上传的背景图。
async fn api_panel_background_delete(
    State(ctx): State<Arc<ServerCtx>>,
    Json(body): Json<BackgroundDeleteRequest>,
) -> Result<Json<Vec<BackgroundItem>>, ApiError> {
    let dir = crate::config::Config::config_dir().join("backgrounds");
    let name = safe_bg_name(&body.name).ok_or_else(|| ApiError::bad_request("文件名不合法"))?;
    let path = dir.join(&name);
    if !path.is_file() {
        return Err(ApiError::not_found(format!("找不到图片 {name}")));
    }
    std::fs::remove_file(&path).map_err(|e| ApiError::internal(format!("删除失败：{e}")))?;

    // 默认样式若还引用它，清空引用，避免面板指向一个不存在的文件
    let url = format!("/bg/{name}");
    let mut next = ctx.config();
    if next.panel.bg_image.as_deref() == Some(url.as_str()) {
        next.panel.bg_image = None;
        save_config(ctx.as_ref(), &next)?;
        info!(name = %name, "已删除背景图并清空默认引用");
    } else {
        info!(name = %name, "已删除背景图");
    }
    Ok(Json(list_backgrounds()))
}

/// 校验背景图文件名：只允许单层文件名 + 白名单扩展名。
///
/// 与 `bg_file` 用同一套规则——重命名/删除也必须挡目录穿越。
fn safe_bg_name(raw: &str) -> Option<String> {
    let name = raw.trim().rsplit('/').next().unwrap_or(raw).to_string();
    if name.is_empty() || name.contains('\\') || name.contains("..") {
        return None;
    }
    let lower = name.to_ascii_lowercase();
    let ok = [".png", ".jpg", ".jpeg", ".webp", ".gif"]
        .iter()
        .any(|ext| lower.ends_with(ext));
    ok.then_some(name)
}

/// 列出背景图（`api_panel_backgrounds` 的内部实现，供多个接口复用）。
fn list_backgrounds() -> Vec<BackgroundItem> {
    let dir = crate::config::Config::config_dir().join("backgrounds");
    let mut items: Vec<BackgroundItem> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if safe_bg_name(name).is_none() {
                continue;
            }
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            let modified = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs());
            items.push(BackgroundItem {
                url: format!("/bg/{name}"),
                name: name.to_string(),
                size,
                modified,
            });
        }
    }
    items.sort_by(|a, b| b.modified.cmp(&a.modified));
    items
}

/// `POST /api/panel/background` —— 上传面板背景图。
///
/// OBS 的浏览器源会拦 `file://`，面板只能引用 `http://127.0.0.1:PORT/...`，
/// 所以图片必须存到程序自己的目录，再由 `/bg/<name>` 提供。
/// 请求体是原始图片字节（`Content-Type: image/*`），文件名由后端生成，避免目录穿越。
async fn api_panel_background(
    State(_ctx): State<Arc<ServerCtx>>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, ApiError> {
    const MAX_BYTES: usize = 8 * 1024 * 1024;
    if body.is_empty() {
        return Err(ApiError::bad_request("图片内容为空"));
    }
    if body.len() > MAX_BYTES {
        return Err(ApiError::bad_request(format!(
            "图片过大（{} 字节，上限 {} 字节）",
            body.len(),
            MAX_BYTES
        )));
    }

    // 依据 Content-Type 决定扩展名；不接受无法识别的类型
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let ext = match content_type.as_str() {
        t if t.starts_with("image/png") => "png",
        t if t.starts_with("image/jpeg") || t.starts_with("image/jpg") => "jpg",
        t if t.starts_with("image/webp") => "webp",
        t if t.starts_with("image/gif") => "gif",
        other => {
            return Err(ApiError::bad_request(format!(
                "不支持的图片类型：{other}（支持 png/jpeg/webp/gif）"
            )))
        }
    };

    let dir = crate::config::Config::config_dir().join("backgrounds");
    std::fs::create_dir_all(&dir)
        .map_err(|e| ApiError::internal(format!("创建背景图目录失败：{e}")))?;

    // 文件名自己生成，杜绝目录穿越
    let name = format!("bg-{}.{ext}", uuid::Uuid::new_v4());
    let path = dir.join(&name);
    std::fs::write(&path, &body).map_err(|e| ApiError::internal(format!("写入图片失败：{e}")))?;

    let url = format!("/bg/{name}");
    info!(path = %path.display(), bytes = body.len(), %url, "面板背景图已保存");
    Ok(Json(serde_json::json!({ "url": url })))
}

/// `/bg/{name}` —— 提供背景图。
/// 只允许单层文件名（拒绝路径分隔符），且必须是已知图片扩展名。
async fn bg_file(AxumPath(name): AxumPath<String>) -> Response {
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return (StatusCode::BAD_REQUEST, "非法的文件名").into_response();
    }
    let lower = name.to_ascii_lowercase();
    let allowed = [".png", ".jpg", ".jpeg", ".webp", ".gif"];
    if !allowed.iter().any(|ext| lower.ends_with(ext)) {
        return (StatusCode::BAD_REQUEST, "不支持的图片类型").into_response();
    }

    let path = crate::config::Config::config_dir()
        .join("backgrounds")
        .join(&name);
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let content_type = if lower.ends_with(".png") {
                "image/png"
            } else if lower.ends_with(".webp") {
                "image/webp"
            } else if lower.ends_with(".gif") {
                "image/gif"
            } else {
                "image/jpeg"
            };
            (
                [
                    (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
                    // 文件名带 uuid，内容不会变，可以长缓存
                    (
                        header::CACHE_CONTROL,
                        HeaderValue::from_static("public, max-age=604800"),
                    ),
                ],
                bytes,
            )
                .into_response()
        }
        Err(_) => (StatusCode::NOT_FOUND, "背景图不存在").into_response(),
    }
}

/// `GET /api/blacklist` —— 读取黑名单。
async fn api_blacklist(State(ctx): State<Arc<ServerCtx>>) -> Json<Vec<BlacklistEntry>> {
    Json(ctx.config().blacklist)
}

/// `POST /api/blacklist` 请求体。
#[derive(Debug, Deserialize)]
pub struct BlacklistAddRequest {
    /// 歌名（必填）。
    pub title: String,
    /// 歌手（可选；留空表示拉黑这首歌的所有版本）。
    #[serde(default)]
    pub artist: Option<String>,
    /// 备注。
    #[serde(default)]
    pub note: Option<String>,
}

/// `POST /api/blacklist` —— 加入黑名单。
///
/// 命中的歌弹幕直接点不了（`RejectReason::Blacklisted`，不搜索不入队），但主播可无视。
async fn api_blacklist_add(
    State(ctx): State<Arc<ServerCtx>>,
    Json(body): Json<BlacklistAddRequest>,
) -> Result<Json<Vec<BlacklistEntry>>, ApiError> {
    let mut entry = BlacklistEntry::new(&body.title, body.artist.clone().unwrap_or_default());
    if !entry.is_valid() {
        return Err(ApiError::bad_request("歌名不能为空"));
    }
    entry.note = body.note;

    // 在写锁里先判重再插入，避免"检查后插入"之间的竞争
    let mut next = ctx.config();
    if !crate::queue::blacklist::add(&mut next.blacklist, entry) {
        return Err(ApiError::bad_request(format!(
            "《{}》已经在黑名单里了",
            body.title.trim()
        )));
    }
    save_config(ctx.as_ref(), &next)?;
    info!(title = %body.title, "已加入黑名单");
    Ok(Json(next.blacklist))
}

/// `DELETE /api/blacklist` 请求体（DELETE 带体虽然不常见，但比拼查询串更清晰）。
#[derive(Debug, Deserialize)]
pub struct BlacklistRemoveRequest {
    /// 歌名。
    pub title: String,
    /// 歌手（与加入时保持一致）。
    #[serde(default)]
    pub artist: Option<String>,
}

/// `DELETE /api/blacklist` —— 移出黑名单。
async fn api_blacklist_remove(
    State(ctx): State<Arc<ServerCtx>>,
    Json(body): Json<BlacklistRemoveRequest>,
) -> Result<Json<Vec<BlacklistEntry>>, ApiError> {
    let mut next = ctx.config();
    let removed = crate::queue::blacklist::remove(
        &mut next.blacklist,
        &body.title,
        &body.artist.clone().unwrap_or_default(),
    );
    if removed == 0 {
        return Err(ApiError::bad_request(format!(
            "黑名单里没有《{}》",
            body.title.trim()
        )));
    }
    save_config(ctx.as_ref(), &next)?;
    info!(title = %body.title, removed, "已移出黑名单");
    Ok(Json(next.blacklist))
}

/// 保存配置：落盘 + 替换内存副本 + 广播状态。
///
/// 黑名单增删都走这一套，避免出现「只改内存没落盘」这类不一致。
fn save_config(ctx: &ServerCtx, next: &Config) -> Result<(), ApiError> {
    next.save_to(&ctx.config_path)
        .map_err(|e| ApiError::internal(format!("保存配置失败：{e}")))?;
    *ctx.config.write().unwrap_or_else(|e| e.into_inner()) = next.clone();
    ctx.state.broadcast_state();
    Ok(())
}

/// `GET /api/requests` —— 点歌统计（最近记录 + 冷却中的用户）。
async fn api_requests(State(ctx): State<Arc<ServerCtx>>) -> Json<crate::models::RequestStats> {
    Json(ctx.state.read().stats.clone())
}

/// `GET /api/requests/log` 查询参数。
#[derive(Debug, Deserialize)]
pub struct RequestLogQuery {
    /// 只看某种结果：`queued` / `rejected`；省略表示全部。
    #[serde(default)]
    pub outcome: Option<String>,
    /// 是否只看某个人（按昵称或 UID 精确匹配）。
    #[serde(default)]
    pub user: Option<String>,
    /// 最多返回多少条（默认 500，上限 2000）。
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `GET /api/requests/log` —— 完整点歌日志。
///
/// `stats.recent` 随 `/ws` 全量广播故只留最近几十条，完整日志（最多 2000 条）由本接口按需拉取。
async fn api_requests_log(
    State(ctx): State<Arc<ServerCtx>>,
    Query(query): Query<RequestLogQuery>,
) -> Json<Vec<crate::models::RequestRecord>> {
    let limit = query.limit.unwrap_or(500).min(crate::queue::request::MAX_RECENT);
    let outcome = query
        .outcome
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty());
    let user = query
        .user
        .as_deref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let log = ctx.requests.recent();
    let filtered: Vec<_> = log
        .into_iter()
        .filter(|r| match &outcome {
            Some(want) => &r.outcome == want,
            None => true,
        })
        .filter(|r| match &user {
            Some(want) => {
                &r.user == want || r.uid.as_deref() == Some(want.as_str())
            }
            None => true,
        })
        .take(limit)
        .collect();

    Json(filtered)
}

/// `POST /api/queue/{id}/{action}`，action ∈ remove | top | up | down
async fn api_queue_action(
    State(ctx): State<Arc<ServerCtx>>,
    AxumPath((id, action)): AxumPath<(String, String)>,
) -> Result<Json<AppState>, ApiError> {
    let uuid = uuid::Uuid::parse_str(&id)
        .map_err(|_| ApiError::bad_request(format!("队列项 ID 非法：{id}")))?;

    let result = match action.as_str() {
        // 各操作返回类型不同，这里统一收敛为 `Result<(), QueueError>`。
        "remove" => queue::remove(&ctx.state, uuid).map(|_| ()),
        "top" => queue::move_to_top(&ctx.state, uuid),
        "up" => queue::shift(&ctx.state, uuid, -1),
        "down" => queue::shift(&ctx.state, uuid, 1),
        other => return Err(ApiError::not_found(format!("未知队列操作：{other}"))),
    };

    result.map_err(ApiError::from)?;
    ctx.state.broadcast_state();
    Ok(Json(ctx.state.snapshot()))
}

/// `POST /api/queue/clear`
async fn api_queue_clear(State(ctx): State<Arc<ServerCtx>>) -> Json<AppState> {
    queue::clear(&ctx.state);
    clear_queue_everywhere(&ctx.state);
    ctx.state.broadcast_state();
    Json(ctx.state.snapshot())
}

/// `POST /api/queue/clear-playlist` —— 清空播放列表（点歌队列 + 已生成的播放序列）。
///
/// 与相邻接口的区别：`/api/queue/clear` 只清点歌队列，
/// `/api/queue/clear-and-play-idle` 清完立刻播空闲歌单；
/// 本接口把点歌队列与整个播放序列（含已播历史）都清掉并停掉当前播放，但不碰空闲歌单。
async fn api_queue_clear_playlist(
    State(ctx): State<Arc<ServerCtx>>,
) -> Result<Json<AppState>, ApiError> {
    queue::clear(&ctx.state);
    if let Some(player) = &ctx.player {
        player.stop_and_clear().await;
    }
    ctx.state.mutate(|s| {
        s.playing.clear();
        s.cursor = 0;
        s.current = None;
        s.current_is_idle = false;
        s.idle_pending_return = false;
        s.player.playing = false;
        s.player.paused = false;
        s.player.position = 0.0;
        s.player.duration = 0.0;
        s.player.lyrics = None;
        s.player.lyric_index = None;
    });
    info!("播放列表已清空");
    ctx.state.broadcast_state();
    Ok(Json(ctx.state.snapshot()))
}

/// 一键清空点歌列表并开始播空闲歌单。
///
/// 清空点歌队列 → 丢弃播放序列里尚未播的部分（`cursor` 右边，否则「清空」看起来没生效）
/// → 保留已播历史（`cursor` 左边，「上一首」仍能回去）→ 立刻按空闲歌单出歌（书签那首重头播）。
async fn api_queue_clear_and_play_idle(
    State(ctx): State<Arc<ServerCtx>>,
) -> Result<Json<AppState>, ApiError> {
    queue::clear(&ctx.state);
    let dropped = queue::clear_upcoming(&ctx.state);
    info!(dropped, "已清空点歌列表，准备播放空闲歌单");

    // 清空后的"当前曲目"如果来自点歌队列，就不能继续当作在播
    ctx.state.mutate(|s| {
        if !s.current_is_idle {
            s.current = None;
            s.player.playing = false;
            s.player.paused = false;
            s.player.position = 0.0;
            s.player.duration = 0.0;
            s.player.lyrics = None;
            s.player.lyric_index = None;
        }
    });

    if let Some(player) = &ctx.player {
        match player.play_idle_next().await {
            Ok(Some(item)) => info!(title = %item.song.title, "清空后已开始播放空闲歌单"),
            Ok(None) => info!("空闲歌单为空，清空后无歌可播"),
            Err(err) => warn!(error = %err, "清空后播放空闲歌单失败"),
        }
    }
    ctx.state.broadcast_state();
    Ok(Json(ctx.state.snapshot()))
}

/// 清空点歌队列时，把该队列的条目也从播放序列的待播部分里摘掉。
/// 只清右侧（未播），保留左侧已播，「上一首」仍要能回去。
fn clear_queue_everywhere(state: &StateCell) {
    state.mutate(|s| {
        let queue_ids: std::collections::HashSet<_> = s.queue.iter().map(|i| i.id).collect();
        // 从头到尾重建，保留「已播部分」与不属于点歌队列的条目（如空闲歌单）
        let cursor_item = s.playing.get(s.cursor).map(|i| i.id);
        let mut kept: Vec<crate::models::QueueItem> = Vec::with_capacity(s.playing.len());
        for (index, item) in s.playing.drain(..).enumerate() {
            let is_played = index <= s.cursor;
            let is_queue_item = queue_ids.contains(&item.id);
            if is_played || !is_queue_item {
                kept.push(item);
            }
        }
        s.playing = kept;
        // 游标修正：仍指向原来那首（若它还在）
        s.cursor = cursor_item
            .and_then(|id| s.playing.iter().position(|i| i.id == id))
            .unwrap_or_else(|| s.playing.len().saturating_sub(1));
    });
}

/// `POST /api/queue/add` 请求体（手动加歌）。
///
/// 只给 `title` / `artist`：创建占位歌曲，随后由解析器补齐真实曲目；
/// 直接给 `song`（搜索结果）：带上真实 ID，标记为已解析，跳过搜索。
#[derive(Debug, Deserialize)]
pub struct AddSongRequest {
    /// 歌名（与 `song` 二选一）。
    #[serde(default)]
    pub title: Option<String>,
    /// 歌手，可省略。
    #[serde(default)]
    pub artist: Option<String>,
    /// 点歌人展示名，默认 `手动添加`。
    #[serde(default)]
    pub requested_by: Option<String>,
    /// 完整歌曲对象（来自 `/api/music/search`）。
    #[serde(default)]
    pub song: Option<crate::models::Song>,
    /// 优先级：`host`（默认，主播点歌机）/ `danmaku`（当成观众点歌）。
    ///
    /// 主播通过控制台加歌默认为最高优先级：插到所有弹幕点歌之前，不受弹幕上限约束。
    #[serde(default)]
    pub priority: Option<crate::models::QueuePriority>,
}

/// `POST /api/queue/add`
async fn api_queue_add(
    State(ctx): State<Arc<ServerCtx>>,
    Json(body): Json<AddSongRequest>,
) -> Result<Json<AppState>, ApiError> {
    let requested_by = body
        .requested_by
        .clone()
        .unwrap_or_else(|| "点歌机".to_string());
    // 主播点歌机默认最高优先级
    let priority = body
        .priority
        .unwrap_or(crate::models::QueuePriority::Host);

    // 情况一：直接给完整歌曲（搜索结果）→ 已解析，无需再搜索
    if let Some(mut song) = body.song {
        if song.title.trim().is_empty() {
            return Err(ApiError::bad_request("歌曲标题不能为空"));
        }
        // 点歌机也判重：同一首歌在队列里排两次毫无意义，
        // 只会让「下一首」看起来没反应。已有则直接返回现状。
        if let Some(existing) = queue::find_duplicate(&ctx.state, &song.title, &song.artist) {
            return Err(ApiError::notice(format!(
                "《{}》已经在队列里了（第 {} 位）",
                existing.title,
                existing.position + 1
            )));
        }
        song.source = crate::models::SongSource::Resolved;
        song.source_error = None;
        let item = crate::models::QueueItem::with_priority(song, requested_by, None, priority);
        queue::push(&ctx.state, item);
        queue::account_enqueue(&ctx.state, priority);
        ctx.state.broadcast_state();
        return Ok(Json(ctx.state.snapshot()));
    }

    // 情况二：只给文本 → 占位入队，交给解析器
    let title = body.title.unwrap_or_default();
    let title = title.trim();
    if title.is_empty() {
        return Err(ApiError::bad_request("歌名不能为空"));
    }
    let artist = body.artist.unwrap_or_default();
    let artist = artist.trim();
    // 文本路径同样判重（此时歌手可能为空，按歌名匹配）
    if let Some(existing) = queue::find_duplicate(&ctx.state, title, artist) {
        return Err(ApiError::notice(format!(
            "《{}》已经在队列里了（第 {} 位）",
            existing.title,
            existing.position + 1
        )));
    }
    let song = crate::models::Song::placeholder(title, artist);
    let item = crate::models::QueueItem::with_priority(song, requested_by, None, priority);
    let item_id = item.id;
    queue::push(&ctx.state, item);
    queue::account_enqueue(&ctx.state, priority);
    ctx.state.broadcast_state();

    // 与弹幕点歌一致：交给解析器搜索真实曲目
    let request = crate::queue::SongRequest {
        title: title.to_string(),
        artist: artist.to_string(),
        raw: String::new(),
    };
    let job = crate::music::ResolveJob::new(item_id, &request);
    if !ctx.requests.enqueue_resolution(job) {
        debug!(%title, "解析器不可用，手动加歌保留占位");
    }

    Ok(Json(ctx.state.snapshot()))
}

// ──────────────────────────── 空闲歌单 API ────────────────────────────

/// `GET /api/idle` 响应。
#[derive(Debug, Serialize)]
pub struct IdleResponse {
    /// 歌单条目。
    pub items: Vec<crate::models::QueueItem>,
    /// 播放模式。
    pub mode: crate::models::IdleMode,
    /// **书签**：正在播（或最近播过）的那一首下标（阶段 10d）。
    pub current: Option<usize>,
    /// **下一次取歌**的下标（阶段 10d）。
    pub next: usize,
    /// 当前正在播的是不是空闲歌单的歌。
    pub current_is_idle: bool,
    /// 所有可选播放模式（界面下拉直接用，避免前后端枚举不一致）。
    pub modes: Vec<IdleModeOption>,
}

/// 供界面展示的模式选项。
#[derive(Debug, Serialize)]
pub struct IdleModeOption {
    /// 取值。
    pub value: crate::models::IdleMode,
    /// 显示名。
    pub label: &'static str,
}

/// `GET /api/idle`
async fn api_idle_get(State(ctx): State<Arc<ServerCtx>>) -> Json<IdleResponse> {
    Json(idle_response(&ctx))
}

/// 组装空闲歌单响应。
fn idle_response(ctx: &Arc<ServerCtx>) -> IdleResponse {
    let guard = ctx.state.read();
    IdleResponse {
        items: guard.idle.clone(),
        mode: guard.idle_mode,
        current: guard.idle_current,
        next: guard.idle_next,
        current_is_idle: guard.current_is_idle,
        modes: crate::models::IdleMode::ALL
            .iter()
            .map(|m| IdleModeOption {
                value: *m,
                label: m.label(),
            })
            .collect(),
    }
}

/// `POST /api/idle` —— 往空闲歌单加歌。
///
/// 用法与 `POST /api/queue/add` 一致：给 `song` 表示已解析，只给 `title` / `artist` 则由解析器搜索。
/// 路径就是 `/api/idle`（与 `GET` / `DELETE` 同一个路由，按方法区分），没有 `/api/idle/add`。
async fn api_idle_add(
    State(ctx): State<Arc<ServerCtx>>,
    Json(body): Json<AddSongRequest>,
) -> Result<Json<IdleResponse>, ApiError> {
    let requested_by = body
        .requested_by
        .clone()
        .unwrap_or_else(|| "空闲歌单".to_string());

    // 情况一：完整歌曲（搜索结果），跳过搜索
    if let Some(mut song) = body.song {
        if song.title.trim().is_empty() {
            return Err(ApiError::bad_request("歌曲标题不能为空"));
        }
        song.source = crate::models::SongSource::Resolved;
        song.source_error = None;
        let item = crate::models::QueueItem::with_priority(
            song,
            requested_by,
            None,
            crate::models::QueuePriority::Host,
        );
        ctx.state.mutate(|s| s.idle.push(item));
        ctx.state.broadcast_state();
        return Ok(Json(idle_response(&ctx)));
    }

    // 情况二：文本 → 占位 + 交给解析器
    let title = body.title.unwrap_or_default();
    let title = title.trim().to_string();
    if title.is_empty() {
        return Err(ApiError::bad_request("歌名不能为空"));
    }
    let artist = body.artist.unwrap_or_default().trim().to_string();
    let song = crate::models::Song::placeholder(&title, &artist);
    let item = crate::models::QueueItem::with_priority(
        song,
        requested_by,
        None,
        crate::models::QueuePriority::Host,
    );
    let item_id = item.id;
    ctx.state.mutate(|s| s.idle.push(item));
    ctx.state.broadcast_state();

    let request = crate::queue::SongRequest {
        title: title.clone(),
        artist: artist.clone(),
        raw: String::new(),
    };
    let job = crate::music::ResolveJob::new(item_id, &request);
    if !ctx.requests.enqueue_resolution(job) {
        debug!(%title, "解析器不可用，空闲歌单条目保留占位");
    }

    Ok(Json(idle_response(&ctx)))
}

/// `DELETE /api/idle` 请求体。
#[derive(Debug, Deserialize)]
pub struct IdleRemoveRequest {
    /// 条目 id。
    pub id: uuid::Uuid,
}

/// `DELETE /api/idle` —— 从空闲歌单移除一首。
async fn api_idle_remove(
    State(ctx): State<Arc<ServerCtx>>,
    Json(body): Json<IdleRemoveRequest>,
) -> Result<Json<IdleResponse>, ApiError> {
    let removed = ctx.state.mutate(|s| {
        s.idle
            .iter()
            .position(|i| i.id == body.id)
            .map(|pos| s.idle.remove(pos))
    });
    if removed.is_none() {
        return Err(ApiError::bad_request("空闲歌单里没有这个条目"));
    }
    // 删除会让后面的下标整体前移，两个索引都要跟着修正，
    // 否则「书签」和「下一次取歌」会指到别的歌上（表现为跳过一首）。
    ctx.state.mutate(|s| {
        s.idle_next = s.idle_next.min(s.idle.len());
        if let Some(current) = s.idle_current {
            s.idle_current = if current >= s.idle.len() {
                // 书签指向的正是被删掉的那首（或已越界）→ 退回下一次取歌位置
                None
            } else {
                Some(current)
            };
        }
    });
    ctx.state.broadcast_state();
    Ok(Json(idle_response(&ctx)))
}

/// `POST /api/idle/clear` —— 清空空闲歌单。
async fn api_idle_clear(State(ctx): State<Arc<ServerCtx>>) -> Json<IdleResponse> {
    ctx.state.mutate(|s| {
        s.idle.clear();
        s.idle_current = None;
        s.idle_next = 0;
    });
    ctx.state.broadcast_state();
    Json(idle_response(&ctx))
}

/// `POST /api/idle/mode` 请求体。
#[derive(Debug, Deserialize)]
pub struct IdleModeRequest {
    /// 新的播放模式。
    pub mode: crate::models::IdleMode,
}

/// `POST /api/idle/mode` —— 切换空闲歌单播放模式。
async fn api_idle_mode(
    State(ctx): State<Arc<ServerCtx>>,
    Json(body): Json<IdleModeRequest>,
) -> Json<IdleResponse> {
    ctx.state.mutate(|s| {
        s.idle_mode = body.mode;
        // 切模式后夹取索引，避免越界后卡住不出歌
        if s.idle_next > s.idle.len() {
            s.idle_next = 0;
        }
        if s.idle_current.is_some_and(|i| i >= s.idle.len()) {
            s.idle_current = None;
        }
    });
    info!(mode = ?body.mode, "空闲歌单播放模式已切换");
    ctx.state.broadcast_state();
    Json(idle_response(&ctx))
}

/// `POST /api/idle/play` 请求体。
#[derive(Debug, Deserialize)]
pub struct IdlePlayRequest {
    /// 播某一首（按 id）。省略则播下一首。
    #[serde(default)]
    pub id: Option<uuid::Uuid>,
    /// 播某一首（按下标，从 0 开始）。与 `id` 二选一。
    #[serde(default)]
    pub index: Option<usize>,
}

/// `POST /api/idle/play` —— 立即播放空闲歌单里的一首。
/// 按 `id` 或 `index` 指定，都不传则播下一首。
async fn api_idle_play(
    State(ctx): State<Arc<ServerCtx>>,
    Json(body): Json<IdlePlayRequest>,
) -> Result<Json<AppState>, ApiError> {
    let Some(player) = ctx.player.clone() else {
        return Err(ApiError::bad_request("当前没有可用的播放器（未检测到 mpv）"));
    };

    if let Some(id) = body.id {
        let index = {
            let guard = ctx.state.read();
            guard.idle.iter().position(|i| i.id == id)
        };
        let Some(index) = index else {
            return Err(ApiError::bad_request("空闲歌单里没有这个条目"));
        };
        player
            .play_idle_index(index)
            .await
            .map_err(|e| ApiError::internal(format!("播放失败：{e}")))?;
        return Ok(Json(ctx.state.snapshot()));
    }

    if let Some(index) = body.index {
        player
            .play_idle_index(index)
            .await
            .map_err(|e| ApiError::bad_request(format!("播放失败：{e}")))?;
        return Ok(Json(ctx.state.snapshot()));
    }

    // 没指定就播下一首
    player
        .start_next()
        .await
        .map_err(|e| ApiError::internal(format!("播放失败：{e}")))?;
    Ok(Json(ctx.state.snapshot()))
}

// ──────────────────────────── 收藏歌单导入 API ────────────────────────────

/// `GET /api/music/playlists` 查询参数。
#[derive(Debug, Deserialize)]
pub struct PlaylistsQuery {
    /// 平台：`netease` / `qq`；省略用默认平台。
    #[serde(default)]
    pub platform: Option<crate::models::MusicPlatform>,
}

/// `GET /api/music/playlists` 响应。
#[derive(Debug, Serialize)]
pub struct PlaylistsResponse {
    /// 平台。
    pub platform: crate::models::MusicPlatform,
    /// 歌单列表。
    pub playlists: Vec<crate::music::PlaylistInfo>,
    /// 登录状态（未登录时歌单为空，界面据此提示去登录）。
    pub logged_in: bool,
}

/// `GET /api/music/playlists` —— 列出某个平台的收藏歌单。
async fn api_music_playlists(
    State(ctx): State<Arc<ServerCtx>>,
    Query(query): Query<PlaylistsQuery>,
) -> Result<Json<PlaylistsResponse>, ApiError> {
    let platform = match query.platform {
        Some(p) => p,
        None => ctx.music.search_platform().await.primary().unwrap_or(
            crate::models::MusicPlatform::Qq,
        ),
    };

    let logged_in = ctx.music.status(platform).await.logged_in;
    if !logged_in {
        // 未登录是**使用顺序问题**，不是接口故障：给出可操作的提示，
        // 而不是让下面那次必然失败的网络请求抛出一句技术性报错。
        return Err(ApiError::notice(format!(
            "尚未登录{}：请先在「直播与播放」页登录该平台，再读取收藏歌单",
            platform_label(platform)
        )));
    }

    let playlists = ctx
        .music
        .user_playlists(platform)
        .await
        .map_err(|e| ApiError::bad_request(crate::music::resolver::describe_music_error(&e)))?;

    Ok(Json(PlaylistsResponse {
        platform,
        playlists,
        logged_in,
    }))
}

/// 音乐平台的中文名（用于界面提示）。
fn platform_label(platform: crate::models::MusicPlatform) -> &'static str {
    match platform {
        crate::models::MusicPlatform::Qq => "QQ 音乐",
        crate::models::MusicPlatform::Netease => "网易云音乐",
    }
}

/// `POST /api/idle/import` 请求体。
#[derive(Debug, Deserialize)]
pub struct IdleImportRequest {
    /// 平台。
    pub platform: crate::models::MusicPlatform,
    /// 歌单 ID。
    pub playlist_id: String,
    /// `append`（默认，追加）或 `replace`（先清空再导入）。
    #[serde(default)]
    pub mode: Option<String>,
    /// 最多导入多少首（防止「我喜欢」500+ 首一次性灌爆）。
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `POST /api/idle/import` —— 把收藏歌单导入到空闲歌单。
/// `mode` 为 `append`（默认，追加）或 `replace`（先清空再导入）。
async fn api_idle_import(
    State(ctx): State<Arc<ServerCtx>>,
    Json(body): Json<IdleImportRequest>,
) -> Result<Json<IdleResponse>, ApiError> {
    let songs = ctx
        .music
        .playlist_tracks(body.platform, &body.playlist_id)
        .await
        .map_err(|e| ApiError::bad_request(crate::music::resolver::describe_music_error(&e)))?;

    if songs.is_empty() {
        return Err(ApiError::bad_request("这个歌单里没有可导入的曲目"));
    }

    let limit = body.limit.unwrap_or(500).min(2000);
    let total = songs.len();
    let truncated = total > limit;

    let items: Vec<crate::models::QueueItem> = songs
        .into_iter()
        .take(limit)
        .map(|song| {
            // 来源标为「空闲歌单」，与手动加歌一致
            crate::models::QueueItem::with_priority(
                song,
                "空闲歌单".to_string(),
                None,
                crate::models::QueuePriority::Host,
            )
        })
        .collect();
    let imported = items.len();

    let replace = body
        .mode
        .as_deref()
        .map(|m| m.eq_ignore_ascii_case("replace"))
        .unwrap_or(false);

    ctx.state.mutate(|s| {
        if replace {
            s.idle.clear();
            // 「替换导入」换掉整个曲库，旧下标不再对应任何歌，必须重置索引，
            // 否则书签会指到完全不同的曲目上。
            s.idle_current = None;
            s.idle_next = 0;
        }
        s.idle.extend(items);
    });
    ctx.state.broadcast_state();

    info!(
        platform = ?body.platform,
        playlist_id = %body.playlist_id,
        imported,
        total,
        truncated,
        replace,
        "收藏歌单已导入空闲歌单"
    );
    Ok(Json(idle_response(&ctx)))
}

/// `GET /api/config`
async fn api_config_get(State(ctx): State<Arc<ServerCtx>>) -> Json<Config> {
    Json(ctx.config())
}

/// `PUT /api/config`
///
/// 保存后立即生效：配置通过 `Arc<RwLock<Config>>` 共享给点歌服务，
/// 后者每次处理弹幕时读取最新规则，因此冷却 / 上限 / 正则可热更新。
///
/// ## ⚠️ 缺字段的语义是「保持不变」，不是「清空」
/// `Config` 的所有字段都带 `#[serde(default)]`，所以请求体里**省略**
/// `blacklist` 会被反序列化成空数组。若照此落盘，用户在设置页改个端口
/// 就会把黑名单整份抹掉（黑名单是独立增删的，不在设置表单里）。
/// 因此这里只接受请求体里**显式出现**的 `blacklist`。
async fn api_config_put(
    State(ctx): State<Arc<ServerCtx>>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<Config>, ApiError> {
    let mut next: Config = serde_json::from_value(body.clone())
        .map_err(|e| ApiError::bad_request(format!("配置格式不正确：{e}")))?;
    // 请求体没带黑名单 → 沿用当前值（避免静默清空）
    if body.get("blacklist").is_none() {
        next.blacklist = ctx.config().blacklist;
    }

    // 1) 先校验：正则非法时**不落盘**，避免把坏配置写进磁盘导致下次启动回退。
    let changed_regex = {
        let previous = ctx.config.read().unwrap_or_else(|e| e.into_inner());
        previous.rules.command_regex != next.rules.command_regex
    };
    let new_parser = if changed_regex {
        match crate::queue::SongRequestParser::from_rules(&next.rules) {
            Ok(parser) => Some(parser),
            Err(err) => {
                warn!(error = %err, "新的点歌正则非法，已拒绝保存");
                return Err(ApiError::bad_request(format!("点歌正则非法：{err}")));
            }
        }
    } else {
        None
    };

    // 2) 落盘并替换内存配置
    next.save_to(&ctx.config_path)
        .map_err(|e| ApiError::internal(format!("保存配置失败：{e}")))?;
    *ctx.config.write().unwrap_or_else(|e| e.into_inner()) = next.clone();

    // 3) 正则有变化时热更新解析器
    if let Some(parser) = new_parser {
        ctx.requests.set_parser(parser);
        info!(regex = %next.rules.command_regex, "点歌指令正则已热更新");
    }

    // 4) 搜索平台策略也要立刻生效（它决定用哪个平台、是否跨平台回退）
    ctx.music
        .apply_search_platform(next.rules.search_platform)
        .await;
    debug!(
        strategy = ?next.rules.search_platform,
        "搜索平台策略已应用"
    );

    info!("配置已更新并落盘");
    ctx.state.broadcast_state();
    Ok(Json(next))
}

// ──────────────────────────── 播放控制 API ────────────────────────────

/// `POST /api/player/pause`
/// 有播放器时转发到 mpv；占位模式下只改状态（便于没有 mpv 时验证界面）。
async fn api_player_pause(
    State(ctx): State<Arc<ServerCtx>>,
    Json(body): Json<PauseRequest>,
) -> Json<AppState> {
    match &ctx.player {
        Some(player) => {
            if let Err(err) = player.backend().set_paused(body.paused).await {
                warn!(error = %err, "切换暂停失败");
            }
            ctx.state.mutate(|s| {
                s.player.paused = body.paused;
                s.player.playing = !body.paused && s.current.is_some();
            });
            ctx.state.broadcast_state();
        }
        None => {
            // 占位模式：仅状态
            ctx.state.mutate(|s| {
                s.player.paused = body.paused;
                s.player.playing = !body.paused;
            });
        }
    }
    Json(ctx.state.snapshot())
}

/// 暂停请求体。
#[derive(Debug, Deserialize)]
pub struct PauseRequest {
    /// true = 暂停。
    pub paused: bool,
}

/// `POST /api/player/volume`
///
/// 必须落盘（写进 `config.player.volume`）：只改内存 + 通知 mpv 的话，
/// 用户重开软件音量会被重置回默认值。
async fn api_player_volume(
    State(ctx): State<Arc<ServerCtx>>,
    Json(body): Json<VolumeRequest>,
) -> Json<AppState> {
    let volume = body.volume.clamp(0.0, 100.0) as u8;
    if let Some(player) = &ctx.player {
        if let Err(err) = player.backend().set_volume(volume).await {
            warn!(error = %err, "设置音量失败");
        }
    }
    ctx.state.mutate(|s| s.player.volume = volume);

    // 同步进配置并保存，否则重启就丢
    let saved = {
        let mut guard = ctx.config.write().unwrap_or_else(|e| e.into_inner());
        if guard.player.volume != volume {
            guard.player.volume = volume;
            Some(guard.clone())
        } else {
            None
        }
    };
    if let Some(next) = saved {
        if let Err(err) = save_config(ctx.as_ref(), &next) {
            warn!(error = %err, "音量落盘失败");
        } else {
            debug!(volume, "音量已保存");
        }
    }

    Json(ctx.state.snapshot())
}

/// 音量请求体。
#[derive(Debug, Deserialize)]
pub struct VolumeRequest {
    /// 0-100，超出范围会被裁剪。
    pub volume: f64,
}

/// `POST /api/player/skip`
///
/// 不能消费请求体：前端 `request()` 会给所有请求统一加 `Content-Type: application/json`，
/// 而本接口是空 body 的 POST；axum 的 `Json<T>`（以及 `Option<Json<T>>`）见到 JSON 声明
/// 加空 body 就返回 400 `Failed to parse the request body as JSON: EOF while parsing a value`，
/// 前端显示为「请求 /api/player/skip 失败」。
///
/// 本接口的参数是空占位，所以直接不消费 body（与 `previous` / `replay` 一致），从根上消除该失败。
async fn api_player_skip(State(ctx): State<Arc<ServerCtx>>) -> Json<AppState> {
    match &ctx.player {
        Some(player) => match player.advance(true).await {
            Ok(Some(item)) => info!(title = %item.song.title, "已跳到下一首"),
            Ok(None) => info!("队列已空，跳过后无歌曲可播"),
            Err(err) => warn!(error = %err, "跳过失败"),
        },
        None => {
            queue::skip_current(&ctx.state);
            ctx.state.broadcast_state();
        }
    }
    Json(ctx.state.snapshot())
}

/// `POST /api/player/seek` 请求体。
#[derive(Debug, Deserialize)]
pub struct SeekRequest {
    /// 目标位置（秒）。
    pub position: f64,
}

/// `POST /api/player/seek` —— 拖动进度条跳转。
/// 此前没有这个路由，进度条只是展示用的 `div`，现在 `.progress` 可点击 / 可拖动。
async fn api_player_seek(
    State(ctx): State<Arc<ServerCtx>>,
    Json(body): Json<SeekRequest>,
) -> Result<Json<AppState>, ApiError> {
    let Some(player) = &ctx.player else {
        return Err(ApiError::conflict(
            "播放器未初始化（未找到 mpv），请先安装 mpv 或设置 BSR_MPV_PATH".to_string(),
        ));
    };
    if !body.position.is_finite() || body.position < 0.0 {
        return Err(ApiError::bad_request("跳转位置必须是 >= 0 的有限数值".to_string()));
    }
    if let Err(err) = player.seek(body.position).await {
        return Err(ApiError::conflict(err));
    }
    Ok(Json(ctx.state.snapshot()))
}

/// `POST /api/player/previous` —— 回到播放序列里的上一首。
///
/// 与 `/api/player/replay`（当前这首重头播）语义不同，不要混用。
/// 已在第一首时返回 200 + `{"notice": ...}`：那是正常边界，不是接口故障（早期返回 409）。
async fn api_player_previous(State(ctx): State<Arc<ServerCtx>>) -> Result<Json<AppState>, ApiError> {
    let Some(player) = &ctx.player else {
        return Err(ApiError::conflict(
            "播放器未初始化（未找到 mpv），请先安装 mpv 或设置 BSR_MPV_PATH".to_string(),
        ));
    };
    match player.previous().await {
        Ok(Some(item)) => {
            info!(title = %item.song.title, "已回到上一首");
        }
        Ok(None) => {}
        // 边界（已是第一首）→ 提示；其余（如版权受限播不了）→ 真正的错误
        Err(err) if err.starts_with("没有更多了") => {
            return Err(ApiError::notice(err));
        }
        Err(err) => return Err(ApiError::conflict(err)),
    }
    Ok(Json(ctx.state.snapshot()))
}

/// `POST /api/player/replay` —— 重头播放当前歌曲。
async fn api_player_replay(State(ctx): State<Arc<ServerCtx>>) -> Result<Json<AppState>, ApiError> {
    let Some(player) = &ctx.player else {
        return Err(ApiError::conflict(
            "播放器未初始化（未找到 mpv），请先安装 mpv 或设置 BSR_MPV_PATH".to_string(),
        ));
    };
    match player.replay_current().await {
        Ok(Some(item)) => info!(title = %item.song.title, "重头播放当前歌曲"),
        Ok(None) => {}
        Err(err) => return Err(ApiError::conflict(err)),
    }
    Ok(Json(ctx.state.snapshot()))
}

/// `POST /api/player/play` —— 继续播放当前曲目，或开始播队列。
///
/// 「继续播放」优先：重启后从快照恢复了上次的曲目与位置，点这个按钮
/// 应当**接着那一首、从上次的秒数**放，而不是把它当"下一首"跳过。
async fn api_player_play(State(ctx): State<Arc<ServerCtx>>) -> Result<Json<AppState>, ApiError> {
    let Some(player) = &ctx.player else {
        return Err(ApiError::conflict(
            "播放器未初始化（未找到 mpv），请先安装 mpv 或设置 BSR_MPV_PATH".to_string(),
        ));
    };
    match player.resume_or_start().await {
        Ok(Some(item)) => info!(title = %item.song.title, "开始播放"),
        Ok(None) => info!("队列为空，没有可播放的歌曲"),
        Err(err) => return Err(ApiError::conflict(err)),
    }
    Ok(Json(ctx.state.snapshot()))
}

/// `POST /api/player/mode`
///
/// 除改内存状态外还必须落盘（写进 `config.play_mode`）：
/// 早期只改内存，用户反馈每次重开软件播放模式都被重置。
async fn api_player_mode(
    State(ctx): State<Arc<ServerCtx>>,
    Json(body): Json<ModeRequest>,
) -> Json<AppState> {
    ctx.state.mutate(|s| s.play_mode = body.mode);

    // 同步进配置并保存，否则重启就丢
    let saved = {
        let mut guard = ctx.config.write().unwrap_or_else(|e| e.into_inner());
        if guard.play_mode != body.mode {
            guard.play_mode = body.mode;
            Some(guard.clone())
        } else {
            None
        }
    };
    if let Some(next) = saved {
        if let Err(err) = save_config(ctx.as_ref(), &next) {
            warn!(error = %err, "播放模式落盘失败");
        } else {
            info!(mode = ?body.mode, "播放模式已保存");
        }
    }

    Json(ctx.state.snapshot())
}

/// 播放模式请求体。
#[derive(Debug, Deserialize)]
pub struct ModeRequest {
    /// 目标播放模式。
    pub mode: crate::models::PlayMode,
}

/// `GET /api/player/status` —— 播放器可用性与进度。
async fn api_player_status(State(ctx): State<Arc<ServerCtx>>) -> Json<PlayerStatusResponse> {
    let (position, duration, volume, paused, playing) = {
        let state = ctx.state.read();
        (
            state.player.position,
            state.player.duration,
            state.player.volume,
            state.player.paused,
            state.player.playing,
        )
    };
    Json(PlayerStatusResponse {
        available: ctx.player.is_some(),
        position,
        duration,
        volume,
        paused,
        playing,
        mode: ctx.state.read().play_mode,
    })
}

/// 播放器状态响应。
#[derive(Debug, Serialize)]
pub struct PlayerStatusResponse {
    /// mpv 是否已接线（未安装 mpv 时为 false）。
    pub available: bool,
    /// 播放位置（秒）。
    pub position: f64,
    /// 时长（秒）。
    pub duration: f64,
    /// 音量。
    pub volume: u8,
    /// 是否暂停。
    pub paused: bool,
    /// 是否正在播放。
    pub playing: bool,
    /// 播放模式。
    pub mode: crate::models::PlayMode,
}

// ──────────────────────────── WebSocket ────────────────────────────

/// `GET /ws?snapshot=false` 可跳过首帧全量快照。
#[derive(Debug, Default, Deserialize)]
pub struct WsQuery {
    /// 是否在连接后立刻下发一次全量状态，默认 true。
    #[serde(default)]
    pub snapshot: Option<bool>,
}

/// WS 升级入口。
async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(query): Query<WsQuery>,
    State(ctx): State<Arc<ServerCtx>>,
) -> Response {
    ws.on_upgrade(move |socket| ws_session(socket, ctx, query.snapshot.unwrap_or(true)))
}

/// 计算并推送一次全量快照。
async fn push_snapshot(
    state: &StateCell,
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
) -> Result<(), axum::Error> {
    let snapshot = HubEvent::State(Box::new(state.snapshot()));
    sender.send(Message::Text(snapshot.to_text().into())).await
}

/// 单个 WS 连接的生命周期。
async fn ws_session(socket: WebSocket, ctx: Arc<ServerCtx>, send_snapshot: bool) {
    // 直接克隆内部的 `StateCell`（它本身是 Arc 包装，克隆很廉价），
    // 避免把 `Arc<ServerCtx>` 传进推送函数造成类型不匹配。
    let state = ctx.state.clone();
    let mut rx = ctx.state.subscribe();
    let (mut sender, mut receiver) = socket.split();

    let (version, started_at) = {
        let guard = state.read();
        (guard.version.clone(), guard.started_at.to_rfc3339())
    };
    let hello = HubEvent::Hello {
        version,
        started_at,
    };
    if sender.send(Message::Text(hello.to_text().into())).await.is_err() {
        return;
    }

    if send_snapshot && push_snapshot(&state, &mut sender).await.is_err() {
        return;
    }
    info!("WS 客户端已连接");

    loop {
        tokio::select! {
            // ── 客户端 → 服务器 ──────────────────────────────────────────
            incoming = receiver.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        if let Err(err) = handle_client_message(&text) {
                            let ev = HubEvent::Error(err);
                            if sender.send(Message::Text(ev.to_text().into())).await.is_err() {
                                break;
                            }
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        if sender.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(err)) => {
                        warn!(error = %err, "WS 读取失败");
                        break;
                    }
                }
            }

            // ── 服务器 → 客户端 ──────────────────────────────────────────
            event = rx.recv() => {
                match event {
                    Ok(ev) => {
                        if sender.send(Message::Text(ev.to_text().into())).await.is_err() {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        // 客户端消费太慢被丢弃了若干条消息，补发全量快照保证最终一致。
                        warn!(skipped, "WS 客户端消费过慢，补发状态快照");
                        if push_snapshot(&state, &mut sender).await.is_err() {
                            break;
                        }
                    }
                    Err(RecvError::Closed) => break,
                }
            }

            // ── 保活：即使长时间无事件也发一个 ping，避免中间层断开 ────────
            _ = tokio::time::sleep(Duration::from_secs(30)) => {
                if sender.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
        }
    }

    info!("WS 客户端已断开");
}

/// 处理客户端发来的文本消息。
/// 目前只支持 `ping`，后续可在此扩展面板上报等能力。
fn handle_client_message(text: &str) -> Result<(), String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("JSON 解析失败：{e}"))?;
    match value.get("type").and_then(|t| t.as_str()) {
        Some("ping") => Ok(()),
        Some(other) => Err(format!("暂不支持的消息类型：{other}")),
        None => Err("消息缺少 type 字段".to_string()),
    }
}

// ──────────────────────────── 错误类型 ────────────────────────────

/// 统一的 API 错误，序列化为 `{"error": "..."}`。
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

/// 供 `tracing` 等日志宏直接格式化（`error = %err`）。
/// 没有它 `warn!(error = %err)` 无法编译，而错误日志是排查接口报错的第一手信息。
impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.status.as_u16(), self.message)
    }
}

impl std::error::Error for ApiError {}

impl ApiError {
    /// 400 参数错误。
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    /// 404 资源不存在。
    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    /// 500 内部错误。
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }

    /// 409 状态冲突（例如未登录、版权受限）。
    pub fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
        }
    }

    /// 502 上游服务返回异常。
    pub fn bad_gateway(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: message.into(),
        }
    }

    /// 501 尚未实现。
    pub fn not_implemented(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_IMPLEMENTED,
            message: message.into(),
        }
    }

    /// 200 + `{"notice": "..."}`：操作成功但没有内容可做。
    ///
    /// 用于「已经是第一首，没有上一首了」这类正常边界：它不是错误，不该给非 2xx
    /// （界面会弹「请求失败」，用户以为坏了），也不该静默返回空快照（点了没反应）。
    pub fn notice(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::OK,
            message: message.into(),
        }
    }

    /// 是否是上面那种「无内容可做」的提示（决定响应体形状）。
    fn is_notice(&self) -> bool {
        self.status == StatusCode::OK
    }
}

impl From<queue::QueueError> for ApiError {
    fn from(err: queue::QueueError) -> Self {
        match err {
            queue::QueueError::NotFound(id) => ApiError::not_found(format!("队列项不存在：{id}")),
            // 历史为空：这是「没有上一首可回退」的正常情况，用 409 让界面给提示
            queue::QueueError::NoHistory => {
                ApiError::conflict("没有上一首可播放（播放历史为空）".to_string())
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // 「无内容可做」用 200 + `{"notice": ...}`：
        // 前端据此弹一条信息提示（如「没有更多了」），而不是错误。
        if self.is_notice() {
            let body = serde_json::json!({ "notice": self.message });
            return (self.status, Json(body)).into_response();
        }
        let body = serde_json::json!({ "error": self.message });
        (self.status, Json(body)).into_response()
    }
}
