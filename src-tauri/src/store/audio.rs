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
    /// 本轨录制时走了软件 AEC 路径(capture_path=aec):转码前的离线回声清洗
    /// 只对这类轨道启动。录制启用时写 true,从不清除(续录混合场景由清洗端的
    /// 置信度门限兜底)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soft_aec: Option<bool>,
    /// 离线清洗结果(排障用):估计延迟/置信度/分段数。None=未清洗过。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clean: Option<CleanInfo>,
    /// 墙钟-样本对账(见 SyncInfo)。None = 该轨录制期未记录(旧笔记/中断)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<SyncInfo>,
    /// 成品轨专用,见 MixInfo。源轨恒 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mix: Option<MixInfo>,
}

/// 波形桶数,与前端 WAVE_BARS 对齐(260 桶约 1KB JSON,audio.json 体积可忽略)。
pub const WAVEFORM_BUCKETS: usize = 260;

/// 离线清洗结果:估计延迟/置信度/分段数。存进 audio.json 用于排障。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanInfo {
    pub delay_ms: u32,
    pub confidence: f32,
    pub segments: u32,
    /// 神经残余级(DTLN-aec)是否实际参与:None=旧记录(该字段引入前写入,未知);
    /// Some(false)=AEC3-only(模型未在场或推理失败);Some(true)=神经级已叠加。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub neural: Option<bool>,
}

/// 成品轨完整性标记 + 消费口径。只在混音**正常定稿**(实时)或补生成**原子改名
/// 成功后**(离线)写入;回滚、放弃、panic 路径全都到不了写入点——因此它的存在
/// 本身就是「这条轨是完整产物」的盘上证据,mixed_track() 文档里两条无标记残留
/// 路径自此可判定。缺失不单独定罪(一期录的 mixed 没有它),时长交叉核对仍是
/// 最终裁决,见 retranscribe::input::mixed_untrusted。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MixInfo {
    /// "live"(录制期混出,时间轴含首帧偏移)或 "regen"(离线补生成,按
    /// offset_ms 定位,段落 seek 无需修正)。
    pub origin: String,
    /// 消费 mixed 时各源段落 seek 要加的修正量(ms)。live = 各源首帧偏移
    /// (末场值;续录多场的历史场次只能近似,量级数十~数百 ms)。regen = 空表。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub seek_offset_ms: BTreeMap<String, u64>,
    /// 定稿时量出的**整文件**时长(WAV 字节口径;续录跨全部场次,不是本场净时长
    /// ——消费端 mixed_untrusted 拿它与源轨全长终点比对,codex P1 修正)。
    /// 未转码时 mixed_untrusted 的时长读数来源。
    pub track_ms: u64,
    /// 混音和 |x|>1.0 的样本数(issue #124 观测):软限幅接手前注定被硬削的量,
    /// 即旧行为的削波计数。老记录缺键读 0。
    #[serde(default, skip_serializing_if = "audio_stat_is_zero")]
    pub clipped_samples: u64,
    /// 混音和超过软限幅拐点、被压过的样本数(含上一类)。
    #[serde(default, skip_serializing_if = "audio_stat_is_zero")]
    pub limited_samples: u64,
    /// 计数是否出自限幅观测(codex:未测≠零)。老记录缺键读 false =「仪表化之前
    /// 的录音,削波量未知」;新写恒 true,此时计数为 0 才真正表示「干净」。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub limit_metered: bool,
}

fn audio_stat_is_zero(v: &u64) -> bool {
    *v == 0
}

