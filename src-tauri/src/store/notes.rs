use super::{
    write_meta_atomic, write_speakers_atomic, Note, NoteMeta, NoteSummary, SegmentRecord,
    SpeakerMeta, SCHEMA_VERSION,
};
use std::collections::BTreeMap;
use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};

/// 非活动写者全局编辑锁。NoteStore 每命令新建、无状态,speakers.json /
/// segments.jsonl 的 read-modify-write 之间没有任何互斥,并发编辑会整表互相
/// 覆盖丢更新。锁内建于变更方法——调用方无法遗忘;编辑均为毫秒级稀有操作,
/// 跨笔记串行无感知。活动写者走 NoteWriter 自己的锁,与此无关。
/// 毒化忽略(into_inner):每次落盘各自原子,持锁线程 panic 不留半写状态。
static EDIT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn edit_guard() -> std::sync::MutexGuard<'static, ()> {
    EDIT_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// 笔记静态读写：目录扫描出列表，逐行解析 jsonl，损坏容忍。
pub struct NoteStore {
    notes_dir: PathBuf,
}

impl NoteStore {
    pub fn new(notes_dir: PathBuf) -> Self {
        Self { notes_dir }
    }

    /// id 合法性校验（防路径穿越）+ 存在性检查。
    pub(super) fn note_dir(&self, id: &str) -> anyhow::Result<PathBuf> {
        super::validate_note_id(id)?;
        let dir = self.notes_dir.join(id);
        if !dir.is_dir() {
            anyhow::bail!("笔记不存在: {id}");
        }
        Ok(dir)
    }

    /// 扫描 notes 目录，按 started_at 倒序（RFC3339 同时区字典序即时间序；
    /// meta 损坏项 started_at 为空串，自然沉底）。
    pub fn list(&self) -> Vec<NoteSummary> {
        let Ok(entries) = fs::read_dir(&self.notes_dir) else {
            return Vec::new();
        };
        let mut out: Vec<NoteSummary> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .map(|e| summarize(&e.path()))
            .collect();
        out.sort_by(|a, b| b.started_at.cmp(&a.started_at).then(b.id.cmp(&a.id)));
        out
    }

    pub fn load(&self, id: &str) -> anyhow::Result<Note> {
        let dir = self.note_dir(id)?;
        let meta = read_meta(&dir).unwrap_or_else(|| fallback_meta(&dir));
        let mut segments = Vec::new();
        let mut skipped_lines = 0u32;
        if let Ok(f) = fs::File::open(dir.join("segments.jsonl")) {
            for line in std::io::BufReader::new(f).lines() {
                let Ok(line) = line else {
                    skipped_lines += 1;
                    continue;
                };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<SegmentRecord>(&line) {
                    Ok(r) => segments.push(r),
                    Err(_) => skipped_lines += 1,
                }
            }
        }
        // 读侧单一真值源:过滤空白段 + 按 start_ms 稳定排序(同值按 seq),消除
        // ECHO hold 造成的落盘交错——详情页与导出共同继承此语义,防两处漂移。
        // 磁盘文件序不动:编辑重写走 read_jsonl_lines 原始行,续录 next_seq 由
        // writer 自扫 jsonl,均不经此处。空白段非损坏,不计 skipped_lines。
        segments.retain(|s| !s.text.trim().is_empty());
        segments.sort_by(|a, b| a.start_ms.cmp(&b.start_ms).then(a.seq.cmp(&b.seq)));
        let speakers = read_speakers(&dir);
        Ok(Note { meta, segments, skipped_lines, speakers })
    }

    pub fn rename(&self, id: &str, title: &str) -> anyhow::Result<()> {
        let _guard = edit_guard();
        let dir = self.note_dir(id)?;
        let mut meta = read_meta(&dir).unwrap_or_else(|| fallback_meta(&dir));
        meta.title = title.to_string();
        write_meta_atomic(&dir, &meta)
    }

