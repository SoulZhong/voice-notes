use super::engine::{ModelSpec, OfflineEngine};
use super::{Recognizer, Transcript};
use std::path::Path;

/// 基于 sherpa-onnx 的离线 Paraformer-large 识别器(中文,带 token 级时间戳)。
pub struct ParaformerRecognizer {
    inner: OfflineEngine,
}

impl ParaformerRecognizer {
    /// model_dir 应包含 model.int8.onnx 与 tokens.txt(manifest PF_DIR 解压布局)。
    /// provider: None = sherpa 默认(CPU);见 asr::provider_override。
    pub fn new(model_dir: &Path, provider: Option<String>) -> anyhow::Result<Self> {
        let model = model_dir.join("model.int8.onnx");
        let tokens = model_dir.join("tokens.txt");
        if !model.exists() || !tokens.exists() {
            anyhow::bail!("在 {:?} 找不到 model.int8.onnx / tokens.txt", model_dir);
        }
        let spec = ModelSpec::Paraformer {
            model: model.to_string_lossy().into_owned(),
            tokens: tokens.to_string_lossy().into_owned(),
        };
        let inner = OfflineEngine::new(&spec, super::sense_voice::default_threads(), provider.as_deref())
            .map_err(|e| anyhow::anyhow!("加载 Paraformer 失败: {e}"))?;
        Ok(Self { inner })
    }
}

impl Recognizer for ParaformerRecognizer {
    fn engine_id(&self) -> &'static str {
        crate::settings::ASR_PARAFORMER
    }

    fn recognize(&mut self, samples: &[f32]) -> anyhow::Result<Transcript> {
        // paraformer 无语言标签时 lang 为空串:语言过滤走文本兜底(whisper 同路径)
        self.inner.transcribe(16000, samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_model_dir_errors_cleanly() {
        let err = ParaformerRecognizer::new(std::path::Path::new("/nonexistent-pf-dir"), None)
            .err()
            .expect("目录不存在应报错而非 panic");
        assert!(err.to_string().contains("nonexistent-pf-dir"));
    }

    /// 需本机已下载 paraformer 工件:cargo test --lib asr::paraformer -- --ignored
    #[test]
    #[ignore]
    fn transcribes_nonempty_with_timestamps() {
        let dir = crate::models::root().join(crate::models::PF_DIR);
        let mut r = ParaformerRecognizer::new(&dir, None).unwrap();
        // 1s 静音也应返回结构完整(text 可空);真语音断言用 golden 脚本做
        let t = r.recognize(&vec![0.0f32; 16000]).unwrap();
        assert_eq!(t.tokens.len(), t.timestamps.len());
    }
}
