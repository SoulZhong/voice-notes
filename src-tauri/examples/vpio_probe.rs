//! P4 Task 1 — VPIO spike:验证 macOS `kAudioUnitSubType_VoiceProcessingIO`(Apple 内建 AEC)
//! 能否在 Rust 里替代 cpal 采集麦克风,并观测其对「扬声器外放泄漏」的回声消除效果。
//!
//! 探针代码允许粗糙(这是 spike,不是产品代码)。产品化见 Task 8(VpioMicrophone)。
//!
//! 用法(cd src-tauri):
//!   录制(VPIO):   cargo run --example vpio_probe -- --backend vpio --secs 5 --out /tmp/vpio_probe.wav
//!   录制(cpal 对照):cargo run --example vpio_probe -- --backend cpal --secs 5 --out /tmp/cpal_probe.wav
//!   能量分析:      cargo run --example vpio_probe -- --rms /tmp/vpio_probe.wav
//!   ASR 转写:      cargo run --example vpio_probe -- --asr /tmp/vpio_probe.wav
//!
//! 自动化对照(afplay 外放模拟对方说话):
//!   ( for i in 1 2 3 ...; do afplay tests/fixtures/sample_16k.wav; done ) &
//!   cargo run --example vpio_probe -- --backend vpio --out /tmp/vpio_probe.wav

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // 子命令:纯分析,不碰音频硬件。
    if let Some(p) = flag_value(&args, "--rms") {
        return report_rms(&p);
    }
    if let Some(p) = flag_value(&args, "--asr") {
        return run_asr(&p);
    }

    let backend = flag_value(&args, "--backend").unwrap_or_else(|| "vpio".into());
    let secs: f64 = flag_value(&args, "--secs")
        .and_then(|s| s.parse().ok())
        .unwrap_or(5.0);
    let out = flag_value(&args, "--out").unwrap_or_else(|| format!("/tmp/{backend}_probe.wav"));

    let result = match backend.as_str() {
        #[cfg(target_os = "macos")]
        "vpio" => record_vpio(secs),
        #[cfg(target_os = "macos")]
        "hal" => record_hal(secs),
        #[cfg(target_os = "macos")]
        "raw" => record_vpio_raw(secs),
        "cpal" => record_cpal(secs),
        #[cfg(not(target_os = "macos"))]
        "vpio" => Err(anyhow::anyhow!("VPIO 仅 macOS")),
        other => Err(anyhow::anyhow!("未知 backend: {other}(用 vpio|cpal)")),
    };

    match result {
        Ok((samples, rate)) => {
            let (rms, peak, nonzero) = stats(&samples);
            println!(
                "[{backend}] 采集完成: {} 帧, {rate} Hz, mono | RMS={rms:.6} peak={peak:.6} 非零占比={:.1}%",
                samples.len(),
                nonzero * 100.0
            );
            if let Err(e) = write_wav(&out, &samples, rate) {
                eprintln!("写 WAV 失败: {e}");
                std::process::exit(1);
            }
            println!("[{backend}] 已写入 {out}");
        }
        Err(e) => {
            eprintln!("[{backend}] 采集失败: {e:?}");
            std::process::exit(1);
        }
    }
}

