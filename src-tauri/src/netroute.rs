//! 下载线路(见 CONTEXT.md「下载」):从 GitHub 拉模型与升级包时,在**直连**与**加速镜像**
//! 之间自动选择,用户不手动切换。
//!
//! 规则:先走上次成功的线路(新装默认直连);连不上、卡住(STALL 内一个字节都没收到)
//! 或太慢(SLOW_WINDOW 内平均不到 SLOW_BPS)就换下一条,沿用同一个 .part 断点续传——
//! 镜像是「前缀 + 原始 URL」的透明代理,两条线路给的是同一份字节。所有线路都试过一遍
//! 仍不成,第二轮不再判慢(慢也比失败强),只拦连不上与卡住。
//! 成功的那条线路记进 `download_route` 文件,下次先用它。
//!
//! 取代旧的「镜像加速」开关(默认开、先镜像后原站):国外与网好的用户白走一道代理,
//! 国内用户镜像挂了要等很久才回退原站。

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// 加速镜像(ghproxy 风格:前缀 + 完整原始 URL),按序尝试。公共代理会波动,失效时改此列表并发版。
pub const MIRRORS: &[&str] = &["https://ghfast.top/", "https://gh-proxy.com/", "https://ghproxy.net/"];

/// 建连超时:死镜像/黑洞路由快速失败。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 卡住:这么久一个字节都没收到。
const STALL: Duration = Duration::from_secs(15);
/// 判慢的观察窗口与门槛:窗口内平均低于 SLOW_BPS 即换线路。
const SLOW_WINDOW: Duration = Duration::from_secs(20);
const SLOW_BPS: u64 = 200 * 1024;

/// 一条候选线路。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub url: String,
    pub mirror: bool,
}

/// 候选线路顺序:上次走镜像成功则镜像在前,否则直连在前。
pub fn candidates(origin: &str, prefer_mirror: bool) -> Vec<Candidate> {
    let direct = Candidate { url: origin.to_string(), mirror: false };
    let mirrors = MIRRORS.iter().map(|p| Candidate { url: format!("{p}{origin}"), mirror: true });
    if prefer_mirror {
        mirrors.chain(std::iter::once(direct)).collect()
    } else {
        std::iter::once(direct).chain(mirrors).collect()
    }
}

/// 上次成功的线路,存在 app 数据目录的一个小文件里(不进 settings.json:前端整份回写
/// 设置时会把后台刚记下的线路冲掉)。
#[derive(Debug, Clone)]
pub struct RouteMemory {
    path: PathBuf,
}

impl RouteMemory {
    pub fn new(app_data: &Path) -> Self {
        Self { path: app_data.join("download_route") }
    }

    pub fn prefers_mirror(&self) -> bool {
        fs::read_to_string(&self.path).map(|s| s.trim() == "mirror").unwrap_or(false)
    }

    pub fn remember(&self, mirror: bool) {
        let _ = fs::write(&self.path, if mirror { "mirror" } else { "direct" });
    }
}

/// 判慢:按固定窗口累计字节,窗口满时平均速度低于门槛即判慢。
#[derive(Debug)]
pub struct SpeedGate {
    window_start: Instant,
    window_bytes: u64,
}

impl SpeedGate {
    pub fn new(now: Instant) -> Self {
        Self { window_start: now, window_bytes: 0 }
    }

    /// 记一笔收到的字节;返回 true = 太慢,该换线路。
    pub fn too_slow(&mut self, now: Instant, n: u64) -> bool {
        self.window_bytes += n;
        let elapsed = now.duration_since(self.window_start);
        if elapsed < SLOW_WINDOW {
            return false;
        }
        let slow = self.window_bytes < SLOW_BPS * elapsed.as_secs();
        self.window_start = now;
        self.window_bytes = 0;
        slow
    }
}

/// 下载进度。
#[derive(Debug, Clone, Copy)]
pub struct Progress {
    pub received: u64,
    /// 0 = 服务端没给总长。
    pub total: u64,
    /// 当前走的是镜像。
    pub mirror: bool,
    /// 已经从首选线路换过(界面据此提示「网络较慢,已切换加速线路」)。
    pub switched: bool,
}

/// 一次尝试的失败原因。
#[derive(Debug)]
enum Fail {
    Cancelled,
    /// 换线路:太慢、卡住、连不上、5xx。
    Retry(String),
    /// 这条线路别再试:4xx。
    Skip(String),
}

