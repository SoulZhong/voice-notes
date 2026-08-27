//! 每源帧转发级(capture → tap → segment_worker):健康统计 + 断流静音填充 + 失联通知。
//!
//! 设计源自对 meetily 的对比调研(2026-07-18 计划文档):它的 SourceBuffer 用
//! gap 检测 + 静音插入保住「混音前两路等长」;本仓是双路独立架构,不混音,但
//! **每源时间轴 = 已接受样本数 / 采样率**,采集断流会让该轨时钟落后墙钟,双轨
//! 时间戳从此错位(mic 说的话和 system 的回答在纪要里前后颠倒)。因此把同一
//! 设计翻译到逐源管线:断流期间按墙钟差补零帧,时间轴不塌。
//!
//! 这在 Windows 上不是鲁棒性而是**正确性**:WASAPI loopback 无音频播放时回调
//! 根本不触发(cpal 对 eRender 设备走 AUDCLNT_STREAMFLAGS_LOOPBACK 的固有行为,
//! swyh-rs 以 "InjectSilence" 补偿同款问题)——系统声轨的静默期全靠本级补齐。
//! macOS SCK 静音也持续回调,mic(cpal/VPIO)同理,填充仅在设备真异常时兜底。
//!
//! 失联通知(on_stall/on_recover)供会话级断连自愈(ResilientCapture)消费:
//! 帧荒超阈值报一次 stall(去抖:恢复前不重复),恢复后报 recover 并允许再触发。

use crate::audio::{AudioCapture, AudioFrame, Source};
use crate::pipeline::drift_monitor::DriftMonitor;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use serde::Serialize;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// 时钟核对的默认评估窗。见 `TapPolicy::rate_eval_window`。
const RATE_EVAL_WINDOW: Duration = Duration::from_secs(30);
/// 快窗:粗偏差(>RATE_FAST_TOLERANCE)不必等满窗,5 秒就下手。
///
/// 为什么要有它:检测期间的内容**已经按错的率发下去了**,补齐只能把之后的内容摆正,
/// 补不回窗内那段的相位。窗内残余 = 窗长 × 偏差,44.1k 被当 48k 时满窗要留 2.44s
/// 残余(整段听得见重影),快窗把它压到 0.44s。彻底消掉需要缓冲整个窗再放行,那会给
/// 实时转写加同样长的启动延迟,不划算——粗偏差正是伤害最大、又最容易在 5 秒内认定的
/// 那一类,细偏差(1~3%)每秒只错开几十毫秒,等满窗拿更稳的估计更值。
const RATE_FAST_WINDOW: Duration = Duration::from_secs(5);
/// 快窗判定用的容差。远高于 RATE_TOLERANCE:5 秒窗的测量噪声更大,只用来抓
/// "采样率整个记错了"这种量级(实测那场是 8.8%),不抓晶振失配那种细活。
const RATE_FAST_TOLERANCE: f64 = 0.03;
/// 相位还款窗:检测期间累计的错位在这么长的后续时间里**线性**还清。
const PHASE_TRIM_SECS: f64 = 60.0;
/// 还款时单帧最多把采样率拧动多少(2%,约 35 音分,连续语音上听不出来)。
/// 它只是上限;实际速率由「剩余债务 / 剩余时间」定,通常远低于此。
const PHASE_TRIM_MAX: f64 = 0.02;
/// 债务小于此值即视为还清(半毫秒,远低于任何听感与门控阈值)。
const PHASE_TRIM_EPS: f64 = 0.000_5;
/// mixed 轨固定为 16kHz;首帧墙钟偏移按这个口径交给时间轴混音器。
const MIX_TIMELINE_RATE: u64 = 16_000;

/// 还款期内**恒定**的目标采样率:债务确定时一次算定,直到还清都不再变。
///
/// 恒定是硬要求,不只是简洁:下游 `segment_worker` 一见 `sample_rate` 变化就重建
/// `StreamResampler`,而那个重采样器的全部意义就是跨块保持相位与全局样本计数
/// (见其文档:逐块重建会注入 ~0.2% 的时钟漂移)。逐帧按当前债务重算速率会在 60s
/// 内产生上百次不同的整数率、即上百次重建,等于把它设计要解决的问题又造回来。
/// 一次算定则整个还款期只有两三次变率。
///
/// 恒定率同时天然是线性还款:每秒还固定的量,窗内正好还清。逐帧重算才会退化成
/// 指数衰减(d(debt)/dt = -debt/T,一个窗过完仍剩 1/e)。
///
/// 残留代价:每次变率仍会让下游重建一次重采样器,丢掉不足一个样本的相位与计数余数
/// (~20µs)。整段还款只变两三次,合计几十微秒,远在听感与门控阈值之下,故不再为此
/// 去改 StreamResampler 的相位表示——那需要把它从"全局计数"改成"显式相位",
/// 收益与风险不成比例。
fn phase_trim_rate(debt: f64, window: f64, base_rate: u32, max: f64) -> u32 {
    let base = base_rate as f64;
    let k = (debt / window.max(1e-3)).clamp(-max, max);
    let target = base * (1.0 + k);
    // 取整避开 round:债务小到 k·base<0.5Hz 时 round 会把率原样还回来,还款额恒 0。
    (if k > 0.0 { target.ceil() } else { target.floor() }).max(1.0) as u32
}

/// 本帧还款:返回 (本帧该盖的采样率, 实际还掉的秒数)。
///
/// 常规帧直接用恒定的 `held`;只有**最后一帧**——按 held 还款会超过剩余债务时——
/// 才反解一个不过冲的整数率。不能只把账面 `repaid` 截断到 debt 而仍盖 held:
/// 下游与 `forwarded` 都按盖上去的率算时长,截断账面等于债务清零了、真实时间轴却
/// 已经过冲。10ms 帧上这点差是几十微秒,但断流恢复后的突发帧(可达上百毫秒)会把它
/// 放大成可闻错位。
fn phase_trim_step(debt: f64, held: u32, base_rate: u32, per_channel: usize) -> (u32, f64) {
    let (base, per) = (base_rate as f64, per_channel as f64);
    let full = per / base - per / held as f64;
    if full.abs() <= debt.abs() {
        return (held, full);
    }
    // 反解 per/base - per/r = debt,并朝"还不足"的一侧取整,保证不过冲。
    let denom = per / base - debt;
    if denom <= 0.0 {
        return (base_rate, 0.0); // 数值退化:本帧不还,留给下一帧
    }
    let exact = per / denom;
    let r = (if debt > 0.0 { exact.floor() } else { exact.ceil() }).max(1.0);
    let repaid = per / base - per / r;
    if repaid.abs() > debt.abs() {
        return (base_rate, 0.0); // 仍会过冲则不还,宁可留着
    }
    (r as u32, repaid)
}

/// 实测速率与当前生效采样率相差多少才改写(1%)。
///
/// 下界由测量噪声定:30s 窗内帧到达抖动 ±20ms 只带来 ~0.07% 误差,1% 有充足余量,
/// 不会追着噪声抖。上界由危害定:1% 即每分钟错位 0.6s,一小时的会能拉开 36s,
/// 必须拦下。真正的设备晶振失配在 100ppm 量级(0.01%),落在容差内不动它——那种
/// 量级 AEC 自己能跟上,而按噪声去改写反而有害。
const RATE_TOLERANCE: f64 = 0.01;
/// 实测速率相对声明值的合理性夹板。落在此范围外的偏差不可能是"声明的率写错了"
/// (设备不会差一倍以上),而是丢帧/缓冲回吐之类的别的故障,改写采样率只会把时间轴
/// 推得更歪——这时保留声明值并留日志。
const RATE_SANITY: (f64, f64) = (0.5, 2.0);

/// 判洞的绝对下限(与"至少缺一整帧"取大者):挡住时戳量化与设备时钟抖动
/// (实测同帧内 <100µs),也避免超短帧场景下把噪声当洞。
const HW_HOLE_MIN: Duration = Duration::from_millis(3);

/// 判洞比较的舍入容差:见 holey 处注释。
const HW_ROUND_TOL: Duration = Duration::from_millis(1);

/// 单个补零帧的上限时长。一次长断流(睡眠/长时间无回调)的洞可能是分钟级,
/// 一把 Vec 分配下去就是上百 MB(Codex P1);按这个粒度切块发,峰值内存可控。
const FILL_CHUNK: Duration = Duration::from_millis(200);


/// 每源健康计数(原子字段,tap 线程写、查询命令读,无锁)。
#[derive(Default)]
pub struct SourceHealth {
    pub frames: AtomicU64,
    pub samples: AtomicU64,
    /// 帧荒次数(一次断流不论多长计 1;以 fill_after 为判定阈值)。
    pub gaps: AtomicU32,
    /// 累计填充的静音时长(毫秒)。
    pub silence_ms: AtomicU64,
    /// 采集重启次数(由 ResilientCapture 递增,tap 不写)。
    pub restarts: AtomicU32,
    /// 时钟核对改写采样率的次数(>0 说明该源声明的采样率与实测不符)。
    pub rate_fixes: AtomicU32,
    /// 硬件时基**区间作废**次数:这一段不能拿去测采样率。触发条件是所有
    /// `usable == false` 的情形——时戳回退、跳变 ≥ fill_after、突发缓冲回吐
    /// (样本比时间多)、隐含速率越出 RATE_SANITY 夹板,**以及**真丢样的洞。
    ///
    /// 注意它不是「丢了多少次样本」:洞只是它的子集。2026-08-17 排障把本字段
    /// 当洞计数读,得出「514 次硬件断流」的错误结论(Codex 复核 P1)——真丢样
    /// 看 `hw_holes` 与 `hw_gap_ms`。消费端再慢,只要采集连续此值不动,与
    /// send_wait/cap_queue_hw 合起来仍能把「worker 慢」和「设备/HAL 丢帧」分开。
    pub hw_gaps: AtomicU32,
    /// 判定为**真丢了样本**的洞次数(hw_gaps 的子集,与 hw_gap_ms 同源同判据)。
    /// 「发生过几次断流」只能看它。
    ///
    /// 但它**不区分是谁丢的**(Codex review P2):判洞看的是"时间过去了、样本没来",
    /// HAL/设备丢的和我们自己回调 try_send 丢的在硬件时戳上完全同形,都会记在这里。
    /// 想分清归因必须结合 cap_dropped_samples,不能单看它就断言"该换设备"。
    pub hw_holes: AtomicU32,
    /// 采集回调因下游队列满而 try_send 丢弃的样本数(每通道)。停流时从采集
    /// 后端取回。它计的是缺口里**可归因到我们自己**的那部分(下游背压顶到了采集
    /// 回调)。注意量纲:这里是样本数,hw_holes 是次数,两者不可直接比较——联判要
    /// 先按原生采样率把样本换成毫秒再与 hw_gap_ms 比。详见 store::audio::SyncInfo
    /// 上的说明(含两个已知偏差与 0 的二义)。
    pub cap_dropped_samples: AtomicU64,
    /// 按硬件时戳判定并**已补回时间轴**的洞总时长(ms)。hw_gaps 只说"发生过",
    /// 这个说"丢了多久、补了多久"——1694 次断裂对应多少秒,此前无从对账
    /// (2026-08-16:一场 98 分钟录音 438 秒被直接压掉,就是因为只计数不补)。
    pub hw_gap_ms: AtomicU64,
    /// 采集→tap 队列深度高水位(tap 每收一帧采样一次残余深度)。持续走高 =
    /// tap 或其下游追不上采集节奏。
    pub cap_queue_hw: AtomicU32,
    /// tap 向 worker 转发被阻塞的累计/单次峰值毫秒数。>0 = worker 侧背压已经
    /// 顶到 tap;逼近采集缓冲总量时回调将被阻塞、HAL 开始真丢样。
    pub send_wait_ms: AtomicU64,
    pub send_wait_max_ms: AtomicU64,
    /// 首个真实帧相对本场最早首帧的 16k 样本偏移,+1 编码(0 表示尚未见首帧)。
    /// mixed sink 在首个重采样块到达时读取,把两源放进同一墙钟原点。
    first_frame_offset_16k_plus_one: AtomicU64,
}

/// 健康快照(pipeline_health 命令的序列化单元)。
#[derive(Debug, Clone, Serialize)]
pub struct HealthSnapshot {
    pub source: String,
    pub frames: u64,
    pub samples: u64,
    pub gaps: u32,
    pub silence_ms: u64,
    pub restarts: u32,
    pub rate_fixes: u32,
    pub hw_gaps: u32,
    #[serde(default)]
    pub hw_holes: u32,
    #[serde(default)]
    pub hw_gap_ms: u64,
    #[serde(default)]
    pub cap_dropped_samples: u64,
    pub cap_queue_hw: u32,
    pub send_wait_ms: u64,
    pub send_wait_max_ms: u64,
}

impl SourceHealth {
    pub fn snapshot(&self, source: Source) -> HealthSnapshot {
        HealthSnapshot {
            source: source.as_str().to_string(),
            frames: self.frames.load(Ordering::Relaxed),
            samples: self.samples.load(Ordering::Relaxed),
            gaps: self.gaps.load(Ordering::Relaxed),
            silence_ms: self.silence_ms.load(Ordering::Relaxed),
            restarts: self.restarts.load(Ordering::Relaxed),
            rate_fixes: self.rate_fixes.load(Ordering::Relaxed),
            hw_gaps: self.hw_gaps.load(Ordering::Relaxed),
            hw_holes: self.hw_holes.load(Ordering::Relaxed),
            hw_gap_ms: self.hw_gap_ms.load(Ordering::Relaxed),
            cap_dropped_samples: self.cap_dropped_samples.load(Ordering::Relaxed),
            cap_queue_hw: self.cap_queue_hw.load(Ordering::Relaxed),
            send_wait_ms: self.send_wait_ms.load(Ordering::Relaxed),
            send_wait_max_ms: self.send_wait_max_ms.load(Ordering::Relaxed),
        }
    }

    fn record_first_frame(&self, now: Instant, origin: &OnceLock<Instant>) {
        let zero = *origin.get_or_init(|| now);
        let elapsed = now.checked_duration_since(zero).unwrap_or_default();
        let offset = (elapsed.as_nanos() * MIX_TIMELINE_RATE as u128 / 1_000_000_000) as u64;
        let _ = self.first_frame_offset_16k_plus_one.compare_exchange(
            0,
            offset.saturating_add(1),
            Ordering::Release,
            Ordering::Relaxed,
        );
    }

    /// 尚未见首帧时保守返回 0;正常生产路径中 audio sink 只会在首帧经过 tap 后调用。
    pub fn first_frame_offset_16k(&self) -> u64 {
        self.first_frame_offset_16k_plus_one
            .load(Ordering::Acquire)
            .saturating_sub(1)
    }

    #[cfg(test)]
    pub(crate) fn set_first_frame_offset_16k_for_test(&self, offset: u64) {
        self.first_frame_offset_16k_plus_one
            .store(offset.saturating_add(1), Ordering::Release);
    }
}

/// 断流填充与失联判定的每源策略。阈值依据(计划文档「依赖调研结论」):
/// - Mic:正常麦克风静音也持续出帧,帧荒即设备异常 → 500ms 起填。
/// - System(macOS SCK):静音也回调,帧荒罕见 → 1s 起填,宽容调度毛刺。
/// - System(Windows loopback):无播放即无回调,填充属常态 → 250ms 起填,
///   压低静默期时间轴误差(每次断流的填充起点误差上限即 fill_after)。
/// stall_after 供断连自愈:mic 帧荒 3s 基本可判设备死亡;System 在 Windows
/// 上「长时间无回调」是正常静默,不能据此判死 → 传 None 关闭失联通知。
#[derive(Clone, Copy)]
pub struct TapPolicy {
    /// 帧荒超过该时长后开始补零帧(也是 gap 计数的判定阈值)。
    pub fill_after: Duration,
    /// 帧荒超过该时长后触发 on_stall(None = 不判失联)。
    pub stall_after: Option<Duration>,
    /// recv 超时步长,亦即每轮补零的粒度上限。
    pub tick: Duration,
    /// 时钟核对的评估窗:累计到这么久的「连续出帧时间」才做一次实测速率判定。
    /// 越长越稳(抖动被平均掉)、越迟钝;30s 下调度抖动带来的误差约 0.1%,远低于
    /// RATE_TOLERANCE,又能在半分钟内追上设备换率。
    pub rate_eval_window: Duration,
    /// 快窗:粗偏差不等满窗,累计到这么久就判一次(阈值 RATE_FAST_TOLERANCE)。
    pub rate_fast_window: Duration,
    /// 相位还款:把检测期累计的错位摊到多长时间里还清,以及单帧最多拧动多少采样率。
    /// 生产取 60s/2%(听不出);测试可缩短以在秒级内跑完同一段收敛逻辑。
    pub phase_trim_window: Duration,
    pub phase_trim_max: f64,
    /// 断流风暴告警的滚动窗与阈值(洞时长/墙钟)。窗越长越稳、越迟钝;
    /// 阈值 0.05 = 窗内 5% 的时长是补零就叫。见 GapStormDetector。
    /// 覆盖矩阵(issue #125):判据 hw_gap_ms 只在硬件时戳判洞分支增长——
    /// macOS mic/system 与 Windows mic 有时戳、能报;**Windows system
    /// (loopback,host_time_ns=None)是已声明盲区**,不能拿补零量替代判据
    /// (loopback「无播放即无回调」,补零是常态,换判据=满屏误报)。
    pub gap_storm_window: Duration,
    pub gap_storm_threshold: f64,
}

/// 断流风暴默认判据:一分钟窗、5%。
/// 窗取 60s:短于此,一次一秒级断流就能把比例顶过阈值(蓝牙偶发一次是常态);
/// 长于此,报警来得太迟,会已经开完了。
/// 阈值取 5%:2026-08-17 那场是 14.2%,而健康场次实测在 0.1% 以下,
/// 5% 落在两者中间足够宽的空档里,不靠调参也不会误报。
const GAP_STORM_WINDOW: Duration = Duration::from_secs(60);
const GAP_STORM_THRESHOLD: f64 = 0.05;

