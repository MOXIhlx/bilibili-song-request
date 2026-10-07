//! 排障示例：直连运行中的 mpv IPC，查询播放与音频输出的真实状态。
//!
//! ## 为什么需要它
//! 「没有声音」有太多种可能：mpv 静音、音量 0、音频设备选择错误、
//! 文件加载失败、进度没推进……界面上看到的 `position=0` 无法区分。
//! 本工具直接问 mpv 本人，把关键属性一次性打出来：
//! `idle-active` / `pause` / `mute` / `volume` / `audio-device` /
//! `audio-params` / `time-pos` / `duration` / `path` / `filename` /
//! `eof-reached` / `media-title`，以及已加载的音频轨列表。
//!
//! ## 用法
//! ```powershell
//! cargo run --example mpv_inspect                     # 默认 \\.\pipe\mpvpipe
//! cargo run --example mpv_inspect -- \\.\pipe\自定义
//! ```
//!
//! 需要程序正在运行（mpv 已启动）。本工具只读，不会改变播放状态。

use std::collections::HashMap;
use std::time::Duration;

use interprocess::local_socket::traits::tokio::Stream as _;
use interprocess::local_socket::{GenericFilePath, ToFsName};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

type MpvStream = interprocess::local_socket::tokio::Stream;

/// 要查询的属性（都是只读）。
const PROPS: &[&str] = &[
    "idle-active",
    "pause",
    "mute",
    "volume",
    "audio-device",
    "audio-device-list",
    "audio-params",
    "audio-codec-name",
    "audio-bitrate",
    "time-pos",
    "duration",
    "path",
    "filename",
    "media-title",
    "eof-reached",
    "core-idle",
    "cache-buffering-state",
    "demuxer-cache-time",
    "track-list",
    "playlist-count",
    "playlist-pos",
];

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let pipe = std::env::args()
        .nth(1)
        .unwrap_or_else(|| r"\\.\pipe\mpvpipe".to_string());
    println!("连接 mpv IPC：{pipe}\n");

    let name = std::path::Path::new(&pipe).to_fs_name::<GenericFilePath>()?;
    let stream = match tokio::time::timeout(Duration::from_secs(3), MpvStream::connect(name)).await
    {
        Ok(Ok(s)) => s,
        Ok(Err(err)) => {
            eprintln!("❌ 连接失败：{err}");
            eprintln!("   可能原因：mpv 未启动 / 程序未运行 / 管道名不是 {pipe}");
            std::process::exit(1);
        }
        Err(_) => {
            eprintln!("❌ 连接超时：3 秒内没能连上 {pipe}");
            std::process::exit(1);
        }
    };

    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half).lines();
    let mut pending: HashMap<u64, String> = HashMap::new();

    // 依次请求所有属性（request_id 与属性名对应）
    for (index, prop) in PROPS.iter().enumerate() {
        let id = index as u64 + 1;
        pending.insert(id, (*prop).to_string());
        let cmd = json!({ "command": ["get_property", prop], "request_id": id }).to_string();
        write_half.write_all(cmd.as_bytes()).await?;
        write_half.write_all(b"\n").await?;
    }
    write_half.flush().await?;

    println!("{:<26} {}", "属性", "值");
    println!("{}", "-".repeat(80));
    let mut answered = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);

    while answered < PROPS.len() && tokio::time::Instant::now() < deadline {
        let line = match tokio::time::timeout(Duration::from_secs(1), reader.next_line()).await {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => break,
            Ok(Err(_)) => break,
            Err(_) => continue,
        };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = value.get("request_id").and_then(|v| v.as_u64()) else {
            continue;
        };
        let Some(prop) = pending.remove(&id) else {
            continue;
        };
        answered += 1;

        let shown = match (value.get("error").and_then(|v| v.as_str()), value.get("data")) {
            // ⚠️ mpv 正常返回时 `error` 字段是字符串 "success"，**值在 data 里**；
            // 只有真正出错时 error 才是错误描述。早期版本误把 "success" 当错误打印，
            // 结果每个属性都显示 (success)，看不到任何有效信息。
            (Some("success"), Some(data)) => match data {
                // audio-device-list 太长，只打印数量
                Value::Array(list) if prop == "audio-device-list" => {
                    format!("共 {} 个设备", list.len())
                }
                // track-list 只打印音频轨
                Value::Array(list) if prop == "track-list" => {
                    let audio: Vec<String> = list
                        .iter()
                        .filter(|t| t.get("type").and_then(|v| v.as_str()) == Some("audio"))
                        .map(|t| {
                            format!(
                                "id={} codec={} selected={}",
                                t.get("id").map(|v| v.to_string()).unwrap_or_default(),
                                t.get("codec").and_then(|v| v.as_str()).unwrap_or("?"),
                                t.get("selected").map(|v| v.to_string()).unwrap_or_default()
                            )
                        })
                        .collect();
                    if audio.is_empty() {
                        "（没有音频轨！）".to_string()
                    } else {
                        audio.join(" | ")
                    }
                }
                Value::Null => "（null）".to_string(),
                other => {
                    let s = other.to_string();
                    if s.chars().count() > 140 {
                        format!("{}…", s.chars().take(140).collect::<String>())
                    } else {
                        s
                    }
                }
            },
            (Some("success"), None) => "（无数据）".to_string(),
            (Some(err), _) => format!("（{err}）"),
            (None, _) => "（无 error 字段）".to_string(),
        };
        println!("{prop:<26} {shown}");
    }

    // ── 汇总判读 ────────────────────────────────────────────────────────────
    println!("\n—— 快速判读 ——");
    println!("如果 `idle-active` 为 true → mpv 处于空闲，根本没加载文件；");
    println!("如果 `track-list` 没有音频轨 → 该地址不是可解码的音频流；");
    println!("如果 `mute` 为 true 或 `volume` 为 0 → 静音/音量为 0；");
    println!("如果 `audio-device` 是 `null`/`auto` 且 `audio-params` 为空 → 没有音频输出设备；");
    println!("如果 `time-pos` 一直不涨 → 文件在缓冲或无法播放（看 `cache-buffering-state`）。");

    Ok(())
}
