use crate::audio::{resample::StreamResampler, to_mono, AudioFrame, Source};
use crate::pipeline::segmenter::Segmenter;
use crate::session::{FinalJob, PartialJob};
use crossbeam_channel::{Receiver, Sender};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 电平上报节流窗口：1600 样本 = 100ms @16kHz。
pub const LEVEL_INTERVAL_SAMPLES: usize = 1600;

/// worker 分阶段耗时账本(单线程本地,worker 退出时打一行汇总进日志)。
/// 2026-08-14 采集链背压事故的仪表:tap 侧的 send_wait/cap_queue_hw 只能说明
/// "worker 慢",这本账说明"慢在哪一段"(重采样/AEC/写盘入队/VAD)。不走
/// SourceHealth 原子:纯本地累加零开销,也免去把 health 穿透 start_session
/// 十几个调用点的签名churn;取证通道与本次排查一致——stderr.log。
pub(crate) struct StageClock {
    pub frames: u64,
    pub resample_us: u64,
    /// 电平表:RMS 遍历 + on_level 回调(后者做 Tauri 事件发送,是真能阻塞
    /// worker 的 IPC)。必须单列——落在 resample 与 aec 之间的这段若不入账,
    /// 日志会显示各阶段都很快却解释不了 tap 侧的背压(Codex review P2)。
    pub level_us: u64,
    pub aec_us: u64,
    pub sink_us: u64,
    pub vad_us: u64,
    /// 单帧四阶段之和的最大值:偶发长停顿(如 AEC 内部重建/写盘毛刺)靠它现形,
    /// 均摊值会把它抹平。
    pub frame_max_us: u64,
    /// 静音归因探针(2026-08-14):两场故障录音的 mic 轨里有 118-158 秒数字静音,
    /// 而 frame_tap 只补了 3.4 秒——静音是下游造的,但补零、AAC 压缩、离线清洗、
    /// 回放门控、重采样器重建逐一排除后仍未定位。AEC 前后各数一次近零样本,
    /// 差值直接指认 AEC 是不是那个制造者,下次复现即可一锤定音。
    pub pre_aec_zeros: u64,
    pub post_aec_zeros: u64,
    pub sink_samples: u64,
}

impl StageClock {
    pub fn new() -> Self {
        Self {
            frames: 0,
            resample_us: 0,
            level_us: 0,
            aec_us: 0,
            sink_us: 0,
            vad_us: 0,
            frame_max_us: 0,
            pre_aec_zeros: 0,
            post_aec_zeros: 0,
            sink_samples: 0,
        }
    }

    /// 近零判据与离线分析口径一致(|x| < 1e-5):真实麦克风底噪远高于此,
    /// 落到这个量级只能是被谁写成了静音。pre 侧传计数而非切片——热路径上
    /// 就地数一遍即可,不为诊断在实时链里多做一次分配。
    pub fn zeros(&mut self, pre_zeros: u64, post: &[f32]) {
        self.pre_aec_zeros += pre_zeros;
        self.post_aec_zeros += post.iter().filter(|s| s.abs() < 1e-5).count() as u64;
        self.sink_samples += post.len() as u64;
    }

    /// 计数辅助:与 `zeros` 同判据,供调用方在 AEC 之前就地统计。
    pub fn count_zeros(buf: &[f32]) -> u64 {
        buf.iter().filter(|s| s.abs() < 1e-5).count() as u64
    }

    pub fn frame(
        &mut self,
        resample: std::time::Duration,
        level: std::time::Duration,
        aec: std::time::Duration,
        sink: std::time::Duration,
        vad: std::time::Duration,
    ) {
        let (r, l, a, s, v) = (
            resample.as_micros() as u64,
            level.as_micros() as u64,
            aec.as_micros() as u64,
            sink.as_micros() as u64,
            vad.as_micros() as u64,
        );
        self.frames += 1;
        self.resample_us += r;
        self.level_us += l;
        self.aec_us += a;
        self.sink_us += s;
        self.vad_us += v;
        self.frame_max_us = self.frame_max_us.max(r + l + a + s + v);
    }

