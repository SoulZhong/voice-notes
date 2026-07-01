// 需要 VAD 模型；默认 ignore：cargo test --test segmenter_it -- --ignored
use std::path::PathBuf;

fn read_wav_16k(path: &str) -> Vec<f32> {
    let mut r = hound::WavReader::open(path).expect("wav");
    let spec = r.spec();
    assert_eq!(spec.sample_rate, 16000);
    match spec.sample_format {
        hound::SampleFormat::Float => r.samples::<f32>().map(|s| s.unwrap()).collect(),
        hound::SampleFormat::Int => r.samples::<i16>().map(|s| s.unwrap() as f32 / 32768.0).collect(),
    }
}

#[test]
#[ignore]
fn silero_segments_speech_then_silence() {
    use app_lib::pipeline::segmenter::Segmenter;
    use app_lib::pipeline::silero::SileroSegmenter;
    let model = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("models/silero_vad.onnx");
    let samples = read_wav_16k(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sample_16k.wav"));
    let mut seg = SileroSegmenter::new(&model).expect("load vad");
    // 按 ~30ms 块喂入，模拟真实节奏
    for chunk in samples.chunks(512) {
        seg.accept(chunk);
    }
    seg.flush();
    let finished = seg.take_finished();
    assert!(!finished.is_empty(), "应至少切出一个语句段");
    assert!(finished.iter().all(|s| !s.samples.is_empty()));
}