// ---------------------------------------------------------------------------
// VPIO 采集(coreaudio-rs 0.12,IOType::VoiceProcessingIO)
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
fn record_vpio(secs: f64) -> anyhow::Result<(Vec<f32>, u32)> {
    use coreaudio::audio_unit::audio_format::LinearPcmFlags;
    use coreaudio::audio_unit::render_callback::{data, Args};
    use coreaudio::audio_unit::{AudioUnit, Element, IOType, SampleFormat, Scope, StreamFormat};
    use coreaudio::sys::{
        kAudioOutputUnitProperty_CurrentDevice, kAudioOutputUnitProperty_EnableIO,
        kAudioUnitProperty_StreamFormat,
    };
    use std::sync::{Arc, Mutex};

    // 实验开关(spike 用,方便不改代码试不同配置):
    //   VPIO_KEEP_OUTPUT=1  → 保持 output(bus0)开启(VPIO 是双工单元,AEC 需要输出参考)
    //   VPIO_SET_DEVICE=1   → 显式把 CurrentDevice 设为默认输入设备
    let keep_output = std::env::var("VPIO_KEEP_OUTPUT").is_ok();
    let set_device = std::env::var("VPIO_SET_DEVICE").is_ok();

    // 1) 创建 VPIO 单元。注意 AudioUnit::new 会立即 AudioUnitInitialize,
    //    而 EnableIO / StreamFormat 只能在 *未初始化* 状态下修改,故先 uninitialize。
    let mut au = AudioUnit::new(IOType::VoiceProcessingIO)
        .map_err(|e| anyhow::anyhow!("创建 VoiceProcessingIO 单元失败: {e:?}"))?;
    au.uninitialize()
        .map_err(|e| anyhow::anyhow!("uninitialize 失败: {e:?}"))?;

    // 2) input scope element 1 开 EnableIO;output scope element 0 关(纯采集)。
    let enable: u32 = 1;
    let disable: u32 = 0;
    au.set_property(
        kAudioOutputUnitProperty_EnableIO,
        Scope::Input,
        Element::Input, // bus 1 = 硬件输入
        Some(&enable),
    )
    .map_err(|e| anyhow::anyhow!("启用 input IO 失败: {e:?}"))?;
    au.set_property(
        kAudioOutputUnitProperty_EnableIO,
        Scope::Output,
        Element::Output, // bus 0 = 硬件输出
        Some(if keep_output { &enable } else { &disable }),
    )
    .map_err(|e| anyhow::anyhow!("设置 output IO 失败: {e:?}"))?;

    // 可选:显式绑定默认输入设备(VPIO 缺省用系统默认输入)。
    if set_device {
        if let Some(dev) = coreaudio::audio_unit::macos_helpers::get_default_device_id(true) {
            au.set_property(
                kAudioOutputUnitProperty_CurrentDevice,
                Scope::Global,
                Element::Output,
                Some(&dev),
            )
            .map_err(|e| anyhow::anyhow!("设置 CurrentDevice 失败: {e:?}"))?;
            println!("[vpio] CurrentDevice = {dev}");
        }
    }

    // 3) 设 client 端格式(input 元件的 output scope = 你的回调读到的格式)。
    //    优先请求 16k f32 mono;若 initialize 不接受则回退到设备原生率。
    let want = |rate: f64| StreamFormat {
        sample_rate: rate,
        sample_format: SampleFormat::F32,
        // set_input_callback 的 data::NonInterleaved 要求非交错;非交错下强制单声道。
        flags: LinearPcmFlags::IS_FLOAT
            | LinearPcmFlags::IS_PACKED
            | LinearPcmFlags::IS_NON_INTERLEAVED,
        channels: 1,
    };

    let mut chosen_rate = 0.0f64;
    for rate in [16_000.0, 48_000.0, 44_100.0] {
        let asbd = want(rate).to_asbd();
        if au
            .set_property(
                kAudioUnitProperty_StreamFormat,
                Scope::Output,
                Element::Input,
                Some(&asbd),
            )
            .is_err()
        {
            continue;
        }
        if au.initialize().is_ok() {
            chosen_rate = rate;
            break;
        }
        // 初始化失败:回到未初始化状态再试下一个采样率。
        let _ = au.uninitialize();
    }
    if chosen_rate == 0.0 {
        anyhow::bail!("VPIO 无法在 16k/48k/44.1k 任一采样率下初始化(f32 mono)");
    }

    // 读回实际生效的输入格式(真相以设备为准)。
    let actual = au
        .input_stream_format()
        .map_err(|e| anyhow::anyhow!("读取输入格式失败: {e:?}"))?;
    let actual_rate = actual.sample_rate as u32;
    println!(
        "[vpio] 请求 {chosen_rate} Hz → 实际生效 sample_rate={} sample_format={:?} channels={} flags={:?}",
        actual.sample_rate, actual.sample_format, actual.channels, actual.flags
    );

    // 诊断:set_input_callback 内部按 kAudioDevicePropertyBufferFrameSize 预分配 render 缓冲,
    // 若该值取不到/为 0,AudioUnitRender 每次都失败 → 回调静默丢帧。
    match au.get_property::<u32>(
        coreaudio::sys::kAudioDevicePropertyBufferFrameSize,
        Scope::Global,
        Element::Output,
    ) {
        Ok(n) => println!("[vpio] buffer_frame_size = {n}"),
        Err(e) => println!("[vpio] buffer_frame_size 读取失败: {e:?}"),
    }

    // 4) 注册 input 回调,把帧塞进共享缓冲。
    let buf = Arc::new(Mutex::new(Vec::<f32>::new()));
    let producer = buf.clone();
    type CbArgs = Args<data::NonInterleaved<f32>>;
    au.set_input_callback(move |args: CbArgs| {
        let CbArgs {
            num_frames,
            mut data,
            ..
        } = args;
        if let Ok(mut v) = producer.lock() {
            // 单声道:取第一个(唯一)声道。
            if let Some(ch) = data.channels_mut().next() {
                v.extend_from_slice(&ch[..num_frames]);
            }
        }
        Ok(())
    })
    .map_err(|e| anyhow::anyhow!("set_input_callback 失败(权限?格式?): {e:?}"))?;

    au.start()
        .map_err(|e| anyhow::anyhow!("start 失败: {e:?}"))?;
    println!("[vpio] 采集中 {secs}s ...(若有 afplay 外放,VPIO 应抑制其泄漏)");
    std::thread::sleep(std::time::Duration::from_secs_f64(secs));
    au.stop().ok();

    let samples = Arc::try_unwrap(buf)
        .map(|m| m.into_inner().unwrap_or_default())
        .unwrap_or_default();
    if samples.is_empty() {
        anyhow::bail!("VPIO 回调未产出任何帧(检查麦克风权限:系统设置→隐私→麦克风)");
    }
    Ok((samples, actual_rate))
}

