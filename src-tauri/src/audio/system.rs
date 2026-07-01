//! macOS 系统声音采集（ScreenCaptureKit）。本文件仅 macOS 编译。
//! Task 2 先放纯函数 planar_to_mono；Task 3 加 SystemAudioCapture。

/// 把多个声道平面（planar：每声道一段等长 f32）按样本平均成单声道。
/// 空输入 → 空；单声道 → 克隆；多声道以最短声道长度为准，避免越界。
pub fn planar_to_mono(channels: &[Vec<f32>]) -> Vec<f32> {
    match channels.len() {
        0 => Vec::new(),
        1 => channels[0].clone(),
        n => {
            let len = channels.iter().map(|c| c.len()).min().unwrap_or(0);
            (0..len)
                .map(|i| channels.iter().map(|c| c[i]).sum::<f32>() / n as f32)
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planar_stereo_averages_per_sample() {
        let ch = vec![vec![1.0, 3.0, 5.0], vec![3.0, 5.0, 7.0]];
        assert_eq!(planar_to_mono(&ch), vec![2.0, 4.0, 6.0]);
    }

    #[test]
    fn planar_empty_and_mono() {
        assert_eq!(planar_to_mono(&[]), Vec::<f32>::new());
        assert_eq!(planar_to_mono(&[vec![0.1, 0.2]]), vec![0.1, 0.2]);
    }

    #[test]
    fn planar_uses_shortest_channel_len() {
        let ch = vec![vec![2.0, 4.0], vec![6.0]];
        assert_eq!(planar_to_mono(&ch), vec![4.0]); // (2+6)/2；第二样本因越界被裁掉
    }
}
