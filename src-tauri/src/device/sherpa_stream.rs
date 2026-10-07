//! 听写用的 sherpa 两步识别(Windows 默认;macOS 在 Apple 语音识别不可用时退回)。
//!
//! 说话时:流式 zipformer(中英双语,int8)边听边出字,作设备屏上的实时字幕;
//! 说完后:SenseVoice(int8)对整段音频再识别一遍,出最终插入的文字——往编程
//! Agent 里说话技术名词多,终稿质量决定这个功能有没有用(用户拍板 2026-10-07)。
//! SenseVoice 不在场或失败时,退回流式的最后结果。
//!
//! 识别在独立线程上跑(解码耗时不能卡设备主循环);结果经 RecogSink 回送。
//! 热词:两种模型都不接收热词(SenseVoice 不支持;流式模型需要 bpe 词表才能用,
//! 而只影响实时字幕),所以 sherpa 这条路上热词不生效——只有 Apple 那条路用热词。

use crate::asr::engine::{ModelSpec, OfflineEngine};
use sherpa_onnx_sys as sys;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use vibe_device::runtime::RecogSink;
use vibe_device::session::{RecogEvent, Recognizer, RecognizerHealth, RecognizerStartError};

extern "C" {
    fn vn_sherpa_create_online_recognizer(config: *const c_void) -> *const c_void;
    fn vn_sherpa_create_online_stream(recognizer: *const c_void) -> *const c_void;
    /// 0=成功;-1=C++ 异常已被屏障捕获。
    fn vn_sherpa_online_feed(
        recognizer: *const c_void,
        stream: *const c_void,
        sample_rate: i32,
        samples: *const f32,
        n: i32,
        finished: i32,
        out_json: *mut *const c_char,
    ) -> i32;
}

/// 流式模型目录(models::ARTIFACTS 的 "dictation_stream")。
pub use crate::models::STREAM_DIR;
const STREAM_ENCODER: &str = "encoder-epoch-99-avg-1.int8.onnx";
const STREAM_DECODER: &str = "decoder-epoch-99-avg-1.int8.onnx";
const STREAM_JOINER: &str = "joiner-epoch-99-avg-1.int8.onnx";
const STREAM_TOKENS: &str = "tokens.txt";
/// 说完补一段静音再收尾,流式模型末字不被吞(与 Apple 那边同理)。
const TAIL_SILENCE: usize = 16_000 / 2;
/// 听写是短句,解码线程数不必多:不和同时进行的会议录制抢 CPU。
const THREADS: i32 = 2;

fn stream_dir(models_root: &Path) -> PathBuf {
    models_root.join(STREAM_DIR)
}

/// 流式模型是否齐全(实时字幕必需)。
pub fn stream_model_present(models_root: &Path) -> bool {
    let d = stream_dir(models_root);
    [STREAM_ENCODER, STREAM_DECODER, STREAM_JOINER, STREAM_TOKENS]
        .iter()
        .all(|f| d.join(f).is_file())
}

/// SenseVoice 终稿模型:优先 int8(约 240MB,常驻内存小,听写短句准确率够),没有再用全精度。
fn final_model() -> Option<(PathBuf, PathBuf)> {
    let d = crate::models::asr_model_dir(crate::settings::ASR_SENSE_VOICE);
    let tokens = d.join("tokens.txt");
    if !tokens.is_file() {
        return None;
    }
    ["model.int8.onnx", "model.onnx"]
        .iter()
        .map(|f| d.join(f))
        .find(|p| p.is_file())
        .map(|m| (m, tokens))
}

/// 两步识别是否可用:流式模型必须在;终稿模型缺失时仍可用(退回流式结果)。
pub fn available(models_root: &Path) -> bool {
    stream_model_present(models_root)
}

/// 流式识别器(sherpa C 对象,单持有者)。
struct OnlineEngine {
    rec: *const c_void,
}

// SAFETY: sherpa C 对象在单持有者用法下可跨线程移动(同 OfflineEngine)。
unsafe impl Send for OnlineEngine {}

