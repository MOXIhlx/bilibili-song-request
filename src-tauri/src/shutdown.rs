//! 进程优雅退出信号。
//!
//! ## 为什么需要
//! 服务器线程里的 runtime 为了保持存活，一直阻塞在 `future::pending()` 上。
//! 窗口关闭时如果直接 `std::process::exit`，这个 runtime 会被**丢弃而不 drop**，
//! 于是它内部持有的 mpv 子进程句柄的 `kill_on_drop(true)` 也就不生效——
//! 结果就是「程序关了，mpv 还在后台」（实测确认过这个现象）。
//!
//! 因此这里提供一个一次性信号：
//!  - 窗口关闭时调用 [`request`]；
//!  - 服务器 runtime 用 [`wait`] 等待它，收到后正常结束 `block_on`，
//!    runtime 被 drop → 子进程被 kill；
//!  - 主线程用 [`wait_blocking`] 带超时地等服务器线程收尾，给清理留出时间。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use tokio::sync::Notify;

/// 全局退出通知。
fn notify() -> &'static Notify {
    static NOTIFY: OnceLock<Notify> = OnceLock::new();
    NOTIFY.get_or_init(Notify::new)
}

/// 是否已经请求退出。
fn requested_flag() -> &'static AtomicBool {
    static REQUESTED: OnceLock<AtomicBool> = OnceLock::new();
    REQUESTED.get_or_init(|| AtomicBool::new(false))
}

/// 请求退出（幂等，可从任意线程调用）。
pub fn request() {
    if !requested_flag().swap(true, Ordering::SeqCst) {
        notify().notify_waiters();
    }
}

/// 是否已经请求过退出。
pub fn is_requested() -> bool {
    requested_flag().load(Ordering::SeqCst)
}

/// 异步等待退出信号（供 tokio 任务使用）。
pub async fn wait() {
    // 处理「请求早于等待」的竞态
    if is_requested() {
        return;
    }
    notify().notified().await;
}

/// 同步等待退出信号，最多 `timeout`。
///
/// 返回 `true` 表示在超时前收到了信号。
pub fn wait_blocking(timeout: Duration) -> bool {
    if is_requested() {
        return true;
    }
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(_) => return is_requested(),
    };
    runtime.block_on(async {
        tokio::time::timeout(timeout, wait()).await.is_ok()
    })
}

/// 测试与多次运行用：重置信号状态。
#[cfg(test)]
pub fn reset() {
    requested_flag().store(false, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 全局退出信号是**进程级单例**，而 cargo test 默认并行执行。
    /// 因此所有会读写该状态的用例必须串行化，否则会出现
    /// 「一个用例 reset 掉另一个用例刚 request 的状态」这类随机失败。
    fn serial_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn request_then_wait_blocking_returns_true() {
        let _guard = serial_guard();
        reset();
        assert!(!is_requested());
        request();
        assert!(is_requested());
        assert!(wait_blocking(Duration::from_millis(100)));
    }

    #[test]
    fn request_is_idempotent() {
        let _guard = serial_guard();
        reset();
        request();
        request();
        assert!(is_requested());
    }

    #[tokio::test]
    async fn wait_resolves_after_request() {
        let _guard = serial_guard();
        reset();
        let handle = tokio::spawn(async {
            wait().await;
            true
        });
        // 确保任务已进入等待
        tokio::time::sleep(Duration::from_millis(50)).await;
        request();
        let result = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("应被唤醒")
            .expect("任务不应 panic");
        assert!(result);
    }

    #[test]
    fn wait_blocking_times_out_without_request() {
        let _guard = serial_guard();
        reset();
        // 未请求退出时应超时返回 false（而不是永久阻塞）
        assert!(!wait_blocking(Duration::from_millis(80)));
    }
}
