use super::{AudioCapture, AudioFrame, CaptureEvent};
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
    /// 运行期流错误上报口(断连自愈消费);未接线时仅落日志,行为同引入前。
    events: Option<Sender<CaptureEvent>>,
    /// 回调因下游积压丢弃的样本数(每声道,累计)。仅供停流时汇总日志;
    /// 时间轴的修补由 tap 按硬件时戳负责(见 frame_tap 的 holey 判定)。
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Microphone {
    pub fn new() -> Self {
        Self { stop_tx: None, events: None, dropped: Default::default() }
    }

    /// 带流错误上报的构造:err_fn 触发时把错误升格为 CaptureEvent(仍保留日志)。
    pub fn with_events(events: Sender<CaptureEvent>) -> Self {
        Self { stop_tx: None, events: Some(events), dropped: Default::default() }
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

        // --- stop-channel (signal-only: never sends, drop = disconnect) ---
        let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(0);
        // --- ready-channel: background thread reports whether the stream opened ---
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<(), String>>(1);

        // --- background thread owns the !Send stream ---
        let events = self.events.clone();
        let dropped = self.dropped.clone();
        std::thread::spawn(move || {
            let err_fn = move |e: cpal::StreamError| {
                eprintln!("麦克风流错误: {e}");
                if let Some(tx) = &events {
                    let _ = tx.send(CaptureEvent::Error(e.to_string()));
                }
            };
            // 首个回调锚定 (capture StreamInstant, host_time::now_ns())，此后用
            // capture.duration_since(anchor) 做差换算——StreamInstant 字段私有，
            // 只能做差，不能直接取绝对值。anchor_ns 是回调进入时刻，与硬件时刻
            // 差一个缓冲时长量级的常量偏移，不影响斜率(ppm)；绝对偏移由 E1 互相关标定。
            let mut anchor: Option<(cpal::StreamInstant, u64)> = None;
            let dropped_cb = dropped.clone();
            let stream = match device.build_input_stream(
                &stream_config,
                move |data: &[f32], info: &cpal::InputCallbackInfo| {
                    let dropped = &dropped_cb;
                    let cap = info.timestamp().capture;
                    let host_time_ns = match anchor {
                        None => {
                            let now = crate::audio::host_time::now_ns();
                            anchor = Some((cap, now));
                            Some(now)
                        }
                        Some((a_inst, a_ns)) => {
                            cap.duration_since(&a_inst).map(|d| a_ns + d.as_nanos() as u64)
                        }
                    };
                    let per_ch = data.len() / channels.max(1) as usize;
                    // 绝不阻塞:这是 CoreAudio 的实时回调线程,被下游背压顶住就等于让
                    // HAL 整块丢音(2026-08-16 实测:98 分钟录音丢了 13.5 分钟,tap 被顶
                    // 最长 1.198 秒)。宁可这一帧不要,也要立刻返回。
                    //
                    // 丢掉的时长由 tap 按硬件时戳补零补回时间轴(见 frame_tap 的 holey
                    // 判定):下一帧的时戳会把这段缺口如实暴露出来。这里刻意**不**在回调
                    // 里自己补零——试过三版(跨线程计数器 / 补进下一帧 / 回拨时戳),每版
                    // 都引出新问题:计数在 FIFO 里的位置错、混合帧的时戳无法用下游的率
                    // 表达、合成前缀污染漂移诊断、超过上限的丢失无处安放。要做对得给
                    // AudioFrame 加"合成样本"标记让 tap 与 drift 区别对待,那是独立改动。
                    //
                    // 已知残余:可变帧长下,若丢掉的那个回调比前一个短,时戳缺口不足
                    // "一整帧",tap 的保守判据认不出来,该段仍会被压掉(每次 < 一个回调
                    // 周期)。等价的老行为是**整段阻塞**,两害相权取其轻。
                    if sink
                        .try_send(AudioFrame {
                            samples: data.to_vec(),
                            sample_rate,
                            channels,
                            host_time_ns,
                        })
                        .is_err()
                    {
                        dropped.fetch_add(per_ch as u64, std::sync::atomic::Ordering::Relaxed);
                    }
                },
                err_fn,
                None, // no timeout
            ) {
                Ok(s) => s,
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("无法建立麦克风流: {e}")));
                    return;
                }
            };
            if let Err(e) = stream.play() {
                let _ = ready_tx.send(Err(format!("无法启动麦克风流: {e}")));
                return;
            }
            // 流已成功开启，通知 start() 可以放心返回。
            let _ = ready_tx.send(Ok(()));
            // Block until stop_tx is dropped (stop() called).
            stop_rx.recv().ok();
            // 停流汇总:回调丢过帧就说清楚丢了多少——它意味着下游(tap→worker)
            // 没跟上采集,时间轴虽由 tap 补零保住,内容是真的少了那么多。
            let lost = dropped.load(std::sync::atomic::Ordering::Relaxed);
            if lost > 0 {
                eprintln!(
                    "麦克风回调丢样: {lost} 样本(约 {:.1}s @{sample_rate}Hz)——下游积压顶到了采集回调",
                    lost as f64 / sample_rate.max(1) as f64
                );
            }
            // `stream` drops here, stopping capture.
        });

        // 等待后台线程确认流是否真正开启，把静默失败变成可见错误。
        match ready_rx.recv() {
            Ok(Ok(())) => {
                // 与 vpio.rs 的启动日志对仗:capture_path=aec 排障时靠这行确认
                // 本场走的是普通输入(无 ducking)而非 VPIO。
                eprintln!("普通麦克风已启动(无 AEC/ducking): {sample_rate} Hz, f32 x{channels}");
                // 麦克风模式留痕:必须等流真的跑起来再读——activeMicrophoneMode 描述的是
                // **当前生效**的输入路由,开录请求刚被受理时读到的可能还是上一场的值
                // (Codex P2)。系统层「语音突显」会把非人声削成绝对零、判错时连人声一起
                // 削(2026-08-16 实测吃掉近两成语音),而这发生在音频进入本进程之前;
                // 事后排查靠这行分辨"系统削的"还是"我们链路丢的"。
                let mm = crate::audio::mic_mode::active();
                if mm.damages_audio() {
                    eprintln!(
                        "[采集] 麦克风模式=语音突显:系统会把非人声削成绝对零,建议在控制中心改回「标准」"
                    );
                } else {
                    eprintln!("[采集] 麦克风模式={}", mm.as_str());
                }
            }
            Ok(Err(e)) => return Err(anyhow::anyhow!(e)),
            Err(_) => return Err(anyhow::anyhow!("麦克风线程意外退出，未能开启音频流")),
        }

        self.stop_tx = Some(stop_tx);
        Ok(())
    }

    fn stop(&mut self) {
        // Dropping the sender closes the channel, unblocking the background thread.
        self.stop_tx = None;
    }

}
