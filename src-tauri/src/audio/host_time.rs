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
    ticks_to_ns_with(ticks, numer, denom)
}

/// mach 时基换算的纯算术(抽出来是为了能直测非 1:1 时基:Apple Silicon 的
/// timebase 恰为 1:1,在本机跑 `mach_ticks_to_ns` 等于没验算术,而 Intel Mac
/// 上是 125/3 之类的真分数——算错就是整条 hw 时间轴错。issue #100 条 7)。
/// u128 中间量防溢出;denom==0 是非法时基,按 1:1 退化(与查询失败同待遇)。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn ticks_to_ns_with(ticks: u64, numer: u64, denom: u64) -> u64 {
    if denom == 0 {
        return ticks;
    }
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

    /// issue #100 条 7:mach 时基换算的非 1:1 分支从未被直测过。Apple Silicon 的
    /// timebase 恰是 1:1,`mach_ticks_to_ns` 在本机跑等于没验算术;Intel Mac 上
    /// 是 125/3 之类的真分数,算错就是整条 hw 时间轴错。把算术抽成纯函数直测,
    /// 不必等 Intel 机器冒烟。
    #[test]
    fn mach_ticks_scale_handles_non_unity_timebase() {
        // 1:1(Apple Silicon):原样透传
        assert_eq!(ticks_to_ns_with(1_000, 1, 1), 1_000);
        // 125/3(Intel Mac 常见):24 ticks = 1000ns
        assert_eq!(ticks_to_ns_with(24, 125, 3), 1_000);
        // 大数不得溢出:u64 上限量级的 ticks 走 u128 中间量
        assert_eq!(ticks_to_ns_with(u64::MAX / 125, 125, 1), (u64::MAX / 125) * 125);
        // 分母为 0 是非法时基,按 1:1 退化而不是除零 panic
        assert_eq!(ticks_to_ns_with(1_000, 125, 0), 1_000);
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
