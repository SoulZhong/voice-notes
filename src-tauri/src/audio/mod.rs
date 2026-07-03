pub mod resample;
pub mod mock;
pub mod microphone;
#[cfg(target_os = "macos")]
pub mod system;
#[cfg(target_os = "macos")]
pub mod vpio;

use crossbeam_channel::Sender;

/// 一帧原始音频，来自采集设备的原生格式。
#[derive(Debug, Clone)]
pub struct AudioFrame {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: u16,
}

/// 音频来源标记：接线时确定，随 Job/事件流转。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Mic,
    System,
}

impl Source {
    /// IPC 事件里用的稳定字符串。
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::Mic => "mic",
            Source::System => "system",
        }
    }
}

/// 音频采集源的统一接口。后续计划新增系统声音 / 其他平台时实现本 trait。
pub trait AudioCapture: Send {
    /// 开始采集；每采到一块就通过 sink 发出一帧。非阻塞。
    fn start(&mut self, sink: Sender<AudioFrame>) -> anyhow::Result<()>;
    /// 停止采集并释放设备。
    fn stop(&mut self);
}

/// 交错多声道 -> 单声道（按帧平均各声道）。
pub fn to_mono(samples: &[f32], channels: u16) -> Vec<f32> {
    if channels <= 1 {
        return samples.to_vec();
    }
    let ch = channels as usize;
    samples
        .chunks(ch)
        .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_mono_averages_stereo_pairs() {
        // 交错立体声: L0,R0, L1,R1
        let stereo = vec![0.0, 1.0, 0.5, -0.5];
        let mono = to_mono(&stereo, 2);
        assert_eq!(mono, vec![0.5, 0.0]);
    }

    #[test]
    fn to_mono_passthrough_for_mono() {
        let m = vec![0.1, 0.2, 0.3];
        assert_eq!(to_mono(&m, 1), m);
    }

    #[test]
    fn source_as_str_maps_to_ipc_strings() {
        assert_eq!(Source::Mic.as_str(), "mic");
        assert_eq!(Source::System.as_str(), "system");
    }
}
