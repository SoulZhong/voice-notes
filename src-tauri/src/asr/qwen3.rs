use super::engine::{ModelSpec, OfflineEngine};
use super::{Recognizer, Transcript};
use std::path::Path;

/// 基于 sherpa-onnx 的离线 Qwen3-ASR 0.6B int8 识别器(52 语种/中英混说,
/// LLM 解码,原生支持热词上下文偏置)。选型依据与已知限制(无 token 时间戳 →
/// diarization 段级降级;自回归 CPU 延迟需实测)见 2026-07-28 ASR 调研。
pub struct Qwen3Recognizer {
    inner: OfflineEngine,
}

impl Qwen3Recognizer {
    /// model_dir 应包含 conv_frontend.onnx / encoder.int8.onnx / decoder.int8.onnx /
    /// tokenizer/(vocab.json 等),即 manifest QWEN3_DIR 解压布局。
    /// provider: None = sherpa 默认(CPU);hotwords: 逗号分隔热词,None = 不启用。
    pub fn new(model_dir: &Path, provider: Option<String>, hotwords: Option<String>) -> anyhow::Result<Self> {
        let conv_frontend = model_dir.join("conv_frontend.onnx");
        let encoder = model_dir.join("encoder.int8.onnx");
        let decoder = model_dir.join("decoder.int8.onnx");
        let tokenizer_dir = model_dir.join("tokenizer");
        for (p, what) in [
            (&conv_frontend, "conv_frontend.onnx"),
            (&encoder, "encoder.int8.onnx"),
            (&decoder, "decoder.int8.onnx"),
            (&tokenizer_dir, "tokenizer/"),
        ] {
            if !p.exists() {
                anyhow::bail!("在 {:?} 找不到 {what}", model_dir);
            }
        }
        let spec = ModelSpec::Qwen3 {
            conv_frontend: conv_frontend.to_string_lossy().into_owned(),
            encoder: encoder.to_string_lossy().into_owned(),
            decoder: decoder.to_string_lossy().into_owned(),
            tokenizer_dir: tokenizer_dir.to_string_lossy().into_owned(),
            hotwords,
        };
        let inner = OfflineEngine::new(&spec, super::sense_voice::default_threads(), provider.as_deref())
            .map_err(|e| anyhow::anyhow!("加载 Qwen3-ASR 失败: {e}"))?;
        Ok(Self { inner })
    }
}

impl Recognizer for Qwen3Recognizer {
    fn engine_id(&self) -> &'static str {
        crate::settings::ASR_QWEN3
    }

    fn recognize(&mut self, samples: &[f32]) -> anyhow::Result<Transcript> {
        // LLM 解码无 token 时间戳(timestamps 恒空 → diarization 段级降级);
        // lang 若模型给出则透传。
        self.inner.transcribe(16000, samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_model_dir_errors_cleanly() {
        let err = Qwen3Recognizer::new(std::path::Path::new("/nonexistent-qwen3-dir"), None, None)
            .err()
            .expect("目录不存在应报错而非 panic");
        assert!(err.to_string().contains("nonexistent-qwen3-dir"));
    }
}