// ---------------------------------------------------------------------------
// 直调 C API(coreaudio::sys):VPIO 采集,自己写 AURenderCallback + AudioUnitRender,
// 记录 AudioUnitRender 的真实 OSStatus——用来判定 Task 8 应走 coreaudio-rs 还是 coreaudio-sys。
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
struct RawCtx {
    unit: coreaudio::sys::AudioUnit,
    samples: std::sync::Mutex<Vec<f32>>,
    calls: std::sync::atomic::AtomicUsize,
    render_ok: std::sync::atomic::AtomicUsize,
    last_err: std::sync::atomic::AtomicI32,
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn raw_input_cb(
    in_ref_con: *mut std::os::raw::c_void,
    io_action_flags: *mut coreaudio::sys::AudioUnitRenderActionFlags,
    in_time_stamp: *const coreaudio::sys::AudioTimeStamp,
    in_bus_number: u32,
    in_number_frames: u32,
    _io_data: *mut coreaudio::sys::AudioBufferList,
) -> coreaudio::sys::OSStatus {
    use std::sync::atomic::Ordering;
    let ctx = &*(in_ref_con as *const RawCtx);
    ctx.calls.fetch_add(1, Ordering::Relaxed);

    let mut scratch = vec![0f32; in_number_frames as usize];
    let mut abl = coreaudio::sys::AudioBufferList {
        mNumberBuffers: 1,
        mBuffers: [coreaudio::sys::AudioBuffer {
            mNumberChannels: 1,
            mDataByteSize: (in_number_frames * 4) as u32,
            mData: scratch.as_mut_ptr() as *mut std::os::raw::c_void,
        }],
    };
    let status = coreaudio::sys::AudioUnitRender(
        ctx.unit,
        io_action_flags,
        in_time_stamp,
        in_bus_number,
        in_number_frames,
        &mut abl,
    );
    if status != 0 {
        ctx.last_err.store(status, Ordering::Relaxed);
        return 0; // 别把错误往上抛,继续,只记录
    }
    ctx.render_ok.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut v) = ctx.samples.lock() {
        v.extend_from_slice(&scratch);
    }
    0
}

