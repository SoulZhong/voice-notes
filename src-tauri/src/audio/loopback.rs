//! Windows 系统声音采集(WASAPI loopback)。本文件仅 Windows 编译。
//!
//! 原理:cpal 对「输出」设备建 input stream 时,WASAPI 后端自动附加
//! AUDCLNT_STREAMFLAGS_LOOPBACK(v0.13.1 起,PR #478),即采集该设备正在播放
//! 的混音——系统声音。要求 Win10 1703+(event-driven loopback 才被系统支持)。
//!
//! 两个关键事实(2026-07-18 调研实证,详见计划文档):
//! 1. **格式必须取 `default_output_config()`**——对输出设备调 input config 系列
//!    会返回 StreamTypeNotSupported / 空迭代器。共享模式 mix format 通常是
//!    f32 / 48kHz / 2ch,但仍按报告的格式处理,i16 设备转 f32。
//! 2. **无音频播放时回调根本不触发**(不是回调静音帧)。静默期的时间轴由
//!    FrameTap 按墙钟补零维持(TapPolicy::system_loopback,250ms 起填),
//!    这是本采集路线在双路独立时间轴架构下成立的前提。
//!
//! 线程/握手/停止与 microphone.rs 同构:cpal::Stream 不是 Send,后台线程持流,
//! drop stop_tx 即停。运行期流错误(默认输出设备切换/被独占等)升格为
//! CaptureEvent 供断连自愈重启——重启会重新解析默认输出设备,天然跟随设备切换。
//!
//! 错误分类:Windows 环回无授权概念,启动失败一律 "unavailable:" 前缀
//! (classify_system 据此归类降级横幅)。

use super::{AudioCapture, AudioFrame, CaptureEvent};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use crossbeam_channel::Sender;

pub struct LoopbackCapture {
    /// drop = 通知后台线程停流(与 microphone.rs 同款惯用法)。
    stop_tx: Option<crossbeam_channel::Sender<()>>,
    /// 运行期流错误上报口(断连自愈消费);未接线时仅落日志。
    events: Option<Sender<CaptureEvent>>,
}

impl LoopbackCapture {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self { stop_tx: None, events: None }
    }

    pub fn with_events(events: Sender<CaptureEvent>) -> Self {
        Self { stop_tx: None, events: Some(events) }
    }
}

impl AudioCapture for LoopbackCapture {
    fn start(&mut self, sink: Sender<AudioFrame>) -> anyhow::Result<()> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow::anyhow!("unavailable: 找不到默认输出设备(系统声音环回不可用)"))?;
        // 关键:环回流的格式取自输出配置(见模块头注事实 1)。
        let supported = device
            .default_output_config()
            .map_err(|e| anyhow::anyhow!("unavailable: 无法读取输出设备配置: {e}"))?;
        let sample_rate = supported.sample_rate().0;
        let channels = supported.channels();
        let sample_format = supported.sample_format();
        let stream_config: cpal::StreamConfig = supported.into();

        let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(0);
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<(), String>>(1);

        let events = self.events.clone();
        std::thread::spawn(move || {
            let err_fn = move |e: cpal::StreamError| {
                eprintln!("系统声音环回流错误: {e}");
                if let Some(tx) = &events {
                    let _ = tx.send(CaptureEvent::Error(e.to_string()));
                }
            };
            // 共享模式 mix format 几乎总是 f32;个别驱动报 i16,转 f32 送同一条链。
            let built = match sample_format {
                SampleFormat::F32 => device.build_input_stream(
                    &stream_config,
                    move |data: &[f32], _: &cpal::InputCallbackInfo| {
                        let _ = sink.send(AudioFrame {
                            samples: data.to_vec(),
                            sample_rate,
                            channels,
                        });
                    },
                    err_fn,
                    None,
                ),
                SampleFormat::I16 => device.build_input_stream(
                    &stream_config,
                    move |data: &[i16], _: &cpal::InputCallbackInfo| {
                        let samples: Vec<f32> =
                            data.iter().map(|s| *s as f32 / 32768.0).collect();
                        let _ = sink.send(AudioFrame { samples, sample_rate, channels });
                    },
                    err_fn,
                    None,
                ),
                other => {
                    let _ = ready_tx
                        .send(Err(format!("unavailable: 环回流格式不支持: {other}")));
                    return;
                }
            };
            let stream = match built {
                Ok(s) => s,
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("unavailable: 无法建立环回流: {e}")));
                    return;
                }
            };
            if let Err(e) = stream.play() {
                let _ = ready_tx.send(Err(format!("unavailable: 无法启动环回流: {e}")));
                return;
            }
            let _ = ready_tx.send(Ok(()));
            // 阻塞至 stop_tx 被 drop;stream 随线程退出而 drop,停止采集。
            stop_rx.recv().ok();
        });

        match ready_rx.recv() {
            Ok(Ok(())) => {
                eprintln!("系统声音环回已启动(WASAPI loopback): {sample_rate} Hz x{channels}");
            }
            Ok(Err(e)) => return Err(anyhow::anyhow!(e)),
            Err(_) => return Err(anyhow::anyhow!("unavailable: 环回线程意外退出,未能开启音频流")),
        }

        self.stop_tx = Some(stop_tx);
        Ok(())
    }

    fn stop(&mut self) {
        self.stop_tx = None;
    }
}
