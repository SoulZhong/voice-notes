// 软件 AEC(WebRTC AEC3):Windows 上依赖不可构建(见 aec_stub.rs 头注),
// 用 #[path] 顶替同形桩模块——消费方(session/segment_worker/echo_clean)零分叉。
#[cfg(not(windows))]
pub mod aec;
#[cfg(windows)]
#[path = "aec_stub.rs"]
pub mod aec;
pub mod resample;
pub mod timeline_mix;
pub mod mock;
pub mod microphone;
pub mod resilient;
pub mod delay_estimate;
pub mod echo_clean;
pub mod aec_align;
pub mod neural_aec;
pub mod host_time;
pub mod drift_dll;
pub mod actual_rate;
// 麦克风模式(系统层「语音突显」检测):双平台同形,非 macOS 恒 Unknown。
pub mod mic_mode;
#[cfg(target_os = "macos")]
pub mod system;
#[cfg(target_os = "macos")]
pub mod vpio;
#[cfg(windows)]
pub mod loopback;

use crossbeam_channel::Sender;

/// 一帧原始音频，来自采集设备的原生格式。
#[derive(Debug, Clone)]
pub struct AudioFrame {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: u16,
    /// 帧首样本的 host 时刻(ns,mach 时基;见 audio/host_time.rs)。
    /// None = 采集后端拿不到硬件时间戳,漂移传感器按"到达墙钟"降级(行为等同引入前)。
    pub host_time_ns: Option<u64>,
    /// 这一帧是不是我们**自己造的**(FrameTap 按墙钟补的零),而非设备送来的。
    ///
    /// 为什么单列一个标记而不看 `host_time_ns.is_none()`:后者只说明"这帧没有
    /// 硬件时戳",VPIO/SCK 在某些路径下也会给不出时戳,两者不是一回事。没有这个
    /// 标记,补零帧到了下游就和"设备真的送来一段数字静音"完全无法区分——2026-08-17
    /// 排障里最终 m4a 的绝对零段究竟是补零还是系统削的,只能靠比例反推(Codex 复核 P1)。
    ///
    /// 消费方按需使用,默认行为不变:补零帧仍照常参与重采样/AEC/落盘,时间轴语义
    /// 不受影响。它先解决"看得见",怎么区别对待是后续独立改动。
    pub synthetic: bool,
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

    /// 采集回调因下游队列满而丢弃的样本数(每通道,累计)。
    ///
    /// 为什么要暴露:回调丢样和设备/HAL 没供帧在 hw 时戳上长得一模一样——都是
    /// 一个缺口——但归因相反(前者修下游背压,后者换设备/连接方式)。不把它记进
    /// audio.json,事后就只能靠猜。默认 0:后端不丢样(VPIO/SCK 阻塞语义不同)
    /// 或不统计时,不该被记上莫须有的丢样。
    fn dropped_samples(&self) -> u64 {
        0
    }
}

/// 采集流运行期事件(启动期错误走 start 的 Err,不在此列)。
/// cpal 系后端把流错误回调升格为本事件供断连自愈消费;未接线的后端
/// (VPIO/SCK)其运行期死亡由 FrameTap 帧荒检测兜底——两条探测路径互补。
#[derive(Debug, Clone)]
pub enum CaptureEvent {
    /// 流错误(设备拔出/被系统回收等),流已不可用。
    Error(String),
}

/// 某个默认设备(输入或输出)是否走蓝牙。`selector` 取
/// `kAudioHardwarePropertyDefaultInputDevice` / `...DefaultOutputDevice`。
/// 查询失败一律按"非蓝牙"处理,不挡任何流程——这是提示类判定,宁可漏报不可误挡。
#[cfg(target_os = "macos")]
fn default_device_is_bluetooth(selector: u32) -> bool {
    use coreaudio::sys::*;
    unsafe {
        let mut dev: AudioDeviceID = 0;
        let mut size = std::mem::size_of::<AudioDeviceID>() as u32;
        let addr = AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMaster,
        };
        if AudioObjectGetPropertyData(
            kAudioObjectSystemObject,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            &mut dev as *mut _ as *mut _,
        ) != 0
            || dev == kAudioObjectUnknown
        {
            return false;
        }
        let addr = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyTransportType,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMaster,
        };
        let mut transport: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        if AudioObjectGetPropertyData(
            dev,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            &mut transport as *mut _ as *mut _,
        ) != 0
        {
            return false;
        }
        transport == kAudioDeviceTransportTypeBluetooth
            || transport == kAudioDeviceTransportTypeBluetoothLE
    }
}

