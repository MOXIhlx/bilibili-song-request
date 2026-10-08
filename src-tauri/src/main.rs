//! 本地弹幕点歌机 —— 桌面程序入口。
//!
//! 启动顺序（两件事互不阻塞）：
//!  1. 读取配置（`%APPDATA%/bilibili-song-request/config.json`）
//!  2. 在独立线程里跑 tokio runtime，拉起内嵌 axum 服务器（`127.0.0.1:17777`）
//!  3. 打开 Tauri 窗口，并把内嵌服务器地址注入 WebView（`window.__BSR_SERVER__`）
//!
//! 之所以把 axum 放在自己的线程/runtime，而不是 Tauri 的 `async_runtime`：
//! 内嵌服务器是长期后台任务，与窗口生命周期解耦，窗口关闭后任务被进程退出自然回收。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;

use bilibili_song_request_lib::bilibili::{AuthConfig, DanmakuHandle};
use bilibili_song_request_lib::binresolver;
use bilibili_song_request_lib::config::Config;
use bilibili_song_request_lib::models::{AppState, BilibiliState, MusicPlatform, PlayerState};
use bilibili_song_request_lib::music::{MusicService, NamedValue, StoredIn};
use bilibili_song_request_lib::player::{
    MpvConfig, MpvController, PlayerBackend, PlayerController, PlayerEvent,
};
use bilibili_song_request_lib::queue::{self, QueueStore};
use bilibili_song_request_lib::server;
use bilibili_song_request_lib::shutdown;
use bilibili_song_request_lib::state::{EventBus, ServerHandles, StateCell};
use bilibili_song_request_lib::VERSION;

use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use tracing::{debug, error, info, warn};

