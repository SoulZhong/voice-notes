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
use crate::asr::streaming::{stream_dir, OnlineEngine, OnlineStream};
pub use crate::asr::streaming::stream_model_present;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use vibe_device::runtime::RecogSink;
use vibe_device::session::{RecogEvent, Recognizer, RecognizerHealth, RecognizerStartError};

/// 说完补一段静音再收尾,流式模型末字不被吞(与 Apple 那边同理)。
const TAIL_SILENCE: usize = 16_000 / 2;
/// 听写是短句,解码线程数不必多:不和同时进行的会议录制抢 CPU。
const THREADS: i32 = 2;

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
                    match OnlineEngine::new(&stream_dir(&root), THREADS) {
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


    /// 手动实验:VN_STREAM_WAV=<16k 单声道 wav> cargo test stream_probe -- --ignored --nocapture
    #[test]
    #[ignore]
    fn stream_probe() {
        let wav = std::env::var("VN_STREAM_WAV").expect("VN_STREAM_WAV");
        let pcm = vibe_device::sim::read_wav(std::path::Path::new(&wav)).unwrap();
        let engine = OnlineEngine::new(&stream_dir(&crate::models::root()), THREADS).unwrap();
        let mut st = engine.stream().unwrap();
        let mut last = String::new();
        for chunk in pcm.chunks(320) {
            let f: Vec<f32> = chunk.iter().map(|s| *s as f32 / 32768.0).collect();
            let t = st.feed(&f, false).unwrap();
            if t != last {
                println!("partial: {t}");
                last = t;
            }
        }
        println!("final: {}", st.feed(&vec![0f32; TAIL_SILENCE], true).unwrap());
    }

    /// 手动实验:实时字幕两种做法的 CPU 对比(VN_STREAM_WAV,cargo test caption_cpu -- --ignored --nocapture)。
    /// 旧:每 1s 把当前句(按 6s 一句切)整段交 SenseVoice(录制同款:全精度、default_threads)。
    /// 新:流式 zipformer 按 100ms 小块一路喂下去。两边都不含每句一次的定稿。
    #[test]
    #[ignore]
    fn caption_cpu() {
        fn cpu() -> f64 {
            let mut u: libc::rusage = unsafe { std::mem::zeroed() };
            unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
            let t = |v: libc::timeval| v.tv_sec as f64 + v.tv_usec as f64 / 1e6;
            t(u.ru_utime) + t(u.ru_stime)
        }
        let wav = std::env::var("VN_STREAM_WAV").expect("VN_STREAM_WAV");
        let pcm: Vec<f32> = vibe_device::sim::read_wav(std::path::Path::new(&wav))
            .unwrap()
            .iter()
            .map(|s| *s as f32 / 32768.0)
            .collect();
        let secs = pcm.len() as f64 / 16000.0;

        let root = crate::models::root();
        let dir = crate::models::asr_model_dir("sense_voice");
        let spec = ModelSpec::SenseVoice {
            model: dir.join("model.onnx").to_string_lossy().into_owned(),
            tokens: dir.join("tokens.txt").to_string_lossy().into_owned(),
            use_itn: true,
        };
        let mut sv = OfflineEngine::new(&spec, crate::asr::sense_voice::default_threads(), None).unwrap();
        sv.transcribe(16000, &pcm[..16000]).unwrap(); // 预热
        let t0 = cpu();
        let mut calls = 0;
        for utt in pcm.chunks(16000 * 6) {
            let mut k = 16000;
            while k <= utt.len() {
                sv.transcribe(16000, &utt[..k]).unwrap();
                calls += 1;
                k += 16000;
            }
        }
        let old = cpu() - t0;

        let engine = OnlineEngine::new(&stream_dir(&root), THREADS).unwrap();
        let t0 = cpu();
        let mut st = engine.stream().unwrap();
        let mut last = String::new();
        for chunk in pcm.chunks(1600) {
            last = st.feed(chunk, false).unwrap();
        }
        let new = cpu() - t0;
        println!(
            "音频 {secs:.1}s | 旧:SenseVoice {calls} 次重识别 CPU {old:.2}s ({:.0}%实时) | 新:流式 CPU {new:.2}s ({:.0}%实时) | 倍数 {:.1}x | 流式末字幕: {last}",
            old / secs * 100.0,
            new / secs * 100.0,
            old / new
        );
    }

    #[test]
    fn clean_final_strips_tags() {
        assert_eq!(clean_final("<|zh|><|NEUTRAL|>你好。"), "你好。");
        assert_eq!(clean_final("你好<|"), "你好");
        assert_eq!(clean_final(" plain "), "plain");
    }
}
