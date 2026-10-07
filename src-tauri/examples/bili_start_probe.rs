//! 排障示例：打印 B 站开放平台 `v2/app/start` 的**实际请求内容**，并可选择性真发一次。
//!
//! ## 为什么需要它
//! 连接失败时最难判断的是「到底是我的请求构造错了，还是网络/服务端的问题」。
//! 本工具把签名过程完全摊开（待签名串、MD5、Authorization），
//! 可以拿去和 B 站官方的[签名验证工具]逐字节对比。
//!
//! [签名验证工具]: https://bilibili.apifox.cn/doc-885734
//!
//! ## 用法
//! ```powershell
//! # 只打印请求内容（不联网，安全，可以随便跑）
//! cargo run --example bili_start_probe -- --dry-run <app_id> <access_key_id> <access_key_secret> <身份码>
//!
//! # 真发一次（会联网）
//! cargo run --example bili_start_probe -- <app_id> <access_key_id> <access_key_secret> <身份码>
//! ```
//!
//! 输出里的 `x-bili-content-md5` 与 `Authorization` 可以直接填进官方验证工具核对。

use bilibili_song_request_lib::bilibili::{build_headers, AuthConfig, START_URL};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();

    if positional.len() < 4 {
        eprintln!(
            "用法：bili_start_probe [--dry-run] <app_id> <access_key_id> <access_key_secret> <身份码>"
        );
        eprintln!("（--dry-run 只打印请求内容，不联网）");
        std::process::exit(2);
    }

    let auth = AuthConfig {
        app_id: positional[0].clone(),
        access_key_id: positional[1].clone(),
        access_key_secret: positional[2].clone(),
        code: positional[3].clone(),
    };

    // 与真实请求完全一致的 body
    let body = serde_json::json!({ "code": auth.code, "app_id": auth.app_id }).to_string();
    let headers = build_headers(&auth, &body);

    println!("=== 请求 ===");
    println!("POST {START_URL}");
    println!("body: {body}");
    println!("\n=== 请求头 ===");
    for (key, value) in &headers {
        println!("{key}: {value}");
    }

    // 把待签名串也还原出来，方便与官方工具逐行比对
    let md5 = headers
        .iter()
        .find(|(k, _)| k == "x-bili-content-md5")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let ts = headers
        .iter()
        .find(|(k, _)| k == "x-bili-timestamp")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let nonce = headers
        .iter()
        .find(|(k, _)| k == "x-bili-signature-nonce")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let payload = bilibili_song_request_lib::bilibili::signature_payload(
        &auth.access_key_id,
        &md5,
        ts.parse().unwrap_or(0),
        &nonce,
    );
    println!("\n=== 待签名串（官方规范：6 行，字典序，末行无换行）===");
    println!("{payload}");
    println!("（共 {} 行）", payload.lines().count());
    println!("\n提示：把上面这段和 x-bili-accesskeyid/secret 填进官方签名验证工具，");
    println!("      算出的 Authorization 应与上面请求头里的一致。");

    if dry_run {
        println!("\n--dry-run：不发送请求。");
        return;
    }

    println!("\n=== 发送请求 ===");
    let client = reqwest::Client::new();
    let mut request = client.post(START_URL).timeout(std::time::Duration::from_secs(15)).body(body);
    for (key, value) in &headers {
        request = request.header(key, value);
    }

    match request.send().await {
        Ok(response) => {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            println!("HTTP {status}");
            println!("响应体（{} 字节）：{}", text.len(), &text[..text.len().min(800)]);
            match bilibili_song_request_lib::bilibili::parse_start_response(
                status.as_u16(),
                &text,
            ) {
                Ok(session) => {
                    println!("\n✅ 解析成功：");
                    println!("  wss_url = {}", session.wss_url);
                    println!("  心跳间隔 = {} 秒", session.heartbeat_interval);
                    println!("  game_id = {:?}", session.game_id);
                    println!("  room_id = {:?}", session.room_id);
                    println!("  auth_body 长度 = {}", session.auth_body.len());
                }
                Err(err) => println!("\n❌ 解析失败：{err}"),
            }
        }
        Err(err) => println!("❌ 请求失败（网络层）：{err}"),
    }
}
