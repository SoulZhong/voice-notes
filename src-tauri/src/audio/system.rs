//! macOS 系统声音采集（ScreenCaptureKit）。本文件仅 macOS 编译。
//! Task 2 先放纯函数 planar_to_mono；Task 3 加 SystemAudioCapture。

use super::{AudioCapture, AudioFrame};
use crossbeam_channel::Sender;
use screencapturekit::prelude::*;

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

// ---------------------------------------------------------------------------
// bytes_to_f32 — pure helper (testable without a device)
// ---------------------------------------------------------------------------

/// 把原始字节（IEEE 754 float32 little-endian，4 字节 / 样本）转为 f32 切片。
/// 不足 4 字节的尾部数据静默丢弃。
///
/// ScreenCaptureKit 在 macOS 13+ 的音频格式为 LPCM f32 LE（kAudioFormatFlagIsFloat，
/// 无 kAudioFormatFlagIsBigEndian），本函数依此假设。若格式不匹配须在 smoke 时修正。
fn bytes_to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

// ---------------------------------------------------------------------------
// CMSampleBuffer helpers
// ---------------------------------------------------------------------------

/// 从 CMSampleBuffer 的格式描述取采样率（Hz）。
///
/// 调用链：CMSampleBuffer::format_description() → CMFormatDescription::audio_sample_rate()
/// 若 format_description() 返回 None（某些 macOS 版本在音频 buffer 上有此行为），
/// 或 audio_sample_rate() 返回 None，则返回 None；调用方回退到 48 000。
fn audio_sample_rate(sample: &CMSampleBuffer) -> Option<u32> {
    let rate = sample.format_description()?.audio_sample_rate()?;
    if rate > 0.0 {
        Some(rate as u32)
    } else {
        None
    }
}

/// 从 CMSampleBuffer 中提取单声道 f32 样本。
///
/// 支持两种 AudioBufferList 布局（运行时自动判断）：
///
/// **PLANAR**（>1 个 AudioBuffer，每个 buffer 的 number_channels == 1）：
///   每个 buffer 作为一个独立声道平面，通过 `planar_to_mono` 平均混缩为单声道。
///
/// **INTERLEAVED**（1 个 AudioBuffer，number_channels > 1）：
///   交错多声道数据，通过 `super::to_mono` 按帧平均混缩为单声道。
///
/// **MONO**（1 个 AudioBuffer，number_channels == 1）：
///   直接返回样本，无需混缩。
///
/// 上述假设待 smoke 测试确认。若 buffer_list 不可用、为空、或格式无法解析，
/// 静默返回空 Vec（不 panic，不发帧）。
fn extract_audio_mono(sample: &CMSampleBuffer) -> Vec<f32> {
    let abl = match sample.audio_buffer_list() {
        Some(l) => l,
        None => return Vec::new(),
    };

    let n = abl.num_buffers();
    if n == 0 {
        return Vec::new();
    }

    if n > 1 {
        // PLANAR：多个 buffer，每个 buffer 为一个声道平面
        let planes: Vec<Vec<f32>> = abl.iter().map(|buf| bytes_to_f32(buf.data())).collect();
        planar_to_mono(&planes)
    } else {
        // 单个 buffer：可能是 mono 也可能是 interleaved
        let buf = match abl.get(0) {
            Some(b) => b,
            None => return Vec::new(),
        };
        let channels = buf.number_channels;
        let samples = bytes_to_f32(buf.data());
        if channels <= 1 {
            samples
        } else {
            // INTERLEAVED：调用 super::to_mono 做帧内平均
            super::to_mono(&samples, channels as u16)
        }
    }
}

// ---------------------------------------------------------------------------
// SystemAudioCapture
// ---------------------------------------------------------------------------

/// ScreenCaptureKit 系统声音采集。
///
/// 使用 SCKit 捕获系统音频（macOS 13+，需要屏幕录制权限）。
///
/// SCKit 类型（SCContentFilter、SCStreamConfiguration、SCStream）均实现了
/// `Send + Sync`（见 crate 源码中的 `unsafe impl`），所以可以跨线程传递。
/// 但为了与 microphone.rs 保持一致的线程/握手/停止惯用法，流的持有和
/// start_capture / stop_capture 仍在后台线程执行，避免阻塞调用方。
///
/// 错误分类：
/// - 屏幕录制未授权 → `Err`，`to_string()` 以 `"unauthorized:"` 开头
/// - 其他启动失败  → `Err`，`to_string()` 以 `"unavailable:"` 开头
pub struct SystemAudioCapture {
    /// 持有此 sender 期间后台线程保持运行；drop 即发出停止信号。
    stop_tx: Option<crossbeam_channel::Sender<()>>,
}

impl SystemAudioCapture {
    pub fn new() -> Self {
        Self { stop_tx: None }
    }
}