impl OnlineEngine {
    fn new(dir: &Path) -> anyhow::Result<Self> {
        let s = |f: &str| CString::new(dir.join(f).to_string_lossy().into_owned()).unwrap_or_default();
        let (enc, dec, join, tok) = (s(STREAM_ENCODER), s(STREAM_DECODER), s(STREAM_JOINER), s(STREAM_TOKENS));
        // SAFETY: POD 配置,零值 = C 端各字段默认(采样率 16k、80 维特征、greedy_search)。
        let mut cfg: sys::OnlineRecognizerConfig = unsafe { std::mem::zeroed() };
        cfg.model_config.transducer.encoder = enc.as_ptr();
        cfg.model_config.transducer.decoder = dec.as_ptr();
        cfg.model_config.transducer.joiner = join.as_ptr();
        cfg.model_config.tokens = tok.as_ptr();
        cfg.model_config.num_threads = THREADS;
        // 端点检测关掉:听写的起止由设备按键决定,不靠静音切句。
        cfg.enable_endpoint = 0;
        // SAFETY: cfg 与其引用的 CString 在调用期间存活;屏障吞掉 C++ 异常。
        let rec = unsafe { vn_sherpa_create_online_recognizer(&cfg as *const _ as *const c_void) };
        anyhow::ensure!(!rec.is_null(), "创建流式识别器失败(模型文件不完整?见 stderr 的 [sherpa-barrier] 行)");
        Ok(Self { rec })
    }

    fn stream(&self) -> anyhow::Result<OnlineStream> {
        // SAFETY: rec 来自本引擎。
        let s = unsafe { vn_sherpa_create_online_stream(self.rec) };
        anyhow::ensure!(!s.is_null(), "创建识别流失败");
        Ok(OnlineStream { rec: self.rec, s })
    }
}

impl Drop for OnlineEngine {
    fn drop(&mut self) {
        unsafe { sys::SherpaOnnxDestroyOnlineRecognizer(self.rec as *const sys::OnlineRecognizer) }
    }
}

struct OnlineStream {
    rec: *const c_void,
    s: *const c_void,
}

impl OnlineStream {
    /// 喂音频(可为空)、可选标记结束,返回当前识别文字。
    fn feed(&mut self, samples: &[f32], finished: bool) -> anyhow::Result<String> {
        let mut json: *const c_char = std::ptr::null();
        // SAFETY: 指针来自本流/引擎;samples 在调用期间存活;整段在 C++ 屏障内。
        let rc = unsafe {
            vn_sherpa_online_feed(
                self.rec,
                self.s,
                16_000,
                samples.as_ptr(),
                samples.len() as i32,
                finished as i32,
                &mut json,
            )
        };
        anyhow::ensure!(rc == 0, "流式识别内部异常(已被屏障捕获)");
        if json.is_null() {
            return Ok(String::new());
        }
        // SAFETY: C 端返回的 NUL 结尾字符串,用完即释放。
        let text = unsafe {
            let s = CStr::from_ptr(json).to_string_lossy().into_owned();
            sys::SherpaOnnxDestroyOnlineStreamResultJson(json);
            s
        };
        Ok(result_text(&text))
    }
}

impl Drop for OnlineStream {
    fn drop(&mut self) {
        unsafe { sys::SherpaOnnxDestroyOnlineStream(self.s as *const sys::OnlineStream) }
    }
}

/// 流式结果 JSON 的 text 字段。zipformer bilingual 的英文 token 是大写 BPE,
/// 小写化让实时字幕读起来不像在喊。
fn result_text(json: &str) -> String {
    let text = serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(str::to_owned))
        .unwrap_or_default();
    text.trim().to_lowercase()
}

