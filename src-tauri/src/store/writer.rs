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
    /// 续录时间轴偏移：resume 路径 = 上一场最大 end_ms，create 路径恒 0。
    /// on_final 落盘/emit 前 start_ms/end_ms 均需 + base_ms，保证时间轴连续。
    base_ms: u64,
    /// 本会话新建标记：create() 置 true，resume() 置 false。
    /// 用于 abort_or_finalize 区分：零段新建空笔记删除；零段既有笔记保留（不丢内容）。
    created_this_session: bool,
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
            base_ms: 0,
            created_this_session: true,
        })
    }

    /// 续录一场非活动（已中断或已完成）笔记：读 meta（缺失/损坏 → Err）→ 置
    /// state=recording、ended_at=None 原子写；扫 segments.jsonl 得 next_seq（最大
    /// 可解析 seq + 1，空文件/全不可解析 → 0）与 base_ms（最大可解析 end_ms，同上
    /// 兜底 0）——不可解析的尾行（如崩溃截断的半行）与 NoteStore::load 一致地容忍
    /// 跳过；加载 speakers.json（缺失 → 空表）；重开 append 句柄。
    pub fn resume(notes_dir: &Path, id: &str) -> anyhow::Result<Self> {
        super::validate_note_id(id)?;
        let dir = notes_dir.join(id);
        if !dir.is_dir() {
            anyhow::bail!("笔记不存在: {id}");
        }

        let meta_str = std::fs::read_to_string(dir.join("meta.json"))
            .map_err(|e| anyhow::anyhow!("读 meta.json 失败: {e}"))?;
        let mut meta: NoteMeta = serde_json::from_str(&meta_str)
            .map_err(|e| anyhow::anyhow!("meta.json 解析失败: {e}"))?;
        meta.state = "recording".into();
        meta.ended_at = None;
        write_meta_atomic(&dir, &meta)?;

        let content = std::fs::read_to_string(dir.join("segments.jsonl")).unwrap_or_default();
        let mut next_seq = 0u64;
        let mut base_ms = 0u64;
        for line in content.lines() {
            if let Ok(rec) = serde_json::from_str::<SegmentRecord>(line) {
                next_seq = next_seq.max(rec.seq + 1);
                base_ms = base_ms.max(rec.end_ms);
            }
        }

        let speakers: BTreeMap<String, SpeakerMeta> = std::fs::read_to_string(dir.join("speakers.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("segments.jsonl"))?;
        // 修复截断尾行：崩溃可能发生在写完段字节、写入换行符之前，留下无尾随换行
        // 的半行。若不修复，后续 append 会把新段字节直接拼接到这半行末尾，破坏其
        // 后每一行的 JSON 结构（半行本身仍按损坏行被 load 跳过，不受影响）。
        if !content.is_empty() && !content.ends_with('\n') {
            file.write_all(b"\n")?;
        }

        Ok(Self {
            dir,
            meta,
            file: Some(file),
            next_seq,
            pending: VecDeque::new(),
            speakers,
            base_ms,
            created_this_session: false,
        })
    }

    pub fn note_id(&self) -> &str {
        &self.meta.id
    }

    /// 续录时间轴偏移量（create 路径恒 0，resume 路径 = 续录前最大 end_ms）。
    pub fn base_ms(&self) -> u64 {
        self.base_ms
    }

    /// 是否已产生过任何定稿段（含仍在待写队列中的）。
    pub fn has_content(&self) -> bool {
        self.next_seq > 0
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 本会话新建标记：create() 置 true，resume() 置 false。
    /// 用于 abort_or_finalize 区分：零段新建空笔记删除；零段既有笔记保留（不丢内容）。
    pub fn created_this_session(&self) -> bool {
        self.created_this_session
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
            .or_insert_with(|| SpeakerMeta { name: String::new(), sources: Vec::new(), centroid: None, count: 0 })
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
        rms: Option<f32>,
    ) -> anyhow::Result<()> {
        let rec = SegmentRecord {
            seq: self.next_seq,
            source: source.into(),
            text: text.into(),
            start_ms,
            end_ms,
            speaker: speaker.map(String::from),
            rms,
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

    /// 合入 worker 结束时的质心快照(DiarEvent::Snapshot)：只 merge 质心/count 进
    /// 已有或新建表项，不落盘（由既有 finalize→persist_speakers 落）。
    /// 已有表项：只更新 centroid/count，不动 name/sources（sources 已由 sync_speakers 维护）。
    /// 新建表项：name 空串，sources 取快照。
    pub fn store_centroids(&mut self, snaps: &[crate::diar::registry::ClusterSnapshot]) {
        for s in snaps {
            match self.speakers.get_mut(&s.id) {
                Some(entry) => {
                    entry.centroid = Some(s.centroid.clone());
                    entry.count = s.count;
                }
                None => {
                    self.speakers.insert(
                        s.id.clone(),
                        SpeakerMeta {
                            name: String::new(),
                            sources: s.sources.iter().cloned().collect(),
                            centroid: Some(s.centroid.clone()),
                            count: s.count,
                        },
                    );
                }
            }
        }
    }

    /// 从内存说话人表构造 registry 快照，供续录时重建 SpeakerRegistry（编号续接、
    /// 质心延续）。消费者：lib.rs 的 spawn_session（New/Resume 均调用，New 路径
    /// 表为空 ⇒ 等价 SpeakerRegistry::new()）。
    ///
    /// 不过滤无质心的表项（P4.5 前的旧笔记、或曾因嵌入失败/降级而从未落过质心的
    /// 说话人）：这些项以 `centroid: Vec::new()` 输出，仍带着原 id。若在此过滤掉，
    /// `SpeakerRegistry::from_snapshot` 就看不到这些 id，编号会从 1 重来，续录时
    /// 新说话人被分配到旧 id 上，`sync_speakers` 就会把新人的段挂上旧人的名字
    /// （张冠李戴）。`from_snapshot` 按设计处理空质心项：只计入编号延续，不建簇。
    pub fn registry_snapshot(&self) -> Vec<crate::diar::registry::ClusterSnapshot> {
        self.speakers
            .iter()
            .map(|(id, m)| crate::diar::registry::ClusterSnapshot {
                id: id.clone(),
                centroid: m.centroid.clone().unwrap_or_default(),
                count: m.count,
                sources: m.sources.iter().cloned().collect(),
                // SpeakerMeta 暂未持有 person/total_ms(库联动是后续任务的事);
                // 这里先给中性默认值,续录编号/质心的既有行为不变。
                person: None,
                total_ms: 0,
            })
            .collect()
    }

    /// 合入声纹归簇产生的说话人信息：只增不删，已有名字保留，sources 取并集；
    /// 有实际变化时才原子写 speakers.json（避免无谓落盘）。
    pub fn sync_speakers(&mut self, infos: &[(String, Vec<String>)]) -> anyhow::Result<()> {
        let mut changed = false;
        for (id, sources) in infos {
            let entry = self.speakers.entry(id.clone()).or_insert_with(|| {
                changed = true;
                SpeakerMeta { name: String::new(), sources: Vec::new(), centroid: None, count: 0 }
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
                .or_insert_with(|| SpeakerMeta { name: String::new(), sources: Vec::new(), centroid: None, count: 0 });
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
        w.append_final("mic", "第一句", 0, 1500, None, None).unwrap();
        w.append_final("system", "second", 1500, 3000, None, None).unwrap();
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
        assert!(w.append_final("mic", "丢不得", 0, 1000, None, None).is_err());

        // 目录恢复后，下一次追加把队列里的段一并补写
        std::fs::create_dir_all(&dir).unwrap();
        w.append_final("mic", "第二句", 1000, 2000, None, None).unwrap();
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
        assert!(w.append_final("mic", "会丢失吗", 0, 1000, None, None).is_err());

        // 目录仍不存在：finalize 应失败，且不得把 state 标记为 complete
        // （此时磁盘上连 meta.json 都不存在，正是"不诚实的 complete"要避免的场景）。
        assert!(w.finalize(now()).is_err());

        // 目录恢复后：finalize 应能补写队列并把 meta 正常置 complete，
        // 验证"失败不置 complete、恢复后可补救"的语义。
        std::fs::create_dir_all(&dir).unwrap();
        w.append_final("mic", "第二句", 1000, 2000, None, None).unwrap();
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
                Ok(crate::asr::Transcript { text: format!("len={}", s.len()), ..Default::default() })
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
            crate::diar::registry::SpeakerRegistry::new(),
            std::time::Duration::from_millis(50), // 短 hold,单 Mic 源无回声可比对,值本身无关紧要
            16000,
            4000,
            move |src, text, start_ms, end_ms, spk, rms| {
                w2.lock()
                    .unwrap()
                    .append_final(src.as_str(), &text, start_ms, end_ms, spk.as_deref(), rms)
                    .unwrap();
                *e2.lock().unwrap() += 1;
            },
            |_, _| {},
            |_| {},
            None,
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
        w.append_final("mic", "甲说", 0, 2000, Some("S1"), None).unwrap();
        w.append_final("system", "乙说", 2000, 4000, Some("S2"), None).unwrap();
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
        w.append_final("mic", "甲说", 0, 2000, Some("S1"), None).unwrap();
        w.append_final("system", "乙说", 2000, 4000, Some("S2"), None).unwrap();
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
        w.append_final("mic", "甲说", 0, 2000, Some("S1"), None).unwrap();
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

    #[test]
    fn store_centroids_persists_and_old_format_speakers_json_still_parses() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), now()).unwrap();
        let id = w.note_id().to_string();
        w.sync_speakers(&[("S1".into(), vec!["mic".into()])]).unwrap();

        // store_centroids 只 merge 质心/count,不落盘;显式 persist_speakers 后重读应在。
        w.store_centroids(&[crate::diar::registry::ClusterSnapshot {
            id: "S1".into(),
            centroid: vec![0.1, 0.2, 0.3],
            count: 4,
            sources: std::collections::BTreeSet::from(["mic".to_string()]),
            person: None,
            total_ms: 0,
        }]);
        w.persist_speakers().unwrap();

        let store = crate::store::NoteStore::new(tmp.path().to_path_buf());
        let note = store.load(&id).unwrap();
        assert_eq!(note.speakers["S1"].centroid, Some(vec![0.1, 0.2, 0.3]));
        assert_eq!(note.speakers["S1"].count, 4);

        // 旧格式(P4 上线前产物,无 centroid/count 字段)speakers.json 仍可解析:
        // serde default 兜底为 None / 0。
        std::fs::write(
            w.dir().join("speakers.json"),
            r#"{"S9":{"name":"老王","sources":["mic"]}}"#,
        )
        .unwrap();
        let note2 = store.load(&id).unwrap();
        let s9 = &note2.speakers["S9"];
        assert_eq!(s9.name, "老王");
        assert_eq!(s9.centroid, None, "旧格式无 centroid 字段应兜底为 None");
        assert_eq!(s9.count, 0, "旧格式无 count 字段应兜底为 0");
    }

    #[test]
    fn store_centroids_creates_new_entry_with_empty_name_and_snapshot_sources() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), now()).unwrap();
        // S7 此前不在表中(未经 sync_speakers)——store_centroids 应新建表项。
        w.store_centroids(&[crate::diar::registry::ClusterSnapshot {
            id: "S7".into(),
            centroid: vec![1.0, 0.0],
            count: 2,
            sources: std::collections::BTreeSet::from(["system".to_string()]),
            person: None,
            total_ms: 0,
        }]);
        let s7 = &w.speakers()["S7"];
        assert_eq!(s7.name, "", "新建项 name 空串");
        assert_eq!(s7.sources, vec!["system".to_string()], "新建项 sources 取快照");
        assert_eq!(s7.centroid, Some(vec![1.0, 0.0]));
        assert_eq!(s7.count, 2);
    }

    #[test]
    fn store_centroids_existing_entry_only_merges_centroid_and_count() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), now()).unwrap();
        w.sync_speakers(&[("S1".into(), vec!["mic".into()])]).unwrap();
        w.set_speaker_name("S1", "张三");

        w.store_centroids(&[crate::diar::registry::ClusterSnapshot {
            id: "S1".into(),
            centroid: vec![0.5, 0.5],
            count: 9,
            sources: std::collections::BTreeSet::from(["system".to_string()]),
            person: None,
            total_ms: 0,
        }]);
        let s1 = &w.speakers()["S1"];
        assert_eq!(s1.name, "张三", "已有表项 name 不受影响");
        assert_eq!(s1.sources, vec!["mic".to_string()], "已有表项 sources 不受快照影响");
        assert_eq!(s1.centroid, Some(vec![0.5, 0.5]));
        assert_eq!(s1.count, 9);
    }

    #[test]
    fn registry_snapshot_keeps_entries_without_centroid_as_empty_centroid() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), now()).unwrap();
        w.sync_speakers(&[
            ("S1".into(), vec!["mic".into()]),
            ("S2".into(), vec!["system".into()]),
        ])
        .unwrap();
        w.store_centroids(&[crate::diar::registry::ClusterSnapshot {
            id: "S1".into(),
            centroid: vec![1.0, 0.0],
            count: 3,
            sources: std::collections::BTreeSet::from(["mic".to_string()]),
            person: None,
            total_ms: 0,
        }]);
        // S2 无质心：不应被过滤掉，须以空质心出现在快照中（否则编号会跳过 S2 的
        // 位置，续录时新说话人可能被分配到 S2 的旧 id 上）。
        let snaps = w.registry_snapshot();
        assert_eq!(snaps.len(), 2, "无质心项不应被过滤，仍计入快照");
        let s1 = snaps.iter().find(|s| s.id == "S1").unwrap();
        assert_eq!(s1.centroid, vec![1.0, 0.0]);
        assert_eq!(s1.count, 3);
        assert!(s1.sources.contains("mic"));
        let s2 = snaps.iter().find(|s| s.id == "S2").unwrap();
        assert!(s2.centroid.is_empty(), "无质心项应以空 centroid 输出");
    }

    /// 回归 P4.5 终审 Finding 1：P4.5 前的旧笔记（或曾降级会话）speakers 表里
    /// S1/S2 从未有过质心。续录时 registry_snapshot → from_snapshot 必须让编号
    /// 从 S3 续接，而不是从 S1 重来——否则新说话人会被分配到 S1/S2 的旧 id 上，
    /// sync_speakers 就会把新人的段挂上旧人的名字（张冠李戴）。
    #[test]
    fn registry_snapshot_roundtrip_continues_numbering_past_old_note_without_centroids() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), now()).unwrap();
        // 模拟旧笔记：S1/S2 只有 sync_speakers（无质心），从未 store_centroids。
        w.sync_speakers(&[
            ("S1".into(), vec!["mic".into()]),
            ("S2".into(), vec!["system".into()]),
        ])
        .unwrap();

        let snaps = w.registry_snapshot();
        let mut registry = crate::diar::registry::SpeakerRegistry::from_snapshot(&snaps);
        assert_eq!(registry.speakers().len(), 0, "空质心项不建簇");

        // 新说话人一段够长的音频：应分配 S3（编号续接），不撞 S1/S2。
        let long: Vec<f32> = {
            let mut v = vec![0.0f32; 3];
            v[0] = 1.0;
            v
        };
        let id = registry.assign(&long, "mic", 32000 /* 2s，够长建簇 */);
        assert_eq!(id, Some("S3".into()), "新说话人编号应续接旧笔记的最大 id，不撞旧 id");
    }

    #[test]
    fn create_path_base_ms_is_always_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let w = NoteWriter::create(tmp.path(), now()).unwrap();
        assert_eq!(w.base_ms(), 0);
    }

    #[test]
    fn create_marks_created_resume_does_not() {
        let tmp = tempfile::tempdir().unwrap();
        let w = NoteWriter::create(tmp.path(), now()).unwrap();
        let id = w.note_id().to_string();
        assert!(w.created_this_session(), "create() 应设 created_this_session=true");

        let r = NoteWriter::resume(tmp.path(), &id).unwrap();
        assert!(!r.created_this_session(), "resume() 应设 created_this_session=false");
    }

    #[test]
    fn resume_flips_meta_back_to_recording_continues_seq_and_base_ms_and_loads_speakers() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), now()).unwrap();
        let id = w.note_id().to_string();
        w.append_final("mic", "第一句", 0, 1500, None, None).unwrap();
        w.append_final("system", "第二句", 1500, 3000, None, None).unwrap();
        w.sync_speakers(&[("S1".into(), vec!["mic".into()])]).unwrap();
        w.finalize(now()).unwrap();
        assert_eq!(read_meta(&tmp.path().join(&id)).state, "complete");

        let mut r = NoteWriter::resume(tmp.path(), &id).unwrap();
        assert_eq!(r.note_id(), id, "续录复用同一 id/目录");
        assert_eq!(r.base_ms(), 3000, "base_ms = 续录前最大 end_ms");
        let meta = read_meta(r.dir());
        assert_eq!(meta.state, "recording", "resume 后 meta 翻回 recording");
        assert!(meta.ended_at.is_none(), "resume 后 ended_at 清空");
        assert!(r.speakers().contains_key("S1"), "speakers.json 应加载进内存表");

        // resume 后追加：seq 应从 2 续接（此前两段 seq=0,1）。
        r.append_final("mic", "第三句", 0, 1000, None, None).unwrap();
        let lines = read_lines(r.dir());
        assert_eq!(lines.len(), 3);
        let rec2: SegmentRecord = serde_json::from_str(&lines[2]).unwrap();
        assert_eq!(rec2.seq, 2, "resume 后追加的 seq 续接而非从 0 重来");
    }

    #[test]
    fn resume_of_never_finalized_recording_also_works() {
        // 续录语义不限于 complete：中断（仍是 recording 态）的笔记同样可续录。
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), now()).unwrap();
        let id = w.note_id().to_string();
        w.append_final("mic", "崩溃前", 0, 800, None, None).unwrap();
        // 不 finalize，模拟崩溃：meta 仍是 recording。

        let r = NoteWriter::resume(tmp.path(), &id).unwrap();
        assert_eq!(r.base_ms(), 800);
        let meta = read_meta(r.dir());
        assert_eq!(meta.state, "recording");
    }

    #[test]
    fn resume_tolerates_truncated_tail_line_for_next_seq_and_base_ms() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), now()).unwrap();
        let id = w.note_id().to_string();
        w.append_final("mic", "完整句", 0, 1000, None, None).unwrap();
        w.finalize(now()).unwrap();

        // 模拟崩溃写了半行（不可解析，next_seq/base_ms 应只依据可解析行）。
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(tmp.path().join(&id).join("segments.jsonl"))
            .unwrap();
        f.write_all(b"{\"seq\":1,\"source\":\"mic\",\"te").unwrap();
        drop(f);

        let mut r = NoteWriter::resume(tmp.path(), &id).unwrap();
        assert_eq!(r.base_ms(), 1000, "损坏尾行应被跳过，base_ms 取最大可解析 end_ms");
        r.append_final("mic", "续录句", 0, 500, None, None).unwrap();
        // 续录追加的段 seq 应为 1（唯一可解析的前段 seq=0）——证明 next_seq 未被半行干扰到 2。
        let lines = read_lines(r.dir());
        let appended: SegmentRecord = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(appended.seq, 1, "next_seq 应据可解析行计算，不被截断尾行带偏");
    }

    #[test]
    fn resume_missing_id_returns_err() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(NoteWriter::resume(tmp.path(), "does-not-exist").is_err());
    }

    #[test]
    fn resume_corrupt_meta_returns_err() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), now()).unwrap();
        let id = w.note_id().to_string();
        w.finalize(now()).unwrap();
        std::fs::write(tmp.path().join(&id).join("meta.json"), "not json").unwrap();
        assert!(NoteWriter::resume(tmp.path(), &id).is_err());
    }

    #[test]
    fn resume_rejects_path_traversal_ids() {
        let tmp = tempfile::tempdir().unwrap();
        for bad in ["../x", "a/b", "a\\b", "..", ""] {
            assert!(NoteWriter::resume(tmp.path(), bad).is_err(), "应拒绝非法 id: {bad}");
        }
    }

    #[test]
    fn append_final_persists_rms_and_old_lines_tolerated() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), chrono::Local::now()).unwrap();
        let id = w.note_id().to_string();
        w.append_final("mic", "有能量", 0, 900, None, Some(0.123)).unwrap();
        w.append_final("mic", "无能量数据", 1000, 1900, None, None).unwrap();
        w.finalize(chrono::Local::now()).unwrap();
        let store = crate::store::NoteStore::new(tmp.path().to_path_buf());
        let n = store.load(&id).unwrap();
        assert_eq!(n.segments[0].rms, Some(0.123));
        assert_eq!(n.segments[1].rms, None);
        // None 不序列化该键(旧行等价形状,双向兼容)
        let raw = std::fs::read_to_string(tmp.path().join(&id).join("segments.jsonl")).unwrap();
        assert!(raw.lines().next().unwrap().contains("\"rms\""));
        assert!(!raw.lines().nth(1).unwrap().contains("\"rms\""));
    }

    /// 集成测试（仿 full_session_persists_every_final）：第一场会话落 N 段 →
    /// finalize → resume + 新会话再落 M 段（on_final 中 + base_ms，模拟 lib.rs
    /// spawn_session 的偏移逻辑）→ load 出 N+M 段、seq 单调、后 M 段 start_ms ≥ base_ms。
    #[test]
    fn resume_session_continues_seq_and_offsets_timestamps() {
        use crate::audio::mock::MockCapture;
        use crate::audio::{AudioCapture, Source};
        use crate::diar::registry::SpeakerRegistry;
        use crate::pipeline::segmenter::{MockSegmenter, Segmenter};
        use crate::store::NoteStore;
        use std::sync::{Arc, Mutex};

        struct CountingRecognizer;
        impl crate::asr::Recognizer for CountingRecognizer {
            fn recognize(&mut self, s: &[f32]) -> anyhow::Result<crate::asr::Transcript> {
                Ok(crate::asr::Transcript { text: format!("len={}", s.len()), ..Default::default() })
            }
        }

        fn fixture_sources() -> Vec<(Source, Box<dyn AudioCapture>, Box<dyn Segmenter>)> {
            let cap = MockCapture::from_wav(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/sample_16k.wav"
            ))
            .expect("fixture");
            vec![(Source::Mic, Box::new(cap), Box::new(MockSegmenter::new(2000)))]
        }

        let tmp = tempfile::tempdir().unwrap();

        // 第一场会话：落 N 段。
        let writer = Arc::new(Mutex::new(NoteWriter::create(tmp.path(), now()).unwrap()));
        let id = writer.lock().unwrap().note_id().to_string();
        let w2 = writer.clone();
        let start = crate::session::start_session(
            fixture_sources(),
            Box::new(CountingRecognizer),
            None,
            SpeakerRegistry::new(),
            std::time::Duration::from_millis(50), // 短 hold,单 Mic 源无回声可比对,值本身无关紧要
            16000,
            4000,
            move |src, text, start_ms, end_ms, spk, rms| {
                w2.lock()
                    .unwrap()
                    .append_final(src.as_str(), &text, start_ms, end_ms, spk.as_deref(), rms)
                    .unwrap();
            },
            |_, _| {},
            |_| {},
            None,
        )
        .expect("start_session");
        let _ = start.handle.stop();
        writer.lock().unwrap().finalize(now()).unwrap();

        let store = NoteStore::new(tmp.path().to_path_buf());
        let n = store.load(&id).unwrap().segments.len();
        assert!(n > 0, "第一场应产出至少一段");

        // 续录：第二场会话，落 M 段（时间戳按 base_ms 偏移，仿 spawn_session 的 on_final）。
        let resumed = NoteWriter::resume(tmp.path(), &id).unwrap();
        let base_ms = resumed.base_ms();
        assert!(base_ms > 0, "续录前应已有非零 end_ms");
        let writer2 = Arc::new(Mutex::new(resumed));
        let w3 = writer2.clone();
        let start2 = crate::session::start_session(
            fixture_sources(),
            Box::new(CountingRecognizer),
            None,
            SpeakerRegistry::from_snapshot(&writer2.lock().unwrap().registry_snapshot()),
            std::time::Duration::from_millis(50), // 短 hold,单 Mic 源无回声可比对,值本身无关紧要
            16000,
            4000,
            move |src, text, start_ms, end_ms, spk, rms| {
                w3.lock()
                    .unwrap()
                    .append_final(src.as_str(), &text, start_ms + base_ms, end_ms + base_ms, spk.as_deref(), rms)
                    .unwrap();
            },
            |_, _| {},
            |_| {},
            None,
        )
        .expect("start_session (resumed)");
        let _ = start2.handle.stop();
        writer2.lock().unwrap().finalize(now()).unwrap();

        let note = store.load(&id).unwrap();
        assert_eq!(note.segments.len(), n * 2, "N+M 段一段不丢（同一 fixture，M=N）");
        assert!(
            note.segments.windows(2).all(|w| w[1].seq == w[0].seq + 1),
            "seq 跨会话仍单调续接"
        );
        assert!(
            note.segments[n..].iter().all(|s| s.start_ms >= base_ms),
            "续录段 start_ms 均 ≥ base_ms（时间轴连续）"
        );
        assert_eq!(note.meta.state, "complete", "续录后 stop 仍正常收尾为 complete");
    }
}
