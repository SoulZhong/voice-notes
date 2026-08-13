//! 查询 CoreAudio 对默认输入设备的实测采样率(系统自己的漂移测量,当 DLL 旁证)。
//!
//! `kAudioDevicePropertyActualSampleRate` 在本仓库锁定的 coreaudio-sys 版本里已有绑定
//! (确认见 `target/*/build/coreaudio-sys-*/out/coreaudio.rs`),故直接从 `coreaudio::sys`
//! 取用,无需本地定义 FourCC `'asrt'`(0x61737274)。
#[cfg(target_os = "macos")]
pub fn default_input_actual_hz() -> Option<f64> {
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

#[cfg(not(target_os = "macos"))]
pub fn default_input_actual_hz() -> Option<f64> {
    None
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    /// 冒烟:CoreAudio 探测不 crash、可重复调用(结果取决于机器当前默认输入设备,
    /// 不做真值断言——真机冒烟兜底,与 mod.rs 里 `default_output_is_bluetooth` 的
    /// 冒烟测试同款风格)。
    #[test]
    fn default_input_actual_hz_probe_does_not_crash() {
        let a = super::default_input_actual_hz();
        let b = super::default_input_actual_hz();
        // 两次探测应给出同一结论(有/无值一致);具体数值受硬件抖动影响不作比较。
        assert_eq!(a.is_some(), b.is_some(), "同一时刻重复探测应稳定");
    }
}