/// SenseVoice 输出里可能夹带的 <|zh|> 之类标签(sherpa 通常已剥离,兜底)。
fn clean_final(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("<|") {
        out.push_str(&rest[..i]);
        match rest[i..].find("|>") {
            Some(j) => rest = &rest[i + j + 2..],
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out.trim().to_owned()
}

enum Msg {
    Start(u8),
    Push(Vec<i16>),
    Finish,
    Cancel,
}

/// 两步识别器:对 vibe_device 的 Recognizer 接口,内部一条解码线程。
pub struct SherpaRecognizer {
    tx: mpsc::Sender<Msg>,
    models_root: PathBuf,
}

impl SherpaRecognizer {
    pub fn new(models_root: PathBuf, sink: RecogSink) -> Self {
        let (tx, rx) = mpsc::channel::<Msg>();
        let root = models_root.clone();
        std::thread::Builder::new()
            .name("dictation-sherpa".into())
            .spawn(move || worker(root, rx, sink))
            .expect("dictation-sherpa thread");
        Self { tx, models_root }
    }
}

impl Recognizer for SherpaRecognizer {
    fn health(&mut self) -> RecognizerHealth {
        if available(&self.models_root) {
            RecognizerHealth::Ready
        } else {
            RecognizerHealth::Unavailable
        }
    }

    fn start(&mut self, dict: u8) -> Result<(), RecognizerStartError> {
        if !available(&self.models_root) {
            return Err(RecognizerStartError::Unavailable);
        }
        self.tx
            .send(Msg::Start(dict))
            .map_err(|_| RecognizerStartError::Failed("识别线程已退出".into()))
    }

    fn push(&mut self, pcm: &[i16]) {
        let _ = self.tx.send(Msg::Push(pcm.to_vec()));
    }

    fn finish(&mut self) {
        let _ = self.tx.send(Msg::Finish);
    }

    fn cancel(&mut self) {
        let _ = self.tx.send(Msg::Cancel);
    }
}

struct Active {
    dict: u8,
    stream: Option<OnlineStream>,
    audio: Vec<f32>,
    partial: String,
}

fn worker(root: PathBuf, rx: mpsc::Receiver<Msg>, sink: RecogSink) {
    // 模型懒加载、加载后常驻:设备连着就可能随时说话,每句重载太慢。
    let mut online: Option<OnlineEngine> = None;
    let mut offline: Option<OfflineEngine> = None;
    let mut offline_failed = false;
    let mut active: Option<Active> = None;
    for msg in rx {
        match msg {
            Msg::Start(dict) => {
                if online.is_none() {
                    match OnlineEngine::new(&stream_dir(&root)) {
                        Ok(e) => online = Some(e),
                        Err(e) => {
                            eprintln!("听写: 流式模型加载失败: {e:#}");
                            sink(RecogEvent::Error { dict, message: e.to_string() });
                            continue;
                        }
                    }
                }
                let stream = online.as_ref().and_then(|e| match e.stream() {
                    Ok(s) => Some(s),
                    Err(e) => {
                        eprintln!("听写: {e:#}");
                        None
                    }
                });
                active = Some(Active { dict, stream, audio: Vec::new(), partial: String::new() });
            }
            Msg::Push(pcm) => {
                let Some(a) = active.as_mut() else { continue };
                let f: Vec<f32> = pcm.iter().map(|s| *s as f32 / 32768.0).collect();
                a.audio.extend_from_slice(&f);
                if let Some(stream) = a.stream.as_mut() {
                    match stream.feed(&f, false) {
                        Ok(text) if !text.is_empty() && text != a.partial => {
                            a.partial = text.clone();
                            sink(RecogEvent::Partial { dict: a.dict, text });
                        }
                        Ok(_) => {}
                        Err(e) => {
                            eprintln!("听写: 流式解码失败,本句只出终稿: {e:#}");
                            a.stream = None;
                        }
                    }
                }
            }
            Msg::Finish => {
                let Some(mut a) = active.take() else { continue };
                if let Some(stream) = a.stream.as_mut() {
                    let tail = vec![0f32; TAIL_SILENCE];
                    if let Ok(text) = stream.feed(&tail, true) {
                        if !text.is_empty() {
                            a.partial = text;
                        }
                    }
                }
                // 第二步:SenseVoice 整段终稿。
                if offline.is_none() && !offline_failed {
                    match final_model() {
                        Some((model, tokens)) => {
                            let spec = ModelSpec::SenseVoice {
                                model: model.to_string_lossy().into_owned(),
                                tokens: tokens.to_string_lossy().into_owned(),
                                use_itn: true,
                            };
                            match OfflineEngine::new(&spec, THREADS * 2, None) {
                                Ok(e) => offline = Some(e),
                                Err(e) => {
                                    eprintln!("听写: SenseVoice 加载失败,终稿用流式结果: {e:#}");
                                    offline_failed = true;
                                }
                            }
                        }
                        None => {
                            eprintln!("听写: SenseVoice 模型不在,终稿用流式结果");
                            offline_failed = true;
                        }
                    }
                }
                let final_text = match offline.as_mut() {
                    Some(engine) if !a.audio.is_empty() => match engine.transcribe(16_000, &a.audio) {
                        Ok(t) => {
                            let t = clean_final(&t.text);
                            if t.is_empty() { a.partial.clone() } else { t }
                        }
                        Err(e) => {
                            eprintln!("听写: SenseVoice 识别失败,终稿用流式结果: {e:#}");
                            a.partial.clone()
                        }
                    },
                    _ => a.partial.clone(),
                };
                sink(RecogEvent::Final { dict: a.dict, text: final_text });
            }
            Msg::Cancel => {
                active = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_text_reads_json_and_lowercases_bpe() {
        assert_eq!(result_text(r#"{"text":" 把 FUNCTION 改成异步 ","tokens":[]}"#), "把 function 改成异步");
        assert_eq!(result_text("not json"), "");
    }

    #[test]
    fn clean_final_strips_tags() {
        assert_eq!(clean_final("<|zh|><|NEUTRAL|>你好。"), "你好。");
        assert_eq!(clean_final("你好<|"), "你好");
        assert_eq!(clean_final(" plain "), "plain");
    }
}
