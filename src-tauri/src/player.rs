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
    /// 装载代次(2026-08-10 排障):快速切笔记时多个 player_load 并发在跑,完成序由
    /// 装载耗时(解码/对齐/门控)决定而非请求序——后完成的旧笔记装载会覆盖当前内核
    /// (wrong-writer-wins),表现为点播放被掐、图标弹回、放错笔记的音频。
    /// 入口取号,只有仍是最新代次的装载允许装内核/起流。Arc:对齐闭包跑在
    /// spawn_blocking('static),要携带代次探针在正结论提交前复核(Codex 十二轮)。
    load_gen: Arc<AtomicU64>,
    /// 发布互斥(Codex P1):把「查代次→装内核→起流→复查」整段串行化。没有它,
    /// 过期装载 A 起流期间新装载 B 完成发布,A 的兜底 stop_stream 会无差别拆掉 B
    /// 刚装好的内核/停止通道——B 返回 Ok 但 play 报"尚未装载"。持锁期间过期装载
    /// 只可能清到自己刚发布的产物(更新装载的发布必须先拿到本锁)。
    publish: Mutex<()>,
}

impl Default for PlayerHandle {
    fn default() -> Self {
        Self {
            core: Mutex::new(None),
            stop_tx: Mutex::new(None),
            load_gen: Arc::new(AtomicU64::new(0)),
            publish: Mutex::new(()),
        }
    }
}

impl PlayerHandle {
    /// 新装载入口取号:此后到达的装载代次更大,本代次随之过期。
    fn begin_load(&self) -> u64 {
        self.load_gen.fetch_add(1, Ordering::SeqCst) + 1
    }
    /// 本代次是否仍是最新。装内核前与起流后各查一次(见 player_load 注释)。
    fn is_current(&self, gen: u64) -> bool {
        self.load_gen.load(Ordering::SeqCst) == gen
    }
}

/// player_load 的返回:总长之外带回本次装载的后端代次——前端 cleanup 用它做
/// **条件停止**(ifGen):旧组件迟到的 stop 不得作废新组件的装载(Codex 十轮 P1)。
#[derive(Debug, Clone, Serialize)]
pub struct LoadResult {
    pub total_ms: u64,
    pub gen: u64,
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
        return Err(crate::tr!("路径越界", "Path is outside the allowed directory"));
    }
    Ok(canon)
}

/// 触发对齐的轨长差门限:相对差与绝对差都要过。
/// 相对 1% 对应"每分钟错开 0.6s",一小时的会能拉开 36s;绝对 2s 挡掉短录音里
/// 那点收尾差(两轨停止时刻本就差几百毫秒,不是时基问题)。
const ALIGN_MIN_REL: f64 = 0.01;
const ALIGN_MIN_ABS_MS: i64 = 2_000;

/// canonical WAV 的时长(毫秒):16k 单声道 s16 → 每毫秒 32 字节。读不到算 0。
fn wav_duration_ms(p: &Path) -> i64 {
    std::fs::metadata(p).map(|m| ((m.len().saturating_sub(44)) / 32) as i64).unwrap_or(0)
}

/// 两轨长度差是否显著到值得跑一次(要几秒的)时基估计。
/// 相对 1% 对应"每分钟错开 0.6s";绝对 2s 挡掉短录音里的收尾差(两轨停止时刻本就
/// 差几百毫秒,不是时基问题)。任一轨读不出长度即不做。
fn alignment_worth_attempting(mic_ms: i64, sys_ms: i64) -> bool {
    if mic_ms <= 0 || sys_ms <= 0 {
        return false;
    }
    let diff = (sys_ms - mic_ms).abs();
    diff >= ALIGN_MIN_ABS_MS && (diff as f64) >= ALIGN_MIN_REL * sys_ms as f64
}

/// 对齐缓存是否可直接用。**缓存单元 = 对齐音轨 + align.json**,两者都必须比两条
/// 源轨新;缺一即整体重算。
///
/// 只比"音轨 vs mic"是不够的:①align.json 写失败或被用户删掉后,下次会因为音轨
/// 缓存还在而直接跳过,映射永远补不上、删 align.json 也回不到未纠正状态;
/// ②system 轨变化(续录)不会让缓存失效,会拿旧映射继续播。
fn aligned_cache_is_fresh(cache: &Path, align_json: Option<&Path>, sources: &[&Path]) -> bool {
    let newest_src = sources
        .iter()
        .filter_map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .max();
    let newer_than_src = |p: &Path| match (std::fs::metadata(p).and_then(|m| m.modified()), newest_src) {
        (Ok(t), Some(src)) => t >= src,
        _ => false,
    };
    newer_than_src(cache) && align_json.map(newer_than_src).unwrap_or(false)
}

