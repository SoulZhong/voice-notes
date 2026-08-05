//! 回放跨轨门控:构建 mic 轨压低区间,消混音重影("同一句话听两遍")。
//! 设计见 docs/superpowers/specs/2026-07-14-voice-notes-playback-crossgate-design.md。
//! 根因:软件 AEC 场次清洗后 mic 仍有对方残影,与 system 全电平同内容混播成
//! "同源迟到复本"。门控单向只压 mic;一切失败空表降级=现状(不门控)。
//!
//! 判据为什么是电平而不是转写段(2026-08-04 重做):
//!
//! 旧实现按转写段取差集 `system 活跃 ∧ mic 不活跃`,把"mic 有段"当双讲保护。
//! 但回声残影本身响到足以触发 mic 路 VAD 并被识别成 mic 段——真实笔记实测
//! 199 个 mic 段里 111 个(56%)是落在 system 段内、电平低 12dB 的残影。于是
//! 差集恰好把回声最响的地方挖成保护区,门控结构性地压不到该压的位置:
//! 分窗残余互相关过 0.30 的窗占比 29% → 只降到 17%。历次迭代都在调压低深度
//! (-15dB)与离线清洗门限,压根没碰到"位置错了"这个根。
//!
//! 现判据只看信号:把 system 包络回看 400ms 取峰(覆盖任何 0~400ms 的声学回
//! 路延迟,无须逐场估延迟),mic 帧电平低于它 6dB 即压低。依据是实测的分离
//! 度——纯近端说话帧的 mic/system 电平比中位 +63dB,回声帧中位 -26dB,两类
//! 隔着几十 dB,不存在边界含糊。四场真实笔记验证:残余互相关过 0.30 的窗
//! 29%/4%/6%/4% → 0%/0%/3%/0%,误压纯近端语音帧 0.0~0.9%(且那些帧按定义
//! 就比 system 低 6dB 以上,在混音里本就被盖住,压掉不损失可听内容)。

use std::path::Path;

/// -24dB:比旧值(-15dB)更深。判据改成实测电平后误压率已降到 <1%,且误压的
/// 帧按定义在混音里本就被盖住,可以换更大的消影余量(峰 p90 0.219→0.162)。
pub const DUCK_GAIN: f32 = 0.063;
/// 渐变沿 80ms@16k,落在区间内侧,防咔嗒。
const RAMP_SAMPLES: u64 = 1280;
/// 相邻压低区间间隙 <300ms 合并,防增益颤振。
const MERGE_GAP_MS: u64 = 300;
/// 孤立 <200ms 压低区间丢弃,不值得动增益。
const MIN_SPAN_MS: u64 = 200;
const SAMPLES_PER_MS: u64 = 16;

/// 帧长 20ms@16k:门控判据的时间分辨率。
const HOP: usize = 320;
/// 电平平滑窗(帧):100ms,滤掉音节级抖动,避免增益颤振。
const SMOOTH_FRAMES: usize = 5;
/// system 电平回看窗(帧):400ms。取窗内峰值当参考,等价于容忍 0~400ms 的任意
/// 回声延迟——实测各场声学回路延迟 165~245ms 且场内稳定,回看比逐场估延迟更
/// 稳(估错一次整场失效),实测两种做法的消影效果也在同一档。
const LOOKBACK_FRAMES: usize = 20;
/// mic 低于 system 参考多少即判为"无独立内容"(0.5 = -6dB)。
const ECHO_RATIO: f32 = 0.5;
/// system 绝对静音底:低于此值不认为对方在说话,不产生压低区间。
const SYS_ABS_FLOOR: f32 = 0.001;

#[derive(Debug, Clone, PartialEq)]
pub struct GateSpan {
    pub start: u64,
    pub end: u64,
}

/// 读 canonical WAV(44 字节头 + 16bit LE 单声道 16k)的逐帧 RMS,帧长 HOP。
/// 读不动/过短一律返回空——调用方据此降级为不门控。
///
/// 刻意只读一遍、只留下包络:一小时的轨也只剩 18 万个 f32(<1MB),
/// 与回放热路径的 mmap 各走各的,不额外常驻内存。
fn frame_rms_bytes(bytes: &[u8]) -> Vec<f32> {
    const HEADER: usize = 44;
    if bytes.len() <= HEADER {
        return Vec::new();
    }
    // 直接在 s16 字节流上分帧求 RMS,不先整轨 collect 成 Vec<f32>:一小时的轨
    // 那一步就是 230MB 的临时峰值(而且此刻两条完整 WAV 还都在内存里),而结果
    // 只需要 18 万个 f32(<1MB)。语义与逐样本分帧完全一致。
    let pcm = &bytes[HEADER..];
    let usable = pcm.len() / 2 * 2; // 丢掉半个样本的尾巴
    pcm[..usable]
        .chunks(HOP * 2)
        .map(|c| {
            let sumsq: f32 = c
                .chunks_exact(2)
                .map(|b| {
                    let v = i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0;
                    v * v
                })
                .sum();
            (sumsq / (c.len() / 2) as f32).sqrt()
        })
        .collect()
}

