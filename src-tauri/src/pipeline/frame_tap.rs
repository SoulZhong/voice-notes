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
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use serde::Serialize;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
        }
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
}

impl TapPolicy {
    pub fn mic() -> Self {
        Self {
            fill_after: Duration::from_millis(500),
            stall_after: Some(Duration::from_secs(3)),
            tick: Duration::from_millis(100),
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
        }
    }
    #[cfg(windows)]
    pub fn system_loopback() -> Self {
        Self {
            fill_after: Duration::from_millis(250),
            stall_after: None,
            tick: Duration::from_millis(100),
        }
    }
}

/// 失联/恢复通知(可选;断连自愈与 UI 事件在装配层接入)。
pub struct TapNotify {
    pub on_stall: Option<Box<dyn Fn() + Send>>,
    pub on_recover: Option<Box<dyn Fn() + Send>>,
}

impl TapNotify {
    pub fn none() -> Self {
        Self { on_stall: None, on_recover: None }
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
pub struct TappedCapture {
    inner: Box<dyn AudioCapture>,
    source: Source,
    policy: TapPolicy,
    health: Arc<SourceHealth>,
    /// start 时取走(TapNotify 非 Clone);重复 start 本仓不存在,取空则退化为无通知。
    notify: Option<TapNotify>,
    tap: Option<std::thread::JoinHandle<()>>,
}

impl TappedCapture {
    pub fn new(
        inner: Box<dyn AudioCapture>,
        source: Source,
        policy: TapPolicy,
        health: Arc<SourceHealth>,
        notify: TapNotify,
    ) -> Self {
        Self { inner, source, policy, health, notify: Some(notify), tap: None }
    }
}

impl AudioCapture for TappedCapture {
    fn start(&mut self, sink: Sender<AudioFrame>) -> anyhow::Result<()> {
        let (cap_tx, cap_rx) = crossbeam_channel::bounded::<AudioFrame>(256);
        let health = self.health.clone();
        let policy = self.policy;
        let source = self.source;
        let notify = self.notify.take().unwrap_or_else(TapNotify::none);
        self.tap = Some(std::thread::spawn(move || {
            run_frame_tap(source, cap_rx, sink, health, policy, notify)
        }));
        self.inner.start(cap_tx)
    }

    fn stop(&mut self) {
        self.inner.stop();
        if let Some(t) = self.tap.take() {
            let _ = t.join();
        }
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
    // 最近一次真实帧的格式:没收到过帧就不填充(源可能根本没起来,
    // 填零会凭空造出一条空白轨)。
    let mut last_format: Option<(u32, u16)> = None;
    let mut last_frame_at = Instant::now();
    // 已填充到的时间点(≥ last_frame_at):每轮只补 [filled_until, now] 的差量,
    // 断流恢复后设备从"现在"重新出帧,重叠误差上限为一个 tick,可接受。
    let mut filled_until = Instant::now();
    // 本次断流是否已计 gap / 已报 stall(去抖:恢复前不重复)。
    let mut gap_counted = false;
    let mut stalled = false;

    loop {
        match from_capture.recv_timeout(policy.tick) {
            Ok(frame) => {
                if stalled {
                    if let Some(cb) = &notify.on_recover {
                        cb();
                    }
                }
                stalled = false;
                gap_counted = false;
                last_format = Some((frame.sample_rate, frame.channels));
                last_frame_at = Instant::now();
                filled_until = last_frame_at;
                health.frames.fetch_add(1, Ordering::Relaxed);
                health.samples.fetch_add(frame.samples.len() as u64, Ordering::Relaxed);
                if to_worker.send(frame).is_err() {
                    return; // 会话拆除,下游已关
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                let Some((rate, channels)) = last_format else {
                    continue;
                };
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
                // 差量补零:样本数按墙钟差与采样率精确折算,交错声道等比放大。
                let deficit = filled_until.elapsed();
                let frames_n = (deficit.as_secs_f64() * rate as f64) as usize;
                if frames_n == 0 {
                    continue;
                }
                filled_until += Duration::from_secs_f64(frames_n as f64 / rate as f64);
                health
                    .silence_ms
                    .fetch_add((frames_n as u64 * 1000) / rate as u64, Ordering::Relaxed);
                let silence = AudioFrame {
                    samples: vec![0.0; frames_n * channels as usize],
                    sample_rate: rate,
                    channels,
                };
                if to_worker.send(silence).is_err() {
                    return;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32 as StdAtomicU32;

    fn frame(n: usize) -> AudioFrame {
        AudioFrame { samples: vec![0.5; n], sample_rate: 16000, channels: 1 }
    }

    fn fast_policy() -> TapPolicy {
        TapPolicy {
            fill_after: Duration::from_millis(50),
            stall_after: Some(Duration::from_millis(120)),
            tick: Duration::from_millis(10),
        }
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
}
