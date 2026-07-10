//! 停录后把笔记 WAV 转成 AAC m4a(约 8 倍压缩),续录时再解回 WAV。
//!
//! 为什么走子进程 afconvert/afinfo:它们是 macOS 系统内建工具(/usr/bin 下),
//! 转码走系统 AudioToolbox 的 AAC 编码器,零第三方依赖、零额外二进制体积,也不必
//! 在 Rust 侧维护一个 AAC 编解码栈。代价是同步 fork/exec,但转码只在停录/续录这类
//! 低频时刻发生,完全可接受。
//!
//! 为什么所有失败都只降级:音频保留是转写之上的增值层。转码/解码任一步失败,都必须
//! 保住原始字节、保住转写不受影响——编码失败就留着 WAV(下次再转),解码失败就把坏
//! m4a 挪成 `.bad`(移出枚举、字节仍在)、该源本场从头建档。绝不因压缩这件锦上添花
//! 的事删掉用户的录音或中断录制。

use crate::store::audio::{
    bytes_to_ms, clear_track_compressed, load_audio_meta, repair_wav_header, set_track_compressed,
    wav_header, HEADER_LEN,
};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// afconvert(m4af/aac/32kbps)与源 WAV 的时长可能有几十毫秒的编码器边界差,
/// 用它做「编码后时长 ≈ 编码前时长」的收敛校验,超差即判失败、留住 WAV。
pub const DURATION_TOLERANCE_MS: u64 = 100;

const AFCONVERT: &str = "/usr/bin/afconvert";
const AFINFO: &str = "/usr/bin/afinfo";

/// 跑一个子进程并把非零退出连同 stderr 变成 anyhow 错误(失败原因要能进 eprintln)。
fn run(cmd: &mut Command) -> anyhow::Result<std::process::Output> {
    // 用绝对路径 + output() 同步等待:转码是停录/续录时的一次性阻塞动作,无需异步。
    let out = cmd.output()?;
    if !out.status.success() {
        anyhow::bail!(
            "{:?} 退出码 {:?}: {}",
            cmd.get_program(),
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out)
}

/// WAV → m4a(AAC 32kbps 单声道)。参数为控制器本机实测值。
fn afconvert_encode(wav: &Path, m4a_tmp: &Path) -> anyhow::Result<()> {
    run(Command::new(AFCONVERT)
        .args(["-f", "m4af", "-d", "aac", "-b", "32000"])
        .arg(wav)
        .arg(m4a_tmp))?;
    Ok(())
}

/// m4a → WAV(16kHz 单声道 s16le,与录制格式一致,续录端可直接续写)。
fn afconvert_decode(m4a: &Path, wav_tmp: &Path) -> anyhow::Result<()> {
    run(Command::new(AFCONVERT)
        .args(["-f", "WAVE", "-d", "LEI16@16000", "-c", "1"])
        .arg(m4a)
        .arg(wav_tmp))?;
    Ok(())
}

/// 用 afinfo 读音频文件时长(毫秒)。afinfo 输出含一行 `estimated duration: 3.000000 sec`,
/// 解析该行的浮点秒数 ×1000 四舍五入。m4a 容器不能按字节换算时长,只能这样实测。
pub fn probe_duration_ms(path: &Path) -> anyhow::Result<u64> {
    let out = run(Command::new(AFINFO).arg(path))?;
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        // 只认 "estimated duration:" 这行,取冒号后第一个 token 作为秒数。
        if let Some(rest) = line.trim().strip_prefix("estimated duration:") {
            let secs: f64 = rest
                .split_whitespace()
                .next()
                .ok_or_else(|| anyhow::anyhow!("afinfo duration 行无数值: {line:?}"))?
                .parse()?;
            return Ok((secs * 1000.0).round() as u64);
        }
    }
    anyhow::bail!("afinfo 输出未找到 estimated duration: {path:?}")
}

/// 收集目录下以 `suffix` 结尾的文件名(去掉 suffix 得到 source)。
/// 精确后缀匹配天然把 `.m4a.tmp`/`.m4a.bad` 排除在 `.m4a` 枚举之外、
/// 把 `.wav.tmp` 排除在 `.wav` 枚举之外——它们都不以对应后缀结尾。
fn sources_with_suffix(note_dir: &Path, suffix: &str) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(note_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        if let Some(name) = entry.file_name().to_str() {
            if let Some(source) = name.strip_suffix(suffix) {
                if !source.is_empty() {
                    out.push(source.to_string());
                }
            }
        }
    }
    out
}

