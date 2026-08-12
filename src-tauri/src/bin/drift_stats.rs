//! 漂移报告全库汇总:真实分布的出口(设计文档第四节)。
//! 用法: drift_stats <data_root>   递归扫描 drift_report.json

#[derive(Default)]
struct Agg {
    sessions: usize,
    degraded: usize,
    rel_ppm: Vec<f64>,
    reanchors: u64,
    anomalies: Vec<String>,
}

fn ingest(agg: &mut Agg, v: &serde_json::Value, origin: &str) {
    agg.sessions += 1;
    if v["sources"].as_object().map_or(false, |m| m.values().any(|s| s["quality"] == "degraded")) {
        agg.degraded += 1;
    }
    if let Some(p) = v["inter_track"]["rel_ppm"].as_f64() {
        agg.rel_ppm.push(p.abs());
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
    println!("|轨间漂移| ppm  P50={:?} P95={:?} max={:?}",
        percentile(&agg.rel_ppm, 0.5), percentile(&agg.rel_ppm, 0.95), percentile(&agg.rel_ppm, 1.0));
    println!("重锚总数: {}", agg.reanchors);
    for a in agg.anomalies.iter().take(20) { println!("异常 {a}"); }
}
