use super::{Note, NoteStore, RefinedDoc, SegmentRecord, SpeakerMeta};
use std::collections::BTreeMap;
use std::path::PathBuf;

impl NoteStore {
    /// 导出到会议文件夹内的 transcript.md / transcript.txt，返回文件路径。
    /// refined=Some 时导修订稿(所见即所得:用户看着修订稿点导出,不能给他原始逐字稿);
    /// None 走原始 segments 渲染。两者写同一文件名,后导覆盖先导。
    pub fn export(&self, id: &str, format: &str, refined: Option<&RefinedDoc>) -> anyhow::Result<PathBuf> {
        let content = match refined {
            Some(doc) => render_refined(&self.load(id)?.meta.title, doc, format == "md"),
            None => self.render(id, format)?,
        };
        if format != "md" && format != "txt" {
            anyhow::bail!("未知导出格式: {format}");
        }
        let dir = self.note_dir(id)?;
        let name = match format {
            "md" => "transcript.md",
            _ => "transcript.txt",
        };
        let path = dir.join(name);
        std::fs::write(&path, content)?;
        Ok(path)
    }

    /// 渲染导出内容字符串(不落盘)。MCP get_note 与 export 共用同一渲染,防两处漂移。
    pub fn render(&self, id: &str, format: &str) -> anyhow::Result<String> {
        let note = self.load(id)?;
        render_note(&note, format)
    }

    /// 渲染一个已经在内存里的 `Note`,跳过磁盘 load。给 MCP get_note 用——它
    /// 自己已经 load 过同一笔记,再调 `render(id, ..)` 会对同一笔记二次磁盘读取。
    /// export 模块本身是私有 `mod export;`,外部拿不到 `render_note`,所以在
    /// NoteStore 上开一个转发方法。
    pub fn render_loaded(&self, note: &Note, format: &str) -> anyhow::Result<String> {
        render_note(note, format)
    }
}

/// 渲染逻辑本体,供 `render`(先 load 再渲染)与 `render_loaded`(已有 Note,直接渲染)共用。
pub(crate) fn render_note(note: &Note, format: &str) -> anyhow::Result<String> {
    Ok(match format {
        "md" => render_markdown(note),
        "txt" => render_text(note),
        _ => anyhow::bail!("未知导出格式: {format}"),
    })
}

/// 修订稿的 md/txt 渲染(原始稿渲染在下方 render_note,Aing 段形状不同单独渲染;
/// GUI 导出与 MCP get_note 共用本函数,防两处漂移)。
/// 段落标签兜底与前端 speakerLabel 同序:名字 > 关联人物全局编号 > R 簇号。
pub fn render_refined(title: &str, doc: &RefinedDoc, md: bool) -> String {
    let mut out = String::new();
    if md {
        out.push_str(&format!("# {title}\n\n"));
    } else {
        out.push_str(&format!("{title}\n\n"));
    }
    for p in &doc.paragraphs {
        let label = p
            .name
            .clone()
            .filter(|n| !n.is_empty())
            .or_else(|| p.person_id.as_ref().map(|pid| format!("说话人 {}", pid.trim_start_matches('P'))))
            .unwrap_or_else(|| match p.speaker.strip_prefix('R') {
                Some(n) if n.chars().all(|c| c.is_ascii_digit()) => format!("说话人 {n}"),
                _ => p.speaker.clone(),
            });
        let ts = format_ts(p.start_ms);
        if md {
            out.push_str(&format!("**{label}** `[{ts}]`\n\n{}\n\n", p.text));
        } else {
            out.push_str(&format!("{label} [{ts}]\n{}\n\n", p.text));
        }
    }
    out
}

/// 毫秒 → hh:mm:ss。
pub fn format_ts(ms: u64) -> String {
    let s = ms / 1000;
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// 秒 → 人读时长："1 小时 8 分" / "12 分 3 秒" / "45 秒"。
pub(super) fn human_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h} 小时 {m} 分")
    } else if m > 0 {
        format!("{m} 分 {s} 秒")
    } else {
        format!("{s} 秒")
    }
}

/// 段落标签：有说话人 id 且 speakers 表里有非空名字 → 用名字；
/// 有 id 但表里无名（或名为空）→ 「说话人 N」（N 取 id 去掉前导 'S'）；
/// 无 id（未跑声纹/降级）→ 按来源 我/对方。
fn label<'a>(seg: &'a SegmentRecord, speakers: &'a BTreeMap<String, SpeakerMeta>) -> String {
    match &seg.speaker {
        Some(id) => {
            if let Some(name) = speakers.get(id).map(|m| &m.name).filter(|n| !n.is_empty()) {
                name.clone()
            } else {
                format!("说话人 {}", id.trim_start_matches('S'))
            }
        }
        None if seg.source == "mic" => "我".to_string(),
        None => "对方".to_string(),
    }
}

