//! ASR 评测采集(提升方案 Phase 0 / 2026-08-11 调研 §5):同一批已保存笔记音频,
//! 本地引擎与云端 API 双跑,云端结果作对照 reference,产出 `tools/asr_eval.py`
//! 可直接消费的 JSONL(每引擎一份),最后用 `tools/asr_bench_report.py` 出对比表。
//!
//! 云端是"对照样例"不是真值:JSONL 落盘后可人工修订 reference 字段(听音频改错),
//! 修订后的文件即成为 golden 评测集——字段结构与 asr_eval.py 完全一致。
//!
//! 用法:
//!   asr_bench <note_dir>... [--engines sense_voice,qwen3[,paraformer,whisper]]
//!             [--cloud aliyun|volcano] [--app-data <dir>] [--out <dir>]
//!             [--hotwords "词1,词2"](仅 Qwen3 消费)
//!
//! 凭证解析顺序:环境变量(DASHSCOPE_API_KEY / VOLC_APP_KEY+VOLC_ACCESS_KEY)
//! → --app-data 目录下的 settings.json。模型根目录用 VN_MODELS 覆盖(默认与应用一致)。
//! 不带 --cloud 也能跑(只出本地引擎的 hypothesis,reference 留空待人工填)。

use app_lib::asr::cloud::CloudAsr;
use app_lib::pipeline::segmenter::Segmenter;
use app_lib::pipeline::silero::SileroSegmenter;
use app_lib::retranscribe::input::{DualTrackInput, PendingSegment, TranscribeInput};
use app_lib::{asr, models, new_recognizer, settings};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

struct Args {
    notes: Vec<PathBuf>,
    engines: Vec<String>,
    cloud: Option<String>,
    app_data: Option<PathBuf>,
    out: PathBuf,
    hotwords: Option<String>,
}

fn usage() -> ! {
    eprintln!(
        "用法: asr_bench <note_dir>... [--engines sense_voice,qwen3] [--cloud aliyun|volcano] \
         [--app-data <dir>] [--out <dir>] [--hotwords \"词1,词2\"]"
    );
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut notes = Vec::new();
    let mut engines = vec!["sense_voice".to_string(), "qwen3".to_string()];
    let mut cloud = None;
    let mut app_data = None;
    let mut out = None;
    let mut hotwords = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--engines" => {
                let v = it.next().unwrap_or_else(|| usage());
                engines = v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            }
            "--cloud" => cloud = Some(it.next().unwrap_or_else(|| usage())),
            "--app-data" => app_data = Some(PathBuf::from(it.next().unwrap_or_else(|| usage()))),
            "--out" => out = Some(PathBuf::from(it.next().unwrap_or_else(|| usage()))),
            "--hotwords" => hotwords = Some(it.next().unwrap_or_else(|| usage())),
            "-h" | "--help" => usage(),
            _ if a.starts_with("--") => usage(),
            _ => notes.push(PathBuf::from(a)),
        }
    }
    if notes.is_empty() {
        usage();
    }
    let out = out.unwrap_or_else(|| {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        PathBuf::from(format!("asr-bench-{ts}"))
    });
    Args { notes, engines, cloud, app_data, out, hotwords }
}

/// 凭证解析:环境变量优先,其次 --app-data 的 settings.json。
fn make_cloud(provider: &str, app_data: Option<&Path>) -> anyhow::Result<Box<dyn CloudAsr>> {
    let s = app_data.map(settings::load).unwrap_or_default();
    match provider {
        "aliyun" => {
            let key = std::env::var("DASHSCOPE_API_KEY")
                .ok()
                .filter(|k| !k.trim().is_empty())
                .unwrap_or_else(|| s.dashscope_api_key.trim().to_string());
            anyhow::ensure!(!key.is_empty(), "缺 DashScope API Key(env DASHSCOPE_API_KEY 或 --app-data)");
            Ok(Box::new(asr::cloud::aliyun::AliyunAsr::new(key)))
        }
        "volcano" => {
            let app_key = std::env::var("VOLC_APP_KEY")
                .ok()
                .filter(|k| !k.trim().is_empty())
                .unwrap_or_else(|| s.volc_app_key.trim().to_string());
            let access = std::env::var("VOLC_ACCESS_KEY")
                .ok()
                .filter(|k| !k.trim().is_empty())
                .unwrap_or_else(|| s.volc_access_key.trim().to_string());
            anyhow::ensure!(
                !app_key.is_empty() && !access.is_empty(),
                "缺火山凭证(env VOLC_APP_KEY/VOLC_ACCESS_KEY 或 --app-data)"
            );
            Ok(Box::new(asr::cloud::volcano::VolcanoAsr::new(app_key, access)))
        }
        other => anyhow::bail!("未知云端厂商: {other}(可选 aliyun/volcano)"),
    }
}

