pub mod whisper;
pub mod sense_voice;

/// 一次识别的结果文本。
#[derive(Debug, Clone, Default)]
pub struct Transcript {
    pub text: String,
    /// 模型判定的语言标签(SenseVoice 经 sherpa 输出如 "<|zh|>";其它模型/mock 可为空)。
    pub lang: String,
}

/// 语音识别接口。输入须为 16kHz 单声道 f32。
/// 后续计划可新增其它实现（如 whisper-rs）而不动调用方。
pub trait Recognizer: Send {
    fn recognize(&mut self, samples: &[f32]) -> anyhow::Result<Transcript>;
}
