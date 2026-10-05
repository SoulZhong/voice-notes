use super::{Recognizer, Transcript};

/// macOS 系统自带的 SpeechTranscriber(macOS 26 起)。省电档:识别跑在神经网络引擎上,
/// 2026-10-05 实测(#245,M5 Pro,3 段×10 分钟中文会议)CPU 时间约为 SenseVoice 的
/// 1/200、速度快 2-3 倍,中文准确率同档互有胜负。
/// 代价:不出语言标签(语言过滤走文本兜底);标点偏稀;模型由系统管理,不走
/// models 工件下载,而是 AssetInventory 装中文语言包(见 install)。
/// 桥接实现在 swift/apple_asr.swift;SDK < 26 的构建不编该桥(无 cfg apple_asr),
/// 此处全部落到「系统不支持」。
pub struct AppleRecognizer {
    _private: (),
}

/// 系统识别能力的状态,与 Swift 侧 vn_apple_asr_status 一一对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AppleAsrStatus {
    /// 系统版本低于 macOS 26,或本构建不含该桥(Windows / 旧 SDK)。
    Unsupported,
    /// 系统识别不支持中文。
    NoChinese,
    /// 中文语言包未安装,需要先 install。
    NeedsDownload,
    Ready,
}

impl AppleRecognizer {
    /// 语言包没装好时报错而不是建出一个一用就失败的识别器:开录前就该暴露。
    pub fn new() -> anyhow::Result<Self> {
        match status() {
            AppleAsrStatus::Ready => Ok(Self { _private: () }),
            AppleAsrStatus::NeedsDownload => anyhow::bail!("系统中文语音识别包未安装"),
            AppleAsrStatus::NoChinese => anyhow::bail!("系统语音识别不支持中文"),
            AppleAsrStatus::Unsupported => anyhow::bail!("系统语音识别需要 macOS 26 或更新版本"),
        }
    }
}

impl Recognizer for AppleRecognizer {
    fn engine_id(&self) -> &'static str {
        crate::settings::ASR_APPLE
    }

    fn recognize(&mut self, samples: &[f32]) -> anyhow::Result<Transcript> {
        ffi::recognize(samples)
    }
}

/// 状态缓存:就绪判定(models::status)在设置页、托盘、开录守卫里被频繁调用,
/// 每次都过一遍 Swift 查系统语言包不值得;状态只在 install 之后才会变。
static STATUS: std::sync::Mutex<Option<AppleAsrStatus>> = std::sync::Mutex::new(None);

pub fn status() -> AppleAsrStatus {
    let mut g = STATUS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    *g.get_or_insert_with(ffi::status)
}

/// 下载并安装中文语言包,阻塞到装完(可能几十秒到几分钟),调用方须放后台线程。
/// 无论成败都刷新状态缓存。
pub fn install() -> anyhow::Result<()> {
    let r = ffi::install();
    *STATUS.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ffi::status());
    r
}

#[derive(serde::Deserialize)]
#[cfg_attr(not(apple_asr), allow(dead_code))]
struct RawResult {
    text: String,
    tokens: Vec<String>,
    timestamps: Vec<f32>,
}

/// Swift 侧 JSON → Transcript。tokens 与 timestamps 不等长时丢掉两者(说话人细分
/// 退回段级),不让一个桥接异常拖垮整段文本。
#[cfg_attr(not(apple_asr), allow(dead_code))]
fn parse(json: &str) -> anyhow::Result<Transcript> {
    let raw: RawResult = serde_json::from_str(json)?;
    let (tokens, timestamps) = if raw.tokens.len() == raw.timestamps.len() {
        (raw.tokens, raw.timestamps)
    } else {
        (Vec::new(), Vec::new())
    };
    Ok(Transcript { text: raw.text.trim().to_string(), lang: String::new(), tokens, timestamps })
}

