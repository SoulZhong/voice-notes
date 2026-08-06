//! 录制期产物装配:按方案决定落哪些轨。
//!
//! 现状(方案 A)每源一条通道 + 一个写盘线程 + 一个 AudioTrackWriter。方案 B 在此
//! 之上多挂一条混音通道:两源的 sink 各自把样本**再复制一份**发给混音线程,由
//! TimelineMixer 按位置合成后写第三条轨 `mixed.wav`。
//!
//! 硬约束:混音是旁路。通道满/线程死/写盘失败都只影响 mixed.wav,两条源轨与转写
//! 热路径不受任何影响(与 keep_audio 的既有哲学一致——音频落盘是增值旁路)。
//!
//! 单源会话(record_system_only)无从混音,直接不建混音线程:该笔记只有方案 A 可选。

use crate::audio::timeline_mix::{TimelineMixer, DEFAULT_MARGIN_SAMPLES, MIC, SYSTEM};
use crate::audio::Source;
use crate::store::audio::AudioTrackWriter;
use std::path::Path;

/// 装配产物:每源一个 sink 闭包 + 全部写盘线程句柄。形状与 lib.rs 既有构造一致。
pub struct Wiring {
    pub sinks: Vec<(Source, Box<dyn FnMut(&[f32]) + Send>)>,
    pub joins: Vec<std::thread::JoinHandle<()>>,
}

/// 录制方案。做成**装配工厂**而非逐块转发的 accept:后者要两源共享一个 sink 对象
/// → Arc<Mutex<>> → 在绝不许阻塞的采集回调路径上加锁。工厂形态下每源仍是独立
/// 闭包 + 独立通道,方案差异只体现在"装配出什么",零锁。
pub trait RecordingSink: Send {
    fn into_wiring(self: Box<Self>) -> Wiring;
}

/// 方案 A:每源一条通道 + 一个写盘线程 + 一个 AudioTrackWriter。即现状。
pub struct DualTrackSink {
    note_dir: std::path::PathBuf,
    base_ms: u64,
    sources: Vec<Source>,
}

impl DualTrackSink {
    pub fn new(note_dir: &Path, base_ms: u64, sources: &[Source]) -> Self {
        Self { note_dir: note_dir.to_path_buf(), base_ms, sources: sources.to_vec() }
    }
}

impl RecordingSink for DualTrackSink {
    fn into_wiring(self: Box<Self>) -> Wiring {
        let mut sinks: Vec<(Source, Box<dyn FnMut(&[f32]) + Send>)> = Vec::new();
        let mut joins = Vec::new();
        for source in &self.sources {
            let (tx, rx) = crossbeam_channel::unbounded::<Vec<f32>>();
            let mut w = AudioTrackWriter::new(&self.note_dir, source.as_str(), self.base_ms);
            joins.push(std::thread::spawn(move || {
                for chunk in rx.iter() {
                    w.append(&chunk);
                }
                // sink 被 drop → 通道关闭 → w Drop 补头刷盘收尾。
            }));
            sinks.push((
                *source,
                Box::new(move |s: &[f32]| {
                    let _ = tx.send(s.to_vec());
                }) as Box<dyn FnMut(&[f32]) + Send>,
            ));
        }
        Wiring { sinks, joins }
    }
}

/// 方案 B:在方案 A 之上多挂一条混音轨。两源 sink 各把样本**再复制一份**发给混音
/// 线程,TimelineMixer 按位置合成后写 `mixed.wav`。
pub struct MixedSink {
    inner: DualTrackSink,
}

impl MixedSink {
    pub fn new(inner: DualTrackSink) -> Self {
        Self { inner }
    }
}

