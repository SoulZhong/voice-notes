//! sherpa-onnx 离线识别统一引擎(直接走 sys 层 C API)。
//!
//! 为什么不用官方安全封装 `sherpa_onnx::OfflineRecognizer`:其 `get_result` 把 C 端
//! JSON 解析进固定结构体时丢弃了 `lang` 字段,而语言过滤(session::is_foreign_final)
//! 以 lang 为主判据(2026-07-28 ASR 调研,docs/superpowers/research/)。此处自行取
//! JSON 解析,保留 lang。unsafe 全部集中在本模块;VAD/声纹仍走官方安全 API。
//!
//! 配置结构体是纯指针+整数的 POD,C 端对空指针/零值字段有默认值兜底
//! (c-api.cc `SHERPA_ONNX_OR` 宏,如 feature_dim 0→80、decoding_method
//! null→greedy_search),故用 `zeroed` 起底、只填需要的字段——与官方安全封装给
//! 未设字段传 null 的行为一致。

use super::Transcript;
use serde::Deserialize;
use sherpa_onnx_sys as sys;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;

/// 支持的离线模型族与各自的文件布局(路径为绝对路径字符串)。
pub enum ModelSpec {
    SenseVoice { model: String, tokens: String, use_itn: bool },
    Paraformer { model: String, tokens: String },
    Whisper { encoder: String, decoder: String, tokens: String },
    Qwen3 {
        conv_frontend: String,
        encoder: String,
        decoder: String,
        /// tokenizer 目录(含 vocab.json/merges.txt)。
        tokenizer_dir: String,
        /// 逗号分隔热词(上下文偏置),None = 不启用。
        hotwords: Option<String>,
    },
    /// FireRedASR2-AED(encoder-decoder 双 onnx + tokens.txt)。
    FireRed { encoder: String, decoder: String, tokens: String },
}

pub struct OfflineEngine {
    ptr: *const sys::OfflineRecognizer,
}

// SAFETY: sherpa-onnx C 对象在单持有者用法下可跨线程移动(与官方封装同款声明)。
unsafe impl Send for OfflineEngine {}

impl OfflineEngine {
    pub fn new(spec: &ModelSpec, num_threads: i32, provider: Option<&str>) -> anyhow::Result<Self> {
        // CString 必须存活到 create 返回(C 端在 create 内拷贝配置字符串)。
        let mut keep: Vec<CString> = Vec::new();
        let mut c = |s: &str, keep: &mut Vec<CString>| -> *const c_char {
            let cs = CString::new(s).unwrap_or_default();
            let p = cs.as_ptr();
            keep.push(cs); // move 不改变堆缓冲地址,p 持续有效
            p
        };
        // SAFETY: POD 配置,零值 = C 端各字段默认(见模块注释)。
        let mut config: sys::OfflineRecognizerConfig = unsafe { std::mem::zeroed() };
        config.model_config.num_threads = num_threads;
        if let Some(p) = provider {
            config.model_config.provider = c(p, &mut keep);
        }
        match spec {
            ModelSpec::SenseVoice { model, tokens, use_itn } => {
                config.model_config.sense_voice.model = c(model, &mut keep);
                config.model_config.sense_voice.language = c("auto", &mut keep); // zh/en 混合自动检测
                config.model_config.sense_voice.use_itn = *use_itn as i32;
                config.model_config.tokens = c(tokens, &mut keep);
            }
            ModelSpec::Paraformer { model, tokens } => {
                config.model_config.paraformer.model = c(model, &mut keep);
                config.model_config.tokens = c(tokens, &mut keep);
            }
            ModelSpec::Whisper { encoder, decoder, tokens } => {
                config.model_config.whisper.encoder = c(encoder, &mut keep);
                config.model_config.whisper.decoder = c(decoder, &mut keep);
                // language 空 = 自动语种检测;token 时间戳保持关闭,与迁移前行为一致
                // (whisper 路径本就无时间戳,diarization 走段级降级)。
                config.model_config.tokens = c(tokens, &mut keep);
            }
            ModelSpec::Qwen3 { conv_frontend, encoder, decoder, tokenizer_dir, hotwords } => {
                config.model_config.qwen3_asr.conv_frontend = c(conv_frontend, &mut keep);
                config.model_config.qwen3_asr.encoder = c(encoder, &mut keep);
                config.model_config.qwen3_asr.decoder = c(decoder, &mut keep);
                config.model_config.qwen3_asr.tokenizer = c(tokenizer_dir, &mut keep);
                // 采样参数取官方安全封装同款默认(零值在 C 端此结构无 OR 兜底,须显式给)。
                // 长度上限从官方默认(512/128)上调:VAD 段上限 15s,中文密集语速下
                // 一段可达 ~90 字 ≈ 90+ token,官方 128 会在真实会议里截断——2026-08-04
                // 排查一场 33min 笔记的日志里 sherpa 报了 5 次
                // "Result is truncated. max_new_tokens 128 is too small"。留 2 倍余量;
                // max_total_len 同步上调,给音频提示 token 留出空间(否则新上限吃不到)。
                // 上界不敢再放大的原因:近贪心解码偶发复读时,这里就是唯一的止损闸,
                // 每段最坏解码时长与本值成正比。
                config.model_config.qwen3_asr.max_total_len = 1024;
                config.model_config.qwen3_asr.max_new_tokens = 256;
                config.model_config.qwen3_asr.temperature = 1e-6; // 近贪心,转写要确定性
                config.model_config.qwen3_asr.top_p = 0.8;
                config.model_config.qwen3_asr.seed = 42;
                if let Some(h) = hotwords {
                    config.model_config.qwen3_asr.hotwords = c(h, &mut keep);
                }
            }
            ModelSpec::FireRed { encoder, decoder, tokens } => {
                config.model_config.fire_red_asr.encoder = c(encoder, &mut keep);
                config.model_config.fire_red_asr.decoder = c(decoder, &mut keep);
                config.model_config.tokens = c(tokens, &mut keep);
            }
        }
        let ptr = unsafe { sys::SherpaOnnxCreateOfflineRecognizer(&config) };
        drop(keep);
        anyhow::ensure!(!ptr.is_null(), "创建离线识别器失败(检查模型文件是否完整)");
        Ok(Self { ptr })
    }

