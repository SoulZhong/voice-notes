// 需要本地模型；默认 ignore，运行：cargo test --test recognizer_it -- --ignored
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
fn whisper_transcribes_fixture() {
    use app_lib::asr::{whisper::WhisperRecognizer, Recognizer};
    let model_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("models/sherpa-onnx-whisper-base");
    let samples = read_wav_mono_16k(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/sample_16k.wav"
    ));
    let mut rec = WhisperRecognizer::new(&model_dir).expect("加载模型");
    let t = rec.recognize(&samples).expect("识别");
    println!("识别结果: {}", t.text);
    let lower = t.text.to_lowercase();
    assert!(
        lower.contains("hello") || lower.contains("world"),
        "识别结果应含预期关键词，实际: {}",
        t.text
    );
}