impl RecordingSink for MixedSink {
    fn into_wiring(self: Box<Self>) -> Wiring {
        let note_dir = self.inner.note_dir.clone();
        let base_ms = self.inner.base_ms;
        // 单源会话(record_system_only)无从混音:直接退化为方案 A,该笔记只有 A 可选。
        if self.inner.sources.len() < 2 {
            return Box::new(self.inner).into_wiring();
        }
        let mut w = Box::new(self.inner).into_wiring();

        let (tx, rx) = crossbeam_channel::unbounded::<(usize, Vec<f32>)>();
        w.joins.push(std::thread::spawn(move || {
            let mut mixer = TimelineMixer::new(DEFAULT_MARGIN_SAMPLES);
            let mut writer = AudioTrackWriter::new(&note_dir, "mixed", base_ms);
            for (src, chunk) in rx.iter() {
                let out = mixer.accept(src, &chunk);
                if !out.is_empty() {
                    writer.append(&out);
                }
            }
            // 两源 sink 都被 drop → 通道关闭 → 定稿窗内剩余,writer Drop 补头刷盘。
            let tail = mixer.finish();
            if !tail.is_empty() {
                writer.append(&tail);
            }
        }));

        for (source, sink) in w.sinks.iter_mut() {
            let idx = match source {
                Source::Mic => MIC,
                Source::System => SYSTEM,
            };
            let tx = tx.clone();
            let mut inner_sink = std::mem::replace(sink, Box::new(|_: &[f32]| {}));
            *sink = Box::new(move |s: &[f32]| {
                inner_sink(s);
                // 发送失败(混音线程已死)静默忽略:旁路绝不许影响源轨。
                let _ = tx.send((idx, s.to_vec()));
            });
        }
        drop(tx); // 原始 tx 必须丢弃,否则通道永不关闭、混音线程 join 永久阻塞
        w
    }
}

/// 按方案装配。mix=false 即退化为现状。
pub fn build_sinks(note_dir: &Path, base_ms: u64, sources: &[Source], mix: bool) -> Wiring {
    let dual = DualTrackSink::new(note_dir, base_ms, sources);
    if mix {
        Box::new(MixedSink::new(dual)).into_wiring()
    } else {
        Box::new(dual).into_wiring()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::Source;

    /// 喂完样本后拆掉 sink 让通道关闭,再 join 全部写盘线程。
    fn drain(w: Wiring) {
        drop(w.sinks);
        for j in w.joins {
            j.join().unwrap();
        }
    }

    /// mix=false:只落两条源轨,不产生 mixed.wav。
    #[test]
    fn without_mix_only_source_tracks_are_written() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = build_sinks(dir.path(), 0, &[Source::Mic, Source::System], false);
        for (_, s) in w.sinks.iter_mut() {
            s(&[0.5; 160]);
        }
        drain(w);
        assert!(dir.path().join("mic.wav").exists());
        assert!(dir.path().join("system.wav").exists());
        assert!(!dir.path().join("mixed.wav").exists(), "mix=false 不该产出成品轨");
    }

    /// mix=true:三条轨都在,且 mixed 字节数与源轨一致(水位线不丢内容,finish 收尾)。
    #[test]
    fn with_mix_produces_mixed_track_of_equal_length() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = build_sinks(dir.path(), 0, &[Source::Mic, Source::System], true);
        // 两源各喂 100 块 × 160 样本 = 16000 样本 = 1 秒
        for _ in 0..100 {
            for (_, s) in w.sinks.iter_mut() {
                s(&[0.25; 160]);
            }
        }
        drain(w);
        let len = |n: &str| std::fs::metadata(dir.path().join(n)).unwrap().len();
        assert!(dir.path().join("mixed.wav").exists());
        // 44 字节头 + 16000 样本 × 2 字节
        assert_eq!(len("mic.wav"), 44 + 32000);
        assert_eq!(len("mixed.wav"), 44 + 32000, "finish 应把窗内剩余全部定稿");
    }

    /// 单源会话:无从混音,不产出 mixed.wav(降级为只有方案 A 可选)。
    #[test]
    fn single_source_session_produces_no_mixed_track() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = build_sinks(dir.path(), 0, &[Source::System], true);
        for (_, s) in w.sinks.iter_mut() {
            s(&[0.5; 160]);
        }
        drain(w);
        assert!(dir.path().join("system.wav").exists());
        assert!(!dir.path().join("mixed.wav").exists());
    }
}