fn main() {
    init_tracing();

    // ── 配置 ────────────────────────────────────────────────────────────────
    let config = Config::load();
    info!(version = VERSION, base_url = %config.base_url(), "启动弹幕点歌机");
    if !config.bilibili.auto_connect {
        info!("未开启自动连接：可在控制台填写身份码后手动连接（阶段 3 生效）");
    }

    // ── 初始状态 ────────────────────────────────────────────────────────────
    let initial = AppState::new(
        VERSION,
        PlayerState {
            volume: config.player.volume,
            ..PlayerState::default()
        },
        BilibiliState::default(),
    );
    let state = StateCell::new(initial);
    let events = EventBus::default();

    // ── 恢复上次的播放队列 ──────────────────────────────────────────────────
    let queue_store = QueueStore::with_default_path();
    let restored = queue_store.restore(&state);
    // 恢复后按配置初始化「弹幕可点名额」。
    // 不初始化的话 `danmaku_slots` 会是 0，所有弹幕点歌都会被判成「队列已满」。
    queue::ensure_slots(&state, &config.rules);
    if restored > 0 {
        info!(restored, path = %queue_store.path().display(), "已恢复上次的播放队列");
    }

    // ── 内嵌服务器（独立线程 + 独立 runtime）────────────────────────────────
    // ⚠️ mpv 在这个 runtime **内部**启动（见 `spawn_embedded_server`）。
    // 曾经的做法是主线程用临时 runtime 启动 mpv，但 `block_on` 返回后该 runtime
    // 立刻被 drop，里面 spawn 的 IPC 读写任务随之被取消——表现为「mpv 在跑，
    // 但所有命令都报『命令通道已关闭』，进度永远是 0」。
    let handles = match spawn_embedded_server(state.clone(), events.clone(), config.clone()) {
        Ok(handles) => {
            info!("面板地址：{}", handles.panel_url());
            info!("控制台地址：{}", handles.dashboard_url());
            // 地址带默认样式 id：外观由命名样式决定（在 设置 → OBS 面板 里管理）
            info!(
                "OBS 浏览器源示例：{}?style={}",
                handles.panel_url(),
                config.default_style_id
            );
            handles
        }
        Err(err) => {
            // 端口被占用等情况下不让整个程序崩掉：仍然打开窗口，让用户在设置里改端口。
            error!(error = %err, "内嵌服务器启动失败，程序将以「降级」模式运行");
            state.mutate(|s| s.bilibili.last_error = Some(format!("内嵌服务器启动失败：{err}")));
            fallback_handles(config.server.port, state, events)
        }
    };

    // ── 自动连接 B 站弹幕 ───────────────────────────────────────────────────
    if config.bilibili.auto_connect {        match (&handles.danmaku, AuthConfig::from_config(&config.bilibili).validate()) {
            (Some(handle), Ok(())) => {
                let handle = handle.clone();
                let auth = AuthConfig::from_config(&config.bilibili);
                info!(auth = %auth.redacted(), "按配置自动连接 B 站弹幕");
                // 在服务器线程的 runtime 里异步发起连接；失败只记录日志，不影响窗口启动。
                tauri::async_runtime::spawn(async move {
                    if let Err(err) = handle.connect(auth).await {
                        error!(error = %err, "自动连接弹幕失败");
                    }
                });
            }
            (None, _) => warn!("内嵌服务器未启动，跳过自动连接"),
            (Some(_), Err(err)) => {
                warn!(error = %err, "已开启自动连接但配置不完整，跳过（请在控制台补全）");
            }
        }
    } else {
        info!("未开启自动连接：可在控制台填写身份码后手动连接");
    }

    // ── Tauri 窗口 ──────────────────────────────────────────────────────────
    let panel_url = handles.panel_url();
    let dashboard_url = handles.dashboard_url();
    let port = handles.port;
    let app_state = handles.state.clone();
    // Tauri 命令（登录/保存/清除 Cookie）需要拿到音乐服务。
    //
    // ⚠️ 这里必须注册成 `Arc<MusicService>`（而不是 `Option<...>`），
    // 否则 `try_state::<Arc<MusicService>>()` 永远返回 `None`，
    // 表现为登录抓到 Cookie 后却报「音乐服务未初始化」。
    // 降级模式（内嵌服务器起不来）下 `handles.music` 是 `None`，
    // 这时新建一个独立实例，保证登录/粘贴 Cookie 仍可用。
    let music_service = handles
        .music
        .clone()
        .unwrap_or_else(|| std::sync::Arc::new(MusicService::new()));

    let result = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(app_state)
        .manage(music_service)
        .invoke_handler(tauri::generate_handler![
            open_music_login,
            save_music_cookie,
            clear_music_cookie,
        ])
        .setup(move |app| {
            // 把内嵌服务器地址注入 WebView，前端据此访问 /api 与 /ws。
            if let Some(window) = app.get_webview_window("main") {
                let script = format!(
                    "window.__BSR_SERVER__ = {{ host: '127.0.0.1', port: {port} }};"
                );
                if let Err(err) = window.eval(&script) {
                    warn!(error = %err, "注入服务器地址失败");
                }
            }
            info!(panel = %panel_url, dashboard = %dashboard_url, "窗口已就绪");
            Ok(())
        })
        .on_window_event(|window, event| {
            // 窗口关闭：保存队列 → 请求退出 → 等待服务器线程收尾。
            // 必须走 shutdown 信号，否则服务器 runtime 不会被 drop，
            // mpv 子进程的 `kill_on_drop` 不生效，会留下孤立进程。
            //
            // ⚠️ 这个回调对**所有**窗口都会触发。必须只认主窗口：
            // 否则「关掉音乐平台登录窗口」会把整个程序一起退出
            // （实测踩到：登录成功后关窗 → 程序直接退出）。
            if let tauri::WindowEvent::Destroyed = event {
                if window.label() != MAIN_WINDOW_LABEL {
                    debug!(label = window.label(), "子窗口已销毁（不影响主程序）");
                    return;
                }
                // `StateCell` 内部是 Arc，克隆很廉价；这里拷出一份避免持有 tauri::State。
                let state = window
                    .app_handle()
                    .try_state::<StateCell>()
                    .map(|s| (*s).clone());
                shutdown_sequence(state);
            }
        })
        .run(tauri::generate_context!());

    // Tauri 事件循环已退出：再等一次服务器线程收尾（幂等）。
    shutdown::request();
    shutdown::wait_blocking(std::time::Duration::from_secs(3));

    if let Err(err) = result {
        error!(error = %err, "Tauri 启动失败");
        std::process::exit(1);
    }
}

/// 退出流程：保存队列 → 请求服务器 runtime 结束。
///
/// 步骤顺序有意为之：先落盘再请求退出，保证队列不丢。
fn shutdown_sequence(state: Option<StateCell>) {
    if let Some(state) = state {
        let store = QueueStore::with_default_path();
        match store.save(&state) {
            Ok(()) => info!(path = %store.path().display(), "退出前已保存队列"),
            Err(err) => warn!(error = %err, "退出前保存队列失败"),
        }
    }
    shutdown::request();
    // 给 runtime 一点时间 drop（它会在 drop 时结束 mpv 子进程）
    if shutdown::wait_blocking(std::time::Duration::from_secs(3)) {
        info!("已请求退出，正在清理播放器");
    }
}

