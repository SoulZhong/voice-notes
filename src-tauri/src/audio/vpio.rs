//! macOS 麦克风采集：Apple 内建声学回声消除（VoiceProcessingIO / VPIO）。
//! 本文件仅 macOS 编译。
//!
//! 依据 P4 Task 1 spike 报告（`.superpowers/sdd/p4-vpio-spike.md`）：
//! - 必须经 `coreaudio::sys` **直调 C API**（coreaudio-rs 高层封装对 VPIO 回调零帧）。
//! - 输入回调必须在 `AudioUnitInitialize` **之前**注册（AUHAL/VPIO 的硬性要求）。
//! - VPIO 输入端不接受 16k client 格式（`AudioUnitInitialize` 返回 -10875）；用设备原生
//!   采样率 f32 mono 采集，下游 `resample_linear` 负责降到 16k（与 cpal 路径一致）。
//!
//! 线程模型（spike「补充(评审修正)」一节）：`coreaudio::sys::AudioUnit` 是裸指针、非 Send。
//! 与 `microphone.rs`（cpal）一致，AudioUnit 在**后台线程**创建/启动/停止/销毁，
//! `VpioMicrophone` 只持 stop 通道（天然 Send），并复用同款 ready 握手把静默失败变成可见错误。
//! 因此本文件**不需要** `unsafe impl Send`。
//!
//! 运行时回退：麦克风是必备源。若 VPIO 初始化失败，`start` 不返回 Err，而是内部 new 一个
//! cpal `Microphone` 代打；只有 VPIO 与 cpal 都起不来才返回 Err。

use super::microphone::Microphone;
use super::{AudioCapture, AudioFrame};
use coreaudio::sys::*;
use crossbeam_channel::Sender;
use std::os::raw::c_void;

/// 采集后端：优先 VPIO（AEC），失败回退 cpal。
enum Backend {
    /// VPIO 后台线程模式：仅持 stop 通道。丢弃它即通知后台线程停止并释放 AudioUnit。
    Vpio(Sender<()>),
    /// 回退：cpal 麦克风（无 AEC）。
    Cpal(Microphone),
}

/// 带 Apple AEC 的麦克风采集，实现 `AudioCapture`。
pub struct VpioMicrophone {
    backend: Option<Backend>,
}

impl VpioMicrophone {
    pub fn new() -> Self {
        Self { backend: None }
    }
}

impl Default for VpioMicrophone {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioCapture for VpioMicrophone {
    fn start(&mut self, sink: Sender<AudioFrame>) -> anyhow::Result<()> {
        // 先试 VPIO；sink 用 clone 传入，失败时原 sink 仍可交给 cpal 回退。
        match start_vpio(sink.clone()) {
            Ok(stop_tx) => {
                self.backend = Some(Backend::Vpio(stop_tx));
                // 原 sink 在此作用域结束时丢弃，不残留多余 Sender。
                Ok(())
            }
            Err(e) => {
                eprintln!("VPIO(AEC)初始化失败，回退 cpal 麦克风(无回声消除): {e:#}");
                let mut mic = Microphone::new();
                // cpal 也起不来 → 麦克风彻底不可用，向上返回 Err。
                mic.start(sink)?;
                self.backend = Some(Backend::Cpal(mic));
                Ok(())
            }
        }
    }