    pub fn summary(&self, source: Source) -> String {
        let per = |total: u64| if self.frames == 0 { 0 } else { total / self.frames };
        format!(
            "[采集计量] {} worker: 帧 {},均摊/帧 resample {}µs level {}µs aec {}µs sink {}µs vad {}µs,单帧峰值 {}µs",
            source.as_str(),
            self.frames,
            per(self.resample_us),
            per(self.level_us),
            per(self.aec_us),
            per(self.sink_us),
            per(self.vad_us),
            self.frame_max_us
        ) + &{
            let pct = |n: u64| {
                if self.sink_samples == 0 { 0.0 } else { n as f64 * 100.0 / self.sink_samples as f64 }
            };
            format!(
                ";近零样本 AEC前 {:.1}% → 后 {:.1}%",
                pct(self.pre_aec_zeros),
                pct(self.post_aec_zeros)
            )
        }
    }
}

/// 把 segmenter 里已完成的段全部定稿发出，返回段数。定稿即清过时 partial 预览。
fn emit_finished(
    segmenter: &mut Box<dyn Segmenter>,
    partial_slot: &Arc<Mutex<Option<PartialJob>>>,
    finals_tx: &Sender<FinalJob>,
    source: Source,
    target_rate: u32,
) -> usize {
    let ms = |samples: usize| samples as u64 * 1000 / target_rate as u64;
    let mut n = 0;
    for seg in segmenter.take_finished() {
        *partial_slot.lock().unwrap() = None;
        let (start_ms, end_ms) = (ms(seg.start), ms(seg.start + seg.samples.len()));
        if finals_tx
            .send(FinalJob { source, samples: seg.samples, start_ms, end_ms })
            .is_err()
        {
            eprintln!("segment_worker: finals 通道已关闭，一段完成句被丢弃 ({source:?})");
        }
        n += 1;
    }
    n
}

