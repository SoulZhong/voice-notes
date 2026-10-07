//! 听写识别引擎的选择(settings.dictation_engine)。
//!
//! - macOS「自动」:Apple 语音识别(流式、本机、带热词),不可用(没给权限、没有
//!   中文本机识别资源)时退回 sherpa 两步识别——每句开始时判定,权限补上后自动回到 Apple。
//! - Windows:只有 sherpa 两步识别。
//!
//! 听写引擎与会议录制的 asr_model 分开设置:听写要快,录会要准。

use super::sherpa_stream::SherpaRecognizer;
use std::path::PathBuf;
use vibe_device::runtime::{RecogSink, RecognizerFactory};
#[cfg(target_os = "macos")]
use vibe_device::session::{Recognizer, RecognizerHealth, RecognizerStartError};

/// 按设置造识别器工厂(识别器本身在设备主循环线程上构造)。
pub fn factory(engine: String, hotwords: Vec<String>, models_root: PathBuf) -> RecognizerFactory {
    #[cfg(target_os = "macos")]
    {
        use vibe_device::speech_apple::AppleRecognizer;
        Box::new(move |sink: RecogSink| -> Box<dyn vibe_device::session::Recognizer> {
            match engine.as_str() {
                crate::settings::DICTATION_SHERPA => Box::new(SherpaRecognizer::new(models_root, sink)),
                crate::settings::DICTATION_APPLE => Box::new(AppleRecognizer::new(sink, hotwords)),
                _ => Box::new(AutoRecognizer::new(sink, hotwords, models_root)),
            }
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (engine, hotwords);
        Box::new(move |sink: RecogSink| -> Box<dyn vibe_device::session::Recognizer> {
            Box::new(SherpaRecognizer::new(models_root, sink))
        })
    }
}

/// 本次听写由谁在识别。
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Which {
    Apple,
    Sherpa,
}

/// macOS「自动」档:Apple 优先,不可用退回 sherpa。
#[cfg(target_os = "macos")]
struct AutoRecognizer {
    apple: vibe_device::speech_apple::AppleRecognizer,
    sherpa: Option<SherpaRecognizer>,
    sink: RecogSink,
    models_root: PathBuf,
    current: Which,
}

#[cfg(target_os = "macos")]
impl AutoRecognizer {
    fn new(sink: RecogSink, hotwords: Vec<String>, models_root: PathBuf) -> Self {
        Self {
            apple: vibe_device::speech_apple::AppleRecognizer::new(sink.clone(), hotwords),
            sherpa: None,
            sink,
            models_root,
            current: Which::Apple,
        }
    }

    fn sherpa(&mut self) -> &mut SherpaRecognizer {
        let (root, sink) = (self.models_root.clone(), self.sink.clone());
        self.sherpa.get_or_insert_with(|| SherpaRecognizer::new(root, sink))
    }

    fn sherpa_available(&self) -> bool {
        super::sherpa_stream::available(&self.models_root)
    }
}

#[cfg(target_os = "macos")]
impl Recognizer for AutoRecognizer {
    fn health(&mut self) -> RecognizerHealth {
        match self.apple.health() {
            RecognizerHealth::Ready => RecognizerHealth::Ready,
            _ if self.sherpa_available() => RecognizerHealth::Ready,
            other => other,
        }
    }

    fn start(&mut self, dict: u8) -> Result<(), RecognizerStartError> {
        if self.apple.health() == RecognizerHealth::Ready || !self.sherpa_available() {
            self.current = Which::Apple;
            return self.apple.start(dict);
        }
        eprintln!("听写 {dict}: Apple 语音识别不可用,改用 sherpa");
        self.current = Which::Sherpa;
        self.sherpa().start(dict)
    }

    fn push(&mut self, pcm: &[i16]) {
        match self.current {
            Which::Apple => self.apple.push(pcm),
            Which::Sherpa => self.sherpa().push(pcm),
        }
    }

    fn finish(&mut self) {
        match self.current {
            Which::Apple => self.apple.finish(),
            Which::Sherpa => self.sherpa().finish(),
        }
    }

    fn cancel(&mut self) {
        self.apple.cancel();
        if let Some(s) = self.sherpa.as_mut() {
            s.cancel();
        }
    }
}

/// 这个引擎设置下,听写还缺哪些模型工件(models::ARTIFACTS 的 id)。
/// macOS 自动档:Apple 语音识别能用(或权限还没问过)就不缺;用户明确拒绝了
/// 语音识别权限才需要 sherpa 两件套——否则第一次连接时系统权限框还没答,
/// 就会白下一个多 GB 的模型。
pub fn missing_models(engine: &str) -> Vec<&'static str> {
    let root = crate::models::root();
    let needs_sherpa = if cfg!(target_os = "macos") {
        match engine {
            crate::settings::DICTATION_APPLE => false,
            crate::settings::DICTATION_SHERPA => true,
            _ => apple_refused(),
        }
    } else {
        true
    };
    if !needs_sherpa {
        return Vec::new();
    }
    let mut out = Vec::new();
    if !super::sherpa_stream::stream_model_present(&root) {
        out.push("dictation_stream");
    }
    let sv = crate::models::ARTIFACTS.iter().find(|a| a.id == "asr");
    if sv.is_some_and(|a| !crate::models::artifact_present(&root, a)) {
        out.push("asr");
    }
    out
}

/// 用户明确拒绝(或家长控制限制)了 Apple 语音识别权限。
#[cfg(target_os = "macos")]
fn apple_refused() -> bool {
    matches!(
        vibe_device::speech_apple::status_name(vibe_device::speech_apple::authorization_status()),
        "denied" | "restricted"
    )
}

#[cfg(not(target_os = "macos"))]
fn apple_refused() -> bool {
    false
}