    fn stop(&mut self) {
        match self.backend.take() {
            // 丢弃 stop 发送端 → 后台线程 recv 返回 Err → 执行 stop/uninitialize/dispose。
            Some(Backend::Vpio(stop_tx)) => drop(stop_tx),
            Some(Backend::Cpal(mut mic)) => mic.stop(),
            None => {}
        }
    }
}

// ---------------------------------------------------------------------------
// VPIO 实现（直调 C API）
// ---------------------------------------------------------------------------

/// 输入回调上下文。经 `Box::into_raw` 泄漏为原始指针交给 CoreAudio 作为 refCon，
/// 由后台线程在 teardown 时 `Box::from_raw` 回收——生命周期完全在后台线程内闭合，
/// 且 `AudioOutputUnitStop` 返回后回调不再触发，故无 use-after-free。
struct VpioCtx {
    /// 回调需要它来调 `AudioUnitRender` 拉取本轮麦克风数据。
    unit: AudioUnit,
    /// 采到的帧发往下游（VAD/ASR 管线）。
    sink: Sender<AudioFrame>,
    /// 实际生效的采样率（Hz），随每帧透传给下游重采样。
    sample_rate: u32,
}

/// 后台线程持有的句柄，用于 teardown。仅在后台线程内构造与消费，不跨线程传递。
struct VpioHandle {
    unit: AudioUnit,
    ctx: *mut VpioCtx,
}

/// CoreAudio 实时线程回调：拉一轮 f32 mono 数据，打包成 `AudioFrame` 发往下游。
unsafe extern "C" fn input_cb(
    in_ref_con: *mut c_void,
    io_action_flags: *mut AudioUnitRenderActionFlags,
    in_time_stamp: *const AudioTimeStamp,
    in_bus_number: u32,
    in_number_frames: u32,
    _io_data: *mut AudioBufferList,
) -> OSStatus {
    let ctx = &*(in_ref_con as *const VpioCtx);
    let n = in_number_frames as usize;

    // 非交错单声道：1 个 buffer，1 声道。渲染进这块 owned 缓冲后直接 move 进帧。
    let mut buf = vec![0f32; n];
    let mut abl = AudioBufferList {
        mNumberBuffers: 1,
        mBuffers: [AudioBuffer {
            mNumberChannels: 1,
            mDataByteSize: (n * std::mem::size_of::<f32>()) as u32,
            mData: buf.as_mut_ptr() as *mut c_void,
        }],
    };

    let status = AudioUnitRender(
        ctx.unit,
        io_action_flags,
        in_time_stamp,
        in_bus_number,
        in_number_frames,
        &mut abl,
    );
    if status != 0 {
        // 单轮渲染失败（偶发的格式/时序抖动）：丢弃本块，保持采集继续，不向上抛错。
        return 0;
    }

    // 下游已断开（会话结束）时 send 返回 Err，忽略即可。
    let _ = ctx.sink.send(AudioFrame {
        samples: buf,
        sample_rate: ctx.sample_rate,
        channels: 1,
    });
    0
}

/// 在后台线程创建/配置/启动 VPIO，附 ready 握手：`start()` 阻塞至设备确认打开，
/// 失败返回 Err（语义与 `microphone.rs` 完全一致）。成功返回 stop 发送端。
fn start_vpio(sink: Sender<AudioFrame>) -> anyhow::Result<Sender<()>> {
    // stop 通道：只用作信号，drop 发送端 = 断开 = 通知后台线程停止。
    let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(0);
    // ready 通道：后台线程回报设备是否真正打开。
    let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<(), String>>(1);

    std::thread::spawn(move || {
        // 裸指针句柄全程只在本线程内出现，不跨线程，故无需 Send。
        let handle = match unsafe { build_vpio_unit(sink) } {
            Ok(h) => h,
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        };
        // 设备已打开并开始产帧，通知 start() 放心返回。
        let _ = ready_tx.send(Ok(()));
        // 阻塞直到 stop 发送端被丢弃（stop() 调用或 VpioMicrophone 析构）。
        stop_rx.recv().ok();
        // 资源释放：Stop → Uninitialize → Dispose，并回收 refCon。
        unsafe { teardown_vpio_unit(handle) };
    });

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(stop_tx),
        Ok(Err(e)) => Err(anyhow::anyhow!(e)),
        Err(_) => Err(anyhow::anyhow!("VPIO 线程意外退出，未能开启音频流")),
    }
}

/// SDK 常量 kAUVoiceIOProperty_OtherAudioDuckingConfiguration(macOS 14+,
/// coreaudio-sys 绑定未包含,按 AudioUnitProperties.h 本地定义)。
const AU_VOICE_IO_PROPERTY_OTHER_AUDIO_DUCKING_CONFIGURATION: u32 = 2108;
/// kAUVoiceIOOtherAudioDuckingLevelMin(default=0/min=10/mid=20/max=30)。
const AU_VOICE_IO_OTHER_AUDIO_DUCKING_LEVEL_MIN: u32 = 10;

/// AUVoiceIOOtherAudioDuckingConfiguration 的 C 布局镜像:
/// { Boolean(u8) mEnableAdvancedDucking; AUVoiceIOOtherAudioDuckingLevel(u32) mDuckingLevel }。
#[repr(C)]
struct AuVoiceIoOtherAudioDuckingConfiguration {
    enable_advanced_ducking: u8,
    ducking_level: u32,
}

