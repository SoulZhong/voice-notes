pub mod registry;
pub mod split;

use std::path::Path;

/// 声纹嵌入提取器:段音频(16kHz 单声道 f32)→ 嵌入向量。
/// 真实现包 sherpa-onnx speaker embedding 模型;测试用 MockEmbedder。
pub trait SpeakerEmbedder: Send {
    fn embed(&mut self, samples: &[f32]) -> anyhow::Result<Vec<f32>>;
}

/// 带模型身份的嵌入器。**标签在构造时绑定,拿不掉。**
///
/// 为什么需要它:向量写入要声明"这组向量是哪个模型算的"(见 store::voiceprints 的
/// space_ok)。而缓存槽里原本存的是无身份的 `Box<dyn SpeakerEmbedder>`,trait 本身
/// 也没有标签——于是"声明的空间"和"真实用来算的模型"是两个独立来源,中间任何一个
/// 窗口(重建线程 stash 与 set_settings 清缓存交错)都能让它们分家,声明就成了空话
/// (2026-08-19 codex review 设计轮二 P1)。
///
/// 取用时必须核对 [`model`](Self::model) 与当前选型:不符就丢弃重建,绝不将就用。
pub struct TaggedEmbedder {
    model: String,
    inner: Box<dyn SpeakerEmbedder>,
}

impl TaggedEmbedder {
    pub fn new(model: impl Into<String>, inner: Box<dyn SpeakerEmbedder>) -> Self {
        Self { model: model.into(), inner }
    }

    /// 这个实例是用哪个模型建的。写库时的 model 参数应取自这里。
    pub fn model(&self) -> &str {
        &self.model
    }

    /// 拆出内层,交给不关心身份的消费方(会话内部只管算向量)。
    pub fn into_inner(self) -> Box<dyn SpeakerEmbedder> {
        self.inner
    }
}

impl SpeakerEmbedder for TaggedEmbedder {
    fn embed(&mut self, samples: &[f32]) -> anyhow::Result<Vec<f32>> {
        self.inner.embed(samples)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 标签必须跟着实例走。取用方靠它判断"这个实例能不能用",判错的后果是
    /// 用错空间的嵌入器算出一整场向量、再以当前标签写进库(2026-08-19 设计轮二 P1)。
    #[test]
    fn 标签跟着实例走且拆得出内层() {
        struct Zero;
        impl SpeakerEmbedder for Zero {
            fn embed(&mut self, _s: &[f32]) -> anyhow::Result<Vec<f32>> {
                Ok(vec![0.0])
            }
        }
        let mut te = TaggedEmbedder::new("campplus", Box::new(Zero));
        assert_eq!(te.model(), "campplus");
        // 本身就是一个可用的嵌入器,包装不改变行为
        assert_eq!(te.embed(&[0.1]).unwrap(), vec![0.0]);
        // 拆出内层之后标签随之消失——这正是"不关心身份的消费方"该拿到的东西
        let mut inner = te.into_inner();
        assert_eq!(inner.embed(&[0.1]).unwrap(), vec![0.0]);
    }
}
