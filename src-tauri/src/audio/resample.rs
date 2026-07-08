/// 流式线性插值重采样(跨块相位连续)。
///
/// 为什么必须有状态:逐块调 `resample_linear` 会按块独立取整——cpal 麦克风回调
/// 典型 512 样本,512/3=170.67 每块 round 成 171,**每块凭空多 1/3 个样本**,
/// 时间轴被拉长 ~0.195%(1950ppm 虚拟时钟偏差)。后果(2026-07-08 腾讯会议
/// 录音实锤):①mic/system 两轨以 ~110ms/分钟 漂移(19 分钟差 2.4s),AEC3 的
/// 延迟估计(容忍 <100ppm 级)被甩脱锁,软件回声消除全程失效;②长录音的播放
/// 高亮/双轨同步渐进错位。本实现以全局样本计数定输出数(长程比值精确),
/// 尾样本跨块携带保插值连续。
pub struct StreamResampler {
    from: u32,
    to: u32,
    /// 已消费的输入样本总数(计入 tail 之前的历史)。
    consumed_in: u64,
    /// 已产出的输出样本总数。
    emitted_out: u64,
    /// 上一块最后一个样本:输出点落在块边界前后时的左端插值源。
    tail: f32,
}

impl StreamResampler {
    pub fn new(from: u32, to: u32) -> Self {
        Self { from, to, consumed_in: 0, emitted_out: 0, tail: 0.0 }
    }

    /// 输入采样率(设备中途换率时调用方据此判断是否需要重建)。
    pub fn from_rate(&self) -> u32 {
        self.from
    }

    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if self.from == self.to {
            return input.to_vec();
        }
        if input.is_empty() {
            return Vec::new();
        }
        let total_in = self.consumed_in + input.len() as u64;
        // 全局应产出总数:floor(总输入 × to/from)。用整数乘除避免 f64 在超长
        // 会话下的精度塌陷(u64 乘积在 to≤48k、百年时长内都不溢出 u128)。
        let target_total = (total_in as u128 * self.to as u128 / self.from as u128) as u64;
        let n_new = (target_total - self.emitted_out) as usize;
        let mut out = Vec::with_capacity(n_new);
        let ratio_in_per_out = self.from as f64 / self.to as f64;
        for j in 0..n_new {
            let global_out = (self.emitted_out + j as u64) as f64;
            // 该输出样本对应的全局源位置,折算成本块内下标(可为 -1..0 区间:
            // 落在上一块尾样本与本块首样本之间,由 tail 提供左端)。
            let src = global_out * ratio_in_per_out - self.consumed_in as f64;
            let idx = src.floor();
            let frac = (src - idx) as f32;
            let i = idx as isize;
            let s0 = if i < 0 { self.tail } else { input.get(i as usize).copied().unwrap_or(self.tail) };
            let s1 = input.get((i + 1).max(0) as usize).copied().unwrap_or(s0);
            out.push(s0 + (s1 - s0) * frac);
        }
        self.consumed_in = total_in;
        self.emitted_out += n_new as u64;
        self.tail = *input.last().unwrap();
        out
    }
}

/// 单声道线性插值重采样(整块一次性)。**只适用于完整缓冲**;逐块流式场景必须用
/// `StreamResampler`(见其文档:按块独立取整会注入 ~0.2% 时钟漂移)。
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

    /// 锁死漂移修复:512 样本块(cpal 典型回调)喂 60s 音频,产出总数必须精确等于
    /// floor(总输入/3)——旧的逐块 round 会多产出 ~0.195%(60s 即 ~117ms),就是
    /// 两轨漂移与 AEC 失锁的根因。
    #[test]
    fn stream_resampler_has_no_cumulative_drift_on_512_chunks() {
        let mut r = StreamResampler::new(48000, 16000);
        let chunk = vec![0.1f32; 512];
        let mut total_out = 0u64;
        let n_chunks = 48000 * 60 / 512; // ~60s
        for _ in 0..n_chunks {
            total_out += r.process(&chunk).len() as u64;
        }
        let total_in = n_chunks as u64 * 512;
        assert_eq!(total_out, total_in / 3, "长程比值必须精确,不得逐块取整累积");
        // 对照:旧实现每块 round(512/3)=171,累计多 {n}*(171-170.667)≈0.195%
        let old_total: u64 = n_chunks as u64 * 171;
        assert!(old_total > total_in / 3, "旧行为确实超产(测试前提自检)");
    }

    /// 跨块连续性:单调斜坡按怪异块长切开流式重采样,输出仍单调(块边界不跳变)。
    #[test]
    fn stream_resampler_is_continuous_across_chunks() {
        let input: Vec<f32> = (0..48000).map(|i| i as f32).collect(); // 1s 斜坡 @48k
        let mut r = StreamResampler::new(48000, 16000);
        let mut out = Vec::new();
        let mut pos = 0usize;
        for (k, size) in [511usize, 513, 512, 100, 3, 1024].iter().cycle().enumerate() {
            if pos >= input.len() {
                break;
            }
            let end = (pos + size).min(input.len());
            out.extend(r.process(&input[pos..end]));
            pos = end;
            let _ = k;
        }
        assert!((out.len() as i64 - 16000).abs() <= 1, "1s@16k 应 ≈16000 样本,实得 {}", out.len());
        for w in out.windows(2) {
            assert!(w[1] >= w[0], "块边界不得跳变(应单调不减)");
        }
    }

    #[test]
    fn stream_resampler_same_rate_passthrough() {
        let mut r = StreamResampler::new(16000, 16000);
        let x = vec![0.0, 0.5, 1.0];
        assert_eq!(r.process(&x), x);
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
