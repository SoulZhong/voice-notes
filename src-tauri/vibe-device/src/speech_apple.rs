//! Apple Speech (SFSpeechRecognizer, zh-CN) behind the [`Recognizer`] trait.
//!
//! Audio is streamed into an `SFSpeechAudioBufferRecognitionRequest` as
//! 16 kHz mono Float32 buffers. Results arrive on a private operation queue
//! and are forwarded to the Companion as [`RecogEvent`]s.

use crate::audio::SAMPLE_RATE;
use crate::session::{RecogEvent, Recognizer, RecognizerHealth, RecognizerStartError};
use block2::RcBlock;
use objc2::AllocAnyThread;
use objc2::rc::Retained;
use objc2_avf_audio::{AVAudioFormat, AVAudioPCMBuffer};
use objc2_foundation::{NSArray, NSError, NSLocale, NSOperationQueue, NSString};
use objc2_speech::{
    SFSpeechAudioBufferRecognitionRequest, SFSpeechRecognitionResult, SFSpeechRecognitionTask,
    SFSpeechRecognizer, SFSpeechRecognizerAuthorizationStatus,
};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

pub const LOCALE: &str = "zh-CN";
/// Silence appended before `endAudio` (500 ms).
const TAIL_SILENCE_SAMPLES: usize = SAMPLE_RATE as usize / 2;

/// Coding vocabulary that biases recognition (Apple `contextualStrings`).
/// Voice Notes adds the user's hotwords (shared with meeting transcription).
const CODING_VOCABULARY: &[&str] = &[
    "异步",
    "同步",
    "函数",
    "接口",
    "单元测试",
    "重构",
    "提交",
    "合并",
    "分支",
    "变量",
    "参数",
    "返回值",
    "回调",
    "并发",
    "线程",
    "数据库",
    "前端",
    "后端",
    "组件",
    "依赖",
    "部署",
    "日志",
    "报错",
    "调试",
    "编译",
    "类型",
    "注释",
    "Rust",
    "Python",
    "TypeScript",
    "React",
    "API",
    "JSON",
    "commit",
    "PR",
    "bug",
    "Claude",
    "Codex",
    "Cursor",
    "Orca",
];

fn vocabulary(extra: &[String]) -> Vec<String> {
    let mut words: Vec<String> = CODING_VOCABULARY.iter().map(|s| (*s).to_owned()).collect();
    words.extend(
        extra
            .iter()
            .map(|w| w.trim())
            .filter(|w| !w.is_empty())
            .map(str::to_owned),
    );
    words
}

pub type RecogSink = Arc<dyn Fn(RecogEvent) + Send + Sync>;

pub fn authorization_status() -> SFSpeechRecognizerAuthorizationStatus {
    unsafe { SFSpeechRecognizer::authorizationStatus() }
}

/// Whether this process runs from an `.app` bundle (and is therefore its own
/// responsible process for privacy prompts).
pub fn in_app_bundle() -> bool {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().contains(".app/Contents/MacOS/"))
        .unwrap_or(false)
}

pub fn status_name(s: SFSpeechRecognizerAuthorizationStatus) -> &'static str {
    match s {
        SFSpeechRecognizerAuthorizationStatus::Authorized => "authorized",
        SFSpeechRecognizerAuthorizationStatus::Denied => "denied",
        SFSpeechRecognizerAuthorizationStatus::Restricted => "restricted",
        _ => "not determined",
    }
}

/// Ask for Speech Recognition permission. Shows the system prompt the first
/// time; waits up to `wait` for the answer and returns the resulting status.
pub fn request_authorization(wait: Duration) -> SFSpeechRecognizerAuthorizationStatus {
    let current = authorization_status();
    if current != SFSpeechRecognizerAuthorizationStatus::NotDetermined {
        return current;
    }
    if !in_app_bundle() {
        // TCC checks the *responsible* app (the terminal) for the usage
        // description and aborts the process if it is missing.
        log::error!(
            "speech permission not granted yet; run the bundled Voice Notes app once to get the permission prompt"
        );
        return current;
    }
    let (tx, rx) = mpsc::channel();
    let block = RcBlock::new(move |s: SFSpeechRecognizerAuthorizationStatus| {
        let _ = tx.send(s);
    });
    unsafe { SFSpeechRecognizer::requestAuthorization(&block) };
    rx.recv_timeout(wait)
        .unwrap_or_else(|_| authorization_status())
}

struct Active {
    dict: u8,
    request: Retained<SFSpeechAudioBufferRecognitionRequest>,
    task: Retained<SFSpeechRecognitionTask>,
}

pub struct AppleRecognizer {
    recognizer: Option<Retained<SFSpeechRecognizer>>,
    queue: Retained<NSOperationQueue>,
    format: Option<Retained<AVAudioFormat>>,
    sink: RecogSink,
    active: Option<Active>,
    /// Require on-device recognition when the model is available.
    prefer_on_device: bool,
    /// User hotwords appended to [`CODING_VOCABULARY`].
    hotwords: Vec<String>,
}

