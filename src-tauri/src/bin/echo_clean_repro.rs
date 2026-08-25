//! 回声清洗独立复现器(评测/验尸用):对一篇笔记的 mic/system 轨在**当前进程**
//! 里跑 `echo_clean::clean_wav`,输出写 /tmp,不动笔记本体。
//!
//! 为什么存在:2026-08-24 停录收尾在神经残余级(tract-onnx)内 SIGABRT,把整个
//! 应用带崩;应用当时是孤儿进程,panic 消息无人接,只有 .ips 崩溃报告。本工具
//! 让同一段音频在前台进程重放同一条路径——panic 文本直接落 stderr,修复后也用
//! 它验证降级路(「神经残余级失败,保留 AEC3 输出」)确实接住。
//!
//! 用法: echo_clean_repro <note_dir> <models_dir>

use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let (Some(note), Some(models)) = (args.next(), args.next()) else {
        eprintln!("用法: echo_clean_repro <note_dir> <models_dir>");
        std::process::exit(2);
    };
    let note = PathBuf::from(note);
    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(note.join("audio.json"))?)?;
    let off = |src: &str| -> u64 {
        meta["tracks"][src]["offset_ms"].as_u64().unwrap_or(0)
    };
    let mic = note.join("mic.wav");
    let system = note.join("system.wav");
    let out = std::env::temp_dir().join("echo_clean_repro.out.wav");
    eprintln!(
        "复现: mic={} system={} off=({}, {}) → {}",
        mic.display(),
        system.display(),
        off("mic"),
        off("system"),
        out.display()
    );
    let t0 = std::time::Instant::now();
    match app_lib::audio::echo_clean::clean_wav(
        &mic,
        &system,
        off("mic"),
        off("system"),
        &out,
        &PathBuf::from(models),
    ) {
        Ok(Some(r)) => println!("完成({:.1}s): 清洗报告 {r:?}", t0.elapsed().as_secs_f32()),
        Ok(None) => println!("完成({:.1}s): 判定无需清洗", t0.elapsed().as_secs_f32()),
        Err(e) => println!("完成({:.1}s): 清洗报错(未崩) {e:#}", t0.elapsed().as_secs_f32()),
    }
    Ok(())
}
