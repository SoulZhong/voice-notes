//! 延迟估计(两期共用核心):10ms 能量包络 + 归一化互相关。
//! 纯函数无状态;置信度 = 主峰/次峰比(排除主峰 ±300ms 邻域)。
//! 门限不在本模块定——离线清洗(echo_clean)与二期实时侧各自持有并标定。

/// 10ms @16k 一帧。
const ENV_FRAME: usize = 160;
/// 包络帧毫秒数。
const ENV_FRAME_MS: u32 = 10;
/// 最少需要的重叠包络帧数(3s):再短互相关峰不可信。
const MIN_OVERLAP_FRAMES: usize = 300;
/// 次峰排除主峰邻域的半宽(帧,±300ms)。语音/音乐包络自相关宽度约数百 ms:
/// 邻域太窄会把主峰肩膀当"次峰",真回声的置信度被压到 1 附近(实施中踩过)。
const PEAK_EXCLUSION_FRAMES: i64 = 30;

#[derive(Debug, Clone, Copy)]
pub struct DelayEstimate {
    pub delay_ms: u32,
    /// 主峰/次峰比(次峰排除主峰±300ms邻域):峰的唯一性。无关信号下噪声大
    /// (少量有效样本的最大值比,实测可到 4+),不可单独作真回声判据。
    pub confidence: f32,
    /// 主峰 NCC 绝对值:回声强度。真回声(参考的衰减拷贝)接近 1,无关信号
    /// 通常 <0.2——与 confidence 联合门限才可靠。
    pub peak: f32,
}

/// 10ms RMS 能量包络。尾部不足一帧的样本并入最后一帧。
pub fn envelope(samples: &[f32]) -> Vec<f32> {
    samples
        .chunks(ENV_FRAME)
        .map(|c| (c.iter().map(|x| x * x).sum::<f32>() / c.len() as f32).sqrt())
        .collect()
}

/// 在 0..=max_delay_ms 搜索 obs 相对 ref 的延迟。输入为包络(envelope 的输出)。
/// 相关按去均值归一化(NCC);置信度=主峰/次峰(次峰排除主峰±PEAK_EXCLUSION_FRAMES帧)。
pub fn estimate_delay(ref_env: &[f32], obs_env: &[f32], max_delay_ms: u32) -> Option<DelayEstimate> {
    let max_lag = (max_delay_ms / ENV_FRAME_MS) as usize;
    let n = ref_env.len().min(obs_env.len());
    if n <= max_lag || n - max_lag < MIN_OVERLAP_FRAMES {
        return None;
    }
    let mean = |v: &[f32]| v.iter().sum::<f32>() / v.len() as f32;
    let rm = mean(&ref_env[..n]);
    let om = mean(&obs_env[..n]);
    let mut best = (0usize, f32::MIN);
    let mut scores = Vec::with_capacity(max_lag + 1);
    for lag in 0..=max_lag {
        // obs[t] 对齐 ref[t-lag],重叠区间 t ∈ [lag, n)
        let mut dot = 0.0f64;
        let mut nr = 0.0f64;
        let mut no = 0.0f64;
        for t in lag..n {
            let r = (ref_env[t - lag] - rm) as f64;
            let o = (obs_env[t] - om) as f64;
            dot += r * o;
            nr += r * r;
            no += o * o;
        }
        let score = if nr > 0.0 && no > 0.0 { (dot / (nr.sqrt() * no.sqrt())) as f32 } else { 0.0 };
        scores.push(score);
        if score > best.1 {
            best = (lag, score);
        }
    }
    if best.1 <= 0.0 {
        return None;
    }
    // 次峰:排除主峰 ±PEAK_EXCLUSION_FRAMES 帧邻域后的最大值。
    let second = scores
        .iter()
        .enumerate()
        .filter(|(i, _)| (*i as i64 - best.0 as i64).abs() > PEAK_EXCLUSION_FRAMES)
        .map(|(_, s)| *s)
        .fold(f32::MIN, f32::max);
    let confidence = if second > 1e-6 { best.1 / second } else { best.1 / 1e-6 };
    Some(DelayEstimate { delay_ms: best.0 as u32 * ENV_FRAME_MS, confidence, peak: best.1 })
}