    pub fn delete(&self, id: &str) -> anyhow::Result<()> {
        let _guard = edit_guard();
        let dir = self.note_dir(id)?;
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    /// 改说话人显示名：读表（缺失则视为空表新建）→ 设 name → 原子写 speakers.json。
    pub fn rename_speaker(&self, id: &str, speaker_id: &str, name: &str) -> anyhow::Result<()> {
        let _guard = edit_guard();
        let dir = self.note_dir(id)?;
        let mut speakers = read_speakers(&dir);
        speakers
            .entry(speaker_id.to_string())
            .or_insert_with(|| SpeakerMeta { name: String::new(), sources: Vec::new(), centroid: None, count: 0 })
            .name = name.to_string();
        write_speakers_atomic(&dir, &speakers)
    }

    /// 改段落文本。空文本拒绝（如需去段请用 delete_segment）。
    pub fn edit_segment_text(
        &self,
        id: &str,
        seq: u64,
        expected_text: &str,
        new_text: &str,
    ) -> anyhow::Result<()> {
        let _guard = edit_guard();
        let new_text = new_text.trim();
        if new_text.is_empty() {
            anyhow::bail!("文本不能为空（如需去掉这段请用删除）");
        }
        let dir = self.note_dir(id)?;
        let mut lines = read_jsonl_lines(&dir.join("segments.jsonl"));
        find_seg(&mut lines, seq, expected_text)?.text = new_text.to_string();
        write_jsonl_atomic(&dir, &lines)
    }

    /// 物理删除段落行。speakers.json 不清孤儿说话人（无害，chips 仍可改名）。
    pub fn delete_segment(&self, id: &str, seq: u64, expected_text: &str) -> anyhow::Result<()> {
        let _guard = edit_guard();
        let dir = self.note_dir(id)?;
        let mut lines = read_jsonl_lines(&dir.join("segments.jsonl"));
        find_seg(&mut lines, seq, expected_text)?;
        lines.retain(|l| !matches!(l, JsonlLine::Seg(r) if r.seq == seq));
        write_jsonl_atomic(&dir, &lines)
    }

    /// 改段落说话人归属。speaker_id="new" → 分配 S<max+1>（max 跨 speakers.json 键与
    /// 段内既有 speaker id，防与孤儿 id 撞号）先入表再改段（中间崩溃只留无害孤儿）。
    /// 只改 segment.speaker 字段，不回灌声纹质心（离线编辑不影响聚类）。
    pub fn set_segment_speaker(
        &self,
        id: &str,
        seq: u64,
        expected_text: &str,
        speaker_id: &str,
    ) -> anyhow::Result<String> {
        let _guard = edit_guard();
        let dir = self.note_dir(id)?;
        let mut lines = read_jsonl_lines(&dir.join("segments.jsonl"));
        find_seg(&mut lines, seq, expected_text)?;
        let mut speakers = read_speakers(&dir);
        let target = if speaker_id == "new" {
            let num = |s: &str| s.strip_prefix('S').and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
            let max_known = speakers
                .keys()
                .map(|k| num(k))
                .chain(lines.iter().filter_map(|l| match l {
                    JsonlLine::Seg(r) => r.speaker.as_deref().map(num),
                    _ => None,
                }))
                .max()
                .unwrap_or(0);
            let new_id = format!("S{}", max_known + 1);
            speakers.insert(
                new_id.clone(),
                SpeakerMeta { name: String::new(), sources: Vec::new(), centroid: None, count: 0 },
            );
            write_speakers_atomic(&dir, &speakers)?;
            new_id
        } else {
            if !speakers.contains_key(speaker_id) {
                anyhow::bail!("未知说话人: {speaker_id}");
            }
            speaker_id.to_string()
        };
        find_seg(&mut lines, seq, expected_text)?.speaker = Some(target.clone());
        write_jsonl_atomic(&dir, &lines)?;
        Ok(target)
    }
}

/// segments.jsonl 的一行：可解析段或损坏原文。编辑重写时损坏行原样保留（不丢数据）。
enum JsonlLine {
    Seg(SegmentRecord),
    Raw(String),
}

fn read_jsonl_lines(path: &Path) -> Vec<JsonlLine> {
    let Ok(f) = fs::File::open(path) else { return Vec::new() };
    std::io::BufReader::new(f)
        .lines()
        .filter_map(|l| l.ok())
        .filter(|l| !l.trim().is_empty())
        .map(|l| match serde_json::from_str::<SegmentRecord>(&l) {
            Ok(r) => JsonlLine::Seg(r),
            Err(_) => JsonlLine::Raw(l),
        })
        .collect()
}

/// 原子重写 segments.jsonl（tmp+rename，与 meta/speakers 同哲学）。
fn write_jsonl_atomic(dir: &Path, lines: &[JsonlLine]) -> anyhow::Result<()> {
    let tmp = dir.join("segments.jsonl.tmp");
    let mut out = String::new();
    for l in lines {
        match l {
            JsonlLine::Seg(r) => out.push_str(&serde_json::to_string(r)?),
            JsonlLine::Raw(s) => out.push_str(s),
        }
        out.push('\n');
    }
    fs::write(&tmp, out)?;
    fs::rename(&tmp, dir.join("segments.jsonl"))?;
    Ok(())
}

/// 按 seq 定位段并做乐观校验（seq 跨续录单调唯一，见 writer.rs resume 测试）。
fn find_seg<'a>(
    lines: &'a mut [JsonlLine],
    seq: u64,
    expected_text: &str,
) -> anyhow::Result<&'a mut SegmentRecord> {
    for l in lines.iter_mut() {
        if let JsonlLine::Seg(r) = l {
            if r.seq == seq {
                if r.text != expected_text {
                    anyhow::bail!("段落内容已变化，请刷新后重试");
                }
                return Ok(r);
            }
        }
    }
    anyhow::bail!("段落不存在（seq={seq}）")
}

