//! 录制期产物装配:按方案决定落哪些轨。
//!
//! 现状(方案 A)每源一条通道 + 一个写盘线程 + 一个 AudioTrackWriter。方案 B 在此
//! 之上多挂一条混音通道:两源的 sink 各自把样本**再复制一份**发给混音线程,由
//! TimelineMixer 按位置合成后写第三条轨 `mixed.wav`。
//!
//! 硬约束:混音是旁路。线程死、写盘失败、队列拥塞或累加窗超限都只影响 mixed.wav,
//! 两条源轨与转写热路径不受任何影响。mixed 使用有界 `try_send`:满即整轨放弃并
//! 回滚,绝不反压 segment worker;另由 MAX_MIXER_WINDOW_SAMPLES 防一源停摆时窗增长。
//!
//! 单源会话(比如系统声音源构建失败,只剩麦克风)无从混音,直接不建混音线程:该笔记
//! 只有方案 A 可选。
//!
//! 装配契约:喂进 TimelineMixer 的必须是 **post-frame_tap** 流。FrameTap 记录首帧
//! 相对共同单调时钟的偏移,断流则补零;本层用首帧偏移 + 后续样本数组成真实时间轴。

use crate::audio::timeline_mix::{TimelineMixer, DEFAULT_MARGIN_SAMPLES, MIC, SYSTEM};
use crate::audio::Source;
use crate::pipeline::frame_tap::SourceHealth;
use crate::store::audio::{pre_session_track_len, repair_wav_header, AudioTrackWriter};
use crossbeam_channel::TrySendError;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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
/// mixed 旁路的有界队列容量,口径是**两源合计的块数**(mic 与 system 共用这一条通道)。
/// 按常见 10ms 块算:两源都在正常喂料时约 5.1 秒,只有一源在喂时约 10.2 秒。满时立即
/// 放弃整条 mixed,不阻塞 segment worker,也不影响各源自己的无界保真写盘队列。
///
/// 为什么不是更省内存的 256:那是**单源**口径(注释写"约 2.5 秒"),两源共用一条通道后
/// 实际只剩 1.3~2.6 秒,
/// 而误放弃的代价是整场实验数据没了且不可逆(源轨在,但录制期混音的时基优势重算不回来)。
/// 1024 块 16k f32 满载也才约 0.6MB,同一文件里 MAX_MIXER_WINDOW_SAMPLES 已经允许约
/// 1.9MB 的累加窗。取舍很清楚:宁可多占约 0.6MB,也不因为一次写盘卡顿就误弃整轨。
const MIXED_QUEUE_CAPACITY: usize = 1024;

struct MixedChunk {
    src: usize,
    start: u64,
    samples: Vec<f32>,
}

fn try_enqueue_mixed(
    tx: &crossbeam_channel::Sender<MixedChunk>,
    abandoned: &AtomicBool,
    src: usize,
    start: u64,
    samples: &[f32],
) -> bool {
    if abandoned.load(Ordering::Acquire) {
        return false;
    }
    match tx.try_send(MixedChunk { src, start, samples: samples.to_vec() }) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            if !abandoned.swap(true, Ordering::AcqRel) {
                eprintln!(
                    "mixed 写盘队列已满({MIXED_QUEUE_CAPACITY} 块),放弃 mixed.wav,\
                     源轨与转写继续"
                );
            }
            false
        }
        Err(TrySendError::Disconnected(_)) => {
            abandoned.store(true, Ordering::Release);
            false
        }
    }
}

#[derive(Clone, Copy)]
enum MixedRollback {
    RemoveNew,
    /// 截回本场开始前的对齐基线(`pre_session_track_len`)。**不是**装配时的文件长度:
    /// `AudioTrackWriter::open()` 几乎总会把续录轨截短到 `base_ms` 对应的位置(base_ms
    /// 是续录前最大 end_ms,文件尾那段没进 segment 的静音会被切掉),截掉的字节不可逆,
    /// 回滚到旧长度只会把本场刚混出的内容填进空位,拼出一条时长不变、下游无从分辨的
    /// 混合体。基线口径的推导见 `store::audio::pre_session_track_len`。
    Restore(u64),
    PreserveUnknown,
}

