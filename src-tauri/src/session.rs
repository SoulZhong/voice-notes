use crate::asr::Recognizer;
use crate::audio::{resample::resample_linear, to_mono, AudioCapture};
use crate::pipeline::buffer::AccumulatingBuffer;
use crossbeam_channel::bounded;

/// 录制管线核心：从 capture 取帧，归一到 target_rate 单声道，累积成窗，
/// 每窗调用 recognizer，并通过 on_partial 回调发出临时文本。
/// 同步运行直到 capture 的发送端关闭。
pub fn run_pipeline(
    mut capture: Box<dyn AudioCapture>,
    mut recognizer: Box<dyn Recognizer>,
    target_rate: u32,
    window_secs: f32,
    mut on_partial: impl FnMut(String),
) -> anyhow::Result<()> {
    let (tx, rx) = bounded::<crate::audio::AudioFrame>(256);
    capture.start(tx)?;

    // 用立即执行闭包包住主循环，确保无论成功还是识别出错，都执行 capture.stop()。
    let mut buf = AccumulatingBuffer::new(target_rate, window_secs);
    let result = (|| -> anyhow::Result<()> {
        for frame in rx.iter() {
            let mono = to_mono(&frame.samples, frame.channels);
            let resampled = resample_linear(&mono, frame.sample_rate, target_rate);
            if let Some(window) = buf.push(&resampled) {
                let t = recognizer.recognize(&window)?;
                on_partial(t.text);
            }
        }
        // 收尾：剩余不足一窗的也识别一次
        let rest = buf.drain();
        if !rest.is_empty() {
            let t = recognizer.recognize(&rest)?;
            on_partial(t.text);
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
    use std::sync::{Arc, Mutex};

    /// 假识别器：返回收到的样本数，便于断言管线确实送来了归一化音频。
    struct CountingRecognizer;
    impl Recognizer for CountingRecognizer {
        fn recognize(&mut self, samples: &[f32]) -> anyhow::Result<Transcript> {
            Ok(Transcript { text: format!("len={}", samples.len()) })
        }
    }

    #[test]
    fn pipeline_emits_partials_from_wav() {
        let capture = Box::new(
            MockCapture::from_wav(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/sample_16k.wav"
            ))
            .expect("读取 fixture"),
        );
        let collected = Arc::new(Mutex::new(Vec::<String>::new()));
        let c2 = collected.clone();
        run_pipeline(capture, Box::new(CountingRecognizer), 16000, 1.0, move |t| {
            c2.lock().unwrap().push(t)
        })
        .expect("管线运行");
        let got = collected.lock().unwrap();
        assert!(!got.is_empty(), "应至少发出一次 partial");
        assert!(got.last().unwrap().starts_with("len="));
    }
}
