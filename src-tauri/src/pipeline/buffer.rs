/// 累积 16kHz 单声道样本。每累计满 window_secs，就返回"从录制开始到当前"的累积快照，
/// 用于快流：对当前累积段重复识别、实时刷新临时文本。
pub struct AccumulatingBuffer {
    samples: Vec<f32>,
    window_len: usize,
    next_emit_at: usize,
}

impl AccumulatingBuffer {
    pub fn new(sample_rate: u32, window_secs: f32) -> Self {
        let window_len = (sample_rate as f32 * window_secs).round() as usize;
        Self { samples: Vec::new(), window_len: window_len.max(1), next_emit_at: window_len.max(1) }
    }

    pub fn push(&mut self, samples: &[f32]) -> Option<Vec<f32>> {
        self.samples.extend_from_slice(samples);
        if self.samples.len() >= self.next_emit_at {
            self.next_emit_at += self.window_len;
            Some(self.samples.clone())
        } else {
            None
        }
    }

    pub fn drain(&mut self) -> Vec<f32> {
        std::mem::take(&mut self.samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::*;

    #[test]
    fn emits_when_window_reached() {
        // 16kHz, 1 秒窗口 => 16000 样本
        let mut buf = AccumulatingBuffer::new(16000, 1.0);
        assert!(buf.push(&vec![0.0; 8000]).is_none(), "不足一窗不应吐出");
        let out = buf.push(&vec![0.0; 8000]).expect("应吐出一窗");
        assert_eq!(out.len(), 16000);
    }

    #[test]
    fn window_grows_across_pushes() {
        // 第二窗应包含累积的全部历史（快流：每次识别"当前累积段"）
        let mut buf = AccumulatingBuffer::new(16000, 1.0);
        buf.push(&vec![1.0; 16000]); // 第 1 窗
        let out = buf.push(&vec![2.0; 16000]).expect("第 2 窗");
        assert_eq!(out.len(), 32000, "第二窗应是累积长度");
    }

    #[test]
    fn drain_returns_remainder() {
        let mut buf = AccumulatingBuffer::new(16000, 1.0);
        buf.push(&vec![0.0; 5000]);
        assert_eq!(buf.drain().len(), 5000);
    }
}
