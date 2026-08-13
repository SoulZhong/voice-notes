//! E1 标定:互相关直接量两条 WAV 的真实错位曲线,校准 DLL 传感器
//! (docs/2026-08-12-clock-drift-sensor-design.md 第五节 E1)。

/// 归一化互相关:在 b 中搜索与 a[t0..t0+win] 最相似的偏移(±search 样本)。
/// 返回 (best_offset_samples, peak_corr)。互相关无峰(能量过低)→ None。
fn xcorr_offset(a: &[f32], b: &[f32], t0: usize, win: usize, search: i64) -> Option<(i64, f32)> {
    let a_seg = a.get(t0..t0 + win)?;
    let ea: f32 = a_seg.iter().map(|x| x * x).sum();
    if ea < 1e-6 { return None; }
    let mut best = (0i64, f32::MIN);
    for off in -search..=search {
        let start = t0 as i64 + off;
        if start < 0 { continue; }
        let Some(b_seg) = b.get(start as usize..start as usize + win) else { continue };
        let eb: f32 = b_seg.iter().map(|x| x * x).sum();
        if eb < 1e-6 { continue; }
        let dot: f32 = a_seg.iter().zip(b_seg).map(|(x, y)| x * y).sum();
        let corr = dot / (ea.sqrt() * eb.sqrt());
        if corr > best.1 { best = (off, corr); }
    }
    (best.1 > 0.5).then_some(best)
}