/// 放弃 mixed 时回滚。新轨直接删;续录轨截回对齐基线并修正头,于是文件恒为上一场
/// 内容的**真前缀**——既不丢上一场,也不把本场任何字节伪装成上一场的内容。
///
/// 只在本场真的成功 append 过之后才该调用:没 append 过 `open()` 就从未执行,文件还是
/// 装配前的样子,此时任何 set_len / 重写头都是纯粹的破坏面(路径上若是个非 WAV 文件,
/// `repair_wav_header` 会直接写坏它的前 44 字节)。
fn rollback_mixed(path: &Path, rollback: MixedRollback) {
    match rollback {
        MixedRollback::Restore(len) => {
            let result = (|| -> anyhow::Result<()> {
                let file = std::fs::OpenOptions::new().write(true).open(path)?;
                file.set_len(len)?;
                drop(file);
                repair_wav_header(path)?;
                Ok(())
            })();
            if let Err(e) = result {
                eprintln!("mixed 放弃后回滚续录轨失败({}): {e}", path.display());
            }
        }
        MixedRollback::RemoveNew => {
            if let Err(e) = std::fs::remove_file(path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    eprintln!("mixed 放弃后删除残留失败({}): {e}", path.display());
                }
            }
        }
        // 装配时无法确认路径是否已有用户数据,宁可留下可诊断残留也不冒险删除。
        MixedRollback::PreserveUnknown => {}
    }
}

/// 装配产物:每源一个 sink 闭包 + 全部写盘线程句柄。形状与 lib.rs 既有构造一致。
pub struct Wiring {
    pub sinks: Vec<(Source, Box<dyn FnMut(&[f32]) + Send>)>,
    pub joins: Vec<std::thread::JoinHandle<()>>,
    /// 每源 writer 本场至少成功追加过一个非空块。停录对账只消费这些源,避免旧 WAV
    /// 在启动失败的续录中被误当成本场产物。
    pub activity: Vec<(Source, Arc<AtomicBool>)>,
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
        let mut activity = Vec::new();
        for source in &self.sources {
            let (tx, rx) = crossbeam_channel::unbounded::<Vec<f32>>();
            let mut w = AudioTrackWriter::new(&self.note_dir, source.as_str(), self.base_ms);
            let wrote = Arc::new(AtomicBool::new(false));
            let wrote_worker = wrote.clone();
            joins.push(std::thread::spawn(move || {
                for chunk in rx.iter() {
                    if !chunk.is_empty() && w.append(&chunk) {
                        wrote_worker.store(true, Ordering::Release);
                    }
                }
                // sink 被 drop → 通道关闭 → w Drop 补头刷盘收尾。
            }));
            sinks.push((
                *source,
                Box::new(move |s: &[f32]| {
                    let _ = tx.send(s.to_vec());
                }) as Box<dyn FnMut(&[f32]) + Send>,
            ));
            activity.push((*source, wrote));
        }
        Wiring { sinks, joins, activity }
    }
}

/// 方案 B:在方案 A 之上多挂一条混音轨。两源 sink 各把样本**再复制一份**发给混音
/// 线程,TimelineMixer 按位置合成后写 `mixed.wav`。
pub struct MixedSink {
    inner: DualTrackSink,
    first_offsets: Vec<(Source, Arc<SourceHealth>)>,
}

