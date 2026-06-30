use crate::asr::Recognizer;
use crate::audio::{resample::resample_linear, to_mono, AudioCapture};
use crate::pipeline::segmenter::Segmenter;
use crossbeam_channel::bounded;

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
    }
}
