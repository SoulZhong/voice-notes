use super::{AudioCapture, AudioFrame, CaptureEvent};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use crossbeam_channel::Sender;

/// Real microphone capture via cpal.
///
/// `cpal::Stream` is `!Send`, so we cannot store it directly in `Microphone`
/// when `AudioCapture: Send` is required. Instead we own the stream on a
/// dedicated background thread and communicate via a stop-channel.
/// 回调侧丢样补零的上限(每声道样本数,约 5 秒 @48k)。丢得比这还多说明下游已经
/// 长时间不动了,再补下去只是在实时回调里做大块分配;超出的部分交给 tap 的时戳
/// 补洞与 drift 记账,宁可少补也不能让回调自己变成卡顿源。
const DROP_PAD_CAP: usize = 48_000 * 5;

/// 把此前丢掉的样本以静音补在本批数据前面,并把时间戳回拨到**第一个样本**的时刻。
///
/// 纯函数,不清 `pending`——清零由调用方在 try_send **成功之后**做:队列持续满时
/// 每次尝试都会失败,提前清零会让先前累计的丢失被逐次遗忘,最后只补上最后一个
/// 回调那点(Codex 七轮 P1)。
///
/// 时戳必须回拨(Codex 七轮 P1):cpal 给的是 `data` 首样本的捕获时刻,补零之后这
/// 一帧的首样本比它早了 pad 那么多。不回拨的话,tap 看到的"上一帧到本帧"间隔仍然
/// 大于本帧携带的样本时长,会把同一段缺口按 HAL 丢失再补一次,轨道反而被拉长。
fn padded_frame(
    pending: usize,
    data: &[f32],
    channels: u16,
    sample_rate: u32,
    ts: Option<u64>,
) -> (Vec<f32>, Option<u64>) {
    if pending == 0 {
        return (data.to_vec(), ts);
    }
    let ch = channels.max(1) as usize;
    let pad = pending * ch;
    let mut out = Vec::with_capacity(pad + data.len());
    out.resize(pad, 0.0);
    out.extend_from_slice(data);
    let ts = ts.map(|t| {
        let back = if sample_rate > 0 {
            (pending as u64 * 1_000_000_000) / sample_rate as u64
        } else {
            0
        };
        t.saturating_sub(back)
    });
    (out, ts)
}

pub struct Microphone {
    /// Dropping this sender signals the background thread to stop the stream.
    stop_tx: Option<crossbeam_channel::Sender<()>>,
    /// 运行期流错误上报口(断连自愈消费);未接线时仅落日志,行为同引入前。
    events: Option<Sender<CaptureEvent>>,
    /// 回调因下游积压丢弃的样本数(每声道,累计)。仅供停流时汇总日志——
    /// 时间轴由回调自己补零保住(见 start 里的 pad_dropped)。
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
            // 待补的丢样(每声道样本数):下一次成功发送时补在数据前面。
            let mut pending_drop: usize = 0;
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
                    // 绝不阻塞:这是 CoreAudio 的实时回调线程,它一旦被下游背压顶住,
                    // HAL 侧就开始整块丢音(2026-08-16 实测:一场 98 分钟录音丢了
                    // 13.5 分钟,tap 被顶最长 1.198 秒)。宁可这一帧不要,也要立刻返回。
                    //
                    // 丢掉的那段在**这里**补零补进下一帧,而不是留给 tap 事后猜:
                    // 位置天然正确(就在下一批数据之前),时长天然对齐(样本数与时戳
                    // 走过的时间重新一致,对账不会被搅乱),也不需要任何跨线程的
                    // 丢失元数据(Codex 六轮:计数器方案在 FIFO 里的位置全错,且生产
                    // 装配隔着 ResilientCapture 根本读不到)。
                    let (payload, ts) =
                        padded_frame(pending_drop, data, channels, sample_rate, host_time_ns);
                    let sent = sink
                        .try_send(AudioFrame {
                            samples: payload,
                            sample_rate,
                            channels,
                            host_time_ns: ts,
                        })
                        .is_ok();
                    if sent {
                        pending_drop = 0; // 补出去了才清,失败要留着继续攒
                    } else {
                        let per_ch = data.len() / channels.max(1) as usize;
                        pending_drop = (pending_drop + per_ch).min(DROP_PAD_CAP);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_pending_drop_passes_data_through() {
        let (out, ts) = padded_frame(0, &[0.1, 0.2], 1, 16_000, Some(5_000));
        assert_eq!(out, vec![0.1, 0.2]);
        assert_eq!(ts, Some(5_000), "没补零就不该动时戳");
    }

    #[test]
    fn dropped_span_is_padded_in_front() {
        // 丢了 3 个(每声道)样本,下一批数据前面就该有 3 个静音——补在丢失的位置上,
        // 时长与丢失量一致,下游的时间轴因此不被压缩。
        let (out, _) = padded_frame(3, &[0.5, 0.6], 1, 16_000, None);
        assert_eq!(out, vec![0.0, 0.0, 0.0, 0.5, 0.6]);
    }

    #[test]
    fn timestamp_is_rewound_to_the_first_padded_sample() {
        // 16k 下补 160 个样本 = 10ms;时戳必须回拨 10ms,否则 tap 会把同一段缺口
        // 当 HAL 丢失再补一次(Codex 七轮 P1)。
        let (_, ts) = padded_frame(160, &[0.0], 1, 16_000, Some(100_000_000));
        assert_eq!(ts, Some(90_000_000));
    }

    #[test]
    fn missing_timestamp_stays_missing() {
        let (_, ts) = padded_frame(160, &[0.0], 1, 16_000, None);
        assert_eq!(ts, None);
    }

    #[test]
    fn padding_respects_channel_interleaving() {
        // 双声道:每声道 2 个样本 = 4 个交错样本。
        let (out, _) = padded_frame(2, &[1.0, -1.0], 2, 48_000, None);
        assert_eq!(out.len(), 4 + 2);
        assert!(out[..4].iter().all(|s| *s == 0.0));
        assert_eq!(&out[4..], &[1.0, -1.0]);
    }

    #[test]
    fn pending_is_not_cleared_by_the_helper() {
        // 纯函数不清零:清零由调用方在发送成功后做。队列持续满时每次都失败,
        // 提前清零会把先前累计的丢失逐次遗忘。
        let pending = 5usize;
        let (a, _) = padded_frame(pending, &[0.0], 1, 16_000, None);
        let (b, _) = padded_frame(pending, &[0.0], 1, 16_000, None);
        assert_eq!(a.len(), b.len(), "同一 pending 两次调用结果一致");
    }

    #[test]
    fn cap_bounds_the_allocation() {
        let (out, _) = padded_frame(DROP_PAD_CAP, &[0.0], 1, 48_000, None);
        assert_eq!(out.len(), DROP_PAD_CAP + 1);
    }
}
