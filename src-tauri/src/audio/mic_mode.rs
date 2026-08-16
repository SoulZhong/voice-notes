//! macOS 麦克风模式(标准 / 宽谱 / 语音突显)读取。
//!
//! 为什么需要它:「语音突显」是系统层的人声分离,把它判定为非人声的部分**削成绝对零**,
//! 而且判错时连人声一起削。2026-08-16 实测同一台机器同一支内置麦——开着它录的两场,
//! ≥100ms 的数字纯静音占 18.6% / 20.6%,其中 45 段直接切在人声电平上(合计 19.3 秒);
//! 关掉后立刻是 0 段 0.0%,底噪回到 −45 dBFS 的真实房间声。这一切发生在音频进入本进程
//! **之前**,我们的采集链再干净也救不回来,而用户在应用里看不到任何提示——只能靠耳朵
//! 发现录音"掉字"。所以这里只做一件事:把系统状态读出来,交给 UI 明说。
//!
//! 实现取 `AVCaptureDevice.activeMicrophoneMode`(macOS 12+ 类属性)。用 `#[link]` 直接
//! 链 AVFoundation 而不是 dlopen:框架在任何 macOS 上都在,链上去即可让类在启动时可见;
//! 仍保留 respondsToSelector 与类查找的双重兜底,老系统上安静回落 Unknown。
//! 非 macOS 平台恒为 Unknown(Windows 的等价物是各家驱动的"AI 降噪",不在此列)。

/// 系统麦克风模式。`Unknown` = 读不到(老系统/非 macOS/API 缺失),UI 一律按"不提示"处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicMode {
    Standard,
    WideSpectrum,
    VoiceIsolation,
    Unknown,
}

impl MicMode {
    /// AVCaptureMicrophoneMode 原始值 → 枚举。未知取值不猜,回落 Unknown。
    pub fn from_raw(raw: i64) -> Self {
        match raw {
            0 => MicMode::Standard,
            1 => MicMode::WideSpectrum,
            2 => MicMode::VoiceIsolation,
            _ => MicMode::Unknown,
        }
    }

    /// 稳定的机读标识(前端按它判定是否告警,不要用中文文案判)。
    pub fn as_str(self) -> &'static str {
        match self {
            MicMode::Standard => "standard",
            MicMode::WideSpectrum => "wide_spectrum",
            MicMode::VoiceIsolation => "voice_isolation",
            MicMode::Unknown => "unknown",
        }
    }

    /// 这个模式会不会破坏录音。只有语音突显会——宽谱是"少处理",反而更保真。
    pub fn damages_audio(self) -> bool {
        matches!(self, MicMode::VoiceIsolation)
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::MicMode;
    use objc2::runtime::AnyClass;
    use objc2::{msg_send, sel};

    // 链上 AVFoundation:应用本身不依赖它,不显式链的话 AVCaptureDevice 这个类
    // 在运行时根本不存在,查找必然落空。
    #[link(name = "AVFoundation", kind = "framework")]
    unsafe extern "C" {}

    pub fn active() -> MicMode {
        let Some(cls) = AnyClass::get(c"AVCaptureDevice") else {
            return MicMode::Unknown;
        };
        // 类方法探测:respondsToSelector: 发给类对象查的正是类方法(macOS 12 以下没有)。
        let responds: bool = unsafe { msg_send![cls, respondsToSelector: sel!(activeMicrophoneMode)] };
        if !responds {
            return MicMode::Unknown;
        }
        let raw: isize = unsafe { msg_send![cls, activeMicrophoneMode] };
        MicMode::from_raw(raw as i64)
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::MicMode;
    pub fn active() -> MicMode {
        MicMode::Unknown
    }
}

/// 当前生效的麦克风模式。读不到一律 Unknown,调用方不得据此阻塞录制——
/// 这是提示,不是门禁。
pub fn active() -> MicMode {
    imp::active()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_values_map_to_apple_constants() {
        // AVCaptureMicrophoneMode:standard=0 wideSpectrum=1 voiceIsolation=2。
        assert_eq!(MicMode::from_raw(0), MicMode::Standard);
        assert_eq!(MicMode::from_raw(1), MicMode::WideSpectrum);
        assert_eq!(MicMode::from_raw(2), MicMode::VoiceIsolation);
    }

    #[test]
    fn unknown_raw_values_never_guess() {
        // 苹果将来加档位时,宁可不提示也不能错报成"语音突显"。
        for v in [-1, 3, 99] {
            assert_eq!(MicMode::from_raw(v), MicMode::Unknown);
            assert!(!MicMode::from_raw(v).damages_audio());
        }
    }

    #[test]
    fn only_voice_isolation_is_flagged_as_damaging() {
        assert!(MicMode::VoiceIsolation.damages_audio());
        // 宽谱是"少处理",比标准还保真,不该告警。
        assert!(!MicMode::WideSpectrum.damages_audio());
        assert!(!MicMode::Standard.damages_audio());
        assert!(!MicMode::Unknown.damages_audio());
    }

    #[test]
    fn machine_readable_ids_are_stable() {
        // 前端按这些字符串判定,改动即破坏契约。
        assert_eq!(MicMode::Standard.as_str(), "standard");
        assert_eq!(MicMode::VoiceIsolation.as_str(), "voice_isolation");
        assert_eq!(MicMode::WideSpectrum.as_str(), "wide_spectrum");
        assert_eq!(MicMode::Unknown.as_str(), "unknown");
    }

    /// 真机冒烟:本机读得到就必须是四档之一(不 panic、不返回野值)。
    #[test]
    fn reading_the_live_system_never_panics() {
        let m = active();
        assert!(matches!(
            m,
            MicMode::Standard | MicMode::WideSpectrum | MicMode::VoiceIsolation | MicMode::Unknown
        ));
    }
}
