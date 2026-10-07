/**
 * 与后端 / Tauri 事件名对应的常量。
 *
 * 前端多处需要监听同一事件，集中在这里可以避免字符串散落各处拼错。
 */

/** 音乐平台登录结果事件（由 Rust `main.rs` 的 `EVENT_MUSIC_LOGIN` 发出）。 */
export const EVENT_MUSIC_LOGIN = 'music-login-result'
