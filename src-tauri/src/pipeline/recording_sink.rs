//! 录制期产物装配:按方案决定落哪些轨。
//!
//! 现状(方案 A)每源一条通道 + 一个写盘线程 + 一个 AudioTrackWriter。方案 B 在此
//! 之上多挂一条混音通道:两源的 sink 各自把样本**再复制一份**发给混音线程,由
//! TimelineMixer 按位置合成后写第三条轨 `mixed.wav`。
//!
//! 硬约束:混音是旁路。线程死/写盘失败/内存无界增长都只影响 mixed.wav,两条源轨
//! 与转写热路径不受任何影响(与 keep_audio 的既有哲学一致——音频落盘是增值旁路)。
//! (不是"通道满"——两源到混音线程用的是 crossbeam unbounded 通道,永远不会满;
//! 真正的风险是某一源彻底停止喂料时 TimelineMixer 的累加窗无界增长,见下方
//! MAX_MIXER_WINDOW_SAMPLES 处的处理。)
//!
//! 单源会话(record_system_only)无从混音,直接不建混音线程:该笔记只有方案 A 可选。
//!
//! 装配契约:喂进 TimelineMixer 的必须是 **post-frame_tap** 流(断流已补零帧,样本数
//! 即时间轴位置,见 timeline_mix.rs 模块头注)。本文件正是决定"谁的样本进 mixer"的
//! 装配层——接到 pre-tap 的流会让位置语义直接失效且不报错,排查会非常痛苦。

use crate::audio::timeline_mix::{TimelineMixer, DEFAULT_MARGIN_SAMPLES, MIC, SYSTEM};
use crate::audio::Source;
use crate::store::audio::AudioTrackWriter;
use std::path::Path;

/// 混音成品轨文件名(不含扩展名对应的 source 标识)。下游读取端(转码/枚举/播放)
/// 需要与写入端用同一个名字,故提成常量而非各处散落字面量。
pub const MIXED_TRACK: &str = "mixed";

