use super::segmenter::{Segment, Segmenter};

/// 云端模式的"分段器":不断句(厂商服务端 VAD 负责),只把前处理后的帧转发给
/// run_cloud_asr_worker。借 Segmenter 之形复用 run_segment_worker 全部逻辑
/// (AEC/暂停/电平),segment_worker 零改动。
pub struct CloudForwarder {
    tx: crossbeam_channel::Sender<Vec<f32>>,
}

impl CloudForwarder {
    pub fn new(tx: crossbeam_channel::Sender<Vec<f32>>) -> Self {
        Self { tx }
    }
}

impl Segmenter for CloudForwarder {
    fn accept(&mut self, samples: &[f32]) {
        // 与 silero.rs accept 同款消毒:NaN/Inf 会污染厂商解码(其 s16 转换回绕)。
        let clean: Vec<f32> = samples
            .iter()
            .map(|&x| if x.is_finite() { x } else { 0.0 })
            .collect();
        let _ = self.tx.send(clean); // 接收端关闭 = 会话收尾,静默丢弃
    }
    fn take_finished(&mut self) -> Vec<Segment> {
        Vec::new()
    }
    fn current_partial(&mut self) -> Option<Vec<f32>> {
        None
    }
    fn flush(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwards_sanitized_frames_and_never_yields_segments() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut f = CloudForwarder::new(tx);
        f.accept(&[0.5, f32::NAN, f32::INFINITY]);
        assert_eq!(
            rx.recv().unwrap(),
            vec![0.5, 0.0, 0.0],
            "非有限值消毒(同 silero 入口)"
        );
        assert!(f.take_finished().is_empty());
        assert!(f.current_partial().is_none());
        f.flush(); // 不 panic 即可
    }
}