/// 停录后转码整个笔记目录:逐 `<source>.wav` 转成 `<source>.m4a`,校验通过才替换。
///
/// 崩溃收敛靠两点:(1) 开始先扫掉本目录所有 `*.m4a.tmp` 半成品;(2) 若某源
/// `<source>.m4a` 已存在(说明上次「rename 完成、删 WAV 前」崩了),直接删 WAV 收口,
/// 不重复编码。任何一步失败:删掉本源的 tmp、留住 WAV、eprintln、继续下一轨。
pub fn transcode_note_dir(note_dir: &Path) {
    // 清残留:上次编码写到一半 `<x>.m4a.tmp` 就崩,这些半成品既不完整也不该被枚举。
    for source in sources_with_suffix(note_dir, ".m4a.tmp") {
        let _ = std::fs::remove_file(note_dir.join(format!("{source}.m4a.tmp")));
    }
    for source in sources_with_suffix(note_dir, ".wav") {
        let wav = note_dir.join(format!("{source}.wav"));
        let m4a = note_dir.join(format!("{source}.m4a"));
        let m4a_tmp = note_dir.join(format!("{source}.m4a.tmp"));
        // 上次已产出 m4a 却没删成 WAV:两文件并存,删 WAV 完成收敛即可,不重编。
        if m4a.exists() {
            if let Err(e) = std::fs::remove_file(&wav) {
                eprintln!("转码收敛删残留 WAV 失败({}): {e}", wav.display());
            }
            continue;
        }
        if let Err(e) = transcode_one(note_dir, &source, &wav, &m4a, &m4a_tmp) {
            let _ = std::fs::remove_file(&m4a_tmp);
            eprintln!("转码失败,保留原始 WAV({}): {e}", wav.display());
        }
    }
}

/// 单轨转码:repair 头 → 空轨跳过 → 编码到 tmp → 时长核对 → 记 meta → rename → 删 WAV。
/// 返回 Err 时调用方负责删 tmp、留 WAV。
fn transcode_one(
    note_dir: &Path,
    source: &str,
    wav: &Path,
    m4a: &Path,
    m4a_tmp: &Path,
) -> anyhow::Result<()> {
    // 先把可能陈旧的 WAV 头按实际长度修正,否则 afconvert 会照头里的短 data 尺寸编码。
    repair_wav_header(wav)?;
    let wav_len = std::fs::metadata(wav)?.len();
    // 空轨(只有 44 字节头、无样本):没内容可压,直接跳过,留着让枚举端按空轨忽略。
    if wav_len <= HEADER_LEN {
        return Ok(());
    }
    let wav_ms = bytes_to_ms(wav_len - HEADER_LEN);
    afconvert_encode(wav, m4a_tmp)?;
    let encoded_ms = probe_duration_ms(m4a_tmp)?;
    let drift = (encoded_ms as i64 - wav_ms as i64).unsigned_abs();
    if drift > DURATION_TOLERANCE_MS {
        anyhow::bail!("编码后时长 {encoded_ms}ms 与源 {wav_ms}ms 相差 {drift}ms,超允差");
    }
    // 波形预计算必须赶在 WAV 删除前(m4a 解码贵,之后无从再算);失败不阻塞转码,
    // 播放器回退段落包络。
    let waveform = match crate::store::audio::waveform_from_wav(wav) {
        Ok(w) => Some(w),
        Err(e) => {
            eprintln!("波形预计算失败,播放器将回退段落包络({}): {e}", wav.display());
            None
        }
    };
    // 先记 meta(m4a 时长无法按字节反推,list_tracks 只能读这里)再 rename:
    // 若记 meta 后 rename 崩,下次进来 m4a 不存在 → 走正常编码重记,幂等。
    set_track_compressed(note_dir, source, wav_ms, waveform)?;
    std::fs::rename(m4a_tmp, m4a)?;
    // WAV 删除是收尾:即便失败,m4a 已就位,下次 transcode 的「并存收敛」分支会补删。
    if let Err(e) = std::fs::remove_file(wav) {
        eprintln!("转码成功但删原始 WAV 失败,留待下次收敛({}): {e}", wav.display());
    }
    Ok(())
}

