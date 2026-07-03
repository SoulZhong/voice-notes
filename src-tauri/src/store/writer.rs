use super::{write_meta_atomic, write_speakers_atomic, NoteMeta, SegmentRecord, SpeakerMeta, SCHEMA_VERSION};
use chrono::{DateTime, Local};
use std::collections::{BTreeMap, VecDeque};
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
    /// 说话人表内存副本，随 sync_speakers/merge_speaker 原子落盘 speakers.json。
    speakers: BTreeMap<String, SpeakerMeta>,
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
        Ok(Self {
            dir,
            meta,
            file: Some(file),
            next_seq: 0,
            pending: VecDeque::new(),
            speakers: BTreeMap::new(),
        })
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

    /// 说话人表只读访问（供 IPC 层组装 SpeakersEvent，不落盘）。
    pub fn speakers(&self) -> &BTreeMap<String, SpeakerMeta> {
        &self.speakers
    }

    /// 改说话人显示名：只更新内存表，不落盘（落盘由 NoteStore::rename_speaker
    /// 那次直写完成）。防止后续 sync_speakers 覆写——它本就保留非空名，此处只是
    /// 让活动会话的内存态与磁盘同步，避免下一次归簇事件把刚改的名字"打回原形"。
    pub fn set_speaker_name(&mut self, id: &str, name: &str) {
        self.speakers
            .entry(id.to_string())
            .or_insert_with(|| SpeakerMeta { name: String::new(), sources: Vec::new() })
            .name = name.to_string();
    }

    /// 追加一条定稿段。失败时段留在待写队列并返回 Err（调用方发 storage 降级事件），
    /// 后续调用先重试队列，保证顺序与 seq 单调。
    pub fn append_final(
        &mut self,
        source: &str,
        text: &str,
        start_ms: u64,
        end_ms: u64,
        speaker: Option<&str>,
    ) -> anyhow::Result<()> {
        let rec = SegmentRecord {
            seq: self.next_seq,
            source: source.into(),
            text: text.into(),
            start_ms,
            end_ms,
            speaker: speaker.map(String::from),
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
        // 兜底落盘说话人表：活动会话期间改名/归簇均只改内存 + 增量落盘，
        // 收尾时再确保磁盘与内存一致（失败不阻塞主流程，仅告警）。
        if !self.speakers.is_empty() {
            if let Err(e) = self.persist_speakers() {
                eprintln!("finalize: speakers.json 落盘失败（不阻塞收尾）: {e}");
            }
        }
        self.meta.ended_at = Some(now.to_rfc3339());
        self.meta.state = "complete".into();
        write_meta_atomic(&self.dir, &self.meta)
    }

    /// 从内存说话人表原子落盘 speakers.json（复用 write_speakers_atomic）。
    /// 供活动会话改名走单写者路径（rename_speaker command）与 finalize 兜底调用。
    pub fn persist_speakers(&self) -> anyhow::Result<()> {
        write_speakers_atomic(&self.dir, &self.speakers)
    }

    /// 合入声纹归簇产生的说话人信息：只增不删，已有名字保留，sources 取并集；
    /// 有实际变化时才原子写 speakers.json（避免无谓落盘）。
    pub fn sync_speakers(&mut self, infos: &[(String, Vec<String>)]) -> anyhow::Result<()> {
        let mut changed = false;
        for (id, sources) in infos {
            let entry = self.speakers.entry(id.clone()).or_insert_with(|| {
                changed = true;
                SpeakerMeta { name: String::new(), sources: Vec::new() }
            });
            for s in sources {
                if !entry.sources.contains(s) {
                    entry.sources.push(s.clone());
                    changed = true;
                }
            }
        }
        if changed {
            write_speakers_atomic(&self.dir, &self.speakers)?;
        }
        Ok(())
    }

    /// 合并两个说话人 id：loser 的段与 sources 归入 winner。
    /// 先 flush_pending 保证 jsonl 完整，再逐行重写 segments.jsonl
    /// （不可解析行原样保留，避免吞掉损坏但仍有诊断价值的行）到临时文件后原子替换；
    /// speakers 表移除 loser、sources 并入 winner（winner 已有名字保留，否则继承 loser 的名字），原子写。
    pub fn merge_speaker(&mut self, loser: &str, winner: &str) -> anyhow::Result<()> {
        self.flush_pending()?;

        let path = self.dir.join("segments.jsonl");
        // 读失败（瞬时 IO 错误）绝不能当空串：否则下方原子替换会把整场转写
        // 覆写成空文件，静默丢失全部内容。中止合并，内存 speakers 表此时未动。
        let content = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("读 segments.jsonl 失败（合并中止，避免清空）: {e}"))?;
        let mut out = String::new();
        for line in content.lines() {
            match serde_json::from_str::<SegmentRecord>(line) {
                Ok(mut rec) => {
                    if rec.speaker.as_deref() == Some(loser) {
                        rec.speaker = Some(winner.to_string());
                    }
                    out.push_str(&serde_json::to_string(&rec)?);
                }
                Err(_) => out.push_str(line), // 不可解析行原样保留
            }
            out.push('\n');
        }
        let tmp = self.dir.join("segments.jsonl.tmp");
        std::fs::write(&tmp, out)?;
        std::fs::rename(&tmp, &path)?;
        // 重写替换了 segments.jsonl 的磁盘文件，旧句柄仍指向被替换前的 inode；
        // 丢弃句柄，下次 flush_pending 会按新路径重开，避免写入"消失"的文件。
        self.file = None;

        if let Some(loser_meta) = self.speakers.remove(loser) {
            let winner_entry = self
                .speakers
                .entry(winner.to_string())
                .or_insert_with(|| SpeakerMeta { name: String::new(), sources: Vec::new() });
            if winner_entry.name.is_empty() && !loser_meta.name.is_empty() {
                winner_entry.name = loser_meta.name;
            }
            for s in loser_meta.sources {
                if !winner_entry.sources.contains(&s) {
                    winner_entry.sources.push(s);
                }
            }
        }
        write_speakers_atomic(&self.dir, &self.speakers)
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
        w.append_final("mic", "第一句", 0, 1500, None).unwrap();
        w.append_final("system", "second", 1500, 3000, None).unwrap();
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
        assert!(w.append_final("mic", "丢不得", 0, 1000, None).is_err());

        // 目录恢复后，下一次追加把队列里的段一并补写
        std::fs::create_dir_all(&dir).unwrap();
        w.append_final("mic", "第二句", 1000, 2000, None).unwrap();
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
        assert!(w.append_final("mic", "会丢失吗", 0, 1000, None).is_err());

        // 目录仍不存在：finalize 应失败，且不得把 state 标记为 complete
        // （此时磁盘上连 meta.json 都不存在，正是"不诚实的 complete"要避免的场景）。
        assert!(w.finalize(now()).is_err());

        // 目录恢复后：finalize 应能补写队列并把 meta 正常置 complete，
        // 验证"失败不置 complete、恢复后可补救"的语义。
        std::fs::create_dir_all(&dir).unwrap();
        w.append_final("mic", "第二句", 1000, 2000, None).unwrap();
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
            move |src, text, start_ms, end_ms, spk| {
                w2.lock()
                    .unwrap()
                    .append_final(src.as_str(), &text, start_ms, end_ms, spk.as_deref())
                    .unwrap();
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

    #[test]
    fn speakers_sync_merge_and_rewrite() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), now()).unwrap();
        let id = w.note_id().to_string();
        w.append_final("mic", "甲说", 0, 2000, Some("S1")).unwrap();
        w.append_final("system", "乙说", 2000, 4000, Some("S2")).unwrap();
        w.sync_speakers(&[("S1".into(), vec!["mic".into()]), ("S2".into(), vec!["system".into()])]).unwrap();
        // 合并 S2 → S1：jsonl 重写 + speakers 表收缩
        w.merge_speaker("S2", "S1").unwrap();
        w.finalize(now()).unwrap();

        let store = crate::store::NoteStore::new(tmp.path().to_path_buf());
        let note = store.load(&id).unwrap();
        assert!(note.segments.iter().all(|s| s.speaker.as_deref() == Some("S1")), "S2 段已重写为 S1");
        assert!(note.speakers.contains_key("S1"));
        assert!(!note.speakers.contains_key("S2"));
        assert!(note.speakers["S1"].sources.contains(&"system".to_string()), "sources 并入");
    }

    #[test]
    fn merge_speaker_read_failure_leaves_data_intact() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), now()).unwrap();
        w.append_final("mic", "甲说", 0, 2000, Some("S1")).unwrap();
        w.append_final("system", "乙说", 2000, 4000, Some("S2")).unwrap();
        w.sync_speakers(&[("S1".into(), vec!["mic".into()]), ("S2".into(), vec!["system".into()])]).unwrap();

        // 构造读失败：丢弃句柄、删掉 segments.jsonl 并在同名处建目录，
        // read_to_string 必失败（"Is a directory"）。
        let path = w.dir().join("segments.jsonl");
        w.file = None;
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();

        // 合并必须返回 Err 且不 panic；内存 speakers 表不得已被修改（S2 仍在）。
        assert!(w.merge_speaker("S2", "S1").is_err(), "读失败应中止合并而非清空");
        assert!(w.speakers().contains_key("S2"), "Err 路径下 speakers 表未被改动");
        assert!(w.speakers().contains_key("S1"));

        // 恢复（删目录）后不再触发路径存在的清空——重点是上面的 Err 与不 panic。
        std::fs::remove_dir(&path).unwrap();
    }

    #[test]
    fn persist_speakers_reloads_and_finalize_syncs() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), now()).unwrap();
        let id = w.note_id().to_string();
        w.append_final("mic", "甲说", 0, 2000, Some("S1")).unwrap();
        w.sync_speakers(&[("S1".into(), vec!["mic".into()])]).unwrap();

        // set_speaker_name + persist_speakers 后重开 NoteStore.load，名字应在磁盘上。
        w.set_speaker_name("S1", "张三");
        w.persist_speakers().unwrap();
        let store = crate::store::NoteStore::new(tmp.path().to_path_buf());
        assert_eq!(store.load(&id).unwrap().speakers["S1"].name, "张三");

        // 再改内存但不显式落盘；finalize 兜底应把磁盘同步到内存态。
        w.set_speaker_name("S1", "李四");
        w.finalize(now()).unwrap();
        let note = store.load(&id).unwrap();
        assert_eq!(note.speakers["S1"].name, "李四", "finalize 兜底落盘 speakers");
        assert_eq!(note.speakers, *w.speakers(), "speakers.json 与内存一致");
    }
}