/// 把 `origin` 下载到 `part`(断点续传)。返回 Ok(total) 时 `part` 已是完整文件
/// (服务端 416 = 本地已有全量,也算完成,由调用方校验)。
/// 取消返回 Err("cancelled"),保留 `part`。
pub fn fetch(
    origin: &str,
    part: &Path,
    memory: &RouteMemory,
    cancel: &AtomicBool,
    progress: &dyn Fn(Progress),
) -> Result<u64, String> {
    fetch_from(&candidates(origin, memory.prefers_mirror()), part, memory, cancel, progress)
}

fn fetch_from(
    list: &[Candidate],
    part: &Path,
    memory: &RouteMemory,
    cancel: &AtomicBool,
    progress: &dyn Fn(Progress),
) -> Result<u64, String> {
    let mut last_err = String::new();
    // 4xx 的线路第二轮不再试(同一地址再请求还是 4xx)。
    let mut skipped = vec![false; list.len()];
    for pass in 0..2 {
        let judge_slow = pass == 0;
        for (i, c) in list.iter().enumerate() {
            if skipped[i] {
                continue;
            }
            let switched = pass > 0 || i > 0;
            match attempt(c, part, judge_slow, switched, cancel, progress) {
                Ok(total) => {
                    memory.remember(c.mirror);
                    return Ok(total);
                }
                Err(Fail::Cancelled) => return Err("cancelled".into()),
                Err(Fail::Skip(e)) => {
                    eprintln!("下载线路: {} 失败,换下一条: {e}", c.url);
                    skipped[i] = true;
                    last_err = e;
                }
                Err(Fail::Retry(e)) => {
                    eprintln!("下载线路: {} 失败,换下一条: {e}", c.url);
                    last_err = e;
                }
            }
        }
    }
    Err(last_err)
}