/// 续录前把整个笔记目录解回 WAV:逐 `<source>.m4a` 解成 `<source>.wav`,校验通过才替换。
/// 失败即降级:删 tmp、把坏 m4a 挪成 `<source>.m4a.bad`(移出枚举、字节保留)、清压缩
/// 标记(该源本场从 base_ms 重新建档)、eprintln。
pub fn decode_note_to_wav(note_dir: &Path) {
    for source in sources_with_suffix(note_dir, ".m4a") {
        let m4a = note_dir.join(format!("{source}.m4a"));
        let wav = note_dir.join(format!("{source}.wav"));
        let wav_tmp = note_dir.join(format!("{source}.wav.tmp"));
        if let Err(e) = decode_one(note_dir, &source, &m4a, &wav, &wav_tmp) {
            let _ = std::fs::remove_file(&wav_tmp);
            // 坏 m4a 不删:挪成 .bad 移出枚举但字节留存,便于事后取证/手工恢复。
            if let Err(re) = std::fs::rename(&m4a, note_dir.join(format!("{source}.m4a.bad"))) {
                eprintln!("解码失败且挪 .bad 失败({}): {re}", m4a.display());
            }
            let _ = clear_track_compressed(note_dir, &source);
            eprintln!("解码失败,该源降级为无音频从头建档({}): {e}", m4a.display());
        }
    }
}

/// 单轨解码:解到 tmp → 时长核对 → rename → 删 m4a → 清压缩标记。
/// 返回 Err 时调用方负责删 tmp、挪 .bad、清标记。
fn decode_one(
    note_dir: &Path,
    source: &str,
    m4a: &Path,
    wav: &Path,
    wav_tmp: &Path,
) -> anyhow::Result<()> {
    afconvert_decode(m4a, wav_tmp)?;
    // afconvert 产出的 WAV 不是我们的标准 44 头:fmt 块 40 字节、data 前还塞一个 FLLR
    // 页对齐填充块。故不能按 `文件长-44` 算 data,必须解析 RIFF 找 data 块拿纯 PCM,
    // 再用标准 44 头重写——续录端 AudioTrackWriter 只认 44 头,不重写就会错位损坏。
    let pcm = extract_wav_data(wav_tmp)?;
    if pcm.is_empty() {
        anyhow::bail!("解码得到空 WAV: {}", m4a.display());
    }
    // meta 里有该源转码时记的 duration_ms → 核对解码结果与之相符(允差内);
    // 无记录(如 meta 丢失)则跳过时长校验,只要非空即接受——降级容忍。
    if let Some(recorded) = load_audio_meta(note_dir)
        .tracks
        .get(source)
        .and_then(|t| t.duration_ms)
    {
        let decoded_ms = bytes_to_ms(pcm.len() as u64);
        let drift = (decoded_ms as i64 - recorded as i64).unsigned_abs();
        if drift > DURATION_TOLERANCE_MS {
            anyhow::bail!("解码后时长 {decoded_ms}ms 与记录 {recorded}ms 相差 {drift}ms,超允差");
        }
    }
    // 用标准头重写 tmp(覆盖 afconvert 的胖头版本),再 rename 原子替换。
    let mut canonical = Vec::with_capacity(HEADER_LEN as usize + pcm.len());
    canonical.extend_from_slice(&wav_header(pcm.len() as u32));
    canonical.extend_from_slice(&pcm);
    std::fs::write(wav_tmp, &canonical)?;
    std::fs::rename(wav_tmp, wav)?;
    std::fs::remove_file(m4a)?;
    // 清 codec/duration:该源回到 WAV 枚举,续录端按 WAV 尾部对齐续写。
    clear_track_compressed(note_dir, source)?;
    Ok(())
}