/// 启动 mpv（若配置允许），失败时只记录警告并返回 `None`。
///
/// 未找到 mpv 不应阻止程序启动：主播仍能看到面板、管理队列、配置身份码，
/// 只是没有声音（`/api/player/status` 的 `available` 会是 false）。
///
/// **必须在服务器 runtime 内调用**：本函数会 spawn IPC 读写任务，
/// 若在临时 runtime 里调用，runtime 一 drop 这些任务就没了（详见调用点注释）。
async fn start_mpv(config: &Config) -> Option<Arc<MpvController>> {
    if !config.player.auto_start {
        info!("配置里关闭了播放器自动启动");
        return None;
    }

    // 解析 mpv 路径：配置优先，其次自动查找
    let binary = if config.player.mpv_binary.trim().is_empty() {
        match binresolver::find_mpv() {
            Ok(located) => {
                info!(path = %located.path.display(), source = %located.source, "已定位 mpv");
                located.path.display().to_string()
            }
            Err(message) => {
                warn!("{message}");
                warn!("播放功能不可用（仍可点歌、管理队列）");
                return None;
            }
        }
    } else {
        config.player.mpv_binary.trim().to_string()
    };

    let mpv_config = MpvConfig {
        binary,
        pipe_name: config.player.pipe_name.clone(),
        volume: config.player.volume,
        extra_args: config.player.extra_args.clone(),
    };

    match MpvController::spawn(mpv_config).await {
        Ok(controller) => {
            info!("mpv 已就绪，播放功能可用");
            Some(controller)
        }
        Err(err) => {
            warn!(error = %err, "mpv 启动失败，播放功能不可用");
            None
        }
    }
}

