//! 一次性探针：验证 interprocess + mpv 的 IPC 管道是否**保持长连接**。
//!
//! 背景：应用里「写入 mpv IPC」的任务会在几秒内悄悄结束（连错误都没报），
//! 导致播放命令根本没送到 mpv。为区分「interprocess 用法问题」与
//! 「mpv 管道本身不持久」，这里用最小代码复现同样的连接方式并记录每次写的结果。
//!
//! 运行：cargo run --example ipc_probe -- <mpv路径> <管道名>

use std::time::Duration;

use interprocess::local_socket::traits::tokio::Stream as _;
use interprocess::local_socket::{GenericFilePath, ToFsName};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mpv = std::env::args()
        .nth(1)
        .unwrap_or_else(|| r"C:\Program Files\MPV Player\mpv.exe".to_string());
    let pipe = std::env::args()
        .nth(2)
        .unwrap_or_else(|| r"\\.\pipe\bsr-probe".to_string());

    println!("[probe] mpv = {mpv}");
    println!("[probe] pipe = {pipe}");

    let mut child = tokio::process::Command::new(&mpv)
        .args([
            "--idle=yes",
            "--no-video",
            "--keep-open=no",
            &format!("--input-ipc-server={pipe}"),
            "--volume=30",
            "--audio-display=no",
            "--cache=yes",
            "--cache-secs=30",
            "--network-timeout=15",
            "--stream-lavf-o=reconnect=1,reconnect_streamed=1,reconnect_delay_max=5",
            "--term-status-msg=",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;

    // 轮询等待管道
    let name = std::path::Path::new(&pipe).to_fs_name::<GenericFilePath>()?;
    let mut stream = None;
    for attempt in 1..=40 {
        match interprocess::local_socket::tokio::Stream::connect(name.clone()).await {
            Ok(s) => {
                println!("[probe] 连接成功 attempt={attempt}");
                stream = Some(s);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    let stream = stream.ok_or("连接 mpv IPC 超时")?;

    let (read_half, write_half) = tokio::io::split(stream);

    // 读任务：持续打印 mpv 的每一行输出
    tokio::spawn(async move {
        let mut lines = BufReader::new(read_half).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => println!("[read] {line}"),
                Ok(None) => {
                    println!("[read] EOF（对端关闭）");
                    break;
                }
                Err(err) => {
                    println!("[read] 错误: {err}");
                    break;
                }
            }
        }
    });

    // 写任务：每 2 秒发一条命令，并把每次写的结果打出来
    let mut writer = write_half;
    for i in 1..=10u32 {
        let cmd = format!("{{\"command\":[\"get_property\",\"time-pos\"],\"request_id\":{i}}}\n");
        match writer.write_all(cmd.as_bytes()).await {
            Ok(()) => {
                print!("[write] #{i} write_all OK ");
                match writer.flush().await {
                    Ok(()) => println!("flush OK"),
                    Err(err) => {
                        println!("flush 失败: {err}");
                        break;
                    }
                }
            }
            Err(err) => {
                println!("[write] #{i} write_all 失败: {err}");
                break;
            }
        }
        // 顺便看一下 mpv 进程是否还活着
        match child.try_wait() {
            Ok(Some(status)) => {
                println!("[probe] mpv 已退出: {status}");
                break;
            }
            Ok(None) => {}
            Err(err) => println!("[probe] try_wait 失败: {err}"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    println!("[probe] 结束，杀掉 mpv");
    let _ = child.kill().await;
    Ok(())
}
