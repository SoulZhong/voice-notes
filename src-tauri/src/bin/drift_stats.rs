//! 漂移报告全库汇总:真实分布的出口(设计文档第四节)。
//! 用法: drift_stats <data_root> [--since YYYYMMDD]   递归扫描 drift_report.json
//!
//! `--since` 是给 issue #99 的 E2 用的:**PR #103 之前的报告不能进基线**。那批数据里的
//! 重锚与 rate_ppm 全是测量 bug 的产物——采样率对账当时用 tap 线程到达间隔算,消费端一被
//! 抢占就报出假低率,触发 rate_fix 改写采样率并清相位。拿它算 P50/P95 会得到完全错误的
//! 分布(实测:故障期三场贡献了全库 2327 次重锚里的 2325 次)。
//! 判据按**笔记目录名的日期前缀**(YYYYMMDD-HHMMSS)过滤,粗但够用:笔记 id 就是建档时刻。
//! 边界说明:续录场次(history)跟着所属笔记一起算,极端情况下一篇 #103 之前建的笔记在
//! 之后续录,会被这条过滤一并挡掉——宁可少算,不可混入污染数据。

#[derive(Default)]
struct Agg {
    sessions: usize,
    degraded: usize,
    rel_ppm: Vec<f64>,
    reanchors: u64,
    anomalies: Vec<String>,
    // Codex review Fix 3(P2)配套:build_report 现在只在双源都 converged 时才发布
    // inter_track(未收敛的估计是假精度),drift_stats 的 rel_ppm 分布默认自动跳过
    // 被过滤掉的场次(as_f64() 对 null 返回 None)。这个计数让"多少场被过滤掉了"
    // 这件事本身可见,而不是悄悄少算。
    with_inter_track: usize,
    /// 按设备名归类(issue #100 条 6):内置麦、蓝牙 HFP、USB 声卡的漂移特性天差
    /// 地别,混在一起的分布看不出"最差设备组合"。设备名缺失(旧报告/非 macOS/
    /// 查询失败)的场次归到 "(未知设备)"。
    by_device: std::collections::BTreeMap<String, DeviceAgg>,
}

#[derive(Default)]
struct DeviceAgg {
    sessions: usize,
    reanchors: u64,
    degraded: usize,
}

const UNKNOWN_DEVICE: &str = "(未知设备)";

/// 一份报告(顶层 = 最新场次)可能带 history(issue #100 条 4:续录/多次停录的
/// 旧场次)。逐场次全部计入,否则续录笔记只有最后一场进统计。
fn ingest(agg: &mut Agg, v: &serde_json::Value, origin: &str) {
    for old in v["history"].as_array().into_iter().flatten() {
        ingest_one(agg, old, origin);
    }
    ingest_one(agg, v, origin);
}

fn ingest_one(agg: &mut Agg, v: &serde_json::Value, origin: &str) {
    agg.sessions += 1;
    if v["sources"].as_object().map_or(false, |m| m.values().any(|s| s["quality"] == "degraded")) {
        agg.degraded += 1;
    }
    // ingest 已经用 as_f64():inter_track 为 null(单源场次,或双源未同时收敛)时
    // 这里自动返回 None、静默跳过——不必额外处理,只需顺带统计一下"有多少场次
    // 真正贡献了这个数",让下方 rel_ppm 分布看着比 sessions 少时不会显得莫名其妙。
    if let Some(p) = v["inter_track"]["rel_ppm"].as_f64() {
        agg.rel_ppm.push(p.abs());
        agg.with_inter_track += 1;
    }
    // 设备维度取 mic 的设备名(system 走 SCK,不绑具体输入设备)。
    let device = v["sources"]["mic"]["device_name"]
        .as_str()
        .unwrap_or(UNKNOWN_DEVICE)
        .to_string();
    let degraded_here = v["sources"]
        .as_object()
        .map_or(false, |m| m.values().any(|s| s["quality"] == "degraded"));
    let d = agg.by_device.entry(device).or_default();
    d.sessions += 1;
    if degraded_here {
        d.degraded += 1;
    }
    for s in v["sources"].as_object().into_iter().flat_map(|m| m.values()) {
        let n = s["reanchors"].as_u64().unwrap_or(0);
        agg.reanchors += n;
        d.reanchors += n;
    }
    for a in v["anomalies"].as_array().into_iter().flatten() {
        agg.anomalies.push(format!("{origin}: {a}"));
    }
}

