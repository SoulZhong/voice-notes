//! 说话人识别评测(spec rev2「测试与评测」)。P1 评的是纯声学基线
//! (S/R 簇的库关联 vs 人工真值);P2a 起 identify 的分档结果落 identify.json
//! 后,本工具按同一真值格式扩展分档查准/查全——当前版本先给出整体
//! 正确/误认/未识别 与 precision/recall。
//! 只读 JSON、不依赖库 crate:评测工具必须在应用任何状态下可用。
//!
//! 用法:
//!   标注模板:speaker_eval init <notes_dir> <note_id>
//!   评测:    speaker_eval run <notes_dir> <voiceprints.json> <truth.jsonl>
//!
//! 真值 JSONL 每行 {"note_id":"...","speaker_id":"S3"|"R2","person":"P12"|"张伟"|""}:
//! - speaker_id 以 R 开头比对修订稿段落关联,否则比对 speakers.json;
//! - person 形如 P<数字> 按 person_id(经 redirects 归一)比对——改名/同名不受
//!   影响,优先使用;否则按库中人名比对;空串=「标注过、确认无法归属」,此时
//!   预测出任何非空人物计误认(最危险的强行认人错误,不许躲进未识别)。

use std::collections::BTreeMap;

use serde_json::Value;

/// redirects 归一,与 store::VoiceprintStore::resolve 同语义(至多 8 跳,防环)。
fn resolve<'a>(
    redirects: &'a BTreeMap<String, String>,
    people: &BTreeMap<String, Value>,
    id: &'a str,
) -> Option<&'a str> {
    let mut cur = id;
    for _ in 0..8 {
        if people.contains_key(cur) {
            return Some(cur);
        }
        match redirects.get(cur) {
            Some(next) => cur = next,
            None => return None,
        }
    }
    None
}

#[derive(Debug, Default, PartialEq)]
struct Metrics {
    labeled: usize,
    correct: usize,
    wrong: usize,
    unassigned: usize,
}

impl Metrics {
    /// precision = 正确 / 所有做出的非空预测;recall = 正确 / 全部标注。
    fn precision(&self) -> f64 {
        let made = self.correct + self.wrong;
        if made == 0 {
            0.0
        } else {
            self.correct as f64 / made as f64
        }
    }
    fn recall(&self) -> f64 {
        if self.labeled == 0 {
            0.0
        } else {
            self.correct as f64 / self.labeled as f64
        }
    }
}

fn is_pid(s: &str) -> bool {
    s.strip_prefix('P')
        .is_some_and(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit()))
}

/// truth: (note_id, speaker_id, want);predicted: (note,spk) -> (person_id, name)。
fn score(
    truth: &[(String, String, String)],
    predicted: &BTreeMap<(String, String), (String, String)>,
) -> Metrics {
    let mut m = Metrics::default();
    for (note, spk, want) in truth {
        m.labeled += 1;
        let got = predicted.get(&(note.clone(), spk.clone()));
        let (got_id, got_name) = match got {
            Some((i, n)) if !i.is_empty() || !n.is_empty() => (i.as_str(), n.as_str()),
            _ => {
                if want.is_empty() {
                    m.correct += 1; // 真值「无法归属」且模型也没认:正确的保守
                } else {
                    m.unassigned += 1;
                }
                continue;
            }
        };
        if want.is_empty() {
            m.wrong += 1; // 真值明确无法归属,模型强行认人:最危险的错误
        } else if is_pid(want) {
            if got_id == want {
                m.correct += 1
            } else {
                m.wrong += 1
            }
        } else if got_name == want {
            m.correct += 1
        } else {
            m.wrong += 1
        }
    }
    m
}

fn read_json(path: &std::path::Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// 簇指纹复刻:sha256(升序 seq 的 LE 字节) 前 8 字节 hex。与库内
/// feedback::seq_fingerprint / refine::identify::cluster_fingerprint 同口径;
/// 本 bin 刻意不依赖库 crate,算法漂移由 fingerprint_locks_algorithm 测试钉死。
fn fingerprint(seqs: &std::collections::BTreeSet<u64>) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for s in seqs {
        h.update(s.to_le_bytes());
    }
    hex::encode(&h.finalize()[..8])
}