/// 当前默认输出设备是否蓝牙(macOS)。用途:capture_path=aec(软件回声消除)路径下,
/// 蓝牙播放延迟(300~600ms+)远超 WebRTC AEC3 的延迟估计范围(约 250ms),
/// 软件回声消除完全失效,mic 轨会混入近乎全量的对方声音(2026-07-08 蓝牙
/// 实录实锤:两轨互相关包络峰 lag≈600ms、mic 残余电平与 system 同量级)——
/// 录制页据此在开录前给出预警(与 capture_path 设置无关,不按设置项门控)。
#[cfg(target_os = "macos")]
pub fn default_output_is_bluetooth() -> bool {
    default_device_is_bluetooth(coreaudio::sys::kAudioHardwarePropertyDefaultOutputDevice)
}

/// 当前默认**输入**设备是否蓝牙(macOS)。与输出侧是两件事:输出侧管的是回声消除
/// 失效(听感),输入侧管的是**内容丢失**——蓝牙麦克风走 HFP/SCO,上行带宽要和
/// 会议软件争,实测一场 22 分钟会议因此丢了 14.2% 的时长(笔记 20260817-112430,
/// hw_gap_ms=194614),而同场 48kHz 的系统声音轨零断流。开录前据此劝退。
#[cfg(target_os = "macos")]
pub fn default_input_is_bluetooth() -> bool {
    default_device_is_bluetooth(coreaudio::sys::kAudioHardwarePropertyDefaultInputDevice)
}

/// 择优挑一个**非蓝牙**输入设备(录前设备检查自动择优,2026-08-22 设计):
/// 内置麦克风(BuiltIn)优先,其次任意非蓝牙输入;返回设备名(cpal 按名字匹配)。
/// 查询失败/无候选返回 None——与蓝牙判定同一原则:提示类逻辑宁可放弃不可误挡。
#[cfg(target_os = "macos")]
pub fn pick_non_bluetooth_input() -> Option<String> {
    pick_non_bluetooth_input_device().map(|(_, n)| n)
}

