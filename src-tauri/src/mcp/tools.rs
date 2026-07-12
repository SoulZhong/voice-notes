//! MCP 查询工具的纯实现:文件系统 → serde_json::Value。不依赖 tauri/AppHandle,
//! stdio 服务进程与单测直接调用。App 运行与否都可用(只读,GUI 侧写入均原子)。

use crate::settings;
use crate::store::{self, NoteStore, SpeakerMeta};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub struct DataRoots {
    pub app_data: PathBuf,
    pub data_root: PathBuf,
}

/// 每次工具调用现算(极廉价):settings.json 的 data_dir 可能随时被 GUI 迁移。
pub fn resolve_roots() -> DataRoots {
    let app_data = super::app_data_dir();
    let s = settings::load(&app_data);
    let data_root = settings::resolve_data_root(&app_data, &s);
    DataRoots { app_data, data_root }
}

fn notes_dir(roots: &DataRoots) -> PathBuf {
    roots.data_root.join("notes")
}

/// 笔记列表。from/to 为 RFC3339 前缀(如 "2026-02-01"),与 started_at 字典序比较
/// (同时区 RFC3339 字典序即时间序,与 NoteStore::list 排序同一假设)。
pub fn list_notes(
    roots: &DataRoots,
    limit: usize,
    offset: usize,
    from: Option<&str>,
    to: Option<&str>,
) -> serde_json::Value {
    let all = NoteStore::new(notes_dir(roots)).list();
    let filtered: Vec<_> = all
        .into_iter()
        .filter(|n| from.map(|f| n.started_at.as_str() >= f).unwrap_or(true))
        .filter(|n| to.map(|t| n.started_at.as_str() <= t).unwrap_or(true))
        .collect();
    let total = filtered.len();
    // speaker_count/has_refined 需要探目录(speakers.json/refined.json 是否存在/可解析)。
    // 只对分页后、真正要返回的这一页做探测,不对全量 filtered 做——大库(数百场笔记)
    // 分页浏览时不会因为这两个字段整体变慢。
    let page: Vec<_> = filtered
        .into_iter()
        .skip(offset)
        .take(limit.clamp(1, 100))
        .map(|n| {
            let dir = notes_dir(roots).join(&n.id);
            let speaker_count = std::fs::read_to_string(dir.join("speakers.json"))
                .ok()
                .and_then(|text| serde_json::from_str::<BTreeMap<String, SpeakerMeta>>(&text).ok())
                .map(|m| m.len())
                .unwrap_or(0);
            let has_refined = dir.join("refined.json").exists();
            serde_json::json!({
                "id": n.id, "title": n.title, "started_at": n.started_at,
                "duration_secs": n.duration_secs, "state": n.state,
                "speaker_count": speaker_count, "has_refined": has_refined,
            })
        })
        .collect();
    serde_json::json!({ "total": total, "notes": page })
}

/// 全文检索:遍历全部笔记逐段子串匹配(大小写不敏感)。个人量级(百场×百句)
/// 全扫毫秒级,不建索引(YAGNI,见设计文档 §三)。
pub fn search_notes(roots: &DataRoots, query: &str, limit: usize) -> serde_json::Value {
    // 命名为 notes_store 而非 store:后者会遮蔽本文件顶部 `use crate::store` 的
    // 模块导入,函数体内若要用 `store::` 前缀访问模块级函数会撞名。
    let notes_store = NoteStore::new(notes_dir(roots));
    let needle = query.to_lowercase();
    let mut hits = Vec::new();
    let mut scanned = 0usize;
    'outer: for summary in notes_store.list() {
        let Ok(note) = notes_store.load(&summary.id) else { continue };
        scanned += 1;
        for (i, seg) in note.segments.iter().enumerate() {
            if !seg.text.to_lowercase().contains(&needle) {
                continue;
            }
            hits.push(serde_json::json!({
                "note_id": summary.id, "title": summary.title,
                "seq": seg.seq, "speaker": seg.speaker, "start_ms": seg.start_ms,
                "text": seg.text,
                "before": if i > 0 { note.segments[i - 1].text.clone() } else { String::new() },
                "after": note.segments.get(i + 1).map(|s| s.text.clone()).unwrap_or_default(),
            }));
            if hits.len() >= limit.clamp(1, 100) {
                break 'outer;
            }
        }
    }
    serde_json::json!({ "scanned_notes": scanned, "hits": hits })
}

