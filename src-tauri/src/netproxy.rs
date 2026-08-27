//! 出站 HTTP 的代理探测(2026-08-28,v0.13.1 冒烟实录):GUI 进程不继承终端的
//! 代理环境变量,而 ureq/reqwest 默认也不读 macOS 系统代理——Clash「系统代理」
//! 模式下,应用直连 github.com / 豆包 时通时不通:一键更新报 error sending request,
//! Aing 的传输层失败占 97%(见 memory「Aing 失败是网络不是模型」)。
//!
//! 统一口径:先看代理环境变量(终端/CI 启动),再看 macOS 系统代理(scutil --proxy),
//! 都没有就直连。**只取 HTTP 代理**(ureq 默认特性不含 SOCKS;Clash 的 7890 同时
//! 提供 HTTP CONNECT)。每次请求前现探(scutil 约 10ms),用户切换 Clash 立刻生效。

use std::time::Duration;

/// 代理 URL(如 `http://127.0.0.1:7890`);None = 直连。
pub fn system_proxy() -> Option<String> {
    for key in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return Some(normalize(&v));
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = std::process::Command::new("scutil").arg("--proxy").output() {
            if let Some(p) = parse_scutil(&String::from_utf8_lossy(&out.stdout)) {
                return Some(p);
            }
        }
    }
    None
}

/// 无 scheme 的 `host:port` 补 http://;socks 写法照原样(ureq 无 socks 特性时会
/// 在建 Proxy 时报错,由调用方降级直连)。
fn normalize(v: &str) -> String {
    if v.contains("://") {
        v.to_string()
    } else {
        format!("http://{v}")
    }
}

/// 解析 `scutil --proxy` 文本:HTTPS 代理优先(更新/LLM 都是 https),其次 HTTP。
/// 只认 Enable=1 且 host/port 齐全的条目。
pub(crate) fn parse_scutil(text: &str) -> Option<String> {
    let get = |k: &str| -> Option<String> {
        text.lines().find_map(|l| {
            let l = l.trim();
            let (key, val) = l.split_once(':')?;
            (key.trim() == k).then(|| val.trim().to_string())
        })
    };
    for (en, host, port) in [
        ("HTTPSEnable", "HTTPSProxy", "HTTPSPort"),
        ("HTTPEnable", "HTTPProxy", "HTTPPort"),
    ] {
        if get(en).as_deref() == Some("1") {
            if let (Some(h), Some(p)) = (get(host), get(port)) {
                if !h.is_empty() && !p.is_empty() {
                    return Some(format!("http://{h}:{p}"));
                }
            }
        }
    }
    None
}

/// 带代理(若探到)的 ureq Agent。代理 URL 非法(如 socks 写法)时降级直连并打日志,
/// 绝不因代理配置把请求挡死。timeout 由调用方在 Request 上照旧设置。
pub fn agent() -> ureq::Agent {
    let mut b = ureq::AgentBuilder::new();
    if let Some(p) = system_proxy() {
        match ureq::Proxy::new(&p) {
            Ok(px) => b = b.proxy(px),
            Err(e) => eprintln!("netproxy: 代理 {p} 无法使用({e}),本次直连"),
        }
    }
    b.build()
}

/// 与 agent() 同款,但带连接/读超时(模型下载那类长流用)。
pub fn agent_with_timeouts(connect: Duration, read: Duration) -> ureq::Agent {
    let mut b = ureq::AgentBuilder::new().timeout_connect(connect).timeout_read(read);
    if let Some(p) = system_proxy() {
        match ureq::Proxy::new(&p) {
            Ok(px) => b = b.proxy(px),
            Err(e) => eprintln!("netproxy: 代理 {p} 无法使用({e}),本次直连"),
        }
    }
    b.build()
}

/// 前端一键更新用:tauri-plugin-updater 的 check({ proxy }) 需要显式代理 URL。
#[tauri::command]
pub fn system_proxy_url() -> Option<String> {
    system_proxy()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scutil_prefers_https_then_http_and_requires_enable() {
        let both = "HTTPEnable : 1\nHTTPProxy : 127.0.0.1\nHTTPPort : 7890\nHTTPSEnable : 1\nHTTPSProxy : 10.0.0.2\nHTTPSPort : 8888\n";
        assert_eq!(parse_scutil(both).as_deref(), Some("http://10.0.0.2:8888"));
        let http_only = "HTTPEnable : 1\nHTTPProxy : 127.0.0.1\nHTTPPort : 7890\nHTTPSEnable : 0\n";
        assert_eq!(parse_scutil(http_only).as_deref(), Some("http://127.0.0.1:7890"));
        let disabled = "HTTPEnable : 0\nHTTPProxy : 127.0.0.1\nHTTPPort : 7890\n";
        assert_eq!(parse_scutil(disabled), None);
        assert_eq!(parse_scutil(""), None);
    }

    #[test]
    fn normalize_adds_scheme_only_when_missing() {
        assert_eq!(normalize("127.0.0.1:7890"), "http://127.0.0.1:7890");
        assert_eq!(normalize("http://a:1"), "http://a:1");
        assert_eq!(normalize("socks5://a:1"), "socks5://a:1");
    }

    /// 非法代理不能挡死请求:agent() 必须仍能构建(降级直连)。
    #[test]
    fn agent_builds_even_with_unusable_proxy_env() {
        std::env::set_var("HTTPS_PROXY", "socks5://127.0.0.1:1");
        let _ = agent();
        std::env::remove_var("HTTPS_PROXY");
    }
}
