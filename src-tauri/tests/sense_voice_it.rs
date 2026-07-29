// 需要本地 SenseVoice 模型；默认 ignore，运行：
// cargo test --test sense_voice_it -- --ignored
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

/// CoreML provider 实验入口(2026-07-28 ASR 调研):对比 None(CPU)与
/// Some("coreml") 的加载可行性与耗时。sherpa 预编译二进制若未带 CoreML EP,
/// onnxruntime 会告警回退 CPU——测试仍应通过,看 stdout 耗时与告警判断是否生效。
#[test]
#[ignore]
fn sense_voice_coreml_provider_smoke_and_timing() {
    use app_lib::asr::{sense_voice::SenseVoiceRecognizer, Recognizer};
    let model_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("models/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17");
    let samples = read_wav_mono_16k(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/sample_zh_16k.wav"
    ));
    for provider in [None, Some("coreml".to_string())] {
        let label = provider.clone().unwrap_or_else(|| "cpu(默认)".into());
        let t0 = std::time::Instant::now();
        let mut rec = SenseVoiceRecognizer::new(&model_dir, provider)
            .unwrap_or_else(|e| panic!("provider={label} 加载失败: {e}"));
        let loaded = t0.elapsed();
        let t1 = std::time::Instant::now();
        let t = rec.recognize(&samples).expect("识别");
        println!(
            "provider={label}: 加载 {loaded:?}, 识别 {:?}, 文本: {}",
            t1.elapsed(),
            t.text
        );
        assert!(!t.text.is_empty(), "provider={label} 识别结果不应为空");
    }
}

#[test]
#[ignore]
fn sense_voice_transcribes_chinese() {
    use app_lib::asr::{sense_voice::SenseVoiceRecognizer, Recognizer};
    let model_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("models/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17");
    let samples = read_wav_mono_16k(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/sample_zh_16k.wav"
    ));
    let mut rec = SenseVoiceRecognizer::new(&model_dir, None).expect("加载 SenseVoice 模型");
    let t = rec.recognize(&samples).expect("识别");
    println!("SenseVoice 识别结果: {} (lang={:?}, tokens={}, timestamps={})", t.text, t.lang, t.tokens.len(), t.timestamps.len());
    assert!(!t.text.is_empty(), "识别结果不应为空");
    // 迁移契约(2026-07-28 sherpa-onnx 官方 crate 迁移):语言过滤强依赖 lang,
    // 段内说话人切分强依赖等长 token 时间戳——两者缺失都是静默降级,必须锚死。
    assert!(t.lang.contains("zh"), "SenseVoice 必须给出语言标签,实际: {:?}", t.lang);
    assert_eq!(t.tokens.len(), t.timestamps.len(), "token 时间戳必须与 tokens 等长");
    assert!(!t.timestamps.is_empty(), "SenseVoice 必须有 token 时间戳");
    assert!(
        t.text.chars().any(|c| c >= '\u{4e00}' && c <= '\u{9fff}'),
        "识别结果应含 CJK 汉字，实际: {}",
        t.text
    );
}