impl AudioCapture for SystemAudioCapture {
    fn start(&mut self, sink: Sender<AudioFrame>) -> anyhow::Result<()> {
        // --- 权限检查 / 内容枚举（同步，阻塞约 <1 s）-----------------------
        // SCShareableContent::get() 在未授权时返回 Err(SCError)；这是最常见的
        // 初始化失败原因，单独分类为 "unauthorized:" 以便上层按需降级。
        let content = SCShareableContent::get().map_err(|e| {
            anyhow::anyhow!("unauthorized: 无法枚举可共享内容（未授权屏幕录制？）: {e}")
        })?;

        let display = content
            .displays()
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("unavailable: 未找到可采集的显示器"))?;

        // --- 构建 filter 和 config -------------------------------------------
        // with_width(2) / with_height(2)：最小帧尺寸；我们只要音频，画面尽量小。
        // with_captures_audio(true)：开启系统音频采集。
        // with_sample_rate(48_000) / with_channel_count(2)：请求 48 kHz 立体声；
        // 实际采样率由格式描述确认（smoke 时验证）。
        let filter = SCContentFilter::create()
            .with_display(&display)
            .with_excluding_windows(&[])
            .build();

        let config = SCStreamConfiguration::new()
            .with_width(2_u32)
            .with_height(2_u32)
            .with_captures_audio(true)
            .with_sample_rate(48_000_i32)
            .with_channel_count(2_i32);

        // --- 停止通道 + 就绪通道 -------------------------------------------
        let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(0);
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<(), String>>(1);

        // --- 后台线程：持有 SCStream，驱动采集生命周期 ----------------------
        std::thread::spawn(move || {
            let mut stream = SCStream::new(&filter, &config);

            // 音频输出处理器（实现 SCStreamOutputTrait）
            struct AudioSink {
                tx: Sender<AudioFrame>,
            }

            impl SCStreamOutputTrait for AudioSink {
                fn did_output_sample_buffer(
                    &self,
                    sample: CMSampleBuffer,
                    of_type: SCStreamOutputType,
                ) {
                    // 只处理 Audio 类型回调；Screen 帧（即使我们设置了极小分辨率）忽略。
                    if of_type != SCStreamOutputType::Audio {
                        return;
                    }
                    let sample_rate = audio_sample_rate(&sample).unwrap_or(48_000);
                    let mono = extract_audio_mono(&sample);
                    if !mono.is_empty() {
                        // sink 断开时 send 会失败，忽略即可。
                        let _ = self.tx.send(AudioFrame {
                            samples: mono,
                            sample_rate,
                            channels: 1,
                        });
                    }
                }
            }

            stream.add_output_handler(AudioSink { tx: sink }, SCStreamOutputType::Audio);

            // start_capture 是阻塞式（等待 Swift 异步完成）
            if let Err(e) = stream.start_capture() {
                let _ = ready_tx
                    .send(Err(format!("unavailable: 无法启动系统声音流: {e}")));
                return;
            }

            // 通知 start() 流已成功开启，可以安全返回。
            let _ = ready_tx.send(Ok(()));

            // 阻塞等待 stop_tx 被 drop（即 stop() 被调用）。
            stop_rx.recv().ok();

            // stop_capture 是阻塞式；忽略错误（流可能已因权限撤销等原因停止）。
            stream.stop_capture().ok();
            // stream 在此 drop，释放 SCStream 资源。
        });

        // --- 等待后台线程确认流状态 -----------------------------------------
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(anyhow::anyhow!(e)),
            Err(_) => return Err(anyhow::anyhow!("unavailable: 系统声音线程意外退出")),
        }

        self.stop_tx = Some(stop_tx);
        Ok(())
    }

    fn stop(&mut self) {
        // drop sender → channel 断开 → 后台 recv() 返回 Err → 线程退出
        self.stop_tx = None;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- planar_to_mono (Task 2, 保留) ------------------------------------

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

    // --- bytes_to_f32 (Task 3 新增纯函数测试) ------------------------------

    #[test]
    fn bytes_to_f32_converts_le_bytes() {
        // 1.0_f32 in IEEE 754 LE = [0x00, 0x00, 0x80, 0x3F]
        let bytes = 1.0_f32.to_le_bytes();
        let result = bytes_to_f32(&bytes);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], 1.0_f32);
    }

    #[test]
    fn bytes_to_f32_multiple_samples() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0.5_f32.to_le_bytes());
        bytes.extend_from_slice(&(-1.0_f32).to_le_bytes());
        bytes.extend_from_slice(&0.0_f32.to_le_bytes());
        let result = bytes_to_f32(&bytes);
        assert_eq!(result, vec![0.5, -1.0, 0.0]);
    }

    #[test]
    fn bytes_to_f32_truncates_trailing_partial() {
        // 5 bytes: one complete f32 + 1 trailing byte → only 1 sample
        let mut bytes = 42.0_f32.to_le_bytes().to_vec();
        bytes.push(0xFF); // trailing byte, not a full f32
        let result = bytes_to_f32(&bytes);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], 42.0_f32);
    }

    #[test]
    fn bytes_to_f32_empty_input() {
        assert_eq!(bytes_to_f32(&[]), Vec::<f32>::new());
    }
}