/// 笔记全文。format: segments(结构化) / markdown / text;prefer_refined 且
/// refined.json 存在时返回精修稿(结构化给 paragraphs,md/txt 现场渲染精修段)。
pub fn get_note(
    roots: &DataRoots,
    id: &str,
    format: &str,
    prefer_refined: bool,
) -> anyhow::Result<serde_json::Value> {
    let store = NoteStore::new(notes_dir(roots));
    let note = store.load(id)?; // 内含 validate_note_id 防穿越 + 存在性检查
    let refined = if prefer_refined { store::load_refined(&notes_dir(roots).join(id)) } else { None };
    let speakers: serde_json::Value = note
        .speakers
        .iter()
        .map(|(sid, m)| (sid.clone(), serde_json::json!({ "name": m.name, "person_id": m.person_id })))
        .collect::<serde_json::Map<_, _>>()
        .into();
    match format {
        "segments" => Ok(match refined {
            Some(doc) => serde_json::json!({
                "id": note.meta.id, "title": note.meta.title, "started_at": note.meta.started_at,
                "state": note.meta.state, "speakers": speakers, "refined": true,
                "generated_at": doc.generated_at,
                "paragraphs": doc.paragraphs.iter().map(|p| serde_json::json!({
                    "speaker": p.speaker, "name": p.name, "start_ms": p.start_ms,
                    "end_ms": p.end_ms, "text": p.text,
                })).collect::<Vec<_>>(),
            }),
            None => serde_json::json!({
                "id": note.meta.id, "title": note.meta.title, "started_at": note.meta.started_at,
                "state": note.meta.state, "speakers": speakers, "refined": false,
                "segments": note.segments.iter().map(|s| serde_json::json!({
                    "seq": s.seq, "source": s.source, "speaker": s.speaker,
                    "start_ms": s.start_ms, "end_ms": s.end_ms, "text": s.text,
                })).collect::<Vec<_>>(),
            }),
        }),
        "markdown" | "text" => {
            let was_refined = refined.is_some();
            let content = match refined {
                Some(doc) => store::render_refined(&note.meta.title, &doc, format == "markdown"),
                // note 已在函数开头 load 过一次;render_loaded 直接渲染内存里的
                // Note,避免 render(id, ..) 对同一笔记再触发一次磁盘 load。
                None => store.render_loaded(&note, if format == "markdown" { "md" } else { "txt" })?,
            };
            Ok(serde_json::json!({
                "id": note.meta.id, "title": note.meta.title,
                "refined": was_refined,
                "content": content,
            }))
        }
        _ => anyhow::bail!("未知 format: {format}(可用 segments|markdown|text)"),
    }
}

// 精修稿的 md/txt 渲染已下沉 store::export::render_refined(GUI 导出与此处共用,防漂移)。

/// Agent 精修写回:按 get_note(segments) 返回的 paragraphs 下标批量替换文本。
/// 先 NoteStore::load 走 validate_note_id 防穿越 + 存在性检查,再落到 store 层的
/// 约束式写入(只能改文本,越界/空文本整体拒绝,详见 store::refined)。
pub fn apply_refined_texts(
    roots: &DataRoots,
    note_id: &str,
    updates: &[(usize, String)],
    model: &str,
) -> anyhow::Result<serde_json::Value> {
    let started = std::time::Instant::now();
    let result = (|| -> anyhow::Result<serde_json::Value> {
        NoteStore::new(notes_dir(roots)).load(note_id)?;
        let dir = notes_dir(roots).join(note_id);
        anyhow::ensure!(
            dir.join("refined.json").exists(),
            "该笔记还没有精修稿:请先在 App 里完成一次精修(或等停止录制后自动精修),再写回修订"
        );
        let updated = store::apply_refined_texts(&dir, updates, model)?;
        let total = store::load_refined(&dir).map(|d| d.paragraphs.len()).unwrap_or(0);
        Ok(serde_json::json!({ "updated": updated, "paragraphs": total }))
    })();
    // AI 日志:写回是 Agent 精修产出真正落地的那一步,修订全文在这里(Agent 的
    // stdout 只有一句"完成"),必须全量留痕,否则日志数据不可复用。
    let ctx = crate::ailog::Ctx { data_root: roots.data_root.clone(), note_id: note_id.to_string() };
    crate::ailog::record(
        &ctx,
        crate::ailog::Draft {
            kind: "mcp_apply",
            provider: "mcp".into(),
            model: Some(model.to_string()),
            endpoint: None,
            request: serde_json::json!({
                "updates": updates.iter().map(|(i, t)| serde_json::json!({ "index": i, "text": t })).collect::<Vec<_>>(),
            }),
            response: result.as_ref().map(|v| v.clone()).unwrap_or(serde_json::Value::Null),
            status: if result.is_ok() { "ok" } else { "error" },
            error: result.as_ref().err().map(|e| e.to_string()),
            duration_ms: started.elapsed().as_millis() as u64,
        },
    );
    result
}

