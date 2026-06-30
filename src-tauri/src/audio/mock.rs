use super::{AudioCapture, AudioFrame};
use crossbeam_channel::Sender;

/// 测试用采集源：把一个 WAV 一次性按帧发出后结束。
pub struct MockCapture {
    frames: Vec<AudioFrame>,
}

impl MockCapture {
    pub fn from_wav(path: &str) -> anyhow::Result<Self> {
        let mut reader = hound::WavReader::open(path)?;
        let spec = reader.spec();
        let samples: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Float => reader.samples::<f32>().map(|s| s.unwrap()).collect(),
            hound::SampleFormat::Int => {
                reader.samples::<i16>().map(|s| s.unwrap() as f32 / 32768.0).collect()
            }
        };
        // 切成 ~100ms 的帧，模拟真实采集节奏
        let frame_len = (spec.sample_rate as usize / 10) * spec.channels as usize;
        let frames = samples
            .chunks(frame_len.max(1))
            .map(|c| AudioFrame {
                samples: c.to_vec(),
                sample_rate: spec.sample_rate,
                channels: spec.channels,
            })
            .collect();
        Ok(Self { frames })
    }
}

impl AudioCapture for MockCapture {
    fn start(&mut self, sink: Sender<AudioFrame>) -> anyhow::Result<()> {
        for f in self.frames.drain(..) {
            let _ = sink.send(f);
        }
        Ok(()) // 发完即返回，sink 被 drop 后接收端结束
    }
    fn stop(&mut self) {}
}