/// 按 obs 时间轴分窗(win_ms)逐窗估计。ref 与 obs 取同一 [start..end) 窗口,
/// lag 语义即全局延迟;窗首 lag 帧因参考越窗不参与相关(60s 窗 vs ≤1.2s 延迟,
/// 损失可忽略)。不得给 ref 段前伸提前量——那会让真实延迟对应的 lag 变负,
/// 掉出 0..=max_lag 搜索域(实施中踩过:窗估计撞 1200ms 边界)。
pub fn estimate_windows(
    ref_env: &[f32],
    obs_env: &[f32],
    win_ms: u32,
    max_delay_ms: u32,
) -> Vec<Option<DelayEstimate>> {
    let win = (win_ms / ENV_FRAME_MS) as usize;
    let n = obs_env.len();
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < n {
        let end = (start + win).min(n);
        let end_r = end.min(ref_env.len());
        if start >= end_r {
            out.push(None);
        } else {
            out.push(estimate_delay(&ref_env[start..end_r], &obs_env[start..end], max_delay_ms));
        }
        start = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 分块调幅噪声:每 300ms 一个 LCG 随机增益,包络非周期,互相关峰唯一。
    /// (正弦调幅是周期的,相关峰会按周期重复,测不出真延迟。)
    pub(crate) fn block_modulated_noise(len: usize, seed: &mut u64) -> Vec<f32> {
        let block = 4800; // 300ms @16k
        let mut gain = 0.5f32;
        (0..len)
            .map(|i| {
                if i % block == 0 {
                    *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    gain = 0.1 + 0.9 * ((*seed >> 33) as f32 / (1u64 << 31) as f32).abs();
                }
                *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (((*seed >> 33) as f32 / (1u64 << 31) as f32) - 0.5) * gain
            })
            .collect()
    }

    #[test]
    fn envelope_is_10ms_rms_frames() {
        let s = vec![0.5f32; 320]; // 20ms 常值
        let env = envelope(&s);
        assert_eq!(env.len(), 2);
        assert!((env[0] - 0.5).abs() < 1e-4 && (env[1] - 0.5).abs() < 1e-4);
    }

    #[test]
    fn estimates_600ms_delay_within_20ms() {
        let mut seed = 3u64;
        let reference = block_modulated_noise(16_000 * 60, &mut seed); // 60s
        let delay = 9600; // 600ms
        let mut observed = vec![0.0f32; reference.len()];
        for i in delay..reference.len() {
            observed[i] = reference[i - delay] * 0.4;
        }
        let est = estimate_delay(&envelope(&reference), &envelope(&observed), 1200).unwrap();
        assert!((est.delay_ms as i64 - 600).unsigned_abs() <= 20, "估计 {}ms", est.delay_ms);
        assert!(est.confidence >= 2.0, "真回声峰唯一性应显著: {}", est.confidence);
        assert!(est.peak >= 0.5, "真回声主峰 NCC 应高: {}", est.peak);
    }

    #[test]
    fn unrelated_signals_yield_low_peak() {
        let mut s1 = 7u64;
        let mut s2 = 1234u64;
        let a = block_modulated_noise(16_000 * 60, &mut s1);
        let b = block_modulated_noise(16_000 * 60, &mut s2);
        let est = estimate_delay(&envelope(&a), &envelope(&b), 1200);
        // 无关信号的比值(confidence)噪声大不可断言;判别力在绝对峰值。
        if let Some(e) = est {
            assert!(e.peak < 0.25, "无关信号主峰 NCC 应低: {}", e.peak);
        }
    }

    #[test]
    fn windows_detect_drifted_delays() {
        let mut seed = 11u64;
        let reference = block_modulated_noise(16_000 * 120, &mut seed); // 120s
        let mut observed = vec![0.0f32; reference.len()];
        let half = reference.len() / 2;
        for i in 9600..half {
            observed[i] = reference[i - 9600] * 0.4; // 前半 600ms
        }
        for i in (half + 10_560)..reference.len() {
            observed[i] = reference[i - 10_560] * 0.4; // 后半 660ms
        }
        let wins = estimate_windows(&envelope(&reference), &envelope(&observed), 30_000, 1200);
        assert_eq!(wins.len(), 4); // 120s / 30s
        let d0 = wins[0].as_ref().unwrap().delay_ms as i64;
        let d3 = wins[3].as_ref().unwrap().delay_ms as i64;
        assert!((d0 - 600).unsigned_abs() <= 20 && (d3 - 660).unsigned_abs() <= 20,
            "分窗应跟上漂移: {d0} / {d3}");
    }

    #[test]
    fn too_short_input_returns_none() {
        assert!(estimate_delay(&[0.1; 10], &[0.1; 10], 1200).is_none());
    }
}
