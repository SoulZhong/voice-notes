use super::{Recognizer, Transcript};
use std::path::Path;

/// 基于 sherpa-onnx SenseVoice-small 的离线识别器（zh/en/ja/ko/yue 多语言）。
pub struct SenseVoiceRecognizer {
    inner: sherpa_rs::sense_voice::SenseVoiceRecognizer,
}

impl SenseVoiceRecognizer {
    /// model_dir 应包含 model.onnx（非 int8，全精度）和 tokens.txt。
    pub fn new(model_dir: &Path) -> anyhow::Result<Self> {
        let model = find_model_onnx(model_dir)?;
        let tokens = find_tokens(model_dir)?;

        let num_threads = std::thread::available_parallelism()
            .map(|n| n.get().min(8) as i32)
            .unwrap_or(4);

        let config = sherpa_rs::sense_voice::SenseVoiceConfig {
            model: model.to_string_lossy().into_owned(),
            tokens: tokens.to_string_lossy().into_owned(),
            language: "auto".into(), // zh/en 混合自动检测
            use_itn: true,
            provider: None,
            num_threads: Some(num_threads),
            debug: false,
        };
        let inner = sherpa_rs::sense_voice::SenseVoiceRecognizer::new(config)
            .map_err(|e| anyhow::anyhow!("加载 SenseVoice 失败: {e}"))?;
        Ok(Self { inner })
    }
}

impl Recognizer for SenseVoiceRecognizer {
    fn recognize(&mut self, samples: &[f32]) -> anyhow::Result<Transcript> {
        let result = self.inner.transcribe(16000, samples);
        Ok(Transcript {
            text: result.text,
            lang: result.lang,
            tokens: result.tokens,
            timestamps: result.timestamps,
        })
    }
}

/// 找到目录中的 model.onnx，优先跳过 int8 量化版本（全精度中文质量更好）。
fn find_model_onnx(dir: &Path) -> anyhow::Result<std::path::PathBuf> {
    let mut fallback: Option<std::path::PathBuf> = None;
    for entry in std::fs::read_dir(dir)? {
        let p = entry?.path();
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name.ends_with(".onnx") {
            if !name.contains("int8") {
                // 优先返回全精度版本
                return Ok(p);
            }
            // int8 备选
            fallback = Some(p);
        }
    }
    fallback.ok_or_else(|| anyhow::anyhow!("在 {:?} 找不到 model.onnx", dir))
}

/// 在目录中找到 tokens.txt。
fn find_tokens(dir: &Path) -> anyhow::Result<std::path::PathBuf> {
    let exact = dir.join("tokens.txt");
    if exact.exists() {
        return Ok(exact);
    }
    for entry in std::fs::read_dir(dir)? {
        let p = entry?.path();
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name.ends_with("tokens.txt") {
            return Ok(p);
        }
    }
    anyhow::bail!("在 {:?} 找不到 tokens.txt", dir)
}