/// m4a 解码为**标准 44 头** WAV 到任意目标路径(原生播放器的解码缓存用)。
/// afconvert 产物是胖头(40 字节 fmt + FLLR 填充),mmap 按 `44+2i` 直接索引会错位,
/// 必须解析 RIFF 取纯 data 后用标准头重写——与 decode_one 的续录路径同一策略。
/// tmp 同目录写、rename 原子落位,失败清 tmp。
pub fn decode_m4a_to_standard_wav(m4a: &Path, dest: &Path) -> anyhow::Result<()> {
    let tmp = dest.with_extension("wav.tmp");
    let run = || -> anyhow::Result<()> {
        afconvert_decode(m4a, &tmp)?;
        let pcm = extract_wav_data(&tmp)?;
        if pcm.is_empty() {
            anyhow::bail!("解码得到空 WAV: {}", m4a.display());
        }
        let mut canonical = Vec::with_capacity(HEADER_LEN as usize + pcm.len());
        canonical.extend_from_slice(&wav_header(pcm.len() as u32));
        canonical.extend_from_slice(&pcm);
        std::fs::write(&tmp, &canonical)?;
        std::fs::rename(&tmp, dest)?;
        Ok(())
    };
    run().inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// 从任意合法 WAV(含 afconvert 的 FLLR 填充块变体)里取出 `data` 块的纯 PCM 字节。
/// 逐块跳过 fmt/FLLR 等,只认 data;找不到即 Err(交由 decode 降级)。
fn extract_wav_data(path: &Path) -> anyhow::Result<Vec<u8>> {
    let bytes = std::fs::read(path)?;
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        anyhow::bail!("非 WAV 数据: {}", path.display());
    }
    let mut pos = 12usize;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes([bytes[pos + 4], bytes[pos + 5], bytes[pos + 6], bytes[pos + 7]]) as usize;
        let start = pos + 8;
        if id == b"data" {
            let end = start.saturating_add(size).min(bytes.len());
            return Ok(bytes[start..end].to_vec());
        }
        // 块尾按 RIFF 规则补齐到偶数;溢出即停,避免坏块导致死循环。
        pos = start.saturating_add(size).saturating_add(size & 1);
    }
    anyhow::bail!("WAV 无 data 块: {}", path.display())
}

/// 旧笔记波形懒回填:波形预计算上线前转码的 m4a 没有 waveform——解码到临时 WAV、
/// 取纯 PCM 桶化、写回 audio.json。秒级阻塞,调用方放后台线程;失败只降级
/// (播放器继续用段落包络),临时文件成败都清。
pub fn backfill_waveform(note_dir: &Path, source: &str) -> anyhow::Result<()> {
    let m4a = note_dir.join(format!("{source}.m4a"));
    let tmp = note_dir.join(format!(".{source}.waveform.wav.tmp"));
    let pcm = afconvert_decode(&m4a, &tmp).and_then(|_| extract_wav_data(&tmp));
    let _ = std::fs::remove_file(&tmp);
    let wf = crate::store::audio::waveform_from_pcm(&pcm?);
    crate::store::audio::set_track_waveform(note_dir, source, wf)
}

/// 受锁保护的队列状态。三者必须一起改、一起看,所以塞进同一个 `Mutex`:
/// - `queue`:待转码的笔记目录(FIFO);
/// - `current`:worker 此刻正在转的目录(放锁后转码期间一直挂着,供 cancel/pause 观测);
/// - `paused`:迁移期间置真,worker 见真就不出队(让底层目录静止,不被转码改写)。
struct QState {
    queue: VecDeque<PathBuf>,
    current: Option<PathBuf>,
    paused: bool,
}

/// 16k 单声道 s16le WAV → f32 samples(复用 extract_wav_data 的胖头兼容)。
pub fn read_wav_f32(wav: &Path) -> anyhow::Result<Vec<f32>> {
    let data = extract_wav_data(wav)?;
    Ok(data
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
        .collect())
}