/// 修订稿(aing.json/refined.json)→ 每个 R 簇的成员 seq 集。
fn doc_members(doc: &Value) -> BTreeMap<String, std::collections::BTreeSet<u64>> {
    let mut out: BTreeMap<String, std::collections::BTreeSet<u64>> = BTreeMap::new();
    for p in doc["paragraphs"].as_array().cloned().unwrap_or_default() {
        let Some(spk) = p["speaker"].as_str() else { continue };
        let seqs = p["source_seqs"].as_array().cloned().unwrap_or_default();
        let entry = out.entry(spk.to_string()).or_default();
        for q in seqs {
            if let Some(v) = q.as_u64() {
                entry.insert(v);
            }
        }
    }
    out
}

/// identify.json → (note,R簇) 的推断预测:(tier, person_id, person_name/new_name)。
/// 按指纹匹配现稿簇(R 号会随重聚类变化,指纹不会);status 无关——评测用生成时
/// 原始裁决,人工决策不污染模型准确率。new_name 条目 person_id 为空、按名字比对。
fn identify_predictions(
    note_id: &str,
    idoc: &Value,
    members: &BTreeMap<String, std::collections::BTreeSet<u64>>,
    people: &BTreeMap<String, Value>,
    redirects: &BTreeMap<String, String>,
) -> BTreeMap<(String, String), (String, String, String)> {
    let fp_to_speaker: BTreeMap<String, String> =
        members.iter().map(|(sp, seqs)| (fingerprint(seqs), sp.clone())).collect();
    let mut out = BTreeMap::new();
    for a in idoc["assignments"].as_array().cloned().unwrap_or_default() {
        let Some(fp) = a["fingerprint"].as_str() else { continue };
        let Some(spk) = fp_to_speaker.get(fp) else { continue };
        let tier = a["tier"].as_str().unwrap_or("low").to_string();
        let (pid, name) = match a["person_id"].as_str() {
            Some(p) => match resolve(redirects, people, p) {
                Some(rid) => (rid.to_string(), people[rid]["name"].as_str().unwrap_or("").to_string()),
                None => continue,
            },
            None => (String::new(), a["new_name"].as_str().unwrap_or("").to_string()),
        };
        out.insert((note_id.to_string(), spk.clone()), (tier, pid, name));
    }
    out
}

