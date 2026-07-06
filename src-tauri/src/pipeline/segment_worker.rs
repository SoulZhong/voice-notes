use crate::audio::{resample::resample_linear, to_mono, AudioFrame, Source};
use crate::pipeline::segmenter::Segmenter;
use crate::session::{FinalJob, PartialJob};
use crossbeam_channel::{Receiver, Sender};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// 电平上报节流窗口：1600 样本 = 100ms @16kHz。
pub const LEVEL_INTERVAL_SAMPLES: usize = 1600;

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
/// aec（软件回声消除,「保持外放音量」模式）:system 路 Render 喂远端参考(样本不变),
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
    for frame in frame_rx.iter() {
        let mono = to_mono(&frame.samples, frame.channels);
        let resampled = resample_linear(&mono, frame.sample_rate, target_rate);

        if let Some(cb) = &on_level {
            level_sumsq += resampled.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>();
            level_count += resampled.len();
            if level_count >= LEVEL_INTERVAL_SAMPLES {
                cb((level_sumsq / level_count as f64).sqrt() as f32);
                level_sumsq = 0.0;
                level_count = 0;
            }
        }

        if paused.load(Ordering::Relaxed) {
            if !was_paused {
                was_paused = true;
                // 暂停跳变：在途语句立刻定稿（不丢已说的话），清预览。
                segmenter.flush();
                emit_finished(&mut segmenter, &partial_slot, &finals_tx, source, target_rate);
                *partial_slot.lock().unwrap() = None;
                since_partial = 0;
            }
            continue; // 丢帧：暂停期时间轴冻结
        }
        was_paused = false;

        // 软件回声消除:mic 路消回声(输出为 10ms 整帧倍数,余量滞留 AEC 内部),
        // system 路喂远端参考后原样继续。暂停期在闸前丢帧,两侧都不喂 AEC。
        let resampled = match aec.as_mut() {
            Some(crate::audio::aec::AecRole::Capture(c)) => c.process(&resampled),
            Some(crate::audio::aec::AecRole::Render(r)) => {
                r.push(&resampled);
                resampled
            }
            None => resampled,
        };
        if resampled.is_empty() {
            continue; // capture 侧不足一个 10ms 帧:本轮无输出,等凑齐
        }

        if let Some(sink) = &mut audio_sink {
            sink(&resampled);
        }
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
    }

    // 采集结束：尾段定稿
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
        let frame = AudioFrame { samples: vec![0.0; 50], sample_rate: 16000, channels: 1 };
        ftx.send(frame.clone()).unwrap();
        ftx.send(frame).unwrap();
        drop(ftx); // close channel → worker exits after processing both frames

        worker.join().unwrap();

        assert!(
            slot.lock().unwrap().is_none(),
            "slot must be cleared to None when throttle fires with no current partial"
        );
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
        let frame = |n: usize| AudioFrame { samples: vec![0.1; n], sample_rate: 16000, channels: 1 };

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
        let frame = AudioFrame { samples: vec![0.5; LEVEL_INTERVAL_SAMPLES], sample_rate: 16000, channels: 1 };
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
        ftx.send(AudioFrame { samples: vec![0.25; 2500], sample_rate: 16000, channels: 1 }).unwrap();
        let _ = final_rx.recv_timeout(std::time::Duration::from_secs(2)).expect("首段定稿");
        // 2) 暂停期帧不写(时间轴冻结,音频同步冻结)。
        paused.store(true, Ordering::Relaxed);
        ftx.send(AudioFrame { samples: vec![0.9; 800], sample_rate: 16000, channels: 1 }).unwrap();
        let _ = final_rx.recv_timeout(std::time::Duration::from_secs(2)).expect("暂停跳变 flush");
        // 3) 恢复后继续写。
        paused.store(false, Ordering::Relaxed);
        ftx.send(AudioFrame { samples: vec![0.5; 300], sample_rate: 16000, channels: 1 }).unwrap();
        drop(ftx);
        worker.join().unwrap();

        let got = sunk.lock().unwrap();
        assert_eq!(got.len(), 2800, "sink 收到的样本数 = 非暂停期 accept 的样本数");
        assert!(got[..2500].iter().all(|v| (*v - 0.25).abs() < 1e-6));
        assert!(got[2500..].iter().all(|v| (*v - 0.5).abs() < 1e-6), "暂停期 0.9 帧未混入");
    }
}
