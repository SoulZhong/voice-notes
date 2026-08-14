pub mod engine;
pub mod whisper;
pub mod sense_voice;
pub mod paraformer;
pub mod qwen3;
pub mod fire_red;
pub mod cloud;

/// 一次识别的结果文本。
#[derive(Debug, Clone, Default)]
pub struct Transcript {
    pub text: String,
    /// 模型判定的语言标签(SenseVoice 经 sherpa 输出如 "<|zh|>";其它模型/mock 可为空)。
    pub lang: String,
    /// 识别的 token 列表。
    pub tokens: Vec<String>,
    /// token 级时间戳(秒,相对段首,与 tokens 等长;模型异常时可能为空)。
    /// 供段内说话人分离按变更点切分文本——识别只跑一次,不重复 ASR。
    pub timestamps: Vec<f32>,
}

/// 语音识别接口。输入须为 16kHz 单声道 f32。
/// 后续计划可新增其它实现（如 whisper-rs）而不动调用方。
pub trait Recognizer: Send {
    fn recognize(&mut self, samples: &[f32]) -> anyhow::Result<Transcript>;

    /// 本实例是哪个引擎(落进 NoteMeta.asr_engine 供事后取证)。
    /// 必须由实例自报而不是回头再读设置:识别器可能来自开录前的常驻预载,
    /// 用户在预载与开录之间改了选型时,设置里的值与真正在跑的实例就对不上了
    /// ——那恰好是本字段要抓的场景(Codex review P2)。默认值只兜底测试桩。
    fn engine_id(&self) -> &'static str {
        "unknown"
    }
}

/// settings.asr_provider → sherpa provider 覆盖。空/空白 = 不覆盖(沿用 sherpa 默认)。
/// sherpa-rs 0.6.8 的 get_default_provider() 硬编码 "cpu",想试 CoreML/CUDA 只能经
/// config.provider 显式传入,这里是唯一入口。
pub fn provider_override(setting: &str) -> Option<String> {
    let trimmed = setting.trim();
    if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_default_has_empty_token_fields() {
        let t = Transcript { text: "x".into(), ..Default::default() };
        assert!(t.tokens.is_empty() && t.timestamps.is_empty());
    }

    #[test]
    fn provider_override_blank_is_none_and_value_is_trimmed() {
        assert_eq!(provider_override(""), None);
        assert_eq!(provider_override("  "), None);
        assert_eq!(provider_override(" coreml "), Some("coreml".to_string()));
    }
}