/// 同上,但连 AudioDeviceID 一起返回:VPIO 路径注入设备属性要的是 ID 不是名字
/// (issue #165;cpal 路径按名字绑定,两个调用方各取所需)。
#[cfg(target_os = "macos")]
pub fn pick_non_bluetooth_input_device() -> Option<(u32, String)> {
    use coreaudio::sys::*;
    unsafe {
        let addr = AudioObjectPropertyAddress {
            mSelector: kAudioHardwarePropertyDevices,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMaster,
        };
        let mut size: u32 = 0;
        if AudioObjectGetPropertyDataSize(kAudioObjectSystemObject, &addr, 0, std::ptr::null(), &mut size) != 0 {
            return None;
        }
        let n = size as usize / std::mem::size_of::<AudioDeviceID>();
        let mut ids = vec![0 as AudioDeviceID; n];
        if AudioObjectGetPropertyData(
            kAudioObjectSystemObject, &addr, 0, std::ptr::null(), &mut size,
            ids.as_mut_ptr() as *mut _,
        ) != 0 {
            return None;
        }
        let prop_u32 = |dev: AudioDeviceID, sel: u32, scope: u32| -> Option<u32> {
            let a = AudioObjectPropertyAddress { mSelector: sel, mScope: scope, mElement: kAudioObjectPropertyElementMaster };
            let mut v: u32 = 0;
            let mut sz = std::mem::size_of::<u32>() as u32;
            (AudioObjectGetPropertyData(dev, &a, 0, std::ptr::null(), &mut sz, &mut v as *mut _ as *mut _) == 0).then_some(v)
        };
        let input_channels = |dev: AudioDeviceID| -> u32 {
            let a = AudioObjectPropertyAddress {
                mSelector: kAudioDevicePropertyStreamConfiguration,
                mScope: kAudioObjectPropertyScopeInput,
                mElement: kAudioObjectPropertyElementMaster,
            };
            let mut sz: u32 = 0;
            if AudioObjectGetPropertyDataSize(dev, &a, 0, std::ptr::null(), &mut sz) != 0 || sz == 0 {
                return 0;
            }
            let mut buf = vec![0u8; sz as usize];
            if AudioObjectGetPropertyData(dev, &a, 0, std::ptr::null(), &mut sz, buf.as_mut_ptr() as *mut _) != 0 {
                return 0;
            }
            let abl = &*(buf.as_ptr() as *const AudioBufferList);
            std::slice::from_raw_parts(abl.mBuffers.as_ptr(), abl.mNumberBuffers as usize)
                .iter()
                .map(|b| b.mNumberChannels)
                .sum()
        };
        let dev_name = |dev: AudioDeviceID| -> Option<String> {
            let a = AudioObjectPropertyAddress {
                mSelector: kAudioObjectPropertyName,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMaster,
            };
            let mut cf: CFStringRef = std::ptr::null();
            let mut sz = std::mem::size_of::<CFStringRef>() as u32;
            if AudioObjectGetPropertyData(dev, &a, 0, std::ptr::null(), &mut sz, &mut cf as *mut _ as *mut _) != 0
                || cf.is_null()
            {
                return None;
            }
            let mut buf = [0i8; 256];
            let ok = CFStringGetCString(cf, buf.as_mut_ptr(), buf.len() as CFIndex, kCFStringEncodingUTF8) != 0;
            CFRelease(cf as *const _);
            ok.then(|| std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
                .filter(|s| !s.is_empty())
        };
        let mut best: Option<(u32, AudioDeviceID, String)> = None; // (rank: 0=内置 1=其它, id, name)
        for dev in ids {
            if input_channels(dev) == 0 {
                continue;
            }
            let Some(t) = prop_u32(dev, kAudioDevicePropertyTransportType, kAudioObjectPropertyScopeGlobal) else { continue };
            if t == kAudioDeviceTransportTypeBluetooth || t == kAudioDeviceTransportTypeBluetoothLE {
                continue;
            }
            // 聚合/虚拟设备不作候选:名义上有输入,实际路由不明,自动换上去反而坑人。
            if t == kAudioDeviceTransportTypeAggregate || t == kAudioDeviceTransportTypeVirtual {
                continue;
            }
            let Some(name) = dev_name(dev) else { continue };
            let rank = if t == kAudioDeviceTransportTypeBuiltIn { 0 } else { 1 };
            if best.as_ref().map_or(true, |(r, _, _)| rank < *r) {
                let done = rank == 0;
                best = Some((rank, dev, name));
                if done {
                    break;
                }
            }
        }
        best.map(|(_, id, n)| (id, n))
    }
}

#[cfg(not(target_os = "macos"))]
pub fn pick_non_bluetooth_input() -> Option<String> {
    None
}

#[cfg(not(target_os = "macos"))]
pub fn default_output_is_bluetooth() -> bool {
    false
}

#[cfg(not(target_os = "macos"))]
pub fn default_input_is_bluetooth() -> bool {
    false
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

#[cfg(all(test, target_os = "macos"))]
mod bt_probe_tests {
    /// 冒烟:CoreAudio 探测不 crash、可重复调用(结果取决于机器当前输出设备,
    /// 不做真值断言;与 system_profiler 的人工对照见 2026-07-08 校准记录)。
    #[test]
    fn default_output_probe_does_not_crash() {
        let a = super::default_output_is_bluetooth();
        let b = super::default_output_is_bluetooth();
        assert_eq!(a, b, "同一时刻重复探测应稳定");
    }
}