/// 头部第二行："2026-07-03 15:04 – 16:12(1 小时 8 分)"；中断会议结束时间标「中断」。
fn header_line(note: &Note) -> Option<String> {
    let start = chrono::DateTime::parse_from_rfc3339(&note.meta.started_at).ok()?;
    let start_str = start.format("%Y-%m-%d %H:%M").to_string();
    match note
        .meta
        .ended_at
        .as_deref()
        .and_then(|e| chrono::DateTime::parse_from_rfc3339(e).ok())
    {
        Some(end) => {
            let dur = human_duration((end - start).num_seconds().max(0) as u64);
            Some(format!("{start_str} – {}({dur})", end.format("%H:%M")))
        }
        None => Some(format!("{start_str} – 中断")),
    }
}

pub(super) fn render_markdown(note: &Note) -> String {
    let mut out = format!("# {}\n\n", note.meta.title);
    if let Some(h) = header_line(note) {
        out.push_str(&h);
        out.push_str("\n\n");
    }
    for seg in &note.segments {
        out.push_str(&format!(
            "**[{}] {}** {}\n\n",
            label(seg, &note.speakers),
            format_ts(seg.start_ms),
            seg.text
        ));
    }
    out
}

pub(super) fn render_text(note: &Note) -> String {
    let mut out = format!("{}\n\n", note.meta.title);
    if let Some(h) = header_line(note) {
        out.push_str(&h);
        out.push_str("\n\n");
    }
    for seg in &note.segments {
        out.push_str(&format!(
            "[{}] {} {}\n",
            label(seg, &note.speakers),
            format_ts(seg.start_ms),
            seg.text
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::writer::NoteWriter;
    use crate::store::NoteStore;

    #[test]
    fn format_ts_is_hhmmss() {
        assert_eq!(format_ts(0), "00:00:00");
        assert_eq!(format_ts(83_000), "00:01:23");
        assert_eq!(format_ts(4_083_000), "01:08:03");
    }

    #[test]
    fn human_duration_formats() {
        assert_eq!(human_duration(4080), "1 小时 8 分");
        assert_eq!(human_duration(723), "12 分 3 秒");
        assert_eq!(human_duration(45), "45 秒");
    }

    #[test]
    fn export_md_and_txt() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), chrono::Local::now()).unwrap();
        let id = w.note_id().to_string();
        w.append_final("mic", "今天开会讨论项目进度。", 83_000, 86_000, None, None).unwrap();
        w.append_final("system", "好的，先看上周的问题。", 91_000, 94_000, None, None).unwrap();
        w.finalize(chrono::Local::now()).unwrap();

        let store = NoteStore::new(tmp.path().to_path_buf());
        let md_path = store.export(&id, "md", None).unwrap();
        assert_eq!(md_path.file_name().unwrap(), "transcript.md");
        let md = std::fs::read_to_string(&md_path).unwrap();
        let title = store.load(&id).unwrap().meta.title;
        assert!(md.starts_with(&format!("# {title}\n")), "首行为标题: {md}");
        assert!(md.contains("**[我] 00:01:23** 今天开会讨论项目进度。"), "{md}");
        assert!(md.contains("**[对方] 00:01:31** 好的，先看上周的问题。"), "{md}");

        let txt_path = store.export(&id, "txt", None).unwrap();
        let txt = std::fs::read_to_string(&txt_path).unwrap();
        assert!(txt.contains("[我] 00:01:23 今天开会讨论项目进度。"), "{txt}");
        assert!(!txt.contains("**"), "纯文本无 markdown 记号");

        assert!(store.export(&id, "pdf", None).is_err(), "未知格式报错");
    }

    #[test]
    fn export_refined_renders_paragraphs_with_label_fallbacks() {
        use crate::store::{RefineStages, RefinedDoc, RefinedParagraph};
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), chrono::Local::now()).unwrap();
        let id = w.note_id().to_string();
        w.append_final("mic", "原始句。", 0, 2000, Some("S1"), None).unwrap();
        w.finalize(chrono::Local::now()).unwrap();

        let para = |speaker: &str, name: Option<&str>, person: Option<&str>, text: &str| RefinedParagraph {
            speaker: speaker.into(),
            name: name.map(str::to_string),
            person_id: person.map(str::to_string),
            start_ms: 0,
            end_ms: 2000,
            text: text.into(),
            source_seqs: vec![0],
        };
        let doc = RefinedDoc {
            schema_version: 1,
            generated_at: "t".into(),
            llm_model: None,
            stages: RefineStages { filter: "done".into(), recluster: "done".into(), llm: "done".into() },
            discarded_seqs: vec![],
            paragraphs: vec![
                para("R1", Some("张三"), Some("P1"), "有名字用名字。"),
                para("R2", None, Some("P4"), "无名有关联用全局编号。"),
                para("R3", None, None, "全无按 R 簇号兜底。"),
            ],
        };
        let store = NoteStore::new(tmp.path().to_path_buf());
        let md = std::fs::read_to_string(store.export(&id, "md", Some(&doc)).unwrap()).unwrap();
        assert!(md.contains("**张三** `[00:00:00]`"), "{md}");
        assert!(md.contains("**说话人 4**"), "关联人物按 P 号: {md}");
        assert!(md.contains("**说话人 3**"), "未关联按 R 号: {md}");
        assert!(md.contains("有名字用名字。"), "{md}");
        assert!(!md.contains("原始句。"), "Aing 导出不含原始段: {md}");
        // 同名文件:再按原始稿导出,覆盖为原始内容(所见即所得,后导为准)。
        let md2 = std::fs::read_to_string(store.export(&id, "md", None).unwrap()).unwrap();
        assert!(md2.contains("原始句。"), "{md2}");
    }

    #[test]
    fn export_uses_speaker_name_when_present() {
        let mut speakers = std::collections::BTreeMap::new();
        speakers.insert(
            "S1".to_string(),
            crate::store::SpeakerMeta { name: "张三".into(), sources: vec![], centroid: None, count: 0, person_id: None },
        );
        let note = crate::store::Note {
            meta: crate::store::NoteMeta {
                schema_version: 1,
                id: "x".into(),
                title: "t".into(),
                started_at: String::new(),
                ended_at: None,
                state: "complete".into(),
            },
            segments: vec![
                crate::store::SegmentRecord {
                    seq: 0,
                    source: "mic".into(),
                    text: "hi".into(),
                    start_ms: 0,
                    end_ms: 1000,
                    speaker: Some("S1".into()),
                    rms: None,
                },
                crate::store::SegmentRecord {
                    seq: 1,
                    source: "system".into(),
                    text: "yo".into(),
                    start_ms: 1000,
                    end_ms: 2000,
                    speaker: Some("S2".into()), // 表中无此 id
                    rms: None,
                },
                crate::store::SegmentRecord {
                    seq: 2,
                    source: "mic".into(),
                    text: "plain".into(),
                    start_ms: 2000,
                    end_ms: 3000,
                    speaker: None,
                    rms: None,
                },
            ],
            skipped_lines: 0,
            speakers,
        };
        let md = render_markdown(&note);
        assert!(md.contains("**[张三] 00:00:00** hi"), "{md}");
        assert!(md.contains("**[说话人 2] 00:00:01** yo"), "无名兜底为「说话人 N」: {md}");
        assert!(md.contains("**[我] 00:00:02** plain"), "speaker null 仍走 我/对方: {md}");
    }

    #[test]
    fn header_line_covers_normal_interrupted_and_corrupt() {
        // Test normal case: both started_at and ended_at are valid
        let note_normal = crate::store::Note {
            meta: crate::store::NoteMeta {
                schema_version: 1,
                id: "x".into(),
                title: "t".into(),
                started_at: "2026-07-03T15:04:00+08:00".into(),
                ended_at: Some("2026-07-03T16:12:00+08:00".into()),
                state: "complete".into(),
            },
            segments: vec![],
            skipped_lines: 0,
            speakers: Default::default(),
        };
        let md_normal = render_markdown(&note_normal);
        assert!(md_normal.contains("2026-07-03 15:04 – 16:12(1 小时 8 分)"),
            "normal case should contain time range with half-width brackets: {md_normal}");

        // Test interrupted case: ended_at is None
        let note_interrupted = crate::store::Note {
            meta: crate::store::NoteMeta {
                schema_version: 1,
                id: "x".into(),
                title: "t".into(),
                started_at: "2026-07-03T15:04:00+08:00".into(),
                ended_at: None,
                state: "complete".into(),
            },
            segments: vec![],
            skipped_lines: 0,
            speakers: Default::default(),
        };
        let md_interrupted = render_markdown(&note_interrupted);
        assert!(md_interrupted.contains("2026-07-03 15:04 – 中断"),
            "interrupted case should contain 中断: {md_interrupted}");

        // Test corrupt case: started_at is empty
        let note_corrupt = crate::store::Note {
            meta: crate::store::NoteMeta {
                schema_version: 1,
                id: "x".into(),
                title: "t".into(),
                started_at: String::new(),
                ended_at: None,
                state: "complete".into(),
            },
            segments: vec![],
            skipped_lines: 0,
            speakers: Default::default(),
        };
        let md_corrupt = render_markdown(&note_corrupt);
        assert!(!md_corrupt.contains(" – "),
            "corrupt case should not contain ` – ` (header_line skipped): {md_corrupt}");
        assert!(md_corrupt.contains("# t"),
            "corrupt case should still contain title: {md_corrupt}");
    }

    #[test]
    fn export_inherits_display_order_and_blank_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), chrono::Local::now()).unwrap();
        let id = w.note_id().to_string();
        w.append_final("mic", "后说的", 5000, 6000, None, None).unwrap();
        w.append_final("system", "  ", 500, 900, None, None).unwrap();
        w.append_final("mic", "先说的", 1000, 1500, None, None).unwrap();
        w.finalize(chrono::Local::now()).unwrap();
        let store = NoteStore::new(tmp.path().to_path_buf());
        let txt = std::fs::read_to_string(store.export(&id, "txt", None).unwrap()).unwrap();
        let (i_first, i_later) = (txt.find("先说的").unwrap(), txt.find("后说的").unwrap());
        assert!(i_first < i_later, "导出按 start_ms 序而非落盘序: {txt}");
        assert_eq!(txt.lines().filter(|l| l.starts_with('[')).count(), 2, "空白段被过滤,只剩两段: {txt}");
    }
}