impl MixedSink {
    pub fn with_first_offsets(
        inner: DualTrackSink,
        first_offsets: &[(Source, Arc<SourceHealth>)],
    ) -> Self {
        Self { inner, first_offsets: first_offsets.to_vec() }
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
        // 单源会话(比如系统声音源构建失败,只剩麦克风)无从混音:直接退化为方案 A,
        // 该笔记只有 A 可选。
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
        let mixed_path = note_dir.join(format!("{MIXED_TRACK}.wav"));
        let rollback = match std::fs::metadata(&mixed_path) {
            // 基线由 store::audio 算(那里握着 open() 的对齐公式与 offset_ms),本层不复刻:
            // 两处各算一遍正是「回滚恢复的是长度不是内容」那个 bug 的成因。
            Ok(meta) if meta.is_file() => {
                MixedRollback::Restore(pre_session_track_len(&note_dir, MIXED_TRACK, base_ms, meta.len()))
            }
            Ok(_) => MixedRollback::PreserveUnknown,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => MixedRollback::RemoveNew,
            Err(e) => {
                eprintln!(
                    "装配 mixed 时无法读取既有路径元数据,放弃时将保守保留({}): {e}",
                    mixed_path.display()
                );
                MixedRollback::PreserveUnknown
            }
        };

        // codex P1:装配即清旧完整性标记——writer 首次 append 就会 truncate/append
        // 文件,旧标记(描述装配前内容)一旦残留,本场异常中断后 mixed_untrusted 会
        // 拿旧读数为已被改动的文件背书。清不掉就整体退回方案 A:混音是纯增值旁路,
        // 宁可这场没有成品轨,不可留下"标记与内容脱钩"的可能。
        // 续录场次不清零观测(codex):mixed.wav 前缀是上一场的内容,其削波计数
        // 也是整文件读数的一部分。清标记前留存,定稿时与本场计数相加。
        // 前缀计数仅在旧标记 limit_metered 时可信;混入未测前缀时整体也只能算未测。
        let prior_mix = crate::store::audio::track_mix(&note_dir, MIXED_TRACK);
        if let Err(e) = crate::store::audio::clear_track_mix(&note_dir, MIXED_TRACK) {
            eprintln!("续录装配清旧 mixed 完整性标记失败,本场退回双轨方案: {e}");
            return w;
        }

        let (tx, rx) = crossbeam_channel::bounded::<MixedChunk>(MIXED_QUEUE_CAPACITY);
        let enqueue_abandoned = Arc::new(AtomicBool::new(false));
        let worker_abandoned = enqueue_abandoned.clone();
        // 定稿时写 MixInfo.seek_offset_ms 用:线程要在两源 sink 全部 drop 之后才收尾,
        // 彼时 first_frame_offset 已是终值(它只在首帧记录一次,之后只读)。
        let finalize_offsets = self.first_offsets.clone();
        // (prior_clipped, prior_limited, prior_metered):无旧标记 = 全新轨,视为已测零
        let (prior_clipped, prior_limited, prior_metered) = match &prior_mix {
            Some(m) => (m.clipped_samples, m.limited_samples, m.limit_metered),
            None => (0, 0, true),
        };
        w.joins.push(std::thread::spawn(move || {
            let mut mixer = TimelineMixer::new(DEFAULT_MARGIN_SAMPLES);
            // Option 包住:abandoned 分支需要在删除文件前先把 writer 显式 drop 掉,
            // 让它的 Drop::flush_header 跑完(否则文件可能还开着、尺寸头是旧的,
            // 删除时机早了在 Windows 等平台还可能因为文件被占用而失败)。
            let mut writer = Some(AudioTrackWriter::new(&note_dir, MIXED_TRACK, base_ms));
            // 旁路自杀开关:一旦累加窗超限就 break,不再等 rx 关闭。break 之后不调
            // finish()——已经放弃这条轨,把窗内剩余也吐出去只会让半成品更长,没有意义。
            let mut abandoned = false;
            let mut seen = [false; 2];
            // 本场是否真的往盘上追加过内容。为假即 AudioTrackWriter 全程停在 Pending、
            // open() 从未执行(它是惰性建档),文件仍是装配前的样子——此时回滚是纯破坏
            // 面:新轨路径上根本没有文件可删,续录轨则会被白截一刀、头被重写一遍(路径
            // 上若是个非 WAV 文件更会被直接写坏)。故放弃时以它为闸。
            let mut appended = false;
            for chunk in rx.iter() {
                if worker_abandoned.load(Ordering::Acquire) {
                    abandoned = true;
                    break;
                }
                seen[chunk.src] = true;
                let out = mixer.accept_at(chunk.src, chunk.start, &chunk.samples);
                if !out.is_empty() {
                    if let Some(w) = writer.as_mut() {
                        if w.append(&out) {
                            appended = true;
                        } else {
                            eprintln!("mixed 写盘失败,放弃该成品轨,源轨不受影响");
                            worker_abandoned.store(true, Ordering::Release);
                            abandoned = true;
                            break;
                        }
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
            if worker_abandoned.load(Ordering::Acquire) {
                abandoned = true;
            }
            if !seen.iter().all(|seen| *seen) {
                eprintln!("混音旁路收尾时有源从未产帧,放弃 mixed.wav,源轨不受影响");
                abandoned = true;
            }
            if !abandoned {
                // 两源 sink 都被 drop → 通道关闭 → 定稿窗内剩余,writer Drop 补头刷盘。
                let tail = mixer.finish();
                if !tail.is_empty() {
                    if let Some(w) = writer.as_mut() {
                        if w.append(&tail) {
                            appended = true;
                        } else {
                            eprintln!("mixed 收尾写盘失败,放弃该成品轨,源轨不受影响");
                            abandoned = true;
                        }
                    }
                }
            }
            if abandoned && appended {
                // 必须先关 writer 再回滚:Drop 会收尾头部并释放句柄;随后新轨删除,
                // 续录轨截回本场开始前的对齐基线,不会留下可被下游误认的截断成品。
                //
                // 已知残余(不修,如实记录):open() 成功、首次 append 却写盘失败时
                // appended 仍为 false,文件停在对齐后的长度上——它不含本场任何内容,
                // 但若对齐是零填充方向,那段零会留在盘上。丢的只是"顺手把零也清掉",
                // 而 open() 的截短本就不可逆,再回滚一次也换不回内容。
                drop(writer.take());
                rollback_mixed(&mixed_path, rollback);
            } else if !abandoned && appended {
                // 正常定稿的唯一出口:写完整性标记(MixInfo)。所有 abandon/rollback
                // 分支都进不到这里,「有 MixInfo ⇔ 内容完整」由控制流保证。先关 writer
                // 让 Drop 补头刷盘,session_track_ms 量到的才是终值。写失败只降级:
                // 轨本身是好的,消费方退回时长交叉核对(mixed_untrusted 的 duration 链)。
                drop(writer.take());
                // track_ms 必须是**整文件**时长(codex P1):消费端(mixed_untrusted)
                // 拿它与源轨全长终点比对;session_track_ms 的"本场净时长"口径在续录
                // 笔记上必然偏差。直接量 WAV 字节。
                match std::fs::metadata(&mixed_path) {
                    Ok(m) => {
                        let track_ms = crate::store::audio::bytes_to_ms(
                            m.len().saturating_sub(crate::store::audio::HEADER_LEN),
                        );
                        let seek_offset_ms = finalize_offsets
                            .iter()
                            .map(|(s, h)| (s.as_str().to_string(), h.first_frame_offset_16k() / 16))
                            .collect();
                        if let Err(e) = crate::store::audio::set_track_mix(
                            &note_dir,
                            MIXED_TRACK,
                            crate::store::audio::MixInfo {
                                origin: "live".into(),
                                seek_offset_ms,
                                track_ms,
                                // 削波观测(issue #124):没有它,削波量只能事后解码
                                // m4a 反推。整文件口径 = 上一场留存 + 本场。
                                clipped_samples: prior_clipped
                                    + mixer.limit_stats().clipped_samples,
                                limited_samples: prior_limited
                                    + mixer.limit_stats().limited_samples,
                                // 前缀未测(仪表化前录的)则整文件也只能标未测,
                                // 计数仅为下界
                                limit_metered: prior_metered,
                            },
                        ) {
                            eprintln!("[mix] 完整性标记写入失败(轨内容不受影响): {e}");
                        }
                    }
                    Err(e) => eprintln!("[mix] 定稿后量不到 mixed 轨长,跳过完整性标记: {e}"),
                }
            }
        }));

        for (source, sink) in w.sinks.iter_mut() {
            let idx = match source {
                Source::Mic => MIC,
                Source::System => SYSTEM,
            };
            let first_offset = self
                .first_offsets
                .iter()
                .find(|(candidate, _)| candidate == source)
                .map(|(_, health)| health.clone());
            let tx = tx.clone();
            let enqueue_abandoned = enqueue_abandoned.clone();
            let mut inner_sink = std::mem::replace(sink, Box::new(|_: &[f32]| {}));
            let mut next_pos = None;
            *sink = Box::new(move |s: &[f32]| {
                inner_sink(s);
                let start = next_pos.unwrap_or_else(|| {
                    first_offset
                        .as_ref()
                        .map(|health| health.first_frame_offset_16k())
                        .unwrap_or(0)
                });
                next_pos = Some(start.saturating_add(s.len() as u64));
                let _ = try_enqueue_mixed(&tx, &enqueue_abandoned, idx, start, s);
            });
        }
        drop(tx); // 原始 tx 必须丢弃,否则通道永不关闭、混音线程 join 永久阻塞
        w
    }
}

/// 按方案装配。mix=false 即退化为现状。
pub fn build_sinks(note_dir: &Path, base_ms: u64, sources: &[Source], mix: bool) -> Wiring {
    build_sinks_with_first_offsets(note_dir, base_ms, sources, &[], mix)
}

pub fn build_sinks_with_first_offsets(
    note_dir: &Path,
    base_ms: u64,
    sources: &[Source],
    first_offsets: &[(Source, Arc<SourceHealth>)],
    mix: bool,
) -> Wiring {
    let dual = DualTrackSink::new(note_dir, base_ms, sources);
    if mix {
        Box::new(MixedSink::with_first_offsets(dual, first_offsets)).into_wiring()
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

    #[test]
    fn source_activity_marks_only_after_successful_writer_append() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = build_sinks(dir.path(), 0, &[Source::Mic], false);
        let wrote = w.activity[0].1.clone();
        assert!(!wrote.load(Ordering::Acquire));
        sink_for(&mut w, Source::Mic)(&[0.2; 160]);
        drain(w);
        assert!(
            wrote.load(Ordering::Acquire),
            "writer 成功追加后应留下本场活动标记"
        );
    }

    #[test]
    fn source_activity_stays_false_when_writer_creation_fails() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("mic.wav")).unwrap();
        let mut w = build_sinks(dir.path(), 0, &[Source::Mic], false);
        let wrote = w.activity[0].1.clone();
        sink_for(&mut w, Source::Mic)(&[0.2; 160]);
        drain(w);
        assert!(
            !wrote.load(Ordering::Acquire),
            "建档失败不能被误记为本场已写音频"
        );
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
        // 正常定稿的盘上证据:MixInfo 必须在(且只在)这条路径写出。
        let meta = crate::store::audio::load_audio_meta(dir.path());
        let mix = meta
            .tracks
            .get(MIXED_TRACK)
            .and_then(|t| t.mix.as_ref())
            .expect("正常定稿必须写 MixInfo");
        assert_eq!(mix.origin, "live");
        assert_eq!(mix.track_ms, 1000, "16000 样本 @16k = 1000ms");
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

    /// 生产接线回归:FrameTap 记录的首帧墙钟偏移必须真正传进 mixer,而不是只存在
    /// health 里。system 晚 160 样本开始,前 160 个 mixed 样本只能有 mic。
    #[test]
    fn first_frame_health_offsets_are_applied_to_mixed_timeline() {
        let dir = tempfile::tempdir().unwrap();
        let mic_health = Arc::new(SourceHealth::default());
        let system_health = Arc::new(SourceHealth::default());
        mic_health.set_first_frame_offset_16k_for_test(0);
        system_health.set_first_frame_offset_16k_for_test(160);
        let health = [
            (Source::Mic, mic_health),
            (Source::System, system_health),
        ];
        let mut w = build_sinks_with_first_offsets(
            dir.path(),
            0,
            &[Source::Mic, Source::System],
            &health,
            true,
        );
        sink_for(&mut w, Source::Mic)(&[0.1; 320]);
        sink_for(&mut w, Source::System)(&[0.2; 320]);
        drain(w);

        let mixed = read_pcm_i16(&dir.path().join("mixed.wav"));
        assert_eq!(mixed.len(), 480, "晚到源的偏移应扩展共同时间轴");
        let mic_only = f32_to_s16(0.1);
        let both = f32_to_s16(0.3);
        let system_only = f32_to_s16(0.2);
        assert!(
            mixed[..160]
                .iter()
                .all(|&v| (v as i32 - mic_only as i32).abs() <= 2)
        );
        assert!(
            mixed[160..320]
                .iter()
                .all(|&v| (v as i32 - both as i32).abs() <= 2)
        );
        assert!(
            mixed[320..]
                .iter()
                .all(|&v| (v as i32 - system_only as i32).abs() <= 2)
        );
        // 首帧偏移要随定稿进 MixInfo.seek_offset_ms:mixed 消费方(段落 seek)的
        // 修正量来源。160 样本 @16k = 10ms。
        let meta = crate::store::audio::load_audio_meta(dir.path());
        let mix = meta
            .tracks
            .get(MIXED_TRACK)
            .and_then(|t| t.mix.as_ref())
            .expect("正常定稿必须写 MixInfo");
        assert_eq!(mix.seek_offset_ms.get("mic"), Some(&0));
        assert_eq!(mix.seek_offset_ms.get("system"), Some(&10));
    }

    /// mixed 队列必须有界且生产者永不阻塞。满队列时立即翻转 abandon 标记,后续块
    /// 不再复制到旁路;源轨自己的发送路径与该标记无关。
    #[test]
    fn full_mixed_queue_marks_sidecar_abandoned() {
        let (tx, _rx) = crossbeam_channel::bounded(1);
        let abandoned = Arc::new(std::sync::atomic::AtomicBool::new(false));
        assert!(try_enqueue_mixed(
            &tx,
            &abandoned,
            MIC,
            0,
            &[0.1; 16],
        ));
        assert!(!try_enqueue_mixed(
            &tx,
            &abandoned,
            MIC,
            16,
            &[0.1; 16],
        ));
        assert!(abandoned.load(std::sync::atomic::Ordering::Acquire));
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

    /// 回归:一源从未产帧且会话在 30 秒饥饿守卫触发前结束时,收尾不能把唯一来源
    /// `finish()` 成一条看似完整的 mixed.wav。源轨仍须正常保留。
    #[test]
    fn short_one_sided_session_does_not_finalize_mixed_track() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = build_sinks(dir.path(), 0, &[Source::Mic, Source::System], true);
        sink_for(&mut w, Source::Mic)(&[0.3; 1600]);
        drain(w);

        assert!(dir.path().join("mic.wav").exists(), "真实来源的源轨必须保留");
        assert!(
            !dir.path().join("mixed.wav").exists(),
            "两源未都出现时不得把单边缓冲定稿成 mixed.wav"
        );
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
        let meta = crate::store::audio::load_audio_meta(dir.path());
        assert!(
            meta.tracks.get(MIXED_TRACK).and_then(|t| t.mix.as_ref()).is_none(),
            "放弃/回滚路径不得留下完整性标记"
        );
    }

    /// Bug 修复回归,与上面 starvation_after_partial_success 相反方向:同样是"守卫
    /// 触发时 mixed.wav 已经装着写出的内容",但这次内容不是本场自己混出来的,而是
    /// **上一场续录前就已经完好落盘**的。remove_file 不加区分地删,会把续录场景下
    /// 上一场那部分完全正常的内容一并冲掉——数据丢失面比"留一条尾部截断的合法轨"
    /// 大得多。守卫必须只删"本场从零建的轨"(可离线用源轨重算,零损失),续录追加
    /// 的轨必须回滚到装配前长度,既保留上一场,也不能留下本场截断尾巴。
    ///
    /// 搭建:先跑一场完整会话(base_ms=0)产出一条 1 秒、内容正确的 mixed.wav 并
    /// 正常 finish,模拟"上一场"。base_ms=1000 严丝合缝对应它的时长(offset_ms=0,
    /// 1 秒=1000ms),这样第二场 AudioTrackWriter::open() 的续录对齐分支
    /// (`set_len` 到 base_ms 对应字节数)刚好不截断也不补零上一场的内容——如果这里
    /// 算错 base_ms,对齐会把断言意图冲掉,所以特意选一个和预存时长严丝合缝的值。
    /// 第二场先正常混一小段(确认 open() 真的走过续录分支、新内容接到旧内容后面,
    /// 而不是像 one_sided_starvation 用例那样全程停在 Pending 从未开过文件),再让
    /// system 彻底停摆触发守卫。
    ///
    /// 注意这个"严丝合缝"的参数**不是常态**,它只测"不删续录轨"这条性质。真实录制里
    /// base_ms 恒小于上一场轨时长、对齐恒截短——那条路径由
    /// `starvation_in_resumed_session_rolls_back_to_alignment_baseline` 覆盖,别把两条
    /// 当成重复用例删掉其一。
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

        // 核心断言:preexisting 场景下守卫不能删文件,也不能保留本场截断尾巴;
        // 应精确回滚到装配前的完整轨。
        assert!(
            mixed_path.exists(),
            "续录场景守卫误删了 mixed.wav,上一场已完好落盘的内容随之丢失\
             (本次修复要防的正是这个)"
        );
        let mixed = read_pcm_i16(&mixed_path);
        assert_eq!(
            mixed.len(),
            old_mixed.len(),
            "放弃续录 mixed 后应精确回滚到上一场长度"
        );
        // 上一场内容必须原样保留在文件开头(±2 LSB 容差同其它用例)。
        for (i, (&got, &want)) in mixed[..old_mixed.len()].iter().zip(old_mixed.iter()).enumerate() {
            assert!(
                (got as i32 - want as i32).abs() <= 2,
                "位置 {i}: 上一场内容被改动,got {got} want {want}"
            );
        }
        // codex P1 回归:上一场定稿写下的 MixInfo 在本场装配时必须已清掉,放弃路径
        // 也不得把它留下——旧标记描述的是装配前的旧内容,本场 writer 已 truncate/
        // append 过文件,异常后旧标记会为被改动过的文件背书。
        let meta = crate::store::audio::load_audio_meta(dir.path());
        assert!(
            meta.tracks.get(MIXED_TRACK).and_then(|t| t.mix.as_ref()).is_none(),
            "续录放弃后不得残留上一场的完整性标记"
        );
    }

    /// codex P1 回归:续录正常定稿时,MixInfo.track_ms 必须是**整文件**时长——
    /// 消费端(mixed_untrusted)拿它与源轨全长终点比对;若存本场净时长
    /// (session_track_ms 口径,减 base_ms 前缀),续录笔记的未转码校验必然偏差。
    #[test]
    fn continuation_finalize_records_full_file_duration() {
        let dir = tempfile::tempdir().unwrap();
        // 第一场:1 秒,正常定稿。
        {
            let mut w = build_sinks(dir.path(), 0, &[Source::Mic, Source::System], true);
            for _ in 0..100 {
                for (_, s) in w.sinks.iter_mut() {
                    s(&[0.25; 160]);
                }
            }
            drain(w);
        }
        // 第二场:base_ms=1000 严丝合缝续录,再混 1 秒,正常定稿。
        {
            let mut w = build_sinks(dir.path(), 1000, &[Source::Mic, Source::System], true);
            for _ in 0..100 {
                for (_, s) in w.sinks.iter_mut() {
                    s(&[0.25; 160]);
                }
            }
            drain(w);
        }
        let meta = crate::store::audio::load_audio_meta(dir.path());
        let mix = meta
            .tracks
            .get(MIXED_TRACK)
            .and_then(|t| t.mix.as_ref())
            .expect("正常定稿必须写 MixInfo");
        assert_eq!(mix.track_ms, 2000, "track_ms 必须是整文件时长(两场共 2 秒),不是本场净时长");
    }

    /// Bug 修复回归,与上一条同场景但**参数落在常态区间**:上一条特意选了 base_ms 与
    /// 预存轨时长严丝合缝的点(对齐既不截断也不补零),那是唯一让"回滚到装配时文件
    /// 长度"也能歪打正着的取值;真实录制里 `base_ms` 来自 `StoreWriter::base_ms()`,
    /// 是续录前最大 `end_ms`(最后一句话结束的位置),而文件尾还压着用户按停止键前那段
    /// 没进任何 segment 的静音,所以 `base_ms < 上一场轨时长` 才是**常态**,
    /// `AudioTrackWriter::open()` 恒走截短分支。
    ///
    /// 回滚基线若取装配时的文件长度,`set_len` 会把文件拉回**比对齐后更长**,空出来的
    /// 那截正好装着本场刚混出来的内容,拼成一条"上一场前段 + 本场开头"的混合体;更坏的是
    /// 它的 `duration_ms` 与放弃前一模一样,下游任何交叉核对都发现不了。
    ///
    /// 构造:预存轨 1000ms,本场 base_ms=600(模拟 400ms 尾部静音没进 segment),
    /// 于是对齐基线 = 44 + 600ms 对应的 19200 字节。本场混音内容取 0.9+0.9 → 饱和到
    /// 32767,与预存内容(0.25+0.25 → 约 16383)截然不同,越界一个样本即可检出。
    #[test]
    fn starvation_in_resumed_session_rolls_back_to_alignment_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let mixed_path = dir.path().join("mixed.wav");

        // 上一场:正常混 1 秒(16000 样本)并 finish。
        {
            let mut w = build_sinks(dir.path(), 0, &[Source::Mic, Source::System], true);
            for _ in 0..100 {
                for (_, s) in w.sinks.iter_mut() {
                    s(&[0.25; 160]);
                }
            }
            drain(w);
        }
        let old_mixed = read_pcm_i16(&mixed_path);
        assert_eq!(old_mixed.len(), 16000, "前置条件:上一场应是 1 秒");
        let old_len_bytes = std::fs::metadata(&mixed_path).unwrap().len();

        // 本场续录:base_ms=600 < 上一场的 1000ms,open() 会把文件截到 600ms。
        const BASE_MS: u64 = 600;
        let baseline_bytes = 44 + BASE_MS * 16 * 2; // 44 + 19200
        let mut w = build_sinks(dir.path(), BASE_MS, &[Source::Mic, Source::System], true);
        for _ in 0..100 {
            for (_, s) in w.sinks.iter_mut() {
                s(&[0.9; 160]); // 与上一场取值截然不同:混出来饱和到 32767
            }
        }
        // 门限取**上一场的文件长度**而非对齐基线:对齐已把文件截到 44+19200,只有本场
        // 真的追加了内容才可能重新超过 44+32000。用基线当门限则一开始就成立,证明不了
        // 追加发生过。
        assert!(
            wait_until_size_at_least(&mixed_path, old_len_bytes, std::time::Duration::from_secs(5)),
            "前置条件:本场应已把新内容追加到对齐点之后(否则测不到回滚)"
        );
        // system 彻底停摆,mic 继续推进直到累加窗超限触发放弃。
        {
            let mic = sink_for(&mut w, Source::Mic);
            for _ in 0..(MAX_MIXER_WINDOW_SAMPLES / 8000 + 5) {
                mic(&vec![0.3f32; 8000]);
            }
        }
        drain(w);

        assert_eq!(
            std::fs::metadata(&mixed_path).unwrap().len(),
            baseline_bytes,
            "回滚基线必须是对齐后的长度(44+19200),不是装配时的文件长度(44+32000)"
        );
        let rolled = read_pcm_i16(&mixed_path);
        assert_eq!(rolled.len(), 9600, "600ms @16k = 9600 样本");
        // 内容必须是上一场的**真前缀**:逐样本与预存内容一致,且不含本场任何样本。
        let this_session = f32_to_s16(1.0); // 0.9+0.9 饱和后的取值
        for (i, (&got, &want)) in rolled.iter().zip(old_mixed.iter()).enumerate() {
            assert!(
                (got as i32 - want as i32).abs() <= 2,
                "位置 {i}: 回滚后不是上一场内容的真前缀,got {got} want {want}"
            );
            assert_ne!(got, this_session, "位置 {i}: 本场混音内容被回滚保留了下来");
        }
    }