impl TapPolicy {
    pub fn mic() -> Self {
        Self {
            fill_after: Duration::from_millis(500),
            stall_after: Some(Duration::from_secs(3)),
            tick: Duration::from_millis(100),
            rate_eval_window: RATE_EVAL_WINDOW,
            rate_fast_window: RATE_FAST_WINDOW,
            phase_trim_window: Duration::from_secs_f64(PHASE_TRIM_SECS),
            phase_trim_max: PHASE_TRIM_MAX,
            gap_storm_window: GAP_STORM_WINDOW,
            gap_storm_threshold: GAP_STORM_THRESHOLD,
        }
    }
    #[cfg(target_os = "macos")]
    pub fn system_sck() -> Self {
        Self {
            fill_after: Duration::from_secs(1),
            // SCK 静音也持续回调,帧荒 5s 基本可判流死亡(权限被撤/内部崩溃)。
            // 阈值比 mic 宽:SCK 偶发调度毛刺比 cpal 常见,宁慢勿误杀。
            stall_after: Some(Duration::from_secs(5)),
            tick: Duration::from_millis(100),
            rate_eval_window: RATE_EVAL_WINDOW,
            rate_fast_window: RATE_FAST_WINDOW,
            phase_trim_window: Duration::from_secs_f64(PHASE_TRIM_SECS),
            phase_trim_max: PHASE_TRIM_MAX,
            gap_storm_window: GAP_STORM_WINDOW,
            gap_storm_threshold: GAP_STORM_THRESHOLD,
        }
    }
    #[cfg(windows)]
    pub fn system_loopback() -> Self {
        Self {
            fill_after: Duration::from_millis(250),
            stall_after: None,
            tick: Duration::from_millis(100),
            rate_eval_window: RATE_EVAL_WINDOW,
            rate_fast_window: RATE_FAST_WINDOW,
            phase_trim_window: Duration::from_secs_f64(PHASE_TRIM_SECS),
            phase_trim_max: PHASE_TRIM_MAX,
            gap_storm_window: GAP_STORM_WINDOW,
            gap_storm_threshold: GAP_STORM_THRESHOLD,
        }
    }
}

/// 断流风暴判定:滚动窗口内「洞时长 / 墙钟时长」越过阈值就报一次。
///
/// 为什么需要它:`stall_after`(连续 3 秒无帧)只抓得住设备死亡,抓不住
/// 「每次几百毫秒、每分钟二十来次」的高频短断流。2026-08-17 那场蓝牙会议
/// 14.2% 的时长是补零,用户全程零提示,只能会后靠耳朵发现掉字。
///
/// 判据刻意用 `hw_gap_ms`(真丢样时长)除以墙钟,不用 `hw_gaps` 次数——后者
/// 把突发回吐、时戳回退一并计入,语义已被证伪(见 SourceHealth::hw_gaps)。
///
/// 只在**沿**上报:起风暴报一次 Storm,平息报一次 Recovered,中间一律安静。
/// 风暴往往持续整场,每个 tick 都报等于让用户学会忽略它。
///
/// 必须有 Recovered 这一条(Codex review P2):只发上升沿的话,消费端就只能拿定时器
/// 让提示自行过期——而风暴若持续整场,detector 一直不重新武装、不再发事件,提示会
/// 在断流仍在继续时悄悄消失。有了恢复沿,「显示到风暴平息」才真的成立。
///
/// **覆盖范围限制**:判据是 `hw_gap_ms`,它只在**硬件时戳**判洞分支增长。拿不到
/// 时戳的采集后端(Windows loopback 恒 `host_time_ns: None`)因此永远报不出风暴。
///
/// 这是刻意保留的(Codex 二轮 P2 建议改用补零增量,不采纳):Windows loopback 是
/// 「无播放即无回调」,补零本就是常态(见 TapPolicy::system_loopback 的说明),
/// 拿补零量当判据会在那个平台上把静音全报成风暴——把一个漏报换成满屏误报。
/// macOS 两轨与 Windows 的 mic 轨(cpal 有时戳)都不受此限,覆盖到了实际出事的场景。
/// 要覆盖 Windows system 轨,得先有办法把「该响却没响」和「本来就没声音」分开,
/// 那是独立课题。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GapStormEvent {
    /// 起风暴。参数 = 窗内洞时长占比。
    Storm(f64),
    /// 已平息(只有真起过风暴才会发)。
    Recovered,
}

pub struct GapStormDetector {
    window: Duration,
    threshold: f64,
    /// 窗内采样点 (时刻, 累计洞时长 ms);超出窗长的从头部丢弃。
    samples: std::collections::VecDeque<(Instant, u64)>,
    /// false = 正处在已报告的风暴中(等平息);true = 平静,可报下一次风暴。
    armed: bool,
}

impl GapStormDetector {
    pub fn new(window: Duration, threshold: f64) -> Self {
        Self {
            window,
            threshold,
            samples: std::collections::VecDeque::new(),
            armed: true,
        }
    }

    /// 喂入当前时刻与**累计**洞时长。只在沿上返回事件,平时返回 None。
    pub fn observe(&mut self, now: Instant, cumulative_gap_ms: u64) -> Option<GapStormEvent> {
        self.samples.push_back((now, cumulative_gap_ms));
        // 留一个刚好越过窗左沿的点做基准,窗才是满的;否则开录头一分钟
        // 分母偏小,一个洞就能把比例顶过阈值(早期误报)。
        while self.samples.len() > 2
            && now.duration_since(self.samples[1].0) >= self.window
        {
            self.samples.pop_front();
        }
        let (t0, g0) = *self.samples.front()?;
        let span = now.duration_since(t0);
        if span < self.window {
            return None; // 窗未满,不判——样本不足的比例没有意义
        }
        let ratio = cumulative_gap_ms.saturating_sub(g0) as f64 / span.as_millis().max(1) as f64;
        // 钳制到 1.0(2026-08-27 实测:采集线程卡死恢复后,整段停摆期攒的补零一次性
        // 计入累计值,60s 窗读出「5300%」——数值真实反映了欠账,但作为「窗内占比」
        // 展示就是胡话,also 见 issue #123 的口径清单)。>100% 与 =100% 的处置一样:
        // 这一窗全是洞。
        let ratio = ratio.min(1.0);
        if ratio > self.threshold {
            if self.armed {
                self.armed = false;
                return Some(GapStormEvent::Storm(ratio));
            }
        } else if !self.armed {
            // 只有真起过风暴(armed 被拉低过)才发恢复沿——否则安静的一场会在
            // 首次满窗时凭空发一条"已恢复"。
            self.armed = true;
            return Some(GapStormEvent::Recovered);
        }
        None
    }
}

/// 失联/恢复通知(可选;断连自愈与 UI 事件在装配层接入)。
pub struct TapNotify {
    pub on_stall: Option<Box<dyn Fn() + Send>>,
    pub on_recover: Option<Box<dyn Fn() + Send>>,
    /// 断流风暴沿:`Some(比例)` = 起风暴,`None` = 已平息。与 on_stall 互补:
    /// on_stall 抓的是「连续几秒一帧没有」的设备死亡,这个抓的是
    /// 「每次几百毫秒、密集反复」的高频短断流——后者不触发前者,
    /// 却能吃掉一场会 14% 的内容(2026-08-17 蓝牙实录)。
    /// 两条沿都要报,消费端才能一直显示到平息,而不是靠定时器让提示自行过期。
    pub on_gap_storm: Option<Box<dyn Fn(Option<f64>) + Send>>,
}

impl TapNotify {
    pub fn none() -> Self {
        Self { on_stall: None, on_recover: None, on_gap_storm: None }
    }
}

/// 把任意 `AudioCapture` 包上 tap 级的适配器:对 session 层完全透明——
/// start_session 拿到的仍是一个 `AudioCapture`,帧通道语义(关闭级联、Mock
/// 同步灌帧兼容)原样保持。tap 线程在 start 内先于内层采集启动(消费者先行),
/// 内层启动失败时 cap_tx 随错误路径 drop → tap 退出 → sink drop → worker 退出,
/// 与无 tap 时的失败级联一致。stop 后 join tap(通道断开即返回,不久等)。
///
/// 为什么包装而不是改 start_session:平台策略(fill_after 阈值)与健康暴露属于
/// 装配层关心的事,session 层保持平台无关;且既有 Mock 流测试不被填充语义波及。
/// 收尾 join 采集/tap 线程的等待上限(issue #182)。测试档缩短:回归测试要验证
/// 「卡死线程不拖死 stop」,不该真等 5 秒。
#[cfg(not(test))]
const TAP_JOIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(test)]
const TAP_JOIN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);

pub struct TappedCapture {
    inner: Box<dyn AudioCapture>,
    source: Source,
    policy: TapPolicy,
    health: Arc<SourceHealth>,
    /// start 时取走(TapNotify 非 Clone);重复 start 本仓不存在,取空则退化为无通知。
    notify: Option<TapNotify>,
    tap: Option<std::thread::JoinHandle<()>>,
    /// 收尾取消旗(issue #182):采集线程卡死不丢发送端时,tap 靠它自行退出并
    /// 放掉下游发送端,segment worker 才解得开。
    tap_cancel: Arc<std::sync::atomic::AtomicBool>,
    timeline_origin: Arc<OnceLock<Instant>>,
    /// 时钟漂移监视器(Task 5 接线);None = 不采样(未启用/未装配)。
    drift: Option<Arc<DriftMonitor>>,
}

impl TappedCapture {
    pub fn new(
        inner: Box<dyn AudioCapture>,
        source: Source,
        policy: TapPolicy,
        health: Arc<SourceHealth>,
        notify: TapNotify,
    ) -> Self {
        Self::new_with_timeline_origin(
            inner,
            source,
            policy,
            health,
            notify,
            Arc::new(OnceLock::new()),
        )
    }

    /// 同一会话的所有源共享 timeline_origin;第一个真实帧把它钉为 mixed 时间轴 0 点。
    pub fn new_with_timeline_origin(
        inner: Box<dyn AudioCapture>,
        source: Source,
        policy: TapPolicy,
        health: Arc<SourceHealth>,
        notify: TapNotify,
        timeline_origin: Arc<OnceLock<Instant>>,
    ) -> Self {
        Self {
            inner,
            source,
            policy,
            health,
            notify: Some(notify),
            tap: None,
            tap_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            timeline_origin,
            drift: None,
        }
    }

    /// builder:接入该源的时钟漂移监视器(可选)。不改变既有构造函数签名/调用点。
    pub fn with_drift(mut self, m: Arc<DriftMonitor>) -> Self {
        self.drift = Some(m);
        self
    }
}

impl AudioCapture for TappedCapture {
    fn start(&mut self, sink: Sender<AudioFrame>) -> anyhow::Result<()> {
        let (cap_tx, cap_rx) = crossbeam_channel::bounded::<AudioFrame>(256);
        let health = self.health.clone();
        let policy = self.policy;
        let source = self.source;
        let notify = self.notify.take().unwrap_or_else(TapNotify::none);
        let timeline_origin = self.timeline_origin.clone();
        let drift = self.drift.clone();
        let cancel = self.tap_cancel.clone();
        self.tap = Some(std::thread::spawn(move || {
            run_frame_tap_cancellable(
                source,
                cap_rx,
                sink,
                health,
                policy,
                notify,
                timeline_origin,
                drift,
                cancel,
            )
        }));
        self.inner.start(cap_tx)
    }

    fn stop(&mut self) {
        self.inner.stop();
        // 回调丢样在停流后才是终值(采集线程此时已收尾),取回来入账,
        // 它才进得了 audio.json 的对账块。
        self.health
            .cap_dropped_samples
            .store(self.inner.dropped_samples(), Ordering::Relaxed);
        if let Some(t) = self.tap.take() {
            // 有界收尾(issue #182,2026-08-27 实测事故 + codex P1 收口):采集线程
            // 被设备卡死(蓝牙麦断流风暴)时不会丢它的发送端,tap 的 recv 永不因
            // 通道关闭返回——旧的无界 join 把 lifecycle actor 连同整个应用冻死
            // (用户正点「确认删除 322 段」,只能强杀)。
            // 光放弃 join 还不够(codex):tap 手里攥着下游发送端,segment worker
            // 的 frame_rx.iter() 照样收不了尾,冻结只是挪后一站。所以先升取消旗
            // ——tap 的 recv_timeout 每个 tick 都会醒,见旗即退,**自然 drop 下游
            // 发送端**,worker 随之解锁;然后再有界等它退场。超时(旗都救不了,
            // 说明 tap 卡在 send 上等一个满且无人消费的通道,理论罕见)才放弃,
            // 泄漏换可用;音频/转写本就增量落盘,不因放弃少一个字节。
            // 两阶段(codex 二轮 P1):取消旗只留给卡死路径。健康收尾靠上游断开
            // ——tap 会先排干队列里最多 256 帧再退,立即升旗会把这些帧无声扔掉。
            // 阶段一:无旗有界等(健康路径在此返回,语义与旧无界 join 完全一致);
            // 阶段二:超时说明采集线程攥着发送端没死透,升旗让 tap 弃排干自退
            // (此时上游本就不再产真实帧,弃掉的至多是设备卡死前的残留);
            // 仍超时(tap 卡在满通道 send 等罕见形态)才放弃泄漏。
            let (done_tx, done_rx) = crossbeam_channel::bounded::<()>(1);
            let source = self.source;
            std::thread::spawn(move || {
                let _ = t.join();
                let _ = done_tx.send(());
            });
            if done_rx.recv_timeout(TAP_JOIN_TIMEOUT).is_err() {
                // 第一超时就是事故本体(codex 三轮):采集后端攥着发送端没退,
                // 它那条线程已经泄漏——取消旗随后多半能让 tap 体面退场,但
                // 遥测/告警必须记在这里,否则最典型的卡死(本 issue 的蓝牙麦
                // 现场)反而无声无息。
                let hint = match source {
                    crate::audio::Source::Mic => "建议尽快重启应用并改用内置麦克风",
                    _ => "建议尽快重启应用",
                };
                eprintln!(
                    "[采集] {} 采集线程收尾超时(疑似卡死在设备调用,线程泄漏),已升取消旗让 tap 退场;{hint}",
                    source.as_str()
                );
                crate::telemetry::report_error(
                    crate::telemetry::ErrorKind::CaptureTeardown,
                    &format!("{} 采集线程收尾超时(后端线程泄漏),升旗自愈", source.as_str()),
                );
                self.tap_cancel.store(true, std::sync::atomic::Ordering::Release);
                if done_rx.recv_timeout(TAP_JOIN_TIMEOUT).is_err() {
                    eprintln!(
                        "[采集] {} tap 线程也未退出(取消旗未生效,罕见:疑似卡在满通道 send),放弃等待继续收尾",
                        source.as_str()
                    );
                    crate::telemetry::report_error(
                        crate::telemetry::ErrorKind::CaptureTeardown,
                        &format!("{} tap 线程取消旗后仍未退出,已放弃(双线程泄漏)", source.as_str()),
                    );
                }
            }
        }
    }

    /// 透传内层的丢样计数:tap 自己不丢样,套几层包装都该看到同一个数。
    fn dropped_samples(&self) -> u64 {
        self.inner.dropped_samples()
    }
}

/// 运行转发级:阻塞直到上游关闭(采集停止)或下游关闭(会话拆除)。
/// 上游关闭时不再填充(录制正在收尾,时间轴由 flush 定稿),直接退出并
/// 丢弃下游发送端 → worker 进入尾段 flush,与无 tap 时的关闭链完全一致。
pub fn run_frame_tap(
    _source: Source,
    from_capture: Receiver<AudioFrame>,
    to_worker: Sender<AudioFrame>,
    health: Arc<SourceHealth>,
    policy: TapPolicy,
    notify: TapNotify,
) {
    run_frame_tap_with_drift(
        _source,
        from_capture,
        to_worker,
        health,
        policy,
        notify,
        Arc::new(OnceLock::new()),
        None,
    )
}

/// 可取消版本(issue #182):cancel 升起后在下一个 tick 退出,自然 drop to_worker,
/// 下游 segment worker 才收得了尾。除取消检查外与 run_frame_tap_with_drift 同义。
#[allow(clippy::too_many_arguments)]
fn run_frame_tap_cancellable(
    source: Source,
    from_capture: Receiver<AudioFrame>,
    to_worker: Sender<AudioFrame>,
    health: Arc<SourceHealth>,
    policy: TapPolicy,
    notify: TapNotify,
    timeline_origin: Arc<OnceLock<Instant>>,
    drift: Option<Arc<DriftMonitor>>,
    cancel: Arc<std::sync::atomic::AtomicBool>,
) {
    run_frame_tap_inner(
        source,
        from_capture,
        to_worker,
        health,
        policy,
        notify,
        timeline_origin,
        drift,
        Some(cancel),
    )
}

