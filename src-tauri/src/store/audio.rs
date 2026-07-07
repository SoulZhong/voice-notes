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
/// pub(crate):转码模块(transcode.rs)复用同一 WAV 头长常量,避免两处各写 44 漂移。
pub(crate) const HEADER_LEN: u64 = 44;
/// 追加多少样本后刷盘并回写头部尺寸(1s):任意时刻文件都是合法 WAV,崩溃最多丢约 1s。
const FLUSH_INTERVAL_SAMPLES: u64 = AUDIO_SAMPLE_RATE as u64;
/// RIFF 头 data 尺寸是 u32,单轨最大数据量(≈37 小时 @16k s16)。达到即停写,
/// 绝不让尺寸字段回绕产生"头小体大"的损坏文件。
const MAX_DATA_BYTES: u64 = u32::MAX as u64 - 36;

/// f32 样本([-1,1] 外 clamp)→ s16。音频轨道与声纹样本共用,保证两处编码一致。
pub fn f32_to_s16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * 32767.0) as i16
}

/// audio.json 全局写锁:mic/system 两个 worker 线程可能同时首次建档,
/// load→insert→save 之间无互斥会互相覆盖丢掉对方的 offset 项。
static META_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn meta_guard() -> std::sync::MutexGuard<'static, ()> {
    META_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn ms_to_bytes(ms: u64) -> u64 {
    // 损坏的 segments.jsonl 可能带出天文数字 end_ms → base_ms:饱和乘法防回绕,
    // 上限交由调用方(open 对照 MAX_DATA_BYTES 拒绝),不在这里 panic。
    ms.saturating_mul(AUDIO_SAMPLE_RATE as u64) / 1000 * BYTES_PER_SAMPLE
}

/// pub(crate):转码模块用它把 WAV data 字节数换算成毫秒(编码前后时长核对),
/// 与本模块的枚举/对齐共用同一换算,防两处公式分叉。
pub(crate) fn bytes_to_ms(bytes: u64) -> u64 {
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
    /// 转码完成后的编码格式(目前只有 "aac"),None 表示仍是原始 WAV。
    /// skip_serializing_if 让未压缩轨道的 JSON 保持旧形状,新旧版本双向兼容。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    /// 压缩产物(m4a)的总时长。m4a 容器不能像 WAV 那样按字节数换算时长,
    /// 必须由转码器实测后写入这里,list_tracks 直接读取。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// 真实音频波形:WAVEFORM_BUCKETS 桶等分时长,每桶峰值 |sample| 映射 0..255。
    /// 转码时从 WAV 流式预计算(m4a 解码贵,WAV 删除后无从再算);播放器据此画
    /// 音轨,替代按转写段落 rms 聚合的包络(说话稀疏时后者近乎空白,像显示故障)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waveform: Option<Vec<u8>>,
}

/// 波形桶数,与前端 WAVE_BARS 对齐(260 桶约 1KB JSON,audio.json 体积可忽略)。
pub const WAVEFORM_BUCKETS: usize = 260;

/// 从 16k/mono/s16 WAV 流式计算波形桶:每桶取峰值 |i16| 折算 0..255。
/// BufReader 顺序读,1 小时音频(~230MB)亚秒级;不整读进内存。
pub fn waveform_from_wav(path: &Path) -> anyhow::Result<Vec<u8>> {
    use std::io::{BufReader, Read, Seek, SeekFrom};
    let f = std::fs::File::open(path)?;
    let data_len = f.metadata()?.len().saturating_sub(HEADER_LEN);
    let total_samples = (data_len / 2) as usize;
    if total_samples == 0 {
        return Ok(vec![0; WAVEFORM_BUCKETS]);
    }
    let mut r = BufReader::with_capacity(1 << 20, f);
    r.seek(SeekFrom::Start(HEADER_LEN))?;
    let mut out = vec![0u8; WAVEFORM_BUCKETS];
    let mut buf = vec![0u8; 1 << 20];
    let mut idx = 0usize;
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        for ch in buf[..n].chunks_exact(2) {
            let s = i16::from_le_bytes([ch[0], ch[1]]).unsigned_abs();
            // 桶号按样本序号等分;末尾越界样本(header 修复竞态)并入最后一桶。
            let b = (idx * WAVEFORM_BUCKETS / total_samples).min(WAVEFORM_BUCKETS - 1);
            let v = (s >> 7).min(255) as u8; // 32768 满幅 → 256 档,饱和到 255
            if v > out[b] {
                out[b] = v;
            }
            idx += 1;
        }
    }
    Ok(out)
}