    /// M2 回归:本场一个样本都没写出去时,放弃路径必须**完全不碰**盘上的文件。
    ///
    /// AudioTrackWriter 是惰性建档,没 append 过就没 open() 过,文件仍是装配前的样子;
    /// 此时照样跑 set_len + repair_wav_header 是纯破坏面——`PreserveUnknown` 只挡了
    /// 非普通文件,挡不住"普通文件但根本不是 WAV"(用户误放、别的工具留下的同名文件),
    /// 那前 44 字节会被直接写成 WAV 头。
    #[test]
    fn abandoning_without_any_append_leaves_the_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let mixed_path = dir.path().join("mixed.wav");
        let intact = b"this is not a wav file at all".to_vec();
        std::fs::write(&mixed_path, &intact).unwrap();

        // 只有 mic 产帧:水位线恒为 0,writer 全程停在 Pending,收尾时因"有源从未产帧"
        // 判定放弃。
        let mut w = build_sinks(dir.path(), 0, &[Source::Mic, Source::System], true);
        sink_for(&mut w, Source::Mic)(&[0.3; 1600]);
        drain(w);

        assert_eq!(
            std::fs::read(&mixed_path).unwrap(),
            intact,
            "本场没写过一个字节,放弃时不该 set_len,更不该重写它的头"
        );
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
