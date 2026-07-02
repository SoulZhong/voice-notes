//! 系统声音采集 spike：手动运行，验证能否拿到系统音频回调并打印其格式。
//! 运行：cd src-tauri && cargo test --test sckit_probe -- --ignored --nocapture
//! 前置：系统设置授予本终端/应用"屏幕录制"权限；并在别处播放声音。
#![cfg(target_os = "macos")]

use screencapturekit::prelude::*;
use std::sync::mpsc;
use std::time::Duration;

#[test]
#[ignore = "manual: 需屏幕录制授权 + 正在播放的系统声音"]
fn probe_system_audio() {
    let (tx, rx) = mpsc::channel::<String>();

    struct Handler(mpsc::Sender<String>);
    impl SCStreamOutputTrait for Handler {
        fn did_output_sample_buffer(&self, sample: CMSampleBuffer, _t: SCStreamOutputType) {
            // 目标：实锤 T3 extract_audio_mono 的全部假设——
            // 采样率、每 buffer 声道数/字节数、样本按 f32-LE 解码是否合理([-1,1])。
            let rate = sample
                .format_description()
                .and_then(|f| f.audio_sample_rate());
            let mut desc = format!("rate={rate:?}");
            if let Some(list) = sample.audio_buffer_list() {
                desc += &format!(" | num_buffers={}", list.num_buffers());
                for (i, buf) in list.iter().enumerate() {
                    let data = buf.data();
                    // 按 f32-LE 解前 3 个样本（与 audio/system.rs 的 bytes_to_f32 同法）
                    let head: Vec<f32> = data
                        .chunks_exact(4)
                        .take(3)
                        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                        .collect();
                    let peak = data
                        .chunks_exact(4)
                        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]).abs())
                        .fold(0.0f32, f32::max);
                    desc += &format!(
                        " | buf{i}: ch={} bytes={} head={head:?} peak={peak:.4}",
                        buf.number_channels,
                        data.len()
                    );
                }
            } else {
                desc += " | audio_buffer_list=None";
            }
            let _ = self.0.send(desc);
        }
    }

    let content = SCShareableContent::get().expect("需要屏幕录制授权");
    let display = &content.displays()[0];
    let filter = SCContentFilter::create()
        .with_display(display)
        .with_excluding_windows(&[])
        .build();
    let config = SCStreamConfiguration::new()
        .with_width(2)
        .with_height(2)
        .with_captures_audio(true)
        .with_sample_rate(48_000)
        .with_channel_count(2);

    let mut stream = SCStream::new(&filter, &config);
    stream.add_output_handler(Handler(tx), SCStreamOutputType::Audio);
    stream.start_capture().expect("start_capture 失败");

    // 采集最多 ~6 秒（300 × 20ms）：静音帧只打印前 3 帧，之后只打印非零峰值帧，
    // 逮到 3 帧有声帧即可提前结束——足以实锤 f32-LE 解码合理性。
    let mut n = 0;
    let mut loud = 0;
    while let Ok(msg) = rx.recv_timeout(Duration::from_secs(2)) {
        let has_sound = !msg.contains("peak=0.0000");
        if n < 3 || has_sound {
            println!("AUDIO#{n}: {msg}");
        }
        if has_sound {
            loud += 1;
            if loud >= 3 {
                break;
            }
        }
        n += 1;
        if n >= 300 {
            break;
        }
    }
    stream.stop_capture().ok();
    println!("共收到 {n} 帧，其中有声帧 {loud} 帧");
    assert!(n > 0, "未收到系统音频回调——检查屏幕录制授权与是否有声音在播放");
}
