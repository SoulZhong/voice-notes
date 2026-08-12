//! 漂移报告全库汇总:真实分布的出口(设计文档第四节)。
//! 用法: drift_stats <data_root>   递归扫描 drift_report.json

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
}

fn ingest(agg: &mut Agg, v: &serde_json::Value, origin: &str) {
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
    for s in v["sources"].as_object().into_iter().flat_map(|m| m.values()) {
        agg.reanchors += s["reanchors"].as_u64().unwrap_or(0);
    }
    for a in v["anomalies"].as_array().into_iter().flatten() {
        agg.anomalies.push(format!("{origin}: {a}"));
    }
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
    let root = std::env::args().nth(1).expect("用法: drift_stats <data_root>");
    let mut agg = Agg::default();
    let mut stack = vec![std::path::PathBuf::from(&root)];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() { stack.push(p); continue; }
            if p.file_name().map_or(false, |n| n == "drift_report.json") {
                if let Ok(v) = std::fs::read_to_string(&p)
                    .map_err(anyhow::Error::from)
                    .and_then(|s| Ok(serde_json::from_str::<serde_json::Value>(&s)?)) {
                    ingest(&mut agg, &v, &p.display().to_string());
                }
            }
        }
    }
    agg.rel_ppm.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!("场次: {}  含降级源: {}", agg.sessions, agg.degraded);
    println!("含 inter_track 的场次: {}/{}", agg.with_inter_track, agg.sessions);
    println!("|轨间漂移| ppm  P50={:?} P95={:?} max={:?}",
        percentile(&agg.rel_ppm, 0.5), percentile(&agg.rel_ppm, 0.95), percentile(&agg.rel_ppm, 1.0));
    println!("重锚总数: {}", agg.reanchors);
    for a in agg.anomalies.iter().take(20) { println!("异常 {a}"); }
}
