//! 出站 HTTP 的代理探测(2026-08-28,v0.13.1 冒烟实录):GUI 进程不继承终端的
//! 代理环境变量,而 ureq/reqwest 默认也不读 macOS 系统代理——Clash「系统代理」
//! 模式下,应用直连 github.com / 豆包 时通时不通:一键更新报 error sending request,
//! Aing 的传输层失败占 97%(见 memory「Aing 失败是网络不是模型」)。
//!
//! 口径(codex 复审后):**按目标 URL 解析**——
//! - scheme 分流:https 目标用 HTTPS 代理,http 目标用 HTTP 代理(二者可不同);
//! - 绕过规则:NO_PROXY/no_proxy 列表、macOS ExceptionsList、ExcludeSimpleHostnames,
//!   以及 localhost/127.x/::1 恒直连——本地 LLM 端点与内网 webhook 绝不被塞进代理;
//! - 来源优先级:环境变量(终端/CI 启动)→ macOS 系统代理(scutil --proxy)→ 直连。
//! 只取 HTTP 代理(ureq 默认特性不含 SOCKS;Clash 的 7890 同时提供 HTTP CONNECT)。
//! 每次请求前现探(scutil 约 10ms),用户切换 Clash 立刻生效。

use std::time::Duration;

/// 一份代理配置快照。
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ProxyConf {
    pub http: Option<String>,
    pub https: Option<String>,
    /// 绕过表:主机名/域后缀(".corp"、"*.corp")/IPv4 CIDR/"<local>"。
    pub bypass: Vec<String>,
    /// 无点主机名(如 `intranet`)直连(macOS ExcludeSimpleHostnames)。
    pub exclude_simple: bool,
}