fn run_frame_tap_with_drift(
    _source: Source,
    from_capture: Receiver<AudioFrame>,
    to_worker: Sender<AudioFrame>,
    health: Arc<SourceHealth>,
    policy: TapPolicy,
    notify: TapNotify,
    timeline_origin: Arc<OnceLock<Instant>>,
    drift: Option<Arc<DriftMonitor>>,
) {
    run_frame_tap_inner(
        _source,
        from_capture,
        to_worker,
        health,
        policy,
        notify,
        timeline_origin,
        drift,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_frame_tap_inner(
    _source: Source,
    from_capture: Receiver<AudioFrame>,
    to_worker: Sender<AudioFrame>,
    health: Arc<SourceHealth>,
    policy: TapPolicy,
    notify: TapNotify,
    timeline_origin: Arc<OnceLock<Instant>>,
    drift: Option<Arc<DriftMonitor>>,
    cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
) {
    // 最近一次真实帧的格式:没收到过帧就不填充(源可能根本没起来,
    // 填零会凭空造出一条空白轨)。
    let mut last_format: Option<(u32, u16)> = None;
    let mut last_frame_at = Instant::now();
    // 时钟核对:源声明的采样率只是它「自称」的,必须拿实际出样速率复核。
    //
    // 断流填充只对付「不出帧」,对付不了「一直出帧但每秒样本数不够」——后者永远不会
    // 让 recv_timeout 超时,填充分支根本不执行,该轨时间轴就对墙钟线性漂移下去。
    // 2026-08-04 实锤:一场 30 分钟的笔记,mic 在进程中止重启后声明 48kHz、实测约
    // 44kHz,轨长比墙钟短 148s;两轨都按 offset 0 铺进混音,同一句话在两轨相隔从 0
    // 一路拉到两分半各响一次,听感就是"每句话说两遍、间隔越来越大"。软件 AEC 也在
    // 同一时刻脱锁(AEC3 的延迟估计只容忍 <100ppm,这里是 ~90000ppm)。
    //
    // 改写下游帧的 sample_rate 而不是补零:少的样本不是"没出声",是被按错误的率折算
    // 掉了。把率改对,重采样器(segment_worker 见率变即重建)自然把时长与音高一并复位;
    // 补零只会在正常语音里插静音,把内容也弄坏。
    let mut applied_rate: Option<u32> = None;
    let mut clock_span = Duration::ZERO;
    let mut clock_samples: u64 = 0;
    // 测率时基优先用帧自带的硬件时间戳(cpal capture/SCK PTS/VPIO host time):
    // 到达间隔测出的是「采集+排队+tap 调度+下游阻塞」后的交付率,消费端一被抢占
    // 就产出假低率,改写采样率把整场音频变速(2026-08-14 实锤:内置麦被反复改写到
    // 42-46kHz,时间轴拉伸 1.14×,而设备 ActualSampleRate 全程 48k±3ppm)。硬件时基
    // 只随捕获推进,交付抖动天然不进测量;无硬件时戳的源(Windows loopback/合成帧)
    // 退回到达间隔并沿用旧语义。hw_pending:样本数与「前一帧时戳→本帧时戳」的区间
    // 配对(本帧样本对应的区间要等下一帧时戳才闭合)。
    let mut clock_hw_prev: Option<u64> = None;
    let mut clock_hw_pending: u64 = 0;
    // 当前统计窗用的是硬件时基还是到达间隔。hw 时戳会偶发缺失(cpal 的
    // duration_since 返回 None、SCK 的 CMTime 无效、VPIO 的 HostTime 标志位
    // 未置),两种口径的时间绝不能累进同一个 clock_span——混合后足以把观测率
    // 推过阈值,重新触发本模块正要根除的错误改率。切换即重开窗。
    let mut clock_hw_mode = false;
    // 相位债务(秒,带符号):检测期间按错的率发出去的时长与墙钟的差。
    // 正 = 发多了(实际率高于声明),负 = 发少了。改率只止住继续发散,这笔账要单独还。
    //
    // 为什么用"微调采样率慢慢还"而不是补零/丢样本:补零只能还负债(反方向根本补不了,
    // 会留下一个横贯全场的固定错位——快窗下 0.44s,恰好在门控 400ms 回看窗之外、又够
    // 不上回放对齐 2s 的门限,于是整场重影);丢样本要扔掉真实语音。把债摊到
    // PHASE_TRIM_SECS 上只需拧动零点几个百分点的采样率,两个方向都能还,内容一个不丢,
    // 音高变化远在可闻阈值之下。
    let mut phase_debt: f64 = 0.0;
    // 还款期内恒定的目标率(见 phase_trim_rate:下游按率变化重建重采样器,必须少变)。
    let mut phase_rate: Option<u32> = None;
    let mut settle_debt = false;
    // 时间轴锚点(首个真实帧到达时刻)与"已向下游转发的音频总时长"(真实帧 + 补零)。
    //
    // 为什么记总量而不是「补到某个时刻」:断流期补零、设备恢复后又把断流期缓冲的
    // 音频整批吐出来(蓝牙 mic 常态),同一段墙钟时间就被计了两遍,该轨时间轴凭空
    // 变长。2026-08-04 实锤:一场墙钟 1998s 的笔记,mic 轨长 2712s(system 1964s),
    // 多出的 ~683s 全是零串;后果是显示时长/播放进度/双轨时间戳全偏,回放里 mic
    // 与 system 的同一句话被拉开成两处。
    //
    // 改成对齐总量后,补零量恒为 max(0, 墙钟 - 已转发),转发总时长的上界就是
    // 「墙钟 + 一次突发的量」:突发把总量顶到墙钟之前不再补零,自动把超发吃回去,
    // 误差不再累积。
    let mut anchor: Option<Instant> = None;
    let mut forwarded = Duration::ZERO;
    // 采集时钟原点:首个带硬件时戳的帧,连同**那一刻已转发的时长**一起记。
    // 只记时戳不记基线的话,原点之前那些无时戳帧的时长白白算进 forwarded,
    // 欠账被永久低估、后面的洞就补不满(Codex P2)。
    let mut hw_origin: Option<(u64, Duration)> = None;
    // 本次断流是否已计 gap / 已报 stall(去抖:恢复前不重复)。
    let mut gap_counted = false;
    let mut stalled = false;
    // 本次断流是否已实际发过补零帧(Task 5):true 时下一个真实帧到达即视为
    // "补零段结束",据此喂 DriftMonitor 一次 reanchor(soft:保频率,只清相位)。
    let mut filled_gap = false;
    // drift 旁挂自持的格式记忆,与下方 `last_format`(时钟核对用)独立、不共用:
    // 判定"设备是否换了"只服务于要不要喂一次 device_switch reanchor,不该跟时钟
    // 核对的重置时机耦合。见下方 Ok 分支顶部旁挂块。
    let mut drift_last_format: Option<(u32, u16)> = None;
    // Codex review Fix 1(P2):`drift_last_format` 只看 (sample_rate, channels),
    // 同格式换设备(如 48k mono → 48k mono 换了台新麦克风,常见)完全漏检,新设备会
    // 继承旧晶振的 ppm 估计与收敛状态。`health.restarts` 由 ResilientCapture 在采集
    // 重启时递增,是与格式无关的"真的换了后端实例"信号,独立判定并优先于
    // device_switch/gap_end。初值取进入循环时的 restarts 值(此前的重启与本次 tap
    // 生命周期无关,不该在第一帧就误判)。
    let mut drift_last_restarts: u32 = health.restarts.load(Ordering::Relaxed);
    // 断流风暴:每个 tick 采一次「累计洞时长 vs 墙钟」,越过阈值在录制中就叫。
    let mut gap_storm =
        GapStormDetector::new(policy.gap_storm_window, policy.gap_storm_threshold);

    loop {
        // 每轮循环(收到帧或 tick 超时)都采一个点:分母是墙钟,采样密度只影响
        // 判定的时间分辨率,不影响比例本身。
        match gap_storm.observe(Instant::now(), health.hw_gap_ms.load(Ordering::Relaxed)) {
            Some(GapStormEvent::Storm(ratio)) => {
                eprintln!(
                    "[采集] {} 断流风暴:近 {}s 内有 {:.1}% 的时长没有音频帧,已按墙钟补零,\
                     内容是真的少了那么多。{}",
                    _source.as_str(),
                    policy.gap_storm_window.as_secs(),
                    ratio * 100.0,
                    // 建议按源给(Codex 二轮 P2):system 轨的风暴是系统采集出事,
                    // 让人去换麦克风是把排障引向错误方向。
                    match _source {
                        Source::Mic => "建议改用内置麦克风或有线连接",
                        Source::System => "系统声音采集侧异常,与麦克风无关",
                    }
                );
                if let Some(cb) = &notify.on_gap_storm {
                    cb(Some(ratio));
                }
            }
            Some(GapStormEvent::Recovered) => {
                eprintln!("[采集] 断流风暴已平息");
                if let Some(cb) = &notify.on_gap_storm {
                    cb(None);
                }
            }
            None => {}
        }
        // 取消旗(issue #182):正常收尾靠上游关通道;采集线程卡死不放发送端时,
        // 这面旗是唯一退出通路——退出即 drop to_worker,下游 worker 解锁。
        if cancel.as_ref().is_some_and(|c| c.load(std::sync::atomic::Ordering::Acquire)) {
            return;
        }
        match from_capture.recv_timeout(policy.tick) {
            Ok(mut frame) => {
                // 本帧之前按硬件时戳判定的采集洞,转发真实帧前先补齐。两类丢失都走这里:
                // HAL 侧丢的,和我们自己回调 try_send 丢的——后者同样会在下一帧的时戳上
                // 如实体现(采集回调不自己补零,理由见 microphone.rs 那段注释)。
                let mut pending_hole = Duration::ZERO;
                // Task 5:真实帧原始声明率下喂 DriftMonitor(必须在下方时钟核对
                // 改写 sample_rate 之前——DLL 要的是设备原始声明率口径的样本计数
                // 与真实时间戳)。
                //
                // 设备换率(拔插耳机/切设备)与补零段结束在此合并判定,不许同一帧
                // 计两次:拔耳机=断流+换率,物理上是一次事件,若各自旁挂各记一次
                // (device_switch + gap_end)会一来一回把正常会话的 reanchors 计数
                // 撞出 build_report 的 >3 异常阈值(2026-08-12 终审发现)。device_switch
                // 语义覆盖 gap_end(full 重锚已经把相位一并清了,soft 重锚是它的子集,
                // 不必再叠加);只有格式没变、纯粹补零结束时才记 gap_end。
                if let Some(m) = &drift {
                    // Codex review Fix 1(P2):采集重启(可能已换物理设备)优先级最高,
                    // 同帧只发最高一个——命中时下方 switched/filled_gap 判定整体跳过,
                    // 不叠加 device_switch/gap_end(全清已经覆盖它们的语义)。
                    let restarts_now = health.restarts.load(Ordering::Relaxed);
                    let restarted = restarts_now != drift_last_restarts;
                    if restarted {
                        // 采集重启可能已换物理设备,晶振不再是同一颗:与 device_switch
                        // 同等保守处理,连同相位一并全清(full=true)。
                        m.mark_reanchor("capture_restart", true);
                    } else {
                        let switched = drift_last_format
                            .is_some_and(|f| f != (frame.sample_rate, frame.channels));
                        if switched {
                            // 设备真的换了,晶振不再是同一颗:DLL 里攒的频率状态对新设备
                            // 没有意义,连同相位一并全清(full=true)。
                            m.mark_reanchor("device_switch", true);
                        } else if filled_gap {
                            // 必须先 mark_reanchor 后 feed:补零段必然 ≥ fill_after(500ms+),
                            // 远超 DLL 内部自动重锚阈值(5ms)。若先 feed,恢复帧的 push 会先
                            // 触发一次 DLL 内部自动重锚(reanchors 内部 +1)并把巨大相位误差
                            // 写进 last_e(phase_err_us 假尖峰),随后 mark_reanchor 又计一次
                            // ——每个补零段被计 2 次,两次正常断流就把 build_report 的
                            // reanchors>3 阈值撞出误报。reanchor(keep_freq=true)会先置
                            // started_at=None,随后 feed 的 push 就会走"首点纯锚定"早退分支,
                            // 不触发内部自动重锚、不污染 last_e,每个 gap 恰好计 1 次(代价:
                            // 事件 t_s 取的是断流前最后一次 feed 的时刻而非恢复帧时刻,可接受)。
                            m.mark_reanchor("gap_end", false);
                        }
                    }
                    drift_last_format = Some((frame.sample_rate, frame.channels));
                    drift_last_restarts = restarts_now;
                    m.feed(&frame);
                }
                filled_gap = false;
                if stalled {
                    if let Some(cb) = &notify.on_recover {
                        cb();
                    }
                }
                stalled = false;
                gap_counted = false;
                let now = Instant::now();
                let gap = now.duration_since(last_frame_at);
                let first = anchor.is_none();
                if first {
                    health.record_first_frame(now, &timeline_origin);
                }
                // 设备中途换率(拔插耳机/切设备)是正当的断点:声明变了就丢弃旧的实测
                // 结论,按新声明值从头核对,不把上一只设备的速率套到下一只头上。
                if !first && last_format != Some((frame.sample_rate, frame.channels)) {
                    applied_rate = None;
                    clock_span = Duration::ZERO;
                    clock_samples = 0;
                    clock_hw_prev = None;
                    clock_hw_pending = 0;
                    // device_switch 的 drift reanchor 已在本函数顶部的旁挂块里、按
                    // drift_last_format 独立判定并处理(与 gap_end 合并去重,见该处
                    // 注释),这里不再重复喂——两处各自摸一次同一物理事件曾是 P1 双计
                    // 缺陷的根(2026-08-12 终审)。
                }
                last_format = Some((frame.sample_rate, frame.channels));
                last_frame_at = now;
                anchor.get_or_insert(now);
                // 交错多声道:每声道样本数才是时长。rate 为 0 的畸形帧按不计时长处理
                // (下游重采样器同样处理不了,这里只保证不 panic/不污染时间轴)。
                let per_channel = frame.samples.len() / frame.channels.max(1) as usize;
                if frame.sample_rate > 0 {
                    // 只统计「连续出帧」的时间:断流本身以及紧随其后的一次缓冲回吐
                    // (蓝牙 mic 常态)会让瞬时速率忽低忽高,计进去就是把噪声当信号。
                    // 时基选取见 clock_hw_prev 处注释:有硬件时戳按捕获时钟,无则到达间隔。
                    match frame.host_time_ns {
                        Some(ns) => {
                            if !clock_hw_mode {
                                clock_span = Duration::ZERO;
                                clock_samples = 0;
                                clock_hw_mode = true;
                            }
                            if let Some(prev) = clock_hw_prev {
                                let delta_ns = ns.saturating_sub(prev);
                                let delta = Duration::from_nanos(delta_ns);
                                // 区间有效性三查。hw 时戳取自采样时钟,正常情况下
                                // `delta == 上一帧样本数 / 真实率` 精确成立,所以本区间
                                // 的隐含速率就是真实速率——它落到合理性夹板之外,只能是
                                // 这段里丢了样本(洞)或吐了突发缓冲,不是"时钟慢"。
                                // 只查 `delta < fill_after` 远远不够:一个 20ms 的洞摊进
                                // 200ms 窗口就是 -10%,越过容差却仍在夹板内,照样会被
                                // 错判成慢钟改写采样率(Codex review P1)。
                                let implied = if delta_ns > 0 {
                                    clock_hw_pending as f64 * 1e9 / delta_ns as f64
                                } else {
                                    0.0
                                };
                                let declared = frame.sample_rate as f64;
                                // 洞的判据不能挂在 RATE_SANITY 上(Codex P1):丢掉**一个**
                                // 等长回调时 delta 恰好翻倍、implied 正好等于 declared*0.5,
                                // 卡在夹板下沿之内——最常见的单帧丢失既不会被认成洞,还会
                                // 把"一半速率"喂进对账窗口。改为直接看时戳差与所携样本时长
                                // 的缺口:超过 HW_HOLE_MIN 就是洞。
                                // 期望时长按**当前生效**的率算(已改写过就用改写值):
                                // 时戳量的是真实时间,只有拿真实率折样本数才对得上。
                                let effective =
                                    applied_rate.unwrap_or(frame.sample_rate).max(1) as f64;
                                let carried =
                                    Duration::from_secs_f64(clock_hw_pending as f64 / effective);
                                let hole = delta.saturating_sub(carried);
                                // 判洞门槛是"至少缺一整帧"。低于此的持续小缺口是**慢时钟**
                                // (声明 48k 实跑 32k:每帧都缺半帧),那归对账改率处理,
                                // 误判成洞会每帧清统计,改率永远收敛不了(实测打红两条老
                                // 用例);而丢掉一个等长回调恰好缺满一帧,正好被认出来。
                                // 容差(Codex 十轮 P1):设备周期不是整纳秒时,丢一个回调
                                // 算出来的 hole 可能比 carried 差个位数纳秒(512 帧 @48k:
                                // carried 10_666_667ns,两周期时戳差 21_333_333ns),严格
                                // 比较会漏掉这类最常见的单缓冲丢失。1ms 远小于慢时钟场景
                                // 的判别间距(48k 声明/44.1k 实跑每帧才差 0.88ms),不会
                                // 把慢时钟误判成洞。
                                let holey =
                                    ns > prev && hole + HW_ROUND_TOL >= carried.max(HW_HOLE_MIN);
                                let usable = ns > prev
                                    && !holey
                                    && delta < policy.fill_after
                                    && implied >= declared * RATE_SANITY.0
                                    && implied <= declared * RATE_SANITY.1;
                                if usable {
                                    clock_span += delta;
                                    clock_samples += clock_hw_pending;
                                } else {
                                    // 时间过去了、样本没来 = 采集侧真有洞(HAL 丢,或我们的
                                    // 回调 try_send 丢)。只计数不补的话时间轴被直接压缩,
                                    // 转写时间戳与音频越走越偏,内容也悄无声息地少了
                                    // (Codex P0)。突发缓冲回吐(样本比时间多)不是洞,不补。
                                    if holey {
                                        pending_hole = hole;
                                        // 只有这一支是「真丢了样本」。下面那个
                                        // hw_gaps 覆盖所有「区间不可用」,两者
                                        // 语义不同,排障必须分开看(Codex 复核 P1)。
                                        health.hw_holes.fetch_add(1, Ordering::Relaxed);
                                        // 洞有多长在**检测时**就记账,不等补了多少才记
                                        // (Codex 八轮 P2):断流补零可能已经填过其中一段,
                                        // 按残余量记会把 1 秒的洞记成 500ms,这个字段就
                                        // 失去了"到底丢了多久"的意义。
                                        health.hw_gap_ms.fetch_add(
                                            hole.as_millis() as u64,
                                            Ordering::Relaxed,
                                        );
                                    }
                                    clock_span = Duration::ZERO;
                                    clock_samples = 0;
                                    health.hw_gaps.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            hw_origin.get_or_insert((ns, forwarded));
                            clock_hw_prev = Some(ns);
                            clock_hw_pending = per_channel as u64;
                        }
                        None => {
                            if clock_hw_mode {
                                clock_span = Duration::ZERO;
                                clock_samples = 0;
                                clock_hw_mode = false;
                            }
                            clock_hw_prev = None;
                            clock_hw_pending = 0;
                            if !first && gap < policy.fill_after {
                                clock_span += gap;
                                clock_samples += per_channel as u64;
                            } else {
                                clock_span = Duration::ZERO;
                                clock_samples = 0;
                            }
                        }
                    }
                    // 两级判定:快窗只抓粗偏差(伤害最大、5 秒就能认定),满窗抓细偏差。
                    // 快窗未超粗容差时**不清零**,让统计继续攒到满窗再用更稳的估计判一次。
                    let due_full = clock_span >= policy.rate_eval_window;
                    let due_fast = clock_span >= policy.rate_fast_window;
                    if (due_full || due_fast) && clock_samples > 0 {
                        let observed = clock_samples as f64 / clock_span.as_secs_f64();
                        let declared = frame.sample_rate as f64;
                        let effective = applied_rate.unwrap_or(frame.sample_rate) as f64;
                        let tol = if due_full { RATE_TOLERANCE } else { RATE_FAST_TOLERANCE };
                        let over = (observed / effective - 1.0).abs() > tol;
                        if !over && !due_full {
                            // 快窗内没看出粗问题:攒着,等满窗。
                        } else if observed >= declared * RATE_SANITY.0
                            && observed <= declared * RATE_SANITY.1
                        {
                            if over {
                                eprintln!(
                                    "采集时钟核对: 声明 {declared:.0}Hz 实测 {observed:.0}Hz\
                                     (差 {:+.1}%),按实测改写下游采样率",
                                    (observed / declared - 1.0) * 100.0
                                );
                                applied_rate = Some(observed.round() as u32);
                                health.rate_fixes.fetch_add(1, Ordering::Relaxed);
                                settle_debt = true;
                                // rate_fix 恰恰是对账**证实**了 DLL 正在测的偏差(同一颗
                                // 晶振、同一声明率下频率状态仍然有效),不是"判断失灵要
                                // 推倒重来"——清零频率状态反而会把最该出数的设备的 ppm
                                // 抹掉。只清相位:声明率被改写会给下游引入一次相位跳变,
                                // 这笔相位差不能沿用旧锚点。
                                if let Some(m) = &drift {
                                    m.mark_reanchor("rate_fix", false);
                                }
                            }
                        } else {
                            eprintln!(
                                "采集时钟核对: 实测 {observed:.0}Hz 与声明 {declared:.0}Hz \
                                 相差过大(非采样率问题),不改写"
                            );
                        }
                        // 只有真判过一轮才重开统计;快窗放过的那次要接着攒到满窗,
                        // 否则统计永远停在快窗长度,细偏差再也攒不到满窗、判不出来。
                        if over || due_full {
                            clock_span = Duration::ZERO;
                            clock_samples = 0;
                        }
                    }
                }
                // 改写后再记时长:forwarded 要反映下游将如何解释这批样本。
                if let Some(rate) = applied_rate {
                    frame.sample_rate = rate;
                }
                // 刚判出新速率:把检测期间攒下的相位差记成债务(带符号,两个方向都记)。
                if settle_debt {
                    settle_debt = false;
                    if let Some(anchor) = anchor {
                        phase_debt =
                            forwarded.as_secs_f64() - anchor.elapsed().as_secs_f64();
                        if phase_debt.abs() > PHASE_TRIM_EPS {
                            eprintln!(
                                "采集时钟核对: 检测期间累计相位差 {:+.2}s,按 {:.0}% 上限\
                                 微调采样率摊还",
                                phase_debt,
                                policy.phase_trim_max * 100.0
                            );
                        }
                    }
                }
                // 补洞必须发生在**给本帧记账之前**(Codex P1):零帧排在真实帧前面,
                // 若先把本帧的时长记进 forwarded,欠账就少算一帧,每次修补都短一截、
                // 真实帧也被放早了。
                //
                // 上限用**采集时钟**的欠账而不是墙钟(Codex P1):墙钟包含这一帧在队列里
                // 排的时间,背压越重越虚高,会把 Timeout 分支已经补过的那段再补一遍。
                // 采集时钟(首帧时戳到本帧时戳)减去已转发时长,才正好是"还缺多少音频"。
                if pending_hole > Duration::ZERO && frame.sample_rate > 0 {
                    let captured = match (hw_origin, frame.host_time_ns) {
                        (Some((o, base)), Some(ns)) if ns > o => base + Duration::from_nanos(ns - o),
                        // 没有时戳原点就退回墙钟(与断流补零同口径,只会更保守)。
                        _ => anchor.map(|a| a.elapsed()).unwrap_or(Duration::ZERO),
                    };
                    let fill = pending_hole.min(captured.saturating_sub(forwarded));
                    let mut left = fill;
                    let ch = frame.channels.max(1) as usize;
                    while left > Duration::ZERO {
                        let step = left.min(FILL_CHUNK);
                        let n = (step.as_secs_f64() * frame.sample_rate as f64) as usize;
                        if n == 0 {
                            break;
                        }
                        forwarded += Duration::from_secs_f64(n as f64 / frame.sample_rate as f64);
                        let ms = (n as u64 * 1000) / frame.sample_rate as u64;
                        health.silence_ms.fetch_add(ms, Ordering::Relaxed);
                        let silence = AudioFrame {
                            samples: vec![0.0; n * ch],
                            sample_rate: frame.sample_rate,
                            channels: frame.channels,
                            host_time_ns: None, // 合成帧,不参与时钟核对
                            // 这就是那个「我们自己造的零」——下游据此能把它
                            // 和设备送来的数字静音分开。
                            synthetic: true,
                        };
                        if to_worker.send(silence).is_err() {
                            return;
                        }
                        left = left.saturating_sub(step);
                    }
                    // 补零改了 forwarded,相位债务要按当前真实残差重算,否则后续帧继续
                    // 按旧债拧速率,把刚补的这段又慢慢抹掉。基准必须仍是**采集时钟**
                    // (Codex P1):这帧可能在队列里排过很久,拿墙钟算会凭空造出一笔等大
                    // 的负债,后续帧被拖慢,时间轴又被拉长一次。
                    phase_debt = forwarded.as_secs_f64() - captured.as_secs_f64();
                    phase_rate = None;
                }
                // 还款:把采样率往债务的反方向拧一点点,下游据此多算/少算一点时长,
                // forwarded 就平滑地追上墙钟。债清了立刻停手,恢复实测速率。
                if frame.sample_rate > 0 && phase_debt.abs() > PHASE_TRIM_EPS {
                    let base = frame.sample_rate;
                    let held = *phase_rate.get_or_insert_with(|| {
                        phase_trim_rate(
                            phase_debt,
                            policy.phase_trim_window.as_secs_f64(),
                            base,
                            policy.phase_trim_max,
                        )
                    });
                    let (rate, repaid) = phase_trim_step(phase_debt, held, base, per_channel);
                    phase_debt -= repaid;
                    frame.sample_rate = rate;
                    if phase_debt.abs() <= PHASE_TRIM_EPS {
                        phase_rate = None; // 还清,下一帧恢复实测率
                    }
                } else {
                    phase_rate = None;
                }
                if frame.sample_rate > 0 {
                    forwarded += Duration::from_secs_f64(
                        per_channel as f64 / frame.sample_rate as f64,
                    );
                }
                health.frames.fetch_add(1, Ordering::Relaxed);
                health.samples.fetch_add(frame.samples.len() as u64, Ordering::Relaxed);
                // 背压计量:收帧后的残余队列深度 + 本次转发被下游顶住的时长。
                let depth = from_capture.len() as u32;
                if depth > 0 {
                    health.cap_queue_hw.fetch_max(depth, Ordering::Relaxed);
                }
                let send_t = Instant::now();
                if to_worker.send(frame).is_err() {
                    return; // 会话拆除,下游已关
                }
                let waited = send_t.elapsed().as_millis() as u64;
                if waited > 0 {
                    health.send_wait_ms.fetch_add(waited, Ordering::Relaxed);
                    health.send_wait_max_ms.fetch_max(waited, Ordering::Relaxed);
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                let Some((declared, channels)) = last_format else {
                    continue;
                };
                // 补零也走改写后的率:下游按同一个率解释真实帧与零帧,时间轴才自洽。
                let rate = applied_rate.unwrap_or(declared);
                let drought = last_frame_at.elapsed();
                if drought < policy.fill_after {
                    continue;
                }
                if !gap_counted {
                    gap_counted = true;
                    health.gaps.fetch_add(1, Ordering::Relaxed);
                }
                if !stalled {
                    if let Some(stall_after) = policy.stall_after {
                        if drought >= stall_after {
                            stalled = true;
                            if let Some(cb) = &notify.on_stall {
                                cb();
                            }
                        }
                    }
                }
                // 差量补零:补到「已转发总时长 == 墙钟」为止,交错声道等比放大。
                // 已转发量领先墙钟(设备刚吐过突发缓冲)时 deficit 为 0——本轮不补,
                // 让超发自然被墙钟追平,不再累积。
                let Some(anchor) = anchor else { continue };
                let deficit = anchor.elapsed().saturating_sub(forwarded);
                let frames_n = (deficit.as_secs_f64() * rate as f64) as usize;
                if frames_n == 0 {
                    continue;
                }
                forwarded += Duration::from_secs_f64(frames_n as f64 / rate as f64);
                // 断流补零同样在改 forwarded,若不把债务重算成"当前真实残差",还款
                // 会把已经被补零抹平的那部分再还一遍,反向拧过头。
                phase_debt = forwarded.as_secs_f64() - anchor.elapsed().as_secs_f64();
                health
                    .silence_ms
                    .fetch_add((frames_n as u64 * 1000) / rate as u64, Ordering::Relaxed);
                let silence = AudioFrame {
                    samples: vec![0.0; frames_n * channels as usize],
                    sample_rate: rate,
                    channels,
                    // 合成补零帧，无真实硬件时间戳。
                    host_time_ns: None,
                    // 帧荒期补的零(另一处在 pending_hole 补洞分支):同样是我们
                    // 自己造的,下游要能认出来。
                    synthetic: true,
                };
                if to_worker.send(silence).is_err() {
                    return;
                }
                filled_gap = true;
            }
            // 采集已断开:直接退出。
            //
            // 这里刻意**不**做"按欠账补最后一段":队列一直满到停录时,最后那截被回调
            // 丢掉的音频确实没有下一帧来暴露,时戳补洞覆盖不到它。但补它需要一个可信的
            // 终点——Disconnected 是在队列排空、所有阻塞 send 都完成之后才观察到的,
            // 拿那一刻的墙钟算欠账会把拆除耗时算成缺口,凭空补出静音;而且同步补零会
            // 拖住 TappedCapture::stop(),另一路采集在此期间还在录,两轨又错开
            // (Codex 四轮两条 P1)。要做对得先把采集停止改成两阶段(先给所有源发停,
            // 再各自收尾),那是独立改动。眼下这段残缺是**可见**的:回调丢样有日志、
            // hw_gaps 有计数、drift_ms 记着残差。
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32 as StdAtomicU32;

    #[test]
    fn shared_origin_records_relative_first_frame_offsets() {
        let origin = OnceLock::new();
        let mic = SourceHealth::default();
        let system = SourceHealth::default();
        let first = Instant::now();

        mic.record_first_frame(first, &origin);
        system.record_first_frame(first + Duration::from_millis(10), &origin);

        assert_eq!(mic.first_frame_offset_16k(), 0);
        assert_eq!(system.first_frame_offset_16k(), 160);
    }

    fn frame(n: usize) -> AudioFrame {
        AudioFrame { samples: vec![0.5; n], sample_rate: 16000, channels: 1, host_time_ns: None, synthetic: false }
    }

    fn fast_policy() -> TapPolicy {
        TapPolicy {
            fill_after: Duration::from_millis(50),
            stall_after: Some(Duration::from_millis(120)),
            tick: Duration::from_millis(10),
            // 默认给足够大的评估窗:除时钟核对本身的用例外,其它用例都不该被它波及。
            rate_eval_window: Duration::from_secs(3600),
            rate_fast_window: Duration::from_secs(3600),
            // 还款窗按比例缩短:秒级用例里跑完与生产同一段收敛逻辑。
            phase_trim_window: Duration::from_millis(200),
            phase_trim_max: 0.5,
            // 默认给足够大的窗:除风暴用例本身外,其它用例不该被它波及。
            gap_storm_window: Duration::from_secs(3600),
            gap_storm_threshold: 1.0,
        }
    }

    /// 墙钟喂帧类用例的策略:fill_after 放宽到 500ms。
    ///
    /// 这类用例靠 10ms sleep 驱动,而 fast_policy 的 fill_after 只有 50ms——机器一忙
    /// (全量并行、旁边在跑构建)一次超时的 sleep 就会被判成断流,把时钟统计清零,
    /// 速率判定随之推迟或不发生。表现是"单独跑全过、全量跑偶发红",查起来很费劲。
    /// 它们测的都不是断流,放宽即可。
    fn wallclock_policy() -> TapPolicy {
        TapPolicy { fill_after: Duration::from_millis(500), ..fast_policy() }
    }

    /// 以真实墙钟折算样本数喂帧:实际投喂速率恒为 `true_rate`,不受调度抖动影响
    /// (若按固定样本数定时发,睡眠抖动会直接变成速率误差,断言就没法收紧)。
    fn feed_at_rate(
        ctx: &Sender<AudioFrame>,
        declared: u32,
        true_rate: f64,
        dur: Duration,
    ) -> Duration {
        let t0 = Instant::now();
        let mut sent = 0u64;
        let mut worst_gap = Duration::ZERO;
        let mut last = Instant::now();
        while t0.elapsed() < dur {
            std::thread::sleep(Duration::from_millis(10));
            worst_gap = worst_gap.max(last.elapsed());
            last = Instant::now();
            let want = (t0.elapsed().as_secs_f64() * true_rate) as u64;
            let n = want.saturating_sub(sent) as usize;
            if n == 0 {
                continue;
            }
            if ctx
                .send(AudioFrame {
                    samples: vec![0.2; n],
                    sample_rate: declared,
                    channels: 1,
                    host_time_ns: None,
                    synthetic: false,
                })
                .is_err()
            {
                break;
            }
            sent = want;
        }
        worst_gap
    }

    /// 机器被压满时 10ms 的 sleep 能超时几百毫秒,墙钟驱动的用例就失去计时基准
    /// (超过 fill_after 会被判成断流、清零时钟统计)。这时如实跳过计时断言并说明,
    /// 而不是把阈值放宽到测不出问题——控制器本身的数学另有确定性仿真覆盖。
    fn timing_sound(worst_gap: Duration, policy: &TapPolicy) -> bool {
        if worst_gap >= policy.fill_after {
            eprintln!(
                "跳过计时断言:调度超时 {:?} ≥ fill_after {:?},本次计时基准不可信",
                worst_gap, policy.fill_after
            );
            return false;
        }
        true
    }

    /// 带硬件时间戳直发一批帧(hw 时基推进 step_ns/帧,样本 n/帧),返回转发结果与健康。
    /// 不 sleep:hw 测率路径不依赖到达节奏,这正是它存在的意义。
    fn run_hw_feed(
        policy: TapPolicy,
        declared: u32,
        n_per_frame: usize,
        step_ns: u64,
        frames_n: usize,
        jump_at: Option<(usize, u64)>,
        arrival_sleep: Option<Duration>,
    ) -> (Vec<AudioFrame>, Arc<SourceHealth>) {
        let (ctx, crx) = crossbeam_channel::unbounded();
        let (wtx, wrx) = crossbeam_channel::unbounded();
        let health = Arc::new(SourceHealth::default());
        let h2 = health.clone();
        let t = std::thread::spawn(move || {
            run_frame_tap(Source::Mic, crx, wtx, h2, policy, TapNotify::none())
        });
        let mut ns: u64 = 1_000;
        for i in 0..frames_n {
            if let Some((at, jump)) = jump_at {
                if i == at {
                    ns += jump;
                }
            }
            if let Some(d) = arrival_sleep {
                std::thread::sleep(d);
            }
            ctx.send(AudioFrame {
                samples: vec![0.2; n_per_frame],
                sample_rate: declared,
                channels: 1,
                host_time_ns: Some(ns),
                synthetic: false,
            })
            .unwrap();
            ns += step_ns;
        }
        drop(ctx);
        t.join().unwrap();
        (wrx.try_iter().collect(), health)
    }

    /// 2026-08-14 变速失真事故的根修:旧实现用 tap 线程的到达间隔测率,消费端一卡
    /// (排队/调度/下游阻塞)就测出假低率并改写采样率,把整场音频变速 1.14×。
    /// 帧带硬件时间戳时必须按捕获时钟测率——到达节奏在这里被刻意打乱(全部瞬时
    /// 灌入,到达速率无穷大;或由调度随机拖慢),但 hw 时基显示真率==声明率,
    /// 绝不允许改写。
    #[test]
    fn hw_timestamps_immune_to_delivery_jitter() {
        const DECLARED: u32 = 48_000;
        let policy = TapPolicy {
            rate_eval_window: Duration::from_millis(200),
            phase_trim_max: 0.0,
            ..fast_policy()
        };
        // 480 样本/帧 @ 每帧 hw 推进 10ms = 真率恰为 48k;但到达节奏被拖慢到每帧
        // ~11.5ms(等效到达率 ~42k,恰在 RATE_SANITY 夹板内)——正是 2026-08-14 事故
        // 形态:旧的到达间隔测率会在此把 48k 改写成 ~42k 并整场变速。
        let (got, health) = run_hw_feed(
            policy,
            DECLARED,
            480,
            10_000_000,
            60,
            None,
            Some(Duration::from_micros(11_500)),
        );
        assert!(got.len() >= 60, "全部转发: {}", got.len());
        assert!(
            got.iter().all(|f| f.sample_rate == DECLARED),
            "hw 真率==声明率,任何帧都不得改写"
        );
        assert_eq!(health.rate_fixes.load(Ordering::Relaxed), 0, "不得记 rate_fix");
        assert_eq!(health.hw_gaps.load(Ordering::Relaxed), 0, "hw 时基连续,不得记跳变");
    }

    /// 真谎报在 hw 时基下照样抓:hw 跨度 T 内只出 2/3·48k·T 个样本(声明 48k、
    /// 真率 32k),评估窗满后必须改写。确定性版的
    /// `corrects_source_that_lies_about_its_sample_rate`(无 sleep,不受机器负载影响)。
    #[test]
    fn hw_timestamps_still_correct_genuine_rate_lie() {
        const DECLARED: u32 = 48_000;
        let policy = TapPolicy {
            rate_eval_window: Duration::from_millis(200),
            phase_trim_max: 0.0,
            ..fast_policy()
        };
        // 每帧 hw 推进 10ms 却只有 320 样本 → 真率 32k。40 帧 = 400ms hw 跨度。
        let (got, health) = run_hw_feed(policy, DECLARED, 320, 10_000_000, 40, None, None);
        assert_eq!(got[0].sample_rate, DECLARED, "评估窗满之前不得改写");
        let last = got.last().unwrap().sample_rate as f64;
        assert!(
            (last - 32_000.0).abs() / 32_000.0 < 0.01,
            "末帧应为实测率 ~32k,得到 {last}"
        );
        assert!(health.rate_fixes.load(Ordering::Relaxed) >= 1);
    }

    /// hw 时基跳变(≥fill_after,设备断流后缓冲回吐/换设备)要把测率统计清零,
    /// 跳变前后各自不足评估窗时不得拼起来判定。
    #[test]
    fn hw_timestamp_jump_resets_rate_stats() {
        const DECLARED: u32 = 48_000;
        let policy = TapPolicy {
            rate_eval_window: Duration::from_millis(200),
            phase_trim_max: 0.0,
            ..fast_policy()
        };
        // 两段各 15 帧(150ms hw 跨度,不足 200ms 评估窗),中间 hw 跳 1s;
        // 样本数按 32k 谎报——若统计未被跳变清零,拼起来就会满窗并错误改写。
        let (got, health) =
            run_hw_feed(policy, DECLARED, 320, 10_000_000, 30, Some((15, 1_000_000_000)), None);
        assert!(
            got.iter().all(|f| f.sample_rate == DECLARED),
            "两段各自不足评估窗,不得改写"
        );
        assert_eq!(health.rate_fixes.load(Ordering::Relaxed), 0);
        assert_eq!(
            health.hw_gaps.load(Ordering::Relaxed),
            1,
            "hw 时基跳变计 1 次——它是「采集侧真有洞」的直接计数,与消费端慢无关"
        );
    }

    /// 按脚本喂帧:每项 (样本数, 可选 hw 时戳, 发送前等待)。用于精确构造
    /// 「时基切换」「hw 区间含洞」这类只靠 run_hw_feed 表达不出来的形态。
    fn run_scripted(
        policy: TapPolicy,
        declared: u32,
        script: &[(usize, Option<u64>, Duration)],
    ) -> (Vec<AudioFrame>, Arc<SourceHealth>) {
        run_scripted_with_notify(policy, declared, script, TapNotify::none())
    }

    /// 同 run_scripted,但可注入通知回调(断流风暴一类的接线回归用)。
    fn run_scripted_with_notify(
        policy: TapPolicy,
        declared: u32,
        script: &[(usize, Option<u64>, Duration)],
        notify: TapNotify,
    ) -> (Vec<AudioFrame>, Arc<SourceHealth>) {
        let (ctx, crx) = crossbeam_channel::unbounded();
        let (wtx, wrx) = crossbeam_channel::unbounded();
        let health = Arc::new(SourceHealth::default());
        let h2 = health.clone();
        let t = std::thread::spawn(move || {
            run_frame_tap(Source::Mic, crx, wtx, h2, policy, notify)
        });
        for (n, hw, wait) in script {
            if !wait.is_zero() {
                std::thread::sleep(*wait);
            }
            ctx.send(AudioFrame {
                samples: vec![0.2; *n],
                sample_rate: declared,
                channels: 1,
                host_time_ns: *hw,
                synthetic: false,
            })
            .unwrap();
        }
        drop(ctx);
        t.join().unwrap();
        (wrx.try_iter().collect(), health)
    }

    /// Codex review P1:hw 时戳偶发缺失(cpal duration_since 返回 None、SCK CMTime
    /// 无效、VPIO HostTime 标志位未置)时,若把到达间隔累加进同一个 clock_span,
    /// 两种时基就被混在一起,足以把观测率推过阈值、重新触发本次修复要根除的错误改率。
    /// 时基切换必须重开统计窗。
    #[test]
    fn timebase_switch_resets_rate_stats_instead_of_mixing() {
        const DECLARED: u32 = 48_000;
        let policy = TapPolicy {
            rate_eval_window: Duration::from_millis(200),
            fill_after: Duration::from_millis(500),
            phase_trim_max: 0.0,
            ..fast_policy()
        };
        let mut script: Vec<(usize, Option<u64>, Duration)> = Vec::new();
        // hw 段:15 帧 × 10ms = 140ms 真实跨度(不足 200ms 评估窗,自身不判定)
        let mut ns = 1_000u64;
        for _ in 0..15 {
            script.push((480, Some(ns), Duration::ZERO));
            ns += 10_000_000;
        }
        // 无时戳段:5 帧,到达间隔 20ms(共 100ms)。两段若被拼接 = 240ms > 评估窗,
        // 混合口径算出 ~40kHz → 误判改率;正确行为是切换时重开窗,谁都判不了。
        for _ in 0..5 {
            script.push((480, None, Duration::from_millis(20)));
        }
        let (got, health) = run_scripted(policy, DECLARED, &script);
        assert!(
            got.iter().all(|f| f.sample_rate == DECLARED),
            "时基切换不得拼接成一次判定"
        );
        assert_eq!(health.rate_fixes.load(Ordering::Relaxed), 0);
    }

    /// Codex review P1:`delta < fill_after` 证明不了采集连续。设备掉了 200ms
    /// 却只交付一帧样本时,该区间的瞬时速率荒谬(样本数配不上时间跨度),
    /// 累加进窗口就会压低观测率、触发错误改率——正是本次事故的形态。
    /// 含洞区间必须整段作废并计一次 hw_gaps。
    #[test]
    fn hw_interval_with_capture_hole_is_rejected_not_averaged() {
        const DECLARED: u32 = 48_000;
        let policy = TapPolicy {
            rate_eval_window: Duration::from_millis(200),
            fill_after: Duration::from_millis(500),
            phase_trim_max: 0.0,
            ..fast_policy()
        };
        let mut script: Vec<(usize, Option<u64>, Duration)> = Vec::new();
        let mut ns = 1_000u64;
        for i in 0..40 {
            script.push((480, Some(ns), Duration::ZERO));
            // 第 5 帧后多跳 20ms:洞不大不小,恰恰最危险——摊进 200ms 窗口是 -10%,
            // 越过容差却仍落在 RATE_SANITY 夹板内,旧实现会照此改写采样率。
            // 但该区间本身隐含速率仅 16kHz(480 样本 / 30ms),配不上时间跨度。
            ns += if i == 5 { 30_000_000 } else { 10_000_000 };
        }
        let (got, health) = run_scripted(policy, DECLARED, &script);
        assert!(
            got.iter().all(|f| f.sample_rate == DECLARED),
            "含洞区间不得被平均进测率,更不得据此改写采样率"
        );
        assert_eq!(health.rate_fixes.load(Ordering::Relaxed), 0);
        assert_eq!(health.hw_gaps.load(Ordering::Relaxed), 1, "洞要记一次 hw_gaps");
        assert_eq!(health.hw_holes.load(Ordering::Relaxed), 1, "真丢样要记一次 hw_holes");
    }

    /// Codex 复核 P1(2026-08-17):`hw_gaps` 在**所有**「区间不可用」分支都 +1——
    /// 时戳回退、突发缓冲回吐、隐含速率越界都算,只有 `holey` 才累加 hw_gap_ms。
    /// 那次排障据此把 514 读成「514 次硬件断流」,而真正可信的量只有 hw_gap_ms。
    /// 拆出 hw_holes:只在判洞时 +1,让「丢了样本」与「这段不能拿来测率」分家。
    ///
    /// 本例造突发回吐:时戳只前进 4ms,却仍携带上一帧 10ms 的样本(样本比时间多)。
    /// 区间必须作废(隐含速率 120kHz,拿去测率会毁掉判定),但一个样本都没丢。
    #[test]
    fn burst_flush_invalidates_interval_without_counting_a_hole() {
        const DECLARED: u32 = 48_000;
        let policy = TapPolicy {
            rate_eval_window: Duration::from_millis(200),
            fill_after: Duration::from_millis(500),
            phase_trim_max: 0.0,
            ..fast_policy()
        };
        let mut script: Vec<(usize, Option<u64>, Duration)> = Vec::new();
        let mut ns = 1_000u64;
        for i in 0..40 {
            script.push((480, Some(ns), Duration::ZERO));
            ns += if i == 5 { 4_000_000 } else { 10_000_000 };
        }
        let (_got, health) = run_scripted(policy, DECLARED, &script);
        assert_eq!(
            health.hw_gaps.load(Ordering::Relaxed),
            1,
            "样本比时间多的区间不能拿来测率,要计一次 hw_gaps"
        );
        assert_eq!(
            health.hw_holes.load(Ordering::Relaxed),
            0,
            "突发回吐一个样本都没丢,不得计成洞"
        );
        assert_eq!(
            health.hw_gap_ms.load(Ordering::Relaxed),
            0,
            "没丢样就没有洞时长"
        );
    }

    /// Codex 复核 P1(2026-08-17):采集回调因下游队列满而 try_send 丢掉的样本,
    /// 此前只在停流时 eprintln,进不了 audio.json。缺了它,排障没法把
    /// 「设备/HAL 没供帧」和「我们自己的回调丢帧」分开——两者都表现为 hw 时戳
    /// 出现缺口,归因却完全相反(前者换设备,后者修下游)。
    #[test]
    fn capture_callback_drops_are_recorded_into_health() {
        struct DroppingCapture(u64);
        impl crate::audio::AudioCapture for DroppingCapture {
            fn start(&mut self, _sink: Sender<AudioFrame>) -> anyhow::Result<()> {
                Ok(())
            }
            fn stop(&mut self) {}
            fn dropped_samples(&self) -> u64 {
                self.0
            }
        }

        let health = Arc::new(SourceHealth::default());
        let mut cap = TappedCapture::new(
            Box::new(DroppingCapture(1234)),
            Source::Mic,
            fast_policy(),
            health.clone(),
            TapNotify::none(),
        );
        let (tx, _rx) = crossbeam_channel::unbounded();
        cap.start(tx).unwrap();
        cap.stop();

        assert_eq!(
            health.cap_dropped_samples.load(Ordering::Relaxed),
            1234,
            "回调丢样必须入账,否则 audio.json 里看不见"
        );
        assert_eq!(
            health.snapshot(Source::Mic).cap_dropped_samples,
            1234,
            "快照要带上它,audio.json 才写得出去"
        );
    }

    /// 健康收尾不丢队列帧(codex 二轮 P1 回归):stop 时上游正常断开,tap 必须
    /// 先排干在途帧再退——取消旗若抢在排干前生效,最多 256 帧会无声消失。
    #[test]
    fn healthy_stop_drains_queued_frames() {
        struct BurstCapture;
        impl crate::audio::AudioCapture for BurstCapture {
            fn start(&mut self, sink: Sender<AudioFrame>) -> anyhow::Result<()> {
                for i in 0..40 {
                    let _ = sink.send(AudioFrame {
                        samples: vec![i as f32; 160],
                        sample_rate: 16000,
                        channels: 1,
                        host_time_ns: None,
                        synthetic: false,
                    });
                }
                Ok(()) // start 返回即全部帧已在途;sink 随 self 一起活到 stop
            }
            fn stop(&mut self) {}
        }
        let health = Arc::new(SourceHealth::default());
        let mut cap = TappedCapture::new(
            Box::new(BurstCapture),
            Source::Mic,
            fast_policy(),
            health,
            TapNotify::none(),
        );
        let (tx, rx) = crossbeam_channel::unbounded();
        cap.start(tx).unwrap();
        cap.stop(); // BurstCapture 无后台线程,start 后发送端仅存于…无处:已 drop → 上游断开
        let mut got = 0;
        while let Ok(f) = rx.recv_timeout(std::time::Duration::from_millis(200)) {
            if f.samples.first().copied().unwrap_or(-1.0) >= 0.0 {
                got += 1;
            }
        }
        assert_eq!(got, 40, "stop 前已在途的帧一帧不许丢");
    }

    /// 比例钳制(issue #123 口径):采集线程卡死恢复后,停摆期攒的补零一次性入账,
    /// 原始比例可冲到 53(=5300%)——展示为「窗内占比」必须钳回 100%。
    #[test]
    fn storm_ratio_is_clamped_to_100_percent() {
        let mut d = GapStormDetector::new(std::time::Duration::from_secs(60), 0.05);
        let t0 = Instant::now();
        assert!(d.observe(t0, 0).is_none());
        // 61s 后一次性入账 53 分钟欠账
        let t1 = t0 + std::time::Duration::from_secs(61);
        match d.observe(t1, 53 * 60 * 1000) {
            Some(GapStormEvent::Storm(r)) => {
                assert!(r <= 1.0, "窗内占比不得超过 100%,得 {r}");
            }
            other => panic!("应起风暴,得 {other:?}"),
        }
    }

    /// 卡死采集线程不拖死 stop(issue #182 回归):后端把 sink 发送端泄漏给一条
    /// 永不退出的线程(模拟设备卡死的采集回调持有者),tap 的 recv 永不返回——
    /// stop() 必须在有界超时后放弃 join 返回,而不是把调用方(lifecycle actor,
    /// 事故当天连带整个应用)冻死。
    #[test]
    fn stop_returns_despite_wedged_capture_thread() {
        struct WedgedCapture;
        impl crate::audio::AudioCapture for WedgedCapture {
            fn start(&mut self, sink: Sender<AudioFrame>) -> anyhow::Result<()> {
                // 模拟卡死:发送端交给一条永远睡着的线程,通道永不关闭。
                std::thread::spawn(move || {
                    let _keep = sink;
                    loop {
                        std::thread::sleep(std::time::Duration::from_secs(3600));
                    }
                });
                Ok(())
            }
            fn stop(&mut self) {} // 停不动:真设备卡死时 stop 信号也救不了回调线程
        }
        let health = Arc::new(SourceHealth::default());
        let mut cap = TappedCapture::new(
            Box::new(WedgedCapture),
            Source::Mic,
            fast_policy(),
            health,
            TapNotify::none(),
        );
        let (tx, _rx) = crossbeam_channel::unbounded();
        cap.start(tx).unwrap();
        let begin = std::time::Instant::now();
        cap.stop();
        let took = begin.elapsed();
        assert!(
            took < TAP_JOIN_TIMEOUT * 5,
            "stop 应在超时档附近返回,实际等了 {took:?}"
        );
        // codex P1 的核心:stop 返回还不够,tap 必须已放掉下游发送端——
        // segment worker 的 frame_rx.iter() 靠通道关闭收尾,发送端不放它就冻着。
        let deadline = std::time::Instant::now() + TAP_JOIN_TIMEOUT * 5;
        loop {
            match _rx.recv_timeout(std::time::Duration::from_millis(50)) {
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                Ok(_) => {} // 排掉退场前可能填充的静音帧
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "下游通道迟迟不关闭:tap 没放发送端,worker 仍会冻死"
                    );
                }
            }
        }
    }

    /// 默认实现返回 0:不丢样(或不统计)的后端不该被记上莫须有的丢样。
    #[test]
    fn backends_without_drop_accounting_report_zero() {
        struct QuietCapture;
        impl crate::audio::AudioCapture for QuietCapture {
            fn start(&mut self, _sink: Sender<AudioFrame>) -> anyhow::Result<()> {
                Ok(())
            }
            fn stop(&mut self) {}
        }
        let health = Arc::new(SourceHealth::default());
        let mut cap = TappedCapture::new(
            Box::new(QuietCapture),
            Source::System,
            fast_policy(),
            health.clone(),
            TapNotify::none(),
        );
        let (tx, _rx) = crossbeam_channel::unbounded();
        cap.start(tx).unwrap();
        cap.stop();
        assert_eq!(health.cap_dropped_samples.load(Ordering::Relaxed), 0);
    }

    /// 断流风暴要在**录制中**就说出来。2026-08-17 那场蓝牙会议丢了 14.2% 的
    /// 时长,用户全程无提示,只能会后靠耳朵发现掉字;而 stall_after(3s 无帧)
    /// 抓不住这种「每次几百毫秒、每分钟二十来次」的高频短断流。
    /// 判据用 hw_gap_ms/墙钟 的滚动比例,不用语义已被证伪的 hw_gaps 计数。
    #[test]
    fn gap_storm_fires_once_on_rising_edge_and_rearms_after_recovery() {
        let t0 = Instant::now();
        let mut d = GapStormDetector::new(Duration::from_secs(60), 0.05);

        // 干净的一分钟:一个洞都没有,不许报。
        for s in 0..=60 {
            assert_eq!(d.observe(t0 + Duration::from_secs(s), 0), None, "无洞不得告警");
        }

        // 接下来一分钟丢 9 秒(15%),跨过 5% 阈值 → 上升沿报一次。
        let mut fired = Vec::new();
        for s in 61..=120 {
            let gap_ms = (s - 60) * 150; // 每秒累计 150ms 洞 = 15%
            if let Some(e) = d.observe(t0 + Duration::from_secs(s), gap_ms) {
                fired.push((s, e));
            }
        }
        assert_eq!(fired.len(), 1, "同一场风暴只报一次,实测 {fired:?}");
        assert!(
            matches!(fired[0].1, GapStormEvent::Storm(r) if r >= 0.05),
            "报出来的比例要是真比例,实测 {:?}",
            fired[0].1
        );

        // 风暴持续:仍然不重复报。
        let mut gap = 120u64 * 150;
        for s in 121..=180 {
            gap += 150;
            assert_eq!(
                d.observe(t0 + Duration::from_secs(s), gap),
                None,
                "持续风暴不得反复打扰"
            );
        }

        // 恢复:洞不再增长,滚动窗口内比例回落到阈值下。
        for s in 181..=300 {
            let _ = d.observe(t0 + Duration::from_secs(s), gap);
        }
        // 再来一场风暴 → 重新武装,应再报一次。
        let mut refired = 0;
        for s in 301..=360 {
            gap += 150;
            if matches!(d.observe(t0 + Duration::from_secs(s), gap), Some(GapStormEvent::Storm(_))) {
                refired += 1;
            }
        }
        assert_eq!(refired, 1, "回落后再起风暴要能再报");
    }

    /// Codex 复核 P1(2026-08-17):补零帧到了下游就再也认不出来了——它和设备
    /// 真的送来一段数字静音长得一模一样(host_time_ns=None 只是"没时戳",
    /// 不等于"我们造的")。于是最终 m4a 里的绝对零段既可能是补零、也可能是
    /// 设备/系统削的,归因只能靠猜;想做「AGC2 关/开 A/B」更是无从下手。
    /// 给帧打上来源标记,让下游能区别对待。
    #[test]
    fn zero_fill_frames_are_marked_synthetic_and_real_frames_are_not() {
        let (ctx, crx) = crossbeam_channel::unbounded();
        let (wtx, wrx) = crossbeam_channel::unbounded();
        let health = Arc::new(SourceHealth::default());
        let h2 = health.clone();
        let t = std::thread::spawn(move || {
            run_frame_tap(Source::System, crx, wtx, h2, fast_policy(), TapNotify::none())
        });
        ctx.send(frame(160)).unwrap();
        std::thread::sleep(Duration::from_millis(250));
        ctx.send(frame(160)).unwrap();
        drop(ctx);
        t.join().unwrap();
        let got: Vec<AudioFrame> = wrx.try_iter().collect();
        assert!(got.len() > 2, "断流期应有补零帧: {}", got.len());
        assert!(!got[0].synthetic, "首帧是真帧,不得标成合成");
        assert!(!got[got.len() - 1].synthetic, "尾帧是真帧,不得标成合成");
        assert!(
            got[1..got.len() - 1].iter().all(|f| f.synthetic),
            "断流期补的每一帧都必须自报是合成的"
        );
    }

    /// 接线回归:tap 真的会在断流风暴时叫出来,而不是只把数字埋进 audio.json。
    /// 会后才看得见的指标救不了正在开的这场会。
    #[test]
    fn tap_reports_gap_storm_while_recording() {
        const DECLARED: u32 = 16_000;
        let fired = Arc::new(AtomicU32::new(0));
        let f2 = fired.clone();
        let notify = TapNotify {
            on_stall: None,
            on_recover: None,
            on_gap_storm: Some(Box::new(move |ratio| {
                // 只数起风暴那一沿,平息沿不计。
                if ratio.is_some() {
                    f2.fetch_add(1, Ordering::Relaxed);
                }
            })),
        };
        let policy = TapPolicy {
            gap_storm_window: Duration::from_millis(120),
            gap_storm_threshold: 0.05,
            fill_after: Duration::from_millis(500),
            phase_trim_max: 0.0,
            ..fast_policy()
        };
        // 每帧 160 样本(10ms @16k),但硬件时戳每次前进 20ms:每帧都缺一整帧
        // = 持续断流。墙钟侧每帧 sleep 5ms,窗内比例远超 5%。
        let mut script: Vec<(usize, Option<u64>, Duration)> = Vec::new();
        let mut ns = 1_000u64;
        for _ in 0..60 {
            script.push((160, Some(ns), Duration::from_millis(5)));
            ns += 20_000_000;
        }
        let (_got, health) = run_scripted_with_notify(policy, DECLARED, &script, notify);
        assert!(
            health.hw_gap_ms.load(Ordering::Relaxed) > 0,
            "前提:这段脚本必须真的判出洞"
        );
        assert_eq!(fired.load(Ordering::Relaxed), 1, "断流风暴要在录制中报一次");
    }

    /// Codex review P2:后端只发上升沿,前端就只能拿定时器让横幅自行过期——
    /// 风暴若持续整场,detector 一直不重新武装、不再发事件,横幅 90 秒后永久消失,
    /// 而那时断流还在继续。必须补一条**恢复沿**,让"显示到风暴平息"真的成立。
    #[test]
    fn gap_storm_reports_recovery_edge_so_the_banner_can_stay_until_it_is_over() {
        let t0 = Instant::now();
        let mut d = GapStormDetector::new(Duration::from_secs(60), 0.05);
        for s in 0..=60 {
            assert_eq!(d.observe(t0 + Duration::from_secs(s), 0), None);
        }
        // 起风暴:上升沿报一次 Storm。
        let mut fired = Vec::new();
        for s in 61..=130 {
            let gap = (s - 60) * 150;
            if let Some(e) = d.observe(t0 + Duration::from_secs(s), gap) {
                fired.push((s, e));
            }
        }
        assert_eq!(fired.len(), 1, "起风暴只报一次,实测 {fired:?}");
        assert!(
            matches!(fired[0].1, GapStormEvent::Storm(r) if r >= 0.05),
            "第一条必须是 Storm 且带真实比例"
        );

        // 风暴停了(洞不再增长):窗内比例滑落到阈值下时要报一次 Recovered,
        // 且只报一次——否则整场剩余时间每个 tick 都在发"已恢复"。
        let gap = 130u64 * 150;
        let mut recovered = Vec::new();
        for s in 131..=400 {
            if let Some(e) = d.observe(t0 + Duration::from_secs(s), gap) {
                recovered.push((s, e));
            }
        }
        assert_eq!(recovered.len(), 1, "恢复也只报一次,实测 {recovered:?}");
        assert!(
            matches!(recovered[0].1, GapStormEvent::Recovered),
            "第二条必须是 Recovered"
        );
    }

    /// 从没起过风暴就不该报"已恢复":那会让前端凭空收到一条恢复事件。
    #[test]
    fn quiet_session_never_reports_recovery() {
        let t0 = Instant::now();
        let mut d = GapStormDetector::new(Duration::from_secs(60), 0.05);
        for s in 0..=300 {
            assert_eq!(d.observe(t0 + Duration::from_secs(s), 0), None, "第 {s} 秒");
        }
    }

    /// 阈值以下的零星断流不报警:正常设备偶发一两次几十毫秒的洞是常态,
    /// 报了就成了狼来了,用户会连真的风暴一起忽略。
    #[test]
    fn occasional_small_gaps_stay_below_the_alarm() {
        let t0 = Instant::now();
        let mut d = GapStormDetector::new(Duration::from_secs(60), 0.05);
        let mut gap = 0u64;
        for s in 0..=600 {
            // 每 30 秒来一个 200ms 的洞 ≈ 0.67%,远低于 5%。
            if s % 30 == 0 {
                gap += 200;
            }
            assert_eq!(
                d.observe(t0 + Duration::from_secs(s), gap),
                None,
                "第 {s} 秒:零星小洞不该告警"
            );
        }
    }

    /// 下游背压计量:worker 通道被塞住时,tap 要记录 send 阻塞时长与采集队列高水位。
    /// 这两个数把「worker 慢于实时」从推测变成每场可回看的证据(2026-08-14 事故里
    /// 只能靠对账风暴倒推,缺这层仪表)。
    #[test]
    fn tap_records_downstream_backpressure_metrics() {
        let (ctx, crx) = crossbeam_channel::unbounded();
        let (wtx, wrx) = crossbeam_channel::bounded::<AudioFrame>(1);
        let health = Arc::new(SourceHealth::default());
        let h2 = health.clone();
        let policy = fast_policy();
        let t = std::thread::spawn(move || {
            run_frame_tap(Source::Mic, crx, wtx, h2, policy, TapNotify::none())
        });
        // 一次性灌 6 帧:tap 转发第 1 帧后,后续 send 因 bounded(1) 满而阻塞,
        // 其间未被取走的帧堆在采集队列里 → 高水位可测。
        for _ in 0..6 {
            ctx.send(frame(160)).unwrap();
        }
        // 慢速排空:每 25ms 取一帧,制造可观测的阻塞时长(4 次 × ~25ms)。
        let mut got = 0;
        while got < 6 {
            std::thread::sleep(Duration::from_millis(25));
            if wrx.recv_timeout(Duration::from_secs(2)).is_ok() {
                got += 1;
            } else {
                break;
            }
        }
        drop(ctx);
        // 排干剩余(补零帧可能仍在路上)。
        while wrx.recv_timeout(Duration::from_millis(50)).is_ok() {}
        t.join().unwrap();

        assert!(
            health.send_wait_ms.load(Ordering::Relaxed) >= 30,
            "send 阻塞累计应可观测: {}ms",
            health.send_wait_ms.load(Ordering::Relaxed)
        );
        assert!(
            health.send_wait_max_ms.load(Ordering::Relaxed) >= 10,
            "单次阻塞峰值应可观测: {}ms",
            health.send_wait_max_ms.load(Ordering::Relaxed)
        );
        assert!(
            health.cap_queue_hw.load(Ordering::Relaxed) >= 2,
            "采集队列高水位应 ≥2: {}",
            health.cap_queue_hw.load(Ordering::Relaxed)
        );
    }

    /// 源一直出帧、但每秒样本数与它声明的采样率不符时,tap 必须按实测速率改写下游帧
    /// 的 sample_rate。这是「回放里同一句话说两遍、间隔越拉越大」的根:断流填充只
    /// 管"不出帧",管不了"出得不够快",后者永远不触发超时分支,时间轴就一路漂。
    /// 2026-08-04 实锤见 run_frame_tap 内注释。
    #[test]
    fn corrects_source_that_lies_about_its_sample_rate() {
        const DECLARED: u32 = 48_000;
        const TRUE_RATE: f64 = 32_000.0; // 2/3 的谎报:远超容差,又在合理性夹板内
        let (ctx, crx) = crossbeam_channel::unbounded();
        let (wtx, wrx) = crossbeam_channel::unbounded();
        let health = Arc::new(SourceHealth::default());
        let h2 = health.clone();
        // 本用例只验"速率判定",把还款关掉(phase_trim_max=0):还款期间盖的率是
        // 刻意偏离实测率的,混进来会让"末帧==实测率"这条断言测的东西不再单一。
        let policy = TapPolicy {
            rate_eval_window: Duration::from_millis(200),
            phase_trim_max: 0.0,
            ..wallclock_policy()
        };
        let t = std::thread::spawn(move || {
            run_frame_tap(Source::Mic, crx, wtx, h2, policy, TapNotify::none())
        });
        let gap = feed_at_rate(&ctx, DECLARED, TRUE_RATE, Duration::from_millis(600));
        drop(ctx);
        t.join().unwrap();

        let got: Vec<AudioFrame> = wrx.try_iter().collect();
        assert!(got.len() > 10, "应转发多帧: {}", got.len());
        assert_eq!(got[0].sample_rate, DECLARED, "评估窗满之前不得改写");
        if !timing_sound(gap, &policy) {
            return;
        }
        let last = got.last().unwrap().sample_rate as f64;
        assert!(
            (last - TRUE_RATE).abs() / TRUE_RATE < 0.05,
            "末帧应被改写为实测速率 ≈{TRUE_RATE},实得 {last}"
        );
        assert!(health.rate_fixes.load(Ordering::Relaxed) >= 1, "应记一次改写");
    }

    /// 按**生产参数**(60s 窗 / 2% 上限)确定性仿真还款:无线程、无 sleep,逐帧调
    /// 纯函数,断言债务在窗内线性归零、不过冲、且**下游看到的采样率种类极少**。
    ///
    /// 这条锁死四个曾经真实发生的缺陷:①按"债务/总窗长"逐帧重算是指数衰减而非按期
    /// 还清(60s 窗过完 +442ms 仍剩 163ms=1/e);②采样率取整用 round 时,债务小到
    /// k·base<0.5Hz 就再也还不动,永远卡在 ~0.65ms;③逐帧变率会让 segment_worker
    /// 反复重建 StreamResampler(实测约 138 次/60s),把那个重采样器要解决的跨块相位
    /// 问题又造回来;④末帧只截断账面 repaid 却仍盖原率,债务清零而真实时间轴已过冲。
    /// 缩小参数的仿真测不出这些,必须按生产参数跑。
    #[test]
    fn repays_phase_debt_linearly_at_production_parameters() {
        const BASE: u32 = 48_000;
        const FRAME_MS: f64 = 10.0;
        let per = (BASE as f64 * FRAME_MS / 1000.0) as usize;
        // +442ms 是"实际 48k/声明 44.1k"在 5s 快窗下的多发量;-406ms 是反向欠发量。
        for start in [0.442f64, -0.406] {
            let mut debt = start;
            let mut t = 0.0f64;
            let mut at_window = None;
            let mut halfway = None;
            let mut cleared_at = None;
            let mut rates = std::collections::BTreeSet::new();
            let held = phase_trim_rate(debt, PHASE_TRIM_SECS, BASE, PHASE_TRIM_MAX);
            // 恒定率的拧动幅度必须在听感上限内
            let k = (held as f64 - BASE as f64) / BASE as f64;
            assert!(k.abs() <= PHASE_TRIM_MAX + 1e-9, "start={start}: 拧动 {k:.4} 超上限");
            // debt>0 = 已多发时长 → 要少算,采样率要**调高**(per/rate 变小),故同号。
            assert_eq!(k.signum(), start.signum(), "start={start}: 拧动方向错");

            while t < PHASE_TRIM_SECS * 1.5 {
                if debt.abs() <= PHASE_TRIM_EPS {
                    cleared_at.get_or_insert(t);
                    break;
                }
                let (rate, repaid) = phase_trim_step(debt, held, BASE, per);
                rates.insert(rate);
                assert!(
                    repaid.abs() <= debt.abs() + 1e-12,
                    "start={start} t={t:.1}: 还款过冲(debt={debt:.9} repaid={repaid:.9})"
                );
                assert!(
                    repaid == 0.0 || repaid.signum() == debt.signum(),
                    "start={start} t={t:.1}: 还款方向错了"
                );
                debt -= repaid;
                t += FRAME_MS / 1000.0;
                if halfway.is_none() && t >= PHASE_TRIM_SECS / 2.0 {
                    halfway = Some(debt);
                }
                if at_window.is_none() && t >= PHASE_TRIM_SECS {
                    at_window = Some(debt);
                }
            }

            // 下游每见一个新率就重建一次 StreamResampler;整段还款只该有恒定率
            // (+ 末帧可能一个不过冲的收尾率)。逐帧重算的旧实现在这里是上百种。
            assert!(
                rates.len() <= 2,
                "start={start}: 还款期内下游只应看到 1~2 种采样率,实得 {} 种",
                rates.len()
            );
            // 半程应剩约一半(线性);指数衰减在此处会剩 1/√e ≈ 61%
            let mid = halfway.unwrap_or(0.0).abs() / start.abs();
            assert!(
                (0.35..=0.60).contains(&mid),
                "start={start}: 半程应剩约一半(线性),实剩 {:.0}%",
                mid * 100.0
            );
            // 窗末残余:与旧实现最直接的对照(指数衰减在此处剩 37%)
            let left = at_window.map(|d| d.abs()).unwrap_or(0.0) / start.abs();
            assert!(
                left < 0.05,
                "start={start}: 窗末残余应 <5%(旧指数衰减是 37%),实剩 {:.1}%",
                left * 100.0
            );
            let cleared = cleared_at.unwrap_or(f64::INFINITY);
            assert!(
                cleared <= PHASE_TRIM_SECS * 1.05,
                "start={start}: 应在窗长的 105% 内清零,实际 {cleared:.1}s(残余 {debt:.9}s)"
            );
        }
    }

    /// 突发帧(断流恢复后设备整批回吐)不得因"只截断账面"而让真实时间轴过冲:
    /// 盖上去的率必须自身就还不过头。
    #[test]
    fn phase_trim_never_overshoots_even_on_a_burst_frame() {
        const BASE: u32 = 48_000;
        let held = phase_trim_rate(0.4, PHASE_TRIM_SECS, BASE, PHASE_TRIM_MAX);
        // 剩 1ms 债务,却来了一帧 1 秒的突发音频
        let debt = 0.001;
        let per = BASE as usize; // 1s
        let (rate, repaid) = phase_trim_step(debt, held, BASE, per);
        assert!(repaid <= debt + 1e-12, "账面不得过冲: {repaid}");
        // 关键:按**返回的率**算出的真实时长差同样不得过冲
        let actual = per as f64 / BASE as f64 - per as f64 / rate as f64;
        assert!(
            actual <= debt + 1e-9,
            "真实时长差 {actual:.9}s 不得超过剩余债务 {debt:.9}s(率 {rate})"
        );
    }

    /// 内容锚点:在墙钟已知时刻埋一个满幅脉冲    /// 内容锚点:在墙钟已知时刻埋一个满幅脉冲,检查它在**下游时间轴上的位置**。
    ///
    /// 只对总样本量做断言是不够的——总长对上了不代表内容摆对了位置(补齐能把总长凑齐,
    /// 却可能把内容整体推偏)。这里直接量相位:脉冲在逻辑时间轴上的位置必须等于它被
    /// 喂进来时的墙钟时刻。两个速率方向都测。
    #[test]
    fn marker_lands_at_its_wall_clock_position_in_both_rate_directions() {
        const DECLARED: u32 = 48_000;
        for true_rate in [32_000.0f64, 64_000.0] {
            let (ctx, crx) = crossbeam_channel::unbounded();
            let (wtx, wrx) = crossbeam_channel::unbounded();
            let health = Arc::new(SourceHealth::default());
            // 快窗 150ms:脉冲埋在它之后,量的是"纠正生效后的内容位置"。
            // 还款窗 150ms、上限 50%:相位差在几百毫秒内还清,断言才能收紧到 15ms
            // ——旧版把容差放到 60ms、而残差恰好 50ms,那样的"通过"证明不了任何事。
            let policy = TapPolicy {
                rate_fast_window: Duration::from_millis(150),
                rate_eval_window: Duration::from_secs(3600),
                phase_trim_window: Duration::from_millis(150),
                phase_trim_max: 0.5,
                // fill_after 放宽到 500ms:本用例不测断流,而 fast_policy 的 50ms 会让
                // 负载下一次超时的 sleep 被当成断流、把时钟统计清零,检测被推到脉冲
                // 之后 —— 表现为间歇性失败。放宽后调度抖动不再影响判定时刻。
                fill_after: Duration::from_millis(500),
                ..fast_policy()
            };
            let t = std::thread::spawn(move || {
                run_frame_tap(Source::Mic, crx, wtx, health, policy, TapNotify::none())
            });

            // 时间原点必须与下游一致:下游的 logical 从**首帧**起算,若 mark 的墙钟
            // 从 t0 起算,首帧之前那次 sleep 与调度延迟(~10ms 且随负载浮动)会直接
            // 计入误差——这正是此前偶发失败的成因(实测捕到 0.696s 对 0.712s)。
            // 故记下首帧真正发出的时刻,并以它为原点度量 mark。
            let mark_at = Duration::from_millis(700);
            let t0 = Instant::now();
            let mut worst_gap = Duration::ZERO;
            let mut last = Instant::now();
            let mut sent = 0u64;
            let mut first_sent: Option<Instant> = None;
            let mut marked = false;
            let mut mark_wall = 0.0f64;
            while t0.elapsed() < Duration::from_millis(1100) {
                std::thread::sleep(Duration::from_millis(10));
                worst_gap = worst_gap.max(last.elapsed());
                last = Instant::now();
                let el = t0.elapsed();
                let want = (el.as_secs_f64() * true_rate) as u64;
                let n = want.saturating_sub(sent) as usize;
                if n == 0 {
                    continue;
                }
                let mut samples = vec![0.2f32; n];
                let now = Instant::now();
                if !marked && el >= mark_at {
                    samples[0] = 1.0; // 满幅脉冲,与 0.2 的底噪拉开
                    marked = true;
                    // 以首帧为原点;脉冲是本帧第 0 个样本,即本帧起点。
                    mark_wall = now.duration_since(first_sent.unwrap_or(now)).as_secs_f64();
                }
                ctx.send(AudioFrame {
                    samples,
                    sample_rate: DECLARED,
                    channels: 1,
                    host_time_ns: None,
                    synthetic: false,
                })
                .unwrap();
                first_sent.get_or_insert(now);
                sent = want;
            }
            drop(ctx);
            t.join().unwrap();
            assert!(marked, "脉冲应已喂出");
            if !timing_sound(worst_gap, &policy) {
                continue;
            }

            // 沿下游时间轴累加,找脉冲落在第几秒。
            let mut logical = 0.0f64;
            let mut mark_logical = None;
            for f in wrx.try_iter() {
                let per = f.samples.len() / f.channels.max(1) as usize;
                if mark_logical.is_none() {
                    if let Some(i) = f.samples.iter().position(|s| *s > 0.9) {
                        mark_logical = Some(logical + i as f64 / f.sample_rate as f64);
                    }
                }
                logical += per as f64 / f.sample_rate as f64;
            }
            let got = mark_logical.expect("下游应能找到脉冲");
            assert!(
                (got - mark_wall).abs() < 0.015,
                "true_rate={true_rate}: 脉冲逻辑位置 {got:.3}s 应贴合墙钟 {mark_wall:.3}s"
            );
        }
    }

    /// 端到端:谎报采样率的源,**最终写出的音频总时长必须等于墙钟**。
    ///
    /// 只断言"末帧采样率改对了"是不够的——改率只止住继续漂,检测窗内已经按错的率送
    /// 下去的那一段追不回来:44.1k 被当 48k 时 30s 墙钟只写进 27.56s,留下 2.44s 固定
    /// 错位跟着整场走,而且它在长录音里占比不到 1%,回放侧的兜底阈值也不会触发。
    /// 本测试按下游的解释(样本数 ÷ 该帧标称率)累加逻辑时长,与墙钟对表。
    #[test]
    fn total_logical_duration_matches_wall_clock_despite_lying_rate() {
        const DECLARED: u32 = 48_000;
        const TRUE_RATE: f64 = 32_000.0;
        let (ctx, crx) = crossbeam_channel::unbounded();
        let (wtx, wrx) = crossbeam_channel::unbounded();
        let health = Arc::new(SourceHealth::default());
        let policy =
            TapPolicy { rate_eval_window: Duration::from_millis(200), ..wallclock_policy() };
        let t = std::thread::spawn(move || {
            run_frame_tap(Source::Mic, crx, wtx, health, policy, TapNotify::none())
        });
        let t0 = Instant::now();
        let gap = feed_at_rate(&ctx, DECLARED, TRUE_RATE, Duration::from_millis(800));
        let wall = t0.elapsed().as_secs_f64();
        drop(ctx);
        t.join().unwrap();

        if !timing_sound(gap, &policy) {
            return;
        }
        let logical: f64 = wrx
            .try_iter()
            .map(|f| f.samples.len() as f64 / f.channels.max(1) as f64 / f.sample_rate as f64)
            .sum();
        // 容差 8%:首帧到达前的空窗 + 评估窗末尾那一帧的粒度,都是墙钟的零头。
        // 未补齐时这里会短掉整整 1/3 的评估窗(0.2s×(1-32/48)≈0.067s,占 800ms 的 8.3%)。
        assert!(
            (logical - wall).abs() / wall < 0.08,
            "逻辑时长 {logical:.3}s 应贴合墙钟 {wall:.3}s(差 {:.1}%)",
            (logical - wall).abs() / wall * 100.0
        );
    }

    /// 声明与实测一致时绝不改写——误改会把本来正常的轨道弄出漂移和变调。
    #[test]
    fn leaves_honest_sample_rate_untouched() {
        const RATE: u32 = 16_000;
        let (ctx, crx) = crossbeam_channel::unbounded();
        let (wtx, wrx) = crossbeam_channel::unbounded();
        let health = Arc::new(SourceHealth::default());
        let h2 = health.clone();
        let policy =
            TapPolicy { rate_eval_window: Duration::from_millis(200), ..wallclock_policy() };
        let t = std::thread::spawn(move || {
            run_frame_tap(Source::Mic, crx, wtx, h2, policy, TapNotify::none())
        });
        let gap = feed_at_rate(&ctx, RATE, RATE as f64, Duration::from_millis(600));
        drop(ctx);
        t.join().unwrap();

        let got: Vec<AudioFrame> = wrx.try_iter().collect();
        assert!(got.len() > 10);
        if !timing_sound(gap, &policy) {
            return;
        }
        assert!(
            got.iter().all(|f| f.sample_rate == RATE),
            "诚实的源不得被改写: {:?}",
            got.iter().map(|f| f.sample_rate).collect::<Vec<_>>()
        );
        assert_eq!(health.rate_fixes.load(Ordering::Relaxed), 0);
    }

    /// 断流 + 恢复后整批回吐(蓝牙 mic 常态)不得被误判成速率异常:那段时间瞬时
    /// 速率先 0 后暴涨,若计进统计就会把一条正常轨改写坏。
    #[test]
    fn drought_and_burst_do_not_trigger_rate_correction() {
        const RATE: u32 = 16_000;
        let (ctx, crx) = crossbeam_channel::unbounded();
        let (wtx, wrx) = crossbeam_channel::unbounded();
        let health = Arc::new(SourceHealth::default());
        let h2 = health.clone();
        let policy =
            TapPolicy { rate_eval_window: Duration::from_millis(150), ..fast_policy() };
        let t = std::thread::spawn(move || {
            run_frame_tap(Source::Mic, crx, wtx, h2, policy, TapNotify::none())
        });
        for _ in 0..3 {
            let _ = feed_at_rate(&ctx, RATE, RATE as f64, Duration::from_millis(120));
            std::thread::sleep(Duration::from_millis(200)); // 断流,tap 在此补零
            // 回吐:断流期积压的样本一次性到达
            ctx.send(AudioFrame {
                samples: vec![0.2; (RATE as f64 * 0.2) as usize],
                sample_rate: RATE,
                channels: 1,
                host_time_ns: None,
                synthetic: false,
            })
            .unwrap();
        }
        drop(ctx);
        t.join().unwrap();

        let got: Vec<AudioFrame> = wrx.try_iter().collect();
        assert!(
            got.iter().all(|f| f.sample_rate == RATE),
            "断流/回吐不得触发改写"
        );
        assert_eq!(health.rate_fixes.load(Ordering::Relaxed), 0);
        assert!(health.gaps.load(Ordering::Relaxed) >= 1, "断流本身仍应被计为 gap");
    }

    /// 实测速率离声明值超过一倍:病因不是"率写错了",改写只会把时间轴推得更歪。
    /// 保留声明值(留日志),把处置交给上层。
    #[test]
    fn absurd_measured_rate_is_not_applied() {
        const DECLARED: u32 = 48_000;
        let (ctx, crx) = crossbeam_channel::unbounded();
        let (wtx, wrx) = crossbeam_channel::unbounded();
        let health = Arc::new(SourceHealth::default());
        let h2 = health.clone();
        let policy =
            TapPolicy { rate_eval_window: Duration::from_millis(200), ..wallclock_policy() };
        let t = std::thread::spawn(move || {
            run_frame_tap(Source::Mic, crx, wtx, h2, policy, TapNotify::none())
        });
        let _ = feed_at_rate(&ctx, DECLARED, 4_000.0, Duration::from_millis(600));
        drop(ctx);
        t.join().unwrap();

        let got: Vec<AudioFrame> = wrx.try_iter().collect();
        assert!(
            got.iter().all(|f| f.sample_rate == DECLARED),
            "越过合理性夹板不得改写"
        );
        assert_eq!(health.rate_fixes.load(Ordering::Relaxed), 0);
    }

    /// 帧原样转发、计数正确;上游关闭 → 下游关闭(worker 收到通道关闭得以 flush)。
    #[test]
    fn forwards_frames_and_counts() {
        let (ctx, crx) = crossbeam_channel::unbounded();
        let (wtx, wrx) = crossbeam_channel::unbounded();
        let health = Arc::new(SourceHealth::default());
        let h2 = health.clone();
        let t = std::thread::spawn(move || {
            run_frame_tap(Source::Mic, crx, wtx, h2, fast_policy(), TapNotify::none())
        });
        ctx.send(frame(160)).unwrap();
        ctx.send(frame(320)).unwrap();
        drop(ctx);
        t.join().unwrap();
        let got: Vec<AudioFrame> = wrx.try_iter().collect();
        assert_eq!(got.len(), 2, "两帧全部转发");
        assert_eq!(got[0].samples.len(), 160);
        assert_eq!(got[1].samples.len(), 320);
        let snap = health.snapshot(Source::Mic);
        assert_eq!((snap.frames, snap.samples), (2, 480));
        assert_eq!(snap.gaps, 0, "无断流不计 gap");
        assert_eq!(snap.source, "mic");
    }

    /// 断流超过 fill_after 后补零,补量≈墙钟差;恢复后 gap 恰计 1 次。
    #[test]
    fn fills_silence_during_drought() {
        let (ctx, crx) = crossbeam_channel::unbounded();
        let (wtx, wrx) = crossbeam_channel::unbounded();
        let health = Arc::new(SourceHealth::default());
        let h2 = health.clone();
        let t = std::thread::spawn(move || {
            run_frame_tap(Source::System, crx, wtx, h2, fast_policy(), TapNotify::none())
        });
        ctx.send(frame(160)).unwrap();
        std::thread::sleep(Duration::from_millis(250));
        ctx.send(frame(160)).unwrap();
        drop(ctx);
        t.join().unwrap();
        let got: Vec<AudioFrame> = wrx.try_iter().collect();
        // 首帧 + 若干零帧 + 尾帧
        assert!(got.len() > 2, "断流期应有补零帧: {}", got.len());
        assert!(got[1..got.len() - 1].iter().all(|f| f.samples.iter().all(|s| *s == 0.0)));
        let filled: usize = got[1..got.len() - 1].iter().map(|f| f.samples.len()).sum();
        // 250ms 断流 @16k ≈ 4000 样本;填充从断流起点(≈首帧时刻)算起,
        // 上界=断流全长,下界=断流长-fill_after-一个 tick(判定与调度延迟)。
        assert!(
            (2000..=4800).contains(&filled),
            "补零量应约等于墙钟差: {filled} 样本"
        );
        assert_eq!(health.gaps.load(Ordering::Relaxed), 1, "一次断流计一次 gap");
        assert!(health.silence_ms.load(Ordering::Relaxed) >= 100);
    }

    /// 断流补零 + 设备恢复后吐出断流期缓冲(蓝牙 mic 常态)反复发生时,该轨时间轴
    /// 不得随轮数累积变长:转发总时长的上界是「墙钟 + 一次突发」,而不是墙钟的两倍。
    /// 这是 mic 轨曾比墙钟长 714s(2712s vs 1998s)的回归锁。
    #[test]
    fn burst_after_drought_does_not_inflate_timeline() {
        const ROUNDS: u32 = 4;
        const STALL: Duration = Duration::from_millis(200);
        const BURST_SAMPLES: usize = 3200; // 200ms @16k:断流期设备缓冲的回吐量

        let (ctx, crx) = crossbeam_channel::unbounded();
        let (wtx, wrx) = crossbeam_channel::unbounded();
        let health = Arc::new(SourceHealth::default());
        let t = std::thread::spawn(move || {
            run_frame_tap(Source::Mic, crx, wtx, health, fast_policy(), TapNotify::none())
        });

        let t0 = Instant::now();
        ctx.send(frame(160)).unwrap(); // 首帧:建立格式与时间轴锚点
        for _ in 0..ROUNDS {
            std::thread::sleep(STALL); // tap 在此期间补零
            ctx.send(frame(BURST_SAMPLES)).unwrap(); // 设备把断流期缓冲整批吐出
        }
        let wall = t0.elapsed();
        drop(ctx);
        t.join().unwrap();

        let forwarded: usize = wrx.try_iter().map(|f| f.samples.len()).sum();
        let wall_samples = (wall.as_secs_f64() * 16_000.0) as usize;
        // 上界:墙钟 + 一次突发 + 一个 tick 的调度余量。旧实现(按墙钟差补,不看
        // 已转发量)在同样脚本下会逼近 2×墙钟。
        let cap = wall_samples + BURST_SAMPLES + 16 * 10 * 2;
        assert!(
            forwarded <= cap,
            "转发总量不得随轮数累积:{forwarded} 样本 > 上界 {cap}(墙钟 {wall_samples})"
        );
        // 下界:确实补了零(否则本测试退化成"什么都没发生"也能过)。
        assert!(
            forwarded > wall_samples / 2,
            "断流期应补零维持时间轴:{forwarded} 样本 vs 墙钟 {wall_samples}"
        );
    }

    /// 从未收到帧(源没起来)绝不填充——不凭空造空白轨。
    #[test]
    fn never_fills_before_first_frame() {
        let (ctx, crx) = crossbeam_channel::unbounded::<AudioFrame>();
        let (wtx, wrx) = crossbeam_channel::unbounded();
        let health = Arc::new(SourceHealth::default());
        let h2 = health.clone();
        let t = std::thread::spawn(move || {
            run_frame_tap(Source::System, crx, wtx, h2, fast_policy(), TapNotify::none())
        });
        std::thread::sleep(Duration::from_millis(150));
        drop(ctx);
        t.join().unwrap();
        assert_eq!(wrx.try_iter().count(), 0, "无真实帧则无任何输出");
        assert_eq!(health.gaps.load(Ordering::Relaxed), 0);
    }

    /// stall 一次断流只报一次,来帧报 recover,后续断流可再触发。
    #[test]
    fn stall_and_recover_notifications_debounced() {
        static STALLS: StdAtomicU32 = StdAtomicU32::new(0);
        static RECOVERS: StdAtomicU32 = StdAtomicU32::new(0);
        STALLS.store(0, Ordering::SeqCst);
        RECOVERS.store(0, Ordering::SeqCst);
        let (ctx, crx) = crossbeam_channel::unbounded();
        let (wtx, _wrx) = crossbeam_channel::unbounded();
        let notify = TapNotify {
            on_stall: Some(Box::new(|| {
                STALLS.fetch_add(1, Ordering::SeqCst);
            })),
            on_recover: Some(Box::new(|| {
                RECOVERS.fetch_add(1, Ordering::SeqCst);
            })),
            on_gap_storm: None,
        };
        let health = Arc::new(SourceHealth::default());
        let t = std::thread::spawn(move || {
            run_frame_tap(Source::Mic, crx, wtx, health, fast_policy(), notify)
        });
        ctx.send(frame(160)).unwrap();
        std::thread::sleep(Duration::from_millis(300)); // 远超 stall_after=120ms
        ctx.send(frame(160)).unwrap(); // 恢复
        std::thread::sleep(Duration::from_millis(50)); // 不足 stall_after,不再报
        drop(ctx);
        t.join().unwrap();
        assert_eq!(STALLS.load(Ordering::SeqCst), 1, "一次断流报一次 stall");
        assert_eq!(RECOVERS.load(Ordering::SeqCst), 1, "恢复报一次 recover");
    }

    /// TappedCapture 对 session 层透明:Mock 同步灌帧全量透传,
    /// 内层结束(sender drop)→ tap 退出 → sink 关闭级联保持;stop 可 join。
    #[test]
    fn tapped_capture_is_transparent_wrapper() {
        use crate::audio::mock::MockCapture;
        let inner = MockCapture::from_wav(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/sample_16k.wav"
        ))
        .expect("fixture");
        let health = Arc::new(SourceHealth::default());
        let mut cap = TappedCapture::new(
            Box::new(inner),
            Source::Mic,
            fast_policy(),
            health.clone(),
            TapNotify::none(),
        );
        let (sink_tx, sink_rx) = crossbeam_channel::unbounded();
        cap.start(sink_tx).expect("start");
        // MockCapture 同步发完即返回;tap 排干后随通道关闭退出 → sink 关闭。
        let got: Vec<AudioFrame> = sink_rx.iter().collect();
        assert!(!got.is_empty());
        let total: usize = got.iter().map(|f| f.samples.len()).sum();
        assert_eq!(
            total,
            health.samples.load(Ordering::Relaxed) as usize,
            "透传样本数与统计一致(fixture 全量,无填充混入)"
        );
        cap.stop(); // join tap,不悬挂
    }

    /// 装配栈组合测试:TappedCapture(ResilientCapture(脚本采集))——lib.rs 生产
    /// 装配的同款层叠。脚本第一实例发帧后报流错误,自愈重建第二实例继续发帧;
    /// 断言:两实例的帧到达同一 sink、恢复回调驱动 restarts 计数、且(实例间
    /// 退避窗口 > fill_after)断流被 tap 计入 gaps 并补零维持时间轴。
    #[test]
    fn full_stack_tapped_resilient_survives_stream_error() {
        use crate::audio::resilient::{CaptureFactory, ResilientCapture, ResilientNotify};
        use crate::audio::CaptureEvent;

        struct Scripted {
            frames: usize,
            error_after_start: bool,
            events_tx: crossbeam_channel::Sender<CaptureEvent>,
        }
        impl AudioCapture for Scripted {
            fn start(&mut self, sink: Sender<AudioFrame>) -> anyhow::Result<()> {
                for _ in 0..self.frames {
                    let _ = sink.send(AudioFrame {
                        samples: vec![0.3; 160],
                        sample_rate: 16000,
                        channels: 1,
                        host_time_ns: None,
                        synthetic: false,
                    });
                }
                if self.error_after_start {
                    let _ = self.events_tx.send(CaptureEvent::Error("gone".into()));
                }
                Ok(())
            }
            fn stop(&mut self) {}
        }

        let built = Arc::new(StdAtomicU32::new(0));
        let b2 = built.clone();
        let factory: CaptureFactory = Box::new(move || {
            let n = b2.fetch_add(1, Ordering::SeqCst);
            let (etx, erx) = crossbeam_channel::unbounded();
            (
                Box::new(Scripted { frames: 2, error_after_start: n == 0, events_tx: etx })
                    as Box<dyn AudioCapture>,
                erx,
            )
        });
        let health = Arc::new(SourceHealth::default());
        let h2 = health.clone();
        let resilient = ResilientCapture::with_backoff(
            factory,
            ResilientNotify {
                // 生产装配同款:恢复回调递增健康计数(lib.rs 里另发 ipc 事件)。
                on_recovered: Some(Box::new(move || {
                    h2.restarts.fetch_add(1, Ordering::Relaxed);
                })),
                on_lost: None,
            },
            // 退避 120ms > fill_after 50ms:重建窗口内 tap 必然观察到断流并补零。
            vec![Duration::from_millis(120)],
        );
        let mut cap = TappedCapture::new(
            Box::new(resilient),
            Source::Mic,
            fast_policy(),
            health.clone(),
            TapNotify::none(),
        );
        let (sink_tx, sink_rx) = crossbeam_channel::unbounded();
        cap.start(sink_tx).expect("start");

        // 有界等待:两实例各 2 帧真实样本(0.3)到达;其间夹杂补零帧。
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut real = 0;
        let mut zeros = 0usize;
        while real < 4 && std::time::Instant::now() < deadline {
            if let Ok(f) = sink_rx.recv_timeout(Duration::from_millis(50)) {
                if f.samples.iter().any(|s| *s != 0.0) {
                    real += 1;
                } else {
                    zeros += f.samples.len();
                }
            }
        }
        assert_eq!(real, 4, "两实例的真实帧全部到达同一 sink");
        assert_eq!(built.load(Ordering::SeqCst), 2, "自愈恰重建一次");
        // 帧在重建实例 start() 内同步发出,on_recovered 在 start() 返回后才调:
        // 收满帧≠回调已执行,restarts 须有界轮询(与 resilient 单测同教训)。
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while health.restarts.load(Ordering::Relaxed) == 0
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        let snap = health.snapshot(Source::Mic);
        assert_eq!(snap.restarts, 1, "恢复回调驱动 restarts 计数");
        assert!(snap.gaps >= 1, "重建窗口的断流应计入 gaps");
        assert!(zeros > 0 && snap.silence_ms > 0, "断流期补零维持时间轴");
        cap.stop();
    }

    /// 下游关闭(会话拆除)时 tap 退出,不 panic 不空转。
    #[test]
    fn exits_when_worker_side_closed() {
        let (ctx, crx) = crossbeam_channel::unbounded();
        let (wtx, wrx) = crossbeam_channel::unbounded();
        drop(wrx);
        let health = Arc::new(SourceHealth::default());
        let t = std::thread::spawn(move || {
            run_frame_tap(Source::Mic, crx, wtx, health, fast_policy(), TapNotify::none())
        });
        ctx.send(frame(160)).unwrap();
        t.join().unwrap(); // send 失败即返回
    }

    /// Task 5 接线:真实帧喂 DriftMonitor,补零帧不喂;补零段结束(恢复收到真实帧)
    /// 必须记一次 reanchor 事件。
    ///
    /// 事件 kind 断言用 "reanchor_soft"(而非设计文档草案里的字面 "reanchor"):
    /// `DriftMonitor::mark_reanchor` 的既有实现(Task 4,commit 1d01b5c)按
    /// `full` 参数区分写 "reanchor_full"/"reanchor_soft" 两种 kind——gap_end 传
    /// full=false(保频率,只清相位),故落地为 "reanchor_soft"。这是 Task 4 已落地
    /// 的既定契约,本任务(仅接线 frame_tap)不改 drift_monitor.rs。
    /// P0 回归(2026-08-16 事故):硬件时戳看出"时间过去了、样本没来"的洞时,
    /// 必须按时戳把这段时间补回时间轴,而不是只 hw_gaps+1 让轨道整体缩短。
    /// 事故形态:一场 98 分钟录音,墙钟 5904s、落盘 5466s,438s 被直接压掉。
    #[test]
    fn hardware_gap_is_filled_back_into_the_timeline() {
        let (cap_tx, cap_rx) = crossbeam_channel::bounded::<AudioFrame>(64);
        let (out_tx, out_rx) = crossbeam_channel::bounded::<AudioFrame>(64);
        let health = std::sync::Arc::new(SourceHealth::default());
        // fill_after 放大到 5s:确保补出来的零**不是** Timeout 分支干的,
        // 只可能来自本次要验的时戳补洞。
        let policy = TapPolicy { fill_after: Duration::from_secs(5), ..wallclock_policy() };
        let h2 = health.clone();
        let t = std::thread::spawn(move || {
            run_frame_tap(Source::Mic, cap_rx, out_tx, h2, policy, TapNotify::none())
        });
        // 三帧 10ms 正常,然后时戳直接跳 300ms(中间的样本被 HAL/回调丢了),
        // 再来一帧。墙钟这边也真的等 300ms,否则"欠账"为 0、按设计不补。
        let mut ns = 0u64;
        for _ in 0..3 {
            cap_tx.send(AudioFrame { samples: vec![0.5; 160], sample_rate: 16_000,
                channels: 1, host_time_ns: Some(ns), synthetic: false }).unwrap();
            ns += 10_000_000;
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(300));
        ns += 300_000_000;
        cap_tx.send(AudioFrame { samples: vec![0.5; 160], sample_rate: 16_000,
            channels: 1, host_time_ns: Some(ns), synthetic: false }).unwrap();
        drop(cap_tx);
        t.join().unwrap();

        let mut zeros = 0usize;
        let mut real = 0usize;
        while let Ok(f) = out_rx.try_recv() {
            if f.samples.iter().all(|s| *s == 0.0) { zeros += f.samples.len(); } else { real += f.samples.len(); }
        }
        assert_eq!(real, 4 * 160, "四个真实帧必须原样转发");
        // 300ms @16k ≈ 4800 样本;允许墙钟欠账带来的下浮,但不能一个都不补。
        assert!(zeros >= 3_000, "洞必须补回时间轴,实测只补了 {zeros} 样本");
        let s = health.snapshot(Source::Mic);
        assert!(s.hw_gaps >= 1, "洞要计数");
        assert!(s.hw_gap_ms >= 180, "补了多久要能对账,实测 {}ms", s.hw_gap_ms);
    }

    /// Codex P1 回归:丢掉**一个**等长回调时,时戳差恰好翻倍、隐含率正好落在
    /// RATE_SANITY 下沿之内——旧判据(挂夹板)既不认它是洞,还把"一半速率"喂进
    /// 对账窗口。现在判据是"至少缺一整帧",单帧丢失必须被认出来并补回时间轴。
    #[test]
    fn a_single_dropped_callback_is_detected_and_repaired() {
        let (cap_tx, cap_rx) = crossbeam_channel::bounded::<AudioFrame>(64);
        let (out_tx, out_rx) = crossbeam_channel::bounded::<AudioFrame>(64);
        let health = std::sync::Arc::new(SourceHealth::default());
        let policy = TapPolicy { fill_after: Duration::from_secs(5), ..wallclock_policy() };
        let h2 = health.clone();
        let t = std::thread::spawn(move || {
            run_frame_tap(Source::Mic, cap_rx, out_tx, h2, policy, TapNotify::none())
        });
        // 10ms 一帧,第 4 帧的时戳跳了 20ms(中间那一帧被回调 try_send 丢了)。
        let mut ns = 0u64;
        for i in 0..6 {
            cap_tx.send(AudioFrame { samples: vec![0.5; 160], sample_rate: 16_000,
                channels: 1, host_time_ns: Some(ns), synthetic: false }).unwrap();
            ns += if i == 2 { 20_000_000 } else { 10_000_000 };
            std::thread::sleep(Duration::from_millis(if i == 2 { 20 } else { 10 }));
        }
        drop(cap_tx);
        t.join().unwrap();
        let mut zeros = 0usize;
        while let Ok(f) = out_rx.try_recv() {
            if f.samples.iter().all(|s| *s == 0.0) { zeros += f.samples.len(); }
        }
        let s = health.snapshot(Source::Mic);
        assert_eq!(s.hw_gaps, 1, "单帧丢失要认成一次洞");
        assert!(zeros >= 80, "丢掉的那 10ms 要补回来,实测 {zeros} 样本");
        assert!(s.hw_gap_ms >= 5, "补了多久要记账,实测 {}ms", s.hw_gap_ms);
    }

    /// 补零分块上限:一次长洞不得整块分配(13 分钟 @48k 单声道 ≈ 150MB)。
    /// 用一个 3 秒的洞验证输出被切成多帧、且每帧都不超过 FILL_CHUNK。
    #[test]
    fn long_hole_is_filled_in_bounded_chunks() {
        let (cap_tx, cap_rx) = crossbeam_channel::bounded::<AudioFrame>(64);
        let (out_tx, out_rx) = crossbeam_channel::bounded::<AudioFrame>(512);
        let health = std::sync::Arc::new(SourceHealth::default());
        // fill_after 大于洞长:排除 Timeout 分支参与,补零只可能来自时戳补洞。
        let policy = TapPolicy { fill_after: Duration::from_secs(30), ..wallclock_policy() };
        let h2 = health.clone();
        let t = std::thread::spawn(move || {
            run_frame_tap(Source::Mic, cap_rx, out_tx, h2, policy, TapNotify::none())
        });
        let mut ns = 0u64;
        cap_tx.send(AudioFrame { samples: vec![0.5; 160], sample_rate: 16_000, channels: 1,
            host_time_ns: Some(ns), synthetic: false }).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        ns += 10_000_000;
        cap_tx.send(AudioFrame { samples: vec![0.5; 160], sample_rate: 16_000, channels: 1,
            host_time_ns: Some(ns), synthetic: false }).unwrap();
        // 墙钟也真的过 3 秒,否则采集时钟欠账被墙钟口径的回退夹住。
        std::thread::sleep(Duration::from_millis(3000));
        ns += 3_000_000_000;
        cap_tx.send(AudioFrame { samples: vec![0.5; 160], sample_rate: 16_000, channels: 1,
            host_time_ns: Some(ns), synthetic: false }).unwrap();
        drop(cap_tx);
        t.join().unwrap();
        let mut zeros = 0usize;
        let mut max_frame = 0usize;
        while let Ok(f) = out_rx.try_recv() {
            if f.samples.iter().all(|s| *s == 0.0) {
                zeros += f.samples.len();
                max_frame = max_frame.max(f.samples.len());
            }
        }
        assert!(zeros >= 16_000, "3 秒的洞至少补回 1 秒,实测 {zeros} 样本");
        let cap = (FILL_CHUNK.as_secs_f64() * 16_000.0) as usize;
        assert!(max_frame <= cap, "单个补零帧 {max_frame} 样本超过上限 {cap}");
    }

    /// 反向锁:正常停录(队列即时排空)不得在尾巴上挂静音。
    #[test]
    fn clean_stop_adds_no_tail_silence() {
        let (cap_tx, cap_rx) = crossbeam_channel::bounded::<AudioFrame>(64);
        let (out_tx, out_rx) = crossbeam_channel::bounded::<AudioFrame>(64);
        let health = std::sync::Arc::new(SourceHealth::default());
        let policy = TapPolicy { fill_after: Duration::from_secs(30), ..wallclock_policy() };
        let h2 = health.clone();
        let t = std::thread::spawn(move || {
            run_frame_tap(Source::Mic, cap_rx, out_tx, h2, policy, TapNotify::none())
        });
        let mut ns = 0u64;
        for _ in 0..5 {
            cap_tx.send(AudioFrame { samples: vec![0.5; 160], sample_rate: 16_000, channels: 1,
                host_time_ns: Some(ns), synthetic: false }).unwrap();
            ns += 10_000_000;
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(cap_tx);
        t.join().unwrap();
        let mut zeros = 0usize;
        while let Ok(f) = out_rx.try_recv() {
            if f.samples.iter().all(|s| *s == 0.0) { zeros += f.samples.len(); }
        }
        assert_eq!(zeros, 0, "正常停录不该补静音,补了 {zeros} 样本");
    }

    /// 反向锁:突发缓冲回吐(样本一次到得比时间多)不是洞,绝不能补零——
    /// 补了就是凭空拉长时间轴。
    #[test]
    fn burst_delivery_is_not_mistaken_for_a_gap() {
        let (cap_tx, cap_rx) = crossbeam_channel::bounded::<AudioFrame>(64);
        let (out_tx, out_rx) = crossbeam_channel::bounded::<AudioFrame>(64);
        let health = std::sync::Arc::new(SourceHealth::default());
        let policy = TapPolicy { fill_after: Duration::from_secs(5), ..wallclock_policy() };
        let h2 = health.clone();
        let t = std::thread::spawn(move || {
            run_frame_tap(Source::Mic, cap_rx, out_tx, h2, policy, TapNotify::none())
        });
        // 时戳只走 1ms,却一次交付 100ms 的样本(隐含率 100 倍,夹板外)。
        let mut ns = 0u64;
        cap_tx.send(AudioFrame { samples: vec![0.5; 160], sample_rate: 16_000, channels: 1,
            host_time_ns: Some(ns), synthetic: false }).unwrap();
        ns += 1_000_000;
        cap_tx.send(AudioFrame { samples: vec![0.5; 1600], sample_rate: 16_000, channels: 1,
            host_time_ns: Some(ns), synthetic: false }).unwrap();
        drop(cap_tx);
        t.join().unwrap();
        let mut zeros = 0usize;
        while let Ok(f) = out_rx.try_recv() {
            if f.samples.iter().all(|s| *s == 0.0) { zeros += f.samples.len(); }
        }
        assert_eq!(zeros, 0, "回吐不是洞,不得补零");
        assert_eq!(health.snapshot(Source::Mic).hw_gap_ms, 0);
    }

    #[test]
    fn drift_monitor_fed_with_real_frames_only() {
        use crate::pipeline::drift_monitor::DriftMonitor;
        let monitor = std::sync::Arc::new(DriftMonitor::new(16_000));
        let (cap_tx, cap_rx) = crossbeam_channel::bounded::<AudioFrame>(64);
        let (out_tx, out_rx) = crossbeam_channel::bounded::<AudioFrame>(64);
        let health = std::sync::Arc::new(SourceHealth::default());
        let policy = TapPolicy { fill_after: Duration::from_millis(50), ..wallclock_policy() };
        let m2 = monitor.clone();
        let t = std::thread::spawn(move || {
            run_frame_tap_with_drift(
                Source::Mic, cap_rx, out_tx, health, policy, TapNotify::none(),
                std::sync::Arc::new(OnceLock::new()), Some(m2),
            )
        });
        // 两个真实帧 → 帧荒 200ms(触发补零) → 再一个真实帧
        for k in 0..2u64 {
            cap_tx.send(AudioFrame { samples: vec![0.5; 160], sample_rate: 16_000, channels: 1,
                host_time_ns: Some(k * 10_000_000), synthetic: false }).unwrap();
        }
        std::thread::sleep(Duration::from_millis(200));
        cap_tx.send(AudioFrame { samples: vec![0.5; 160], sample_rate: 16_000, channels: 1,
            host_time_ns: Some(300_000_000), synthetic: false }).unwrap();
        drop(cap_tx);
        t.join().unwrap();
        while out_rx.try_recv().is_ok() {}
        let r = monitor.snapshot();
        // 3 个真实帧全喂,补零帧不喂:全程带时间戳,quality 应仍是 "hw"。
        assert_eq!(r.quality, "hw");
        // 补零段结束应记一次重锚事件(soft:gap_end 保频率,只清相位)。
        assert!(r.events.iter().any(|e| e.kind == "reanchor_soft" && e.why == "gap_end"),
            "补零恢复必须重锚,events: {:?}", r.events);
        // 回归锁定:先 mark_reanchor 后 feed,单次断流的 reanchors 恰为 1(不会
        // 因 DLL 内部自动重锚被顺带触发而计成 2,见本函数上方 gap_end 顺序注释)。
        assert_eq!(r.reanchors, 1, "单次断流应恰好计 1 次 reanchor,snapshot: {:?}", r);
    }

    /// 终审 P1 回归锁:拔插耳机=断流(gap_end)+换率(device_switch)一次物理事件,
    /// 不得被旁挂各记一次而计成 2。场景:先喂 48k 帧,断流超过 fill_after,再以
    /// 44.1k 恢复(既是"补零段结束"又是"格式变了")。
    ///
    /// 断言口径按实际链路推演(非拍脑袋放宽):
    /// - events 里恰一条 device_switch(kind="reanchor_full"),没有 gap_end——
    ///   device_switch 分支覆盖了 gap_end 分支(见 run_frame_tap_with_drift 顶部
    ///   旁挂块的 if/else if)。另有一条 nominal_relock 事件(DriftMonitor::feed
    ///   自身对"标称率变了"的既有记录,与本次修复无关,不断言其存不存在之外的内容)。
    /// - snapshot().reanchors == 1:mark_reanchor("device_switch", true) 发生在
    ///   *旧* DriftDll 实例(nominal_hz=48000)身上,使其内部 reanchors 计数变 1;
    ///   随后 feed(44100 帧) 检测到标称率变化,触发 nominal_relock,把旧实例的
    ///   reanchors(=1)结转进 reanchor_carry,再换上全新的 44100 实例(reanchors=0)。
    ///   snapshot 的 reanchors = reanchor_carry(1) + 新实例.reanchors(0) = 1。
    #[test]
    fn device_switch_after_gap_counts_once_not_twice() {
        use crate::pipeline::drift_monitor::DriftMonitor;
        // 0 = 惰性初始化,与生产装配(lib.rs)同款:首帧到达前不知道设备声明率。
        let monitor = std::sync::Arc::new(DriftMonitor::new(0));
        let (cap_tx, cap_rx) = crossbeam_channel::bounded::<AudioFrame>(64);
        let (out_tx, out_rx) = crossbeam_channel::bounded::<AudioFrame>(64);
        let health = std::sync::Arc::new(SourceHealth::default());
        let policy = TapPolicy { fill_after: Duration::from_millis(50), ..wallclock_policy() };
        let m2 = monitor.clone();
        let t = std::thread::spawn(move || {
            run_frame_tap_with_drift(
                Source::Mic, cap_rx, out_tx, health, policy, TapNotify::none(),
                std::sync::Arc::new(OnceLock::new()), Some(m2),
            )
        });
        // 48k 首帧,建立格式与 DLL 锚点。
        cap_tx
            .send(AudioFrame {
                samples: vec![0.5; 480],
                sample_rate: 48_000,
                channels: 1,
                host_time_ns: Some(0),
                synthetic: false,
            })
            .unwrap();
        // 断流远超 fill_after(50ms),确保至少发出一次补零帧(filled_gap=true)。
        std::thread::sleep(Duration::from_millis(200));
        // 以不同采样率恢复:物理上"拔插耳机"的一次事件,应只记 device_switch。
        cap_tx
            .send(AudioFrame {
                samples: vec![0.5; 441],
                sample_rate: 44_100,
                channels: 1,
                host_time_ns: Some(300_000_000),
                synthetic: false,
            })
            .unwrap();
        drop(cap_tx);
        t.join().unwrap();
        while out_rx.try_recv().is_ok() {}

        let r = monitor.snapshot();
        let device_switch_events: Vec<_> =
            r.events.iter().filter(|e| e.why == "device_switch").collect();
        assert_eq!(
            device_switch_events.len(),
            1,
            "应恰有一条 device_switch 事件,events: {:?}",
            r.events
        );
        assert_eq!(device_switch_events[0].kind, "reanchor_full");
        assert!(
            !r.events.iter().any(|e| e.why == "gap_end"),
            "device_switch 应覆盖 gap_end,不得同时出现: {:?}",
            r.events
        );
        assert_eq!(
            r.reanchors, 1,
            "device_switch 的 mark 计入旧实例(48k)后被 nominal_relock 结转,\
             新实例(44.1k)尚无重锚,snapshot: {:?}",
            r
        );
    }

    /// Codex review Fix 1(P2):同格式换设备(如 48k mono → 48k mono 换了台新麦克风)
    /// 只看 (sample_rate, channels) 的 device_switch 检测不到,但 ResilientCapture
    /// 采集重启(health.restarts 递增)是真正的"换了后端实例"信号,必须独立触发
    /// capture_restart 全清重锚,且该帧不得再叠加发出 gap_end。
    #[test]
    fn capture_restart_triggers_reanchor_without_double_counting_gap_end() {
        use crate::pipeline::drift_monitor::DriftMonitor;
        let monitor = std::sync::Arc::new(DriftMonitor::new(0));
        let (cap_tx, cap_rx) = crossbeam_channel::bounded::<AudioFrame>(64);
        let (out_tx, out_rx) = crossbeam_channel::bounded::<AudioFrame>(64);
        let health = std::sync::Arc::new(SourceHealth::default());
        let h2 = health.clone();
        let policy = TapPolicy { fill_after: Duration::from_millis(50), ..wallclock_policy() };
        let m2 = monitor.clone();
        let t = std::thread::spawn(move || {
            run_frame_tap_with_drift(
                Source::Mic, cap_rx, out_tx, h2, policy, TapNotify::none(),
                std::sync::Arc::new(OnceLock::new()), Some(m2),
            )
        });
        // 首帧:建立格式(48k mono)与 DLL 锚点。
        cap_tx
            .send(AudioFrame {
                samples: vec![0.5; 480],
                sample_rate: 48_000,
                channels: 1,
                host_time_ns: Some(0),
                synthetic: false,
            })
            .unwrap();
        // 断流超过 fill_after(50ms),tap 会补零(filled_gap=true)——不加这一步就测不出
        // "capture_restart 优先于 gap_end、同帧只发一个"这条,因为没有断流本就不会有
        // gap_end 候选。
        std::thread::sleep(Duration::from_millis(200));
        // 模拟采集重启:ResilientCapture 换了后端实例,但新设备恰好也是 48k mono
        // ——格式元组不变,device_switch 检测不到。
        health.restarts.fetch_add(1, Ordering::Relaxed);
        // 恢复帧格式不变(同格式换设备):若无本修复,会走 filled_gap 分支记 gap_end;
        // 修复后应改记 capture_restart,且不再叠加 gap_end。
        cap_tx
            .send(AudioFrame {
                samples: vec![0.5; 480],
                sample_rate: 48_000,
                channels: 1,
                host_time_ns: Some(300_000_000),
                synthetic: false,
            })
            .unwrap();
        drop(cap_tx);
        t.join().unwrap();
        while out_rx.try_recv().is_ok() {}

        let r = monitor.snapshot();
        assert!(
            r.events.iter().any(|e| e.why == "capture_restart" && e.kind == "reanchor_full"),
            "采集重启应记一次 capture_restart 全清重锚,events: {:?}",
            r.events
        );
        assert!(
            !r.events.iter().any(|e| e.why == "gap_end"),
            "capture_restart 帧不得同帧再叠加 gap_end,events: {:?}",
            r.events
        );
    }
}
