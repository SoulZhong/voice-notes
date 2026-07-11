//! 说话人验证评测基准(长期保留,#[ignore]):用声纹库录音样本构建同人/异人配对,
//! 评测嵌入模型的跨场分辨力(同人对来自多样本人物——多样本主要经人工确认的合并
//! 带入,是可用的弱真值;首份样本为自动采集,存在少量标签噪声)。
//! 输出:同人/异人相似度分布、EER、最优准确率。用于模型选型(CAM++ vs ERes2NetV2
//! 等)与前端处理(响度归一)对比——改嵌入主链路前先在这把尺子上比过再动。
//!
//! 用法: EVAL_MODEL=<model.onnx> EVAL_VP=<voiceprints目录> [EVAL_LOUDNORM=1] \
//!       cargo test --test eval_speaker_verification -- --ignored --nocapture

use app_lib::diar::{SherpaEmbedder, SpeakerEmbedder};

fn norm(v: &[f32]) -> Vec<f32> {
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.iter().map(|x| x / n).collect()
}

#[test]
#[ignore]
fn eval_verification() {
    let model = std::env::var("EVAL_MODEL").expect("EVAL_MODEL");
    let vp_dir = std::env::var("EVAL_VP").expect("EVAL_VP");
    let loudnorm = std::env::var("EVAL_LOUDNORM").is_ok();
    let mut e = SherpaEmbedder::new(std::path::Path::new(&model)).expect("模型加载失败");

    // 样本文件名 P<n>[-slot].wav → 身份 = P<n>
    let mut items: Vec<(String, Vec<f32>)> = Vec::new();
    let mut files: Vec<_> = std::fs::read_dir(&vp_dir)
        .unwrap()
        .filter_map(|f| f.ok())
        .map(|f| f.path())
        .filter(|p| p.extension().map_or(false, |x| x == "wav"))
        .collect();
    files.sort();
    for p in files {
        let stem = p.file_stem().unwrap().to_string_lossy().to_string();
        let pid = stem.split('-').next().unwrap().to_string();
        let mut r = hound::WavReader::open(&p).unwrap();
        let mut s: Vec<f32> =
            r.samples::<i16>().filter_map(|x| x.ok()).map(|v| v as f32 / 32768.0).collect();
        if s.len() < 16_000 {
            continue;
        }
        if loudnorm {
            let rms = (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt();
            if rms > 1e-4 {
                let peak = s.iter().fold(0f32, |m, x| m.max(x.abs()));
                let k = (0.08 / rms).min(0.99 / peak.max(1e-6));
                for x in s.iter_mut() {
                    *x *= k;
                }
            }
        }
        if let Ok(v) = e.embed(&s) {
            items.push((pid, norm(&v)));
        }
    }

    let mut same = Vec::new();
    let mut diff = Vec::new();
    for i in 0..items.len() {
        for j in (i + 1)..items.len() {
            let s: f32 = items[i].1.iter().zip(&items[j].1).map(|(x, y)| x * y).sum();
            if items[i].0 == items[j].0 {
                same.push(s);
            } else {
                diff.push(s);
            }
        }
    }
    let mean = |v: &[f32]| v.iter().sum::<f32>() / v.len().max(1) as f32;

    // EER:扫阈值找 FAR≈FRR;顺带报最优总准确率。
    let (mut eer, mut eer_th, mut best_acc, mut best_th) = (1.0f32, 0.0f32, 0.0f32, 0.0f32);
    let mut t = -0.2f32;
    while t <= 1.0 {
        let frr = same.iter().filter(|&&s| s < t).count() as f32 / same.len().max(1) as f32;
        let far = diff.iter().filter(|&&s| s >= t).count() as f32 / diff.len().max(1) as f32;
        if (far - frr).abs() < (eer * 2.0 - 0.0).abs().max(0.005) && (far + frr) / 2.0 < eer {
            eer = (far + frr) / 2.0;
            eer_th = t;
        }
        let acc = (same.iter().filter(|&&s| s >= t).count() + diff.iter().filter(|&&s| s < t).count())
            as f32
            / (same.len() + diff.len()) as f32;
        if acc > best_acc {
            best_acc = acc;
            best_th = t;
        }
        t += 0.01;
    }

    println!(
        "样本={} 同人对={} 异人对={} loudnorm={}",
        items.len(),
        same.len(),
        diff.len(),
        loudnorm
    );
    println!(
        "同人: 均值={:.3} 最小={:.3} | 异人: 均值={:.3} 最大={:.3} | 间隔(同均-异均)={:.3}",
        mean(&same),
        same.iter().cloned().fold(f32::INFINITY, f32::min),
        mean(&diff),
        diff.iter().cloned().fold(f32::NEG_INFINITY, f32::max),
        mean(&same) - mean(&diff),
    );
    println!("EER≈{:.1}% @阈值 {:.2} | 最优准确率={:.1}% @阈值 {:.2}", eer * 100.0, eer_th, best_acc * 100.0, best_th);
}
