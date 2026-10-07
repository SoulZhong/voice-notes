//! 听写笔记的落盘(CONTEXT.md「设备听写」:听写笔记 / 听写记录)。
//!
//! 往同一个目标(会话)说过的全部听写归成一篇听写笔记,跨天持续追加。目录:
//!
//! ```text
//! <data_root>/dictations/<id>/meta.json      目标键、应用名、会话标签、创建/最后听写时间
//!                          /records.jsonl    逐句记录(撤销只改标记,整文件原子重写)
//!                          /audio/<rid>.wav  可选的原始音频(16 kHz 单声道,保留 30 天)
//! ```
//!
//! id 由目标键哈希而来,同一会话恒落同一篇,不需要索引文件。与会议笔记(notes/)
//! 完全分开:没有说话人、修订稿、Aing,也不参与声纹。

use anyhow::Context;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// 存下的音频保留天数(用户拍板 2026-10-07)。到期只删音频,文字永留。
pub const AUDIO_RETENTION_DAYS: i64 = 30;
/// 列表里供前端过滤的文字上限(字符数),最近的句子优先。
const SEARCH_TEXT_MAX: usize = 20_000;
/// 前端列表一行的预览长度(字符数)。
const PREVIEW_MAX: usize = 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DictationMeta {
    pub id: String,
    /// 目标键(见 vibe_device::session::RecordTarget::key)。
    pub key: String,
    /// 应用名(Orca / 微信 / ChatGPT / 企业微信)。
    pub app: String,
    /// 会话标签;同一键下标签变了(如 Orca 终端改了标题)取最新。
    pub label: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DictationRecord {
    /// 记录 id(毫秒时间戳 + 设备听写序号,同篇内唯一)。
    pub rid: String,
    pub at: String,
    pub text: String,
    /// 插入后在设备上按了撤销。
    #[serde(default)]
    pub undone: bool,
    /// 音频文件名(audio/ 下);None = 没存或已过保留期。
    #[serde(default)]
    pub audio: Option<String>,
    #[serde(default)]
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DictationNote {
    pub meta: DictationMeta,
    pub records: Vec<DictationRecord>,
    /// 音频目录的绝对路径(前端拼 asset URL 播放)。
    pub audio_dir: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DictationSummary {
    pub id: String,
    pub app: String,
    pub label: String,
    pub created_at: String,
    pub updated_at: String,
    /// 未撤销的句数。
    pub count: usize,
    /// 最近一句(未撤销)。
    pub preview: String,
    /// 标签 + 各句文字(新句在前,截断),供侧栏过滤。
    pub search_text: String,
}

/// 一次落盘的听写:写到哪篇(id)、哪条(rid)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub id: String,
    pub rid: String,
}

/// 要保存的一句听写。
pub struct NewRecord<'a> {
    pub key: &'a str,
    pub app: &'a str,
    pub label: &'a str,
    pub dict: u8,
    pub text: &'a str,
    /// 16 kHz 单声道 PCM;None = 不存音频。
    pub pcm: Option<&'a [i16]>,
}

pub struct DictationStore {
    root: PathBuf,
}

/// 目标键 → 听写笔记 id。前缀 "d" 与会议笔记的时间戳 id 天然不撞。
pub fn note_id(key: &str) -> String {
    let h = Sha256::digest(key.as_bytes());
    let hex: String = h.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("d{hex}")
}

/// 是否听写笔记 id(MCP get_note 据此分流)。
pub fn is_note_id(id: &str) -> bool {
    valid_id(id)
}

/// id 只允许 d + 十六进制(防路径穿越:id 来自前端)。
fn valid_id(id: &str) -> bool {
    id.len() == 17 && id.starts_with('d') && id[1..].chars().all(|c| c.is_ascii_hexdigit())
}

fn write_atomic(path: &Path, body: &[u8]) -> anyhow::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn preview(text: &str) -> String {
    let t = text.trim();
    if t.chars().count() <= PREVIEW_MAX {
        t.to_owned()
    } else {
        let mut s: String = t.chars().take(PREVIEW_MAX - 1).collect();
        s.push('…');
        s
    }
}

impl DictationStore {
    /// `data_root` 是 Voice Notes 的数据根(与 notes/ 同级)。
    pub fn new(data_root: &Path) -> Self {
        Self { root: data_root.join("dictations") }
    }

    fn dir(&self, id: &str) -> anyhow::Result<PathBuf> {
        anyhow::ensure!(valid_id(id), "无效的听写笔记 id: {id}");
        Ok(self.root.join(id))
    }

    fn load_meta(&self, id: &str) -> anyhow::Result<DictationMeta> {
        let p = self.dir(id)?.join("meta.json");
        let text = std::fs::read_to_string(&p).with_context(|| format!("读 {}", p.display()))?;
        Ok(serde_json::from_str(&text)?)
    }

    fn save_meta(&self, meta: &DictationMeta) -> anyhow::Result<()> {
        let dir = self.dir(&meta.id)?;
        std::fs::create_dir_all(&dir)?;
        write_atomic(&dir.join("meta.json"), &serde_json::to_vec_pretty(meta)?)
    }

    fn load_records(&self, id: &str) -> anyhow::Result<Vec<DictationRecord>> {
        let p = self.dir(id)?.join("records.jsonl");
        let text = match std::fs::read_to_string(&p) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        // 坏行(异常退出写了半行)跳过,不让一行拖垮整篇。
        Ok(text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect())
    }

    fn save_records(&self, id: &str, records: &[DictationRecord]) -> anyhow::Result<()> {
        let mut body = String::new();
        for r in records {
            body.push_str(&serde_json::to_string(r)?);
            body.push('\n');
        }
        write_atomic(&self.dir(id)?.join("records.jsonl"), body.as_bytes())
    }

    /// 追加一句听写(建篇或续写),可附音频。
    pub fn append(&self, rec: NewRecord<'_>, now: chrono::DateTime<chrono::Local>) -> anyhow::Result<Saved> {
        let id = note_id(rec.key);
        let at = now.to_rfc3339();
        let mut meta = match self.load_meta(&id) {
            Ok(m) => m,
            Err(_) => DictationMeta {
                id: id.clone(),
                key: rec.key.to_owned(),
                app: rec.app.to_owned(),
                label: rec.label.to_owned(),
                created_at: at.clone(),
                updated_at: at.clone(),
            },
        };
        meta.app = rec.app.to_owned();
        if !rec.label.trim().is_empty() {
            meta.label = rec.label.to_owned();
        }
        meta.updated_at = at.clone();
        self.save_meta(&meta)?;

        let rid = format!("{}-{}", now.timestamp_millis(), rec.dict);
        let dir = self.dir(&id)?;
        let (audio, duration_ms) = match rec.pcm.filter(|p| !p.is_empty()) {
            Some(pcm) => {
                let name = format!("{rid}.wav");
                let audio_dir = dir.join("audio");
                std::fs::create_dir_all(&audio_dir)?;
                write_wav(&audio_dir.join(&name), pcm)?;
                (Some(name), pcm.len() as u64 * 1000 / 16_000)
            }
            None => (None, 0),
        };
        let record = DictationRecord {
            rid: rid.clone(),
            at,
            text: rec.text.to_owned(),
            undone: false,
            audio,
            duration_ms,
        };
        // 追加写一行即可;撤销/删除才整文件重写。
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("records.jsonl"))?;
        writeln!(f, "{}", serde_json::to_string(&record)?)?;
        Ok(Saved { id, rid })
    }

    /// 设备上撤销了这句。
    pub fn mark_undone(&self, id: &str, rid: &str) -> anyhow::Result<()> {
        let mut records = self.load_records(id)?;
        let Some(r) = records.iter_mut().find(|r| r.rid == rid) else {
            return Ok(());
        };
        r.undone = true;
        self.save_records(id, &records)
    }

    pub fn load(&self, id: &str) -> anyhow::Result<DictationNote> {
        let dir = self.dir(id)?;
        Ok(DictationNote {
            meta: self.load_meta(id)?,
            records: self.load_records(id)?,
            audio_dir: dir.join("audio").to_string_lossy().into_owned(),
        })
    }

    /// 全部听写笔记,最近听写的在前。读不了的篇跳过。
    pub fn list(&self) -> Vec<DictationSummary> {
        let Ok(rd) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut out: Vec<DictationSummary> = rd
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_owned))
            .filter(|id| valid_id(id))
            .filter_map(|id| {
                let meta = self.load_meta(&id).ok()?;
                let records = self.load_records(&id).unwrap_or_default();
                let live: Vec<&DictationRecord> = records.iter().filter(|r| !r.undone).collect();
                let mut search_text = meta.label.clone();
                for r in live.iter().rev() {
                    if search_text.chars().count() >= SEARCH_TEXT_MAX {
                        break;
                    }
                    search_text.push('\n');
                    search_text.push_str(&r.text);
                }
                Some(DictationSummary {
                    preview: live.last().map(|r| preview(&r.text)).unwrap_or_default(),
                    count: live.len(),
                    id: meta.id,
                    app: meta.app,
                    label: meta.label,
                    created_at: meta.created_at,
                    updated_at: meta.updated_at,
                    search_text,
                })
            })
            .collect();
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        out
    }

    pub fn delete_note(&self, id: &str) -> anyhow::Result<()> {
        let dir = self.dir(id)?;
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }

    /// 删一句(连同音频);删到一句不剩时整篇删掉。
    pub fn delete_record(&self, id: &str, rid: &str) -> anyhow::Result<()> {
        let mut records = self.load_records(id)?;
        let Some(i) = records.iter().position(|r| r.rid == rid) else {
            return Ok(());
        };
        let r = records.remove(i);
        if let Some(name) = r.audio {
            let _ = std::fs::remove_file(self.dir(id)?.join("audio").join(name));
        }
        if records.is_empty() {
            return self.delete_note(id);
        }
        self.save_records(id, &records)
    }

    /// 删除早于 `cutoff` 的音频(文字保留)。返回删掉的文件数。
    pub fn purge_audio(&self, cutoff: chrono::DateTime<chrono::Local>) -> usize {
        let Ok(rd) = std::fs::read_dir(&self.root) else {
            return 0;
        };
        let mut purged = 0;
        for id in rd.flatten().filter_map(|e| e.file_name().to_str().map(str::to_owned)) {
            if !valid_id(&id) {
                continue;
            }
            let Ok(mut records) = self.load_records(&id) else {
                continue;
            };
            let mut changed = false;
            for r in records.iter_mut() {
                let old = chrono::DateTime::parse_from_rfc3339(&r.at).is_ok_and(|t| t < cutoff);
                if !old {
                    continue;
                }
                if let Some(name) = r.audio.take() {
                    if let Ok(dir) = self.dir(&id) {
                        let _ = std::fs::remove_file(dir.join("audio").join(name));
                    }
                    purged += 1;
                    changed = true;
                }
            }
            if changed {
                let _ = self.save_records(&id, &records);
            }
        }
        purged
    }
}

