// 需要本地 Qwen3-ASR 模型;默认 ignore,运行:
// cargo test --test qwen3_it -- --ignored --nocapture
use std::path::PathBuf;

fn read_wav_mono_16k(path: &str) -> Vec<f32> {
    let mut reader = hound::WavReader::open(path).expect("打开 WAV");
    let spec = reader.spec();
    assert_eq!(spec.sample_rate, 16000, "fixture 必须是 16kHz");
    assert_eq!(spec.channels, 1, "fixture 必须是单声道");
    match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().map(|s| s.unwrap()).collect(),
        hound::SampleFormat::Int => reader
            .samples::<i16>()
            .map(|s| s.unwrap() as f32 / 32768.0)
            .collect(),
    }
}

fn model_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("models/sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25")
}

#[test]
#[ignore]
fn qwen3_transcribes_chinese_and_reports_capability_shape() {
    use app_lib::asr::{qwen3::Qwen3Recognizer, Recognizer};
    let samples = read_wav_mono_16k(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/sample_zh_16k.wav"
    ));
    let t0 = std::time::Instant::now();
    let mut rec = Qwen3Recognizer::new(&model_dir(), None, None).expect("加载 Qwen3-ASR 模型");
    let loaded = t0.elapsed();
    let t1 = std::time::Instant::now();
    let t = rec.recognize(&samples).expect("识别");
    let audio_secs = samples.len() as f32 / 16000.0;
    let rtf = t1.elapsed().as_secs_f32() / audio_secs;
    println!(
        "Qwen3 加载 {loaded:?}, 识别 {:?} (音频 {audio_secs:.1}s, RTF {rtf:.3}), lang={:?}, tokens={}, timestamps={}, 文本: {}",
        t1.elapsed(),
        t.lang,
        t.tokens.len(),
        t.timestamps.len(),
        t.text
    );
    assert!(!t.text.is_empty(), "识别结果不应为空");
    assert!(
        t.text.chars().any(|c| c >= '\u{4e00}' && c <= '\u{9fff}'),
        "识别结果应含 CJK 汉字,实际: {}",
        t.text
    );
    // 能力形状记录(调研结论的回归锚点):LLM 解码无 token 时间戳 → timestamps 为空
    // 或与 tokens 不等长时,diarization 走段级降级(session.rs split_final)。
    // 若未来 sherpa 为 qwen3 补了时间戳,这条断言会提醒我们解锁段内切分。
    assert!(
        t.timestamps.is_empty() || t.timestamps.len() == t.tokens.len(),
        "timestamps 要么空要么与 tokens 等长,否则上游按缺失处理"
    );
}

/// 热词上下文偏置冒烟:带热词的识别器应能正常构造并出字(效果评估交给 bake-off)。
#[test]
#[ignore]
fn qwen3_accepts_hotwords_config() {
    use app_lib::asr::{qwen3::Qwen3Recognizer, Recognizer};
    let samples = read_wav_mono_16k(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/sample_zh_16k.wav"
    ));
    let mut rec = Qwen3Recognizer::new(&model_dir(), None, Some("项目进度,下一步计划".into()))
        .expect("带热词加载 Qwen3-ASR");
    let t = rec.recognize(&samples).expect("识别");
    assert!(!t.text.is_empty(), "热词配置下识别结果不应为空");
}
