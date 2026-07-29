//! 火山云端 ASR 真实 API 集成测试。默认 `#[ignore]`:要凭证、要网、要花钱。
//!
//! 跑法:
//! ```sh
//! VN_VOLC_APP_KEY=... VN_VOLC_ACCESS_KEY=... \
//!   cargo test --test cloud_volcano_it -- --ignored --nocapture
//! ```
//! 未设凭证时不 panic 而是打印跳过:CI 上"整体 --ignored 跑一遍"不该因为缺密钥红掉。

use app_lib::asr::cloud::{volcano::VolcanoAsr, CloudAsr, CloudEvent};
use std::time::Duration;

/// 从 qwen3_it.rs 拷来的助手(集成测试各自独立编译,共享要另建 crate,不值当)。
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

fn fixture() -> Vec<f32> {
    read_wav_mono_16k(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/sample_zh_16k.wav"
    ))
}

fn creds() -> Option<(String, String)> {
    match (
        std::env::var("VN_VOLC_APP_KEY"),
        std::env::var("VN_VOLC_ACCESS_KEY"),
    ) {
        (Ok(app), Ok(key)) => Some((app, key)),
        _ => {
            eprintln!("跳过:未设 VN_VOLC_APP_KEY/VN_VOLC_ACCESS_KEY");
            None
        }
    }
}

fn has_chinese(s: &str) -> bool {
    s.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
}

#[test]
#[ignore]
fn volcano_streams_chinese_fixture() {
    let Some((app, key)) = creds() else { return };
    let asr = VolcanoAsr::new(app, key);
    let mut stream = asr.open_stream().expect("开流");
    let samples = fixture();

    // 200ms 一帧、按真实时钟节奏推:厂商按流速做端点检测,一次灌完会被限流。
    for chunk in samples.chunks(3200) {
        (stream.push)(chunk).expect("推流");
        std::thread::sleep(Duration::from_millis(100));
    }
    (stream.finish)().expect("末包");

    let mut text = String::new();
    let mut closed_error = None;
    let mut got_closed = false;
    while let Ok(ev) = stream.events.recv_timeout(Duration::from_secs(10)) {
        match ev {
            CloudEvent::Definite(u) => text.push_str(&u.text),
            CloudEvent::Interim { text: t } => eprintln!("interim: {t}"),
            CloudEvent::Closed { error } => {
                closed_error = error;
                got_closed = true;
                break;
            }
        }
    }
    assert!(got_closed, "应收到 Closed 事件");
    assert!(
        closed_error.is_none(),
        "我方 finish 后的关闭必须是 None(否则 worker 会当断连去重连): {closed_error:?}"
    );
    assert!(has_chinese(&text), "应识别出中文: {text}");
}

#[test]
#[ignore]
fn volcano_flash_batch_transcribes_fixture() {
    let Some((app, key)) = creds() else { return };
    let asr = VolcanoAsr::new(app, key);
    let utterances = asr.transcribe_batch(&fixture()).expect("批式识别");
    let text: String = utterances.iter().map(|u| u.text.as_str()).collect();
    eprintln!("flash: {text}");
    assert!(has_chinese(&text), "批式应识别出中文: {text}");
}

#[test]
#[ignore]
fn volcano_flash_rejects_bad_credentials() {
    // 凭证错时必须报错,而不是把失败当成"这段没人说话"静默返回空。
    let asr = VolcanoAsr::new("not-a-real-app".into(), "not-a-real-key".into());
    let err = asr.transcribe_batch(&fixture()).expect_err("坏凭证应报错");
    eprintln!("坏凭证错误: {err:#}");
}