/// 创建并启动 VPIO 单元。全部 OSStatus 逐一检查；任一步失败都会释放已分配资源后返回 Err。
///
/// # Safety
/// 直调 CoreAudio C API，操作裸 `AudioUnit` 句柄。仅在 `start_vpio` 的后台线程内调用。
unsafe fn build_vpio_unit(sink: Sender<AudioFrame>) -> Result<VpioHandle, String> {
    // 1) 定位并实例化 VoiceProcessingIO 组件。
    let desc = AudioComponentDescription {
        componentType: kAudioUnitType_Output,
        componentSubType: kAudioUnitSubType_VoiceProcessingIO,
        componentManufacturer: kAudioUnitManufacturer_Apple,
        componentFlags: 0,
        componentFlagsMask: 0,
    };
    let comp = AudioComponentFindNext(std::ptr::null_mut(), &desc);
    if comp.is_null() {
        return Err("找不到 VoiceProcessingIO 组件".to_string());
    }
    let mut unit: AudioUnit = std::ptr::null_mut();
    let st = AudioComponentInstanceNew(comp, &mut unit);
    if st != 0 {
        return Err(format!("AudioComponentInstanceNew 失败: OSStatus={st}"));
    }
    // 此后任何失败都要 dispose；用闭包统一清理路径。
    let fail = |unit: AudioUnit, ctx: Option<*mut VpioCtx>, msg: String| -> String {
        if let Some(ctx) = ctx {
            drop(Box::from_raw(ctx));
        }
        AudioComponentInstanceDispose(unit);
        msg
    };

    // 2) EnableIO：input(scope=Input, elem=1)=1；output(scope=Output, elem=0)=0（纯采集）。
    let one: u32 = 1;
    let zero: u32 = 0;
    let st = AudioUnitSetProperty(
        unit,
        kAudioOutputUnitProperty_EnableIO,
        kAudioUnitScope_Input,
        1,
        &one as *const _ as *const c_void,
        std::mem::size_of::<u32>() as u32,
    );
    if st != 0 {
        return Err(fail(unit, None, format!("启用输入 IO 失败: OSStatus={st}")));
    }
    let st = AudioUnitSetProperty(
        unit,
        kAudioOutputUnitProperty_EnableIO,
        kAudioUnitScope_Output,
        0,
        &zero as *const _ as *const c_void,
        std::mem::size_of::<u32>() as u32,
    );
    if st != 0 {
        return Err(fail(unit, None, format!("关闭输出 IO 失败: OSStatus={st}")));
    }

    // 3) 客户端格式：读设备原生采样率，再设 f32 mono 非交错。请求 16k 会在 initialize 报
    //    -10875，故不硬编码 16k；也不硬编码 44.1k——读原生率最稳，兼容非 44.1k 设备。
    let mut native = std::mem::zeroed::<AudioStreamBasicDescription>();
    let mut sz = std::mem::size_of::<AudioStreamBasicDescription>() as u32;
    let st = AudioUnitGetProperty(
        unit,
        kAudioUnitProperty_StreamFormat,
        kAudioUnitScope_Output,
        1,
        &mut native as *mut _ as *mut c_void,
        &mut sz,
    );
    if st != 0 {
        return Err(fail(unit, None, format!("读取输入原生格式失败: OSStatus={st}")));
    }
    let native_rate = if native.mSampleRate > 0.0 {
        native.mSampleRate
    } else {
        44_100.0 // 兜底：spike 实测机原生率。
    };

    let asbd = AudioStreamBasicDescription {
        mSampleRate: native_rate,
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsFloat
            | kAudioFormatFlagIsPacked
            | kAudioFormatFlagIsNonInterleaved,
        mBytesPerPacket: 4,
        mFramesPerPacket: 1,
        mBytesPerFrame: 4,
        mChannelsPerFrame: 1,
        mBitsPerChannel: 32,
        mReserved: 0,
    };
    let st = AudioUnitSetProperty(
        unit,
        kAudioUnitProperty_StreamFormat,
        kAudioUnitScope_Output,
        1,
        &asbd as *const _ as *const c_void,
        std::mem::size_of::<AudioStreamBasicDescription>() as u32,
    );
    if st != 0 {
        return Err(fail(
            unit,
            None,
            format!("设置输入客户端格式(f32 mono {native_rate}Hz)失败: OSStatus={st}"),
        ));
    }

    // 读回实际生效格式，采样率以设备为准透传下游。
    let mut got = std::mem::zeroed::<AudioStreamBasicDescription>();
    let mut sz = std::mem::size_of::<AudioStreamBasicDescription>() as u32;
    let st = AudioUnitGetProperty(
        unit,
        kAudioUnitProperty_StreamFormat,
        kAudioUnitScope_Output,
        1,
        &mut got as *mut _ as *mut c_void,
        &mut sz,
    );
    if st != 0 {
        return Err(fail(unit, None, format!("回读输入格式失败: OSStatus={st}")));
    }
    let sample_rate = got.mSampleRate as u32;

    // 4) refCon 上下文（含 unit 句柄 + sink + 采样率）。此后失败须回收该 Box。
    let ctx = Box::into_raw(Box::new(VpioCtx {
        unit,
        sink,
        sample_rate,
    }));

    // 5) 注册输入回调——务必在 AudioUnitInitialize 之前（spike 核心结论）。
    let cb = AURenderCallbackStruct {
        inputProc: Some(input_cb),
        inputProcRefCon: ctx as *mut c_void,
    };
    let st = AudioUnitSetProperty(
        unit,
        kAudioOutputUnitProperty_SetInputCallback,
        kAudioUnitScope_Global,
        0,
        &cb as *const _ as *const c_void,
        std::mem::size_of::<AURenderCallbackStruct>() as u32,
    );
    if st != 0 {
        return Err(fail(
            unit,
            Some(ctx),
            format!("注册输入回调失败: OSStatus={st}"),
        ));
    }

    // 5.5) 其它 app 播放的自动压低(ducking)调到最小档。VPIO 启用后 macOS 进入
    //      "通话模式"大幅压低全系统外放;本应用的对方声音走 ScreenCaptureKit 数字
    //      直采(不经扬声器),压外放只伤听感不帮转写质量,故调最小。属性为
    //      macOS 14+(kAUVoiceIOProperty_OtherAudioDuckingConfiguration),
    //      旧系统不识别:非致命,打日志沿用系统默认。
    let duck = AuVoiceIoOtherAudioDuckingConfiguration {
        enable_advanced_ducking: 0, // 静态压低(不随语音活动动态调),行为可预期
        ducking_level: AU_VOICE_IO_OTHER_AUDIO_DUCKING_LEVEL_MIN,
    };
    let st = AudioUnitSetProperty(
        unit,
        AU_VOICE_IO_PROPERTY_OTHER_AUDIO_DUCKING_CONFIGURATION,
        kAudioUnitScope_Global,
        0,
        &duck as *const _ as *const c_void,
        std::mem::size_of::<AuVoiceIoOtherAudioDuckingConfiguration>() as u32,
    );
    if st != 0 {
        eprintln!("VPIO ducking 最小档设置失败(OSStatus={st}),沿用系统默认压低");
    }

    // 6) 初始化并启动。
    let st = AudioUnitInitialize(unit);
    if st != 0 {
        return Err(fail(
            unit,
            Some(ctx),
            format!("AudioUnitInitialize 失败: OSStatus={st}"),
        ));
    }
    let st = AudioOutputUnitStart(unit);
    if st != 0 {
        // 已 initialize，teardown 需对称 uninitialize；这里手动做全套后回收 ctx。
        AudioUnitUninitialize(unit);
        return Err(fail(
            unit,
            Some(ctx),
            format!("AudioOutputUnitStart 失败: OSStatus={st}"),
        ));
    }

    eprintln!("VPIO(AEC)麦克风已启动: {sample_rate} Hz, f32 mono");
    Ok(VpioHandle { unit, ctx })
}