/// 取某音轨全场 PCM:停止后 wav 尚在盘上直读;转码完成后仅剩 m4a 则解码到临时 wav 再读。
pub fn track_pcm(note_dir: &Path, source: &str) -> anyhow::Result<Vec<f32>> {
    let wav = note_dir.join(format!("{source}.wav"));
    if wav.exists() {
        return read_wav_f32(&wav);
    }
    let m4a = note_dir.join(format!("{source}.m4a"));
    if !m4a.exists() {
        anyhow::bail!("音轨 {source} 的 wav/m4a 均不存在于 {:?}", note_dir);
    }
    let tmp = note_dir.join(format!(".{source}.refine.wav.tmp"));
    let _ = std::fs::remove_file(&tmp);
    // 失败也要清 tmp:afconvert 可能已写出半截文件,不清就永久残留在笔记目录。
    if let Err(e) = afconvert_decode(&m4a, &tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    let out = read_wav_f32(&tmp);
    let _ = std::fs::remove_file(&tmp);
    out
}

/// 全局串行转码队列:整个进程只有一个 worker 线程按序转码,永不并发跑 afconvert。
///
/// 为什么要队列而不是「停录就地转」:转一笔要跑数秒的 afconvert 子进程,若停录线程
/// 内联转码,会阻塞停录返回;多笔记接连停录还会叠着跑、抢 CPU。改为入队 + 单 worker
/// 串行消费:停录只是 O(1) 入队即返回,转码在后台一笔接一笔。
///
/// 为什么 `Mutex<QState>` + `Condvar` 而不是 channel:本队列不是纯生产者-消费者,
/// 还要支持「摘除某个待转项」「等某个 in-flight 转完」「暂停并等排空」这些需要审视/
/// 修改队列内部状态的操作,channel 的黑盒 FIFO 做不到。Condvar 让 worker 空闲时挂起、
/// 有活时被 `notify` 唤醒,不空转烧 CPU。
///
/// 锁协议(全类型统一遵守,避免死锁/丢唤醒):
/// - 任何读写 `queue`/`current`/`paused` 都必须先拿 `state` 锁;
/// - 任何**改变**这三者的路径,改完都要 `cv.notify_all()`,否则等在 cv 上的
///   cancel/pause/worker 可能永久错过这次状态变化(丢唤醒);
/// - 所有 `cv.wait` 都套在「重新检查条件」的循环里(worker 是外层 `loop`,cancel/pause
///   是 `while`),因为 Condvar 允许虚假唤醒 + 唤醒后条件可能已被别人改回;
/// - **绝不持 `state` 锁调 `process`**:afconvert 要跑数秒,若持锁转码,这数秒内所有
///   enqueue/cancel_and_wait/pause_and_wait 全堵在锁上 —— 停录/续录直接卡死。故 worker
///   取到队头、置好 `current` 后立刻放锁,再调 `process`。
pub struct TranscodeQueue {
    state: Mutex<QState>,
    cv: Condvar,
}

impl TranscodeQueue {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(QState {
                queue: VecDeque::new(),
                current: None,
                paused: false,
            }),
            cv: Condvar::new(),
        })
    }

    /// 入队一个待转目录。去重:已在队列中、或正是 worker 此刻在转的 `current`,都跳过 ——
    /// 同一目录重复转码是纯浪费(幂等但白跑一遍 afconvert),启动回溯扫描 + 停录可能对同
    /// 一目录各入一次,这里挡住。改了队列就 `notify_all` 唤醒可能在等活的 worker。
    pub fn enqueue(&self, note_dir: PathBuf) {
        let mut st = self.state.lock().unwrap();
        if st.current.as_deref() == Some(note_dir.as_path())
            || st.queue.iter().any(|p| p == &note_dir)
        {
            return;
        }
        st.queue.push_back(note_dir);
        self.cv.notify_all();
    }

    /// 续录某目录前调用:把它从队列摘掉,并阻塞到它「不再是 in-flight」为止。
    /// 为什么必须等 in-flight:续录要把 m4a 解回 wav,若此刻 worker 正把该目录的 wav
    /// 转成 m4a,两者会撞文件。摘队列 `retain` 处理「还没轮到」的情况;`while current==它`
    /// 等 cv 处理「正在转」的情况 —— worker 转完清 `current` 会 `notify_all`,唤醒本等待。
    pub fn cancel_and_wait(&self, note_dir: &Path) {
        let mut st = self.state.lock().unwrap();
        st.queue.retain(|p| p != note_dir);
        // while 而非 if:虚假唤醒 + 每次唤醒都要重判 current 是否仍等于本目录。
        while st.current.as_deref() == Some(note_dir) {
            st = self.cv.wait(st).unwrap();
        }
    }

    /// 迁移前调用:置 `paused`(worker 从此不出队),并等 `current` 排空(等当前 in-flight
    /// 转完)。迁移要挪动笔记根目录,必须先让转码彻底静止:既不能有新的开转,也不能有正在
    /// 转的。返回后到 `unpause` 之间,worker 保证不碰任何目录。
    pub fn pause_and_wait(&self) {
        let mut st = self.state.lock().unwrap();
        st.paused = true;
        // while:等到确实没有 in-flight;worker 清 current 时 notify_all 唤醒这里复查。
        while st.current.is_some() {
            st = self.cv.wait(st).unwrap();
        }
    }

    /// 迁移完成后解除暂停,并 `notify_all` 唤醒挂着等活的 worker,让它继续消费队列。
    pub fn unpause(&self) {
        let mut st = self.state.lock().unwrap();
        st.paused = false;
        self.cv.notify_all();
    }

    /// 启动常驻 worker 线程,串行消费队列。`running` 传 AppState 的录制标志、`process`
    /// 传 `transcode_note_dir`(测试传桩)—— 参数化是为了单测能用假处理函数验时序。
    /// process 为泛型闭包:lib.rs 在真实转码函数外再包一层"完成事件"通知(转码完成
    /// 瞬间源 WAV 被删,已打开详情页的播放器引用失效——前端收事件重拉音轨修此竞态)。
    pub fn spawn_worker(
        self: &Arc<Self>,
        running: Arc<Mutex<bool>>,
        process: impl Fn(&Path) + Send + 'static,
    ) {
        let me = Arc::clone(self);
        std::thread::spawn(move || loop {
            // 录制中让路:转码要抢 CPU,录制阶段优先保采集不卡顿,所以录制中只 sleep 不出队。
            // 用短睡眠轮询而非 cv:`running` 不归本锁管(是 AppState 的),没法在它变化时被
            // notify;停录属低频事件,2s 的让路延迟无感。
            if *running.lock().unwrap() {
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
            // 取一笔活:置好 current 后立刻放锁(块结束即解锁),再在锁外调 process。
            let next = {
                let mut st = me.state.lock().unwrap();
                if st.paused || st.queue.is_empty() {
                    // 无活可做:带 2s 超时挂起。超时是为了周期性回到外层重判 `running`
                    //(它不受本 cv 管,只能靠超时轮询);被 enqueue/unpause 的 notify 提前
                    // 唤醒则更及时。无论哪种唤醒,都 continue 回外层从头重判所有条件。
                    let _ = me.cv.wait_timeout(st, Duration::from_secs(2)).unwrap();
                    continue;
                }
                let dir = st.queue.pop_front().unwrap();
                st.current = Some(dir.clone());
                dir
            }; // ← 锁在此释放:process 期间不持锁,enqueue/cancel/pause 可自由推进。

            // panic 防护,双保险 —— 转码是增值层,一次 panic 绝不能被队列放大成全局死锁:
            // 若 process panic 而无防护,worker 在锁外(Mutex 不中毒),线程直接 unwind
            // 死亡,「清 current + notify」永不执行 → current 永久 Some(dir),此后
            // cancel_and_wait(该目录) 与 pause_and_wait 的 while 条件恒真且无人再唤醒,
            // 续录/迁移被彻底锁死。
            // ① Drop 守卫:清理动作放进守卫的 Drop,正常返回、panic unwind、乃至将来
            //    有人改掉 catch_unwind,清理都必达(unwind 也会跑 Drop)。
            // ② catch_unwind:把 panic 拦在本次迭代内,worker 线程本身不死,队列里
            //    其余笔记照常消费。AssertUnwindSafe 成立:闭包只捕获 &next(PathBuf,
            //    panic 后不再被读)与 fn 指针,无跨 panic 共享可变状态。
            struct CurrentGuard<'a>(&'a TranscodeQueue);
            impl Drop for CurrentGuard<'_> {
                fn drop(&mut self) {
                    // 转完(或炸完)清 current 并唤醒:cancel_and_wait/pause_and_wait
                    // 可能正等这一刻。
                    let mut st = self.0.state.lock().unwrap();
                    st.current = None;
                    self.0.cv.notify_all();
                }
            }
            let _guard = CurrentGuard(&me);
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| process(&next))).is_err()
            {
                eprintln!("转码任务 panic,跳过该目录继续消费队列: {}", next.display());
            }
            // _guard 在此 drop → 清 current + notify_all。
        });
    }
}

