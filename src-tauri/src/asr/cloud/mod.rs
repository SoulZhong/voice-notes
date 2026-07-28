//! 云端流式 ASR 适配层边界(spec §2)。厂商差异封装在 volcano/aliyun 子模块,
//! 会话层只见本文件的类型。协议编解码全部纯函数化,单测不碰网络。

pub mod volcano;
// pub mod aliyun;   // Task 9 解开

#[derive(Debug, Clone, PartialEq)]
pub struct CloudWord { pub text: String, pub start_ms: u64, pub end_ms: u64 }

#[derive(Debug, Clone, PartialEq)]
pub struct DefiniteUtterance {
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
    /// 逐词时间戳(有则喂 diarization 按词切分;空 → 段级降级,同本地 Qwen3)。
    pub words: Vec<CloudWord>,
    /// 厂商语种标签,通常为空 → 语言过滤走文本兜底。
    pub lang: String,
}

#[derive(Debug, Clone)]
pub enum CloudEvent {
    Interim { text: String },
    Definite(DefiniteUtterance),
    /// 连接终止。error=None 为正常关闭(finish 后);Some 触发重连状态机。
    Closed { error: Option<String> },
}

pub struct CloudStream {
    pub push: Box<dyn FnMut(&[f32]) -> anyhow::Result<()> + Send>,
    pub finish: Box<dyn FnOnce() -> anyhow::Result<()> + Send>,
    pub events: crossbeam_channel::Receiver<CloudEvent>,
}

pub trait CloudAsr: Send + Sync {
    fn open_stream(&self) -> anyhow::Result<CloudStream>;
    /// 批式补识(断网缺口):调用方先用本地 VAD 切段(≤15s),逐段调用;
    /// 返回时间戳相对段首,调用方叠加段偏移(阿里实现无时间戳,返回单条
    /// 覆盖 [0, 段长] 的 utterance,见 spec §2.1 偏差)。
    fn transcribe_batch(&self, samples: &[f32]) -> anyhow::Result<Vec<DefiniteUtterance>>;
}

/// f32 [-1,1] → PCM s16le 字节(两家线上格式)。超界钳制防 AGC 毛刺回绕。
pub fn f32_to_pcm_s16le(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for &x in samples {
        let clamped = x.clamp(-1.0, 1.0);
        let scale = if clamped < 0.0 { 32768.0 } else { 32767.0 };
        let v = (clamped * scale) as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

#[cfg(test)]
pub struct MockCloudAsr {
    streams: std::sync::Mutex<std::collections::VecDeque<Vec<CloudEvent>>>,
    batches: std::sync::Mutex<std::collections::VecDeque<anyhow::Result<Vec<DefiniteUtterance>>>>,
    /// 推流样本计数(断言"断连期间没往死流推"用)。
    pub pushed_samples: std::sync::Arc<std::sync::Mutex<usize>>,
}

#[cfg(test)]
impl MockCloudAsr {
    pub fn new(stream_scripts: Vec<Vec<CloudEvent>>,
               batch_scripts: Vec<anyhow::Result<Vec<DefiniteUtterance>>>) -> Self {
        Self {
            streams: std::sync::Mutex::new(stream_scripts.into()),
            batches: std::sync::Mutex::new(batch_scripts.into()),
            pushed_samples: Default::default(),
        }
    }
}

#[cfg(test)]
impl CloudAsr for MockCloudAsr {
    fn open_stream(&self) -> anyhow::Result<CloudStream> {
        let script = self.streams.lock().unwrap().pop_front()
            .ok_or_else(|| anyhow::anyhow!("MockCloudAsr 流脚本耗尽"))?;
        let (tx, rx) = crossbeam_channel::unbounded();
        for ev in script { let _ = tx.send(ev); }
        // tx 随闭包存活:events 通道在 CloudStream drop 前不断开。
        let counter = self.pushed_samples.clone();
        Ok(CloudStream {
            push: Box::new(move |s: &[f32]| { *counter.lock().unwrap() += s.len(); let _ = &tx; Ok(()) }),
            finish: Box::new(|| Ok(())),
            events: rx,
        })
    }
    fn transcribe_batch(&self, _samples: &[f32]) -> anyhow::Result<Vec<DefiniteUtterance>> {
        self.batches.lock().unwrap().pop_front()
            .unwrap_or_else(|| Err(anyhow::anyhow!("MockCloudAsr 批式脚本耗尽")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f32_to_pcm_s16le_clamps_and_encodes_little_endian() {
        let pcm = f32_to_pcm_s16le(&[0.0, 1.0, -1.0, 2.0]);
        assert_eq!(pcm.len(), 8);
        assert_eq!(&pcm[0..2], &[0x00, 0x00]);
        assert_eq!(&pcm[2..4], &(i16::MAX).to_le_bytes());       // 1.0 → 32767
        assert_eq!(&pcm[4..6], &(-32768i16).to_le_bytes());      // -1.0
        assert_eq!(&pcm[6..8], &(i16::MAX).to_le_bytes());       // 超界钳制
    }

    #[test]
    fn mock_cloud_asr_replays_scripts_per_stream_and_batch() {
        let mock = MockCloudAsr::new(
            vec![vec![CloudEvent::Interim { text: "你".into() },
                      CloudEvent::Closed { error: Some("net".into()) }],
                 vec![CloudEvent::Closed { error: None }]],
            vec![Ok(vec![DefiniteUtterance { text: "补".into(), start_ms: 0, end_ms: 500,
                                              words: vec![], lang: String::new() }])],
        );
        let s1 = mock.open_stream().unwrap();
        assert!(matches!(s1.events.recv().unwrap(), CloudEvent::Interim { .. }));
        assert!(matches!(s1.events.recv().unwrap(), CloudEvent::Closed { error: Some(_) }));
        let s2 = mock.open_stream().unwrap();
        assert!(matches!(s2.events.recv().unwrap(), CloudEvent::Closed { error: None }));
        assert!(mock.open_stream().is_err(), "脚本耗尽应报错(测试脚本写漏时快速暴露)");
        assert_eq!(mock.transcribe_batch(&[0.0; 160]).unwrap()[0].text, "补");
        assert_eq!(*mock.pushed_samples.lock().unwrap(), 0);
    }
}
