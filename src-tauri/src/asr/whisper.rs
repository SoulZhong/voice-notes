use super::engine::{ModelSpec, OfflineEngine};
use super::{Recognizer, Transcript};
use std::path::Path;

/// 基于 sherpa-onnx 的离线 Whisper 识别器。
pub struct WhisperRecognizer {
    inner: OfflineEngine,
}

impl WhisperRecognizer {
    /// model_dir 应包含 sherpa-onnx 导出的 *-encoder.onnx / *-decoder.onnx / tokens.txt。
    /// provider: None = sherpa 默认(CPU);见 asr::provider_override。
    pub fn new(model_dir: &Path, provider: Option<String>) -> anyhow::Result<Self> {
        // Prefer int8 onnx for speed on CPU (base int8 accuracy is fine for the skeleton);
        // fall back to the full-precision file when int8 is absent.
        let encoder = find_onnx(model_dir, "encoder")?;
        let decoder = find_onnx(model_dir, "decoder")?;
        let tokens = super::sense_voice::find_tokens(model_dir)?;
        let spec = ModelSpec::Whisper {
            encoder: encoder.to_string_lossy().into_owned(),
            decoder: decoder.to_string_lossy().into_owned(),
            tokens: tokens.to_string_lossy().into_owned(),
        };
        let inner = OfflineEngine::new(&spec, super::sense_voice::default_threads(), provider.as_deref())
            .map_err(|e| anyhow::anyhow!("加载 Whisper 失败: {e}"))?;
        Ok(Self { inner })
    }
}

impl Recognizer for WhisperRecognizer {
    fn engine_id(&self) -> &'static str {
        crate::settings::ASR_WHISPER
    }

    fn recognize(&mut self, samples: &[f32]) -> anyhow::Result<Transcript> {
        // token 时间戳未启用(enable_token_timestamps=0,迁移前行为):timestamps 恒空,
        // diarization 走段级降级;lang 若模型给出则透传(此前恒空,语言过滤只会更准)。
        self.inner.transcribe(16000, samples)
    }
}

/// 在目录中找到文件名包含关键字的 .onnx 文件，优先 int8 量化版本（CPU 上更快）；
/// 没有 int8 时回退到全精度版本。
fn find_onnx(dir: &Path, keyword: &str) -> anyhow::Result<std::path::PathBuf> {
    let mut fallback: Option<std::path::PathBuf> = None;
    for entry in std::fs::read_dir(dir)? {
        let p = entry?.path();
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name.ends_with(".onnx") && name.contains(keyword) {
            if name.contains("int8") {
                return Ok(p);
            }
            fallback = Some(p);
        }
    }
    fallback.ok_or_else(|| anyhow::anyhow!("在 {:?} 找不到包含 '{}' 的 .onnx", dir, keyword))
}
