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
            // 目标：打印采样率/声道/缓冲个数/首缓冲字节数，判定 f32 与 planar/interleaved。
            // NOTE: format_description() is on CMSampleBuffer directly (apple_cf);
            //       audio_buffer_list() is from CMSampleBufferExt (in prelude).
            let fmt = sample.format_description();
            let list = sample.audio_buffer_list();
            let _ = self
                .0
                .send(format!("format_description={fmt:?} | audio_buffer_list={list:?}"));
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

    let mut n = 0;
    while let Ok(msg) = rx.recv_timeout(Duration::from_secs(2)) {
        println!("AUDIO#{n}: {msg}");
        n += 1;
        if n >= 20 {
            break;
        }
    }
    stream.stop_capture().ok();
    assert!(n > 0, "未收到系统音频回调——检查屏幕录制授权与是否有声音在播放");
}
