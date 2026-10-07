//! HTML 页面模板与字符串注入工具。
//!
//! 这些函数只做纯字符串处理（不读磁盘、不依赖 axum），
//! 因此可以被 `webdist` 与 `server` 双向复用而不会形成循环依赖。

/// 转义 HTML 文本，避免把用户数据（错误信息）直接拼进页面。
fn escape_html(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// 从 `http://127.0.0.1:17777` 里拆出 `("127.0.0.1", 17777)`。
///
/// 解析失败时回落成 `("127.0.0.1", 0)`；前端在 `port == 0` 时会忽略它，
/// 改用 `base`，所以不会造成坏地址。
fn split_host_port(base: &str) -> (String, u16) {
    let rest = base.split("://").nth(1).unwrap_or(base);
    let rest = rest.split('/').next().unwrap_or(rest);
    match rest.rsplit_once(':') {
        Some((host, port)) => (
            host.to_string(),
            port.parse::<u16>().unwrap_or(0),
        ),
        None => (rest.to_string(), 0),
    }
}/// 把 `window.__BSR_SERVER__` 注入到页面 `<head>` 里。
///
/// ## 为什么同时给 `base` 与 `host`/`port`
/// 前端 `resolveBase()` 需要拼出**绝对地址**（内嵌预览是跨源的，
/// 相对路径 `/bg/x.jpg` 会被解析到 `tauri://localhost` 而 404）。
///
/// 历史上两处注入的字段**不一致**：
///  - `main.rs`（Tauri 窗口）注入 `{ host, port }`；
///  - 本文件（HTTP 服务）注入 `{ base }`。
/// 前端早期只认 `host`/`port`，于是在 HTTP 服务出来的页面上
/// `API_BASE` 成了空串。现在两个字段都给，前端两种都能认，避免再次错位。
pub fn inject_server_hint(html: &str, title: &str, server_hint: Option<&str>) -> String {
    let script = match server_hint {
        Some(base) => {
            let clean = base.replace('"', "");
            // 从 base 里拆出 host/port，供只认这两个字段的旧代码使用
            let (host, port) = split_host_port(&clean);
            format!(
                "<script>window.__BSR_SERVER__={{\"base\":\"{clean}\",\"host\":\"{host}\",\"port\":{port}}};</script>"
            )
        }
        None => String::new(),
    };

    let mut out = html.to_string();

    // 替换 <title>
    if let (Some(start), Some(end)) = (out.find("<title>"), out.find("</title>")) {
        if start < end {
            out.replace_range(start..end + "</title>".len(), &format!("<title>{}</title>", escape_html(title)));
        }
    }

    // 注入脚本：紧跟 <head> 之后
    if !script.is_empty() {
        if let Some(pos) = out.find("<head>") {
            out.insert_str(pos + "<head>".len(), &script);
        } else {
            out.insert_str(0, &script);
        }
    }

    out
}

/// 生成一个用于提示错误/说明的独立页面。
pub fn error_page(title: &str, body_html: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8" />
<title>{title}</title>
<style>
  body {{ margin: 0; padding: 40px; background: #0b1220; color: #e8eefc;
         font-family: 'Microsoft YaHei', system-ui, sans-serif; line-height: 1.7; }}
  h1 {{ font-size: 20px; margin: 0 0 12px; color: #7dd3fc; }}
  code, pre {{ background: #131c2e; border: 1px solid #24314b; border-radius: 6px;
               padding: 2px 6px; font-size: 13px; }}
  pre {{ padding: 10px 12px; overflow: auto; }}
</style>
</head>
<body>
<h1>{title}</h1>
{body}
</body>
</html>"#,
        title = escape_html(title),
        body = body_html,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injects_server_hint_and_title() {
        let html = "<html><head><title>old</title></head><body></body></html>";
        let out = inject_server_hint(html, "新标题", Some("http://127.0.0.1:17777"));
        assert!(out.contains("<title>新标题</title>"));
        assert!(out.contains("window.__BSR_SERVER__"));
        assert!(out.contains("http://127.0.0.1:17777"));
    }

    #[test]
    fn escapes_error_text() {
        let page = error_page("<bad>", "<p>x</p>");
        assert!(page.contains("&lt;bad&gt;"));
    }
}
