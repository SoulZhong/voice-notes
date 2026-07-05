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

/// 段长硬上限(样本数,16kHz × 15s),与 config.max_speech_duration 对齐。
/// 该配置一路透传 sherpa,但当前版本实测不强制(冒烟见 36s 超长段照常产出),
/// 故在分段器出口自兜底:超长段按此硬切。混说场景一段多人,段越长声纹污染越重,
/// 识别也劣化——上限必须真实生效。
const MAX_SEGMENT_SAMPLES: usize = 15 * 16000;

/// 超长段按 MAX_SEGMENT_SAMPLES 硬切,子段 start 顺延样本偏移(时间轴连续)。
fn split_long(samples: Vec<f32>, start: usize) -> Vec<Segment> {
    if samples.len() <= MAX_SEGMENT_SAMPLES {
        return vec![Segment { samples, start }];
    }
    let mut out = Vec::new();
    let mut off = 0;
    while samples.len() - off > MAX_SEGMENT_SAMPLES {
        out.push(Segment {
            samples: samples[off..off + MAX_SEGMENT_SAMPLES].to_vec(),
            start: start + off,
        });
        off += MAX_SEGMENT_SAMPLES;
    }
    out.push(Segment { samples: samples[off..].to_vec(), start: start + off });
    out
}

impl Segmenter for SileroSegmenter {
    fn accept(&mut self, samples: &[f32]) {
        self.vad.accept_waveform(samples.to_vec());
        if self.vad.is_speech() {
            self.current.extend_from_slice(samples);
        } else {
            // 静音期清空预览缓冲：避免噪声导致 is_speech 抖动却不成段时，
            // current 里残留过时片段被当成 partial 显示。
            self.current.clear();
        }
    }

    fn take_finished(&mut self) -> Vec<Segment> {
        let mut out = Vec::new();
        while !self.vad.is_empty() {
            let seg = self.vad.front();
            out.extend(split_long(seg.samples, seg.start.max(0) as usize));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::segmenter::Segmenter;

    /// 硬切纯逻辑:边界内不切、超长切块、尾块与时间轴偏移正确。
    #[test]
    fn split_long_caps_segment_length_and_keeps_offsets() {
        // 恰好上限:原样一段
        let one = split_long(vec![0.0; MAX_SEGMENT_SAMPLES], 100);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].samples.len(), MAX_SEGMENT_SAMPLES);
        assert_eq!(one[0].start, 100);
        // 2.5 倍上限:三块,偏移顺延,总样本无增无减
        let n = MAX_SEGMENT_SAMPLES * 5 / 2;
        let parts = split_long(vec![0.0; n], 1000);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].start, 1000);
        assert_eq!(parts[1].start, 1000 + MAX_SEGMENT_SAMPLES);
        assert_eq!(parts[2].start, 1000 + 2 * MAX_SEGMENT_SAMPLES);
        assert_eq!(parts[2].samples.len(), n - 2 * MAX_SEGMENT_SAMPLES);
        assert_eq!(parts.iter().map(|p| p.samples.len()).sum::<usize>(), n);
    }

    /// 暂停功能依赖：flush 之后继续 accept，段的 start 样本偏移必须延续而非归零。
    /// 需要真实模型：cargo test -- --ignored（或 VN_MODELS 指向模型目录）。
    #[test]
    #[ignore]
    fn flush_midstream_keeps_timeline_monotonic() {
        let model = crate::models::root().join("silero_vad.onnx");
        let mut seg = SileroSegmenter::new(&model).expect("加载 VAD");
        let wav = {
            let mut r = hound::WavReader::open(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/sample_16k.wav"
            ))
            .expect("fixture");
            r.samples::<i16>().map(|s| s.unwrap() as f32 / 32768.0).collect::<Vec<f32>>()
        };
        seg.accept(&wav);
        seg.flush();
        let a = seg.take_finished();
        assert!(!a.is_empty(), "fixture 是真实语音，flush 应产段");
        seg.accept(&wav);
        seg.flush();
        let b = seg.take_finished();
        assert!(!b.is_empty());
        let last_a = a.last().unwrap();
        assert!(
            b[0].start >= last_a.start + last_a.samples.len(),
            "flush 后时间轴延续不重叠: b.start={} vs a.end={}",
            b[0].start,
            last_a.start + last_a.samples.len()
        );
    }
}