impl AppleRecognizer {
    pub fn new(sink: RecogSink, hotwords: Vec<String>) -> Self {
        let queue = NSOperationQueue::new();
        queue.setMaxConcurrentOperationCount(1);
        let format = unsafe {
            AVAudioFormat::initStandardFormatWithSampleRate_channels(
                AVAudioFormat::alloc(),
                SAMPLE_RATE as f64,
                1,
            )
        };
        let mut me = Self {
            recognizer: None,
            queue,
            format,
            sink,
            active: None,
            prefer_on_device: true,
            hotwords,
        };
        me.ensure_recognizer();
        me
    }

    fn ensure_recognizer(&mut self) -> Option<&Retained<SFSpeechRecognizer>> {
        if self.recognizer.is_none() {
            let locale = NSLocale::localeWithLocaleIdentifier(&NSString::from_str(LOCALE));
            let r =
                unsafe { SFSpeechRecognizer::initWithLocale(SFSpeechRecognizer::alloc(), &locale) };
            match r {
                Some(r) => {
                    unsafe { r.setQueue(&self.queue) };
                    let on_device = unsafe { r.supportsOnDeviceRecognition() };
                    log::info!("speech recognizer {LOCALE}: on-device support = {on_device}");
                    self.recognizer = Some(r);
                }
                None => log::error!("no speech recognizer for {LOCALE}"),
            }
        }
        self.recognizer.as_ref()
    }
}

fn error_text(err: &NSError) -> String {
    format!(
        "{} {}: {}",
        err.domain(),
        err.code(),
        err.localizedDescription()
    )
}

impl Recognizer for AppleRecognizer {
    fn health(&mut self) -> RecognizerHealth {
        if authorization_status() != SFSpeechRecognizerAuthorizationStatus::Authorized {
            return RecognizerHealth::NoPermission;
        }
        match self.ensure_recognizer() {
            Some(r) if unsafe { r.isAvailable() } => RecognizerHealth::Ready,
            _ => RecognizerHealth::Unavailable,
        }
    }

    fn start(&mut self, dict: u8) -> Result<(), RecognizerStartError> {
        self.cancel();
        if authorization_status() != SFSpeechRecognizerAuthorizationStatus::Authorized {
            return Err(RecognizerStartError::NoPermission);
        }
        let prefer_on_device = self.prefer_on_device;
        let recognizer = self
            .ensure_recognizer()
            .cloned()
            .ok_or(RecognizerStartError::Unavailable)?;
        if !unsafe { recognizer.isAvailable() } {
            return Err(RecognizerStartError::Unavailable);
        }
        let request = unsafe { SFSpeechAudioBufferRecognitionRequest::new() };
        unsafe {
            request.setShouldReportPartialResults(true);
            request.setAddsPunctuation(true);
            let words: Vec<Retained<NSString>> =
                vocabulary(&self.hotwords).iter().map(|w| NSString::from_str(w)).collect();
            request.setContextualStrings(&NSArray::from_retained_slice(&words));
            if prefer_on_device && recognizer.supportsOnDeviceRecognition() {
                request.setRequiresOnDeviceRecognition(true);
            } else {
                log::warn!("on-device recognition unavailable for {LOCALE}; using Apple's server");
            }
        }
        let sink = self.sink.clone();
        let handler = RcBlock::new(
            move |result: *mut SFSpeechRecognitionResult, error: *mut NSError| {
                // SAFETY: Speech passes either nil or a valid object for the call.
                if let Some(result) = unsafe { result.as_ref() } {
                    let text = unsafe { result.bestTranscription().formattedString() }.to_string();
                    if unsafe { result.isFinal() } {
                        sink(RecogEvent::Final { dict, text });
                    } else {
                        sink(RecogEvent::Partial { dict, text });
                    }
                } else if let Some(err) = unsafe { error.as_ref() } {
                    sink(RecogEvent::Error {
                        dict,
                        message: error_text(err),
                    });
                }
            },
        );
        let task =
            unsafe { recognizer.recognitionTaskWithRequest_resultHandler(&request, &handler) };
        self.active = Some(Active {
            dict,
            request,
            task,
        });
        Ok(())
    }

    fn push(&mut self, pcm: &[i16]) {
        let (Some(active), Some(format)) = (&self.active, &self.format) else {
            return;
        };
        if pcm.is_empty() {
            return;
        }
        let Some(buffer) = (unsafe {
            AVAudioPCMBuffer::initWithPCMFormat_frameCapacity(
                AVAudioPCMBuffer::alloc(),
                format,
                pcm.len() as u32,
            )
        }) else {
            log::error!("cannot allocate audio buffer");
            return;
        };
        unsafe {
            let channels = buffer.floatChannelData();
            if channels.is_null() {
                return;
            }
            let data = (*channels).as_ptr();
            for (i, s) in pcm.iter().enumerate() {
                *data.add(i) = *s as f32 / 32768.0;
            }
            buffer.setFrameLength(pcm.len() as u32);
            active.request.appendAudioPCMBuffer(&buffer);
        }
    }

    fn finish(&mut self) {
        if self.active.is_none() {
            return;
        }
        // Speech often drops the last syllable when audio stops abruptly
        // (the button press cuts the recording); a short silent tail helps.
        self.push(&[0; TAIL_SILENCE_SAMPLES]);
        if let Some(a) = &self.active {
            log::debug!("end audio for dictation {}", a.dict);
            unsafe { a.request.endAudio() };
        }
    }

    fn cancel(&mut self) {
        if let Some(a) = self.active.take() {
            unsafe {
                a.request.endAudio();
                a.task.cancel();
            }
        }
    }
}