/// 标注模板:S 层逐说话人一行(含 segments 里出现但缺 metadata 的孤儿 id),
/// R 层逐修订稿说话人一行;# 注释行给名字线索与首段文本。
fn init(notes_dir: &str, note_id: &str) -> i32 {
    let dir = std::path::Path::new(notes_dir).join(note_id);
    let speakers: BTreeMap<String, Value> = read_json(&dir.join("speakers.json"))
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    let segments = std::fs::read_to_string(dir.join("segments.jsonl")).unwrap_or_default();
    let seg_rows: Vec<Value> = segments.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    let mut s_ids: Vec<String> = speakers.keys().cloned().collect();
    for v in &seg_rows {
        if let Some(sp) = v["speaker"].as_str() {
            if !s_ids.iter().any(|x| x == sp) {
                s_ids.push(sp.to_string());
            }
        }
    }
    for sid in &s_ids {
        let name = speakers.get(sid).and_then(|m| m["name"].as_str()).unwrap_or("");
        let sample = seg_rows
            .iter()
            .find(|v| v["speaker"].as_str() == Some(sid))
            .and_then(|v| v["text"].as_str())
            .map(|s| s.chars().take(40).collect::<String>())
            .unwrap_or_default();
        println!("# {sid} name={name} 首段:{sample}");
        println!(r#"{{"note_id":"{note_id}","speaker_id":"{sid}","person":""}}"#);
    }
    // R 层(有修订稿才有):aing.json 优先,legacy refined.json 兜底。
    let doc = read_json(&dir.join("aing.json")).or_else(|| read_json(&dir.join("refined.json")));
    if let Some(doc) = doc {
        let mut seen = std::collections::BTreeSet::new();
        for p in doc["paragraphs"].as_array().unwrap_or(&vec![]) {
            let Some(r) = p["speaker"].as_str() else { continue };
            if !seen.insert(r.to_string()) {
                continue;
            }
            let name = p["name"].as_str().unwrap_or("");
            let sample = p["text"]
                .as_str()
                .map(|s| s.chars().take(40).collect::<String>())
                .unwrap_or_default();
            println!("# {r} name={name} 首段:{sample}");
            println!(r#"{{"note_id":"{note_id}","speaker_id":"{r}","person":""}}"#);
        }
    }
    0
}

fn run(notes_dir: &str, vp_path: &str, truth_path: &str) -> i32 {
    let Some(vp) = read_json(std::path::Path::new(vp_path)) else {
        eprintln!("voiceprints.json 读取失败");
        return 2;
    };
    let people: BTreeMap<String, Value> = serde_json::from_value(vp["people"].clone()).unwrap_or_default();
    let redirects: BTreeMap<String, String> =
        serde_json::from_value(vp["redirects"].clone()).unwrap_or_default();

    // 解析真值:缺字段/重复键直接失败——损坏标注进分母比报错更糟。
    let mut truth: Vec<(String, String, String)> = Vec::new();
    let mut keys = std::collections::BTreeSet::new();
    let content = match std::fs::read_to_string(truth_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("truth 读取失败: {e}");
            return 2;
        }
    };
    for (ln, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            eprintln!("truth 第 {} 行非法 JSON", ln + 1);
            return 2;
        };
        let (Some(note), Some(spk), Some(person)) =
            (v["note_id"].as_str(), v["speaker_id"].as_str(), v["person"].as_str())
        else {
            eprintln!("truth 第 {} 行缺字段(note_id/speaker_id/person)", ln + 1);
            return 2;
        };
        if !keys.insert((note.to_string(), spk.to_string())) {
            eprintln!("truth 第 {} 行重复标注 ({note},{spk})", ln + 1);
            return 2;
        }
        truth.push((note.into(), spk.into(), person.into()));
    }

    // 按 note 分组各读一次;S 层读 speakers.json,R 层读 aing.json/refined.json。
    let mut predicted: BTreeMap<(String, String), (String, String)> = BTreeMap::new();
    // identify 分档预测:(note,R簇) -> (tier, person_id, name)。
    let mut identify_pred: BTreeMap<(String, String), (String, String, String)> = BTreeMap::new();
    let mut by_note: BTreeMap<&str, Vec<&(String, String, String)>> = BTreeMap::new();
    for t in &truth {
        by_note.entry(t.0.as_str()).or_default().push(t);
    }
    for (note_id, rows) in by_note {
        let dir = std::path::Path::new(notes_dir).join(note_id);
        let speakers: BTreeMap<String, Value> = read_json(&dir.join("speakers.json"))
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        let doc = read_json(&dir.join("aing.json")).or_else(|| read_json(&dir.join("refined.json")));
        if let (Some(d), Some(idoc)) = (doc.as_ref(), read_json(&dir.join("identify.json"))) {
            let members = doc_members(d);
            identify_pred.extend(identify_predictions(note_id, &idoc, &members, &people, &redirects));
        }
        for (_, spk, _) in rows {
            let pid = if spk.starts_with('R') {
                doc.as_ref().and_then(|d| {
                    d["paragraphs"]
                        .as_array()?
                        .iter()
                        .find(|p| p["speaker"].as_str() == Some(spk))
                        .and_then(|p| p["person_id"].as_str().map(str::to_string))
                })
            } else {
                speakers.get(spk).and_then(|m| m["person_id"].as_str().map(str::to_string))
            };
            let Some(pid) = pid else { continue };
            let Some(rid) = resolve(&redirects, &people, &pid) else { continue };
            let name = people[rid]["name"].as_str().unwrap_or("").to_string();
            predicted.insert((note_id.to_string(), spk.clone()), (rid.to_string(), name));
        }
    }

    let m = score(&truth, &predicted);
    println!("标注 {}  正确 {}  误认 {}  未识别 {}", m.labeled, m.correct, m.wrong, m.unassigned);
    println!(
        "precision {:.1}%  recall {:.1}%(P2b 验收线看 precision;误认是最危险错误)",
        100.0 * m.precision(),
        100.0 * m.recall()
    );

    // identify 分档评测:R 层真值 × identify 推断(按指纹匹配,status 无关)。
    if !identify_pred.is_empty() {
        let r_truth: Vec<(String, String, String)> =
            truth.iter().filter(|(_, spk, _)| spk.starts_with('R')).cloned().collect();
        let new_name_n = identify_pred.values().filter(|(_, pid, _)| pid.is_empty()).count();
        for (label, tiers) in [
            ("high", &["high"][..]),
            ("high+medium", &["high", "medium"][..]),
            ("all", &["high", "medium", "low"][..]),
        ] {
            let bucket: BTreeMap<(String, String), (String, String)> = identify_pred
                .iter()
                .filter(|(_, (tier, _, _))| tiers.contains(&tier.as_str()))
                .map(|(k, (_, pid, name))| (k.clone(), (pid.clone(), name.clone())))
                .collect();
            let bm = score(&r_truth, &bucket);
            println!(
                "[identify] {label}: 标注 {} 正确 {} 误认 {} 未识别 {}  precision {:.1}% recall {:.1}%",
                bm.labeled, bm.correct, bm.wrong, bm.unassigned,
                100.0 * bm.precision(), 100.0 * bm.recall()
            );
        }
        if new_name_n > 0 {
            println!("[identify] 含 {new_name_n} 条新面孔预测(按名字比对,同名风险自担)");
        }
    }
    0
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let code = match args.get(1).map(String::as_str) {
        Some("init") if args.len() == 4 => init(&args[2], &args[3]),
        Some("run") if args.len() == 5 => run(&args[2], &args[3], &args[4]),
        _ => {
            eprintln!(
                "用法:\n  speaker_eval init <notes_dir> <note_id>\n  speaker_eval run <notes_dir> <voiceprints.json> <truth.jsonl>"
            );
            2
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_follows_redirects_with_hop_cap() {
        let mut redirects = BTreeMap::new();
        redirects.insert("P1".to_string(), "P2".to_string());
        redirects.insert("P2".to_string(), "P3".to_string());
        let mut people = BTreeMap::new();
        people.insert("P3".to_string(), Value::Null);
        assert_eq!(resolve(&redirects, &people, "P1"), Some("P3"));
        assert_eq!(resolve(&redirects, &people, "P9"), None);
        // 自环不挂死。
        let mut looped = BTreeMap::new();
        looped.insert("PX".to_string(), "PX".to_string());
        assert_eq!(resolve(&looped, &BTreeMap::new(), "PX"), None);
    }

    #[test]
    fn score_covers_pid_name_empty_truth_cases() {
        let truth = vec![
            ("n".into(), "S1".into(), "P3".into()),   // pid 命中
            ("n".into(), "S2".into(), "张伟".into()), // 名字误认
            ("n".into(), "S3".into(), "".into()),     // 真值无归属,模型强行认 → wrong
            ("n".into(), "S4".into(), "李雷".into()), // 无预测 → unassigned
            ("n".into(), "S5".into(), "".into()),     // 真值无归属,模型也没认 → correct
        ];
        let mut p = BTreeMap::new();
        p.insert(("n".to_string(), "S1".to_string()), ("P3".to_string(), "王五".to_string()));
        p.insert(("n".to_string(), "S2".to_string()), ("P9".to_string(), "赵六".to_string()));
        p.insert(("n".to_string(), "S3".to_string()), ("P1".to_string(), "钱七".to_string()));
        let m = score(&truth, &p);
        assert_eq!(
            m,
            Metrics { labeled: 5, correct: 2, wrong: 2, unassigned: 1 }
        );
        assert!((m.precision() - 0.5).abs() < 1e-9);
        assert!((m.recall() - 0.4).abs() < 1e-9);
    }

    #[test]
    fn is_pid_rejects_names_and_partials() {
        assert!(is_pid("P12"));
        assert!(!is_pid("P"));
        assert!(!is_pid("P12a"));
        assert!(!is_pid("张伟"));
    }

    #[test]
    fn run_end_to_end_on_fixture() {
        let root = tempfile::tempdir().unwrap();
        let note = root.path().join("n1");
        std::fs::create_dir_all(&note).unwrap();
        std::fs::write(
            note.join("speakers.json"),
            r#"{"S1":{"name":"","sources":["mic"],"centroid":[],"count":1,"person_id":"P1"}}"#,
        )
        .unwrap();
        std::fs::write(
            note.join("segments.jsonl"),
            r#"{"seq":0,"source":"mic","text":"你好","start_ms":0,"end_ms":2000,"speaker":"S1"}"#,
        )
        .unwrap();
        let vp = root.path().join("voiceprints.json");
        std::fs::write(
            &vp,
            r#"{"schema_version":1,"next_person":2,"people":{"P1":{"name":"张伟","centroids":{},"session_centroids":{},"total_ms":0,"last_seen":""}},"redirects":{},"embedding_model":"campplus"}"#,
        )
        .unwrap();
        let truth = root.path().join("truth.jsonl");
        std::fs::write(&truth, format!("{}\n", r#"{"note_id":"n1","speaker_id":"S1","person":"P1"}"#)).unwrap();
        assert_eq!(
            run(root.path().to_str().unwrap(), vp.to_str().unwrap(), truth.to_str().unwrap()),
            0
        );
        // 重复标注必须整体报错。
        std::fs::write(
            &truth,
            format!("{0}\n{0}\n", r#"{"note_id":"n1","speaker_id":"S1","person":"P1"}"#),
        )
        .unwrap();
        assert_eq!(
            run(root.path().to_str().unwrap(), vp.to_str().unwrap(), truth.to_str().unwrap()),
            2
        );
    }
    #[test]
    fn fingerprint_locks_algorithm() {
        // 与库内 feedback::seq_fingerprint 同口径:sha256(LE u64 序列) 前 8 字节 hex。
        // 固定向量钉死算法,任何一侧改动都会在此炸出。
        let seqs: std::collections::BTreeSet<u64> = [0u64, 1].into_iter().collect();
        assert_eq!(fingerprint(&seqs), "9d34149fbd1fe777");
    }

    #[test]
    fn identify_predictions_match_by_fingerprint_not_r_number() {
        // 现稿 R9 的成员集 {0,1} 与 identify 里存的指纹一致:即便当年叫 R2,
        // 仍应命中;不匹配的指纹被跳过。
        let doc = serde_json::json!({
            "paragraphs": [
                { "speaker": "R9", "source_seqs": [0, 1], "text": "a" }
            ]
        });
        let members = doc_members(&doc);
        let seqs: std::collections::BTreeSet<u64> = [0u64, 1].into_iter().collect();
        let idoc = serde_json::json!({
            "assignments": [
                { "fingerprint": fingerprint(&seqs), "cluster": "R2", "person_id": "P1",
                  "tier": "high", "status": "suggested" },
                { "fingerprint": "dead00000000beef", "cluster": "R3", "person_id": "P1",
                  "tier": "high", "status": "suggested" }
            ]
        });
        let mut people = BTreeMap::new();
        people.insert("P1".to_string(), serde_json::json!({ "name": "张伟" }));
        let preds = identify_predictions("n1", &idoc, &members, &people, &BTreeMap::new());
        assert_eq!(preds.len(), 1);
        let (tier, pid, name) = &preds[&("n1".to_string(), "R9".to_string())];
        assert_eq!((tier.as_str(), pid.as_str(), name.as_str()), ("high", "P1", "张伟"));
    }
}
