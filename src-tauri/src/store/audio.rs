//! 笔记音频落盘(16kHz 单声道 s16le WAV,每源一个文件)与轨道枚举。
//! 设计见 docs/superpowers/specs/2026-07-05-voice-notes-audio-retention-playback-design.md。
//!
//! 对齐不变式:写入的样本与 segment_worker 喂给 segmenter 的样本严格同源(同一路
//! 重采样流、同在暂停闸之后),因此「文件内毫秒 + offset_ms == 段时间轴毫秒」按构造
//! 成立,播放跟随高亮无需任何对时逻辑。
//!
//! 音频是增值层:本模块任何失败都只降级(eprintln/停写),绝不影响转写落盘。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// 固定录制格式:16kHz 单声道 s16le。
pub const AUDIO_SAMPLE_RATE: u32 = 16_000;
const BYTES_PER_SAMPLE: u64 = 2;
const HEADER_LEN: u64 = 44;
/// 追加多少样本后刷盘并回写头部尺寸(1s):任意时刻文件都是合法 WAV,崩溃最多丢约 1s。
const FLUSH_INTERVAL_SAMPLES: u64 = AUDIO_SAMPLE_RATE as u64;

fn ms_to_bytes(ms: u64) -> u64 {
    ms * AUDIO_SAMPLE_RATE as u64 / 1000 * BYTES_PER_SAMPLE
}

fn bytes_to_ms(bytes: u64) -> u64 {
    bytes / BYTES_PER_SAMPLE * 1000 / AUDIO_SAMPLE_RATE as u64
}

/// audio.json:各轨道 0 时刻对应笔记时间轴的毫秒。轨道可中途出现(续录旧笔记、
/// 某源第二场才授权成功),offset_ms 记录它出现时的 base_ms。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AudioMeta {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub tracks: BTreeMap<String, TrackMeta>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrackMeta {
    #[serde(default)]
    pub offset_ms: u64,
}

/// 缺失/损坏 → 默认空表(全 0 offset 由 tracks 缺项兜底),不 Err:与本仓损坏容忍哲学一致。
pub fn load_audio_meta(note_dir: &Path) -> AudioMeta {
    std::fs::read_to_string(note_dir.join("audio.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_audio_meta(note_dir: &Path, meta: &AudioMeta) -> anyhow::Result<()> {
    let tmp = note_dir.join("audio.json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(meta)?)?;
    std::fs::rename(&tmp, note_dir.join("audio.json"))?;
    Ok(())
}

/// 44 字节标准 PCM WAV 头。data_len 为 data 块字节数。
fn wav_header(data_len: u32) -> [u8; HEADER_LEN as usize] {
    let mut h = [0u8; HEADER_LEN as usize];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36u32.wrapping_add(data_len)).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes()); // fmt 块长
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    h[22..24].copy_from_slice(&1u16.to_le_bytes()); // 单声道
    h[24..28].copy_from_slice(&AUDIO_SAMPLE_RATE.to_le_bytes());
    h[28..32].copy_from_slice(&(AUDIO_SAMPLE_RATE * 2).to_le_bytes()); // 字节率
    h[32..34].copy_from_slice(&2u16.to_le_bytes()); // 块对齐
    h[34..36].copy_from_slice(&16u16.to_le_bytes()); // 位深
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_len.to_le_bytes());
    h
}

/// 按实际文件长度回写 RIFF/data 尺寸(崩溃恢复:头是按刷盘节奏回写的,硬崩后可能
/// 落后于实际数据)。文件短于头长视为损坏,重写为空 WAV 头。
pub fn repair_wav_header(path: &Path) -> anyhow::Result<()> {
    let mut f = OpenOptions::new().read(true).write(true).open(path)?;
    let len = f.metadata()?.len();
    let data_len = len.saturating_sub(HEADER_LEN);
    // data 块必须是整样本:崩溃可能留半个样本的尾巴,truncate 掉。
    let data_len = data_len - data_len % BYTES_PER_SAMPLE;
    f.set_len(HEADER_LEN + data_len)?;
    f.seek(SeekFrom::Start(0))?;
    f.write_all(&wav_header(data_len as u32))?;
    f.flush()?;
    Ok(())
}

/// 单轨道追加写。**惰性建档**:构造无 IO,首次 append 才建/开文件——源启动失败或
/// 全程无帧就不留空轨道(也避免它在下一场续录时被零填充成大段静音)。首个写入样本
/// 恰是本场样本钟的 0 点,故新建轨道 offset_ms = base_ms 严格成立。
/// append 攒够 1s 刷盘回写头,Drop 兜底收尾;任何失败 eprintln 后永久停写
/// (增值层降级,绝不拖垮转写)。
pub struct AudioTrackWriter {
    note_dir: PathBuf,
    source: String,
    base_ms: u64,
    state: TrackState,
}