/// 纯 PCM 字节(s16le)桶化,公式与 waveform_from_wav 一致。旧笔记回填用:
/// m4a 解码产物经 extract_wav_data 拿到的就是纯 data 字节,没有 44 头可跳。
pub fn waveform_from_pcm(bytes: &[u8]) -> Vec<u8> {
    let total_samples = bytes.len() / 2;
    let mut out = vec![0u8; WAVEFORM_BUCKETS];
    if total_samples == 0 {
        return out;
    }
    for (idx, ch) in bytes.chunks_exact(2).enumerate() {
        let s = i16::from_le_bytes([ch[0], ch[1]]).unsigned_abs();
        let b = (idx * WAVEFORM_BUCKETS / total_samples).min(WAVEFORM_BUCKETS - 1);
        let v = (s >> 7).min(255) as u8;
        if v > out[b] {
            out[b] = v;
        }
    }
    out
}

/// 单独写入某轨波形(旧笔记懒回填)。持 META_LOCK,同 set_track_compressed。
pub fn set_track_waveform(note_dir: &Path, source: &str, waveform: Vec<u8>) -> anyhow::Result<()> {
    let _guard = meta_guard();
    let mut meta = load_audio_meta(note_dir);
    meta.tracks.entry(source.to_string()).or_default().waveform = Some(waveform);
    save_audio_meta(note_dir, &meta)
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

/// 转码器(Task 5)完成 `<source>.m4a` 后调用:记下 codec/duration_ms,
/// list_tracks 据此把该轨道的枚举从 WAV 切到 m4a。
/// 持 META_LOCK:与 AudioTrackWriter::open 等其它 load→改→save 序列互斥,
/// 避免并发建档/转码互相覆盖 audio.json。
pub fn set_track_compressed(
    note_dir: &Path,
    source: &str,
    duration_ms: u64,
    waveform: Option<Vec<u8>>,
) -> anyhow::Result<()> {
    let _guard = meta_guard();
    let mut meta = load_audio_meta(note_dir);
    let track = meta.tracks.entry(source.to_string()).or_default();
    track.codec = Some("aac".to_string());
    track.duration_ms = Some(duration_ms);
    if waveform.is_some() {
        track.waveform = waveform;
    }
    save_audio_meta(note_dir, &meta)
}

/// 回落到 WAV 逻辑(如转码失败需要撤销/重录):清掉 codec/duration_ms,offset_ms 不动。
pub fn clear_track_compressed(note_dir: &Path, source: &str) -> anyhow::Result<()> {
    let _guard = meta_guard();
    let mut meta = load_audio_meta(note_dir);
    if let Some(track) = meta.tracks.get_mut(source) {
        track.codec = None;
        track.duration_ms = None;
    }
    save_audio_meta(note_dir, &meta)
}

/// 44 字节标准 PCM WAV 头。data_len 为 data 块字节数。
/// pub(crate):转码模块解码后需把 afconvert 产出的非标准头 WAV(带 FLLR 对齐填充块、
/// 40 字节 fmt 块)重写回这套标准 44 头,续录端(AudioTrackWriter 假定 44 头)才不踩坑。
pub(crate) fn wav_header(data_len: u32) -> [u8; HEADER_LEN as usize] {
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
    /// - 已存在:set_len 到 (base_ms - offset_ms) 对应字节(截掉上场末尾静音/被丢段,
    ///   不足则零填充)并重写头——续录新音频落位即对齐(陈旧头也一并被这次重写覆盖)。
    ///
    /// 全程持 audio.json 写锁:两源 worker 可能同时首次建档,load→save 无互斥会丢项。
    fn open(&self) -> anyhow::Result<(File, PathBuf, u64)> {
        let _guard = meta_guard();
        let path = self.note_dir.join(format!("{}.wav", self.source));
        let mut meta = load_audio_meta(&self.note_dir);
        if path.exists() {
            let existing_data = std::fs::metadata(&path)?.len().saturating_sub(HEADER_LEN);
            let offset_ms = match meta.tracks.get(&self.source) {
                Some(t) => t.offset_ms,
                None => {
                    // audio.json 丢失/缺项:offset=0 会把中途出现的轨道整体前移并被
                    // 破坏性 set_len 固化。按「上场停止时文件尾 ≈ base_ms」的对齐
                    // 不变式反推 offset = base_ms - 时长(负值饱和为 0,等价旧行为),
                    // 并立即回写补全,让重建只发生一次。
                    let est = self.base_ms.saturating_sub(bytes_to_ms(existing_data));
                    meta.schema_version = 1;
                    meta.tracks.insert(self.source.clone(), TrackMeta { offset_ms: est, ..Default::default() });
                    save_audio_meta(&self.note_dir, &meta)?;
                    est
                }
            };
            // base_ms 只增不减且轨道创建时 offset = 当时的 base,故差值非负;防御 saturating。
            let target = ms_to_bytes(self.base_ms.saturating_sub(offset_ms));
            if target > MAX_DATA_BYTES {
                anyhow::bail!("对齐目标超出 WAV 尺寸上限(base_ms 异常?): {target} 字节");
            }
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
            meta.tracks.insert(self.source.clone(), TrackMeta { offset_ms: self.base_ms, ..Default::default() });
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
        if let TrackState::Open { data_len, path, .. } = &self.state {
            if *data_len + (samples.len() as u64) * BYTES_PER_SAMPLE > MAX_DATA_BYTES {
                eprintln!("音频轨道达到 WAV 4GiB 尺寸上限,停写({})", path.display());
                self.flush_header(); // 已写内容仍是合法 WAV
                self.state = TrackState::Failed;
                return;
            }
        }
        let TrackState::Open { file, path, data_len, since_flush, buf } = &mut self.state else {
            return;
        };
        buf.clear();
        buf.reserve(samples.len() * 2);
        for s in samples {
            buf.extend_from_slice(&f32_to_s16(*s).to_le_bytes());
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
    /// 真实音频波形(0..255 峰值桶,见 TrackMeta::waveform)。已转码轨取预计算值;
    /// 未转码 WAV 现算(流式读,亚秒级);None = 旧笔记无从取得,前端回退段落包络。
    pub waveform: Option<Vec<u8>>,
}

/// 已知源集合 = audio.json 记录过的 ∪ 内建两源:写入端(lib.rs 按配置源建档)与
/// 读取端由 audio.json 桥接对齐,未来新增源不会在这里被漏掉。
fn known_sources(meta: &AudioMeta) -> Vec<String> {
    let mut sources: Vec<String> = vec!["mic".into(), "system".into()];
    for s in meta.tracks.keys() {
        if !sources.iter().any(|x| x == s) {
            sources.push(s.clone());
        }
    }
    sources
}

/// 枚举笔记的音频轨道(详情页播放器用)。每源优先上报已转码的 m4a、否则回落 WAV。
/// 时长口径按格式区分:WAV 由字节数换算(bytes_to_ms);m4a 例外——容器不能按字节换算,
/// 时长取转码器实测后写进 audio.json 的记录(记录缺失即视为损坏,跳过该轨,不回落 WAV)。
pub fn list_tracks(note_dir: &Path) -> Vec<TrackInfo> {
    let meta = load_audio_meta(note_dir);
    let mut out = Vec::new();
    for source in known_sources(&meta) {
        let m4a_path = note_dir.join(format!("{source}.m4a"));
        if m4a_path.exists() {
            // 转码已完成:优先上报 m4a。m4a 容器不能按字节数换算时长,只能取转码器
            // 实测写入 audio.json 的记录;记录缺失说明转码/写档中途失败,视为损坏跳过
            // 该轨(而非回落 WAV——WAV 大概率已被转码流水线删除)。
            let Some(duration_ms) = meta.tracks.get(&source).and_then(|t| t.duration_ms) else {
                continue;
            };
            out.push(TrackInfo {
                path: m4a_path.to_string_lossy().into_owned(),
                offset_ms: meta.tracks.get(&source).map(|t| t.offset_ms).unwrap_or(0),
                waveform: meta.tracks.get(&source).and_then(|t| t.waveform.clone()),
                source,
                duration_ms,
            });
            continue;
        }
        let path = note_dir.join(format!("{source}.wav"));
        let Ok(md) = std::fs::metadata(&path) else { continue };
        if md.len() <= HEADER_LEN {
            continue; // 空轨道(刚建头没内容/损坏残留)不给前端,免得渲染空播放器
        }
        out.push(TrackInfo {
            path: path.to_string_lossy().into_owned(),
            offset_ms: meta.tracks.get(&source).map(|t| t.offset_ms).unwrap_or(0),
            // 未转码 WAV(中断笔记/转码失败降级)现算:流式读亚秒级,详情页打开是
            // 低频动作;失败回 None,前端退段落包络,不挡枚举。
            waveform: waveform_from_wav(&path).ok(),
            source,
            duration_ms: bytes_to_ms(md.len() - HEADER_LEN),
        });
    }
    out
}

/// 陈旧头校验:实际长度与头部 data 尺寸不一致才重写(非活动笔记打开详情时调用;
/// 活动笔记跳过,避免与录制线程的头回写互踩)。
/// 只对 `.wav` 有意义(WAV 头才有"陈旧"这回事,m4a 时长是转码器一次性写死的);
/// 下面固定 open `<source>.wav`,某源已转码则该文件不存在,`Ok(md) else continue`
/// 天然跳过,无需额外分支。
pub fn repair_stale_tracks(note_dir: &Path) {
    let meta = load_audio_meta(note_dir);
    for source in known_sources(&meta) {
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

    #[test]
    fn waveform_buckets_track_peaks_and_pcm_agrees() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("t.wav");
        // 前半静音、后半半幅(16384 → 桶值 128):前半桶应为 0,后半桶应为 128。
        let half = WAVEFORM_BUCKETS * 100; // 每桶 100 样本,整除避免边界桶混采
        let mut data = Vec::with_capacity(half * 4);
        for _ in 0..half {
            data.extend_from_slice(&0i16.to_le_bytes());
        }
        for _ in 0..half {
            data.extend_from_slice(&16384i16.to_le_bytes());
        }
        let mut file = wav_header(data.len() as u32).to_vec();
        file.extend_from_slice(&data);
        std::fs::write(&wav, &file).unwrap();

        let wf = waveform_from_wav(&wav).unwrap();
        assert_eq!(wf.len(), WAVEFORM_BUCKETS);
        assert!(wf[..WAVEFORM_BUCKETS / 2].iter().all(|&v| v == 0), "前半应静音");
        assert!(wf[WAVEFORM_BUCKETS / 2..].iter().all(|&v| v == 128), "后半应半幅");
        // 流式(waveform_from_wav)与整块(waveform_from_pcm,回填路径)必须同答案。
        assert_eq!(wf, waveform_from_pcm(&data));
    }

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

    #[test]
    fn list_tracks_prefers_m4a_with_recorded_duration() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = AudioTrackWriter::new(tmp.path(), "mic", 0);
        w.append(&vec![0.1f32; 1600]); // 100ms WAV
        drop(w);
        // 模拟转码完成:m4a 文件(内容不重要,枚举只看存在性)+ meta 标记
        std::fs::write(tmp.path().join("mic.m4a"), b"fake m4a").unwrap();
        set_track_compressed(tmp.path(), "mic", 100, None).unwrap();
        std::fs::remove_file(tmp.path().join("mic.wav")).unwrap();

        let tracks = list_tracks(tmp.path());
        assert_eq!(tracks.len(), 1);
        assert!(tracks[0].path.ends_with("mic.m4a"));
        assert_eq!(tracks[0].duration_ms, 100, "m4a 时长来自 audio.json 而非字节换算");
        // roundtrip 兼容:文件里真写进了字段
        let meta = load_audio_meta(tmp.path());
        assert_eq!(meta.tracks["mic"].codec.as_deref(), Some("aac"));

        // 清除后回落 WAV 逻辑
        std::fs::remove_file(tmp.path().join("mic.m4a")).unwrap();
        clear_track_compressed(tmp.path(), "mic").unwrap();
        let mut w = AudioTrackWriter::new(tmp.path(), "mic", 0);
        w.append(&vec![0.1f32; 1600]);
        drop(w);
        let tracks = list_tracks(tmp.path());
        assert!(tracks[0].path.ends_with("mic.wav"));
    }

    #[test]
    fn m4a_without_duration_is_skipped_and_old_meta_parses() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("mic.m4a"), b"fake").unwrap();
        // 只有 offset 的旧形状 audio.json(无 codec/duration)→ 可解析;m4a 无 duration 记录 → 跳过
        std::fs::write(tmp.path().join("audio.json"), r#"{"schema_version":1,"tracks":{"mic":{"offset_ms":0}}}"#).unwrap();
        assert!(list_tracks(tmp.path()).is_empty(), "无 duration 记录的 m4a 不上报");
    }
}
