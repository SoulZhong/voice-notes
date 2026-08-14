//! 查询 CoreAudio 对默认输入设备的实测采样率(系统自己的漂移测量,当 DLL 旁证)。
//!
//! `kAudioDevicePropertyActualSampleRate` 在本仓库锁定的 coreaudio-sys 版本里已有绑定
//! (确认见 `target/*/build/coreaudio-sys-*/out/coreaudio.rs`),故直接从 `coreaudio::sys`
//! 取用,无需本地定义 FourCC `'asrt'`(0x61737274)。
/// 当前默认输入设备的 AudioDeviceID。会话起点解析一次即可——录音流在创建时就
/// 绑定到某个具体设备,此后用户改系统默认输入并不会把已开的流迁走。
/// (issue #100 条 2:旧实现每次轮询都重新解析默认设备,默认设备中途一变,
/// 旁证就短暂地测到了另一只设备,与本场录音无关。)
#[cfg(target_os = "macos")]
pub fn default_input_device_id() -> Option<u32> {
    use coreaudio::sys::*;
    unsafe {
        let mut dev: AudioDeviceID = 0;
        let mut size = std::mem::size_of::<AudioDeviceID>() as u32;
        let addr = AudioObjectPropertyAddress {
            mSelector: kAudioHardwarePropertyDefaultInputDevice,
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
            || dev == 0
        {
            return None;
        }
        Some(dev)
    }
}

/// 指定设备的实测采样率。轮询固定用会话起点解析出的设备,不跟随默认设备变化。
#[cfg(target_os = "macos")]
pub fn actual_hz_of(dev: u32) -> Option<f64> {
    use coreaudio::sys::*;
    unsafe {
        if dev == 0 {
            return None;
        }
        let mut rate: f64 = 0.0;
        let mut size = std::mem::size_of::<f64>() as u32;
        let addr = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyActualSampleRate,
            mScope: kAudioObjectPropertyScopeInput,
            mElement: kAudioObjectPropertyElementMaster,
        };
        if AudioObjectGetPropertyData(
            dev,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            &mut rate as *mut _ as *mut _,
        ) != 0
            || rate <= 0.0
        {
            return None;
        }
        Some(rate)
    }
}

/// 设备名(issue #100 条 6:报告里带上设备标识,drift_stats 才能按"最差设备组合"
/// 归类)。查询失败一律 None,纯诊断字段,不影响任何行为。
#[cfg(target_os = "macos")]
pub fn device_name(dev: u32) -> Option<String> {
    use coreaudio::sys::*;
    unsafe {
        if dev == 0 {
            return None;
        }
        let mut cf: CFStringRef = std::ptr::null();
        let mut size = std::mem::size_of::<CFStringRef>() as u32;
        let addr = AudioObjectPropertyAddress {
            mSelector: kAudioObjectPropertyName,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMaster,
        };
        if AudioObjectGetPropertyData(
            dev,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            &mut cf as *mut _ as *mut _,
        ) != 0
            || cf.is_null()
        {
            return None;
        }
        // CFString → UTF-8:先问所需字节数,再取。失败一律 None,并始终释放。
        let len = CFStringGetLength(cf);
        let max = CFStringGetMaximumSizeForEncoding(len, kCFStringEncodingUTF8) + 1;
        let mut buf = vec![0i8; max as usize];
        let ok = CFStringGetCString(cf, buf.as_mut_ptr(), max, kCFStringEncodingUTF8) != 0;
        CFRelease(cf as *const _);
        if !ok {
            return None;
        }
        let s = std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned();
        (!s.is_empty()).then_some(s)
    }
}

#[cfg(not(target_os = "macos"))]
pub fn default_input_device_id() -> Option<u32> {
    None
}

#[cfg(not(target_os = "macos"))]
pub fn actual_hz_of(_dev: u32) -> Option<f64> {
    None
}

#[cfg(not(target_os = "macos"))]
pub fn device_name(_dev: u32) -> Option<String> {
    None
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    /// 冒烟:CoreAudio 探测不 crash、可重复调用(结果取决于机器当前默认输入设备,
    /// 不做真值断言——真机冒烟兜底,与 mod.rs 里 `default_output_is_bluetooth` 的
    /// 冒烟测试同款风格)。
    #[test]
    fn device_probes_do_not_crash_and_are_stable() {
        let dev = super::default_input_device_id();
        assert_eq!(
            dev.is_some(),
            super::default_input_device_id().is_some(),
            "同一时刻重复解析默认输入设备应稳定"
        );
        let Some(dev) = dev else { return }; // CI 容器无音频设备:探测返回 None 即通过
        let a = super::actual_hz_of(dev);
        let b = super::actual_hz_of(dev);
        // 两次探测应给出同一结论(有/无值一致);具体数值受硬件抖动影响不作比较。
        assert_eq!(a.is_some(), b.is_some(), "同一时刻重复探测应稳定");
        if let Some(hz) = a {
            assert!(hz > 0.0 && hz < 1_000_000.0, "实测率应在合理量级: {hz}");
        }
        // 设备名:取得到就该非空;取不到(权限/驱动异常)返回 None 也是合法结论。
        if let Some(name) = super::device_name(dev) {
            assert!(!name.trim().is_empty(), "设备名不应是空串");
        }
    }

    /// 非法设备号一律 None,不 panic、不读野指针。
    #[test]
    fn zero_device_id_is_rejected() {
        assert!(super::actual_hz_of(0).is_none());
        assert!(super::device_name(0).is_none());
    }
}
