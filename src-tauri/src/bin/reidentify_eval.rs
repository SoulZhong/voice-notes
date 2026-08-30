//! 按**当前声纹库**重认最近几场会议的说话人簇(只读,不改任何笔记):
//! 把每场 speakers.json 里每个簇的质心当探针,走生产认领路径(种子注入 +
//! registry.assign,与开录/Aing 同一套门槛与差距门),输出「现关联 vs 现库会认成谁」
//! 及前两名相似度。用途:审完样本后看库有没有变准——现关联里用户已核对的簇是真值,
//! 未核对的簇看结果自己判断。
//!
//! 用法: reidentify_eval <data_root> <speaker_model.onnx> [--last N] [--multi]
//!
//! `--last N` 最近 N 场(默认 8);`--multi` 按多质心种子(与设置 multi_centroid 同口径)。
//! 只用 speakers.json 里的簇质心(实时阶段的均值),簇 count=0 的(拆分/折叠残留)跳过。
use app_lib::store::{seed_clusters, seed_clusters_multi, NoteStore, VoiceprintStore};
use std::collections::BTreeMap;

/// 簇质心视作长段:段长闸恒过(与 speaker_loso_eval 同一取舍)。
const PROBE_SAMPLES: usize = 48_000;

fn normalize(v: &[f32]) -> Option<Vec<f32>> {
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    (n.is_finite() && n >= 1e-6).then(|| v.iter().map(|x| x / n).collect())
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(root), Some(_model)) = (args.next(), args.next()) else {
        eprintln!("用法: reidentify_eval <data_root> <speaker_model.onnx> [--last N] [--multi]");
        std::process::exit(2);
    };
    let flags: Vec<String> = args.collect();
    let mut last = 8usize;
    if let Some(i) = flags.iter().position(|a| a == "--last") {
        last = flags.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(8);
    }
    let multi = flags.iter().any(|a| a == "--multi");
    let root = std::path::PathBuf::from(root);

    let vp_store = VoiceprintStore::new(root.clone());
    let vp = vp_store.load();
    let seeds = if multi { seed_clusters_multi(&vp) } else { seed_clusters(&vp) };
    let label = |pid: &str| -> String {
        let r = VoiceprintStore::resolve(&vp, pid).unwrap_or(pid);
        match vp.people.get(r).map(|p| p.name.as_str()) {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => r.to_string(),
        }
    };
    // 每人主质心(mic 优先)用于报前两名裸分。
    let mut person_vecs: Vec<(String, Vec<f32>)> = Vec::new();
    for (pid, p) in &vp.people {
        if VoiceprintStore::resolve(&vp, pid) != Some(pid.as_str()) || p.voiceprint_quarantined {
            continue;
        }
        let c = p.centroids.get("mic").or_else(|| p.centroids.values().next());
        if let Some(c) = c.and_then(|c| normalize(&c.vec)) {
            person_vecs.push((pid.clone(), c));
        }
    }

    let notes_dir = root.join("notes");
    let ns = NoteStore::new(notes_dir);
    let mut list = ns.list();
    list.truncate(last); // list 已按开始时间倒序
    println!(
        "种子 {} 席({}),画廊 {} 人;最近 {} 场",
        seeds.len(),
        if multi { "主质心+子质心" } else { "仅主质心" },
        person_vecs.len(),
        list.len()
    );

    let (mut agree, mut disagree, mut abstain_linked, mut claim_unlinked, mut skipped) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut agree_segs = 0u64;
    let mut total_segs = 0u64;
    for n in &list {
        let Ok(note) = ns.load(&n.id) else { continue };
        println!("\n## {} {}", n.id, n.title);
        let mut rows: Vec<(&String, &app_lib::store::SpeakerMeta)> = note.speakers.iter().collect();
        rows.sort_by(|a, b| b.1.count.cmp(&a.1.count));
        for (sid, m) in rows {
            if m.count == 0 {
                continue;
            }
            let Some(c) = m.centroid.as_ref().and_then(|c| normalize(c)) else {
                skipped += 1;
                continue;
            };
            let mut r = app_lib::diar::registry::SpeakerRegistry::with_seeds(&[], &seeds);
            let got = r.assign(&c, "mic", PROBE_SAMPLES).and_then(|cid| {
                r.speakers().into_iter().find(|s| s.id == cid).and_then(|s| s.person)
            });
            let cur = m.person_id.as_deref().and_then(|p| VoiceprintStore::resolve(&vp, p)).map(str::to_string);
            // 前两名裸分(按人取主质心)
            let mut sims: Vec<(f32, &String)> = person_vecs
                .iter()
                .map(|(pid, v)| (c.iter().zip(v).map(|(a, b)| a * b).sum::<f32>(), pid))
                .collect();
            sims.sort_by(|a, b| b.0.total_cmp(&a.0));
            let top: Vec<String> = sims.iter().take(2).map(|(s, p)| format!("{} {:.2}", label(p), s)).collect();
            let verdict = match (&cur, &got) {
                (Some(a), Some(b)) if a == b => {
                    agree += 1;
                    agree_segs += m.count;
                    "一致"
                }
                (Some(_), Some(_)) => {
                    disagree += 1;
                    "**不一致**"
                }
                (Some(_), None) => {
                    abstain_linked += 1;
                    "弃权"
                }
                (None, Some(_)) => {
                    claim_unlinked += 1;
                    "新认领"
                }
                (None, None) => "无",
            };
            if cur.is_some() {
                total_segs += m.count;
            }
            println!(
                "  {:<5} {:>4}段  现:{:<10} 库认:{:<10} {:<8} [{}]",
                sid,
                m.count,
                cur.as_deref().map(label).unwrap_or_else(|| "-".into()),
                got.as_deref().map(label).unwrap_or_else(|| "-".into()),
                verdict,
                top.join(" / ")
            );
        }
    }
    let _ = BTreeMap::<String, ()>::new();
    println!(
        "\n汇总(只统计现有关联的簇):一致 {agree}  不一致 {disagree}  弃权 {abstain_linked} | 未关联簇被认领 {claim_unlinked} | 无质心跳过 {skipped}"
    );
    let linked = agree + disagree + abstain_linked;
    if linked > 0 {
        println!(
            "簇级一致率 {:.1}%({}/{});按段加权 {:.1}%({}/{})",
            agree as f64 * 100.0 / linked as f64,
            agree,
            linked,
            if total_segs > 0 { agree_segs as f64 * 100.0 / total_segs as f64 } else { 0.0 },
            agree_segs,
            total_segs
        );
    }
}