#[cfg(target_os = "macos")]
fn record_vpio_raw(secs: f64) -> anyhow::Result<(Vec<f32>, u32)> {
    use coreaudio::sys::*;
    use std::sync::atomic::Ordering;

    unsafe {
        let desc = AudioComponentDescription {
            componentType: kAudioUnitType_Output,
            componentSubType: kAudioUnitSubType_VoiceProcessingIO,
            componentManufacturer: kAudioUnitManufacturer_Apple,
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        let comp = AudioComponentFindNext(std::ptr::null_mut(), &desc);
        if comp.is_null() {
            anyhow::bail!("找不到 VoiceProcessingIO 组件");
        }
        let mut unit: AudioUnit = std::ptr::null_mut();
        let st = AudioComponentInstanceNew(comp, &mut unit);
        if st != 0 {
            anyhow::bail!("AudioComponentInstanceNew 失败: {st}");
        }

        // EnableIO: input(scope=Input,elem=1)=1;output(scope=Output,elem=0)=0
        let one: u32 = 1;
        let zero: u32 = 0;
        let chk = |st: OSStatus, what: &str| -> anyhow::Result<()> {
            if st != 0 {
                anyhow::bail!("{what} 失败: OSStatus={st}");
            }
            Ok(())
        };
        chk(
            AudioUnitSetProperty(
                unit,
                kAudioOutputUnitProperty_EnableIO,
                kAudioUnitScope_Input,
                1,
                &one as *const _ as *const _,
                4,
            ),
            "EnableIO(input)",
        )?;
        chk(
            AudioUnitSetProperty(
                unit,
                kAudioOutputUnitProperty_EnableIO,
                kAudioUnitScope_Output,
                0,
                &zero as *const _ as *const _,
                4,
            ),
            "EnableIO(output off)",
        )?;

        // client 格式:请求 16k f32 mono 非交错;设在 input 元件(bus1)的 output scope。
        let rate = std::env::var("RAW_RATE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(16_000.0f64);
        let mut asbd = AudioStreamBasicDescription {
            mSampleRate: rate,
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
        let fmt_st = AudioUnitSetProperty(
            unit,
            kAudioUnitProperty_StreamFormat,
            kAudioUnitScope_Output,
            1,
            &asbd as *const _ as *const _,
            std::mem::size_of::<AudioStreamBasicDescription>() as u32,
        );
        if fmt_st != 0 {
            println!("[raw] 设 16k 格式被拒(OSStatus={fmt_st}),改用设备原生格式");
        } else {
            println!("[raw] 已设客户端格式 {rate} Hz f32 mono");
        }

        // 读回实际输入格式
        let mut got = std::mem::zeroed::<AudioStreamBasicDescription>();
        let mut sz = std::mem::size_of::<AudioStreamBasicDescription>() as u32;
        AudioUnitGetProperty(
            unit,
            kAudioUnitProperty_StreamFormat,
            kAudioUnitScope_Output,
            1,
            &mut got as *mut _ as *mut _,
            &mut sz,
        );
        println!(
            "[raw] 实际输入格式: rate={} ch={} bits={} flags={:#x}",
            got.mSampleRate, got.mChannelsPerFrame, got.mBitsPerChannel, got.mFormatFlags
        );
        asbd = got;

        // ctx（泄漏到 stop 之后）
        let ctx = Box::into_raw(Box::new(RawCtx {
            unit,
            samples: std::sync::Mutex::new(Vec::new()),
            calls: std::sync::atomic::AtomicUsize::new(0),
            render_ok: std::sync::atomic::AtomicUsize::new(0),
            last_err: std::sync::atomic::AtomicI32::new(0),
        }));

        let cb = AURenderCallbackStruct {
            inputProc: Some(raw_input_cb),
            inputProcRefCon: ctx as *mut _,
        };
        chk(
            AudioUnitSetProperty(
                unit,
                kAudioOutputUnitProperty_SetInputCallback,
                kAudioUnitScope_Global,
                0,
                &cb as *const _ as *const _,
                std::mem::size_of::<AURenderCallbackStruct>() as u32,
            ),
            "SetInputCallback",
        )?;

        chk(AudioUnitInitialize(unit), "AudioUnitInitialize")?;
        chk(AudioOutputUnitStart(unit), "AudioOutputUnitStart")?;
        println!("[raw] 采集中 {secs}s ...");
        std::thread::sleep(std::time::Duration::from_secs_f64(secs));
        AudioOutputUnitStop(unit);

        let ctx_ref = &*ctx;
        let calls = ctx_ref.calls.load(Ordering::Relaxed);
        let ok = ctx_ref.render_ok.load(Ordering::Relaxed);
        let err = ctx_ref.last_err.load(Ordering::Relaxed);
        println!("[raw] 回调触发 {calls} 次, AudioUnitRender 成功 {ok} 次, 最后错误码={err}");
        let samples = ctx_ref.samples.lock().unwrap().clone();
        let out_rate = asbd.mSampleRate as u32;

        AudioUnitUninitialize(unit);
        AudioComponentInstanceDispose(unit);
        drop(Box::from_raw(ctx));

        if samples.is_empty() {
            anyhow::bail!("raw VPIO 无帧(回调 {calls} 次,渲染错误码 {err})");
        }
        Ok((samples, out_rate))
    }
}

// ---------------------------------------------------------------------------
// HalOutput 输入对照(feedback.rs 配方):验证 coreaudio-rs 的 input callback
// 机制本身在本环境是否可用,以隔离「VPIO 特有问题」vs「库/环境问题」。
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
fn record_hal(secs: f64) -> anyhow::Result<(Vec<f32>, u32)> {
    use coreaudio::audio_unit::audio_format::LinearPcmFlags;
    use coreaudio::audio_unit::macos_helpers::{audio_unit_from_device_id, get_default_device_id};
    use coreaudio::audio_unit::render_callback::{data, Args};
    use coreaudio::audio_unit::{Element, SampleFormat, Scope, StreamFormat};
    use coreaudio::sys::kAudioUnitProperty_StreamFormat;
    use std::sync::{Arc, Mutex};

    let dev = get_default_device_id(true).ok_or_else(|| anyhow::anyhow!("无默认输入设备"))?;
    let mut au = audio_unit_from_device_id(dev, true)
        .map_err(|e| anyhow::anyhow!("HalOutput 输入单元创建失败: {e:?}"))?;
    let fmt = StreamFormat {
        sample_rate: 44_100.0,
        sample_format: SampleFormat::F32,
        flags: LinearPcmFlags::IS_FLOAT
            | LinearPcmFlags::IS_PACKED
            | LinearPcmFlags::IS_NON_INTERLEAVED,
        channels: 1,
    };
    let asbd = fmt.to_asbd();
    au.set_property(
        kAudioUnitProperty_StreamFormat,
        Scope::Output,
        Element::Input,
        Some(&asbd),
    )
    .map_err(|e| anyhow::anyhow!("设置格式失败: {e:?}"))?;

    let buf = Arc::new(Mutex::new(Vec::<f32>::new()));
    let producer = buf.clone();
    type CbArgs = Args<data::NonInterleaved<f32>>;
    au.set_input_callback(move |args: CbArgs| {
        let CbArgs { num_frames, mut data, .. } = args;
        if let (Ok(mut v), Some(ch)) = (producer.lock(), data.channels_mut().next()) {
            v.extend_from_slice(&ch[..num_frames]);
        }
        Ok(())
    })
    .map_err(|e| anyhow::anyhow!("set_input_callback 失败: {e:?}"))?;
    au.start().map_err(|e| anyhow::anyhow!("start 失败: {e:?}"))?;
    println!("[hal] 采集中 {secs}s ...");
    std::thread::sleep(std::time::Duration::from_secs_f64(secs));
    au.stop().ok();
    let samples = buf.lock().unwrap().clone();
    if samples.is_empty() {
        anyhow::bail!("HalOutput 输入也没帧 → coreaudio-rs input-callback 机制在本环境不可用");
    }
    Ok((samples, 44_100))
}

// ---------------------------------------------------------------------------
// cpal 采集对照(复用主 crate 的 cpal 依赖)
// ---------------------------------------------------------------------------
fn record_cpal(secs: f64) -> anyhow::Result<(Vec<f32>, u32)> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::SampleFormat;
    use std::sync::{Arc, Mutex};

    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow::anyhow!("找不到默认麦克风"))?;
    let supported = device.default_input_config()?;
    if supported.sample_format() != SampleFormat::F32 {
        anyhow::bail!("cpal 设备格式非 f32: {}", supported.sample_format());
    }
    let rate = supported.sample_rate().0;
    let channels = supported.channels();
    let cfg: cpal::StreamConfig = supported.into();
    println!("[cpal] 设备格式 sample_rate={rate} channels={channels}");

    let buf = Arc::new(Mutex::new(Vec::<f32>::new()));
    let producer = buf.clone();
    let stream = device.build_input_stream(
        &cfg,
        move |data: &[f32], _: &cpal::InputCallbackInfo| {
            if let Ok(mut v) = producer.lock() {
                // 交错多声道 → 取第一声道,和 VPIO 的单声道对齐比较。
                if channels <= 1 {
                    v.extend_from_slice(data);
                } else {
                    v.extend(data.iter().step_by(channels as usize).copied());
                }
            }
        },
        |e| eprintln!("[cpal] 流错误: {e}"),
        None,
    )?;
    stream.play()?;
    println!("[cpal] 采集中 {secs}s ...(无 AEC,afplay 外放会被完整录进来)");
    std::thread::sleep(std::time::Duration::from_secs_f64(secs));
    drop(stream);

    let samples = buf.lock().unwrap().clone();
    if samples.is_empty() {
        anyhow::bail!("cpal 未产出任何帧");
    }
    Ok((samples, rate))
}

// ---------------------------------------------------------------------------
// 分析工具
// ---------------------------------------------------------------------------
fn stats(s: &[f32]) -> (f32, f32, f32) {
    if s.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let sum_sq: f64 = s.iter().map(|&x| (x as f64) * (x as f64)).sum();
    let rms = (sum_sq / s.len() as f64).sqrt() as f32;
    let peak = s.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
    let nz = s.iter().filter(|&&x| x.abs() > 1e-4).count() as f32 / s.len() as f32;
    (rms, peak, nz)
}

fn report_rms(path: &str) {
    match read_wav(path) {
        Ok((s, rate)) => {
            let (rms, peak, nz) = stats(&s);
            let db = if rms > 0.0 { 20.0 * rms.log10() } else { -120.0 };
            println!(
                "{path}: {} 帧 @ {rate} Hz | RMS={rms:.6} ({db:.1} dBFS) peak={peak:.6} 非零占比={:.1}%",
                s.len(),
                nz * 100.0
            );
        }
        Err(e) => eprintln!("读 {path} 失败: {e}"),
    }
}

fn run_asr(path: &str) {
    #[cfg(target_os = "macos")]
    {
        use app_lib::asr::{sense_voice::SenseVoiceRecognizer, Recognizer};
        use std::path::PathBuf;
        let (mut samples, rate) = match read_wav(path) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("读 {path} 失败: {e}");
                return;
            }
        };
        if rate != 16_000 {
            samples = resample_linear(&samples, rate, 16_000);
            println!("(已把 {rate}Hz 线性重采样到 16kHz 供 SenseVoice)");
        }
        let model_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("models/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17");
        match SenseVoiceRecognizer::new(&model_dir) {
            Ok(mut rec) => match rec.recognize(&samples) {
                Ok(t) => println!("ASR[{path}]: {}", t.text),
                Err(e) => eprintln!("识别失败: {e}"),
            },
            Err(e) => eprintln!("加载 SenseVoice 失败: {e}"),
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = path;
}

// ---------------------------------------------------------------------------
// WAV I/O(hound,统一 f32 mono)
// ---------------------------------------------------------------------------
fn write_wav(path: &str, samples: &[f32], rate: u32) -> anyhow::Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec)?;
    for &s in samples {
        w.write_sample(s)?;
    }
    w.finalize()?;
    Ok(())
}