enum TrackState {
    Pending,
    Open {
        file: File,
        path: PathBuf,
        /// data 块当前字节数(含未刷盘部分)。
        data_len: u64,
        /// 距上次刷盘/回写头以来新增的样本数。
        since_flush: u64,
        buf: Vec<u8>,
    },
    Failed,
}

impl AudioTrackWriter {
    /// 无 IO 构造;真正建档在首次 append。
    pub fn new(note_dir: &Path, source: &str, base_ms: u64) -> Self {
        Self {
            note_dir: note_dir.to_path_buf(),
            source: source.to_string(),
            base_ms,
            state: TrackState::Pending,
        }
    }

    /// 建/开 note_dir 下 `<source>.wav` 并使其尾部对齐 base_ms:
    /// - 不存在:写空头,audio.json 记 offset_ms = base_ms;
    /// - 已存在:修复陈旧头,再 set_len 到 (base_ms - offset_ms) 对应字节
    ///   (截掉上场末尾静音/被丢段,不足则零填充)——续录新音频落位即对齐。
    fn open(&self) -> anyhow::Result<(File, PathBuf, u64)> {
        let path = self.note_dir.join(format!("{}.wav", self.source));
        let mut meta = load_audio_meta(&self.note_dir);
        if path.exists() {
            repair_wav_header(&path)?;
            let offset_ms = meta.tracks.get(&self.source).map(|t| t.offset_ms).unwrap_or(0);
            // base_ms 只增不减且轨道创建时 offset = 当时的 base,故差值非负;防御 saturating。
            let target = ms_to_bytes(self.base_ms.saturating_sub(offset_ms));
            let mut f = OpenOptions::new().read(true).write(true).open(&path)?;
            f.set_len(HEADER_LEN + target)?; // 双向:超长截断,不足零填充
            f.seek(SeekFrom::Start(0))?;
            f.write_all(&wav_header(target as u32))?;
            f.seek(SeekFrom::End(0))?;
            Ok((f, path, target))
        } else {
            let mut f = OpenOptions::new().create_new(true).read(true).write(true).open(&path)?;
            f.write_all(&wav_header(0))?;
            meta.schema_version = 1;
            meta.tracks.insert(self.source.clone(), TrackMeta { offset_ms: self.base_ms });
            save_audio_meta(&self.note_dir, &meta)?;
            Ok((f, path, 0))
        }
    }

    /// 追加一批 f32 样本(clamp 到 [-1,1] 转 s16le)。失败 eprintln 一次后永久停写。
    pub fn append(&mut self, samples: &[f32]) {
        if samples.is_empty() {
            return;
        }
        if matches!(self.state, TrackState::Pending) {
            match self.open() {
                Ok((file, path, data_len)) => {
                    self.state = TrackState::Open { file, path, data_len, since_flush: 0, buf: Vec::new() };
                }
                Err(e) => {
                    eprintln!("音频轨道建档失败,本场 {} 不保留音频: {e}", self.source);
                    self.state = TrackState::Failed;
                    return;
                }
            }
        }
        let TrackState::Open { file, path, data_len, since_flush, buf } = &mut self.state else {
            return;
        };
        buf.clear();
        buf.reserve(samples.len() * 2);
        for s in samples {
            let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
            buf.extend_from_slice(&v.to_le_bytes());
        }
        if let Err(e) = file.write_all(buf) {
            eprintln!("音频落盘失败,本轨道停写({}): {e}", path.display());
            self.state = TrackState::Failed;
            return;
        }
        *data_len += buf.len() as u64;
        *since_flush += samples.len() as u64;
        if *since_flush >= FLUSH_INTERVAL_SAMPLES {
            self.flush_header();
        }
    }

    /// 回写头部尺寸并刷盘,失败即停写。
    fn flush_header(&mut self) {
        let TrackState::Open { file, path, data_len, since_flush, .. } = &mut self.state else {
            return;
        };
        *since_flush = 0;
        let header = wav_header(*data_len as u32);
        let res = (|| -> std::io::Result<()> {
            file.seek(SeekFrom::Start(0))?;
            file.write_all(&header)?;
            file.seek(SeekFrom::End(0))?;
            file.flush()
        })();
        if let Err(e) = res {
            eprintln!("音频头回写失败,本轨道停写({}): {e}", path.display());
            self.state = TrackState::Failed;
        }
    }
}

impl Drop for AudioTrackWriter {
    /// 兜底收尾:worker 任何退出路径都补头+刷盘。Pending/Failed 无事可做。
    fn drop(&mut self) {
        self.flush_header();
    }
}

