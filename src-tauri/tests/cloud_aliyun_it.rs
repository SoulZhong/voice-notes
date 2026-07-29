//! 阿里云 DashScope ASR 真实 API 集成测试。默认 `#[ignore]`:要凭证、要网、要花钱。
//!
//! 跑法:
//! ```sh
//! VN_DASHSCOPE_API_KEY=... cargo test --test cloud_aliyun_it -- --ignored --nocapture
//! ```
//! 未设凭证时不 panic 而是打印跳过:CI 上"整体 --ignored 跑一遍"不该因为缺密钥红掉。

use app_lib::asr::cloud::{aliyun::AliyunAsr, CloudAsr, CloudEvent};
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

fn api_key() -> Option<String> {
    match std::env::var("VN_DASHSCOPE_API_KEY") {
        Ok(k) if !k.is_empty() => Some(k),
        _ => {
            eprintln!("跳过:未设 VN_DASHSCOPE_API_KEY");
            None
        }
    }
}

fn has_chinese(s: &str) -> bool {
    s.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
}

#[test]
#[ignore]
fn aliyun_streams_chinese_fixture() {
    let Some(key) = api_key() else { return };
    let asr = AliyunAsr::new(key);
    let mut stream = asr.open_stream().expect("开流");
    let samples = fixture();

    // 200ms 一帧(3200 样本 @16k)、按真实时钟节奏推:厂商按流速做端点检测,
    // 推快了会被限流,所以 sleep 必须等于帧长而不是它的一半。
    for chunk in samples.chunks(3200) {
        (stream.push)(chunk).expect("推流");
        std::thread::sleep(Duration::from_millis(200));
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
fn aliyun_batch_transcribes_fixture() {
    let Some(key) = api_key() else { return };
    let asr = AliyunAsr::new(key);
    let utterances = asr.transcribe_batch(&fixture()).expect("批式识别");
    let text: String = utterances.iter().map(|u| u.text.as_str()).collect();
    eprintln!("qwen3-flash: {text}");
    assert!(has_chinese(&text), "批式应识别出中文: {text}");
}

#[test]
#[ignore]
fn aliyun_batch_rejects_bad_credentials() {
    // 凭证错时必须报错,而不是把失败当成"这段没人说话"静默返回空。
    let asr = AliyunAsr::new("sk-not-a-real-key".into());
    let err = asr.transcribe_batch(&fixture()).expect_err("坏凭证应报错");
    let msg = format!("{err:#}");
    eprintln!("坏凭证错误: {msg}");
    // 光 expect_err 不够:断网/DNS 挂了也会报错,这条测试就会在"根本没打到厂商"
    // 的情况下假绿。所以要求错误里带上 HTTP 层拒绝的证据。
    // 匹配保持宽松(状态码或错误码任一即可):厂商随时可能调整具体状态码与文案,
    // 这里要守的是"请求确实到达并被拒",不是某个精确码。
    assert!(
        [
            "401",
            "403",
            "InvalidApiKey",
            "Invalid API-key",
            "AccessDenied"
        ]
        .iter()
        .any(|k| msg.contains(k)),
        "错误应含 HTTP 级拒绝证据(而非 DNS/离线失败): {msg}"
    );
}