/// speakers.json 缺失/损坏 → 空表（P3 产物无此文件，属正常情况，容忍不报错）。
fn read_speakers(dir: &Path) -> BTreeMap<String, SpeakerMeta> {
    fs::read_to_string(dir.join("speakers.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn read_meta(dir: &Path) -> Option<NoteMeta> {
    let s = fs::read_to_string(dir.join("meta.json")).ok()?;
    serde_json::from_str(&s).ok()
}

/// meta 损坏/缺失兜底：以文件夹名当 id，标题标注损坏，按 complete 展示。
fn fallback_meta(dir: &Path) -> NoteMeta {
    let id = dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    NoteMeta {
        schema_version: SCHEMA_VERSION,
        id: id.clone(),
        title: format!("{id}（元数据损坏）"),
        started_at: String::new(),
        ended_at: None,
        state: "complete".into(),
    }
}

fn summarize(dir: &Path) -> NoteSummary {
    let meta = read_meta(dir).unwrap_or_else(|| fallback_meta(dir));
    // 活跃时长优先：以段落时间轴最大 end_ms 为准（与转写时间戳/录制计时一致，
    // 不含暂停与尾部静默）；无可解析段落的完成会议回退墙钟时长。
    let duration_secs = max_end_ms(&dir.join("segments.jsonl"))
        .map(|ms| ms / 1000)
        .or_else(|| if meta.state == "complete" { duration_from_meta(&meta) } else { None });
    NoteSummary {
        id: meta.id,
        title: meta.title,
        started_at: meta.started_at,
        duration_secs,
        state: meta.state,
    }
}

fn duration_from_meta(meta: &NoteMeta) -> Option<u64> {
    let start = chrono::DateTime::parse_from_rfc3339(&meta.started_at).ok()?;
    let end = chrono::DateTime::parse_from_rfc3339(meta.ended_at.as_deref()?).ok()?;
    Some((end - start).num_seconds().max(0) as u64)
}

fn max_end_ms(jsonl: &Path) -> Option<u64> {
    let f = fs::File::open(jsonl).ok()?;
    let mut max = None;
    for line in std::io::BufReader::new(f).lines() {
        let Ok(line) = line else { continue };
        if let Ok(r) = serde_json::from_str::<SegmentRecord>(&line) {
            max = Some(r.end_ms.max(max.unwrap_or(0)));
        }
    }
    max
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::writer::NoteWriter;

    fn now() -> chrono::DateTime<chrono::Local> {
        chrono::Local::now()
    }

    /// 造一场完成的会议，返回 id。
    fn make_note(notes_dir: &std::path::Path, texts: &[&str], finalize: bool) -> String {
        let mut w = NoteWriter::create(notes_dir, now()).unwrap();
        for (i, t) in texts.iter().enumerate() {
            let s = i as u64 * 1000;
            w.append_final(if i % 2 == 0 { "mic" } else { "system" }, t, s, s + 900, None).unwrap();
        }
        if finalize {
            w.finalize(now()).unwrap();
        }
        w.note_id().to_string()
    }

    #[test]
    fn list_sorts_desc_and_loads_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let id1 = make_note(tmp.path(), &["你好", "hello"], true);
        let id2 = make_note(tmp.path(), &["第二场"], true);
        let store = NoteStore::new(tmp.path().to_path_buf());

        let list = store.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, id2, "倒序：新会议在前");
        assert_eq!(list[1].id, id1);
        assert_eq!(list[0].state, "complete");
        assert!(list[0].duration_secs.is_some());

        let note = store.load(&id1).unwrap();
        assert_eq!(note.segments.len(), 2);
        assert_eq!(note.segments[0].text, "你好");
        assert_eq!(note.segments[1].source, "system");
        assert_eq!(note.skipped_lines, 0);
    }

    #[test]
    fn interrupted_note_lists_with_duration_from_last_segment() {
        let tmp = tempfile::tempdir().unwrap();
        let id = make_note(tmp.path(), &["一", "二", "三"], false); // 不 finalize = 崩溃
        let store = NoteStore::new(tmp.path().to_path_buf());
        let list = store.list();
        assert_eq!(list[0].state, "recording", "落盘态保持诚实");
        // 第 3 段 end_ms = 2000+900 → 2 秒
        assert_eq!(list[0].duration_secs, Some(2));
        let note = store.load(&id).unwrap();
        assert_eq!(note.segments.len(), 3, "崩溃前内容完好");
    }

    #[test]
    fn duration_prefers_segment_timeline_over_wall_clock() {
        let tmp = tempfile::tempdir().unwrap();
        // 完成的会议，段落时间轴止于 1900ms → 时长应为 1 秒（活跃时长），而非墙钟差。
        let id = make_note(tmp.path(), &["a", "b"], true);
        let store = NoteStore::new(tmp.path().to_path_buf());
        let list = store.list();
        assert_eq!(list[0].id, id);
        assert_eq!(list[0].duration_secs, Some(1), "以最大 end_ms(1900ms)为准");
    }

    #[test]
    fn load_skips_truncated_tail_line() {
        let tmp = tempfile::tempdir().unwrap();
        let id = make_note(tmp.path(), &["完整句"], false);
        // 模拟崩溃写了半行
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(tmp.path().join(&id).join("segments.jsonl"))
            .unwrap();
        f.write_all(b"{\"seq\":1,\"source\":\"mic\",\"te").unwrap();
        drop(f);

        let note = NoteStore::new(tmp.path().to_path_buf()).load(&id).unwrap();
        assert_eq!(note.segments.len(), 1);
        assert_eq!(note.skipped_lines, 1);
    }

    #[test]
    fn corrupt_meta_falls_back_to_folder_name() {
        let tmp = tempfile::tempdir().unwrap();
        let id = make_note(tmp.path(), &["x"], true);
        std::fs::write(tmp.path().join(&id).join("meta.json"), "not json").unwrap();
        let store = NoteStore::new(tmp.path().to_path_buf());
        let list = store.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, id);
        assert!(list[0].title.contains("元数据损坏"));
        // 内容仍可读
        let note = store.load(&id).unwrap();
        assert_eq!(note.segments.len(), 1);
    }

    #[test]
    fn rename_and_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let id = make_note(tmp.path(), &["x"], true);
        let store = NoteStore::new(tmp.path().to_path_buf());
        store.rename(&id, "周会").unwrap();
        assert_eq!(store.load(&id).unwrap().meta.title, "周会");
        assert_eq!(store.list()[0].title, "周会");
        store.delete(&id).unwrap();
        assert!(store.list().is_empty());
        assert!(store.load(&id).is_err());
    }

    #[test]
    fn rejects_path_traversal_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let store = NoteStore::new(tmp.path().to_path_buf());
        for bad in ["../x", "a/b", "a\\b", "..", ""] {
            assert!(store.delete(bad).is_err(), "应拒绝非法 id: {bad}");
            assert!(store.load(bad).is_err());
            assert!(store.rename(bad, "t").is_err());
        }
    }

    #[test]
    fn empty_or_missing_notes_dir_lists_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let store = NoteStore::new(tmp.path().join("不存在"));
        assert!(store.list().is_empty());
    }

    /// 造带说话人的笔记：segs = (text, speaker)；known = 写入 speakers.json 的说话人表
    /// （与段内 speaker 解耦——测试需要「段里有、表里没有」的孤儿 id）。
    fn make_spk_note(dir: &std::path::Path, segs: &[(&str, Option<&str>)], known: &[&str]) -> String {
        let mut w = NoteWriter::create(dir, now()).unwrap();
        for (i, (t, spk)) in segs.iter().enumerate() {
            let s = i as u64 * 1000;
            w.append_final("mic", t, s, s + 900, *spk).unwrap();
        }
        if !known.is_empty() {
            let pairs: Vec<(String, Vec<String>)> =
                known.iter().map(|s| (s.to_string(), vec!["mic".to_string()])).collect();
            w.sync_speakers(&pairs).unwrap();
        }
        w.finalize(now()).unwrap();
        w.note_id().to_string()
    }

    #[test]
    fn edit_segment_text_rewrites_only_target() {
        let tmp = tempfile::tempdir().unwrap();
        let id = make_spk_note(tmp.path(), &[("原文一", None), ("原文二", None)], &[]);
        let store = NoteStore::new(tmp.path().to_path_buf());
        store.edit_segment_text(&id, 1, "原文二", "改后二").unwrap();
        let n = store.load(&id).unwrap();
        assert_eq!(n.segments[0].text, "原文一", "非目标段不动");
        assert_eq!(n.segments[1].text, "改后二");
        assert_eq!(n.segments[1].seq, 1, "seq/时间戳等其余字段保留");
        assert_eq!(n.segments[1].start_ms, 1000);
    }

    #[test]
    fn edit_rejects_stale_expected_and_blank_text() {
        let tmp = tempfile::tempdir().unwrap();
        let id = make_spk_note(tmp.path(), &[("原文", None)], &[]);
        let store = NoteStore::new(tmp.path().to_path_buf());
        let e = store.edit_segment_text(&id, 0, "别人已改过", "x").unwrap_err();
        assert!(e.to_string().contains("请刷新后重试"), "乐观冲突提示: {e}");
        assert!(store.edit_segment_text(&id, 0, "原文", "   ").is_err(), "空文本拒绝");
        assert!(store.edit_segment_text(&id, 99, "原文", "x").is_err(), "seq 不存在");
        assert_eq!(store.load(&id).unwrap().segments[0].text, "原文", "拒绝路径不落盘");
    }

    #[test]
    fn delete_segment_removes_line_and_preserves_corrupt_raw() {
        let tmp = tempfile::tempdir().unwrap();
        let id = make_spk_note(tmp.path(), &[("一", None), ("二", None)], &[]);
        // 人为插入损坏行：编辑重写后必须原样保留（不丢数据）。
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(tmp.path().join(&id).join("segments.jsonl"))
            .unwrap();
        f.write_all(b"{corrupt-line\n").unwrap();
        drop(f);
        let store = NoteStore::new(tmp.path().to_path_buf());
        store.delete_segment(&id, 0, "一").unwrap();
        let n = store.load(&id).unwrap();
        assert_eq!(n.segments.len(), 1);
        assert_eq!(n.segments[0].text, "二");
        assert_eq!(n.skipped_lines, 1, "损坏行经重写仍在（原样保留）");
    }

    #[test]
    fn set_segment_speaker_existing_new_and_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        // speakers.json 有 S1、S3；另有孤儿段 speaker=S5（表里没有）。
        let id = make_spk_note(tmp.path(), &[("甲", Some("S1")), ("乙", Some("S3")), ("丙", Some("S5"))], &["S1", "S3"]);
        let store = NoteStore::new(tmp.path().to_path_buf());
        // 改为既有说话人
        assert_eq!(store.set_segment_speaker(&id, 0, "甲", "S3").unwrap(), "S3");
        assert_eq!(store.load(&id).unwrap().segments[0].speaker.as_deref(), Some("S3"));
        // 未知说话人拒绝
        assert!(store.set_segment_speaker(&id, 0, "甲", "S99").is_err());
        // 新建：max 取 speakers 表(S1,S3,S5) 与段内(S5) 的并集 → S6
        let got = store.set_segment_speaker(&id, 1, "乙", "new").unwrap();
        assert_eq!(got, "S6");
        let n = store.load(&id).unwrap();
        assert_eq!(n.segments[1].speaker.as_deref(), Some("S6"));
        assert!(n.speakers.contains_key("S6"), "新说话人已入表(空名,无质心)");
        assert_eq!(n.speakers["S6"].name, "");
        assert!(n.speakers["S6"].centroid.is_none());
    }

    #[test]
    fn rename_speaker_persists_and_missing_file_tolerated() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), now()).unwrap();
        let id = w.note_id().to_string();
        w.append_final("mic", "x", 0, 2000, Some("S1")).unwrap();
        w.sync_speakers(&[("S1".into(), vec!["mic".into()])]).unwrap();
        w.finalize(now()).unwrap();
        let store = NoteStore::new(tmp.path().to_path_buf());
        store.rename_speaker(&id, "S1", "张三").unwrap();
        assert_eq!(store.load(&id).unwrap().speakers["S1"].name, "张三");
        // speakers.json 缺失的旧笔记(P3 产物)：load 正常，speakers 为空表
        let id2 = make_note(tmp.path(), &["旧"], true);
        let n2 = store.load(&id2).unwrap();
        assert!(n2.speakers.is_empty());
    }

    /// 并发丢更新回归:rename_speaker 与 set_segment_speaker("new") 两线程互跑,
    /// 无锁时各自 read-modify-write 整表覆盖,终态必然缺改动;EDIT_LOCK 下两者全存活。
    #[test]
    fn concurrent_speaker_edits_do_not_lose_updates() {
        use std::sync::Arc;
        let tmp = tempfile::tempdir().unwrap();
        let dir = Arc::new(tmp.path().to_path_buf());

        // 先创建笔记，确保目录存在
        let id = {
            let mut w = NoteWriter::create(&dir, now()).unwrap();
            w.append_final("mic", "甲", 0, 900, Some("S1")).unwrap();
            w.append_final("mic", "乙", 1000, 1900, Some("S1")).unwrap();
            w.sync_speakers(&[("S1".into(), vec!["mic".into()])]).unwrap();
            w.finalize(now()).unwrap();
            w.note_id().to_string()
        };

        let t1 = std::thread::spawn({
            let (dir, id) = (Arc::clone(&dir), id.clone());
            move || {
                for i in 0..20 {
                    NoteStore::new((*dir).clone()).rename_speaker(&id, "S1", &format!("名{i}")).unwrap();
                }
            }
        });
        let t2 = std::thread::spawn({
            let (dir, id) = (Arc::clone(&dir), id.clone());
            move || {
                for _ in 0..20 {
                    NoteStore::new((*dir).clone()).set_segment_speaker(&id, 1, "乙", "new").unwrap();
                }
            }
        });
        t1.join().unwrap();
        t2.join().unwrap();
        let n = NoteStore::new((*dir).clone()).load(&id).unwrap();
        assert_eq!(n.speakers["S1"].name, "名19", "rename 线程的最后写入存活");
        // S1 + 20 个新建说话人:任何一次丢更新都会让计数不足 21。
        assert_eq!(n.speakers.len(), 21, "20 次新建全部存活,无丢更新");
    }

    /// 读侧单一真值源:load 过滤空白段、按 (start_ms, seq) 稳定排序——
    /// 详情页与导出共同继承,消除 ECHO hold 落盘交错。
    #[test]
    fn load_filters_blank_and_sorts_by_start_ms() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), now()).unwrap();
        let id = w.note_id().to_string();
        w.append_final("mic", "后", 5000, 6000, None).unwrap();       // seq 0
        w.append_final("system", "   ", 500, 900, None).unwrap();     // seq 1 空白段
        w.append_final("mic", "前", 1000, 1500, None).unwrap();       // seq 2
        w.append_final("system", "同前", 1000, 1400, None).unwrap();  // seq 3 同 start,按 seq 稳定
        w.finalize(now()).unwrap();
        let n = NoteStore::new(tmp.path().to_path_buf()).load(&id).unwrap();
        let texts: Vec<&str> = n.segments.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["前", "同前", "后"], "空白段滤除,start_ms 升序,同值按 seq");
        assert_eq!(n.skipped_lines, 0, "空白段不是损坏行,不计 skipped");
    }
}