/// 墙钟-轨时间轴对账:该轨在本场录制里实际落盘的时长 vs 同一场的墙钟时长。
///
/// 为什么要落盘:回放侧的三种离线错位量法在 0.2~0.9s 区间互不吻合(见
/// player_align.rs 头注),分歧本身已达阈值量级,导致"连残余有多大都测不准"。
/// 录制期我们掌握真值——轨落盘长度与墙钟可直接对账,不需要估。有了这条基准才谈得上
/// 判定方案 A/B 孰优,以及反向标定那三种离线量法。
///
/// # 口径怎么定的(踩过坑,别再改回去)
///
/// track_ms 取自 **WAV 实际字节数**,不是采集侧的样本计数器。两条理由缺一不可:
/// - WAV 是重采样之后写入的,天然 16k 单声道口径;而 frame_tap 的 samples 计的是
///   设备原生率、交错多声道的原始样本(见 samples 字段注释),换算不出毫秒。
/// - WAV 是暂停闸之后写入的(sink 调用点在 segment_worker 的暂停 `continue` 之后),
///   天然是净时长;而 samples 在闸之前累加,暂停期照涨,与 wall_ms(净)口径不一致。
///
/// # drift_ms 怎么用(重要)
///
/// drift_ms **含一段系统性正偏置**:各路 capture 在 start_session 内部就已起流产帧,
/// 而墙钟起点 `started` 取于 start_session 返回之后,这段启动窗被算进了 track_ms 却
/// 没算进 wall_ms。消除它要动录制主链路的启动时序,风险大于收益,故不修。
/// 另有两处更小的同向偏置:mic 路软件 AEC 按 10ms 整帧输出,尾部不足一帧的余量滞留在
/// AEC 内部不落盘(负向,< 10ms);轨长换算按字节整除,也有亚毫秒截断。
///
/// 这份清单里**刻意没有**停录拆解耗时(join ASR 线程 + 排干写盘队列),因为它已被排除:
/// wall_ms 的取样点在 `do_stop_teardown` 里被特意放在 `handle.stop()` **之前**。若在
/// join 之后取,拆解耗时(无上界、未被测量,百毫秒到数秒)会整段计进 wall_ms,形成一段
/// 比上述三项都大的**负向**偏置,drift_ms 连符号都不再可预测,下面那套判读指引会失效。
/// 改动那处取样时序前请先读懂这一段。
///
/// 因此:**drift_ms 的绝对值不宜直接用作达标判据**。真正稳健的量是**两轨 drift 之差**
/// (`drift_ms(mic) − drift_ms(system)`):启动窗与暂停对两轨的影响大体同向,相减可
/// 抵消大部分。回放对齐关心的本来就是两轨的**相对**关系,不是各自对墙钟的绝对偏差。
///
/// # 判读陷阱:drift_ms≈0 不等于"对齐良好"
///
/// `frame_tap` 断流时补的静音帧走的是和真实帧完全相同的下游路径,经重采样后照样落进
/// WAV,计入 track_ms。于是一路采集在录制中途彻底死掉时,tap 会一直补零把时间轴撑满,
/// 最终 track_ms ≈ wall_ms、drift_ms ≈ 0——读数是"完美对齐",而那条轨其实半场是静音。
/// 这不是缺陷(补零维持时间轴正是 frame_tap 的设计意图),但是最容易看错的情形。
/// 判读 drift_ms **必须同时看 silence_ms 与 gaps**,silence_ms 占 wall_ms 比例畸高
/// (或 gaps 不为零)时,drift_ms 再小也不能当作"这条轨录得好"的证据。
///
/// 另一处易混淆:读到 `track_ms == 0` 时,先确认是不是**口径修正前的老记录**——本次
/// 口径修正之前,SourceHealth.samples 曾被当 16k 口径直接换算,那批已落盘的旧 sync
/// 记录没有 track_ms 键,serde default 读入即为 0,其 drift_ms 也是旧公式算出的错值,
/// 不代表"本场零内容"。这条和真·零内容的边角会撞车,判读时用 samples 是否为 0 辅助
/// 区分(旧记录 samples 通常非零,真零内容时 samples 也是 0)。
///
/// 零内容边角下还有一个已知的极窄情形,不必为它改代码,归到这条一并说明即可:
/// `AudioTrackWriter::open()` 是懒调用,首次 append 才跑;若本场一个样本都没写,
/// 对齐用的 set_len 从未执行,文件仍停在上一场结束时的长度——这个长度若与 base_ms
/// 不齐,会被换算出一个非零的假漂移。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncInfo {
    /// 本场录制墙钟时长(ms,已扣暂停)。
    pub wall_ms: u64,
    /// 该轨累计原始样本数,**设备原生采样率、交错多声道**口径(frame_tap 直接累加
    /// `frame.samples.len()`,到 16k 单声道的转换在它下游)。实测四条采集路径没有一条
    /// 是 16k:macOS SCK 48k×1、macOS VPIO 48k×1、cpal mic 44.1/48k×2ch、
    /// Windows loopback 48k×2ch。**不是 16k 口径,不可直接换算毫秒**(除以 16 会偏
    /// 3~6 倍);且它在暂停闸之前累加,暂停期照涨。保留纯粹是为排障(看帧量级/是否为零)。
    /// 要时长请用 track_ms。
    pub samples: u64,
    /// 本场该轨的 16k 口径净时长(ms):由 WAV 实际字节数量出,减去本场开始前该轨已有
    /// 的长度。轨文件不存在(采集启动失败/未保留音频)时本条 SyncInfo 整个不写。
    /// 新增字段,serde default:老记录缺这个键时按 0 读入,而不是让整个 audio.json
    /// 反序列化失败——那会连 offset_ms 一起丢掉,把回放对齐搞坏。
    #[serde(default)]
    pub track_ms: u64,
    /// 漂移 = track_ms − wall_ms。正 = 轨比墙钟长,负 = 该轨时钟跑慢。
    /// 含启动窗偏置,用法见结构体文档注释("怎么用"一节)。
    pub drift_ms: i64,
    /// frame_tap 累计补的静音时长(ms)。
    pub silence_ms: u64,
    /// 帧荒次数。
    pub gaps: u32,
    /// 时钟核对改写采样率的次数(>0 说明该源声明的采样率与实测不符)。
    pub rate_fixes: u32,
    /// 硬件时基**区间作废**次数:含真丢样的洞、时戳回退、突发缓冲回吐、隐含
    /// 速率越界。**它不是断流次数**——洞只是其子集,要数断流看 hw_holes
    /// (2026-08-17 排障把本字段当洞计数读,得出「514 次断流」的错误结论)。
    /// 新增字段走 serde default:老记录按 0 读入,不破坏反序列化。
    #[serde(default)]
    pub hw_gaps: u32,
    /// 判定为真丢了样本的洞次数(hw_gaps 的子集,与 hw_gap_ms 同判据)。
    /// 「发生过几次断流」只能看它。
    #[serde(default)]
    pub hw_holes: u32,
    /// 按硬件时戳判定并**已补回时间轴**的洞总时长(ms)。hw_gaps 只说发生过几次,
    /// 这个说丢了多久、补了多久——没有它,1694 次断裂对应多少秒无从对账
    /// (2026-08-16 事故:438 秒被直接压掉,当时只能靠 drift_ms 反推)。
    #[serde(default)]
    pub hw_gap_ms: u64,
    /// 采集回调因下游队列满而丢弃的样本数(每通道,全场累计,含自愈换下的实例)。
    ///
    /// 归因用法(注意量纲,Codex 二轮 P2):本字段是**样本数**,`hw_holes` 是**次数**,
    /// 两者不可直接比较,更不是数值上的子集关系。要联判必须先把样本数按该源的
    /// 原生采样率换成毫秒,再与 `hw_gap_ms`(同为毫秒)比:
    ///   - 换算后接近 hw_gap_ms → 缺口主要是我们自己丢的,先修下游背压;
    ///   - 换算后 ≈ 0 而 hw_gap_ms 仍高 → 确系进程外(设备/链路),换设备或换连接方式。
    ///
    /// 还有两个已知偏差,别当精确账:多个连续丢弃的回调可能被合并判成**一个**洞;
    /// 而尾部丢样、或该源拿不到硬件时戳时,丢的样本根本不会形成 hw_holes/hw_gap_ms。
    /// 另外 0 有二义——可能是真没丢,也可能是该后端不统计(见 AudioCapture::dropped_samples
    /// 的默认实现),判读前先确认本场用的是哪个采集后端。
    #[serde(default)]
    pub cap_dropped_samples: u64,
    /// 采集→tap 队列深度高水位。持续走高 = tap 或其下游追不上采集节奏。
    #[serde(default)]
    pub cap_queue_hw: u32,
    /// tap 向 worker 转发被阻塞的累计/单次峰值毫秒(>0 = worker 背压顶到 tap;
    /// 逼近采集缓冲总量时回调将被阻塞、HAL 开始真丢样)。
    #[serde(default)]
    pub send_wait_ms: u64,
    #[serde(default)]
    pub send_wait_max_ms: u64,
    /// 本源首个真实帧相对本场最早首帧的偏移(ms)。mixed 轨里该源内容整体后移
    /// 这么多(spec §口径差),段落 seek 到 mixed 时要加回去。续录每场覆盖,
    /// 与本结构其余字段同限制。旧数据无此字段 → None,消费方按 0 处理。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_frame_offset_ms: Option<u64>,
}

/// 轨时间轴 − 墙钟。抽成纯函数是为了能被表驱动测试直接打:这条数是后续所有漂移标定
/// 的基准,算错的基准比没有基准更坏,不能只靠 serde 往返测试"顺带"覆盖。
/// i64:两侧都可能更长,符号本身是结论(正=轨长,负=轨短)。
pub fn drift_ms(track_ms: u64, wall_ms: u64) -> i64 {
    track_ms as i64 - wall_ms as i64
}

/// 从 WAV 文件长度算本场该轨的净时长(ms)。纯函数,便于表驱动测试。
///
/// carried = base_ms − offset_ms 是**本场开始前**该轨已有的时长:offset_ms 是该轨 0
/// 时刻在笔记时间轴上的位置(轨可中途出现,见 AudioMeta 注释),AudioTrackWriter::open
/// 的续录分支正是把文件 set_len 到这个长度之后才追加本场内容的。直接减 base_ms 只在
/// offset_ms == 0 时才对——对"第二场才出现的轨",那样会减多,甚至减成负数。
fn track_ms_from_wav_len(wav_len: u64, base_ms: u64, offset_ms: u64) -> u64 {
    let file_ms = bytes_to_ms(wav_len.saturating_sub(HEADER_LEN));
    let carried = base_ms.saturating_sub(offset_ms);
    file_ms.saturating_sub(carried)
}

