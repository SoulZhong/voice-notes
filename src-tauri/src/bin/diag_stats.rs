//! 诊断档案横扫(2026-08-23 数据积累):汇总全部笔记的 diagnostics.json 出总表,
//! 供「先分析数据再设计方案」。用法:
//!   cargo run --features devtools --bin diag_stats -- <notes_dir> [--since 2026-08-01]
//! 无 diagnostics.json 的笔记跳过并计数(存量场次可用应用停录后自动生成的新档案,
//! 或忽略——本工具不回填)。

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let notes_dir = std::path::PathBuf::from(
        args.next().ok_or_else(|| anyhow::anyhow!("用法: diag_stats <notes_dir> [--since YYYY-MM-DD]"))?,
    );
    let mut since = String::new();
    let mut backfill = false;
    while let Some(a) = args.next() {
        if a == "--since" {
            since = args.next().unwrap_or_default();
        } else if a == "--backfill" {
            backfill = true;
        }
    }
    let mut rows: Vec<(String, app_lib::store::diagnostics::DiagnosticsDoc)> = Vec::new();
    let mut missing = 0usize;
    let mut ids: Vec<String> = std::fs::read_dir(&notes_dir)?
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    ids.sort();
    for id in ids {
        if !since.is_empty() && id.as_str() < since.replace('-', "").as_str() {
            continue;
        }
        let dir = notes_dir.join(&id);
        match app_lib::store::diagnostics::load(&dir) {
            Some(d) => rows.push((id, d)),
            None if backfill => {
                // 存量补档:采集快照无从考证,只填盘上可得的(capture 字段留空如实)。
                let now = chrono::Local::now().to_rfc3339();
                match app_lib::store::diagnostics::compute_and_save(
                    &dir,
                    app_lib::store::diagnostics::CaptureMeta::default(),
                    &now,
                ) {
                    Ok(d) => rows.push((id, d)),
                    Err(e) => {
                        eprintln!("{id}: 补档失败 {e}");
                        missing += 1;
                    }
                }
            }
            None => missing += 1,
        }
    }
    println!(
        "{:<16} {:>5} {:>5} {:>6} {:>6} {:>5} {:>12} {:<12} {}",
        "note", "mic", "sys", "ov%", "ov#", "占位", "抑制(总)", "场景", "采集"
    );
    for (id, d) in &rows {
        let sup: u64 = d.suppressions.values().sum();
        println!(
            "{:<16} {:>5} {:>5} {:>5.0}% {:>6} {:>5} {:>12} {:<12} {}{}",
            id,
            d.mic.count,
            d.system.count,
            d.mic_overlap_ratio * 100.0,
            d.mic_overlapped_count,
            d.placeholder_count,
            sup,
            if d.scene_final.is_empty() { "-" } else { &d.scene_final },
            d.capture.capture_path,
            if d.capture.input_override.is_empty() { String::new() } else { format!("(改用 {})", d.capture.input_override) },
        );
    }
    println!("\n共 {} 场有档案;{} 场无(旧存量)。", rows.len(), missing);
    if !rows.is_empty() {
        let dual = rows.iter().filter(|(_, d)| d.scene_final == "dual_path").count();
        let avg_ov: f32 =
            rows.iter().map(|(_, d)| d.mic_overlap_ratio).sum::<f32>() / rows.len() as f32;
        println!("同源双路场次: {dual};平均 mic 重叠率: {:.0}%", avg_ov * 100.0);
        let mut sup_total: std::collections::BTreeMap<String, u64> = Default::default();
        for (_, d) in &rows {
            for (k, v) in &d.suppressions {
                *sup_total.entry(k.clone()).or_default() += v;
            }
        }
        println!("抑制分布: {sup_total:?}");
    }
    Ok(())
}