/// 详情页轨道枚举:扫 audio.json 已知源 + 磁盘上的 {mic,system}.wav 并集。
/// duration 按实际文件长度算(头可能陈旧,播放端修复另走 repair)。
#[derive(Debug, Clone, Serialize)]
pub struct TrackInfo {
    pub source: String,
    pub path: String,
    pub offset_ms: u64,
    pub duration_ms: u64,
}

pub fn list_tracks(note_dir: &Path) -> Vec<TrackInfo> {
    let meta = load_audio_meta(note_dir);
    let mut out = Vec::new();
    for source in ["mic", "system"] {
        let path = note_dir.join(format!("{source}.wav"));
        let Ok(md) = std::fs::metadata(&path) else { continue };
        if md.len() <= HEADER_LEN {
            continue; // 空轨道(刚建头没内容/损坏残留)不给前端,免得渲染空播放器
        }
        out.push(TrackInfo {
            source: source.to_string(),
            path: path.to_string_lossy().into_owned(),
            offset_ms: meta.tracks.get(source).map(|t| t.offset_ms).unwrap_or(0),
            duration_ms: bytes_to_ms(md.len() - HEADER_LEN),
        });
    }
    out
}

/// 陈旧头校验:实际长度与头部 data 尺寸不一致才重写(非活动笔记打开详情时调用;
/// 活动笔记跳过,避免与录制线程的头回写互踩)。
pub fn repair_stale_tracks(note_dir: &Path) {
    for source in ["mic", "system"] {
        let path = note_dir.join(format!("{source}.wav"));
        let Ok(md) = std::fs::metadata(&path) else { continue };
        let mut head = [0u8; HEADER_LEN as usize];
        let stale = File::open(&path)
            .and_then(|mut f| f.read_exact(&mut head).map(|_| ()))
            .map(|_| {
                let recorded = u32::from_le_bytes([head[40], head[41], head[42], head[43]]) as u64;
                recorded != md.len().saturating_sub(HEADER_LEN)
            })
            .unwrap_or(true);
        if stale {
            if let Err(e) = repair_wav_header(&path) {
                eprintln!("修复 WAV 头失败({}): {e}", path.display());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_wav(path: &Path) -> (hound::WavSpec, Vec<i16>) {
        let mut r = hound::WavReader::open(path).unwrap();
        let spec = r.spec();
        let samples: Vec<i16> = r.samples::<i16>().map(|s| s.unwrap()).collect();
        (spec, samples)
    }

    #[test]
    fn append_finalize_roundtrip_readable_by_hound() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = AudioTrackWriter::new(tmp.path(), "mic", 0);
        w.append(&[0.0, 0.5, -0.5, 1.0, -1.0, 2.0, -2.0]); // 越界值应被 clamp
        drop(w); // Drop 兜底收尾

        let (spec, samples) = read_wav(&tmp.path().join("mic.wav"));
        assert_eq!(spec.sample_rate, AUDIO_SAMPLE_RATE);
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.bits_per_sample, 16);
        assert_eq!(samples.len(), 7);
        assert_eq!(samples[0], 0);
        assert_eq!(samples[1], (0.5f32 * 32767.0) as i16);
        assert_eq!(samples[3], 32767);
        assert_eq!(samples[4], -32767);
        assert_eq!(samples[5], 32767, "越界 clamp 到满幅");
        assert_eq!(samples[6], -32767);

        // 新建轨道 offset = base_ms(此处 0),audio.json 落盘。
        let meta = load_audio_meta(tmp.path());
        assert_eq!(meta.tracks["mic"].offset_ms, 0);
    }

    #[test]
    fn open_existing_truncates_or_pads_to_base_ms() {
        let tmp = tempfile::tempdir().unwrap();
        // 第一场:写 2000 个样本(=125ms)。
        let mut w = AudioTrackWriter::new(tmp.path(), "mic", 0);
        w.append(&vec![0.25f32; 2000]);
        drop(w);

        // 续录 base_ms=100(<125ms):首次 append 前先截断到 1600 样本再落新音频。
        let mut w = AudioTrackWriter::new(tmp.path(), "mic", 100);
        w.append(&vec![0.5f32; 160]);
        drop(w);
        let (_, samples) = read_wav(&tmp.path().join("mic.wav"));
        assert_eq!(samples.len(), 1600 + 160, "超长截断到 base_ms 后追加");
        assert_eq!(samples[1599], (0.25f32 * 32767.0) as i16, "截断保留前段");
        assert_eq!(samples[1600], (0.5f32 * 32767.0) as i16, "新音频落位 base_ms");

        // 再续录 base_ms=200(>110ms):零填充到 3200 样本再追加。
        let mut w = AudioTrackWriter::new(tmp.path(), "mic", 200);
        w.append(&vec![0.75f32; 16]);
        drop(w);
        let (_, samples) = read_wav(&tmp.path().join("mic.wav"));
        assert_eq!(samples.len(), 3200 + 16, "不足零填充到 base_ms 后追加");
        assert_eq!(samples[1760], 0, "填充部分为静音");
        assert_eq!(samples[3200], (0.75f32 * 32767.0) as i16);
    }

    #[test]
    fn no_file_created_when_never_appended() {
        let tmp = tempfile::tempdir().unwrap();
        let w = AudioTrackWriter::new(tmp.path(), "system", 0);
        drop(w);
        assert!(!tmp.path().join("system.wav").exists(), "无帧不建档,不留空轨道");
        assert!(!tmp.path().join("audio.json").exists());
    }

    #[test]
    fn track_created_mid_note_records_offset() {
        let tmp = tempfile::tempdir().unwrap();
        // 模拟旧笔记续录/第二场才授权的 system:base_ms=60000 时轨道才出现。
        let mut w = AudioTrackWriter::new(tmp.path(), "system", 60_000);
        w.append(&vec![0.1f32; 160]);
        drop(w);
        let meta = load_audio_meta(tmp.path());
        assert_eq!(meta.tracks["system"].offset_ms, 60_000);
        let (_, samples) = read_wav(&tmp.path().join("system.wav"));
        assert_eq!(samples.len(), 160, "不为 offset 铺零,文件从轨道出现时刻开始");
    }

    #[test]
    fn repair_fixes_stale_header_after_simulated_crash() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mic.wav");
        let mut w = AudioTrackWriter::new(tmp.path(), "mic", 0);
        w.append(&vec![0.1f32; 100]); // 不足 1s,头仍是 0
        // 模拟硬崩:绕过 Drop。
        std::mem::forget(w);

        // 头记 0,实际 100 样本 → hound 读出 0 个样本(陈旧头的症状)。
        let (_, before) = read_wav(&path);
        assert!(before.is_empty(), "陈旧头下播放端看不到数据(前置条件)");

        repair_stale_tracks(tmp.path());
        let (_, after) = read_wav(&path);
        assert_eq!(after.len(), 100, "修复后数据可见");
    }

    #[test]
    fn repair_truncates_half_sample_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mic.wav");
        let mut w = AudioTrackWriter::new(tmp.path(), "mic", 0);
        w.append(&vec![0.1f32; 10]);
        std::mem::forget(w);
        // 追加半个样本的尾巴。
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&[0xAB]).unwrap();
        drop(f);
        repair_wav_header(&path).unwrap();
        let (_, samples) = read_wav(&path);
        assert_eq!(samples.len(), 10, "半样本尾巴被截掉");
    }

    #[test]
    fn list_tracks_reports_offset_and_duration_skips_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = AudioTrackWriter::new(tmp.path(), "mic", 0);
        w.append(&vec![0.1f32; AUDIO_SAMPLE_RATE as usize]); // 1s
        drop(w);
        // 空轨道(只有 44 字节头,如旧版本残留/崩溃残留)不上报。
        std::fs::write(tmp.path().join("system.wav"), wav_header(0)).unwrap();

        let tracks = list_tracks(tmp.path());
        assert_eq!(tracks.len(), 1, "空轨道不上报");
        assert_eq!(tracks[0].source, "mic");
        assert_eq!(tracks[0].offset_ms, 0);
        assert_eq!(tracks[0].duration_ms, 1000);
    }

    #[test]
    fn list_tracks_tolerates_missing_or_corrupt_audio_json() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = AudioTrackWriter::new(tmp.path(), "mic", 0);
        w.append(&vec![0.1f32; 160]);
        drop(w);
        std::fs::write(tmp.path().join("audio.json"), "not json {{").unwrap();
        let tracks = list_tracks(tmp.path());
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].offset_ms, 0, "损坏 audio.json 按 0 offset 容忍");
    }

    #[test]
    fn flush_interval_keeps_file_valid_mid_recording() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = AudioTrackWriter::new(tmp.path(), "mic", 0);
        // 1.5s:跨过一次刷盘节点,此刻(不 drop)文件头至少覆盖前 1s。
        w.append(&vec![0.1f32; (AUDIO_SAMPLE_RATE + AUDIO_SAMPLE_RATE / 2) as usize]);
        let (_, samples) = read_wav(&tmp.path().join("mic.wav"));
        assert!(samples.len() >= AUDIO_SAMPLE_RATE as usize, "录制中途文件即合法可读");
        drop(w);
    }
}