/// 写跳过标记(负结果缓存):唯一名 tmp + create_new + rename——固定名 `fs::write`
/// 会跟随目录里预置的同名符号链接,把写入打到链接指向的任意可写文件上(Codex P1);
/// create_new 遇到已存在路径(含符号链接本身)直接失败不跟随,rename 替换的是目录项
/// 本身(预置链接被整体换成普通文件)。与 store::align::write 同一防线。
/// best-effort:失败只意味着下次装载再估一遍,不上抛。
/// 返回写入标记的**内容 token**(所有权凭据):回滚方读回内容一致才认「还是我那份」。
/// mtime 做凭据有双重歧义(并发覆写后 stat 到别人的 mtime;粗分辨率文件系统 mtime
/// 相等),内容 token 无歧义(Codex 十二轮 P2)。标记新鲜度判定只看 mtime,内容自由。
fn write_align_skip_marker(note_dir: &Path) -> Option<String> {
    use std::io::Write;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let token = format!("{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed));
    let marker = note_dir.join(crate::store::align::ALIGN_SKIP_FILE);
    let tmp = note_dir.join(format!("{}.{token}.tmp", crate::store::align::ALIGN_SKIP_FILE));
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp) {
        Ok(mut f) => {
            if f.write_all(token.as_bytes()).is_err() {
                let _ = std::fs::remove_file(&tmp);
                return None;
            }
            // 发布入锁(Codex 十三轮 P2):与回滚侧的「读 token-比对-删除」互斥,
            // 防回滚方读到自己的 token 后被挂起、本发布落地、恢复后误删新标记。
            let _fs = crate::store::align::ALIGN_FS_LOCK.lock().unwrap();
            if std::fs::rename(&tmp, &marker).is_err() {
                let _ = std::fs::remove_file(&tmp);
                return None;
            }
            Some(token)
        }
        Err(_) => None,
    }
}


/// 移除过期正映射并通知详情页重拉(负结论路径共用):align.json 在负结论下只会让
/// notes 读路径的转写时间戳与原始轨回放错位。持 ALIGN_FS_LOCK 并复验——正缓存已被
/// 重叠的新装载变新鲜(有效映射)则放弃删除;只清确凿过期的映射。
/// drift_ms=0 语义为「映射移除,回到原始时基」,消费方只按 note_id refresh。
fn remove_align_map_and_notify(
    app: &AppHandle,
    note_dir: &Path,
    align_json: &Path,
    cache: &Path,
    sources: &[&Path],
) {
    let _fs = crate::store::align::ALIGN_FS_LOCK.lock().unwrap();
    if aligned_cache_is_fresh(cache, Some(align_json), sources) {
        return; // 新装载刚提交的有效映射,不是要清的过期货
    }
    // 新鲜映射护栏(Codex 十七轮):映射比全部源轨新 = 刚被有意发布(mix_regen 或
    // 另一装载),即便配套音轨缓存缺失/未建也不是陈旧货——本函数只清早于当前源的
    // 过期映射。regen 发布映射与烘焙 mixed 之间的窗口由此免疫清理。
    let newest_src = sources
        .iter()
        .filter_map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .max();
    let map_is_newer = matches!(
        (std::fs::metadata(align_json).and_then(|m| m.modified()), newest_src),
        (Ok(am), Some(src)) if am >= src
    );
    if map_is_newer {
        return;
    }
    // 成品轨护栏(Codex 十四轮 P2):mix_regen 可能已把本映射烘进 mixed.m4a——删映射
    // 会让转写回原始时基,而成品轨仍是纠正过的,按成品轨回放的定位/高亮从此错位。
    // mixed 不老于映射(烘焙不早于映射发布)即保留映射:成品轨一致性优先于双轨
    // 原始回放(负结论下双轨本就不换轨,保留映射维持既有观感,不引入新错位)。
    let mixed = note_dir.join("mixed.m4a");
    let baked = matches!(
        (
            std::fs::metadata(&mixed).and_then(|m| m.modified()),
            std::fs::metadata(align_json).and_then(|m| m.modified()),
        ),
        (Ok(mm), Ok(am)) if mm >= am
    );
    if baked {
        return;
    }
    if std::fs::remove_file(align_json).is_ok() {
        if let Some(note_id) = note_dir.file_name().and_then(|s| s.to_str()) {
            let _ = app.emit(
                "note_realigned",
                crate::ipc::NoteRealignedEvent { note_id: note_id.to_string(), drift_ms: 0 },
            );
        }
    }
}

/// 跳过标记是否有效:比全部源轨新即有效(负结果缓存,见 store::align::ALIGN_SKIP_FILE)。
fn align_skip_is_fresh(marker: &Path, sources: &[&Path]) -> bool {
    let newest_src = sources
        .iter()
        .filter_map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .max();
    match (std::fs::metadata(marker).and_then(|m| m.modified()), newest_src) {
        (Ok(t), Some(src)) => t >= src,
        _ => false,
    }
}