fn write_wav(path: &Path, pcm: &[i16]) -> anyhow::Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 16_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec)?;
    for s in pcm {
        w.write_sample(*s)?;
    }
    w.finalize()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32, m: u32) -> chrono::DateTime<chrono::Local> {
        chrono::Local.with_ymd_and_hms(2026, 10, 7, h, m, 0).unwrap()
    }

    fn rec<'a>(key: &'a str, label: &'a str, dict: u8, text: &'a str) -> NewRecord<'a> {
        NewRecord { key, app: "Orca", label, dict, text, pcm: None }
    }

    #[test]
    fn same_target_appends_to_one_note_across_days() {
        let tmp = tempfile::tempdir().unwrap();
        let s = DictationStore::new(tmp.path());
        let a = s.append(rec("orca:leaf1", "repo · claude", 1, "第一句。"), at(9, 0)).unwrap();
        let tomorrow = chrono::Local.with_ymd_and_hms(2026, 10, 8, 9, 0, 0).unwrap();
        let b = s.append(rec("orca:leaf1", "repo · claude", 2, "第二句。"), tomorrow).unwrap();
        assert_eq!(a.id, b.id);
        let note = s.load(&a.id).unwrap();
        assert_eq!(note.records.len(), 2);
        assert_eq!(note.meta.created_at, at(9, 0).to_rfc3339());
        assert_eq!(note.meta.updated_at, tomorrow.to_rfc3339());
    }

    #[test]
    fn different_targets_make_different_notes_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let s = DictationStore::new(tmp.path());
        s.append(rec("orca:a", "A", 1, "甲"), at(9, 0)).unwrap();
        s.append(rec("app:com.tencent.xinWeChat:张三", "张三", 2, "乙"), at(10, 0)).unwrap();
        let list = s.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].label, "张三");
        assert_eq!(list[1].label, "A");
    }

    #[test]
    fn label_follows_the_latest_title() {
        let tmp = tempfile::tempdir().unwrap();
        let s = DictationStore::new(tmp.path());
        s.append(rec("orca:a", "repo · old", 1, "x"), at(9, 0)).unwrap();
        s.append(rec("orca:a", "repo · new", 2, "y"), at(9, 1)).unwrap();
        assert_eq!(s.list()[0].label, "repo · new");
    }

    #[test]
    fn undone_records_stay_but_do_not_count() {
        let tmp = tempfile::tempdir().unwrap();
        let s = DictationStore::new(tmp.path());
        s.append(rec("orca:a", "A", 1, "保留"), at(9, 0)).unwrap();
        let b = s.append(rec("orca:a", "A", 2, "撤销了"), at(9, 1)).unwrap();
        s.mark_undone(&b.id, &b.rid).unwrap();
        let note = s.load(&b.id).unwrap();
        assert_eq!(note.records.len(), 2);
        assert!(note.records[1].undone);
        let sum = &s.list()[0];
        assert_eq!(sum.count, 1);
        assert_eq!(sum.preview, "保留");
        assert!(!sum.search_text.contains("撤销了"));
    }

    #[test]
    fn audio_is_saved_and_purged_after_retention() {
        let tmp = tempfile::tempdir().unwrap();
        let s = DictationStore::new(tmp.path());
        let pcm = vec![100i16; 16_000];
        let mut r = rec("orca:a", "A", 1, "有音频");
        r.pcm = Some(&pcm);
        let saved = s.append(r, at(9, 0)).unwrap();
        let note = s.load(&saved.id).unwrap();
        let name = note.records[0].audio.clone().unwrap();
        assert_eq!(note.records[0].duration_ms, 1000);
        let wav = Path::new(&note.audio_dir).join(&name);
        assert!(wav.exists());
        // 保留期内不动
        assert_eq!(s.purge_audio(at(8, 0)), 0);
        assert!(wav.exists());
        // 过期:删音频,文字留
        assert_eq!(s.purge_audio(at(9, 30)), 1);
        assert!(!wav.exists());
        let note = s.load(&saved.id).unwrap();
        assert_eq!(note.records[0].audio, None);
        assert_eq!(note.records[0].text, "有音频");
    }

    #[test]
    fn deleting_the_last_record_removes_the_note() {
        let tmp = tempfile::tempdir().unwrap();
        let s = DictationStore::new(tmp.path());
        let a = s.append(rec("orca:a", "A", 1, "一"), at(9, 0)).unwrap();
        let b = s.append(rec("orca:a", "A", 2, "二"), at(9, 1)).unwrap();
        s.delete_record(&a.id, &a.rid).unwrap();
        assert_eq!(s.load(&a.id).unwrap().records.len(), 1);
        s.delete_record(&b.id, &b.rid).unwrap();
        assert!(s.list().is_empty());
    }

    #[test]
    fn ids_from_outside_are_validated() {
        let tmp = tempfile::tempdir().unwrap();
        let s = DictationStore::new(tmp.path());
        assert!(s.load("../notes").is_err());
        assert!(s.delete_note("d../../x").is_err());
        assert!(valid_id(&note_id("anything")));
    }

    #[test]
    fn a_torn_line_does_not_lose_the_note() {
        let tmp = tempfile::tempdir().unwrap();
        let s = DictationStore::new(tmp.path());
        let a = s.append(rec("orca:a", "A", 1, "完整"), at(9, 0)).unwrap();
        let p = tmp.path().join("dictations").join(&a.id).join("records.jsonl");
        let mut body = std::fs::read_to_string(&p).unwrap();
        body.push_str("{\"rid\":\"broken");
        std::fs::write(&p, body).unwrap();
        assert_eq!(s.load(&a.id).unwrap().records.len(), 1);
    }
}
