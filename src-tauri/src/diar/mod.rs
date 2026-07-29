pub mod registry;
pub mod split;

use std::path::Path;

/// 声纹嵌入提取器:段音频(16kHz 单声道 f32)→ 嵌入向量。
/// 真实现包 sherpa-onnx speaker embedding 模型;测试用 MockEmbedder。
pub trait SpeakerEmbedder: Send {
    fn embed(&mut self, samples: &[f32]) -> anyhow::Result<Vec<f32>>;
}

/// sherpa-onnx CAM++ 声纹模型。
pub struct SherpaEmbedder {
    inner: sherpa_onnx::SpeakerEmbeddingExtractor,
}

impl SherpaEmbedder {
    pub fn new(model_path: &Path) -> anyhow::Result<Self> {
        let config = sherpa_onnx::SpeakerEmbeddingExtractorConfig {
            model: Some(model_path.to_string_lossy().into_owned()),
            num_threads: 1,
            ..Default::default()
        };
        let inner = sherpa_onnx::SpeakerEmbeddingExtractor::create(&config)
            .ok_or_else(|| anyhow::anyhow!("加载声纹模型失败(检查 {:?})", model_path))?;
        Ok(Self { inner })
    }
}

impl SpeakerEmbedder for SherpaEmbedder {
    fn embed(&mut self, samples: &[f32]) -> anyhow::Result<Vec<f32>> {
        // 官方 API 是流式喂入:整段一次 accept + input_finished 后 compute,
        // 语义等价于旧 sherpa-rs 的 compute_speaker_embedding(samples, 16000)。
        let stream = self
            .inner
            .create_stream()
            .ok_or_else(|| anyhow::anyhow!("创建声纹流失败"))?;
        stream.accept_waveform(16000, samples);
        stream.input_finished();
        self.inner
            .compute(&stream)
            .ok_or_else(|| anyhow::anyhow!("提取声纹失败(段过短或推理错误)"))
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
