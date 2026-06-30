use super::segmenter::{Segment, Segmenter};
use std::path::Path;

/// 基于 sherpa-onnx Silero VAD 的语句分段器。
/// 内部维护"当前句"缓冲：只在说话时累积，VAD 切出完整段时清空，用于实时 partial。
pub struct SileroSegmenter {
    vad: sherpa_rs::silero_vad::SileroVad,
    current: Vec<f32>,
}

impl SileroSegmenter {
    pub fn new(model_path: &Path) -> anyhow::Result<Self> {
        let config = sherpa_rs::silero_vad::SileroVadConfig {
            model: model_path.to_string_lossy().into_owned(),
            min_silence_duration: 0.6, // 静音 > 0.6s 视为一句结束
            min_speech_duration: 0.25,
            max_speech_duration: 15.0, // 上限：超 15s 强制切，界定每次识别量
            threshold: 0.5,
            sample_rate: 16000,
            window_size: 512,
            num_threads: Some(1),
            ..Default::default()
        };
        // buffer_size_in_seconds：内部环形缓冲容量，给足
        let vad = sherpa_rs::silero_vad::SileroVad::new(config, 30.0)
            .map_err(|e| anyhow::anyhow!("加载 Silero VAD 失败: {e}"))?;
        Ok(Self { vad, current: Vec::new() })
    }
}

impl Segmenter for SileroSegmenter {
    fn accept(&mut self, samples: &[f32]) {
        self.vad.accept_waveform(samples.to_vec());
        if self.vad.is_speech() {
            self.current.extend_from_slice(samples);
        }
    }

    fn take_finished(&mut self) -> Vec<Segment> {
        let mut out = Vec::new();
        while !self.vad.is_empty() {
            let seg = self.vad.front();
            out.push(Segment { samples: seg.samples });
            self.vad.pop();
        }
        if !out.is_empty() {
            // 已完成的语句对应的"当前句"已结束，清空预览缓冲。
            self.current.clear();
        }
        out
    }

    fn current_partial(&mut self) -> Option<Vec<f32>> {
        if self.current.is_empty() { None } else { Some(self.current.clone()) }
    }

    fn flush(&mut self) {
        self.vad.flush();
        self.current.clear();
    }
}