/// 混音旁路累加窗的样本数上限:30 秒 @16k = 480_000。
///
/// 依据:稳态下(两源都在正常喂料)win.len() 恒等于 margin(DEFAULT_MARGIN_SAMPLES
/// = 6400,400ms);30 秒是它的 75 倍。只有"一源彻底停摆"(录制期配置阶段两源都建了
/// 混音线程,但真正的 capture.start() 失败/设备被拔导致 tap 死掉、一帧都不再喂)才
/// 可能触达这个上限——正常的到达抖动不会。超限即证明这条旁路已经不可能再产出有意义
/// 的 mixed.wav,必须自杀退出,不能任由窗口无界增长(实测约 230MB/小时;Rust 内存
/// 分配失败是 abort,不是可恢复错误,拖累的是整个进程和两条本该完好的源轨)。
const MAX_MIXER_WINDOW_SAMPLES: usize = 480_000;

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
        // 混音只对 Mic+System 两源都在场时有意义:用 contains 而非 len() < 2,一是语义
        // 更准确(顺带表达"混音只服务于这两源"),二是挡住重复源([Mic, Mic] 这种畸形
        // 配置会通过 len() 判据、随后两个 writer 抢同一个 mic.wav)。
        // 单源会话(record_system_only)无从混音:直接退化为方案 A,该笔记只有 A 可选。
        if !(self.inner.sources.contains(&Source::Mic) && self.inner.sources.contains(&Source::System)) {
            return Box::new(self.inner).into_wiring();
        }
        let mut w = Box::new(self.inner).into_wiring();

        let (tx, rx) = crossbeam_channel::unbounded::<(usize, Vec<f32>)>();
        w.joins.push(std::thread::spawn(move || {
            let mut mixer = TimelineMixer::new(DEFAULT_MARGIN_SAMPLES);
            let mut writer = AudioTrackWriter::new(&note_dir, MIXED_TRACK, base_ms);
            // 旁路自杀开关:一旦累加窗超限就 break,不再等 rx 关闭。break 之后不调
            // finish()——已经放弃这条轨,把窗内剩余也吐出去只会让半成品更长,没有意义。
            let mut abandoned = false;
            for (src, chunk) in rx.iter() {
                let out = mixer.accept(src, &chunk);
                if !out.is_empty() {
                    writer.append(&out);
                }
                if mixer.win_len() > MAX_MIXER_WINDOW_SAMPLES {
                    eprintln!(
                        "混音旁路窗口超过 {MAX_MIXER_WINDOW_SAMPLES} 样本(一源已停止喂料,\
                         如设备被拔或 capture 启动失败),放弃 mixed.wav,两条源轨不受影响"
                    );
                    abandoned = true;
                    break;
                }
            }
            // break 出循环时 rx 直接被丢弃:两源 sink 的 `let _ = tx.send(...)` 早已把
            // 发送失败静默吞掉,源轨的写盘线程与本线程之间没有别的耦合,提前退出不会
            // 让源轨"落地"受影响。
            if !abandoned {
                // 两源 sink 都被 drop → 通道关闭 → 定稿窗内剩余,writer Drop 补头刷盘。
                let tail = mixer.finish();
                if !tail.is_empty() {
                    writer.append(&tail);
                }
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
    use crate::store::audio::f32_to_s16;

    /// 喂完样本后拆掉 sink 让通道关闭,再 join 全部写盘线程。带超时:若混音线程
    /// 该退出却没退出(例如未来 `drop(tx)` 那一行被误删,通道永不关闭、`rx.iter()`
    /// 永不结束),用例应该在数秒内失败报出原因,而不是挂到 CI job 超时才被杀——
    /// 那时候排查成本远高于一条断言失败。10 秒足够慢机器跑完这几个用例。
    fn drain(w: Wiring) {
        drop(w.sinks);
        for j in w.joins {
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = done_tx.send(j.join());
            });
            match done_rx.recv_timeout(std::time::Duration::from_secs(10)) {
                Ok(res) => res.unwrap(),
                Err(_) => panic!("混音线程未退出:检查 drop(tx) 是否还在——通道不关闭会让 rx.iter() 永不结束"),
            }
        }
    }

    /// 跳过 44 字节 WAV 头,按 s16le 直接解析 PCM。不为此引入新依赖。
    fn read_pcm_i16(path: &Path) -> Vec<i16> {
        let bytes = std::fs::read(path).unwrap();
        bytes[44..].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect()
    }

    /// 按 Source 找到对应 sink 闭包,避免依赖 sinks 在 Vec 里的下标顺序。
    fn sink_for<'a>(w: &'a mut Wiring, want: Source) -> &'a mut Box<dyn FnMut(&[f32]) + Send> {
        w.sinks.iter_mut().find(|(s, _)| *s == want).map(|(_, f)| f).expect("source 未装配")
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
        // 光比字节数锁不住"只透传一路"的实现——那样字节数照样对得上(finish 会把
        // 所有 pos 拉平到 max(pos),不管 finish 调 1 次还是每块调一次,吐出样本总数
        // 恒等于 max(pos))。这里断言内容:两源等速各喂 0.25,和应处处约等于 0.5。
        let mixed = read_pcm_i16(&dir.path().join("mixed.wav"));
        let want = f32_to_s16(0.5);
        for (i, &v) in mixed.iter().enumerate() {
            assert!((v as i32 - want as i32).abs() <= 2, "位置 {i}: got {v} want {want}(±2 LSB)");
        }
    }

    /// 位置正确性:本模块存在的理由。一源(mic)先跑两个不同取值的整块,另一源
    /// (system)在此期间完全没喂过,随后一次性追上。追上的样本必须落在它真实对应
    /// 的时间轴位置上(与 mic 对应块相加),而不是被顶到窗尾或按到达顺序错配。
    #[test]
    fn lagging_source_content_lands_at_correct_timeline_position() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = build_sinks(dir.path(), 0, &[Source::Mic, Source::System], true);
        {
            let mic = sink_for(&mut w, Source::Mic);
            mic(&vec![1.0; 8000]); // 位置 0..8000
            mic(&vec![2.0; 8000]); // 位置 8000..16000,system 此刻仍一帧未喂
        }
        {
            let system = sink_for(&mut w, Source::System);
            system(&vec![0.1; 16000]); // 一次性追上两个位置区间
        }
        drain(w);

        let mixed = read_pcm_i16(&dir.path().join("mixed.wav"));
        assert_eq!(mixed.len(), 16000);
        let want_first = f32_to_s16(1.1);
        let want_second = f32_to_s16(2.1);
        for (i, &v) in mixed[..8000].iter().enumerate() {
            assert!((v as i32 - want_first as i32).abs() <= 2, "位置 {i}: got {v} want {want_first}(1.0 段)");
        }
        for (i, &v) in mixed[8000..].iter().enumerate() {
            let pos = i + 8000;
            assert!((v as i32 - want_second as i32).abs() <= 2, "位置 {pos}: got {v} want {want_second}(2.0 段)");
        }
    }

    /// 硬约束回归:mixed 建档失败只能拖累 mixed 轨,两条源轨必须完整落盘、内容正确。
    /// 用预先占位同名文件模拟建档失败——AudioTrackWriter::open 对新文件用
    /// create_new(true),文件已存在就直接 Err,不会覆盖或截断已有内容,是 macOS/
    /// Linux 上都稳定可行的失败注入方式,不需要摆弄只读权限。
    #[test]
    fn mixed_track_creation_failure_does_not_affect_source_tracks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mixed.wav"), b"occupies the name, not a real wav").unwrap();
        let mut w = build_sinks(dir.path(), 0, &[Source::Mic, Source::System], true);
        for _ in 0..100 {
            for (_, s) in w.sinks.iter_mut() {
                s(&[0.25; 160]);
            }
        }
        drain(w);
        let mic = read_pcm_i16(&dir.path().join("mic.wav"));
        let system = read_pcm_i16(&dir.path().join("system.wav"));
        assert_eq!(mic.len(), 16000, "mixed 建档失败不该影响 mic 轨完整写出");
        assert_eq!(system.len(), 16000, "mixed 建档失败不该影响 system 轨完整写出");
        let want = f32_to_s16(0.25);
        assert!(mic.iter().all(|&v| (v as i32 - want as i32).abs() <= 2), "mic 内容应不受影响");
        assert!(system.iter().all(|&v| (v as i32 - want as i32).abs() <= 2), "system 内容应不受影响");
    }

    /// Critical 回归:一源彻底停摆(capture 启动失败/设备被拔,一帧都不再喂)时,
    /// 混音旁路的累加窗不会无界增长把内存拖到分配失败——超过 MAX_MIXER_WINDOW_SAMPLES
    /// 混音线程即自杀退出,但两条源轨必须完全不受影响、正常完整落盘。
    #[test]
    fn one_sided_starvation_aborts_mixer_without_affecting_source_tracks() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = build_sinks(dir.path(), 0, &[Source::Mic, Source::System], true);
        let chunk = vec![0.3f32; 8000];
        let rounds = MAX_MIXER_WINDOW_SAMPLES / 8000 + 5; // 保证越过上限
        {
            // system 一帧都不喂:模拟 tap 彻底死掉,只有 mic 单独推进。
            let mic = sink_for(&mut w, Source::Mic);
            for _ in 0..rounds {
                mic(&chunk);
            }
        }
        drain(w);
        let mic = read_pcm_i16(&dir.path().join("mic.wav"));
        assert_eq!(mic.len(), 8000 * rounds, "混音旁路放弃不该影响 mic 源轨完整写出");
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