/// 把逐帧 RMS 按 offset_ms 摆到全局帧轴上,长度补齐到 total_frames(轨未覆盖处为 0)。
fn place(local: &[f32], offset_ms: u64, total_frames: usize) -> Vec<f32> {
    let off = (offset_ms * SAMPLES_PER_MS) as usize / HOP;
    let mut out = vec![0.0f32; total_frames];
    for (i, v) in local.iter().enumerate() {
        if let Some(slot) = out.get_mut(off + i) {
            *slot = *v;
        }
    }
    out
}

/// 移动平均平滑(窗 SMOOTH_FRAMES,边界收缩窗口,不引入相位偏移)。
fn smooth(v: &[f32]) -> Vec<f32> {
    let half = SMOOTH_FRAMES / 2;
    (0..v.len())
        .map(|i| {
            let lo = i.saturating_sub(half);
            let hi = (i + half + 1).min(v.len());
            v[lo..hi].iter().sum::<f32>() / (hi - lo) as f32
        })
        .collect()
}

/// 回声判据:mic 帧电平低于「system 回看窗内峰值」ECHO_RATIO 倍,且对方确实在说话。
/// 输入为已摆到同一全局帧轴上的逐帧 RMS。纯函数,便于直接测判据本身。
pub fn build_gate_from_levels(mic_rms: &[f32], sys_rms: &[f32]) -> Vec<GateSpan> {
    let n = mic_rms.len().min(sys_rms.len());
    if n == 0 {
        return Vec::new();
    }
    let mic = smooth(&mic_rms[..n]);
    let sys = smooth(&sys_rms[..n]);

    // system 回看峰:sref[i] = max(sys[i-LOOKBACK..=i])。
    let mut sref = vec![0.0f32; n];
    for i in 0..n {
        let lo = i.saturating_sub(LOOKBACK_FRAMES);
        sref[i] = sys[lo..=i].iter().copied().fold(0.0f32, f32::max);
    }

    // 「对方在说话」的动态底:取本场 sref 的中位数,再夹在 [10%, 50%] 的响电平
    // (p90)之间。中位数让判据自适应各场录音增益;上夹保证对方几乎全程在说的
    // 场次不会因为"中位数==说话电平"而一帧都判不出;下夹与绝对底一起防住
    // system 轨整体极安静时把底噪当成说话。
    let mut sorted = sref.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = sorted[n / 2];
    let loud = sorted[n * 9 / 10];
    let lo = (loud * 0.1).max(SYS_ABS_FLOOR);
    let hi = loud * 0.5;
    let floor = if lo <= hi { median.clamp(lo, hi) } else { lo };

    // 逐帧判定 → 帧区间。
    let mut spans_ms: Vec<(u64, u64)> = Vec::new();
    let frame_ms = |f: usize| (f * HOP) as u64 / SAMPLES_PER_MS;
    let mut run_start: Option<usize> = None;
    for i in 0..n {
        let ducked = sref[i] > floor && mic[i] < sref[i] * ECHO_RATIO;
        match (ducked, run_start) {
            (true, None) => run_start = Some(i),
            (false, Some(s)) => {
                spans_ms.push((frame_ms(s), frame_ms(i)));
                run_start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = run_start {
        spans_ms.push((frame_ms(s), frame_ms(n)));
    }

    // 间隙 <MERGE_GAP_MS 合并 → 短于 MIN_SPAN_MS 丢弃 → 换算采样。
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (s, e) in spans_ms {
        match merged.last_mut() {
            Some(last) if s.saturating_sub(last.1) < MERGE_GAP_MS => last.1 = e,
            _ => merged.push((s, e)),
        }
    }
    merged
        .into_iter()
        .filter(|(s, e)| e - s >= MIN_SPAN_MS)
        .map(|(s, e)| GateSpan { start: s * SAMPLES_PER_MS, end: e * SAMPLES_PER_MS })
        .collect()
}

/// 从两轨 canonical WAV 文件构建门控。任一轨读不出即空表(降级=不门控)。
pub fn build_gate_from_audio(
    mic_wav: &Path,
    mic_offset_ms: u64,
    system_wav: &Path,
    system_offset_ms: u64,
) -> Vec<GateSpan> {
    // mmap 而非 read:两轨同时在手,一小时双轨读进堆里是 ~230MB,而本函数只是扫一遍
    // 求逐帧 RMS(结果 <1MB)。页由系统按需调入/回收,与回放热路径同一套策略。
    let map = |p: &Path| -> Option<memmap2::Mmap> {
        let f = std::fs::File::open(p).ok()?;
        unsafe { memmap2::Mmap::map(&f).ok() }
    };
    let (Some(mic), Some(sys)) = (map(mic_wav), map(system_wav)) else {
        return Vec::new();
    };
    build_gate_from_wav_bytes(&mic, mic_offset_ms, &sys, system_offset_ms)
}

/// 同上,输入为已在内存里的 canonical WAV 字节(离线复现/测试用)。
pub fn build_gate_from_wav_bytes(
    mic_wav: &[u8],
    mic_offset_ms: u64,
    system_wav: &[u8],
    system_offset_ms: u64,
) -> Vec<GateSpan> {
    let mic = frame_rms_bytes(mic_wav);
    let sys = frame_rms_bytes(system_wav);
    if mic.is_empty() || sys.is_empty() {
        return Vec::new();
    }
    let total = (mic_offset_ms as usize * SAMPLES_PER_MS as usize / HOP + mic.len())
        .max(system_offset_ms as usize * SAMPLES_PER_MS as usize / HOP + sys.len());
    build_gate_from_levels(
        &place(&mic, mic_offset_ms, total),
        &place(&sys, system_offset_ms, total),
    )
}

/// 逐采样增益:区间外 1.0;区间内 DUCK_GAIN,边沿 80ms 线性渐变(落区间内侧)。
/// 二分定位,回放热路径每帧每轨一次,开销可忽略。
pub fn gain_at(spans: &[GateSpan], sample: u64) -> f32 {
    let idx = spans.partition_point(|sp| sp.end <= sample);
    let Some(sp) = spans.get(idx) else {
        return 1.0;
    };
    if sample < sp.start {
        return 1.0;
    }
    let into = sample - sp.start;
    let left = sp.end - sample; // sample < sp.end 由 partition_point 保证
    let depth = 1.0 - DUCK_GAIN;
    let g_attack = 1.0 - depth * (into.min(RAMP_SAMPLES) as f32 / RAMP_SAMPLES as f32);
    let g_release = 1.0 - depth * (left.min(RAMP_SAMPLES) as f32 / RAMP_SAMPLES as f32);
    g_attack.max(g_release)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 16; // 1ms = 16 采样
    /// 每帧 20ms:测试里用帧数表达时长更直观。
    fn frames(ms: usize) -> usize {
        ms / 20
    }
    /// 构造一条逐帧 RMS:按 (帧数, 电平) 段拼接。
    fn env(parts: &[(usize, f32)]) -> Vec<f32> {
        parts.iter().flat_map(|&(n, v)| std::iter::repeat_n(v, n)).collect()
    }

    /// 对方在说话、mic 只有低电平残影(-20dB)→ 整段压低。
    #[test]
    fn echo_residue_under_far_end_speech_is_ducked() {
        let sys = env(&[(frames(1000), 0.0), (frames(3000), 0.3), (frames(1000), 0.0)]);
        let mic = env(&[(frames(1000), 0.0), (frames(3000), 0.03), (frames(1000), 0.0)]);
        let g = build_gate_from_levels(&mic, &sys);
        assert_eq!(g.len(), 1, "应产出一段压低区间: {g:?}");
        // 起点=对方开口处;终点因 400ms 回看窗而略微外扩,不得早于对方结束。
        assert_eq!(g[0].start, 1000 * MS);
        assert!(
            g[0].end >= 4000 * MS && g[0].end <= 4500 * MS,
            "终点应覆盖到对方说完(允许回看窗外扩): {}",
            g[0].end / MS
        );
    }

    /// 双讲:mic 与 system 同时高电平 → 不压(近端真人声必须原样留下)。
    /// 这正是旧转写差集判据反过来做错的地方——它按"mic 有段"保护,结果保护的
    /// 是响的回声;这里按电平保护,保护的是响的近端人声。
    #[test]
    fn double_talk_is_not_ducked() {
        let sys = env(&[(frames(4000), 0.3)]);
        let mic = env(&[(frames(4000), 0.3)]);
        assert!(build_gate_from_levels(&mic, &sys).is_empty(), "双讲不得压低");
    }

    /// 近端独自说话(system 静音)→ 不压。
    #[test]
    fn near_end_only_speech_is_not_ducked() {
        let sys = env(&[(frames(4000), 0.0)]);
        let mic = env(&[(frames(4000), 0.2)]);
        assert!(build_gate_from_levels(&mic, &sys).is_empty(), "对方没说话就没有回声可压");
    }

    /// 响的回声(仅低 12dB,旧实现下会被 VAD 当成 mic 段而免于压低)照样要压。
    #[test]
    fn loud_echo_still_ducked() {
        let sys = env(&[(frames(3000), 0.4)]);
        let mic = env(&[(frames(3000), 0.1)]); // -12dB
        let g = build_gate_from_levels(&mic, &sys);
        assert_eq!(g.len(), 1, "-12dB 的响残影必须压: {g:?}");
    }

    /// 间隙 <300ms 合并,防增益颤振。
    #[test]
    fn short_gaps_between_ducked_runs_are_merged() {
        // 两段回声中间夹 200ms 双讲(mic 抬到与 system 同电平)→ 合并成一段
        let sys = env(&[(frames(1000), 0.3), (frames(200), 0.3), (frames(1000), 0.3)]);
        let mic = env(&[(frames(1000), 0.02), (frames(200), 0.3), (frames(1000), 0.02)]);
        let g = build_gate_from_levels(&mic, &sys);
        assert_eq!(g.len(), 1, "间隙 200ms(<300ms)应合并: {g:?}");
    }

    /// 对方只响了一下(100ms)、mic 全程无内容:产出的压低区间会被 400ms 回看窗
    /// 拖出一条尾巴——这是刻意的(回声本就比对方晚到),但尾巴必须有界,
    /// 不能把后面整段 mic 都压掉。
    #[test]
    fn brief_far_end_blip_yields_bounded_span() {
        let sys = env(&[(frames(1000), 0.0), (frames(100), 0.3), (frames(3000), 0.0)]);
        let mic = env(&[(frames(4100), 0.001)]);
        let g = build_gate_from_levels(&mic, &sys);
        assert_eq!(g.len(), 1, "应恰好一段: {g:?}");
        let dur_ms = (g[0].end - g[0].start) / MS;
        assert!(
            (100..=700).contains(&dur_ms),
            "区间长度应为 blip + 回看尾(≤400ms) + 帧粒度,实得 {dur_ms}ms"
        );
    }

    /// 两轨都极安静(无人说话)→ 绝对底挡住,不产生任何压低区间。
    #[test]
    fn silent_tracks_produce_no_spans() {
        let sys = env(&[(frames(5000), 0.0002)]);
        let mic = env(&[(frames(5000), 0.00001)]);
        assert!(build_gate_from_levels(&mic, &sys).is_empty(), "静音场不得门控");
    }

    /// 空输入/长度不等:取短者,不 panic。
    #[test]
    fn empty_or_ragged_input_degrades_to_no_gate() {
        assert!(build_gate_from_levels(&[], &[]).is_empty());
        assert!(build_gate_from_levels(&[0.0; 10], &[]).is_empty());
        let sys = env(&[(frames(3000), 0.3)]);
        let mic = env(&[(frames(500), 0.01)]);
        let _ = build_gate_from_levels(&mic, &sys); // 只要不 panic
    }

    /// 缺文件/过短文件 → 空表降级。
    #[test]
    fn missing_wav_degrades_to_no_gate() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("mic.wav");
        let b = dir.path().join("system.wav");
        assert!(build_gate_from_audio(&a, 0, &b, 0).is_empty(), "缺文件→空");
        std::fs::write(&a, [0u8; 10]).unwrap();
        std::fs::write(&b, [0u8; 10]).unwrap();
        assert!(build_gate_from_audio(&a, 0, &b, 0).is_empty(), "不足一个 WAV 头→空");
    }

    /// offset_ms 把轨摆到全局时间轴上:system 晚 1s 开始,压低区间也应整体后移。
    #[test]
    fn offset_shifts_spans_onto_global_timeline() {
        let sys_local = env(&[(frames(3000), 0.3)]);
        let mic_local = env(&[(frames(4000), 0.02)]);
        let total = frames(4000);
        let g = build_gate_from_levels(
            &place(&mic_local, 0, total),
            &place(&sys_local, 1000, total),
        );
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].start, 1000 * MS, "压低区间应跟着 system 的 offset 后移");
    }

    #[test]
    fn gain_ramps_inside_span_edges() {
        let spans = vec![GateSpan { start: 16_000, end: 48_000 }]; // 1s..3s
        assert_eq!(gain_at(&spans, 0), 1.0, "区间外恒 1.0");
        assert_eq!(gain_at(&spans, 15_999), 1.0);
        // 进沿 80ms(1280 采样)线性 1.0→DUCK_GAIN
        let mid_in = gain_at(&spans, 16_000 + 640);
        assert!((mid_in - (1.0 + DUCK_GAIN) / 2.0).abs() < 0.02, "进沿中点≈均值: {mid_in}");
        assert!((gain_at(&spans, 30_000) - DUCK_GAIN).abs() < 1e-6, "区间腹地=DUCK");
        // 出沿对称
        let mid_out = gain_at(&spans, 48_000 - 640);
        assert!((mid_out - (1.0 + DUCK_GAIN) / 2.0).abs() < 0.02);
        assert_eq!(gain_at(&spans, 48_000), 1.0);
    }
}
