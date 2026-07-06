pub mod whisper;
pub mod sense_voice;
pub mod paraformer;

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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_default_has_empty_token_fields() {
        let t = Transcript { text: "x".into(), ..Default::default() };
        assert!(t.tokens.is_empty() && t.timestamps.is_empty());
    }
}
