use crate::audio::{resample::resample_linear, to_mono, AudioFrame, Source};
use crate::pipeline::segmenter::Segmenter;
use crate::session::{FinalJob, PartialJob};
use crossbeam_channel::{Receiver, Sender};
use std::sync::{Arc, Mutex};

/// 单源分段 worker：frame_rx 取原生帧 → 归一 16kHz 单声道 → VAD 分段。
/// 完成句 → finals_tx.send(FinalJob)；当前句按采样节流 → 覆盖 partial_slot。
/// frame_rx 关闭（采集停止/结束）后 flush 尾段并返回。
pub fn run_segment_worker(
    source: Source,
    frame_rx: Receiver<AudioFrame>,
    target_rate: u32,
    partial_interval_samples: usize,
    finals_tx: Sender<FinalJob>,
    partial_slot: Arc<Mutex<Option<PartialJob>>>,
    mut segmenter: Box<dyn Segmenter>,
) {
    let mut since_partial: usize = 0;
    for frame in frame_rx.iter() {
        let mono = to_mono(&frame.samples, frame.channels);
        let resampled = resample_linear(&mono, frame.sample_rate, target_rate);
        since_partial += resampled.len();
        segmenter.accept(&resampled);

        for seg in segmenter.take_finished() {
            *partial_slot.lock().unwrap() = None; // 定稿：清过时预览
            if finals_tx.send(FinalJob { source, samples: seg.samples }).is_err() {
                eprintln!("segment_worker: finals 通道已关闭，一段完成句被丢弃 ({source:?})");
            }
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
    for seg in segmenter.take_finished() {
        *partial_slot.lock().unwrap() = None;
        if finals_tx.send(FinalJob { source, samples: seg.samples }).is_err() {
            eprintln!("segment_worker: finals 通道已关闭，一段完成句被丢弃 ({source:?})");
        }
    }
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
}
