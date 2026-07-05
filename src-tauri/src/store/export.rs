use super::{Note, NoteStore, SegmentRecord, SpeakerMeta};
use std::collections::BTreeMap;
use std::path::PathBuf;

impl NoteStore {
    /// 导出到会议文件夹内的 transcript.md / transcript.txt，返回文件路径。
    pub fn export(&self, id: &str, format: &str) -> anyhow::Result<PathBuf> {
        let note = self.load(id)?;
        let dir = self.note_dir(id)?;
        let (name, content) = match format {
            "md" => ("transcript.md", render_markdown(&note)),
            "txt" => ("transcript.txt", render_text(&note)),
            _ => anyhow::bail!("未知导出格式: {format}"),
        };
        let path = dir.join(name);
        std::fs::write(&path, content)?;
        Ok(path)
    }
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
        let md_path = store.export(&id, "md").unwrap();
        assert_eq!(md_path.file_name().unwrap(), "transcript.md");
        let md = std::fs::read_to_string(&md_path).unwrap();
        let title = store.load(&id).unwrap().meta.title;
        assert!(md.starts_with(&format!("# {title}\n")), "首行为标题: {md}");
        assert!(md.contains("**[我] 00:01:23** 今天开会讨论项目进度。"), "{md}");
        assert!(md.contains("**[对方] 00:01:31** 好的，先看上周的问题。"), "{md}");

        let txt_path = store.export(&id, "txt").unwrap();
        let txt = std::fs::read_to_string(&txt_path).unwrap();
        assert!(txt.contains("[我] 00:01:23 今天开会讨论项目进度。"), "{txt}");
        assert!(!txt.contains("**"), "纯文本无 markdown 记号");

        assert!(store.export(&id, "pdf").is_err(), "未知格式报错");
    }

    #[test]
    fn export_uses_speaker_name_when_present() {
        let mut speakers = std::collections::BTreeMap::new();
        speakers.insert(
            "S1".to_string(),
            crate::store::SpeakerMeta { name: "张三".into(), sources: vec![], centroid: None, count: 0 },
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
        let txt = std::fs::read_to_string(store.export(&id, "txt").unwrap()).unwrap();
        let (i_first, i_later) = (txt.find("先说的").unwrap(), txt.find("后说的").unwrap());
        assert!(i_first < i_later, "导出按 start_ms 序而非落盘序: {txt}");
        assert_eq!(txt.lines().filter(|l| l.starts_with('[')).count(), 2, "空白段被过滤,只剩两段: {txt}");
    }
}
