use crate::asr::Recognizer;
use crate::audio::{resample::resample_linear, to_mono, AudioCapture};
use crate::audio::Source;
use crate::pipeline::segmenter::Segmenter;
use crossbeam_channel::bounded;
use crossbeam_channel::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 完成句识别任务：进 finals 队列，永不丢弃（保证不丢内容）。
#[derive(Debug, Clone)]
pub struct FinalJob {
    pub source: Source,
    pub samples: Vec<f32>,
}

/// 当前句预览任务：写入每源覆盖式槽，忙时被更新版本覆盖（best-effort）。
#[derive(Debug, Clone)]
pub struct PartialJob {
    pub source: Source,
    pub samples: Vec<f32>,
}

/// 单识别 worker：串行消费 finals（不丢、优先），空闲时取每源最新 partial（best-effort）。
/// finals_rx 关闭且排干后返回。识别失败的完成句 emit "[识别失败]" 占位，worker 不退出。
pub fn run_asr_worker(
    mut recognizer: Box<dyn Recognizer>,
    finals_rx: Receiver<FinalJob>,
    partial_slots: Vec<(Source, Arc<Mutex<Option<PartialJob>>>)>,
    mut on_final: impl FnMut(Source, String),
    mut on_partial: impl FnMut(Source, String),
) {
    loop {
        match finals_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(job) => {
                let text = match recognizer.recognize(&job.samples) {
                    Ok(t) => t.text,
                    Err(_) => "[识别失败]".to_string(),
                };
                on_final(job.source, text);
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                // 空闲：服务每源最新 partial（取出即清空，只识别最新一版）。
                for (src, slot) in &partial_slots {
                    let job = slot.lock().unwrap().take();
                    if let Some(job) = job {
                        if let Ok(t) = recognizer.recognize(&job.samples) {
                            on_partial(*src, t.text);
                        }
                    }
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// 录制管线核心：capture 取帧 → 归一 16kHz 单声道 → 喂 segmenter。
/// 每出现完成语句 → 识别 → on_final；按采样节流对当前句识别 → on_partial。
#[allow(clippy::too_many_arguments)]
pub fn run_pipeline(
    mut capture: Box<dyn AudioCapture>,
    mut recognizer: Box<dyn Recognizer>,
    mut segmenter: Box<dyn Segmenter>,
    target_rate: u32,
    partial_interval_samples: usize,
    mut on_partial: impl FnMut(String),
    mut on_final: impl FnMut(String),
) -> anyhow::Result<()> {
    let (tx, rx) = bounded::<crate::audio::AudioFrame>(256);
    capture.start(tx)?;

    let result = (|| -> anyhow::Result<()> {
        let mut since_partial: usize = 0;
        for frame in rx.iter() {
            let mono = to_mono(&frame.samples, frame.channels);
            let resampled = resample_linear(&mono, frame.sample_rate, target_rate);
            since_partial += resampled.len();
            segmenter.accept(&resampled);

            // 完成的语句：定稿
            for seg in segmenter.take_finished() {
                let t = recognizer.recognize(&seg.samples)?;
                on_final(t.text);
                since_partial = 0; // 定稿后重置 partial 节流
            }

            // 当前句：按采样节流出 partial
            if since_partial >= partial_interval_samples {
                since_partial = 0;
                if let Some(cur) = segmenter.current_partial() {
                    let t = recognizer.recognize(&cur)?;
                    on_partial(t.text);
                }
            }
        }
        // 收尾：尾段定稿
        segmenter.flush();
        for seg in segmenter.take_finished() {
            let t = recognizer.recognize(&seg.samples)?;
            on_final(t.text);
        }
        Ok(())
    })();

    capture.stop();
    result
}

#[cfg(test)]
mod asr_worker_tests {
    use super::*;
    use crate::asr::{Recognizer, Transcript};
    use crate::audio::Source;
    use std::sync::{Arc, Mutex};

    struct CountingRecognizer;
    impl Recognizer for CountingRecognizer {
        fn recognize(&mut self, s: &[f32]) -> anyhow::Result<Transcript> {
            Ok(Transcript { text: format!("len={}", s.len()) })
        }
    }

    struct FlakyRecognizer { n: usize }
    impl Recognizer for FlakyRecognizer {
        fn recognize(&mut self, s: &[f32]) -> anyhow::Result<Transcript> {
            self.n += 1;
            if self.n == 1 {
                anyhow::bail!("boom");
            }
            Ok(Transcript { text: format!("len={}", s.len()) })
        }
    }

    #[test]
    fn emits_all_finals_tagged_in_order() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.0; 10] }).unwrap();
        tx.send(FinalJob { source: Source::System, samples: vec![0.0; 20] }).unwrap();
        drop(tx);

        let finals = Arc::new(Mutex::new(Vec::<(Source, String)>::new()));
        let f2 = finals.clone();
        run_asr_worker(
            Box::new(CountingRecognizer),
            rx,
            vec![],
            move |s, t| f2.lock().unwrap().push((s, t)),
            |_, _| {},
        );
        assert_eq!(
            *finals.lock().unwrap(),
            vec![(Source::Mic, "len=10".into()), (Source::System, "len=20".into())]
        );
    }

    #[test]
    fn failed_final_becomes_placeholder_and_worker_survives() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.0; 3] }).unwrap();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.0; 4] }).unwrap();
        drop(tx);

        let finals = Arc::new(Mutex::new(Vec::<(Source, String)>::new()));
        let f2 = finals.clone();
        run_asr_worker(
            Box::new(FlakyRecognizer { n: 0 }),
            rx,
            vec![],
            move |s, t| f2.lock().unwrap().push((s, t)),
            |_, _| {},
        );
        assert_eq!(
            *finals.lock().unwrap(),
            vec![(Source::Mic, "[识别失败]".into()), (Source::Mic, "len=4".into())]
        );
    }

    #[test]
    fn services_latest_partial_when_idle() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        let slot = Arc::new(Mutex::new(Some(PartialJob { source: Source::System, samples: vec![0.0; 7] })));
        let partials = Arc::new(Mutex::new(Vec::<(Source, String)>::new()));
        let p2 = partials.clone();
        let slot_for_worker = slot.clone();

        let worker = std::thread::spawn(move || {
            run_asr_worker(
                Box::new(CountingRecognizer),
                rx,
                vec![(Source::System, slot_for_worker)],
                |_, _| {},
                move |s, t| p2.lock().unwrap().push((s, t)),
            );
        });

        // 轮询等待 worker 在空闲分支服务了 partial 槽（有界，避免固定 sleep 假设）。
        let mut serviced = false;
        for _ in 0..200 {
            if !partials.lock().unwrap().is_empty() {
                serviced = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        drop(tx); // 结束 worker
        worker.join().unwrap();

        assert!(serviced, "空闲时应服务 partial 槽");
        assert_eq!(*partials.lock().unwrap(), vec![(Source::System, "len=7".into())]);
        assert!(slot.lock().unwrap().is_none(), "partial 取出后槽应清空");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asr::{Recognizer, Transcript};
    use crate::audio::mock::MockCapture;
    use crate::pipeline::segmenter::MockSegmenter;
    use std::sync::{Arc, Mutex};

    /// 假识别器：回传样本数，便于断言管线确实送来了归一化音频。
    struct CountingRecognizer;
    impl Recognizer for CountingRecognizer {
        fn recognize(&mut self, samples: &[f32]) -> anyhow::Result<Transcript> {
            Ok(Transcript { text: format!("len={}", samples.len()) })
        }
    }

    #[test]
    fn pipeline_emits_finals_via_segmenter() {
        let capture = Box::new(
            MockCapture::from_wav(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sample_16k.wav"))
                .expect("fixture"),
        );
        // 小 utterance_len 确保从 fixture 切出多个 final；partial 间隔给小值确保至少触发一次。
        let segmenter = Box::new(MockSegmenter::new(8000));
        let finals = Arc::new(Mutex::new(Vec::<String>::new()));
        let partials = Arc::new(Mutex::new(Vec::<String>::new()));
        let f2 = finals.clone();
        let p2 = partials.clone();
        run_pipeline(
            capture,
            Box::new(CountingRecognizer),
            segmenter,
            16000,
            4000,
            move |t| p2.lock().unwrap().push(t),
            move |t| f2.lock().unwrap().push(t),
        )
        .expect("run");
        assert!(!finals.lock().unwrap().is_empty(), "应至少有一个 final");
        assert!(finals.lock().unwrap().iter().all(|s| s.starts_with("len=")));
        assert!(!partials.lock().unwrap().is_empty(), "应至少触发一次 partial");
    }
}
