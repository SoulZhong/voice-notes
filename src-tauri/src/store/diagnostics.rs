//! 每场会议的诊断档案(2026-08-23,用户指令「积累数据,后续开发先分析再设计」)。
//!
//! 停录后从盘上产物汇总一份 `diagnostics.json`:段落双路统计/重叠率/抑制分布/
//! 场景判定/采集配置/AEC 末读数。纯观测,失败只打日志绝不影响停录;文件可整删。
//! 汇总分析工具见 devtools bin `diag_stats`(横扫全部笔记出总表)。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub const DIAGNOSTICS_FILE: &str = "diagnostics.json";

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct CaptureMeta {
    /// 采集路径快照("aec"|"vpio")。
    pub capture_path: String,
    /// 录前自动择优改用的输入设备(空=没换)。
    pub input_override: String,
    /// 本场声纹模型标签(开录快照)。
    pub speaker_model: String,
    /// 停录时 AEC3 最后一读 erle(dB;None=无读数/无 AEC)。
    pub erle_last_db: Option<f32>,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct SourceStats {
    pub count: u64,
    pub total_ms: u64,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct DiagnosticsDoc {
    pub schema_version: u32,
    pub generated_at: String,
    pub capture: CaptureMeta,
    pub mic: SourceStats,
    pub system: SourceStats,
    /// mic 段时长中被 system 活动覆盖的比例(0~1;同源双路/外放的核心指纹)。
    pub mic_overlap_ratio: f32,
    /// 被 system 覆盖 ≥80% 的 mic 段数(断喂/清洗口径)。
    pub mic_overlapped_count: u64,
    /// "[识别失败]" 占位段数(转写失败面)。
    pub placeholder_count: u64,
    /// 抑制记录按 reason 计数(segment-suppressions.jsonl)。
    pub suppressions: BTreeMap<String, u64>,
    /// 场景判定(scene.json;无则空串)。
    pub scene_final: String,
    pub scene_windows: u64,
}

pub fn load(note_dir: &Path) -> Option<DiagnosticsDoc> {
    serde_json::from_slice(&std::fs::read(note_dir.join(DIAGNOSTICS_FILE)).ok()?).ok()
}

/// 从盘上产物汇总并落盘。segments.jsonl 缺失按空场处理(照写,便于统计空场率)。
pub fn compute_and_save(
    note_dir: &Path,
    capture: CaptureMeta,
    now: &str,
) -> anyhow::Result<DiagnosticsDoc> {
    let mut doc = DiagnosticsDoc {
        schema_version: 1,
        generated_at: now.to_string(),
        capture,
        ..Default::default()
    };
    let mut sys_windows: Vec<(u64, u64)> = Vec::new();
    let mut mics: Vec<(u64, u64, bool)> = Vec::new(); // (start, end, placeholder)
    if let Ok(raw) = std::fs::read_to_string(note_dir.join("segments.jsonl")) {
        for line in raw.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            let (Some(src), Some(a), Some(b)) =
                (v["source"].as_str(), v["start_ms"].as_u64(), v["end_ms"].as_u64())
            else {
                continue;
            };
            let dur = b.saturating_sub(a);
            match src {
                "system" => {
                    doc.system.count += 1;
                    doc.system.total_ms += dur;
                    sys_windows.push((a, b));
                }
                "mic" => {
                    doc.mic.count += 1;
                    doc.mic.total_ms += dur;
                    mics.push((a, b, v["text"].as_str() == Some("[识别失败]")));
                }
                _ => {}
            }
        }
    }
    let mut ov_total: u64 = 0;
    for (a, b, ph) in &mics {
        if *ph {
            doc.placeholder_count += 1;
        }
        let dur = b.saturating_sub(*a).max(1);
        let ov: u64 = sys_windows.iter().map(|(x, y)| (*b).min(*y).saturating_sub((*a).max(*x))).sum();
        let ov = ov.min(dur);
        ov_total += ov;
        if ov as f32 / dur as f32 >= 0.8 {
            doc.mic_overlapped_count += 1;
        }
    }
    doc.mic_overlap_ratio = if doc.mic.total_ms > 0 {
        (ov_total as f64 / doc.mic.total_ms as f64) as f32
    } else {
        0.0
    };
    if let Ok(raw) = std::fs::read_to_string(note_dir.join("segment-suppressions.jsonl")) {
        for line in raw.lines() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(r) = v["reason"].as_str() {
                    *doc.suppressions.entry(r.to_string()).or_default() += 1;
                }
            }
        }
    }
    if let Some(sc) = crate::scene::load(note_dir) {
        doc.scene_final = sc.final_scene;
        doc.scene_windows = sc.windows.len() as u64;
    }
    let path = note_dir.join(DIAGNOSTICS_FILE);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&doc)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_summarizes_overlap_suppressions_and_scene() {
        let dir = tempfile::tempdir().unwrap();
        let segs = [
            r#"{"seq":0,"source":"system","text":"甲","start_ms":0,"end_ms":10000}"#,
            r#"{"seq":1,"source":"mic","text":"乙","start_ms":1000,"end_ms":3000}"#,
            r#"{"seq":2,"source":"mic","text":"[识别失败]","start_ms":20000,"end_ms":22000}"#,
        ];
        std::fs::write(dir.path().join("segments.jsonl"), segs.join("\n")).unwrap();
        std::fs::write(
            dir.path().join("segment-suppressions.jsonl"),
            r#"{"reason":"aec_residue"}
{"reason":"aec_residue"}
{"reason":"echo_match"}"#,
        )
        .unwrap();
        crate::scene::save(
            dir.path(),
            &crate::scene::SceneDoc {
                schema_version: 1,
                windows: vec![crate::scene::SceneWindow {
                    start_ms: 0,
                    end_ms: 22000,
                    scene: "dual_path".into(),
                }],
                final_scene: "dual_path".into(),
            },
        )
        .unwrap();
        let doc = compute_and_save(dir.path(), CaptureMeta::default(), "t").unwrap();
        assert_eq!(doc.mic.count, 2);
        assert_eq!(doc.system.count, 1);
        assert_eq!(doc.mic_overlapped_count, 1, "seq1 全覆盖;占位段独立不算");
        assert_eq!(doc.placeholder_count, 1);
        assert_eq!(doc.suppressions["aec_residue"], 2);
        assert_eq!(doc.scene_final, "dual_path");
        assert!(load(dir.path()).is_some(), "已落盘可回读");
        // 重叠率:mic 总 4000ms,被覆盖 2000ms
        assert!((doc.mic_overlap_ratio - 0.5).abs() < 0.01);
    }
}
