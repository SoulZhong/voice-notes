//! 需真实声纹模型:VN_MODELS=1 cargo test --test embedder_it -- --ignored
use app_lib::diar::{SherpaEmbedder, SpeakerEmbedder};

#[test]
#[ignore]
fn embeds_fixture_to_fixed_dim_unit_scale_vector() {
    let model = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/models/3dspeaker_speech_campplus_sv_zh-cn_16k-common.onnx"
    );
    let mut e = SherpaEmbedder::new(std::path::Path::new(model)).expect("load");
    let wav = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sample_16k.wav");
    let mut reader = hound::WavReader::open(wav).expect("fixture");
    let samples: Vec<f32> = reader
        .samples::<i16>()
        .map(|s| s.unwrap() as f32 / i16::MAX as f32)
        .collect();
    let v1 = e.embed(&samples).expect("embed");
    assert!(!v1.is_empty(), "维度非零");
    let v2 = e.embed(&samples).expect("embed again");
    assert_eq!(v1.len(), v2.len(), "维度稳定");
    // 同段音频两次嵌入应几乎一致(余弦 ≈ 1)
    let dot: f32 = v1.iter().zip(&v2).map(|(a, b)| a * b).sum();
    let n1: f32 = v1.iter().map(|x| x * x).sum::<f32>().sqrt();
    let n2: f32 = v2.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!(dot / (n1 * n2) > 0.99, "同段自相似应≈1");
}