/// 报告路径是否通过 `--since` 门槛。笔记 id 是 `YYYYMMDD-HHMMSS`,字典序即时间序,
/// 故按 `since` 的长度截取同长前缀直接比:给 `20260814` 是按天,给 `20260814-170000`
/// 精确到秒。**必须支持到秒**——修复合并那天当天既有污染场次也有干净场次(本机实测:
/// 8/14 18:07 那场起才干净),只按天分不开。
/// 目录名不像笔记 id(无 8 位数字日期前缀)时**放行**:宁可多算一场,也不静默丢掉
/// 不认识的目录布局(测试夹具、导出目录)。
fn passes_since(report_path: &std::path::Path, since: Option<&str>) -> bool {
    let Some(since) = since else { return true };
    let Some(name) = report_path.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()) else {
        return true;
    };
    // 按**字节**切:目录名可能含非 ASCII(中文笔记夹、导出目录),按 char 边界之外切
    // &str 会 panic(Codex P2)。笔记 id 全是 ASCII,字节序比较与字符串序等价。
    let (nb, sb) = (name.as_bytes(), since.as_bytes());
    if nb.len() < 8 || !nb[..8].iter().all(|c| c.is_ascii_digit()) {
        return true;
    }
    let n = sb.len().min(nb.len());
    nb[..n] >= sb[..n]
}

/// `--since` 的取值形状:`YYYYMMDD` 或 `YYYYMMDD-HHMMSS`(与笔记 id 同构,可直接比前缀)。
fn valid_since(v: &str) -> bool {
    let b = v.as_bytes();
    let shape_ok = match b.len() {
        8 => b.iter().all(|c| c.is_ascii_digit()),
        15 => b[8] == b'-' && b[..8].iter().chain(&b[9..]).all(|c| c.is_ascii_digit()),
        _ => false,
    };
    if !shape_ok {
        return false;
    }
    // 光看形状不够(Codex 十七轮 P2):`20260814-240000` / `-176000` 全是数字也过形状,
    // 但字典序过滤会据此静默排掉一批本该计入的场次——与 fail-closed 的初衷相反。
    let num = |r: std::ops::Range<usize>| v[r].parse::<u32>().unwrap_or(u32::MAX);
    let (mo, d) = (num(4..6), num(6..8));
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return false;
    }
    if b.len() == 15 {
        let (h, mi, sec) = (num(9..11), num(11..13), num(13..15));
        if h > 23 || mi > 59 || sec > 59 {
            return false;
        }
    }
    true
}

fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() { return None; }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    Some(sorted[idx])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ingest_and_percentiles() {
        let mut agg = Agg::default();
        for ppm in [10.0, -50.0, 120.0] {
            let v = serde_json::json!({
                "schema": 1,
                "sources": {"mic": {"quality": "hw", "reanchors": 1}},
                "inter_track": {"rel_ppm": ppm},
                "anomalies": []
            });
            ingest(&mut agg, &v, "n");
        }
        agg.rel_ppm.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(agg.sessions, 3);
        assert_eq!(agg.reanchors, 3);
        assert_eq!(percentile(&agg.rel_ppm, 0.5), Some(50.0));
        assert_eq!(percentile(&agg.rel_ppm, 1.0), Some(120.0));
        assert_eq!(agg.with_inter_track, 3, "三条都带 inter_track,应全计入");
    }

    /// issue #100 条 6:按设备归类。同一台机器上内置麦/蓝牙 HFP/USB 声卡的漂移
    /// 特性完全不同,混在一起的分布没有意义——「最差设备组合」得能被看出来。
    /// 另:条 4 起 drift_report 会把旧场次收进 history,续录笔记的每一场都要计入,
    /// 否则统计仍然只看得到最后一场。
    #[test]
    fn aggregates_per_device_and_walks_history() {
        let mut agg = Agg::default();
        let v = serde_json::json!({
            "schema": 1,
            "sources": {"mic": {"quality": "hw", "reanchors": 2, "device_name": "MacBook Pro麦克风"}},
            "inter_track": serde_json::Value::Null,
            "anomalies": [],
            "history": [
                {"schema": 1, "anomalies": [],
                 "sources": {"mic": {"quality": "degraded", "reanchors": 7, "device_name": "OpenRun Pro by Shokz"}}},
                {"schema": 1, "anomalies": [],
                 "sources": {"mic": {"quality": "hw", "reanchors": 1, "device_name": "MacBook Pro麦克风"}}}
            ]
        });
        ingest(&mut agg, &v, "note");
        assert_eq!(agg.sessions, 3, "history 里的两场也要计入");
        assert_eq!(agg.reanchors, 10);
        assert_eq!(agg.degraded, 1, "只有蓝牙那场降级");
        let built_in = agg.by_device.get("MacBook Pro麦克风").expect("内置麦应有条目");
        assert_eq!((built_in.sessions, built_in.reanchors, built_in.degraded), (2, 3, 0));
        let bt = agg.by_device.get("OpenRun Pro by Shokz").expect("蓝牙应有条目");
        assert_eq!((bt.sessions, bt.reanchors, bt.degraded), (1, 7, 1));
    }

    /// E2 基线的污染门:PR#103 之前建档的笔记必须能被一条命令挡在统计之外
    /// (那批报告的重锚/rate_ppm 是测量 bug 的产物,见文件头)。
    #[test]
    fn since_filter_keeps_only_notes_dated_on_or_after() {
        use std::path::Path;
        let p = |s: &str| Path::new(s).to_path_buf();
        let since = Some("20260814");
        assert!(passes_since(&p("/d/notes/20260814-180747/drift_report.json"), since), "当天建档要留下");
        assert!(passes_since(&p("/d/notes/20260815-074838/drift_report.json"), since), "之后建档要留下");
        assert!(!passes_since(&p("/d/notes/20260813-235959/drift_report.json"), since), "之前建档要挡掉");
        // 精确到秒:修复落地那天当天要能切开(本机实测 8/14 18:07 那场起才干净)
        let sec = Some("20260814-170000");
        assert!(passes_since(&p("/d/notes/20260814-180747/drift_report.json"), sec), "当天晚于阈值的留下");
        assert!(!passes_since(&p("/d/notes/20260814-161211/drift_report.json"), sec), "当天早于阈值的挡掉");
        assert!(!passes_since(&p("/d/notes/20260814-100830/drift_report.json"), sec));
        assert!(passes_since(&p("/d/notes/20260815-074838/drift_report.json"), sec), "次日一律留下");
        // 不给 --since 就全收(与加过滤前的行为一致)
        assert!(passes_since(&p("/d/notes/20260101-000000/drift_report.json"), None));
        // 目录名不是笔记 id(测试夹具、导出目录等):放行,不静默丢
        assert!(passes_since(&p("/d/fixtures/case-a/drift_report.json"), since));
        // 非 ASCII 目录名:按字节切不能 panic(中文名 8 字节处正好在字符中间)
        assert!(passes_since(&p("/d/中文目录名字/drift_report.json"), since));
        assert!(passes_since(&p("/d/中文目录名字/drift_report.json"), Some("20260814-170000")));
    }

    /// 这道门失效时必须是 fail-closed:`--since` 值写错就退出,绝不悄悄退回"全都算"——
    /// 那样拿到的是一份看着正常、实则混了污染场次的基线。
    #[test]
    fn since_value_shape_is_validated() {
        assert!(valid_since("20260814"));
        assert!(valid_since("20260814-170000"));
        assert!(!valid_since(""));
        assert!(!valid_since("2026081"), "位数不足");
        assert!(!valid_since("20260814170000"), "缺分隔符");
        assert!(!valid_since("20260814-17000"), "时分秒位数不对");
        assert!(!valid_since("2026-08-14"), "带横线的日期不是笔记 id 形状");
        // 形状对但日期/时刻不可能的值也要拒:字典序过滤会据此静默排掉一批本该计入的场次
        assert!(!valid_since("20260814-240000"), "24 点不存在");
        assert!(!valid_since("20260814-176000"), "76 分不存在");
        assert!(!valid_since("20261314"), "13 月不存在");
        assert!(!valid_since("20260800"), "0 日不存在");
        assert!(valid_since("20260814-235959"), "边界值要放行");
        assert!(!valid_since("--out"), "把下一个 flag 当成值必须被挡下");
    }

    /// Codex review Fix 3(P2)配套:inter_track 为 null 的场次(单源,或双源未同时
    /// 收敛)不得计入 rel_ppm 分布,但 with_inter_track/sessions 之比要能看出它被过滤了。
    #[test]
    fn sessions_without_inter_track_are_excluded_but_counted() {
        let mut agg = Agg::default();
        let with = serde_json::json!({
            "schema": 1,
            "sources": {"mic": {"quality": "hw", "reanchors": 0}},
            "inter_track": {"rel_ppm": 42.0},
            "anomalies": []
        });
        let without = serde_json::json!({
            "schema": 1,
            "sources": {"mic": {"quality": "hw", "reanchors": 0}},
            "inter_track": serde_json::Value::Null,
            "anomalies": []
        });
        ingest(&mut agg, &with, "a");
        ingest(&mut agg, &without, "b");
        assert_eq!(agg.sessions, 2);
        assert_eq!(agg.with_inter_track, 1, "只有一场真正带 inter_track");
        assert_eq!(agg.rel_ppm.len(), 1);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = args.first().cloned().expect("用法: drift_stats <data_root> [--since YYYYMMDD]");
    // --since 写错了必须**当场报错**,不能静默退回"全都算"(Codex P2):这道门就是防污染的,
    // 失效方式一旦是 fail-open,拿到的就是一份看着正常、实则混了污染数据的基线。
    let since = match args.iter().position(|a| a == "--since") {
        None => None,
        Some(i) => {
            let v = args.get(i + 1).cloned().unwrap_or_else(|| {
                eprintln!("--since 后面缺日期(要 YYYYMMDD 或 YYYYMMDD-HHMMSS)");
                std::process::exit(2);
            });
            if !valid_since(&v) {
                eprintln!("--since 值不合法: {v}(要 YYYYMMDD 或 YYYYMMDD-HHMMSS)");
                std::process::exit(2);
            }
            Some(v)
        }
    };
    // 未知参数一律拒绝(Codex 九轮 P2):`--sine` 这种手滑会让上面的查找返回 None、
    // 悄悄退回"全都算",污染门形同虚设——防污染的开关必须 fail-closed 到底。
    let mut i = 1;
    let mut since_seen = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--since" => {
                since_seen += 1;
                i += 2;
            }
            other => {
                eprintln!("未知参数: {other}(用法: drift_stats <data_root> [--since YYYYMMDD[-HHMMSS]])");
                std::process::exit(2);
            }
        }
    }
    // 重复给也要拒(Codex 十轮 P2):上面的查找只认第一个,写两次的人多半以为后一个生效,
    // 结果按更宽松的那个跑,污染门又白设了。
    if since_seen > 1 {
        eprintln!("--since 给了 {since_seen} 次,只能给一次");
        std::process::exit(2);
    }
    if let Some(s) = &since {
        println!("(只统计 {s} 及之后建档的笔记——E2 基线不得混入 PR#103 之前的污染数据)");
    }
    let mut skipped = 0usize;
    let mut agg = Agg::default();
    let mut stack = vec![std::path::PathBuf::from(&root)];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() { stack.push(p); continue; }
            if p.file_name().map_or(false, |n| n == "drift_report.json") {
                if !passes_since(&p, since.as_deref()) {
                    skipped += 1;
                    continue;
                }
                if let Ok(v) = std::fs::read_to_string(&p)
                    .map_err(anyhow::Error::from)
                    .and_then(|s| Ok(serde_json::from_str::<serde_json::Value>(&s)?)) {
                    ingest(&mut agg, &v, &p.display().to_string());
                }
            }
        }
    }
    agg.rel_ppm.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if skipped > 0 {
        println!("按 --since 挡掉的报告: {skipped} 份");
    }
    println!("场次: {}  含降级源: {}", agg.sessions, agg.degraded);
    println!("含 inter_track 的场次: {}/{}", agg.with_inter_track, agg.sessions);
    println!("|轨间漂移| ppm  P50={:?} P95={:?} max={:?}",
        percentile(&agg.rel_ppm, 0.5), percentile(&agg.rel_ppm, 0.95), percentile(&agg.rel_ppm, 1.0));
    println!("重锚总数: {}", agg.reanchors);
    // 「最差设备组合」:按每场平均重锚数降序,一眼看出哪套设备最不稳。
    let mut devs: Vec<_> = agg.by_device.iter().collect();
    devs.sort_by(|a, b| {
        let per = |d: &DeviceAgg| d.reanchors as f64 / d.sessions.max(1) as f64;
        per(b.1).partial_cmp(&per(a.1)).unwrap_or(std::cmp::Ordering::Equal)
    });
    println!("按设备(重锚/场 降序):");
    for (name, d) in devs {
        println!(
            "  {name}: {} 场,重锚 {}(均 {:.1}/场),降级 {} 场",
            d.sessions,
            d.reanchors,
            d.reanchors as f64 / d.sessions.max(1) as f64,
            d.degraded
        );
    }
    for a in agg.anomalies.iter().take(20) { println!("异常 {a}"); }
}