fn read_wav(path: &str) -> anyhow::Result<(Vec<f32>, u32)> {
    let mut r = hound::WavReader::open(path)?;
    let spec = r.spec();
    let ch = spec.channels.max(1) as usize;
    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => r.samples::<f32>().map(|s| s.unwrap_or(0.0)).collect(),
        hound::SampleFormat::Int => {
            let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
            r.samples::<i32>()
                .map(|s| s.unwrap_or(0) as f32 / max)
                .collect()
        }
    };
    // 取第一声道 → mono
    let mono: Vec<f32> = if ch <= 1 {
        interleaved
    } else {
        interleaved.into_iter().step_by(ch).collect()
    };
    Ok((mono, spec.sample_rate))
}

/// 单声道线性插值重采样(与主 crate audio::resample 同法;例子里不便引私有模块,内联一份)。
#[cfg(target_os = "macos")]
fn resample_linear(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.is_empty() {
        return input.to_vec();
    }
    let ratio = to as f64 / from as f64;
    let out_len = (input.len() as f64 * ratio).round() as usize;
    (0..out_len)
        .map(|i| {
            let pos = i as f64 / ratio;
            let idx = pos.floor() as usize;
            let frac = (pos - idx as f64) as f32;
            let s0 = input.get(idx).copied().unwrap_or(0.0);
            let s1 = input.get(idx + 1).copied().unwrap_or(s0);
            s0 + (s1 - s0) * frac
        })
        .collect()
}

fn flag_value(args: &[String], key: &str) -> Option<String> {
    args.iter().position(|a| a == key).and_then(|i| args.get(i + 1).cloned())
}