    pub fn transcribe(&mut self, sample_rate: i32, samples: &[f32]) -> anyhow::Result<Transcript> {
        unsafe {
            let stream = sys::SherpaOnnxCreateOfflineStream(self.ptr);
            anyhow::ensure!(!stream.is_null(), "创建识别流失败");
            sys::SherpaOnnxAcceptWaveformOffline(
                stream,
                sample_rate,
                samples.as_ptr(),
                samples.len() as i32,
            );
            sys::SherpaOnnxDecodeOfflineStream(self.ptr, stream);
            let json_ptr = sys::SherpaOnnxGetOfflineStreamResultAsJson(stream);
            let transcript = if json_ptr.is_null() {
                Transcript::default()
            } else {
                let json = CStr::from_ptr(json_ptr).to_string_lossy().into_owned();
                sys::SherpaOnnxDestroyOfflineStreamResultJson(json_ptr);
                parse_result_json(&json)
            };
            sys::SherpaOnnxDestroyOfflineStream(stream);
            Ok(transcript)
        }
    }
}

impl Drop for OfflineEngine {
    fn drop(&mut self) {
        unsafe { sys::SherpaOnnxDestroyOfflineRecognizer(self.ptr) }
    }
}

/// sherpa-onnx 结果 JSON(offline-stream.cc AsJsonString)→ Transcript。
/// 键:lang/emotion/event/text/timestamps/durations/tokens/words;此处只取四个。
/// 解析失败(理论上 C++ std::quoted 不转义控制字符可产非法 JSON)→ 空结果 + stderr,
/// 走上游 [识别失败] 占位路径,不 panic。
pub(crate) fn parse_result_json(json: &str) -> Transcript {
    let parsed: ResultJson = match serde_json::from_str(json) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ASR 结果 JSON 解析失败({e}),按空结果处理");
            ResultJson::default()
        }
    };
    Transcript {
        text: parsed.text,
        lang: parsed.lang,
        tokens: parsed.tokens,
        timestamps: parsed.timestamps.unwrap_or_default(),
    }
}

#[derive(Deserialize, Default)]
struct ResultJson {
    #[serde(default)]
    text: String,
    #[serde(default)]
    lang: String,
    #[serde(default)]
    tokens: Vec<String>,
    #[serde(default)]
    timestamps: Option<Vec<f32>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sense_voice_style_json_keeps_lang_tokens_timestamps() {
        let t = parse_result_json(
            r#"{"lang": "<|zh|>", "emotion": "<|NEUTRAL|>", "event": "<|Speech|>", "text": "你好", "timestamps": [0.00, 0.24], "durations": [], "tokens": ["你", "好"], "words": []}"#,
        );
        assert_eq!(t.text, "你好");
        assert_eq!(t.lang, "<|zh|>", "官方安全封装丢 lang,引擎必须保留");
        assert_eq!(t.tokens, vec!["你", "好"]);
        assert_eq!(t.timestamps, vec![0.0, 0.24]);
    }

    #[test]
    fn parse_json_without_timestamps_or_lang_yields_empty_fields() {
        let t = parse_result_json(r#"{"text": "hello", "tokens": ["hello"]}"#);
        assert_eq!(t.text, "hello");
        assert!(t.lang.is_empty());
        assert!(t.timestamps.is_empty(), "缺 timestamps 键 → 空,diarization 走段级降级");
    }

    #[test]
    fn parse_malformed_json_falls_back_to_empty_transcript() {
        let t = parse_result_json("not json {{");
        assert!(t.text.is_empty() && t.lang.is_empty() && t.tokens.is_empty());
    }
}
