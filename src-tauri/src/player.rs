//! 原生回放引擎:单条 cpal 输出流把多轨 PCM 按 offset 混音——播放彻底脱离 WebView。
//!
//! 为什么原生:WKWebView 在打包版(tauri:// 文档源)把 <audio> 会话标为 Autoplaying,
//! 窗口不可见 5 秒宽限后释放 WebContent 前台断言、媒体会话 Interrupted(2026-07-10
//! 系统日志实锤);此前 Web Audio 增益路由更是整体静音。回放走原生后,后台播放、
//! 自动播放策略、静音污染这一类 WebView 媒体坑一次全消,与录音侧同一可靠性等级。
//!
//! 结构:
//! - 音轨 WAV(16k 单声道 s16、标准 44 头)mmap 进回调,随机访问零拷贝,seek=改游标;
//!   m4a 先经 afconvert 解码到应用缓存目录(decode_m4a_to_standard_wav),缓存跨会话
//!   复用、启动时清理过期(见 clean_playback_cache)。
//! - 单输出流 = 单一采样时钟:游标以 16k 源域采样计,双轨对齐按构造成立(与录音侧
//!   「文件内毫秒 + offset_ms == 时间轴毫秒」同一哲学);设备采样率差异由游标按
//!   step=16000/dev_rate 分数步进 + 线性插值消化,无需独立重采样器状态。
//! - 事件:流线程每 200ms 发 player_pos{pos_ms,playing},前端只画 UI 不管时钟;
//!   播完(游标到尾)回调侧自动置停,事件如实带出。
//! - cpal Stream !Send:流线程独占(与 microphone.rs 同模式),stop 通道断开即停。

use memmap2::Mmap;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager, State};

/// 源域采样率:录音落盘恒 16k 单声道 s16(store::audio 的固定格式)。
const SRC_RATE: f64 = 16000.0;
const HEADER_LEN: u64 = 44;
/// 位置事件间隔:歌词跟随按段落级高亮,200ms 粒度足够,还省 IPC。
const POS_EVENT_MS: u64 = 200;
/// 解码缓存保留天数:超期启动清理(mtime 判定)。
const CACHE_KEEP_DAYS: u64 = 7;

/// 轨道 PCM 字节来源:生产走 mmap;单测喂内存,混音核心无需真文件。
enum TrackBytes {
    Mmap(Mmap),
    #[cfg(test)]
    Mem(Vec<u8>),
}

impl TrackBytes {
    fn bytes(&self) -> &[u8] {
        match self {
            TrackBytes::Mmap(m) => m,
            #[cfg(test)]
            TrackBytes::Mem(v) => v,
        }
    }
}

struct Track {
    data: TrackBytes,
    /// 该轨 0 时刻在笔记时间轴上的位置(16k 采样数)。
    offset_samples: u64,
    /// 有效采样数((文件长-44)/2,按实际字节封顶防截断文件越界)。
    len_samples: u64,
    muted: AtomicBool,
    source: String,
    /// 回放压低区间(player_gate 构建;system/无段数据轨为空表=行为同现状)。
    gate: Vec<crate::player_gate::GateSpan>,
}

impl Track {
    /// 第 i 个采样(s16le → f32)。越界回 0(混音端已界判,此为兜底)。
    fn sample(&self, i: u64) -> f32 {
        let b = self.data.bytes();
        let at = (HEADER_LEN + i * 2) as usize;
        if at + 1 >= b.len() {
            return 0.0;
        }
        i16::from_le_bytes([b[at], b[at + 1]]) as f32 / 32768.0
    }
}

struct Core {
    tracks: Vec<Track>,
    /// 时间轴总长(16k 采样数)= max(offset+len)。
    total_samples: u64,
    /// 播放游标(f64 bits,16k 源域采样),回调推进、seek 改写。
    cursor_bits: AtomicU64,
    playing: AtomicBool,
}

impl Core {
    fn cursor(&self) -> f64 {
        f64::from_bits(self.cursor_bits.load(Ordering::Relaxed))
    }
    fn set_cursor(&self, v: f64) {
        self.cursor_bits.store(v.to_bits(), Ordering::Relaxed);
    }
    fn pos_ms(&self) -> u64 {
        (self.cursor() / SRC_RATE * 1000.0) as u64
    }
}

