use super::{write_meta_atomic, NoteMeta, SegmentRecord, SCHEMA_VERSION};
use chrono::{DateTime, Local};
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// 录制期落盘器：meta 原子写 + segments.jsonl 追加写。
/// 写失败时段进内存待写队列（不设上界：内存丢内容比 OOM 更早违背原则，
/// 几小时会议的文本量级仅 MB），后续 append/finalize 先重试队列。
pub struct NoteWriter {
    dir: PathBuf,
    meta: NoteMeta,
    /// segments.jsonl 追加句柄；写失败置 None，重试时按需重开。
    pub(super) file: Option<File>,
    next_seq: u64,
    pending: VecDeque<String>,
}

impl NoteWriter {
    /// 在 notes_dir 下建会议文件夹（id = 本地时间 YYYYmmdd-HHMMSS，同秒冲突加 -2/-3 后缀），
    /// 写入 state=recording 的 meta，打开 segments.jsonl。
    pub fn create(notes_dir: &Path, now: DateTime<Local>) -> anyhow::Result<Self> {
        std::fs::create_dir_all(notes_dir)?;
        let base = now.format("%Y%m%d-%H%M%S").to_string();
        let mut id = base.clone();
        let mut n = 1;
        let dir = loop {
            let d = notes_dir.join(&id);
            if !d.exists() {
                break d;
            }
            n += 1;
            id = format!("{base}-{n}");
        };
        std::fs::create_dir(&dir)?;
        let meta = NoteMeta {
            schema_version: SCHEMA_VERSION,
            id: id.clone(),
            title: now.format("%Y-%m-%d %H:%M 会议").to_string(),
            started_at: now.to_rfc3339(),
            ended_at: None,
            state: "recording".into(),
        };
        write_meta_atomic(&dir, &meta)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("segments.jsonl"))?;
        Ok(Self { dir, meta, file: Some(file), next_seq: 0, pending: VecDeque::new() })
    }

    pub fn note_id(&self) -> &str {
        &self.meta.id
    }

    /// 是否已产生过任何定稿段（含仍在待写队列中的）。
    pub fn has_content(&self) -> bool {
        self.next_seq > 0
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 追加一条定稿段。失败时段留在待写队列并返回 Err（调用方发 storage 降级事件），
    /// 后续调用先重试队列，保证顺序与 seq 单调。
    pub fn append_final(
        &mut self,
        source: &str,
        text: &str,
        start_ms: u64,
        end_ms: u64,
    ) -> anyhow::Result<()> {
        let rec = SegmentRecord {
            seq: self.next_seq,
            source: source.into(),
            text: text.into(),
            start_ms,
            end_ms,
            speaker: None,
        };
        self.next_seq += 1;
        let line = serde_json::to_string(&rec)?;
        self.pending.push_back(line);
        self.flush_pending()
    }

    /// 收尾：先补写待写队列；仍写不出则直接返回 Err、**不动 meta**——state 留在
    /// "recording"，笔记诚实地显示为「已中断」（详情页/列表页已有对应横幅/徽标），
    /// 而不是被静默标记为 complete 掩盖内容缺失。队列补写成功后才把
    /// ended_at 写入、state 置 complete 并原子落盘。
    pub fn finalize(&mut self, now: DateTime<Local>) -> anyhow::Result<()> {
        self.flush_pending()?;
        self.meta.ended_at = Some(now.to_rfc3339());
        self.meta.state = "complete".into();
        write_meta_atomic(&self.dir, &self.meta)
    }

    fn flush_pending(&mut self) -> anyhow::Result<()> {
        while let Some(line) = self.pending.front() {
            if self.file.is_none() {
                self.file = Some(
                    OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(self.dir.join("segments.jsonl"))
                        .map_err(|e| anyhow::anyhow!("重开 segments.jsonl 失败: {e}"))?,
                );
            }
            let file = self.file.as_mut().unwrap();
            let res = file
                .write_all(line.as_bytes())
                .and_then(|_| file.write_all(b"\n"))
                .and_then(|_| file.flush());
            if let Err(e) = res {
                // 句柄可能已坏（如卷被卸载），丢弃句柄，下次重开重试。
                // 半行写入的风险由读取端容忍（load 跳过损坏行）。
                self.file = None;
                anyhow::bail!("写 segments.jsonl 失败: {e}");
            }
            self.pending.pop_front();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::NoteMeta;

    fn now() -> chrono::DateTime<chrono::Local> {
        chrono::Local::now()
    }

    fn read_meta(dir: &std::path::Path) -> NoteMeta {
        serde_json::from_str(&std::fs::read_to_string(dir.join("meta.json")).unwrap()).unwrap()
    }

    fn read_lines(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("segments.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    #[test]
    fn create_writes_recording_meta_and_unique_id() {
        let tmp = tempfile::tempdir().unwrap();
        let w1 = NoteWriter::create(tmp.path(), now()).unwrap();
        let meta = read_meta(w1.dir());
        assert_eq!(meta.state, "recording");
        assert_eq!(meta.schema_version, crate::store::SCHEMA_VERSION);
        assert_eq!(meta.id, w1.note_id());
        assert!(meta.ended_at.is_none());
        assert!(!meta.started_at.is_empty());
        assert!(meta.title.ends_with("会议"));
        // 同秒再建：id 加后缀不冲突
        let w2 = NoteWriter::create(tmp.path(), now()).unwrap();
        assert_ne!(w1.note_id(), w2.note_id());
    }

    #[test]
    fn append_and_finalize_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), now()).unwrap();
        assert!(!w.has_content());
        w.append_final("mic", "第一句", 0, 1500).unwrap();
        w.append_final("system", "second", 1500, 3000).unwrap();
        assert!(w.has_content());

        let lines = read_lines(w.dir());
        assert_eq!(lines.len(), 2);
        let r0: crate::store::SegmentRecord = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(r0.seq, 0);
        assert_eq!(r0.source, "mic");
        assert_eq!(r0.text, "第一句");
        assert_eq!((r0.start_ms, r0.end_ms), (0, 1500));
        assert_eq!(r0.speaker, None);
        let r1: crate::store::SegmentRecord = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(r1.seq, 1);

        w.finalize(now()).unwrap();
        let meta = read_meta(w.dir());
        assert_eq!(meta.state, "complete");
        assert!(meta.ended_at.is_some());
    }

    #[test]
    fn write_failure_queues_and_retries() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), now()).unwrap();
        let dir = w.dir().to_path_buf();

        // 模拟句柄失效 + 目录消失：追加必须失败但段保留在待写队列
        w.file = None;
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(w.append_final("mic", "丢不得", 0, 1000).is_err());

        // 目录恢复后，下一次追加把队列里的段一并补写
        std::fs::create_dir_all(&dir).unwrap();
        w.append_final("mic", "第二句", 1000, 2000).unwrap();
        let lines = read_lines(&dir);
        assert_eq!(lines.len(), 2, "失败段重试补写，一段不丢");
        let r0: crate::store::SegmentRecord = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(r0.text, "丢不得");
        assert_eq!(r0.seq, 0);

        // finalize 重建 meta（此前随目录被删）
        w.finalize(now()).unwrap();
        assert_eq!(read_meta(&dir).state, "complete");
    }

    #[test]
    fn finalize_fails_leaves_recording_state() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), now()).unwrap();
        let dir = w.dir().to_path_buf();

        // 模拟句柄失效 + 目录消失：append 必须失败，段留在待写队列
        w.file = None;
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(w.append_final("mic", "会丢失吗", 0, 1000).is_err());

        // 目录仍不存在：finalize 应失败，且不得把 state 标记为 complete
        // （此时磁盘上连 meta.json 都不存在，正是"不诚实的 complete"要避免的场景）。
        assert!(w.finalize(now()).is_err());

        // 目录恢复后：finalize 应能补写队列并把 meta 正常置 complete，
        // 验证"失败不置 complete、恢复后可补救"的语义。
        std::fs::create_dir_all(&dir).unwrap();
        w.append_final("mic", "第二句", 1000, 2000).unwrap();
        w.finalize(now()).unwrap();

        let meta = read_meta(&dir);
        assert_eq!(meta.state, "complete");
        assert!(meta.ended_at.is_some());
        let lines = read_lines(&dir);
        assert_eq!(lines.len(), 2, "两段都应补写，一段不丢");
    }

    #[test]
    fn full_session_persists_every_final() {
        use crate::audio::mock::MockCapture;
        use crate::audio::{AudioCapture, Source};
        use crate::pipeline::segmenter::{MockSegmenter, Segmenter};
        use crate::store::NoteStore;
        use std::sync::{Arc, Mutex};

        struct CountingRecognizer;
        impl crate::asr::Recognizer for CountingRecognizer {
            fn recognize(&mut self, s: &[f32]) -> anyhow::Result<crate::asr::Transcript> {
                Ok(crate::asr::Transcript { text: format!("len={}", s.len()) })
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let writer = Arc::new(Mutex::new(NoteWriter::create(tmp.path(), now()).unwrap()));
        let id = writer.lock().unwrap().note_id().to_string();
        let emitted = Arc::new(Mutex::new(0usize));

        let cap = MockCapture::from_wav(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/sample_16k.wav"
        ))
        .expect("fixture");
        let sources: Vec<(Source, Box<dyn AudioCapture>, Box<dyn Segmenter>)> =
            vec![(Source::Mic, Box::new(cap), Box::new(MockSegmenter::new(2000)))];

        let (w2, e2) = (writer.clone(), emitted.clone());
        let start = crate::session::start_session(
            sources,
            Box::new(CountingRecognizer),
            None,
            16000,
            4000,
            move |src, text, start_ms, end_ms, _spk| {
                w2.lock().unwrap().append_final(src.as_str(), &text, start_ms, end_ms).unwrap();
                *e2.lock().unwrap() += 1;
            },
            |_, _| {},
            |_| {},
        )
        .expect("start_session");
        let _ = start.handle.stop(); // MockCapture 已灌完帧；stop 排干全部 finals
        writer.lock().unwrap().finalize(now()).unwrap();

        let n = *emitted.lock().unwrap();
        assert!(n > 0, "fixture 应产出至少一个 final");
        let note = NoteStore::new(tmp.path().to_path_buf()).load(&id).unwrap();
        assert_eq!(note.segments.len(), n, "jsonl 行数 = final 事件数，一段不丢");
        assert!(note.segments.windows(2).all(|w| w[1].seq == w[0].seq + 1), "seq 单调");
        assert!(note.segments.windows(2).all(|w| w[1].start_ms >= w[0].start_ms), "时间戳单调");
        assert_eq!(note.meta.state, "complete");
        assert_eq!(note.skipped_lines, 0);
    }
}
