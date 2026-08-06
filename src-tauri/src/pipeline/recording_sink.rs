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
///
/// 分层待办(三期兑现,本期刻意不动):这本质是**存储层的文件名契约**,定义在管线层却
/// 被 `store::audio` 反向引用,方向是倒的。搬家会牵动多处 import,合并前不值得。真正
/// 该动的时机是三期落 `Source::Mixed`——那时 `as_str()` 会成为第二个 "mixed" 真值源,
/// 两处一旦漂移就是静默的枚举/转码错配。届时应由 `Source` 导出该字符串,本常量改为
/// 引用它(而不是反过来),真值源保持唯一。
pub const MIXED_TRACK: &str = "mixed";

/// 混音旁路累加窗的样本数上限:30 秒 @16k = 480_000。
///
/// 依据:稳态下(两源都在正常喂料)win.len() 恒等于 margin(DEFAULT_MARGIN_SAMPLES
/// = 6400,400ms);30 秒是它的 75 倍。只有"一源彻底停摆"(录制期配置阶段两源都建了
/// 混音线程,但真正的 capture.start() 失败/设备被拔导致 tap 死掉、一帧都不再喂)才
/// 可能触达这个上限——正常的到达抖动不会。超限即证明这条旁路已经不可能再产出有意义
/// 的 mixed.wav,必须自杀退出,不能任由窗口无界增长(实测约 230MB/小时;Rust 内存
/// 分配失败是 abort,不是可恢复错误,拖累的是整个进程和两条本该完好的源轨)。
///
/// 真实存在的灰区(不是缺陷,如实记录):这条判据分不清"彻底停摆"和"首帧迟到"——
/// 某源 capture.start() 返回 Ok,但首帧因为设备初始化慢等原因超过 30 秒才到,同样
/// 会被判定为已停摆而放弃。frame_tap 只在收到过至少一帧之后才会用零帧补断流,首帧
/// 到达前对面源是真饥饿,没有数据可补。且这个放弃不可逆——哪怕第 31 秒该源真的
/// 恢复喂料,混音线程已经 break 退出,追不回来。
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
        // 更准确(顺带表达"混音只服务于这两源"),二是挡住混音线程被装配到畸形配置
        // 上([Mic, Mic] 会通过 len() < 2 判据继续往下走)。但这只挡住了混音线程本身,
        // 不是重复源的通用防线:[Mic, Mic] 传进 DualTrackSink::into_wiring 仍会为每个
        // 元素各开一个 writer,两者抢同一个 mic.wav——如实说,这里没有堵住那个问题。
        // 单源会话(record_system_only)无从混音:直接退化为方案 A,该笔记只有 A 可选。
        if !(self.inner.sources.contains(&Source::Mic) && self.inner.sources.contains(&Source::System)) {
            return Box::new(self.inner).into_wiring();
        }
        let mut w = Box::new(self.inner).into_wiring();

        // 续录判据:装配时(spawn 混音线程之前)note_dir 下是否已经躺着一条 mixed.wav。
        // 为真即说明本场是续录——AudioTrackWriter::open() 会走"已存在"分支,对齐
        // set_len 后把本场新样本接在上一场内容后面;这条轨此刻已经装着上一场完好的
        // 内容,不是本场从零建的。为假即本场是从零开始的新轨,守卫放弃时删掉它是
        // 零损失(可离线用两条源轨重算)。必须在 spawn 之前取快照并 move 进闭包——
        // 线程里再判断就会把"本场自己刚建出来的文件"误当成"续录的旧文件"。
        let preexisting = note_dir.join(format!("{MIXED_TRACK}.wav")).exists();

        let (tx, rx) = crossbeam_channel::unbounded::<(usize, Vec<f32>)>();
        w.joins.push(std::thread::spawn(move || {
            let mut mixer = TimelineMixer::new(DEFAULT_MARGIN_SAMPLES);
            // Option 包住:abandoned 分支需要在删除文件前先把 writer 显式 drop 掉,
            // 让它的 Drop::flush_header 跑完(否则文件可能还开着、尺寸头是旧的,
            // 删除时机早了在 Windows 等平台还可能因为文件被占用而失败)。
            let mut writer = Some(AudioTrackWriter::new(&note_dir, MIXED_TRACK, base_ms));
            // 旁路自杀开关:一旦累加窗超限就 break,不再等 rx 关闭。break 之后不调
            // finish()——已经放弃这条轨,把窗内剩余也吐出去只会让半成品更长,没有意义。
            let mut abandoned = false;
            for (src, chunk) in rx.iter() {
                let out = mixer.accept(src, &chunk);
                if !out.is_empty() {
                    if let Some(w) = writer.as_mut() {
                        w.append(&out);
                    }
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
            if abandoned {
                // "不调 finish()" 不等于"没有半截内容落盘"——AudioTrackWriter 每攒够
                // 约 1 秒就会 flush_header 回写尺寸,真实的会议场景常常是两源先正常
                // 混了很久(mixed.wav 早已建档、写出大段内容、回写过合法头),某源才
                // 中途掉线触发这条守卫。如果这里什么都不做,盘上会留下一条**完全合法
                // 但被静默截断**的成品轨:list_tracks 按字节数报出错误的 duration,
                // 转码流程会把它当正常轨转成 m4a 并删掉 WAV,播放器把它当第三条轨
                // 叠加播放——用户唯一能看到的线索是一行 eprintln。这比"轨道不存在"
                // 危险得多,所以必须真正删除已写出的内容,而不只是停止再写。
                //
                // 顺序:先 drop 掉 writer 让它的 Drop 补完头、刷盘、关闭文件句柄,
                // 再删除文件——不能反过来(文件还开着时删除在部分平台行为不可控)。
                drop(writer.take());
                // 但只有 !preexisting(本场从零建的轨)才能删:续录场景里这条 WAV
                // 一开始就装着上一场已经写完、完好无损的内容,AudioTrackWriter::open()
                // 是在它尾部 set_len 对齐后追加,不是重新建档。此时 remove_file 会把
                // 上一场那些完好的内容一并冲掉——数据丢失面比"留一条尾部截断的合法
                // 轨"大得多(后者只是本场这一小段脏,前者连之前几十分钟都没了)。
                // 退化策略:续录场景就留着这条被截断的轨,eprintln 留痕即可,不比
                // 修复前更糟(修复前也是"看似合法但截断",只是现在连上一场也保住了)。
                if preexisting {
                    eprintln!(
                        "混音旁路放弃,但 mixed.wav 是续录追加在上一场内容之后的轨,\
                         为避免连带删掉上一场已完好落盘的内容,保留这条被截断的轨\
                         (下游可能读到错误的 duration,需要人工核实)"
                    );
                } else {
                    let mixed_path = note_dir.join(format!("{MIXED_TRACK}.wav"));
                    if let Err(e) = std::fs::remove_file(&mixed_path) {
                        // 纯单源饥饿(从未定稿过任何样本)时 writer 全程停在 Pending,
                        // Drop 里 flush_header 对非 Open 状态直接 return,文件本就不存在,
                        // NotFound 是这种场景下的正常路径,不必当错误声张。其它错误(如
                        // 权限)才值得留痕排查——但无论如何不能 panic,旁路不许拖累主流程。
                        if e.kind() != std::io::ErrorKind::NotFound {
                            eprintln!(
                                "放弃 mixed.wav 后删除残留文件失败,可能留下时长错误的截断轨\
                                 ({}): {e}",
                                mixed_path.display()
                            );
                        }
                    }
                }
                // 已知未覆盖、不在本次修复范围:如果混音线程本身 panic(而不是走到
                // 这条 abandoned 分支),writer 的 Drop 同样会补一个合法头,但这里的
                // 删除逻辑不会执行——那条路径目前没有清理,需要上层(线程 join 处)
                // 处理 panic 的场景才能补上。
            } else {
                // 两源 sink 都被 drop → 通道关闭 → 定稿窗内剩余,writer Drop 补头刷盘。
                let tail = mixer.finish();
                if !tail.is_empty() {
                    if let Some(w) = writer.as_mut() {
                        w.append(&tail);
                    }
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

    /// 喂完样本后拆掉 sink 让通道关闭,再 join 全部写盘线程。带超时:若某条线程
    /// 该退出却没退出(例如未来 `drop(tx)` 那一行被误删,混音线程的通道永不关闭、
    /// `rx.iter()` 永不结束),用例应该在数秒内失败报出原因,而不是挂到 CI job
    /// 超时才被杀——那时候排查成本远高于一条断言失败。10 秒足够慢机器跑完这几个
    /// 用例。注意 joins 里不止混音线程一条:DualTrackSink 的两条源轨写盘线程也在
    /// 这个循环里等,它们卡住时下面这条 panic 消息不该被误读成"一定是混音线程"。
    fn drain(w: Wiring) {
        drop(w.sinks);
        for j in w.joins {
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = done_tx.send(j.join());
            });
            match done_rx.recv_timeout(std::time::Duration::from_secs(10)) {
                Ok(res) => res.unwrap(),
                Err(_) => panic!(
                    "有写盘/混音线程 10 秒内未退出(源轨线程随 sink drop 而结束;混音线程\
                     还依赖 drop(tx) 让通道关闭)——检查对应通道是否还在正常关闭"
                ),
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

    /// sink 闭包只是把样本 send 进 crossbeam 通道,真正落盘由独立线程异步完成。
    /// 测试里如果要在"喂完一段、还不 drain"的中途断言盘上状态(见下面的
    /// starvation_after_partial_success 用例),不能假设 send 一返回文件就已写出——
    /// 这里轮询等文件出现,而不是在 send 之后立刻断言,避免测试本身的时序假设
    /// 比生产代码的真实时序更脆弱、把纯竞态误判成断言失败。
    fn wait_until_exists(path: &Path, timeout: std::time::Duration) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            if path.exists() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        path.exists()
    }

    /// 同 wait_until_exists 的轮询手法,但断言文件字节数超过某个下限——用于确认
    /// "续录追加"真的发生了(而不只是旧文件原样还在),即 AudioTrackWriter::open()
    /// 走过了续录对齐分支并且后续 append 真把新样本写进了同一个文件句柄。
    fn wait_until_size_at_least(path: &Path, min_len: u64, timeout: std::time::Duration) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            if std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) > min_len {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) > min_len
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
    ///
    /// 取值特意避开 f32_to_s16 的饱和区([-1,1] 外 clamp 到 ±32767)。之前这里用的
    /// 是 1.0/2.0 + system 的 0.1 → 1.1/2.1,两者都会被 clamp 成同一个 32767,两段
    /// 期望值塌缩成同一句"处处 32767"——这样"只透传了 mic、system 内容根本没混
    /// 进去"或"system 落到了错误的半区"都测不出来,断言其实什么都没锁住。换成
    /// 0.1/0.2 + 0.05 后两段期望值(约 4915 / 8191)截然不同,任何一种错误实现都会
    /// 立刻在某一段红。
    #[test]
    fn lagging_source_content_lands_at_correct_timeline_position() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = build_sinks(dir.path(), 0, &[Source::Mic, Source::System], true);
        {
            let mic = sink_for(&mut w, Source::Mic);
            mic(&vec![0.1; 8000]); // 位置 0..8000
            mic(&vec![0.2; 8000]); // 位置 8000..16000,system 此刻仍一帧未喂
        }
        {
            let system = sink_for(&mut w, Source::System);
            system(&vec![0.05; 16000]); // 一次性追上两个位置区间
        }
        drain(w);

        let mixed = read_pcm_i16(&dir.path().join("mixed.wav"));
        assert_eq!(mixed.len(), 16000);
        let want_first = f32_to_s16(0.15);
        let want_second = f32_to_s16(0.25);
        for (i, &v) in mixed[..8000].iter().enumerate() {
            assert!((v as i32 - want_first as i32).abs() <= 2, "位置 {i}: got {v} want {want_first}(0.1 段)");
        }
        for (i, &v) in mixed[8000..].iter().enumerate() {
            let pos = i + 8000;
            assert!((v as i32 - want_second as i32).abs() <= 2, "位置 {pos}: got {v} want {want_second}(0.2 段)");
        }
    }

    /// 硬约束回归:mixed 建档失败只能拖累 mixed 轨,两条源轨必须完整落盘、内容正确。
    ///
    /// 用预先占位同名**文件**模拟建档失败是无效的:AudioTrackWriter::open 对已存在
    /// 路径走的是"续录对齐"分支而不是 create_new(true)——`path.exists()` 为真时,
    /// `metadata().len()` 照样能读出旧占位文件的尺寸,`set_len(44)` 直接把垃圾字节
    /// 截掉,`write_all(wav_header(0))` 写进一个合法空头,建档反而**成功**,混音全程
    /// 正常跑完,断言永远绿。真正能让 open() 落到 Err 分支的是把路径占成一个**目录**:
    /// `path.exists()`/`metadata()` 依然成功,但 `OpenOptions::write(true).open(&path)`
    /// 在 macOS/Linux 上会因为对目录发起写打开而返回 EISDIR。
    #[test]
    fn mixed_track_creation_failure_does_not_affect_source_tracks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("mixed.wav")).unwrap();
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
        // 负向断言:占位目录原样保留、没被写穿成文件——证明建档确实走进了 Err 分支,
        // 而不是像旧手法那样悄悄把占位物覆盖成一份"看似失败其实成功"的空 WAV。
        assert!(dir.path().join("mixed.wav").is_dir(), "建档失败不该动到占位目录");
    }

    /// Critical 回归:一源彻底停摆(capture 启动失败/设备被拔,一帧都不再喂)时,
    /// 混音旁路的累加窗不会无界增长把内存拖到分配失败——超过 MAX_MIXER_WINDOW_SAMPLES
    /// 混音线程即自杀退出,但两条源轨必须完全不受影响、正常完整落盘。
    ///
    /// 同时锁住"放弃"本身:光断言 mic 长度锁不住这条用例名字里说的"aborts mixer"——
    /// 把守卫整段删掉后,mixer 会攒满全部样本、rx 关闭后 finish() 把窗内内容照样吐出
    /// 建档写出 mixed.wav,mic 长度丝毫不受影响,断言依然全绿。必须显式断言
    /// mixed.wav 不存在,才是这条用例存在的理由(推演见下:纯单源饥饿场景里 watermark
    /// 恒为 0,writer.append 从未被调用,writer 全程停在 Pending,Drop 里
    /// flush_header 对非 Open 状态直接 return,文件从未被创建过——删掉守卫则会建档
    /// 写出内容,红绿分明)。
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
        assert!(!dir.path().join("mixed.wav").exists(), "旁路自杀应彻底放弃该轨");
    }

    /// Critical 回归:C1 守卫真正危险的场景不是上面这种"纯"单源饥饿(writer 从未
    /// 建档,天然没有残留文件),而是"先正常混了一段、mixed.wav 已经建档写出内容、
    /// 回写过合法头,某源才中途掉线"。AudioTrackWriter 每攒够约 1 秒就 flush_header
    /// 回写尺寸,不调 finish() 并不能避免半截内容落盘——真实的会议场景大概率是这样:
    /// 两源混了很久之后 system 设备被拔,守卫触发时盘上已经是一条完全合法但会被
    /// 悄悄截断在掉线时刻的成品轨,duration 与内容对不上,下游 list_tracks/转码/
    /// 播放器都会把它当完整轨处理。守卫必须连带删除已写出的内容,不能只是停止再写。
    #[test]
    fn starvation_after_partial_success_removes_already_written_mixed_track() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = build_sinks(dir.path(), 0, &[Source::Mic, Source::System], true);
        // 先让两源正常混一段,确保 mixed.wav 真的建档、写出内容、回写过合法头。
        for _ in 0..100 {
            for (_, s) in w.sinks.iter_mut() {
                s(&[0.25; 160]);
            }
        }
        assert!(
            wait_until_exists(&dir.path().join("mixed.wav"), std::time::Duration::from_secs(5)),
            "前置条件:掉线前 mixed.wav 应已建档写出"
        );
        // 随后 system 彻底停摆,只有 mic 继续推进,直到越过累加窗上限触发自杀。
        let chunk = vec![0.3f32; 8000];
        let rounds = MAX_MIXER_WINDOW_SAMPLES / 8000 + 5;
        {
            let mic = sink_for(&mut w, Source::Mic);
            for _ in 0..rounds {
                mic(&chunk);
            }
        }
        drain(w);
        assert!(
            !dir.path().join("mixed.wav").exists(),
            "旁路自杀必须删除已写出的残留内容,不能留下时长错误但语法合法的成品轨"
        );
        let mic = read_pcm_i16(&dir.path().join("mic.wav"));
        assert_eq!(mic.len(), 160 * 100 + 8000 * rounds, "源轨不受混音旁路自杀影响");
    }

    /// Bug 修复回归,与上面 starvation_after_partial_success 相反方向:同样是"守卫
    /// 触发时 mixed.wav 已经装着写出的内容",但这次内容不是本场自己混出来的,而是
    /// **上一场续录前就已经完好落盘**的。remove_file 不加区分地删,会把续录场景下
    /// 上一场那部分完全正常的内容一并冲掉——数据丢失面比"留一条尾部截断的合法轨"
    /// 大得多。守卫必须只删"本场从零建的轨"(可离线用源轨重算,零损失),续录追加
    /// 的轨即便本场这段截断了也必须保留。
    ///
    /// 搭建:先跑一场完整会话(base_ms=0)产出一条 1 秒、内容正确的 mixed.wav 并
    /// 正常 finish,模拟"上一场"。base_ms=1000 严丝合缝对应它的时长(offset_ms=0,
    /// 1 秒=1000ms),这样第二场 AudioTrackWriter::open() 的续录对齐分支
    /// (`set_len` 到 base_ms 对应字节数)刚好不截断也不补零上一场的内容——如果这里
    /// 算错 base_ms,对齐会把断言意图冲掉,所以特意选一个和预存时长严丝合缝的值。
    /// 第二场先正常混一小段(确认 open() 真的走过续录分支、新内容接到旧内容后面,
    /// 而不是像 one_sided_starvation 用例那样全程停在 Pending 从未开过文件),再让
    /// system 彻底停摆触发守卫。
    #[test]
    fn starvation_during_continuation_preserves_preexisting_mixed_track() {
        let dir = tempfile::tempdir().unwrap();

        // 上一场:正常混 1 秒并正常 finish,mixed.wav 落地一条完好的成品轨。
        {
            let mut w = build_sinks(dir.path(), 0, &[Source::Mic, Source::System], true);
            for _ in 0..100 {
                for (_, s) in w.sinks.iter_mut() {
                    s(&[0.25; 160]);
                }
            }
            drain(w);
        }
        let mixed_path = dir.path().join("mixed.wav");
        assert!(mixed_path.exists(), "前置条件:上一场应正常产出 mixed.wav");
        let old_mixed = read_pcm_i16(&mixed_path);
        assert_eq!(old_mixed.len(), 16000, "前置条件:上一场应是 1 秒(16000 样本)");
        let old_len_bytes = std::fs::metadata(&mixed_path).unwrap().len();

        // 本场续录:base_ms=1000 对应上一场的 1 秒时长。
        let mut w = build_sinks(dir.path(), 1000, &[Source::Mic, Source::System], true);
        // 先正常混一小段,确认 open() 真的把新内容接在旧内容后面(文件变长),
        // 而不是全程停在 Pending 从未打开过文件。
        for _ in 0..50 {
            for (_, s) in w.sinks.iter_mut() {
                s(&[0.25; 160]);
            }
        }
        assert!(
            wait_until_size_at_least(&mixed_path, old_len_bytes, std::time::Duration::from_secs(5)),
            "前置条件:续录期正常混音阶段应已把新内容追加到上一场内容之后"
        );
        // 随后 system 彻底停摆,只有 mic 继续推进,直到越过累加窗上限触发自杀。
        let chunk = vec![0.3f32; 8000];
        let rounds = MAX_MIXER_WINDOW_SAMPLES / 8000 + 5;
        {
            let mic = sink_for(&mut w, Source::Mic);
            for _ in 0..rounds {
                mic(&chunk);
            }
        }
        drain(w);

        // 核心断言:preexisting 场景下守卫不能删文件——留一条尾部截断的合法轨,
        // 也不能把上一场完好的内容一并冲掉。
        assert!(
            mixed_path.exists(),
            "续录场景守卫误删了 mixed.wav,上一场已完好落盘的内容随之丢失\
             (本次修复要防的正是这个)"
        );
        let mixed = read_pcm_i16(&mixed_path);
        assert!(
            mixed.len() >= old_mixed.len(),
            "续录追加后的内容不该比上一场还短: got {} want >= {}",
            mixed.len(),
            old_mixed.len()
        );
        // 上一场内容必须原样保留在文件开头(±2 LSB 容差同其它用例)。
        for (i, (&got, &want)) in mixed[..old_mixed.len()].iter().zip(old_mixed.iter()).enumerate() {
            assert!(
                (got as i32 - want as i32).abs() <= 2,
                "位置 {i}: 上一场内容被改动,got {got} want {want}"
            );
        }
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