/// 混音软限幅:多轨相加在双讲响处会越过 ±1.0,旧代码硬 clamp 会削顶产生刺耳失真。
/// KNEE(0.95)以下逐位透传——单轨回放/多轨轻响时行为与旧版逐采样一致;越过 KNEE 才按
/// `e/(e+r)` 拐点把超出量平滑压入 (KNEE,1.0),渐近 1.0、恒不越界、无硬削顶。KNEE 取 0.95
/// 而非更低,是为了让绝大多数语音峰值(远低于 0.95)完全不被触碰,只驯服真正的叠加过冲。
fn soft_limit(x: f32) -> f32 {
    const KNEE: f32 = 0.95;
    let a = x.abs();
    if a <= KNEE {
        return x;
    }
    let room = 1.0 - KNEE;
    let e = a - KNEE;
    x.signum() * (KNEE + room * (e / (e + room)))
}

/// 混音核心(纯函数,单测覆盖):从 cursor 起以 step 源采样/帧填充 frames 帧,
/// 每帧写 channels 个声道(同值)。返回新 cursor。播完(cursor≥total)置停并静音填充。
fn mix_frames(core: &Core, out: &mut [f32], channels: usize, step: f64) -> f64 {
    let mut cursor = core.cursor();
    for frame in out.chunks_mut(channels) {
        let mut acc = 0.0f32;
        if core.playing.load(Ordering::Relaxed) && cursor < core.total_samples as f64 {
            for t in &core.tracks {
                if t.muted.load(Ordering::Relaxed) {
                    continue;
                }
                let local = cursor - t.offset_samples as f64;
                if local >= 0.0 && local < t.len_samples as f64 {
                    let idx = local as u64;
                    let frac = (local - idx as f64) as f32;
                    let a = t.sample(idx);
                    // 末采样右邻越界时取自身(等价 clamp),不读 0 免得尾部半帧塌陷。
                    let b = if idx + 1 < t.len_samples { t.sample(idx + 1) } else { a };
                    let g = if t.gate.is_empty() {
                        1.0
                    } else {
                        crate::player_gate::gain_at(&t.gate, cursor as u64)
                    };
                    acc += (a + (b - a) * frac) * g;
                }
            }
            cursor += step;
            if cursor >= core.total_samples as f64 {
                cursor = core.total_samples as f64;
                core.playing.store(false, Ordering::Relaxed); // 播完自动停,事件如实带出
            }
        }
        let v = soft_limit(acc);
        for ch in frame.iter_mut() {
            *ch = v;
        }
    }
    core.set_cursor(cursor);
    cursor
}

/// 全局播放器句柄(tauri manage):同一时刻至多一个 Core 在放(单窗口单播放器)。
pub struct PlayerHandle {
    core: Mutex<Option<Arc<Core>>>,
    /// 流线程停止通道(drop/发送皆停,与 microphone.rs 同模式)。
    stop_tx: Mutex<Option<crossbeam_channel::Sender<()>>>,
}

impl Default for PlayerHandle {
    fn default() -> Self {
        Self { core: Mutex::new(None), stop_tx: Mutex::new(None) }
    }
}

#[derive(Debug, Deserialize)]
pub struct LoadTrack {
    pub path: String,
    pub offset_ms: u64,
    pub source: String,
}

#[derive(Debug, Clone, Serialize)]
struct PosEvent {
    pos_ms: u64,
    playing: bool,
}

fn emit_pos(app: &AppHandle, core: &Core) {
    let _ = app.emit(
        "player_pos",
        PosEvent { pos_ms: core.pos_ms(), playing: core.playing.load(Ordering::Relaxed) },
    );
}

