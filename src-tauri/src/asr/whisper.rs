use super::{Recognizer, Transcript};
use std::path::Path;

/// 基于 sherpa-onnx 的离线 Whisper 识别器。
pub struct WhisperRecognizer {
    inner: sherpa_rs::whisper::WhisperRecognizer,
}

impl WhisperRecognizer {
    /// model_dir 应包含 sherpa-onnx 导出的 *-encoder.onnx / *-decoder.onnx / tokens.txt。
    pub fn new(model_dir: &Path) -> anyhow::Result<Self> {
        // Prefer non-int8 onnx files for quality; find_one picks first match.
        let encoder = find_onnx(model_dir, "encoder")?;
        let decoder = find_onnx(model_dir, "decoder")?;
        let tokens = find_tokens(model_dir)?;

        let config = sherpa_rs::whisper::WhisperConfig {
            encoder: encoder.to_string_lossy().into_owned(),
            decoder: decoder.to_string_lossy().into_owned(),
            tokens: tokens.to_string_lossy().into_owned(),
            language: "en".into(), // 测试仅需 hello/world；语种固定为英语
            ..Default::default()
        };
        let inner = sherpa_rs::whisper::WhisperRecognizer::new(config)
            .map_err(|e| anyhow::anyhow!("加载 Whisper 失败: {e}"))?;
        Ok(Self { inner })
    }
}

impl Recognizer for WhisperRecognizer {
    fn recognize(&mut self, samples: &[f32]) -> anyhow::Result<Transcript> {
        // sherpa-rs 0.6.8: transcribe(&mut self, sample_rate: u32, samples: &[f32]) -> WhisperRecognizerResult
        let result = self.inner.transcribe(16000, samples);
        Ok(Transcript { text: result.text })
    }
}

/// 在目录中找到文件名包含关键字且不含 "int8" 的 .onnx 文件（优先非量化版本）。
fn find_onnx(dir: &Path, keyword: &str) -> anyhow::Result<std::path::PathBuf> {
    let mut fallback: Option<std::path::PathBuf> = None;
    for entry in std::fs::read_dir(dir)? {
        let p = entry?.path();
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name.ends_with(".onnx") && name.contains(keyword) {
            if !name.contains("int8") {
                return Ok(p);
            }
            fallback = Some(p);
        }
    }
    fallback.ok_or_else(|| anyhow::anyhow!("在 {:?} 找不到包含 '{}' 的 .onnx", dir, keyword))
}

/// 在目录中找到 tokens.txt（兼容 base-tokens.txt 等命名）。
fn find_tokens(dir: &Path) -> anyhow::Result<std::path::PathBuf> {
    // Exact match first
    let exact = dir.join("tokens.txt");
    if exact.exists() {
        return Ok(exact);
    }
    // Fallback: any file ending with tokens.txt
    for entry in std::fs::read_dir(dir)? {
        let p = entry?.path();
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name.ends_with("tokens.txt") {
            return Ok(p);
        }
    }
    anyhow::bail!("在 {:?} 找不到 tokens.txt", dir)
}