/// audio.json 缺该轨条目时反推 offset_ms:按「上场停止时文件尾 ≈ base_ms」的对齐
/// 不变式倒算。`open()` 走这条路时会把结果回写补全,`pre_session_track_len` 只读地
/// 复用同一公式——两处一旦分叉,回滚基线就会与 `open()` 实际对齐到的长度错开。
fn estimated_offset_ms(base_ms: u64, existing_data: u64) -> u64 {
    base_ms.saturating_sub(bytes_to_ms(existing_data))
}

/// 本场开始前该轨在盘上**应有**的字节数(含 44 字节头)。等于
/// `AudioTrackWriter::open()` 续录对齐后的长度,再与装配时实际长度取小。
///
/// 谁需要它:混音旁路放弃 `mixed.wav` 时要把文件回滚成「不含本场任何字节」的样子。
/// 回滚到**装配时的文件长度**是错的:`base_ms` 来自 `StoreWriter::base_ms()`,是续录前
/// 最大 `end_ms`(最后一句话结束的位置),而文件尾还压着用户按停止键前那段没进任何
/// segment 的静音(VAD 尾巴 + 反应时间),于是 `base_ms < 上一场轨时长` 是**常态**,
/// `open()` 恒走截短分支。截掉的那截不可逆,回滚到旧长度只会把本场刚混出来的字节填进
/// 空出的位置,拼出一条「上一场 + 本场开头」的混合体——而且 `duration_ms` 与放弃前
/// 一模一样,下游任何交叉核对都发现不了。
///
/// 取 min 还挡住反向情形:对齐若是零填充(target 大于现有内容),那段零同样不是上一场
/// 的内容,不该冒充它留在盘上。文件因此恒为装配前内容的**真前缀**。
///
/// `existing_len` 由调用方传入(它判定「这是一个普通文件」时已经 stat 过),让基线取的
/// 与判定用的是同一份快照;也避免调用方为了对齐公式去复刻 `ms_to_bytes` / `HEADER_LEN`
/// ——两处各算一遍正是上述 bug 的成因。
pub fn pre_session_track_len(
    note_dir: &Path,
    source: &str,
    base_ms: u64,
    existing_len: u64,
) -> u64 {
    let existing_data = existing_len.saturating_sub(HEADER_LEN);
    let offset_ms = match load_audio_meta(note_dir).tracks.get(source) {
        Some(t) => t.offset_ms,
        None => estimated_offset_ms(base_ms, existing_data),
    };
    let target = ms_to_bytes(base_ms.saturating_sub(offset_ms));
    HEADER_LEN + existing_data.min(target)
}

/// 量 note_dir 下 `<source>.wav` 得到本场该轨净时长(ms)。
/// 必须在写盘线程 join 之后调用(WAV 头已收尾、文件长度已定终)。
/// None = 该轨没有 WAV(采集启动失败、未保留音频、或文件被移走):无从对账,调用方跳过。
pub fn session_track_ms(note_dir: &Path, source: &str, base_ms: u64) -> Option<u64> {
    let wav_len = std::fs::metadata(note_dir.join(format!("{source}.wav"))).ok()?.len();
    let offset_ms = load_audio_meta(note_dir).tracks.get(source).map(|t| t.offset_ms).unwrap_or(0);
    Some(track_ms_from_wav_len(wav_len, base_ms, offset_ms))
}

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