/// 对 (t_s, offset_ms) 序列做最小二乘线性拟合,返回斜率(ms/s → ×1000 = ppm)。
fn linear_slope_ppm(points: &[(f64, f64)]) -> Option<f64> {
    if points.len() < 2 { return None; }
    let n = points.len() as f64;
    let (sx, sy): (f64, f64) = points.iter().fold((0.0, 0.0), |(a, b), p| (a + p.0, b + p.1));
    let (mx, my) = (sx / n, sy / n);
    let num: f64 = points.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
    let den: f64 = points.iter().map(|p| (p.0 - mx) * (p.0 - mx)).sum();
    (den > 0.0).then(|| num / den * 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_known_offset() {
        // a = 白噪(确定性伪随机);b = a 右移 37 样本
        let mut seed = 1u64;
        let mut noise = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1); (seed >> 40) as f32 / 8388608.0 - 1.0 };
        let a: Vec<f32> = (0..32_000).map(|_| noise()).collect();
        let mut b = vec![0.0f32; 37];
        b.extend_from_slice(&a);
        let (off, corr) = xcorr_offset(&a, &b, 8_000, 1_600, 100).unwrap();
        assert_eq!(off, 37);
        assert!(corr > 0.9);
    }

    #[test]
    fn slope_recovers_ppm() {
        // 100ppm ⇒ 每秒错位增长 0.1ms
        let pts: Vec<(f64, f64)> = (0..30).map(|k| (k as f64 * 10.0, k as f64 * 10.0 * 0.1)).collect();
        assert!((linear_slope_ppm(&pts).unwrap() - 100.0).abs() < 1e-6);
    }

    /// 符号回归:锁死 xcorr_offset + linear_slope_ppm 的符号约定,防止调用顺序/公式改动
    /// 后再次把 E1 标定的判据方向搞反(见 scripts/drift-calibration.md)。
    ///
    /// 物理模型:存在一条"真实内容"时间线 src。轨道 x 的第 n 个采样对应真实时间
    /// t = n / (R·(1+e_x)),即它在 src 上取的是第 n/(1+e_x) 个样本(四舍五入)。
    /// e_x > 0 表示该轨道的实际采样率快于标称值(单位时间录得更多样本)。
    ///
    /// 构造 mic 偏快 +80ppm、system 偏慢 -80ppm,则
    /// inter_track.rel_ppm = mic.rate_ppm - system.rate_ppm = +160ppm。
    /// 按标定文档 Step 4 修正后的调用顺序 xcorr_align system.wav mic.wav(即 a=system,b=mic),
    /// 推导(并经独立数值模拟验证)得 linear_slope_ppm(a,b) ≈ rate_ppm(b) - rate_ppm(a)
    /// = mic.rate_ppm - system.rate_ppm = rel_ppm,应直接读出 +160ppm 左右(而不是 -160ppm)。
    #[test]
    fn slope_sign_matches_rel_ppm_with_system_then_mic_order() {
        let mut seed = 7u64;
        let mut noise = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1); (seed >> 40) as f32 / 8388608.0 - 1.0 };
        let src_len = 700_000usize;
        let src: Vec<f32> = (0..src_len).map(|_| noise()).collect();

        // 按上述物理模型,从公共内容时间线 src 重采样出一条偏快/偏慢 rate_ppm 的轨道。
        let build = |rate_ppm: f64, n_out: usize| -> Vec<f32> {
            let e = rate_ppm / 1e6;
            (0..n_out)
                .map(|n| {
                    let idx = (n as f64 / (1.0 + e)).round();
                    if idx < 0.0 || idx as usize >= src_len { 0.0 } else { src[idx as usize] }
                })
                .collect()
        };

        let r = 8_000f64;
        let n_out = 650_000usize;
        let mic = build(80.0, n_out); // mic 实际采样率偏快 +80ppm
        let system = build(-80.0, n_out); // system 实际采样率偏慢 -80ppm
        let rel_ppm = 80.0 - (-80.0); // = 160.0,对照 drift_report.inter_track.rel_ppm 口径

        // 调用顺序 a=system, b=mic,对齐文档 Step 4 修正后的顺序。
        let (win, step, search) = (800usize, 40_000usize, 300i64);
        let mut pts = Vec::new();
        let mut t0 = 0usize;
        while t0 + win < system.len().min(mic.len()) {
            if let Some((off, _corr)) = xcorr_offset(&system, &mic, t0, win, search) {
                let t_s = t0 as f64 / r;
                let off_ms = off as f64 * 1000.0 / r;
                pts.push((t_s, off_ms));
            }
            t0 += step;
        }
        let ppm = linear_slope_ppm(&pts).expect("应能从多窗采样点拟合出斜率");

        assert!(
            (ppm - rel_ppm).abs() < rel_ppm.abs() * 0.2,
            "符号/数值回归失败:mic 实际偏快 +80ppm、system 实际偏慢 -80ppm 时 rel_ppm=mic-system={rel_ppm:+.1}ppm;\
             xcorr_align 以 (a=system, b=mic) 顺序调用时 linear_slope_ppm 应 ≈ +{rel_ppm:.1}ppm(即 rate_ppm(b)-rate_ppm(a)),\
             容差 ±20%,实得 {ppm:+.1}ppm。若变号或量级偏差过大,说明 xcorr_offset/linear_slope_ppm 的符号约定或\
             drift-calibration.md 里约定的调用顺序(system.wav 在前、mic.wav 在后)被破坏了。"
        );
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (pa, pb) = (args.next().expect("用法: xcorr_align <a.wav> <b.wav>"), args.next().expect("缺第二个 wav"));
    let read = |p: &str| -> (Vec<f32>, u32) {
        let mut r = hound::WavReader::open(p).expect("打不开 wav");
        let spec = r.spec();
        assert_eq!(spec.channels, 1, "只支持 mono");
        let v = r.samples::<i16>().map(|s| s.unwrap() as f32 / 32768.0).collect();
        (v, spec.sample_rate)
    };
    let (a, ra) = read(&pa);
    let (b, rb) = read(&pb);
    assert_eq!(ra, rb, "两轨采样率必须一致");
    let (win, step, search) = (ra as usize, ra as usize * 10, ra as i64 / 4);
    let mut pts = Vec::new();
    let mut t0 = 0usize;
    println!("t_s\toffset_ms\tcorr");
    while t0 + win < a.len().min(b.len()) {
        if let Some((off, corr)) = xcorr_offset(&a, &b, t0, win, search) {
            let (t_s, off_ms) = (t0 as f64 / ra as f64, off as f64 * 1000.0 / ra as f64);
            println!("{t_s:.0}\t{off_ms:+.2}\t{corr:.2}");
            pts.push((t_s, off_ms));
        }
        t0 += step;
    }
    match linear_slope_ppm(&pts) {
        // 符号约定:linear_slope_ppm(a, b) ≈ rate_ppm(b) - rate_ppm(a),即正值 = 第二个文件
        // (b)相对第一个文件(a)实际采样率偏快。按标定文档顺序 xcorr_align system.wav mic.wav
        // 调用(a=system, b=mic)时,本值直接对齐 drift_report.inter_track.rel_ppm
        // = mic.rate_ppm - system.rate_ppm,可与之直接作差比较,无需再取反。
        Some(ppm) => println!(
            "互相关口径轨间漂移: {ppm:+.1} ppm(正值=第二个文件相对第一个偏快;若按 <system.wav> <mic.wav> 顺序调用,口径对齐 drift_report.inter_track.rel_ppm = mic.rate_ppm - system.rate_ppm)"
        ),
        None => println!("有效窗不足,无法拟合"),
    }
}
