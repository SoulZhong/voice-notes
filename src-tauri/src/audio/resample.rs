/// 单声道线性插值重采样。骨架阶段够用；高质量重采样（rubato）留待后续优化。
pub fn resample_linear(input: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if from_rate == to_rate || input.is_empty() {
        return input.to_vec();
    }
    let ratio = to_rate as f64 / from_rate as f64;
    let out_len = (input.len() as f64 * ratio).round() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_pos = i as f64 / ratio;
        let idx = src_pos.floor() as usize;
        let frac = (src_pos - idx as f64) as f32;
        let s0 = input.get(idx).copied().unwrap_or(0.0);
        let s1 = input.get(idx + 1).copied().unwrap_or(s0);
        out.push(s0 + (s1 - s0) * frac);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_rate_is_passthrough() {
        let x = vec![0.0, 0.5, 1.0];
        assert_eq!(resample_linear(&x, 16000, 16000), x);
    }

    #[test]
    fn downsample_48k_to_16k_thirds_the_length() {
        let input: Vec<f32> = (0..4800).map(|i| i as f32).collect(); // 0.1s @48k
        let out = resample_linear(&input, 48000, 16000);
        // 16k 下 0.1s ≈ 1600 个样本，容许 ±2
        assert!((out.len() as i64 - 1600).abs() <= 2, "len = {}", out.len());
    }

    #[test]
    fn linear_ramp_stays_monotonic() {
        let input: Vec<f32> = (0..300).map(|i| i as f32).collect();
        let out = resample_linear(&input, 44100, 16000);
        for w in out.windows(2) {
            assert!(w[1] >= w[0], "应单调不减");
        }
    }
}