/// m4a 的解码缓存路径:cache_dir/playback/<源路径哈希>-<文件名>.wav。
/// 哈希用 sha2(已有依赖),文件名后缀留可读性便于排障。
fn cache_path_for(app: &AppHandle, m4a: &Path) -> anyhow::Result<PathBuf> {
    use sha2::{Digest, Sha256};
    let dir = app.path().app_cache_dir()?.join("playback");
    std::fs::create_dir_all(&dir)?;
    let mut h = Sha256::new();
    h.update(m4a.to_string_lossy().as_bytes());
    let hash = hex::encode(&h.finalize()[..8]);
    let name = m4a.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    Ok(dir.join(format!("{hash}-{name}.wav")))
}

/// 启动清理:回收超期(CACHE_KEEP_DAYS)的解码缓存。播放缓存可再生,清错无害。
pub fn clean_playback_cache(app: &AppHandle) {
    let Ok(dir) = app.path().app_cache_dir().map(|d| d.join("playback")) else { return };
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    let keep = std::time::Duration::from_secs(CACHE_KEEP_DAYS * 24 * 3600);
    for e in entries.filter_map(|e| e.ok()) {
        let stale = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|age| age > keep)
            .unwrap_or(true); // 读不到元数据的残留一并清
        if stale {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// 校验音轨路径落在本应用 notes 目录内(canonicalize 前缀匹配),拒绝越权读任意文件。
fn validate_under_notes(app: &AppHandle, path: &Path) -> Result<PathBuf, String> {
    let notes = crate::notes_dir(app).map_err(|e| e.to_string())?;
    let notes_canon = std::fs::canonicalize(&notes).map_err(|e| e.to_string())?;
    let canon = std::fs::canonicalize(path).map_err(|e| e.to_string())?;
    if !canon.starts_with(&notes_canon) {
        return Err("路径越界".into());
    }
    Ok(canon)
}

/// 装载音轨并(重)起输出流。m4a 先解码到缓存(秒级,spawn_blocking 不占主线程,
/// 前端 await 本命令即拿到就绪信号);返回时间轴总长 ms。
#[tauri::command]
pub async fn player_load(
    app: AppHandle,
    state: State<'_, PlayerHandle>,
    tracks: Vec<LoadTrack>,
) -> Result<u64, String> {
    // 先停旧流(切笔记):旧 Core 一并丢弃,防旧事件串台。
    stop_stream(&state);

    // 路径校验 + m4a 解码规划(阻塞段全部挪到 spawn_blocking)。
    // note_dir:取首条轨校验后路径的父目录(m4a 会被换成缓存路径,故须在换之前取,
    // 各轨同属一个笔记,取一次即可)——segments.jsonl 与音轨文件同目录。
    let mut note_dir: Option<PathBuf> = None;
    let mut plan: Vec<(PathBuf, u64, String)> = Vec::new();
    for t in &tracks {
        let src = validate_under_notes(&app, Path::new(&t.path))?;
        if note_dir.is_none() {
            note_dir = src.parent().map(|p| p.to_path_buf());
        }
        let wav = if src.extension().and_then(|e| e.to_str()) == Some("m4a") {
            let cache = cache_path_for(&app, &src).map_err(|e| e.to_string())?;
            let fresh = match (std::fs::metadata(&cache), std::fs::metadata(&src)) {
                (Ok(c), Ok(s)) => match (c.modified(), s.modified()) {
                    (Ok(cm), Ok(sm)) => cm >= sm,
                    _ => false,
                },
                _ => false,
            };
            if !fresh {
                let (src2, cache2) = (src.clone(), cache.clone());
                tauri::async_runtime::spawn_blocking(move || {
                    crate::store::transcode::decode_m4a_to_standard_wav(&src2, &cache2)
                })
                .await
                .map_err(|e| format!("解码任务失败: {e}"))?
                .map_err(|e| format!("解码 m4a 失败: {e}"))?;
            }
            cache
        } else {
            src
        };
        plan.push((wav, t.offset_ms, t.source.clone()));
    }

    // 回放门控:按转写段活跃度构建 mic 轨压低区间(任何失败空表降级=现状)。
    let gate_spans = match &note_dir {
        Some(dir) => {
            let seg_path = dir.join("segments.jsonl");
            let segs = crate::player_gate::parse_segments_jsonl(&seg_path);
            if segs.is_empty() {
                eprintln!("回放门控: segments 缺失或为空,本次回放不做门控");
                Vec::new()
            } else {
                crate::player_gate::build_gate(&segs)
            }
        }
        None => {
            eprintln!("回放门控: 无法定位笔记目录,本次回放不做门控");
            Vec::new()
        }
    };
    if !gate_spans.is_empty() {
        eprintln!("回放门控: {} 个压低区间(mic 轨,-15dB,双讲保护)", gate_spans.len());
    }

    // mmap 装载 + Core 组装。
    let mut loaded = Vec::new();
    for (wav, offset_ms, source) in plan {
        let f = std::fs::File::open(&wav).map_err(|e| format!("打开音轨失败: {e}"))?;
        let len = f.metadata().map_err(|e| e.to_string())?.len();
        if len <= HEADER_LEN {
            continue; // 空轨容忍:枚举端一般已滤,这里兜底
        }
        // SAFETY: 只读 mmap;录制停止后转码队列仍会碰这个 wav,但不会造成 UB——
        // repair_wav_header 只截尾 ≤1 字节(data_len % 2),亚页级,已映射页仍有后备字节,不会 SIGBUS;
        // remove_file 是 unlink,已映射的 inode 存活到 munmap;
        // 真正的内容替换(解码/转码产物)都走 tmp+rename = 新 inode,旧映射不受影响;
        // len_samples 在映射时按实际文件长度封顶,读取永不越过映射长度。
        // 警示:若以后 repair_wav_header 截掉超过亚页的长度,或引入任何"原地截断"写入器,
        // 这里会变成真 SIGBUS——届时必须改为 copy-read 或文件锁。
        let mmap = unsafe { Mmap::map(&f) }.map_err(|e| format!("mmap 失败: {e}"))?;
        loaded.push(Track {
            data: TrackBytes::Mmap(mmap),
            offset_samples: offset_ms * SRC_RATE as u64 / 1000,
            len_samples: (len - HEADER_LEN) / 2,
            muted: AtomicBool::new(false),
            gate: if source == "mic" { gate_spans.clone() } else { Vec::new() },
            source,
        });
    }
    if loaded.is_empty() {
        return Err("没有可播放的音轨".into());
    }
    let total_samples = loaded.iter().map(|t| t.offset_samples + t.len_samples).max().unwrap_or(0);
    let core = Arc::new(Core {
        tracks: loaded,
        total_samples,
        cursor_bits: AtomicU64::new(0f64.to_bits()),
        playing: AtomicBool::new(false),
    });
    *state.core.lock().unwrap() = Some(core.clone());
    if let Err(e) = start_stream(&app, &state, core) {
        stop_stream(&state); // 起流失败不留残核:否则后续 play 假成功、UI 卡"播放中"
        return Err(e);
    }
    Ok((total_samples as f64 / SRC_RATE * 1000.0) as u64)
}

/// 起输出流线程:线程独占 !Send 的 cpal Stream,兼任 200ms 位置事件发射;
/// stop 通道 recv_timeout 一石二鸟(定时 + 断开即停)。
fn start_stream(app: &AppHandle, state: &State<'_, PlayerHandle>, core: Arc<Core>) -> Result<(), String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(0);
    let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<(), String>>(1);
    let app = app.clone();
    std::thread::spawn(move || {
        let opened = (|| -> Result<(cpal::Stream, f64), String> {
            let device = cpal::default_host()
                .default_output_device()
                .ok_or_else(|| "找不到输出设备".to_string())?;
            let supported = device.default_output_config().map_err(|e| e.to_string())?;
            if supported.sample_format() != cpal::SampleFormat::F32 {
                return Err(format!("输出格式不支持: {}(仅支持 f32)", supported.sample_format()));
            }
            let config: cpal::StreamConfig = supported.into();
            let channels = config.channels as usize;
            let step = SRC_RATE / config.sample_rate.0 as f64;
            let mix_core = core.clone();
            let stream = device
                .build_output_stream(
                    &config,
                    move |out: &mut [f32], _| {
                        mix_frames(&mix_core, out, channels, step);
                    },
                    |e| eprintln!("播放流错误: {e}"),
                    None,
                )
                .map_err(|e| e.to_string())?;
            stream.play().map_err(|e| e.to_string())?;
            Ok((stream, step))
        })();
        let _stream = match opened {
            Ok((s, _)) => {
                let _ = ready_tx.send(Ok(()));
                s
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        };
        // 事件泵:200ms 一发;stop 关闭/收到即退出(流随线程结束 drop 停止)。
        loop {
            match stop_rx.recv_timeout(std::time::Duration::from_millis(POS_EVENT_MS)) {
                Ok(()) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => emit_pos(&app, &core),
            }
        }
    });
    ready_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .map_err(|_| "输出流启动超时".to_string())??;
    *state.stop_tx.lock().unwrap() = Some(stop_tx);
    Ok(())
}

fn stop_stream(state: &State<'_, PlayerHandle>) {
    *state.stop_tx.lock().unwrap() = None; // drop 即断开,流线程退出
    *state.core.lock().unwrap() = None;
}

#[tauri::command]
pub fn player_play(app: AppHandle, state: State<'_, PlayerHandle>) -> Result<(), String> {
    let g = state.core.lock().unwrap();
    let core = g.as_ref().ok_or("尚未装载音轨")?;
    // 播完再按:从头来(与旧前端播放器语义一致)。
    if core.cursor() >= core.total_samples as f64 {
        core.set_cursor(0.0);
    }
    core.playing.store(true, Ordering::Relaxed);
    emit_pos(&app, core);
    Ok(())
}

#[tauri::command]
pub fn player_pause(app: AppHandle, state: State<'_, PlayerHandle>) -> Result<(), String> {
    let g = state.core.lock().unwrap();
    let core = g.as_ref().ok_or("尚未装载音轨")?;
    core.playing.store(false, Ordering::Relaxed);
    emit_pos(&app, core);
    Ok(())
}

#[tauri::command]
pub fn player_seek(app: AppHandle, state: State<'_, PlayerHandle>, ms: u64) -> Result<(), String> {
    let g = state.core.lock().unwrap();
    let core = g.as_ref().ok_or("尚未装载音轨")?;
    let target = (ms as f64 / 1000.0 * SRC_RATE).min(core.total_samples as f64);
    core.set_cursor(target);
    emit_pos(&app, core);
    Ok(())
}

#[tauri::command]
pub fn player_set_muted(state: State<'_, PlayerHandle>, source: String, muted: bool) -> Result<(), String> {
    let g = state.core.lock().unwrap();
    let core = g.as_ref().ok_or("尚未装载音轨")?;
    for t in &core.tracks {
        if t.source == source {
            t.muted.store(muted, Ordering::Relaxed);
        }
    }
    Ok(())
}

#[tauri::command]
pub fn player_stop(state: State<'_, PlayerHandle>) -> Result<(), String> {
    stop_stream(&state);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造内存轨:samples 为 s16 值序列。
    fn mem_track(samples: &[i16], offset_ms: u64, source: &str) -> Track {
        let mut bytes = vec![0u8; HEADER_LEN as usize];
        for s in samples {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        Track {
            data: TrackBytes::Mem(bytes),
            offset_samples: offset_ms * 16,
            len_samples: samples.len() as u64,
            muted: AtomicBool::new(false),
            source: source.into(),
            gate: Vec::new(),
        }
    }

    fn core_of(tracks: Vec<Track>) -> Core {
        let total = tracks.iter().map(|t| t.offset_samples + t.len_samples).max().unwrap_or(0);
        Core {
            tracks,
            total_samples: total,
            cursor_bits: AtomicU64::new(0f64.to_bits()),
            playing: AtomicBool::new(true),
        }
    }

    fn track_from_canonical_wav(
        bytes: Vec<u8>,
        offset_ms: u64,
        source: &str,
        gate: Vec<crate::player_gate::GateSpan>,
    ) -> Track {
        let len_samples = (bytes.len() as u64 - HEADER_LEN) / 2;
        Track {
            data: TrackBytes::Mem(bytes),
            offset_samples: offset_ms * 16,
            len_samples,
            muted: AtomicBool::new(false),
            source: source.into(),
            gate,
        }
    }

    /// 离线复现真实播放器混音,供排查"叠放两遍/门控错位"类回放 bug。
    /// 解码走生产同款 `decode_m4a_to_standard_wav`(44 头 canonical),门控走真 build_gate,
    /// 采样/插值/门控全部经真 `mix_frames`,48k 设备率(与真机同 step=1/3)。
    /// 输出 48k 单声道 WAV,可直接试听或做自相关看有没有被叠出回声。
    /// env: VN_MIX_NOTE=笔记目录  VN_MIX_OUT=输出wav  VN_MIX_START_MS(默0) VN_MIX_DUR_MS(默600000)
    #[test]
    #[ignore]
    fn render_playback_mix() {
        let note = std::path::PathBuf::from(std::env::var("VN_MIX_NOTE").expect("VN_MIX_NOTE"));
        let out_p = std::env::var("VN_MIX_OUT").expect("VN_MIX_OUT");
        let start_ms: u64 =
            std::env::var("VN_MIX_START_MS").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
        let dur_ms: u64 =
            std::env::var("VN_MIX_DUR_MS").ok().and_then(|s| s.parse().ok()).unwrap_or(600_000);

        // 生产同款解码:m4a → 44 头 canonical WAV(afconvert 的 FLLR 头已被 extract 掉)。
        let tmp = tempfile::tempdir().unwrap();
        let decode = |src: &str| -> Vec<u8> {
            let m4a = note.join(format!("{src}.m4a"));
            let wav = note.join(format!("{src}.wav"));
            if wav.exists() {
                return std::fs::read(&wav).unwrap();
            }
            let dest = tmp.path().join(format!("{src}.wav"));
            crate::store::transcode::decode_m4a_to_standard_wav(&m4a, &dest).unwrap();
            std::fs::read(&dest).unwrap()
        };

        // 真门控:segments.jsonl → build_gate(只压 mic)。
        let segs = crate::player_gate::parse_segments_jsonl(&note.join("segments.jsonl"));
        let gate = crate::player_gate::build_gate(&segs);
        eprintln!("门控压低区间: {} 个", gate.len());

        // 真轨道偏移:audio.json。
        let meta = crate::store::audio::load_audio_meta(&note);
        let off = |s: &str| meta.tracks.get(s).map(|t| t.offset_ms).unwrap_or(0);
        let mic = track_from_canonical_wav(decode("mic"), off("mic"), "mic", gate);
        let sys = track_from_canonical_wav(decode("system"), off("system"), "system", Vec::new());
        eprintln!(
            "mic {} 采样 offset {}ms | system {} 采样 offset {}ms",
            mic.len_samples, off("mic"), sys.len_samples, off("system")
        );
        let core = core_of(vec![mic, sys]);

        // 48k 设备(真机同 step),从 start_ms 渲染 dur_ms。
        let device_rate = 48_000u32;
        let step = SRC_RATE / device_rate as f64;
        core.set_cursor((start_ms * 16) as f64);
        let out_frames = (dur_ms * device_rate as u64 / 1000) as usize;
        let mut pcm: Vec<i16> = Vec::with_capacity(out_frames);
        let mut buf = vec![0f32; device_rate as usize]; // 每次 1s,单声道
        let mut done = 0usize;
        while done < out_frames {
            let n = (out_frames - done).min(buf.len());
            let slice = &mut buf[..n];
            slice.iter_mut().for_each(|v| *v = 0.0);
            mix_frames(&core, slice, 1, step);
            pcm.extend(slice.iter().map(|v| (v.clamp(-1.0, 1.0) * 32767.0) as i16));
            done += n;
        }

        // 写 44 头 WAV @ device_rate 单声道 16-bit。
        let data_len = (pcm.len() * 2) as u32;
        let mut h: Vec<u8> = Vec::with_capacity(44);
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&(36 + data_len).to_le_bytes());
        h.extend_from_slice(b"WAVE");
        h.extend_from_slice(b"fmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&1u16.to_le_bytes());
        h.extend_from_slice(&1u16.to_le_bytes());
        h.extend_from_slice(&device_rate.to_le_bytes());
        h.extend_from_slice(&(device_rate * 2).to_le_bytes());
        h.extend_from_slice(&2u16.to_le_bytes());
        h.extend_from_slice(&16u16.to_le_bytes());
        h.extend_from_slice(b"data");
        h.extend_from_slice(&data_len.to_le_bytes());
        let mut bytes = h;
        bytes.reserve(pcm.len() * 2);
        for s in &pcm {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        std::fs::write(&out_p, &bytes).unwrap();
        eprintln!("渲染完成: {} 帧 @ {}Hz → {}", pcm.len(), device_rate, out_p);
    }

    /// 软限幅:KNEE 下逐位透传,过冲平滑压入且恒不越界。
    #[test]
    fn soft_limit_transparent_below_knee_and_bounded_above() {
        // 透传区:单轨/轻响逐位不变。
        for x in [0.0f32, 0.25, -0.5, 0.75, 0.9, 0.95, -0.95] {
            assert_eq!(soft_limit(x), x, "KNEE 下必须逐位透传: {x}");
        }
        // 过冲区:被压、但绝不越界,且单调保号。
        for x in [0.96f32, 1.0, 1.5, 2.0, 5.0] {
            let y = soft_limit(x);
            assert!(y > 0.95 && y < 1.0, "过冲压入 (0.95,1.0): {x}->{y}");
            assert_eq!(soft_limit(-x), -y, "奇对称");
        }
        // 双讲典型过冲 acc=2.0 不再硬削顶到 1.0。
        assert!((soft_limit(2.0) - 0.9976).abs() < 1e-3);
        // 连续:KNEE 两侧不跳变。
        assert!((soft_limit(0.9501) - 0.95).abs() < 1e-3);
    }

    /// step=1(设备率=源率)双声道:两轨错位叠加,offset 之前只有先行轨。
    #[test]
    fn mixes_offset_tracks_on_shared_timeline() {
        // 轨A 从 0 起 [16384,16384,16384,16384](=0.5);轨B offset 2 采样起 [8192,8192](=0.25)。
        let a = mem_track(&[16384; 4], 0, "mic");
        let mut b = mem_track(&[8192; 2], 0, "system");
        b.offset_samples = 2;
        let core = core_of(vec![a, b]);
        let mut out = vec![0f32; 4 * 2]; // 4 帧 × 2 声道
        mix_frames(&core, &mut out, 2, 1.0);
        let frames: Vec<f32> = out.chunks(2).map(|c| c[0]).collect();
        assert!((frames[0] - 0.5).abs() < 1e-3, "offset 前仅轨A: {}", frames[0]);
        assert!((frames[1] - 0.5).abs() < 1e-3);
        assert!((frames[2] - 0.75).abs() < 1e-3, "重叠区 A+B: {}", frames[2]);
        assert!((frames[3] - 0.75).abs() < 1e-3);
        // 双声道同值
        assert_eq!(out[0], out[1]);
    }

    #[test]
    fn muted_track_is_skipped_and_unmute_restores() {
        let a = mem_track(&[16384; 4], 0, "mic");
        let b = mem_track(&[8192; 4], 0, "system");
        let core = core_of(vec![a, b]);
        core.tracks[1].muted.store(true, Ordering::Relaxed);
        let mut out = vec![0f32; 2];
        mix_frames(&core, &mut out, 1, 1.0);
        assert!((out[0] - 0.5).abs() < 1e-3, "静音轨不入混音: {}", out[0]);
        core.tracks[1].muted.store(false, Ordering::Relaxed);
        let mut out2 = vec![0f32; 2];
        mix_frames(&core, &mut out2, 1, 1.0);
        assert!((out2[0] - 0.75).abs() < 1e-3, "恢复后叠加: {}", out2[0]);
    }

    /// 播完自动置停 + 游标钉在末尾;暂停态输出静音、游标不动。
    #[test]
    fn stops_at_end_and_pause_outputs_silence() {
        let core = core_of(vec![mem_track(&[16384; 3], 0, "mic")]);
        let mut out = vec![0f32; 5];
        mix_frames(&core, &mut out, 1, 1.0);
        assert!(!core.playing.load(Ordering::Relaxed), "到尾自动停");
        assert_eq!(core.cursor(), 3.0, "游标钉在 total");
        assert_eq!(out[3], 0.0, "尾后静音");
        // 暂停态:重置游标后混音不推进、全静音
        core.set_cursor(0.0);
        let mut out2 = vec![0f32; 3];
        mix_frames(&core, &mut out2, 1, 1.0);
        assert_eq!(core.cursor(), 0.0, "暂停不推进");
        assert!(out2.iter().all(|v| *v == 0.0));
    }

    /// 分数步进(48k 设备放 16k 源,step=1/3)线性插值:上采样输出连续渐变。
    #[test]
    fn fractional_step_interpolates() {
        // 源 [0, 30000] → step 1/3 时输出 ≈ [0, 1/3, 2/3] × 0.9155
        let core = core_of(vec![mem_track(&[0, 30000], 0, "mic")]);
        let mut out = vec![0f32; 3];
        mix_frames(&core, &mut out, 1, 1.0 / 3.0);
        let unit = 30000.0 / 32768.0;
        assert!(out[0].abs() < 1e-6);
        assert!((out[1] - unit / 3.0).abs() < 1e-3, "1/3 处插值: {}", out[1]);
        assert!((out[2] - unit * 2.0 / 3.0).abs() < 1e-3, "2/3 处插值: {}", out[2]);
    }

    /// seek 语义:set_cursor 后从新位置继续。
    #[test]
    fn seek_moves_cursor() {
        let core = core_of(vec![mem_track(&[100, 200, 300, 30000], 0, "mic")]);
        core.set_cursor(3.0);
        let mut out = vec![0f32; 1];
        mix_frames(&core, &mut out, 1, 1.0);
        assert!((out[0] - 30000.0 / 32768.0).abs() < 1e-3, "从 seek 点取样: {}", out[0]);
    }

    /// 门控混音:mic 轨在压低区间内乘 DUCK_GAIN,区间外全量;system 轨(空表)不受影响。
    #[test]
    fn gated_mic_is_ducked_in_span_and_full_outside() {
        use crate::player_gate::{GateSpan, DUCK_GAIN};
        // mic 全程常值 8000;区间 [16000,48000) 压低(带 1280 渐变沿)。
        let mut mic = mem_track(&vec![8000i16; 64_000], 0, "mic");
        mic.gate = vec![GateSpan { start: 16_000, end: 48_000 }];
        let core = core_of(vec![mic]);
        let mut out = vec![0f32; 2]; // 单帧双声道,逐点采样
        let probe = |core: &Core, at: u64, out: &mut Vec<f32>| -> f32 {
            core.set_cursor(at as f64);
            mix_frames(core, out, 2, 1.0);
            out[0]
        };
        let full = 8000f32 / 32768.0;
        assert!((probe(&core, 1000, &mut out) - full).abs() < 1e-4, "区间外全量");
        let ducked = probe(&core, 30_000, &mut out);
        assert!((ducked - full * DUCK_GAIN).abs() < 1e-3, "腹地=DUCK: {ducked}");
        let edge = probe(&core, 16_000 + 640, &mut out);
        assert!(edge > ducked && edge < full, "渐变沿介于两者之间: {edge}");
    }

    /// 空 gate 表 = 现状:与未加门控的输出逐采样一致(既有测试的行为锚)。
    #[test]
    fn empty_gate_is_identity() {
        let a = mem_track(&[1000, 2000, 3000], 0, "mic");
        let core = core_of(vec![a]);
        let mut out = vec![0f32; 6];
        mix_frames(&core, &mut out, 2, 1.0);
        let expect = [1000f32, 1000., 2000., 2000., 3000., 3000.].map(|v| v / 32768.0);
        for (o, e) in out.iter().zip(expect) {
            assert!((o - e).abs() < 1e-6, "空表必须逐采样等于现状");
        }
    }
}
