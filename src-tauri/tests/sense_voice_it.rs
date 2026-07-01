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
    let mut rec = SenseVoiceRecognizer::new(&model_dir).expect("加载 SenseVoice 模型");
    let t = rec.recognize(&samples).expect("识别");
    println!("SenseVoice 识别结果: {}", t.text);
    assert!(!t.text.is_empty(), "识别结果不应为空");
    assert!(
        t.text.chars().any(|c| c >= '\u{4e00}' && c <= '\u{9fff}'),
        "识别结果应含 CJK 汉字，实际: {}",
        t.text
    );
}