/// 一段音频的采集结果:云端对照 + 各引擎 hypothesis(与耗时)。
struct SegRow {
    note: String,
    source: String,
    start_ms: u64,
    end_ms: u64,
    reference: String,
    /// engine → (hypothesis, elapsed_ms)
    hyps: BTreeMap<String, (String, u128)>,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("asr_bench 失败: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let args = parse_args();
    std::fs::create_dir_all(&args.out)?;

    // 引擎常驻实例:整个批次只加载一次(Qwen3 加载要数秒/GB 级内存,逐段新建不可行)。
    let mut recognizers: Vec<(String, Box<dyn asr::Recognizer>)> = Vec::new();
    for name in &args.engines {
        eprintln!("加载引擎 {name}…");
        let r = new_recognizer(name, None, args.hotwords.clone())
            .map_err(|e| anyhow::anyhow!("引擎 {name} 加载失败(模型未下载? VN_MODELS 未指?): {e}"))?;
        recognizers.push((name.clone(), r));
    }
    let cloud = match &args.cloud {
        Some(p) => Some((p.clone(), make_cloud(p, args.app_data.as_deref())?)),
        None => None,
    };

    let vad_model = models::root().join("silero_vad.onnx");
    anyhow::ensure!(vad_model.exists(), "缺 VAD 模型: {vad_model:?}(VN_MODELS 指向应用模型目录)");

    let mut rows: Vec<SegRow> = Vec::new();
    let mut cloud_failures = 0usize;
    for note_dir in &args.notes {
        let note = note_dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| note_dir.display().to_string());
        eprintln!("[{note}] 解码与切段…");
        // 与应用同一切段器(同 VAD 参数含补帧):评测所见即线上所见。
        let vad = vad_model.clone();
        let mut input = DualTrackInput::new(
            note_dir.clone(),
            Box::new(move || Ok(Box::new(SileroSegmenter::new(&vad)?) as Box<dyn Segmenter>)),
        );
        let segments: Vec<PendingSegment> = input.segments()?;
        eprintln!("[{note}] {} 段", segments.len());

        for (i, seg) in segments.iter().enumerate() {
            let mut row = SegRow {
                note: note.clone(),
                source: seg.source.clone(),
                start_ms: seg.start_ms,
                end_ms: seg.end_ms,
                reference: String::new(),
                hyps: BTreeMap::new(),
            };
            for (name, rec) in recognizers.iter_mut() {
                let t0 = Instant::now();
                match rec.recognize(&seg.samples) {
                    Ok(t) => {
                        row.hyps.insert(name.clone(), (t.text, t0.elapsed().as_millis()));
                    }
                    Err(e) => eprintln!("[{note}] 段{i} 引擎 {name} 识别失败: {e}"),
                }
            }
            if let Some((pname, c)) = &cloud {
                match c.transcribe_batch(&seg.samples) {
                    Ok(utts) => {
                        row.reference = utts.iter().map(|u| u.text.as_str()).collect::<Vec<_>>().join("");
                    }
                    Err(e) => {
                        cloud_failures += 1;
                        eprintln!("[{note}] 段{i} 云端({pname})失败: {e}");
                    }
                }
            }
            if i % 20 == 0 {
                eprintln!("[{note}] 进度 {}/{}", i + 1, segments.len());
            }
            rows.push(row);
        }
    }

    // 每引擎一份 JSONL(自含 reference,可独立喂 asr_eval.py);行序 = 时间轴序。
    for name in args.engines.iter() {
        let path = args.out.join(format!("{name}.jsonl"));
        let mut f = std::fs::File::create(&path)?;
        for row in &rows {
            let Some((hyp, elapsed_ms)) = row.hyps.get(name) else { continue };
            let audio_ms = row.end_ms.saturating_sub(row.start_ms);
            let line = serde_json::json!({
                "note": row.note,
                "source": row.source,
                "start_ms": row.start_ms,
                "end_ms": row.end_ms,
                "audio_ms": audio_ms,
                "elapsed_ms": elapsed_ms,
                "reference": row.reference,
                "hypothesis": hyp,
            });
            writeln!(f, "{line}")?;
        }
        eprintln!("已写 {}", path.display());
    }
    let summary = serde_json::json!({
        "notes": args.notes.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "engines": args.engines,
        "cloud": args.cloud,
        "hotwords": args.hotwords,
        "segments": rows.len(),
        "cloud_failures": cloud_failures,
    });
    std::fs::write(args.out.join("run.json"), serde_json::to_string_pretty(&summary)?)?;
    eprintln!(
        "完成:{} 段,云端失败 {} 段。下一步: python3 tools/asr_bench_report.py {}",
        rows.len(),
        cloud_failures,
        args.out.display()
    );
    Ok(())
}
