/// 一个已完成的语句音频段（16kHz 单声道 f32）。
#[derive(Debug, Clone)]
pub struct Segment {
    pub samples: Vec<f32>,
}

/// 语句分段器：吃入音频流，切出完整语句，并提供当前未定稿语句用于实时 partial。
/// 真实实现用 Silero VAD；测试用 MockSegmenter。
pub trait Segmenter: Send {
    /// 喂入一块 16kHz 单声道样本。
    fn accept(&mut self, samples: &[f32]);
    /// 取出自上次调用以来已完成的语句（可能为空）。
    fn take_finished(&mut self) -> Vec<Segment>;
    /// 当前正在说、尚未定稿的语句音频；静音/无内容时返回 None。
    fn current_partial(&mut self) -> Option<Vec<f32>>;
    /// 收尾：把尾部残留语句也切成完成段（录制结束时调用）。
    fn flush(&mut self);
}

/// 测试用：每累计 utterance_len 个样本切一段，不依赖模型。
pub struct MockSegmenter {
    utterance_len: usize,
    current: Vec<f32>,
    finished: Vec<Segment>,
}

impl MockSegmenter {
    pub fn new(utterance_len: usize) -> Self {
        Self { utterance_len: utterance_len.max(1), current: Vec::new(), finished: Vec::new() }
    }
}

impl Segmenter for MockSegmenter {
    fn accept(&mut self, samples: &[f32]) {
        self.current.extend_from_slice(samples);
        while self.current.len() >= self.utterance_len {
            let rest = self.current.split_off(self.utterance_len);
            let seg = std::mem::replace(&mut self.current, rest);
            self.finished.push(Segment { samples: seg });
        }
    }
    fn take_finished(&mut self) -> Vec<Segment> {
        std::mem::take(&mut self.finished)
    }
    fn current_partial(&mut self) -> Option<Vec<f32>> {
        if self.current.is_empty() { None } else { Some(self.current.clone()) }
    }
    fn flush(&mut self) {
        if !self.current.is_empty() {
            self.finished.push(Segment { samples: std::mem::take(&mut self.current) });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_emits_segment_per_utterance_len() {
        let mut s = MockSegmenter::new(100);
        s.accept(&vec![0.0; 60]);
        assert!(s.take_finished().is_empty(), "不足一段");
        assert_eq!(s.current_partial().map(|v| v.len()), Some(60));
        s.accept(&vec![0.0; 50]); // 累计 110 >= 100
        let segs = s.take_finished();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].samples.len(), 100);
        // 段产出后，剩余 10 作为当前句
        assert_eq!(s.current_partial().map(|v| v.len()), Some(10));
    }

    #[test]
    fn mock_flush_emits_remainder() {
        let mut s = MockSegmenter::new(100);
        s.accept(&vec![0.0; 30]);
        s.flush();
        let segs = s.take_finished();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].samples.len(), 30);
        assert!(s.current_partial().is_none(), "flush 后无当前句");
    }
}
