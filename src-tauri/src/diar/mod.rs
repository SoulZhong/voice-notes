pub mod registry;

use std::path::Path;

/// 声纹嵌入提取器:段音频(16kHz 单声道 f32)→ 嵌入向量。
/// 真实现包 sherpa-onnx speaker embedding 模型;测试用 MockEmbedder。
pub trait SpeakerEmbedder: Send {
    fn embed(&mut self, samples: &[f32]) -> anyhow::Result<Vec<f32>>;
}

/// sherpa-onnx CAM++ 声纹模型。
pub struct SherpaEmbedder {
    inner: sherpa_rs::speaker_id::EmbeddingExtractor,
}

impl SherpaEmbedder {
    pub fn new(model_path: &Path) -> anyhow::Result<Self> {
        let config = sherpa_rs::speaker_id::ExtractorConfig {
            model: model_path.to_string_lossy().into_owned(),
            num_threads: Some(1),
            ..Default::default()
        };
        let inner = sherpa_rs::speaker_id::EmbeddingExtractor::new(config)
            .map_err(|e| anyhow::anyhow!("加载声纹模型失败: {e}"))?;
        Ok(Self { inner })
    }
}

impl SpeakerEmbedder for SherpaEmbedder {
    fn embed(&mut self, samples: &[f32]) -> anyhow::Result<Vec<f32>> {
        self.inner
            .compute_speaker_embedding(samples.to_vec(), 16000)
            .map_err(|e| anyhow::anyhow!("提取声纹失败: {e}"))
    }
}

/// 测试用:按预置脚本依次返回向量,耗尽后返回最后一个;可注入失败。
pub struct MockEmbedder {
    script: std::collections::VecDeque<anyhow::Result<Vec<f32>>>,
    last: Option<Vec<f32>>,
}

impl MockEmbedder {
    pub fn new(script: Vec<anyhow::Result<Vec<f32>>>) -> Self {
        Self { script: script.into(), last: None }
    }
}

impl SpeakerEmbedder for MockEmbedder {
    fn embed(&mut self, _samples: &[f32]) -> anyhow::Result<Vec<f32>> {
        match self.script.pop_front() {
            Some(Ok(v)) => {
                self.last = Some(v.clone());
                Ok(v)
            }
            Some(Err(e)) => Err(e),
            None => self
                .last
                .clone()
                .ok_or_else(|| anyhow::anyhow!("MockEmbedder 脚本已耗尽")),
        }
    }
}