#[cfg(apple_asr)]
mod ffi {
    use super::{parse, AppleAsrStatus};
    use crate::asr::Transcript;
    use std::ffi::{c_char, CStr};

    extern "C" {
        fn vn_apple_asr_status() -> i32;
        fn vn_apple_asr_install(err_out: *mut *mut c_char) -> i32;
        fn vn_apple_asr_recognize(samples: *const f32, count: i64, out: *mut *mut c_char) -> i32;
        fn vn_apple_asr_free(p: *mut c_char);
    }

    /// 取走 Swift strdup 出来的字符串并释放。
    unsafe fn take(p: *mut c_char) -> String {
        if p.is_null() {
            return String::new();
        }
        let s = CStr::from_ptr(p).to_string_lossy().into_owned();
        vn_apple_asr_free(p);
        s
    }

    pub fn status() -> AppleAsrStatus {
        match unsafe { vn_apple_asr_status() } {
            3 => AppleAsrStatus::Ready,
            2 => AppleAsrStatus::NeedsDownload,
            1 => AppleAsrStatus::NoChinese,
            _ => AppleAsrStatus::Unsupported,
        }
    }

    pub fn install() -> anyhow::Result<()> {
        let mut err: *mut c_char = std::ptr::null_mut();
        let rc = unsafe { vn_apple_asr_install(&mut err) };
        let msg = unsafe { take(err) };
        if rc == 0 { Ok(()) } else { anyhow::bail!("安装系统中文语音识别包失败: {msg}") }
    }

    pub fn recognize(samples: &[f32]) -> anyhow::Result<Transcript> {
        let mut out: *mut c_char = std::ptr::null_mut();
        let rc = unsafe { vn_apple_asr_recognize(samples.as_ptr(), samples.len() as i64, &mut out) };
        let s = unsafe { take(out) };
        if rc != 0 {
            anyhow::bail!("系统语音识别失败: {s}");
        }
        parse(&s)
    }
}

#[cfg(not(apple_asr))]
mod ffi {
    use super::AppleAsrStatus;
    use crate::asr::Transcript;

    pub fn status() -> AppleAsrStatus {
        AppleAsrStatus::Unsupported
    }

    pub fn install() -> anyhow::Result<()> {
        anyhow::bail!("本构建不含系统语音识别")
    }

    pub fn recognize(_samples: &[f32]) -> anyhow::Result<Transcript> {
        anyhow::bail!("本构建不含系统语音识别")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_keeps_aligned_tokens() {
        let t = parse(r#"{"text":" 你好世界 ","tokens":["你好","世界"],"timestamps":[0.0,0.5]}"#).unwrap();
        assert_eq!(t.text, "你好世界");
        assert_eq!(t.tokens, vec!["你好", "世界"]);
        assert_eq!(t.timestamps, vec![0.0, 0.5]);
        assert!(t.lang.is_empty(), "不出语言标签,语言过滤走文本兜底");
    }

    #[test]
    fn parse_drops_misaligned_tokens_but_keeps_text() {
        let t = parse(r#"{"text":"你好","tokens":["你","好"],"timestamps":[0.0]}"#).unwrap();
        assert_eq!(t.text, "你好");
        assert!(t.tokens.is_empty() && t.timestamps.is_empty());
    }

    /// 真机冒烟:需要 macOS 26 且中文语言包已装。合成静音应得空文本且不崩。
    #[test]
    #[ignore]
    fn smoke_silence_and_status() {
        let st = status();
        eprintln!("apple asr status = {st:?}");
        if st != AppleAsrStatus::Ready {
            return;
        }
        let mut r = AppleRecognizer::new().unwrap();
        let t = r.recognize(&vec![0.0f32; 16000 * 2]).unwrap();
        assert!(t.text.chars().count() < 4, "静音不应出大段文本: {:?}", t.text);
        let empty = r.recognize(&[]).unwrap();
        assert!(empty.text.is_empty());
    }
}