fn attempt(
    c: &Candidate,
    part: &Path,
    judge_slow: bool,
    switched: bool,
    cancel: &AtomicBool,
    progress: &dyn Fn(Progress),
) -> Result<u64, Fail> {
    if cancel.load(Ordering::Relaxed) {
        return Err(Fail::Cancelled);
    }
    let mut offset = part.metadata().map(|m| m.len()).unwrap_or(0);
    let agent = crate::netproxy::agent_with_timeouts(&c.url, CONNECT_TIMEOUT, STALL);
    let req = agent.get(&c.url);
    let req = if offset > 0 { req.set("Range", &format!("bytes={offset}-")) } else { req };
    let resp = match req.call() {
        Ok(r) => r,
        Err(ureq::Error::Status(416, _)) => return Ok(offset),
        Err(ureq::Error::Status(code, _)) if (400..500).contains(&code) => {
            return Err(Fail::Skip(format!("HTTP {code}")));
        }
        Err(e) => return Err(Fail::Retry(e.to_string())),
    };
    let out = match resp.status() {
        206 => fs::OpenOptions::new().append(true).open(part),
        200 => {
            offset = 0; // 不支持 Range:从头来
            fs::File::create(part)
        }
        s => return Err(Fail::Retry(format!("HTTP {s}"))),
    }
    .map_err(|e| Fail::Skip(e.to_string()))?;
    let total = resp
        .header("Content-Length")
        .and_then(|v| v.parse::<u64>().ok())
        .map(|n| offset + n)
        .unwrap_or(0);
    let mut reader = resp.into_reader();
    let mut out = std::io::BufWriter::new(out);
    let mut received = offset;
    let mut buf = [0u8; 64 * 1024];
    let mut gate = SpeedGate::new(Instant::now());
    let mut last_emit = Instant::now() - Duration::from_secs(1);
    let report = |received| progress(Progress { received, total, mirror: c.mirror, switched });
    report(received);
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = out.flush();
            return Err(Fail::Cancelled);
        }
        let n = match reader.read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                let _ = out.flush();
                return Err(Fail::Retry(e.to_string()));
            }
        };
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).map_err(|e| Fail::Skip(e.to_string()))?;
        received += n as u64;
        if last_emit.elapsed() >= Duration::from_millis(250) {
            last_emit = Instant::now();
            report(received);
        }
        if judge_slow && gate.too_slow(Instant::now(), n as u64) {
            let _ = out.flush();
            return Err(Fail::Retry(crate::tr!("速度太慢", "Too slow")));
        }
    }
    out.flush().map_err(|e| Fail::Skip(e.to_string()))?;
    report(received);
    if total > 0 && received < total {
        return Err(Fail::Retry(crate::tr!("连接提前断开", "Connection closed early")));
    }
    Ok(received)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_first_by_default_and_mirror_first_after_a_mirror_success() {
        let o = "https://github.com/a/b/releases/download/x/m.tar.bz2";
        let d = candidates(o, false);
        assert_eq!(d[0], Candidate { url: o.into(), mirror: false });
        assert!(d[1..].iter().all(|c| c.mirror && c.url.ends_with(o)));
        let m = candidates(o, true);
        assert!(m[0].mirror);
        assert_eq!(m.last().unwrap(), &Candidate { url: o.into(), mirror: false });
        assert_eq!(d.len(), m.len());
    }

    #[test]
    fn route_memory_round_trips_and_defaults_to_direct() {
        let dir = tempfile::tempdir().unwrap();
        let mem = RouteMemory::new(dir.path());
        assert!(!mem.prefers_mirror());
        mem.remember(true);
        assert!(mem.prefers_mirror());
        mem.remember(false);
        assert!(!mem.prefers_mirror());
    }

    #[test]
    fn slow_only_after_a_full_window_below_the_floor() {
        let t0 = Instant::now();
        let mut g = SpeedGate::new(t0);
        // 窗口未满:再慢也不判。
        assert!(!g.too_slow(t0 + Duration::from_secs(5), 10));
        // 窗口满,20s 只收了 10+100KB:判慢。
        assert!(g.too_slow(t0 + SLOW_WINDOW, 100 * 1024));
        // 新窗口里够快:不判。
        let t1 = t0 + SLOW_WINDOW;
        assert!(!g.too_slow(t1 + SLOW_WINDOW, SLOW_BPS * 21));
    }

    /// 本机假服务器:每个连接按 `respond(请求头)` 回一段原始 HTTP 响应后关闭。
    fn serve(respond: impl Fn(&str) -> Vec<u8> + Send + 'static) -> String {
        use std::io::{BufRead, BufReader};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                let mut s = s;
                let _ = s.write_all(&respond(&head));
            }
        });
        format!("http://{addr}/pkg")
    }

    #[test]
    fn a_dead_route_falls_through_and_a_cut_download_resumes_on_the_next() {
        let body: Vec<u8> = (0..100u8).collect();
        let dead = serve(|_| b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec());
        // 宣称 100 字节,只发 40 就断开。
        let b1 = body.clone();
        let cut = serve(move |_| {
            let mut r = b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n".to_vec();
            r.extend(&b1[..40]);
            r
        });
        // 认 Range 的好线路。
        let b2 = body.clone();
        let good = serve(move |head| {
            let from = head
                .lines()
                .find_map(|l| l.strip_prefix("Range: bytes="))
                .and_then(|v| v.trim_end_matches('-').parse::<usize>().ok());
            match from {
                Some(n) => {
                    let mut r = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\n\r\n",
                        b2.len() - n
                    )
                    .into_bytes();
                    r.extend(&b2[n..]);
                    r
                }
                None => {
                    let mut r = b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n".to_vec();
                    r.extend(&b2);
                    r
                }
            }
        });
        let list = vec![
            Candidate { url: dead, mirror: false },
            Candidate { url: cut, mirror: false },
            Candidate { url: good, mirror: true },
        ];
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("x.part");
        let mem = RouteMemory::new(dir.path());
        let seen = std::sync::Mutex::new(Vec::new());
        let n = fetch_from(&list, &part, &mem, &AtomicBool::new(false), &|p| seen.lock().unwrap().push(p)).unwrap();
        assert_eq!(n, 100);
        assert_eq!(fs::read(&part).unwrap(), body, "断点续传拼出完整内容");
        assert!(mem.prefers_mirror(), "记住走通的线路");
        let seen = seen.into_inner().unwrap();
        let last = seen.last().unwrap();
        assert!(last.mirror && last.switched, "界面能知道已换到加速线路");
        assert!(seen.iter().any(|p| p.received == 40), "续传从 40 字节接着走");
    }
}