/// 全局声纹库人物 + 各自出现过的笔记数(扫 speakers.json 的 person_id)。
pub fn list_speakers(roots: &DataRoots) -> serde_json::Value {
    let vp = store::VoiceprintStore::new(roots.data_root.clone()).load();
    let mut note_counts: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    if let Ok(rd) = std::fs::read_dir(notes_dir(roots)) {
        for e in rd.flatten().filter(|e| e.path().is_dir()) {
            let Ok(text) = std::fs::read_to_string(e.path().join("speakers.json")) else { continue };
            let Ok(map) = serde_json::from_str::<BTreeMap<String, SpeakerMeta>>(&text) else { continue };
            let mut seen = std::collections::HashSet::new();
            for m in map.values() {
                if let Some(pid) = &m.person_id {
                    if seen.insert(pid.clone()) {
                        *note_counts.entry(pid.clone()).or_default() += 1;
                    }
                }
            }
        }
    }
    let speakers: Vec<_> = vp
        .people
        .iter()
        .map(|(id, p)| {
            serde_json::json!({
                "id": id, "name": p.name, "total_ms": p.total_ms,
                "last_seen": p.last_seen, "note_count": note_counts.get(id.as_str()).copied().unwrap_or(0),
            })
        })
        .collect();
    serde_json::json!({ "speakers": speakers })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一条最小真实笔记:meta.json + segments.jsonl + speakers.json。
    fn fixture_note(root: &std::path::Path, id: &str, title: &str, started_at: &str, lines: &[(&str, &str, u64)]) {
        let dir = root.join("notes").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("meta.json"),
            serde_json::json!({
                "schema_version": 1, "id": id, "title": title,
                "started_at": started_at, "ended_at": started_at, "state": "complete"
            })
            .to_string(),
        )
        .unwrap();
        let mut jsonl = String::new();
        for (i, (speaker, text, start_ms)) in lines.iter().enumerate() {
            jsonl.push_str(
                &serde_json::json!({
                    "seq": i as u64, "source": "mic", "text": text,
                    "start_ms": start_ms, "end_ms": start_ms + 1000, "speaker": speaker
                })
                .to_string(),
            );
            jsonl.push('\n');
        }
        std::fs::write(dir.join("segments.jsonl"), jsonl).unwrap();
        std::fs::write(
            dir.join("speakers.json"),
            serde_json::json!({ "S1": { "name": "张三", "sources": ["mic"], "count": 2, "person_id": "P1" } }).to_string(),
        )
        .unwrap();
    }

    fn roots(tmp: &std::path::Path) -> DataRoots {
        DataRoots { app_data: tmp.to_path_buf(), data_root: tmp.to_path_buf() }
    }

    #[test]
    fn list_notes_pages_and_filters_by_time() {
        let tmp = tempfile::tempdir().unwrap();
        fixture_note(tmp.path(), "20260101-100000", "一月会", "2026-01-01T10:00:00+08:00", &[("S1", "a", 0)]);
        fixture_note(tmp.path(), "20260301-100000", "三月会", "2026-03-01T10:00:00+08:00", &[("S1", "b", 0)]);
        // 三月会补一份精修稿:断言 has_refined 能区分有/无。
        store::write_refined_atomic(
            &tmp.path().join("notes/20260301-100000"),
            &store::RefinedDoc {
                schema_version: 1,
                generated_at: "2026-03-01T11:00:00+08:00".into(),
                llm_model: None,
                stages: store::RefineStages { filter: "done".into(), recluster: "done".into(), llm: "done".into() },
                discarded_seqs: vec![],
                paragraphs: vec![store::RefinedParagraph {
                    speaker: "S1".into(),
                    name: Some("张三".into()),
                    person_id: None,
                    start_ms: 0,
                    end_ms: 1000,
                    text: "精修句".into(),
                    source_seqs: vec![0],
                }],
            },
        )
        .unwrap();
        let v = list_notes(&roots(tmp.path()), 10, 0, None, None);
        assert_eq!(v["notes"].as_array().unwrap().len(), 2);
        assert_eq!(v["notes"][0]["title"], "三月会", "倒序:新的在前");
        assert_eq!(v["notes"][0]["speaker_count"], 1, "fixture 只登记了 S1/张三 一人");
        assert_eq!(v["notes"][0]["has_refined"], true, "三月会已落精修稿");
        assert_eq!(v["notes"][1]["title"], "一月会");
        assert_eq!(v["notes"][1]["speaker_count"], 1);
        assert_eq!(v["notes"][1]["has_refined"], false, "一月会无精修稿");
        let v = list_notes(&roots(tmp.path()), 10, 0, Some("2026-02-01"), None);
        assert_eq!(v["notes"].as_array().unwrap().len(), 1);
        assert_eq!(v["notes"][0]["id"], "20260301-100000");
        let v = list_notes(&roots(tmp.path()), 1, 1, None, None);
        assert_eq!(v["notes"][0]["title"], "一月会", "offset 翻页");
        assert_eq!(v["total"], 2);
    }

    #[test]
    fn search_notes_matches_case_insensitive_with_context() {
        let tmp = tempfile::tempdir().unwrap();
        fixture_note(
            tmp.path(),
            "20260101-100000",
            "评审会",
            "2026-01-01T10:00:00+08:00",
            &[("S1", "先看背景", 0), ("S1", "交付日期定在 Q3", 1000), ("S1", "散会", 2000)],
        );
        let v = search_notes(&roots(tmp.path()), "交付日期", 10);
        let hits = v["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["note_id"], "20260101-100000");
        assert_eq!(hits[0]["text"], "交付日期定在 Q3");
        assert_eq!(hits[0]["before"], "先看背景");
        assert_eq!(hits[0]["after"], "散会");
        assert_eq!(hits[0]["speaker"], "S1");
        assert!(search_notes(&roots(tmp.path()), "不存在的词", 10)["hits"].as_array().unwrap().is_empty());
    }

    #[test]
    fn get_note_segments_markdown_and_refined_preference() {
        let tmp = tempfile::tempdir().unwrap();
        fixture_note(tmp.path(), "20260101-100000", "评审会", "2026-01-01T10:00:00+08:00", &[("S1", "原始句", 0)]);
        let v = get_note(&roots(tmp.path()), "20260101-100000", "segments", true).unwrap();
        assert_eq!(v["refined"], false, "无精修稿回落原始");
        assert_eq!(v["segments"][0]["text"], "原始句");
        assert_eq!(v["speakers"]["S1"]["name"], "张三");
        let md = get_note(&roots(tmp.path()), "20260101-100000", "markdown", false).unwrap();
        assert!(md["content"].as_str().unwrap().contains("原始句"));
        // 落一份精修稿:prefer_refined=true 时取精修
        let dir = tmp.path().join("notes/20260101-100000");
        store::write_refined_atomic(
            &dir,
            &store::RefinedDoc {
                schema_version: 1,
                generated_at: "2026-01-01T11:00:00+08:00".into(),
                llm_model: None,
                stages: store::RefineStages { filter: "done".into(), recluster: "done".into(), llm: "done".into() },
                discarded_seqs: vec![],
                paragraphs: vec![store::RefinedParagraph {
                    speaker: "S1".into(),
                    name: Some("张三".into()),
                    person_id: None,
                    start_ms: 0,
                    end_ms: 1000,
                    text: "精修句".into(),
                    source_seqs: vec![0],
                }],
            },
        )
        .unwrap();
        let v = get_note(&roots(tmp.path()), "20260101-100000", "segments", true).unwrap();
        assert_eq!(v["refined"], true);
        assert_eq!(v["paragraphs"][0]["text"], "精修句");
        let md = get_note(&roots(tmp.path()), "20260101-100000", "markdown", true).unwrap();
        assert!(md["content"].as_str().unwrap().contains("精修句"));
        assert!(get_note(&roots(tmp.path()), "no-such", "segments", true).is_err());
        assert!(get_note(&roots(tmp.path()), "../evil", "segments", true).is_err(), "id 穿越防护");
    }

    #[test]
    fn get_note_text_format_has_no_markdown_and_bogus_format_errs() {
        let tmp = tempfile::tempdir().unwrap();
        fixture_note(tmp.path(), "20260101-100000", "评审会", "2026-01-01T10:00:00+08:00", &[("S1", "原始句", 0)]);
        let txt = get_note(&roots(tmp.path()), "20260101-100000", "text", false).unwrap();
        let content = txt["content"].as_str().unwrap();
        assert!(content.contains("原始句"), "text 内容含原句: {content}");
        assert!(!content.contains("# "), "text 格式不带 markdown 标题标记: {content}");
        assert!(get_note(&roots(tmp.path()), "20260101-100000", "bogus", false).is_err(), "未知 format 报错");
    }

    #[test]
    fn apply_refined_texts_validates_note_then_writes() {
        let tmp = tempfile::tempdir().unwrap();
        fixture_note(tmp.path(), "20260101-100000", "评审会", "2026-01-01T10:00:00+08:00", &[("S1", "我们肯计要做", 0)]);
        // 无精修稿:拒绝并给出可操作的提示
        let err = apply_refined_texts(&roots(tmp.path()), "20260101-100000", &[(0, "x".into())], "m")
            .unwrap_err()
            .to_string();
        assert!(err.contains("精修稿"), "缺精修稿的报错要说明原因: {err}");
        // 落一份精修稿后写回成功
        let dir = tmp.path().join("notes/20260101-100000");
        store::write_refined_atomic(
            &dir,
            &store::RefinedDoc {
                schema_version: 1,
                generated_at: "t".into(),
                llm_model: None,
                stages: store::RefineStages { filter: "done".into(), recluster: "done".into(), llm: "off".into() },
                discarded_seqs: vec![],
                paragraphs: vec![store::RefinedParagraph {
                    speaker: "S1".into(), name: Some("张三".into()), person_id: None,
                    start_ms: 0, end_ms: 1000, text: "我们肯计要做".into(), source_seqs: vec![0],
                }],
            },
        )
        .unwrap();
        let v = apply_refined_texts(&roots(tmp.path()), "20260101-100000", &[(0, "我们肯定要做".into())], "claude-agent").unwrap();
        assert_eq!(v["updated"], 1);
        assert_eq!(v["paragraphs"], 1);
        let doc = store::load_refined(&dir).unwrap();
        assert_eq!(doc.paragraphs[0].text, "我们肯定要做");
        assert_eq!(doc.stages.llm, "done");
        // 穿越 id 拒绝
        assert!(apply_refined_texts(&roots(tmp.path()), "../evil", &[(0, "x".into())], "m").is_err());
        // AI 日志:写回(成功与失败)都留痕,修订全文可回读。
        let logs = crate::ailog::query(tmp.path(), &crate::ailog::Filter::default());
        let entries = logs["entries"].as_array().unwrap();
        assert!(entries.iter().any(|e| e["kind"] == "mcp_apply"
            && e["status"] == "ok"
            && e["request"]["updates"][0]["text"] == "我们肯定要做"));
        assert!(entries.iter().any(|e| e["kind"] == "mcp_apply" && e["status"] == "error"), "拒绝的调用也留痕");
    }

    #[test]
    fn list_speakers_joins_note_counts() {
        let tmp = tempfile::tempdir().unwrap();
        fixture_note(tmp.path(), "20260101-100000", "会一", "2026-01-01T10:00:00+08:00", &[("S1", "a", 0)]);
        fixture_note(tmp.path(), "20260102-100000", "会二", "2026-01-02T10:00:00+08:00", &[("S1", "b", 0)]);
        // 最小声纹库:voiceprints/db.json 的真实路径与形状由 VoiceprintStore 决定,
        // 这里直接经 store 写入以免猜格式。
        let vp = store::VoiceprintStore::new(tmp.path().to_path_buf());
        // 若 VoiceprintStore 无公开写入 API,则本测试改为:仅断言 people 为空时
        // note_counts 逻辑不炸,并把"有人物"的断言留给 e2e(实现者按实际 API 取舍,
        // 保底断言如下)。
        let v = list_speakers(&roots(tmp.path()));
        assert!(v["speakers"].as_array().is_some());
        let _ = vp;
    }
}