/// 未转码 WAV 轨(中断笔记/转码失败降级)的波形懒回填:流式算完写回 audio.json。
/// 与 m4a 的 transcode::backfill_waveform 同角色,只是源是盘上现成的 WAV,无需解码。
/// 秒级阻塞(数小时录音数百 MB),调用方放后台线程;算好后 list_tracks 直读缓存,
/// 不再每次打开详情重扫。失败只降级(前端退段落包络),不影响枚举。
pub fn backfill_wav_waveform(note_dir: &Path, source: &str) -> anyhow::Result<()> {
    let wav = note_dir.join(format!("{source}.wav"));
    let wf = waveform_from_wav(&wav)?;
    set_track_waveform(note_dir, source, wf)
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

/// 录制装配软件 AEC 成功后调用:给 <source> 轨打 soft_aec 标记。幂等。
/// 持 META_LOCK,与 set_track_compressed 同模板。
pub fn set_track_soft_aec(note_dir: &Path, source: &str) -> anyhow::Result<()> {
    let _guard = meta_guard();
    let mut meta = load_audio_meta(note_dir);
    meta.tracks.entry(source.to_string()).or_default().soft_aec = Some(true);
    save_audio_meta(note_dir, &meta)
}

/// 离线清洗完成后调用:记录清洗报告。持 META_LOCK。
pub fn set_track_clean_info(note_dir: &Path, source: &str, info: CleanInfo) -> anyhow::Result<()> {
    let _guard = meta_guard();
    let mut meta = load_audio_meta(note_dir);
    meta.tracks.entry(source.to_string()).or_default().clean = Some(info);
    save_audio_meta(note_dir, &meta)
}

/// 写入某轨的墙钟-轨时间轴对账。全程持 audio.json 写锁;只改 sync 字段,保留其它。
/// 与 set_track_clean_info 同模板,另补写 schema_version = 1——与本模块其它写入点
/// (open 的两条建档分支)一致,不是笔误。
pub fn set_track_sync(note_dir: &Path, source: &str, info: SyncInfo) -> anyhow::Result<()> {
    let _guard = meta_guard();
    let mut meta = load_audio_meta(note_dir);
    meta.schema_version = 1;
    meta.tracks.entry(source.to_string()).or_default().sync = Some(info);
    save_audio_meta(note_dir, &meta)
}

/// 补生成前清掉 mixed 条目的过期读数(codec/duration/waveform/mix)并写新 offset:
/// 旧 m4a 的 duration_ms/waveform 描述的是即将被替换的旧产物,留着会被
/// track_info_for 优先采信;mix 标记描述的也是旧内容,新产物定稿后再由
/// set_track_mix 重写。sync 不清:mixed 从不写 sync。
pub fn reset_mixed_meta(note_dir: &Path, offset_ms: u64) -> anyhow::Result<()> {
    let _guard = meta_guard();
    let mut meta = load_audio_meta(note_dir);
    meta.schema_version = 1;
    let t = meta
        .tracks
        .entry(crate::pipeline::recording_sink::MIXED_TRACK.to_string())
        .or_default();
    t.offset_ms = offset_ms;
    t.codec = None;
    t.duration_ms = None;
    t.waveform = None;
    t.mix = None;
    save_audio_meta(note_dir, &meta)
}

/// 写成品轨完整性标记(见 MixInfo)。只允许在定稿成功的唯一出口调用——放弃/回滚
/// 路径写不到它正是它作为完整性证据的全部依据。
pub fn set_track_mix(note_dir: &Path, source: &str, mix: MixInfo) -> anyhow::Result<()> {
    let _guard = meta_guard();
    let mut meta = load_audio_meta(note_dir);
    meta.schema_version = 1;
    meta.tracks.entry(source.to_string()).or_default().mix = Some(mix);
    save_audio_meta(note_dir, &meta)
}

/// 读某轨 offset_ms(截短判定与 open() 对齐同口径用;缺项按 0,与 open 的
/// 丢档重建路径殊途同归——那条路径反推出的 offset 也以「文件尾≈base」为不变式)。
pub fn track_offset_ms(note_dir: &Path, source: &str) -> u64 {
    let _guard = meta_guard();
    load_audio_meta(note_dir).tracks.get(source).map(|t| t.offset_ms).unwrap_or(0)
}

/// 读成品轨完整性标记(续录装配在 clear 前留存上一场计数用;无标记 None)。
pub fn track_mix(note_dir: &Path, source: &str) -> Option<MixInfo> {
    let _guard = meta_guard();
    load_audio_meta(note_dir).tracks.get(source)?.mix.clone()
}

/// 清成品轨完整性标记。**任何可能改动 mixed 轨字节的操作开始之前必须调用**
/// (续录装配、补生成开工):旧标记描述的是旧内容,文件一旦被 truncate/append,
/// 标记若还在,异常中断后 mixed_untrusted 会拿旧读数为已被改动的文件背书
/// (codex 审查 P1)。条目不存在时静默成功——本就无标记可清。
pub fn clear_track_mix(note_dir: &Path, source: &str) -> anyhow::Result<()> {
    let _guard = meta_guard();
    let mut meta = load_audio_meta(note_dir);
    let Some(t) = meta.tracks.get_mut(source) else { return Ok(()) };
    if t.mix.is_none() {
        return Ok(());
    }
    t.mix = None;
    meta.schema_version = 1;
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
                    let est = estimated_offset_ms(self.base_ms, existing_data);
                    meta.schema_version = 1;
                    // 只改 offset、保留其它字段(soft_aec 等先于建档写入):
                    // 整条替换会把建档之前已写入的标记一并抹掉。
                    meta.tracks.entry(self.source.clone()).or_default().offset_ms = est;
                    save_audio_meta(&self.note_dir, &meta)?;
                    est
                }
            };
            // 续录即将改写这条 WAV(对齐 set_len + 追加新音频),之前详情页懒回填算好并
            // 写进 audio.json 的波形按新内容作废:清掉。否则若本场又中断(没走到转码),
            // 下次打开详情会 waveform.is_some() 而跳过重算,把旧短波形拉伸到新(更长)时长
            // 上错位显示。清空后 list_tracks 报 None → 详情页按新长度重新懒回填。
            let stale_wf = meta.tracks.get(&self.source).map(|t| t.waveform.is_some()).unwrap_or(false);
            if stale_wf {
                if let Some(t) = meta.tracks.get_mut(&self.source) {
                    t.waveform = None;
                }
                save_audio_meta(&self.note_dir, &meta)?;
            }
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
            // 只改 offset、保留其它字段(soft_aec 等先于建档写入):
            // 整条替换会把建档之前已写入的标记一并抹掉。
            meta.tracks.entry(self.source.clone()).or_default().offset_ms = self.base_ms;
            save_audio_meta(&self.note_dir, &meta)?;
            Ok((f, path, 0))
        }
    }

    /// 追加一批 f32 样本(clamp 到 [-1,1] 转 s16le)。成功返回 true;失败 eprintln
    /// 一次后永久停写并返回 false,让需要产物完整性的上层能立即放弃该轨。
    ///
    /// `#[must_use]`:这个返回值是 mixed 旁路唯一能察觉「盘上产物已不完整」的信号,
    /// 静默丢弃它就会留下一条语法合法、内容截断、下游分辨不出的成品轨。确实不关心
    /// 失败的调用点(源轨侧靠 `activity` 标记表达,测试里只是造数据)请写 `let _ =`,
    /// 把「故意忽略」显式化。
    #[must_use]
    pub fn append(&mut self, samples: &[f32]) -> bool {
        if samples.is_empty() {
            return !matches!(self.state, TrackState::Failed);
        }
        if matches!(self.state, TrackState::Pending) {
            match self.open() {
                Ok((file, path, data_len)) => {
                    self.state = TrackState::Open { file, path, data_len, since_flush: 0, buf: Vec::new() };
                }
                Err(e) => {
                    eprintln!("音频轨道建档失败,本场 {} 不保留音频: {e}", self.source);
                    self.state = TrackState::Failed;
                    return false;
                }
            }
        }
        if let TrackState::Open { data_len, path, .. } = &self.state {
            if *data_len + (samples.len() as u64) * BYTES_PER_SAMPLE > MAX_DATA_BYTES {
                eprintln!("音频轨道达到 WAV 4GiB 尺寸上限,停写({})", path.display());
                self.flush_header(); // 已写内容仍是合法 WAV
                self.state = TrackState::Failed;
                return false;
            }
        }
        let TrackState::Open { file, path, data_len, since_flush, buf } = &mut self.state else {
            return false;
        };
        buf.clear();
        buf.reserve(samples.len() * 2);
        for s in samples {
            buf.extend_from_slice(&f32_to_s16(*s).to_le_bytes());
        }
        if let Err(e) = file.write_all(buf) {
            eprintln!("音频落盘失败,本轨道停写({}): {e}", path.display());
            self.state = TrackState::Failed;
            return false;
        }
        *data_len += buf.len() as u64;
        *since_flush += samples.len() as u64;
        if *since_flush >= FLUSH_INTERVAL_SAMPLES {
            self.flush_header();
        }
        !matches!(self.state, TrackState::Failed)
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
/// 注意:这里**包含**成品轨(MIXED_TRACK)——转码、陈旧头修复都要靠它找到那条轨。
/// 「不进播放器轨列表」是 list_tracks 一处的过滤,不是从源头就藏起来。
fn known_sources(meta: &AudioMeta) -> Vec<String> {
    let mut sources: Vec<String> = vec!["mic".into(), "system".into()];
    for s in meta.tracks.keys() {
        if !sources.iter().any(|x| x == s) {
            sources.push(s.clone());
        }
    }
    sources
}

/// 单个源的轨道信息:每源优先上报已转码的 m4a、否则回落 WAV。时长口径按格式区分:
/// WAV 由字节数换算(bytes_to_ms);m4a 例外——容器不能按字节换算,时长取转码器实测后
/// 写进 audio.json 的记录(记录缺失即视为损坏,跳过该轨,不回落 WAV)。
/// list_tracks 与 mixed_track 共用这一份,枚举口径不会因为调用方不同而分叉。
fn track_info_for(note_dir: &Path, meta: &AudioMeta, source: &str) -> Option<TrackInfo> {
    let m4a_path = note_dir.join(format!("{source}.m4a"));
    if m4a_path.exists() {
        // 转码已完成:优先上报 m4a。m4a 容器不能按字节数换算时长,只能取转码器
        // 实测写入 audio.json 的记录;记录缺失说明转码/写档中途失败,视为损坏跳过
        // 该轨(而非回落 WAV——WAV 大概率已被转码流水线删除)。
        let duration_ms = meta.tracks.get(source).and_then(|t| t.duration_ms)?;
        return Some(TrackInfo {
            path: m4a_path.to_string_lossy().into_owned(),
            offset_ms: meta.tracks.get(source).map(|t| t.offset_ms).unwrap_or(0),
            waveform: meta.tracks.get(source).and_then(|t| t.waveform.clone()),
            source: source.to_string(),
            duration_ms,
        });
    }
    let path = note_dir.join(format!("{source}.wav"));
    let md = std::fs::metadata(&path).ok()?;
    if md.len() <= HEADER_LEN {
        return None; // 空轨道(刚建头没内容/损坏残留)不给前端,免得渲染空播放器
    }
    Some(TrackInfo {
        path: path.to_string_lossy().into_owned(),
        offset_ms: meta.tracks.get(source).map(|t| t.offset_ms).unwrap_or(0),
        // 未转码 WAV(中断笔记/转码失败降级):波形与 m4a 同策略——读 audio.json 里
        // 预算好的桶,没有(首次打开)就报 None。曾在这里同步 waveform_from_wav 现算,
        // 但长会议(数小时 WAV 达数百 MB)全扫是切换卡顿主因,已移交 note_audio_info
        // 的后台懒回填(backfill_wav_waveform 算完写回 meta 并发 transcode_done 重拉),
        // 不再阻塞枚举;缺波形期间前端自动退段落包络。
        waveform: meta.tracks.get(source).and_then(|t| t.waveform.clone()),
        source: source.to_string(),
        duration_ms: bytes_to_ms(md.len() - HEADER_LEN),
    })
}

/// 枚举笔记的**源轨**(详情页播放器用)。刻意排除成品轨(MIXED_TRACK,方案 B 录制期
/// 混好的轨):播放器把这里返回的每条轨叠加播放,mic+system+mixed 三条一起播会变成
/// 三重叠加(成品轨本就是 mic+system 混出来的),音量翻倍、听感像回声——而成品轨的
/// 本意恰恰是消除回放重影。第二期的回放方案切换走 mixed_track 单独取那条轨,
/// 不经这里。
pub fn list_tracks(note_dir: &Path) -> Vec<TrackInfo> {
    let meta = load_audio_meta(note_dir);
    known_sources(&meta)
        .into_iter()
        .filter(|s| s != crate::pipeline::recording_sink::MIXED_TRACK)
        .filter_map(|source| track_info_for(note_dir, &meta, &source))
        .collect()
}

/// 单独取成品轨(第二期回放方案切换消费,不进播放器的源轨列表)。
/// 枚举口径与 list_tracks 对源轨完全一致(m4a 优先、时长/波形取法相同),
/// 只是只看 MIXED_TRACK 这一个源,复用 track_info_for 避免口径分叉。
///
/// # 返回 Some 不等于这条轨内容完整(消费前必读)
///
/// 混音是录制主链路的旁路。已知失败会删除本场新轨,或把续录轨截回本场开始前的对齐
/// 基线(`pre_session_track_len`,恒为上一场内容的真前缀),但回滚
/// 本身仍可能因权限、磁盘或线程 panic 失败,盘上因而可能留下一条语法合法、内容却被
/// 截断的 `mixed.wav`。本函数(以及转码、波形)无法仅凭 WAV 头识别这种残留:
/// `duration_ms` 按字节如实算出,但它描述的只是"写到一半的长度",与笔记时间轴对不上。
/// 两条残余路径:
///
/// 1. **回滚失败**:删除新轨或截断续录轨时遇到权限/文件占用等错误,只留一行日志。
/// 2. **混音线程 panic**:`AudioTrackWriter::Drop` 照样把合法头补完刷盘,而正常的
///    删除逻辑根本没机会执行。
///
/// 两条路径**都没有盘上标记**,唯一线索是一行 eprintln——进程重启后连这行都没有。
/// 直接把返回的轨拿去回放,现象是"放到一半突然没声 / 时间轴对不上",极难归因。
///
/// 因此:**第二期消费前须自行校验**(例如拿 `TrackMeta.sync` 里两条源轨的 track_ms
/// 与本轨 duration_ms 交叉核对),或者先补一个盘上标记再消费——`set_track_sync` 是
/// 可直接照抄的模板(读改写 audio.json 的一个 TrackMeta 字段)。本期刻意不加标记:
/// 一期无人消费这条轨,加标记的正确位置在二期的消费方那里一并设计。
pub fn mixed_track(note_dir: &Path) -> Option<TrackInfo> {
    let meta = load_audio_meta(note_dir);
    track_info_for(note_dir, &meta, crate::pipeline::recording_sink::MIXED_TRACK)
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
        let _ = w.append(&[0.0, 0.5, -0.5, 1.0, -1.0, 2.0, -2.0]); // 越界值应被 clamp
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
        let _ = w.append(&vec![0.25f32; 2000]);
        drop(w);

        // 续录 base_ms=100(<125ms):首次 append 前先截断到 1600 样本再落新音频。
        let mut w = AudioTrackWriter::new(tmp.path(), "mic", 100);
        let _ = w.append(&vec![0.5f32; 160]);
        drop(w);
        let (_, samples) = read_wav(&tmp.path().join("mic.wav"));
        assert_eq!(samples.len(), 1600 + 160, "超长截断到 base_ms 后追加");
        assert_eq!(samples[1599], (0.25f32 * 32767.0) as i16, "截断保留前段");
        assert_eq!(samples[1600], (0.5f32 * 32767.0) as i16, "新音频落位 base_ms");

        // 再续录 base_ms=200(>110ms):零填充到 3200 样本再追加。
        let mut w = AudioTrackWriter::new(tmp.path(), "mic", 200);
        let _ = w.append(&vec![0.75f32; 16]);
        drop(w);
        let (_, samples) = read_wav(&tmp.path().join("mic.wav"));
        assert_eq!(samples.len(), 3200 + 16, "不足零填充到 base_ms 后追加");
        assert_eq!(samples[1760], 0, "填充部分为静音");
        assert_eq!(samples[3200], (0.75f32 * 32767.0) as i16);
    }

    /// 回滚基线必须与 open() 的对齐结果一致(截短方向),且在补零方向取现有内容长度
    /// ——补出来的零不是上一场的内容,不该冒充它留在盘上。两个方向都锁,否则混音旁路
    /// 放弃时会把本场字节或凭空的静音当成上一场内容留下来。
    #[test]
    fn pre_session_len_follows_alignment_and_never_exceeds_existing() {
        let tmp = tempfile::tempdir().unwrap();
        // 预存一条 125ms(2000 样本)的轨,offset_ms = 0。
        let mut w = AudioTrackWriter::new(tmp.path(), "mic", 0);
        let _ = w.append(&vec![0.25f32; 2000]);
        drop(w);
        let existing = std::fs::metadata(tmp.path().join("mic.wav")).unwrap().len();
        assert_eq!(existing, HEADER_LEN + 4000);

        // 截短方向(常态:base_ms 落在最后一句话的结束位置,文件尾还有静音)。
        assert_eq!(
            pre_session_track_len(tmp.path(), "mic", 100, existing),
            HEADER_LEN + 3200,
            "100ms @16k s16 = 3200 字节"
        );
        // 补零方向:基线取现有内容,不含 open() 将要补出的静音。
        assert_eq!(
            pre_session_track_len(tmp.path(), "mic", 200, existing),
            existing,
            "补零段不是上一场内容,基线不得越过现有长度"
        );
        // audio.json 缺项时走与 open() 同一条反推公式(offset ≈ base − 现有时长),
        // 结果等价于"保留全部现有内容"。
        std::fs::remove_file(tmp.path().join("audio.json")).unwrap();
        assert_eq!(pre_session_track_len(tmp.path(), "mic", 5000, existing), existing);
    }

    #[test]
    fn no_file_created_when_never_appended() {
        let tmp = tempfile::tempdir().unwrap();
        let w = AudioTrackWriter::new(tmp.path(), "system", 0);
        drop(w);
        assert!(!tmp.path().join("system.wav").exists(), "无帧不建档,不留空轨道");
        assert!(!tmp.path().join("audio.json").exists());
    }

    /// mixed worker 必须能观察 writer 失败并放弃成品轨,不能让 append 静默吞错。
    #[test]
    fn append_reports_track_creation_failure() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("mixed.wav")).unwrap();
        let mut w = AudioTrackWriter::new(tmp.path(), "mixed", 0);
        assert!(
            !w.append(&[0.1; 160]),
            "建档失败必须通过返回值传播给旁路状态机"
        );
    }

    #[test]
    fn track_created_mid_note_records_offset() {
        let tmp = tempfile::tempdir().unwrap();
        // 模拟旧笔记续录/第二场才授权的 system:base_ms=60000 时轨道才出现。
        let mut w = AudioTrackWriter::new(tmp.path(), "system", 60_000);
        let _ = w.append(&vec![0.1f32; 160]);
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
        let _ = w.append(&vec![0.1f32; 100]); // 不足 1s,头仍是 0
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
        let _ = w.append(&vec![0.1f32; 10]);
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
        let _ = w.append(&vec![0.1f32; AUDIO_SAMPLE_RATE as usize]); // 1s
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
    fn resume_clears_stale_waveform_for_recompute() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = AudioTrackWriter::new(tmp.path(), "mic", 0);
        let _ = w.append(&vec![0.1f32; AUDIO_SAMPLE_RATE as usize]); // 1s
        drop(w);
        // 模拟详情页懒回填把波形写进 audio.json。
        set_track_waveform(tmp.path(), "mic", vec![7u8; WAVEFORM_BUCKETS]).unwrap();
        assert!(load_audio_meta(tmp.path()).tracks["mic"].waveform.is_some());
        // 续录:新 writer 从 base_ms=1000 接着写,open() 改写 WAV 前应清掉过期波形,
        // 否则本场再中断时旧短波形会被拉伸到新时长错位显示。
        let mut w2 = AudioTrackWriter::new(tmp.path(), "mic", 1000);
        let _ = w2.append(&vec![0.2f32; AUDIO_SAMPLE_RATE as usize]); // +1s
        drop(w2);
        assert!(
            load_audio_meta(tmp.path()).tracks["mic"].waveform.is_none(),
            "续录改写 WAV 后旧波形作废,list_tracks 报 None 触发按新长度重算"
        );
    }

    #[test]
    fn list_tracks_tolerates_missing_or_corrupt_audio_json() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = AudioTrackWriter::new(tmp.path(), "mic", 0);
        let _ = w.append(&vec![0.1f32; 160]);
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
        let _ = w.append(&vec![0.1f32; (AUDIO_SAMPLE_RATE + AUDIO_SAMPLE_RATE / 2) as usize]);
        let (_, samples) = read_wav(&tmp.path().join("mic.wav"));
        assert!(samples.len() >= AUDIO_SAMPLE_RATE as usize, "录制中途文件即合法可读");
        drop(w);
    }

    #[test]
    fn list_tracks_prefers_m4a_with_recorded_duration() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = AudioTrackWriter::new(tmp.path(), "mic", 0);
        let _ = w.append(&vec![0.1f32; 1600]); // 100ms WAV
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
        let _ = w.append(&vec![0.1f32; 1600]);
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

    #[test]
    fn soft_aec_flag_and_clean_info_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        set_track_soft_aec(dir.path(), "mic").unwrap();
        set_track_soft_aec(dir.path(), "mic").unwrap(); // 幂等
        let meta = load_audio_meta(dir.path());
        assert_eq!(meta.tracks["mic"].soft_aec, Some(true));
        assert!(meta.tracks["mic"].clean.is_none());

        set_track_clean_info(
            dir.path(),
            "mic",
            CleanInfo {
                delay_ms: 600,
                confidence: 3.2,
                segments: 1,
                neural: Some(true),
            },
        )
        .unwrap();
        let meta = load_audio_meta(dir.path());
        let c = meta.tracks["mic"].clean.as_ref().unwrap();
        assert_eq!((c.delay_ms, c.segments), (600, 1));
        assert_eq!(c.neural, Some(true), "neural 应随 CleanInfo 往返");
    }

    #[test]
    fn sync_info_roundtrips_and_old_json_stays_valid() {
        let dir = tempfile::tempdir().unwrap();
        set_track_sync(
            dir.path(),
            "mic",
            SyncInfo {
                wall_ms: 60_000,
                // 原生 48k 单声道跑 60 秒的量级:不是 16k 口径,不参与任何毫秒换算。
                samples: 2_880_000,
                track_ms: 59_937,
                drift_ms: drift_ms(59_937, 60_000),
                silence_ms: 0,
                gaps: 0,
                rate_fixes: 1,
                ..Default::default()
            },
        )
        .unwrap();
        let meta = load_audio_meta(dir.path());
        let s = meta.tracks.get("mic").unwrap().sync.as_ref().expect("应已写入");
        assert_eq!(s.wall_ms, 60_000);
        assert_eq!(s.track_ms, 59_937);
        assert_eq!(s.samples, 2_880_000);
        assert_eq!(s.drift_ms, -63, "负值 = 该轨落盘时长不足墙钟,时钟跑慢");
        assert_eq!(s.rate_fixes, 1);

        // 名副其实的"旧 JSON":不含 sync 键的轨道必须照常反序列化,sync 落 None。
        let old: TrackMeta = serde_json::from_str(r#"{"offset_ms":7}"#).unwrap();
        assert_eq!(old.offset_ms, 7);
        assert!(old.sync.is_none(), "旧 JSON 无 sync 键 → None");

        // 更旧的一档:sync 已写入但没有 track_ms 键(本次口径修正之前落盘的记录)。
        // 必须能读进来 —— 若整条反序列化失败,load_audio_meta 会静默退回空表,
        // 连 offset_ms 都丢掉,回放对齐会被搞坏。
        let older: TrackMeta = serde_json::from_str(
            r#"{"offset_ms":0,"sync":{"wall_ms":60000,"samples":959000,"drift_ms":-63,
                "silence_ms":0,"gaps":0,"rate_fixes":0}}"#,
        )
        .unwrap();
        assert_eq!(older.sync.as_ref().unwrap().track_ms, 0, "缺 track_ms 键 → default 0");
    }

    /// 表驱动:drift_ms 的符号与量级。整个对账任务的数值核心,不允许零覆盖。
    #[test]
    fn drift_ms_signs_and_magnitudes() {
        // (track_ms, wall_ms, 期望 drift, 场景)
        let cases: &[(u64, u64, i64, &str)] = &[
            (60_500, 60_000, 500, "轨长于墙钟 → 正"),
            (59_500, 60_000, -500, "轨短于墙钟 → 负"),
            (60_000, 60_000, 0, "相等 → 零"),
            // 真实量级:48k 谎报为 44.1k 的 mic 跑半小时,轨比墙钟长约 2 分钟。
            (1_920_000, 1_800_000, 120_000, "真实量级:半小时录制漂 2 分钟"),
            (59_937, 60_000, -63, "亚百毫秒残余仍要如实带符号"),
            (0, 60_000, -60_000, "轨全空(采集死了)→ 全负"),
        ];
        for (track, wall, want, what) in cases {
            assert_eq!(drift_ms(*track, *wall), *want, "{what}");
        }
        // 符号可区分:同样偏 500ms,一正一负不能被绝对值抹平。
        assert_ne!(drift_ms(60_500, 60_000), drift_ms(59_500, 60_000));
    }

    /// 表驱动:轨净时长的换算,重点是续录场景要减对 carried(base − offset)。
    #[test]
    fn track_ms_from_wav_len_accounts_for_carried_length() {
        const HDR: u64 = HEADER_LEN;
        let ms_bytes = |ms: u64| ms * 16_000 / 1000 * 2; // 16k s16le mono
        // (文件总长, base_ms, offset_ms, 期望 track_ms, 场景)
        let cases: &[(u64, u64, u64, u64, &str)] = &[
            (HDR + ms_bytes(60_000), 0, 0, 60_000, "首场新轨:整条文件都是本场的"),
            (HDR, 0, 0, 0, "只有头没有数据 → 0"),
            (
                HDR + ms_bytes(90_000),
                60_000,
                0,
                30_000,
                "续录:首场就在的轨(offset=0),减掉 base 得本场增量",
            ),
            (
                HDR + ms_bytes(90_000),
                120_000,
                60_000,
                30_000,
                "续录:第二场才出现的轨(offset=60s),必须减 base−offset 而非 base",
            ),
            (
                HDR + ms_bytes(50_000),
                60_000,
                0,
                0,
                "文件比 carried 还短(本场没写/被截断)→ 饱和为 0,不出负数",
            ),
        ];
        for (len, base, offset, want, what) in cases {
            assert_eq!(track_ms_from_wav_len(*len, *base, *offset), *want, "{what}");
        }
    }

    /// session_track_ms:轨文件缺失时返回 None(调用方据此跳过,不写 SyncInfo)。
    #[test]
    fn session_track_ms_reads_file_and_skips_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(session_track_ms(dir.path(), "mic", 0), None, "无 WAV → None");

        let mut w = AudioTrackWriter::new(dir.path(), "mic", 0);
        let _ = w.append(&vec![0.1f32; 16_000]); // 1 秒 @16k
        drop(w); // Drop 收尾回写头
        assert_eq!(session_track_ms(dir.path(), "mic", 0), Some(1_000));

        // 续录:第二场 base=1000,轨已有 1 秒 → 本场增量为 0。
        assert_eq!(session_track_ms(dir.path(), "mic", 1_000), Some(0));
    }

    /// session_track_ms 必须真从 audio.json 读 offset_ms,不能形同虚设:上面那例全程
    /// offset_ms 恒为 0,即便读 offset 那行被改成写死的常量 0 也测不出来。这里让轨道
    /// 中途才出现(offset_ms=60_000,与建档时的 base_ms 一致,同 open() 新建分支的写入
    /// 语义),同一场内追加 30 秒 —— 本场 carried = base_ms(60_000) − offset_ms(60_000)
    /// = 0,track_ms 应等于全部本场内容 30_000。若 offset 被读成 0,carried 会错算成
    /// 60_000,30_000 饱和减出 0,与期望值 30_000 不同,才有鉴别力。
    #[test]
    fn session_track_ms_reads_offset_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = AudioTrackWriter::new(dir.path(), "mic", 60_000); // 建档即写 offset_ms=60_000
        let _ = w.append(&vec![0.1f32; 16_000 * 30]); // 30 秒 @16k
        drop(w);
        assert_eq!(session_track_ms(dir.path(), "mic", 60_000), Some(30_000));
    }

    #[test]
    fn sync_absent_serializes_to_old_shape() {
        // 未写 sync 的轨道,JSON 不该出现该键(新旧版本双向兼容,与 codec/waveform 同策略)
        let t = TrackMeta { offset_ms: 5, ..Default::default() };
        let j = serde_json::to_string(&t).unwrap();
        assert!(!j.contains("sync"), "无 sync 时不应序列化该键: {j}");
    }

    /// 冒烟实锤回归:soft_aec 标记先于轨道建档写入,建档(open)不得整条替换抹掉它。
    #[test]
    fn track_open_preserves_prior_soft_aec_marker() {
        let dir = tempfile::tempdir().unwrap();
        set_track_soft_aec(dir.path(), "mic").unwrap();

        let mut w = AudioTrackWriter::new(dir.path(), "mic", 0);
        let _ = w.append(&vec![0.1f32; 160]); // 触发首次 open 建档
        drop(w);

        let meta = load_audio_meta(dir.path());
        assert_eq!(meta.tracks["mic"].soft_aec, Some(true), "建档不得抹掉先写入的 soft_aec 标记");
        assert_eq!(meta.tracks["mic"].offset_ms, 0);
    }

    /// 旧 audio.json(无新字段)必须照常反序列化——新字段全 default。
    #[test]
    fn old_audio_json_without_new_fields_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("audio.json"),
            r#"{"schema_version":1,"tracks":{"mic":{"offset_ms":0}}}"#,
        )
        .unwrap();
        let meta = load_audio_meta(dir.path());
        assert_eq!(meta.tracks["mic"].soft_aec, None);
        assert!(meta.tracks["mic"].clean.is_none());
    }

    /// 旧 clean 记录(P3b 引入 neural 字段前写入,无 neural 键)必须照常反序列化,
    /// 缺字段落到 None(而非 Some(false)),如实区分"未知"与"AEC3-only"。
    #[test]
    fn old_clean_info_without_neural_field_defaults_to_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("audio.json"),
            r#"{"schema_version":1,"tracks":{"mic":{"offset_ms":0,"clean":{"delay_ms":600,"confidence":3.2,"segments":1}}}}"#,
        )
        .unwrap();
        let meta = load_audio_meta(dir.path());
        let c = meta.tracks["mic"].clean.as_ref().unwrap();
        assert_eq!((c.delay_ms, c.segments), (600, 1));
        assert_eq!(c.neural, None, "旧记录缺 neural 键应落到 None");
    }

    /// 核心回归:mic/system/mixed 三条 WAV 同时在场(方案 B 录制),list_tracks 只应
    /// 报两条源轨——mixed 混进播放器轨列表会导致 mic+system+mixed 三重叠加播放。
    #[test]
    fn list_tracks_excludes_mixed_track_when_source_tracks_present() {
        let tmp = tempfile::tempdir().unwrap();
        for source in ["mic", "system", crate::pipeline::recording_sink::MIXED_TRACK] {
            let mut w = AudioTrackWriter::new(tmp.path(), source, 0);
            let _ = w.append(&vec![0.1f32; 160]);
            drop(w);
        }

        let tracks = list_tracks(tmp.path());
        let sources: Vec<&str> = tracks.iter().map(|t| t.source.as_str()).collect();
        assert_eq!(tracks.len(), 2, "mixed 不应出现在播放器轨列表里: {sources:?}");
        assert!(sources.contains(&"mic"));
        assert!(sources.contains(&"system"));
        assert!(
            !sources.contains(&crate::pipeline::recording_sink::MIXED_TRACK),
            "成品轨不得混进源轨列表"
        );
    }

    /// mixed_track 单独取成品轨,口径(offset_ms/时长)与 list_tracks 对源轨的口径一致。
    #[test]
    fn mixed_track_returns_the_mixed_source_with_consistent_semantics() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = AudioTrackWriter::new(tmp.path(), "mic", 0);
        let _ = w.append(&vec![0.1f32; AUDIO_SAMPLE_RATE as usize]); // 1s
        drop(w);
        let mut w = AudioTrackWriter::new(
            tmp.path(),
            crate::pipeline::recording_sink::MIXED_TRACK,
            0,
        );
        let _ = w.append(&vec![0.2f32; (AUDIO_SAMPLE_RATE / 2) as usize]); // 0.5s
        drop(w);

        let mixed = mixed_track(tmp.path()).expect("mixed.wav 存在时应能取到成品轨");
        assert_eq!(mixed.source, crate::pipeline::recording_sink::MIXED_TRACK);
        assert_eq!(mixed.offset_ms, 0);
        assert_eq!(mixed.duration_ms, 500, "时长口径与 list_tracks 对 WAV 源轨一致(字节数换算)");

        // list_tracks 里 mic 走同一套 track_info_for,offset/时长口径应互相印证。
        let tracks = list_tracks(tmp.path());
        let mic = tracks.iter().find(|t| t.source == "mic").unwrap();
        assert_eq!(mic.duration_ms, 1000);
        assert_eq!(mic.offset_ms, mixed.offset_ms, "两轨同场起录,offset 口径一致");
    }

    /// 没有 mixed.wav(未开方案 B / 旧笔记)时,mixed_track 返回 None 而不是报错或伪造。
    #[test]
    fn mixed_track_returns_none_when_no_mixed_wav() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = AudioTrackWriter::new(tmp.path(), "mic", 0);
        let _ = w.append(&vec![0.1f32; 160]);
        drop(w);
        assert!(mixed_track(tmp.path()).is_none());
    }

    /// 只有 mixed.wav、没有源轨(极端/测试态)时,list_tracks 必须返回空,
    /// 不能因为 known_sources 里含 mixed 就把它漏给播放器。
    #[test]
    fn list_tracks_empty_when_only_mixed_track_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = AudioTrackWriter::new(
            tmp.path(),
            crate::pipeline::recording_sink::MIXED_TRACK,
            0,
        );
        let _ = w.append(&vec![0.1f32; 160]);
        drop(w);

        assert!(list_tracks(tmp.path()).is_empty(), "只有成品轨时源轨列表应为空");
        assert!(mixed_track(tmp.path()).is_some(), "但 mixed_track 仍应能取到它");
    }

    /// MixInfo 是「正常定稿」的盘上证据:回滚失败/线程 panic 两条残留路径(见
    /// mixed_track 文档)都写不到它。set_track_mix 走读改写 audio.json,不碰其他字段。
    #[test]
    fn set_track_mix_persists_and_preserves_other_fields() {
        let dir = tempfile::tempdir().unwrap();
        set_track_sync(
            dir.path(),
            "mixed",
            SyncInfo {
                wall_ms: 1,
                samples: 0,
                track_ms: 5000,
                drift_ms: 4999,
                silence_ms: 0,
                gaps: 0,
                rate_fixes: 0,
                ..Default::default()
            },
        )
        .unwrap();
        let mix = MixInfo {
            origin: "live".into(),
            seek_offset_ms: [("system".to_string(), 120u64)].into_iter().collect(),
            track_ms: 5000,
            clipped_samples: 0,
            limited_samples: 0,
            limit_metered: true,
        };
        set_track_mix(dir.path(), "mixed", mix.clone()).unwrap();
        let meta = load_audio_meta(dir.path());
        let t = meta.tracks.get("mixed").expect("track 条目");
        assert_eq!(t.mix.as_ref(), Some(&mix));
        assert!(t.sync.is_some(), "既有字段不得被覆盖丢失");
    }

    /// 旧 audio.json(无 first_frame_offset_ms)必须照常反序列化为 None;
    /// 新写出的 JSON 有该字段且往返保真。字段语义:本源首个真实帧相对本场最早
    /// 首帧的偏移(16k 口径换算成 ms),是 mixed 轨段落 seek 修正的数据来源。
    #[test]
    fn sync_first_frame_offset_roundtrip_and_backcompat() {
        let old = r#"{"wall_ms":1,"samples":2,"track_ms":3,"drift_ms":2,"silence_ms":0,"gaps":0,"rate_fixes":0}"#;
        let s: SyncInfo = serde_json::from_str(old).expect("旧数据必须能解析");
        assert_eq!(s.first_frame_offset_ms, None);

        let with = SyncInfo { first_frame_offset_ms: Some(120), ..s };
        let json = serde_json::to_string(&with).unwrap();
        let back: SyncInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.first_frame_offset_ms, Some(120));
    }
}
