//! host 时钟(mach 时基)统一换算:全仓唯一的 ticks→ns / CMTime→ns 入口。
//! 依据:`docs/2026-08-12-clock-drift-sensor-design.md` 第二节。

/// 当前 host 时刻(纳秒)。macOS 用 mach 时基(与 CoreAudio mHostTime、SCK PTS 同源);
/// 其他平台退化为进程内单调钟(仅保证单调,不与任何采集时间戳同源)。
pub fn now_ns() -> u64 {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: mach_absolute_time 无前置条件。
        mach_ticks_to_ns(unsafe { mach_absolute_time() })
    }
    #[cfg(not(target_os = "macos"))]
    {
        use std::sync::OnceLock;
        use std::time::Instant;
        static ORIGIN: OnceLock<Instant> = OnceLock::new();
        ORIGIN.get_or_init(Instant::now).elapsed().as_nanos() as u64
    }
}

#[cfg(target_os = "macos")]
extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

/// mach ticks → ns。timebase 只查一次(进程级常量)。
#[cfg(target_os = "macos")]
pub fn mach_ticks_to_ns(ticks: u64) -> u64 {
    use std::sync::OnceLock;
    static TB: OnceLock<(u64, u64)> = OnceLock::new();
    let (numer, denom) = *TB.get_or_init(|| {
        let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
        // SAFETY: 传合法指针;失败(非零返回)时退化为 1:1(Apple Silicon 实际即 1:1)。
        let rc = unsafe { mach_timebase_info(&mut info) };
        if rc == 0 && info.denom != 0 {
            (info.numer as u64, info.denom as u64)
        } else {
            (1, 1)
        }
    });
    (ticks as u128 * numer as u128 / denom as u128) as u64
}

#[cfg(not(target_os = "macos"))]
pub fn mach_ticks_to_ns(ticks: u64) -> u64 {
    ticks
}

/// CMTime → ns。value<0 或 timescale<=0(含 invalid 标志的常见形态)→ None。
pub fn cmtime_to_ns(value: i64, timescale: i32) -> Option<u64> {
    if value < 0 || timescale <= 0 {
        return None;
    }
    Some((value as u128 * 1_000_000_000 / timescale as u128) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_ns_is_monotonic_nondecreasing() {
        let a = now_ns();
        let b = now_ns();
        assert!(b >= a, "host 时钟必须单调不减: {a} -> {b}");
    }

    #[test]
    fn cmtime_converts_by_timescale() {
        // CMTime{value: 48_000, timescale: 48_000} = 1 秒
        assert_eq!(cmtime_to_ns(48_000, 48_000), Some(1_000_000_000));
        // 非法 timescale → None
        assert_eq!(cmtime_to_ns(1, 0), None);
        assert_eq!(cmtime_to_ns(-1, 48_000), None);
    }
}