/// 停止并彻底释放 VPIO 单元。`AudioOutputUnitStop` 返回后回调不再触发，随后回收 refCon 安全。
///
/// # Safety
/// 仅对 `build_vpio_unit` 返回的、尚未释放的句柄调用一次；只在后台线程内执行。
unsafe fn teardown_vpio_unit(handle: VpioHandle) {
    AudioOutputUnitStop(handle.unit);
    AudioUnitUninitialize(handle.unit);
    AudioComponentInstanceDispose(handle.unit);
    // 回调已不会再触发，安全回收上下文（drop 内含的 Sender，释放通道引用）。
    drop(Box::from_raw(handle.ctx));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 设备自检（需麦克风权限）：VpioMicrophone 应在约 1 秒内产出 f32 帧，stop 后干净收尾。
    /// 手动运行：`cargo test --lib vpio::tests::produces_frames -- --ignored --nocapture`
    #[test]
    #[ignore = "需要真实麦克风与权限；手动运行"]
    fn produces_frames() {
        let (tx, rx) = crossbeam_channel::unbounded::<AudioFrame>();
        let mut mic = VpioMicrophone::new();
        mic.start(tx).expect("start 应成功（VPIO 或 cpal 回退）");

        std::thread::sleep(std::time::Duration::from_millis(1000));
        mic.stop();

        let mut frames = 0usize;
        let mut samples = 0usize;
        let mut rate = 0u32;
        while let Ok(f) = rx.try_recv() {
            frames += 1;
            samples += f.samples.len();
            rate = f.sample_rate;
            assert_eq!(f.channels, 1, "应为单声道");
        }
        println!("采集到 {frames} 帧, {samples} 样本, {rate} Hz");
        assert!(frames > 0, "1 秒内应至少产出一帧");
        assert!(rate >= 16_000, "采样率应为设备原生率(>=16k)");
    }
}