#[cfg(test)]
mod queue_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    static PROCESSED: AtomicUsize = AtomicUsize::new(0);
    fn slow_stub(_: &Path) {
        PROCESSED.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    #[test]
    fn queue_dedups_pauses_and_cancel_waits_inflight() {
        PROCESSED.store(0, Ordering::SeqCst);
        let q = TranscodeQueue::new();
        let running = Arc::new(Mutex::new(false));
        q.spawn_worker(running.clone(), slow_stub);

        let a = PathBuf::from("/tmp/note-a");
        q.enqueue(a.clone());
        q.enqueue(a.clone()); // 去重
                              // 等 worker 拾起 a
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while PROCESSED.load(Ordering::SeqCst) == 0 {
            assert!(std::time::Instant::now() < deadline, "worker 未拾起任务");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // cancel_and_wait 必须阻塞到 in-flight 完成
        let t0 = std::time::Instant::now();
        q.cancel_and_wait(&a);
        // 去重生效:只处理了一次
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(PROCESSED.load(Ordering::SeqCst), 1, "重复入队被去重");
        assert!(t0.elapsed() >= std::time::Duration::from_millis(50), "等待了 in-flight");

        // paused 期间不出队
        q.pause_and_wait();
        q.enqueue(PathBuf::from("/tmp/note-b"));
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(PROCESSED.load(Ordering::SeqCst), 1, "暂停期间不处理");
        q.unpause();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while PROCESSED.load(Ordering::SeqCst) < 2 {
            assert!(std::time::Instant::now() < deadline, "恢复后应继续处理");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    static BOOM_SEEN: AtomicUsize = AtomicUsize::new(0);
    static OK_PROCESSED: AtomicUsize = AtomicUsize::new(0);
    /// 路径含 "boom" 就 panic 的桩:先置观测位再睡 100ms 再炸,让主线程能确认
    /// worker 确实已把该目录置为 in-flight(current)后才炸,而非还没拾起。
    fn panicky_stub(p: &Path) {
        if p.to_string_lossy().contains("boom") {
            BOOM_SEEN.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(100));
            panic!("预期 panic:验证 worker panic 防护");
        }
        OK_PROCESSED.fetch_add(1, Ordering::SeqCst);
    }

    /// process panic 后:worker 必须存活继续消费,current 必须被清(cancel/pause 不死等)。
    /// 注:catch_unwind 拦截前 panic hook 会向 stderr 打一条 panic 记录 —— 这是本测试的
    /// 预期噪声(不用 set_hook 静音:hook 是进程全局的,并行测试下会吞掉别的测试的
    /// panic 信息,得不偿失)。
    #[test]
    fn worker_survives_process_panic_and_clears_current() {
        BOOM_SEEN.store(0, Ordering::SeqCst);
        OK_PROCESSED.store(0, Ordering::SeqCst);
        let q = TranscodeQueue::new();
        let running = Arc::new(Mutex::new(false));
        q.spawn_worker(running, panicky_stub);

        let boom = PathBuf::from("/tmp/note-boom");
        q.enqueue(boom.clone());
        // 等 worker 确实拾起 boom(已置 current、即将 panic)
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while BOOM_SEEN.load(Ordering::SeqCst) == 0 {
            assert!(std::time::Instant::now() < deadline, "worker 未拾起 boom");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // ① panic 后 worker 仍存活:后续任务照常被处理
        q.enqueue(PathBuf::from("/tmp/note-ok"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while OK_PROCESSED.load(Ordering::SeqCst) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "process panic 后 worker 应存活并继续处理后续任务"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // ② current 已被清:对 panic 过的目录 cancel_and_wait / pause_and_wait 不死等。
        //(若 current 卡死为 Some(boom),这两行会永久阻塞,测试超时挂死即失败。)
        q.cancel_and_wait(&boom);
        q.pause_and_wait();
        q.unpause();
    }
}

#[cfg(test)]
#[cfg(target_os = "macos")]
mod tests {
    use super::*;
    use crate::store::audio::AudioTrackWriter;

    #[test]
    fn read_wav_f32_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("t.wav");
        let spec = hound::WavSpec { channels: 1, sample_rate: 16000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
        let mut w = hound::WavWriter::create(&wav, spec).unwrap();
        for i in 0..1600 { w.write_sample(((i % 100) * 300) as i16).unwrap(); }
        w.finalize().unwrap();
        let pcm = read_wav_f32(&wav).unwrap();
        assert_eq!(pcm.len(), 1600);
        assert!(pcm.iter().all(|x| x.abs() <= 1.0));
    }

    #[test]
    fn track_pcm_prefers_wav_and_falls_back_to_m4a() {
        let dir = tempfile::tempdir().unwrap();
        // 只有 wav:直读
        let wav = dir.path().join("mic.wav");
        let spec = hound::WavSpec { channels: 1, sample_rate: 16000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
        let mut w = hound::WavWriter::create(&wav, spec).unwrap();
        for _ in 0..320 { w.write_sample(1000i16).unwrap(); }
        w.finalize().unwrap();
        assert_eq!(track_pcm(dir.path(), "mic").unwrap().len(), 320);
        // 两者皆无:报错带路径
        let err = track_pcm(dir.path(), "system").unwrap_err();
        assert!(err.to_string().contains("system"));
    }

    #[test]
    fn track_pcm_decode_failure_leaves_no_tmp() {
        let dir = tempfile::tempdir().unwrap();
        // 垃圾字节的假 m4a:afconvert 解码必失败,失败路径也不得残留 tmp。
        std::fs::write(dir.path().join("system.m4a"), b"not an m4a").unwrap();
        assert!(track_pcm(dir.path(), "system").is_err());
        assert!(
            !dir.path().join(".system.refine.wav.tmp").exists(),
            "解码失败后临时文件必须被清理"
        );
    }

    fn make_note_with_wav(ms: u64) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = AudioTrackWriter::new(tmp.path(), "mic", 0);
        w.append(&vec![0.1f32; (16 * ms) as usize]); // 16 样本/ms
        drop(w);
        tmp
    }

    #[test]
    fn transcode_replaces_wav_with_verified_m4a() {
        let tmp = make_note_with_wav(3000);
        transcode_note_dir(tmp.path());
        assert!(!tmp.path().join("mic.wav").exists(), "成功后删 WAV");
        assert!(tmp.path().join("mic.m4a").exists());
        let meta = crate::store::audio::load_audio_meta(tmp.path());
        let d = meta.tracks["mic"].duration_ms.unwrap();
        assert!((d as i64 - 3000).unsigned_abs() <= DURATION_TOLERANCE_MS, "时长记录 {d} ≈ 3000");
        // 幂等:再跑一遍无事发生
        transcode_note_dir(tmp.path());
        assert!(tmp.path().join("mic.m4a").exists());
    }

    #[test]
    fn transcode_converges_when_both_files_exist_and_cleans_tmp() {
        let tmp = make_note_with_wav(500);
        std::fs::write(tmp.path().join("mic.m4a.tmp"), b"junk").unwrap(); // 崩溃残留
        transcode_note_dir(tmp.path());
        assert!(!tmp.path().join("mic.m4a.tmp").exists(), "tmp 残留清掉");
        // 模拟"删 wav 前崩溃":重造 wav,与 m4a 并存
        let mut w = AudioTrackWriter::new(tmp.path(), "mic", 0);
        w.append(&vec![0.1f32; 160]);
        drop(w);
        transcode_note_dir(tmp.path());
        assert!(!tmp.path().join("mic.wav").exists(), "并存收敛为只剩 m4a");
        assert!(tmp.path().join("mic.m4a").exists());
    }

    #[test]
    fn decode_restores_wav_for_resume() {
        let tmp = make_note_with_wav(2000);
        transcode_note_dir(tmp.path());
        decode_note_to_wav(tmp.path());
        assert!(tmp.path().join("mic.wav").exists());
        assert!(!tmp.path().join("mic.m4a").exists());
        let meta = crate::store::audio::load_audio_meta(tmp.path());
        assert!(meta.tracks["mic"].codec.is_none(), "压缩标记清除");
        // 样本数与 2000ms 允差内(afconvert 实测 roundtrip 样本精确,此处放允差防编解码器边界)
        let len = std::fs::metadata(tmp.path().join("mic.wav")).unwrap().len() - 44;
        let ms = len / 2 * 1000 / 16000;
        assert!((ms as i64 - 2000).unsigned_abs() <= DURATION_TOLERANCE_MS);
        // 解码后可直接续录:既有对齐逻辑接手
        let mut w = AudioTrackWriter::new(tmp.path(), "mic", 2000);
        w.append(&vec![0.5f32; 160]);
        drop(w);
    }

    #[test]
    fn corrupt_m4a_degrades_to_bad_rename() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("mic.m4a"), b"not audio").unwrap();
        crate::store::audio::set_track_compressed(tmp.path(), "mic", 1000, None).unwrap();
        decode_note_to_wav(tmp.path());
        assert!(tmp.path().join("mic.m4a.bad").exists(), "坏 m4a 移出枚举,字节保留");
        assert!(!tmp.path().join("mic.wav").exists());
    }
}
