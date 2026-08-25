//! 声纹嵌入探针(评测专用):对一批 wav 逐个算嵌入,输出 JSON {文件名: 向量}。
//! 用来在**离开声纹库簿记**的前提下,直接测量嵌入模型在真实样本上的类内/类间
//! 分离度——库里质心是否被污染是另一回事,这里量的是模型与音频本身的地板。
//!
//! 用法: vp_probe <model.onnx> <wav_dir|wav...> > embs.json

use std::path::{Path, PathBuf};

use app_lib::diar::{SherpaEmbedder, SpeakerEmbedder};

fn read_wav(path: &Path) -> Option<Vec<f32>> {
    let mut r = hound::WavReader::open(path).ok()?;
    let spec = r.spec();
    let mut s: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => r
            .samples::<i16>()
            .filter_map(|v| v.ok())
            .map(|v| v as f32 / 32768.0)
            .collect(),
        hound::SampleFormat::Float => r.samples::<f32>().filter_map(|v| v.ok()).collect(),
    };
    if spec.channels > 1 {
        s = s.chunks(spec.channels as usize).map(|c| c.iter().sum::<f32>() / c.len() as f32).collect();
    }
    (s.len() >= 16_000).then_some(s)
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    anyhow::ensure!(args.len() >= 2, "用法: vp_probe <model.onnx> <wav_dir|wav...>");
    let mut e = SherpaEmbedder::new(Path::new(&args[0]))?;
    let mut files: Vec<PathBuf> = Vec::new();
    for a in &args[1..] {
        let p = PathBuf::from(a);
        if p.is_dir() {
            let mut v: Vec<PathBuf> = std::fs::read_dir(&p)?
                .filter_map(|d| d.ok().map(|d| d.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "wav"))
                .collect();
            v.sort();
            files.extend(v);
        } else {
            files.push(p);
        }
    }
    let mut out = serde_json::Map::new();
    for f in &files {
        let Some(s) = read_wav(f) else {
            eprintln!("跳过(过短/读不出): {}", f.display());
            continue;
        };
        match e.embed(&s) {
            Ok(v) => {
                let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                let unit: Vec<f32> = if n > 1e-6 { v.iter().map(|x| x / n).collect() } else { v };
                out.insert(
                    f.file_name().unwrap().to_string_lossy().into_owned(),
                    serde_json::json!({"vec": unit, "secs": s.len() as f32 / 16_000.0}),
                );
            }
            Err(err) => eprintln!("失败 {}: {err}", f.display()),
        }
    }
    eprintln!("完成 {} / {} 个文件", out.len(), files.len());
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}