/// 提交对齐结果:音轨先写临时文件 → 校验长度 → 原子 rename → **最后**发布映射。
/// 返回是否整体提交成功;任一步失败都会清理临时文件并返回 false(下次装载重算)。
///
/// 顺序不能反。先发布映射有个真实的坏窗口:映射一落盘,转写时间戳立刻按新时基显示,
/// 而音轨若在随后的写入中被磁盘写满/中断截断,回放还是原始音频——两边当场对不上;
/// 更糟的是截断文件的 mtime 仍比源轨新,下次装载会把"新映射 + 半截音轨"判成 fresh
/// 直接拿来用。映射是最后一步,它存在即代表音轨已就位。
fn commit_aligned(
    cache: &Path,
    render: impl FnOnce(&mut std::fs::File) -> std::io::Result<u64>,
    note_dir: &Path,
    map: &crate::player_align::TimeMap,
    is_current: impl Fn() -> bool,
) -> bool {
    // 唯一名 + create_new:同 store::align::write 的理由(不跟随预置符号链接)。
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let tmp = cache.with_extension(format!(
        "{}-{}.wav.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    // 直接流式写进临时文件:生产路径不再返回/持有整轨 Vec(见 render_aligned_to)。
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .and_then(|mut f| render(&mut f).and_then(|n| f.sync_all().map(|_| n)));
    let complete = match written {
        Ok(n) => std::fs::metadata(&tmp).map(|m| m.len()).ok() == Some(n),
        Err(_) => false,
    };
    if !complete {
        eprintln!("回放对齐: 对齐音轨写入不完整,本次不对齐(下次装载重试)");
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    // 发布临界区(Codex 十三轮 P1):代次检查必须先于缓存音轨 rename,且 rename 与
    // 映射写入同锁——否则过期渲染可在新装载的 cache+map 配对之上单独覆写音轨,
    // 覆写后的新 mtime 让 aligned_cache_is_fresh 永远接受这对错配。
    let _fs = crate::store::align::ALIGN_FS_LOCK.lock().unwrap();
    if !is_current() {
        eprintln!("回放对齐: 装载已被更新的请求取代,正结论不发布");
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    // std::fs::rename 在 Windows 走 MoveFileEx + MOVEFILE_REPLACE_EXISTING,
    // 覆盖已有目标是既定语义,无需另写平台分支。
    if let Err(e) = std::fs::rename(&tmp, cache) {
        eprintln!("回放对齐: 对齐音轨发布失败({e}),本次不对齐(下次装载重试)");
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    if let Err(e) = crate::store::align::write(note_dir, map) {
        eprintln!("回放对齐: 映射落盘失败({e}),本次不对齐(下次装载重试)");
        return false;
    }
    true
}

/// 纠正后的 mic 轨在笔记时间轴上的 offset。
///
/// 它铺在 system 的本地时基上,但起点未必是 sys_local 0:mic 先开录时
/// (mic_off < sys_off)起点是负的,`render_aligned` 已把那段渲进去,这里把 offset
/// 相应前移,轨在**全局**时间轴上的起点仍是 mic 原来的起点。
pub(crate) fn aligned_track_offset_ms(sys_off_ms: u64, map: &crate::player_align::TimeMap) -> u64 {
    (sys_off_ms as i64 + crate::player_align::map_ms_signed(map, 0)).max(0) as u64
}

/// 把 mic 轨按实测时基映射重采样回 system 的时基(就地改写 plan)。
///
/// 保守到底:轨长差不显著、估不出可信映射、渲染或写盘失败——任一情形都原样返回,
/// 回放行为与不对齐时完全一致。结果按两条源轨的路径+mtime 落缓存,同一笔记只算一次。
async fn align_mic_track(
    app: &AppHandle,
    state: &State<'_, PlayerHandle>,
    gen: u64,
    plan: &mut [(PathBuf, u64, String)],
    durable: &[(String, PathBuf)],
    note_dir: Option<&Path>,
) {
    let idx = |src: &str| plan.iter().position(|(_, _, s)| s == src);
    // 持久源路径(原始 m4a/wav):标记新鲜度与估计期间变更检测的锚点——解码缓存
    // 会淘汰重建,不配当锚点。找不到对应项时回退缓存路径(行为同旧,不失效)。
    let durable_of = |src: &str, fallback: &Path| -> PathBuf {
        durable
            .iter()
            .find(|(s, _)| s == src)
            .map(|(_, p)| p.clone())
            .unwrap_or_else(|| fallback.to_path_buf())
    };
    let (Some(mi), Some(si)) = (idx("mic"), idx("system")) else {
        return; // 单轨笔记没有跨轨时基可言
    };
    let (mic_path, mic_off) = (plan[mi].0.clone(), plan[mi].1);
    let (sys_path, sys_off) = (plan[si].0.clone(), plan[si].1);
    let (mic_src, sys_src) = (durable_of("mic", &mic_path), durable_of("system", &sys_path));
    let (dm, ds) = (wav_duration_ms(&mic_path), wav_duration_ms(&sys_path));
    if !alignment_worth_attempting(dm, ds) {
        return;
    }
    let Ok(cache) = cache_path_for(app, &mic_path.with_extension("aligned.m4a")) else {
        return;
    };
    let Some(note_dir) = note_dir else { return };
    let align_json = note_dir.join(crate::store::align::ALIGN_FILE);

    if !aligned_cache_is_fresh(&cache, Some(&align_json), &[&mic_path, &sys_path]) {
        // 负结果缓存:上次已判定"估不出/不值得纠正"且源轨未变,不再重跑 60-100s 的
        // 估计(2026-08-10 排障:大笔记每次进页白跑一遍,装载期间播放无响应)。
        let skip_marker = note_dir.join(crate::store::align::ALIGN_SKIP_FILE);
        if align_skip_is_fresh(&skip_marker, &[&mic_src, &sys_src]) {
            // 自愈(Codex P1):此处若还躺着 align.json,它必然过期(新鲜的正缓存在上方
            // 分支就命中了)——留着它,notes 读路径仍按旧映射改写转写时间戳,而回放走
            // 原始轨,两边永久错位。删之并通知页面重拉(Codex P2:页面在装载前已按旧
            // 映射改写过时间戳,不通知就一直错位到下次刷新;drift_ms=0 表示映射移除)。
            // 代次门(Codex 五轮 P1):同一笔记两次装载重叠时,过期装载不得删新装载
            // 可能刚提交的有效映射——只有仍是最新代次才有资格做对齐副作用。
            if state.is_current(gen) {
                // 清理复验锚**缓存对**(Codex 十六轮:与顶部新鲜度判定同口径)——缓存重建后
                // 旧 cache+map 对确凿过期,此时若拿持久源复验会误判新鲜、保留映射,而
                // plan 未换对齐轨,转写映射与原始回放错位。持久源只锚跳过标记。
                remove_align_map_and_notify(app, note_dir, &align_json, &cache, &[&mic_path, &sys_path]);
            }
            return;
        }
        eprintln!(
            "回放对齐: 两轨长度差 {:.1}s(mic {:.0}s / system {:.0}s),估计时基映射…",
            (ds - dm).abs() as f64 / 1000.0,
            dm as f64 / 1000.0,
            ds as f64 / 1000.0
        );
        let (m2, s2, c2, nd) =
            (mic_path.clone(), sys_path.clone(), cache.clone(), note_dir.to_path_buf());
        let (md, sd) = (mic_src.clone(), sys_src.clone());
        let gen_probe = state.load_gen.clone();
        let built = tauri::async_runtime::spawn_blocking(move || -> Option<u64> {
            // mmap 而不是 read:估计要同时看两条完整音轨,一小时双轨读进堆里就是
            // ~230MB 常驻,而 player_align 已改成按字节视图逐样本取值,页由系统按需
            // 调入/回收即可(与回放热路径同一套 mmap 策略)。
            let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
            // 估计前采**持久源**快照(原始 m4a/wav,非解码缓存):估计要跑几十秒,期间
            // 续录会换掉源文件——那时结论只代表旧音频,标记不得发布(Codex P2:晚发布
            // 的标记 mtime 反而更新,会把针对新音频的必要重估压掉)。锚持久源还避免
            // 缓存淘汰重建的 mtime 噪声(Codex 十五轮)。
            let pre = (mtime(&md), mtime(&sd));
            let map_file = |p: &Path| -> Option<Mmap> {
                let f = std::fs::File::open(p).ok()?;
                unsafe { Mmap::map(&f).ok() }
            };
            let mic = map_file(&m2)?;
            let sys = map_file(&s2)?;
            // 负结果也落盘(空文件标记):mmap 失败(上方 ?)不落——那是环境性故障,
            // 该重试;"估不出/不值得"是对这份音频的稳定判定,源轨不变结论不变。
            // 过期正映射的删除与页面通知在 await 之后的异步侧做(那里有 AppHandle 能
            // 发 note_realigned);本闭包只负责标记。崩在两步之间 = 标记在、旧映射在,
            // 由上方 skip-fresh 分支的自愈补删,不留永久错位。
            let mark_skip = || {
                if (mtime(&md), mtime(&sd)) != pre {
                    eprintln!("回放对齐: 估计期间源轨已变,不发布跳过标记(下次装载重估)");
                    return;
                }
                let ours = write_align_skip_marker(&nd);
                // 发布后复验(Codex 九轮 P2):比对与落盘之间源又被改写,标记 mtime 反而
                // 比新源新,会压掉对新音频的重估——发现快照变了就撤回。撤回只删**自己
                // 那份**(mtime 凭据一致,Codex 十一轮 P2):并发新装载已覆写的有效标记
                // 不动。撤回后源再变,标记比新源旧本就判不新鲜,链路自洽。
                if (mtime(&md), mtime(&sd)) != pre {
                    // 读-比对-删除与标记发布同锁(Codex 十三轮 P2):否则读到自己 token
                    // 后被挂起,新标记落地,恢复后的删除仍会误删。
                    let marker = nd.join(crate::store::align::ALIGN_SKIP_FILE);
                    let _fs = crate::store::align::ALIGN_FS_LOCK.lock().unwrap();
                    let content = std::fs::read_to_string(&marker).ok();
                    if ours.is_some() && content == ours {
                        eprintln!("回放对齐: 标记落盘期间源轨已变,撤回跳过标记");
                        let _ = std::fs::remove_file(&marker);
                    }
                }
            };
            let Some(a) = crate::player_align::estimate(&mic, mic_off, &sys, sys_off) else {
                eprintln!("回放对齐: 估不出可信映射,记录跳过标记(源轨变更后重估)");
                mark_skip();
                return None;
            };
            if !crate::player_align::worth_correcting(&a) {
                eprintln!("回放对齐: 实测漂移仅 {:.2}s,不值得纠正(记录跳过标记)", a.drift_secs);
                mark_skip();
                return None;
            }
            eprintln!(
                "回放对齐: 探针 {}/{} 命中,最大漂移 {:.1}s,按映射重采样 mic 轨",
                a.accepted, a.probes, a.drift_secs
            );
            let render = |f: &mut std::fs::File| {
                crate::player_align::render_aligned_to(&mic, &a.map, f).map(|(n, _)| n)
            };
            commit_aligned(&c2, render, &nd, &a.map, || gen_probe.load(Ordering::SeqCst) == gen)
                .then(|| (a.drift_secs * 1000.0) as u64)
        })
        .await
        .unwrap_or_else(|e| {
            eprintln!("回放对齐: 任务失败({e}),本次回放不对齐");
            None
        });
        let Some(drift_ms) = built else {
            // 负结论(估不出/不值得/任务失败):过期正映射一并移除并通知页面重拉
            // (Codex P1+P2)——负结论确立后旧 align.json 只会让转写时间戳与原始轨
            // 回放错位,页面在装载前已按旧映射改写过时间戳,不通知就错位到下次刷新。
            // 代次门(Codex 五轮 P1):过期装载的负结论可能晚于新装载的有效提交抵达,
            // 无差别删除会拆掉新映射;过期即放弃副作用(标记写入另有 mtime 快照门)。
            if state.is_current(gen) {
                // 清理复验锚**缓存对**(Codex 十六轮:与顶部新鲜度判定同口径)——缓存重建后
                // 旧 cache+map 对确凿过期,此时若拿持久源复验会误判新鲜、保留映射,而
                // plan 未换对齐轨,转写映射与原始回放错位。持久源只锚跳过标记。
                remove_align_map_and_notify(app, note_dir, &align_json, &cache, &[&mic_path, &sys_path]);
            }
            return;
        };
        // 详情页手里那份段是旧时基的,通知它整页重拉。
        if let Some(note_id) = note_dir.file_name().and_then(|s| s.to_str()) {
            let _ = app.emit(
                "note_realigned",
                crate::ipc::NoteRealignedEvent { note_id: note_id.to_string(), drift_ms },
            );
        }
    }
    // 起点从映射自身取(map.apply(0)),命中缓存时也算得出,不必额外存一份。
    let Some(map) = crate::store::align::read(note_dir) else {
        return; // 映射读不回来就不换轨:宁可不对齐,也不能按错的 offset 铺
    };
    plan[mi] = (cache, aligned_track_offset_ms(sys_off, &map), "mic".to_string());
}

/// 装载音轨并(重)起输出流。m4a 先解码到缓存(秒级,spawn_blocking 不占主线程,
/// 前端 await 本命令即拿到就绪信号);返回时间轴总长 ms。
#[tauri::command]
pub async fn player_load(
    app: AppHandle,
    state: State<'_, PlayerHandle>,
    tracks: Vec<LoadTrack>,
) -> Result<LoadResult, String> {
    // 入口取号+拆除同锁原子(Codex 九轮 P1):取号与拆除若分离,老装载在取号后挂起、
    // 新装载已发布的情形下,老装载恢复后的无差别拆除会拆掉新核。锁内成对后,取号时刻
    // 本代次必为最新、锁内拆到的只可能是旧核;发布段与 player_stop 共用同一把锁定序。
    // 锁在入口段结束即释放,不覆盖后续解码/对齐长路径。
    let gen = {
        let _publish = state.publish.lock().unwrap();
        let g = state.begin_load();
        stop_stream(&state);
        g
    };

    // 路径校验 + m4a 解码规划(阻塞段全部挪到 spawn_blocking)。
    // note_dir:取首条轨校验后路径的父目录(m4a 会被换成缓存路径,故须在换之前取,
    // 各轨同属一个笔记,取一次即可)——segments.jsonl 与音轨文件同目录。
    let mut note_dir: Option<PathBuf> = None;
    let mut plan: Vec<(PathBuf, u64, String)> = Vec::new();
    // 持久源(校验后的原始 m4a/wav 路径):跳过标记的新鲜度锚点。解码缓存会被 7 天
    // 淘汰再重建,mtime 随之变新——拿缓存当锚点,录音没变也会每次淘汰后白跑一遍
    // 60-100s 估计(Codex 十五轮 P2)。
    let mut durable: Vec<(String, PathBuf)> = Vec::new();
    for t in &tracks {
        let src = validate_under_notes(&app, Path::new(&t.path))?;
        let src_orig = src.clone();
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
                .map_err(|e| crate::tr!("解码任务失败: {e}", "Decode task failed: {e}"))?
                .map_err(|e| crate::tr!("解码 m4a 失败: {e}", "Failed to decode m4a: {e}"))?;
            }
            cache
        } else {
            src
        };
        plan.push((wav, t.offset_ms, t.source.clone()));
        durable.push((t.source.clone(), src_orig));
    }

    // 跨轨时基对齐:历史录音里 mic 轨可能整条被压缩(采集侧把设备实际出样速率记错,
    // 见 player_align 模块头),两轨按 offset 一铺,同一句话就被拉开成两处。这里在
    // 门控之前把 mic 轨按实测映射重采样回 system 的时基——门控的电平判据要求两轨对齐
    // 在 400ms 内,不先把时基掰正,门控只会压错地方。
    //
    // 只对"轨长明显对不上"的笔记做:估计要跑几秒,健康的笔记不该为它买单。
    align_mic_track(&app, &state, gen, &mut plan, &durable, note_dir.as_deref()).await;

    // 回放门控:按两轨逐帧电平构建 mic 压低区间(任何失败空表降级=不门控)。
    // 判据不再取自转写段——回声残影本身会被识别成 mic 段,旧的"mic 有段即保护"
    // 恰好把回声最响处挖成保护区,详见 player_gate 模块头。
    // 读两轨包络是一次顺序读:这里已在 spawn_blocking 之后的解码路径上,
    // 与既有 m4a 解码同量级,不额外阻塞 UI。
    let find = |src: &str| {
        plan.iter()
            .find(|(_, _, s)| s == src)
            .map(|(p, off, _)| (p.clone(), *off))
    };
    let gate_spans = match (find("mic"), find("system")) {
        (Some((mic, mic_off)), Some((sys, sys_off))) => {
            tauri::async_runtime::spawn_blocking(move || {
                crate::player_gate::build_gate_from_audio(&mic, mic_off, &sys, sys_off)
            })
            .await
            .unwrap_or_else(|e| {
                eprintln!("回放门控: 构建任务失败({e}),本次回放不做门控");
                Vec::new()
            })
        }
        _ => {
            // 单轨笔记没有跨轨重影可言,不门控即正解。
            Vec::new()
        }
    };
    if !gate_spans.is_empty() {
        eprintln!(
            "回放门控: {} 个压低区间(mic 轨,{:.0}dB,电平判据)",
            gate_spans.len(),
            20.0 * crate::player_gate::DUCK_GAIN.log10()
        );
    }

    // mmap 装载 + Core 组装。
    let mut loaded = Vec::new();
    for (wav, offset_ms, source) in plan {
        let f = std::fs::File::open(&wav).map_err(|e| crate::tr!("打开音轨失败: {e}", "Failed to open the audio track: {e}"))?;
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
        let mmap = unsafe { Mmap::map(&f) }.map_err(|e| crate::tr!("mmap 失败: {e}", "mmap failed: {e}"))?;
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
        return Err(crate::tr!("没有可播放的音轨", "No playable audio track"));
    }
    let total_samples = loaded.iter().map(|t| t.offset_samples + t.len_samples).max().unwrap_or(0);
    let core = Arc::new(Core {
        tracks: loaded,
        total_samples,
        cursor_bits: AtomicU64::new(0f64.to_bits()),
        playing: AtomicBool::new(false),
    });
    // 发布段(持 publish 互斥,Codex P1):查代次→装内核→起流→复查整段串行。
    // 过期装载在首查即弃(未触碰任何共享态);持锁期间发布后才发现过期(新装载
    // 恰在本段进行中进入并 bump 代次)时,兜底 stop_stream 清到的只会是自己刚
    // 发布的产物——更新装载的发布必须先拿本锁,不可能被误拆。
    let _publish = state.publish.lock().unwrap();
    {
        let mut g = state.core.lock().unwrap();
        if !state.is_current(gen) {
            return Err(crate::tr!("装载已被更新的请求取代", "Load superseded by a newer request"));
        }
        *g = Some(core.clone());
    }
    if let Err(e) = start_stream(&app, &state, core) {
        stop_stream(&state); // 起流失败不留残核:否则后续 play 假成功、UI 卡"播放中"
        return Err(e);
    }
    // 起流后复查:装内核→起流之间若有新装载**进入**(入口 stop_stream 不走 publish
    // 锁,会清掉本次刚装的内核),刚起的流带着过期内核复活。过期即自我了断。
    if !state.is_current(gen) {
        stop_stream(&state);
        return Err(crate::tr!("装载已被更新的请求取代", "Load superseded by a newer request"));
    }
    Ok(LoadResult { total_ms: (total_samples as f64 / SRC_RATE * 1000.0) as u64, gen })
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
                .ok_or_else(|| crate::tr!("找不到输出设备", "No audio output device found"))?;
            let supported = device.default_output_config().map_err(|e| e.to_string())?;
            if supported.sample_format() != cpal::SampleFormat::F32 {
                return Err(crate::tr!(
                    "输出格式不支持: {}(仅支持 f32)",
                    "Unsupported output format: {} (only f32 is supported)",
                    supported.sample_format()
                ));
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
        .map_err(|_| crate::tr!("输出流启动超时", "Timed out starting the output stream"))??;
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
    let core = g.as_ref().ok_or_else(|| crate::tr!("尚未装载音轨", "No audio track loaded"))?;
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
    let core = g.as_ref().ok_or_else(|| crate::tr!("尚未装载音轨", "No audio track loaded"))?;
    core.playing.store(false, Ordering::Relaxed);
    emit_pos(&app, core);
    Ok(())
}

#[tauri::command]
pub fn player_seek(app: AppHandle, state: State<'_, PlayerHandle>, ms: u64) -> Result<(), String> {
    let g = state.core.lock().unwrap();
    let core = g.as_ref().ok_or_else(|| crate::tr!("尚未装载音轨", "No audio track loaded"))?;
    let target = (ms as f64 / 1000.0 * SRC_RATE).min(core.total_samples as f64);
    core.set_cursor(target);
    emit_pos(&app, core);
    Ok(())
}

#[tauri::command]
pub fn player_set_muted(state: State<'_, PlayerHandle>, source: String, muted: bool) -> Result<(), String> {
    let g = state.core.lock().unwrap();
    let core = g.as_ref().ok_or_else(|| crate::tr!("尚未装载音轨", "No audio track loaded"))?;
    for t in &core.tracks {
        if t.source == source {
            t.muted.store(muted, Ordering::Relaxed);
        }
    }
    Ok(())
}

#[tauri::command]
pub fn player_stop(state: State<'_, PlayerHandle>, if_gen: Option<u64>) -> Result<(), String> {
    // 停止=清空播放意图,同时推进装载代次让**在途装载**过期(Codex P1):组件销毁后
    // 只清流不作废代次的话,仍在解码/对齐中的装载完成时会复活流+重装内核,排队的
    // play 还会对已离开的笔记开火。推进代次后它们在发布段被拦下,自行返回「已取代」。
    // 取号+拆除进 publish 锁(Codex 九轮 P1):挂起在两步之间的 stop 恢复后不得拆掉
    // 后续装载已发布的核——锁内成对 + 发布段同锁,定序即正确。
    // if_gen 归属条件(Codex 十轮 P1):旧组件 fire-and-forget 的 stop 可能晚于新组件
    // 的装载执行——带上自己最后一次成功装载的代次,后端代次已前进(有更新意图)就
    // no-op,不作废别人的装载;不带条件(None)保留无条件停止语义(显式全停场景)。
    let _publish = state.publish.lock().unwrap();
    if let Some(g) = if_gen {
        if !state.is_current(g) {
            return Ok(());
        }
    }
    state.begin_load();
    stop_stream(&state);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 对齐缓存的四个决策(从 align_mic_track 抽出的纯逻辑;原先只有真机路径覆盖) ──

    fn tmap() -> crate::player_align::TimeMap {
        crate::player_align::TimeMap::new(vec![(0.0, 0.0), (100.0, 109.0)]).unwrap()
    }

    /// 写一个 canonical WAV 骨架(只关心长度,内容无所谓)。
    fn wav_of_ms(path: &Path, ms: u64) {
        let mut b = vec![0u8; HEADER_LEN as usize];
        b.extend(std::iter::repeat(0u8).take(ms as usize * 32));
        std::fs::write(path, b).unwrap();
    }

    /// 把 mtime 推到"比现在晚",用来模拟源轨被更新(续录)。
    fn touch_newer(path: &Path) {
        // 重写一遍即可刷新 mtime;分辨率不足时补一次极短等待。
        std::thread::sleep(std::time::Duration::from_millis(20));
        let b = std::fs::read(path).unwrap();
        std::fs::write(path, b).unwrap();
    }

    #[test]
    fn alignment_trigger_needs_both_relative_and_absolute_gap() {
        assert!(!alignment_worth_attempting(0, 100_000), "读不出长度不做");
        assert!(!alignment_worth_attempting(100_000, 0));
        // 30 分钟录音差 1.5s:过不了绝对门限(收尾差,不是时基问题)
        assert!(!alignment_worth_attempting(1_798_500, 1_800_000));
        // 短录音差 3s 但只占 1.5%……绝对与相对都要过,这里两者都过
        assert!(alignment_worth_attempting(197_000, 200_000));
        // 长录音差 3s:绝对过了,相对(0.17%)没过 → 不做
        assert!(!alignment_worth_attempting(1_797_000, 1_800_000));
        // 真实那场:1665s vs 1813s
        assert!(alignment_worth_attempting(1_665_188, 1_812_928));
        // 反方向(mic 比 system 长)同样要触发
        assert!(alignment_worth_attempting(1_812_928, 1_665_188));
    }

    /// 缓存单元 = 对齐音轨 + align.json,两者都必须比**两条**源轨新。
    /// 这条锁住三个曾经踩过的坑:删 align.json 要能回到未纠正、映射写失败要能重试、
    /// system 轨续录后不得继续用旧映射。
    #[test]
    fn aligned_cache_requires_both_artifacts_newer_than_both_sources() {
        let tmp = tempfile::tempdir().unwrap();
        let (mic, sys) = (tmp.path().join("mic.wav"), tmp.path().join("system.wav"));
        let (cache, aj) = (tmp.path().join("aligned.wav"), tmp.path().join("align.json"));
        wav_of_ms(&mic, 10);
        wav_of_ms(&sys, 10);
        let srcs: [&Path; 2] = [&mic, &sys];

        assert!(!aligned_cache_is_fresh(&cache, Some(&aj), &srcs), "音轨缺失即不新鲜");
        touch_newer(&mic);
        std::fs::write(&cache, b"x").unwrap();
        assert!(!aligned_cache_is_fresh(&cache, Some(&aj), &srcs), "缺 align.json 即不新鲜");
        std::fs::write(&aj, b"{}").unwrap();
        assert!(aligned_cache_is_fresh(&cache, Some(&aj), &srcs), "两者俱全且比源轨新");

        // 删掉映射 → 回到未纠正(不能因为音轨缓存还在就继续用)
        std::fs::remove_file(&aj).unwrap();
        assert!(!aligned_cache_is_fresh(&cache, Some(&aj), &srcs));
        std::fs::write(&aj, b"{}").unwrap();
        assert!(aligned_cache_is_fresh(&cache, Some(&aj), &srcs));

        // system 轨续录变新 → 缓存整体失效
        touch_newer(&sys);
        assert!(!aligned_cache_is_fresh(&cache, Some(&aj), &srcs), "system 轨更新须让缓存失效");

        // 无笔记目录(拿不到 align.json 路径)一律不新鲜
        assert!(!aligned_cache_is_fresh(&cache, None, &srcs));
    }

    // ── 装载代次守卫(2026-08-10 排障):快速切笔记时多个 player_load 并发,完成序
    // 由装载耗时决定,后完成的旧笔记装载会覆盖当前笔记内核(wrong-writer-wins),
    // 用户点播放被掐、放错笔记音频。只有最新代次允许装内核。 ──
    #[test]
    fn load_generation_only_newest_wins() {
        let h = PlayerHandle::default();
        let g1 = h.begin_load();
        assert!(h.is_current(g1), "唯一在飞的装载即最新");
        let g2 = h.begin_load();
        assert!(!h.is_current(g1), "更新的装载进入后,旧装载过期");
        assert!(h.is_current(g2));
    }

    /// 跳过标记(估不出/不值得纠正的负缓存):此前这两种结局不落任何产物,大笔记
    /// 每次进页都重跑 60-100s 估计。标记比两条源轨都新才有效,源轨更新(续录)即重估。
    #[test]
    fn align_skip_marker_freshness() {
        let tmp = tempfile::tempdir().unwrap();
        let (mic, sys) = (tmp.path().join("mic.wav"), tmp.path().join("system.wav"));
        wav_of_ms(&mic, 10);
        wav_of_ms(&sys, 10);
        let marker = tmp.path().join(crate::store::align::ALIGN_SKIP_FILE);
        let srcs: [&Path; 2] = [&mic, &sys];
        assert!(!align_skip_is_fresh(&marker, &srcs), "无标记即重估");
        std::fs::write(&marker, b"").unwrap();
        assert!(align_skip_is_fresh(&marker, &srcs), "标记比源新:跳过重估");
        touch_newer(&mic);
        assert!(!align_skip_is_fresh(&marker, &srcs), "源轨更新(续录)后标记失效");
    }

    /// 预置同名符号链接不得被写穿(Codex P1):固定名 fs::write 会跟随链接把内容打到
    /// 任意可写文件;安全写法(tmp+create_new+rename)应整体替换链接为普通文件。
    #[cfg(unix)]
    #[test]
    fn skip_marker_never_follows_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let victim = tmp.path().join("victim.txt");
        std::fs::write(&victim, b"precious").unwrap();
        let note = tmp.path().join("note");
        std::fs::create_dir(&note).unwrap();
        let marker = note.join(crate::store::align::ALIGN_SKIP_FILE);
        std::os::unix::fs::symlink(&victim, &marker).unwrap();
        write_align_skip_marker(&note);
        assert_eq!(std::fs::read(&victim).unwrap(), b"precious", "链接目标不得被写穿");
        let meta = std::fs::symlink_metadata(&marker).unwrap();
        assert!(meta.file_type().is_file(), "标记应把链接整体替换为普通空文件");
    }

    #[test]
    fn commit_aligned_publishes_audio_then_map() {
        let tmp = tempfile::tempdir().unwrap();
        let note = tmp.path().join("note");
        std::fs::create_dir(&note).unwrap();
        let cache = tmp.path().join("aligned.wav");
        assert!(commit_aligned(&cache, |f| std::io::Write::write_all(f, b"PCMDATA").map(|_| 7), &note, &tmap(), || true));
        assert_eq!(std::fs::read(&cache).unwrap(), b"PCMDATA");
        assert_eq!(crate::store::align::read(&note), Some(tmap()));
        // 不留临时文件
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "临时文件应已清理: {leftovers:?}");
    }

    /// 音轨没能发布时,**映射绝不能已经发布**——否则转写时间戳按新时基显示、
    /// 回放却还是原始音频,两边当场对不上,而且下次装载可能把半截状态判成 fresh。
    #[test]
    fn commit_aligned_never_publishes_map_when_audio_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let note = tmp.path().join("note");
        std::fs::create_dir(&note).unwrap();
        // 让最终 rename 失败:目标是个非空目录
        let cache = tmp.path().join("aligned.wav");
        std::fs::create_dir(&cache).unwrap();
        std::fs::write(cache.join("occupied"), b"x").unwrap();

        assert!(!commit_aligned(&cache, |f| std::io::Write::write_all(f, b"PCMDATA").map(|_| 7), &note, &tmap(), || true));
        assert!(
            crate::store::align::read(&note).is_none(),
            "音轨发布失败时映射不得落盘(否则时间戳与音频对不上)"
        );
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "失败路径也要清理临时文件: {leftovers:?}");
    }

    /// 连续提交不得因临时名冲突而失败(唯一名要真的唯一)。
    #[test]
    fn commit_aligned_is_repeatable() {
        let tmp = tempfile::tempdir().unwrap();
        let note = tmp.path().join("note");
        std::fs::create_dir(&note).unwrap();
        let cache = tmp.path().join("aligned.wav");
        for i in 0..3u8 {
            assert!(commit_aligned(&cache, |f| std::io::Write::write_all(f, &[i; 16]).map(|_| 16), &note, &tmap(), || true), "第 {i} 次提交");
        }
        assert_eq!(std::fs::read(&cache).unwrap(), vec![2u8; 16], "最后一次胜出");
    }

    #[test]
    fn aligned_offset_follows_the_maps_own_start() {
        // 起点为 0:沿用 system 的 offset
        assert_eq!(aligned_track_offset_ms(5_000, &tmap()), 5_000);
        // mic 先开录 8s(起点 -8s):offset 相应前移,开头内容不被顶掉
        let early = crate::player_align::TimeMap::new(vec![(0.0, -8.0), (100.0, 101.0)]).unwrap();
        assert_eq!(aligned_track_offset_ms(20_000, &early), 12_000);
        // 前移到负数则夹到 0(全局起点不可能为负)
        assert_eq!(aligned_track_offset_ms(3_000, &early), 0);
        // mic 后开录 8s
        let late = crate::player_align::TimeMap::new(vec![(0.0, 8.0), (100.0, 109.0)]).unwrap();
        assert_eq!(aligned_track_offset_ms(0, &late), 8_000);
    }

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

        // 真轨道偏移:audio.json。
        let meta = crate::store::audio::load_audio_meta(&note);
        let off = |s: &str| meta.tracks.get(s).map(|t| t.offset_ms).unwrap_or(0);
        let (mic_bytes, sys_bytes) = (decode("mic"), decode("system"));

        // 真门控:两轨电平判据(VN_MIX_NO_GATE=1 渲染无门控对照)。
        let gate = if std::env::var("VN_MIX_NO_GATE").is_ok() {
            Vec::new()
        } else {
            crate::player_gate::build_gate_from_wav_bytes(
                &mic_bytes,
                off("mic"),
                &sys_bytes,
                off("system"),
            )
        };
        eprintln!("门控压低区间: {} 个", gate.len());

        let mic = track_from_canonical_wav(mic_bytes, off("mic"), "mic", gate);
        let sys = track_from_canonical_wav(sys_bytes, off("system"), "system", Vec::new());
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
