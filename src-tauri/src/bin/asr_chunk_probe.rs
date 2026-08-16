//! 判定实验(2026-08-16):长段被识别成一两个字时,**把同一段切短再解**能不能救回来?
//!
//! 背景:一场 7 分钟录音里,sense_voice 把一个 14.6 秒的段解成 "."(FireRed 同段解出
//! 40 个真字),这类"十几秒发言只转出两个字"的段全篇有 4 个。Codex 审计建议加
//! "低字数自动重试(切短块再解)",但那是**假设**——切短到底有没有用,必须先量。
//!
//! 本探针:对给定笔记的指定时间区间,用同一引擎分别按整段 / 若干块长解一遍,打印
//! 各自的字数与文本,供判断"重试策略选切块还是换引擎"。
//!
//! 用法:
//!   asr_chunk_probe <note_dir> <start_ms> <end_ms> [--engine sense_voice] [--chunks 5000,3000]
//!
//! 音频取 note 的 mic 轨(与线上转写同源:线上喂给识别器的就是这条轨的样本)。

use std::path::PathBuf;
use std::process::Command;

fn usage() -> ! {
    eprintln!(
        "用法: asr_chunk_probe <note_dir> <start_ms> <end_ms> [--engine sense_voice] \
         [--chunks 5000,3000]"
    );
    std::process::exit(2)
}

/// 用 ffmpeg 取 mic 轨的一段,转成 16k 单声道 f32。与识别器输入口径一致。
fn decode_span(note_dir: &PathBuf, start_ms: u64, end_ms: u64) -> anyhow::Result<Vec<f32>> {
    let path = note_dir.join("mic.m4a");
    anyhow::ensure!(path.exists(), "缺 mic.m4a: {path:?}");
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-ss"])
        .arg(format!("{:.3}", start_ms as f64 / 1000.0))
        .arg("-t")
        .arg(format!("{:.3}", (end_ms - start_ms) as f64 / 1000.0))
        .arg("-i")
        .arg(&path)
        .args(["-ac", "1", "-ar", "16000", "-f", "f32le", "-"])
        .output()?;
    anyhow::ensure!(out.status.success(), "ffmpeg 解码失败");
    Ok(out
        .stdout
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect())
}

fn main() -> anyhow::Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.len() < 3 {
        usage()
    }
    let note_dir = PathBuf::from(&argv[0]);
    let start_ms: u64 = argv[1].parse().unwrap_or_else(|_| usage());
    let end_ms: u64 = argv[2].parse().unwrap_or_else(|_| usage());
    let mut engine = "sense_voice".to_string();
    let mut chunks_ms: Vec<u64> = vec![5000, 3000];
    let mut i = 3;
    while i < argv.len() {
        match argv[i].as_str() {
            "--engine" => {
                engine = argv.get(i + 1).cloned().unwrap_or_else(|| usage());
                i += 2;
            }
            "--chunks" => {
                chunks_ms = argv
                    .get(i + 1)
                    .unwrap_or_else(|| usage())
                    .split(',')
                    .filter_map(|s| s.trim().parse().ok())
                    .collect();
                i += 2;
            }
            _ => usage(),
        }
    }

    let samples = decode_span(&note_dir, start_ms, end_ms)?;
    eprintln!(
        "区间 {start_ms}-{end_ms}ms({:.1}s),{} 样本 @16k,引擎 {engine}",
        (end_ms - start_ms) as f64 / 1000.0,
        samples.len()
    );

    let mut rec = app_lib::new_recognizer(&engine, None, None)?;
    let whole = rec.recognize(&samples)?;
    println!("整段: {} 字 | {:?}", whole.text.chars().count(), whole.text);

    for cms in chunks_ms {
        let step = (cms as usize) * 16; // 16 样本/ms @16k
        let mut texts = Vec::new();
        let mut total = 0usize;
        for part in samples.chunks(step) {
            // 太短的尾块不喂:识别器对 <0.3s 的碎片基本只会吐标点。
            if part.len() < 16 * 300 {
                continue;
            }
            let t = rec.recognize(part)?;
            total += t.text.chars().count();
            texts.push(t.text);
        }
        println!("{cms}ms 块: {total} 字 | {:?}", texts.join(" / "));
    }
    Ok(())
}
