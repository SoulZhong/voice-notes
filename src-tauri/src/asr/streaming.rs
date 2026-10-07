//! sherpa 流式识别(在线 zipformer 中英双语)的薄封装:设备听写的实时字幕与录音时的
//! 实时字幕共用。C++ 异常经 cxx/sherpa_barrier.cc 的屏障函数拦下,不让它穿过 FFI。

use sherpa_onnx_sys as sys;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};
use std::path::{Path, PathBuf};

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
// 解码器用 fp32:int8 解码器让流式结果叠字(「构构构建脚脚脚脚本」),2026-10-07 实测。
const STREAM_DECODER: &str = "decoder-epoch-99-avg-1.onnx";
const STREAM_JOINER: &str = "joiner-epoch-99-avg-1.int8.onnx";
const STREAM_TOKENS: &str = "tokens.txt";
pub fn stream_dir(models_root: &Path) -> PathBuf {
    models_root.join(STREAM_DIR)
}

/// 流式模型是否齐全(实时字幕必需)。
pub fn stream_model_present(models_root: &Path) -> bool {
    let d = stream_dir(models_root);
    [STREAM_ENCODER, STREAM_DECODER, STREAM_JOINER, STREAM_TOKENS]
        .iter()
        .all(|f| d.join(f).is_file())
}

/// 流式识别器(sherpa C 对象,单持有者)。
pub struct OnlineEngine {
    rec: *const c_void,
}

// SAFETY: sherpa C 对象在单持有者用法下可跨线程移动(同 OfflineEngine)。
unsafe impl Send for OnlineEngine {}

impl OnlineEngine {
    pub fn new(dir: &Path, threads: i32) -> anyhow::Result<Self> {
        let s = |f: &str| CString::new(dir.join(f).to_string_lossy().into_owned()).unwrap_or_default();
        let (enc, dec, join, tok) = (s(STREAM_ENCODER), s(STREAM_DECODER), s(STREAM_JOINER), s(STREAM_TOKENS));
        // SAFETY: POD 配置,零值 = C 端各字段默认(采样率 16k、80 维特征、greedy_search)。
        let mut cfg: sys::OnlineRecognizerConfig = unsafe { std::mem::zeroed() };
        cfg.model_config.transducer.encoder = enc.as_ptr();
        cfg.model_config.transducer.decoder = dec.as_ptr();
        cfg.model_config.transducer.joiner = join.as_ptr();
        cfg.model_config.tokens = tok.as_ptr();
        cfg.model_config.num_threads = threads;
        // 端点检测关掉:听写的起止由设备按键决定,不靠静音切句。
        cfg.enable_endpoint = 0;
        // SAFETY: cfg 与其引用的 CString 在调用期间存活;屏障吞掉 C++ 异常。
        let rec = unsafe { vn_sherpa_create_online_recognizer(&cfg as *const _ as *const c_void) };
        anyhow::ensure!(!rec.is_null(), "创建流式识别器失败(模型文件不完整?见 stderr 的 [sherpa-barrier] 行)");
        Ok(Self { rec })
    }

    pub fn stream(&self) -> anyhow::Result<OnlineStream> {
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

pub struct OnlineStream {
    rec: *const c_void,
    s: *const c_void,
}

impl OnlineStream {
    /// 喂音频(可为空)、可选标记结束,返回当前识别文字。
    pub fn feed(&mut self, samples: &[f32], finished: bool) -> anyhow::Result<String> {
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
pub fn result_text(json: &str) -> String {
    let text = serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(str::to_owned))
        .unwrap_or_default();
    text.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_text_reads_json_and_lowercases_bpe() {
        assert_eq!(result_text(r#"{"text":" 把 FUNCTION 改成异步 ","tokens":[]}"#), "把 function 改成异步");
        assert_eq!(result_text("not json"), "");
    }
}
