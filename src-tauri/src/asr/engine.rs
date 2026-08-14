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
use std::os::raw::{c_char, c_void};

// issue #98:sherpa C API 全链路无 try/catch,onnxruntime 的 C++ 异常穿 extern "C"
// 进 Rust 会在 catch_unwind 处 abort(__rust_foreign_exception,三例 SIGABRT 同签名)。
// 识别调用一律改走 cxx/sherpa_barrier.cc 的 C++ 侧屏障:异常降级为错误码,
// what() 打 stderr。不透明指针以 c_void 过 FFI,与 sys 类型仅作指针转型。
extern "C" {
    fn vn_sherpa_create_offline_recognizer(config: *const c_void) -> *const c_void;
    /// 0=成功(*out_json 可能为 null=无结果);-1=C++ 异常已捕获;-2=建流失败。
    fn vn_sherpa_transcribe(
        recognizer: *const c_void,
        sample_rate: i32,
        samples: *const f32,
        n: i32,
        out_json: *mut *const c_char,
    ) -> i32;
    /// 屏障机制自测(单测用):mode=1 人为 C++ throw,期望 -1;mode=0 期望 0。
    #[allow(dead_code)]
    fn vn_sherpa_barrier_selftest(mode: i32) -> i32;
}

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
        // SAFETY: config 为本函数栈上 POD,屏障只透传指针给 sherpa create。
        let ptr = unsafe {
            vn_sherpa_create_offline_recognizer(&config as *const _ as *const c_void)
                as *const sys::OfflineRecognizer
        };
        drop(keep);
        anyhow::ensure!(
            !ptr.is_null(),
            "创建离线识别器失败(模型文件不完整,或引擎内部异常——见 stderr 的 [sherpa-barrier] 行)"
        );
        Ok(Self { ptr })
    }

    pub fn transcribe(&mut self, sample_rate: i32, samples: &[f32]) -> anyhow::Result<Transcript> {
        // SAFETY: 指针来自本引擎;samples 在调用期间存活;整段 C 调用在 C++ 屏障内,
        // onnxruntime 异常不会穿 FFI(issue #98)。
        unsafe {
            let mut json_ptr: *const c_char = std::ptr::null();
            let rc = vn_sherpa_transcribe(
                self.ptr as *const c_void,
                sample_rate,
                samples.as_ptr(),
                samples.len() as i32,
                &mut json_ptr,
            );
            match rc {
                0 => {}
                -2 => anyhow::bail!("创建识别流失败"),
                _ => anyhow::bail!("识别引擎内部异常(已被屏障捕获,见 stderr 的 [sherpa-barrier] 行)"),
            }
            let transcript = if json_ptr.is_null() {
                Transcript::default()
            } else {
                let json = CStr::from_ptr(json_ptr).to_string_lossy().into_owned();
                sys::SherpaOnnxDestroyOfflineStreamResultJson(json_ptr);
                parse_result_json(&json)
            };
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
/// 复读折叠后每个单元保留的份数:2 份既能保住"对对"式的强调语气,又让
/// 一眼看出这里原本是重复。
const REPEAT_KEEP: usize = 2;
/// 判定为复读的最少连续份数。单字单元门槛更高——"哈哈哈哈哈""对对对"是自然汉语。
/// 取值由真实数据定标(2026-08-14,近期 14 场 5638 段全量回放):8/6 把 16 条解码
/// 垃圾一条不漏地清掉(它们重复 18-100 遍,离门槛极远),同时把误伤的人类强调
/// ("对对对对对对""对不起"×4)从 13 条压到 2 条;再放宽到 10/6 无额外收益。
/// 会议纪要要忠于原话,清理口癖是精修阶段的事,这里只杀解码器垃圾。
const REPEAT_MIN_SINGLE: usize = 8;
const REPEAT_MIN_MULTI: usize = 6;
/// 参与匹配的最长重复单元(字符数)。真实复读单元都很短("然后,""你像这样,"),
/// 放太长会开始误伤排比句。
const REPEAT_UNIT_MAX: usize = 8;

/// 折叠自回归解码的复读尾巴。qwen3/fire_red 这类 LLM 式解码器偶发陷入
/// 重复循环,一路吐同一个单元直到撞上 max_new_tokens 才停(2026-08-14 实测:
/// 近期 14 场笔记 80 段中招,密度最高 133 字/秒,而人类语速仅 5-6 字/秒)。
/// 真实内容在开头,尾巴纯属垃圾——放大 max_new_tokens 只会让垃圾更长、解码更慢,
/// 折叠才是对症。CTC 引擎(SenseVoice/Paraformer)不会复读,且带 token 时间戳,
/// 调用方据此门控(见 parse_result_json)。
fn collapse_repeats(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let mut collapsed = false;
        for unit in 1..=REPEAT_UNIT_MAX.min((chars.len() - i) / 2) {
            let mut count = 1;
            while chars[i..i + unit] == chars[i + unit * count..]
                [..unit.min(chars.len() - (i + unit * count))]
                && i + unit * (count + 1) <= chars.len()
            {
                count += 1;
            }
            let min = if unit == 1 { REPEAT_MIN_SINGLE } else { REPEAT_MIN_MULTI };
            if count >= min {
                for _ in 0..REPEAT_KEEP {
                    out.extend(&chars[i..i + unit]);
                }
                i += unit * count;
                collapsed = true;
                break;
            }
        }
        if !collapsed {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

pub(crate) fn parse_result_json(json: &str) -> Transcript {
    let parsed: ResultJson = match serde_json::from_str(json) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ASR 结果 JSON 解析失败({e}),按空结果处理");
            ResultJson::default()
        }
    };
    let timestamps = parsed.timestamps.unwrap_or_default();
    // 复读折叠只对「无 token 时间戳」的自回归引擎生效:CTC 引擎的 text 与
    // tokens/timestamps 严格等长对应,动一个字就会让段内说话人切分错位,而它们
    // 本就不会复读。折叠改了文本 → 同批 tokens 随即作废,一并清掉不留错位隐患。
    let (text, tokens) = if timestamps.is_empty() {
        let collapsed = collapse_repeats(&parsed.text);
        if collapsed != parsed.text {
            eprintln!(
                "复读折叠: {} 字 → {} 字(解码陷入重复循环,已截取有效前缀)",
                parsed.text.chars().count(),
                collapsed.chars().count()
            );
            (collapsed, Vec::new())
        } else {
            (parsed.text, parsed.tokens)
        }
    } else {
        (parsed.text, parsed.tokens)
    };
    Transcript { text, lang: parsed.lang, tokens, timestamps }
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

    /// 自回归解码复读折叠(2026-08-14 实锤:近期 14 场笔记里 80 段是复读,
    /// 密度 17-133 字/秒——人类语速 5-6 字/秒;真实内容都在开头,尾部是同一
    /// 单元无限重复,直到撞上 max_new_tokens 才停)。样例取自真实数据。
    #[test]
    fn collapse_repeats_cuts_decoder_loops_keeps_natural_emphasis() {
        // 单字复读(真实样例:"我们要用专业去去去去…去" 40+ 次)
        assert_eq!(collapse_repeats(&format!("我们要用专业{}", "去".repeat(44))), "我们要用专业去去");
        // 带标点的词复读(真实样例:"然后,"×21、"这个,"×19)
        assert_eq!(collapse_repeats(&"然后，".repeat(21)), "然后，然后，");
        assert_eq!(collapse_repeats(&format!("对，从这个，{}", "这个，".repeat(19))), "对，从这个，这个，");
        // 多字单元复读(真实样例:"你像这样,"×18)
        assert_eq!(collapse_repeats(&"你像这样，".repeat(18)), "你像这样，你像这样，");
        // 带空格的字母复读(真实样例:"现在,D I P T Y Y Y…")
        assert_eq!(collapse_repeats(&format!("现在，D I P T{}", " Y".repeat(40))), "现在，D I P T Y Y");

        // 自然重复必须原样保留:强调、笑声、短应答、口吃——门槛按真实数据定标,
        // 下面几条都来自实际笔记(密度 3-6 字/秒,确系人声),折叠它们就是篡改原话。
        assert_eq!(collapse_repeats("对对对"), "对对对");
        assert_eq!(collapse_repeats("哈哈哈哈哈"), "哈哈哈哈哈");
        assert_eq!(collapse_repeats("很好很好"), "很好很好");
        assert_eq!(collapse_repeats("对对对对对对。"), "对对对对对对。", "六连「对」是强调,不是复读");
        assert_eq!(collapse_repeats(&"对不起".repeat(4)), "对不起".repeat(4), "连说四遍对不起是人话");
        assert_eq!(collapse_repeats("这个这个这个这个力"), "这个这个这个这个力", "口吃保留");
        assert_eq!(collapse_repeats("这是完全正常的一句话，没有任何重复。"), "这是完全正常的一句话，没有任何重复。");
        assert_eq!(collapse_repeats(""), "");
    }

    /// 折叠只在「无 token 时间戳」时生效(自回归引擎 qwen3/fire_red 恒空);
    /// CTC 引擎带时间戳,text 与 tokens/timestamps 必须严格等长对应,一个字都不能动
    /// ——否则段内说话人切分(group_tokens_by_boundaries)会错位。
    #[test]
    fn repeat_collapse_only_without_timestamps_and_drops_stale_tokens() {
        let looped = "然后，".repeat(21);
        let t = parse_result_json(&format!(r#"{{"text": "{looped}", "tokens": ["然","后"]}}"#));
        assert_eq!(t.text, "然后，然后，", "无时间戳:折叠生效");
        assert!(t.tokens.is_empty(), "文本被改写 → 陈旧 tokens 必须清掉,不留错位隐患");

        let t = parse_result_json(&format!(
            r#"{{"text": "{looped}", "tokens": ["然","后"], "timestamps": [0.0, 0.1]}}"#
        ));
        assert_eq!(t.text, looped, "有时间戳:原样保留,绝不动");
        assert_eq!(t.tokens.len(), 2);
    }

    #[test]
    fn parse_malformed_json_falls_back_to_empty_transcript() {
        let t = parse_result_json("not json {{");
        assert!(t.text.is_empty() && t.lang.is_empty() && t.tokens.is_empty());
    }

    /// issue #98 回归:sherpa C API 无 try/catch,onnxruntime 的 C++ 异常穿过
    /// extern "C" 进 Rust 会在 catch_unwind 处 abort(__rust_foreign_exception)。
    /// 屏障 shim 必须在 C++ 侧把异常吃成错误码——mode=1 人为 throw,mode=0 正常。
    /// 本测试若让进程 SIGABRT 而非断言失败,即屏障失效。
    #[test]
    fn cxx_exception_barrier_catches_foreign_throw() {
        assert_eq!(unsafe { vn_sherpa_barrier_selftest(1) }, -1, "C++ 异常必须被 shim 捕获为 -1");
        assert_eq!(unsafe { vn_sherpa_barrier_selftest(0) }, 0, "无异常路径返回 0");
    }
}
