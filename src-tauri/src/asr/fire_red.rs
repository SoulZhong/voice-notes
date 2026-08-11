use super::engine::{ModelSpec, OfflineEngine};
use super::{Recognizer, Transcript};
use std::path::Path;

/// 基于 sherpa-onnx 的离线 FireRedASR2-AED int8 识别器(小红书 2026-02 开源,
/// Apache-2.0)。选型依据(2026-08-11 调研 §3.2):开源可 CPU 跑的中文精度天花板
/// (4 公开集均值 CER 3.05%),中英混说 + 20+ 方言。官方单线程 RTF 0.333:比
/// SenseVoice 慢一档,实时可用但余量小,首推会后精转写/bake-off 二遍档。
/// 已知限制(2026-08-11 真机验证):sherpa-onnx 1.13.4 的 FireRed 结果 JSON 无
/// token 时间戳(timestamps 恒空,该开关是 Whisper 专属)→ diarization 段级降级,
/// 同 Qwen3;上游透出后可去掉此限制。lang 亦为空,语言过滤走文本兜底。
pub struct FireRedRecognizer {
    inner: OfflineEngine,
}

impl FireRedRecognizer {
    /// model_dir 应包含 encoder.int8.onnx / decoder.int8.onnx / tokens.txt
    /// (manifest FR_DIR 解压布局)。provider: None = sherpa 默认(CPU)。
    pub fn new(model_dir: &Path, provider: Option<String>) -> anyhow::Result<Self> {
        let encoder = model_dir.join("encoder.int8.onnx");
        let decoder = model_dir.join("decoder.int8.onnx");
        let tokens = model_dir.join("tokens.txt");
        for (p, what) in [(&encoder, "encoder.int8.onnx"), (&decoder, "decoder.int8.onnx"), (&tokens, "tokens.txt")] {
            if !p.exists() {
                anyhow::bail!("在 {:?} 找不到 {what}", model_dir);
            }
        }
        let spec = ModelSpec::FireRed {
            encoder: encoder.to_string_lossy().into_owned(),
            decoder: decoder.to_string_lossy().into_owned(),
            tokens: tokens.to_string_lossy().into_owned(),
        };
        let inner = OfflineEngine::new(&spec, super::sense_voice::default_threads(), provider.as_deref())
            .map_err(|e| anyhow::anyhow!("加载 FireRedASR2 失败: {e}"))?;
        Ok(Self { inner })
    }
}

impl Recognizer for FireRedRecognizer {
    fn recognize(&mut self, samples: &[f32]) -> anyhow::Result<Transcript> {
        // FireRed 无语言标签(lang 空):语言过滤走文本兜底(paraformer/whisper 同路径)。
        self.inner.transcribe(16000, samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_model_dir_errors_cleanly() {
        let err = FireRedRecognizer::new(std::path::Path::new("/nonexistent-fr-dir"), None)
            .err()
            .expect("目录不存在应报错而非 panic");
        assert!(err.to_string().contains("nonexistent-fr-dir"));
    }

    /// 需本机已下载 firered 工件:cargo test --lib asr::fire_red -- --ignored --nocapture
    /// 钉能力形状(2026-08-11 真机结果):官方中英混说样例识别正确;timestamps 恒空
    /// (段级降级契约,若上游某版透出时间戳,此断言会翻红提醒解锁 diarization 细分)。
    #[test]
    #[ignore]
    fn transcribes_mixed_speech_and_pins_no_timestamps() {
        let dir = crate::models::asr_model_dir(crate::settings::ASR_FIRERED);
        let mut r = FireRedRecognizer::new(&dir, None).unwrap();
        // 官方自带样例,已知参考转写:"昨天是 MONDAY TODAY IS礼拜二 THE DAY AFTER
        // TOMORROW是星期三"(中英混说)。断言关键词而非全句,容忍上游小版本波动。
        let official = dir.join("test_wavs/0.wav");
        let wav = {
            let mut rd = hound::WavReader::open(&official).expect("官方样例随工件分发");
            rd.samples::<i16>().map(|s| s.unwrap() as f32 / 32768.0).collect::<Vec<f32>>()
        };
        let t = r.recognize(&wav).unwrap();
        eprintln!("official-0.wav => {:?} tokens={} ts={}", t.text, t.tokens.len(), t.timestamps.len());
        assert!(t.text.contains("礼拜二") && t.text.to_uppercase().contains("MONDAY"), "中英混说样例应命中关键词: {}", t.text);
        assert!(t.timestamps.is_empty(), "1.13.4 无时间戳(段级降级);非空说明上游已透出,可解锁词级切分");
        assert_eq!(t.lang, "", "FireRed 无语言标签,语言过滤走文本兜底");
    }
}
