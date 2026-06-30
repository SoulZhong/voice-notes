use super::{AudioCapture, AudioFrame};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use crossbeam_channel::Sender;

/// Real microphone capture via cpal.
///
/// `cpal::Stream` is `!Send`, so we cannot store it directly in `Microphone`
/// when `AudioCapture: Send` is required. Instead we own the stream on a
/// dedicated background thread and communicate via a stop-channel.
pub struct Microphone {
    /// Dropping this sender signals the background thread to stop the stream.
    stop_tx: Option<crossbeam_channel::Sender<()>>,
}

impl Microphone {
    pub fn new() -> Self {
        Self { stop_tx: None }
    }
}

impl AudioCapture for Microphone {
    fn start(&mut self, sink: Sender<AudioFrame>) -> anyhow::Result<()> {
        // --- device & config (validated synchronously before we spawn) ---
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| anyhow::anyhow!("找不到默认麦克风"))?;
        let supported = device.default_input_config()?;
        let sample_rate = supported.sample_rate().0;
        let channels = supported.channels();

        // Guard: only F32 is supported in this skeleton.
        // If the device delivers a different format, return an error rather
        // than silently mis-reading samples.
        if supported.sample_format() != SampleFormat::F32 {
            return Err(anyhow::anyhow!(
                "麦克风格式不支持: {}，当前骨架仅支持 f32 格式",
                supported.sample_format()
            ));
        }

        let stream_config: cpal::StreamConfig = supported.into();

        // --- stop-channel ---
        let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(1);

        // --- background thread owns the !Send stream ---
        std::thread::spawn(move || {
            let err_fn = |e: cpal::StreamError| eprintln!("麦克风流错误: {e}");
            let stream = match device.build_input_stream(
                &stream_config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    let _ = sink.send(AudioFrame {
                        samples: data.to_vec(),
                        sample_rate,
                        channels,
                    });
                },
                err_fn,
                None, // no timeout
            ) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("无法建立麦克风流: {e}");
                    return;
                }
            };
            if let Err(e) = stream.play() {
                eprintln!("无法启动麦克风流: {e}");
                return;
            }
            // Block until stop_tx is dropped (stop() called) or explicitly signalled.
            stop_rx.recv().ok();
            // `stream` drops here, stopping capture.
        });

        self.stop_tx = Some(stop_tx);
        Ok(())
    }

    fn stop(&mut self) {
        // Dropping the sender closes the channel, unblocking the background thread.
        self.stop_tx = None;
    }
}