/// 环境变量 → macOS 系统代理 → 空。
pub(crate) fn detect() -> ProxyConf {
    let env = |keys: &[&str]| -> Option<String> {
        keys.iter().find_map(|k| {
            std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
        })
    };
    let https = env(&["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]).map(|v| normalize(&v));
    let http = env(&["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"]).map(|v| normalize(&v));
    let mut conf = ProxyConf {
        http,
        https,
        bypass: env(&["NO_PROXY", "no_proxy"])
            .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
            .unwrap_or_default(),
        exclude_simple: false,
    };
    if conf.http.is_none() && conf.https.is_none() {
        #[cfg(target_os = "macos")]
        if let Ok(out) = std::process::Command::new("scutil").arg("--proxy").output() {
            let sys = parse_scutil(&String::from_utf8_lossy(&out.stdout));
            // 环境变量没给代理时整份采用系统配置(含它的例外表)。
            conf = sys;
        }
    }
    conf
}

/// 针对目标 URL 决定代理:None = 直连。
pub(crate) fn proxy_for(conf: &ProxyConf, url: &str) -> Option<String> {
    let (scheme, host, port) = split_url(url)?;
    if is_bypassed(conf, host, port) {
        return None;
    }
    let chosen = match scheme {
        "https" => conf.https.clone(),
        "http" => conf.http.clone(),
        _ => None,
    }?;
    // 只支持 http(s):// 形态的代理(codex 二轮):ureq 未启用 socks 特性,socks 写法
    // 在 Proxy::new 时不报错、要到连接时才炸——提前拒绝,降级直连并留日志。
    if chosen.starts_with("http://") || chosen.starts_with("https://") {
        Some(chosen)
    } else {
        eprintln!("netproxy: 不支持的代理方案 {}(仅 http/https),本次直连", redact(&chosen));
        None
    }
}

/// 日志脱敏(codex 三轮):代理 URL 可能带 user:pass@,stderr.log 落盘不能留密码。
fn redact(url: &str) -> String {
    match url.split_once("://") {
        Some((sch, rest)) => match rest.split_once('@') {
            Some((_, host)) => format!("{sch}://***@{host}"),
            None => url.to_string(),
        },
        None => url.to_string(),
    }
}

/// (scheme, host, 显式端口或按 scheme 默认)。
fn split_url(url: &str) -> Option<(&str, &str, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let auth = rest.split(['/', '?', '#']).next()?;
    let hostport = auth.rsplit('@').next()?;
    // IPv6 字面量 [::1]:8080
    let (host, port) = if let Some(stripped) = hostport.strip_prefix('[') {
        let (h, tail) = stripped.split_once(']')?;
        (h, tail.strip_prefix(':').and_then(|p| p.parse().ok()))
    } else {
        match hostport.split_once(':') {
            Some((h, p)) => (h, p.parse().ok()),
            None => (hostport, None),
        }
    };
    let port = port.unwrap_or(if scheme == "https" { 443 } else { 80 });
    Some((scheme, host, port))
}

fn is_bypassed(conf: &ProxyConf, host: &str, port: u16) -> bool {
    let h = host.to_ascii_lowercase();
    if h == "localhost" || h == "::1" || h.starts_with("127.") || h.ends_with(".localhost") {
        return true;
    }
    if conf.exclude_simple && !h.contains('.') && !h.contains(':') {
        return true;
    }
    conf.bypass.iter().any(|rule| rule_matches(rule, &h, port))
}

fn rule_matches(rule: &str, host: &str, port: u16) -> bool {
    let mut r = rule.trim().to_ascii_lowercase();
    if r.is_empty() {
        return false;
    }
    // 带端口的规则(`host:8443`,codex 四轮):端口须与目标一致,再按主机匹配。
    // CIDR 形态含 '/' 无端口;IPv6 字面量规则不带端口(避免与冒号歧义)。
    if !r.contains('/') && r.matches(':').count() == 1 {
        if let Some((h, p)) = r.rsplit_once(':') {
            if let Ok(rp) = p.parse::<u16>() {
                if rp != port {
                    return false;
                }
                r = h.to_string();
            }
        }
    }
    if r == "*" {
        return true;
    }
    if r == "<local>" {
        // 无点主机名才算本地;IPv6 字面量含冒号无点,不能被当成简单主机名(codex 三轮)。
        return !host.contains('.') && !host.contains(':');
    }
    if let Some((net, bits)) = r.split_once('/') {
        if let (Some(n), Some(h), Ok(b)) = (ipv4(net), ipv4(host), bits.parse::<u32>()) {
            let mask = if b == 0 { 0 } else { u32::MAX << (32 - b.min(32)) };
            return n & mask == h & mask;
        }
        return false;
    }
    let suffix = r.trim_start_matches('*').trim_start_matches('.');
    host == suffix || host.ends_with(&format!(".{suffix}"))
}

fn ipv4(s: &str) -> Option<u32> {
    let parts: Vec<u32> = s.split('.').map(|p| p.parse::<u32>().ok()).collect::<Option<_>>()?;
    if parts.len() != 4 || parts.iter().any(|p| *p > 255) {
        return None;
    }
    Some((parts[0] << 24) | (parts[1] << 16) | (parts[2] << 8) | parts[3])
}

/// 无 scheme 的 `host:port` 补 http://;socks 写法照原样(ureq 无 socks 特性时会
/// 在建 Proxy 时报错,由 agent 降级直连)。
fn normalize(v: &str) -> String {
    if v.contains("://") {
        v.to_string()
    } else {
        format!("http://{v}")
    }
}

/// 解析 `scutil --proxy` 文本:HTTP/HTTPS 各自取(Enable=1 且 host/port 齐全),
/// 例外表 ExceptionsList(`<array> { 0 : x  1 : y }`)与 ExcludeSimpleHostnames。
pub(crate) fn parse_scutil(text: &str) -> ProxyConf {
    let get = |k: &str| -> Option<String> {
        text.lines().find_map(|l| {
            let l = l.trim();
            let (key, val) = l.split_once(':')?;
            (key.trim() == k).then(|| val.trim().to_string())
        })
    };
    let pick = |en: &str, host: &str, port: &str| -> Option<String> {
        if get(en).as_deref() != Some("1") {
            return None;
        }
        let (h, p) = (get(host)?, get(port)?);
        (!h.is_empty() && !p.is_empty()).then(|| format!("http://{h}:{p}"))
    };
    let mut bypass = Vec::new();
    let mut in_list = false;
    for l in text.lines() {
        let l = l.trim();
        if l.starts_with("ExceptionsList") {
            in_list = true;
            continue;
        }
        if in_list {
            if l.starts_with('}') {
                in_list = false;
                continue;
            }
            if let Some((_, v)) = l.split_once(':') {
                let v = v.trim();
                if !v.is_empty() {
                    bypass.push(v.to_string());
                }
            }
        }
    }
    ProxyConf {
        http: pick("HTTPEnable", "HTTPProxy", "HTTPPort"),
        https: pick("HTTPSEnable", "HTTPSProxy", "HTTPSPort"),
        bypass,
        exclude_simple: get("ExcludeSimpleHostnames").as_deref() == Some("1"),
    }
}

fn builder_for(url: &str, mut b: ureq::AgentBuilder) -> ureq::AgentBuilder {
    if let Some(p) = proxy_for(&detect(), url) {
        // ureq 2.12 只认 http:// 代理(codex 四轮):https:// 形态留给 reqwest 更新器
        // (system_proxy_url 照常返回),ureq 这边明示不支持并直连。
        if !p.starts_with("http://") {
            eprintln!("netproxy: ureq 不支持 {} 形态代理(仅 http://),本次直连", redact(&p));
            return b;
        }
        match ureq::Proxy::new(&p) {
            Ok(px) => b = b.proxy(px),
            // 代理 URL 非法(如 socks 写法)降级直连并打日志,绝不因代理配置把请求挡死。
            Err(e) => eprintln!("netproxy: 代理 {} 无法使用({e}),本次直连", redact(&p)),
        }
    }
    b
}

/// 针对目标 URL 的 ureq Agent(命中绕过表则直连)。timeout 由调用方在 Request 上设。
pub fn agent_for<S: AsRef<str>>(url: S) -> ureq::Agent {
    builder_for(url.as_ref(), ureq::AgentBuilder::new()).build()
}

/// 同上,带连接/读超时(模型下载那类长流用)。
pub fn agent_with_timeouts<S: AsRef<str>>(url: S, connect: Duration, read: Duration) -> ureq::Agent {
    builder_for(url.as_ref(), ureq::AgentBuilder::new().timeout_connect(connect).timeout_read(read))
        .build()
}

/// 前端一键更新用:tauri-plugin-updater 的 check({ proxy }) 需要显式代理 URL;
/// 按更新端点(https)的口径解析,绕过规则同样生效。
#[tauri::command]
pub fn system_proxy_url(target: String) -> Option<String> {
    proxy_for(&detect(), &target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conf() -> ProxyConf {
        ProxyConf {
            http: Some("http://127.0.0.1:7890".into()),
            https: Some("http://127.0.0.1:7891".into()),
            bypass: vec!["10.0.0.0/8".into(), "*.corp".into(), "example.org".into()],
            exclude_simple: true,
        }
    }

    #[test]
    fn scheme_selects_matching_proxy() {
        let c = conf();
        assert_eq!(proxy_for(&c, "https://github.com/x").as_deref(), Some("http://127.0.0.1:7891"));
        assert_eq!(proxy_for(&c, "http://webhook.example.com/").as_deref(), Some("http://127.0.0.1:7890"));
        assert_eq!(proxy_for(&c, "ftp://x"), None);
    }

    #[test]
    fn bypass_rules_keep_local_and_listed_hosts_direct() {
        let c = conf();
        for u in [
            "http://localhost:11434/v1",
            "http://127.0.0.1:8080/",
            "http://[::1]:9/",
            "http://10.1.2.3/hook",
            "https://llm.corp/v1",
            "https://example.org/",
            "http://intranet/",
        ] {
            assert_eq!(proxy_for(&c, u), None, "{u} 应直连");
        }
        assert!(proxy_for(&c, "https://11.1.2.3/").is_some(), "CIDR 外照走代理");
        assert!(proxy_for(&c, "https://notexample.org/").is_some(), "后缀匹配不能误伤前缀相似域");
    }

    #[test]
    fn scutil_parses_both_proxies_and_exceptions() {
        let text = "HTTPEnable : 1\nHTTPProxy : 127.0.0.1\nHTTPPort : 7890\nHTTPSEnable : 1\nHTTPSProxy : 10.0.0.2\nHTTPSPort : 8888\nExceptionsList : <array> {\n    0 : 192.168.0.0/16\n    1 : localhost\n    2 : *.local\n  }\nExcludeSimpleHostnames : 1\n";
        let c = parse_scutil(text);
        assert_eq!(c.http.as_deref(), Some("http://127.0.0.1:7890"));
        assert_eq!(c.https.as_deref(), Some("http://10.0.0.2:8888"));
        assert_eq!(c.bypass, vec!["192.168.0.0/16", "localhost", "*.local"]);
        assert!(c.exclude_simple);
        assert_eq!(proxy_for(&c, "https://192.168.1.5/"), None);
        assert_eq!(proxy_for(&c, "https://nas.local/"), None);
        let disabled = parse_scutil("HTTPEnable : 0\nHTTPProxy : 127.0.0.1\nHTTPPort : 7890\n");
        assert_eq!(disabled, ProxyConf::default());
    }

    #[test]
    fn local_rule_excludes_ipv6_literals_and_logs_are_redacted() {
        let c = ProxyConf {
            https: Some("http://127.0.0.1:7890".into()),
            bypass: vec!["<local>".into()],
            ..Default::default()
        };
        assert!(proxy_for(&c, "https://[2606:4700:4700::1111]/").is_some(), "公网 IPv6 不算本地");
        assert_eq!(proxy_for(&c, "https://intranet/"), None);
        assert_eq!(redact("socks5://user:pw@proxy:1"), "socks5://***@proxy:1");
        assert_eq!(redact("http://proxy:1"), "http://proxy:1");
    }

    #[test]
    fn port_qualified_bypass_rules_match_only_that_port() {
        let c = ProxyConf {
            https: Some("http://127.0.0.1:7890".into()),
            bypass: vec!["internal.example.com:8443".into()],
            ..Default::default()
        };
        assert_eq!(proxy_for(&c, "https://internal.example.com:8443/api"), None);
        assert!(proxy_for(&c, "https://internal.example.com/api").is_some(), "443 不匹配 8443 规则");
    }

    #[test]
    fn socks_proxy_is_rejected_up_front() {
        let c = ProxyConf { https: Some("socks5://127.0.0.1:1".into()), ..Default::default() };
        assert_eq!(proxy_for(&c, "https://github.com/"), None);
    }

    #[test]
    fn normalize_adds_scheme_only_when_missing() {
        assert_eq!(normalize("127.0.0.1:7890"), "http://127.0.0.1:7890");
        assert_eq!(normalize("http://a:1"), "http://a:1");
        assert_eq!(normalize("socks5://a:1"), "socks5://a:1");
    }

    /// 非法代理不能挡死请求:agent 必须仍能构建(降级直连)。
    #[test]
    fn agent_builds_even_with_unusable_proxy_env() {
        std::env::set_var("HTTPS_PROXY", "socks5://127.0.0.1:1");
        let _ = agent_for("https://example.com/");
        std::env::remove_var("HTTPS_PROXY");
    }
}
