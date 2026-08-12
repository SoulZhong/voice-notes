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
        Some(ppm) => println!("互相关口径轨间漂移: {ppm:+.1} ppm(对照 drift_report.inter_track.rel_ppm)"),
        None => println!("有效窗不足,无法拟合"),
    }
}