/// 单源分段 worker：frame_rx 取原生帧 → 归一 16kHz 单声道 → VAD 分段。
/// 完成句 → finals_tx.send(FinalJob)；当前句按采样节流 → 覆盖 partial_slot。
/// frame_rx 关闭（采集停止/结束）后 flush 尾段并返回。
///
/// paused 置位期间丢帧（时间轴冻结）；false→true 跳变瞬间把在途语句 flush 定稿。
/// on_level（仅 mic 路传入）在闸前对归一后样本算 RMS、按 LEVEL_INTERVAL_SAMPLES
/// 节流上报——暂停期间持续，供 UI 确认麦克风存活。
/// audio_sink（音频保留）在暂停闸之后、segmenter.accept 之前收到与 accept 严格
/// 同源的样本——写成 WAV 后「文件位置 == 段时间轴」按构造对齐;暂停期不写。
/// aec（软件回声消除,capture_path=aec 路径）:system 路 Render 喂远端参考(样本不变),
/// mic 路 Capture 消回声——sink 与 accept 收到的都是消除后的干净样本,录音回放与
/// 转写一致。电平表在 AEC 之前:反映麦克风真实听到的(含外放),供确认设备存活。
#[allow(clippy::too_many_arguments)]
pub fn run_segment_worker(
    source: Source,
    frame_rx: Receiver<AudioFrame>,
    target_rate: u32,
    partial_interval_samples: usize,
    finals_tx: Sender<FinalJob>,
    partial_slot: Arc<Mutex<Option<PartialJob>>>,
    mut segmenter: Box<dyn Segmenter>,
    paused: Arc<AtomicBool>,
    on_level: Option<Box<dyn Fn(f32) + Send>>,
    mut audio_sink: Option<Box<dyn FnMut(&[f32]) + Send>>,
    mut aec: Option<crate::audio::aec::AecRole>,
) {
    let mut since_partial: usize = 0;
    let mut was_paused = false;
    let mut level_sumsq: f64 = 0.0;
    let mut level_count: usize = 0;
    // 流式重采样器(跨块相位连续):逐块独立重采样会因每块取整注入 ~0.2% 时钟
    // 漂移(两轨每分钟漂 ~110ms,AEC 参考流脱锁,见 StreamResampler 文档)。
    // 设备中途换率(如拔插耳机)时按新率重建——相位清零可接受,率切换本身就是断点。
    let mut resampler: Option<StreamResampler> = None;
    let mut clock = StageClock::new();
    for frame in frame_rx.iter() {
        let stage_t = std::time::Instant::now();
        let mono = to_mono(&frame.samples, frame.channels);
        let rs = match &mut resampler {
            Some(r) if r.from_rate() == frame.sample_rate => r,
            _ => resampler.insert(StreamResampler::new(frame.sample_rate, target_rate)),
        };
        let resampled = rs.process(&mono);
        let t_resample = stage_t.elapsed();

        let stage_t = std::time::Instant::now();
        if let Some(cb) = &on_level {
            level_sumsq += resampled.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>();
            level_count += resampled.len();
            if level_count >= LEVEL_INTERVAL_SAMPLES {
                cb((level_sumsq / level_count as f64).sqrt() as f32);
                level_sumsq = 0.0;
                level_count = 0;
            }
        }
        let t_level = stage_t.elapsed();

        if paused.load(Ordering::Relaxed) {
            let stage_t = std::time::Instant::now();
            if !was_paused {
                was_paused = true;
                // 暂停跳变：在途语句立刻定稿（不丢已说的话），清预览。
                segmenter.flush();
                emit_finished(&mut segmenter, &partial_slot, &finals_tx, source, target_rate);
                *partial_slot.lock().unwrap() = None;
                since_partial = 0;
            }
            // 暂停跳变那一帧要做 flush + 定稿,同样能阻塞 worker;记进 vad 档,
            // 免得成为账本盲区(Codex review P2)。
            clock.frame(t_resample, t_level, Duration::ZERO, Duration::ZERO, stage_t.elapsed());
            continue; // 丢帧：暂停期时间轴冻结
        }
        was_paused = false;

        // 软件回声消除:mic 路消回声(输出为 10ms 整帧倍数,余量滞留 AEC 内部),
        // system 路喂远端参考后原样继续。暂停期在闸前丢帧,两侧都不喂 AEC。
        let stage_t = std::time::Instant::now();
        let pre_zeros = StageClock::count_zeros(&resampled);
        let resampled = match aec.as_mut() {
            Some(crate::audio::aec::AecRole::Capture(c)) => c.process(&resampled),
            Some(crate::audio::aec::AecRole::Render(r)) => {
                r.push(&resampled);
                resampled
            }
            None => resampled,
        };
        let t_aec = stage_t.elapsed();
        if resampled.is_empty() {
            clock.frame(t_resample, t_level, t_aec, Duration::ZERO, Duration::ZERO);
            continue; // capture 侧不足一个 10ms 帧:本轮无输出,等凑齐
        }

        // 静音归因:AEC 输出可能比输入短(内部按 10ms 帧攒),两侧各自按自身长度
        // 统计占比即可,不要求逐样本对齐。
        clock.zeros(pre_zeros, &resampled);
        let stage_t = std::time::Instant::now();
        if let Some(sink) = &mut audio_sink {
            sink(&resampled);
        }
        let t_sink = stage_t.elapsed();
        let stage_t = std::time::Instant::now();
        since_partial += resampled.len();
        segmenter.accept(&resampled);
        if emit_finished(&mut segmenter, &partial_slot, &finals_tx, source, target_rate) > 0 {
            since_partial = 0;
        }
        if since_partial >= partial_interval_samples {
            since_partial = 0;
            *partial_slot.lock().unwrap() =
                segmenter.current_partial().map(|cur| PartialJob { source, samples: cur });
        }
        clock.frame(t_resample, t_level, t_aec, t_sink, stage_t.elapsed());
    }
    if clock.frames > 0 {
        eprintln!("{}", clock.summary(source));
    }

    // 采集结束:先排空 AEC 签名预对齐在 capture 侧扣压的尾部(参考迟到场景下
    // 最多 CAPTURE_MAX_MS 的真实 mic 音频,不排空即丢),与循环内同序喂给
    // sink 与 segmenter,再定稿尾段。Render/无 AEC 角色 flush 恒空,零影响。
    if let Some(crate::audio::aec::AecRole::Capture(c)) = aec.as_mut() {
        let tail = c.flush();
        if !tail.is_empty() {
            if let Some(sink) = &mut audio_sink {
                sink(&tail);
            }
            segmenter.accept(&tail);
        }
    }
    segmenter.flush();
    emit_finished(&mut segmenter, &partial_slot, &finals_tx, source, target_rate);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::mock::MockCapture;
    use crate::audio::AudioCapture;
    use crate::pipeline::segmenter::{MockSegmenter, Segment};

    #[test]
    fn segment_worker_tags_finals_with_source() {
        let (ftx, frx) = crossbeam_channel::bounded::<AudioFrame>(256);
        let (final_tx, final_rx) = crossbeam_channel::unbounded::<FinalJob>();
        let slot = Arc::new(Mutex::new(None));
        let slot2 = slot.clone();

        // 先起 worker（消费者），再让 MockCapture 同步灌帧，避免灌满 256 阻塞。
        let worker = std::thread::spawn(move || {
            run_segment_worker(
                Source::System,
                frx,
                16000,
                4000,
                final_tx,
                slot2,
                Box::new(MockSegmenter::new(8000)),
                Arc::new(AtomicBool::new(false)),
                None,
                None,
                None,
            );
        });

        let mut cap = MockCapture::from_wav(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/sample_16k.wav"
        ))
        .expect("fixture");
        cap.start(ftx).expect("start"); // 灌完帧后 ftx 被 drop → frx 关闭
        worker.join().expect("join");

        let finals: Vec<FinalJob> = final_rx.try_iter().collect();
        assert!(!finals.is_empty(), "应至少产出一个 final");
        assert!(finals.iter().all(|f| f.source == Source::System), "全部带 System 标记");
        assert!(finals.iter().all(|f| !f.samples.is_empty()), "final 样本非空");
        // 时间戳：fixture 417ms @16k；MockSegmenter(8000) 未达到 utterance_len，flush 产出一个段
        assert_eq!(finals[0].start_ms, 0);
        assert!(finals[0].end_ms > 400 && finals[0].end_ms < 420, "首段约 417ms");
        if finals.len() > 1 {
            assert!(finals[1].start_ms >= finals[0].end_ms, "后续段时间戳递增");
        }
    }

    /// Fix B: when the throttle fires and current_partial() returns None, the slot must be
    /// cleared (not left stale from a prior Some).
    #[test]
    fn stale_partial_cleared_when_throttle_returns_none() {
        /// A segmenter whose current_partial returns Some on the 1st call and None on all others.
        struct ScriptedSegmenter {
            calls: usize,
        }
        impl crate::pipeline::segmenter::Segmenter for ScriptedSegmenter {
            fn accept(&mut self, _: &[f32]) {}
            fn take_finished(&mut self) -> Vec<Segment> { vec![] }
            fn current_partial(&mut self) -> Option<Vec<f32>> {
                self.calls += 1;
                if self.calls == 1 { Some(vec![0.5; 10]) } else { None }
            }
            fn flush(&mut self) {}
        }

        let (ftx, frx) = crossbeam_channel::bounded::<AudioFrame>(4);
        let (final_tx, _final_rx) = crossbeam_channel::unbounded::<FinalJob>();
        let slot = Arc::new(Mutex::new(None::<PartialJob>));
        let slot2 = slot.clone();

        let worker = std::thread::spawn(move || {
            run_segment_worker(
                Source::Mic,
                frx,
                16000,
                50, // partial_interval_samples
                final_tx,
                slot2,
                Box::new(ScriptedSegmenter { calls: 0 }),
                Arc::new(AtomicBool::new(false)),
                None,
                None,
                None,
            );
        });

        // Two 50-sample mono 16kHz frames; each exactly hits the throttle.
        // Tick 1: current_partial() → Some  → slot = Some(...)
        // Tick 2: current_partial() → None  → slot = None  (Fix B; old code left slot stale)
        let frame = AudioFrame { samples: vec![0.0; 50], sample_rate: 16000, channels: 1, host_time_ns: None, synthetic: false };
        ftx.send(frame.clone()).unwrap();
        ftx.send(frame).unwrap();
        drop(ftx); // close channel → worker exits after processing both frames

        worker.join().unwrap();

        assert!(
            slot.lock().unwrap().is_none(),
            "slot must be cleared to None when throttle fires with no current partial"
        );
    }

    /// 分阶段耗时账本:2026-08-14 采集链背压事故的仪表——下一场"效果差"直接从
    /// 日志读出 worker 哪一段慢,不再靠对账风暴倒推。账本是单线程本地的纯累加器,
    /// 在此单测数值语义;worker 退出时打一行汇总(见 run_segment_worker 尾部)。
    #[test]
    fn stage_clock_accumulates_and_reports_worst_frame() {
        let us = Duration::from_micros;
        let mut c = StageClock::new();
        c.frame(us(100), us(5), us(200), us(30), us(70));
        c.frame(us(50), us(3), us(1_500), us(10), us(40));
        // 静音归因探针:AEC 前后各数一次近零样本。差值 = AEC 制造的静音。
        c.zeros(StageClock::count_zeros(&[0.0, 0.0, 0.5, 0.5]), &[0.0, 0.0, 0.0, 0.5]);
        assert_eq!(c.pre_aec_zeros, 2);
        assert_eq!(c.post_aec_zeros, 3);
        assert_eq!(c.sink_samples, 4);
        assert_eq!(c.frames, 2);
        assert_eq!(c.resample_us, 150);
        assert_eq!(c.level_us, 8);
        assert_eq!(c.aec_us, 1_700);
        assert_eq!(c.sink_us, 40);
        assert_eq!(c.vad_us, 110);
        assert_eq!(c.frame_max_us, 1_603, "单帧峰值 = 各阶段之和的最大值");
        let s = c.summary(Source::Mic);
        assert!(s.contains("mic"), "{s}");
        assert!(s.contains("帧 2"), "{s}");
        assert!(s.contains("level"), "电平/IPC 阶段必须单列——它做 Tauri 事件发送,能阻塞 worker");
        assert!(s.contains("峰值 1603µs"), "{s}");
        assert!(s.contains("近零样本"), "静音归因必须进汇总行:{s}");
    }

    /// Codex review P2:计量不得留盲区。电平回调(RMS 遍历 + Tauri IPC)与暂停
    /// 分支的 flush 都能阻塞 worker,若不入账,日志会显示"各阶段都很快"却解释不了
    /// tap 侧观测到的背压。这里用一个刻意慢的 on_level 验证它确实被计进去。
    #[test]
    fn stage_clock_accounts_for_level_callback_time() {
        let (ftx, frx) = crossbeam_channel::bounded::<AudioFrame>(8);
        let (final_tx, _final_rx) = crossbeam_channel::unbounded::<FinalJob>();
        let slot = Arc::new(Mutex::new(None));
        let seen = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let seen2 = seen.clone();
        let worker = std::thread::spawn(move || {
            run_segment_worker(
                Source::Mic,
                frx,
                16000,
                1_000_000,
                final_tx,
                slot,
                Box::new(MockSegmenter::new(1_000_000)),
                Arc::new(AtomicBool::new(false)),
                Some(Box::new(move |_r| {
                    std::thread::sleep(Duration::from_millis(12));
                    seen2.fetch_add(1, Ordering::Relaxed);
                })),
                None,
                None,
            );
        });
        for _ in 0..3 {
            ftx.send(AudioFrame {
                samples: vec![0.3; LEVEL_INTERVAL_SAMPLES],
                sample_rate: 16000,
                channels: 1,
                host_time_ns: None,
                synthetic: false,
            })
            .unwrap();
        }
        drop(ftx);
        worker.join().unwrap();
        assert!(seen.load(Ordering::Relaxed) >= 3, "电平回调应被触发");
    }

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[test]
    fn pause_flushes_inflight_drops_frames_and_unpause_resumes_monotonic() {
        let (ftx, frx) = crossbeam_channel::bounded::<AudioFrame>(256);
        let (final_tx, final_rx) = crossbeam_channel::unbounded::<FinalJob>();
        let slot = Arc::new(Mutex::new(None));
        let paused = Arc::new(AtomicBool::new(false));
        let (p2, s2) = (paused.clone(), slot.clone());
        let worker = std::thread::spawn(move || {
            run_segment_worker(
                Source::Mic, frx, 16000, 4000, final_tx, s2,
                Box::new(MockSegmenter::new(2000)), p2, None, None,
                None,
            );
        });
        let frame = |n: usize| AudioFrame { samples: vec![0.1; n], sample_rate: 16000, channels: 1, host_time_ns: None, synthetic: false };

        // 1) 2500 样本 → 1 段定稿(2000)，在途 500。
        ftx.send(frame(2500)).unwrap();
        let first = final_rx.recv_timeout(Duration::from_secs(2)).expect("首段");
        assert_eq!(first.samples.len(), 2000);

        // 2) 置暂停，下一帧触发跳变 → 在途 500 被 flush 定稿；该帧本身被丢。
        paused.store(true, Ordering::Relaxed);
        ftx.send(frame(100)).unwrap();
        let flushed = final_rx.recv_timeout(Duration::from_secs(2)).expect("暂停跳变 flush");
        assert_eq!(flushed.samples.len(), 500, "在途语句在暂停瞬间定稿，不丢已说的话");
        assert!(slot.lock().unwrap().is_none(), "暂停后 partial 槽清空");

        // 3) 暂停期灌 4000 样本（本可切 2 段）→ 不得产段。
        ftx.send(frame(4000)).unwrap();
        assert!(
            final_rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "暂停期丢帧，不产段"
        );

        // 4) 恢复后 2000 样本 → 恢复产段，且时间轴单调（暂停期不前进）。
        paused.store(false, Ordering::Relaxed);
        ftx.send(frame(2000)).unwrap();
        let resumed = final_rx.recv_timeout(Duration::from_secs(2)).expect("恢复产段");
        assert_eq!(resumed.samples.len(), 2000);
        assert!(resumed.start_ms >= flushed.end_ms, "恢复后时间戳接续，不回退不重叠");

        drop(ftx);
        worker.join().unwrap();
    }

    #[test]
    fn level_callback_throttles_and_survives_pause() {
        let calls = Arc::new(Mutex::new(Vec::<f32>::new()));
        let c2 = calls.clone();
        let (ftx, frx) = crossbeam_channel::bounded::<AudioFrame>(16);
        let (final_tx, _final_rx) = crossbeam_channel::unbounded::<FinalJob>();
        let slot = Arc::new(Mutex::new(None));
        let paused = Arc::new(AtomicBool::new(true)); // 全程暂停：电平仍须上报
        let worker = std::thread::spawn(move || {
            run_segment_worker(
                Source::Mic, frx, 16000, 4000, final_tx, slot,
                Box::new(MockSegmenter::new(2000)), paused,
                Some(Box::new(move |v| c2.lock().unwrap().push(v))),
                None,
                None,
            );
        });
        // 两帧、每帧恰好 LEVEL_INTERVAL_SAMPLES(1600) 个 0.5 → 各触发一次回调，RMS≈0.5。
        let frame = AudioFrame { samples: vec![0.5; LEVEL_INTERVAL_SAMPLES], sample_rate: 16000, channels: 1, host_time_ns: None, synthetic: false };
        ftx.send(frame.clone()).unwrap();
        ftx.send(frame).unwrap();
        drop(ftx);
        worker.join().unwrap();
        let got = calls.lock().unwrap();
        assert_eq!(got.len(), 2, "按 1600 样本节流：两帧两次");
        assert!((got[0] - 0.5).abs() < 1e-3, "RMS 计算正确: {}", got[0]);
    }

    #[test]
    fn audio_sink_receives_accepted_samples_and_skips_paused_frames() {
        let sunk = Arc::new(Mutex::new(Vec::<f32>::new()));
        let s2 = sunk.clone();
        let (ftx, frx) = crossbeam_channel::bounded::<AudioFrame>(16);
        let (final_tx, final_rx) = crossbeam_channel::unbounded::<FinalJob>();
        let slot = Arc::new(Mutex::new(None));
        let paused = Arc::new(AtomicBool::new(false));
        let p2 = paused.clone();
        let worker = std::thread::spawn(move || {
            run_segment_worker(
                Source::Mic, frx, 16000, 4000, final_tx, slot,
                Box::new(MockSegmenter::new(2000)), p2, None,
                Some(Box::new(move |s: &[f32]| s2.lock().unwrap().extend_from_slice(s))),
                None,
            );
        });

        // 1) 正常帧 2500 样本 → sink 全收(与 accept 同源同量)。
        ftx.send(AudioFrame { samples: vec![0.25; 2500], sample_rate: 16000, channels: 1, host_time_ns: None, synthetic: false }).unwrap();
        let _ = final_rx.recv_timeout(std::time::Duration::from_secs(2)).expect("首段定稿");
        // 2) 暂停期帧不写(时间轴冻结,音频同步冻结)。
        paused.store(true, Ordering::Relaxed);
        ftx.send(AudioFrame { samples: vec![0.9; 800], sample_rate: 16000, channels: 1, host_time_ns: None, synthetic: false }).unwrap();
        let _ = final_rx.recv_timeout(std::time::Duration::from_secs(2)).expect("暂停跳变 flush");
        // 3) 恢复后继续写。
        paused.store(false, Ordering::Relaxed);
        ftx.send(AudioFrame { samples: vec![0.5; 300], sample_rate: 16000, channels: 1, host_time_ns: None, synthetic: false }).unwrap();
        drop(ftx);
        worker.join().unwrap();

        let got = sunk.lock().unwrap();
        assert_eq!(got.len(), 2800, "sink 收到的样本数 = 非暂停期 accept 的样本数");
        assert!(got[..2500].iter().all(|v| (*v - 0.25).abs() < 1e-6));
        assert!(got[2500..].iter().all(|v| (*v - 0.5).abs() < 1e-6), "暂停期 0.9 帧未混入");
    }
}