/// 启动队列持久化：播放状态变化后（带去抖）写入磁盘。
fn spawn_queue_persister(state: &StateCell, store: QueueStore) {
    let mut rx = state.subscribe();
    let state_for_task = state.clone();
    tokio::spawn(async move {
        let mut last_fingerprint = String::new();
        // 上次落盘时的播放位置。进度变化**不**进指纹（否则每秒都写盘），
        // 但也不能完全不存：否则异常退出（进程被杀）时「继续播放」的位置
        // 会退回很久以前。这里按"位置推进超过 10 秒"补一次写。
        let mut last_saved_position = 0.0_f64;
        loop {
            match rx.recv().await {
                Ok(_) => {
                    // 指纹必须覆盖所有**结构性**持久化字段，否则那些字段的变化永远不写盘。
                    let (fingerprint, position) = {
                        let guard = state_for_task.read();
                        let ids = |items: &[bilibili_song_request_lib::models::QueueItem]| {
                            items
                                .iter()
                                .map(|i| i.id.to_string())
                                .collect::<Vec<_>>()
                                .join(",")
                        };
                        (
                            format!(
                                "mode={:?}|queue=[{}]|playing=[{}]|cursor={}|idle=[{}]|idle_mode={:?}|idle_cur={:?}|idle_next={}|current={:?}|is_idle={}",
                                guard.play_mode,
                                ids(&guard.queue),
                                ids(&guard.playing),
                                guard.cursor,
                                ids(&guard.idle),
                                guard.idle_mode,
                                guard.idle_current,
                                guard.idle_next,
                                guard.current.as_ref().map(|i| i.id.to_string()),
                                guard.current_is_idle,
                            ),
                            guard.player.position,
                        )
                    };
                    let structure_changed = fingerprint != last_fingerprint;
                    let position_advanced = (position - last_saved_position).abs() >= 10.0;
                    if !structure_changed && !position_advanced {
                        continue;
                    }
                    last_fingerprint = fingerprint;
                    last_saved_position = position;
                    if let Err(err) = store.save(&state_for_task) {
                        warn!(error = %err, "队列持久化失败");
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// 服务器启动失败时的降级句柄：沿用同一份状态（已写入错误信息）供窗口使用。
fn fallback_handles(port: u16, state: StateCell, events: EventBus) -> ServerHandles {
    // 降级场景下没有真实的服务器任务，用 `None` 占位。
    ServerHandles {
        port,
        state,
        events,
        danmaku: None,
        music: None,
        player: None,
        join: None,
    }
}

/// 在独立线程中启动 tokio runtime 与内嵌服务器。
///
/// 同步等待服务器绑定结果（最多 10 秒），这样主线程拿到的一定是**真实**端口。
///
/// mpv 也在这个 runtime 里启动：它的 IPC 读写任务是 `tokio::spawn` 出来的，
/// 必须活在一个长期存在的 runtime 里，否则任务会被立刻取消。
fn spawn_embedded_server(
    state: StateCell,
    events: EventBus,
    config: Config,
) -> anyhow::Result<ServerHandles> {
    let port = config.server.port;
    // 通道回传：真实端口、弹幕控制器、音乐服务、播放控制器、服务器任务句柄。
    let (tx, rx) = std::sync::mpsc::channel::<
        anyhow::Result<(
            u16,
            DanmakuHandle,
            Arc<MusicService>,
            Option<Arc<PlayerController>>,
            tokio::task::JoinHandle<()>,
        )>,
    >();

    // 线程内需要自己的一份克隆，原始值留给返回的 `ServerHandles`。
    let thread_state = state.clone();
    let thread_events = events.clone();
    let thread_config = config.clone();

    std::thread::Builder::new()
        .name("bsr-http-server".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(2)
                .thread_name("bsr-worker")
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    let _ = tx.send(Err(anyhow::anyhow!("创建 tokio runtime 失败：{err}")));
                    return;
                }
            };

            runtime.block_on(async move {
                // ── mpv（先在服务器 runtime 里启动，再交给 serve）──────────────
                let (backend, player_events): (
                    Option<Arc<dyn PlayerBackend>>,
                    Option<tokio::sync::broadcast::Receiver<PlayerEvent>>,
                ) = match start_mpv(&thread_config).await {
                    Some(mpv) => {
                        // 事件订阅要在装箱成 trait 对象**之前**拿到
                        // （trait 上没有订阅方法）。
                        let events_rx = mpv.subscribe_player_events();
                        (Some(Arc::clone(&mpv) as Arc<dyn PlayerBackend>), Some(events_rx))
                    }
                    None => (None, None),
                };

                // 先按配置端口尝试；失败时退化为「系统分配端口」，保证服务一定能起来。
                let mut result = server::serve(
                    thread_state.clone(),
                    thread_events.clone(),
                    thread_config.clone(),
                    backend,
                    player_events,
                )
                .await;
                if result.is_err() && port != 0 {
                    warn!(port, "端口绑定失败，改用系统分配端口重试");
                    let mut fallback = thread_config.clone();
                    fallback.server.port = 0;
                    result =
                        server::serve(thread_state.clone(), thread_events.clone(), fallback, None, None)
                            .await;
                }

                match result {
                    Ok((actual_port, _ctx, danmaku, music, player, join)) => {
                        // 队列持久化在这个 runtime 里跑（播放事件循环已由
                        // `PlayerController::with_event_loop` 在 serve 内部启动）
                        spawn_queue_persister(&thread_state, QueueStore::with_default_path());
                        let _ = tx.send(Ok((actual_port, danmaku, music, player, join)));
                    }
                    Err(err) => {
                        let _ = tx.send(Err(err));
                        return;
                    }
                }

                // 服务器任务已经 spawn 出去了；这里把 runtime 停在「退出信号」上。
                //
                // ⚠️ 不能用 `future::pending()`：那样 runtime 永远不结束，
                // 进程退出时它不会被 drop，mpv 子进程的 `kill_on_drop` 也就不会触发
                // （实测会出现「程序关了 mpv 还在」的残留进程）。
                shutdown::wait().await;
                info!("收到退出信号，服务器 runtime 即将结束");
            });
        })
        .map_err(|err| anyhow::anyhow!("创建服务器线程失败：{err}"))?;

    match rx.recv_timeout(std::time::Duration::from_secs(10)) {
        Ok(Ok((actual_port, danmaku, music, player, join))) => Ok(ServerHandles {
            port: actual_port,
            state,
            events,
            danmaku: Some(danmaku),
            music: Some(music),
            player,
            join: Some(join),
        }),
        Ok(Err(err)) => Err(err),
        Err(err) => Err(anyhow::anyhow!("等待内嵌服务器就绪超时：{err}")),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 音乐平台登录（内嵌 WebView）
// ─────────────────────────────────────────────────────────────────────────────

/// 音乐平台登录页地址。
fn login_url(platform: MusicPlatform) -> &'static str {
    match platform {
        MusicPlatform::Netease => "https://music.163.com/#/login",
        MusicPlatform::Qq => "https://y.qq.com/",
    }
}

/// 各平台用于识别「本域」的域名后缀，用于 Cookie 诊断输出（只影响日志）。
fn cookie_host_hint(platform: MusicPlatform) -> &'static str {
    bilibili_song_request_lib::music::cookie_host_hint(platform)
}

/// 登录窗口的标签。
const LOGIN_WINDOW_LABEL: &str = "music-login";
/// 主窗口标签（与 `tauri.conf.json` 的 `app.windows[].label` 一致）。
///
/// 用于区分「主窗口关闭（要退出程序）」与「子窗口关闭（什么都不做）」。
const MAIN_WINDOW_LABEL: &str = "main";
/// 轮询登录态的间隔。
const LOGIN_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(800);
/// 连续多少次取不到登录窗口才认定「用户关掉了窗口」。
///
/// 实测背景：WebView2 在站点间跳转（例如网易云登录会跳到 `music.163.com` 的
/// 登录子路径、QQ 音乐会跳到 `ptlogin2.qq.com`）时，管理器里可能瞬时查不到窗口。
/// 若一查不到就判取消，登录会在刚开始时就失败。
const MAX_MISSING_STREAK: u32 = 4;
/// 向平台接口校验登录态的间隔。
///
/// 不能每轮都问：既慢又容易触发风控。抓到候选 Cookie 后每 2.4 秒验一次即可。
const LOGIN_VERIFY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(2400);
/// 登录窗口最长等待时间。
const LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// 登录结果事件名（前端监听它刷新状态）。
pub const EVENT_MUSIC_LOGIN: &str = "music-login-result";

/// 登录结果载荷。
#[derive(Clone, serde::Serialize)]
pub struct MusicLoginResult {
    /// 平台。
    pub platform: MusicPlatform,
    /// 是否成功。
    pub success: bool,
    /// 说明（成功时是存放位置，失败时是原因）。
    pub message: String,
    /// 成功时是否已判定为登录。
    pub logged_in: bool,
}

/// 打开内嵌登录窗口，登录成功后自动抓取 Cookie 并写入凭据库。
///
/// 流程：
/// 1. 打开平台登录页（独立窗口，不影响主窗口）；
/// 2. 每 800ms 轮询一次该窗口的 Cookie，直到出现登录态 Cookie 或超时（5 分钟）；
/// 3. 成功后由 `save_music_cookie` 写入凭据库，关闭登录窗口，并发出
///    [`EVENT_MUSIC_LOGIN`] 事件通知前端。
#[tauri::command]
async fn open_music_login(
    app: tauri::AppHandle,
    platform: MusicPlatform,
) -> Result<(), String> {
    // 已存在同名窗口时先关掉重建，避免开出一堆登录窗口
    if let Some(existing) = app.get_webview_window(LOGIN_WINDOW_LABEL) {
        let _ = existing.close();
    }

    let url = login_url(platform);
    let title = format!("登录{}", platform.display_name());
    let app_for_window = app.clone();

    // ⚠️ 关键：必须等窗口**真正创建完成**再开始轮询。
    //
    // `run_on_main_thread` 只是把闭包**排队**投递到主线程，命令本身立刻返回。
    // 如果这时就启动轮询，第一次 `get_webview_window` 会拿到 `None`
    // （窗口还没注册进管理器），于是被误判成「用户关掉了窗口」——
    // 表现为登录窗口一闪而过、日志里 `polls=0`、提示「操作取消」。
    // 这是实测定位到的问题。
    let (created_tx, created_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();

    // 窗口必须在主线程创建。
    app.run_on_main_thread(move || {
        let result = WebviewWindowBuilder::new(
            &app_for_window,
            LOGIN_WINDOW_LABEL,
            WebviewUrl::External(url.parse().expect("登录地址应为合法 URL")),
        )
        .title(title)
        .inner_size(1024.0, 720.0)
        .resizable(true)
        .build();

        let outcome = match result {
            Ok(window) => {
                // 给窗口装上销毁监听：记录「谁在什么时候关掉了它」。
                // 排障时这段日志决定了是「用户关的」还是「窗口自己没了」。
                let app_watch = app_for_window.clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::Destroyed = event {
                        info!(
                            label = LOGIN_WINDOW_LABEL,
                            elapsed_ms = app_watch
                                .get_webview_window(LOGIN_WINDOW_LABEL)
                                .map(|_| 0u64)
                                .unwrap_or(0),
                            "音乐登录窗口已销毁"
                        );
                    }
                });
                Ok(())
            }
            Err(err) => {
                error!(error = %err, "创建音乐登录窗口失败");
                Err(format!("无法创建登录窗口：{err}"))
            }
        };
        if outcome.is_err() {
            let message = outcome.as_ref().err().cloned().unwrap_or_default();
            let _ = app_for_window.emit(
                EVENT_MUSIC_LOGIN,
                MusicLoginResult {
                    platform,
                    success: false,
                    message,
                    logged_in: false,
                },
            );
        }
        // 接收端可能已经不在（命令出错返回），忽略发送失败
        let _ = created_tx.send(outcome);
    })
    .map_err(|e| format!("无法在主线程创建窗口：{e}"))?;

    // 等窗口创建结果：最多 5 秒，避免主线程异常时永久挂住
    match tokio::time::timeout(std::time::Duration::from_secs(5), created_rx).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(message))) => return Err(message),
        Ok(Err(_)) => return Err("创建登录窗口的任务提前结束".to_string()),
        Err(_) => return Err("创建登录窗口超时（主线程无响应）".to_string()),
    }

    info!(?platform, url, "已打开音乐平台登录窗口");

    // 后台轮询登录态
    let app_for_poll = app.clone();
    tauri::async_runtime::spawn(async move {
        let deadline = std::time::Instant::now() + LOGIN_TIMEOUT;
        let host_hint = cookie_host_hint(platform);
        let mut last_report = String::new();
        let mut polls = 0u32;
        let mut last_values: Vec<NamedValue> = Vec::new();
        let mut missing_streak = 0u32;
        // 平台接口校验的节流时间（避免频繁打接口触发风控）
        let mut last_verify = std::time::Instant::now() - LOGIN_VERIFY_INTERVAL;

        loop {
            if std::time::Instant::now() >= deadline {
                finish_login(
                    &app_for_poll,
                    platform,
                    Err("等待登录超时（5 分钟），已取消".to_string()),
                );
                return;
            }

            // 用户手动关掉窗口 → 视为取消。
            //
            // 但**不能一查不到就判定关闭**：WebView2 在跨进程导航/站点切换时，
            // 管理器里可能瞬时拿不到窗口。因此要求连续多次都拿不到才认定关闭。
            // 失败时把「关窗前最后一次看到的 Cookie」一起写进日志，
            // 否则只剩一句「窗口已关闭」，无法区分
            // 「用户真的关了窗口」和「平台写了别的 Cookie 名」。
            let window = match app_for_poll.get_webview_window(LOGIN_WINDOW_LABEL) {
                Some(window) => {
                    missing_streak = 0;
                    window
                }
                None => {
                    missing_streak += 1;
                    if missing_streak < MAX_MISSING_STREAK {
                        debug!(?platform, missing_streak, "暂时取不到登录窗口，稍后重试");
                        tokio::time::sleep(LOGIN_POLL_INTERVAL).await;
                        continue;
                    }
                    let detail =
                        bilibili_song_request_lib::music::describe_failure(platform, &last_values);
                    warn!(?platform, polls, missing_streak, %detail, "登录窗口已关闭");
                    finish_login(
                        &app_for_poll,
                        platform,
                        Err(format!("登录窗口已关闭，操作取消。{detail}")),
                    );
                    return;
                }
            };

            match window.cookies() {
                Ok(cookies) => {
                    polls += 1;
                    last_values = to_named_values(&cookies);

                    let summary = format!(
                        "第 {polls} 次轮询：共 {} 个 Cookie；其中 {} 相关 {} 个；名字：{}",
                        last_values.len(),
                        host_hint,
                        last_values
                            .iter()
                            .filter(|c| c.domain.contains(host_hint))
                            .count(),
                        last_values
                            .iter()
                            .map(|c| c.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    if summary != last_report {
                        debug!(?platform, %summary, "登录态轮询");
                        last_report = summary;
                    }

                    if let Some((login_name, cookie)) =
                        bilibili_song_request_lib::music::build_cookie_string(platform, &last_values)
                    {
                        // ⚠️ 关键：抓到 Cookie **不等于**登录成功。
                        //
                        // 网易云在页面加载时就会写 `MUSIC_U`（未登录也有）。
                        // 若直接据此判定成功，退出登录后再打开登录窗口会「秒登录」，
                        // 用户根本没机会登录。因此这里必须让平台接口做事实校验：
                        // 只有真的能查到账号，才算登录成功。
                        let now = std::time::Instant::now();
                        if now.duration_since(last_verify) < LOGIN_VERIFY_INTERVAL {
                            // 校验有节流（避免频繁打接口触发风控），等下一轮再验
                            tokio::time::sleep(LOGIN_POLL_INTERVAL).await;
                            continue;
                        }
                        last_verify = now;

                        match verify_cookie_online(&app_for_poll, platform, &cookie).await {
                            Some(account) => {
                                info!(?platform, login_name, %account, "平台接口确认登录有效");
                            }
                            None => {
                                debug!(
                                    ?platform,
                                    "抓到候选 Cookie，但平台接口未确认是登录账号，继续等待"
                                );
                                // 继续轮询：用户可能还没真正完成登录
                                tokio::time::sleep(LOGIN_POLL_INTERVAL).await;
                                continue;
                            }
                        }

                        info!(
                            ?platform,
                            login_name,
                            cookie_len = cookie.len(),
                            "已获取登录 Cookie"
                        );
                        let saved = save_cookie_to_store(&app_for_poll, platform, &cookie).await;
                        finish_login(&app_for_poll, platform, saved);
                        return;
                    }
                }
                Err(err) => {
                    warn!(error = %err, "读取 WebView Cookie 失败，稍后重试");
                }
            }

            tokio::time::sleep(LOGIN_POLL_INTERVAL).await;
        }
    });

    Ok(())
}

/// 把 WebView 里的 Cookie 转成库里的纯函数能处理的表示，
/// 具体的判定与拼接逻辑在 `music::login`（有单测覆盖）。
fn to_named_values(cookies: &[tauri::webview::Cookie<'static>]) -> Vec<NamedValue> {
    cookies
        .iter()
        .map(|c| {
            NamedValue::new(
                c.name().to_string(),
                c.value().to_string(),
                c.domain().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// 用抓到的 Cookie 去平台接口验证登录是否真的有效，返回账号标识。
///
/// 实现方式：把 Cookie 塞进一个**只存在于内存**的 `MusicService` 实例里查账号，
/// **不写凭据库**——校验失败时不能留下「脏」凭据。
async fn verify_cookie_online(
    app: &tauri::AppHandle,
    platform: MusicPlatform,
    cookie: &str,
) -> Option<String> {
    let _ = app;
    let service = MusicService::with_in_memory_cookie_for(platform, cookie);
    service.verify_login(platform).await
}

/// 保存 Cookie 到凭据库，返回给用户看的说明。
async fn save_cookie_to_store(
    app: &tauri::AppHandle,
    platform: MusicPlatform,
    cookie: &str,
) -> Result<(StoredIn, bool), String> {
    let Some(music) = app.try_state::<std::sync::Arc<MusicService>>() else {
        return Err("音乐服务未初始化".to_string());
    };
    match music.save_cookie(platform, cookie).await {
        Ok(stored_in) => {
            let status = music.status(platform).await;
            Ok((stored_in, status.logged_in))
        }
        Err(err) => Err(format!("保存 Cookie 失败：{err}")),
    }
}

/// 结束登录流程：关窗 + 发事件。
fn finish_login(
    app: &tauri::AppHandle,
    platform: MusicPlatform,
    outcome: Result<(StoredIn, bool), String>,
) {
    if let Some(window) = app.get_webview_window(LOGIN_WINDOW_LABEL) {
        let _ = window.close();
    }

    let payload = match outcome {
        Ok((stored_in, logged_in)) => MusicLoginResult {
            platform,
            success: true,
            message: stored_in.describe().to_string(),
            logged_in,
        },
        Err(message) => {
            warn!(?platform, %message, "音乐平台登录未完成");
            MusicLoginResult {
                platform,
                success: false,
                message,
                logged_in: false,
            }
        }
    };

    if let Err(err) = app.emit(EVENT_MUSIC_LOGIN, payload) {
        warn!(error = %err, "发送登录结果事件失败");
    }
}

/// 手动保存 Cookie（浏览器里复制粘贴的场景）。
#[tauri::command]
async fn save_music_cookie(
    app: tauri::AppHandle,
    platform: MusicPlatform,
    cookie: String,
) -> Result<MusicLoginResult, String> {
    let Some(music) = app.try_state::<std::sync::Arc<MusicService>>() else {
        return Err("音乐服务未初始化".to_string());
    };
    let stored_in = music
        .save_cookie(platform, &cookie)
        .await
        .map_err(|e| format!("保存 Cookie 失败：{e}"))?;
    let status = music.status(platform).await;
    Ok(MusicLoginResult {
        platform,
        success: true,
        message: stored_in.describe().to_string(),
        logged_in: status.logged_in,
    })
}

/// 清除某平台的 Cookie（凭据库 + WebView）。
///
/// ## 为什么必须同时清 WebView
/// 内嵌登录窗口用的是**持久化**的 WebView 配置目录，平台的会话 Cookie 就存在里面。
/// 如果只清凭据库，平台自身的登录态还在 WebView 里：下次点「登录」时，
/// 登录窗口一打开就已经是登录状态，第一次轮询立刻抓到 Cookie →
/// 表现为「退出登录后再点登录，秒登录」，用户根本没机会重新登录。
///
/// 这里按**域**筛选（`music::is_platform_cookie`），只删该平台的 Cookie，
/// 不会影响 B 站/其他站点的登录态。
#[tauri::command]
async fn clear_music_cookie(
    app: tauri::AppHandle,
    platform: MusicPlatform,
) -> Result<(), String> {
    let Some(music) = app.try_state::<std::sync::Arc<MusicService>>() else {
        return Err("音乐服务未初始化".to_string());
    };
    music
        .clear_cookie(platform)
        .await
        .map_err(|e| format!("清除 Cookie 失败：{e}"))?;

    // 再清 WebView 里的平台 Cookie（失败不影响凭据已清除的结果，但要记录）
    match clear_webview_browsing_data(&app).await {
        Ok(removed) => info!(?platform, removed, "已清空 WebView 浏览数据（含平台 Cookie）"),
        Err(err) => warn!(?platform, error = %err, "清空 WebView 浏览数据失败"),
    }
    Ok(())
}

/// 在**主线程**上清空 WebView 的浏览数据（Cookie/存储），返回清理前的 Cookie 数。
///
/// ## 为什么用「清空全部」而不是按条删除
/// Tauri/wry 提供了 `delete_cookie`，但**在本项目实测中它不起作用**：
/// 调用不报错、计数也正常，可是回读 Cookie 数量与内容完全没变，
/// `MUSIC_U` 依然在。结果就是「退出登录」表面成功、实际没退出，
/// 再点登录立刻又被判定为已登录（用户看到的是「秒登录」）。
///
/// 本应用的 WebView 配置目录只服务两类页面：
///  1. 主窗口 —— 本地 `tauri.localhost` 控制台，没有第三方会话；
///  2. 音乐平台登录窗口。
/// 所以清空整个 WebView 存储是安全且符合用户预期的（点「退出登录」就该清干净）。
///
/// 数据访问必须在主线程执行，因此用 `run_on_main_thread` + oneshot 回传结果。
async fn clear_webview_browsing_data(app: &tauri::AppHandle) -> Result<usize, String> {
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<usize, String>>();
    let app_for_task = app.clone();

    app.clone()
        .run_on_main_thread(move || {
            let mut removed = 0usize;
            let mut errors: Vec<String> = Vec::new();
            for label in [MAIN_WINDOW_LABEL, LOGIN_WINDOW_LABEL] {
                let Some(window) = app_for_task.get_webview_window(label) else {
                    continue;
                };
                let before = window.cookies().map(|c| c.len()).unwrap_or(0);
                match window.clear_all_browsing_data() {
                    Ok(()) => {
                        // 回读确认：不能只信「调用没报错」
                        let after = window.cookies().map(|c| c.len()).unwrap_or(0);
                        debug!(label, before, after, "已清空 WebView 浏览数据");
                        removed += before.saturating_sub(after);
                    }
                    Err(err) => errors.push(format!("{label}: {err}")),
                }
            }
            let _ = tx.send(if removed == 0 && !errors.is_empty() {
                Err(errors.join("；"))
            } else {
                Ok(removed)
            });
        })
        .map_err(|e| format!("无法在主线程清理 Cookie：{e}"))?;

    match tokio::time::timeout(std::time::Duration::from_secs(8), rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("清理 Cookie 的任务提前结束".to_string()),
        Err(_) => Err("清理 Cookie 超时".to_string()),
    }
}

/// 初始化日志：控制台 + 文件（`%APPDATA%/bilibili-song-request/logs/`）。
fn init_tracing() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_env("BSR_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,wry=warn,tao=warn,bilibili_song_request_lib=debug"));

    let registry = tracing_subscriber::registry().with(filter);

    let log_dir = Config::config_dir().join("logs");
    let file_writer = std::fs::create_dir_all(&log_dir).ok().and_then(|_| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_dir.join("app.log"))
            .ok()
    });

    match file_writer {
        Some(file) => {
            let file_layer = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file));
            let _ = registry.with(tracing_subscriber::fmt::layer()).with(file_layer).try_init();
        }
        None => {
            let _ = registry.with(tracing_subscriber::fmt::layer()).try_init();
        }
    }
}
