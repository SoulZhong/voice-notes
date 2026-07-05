use crate::asr::Recognizer;
use crate::audio::{AudioCapture, AudioFrame, Source};
use crate::diar::registry::SpeakerRegistry;
use crate::diar::SpeakerEmbedder;
use crate::pipeline::segment_worker::run_segment_worker;
use crate::pipeline::segmenter::Segmenter;
use crossbeam_channel::Receiver;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// 跨路回声去重(P4.5 Task 4)：同一人声经「他人电脑外放→房间→本机 mic」形成第二路,
// 声道染色使声纹分裂、转写重复。策略：mic 段识别后先 hold(不落盘/不嵌入),期间
// 若有时间邻近且文本高相似的 system 段出现则丢弃 mic 段；到期无匹配则正常处理。
// 下列为首轮取值(未经真实会议数据校准),P4.5 二轮联调时应根据误伤/漏抓率回调。
/// mic 段最长 hold 时长(ms)，超时未匹配到回声即释放正常处理。
///
/// 注：被 hold 的 mic 段落盘顺序晚于时间上更晚的 system 段（最多晚 echo_hold），
/// 详情页按文件序（seq）渲染时，可能出现可接受的小幅时间交错（≤ echo_hold）。
pub(crate) const ECHO_HOLD_MS: u64 = 2500;
/// 判定「时间邻近」的窗口(ms)：两段时间区间交叠，或起点差小于此值。
const ECHO_WINDOW_MS: u64 = 2500;
/// 判定「文本高相似」的阈值(0~1，见 text_similarity)。
const ECHO_SIM_THRESHOLD: f32 = 0.6;
/// recent_system 缓冲的裁剪窗口(ms)：仅保留最近 10s 内的 system 段供 mic 端比对。
const RECENT_SYSTEM_WINDOW_MS: u64 = 10_000;

/// 归一化：去除空白与常见中英标点、ASCII 转小写，供回声去重的文本比对使用。
fn normalize_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_whitespace() {
            continue;
        }
        if matches!(
            c,
            ',' | '.' | '?' | '!' | ';' | ':' | '，' | '。' | '？' | '！' | '、' | '；' | '：'
        ) {
            continue;
        }
        for lc in c.to_lowercase() {
            out.push(lc);
        }
    }
    out
}

/// 按字符计的 Levenshtein 编辑距离，O(nm)，用于短段文本比对。
fn levenshtein(a: &[char], b: &[char]) -> usize {
    let (n, m) = (a.len(), b.len());
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur = vec![0usize; m + 1];
    for i in 1..=n {
        cur[0] = i;
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[m]
}

/// 文本相似度 = max(1 − 编辑距离/较长串字符数, 归一化后短串被长串完全包含 ? 1.0 : 0.0)。
/// 任一侧归一化后为空串 → 0（避免空文本互相「完全包含」误判）。
fn text_similarity(a: &str, b: &str) -> f32 {
    let na = normalize_text(a);
    let nb = normalize_text(b);
    if na.is_empty() || nb.is_empty() {
        return 0.0;
    }
    let ca: Vec<char> = na.chars().collect();
    let cb: Vec<char> = nb.chars().collect();
    let contains_score = if ca.len() <= cb.len() {
        if nb.contains(&na) { 1.0 } else { 0.0 }
    } else if na.contains(&nb) {
        1.0
    } else {
        0.0
    };
    let max_len = ca.len().max(cb.len()) as f32;
    let dist_score = if max_len == 0.0 {
        0.0
    } else {
        1.0 - (levenshtein(&ca, &cb) as f32 / max_len)
    };
    dist_score.max(contains_score)
}

/// 两段 `[start,end]` 是否「时间邻近」：区间交叠，或起点差 < ECHO_WINDOW_MS。
fn time_near(a_start: u64, a_end: u64, b_start: u64, b_end: u64) -> bool {
    let overlap = a_start <= b_end && b_start <= a_end;
    let start_close = (a_start as i64 - b_start as i64).abs() < ECHO_WINDOW_MS as i64;
    overlap || start_close
}

/// 前 20 字符前缀，供丢弃日志裁剪展示（按 char 计，避免截断多字节字符）。
fn text_prefix20(s: &str) -> String {
    s.chars().take(20).collect()
}

/// 段音频均方根。空段为 0(理论不出现,防御)。
fn rms_of(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|x| x * x).sum::<f32>() / samples.len() as f32).sqrt()
}

/// 字符占比兜底的阈值:字母类字符中假名/谚文超三成即视为外语幻觉。
const FOREIGN_RATIO_THRESHOLD: f32 = 0.3;

/// 语言白名单过滤(会议场景仅中英):模型标签为日/韩,或文本假名/谚文占比过阈 → 外语
/// 幻觉段。SenseVoice 短段常把 AEC 残渣误判成日语;此类段漏过文本回声去重(残渣文
/// 本与 system 段不相似)且会开出垃圾说话人,须在处理链之前整段丢弃。
/// 纯汉字的日语幻觉读作中文,不拦(无损);占位段/空串占比为 0,天然放行。
/// 占比兜底对模型标为 zh 的段同样生效(未提前用标签放行),系有意为之:混杂幻觉
/// (如假名混中文)模型常仍标 zh,标签本身不可靠;误杀面(中文夹整句日语引用)
/// 待 rms/误杀数据复盘时与阈值一并校准。
fn is_foreign_final(lang: &str, text: &str) -> bool {
    let tag: String = lang
        .trim_matches(|c: char| c == '<' || c == '|' || c == '>')
        .to_ascii_lowercase();
    if tag == "ja" || tag == "ko" {
        return true;
    }
    let (mut letters, mut foreign) = (0u32, 0u32);
    for c in text.chars() {
        if !c.is_alphabetic() {
            continue;
        }
        letters += 1;
        let u = c as u32;
        let is_kana = (0x3040..=0x30FF).contains(&u) || (0x31F0..=0x31FF).contains(&u);
        let is_hangul = (0xAC00..=0xD7AF).contains(&u)
            || (0x1100..=0x11FF).contains(&u)
            || (0x3130..=0x318F).contains(&u);
        if is_kana || is_hangul {
            foreign += 1;
        }
    }
    letters > 0 && foreign as f32 / letters as f32 > FOREIGN_RATIO_THRESHOLD
}

/// hold 中的 mic 段：已识别文本，等待与 system 段比对；到期(echo_hold)无匹配则
/// 走完整处理链(embed/assign/on_final)。`embedding_input` 为原始样本，供 release
/// 时才做声纹嵌入（避免被丢弃的段产生任何嵌入副作用）。
struct PendingMic {
    text: String,
    norm: String,
    start_ms: u64,
    end_ms: u64,
    samples_len: usize,
    embedding_input: Vec<f32>,
    held_at: Instant,
    /// hold 前已算好的段级 rms，release 时随 on_final 透传给落盘层。
    rms: f32,
}

/// 已处理的 system 段的轻量记录，供后续到达的 mic 段比对（回声去重）。
struct RecentSystem {
    text: String,
    norm: String,
    start_ms: u64,
    end_ms: u64,
}

/// 完整处理链：embed → assign → take_merges/SpeakersChanged → on_final。
/// 即时路径（system 段、无匹配的 mic 段）与 release 路径（hold 到期/排干的 mic 段）共用，
/// 保证「被丢弃段零副作用、被处理段处理逻辑同源」。
#[allow(clippy::too_many_arguments)]
fn process_final<F1, F2>(
    source: Source,
    text: String,
    start_ms: u64,
    end_ms: u64,
    samples_len: usize,
    embedding_input: &[f32],
    rms: f32,
    embedder: &mut Option<Box<dyn SpeakerEmbedder>>,
    registry: &mut SpeakerRegistry,
    last_sent: &mut Vec<crate::diar::registry::SpeakerInfo>,
    on_final: &mut F1,
    on_diar: &mut F2,
) where
    F1: FnMut(Source, String, u64, u64, Option<String>, Option<f32>),
    F2: FnMut(DiarEvent),
{
    // 声纹:嵌入失败/无 embedder → None,绝不影响文本
    let speaker = embedder.as_mut().and_then(|e| {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| e.embed(embedding_input))) {
            Ok(Ok(v)) => registry.assign(&v, source.as_str(), samples_len),
            Ok(Err(err)) => {
                eprintln!("声纹提取失败({:?} 段): {err}", source);
                None
            }
            Err(_) => {
                eprintln!("声纹提取 panic({:?} 段),该段无标签", source);
                None
            }
        }
    });
    for (loser, winner) in registry.take_merges() {
        on_diar(DiarEvent::Merged { loser, winner });
    }
    let speakers = registry.speakers();
    if speakers != *last_sent {
        *last_sent = speakers.clone();
        on_diar(DiarEvent::SpeakersChanged(speakers));
    }
    on_final(source, text, start_ms, end_ms, speaker, Some(rms));
}

/// 完成句识别任务：进 finals 队列，永不丢弃（保证不丢内容）。
#[derive(Debug, Clone)]
pub struct FinalJob {
    pub source: Source,
    pub samples: Vec<f32>,
    /// 相对该源流开始的毫秒（16kHz 样本钟换算）。
    pub start_ms: u64,
    pub end_ms: u64,
}

/// 当前句预览任务：写入每源覆盖式槽，忙时被更新版本覆盖（best-effort）。
#[derive(Debug, Clone)]
pub struct PartialJob {
    pub source: Source,
    pub samples: Vec<f32>,
}

/// diarization 侧事件:说话人表变化 / 簇合并(需回写落盘与 UI)/ worker 结束时的质心快照
/// (仅存入 writer 内存表,不落盘、不 emit,由既有 finalize→persist_speakers 落盘,P4.5 续录铺底)。
#[derive(Debug, Clone)]
pub enum DiarEvent {
    SpeakersChanged(Vec<crate::diar::registry::SpeakerInfo>),
    Merged { loser: String, winner: String },
    Snapshot(Vec<crate::diar::registry::ClusterSnapshot>),
}

/// 单识别 worker：串行消费 finals（不丢、优先），空闲时取每源最新 partial（best-effort）。
/// finals_rx 关闭且排干后返回。识别失败的完成句 emit "[识别失败]" 占位，worker 不退出。
/// 每条 final 定稿时额外提声纹嵌入并归簇（嵌入失败/无 embedder/panic 均降级为 None，绝不影响文本）；
/// 归簇产生的簇合并 / 说话人表变化通过 on_diar 通知（顺序：先 Merged 后 SpeakersChanged）。
/// 识别得到的语言标签命中外语白名单过滤（`is_foreign_final`）的整段直接丢弃，
/// 与 ECHO 命中同待遇；未被丢弃的段额外算出段级 rms，随 `on_final` 尾参
/// `Option<f32>` 透传给调用方落盘（partial 路径不参与语言过滤，也不算 rms）。
#[allow(clippy::too_many_arguments)]
pub fn run_asr_worker(
    mut recognizer: Box<dyn Recognizer>,
    mut embedder: Option<Box<dyn SpeakerEmbedder>>,
    mut registry: SpeakerRegistry,
    finals_rx: Receiver<FinalJob>,
    echo_hold: Duration,
    partial_slots: Vec<(Source, Arc<Mutex<Option<PartialJob>>>)>,
    mut on_final: impl FnMut(Source, String, u64, u64, Option<String>, Option<f32>),
    mut on_partial: impl FnMut(Source, String),
    mut on_diar: impl FnMut(DiarEvent),
) -> (Box<dyn Recognizer>, Option<Box<dyn SpeakerEmbedder>>) {
    // 与上次发送的完整说话人表比较（非仅 len）：同段内「合并-1+新建+1」净零、
    // 已有簇 sources 增长等变化都能被捕获并同步。
    let mut last_sent: Vec<crate::diar::registry::SpeakerInfo> = Vec::new();
    // 回声去重状态：hold 中的 mic 段（入队序）+ 最近处理过的 system 段（供 mic 端比对）。
    let mut pending_mic: VecDeque<PendingMic> = VecDeque::new();
    let mut recent_system: VecDeque<RecentSystem> = VecDeque::new();

    // release 一个到期/排干的 pending mic 段：走完整处理链，与即时路径同源。
    macro_rules! release_pending {
        ($p:expr) => {{
            let p: PendingMic = $p;
            process_final(
                Source::Mic,
                p.text,
                p.start_ms,
                p.end_ms,
                p.samples_len,
                &p.embedding_input,
                p.rms,
                &mut embedder,
                &mut registry,
                &mut last_sent,
                &mut on_final,
                &mut on_diar,
            );
        }};
    }

    loop {
        match finals_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(job) => {
                // 到期检查(先于本条 final 的处理)：让长时间空转但持续来 final 的场景
                // 也能及时 release，不必等到 timeout tick。
                while pending_mic
                    .front()
                    .is_some_and(|p| p.held_at.elapsed() >= echo_hold)
                {
                    let p = pending_mic.pop_front().unwrap();
                    release_pending!(p);
                }

                let (text, lang) = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    recognizer.recognize(&job.samples)
                })) {
                    Ok(Ok(t)) => (t.text, t.lang),
                    Ok(Err(_)) => ("[识别失败]".to_string(), String::new()),
                    Err(_) => {
                        eprintln!(
                            "run_asr_worker: recognize panicked on a {:?} final; 以占位继续",
                            job.source
                        );
                        ("[识别失败]".to_string(), String::new())
                    }
                };
                // 语言白名单:外语幻觉段与 ECHO 命中同待遇——不 embed/不 assign/
                // 不 emit/不落盘,从源头杜绝垃圾段污染说话人表。占位段占比 0 天然放行。
                if is_foreign_final(&lang, &text) {
                    eprintln!(
                        "语言过滤: 丢弃 {:?} 段 lang=\"{lang}\" text=\"{}\"",
                        job.source,
                        text_prefix20(&text)
                    );
                    // 被丢段无 final 接替，前端只在收到 final 时清 partial 预览，
                    // 幻觉文本会残留成 UI 残影；主动推空 partial 顶掉它。
                    on_partial(job.source, String::new());
                    continue;
                }
                let seg_rms = rms_of(&job.samples);

                match job.source {
                    Source::System => {
                        let sys_norm = normalize_text(&text);
                        // 先对照 pending_mic：命中即丢弃（零副作用），不进入处理链。
                        // 占位文本("[识别失败]"，未归一比较)是"确有发声但识别失败"的
                        // 痕迹，不参与回声比对：双路同时识别失败时文本雷同（都是占位串）
                        // 又时间邻近，若照常比对会把 mic 占位段误判为回声丢弃，静默吞掉
                        // 一段真实发声。故遇到占位段的 pending 直接跳过匹配，原样保留。
                        // retain 闭包内不能直接调用 on_partial（借用冲突：on_partial 是
                        // 外层 FnMut，闭包已捕获 job/sys_norm）；改用局部 flag，retain
                        // 结束后统一补一次空 partial，清掉被丢 mic 段的 UI 残影。
                        let mut dropped_mic = false;
                        pending_mic.retain(|p| {
                            if p.text == "[识别失败]" {
                                return true;
                            }
                            let echoed = time_near(p.start_ms, p.end_ms, job.start_ms, job.end_ms)
                                && text_similarity(&p.norm, &sys_norm) >= ECHO_SIM_THRESHOLD;
                            if echoed {
                                eprintln!(
                                    "回声去重: 丢弃 mic 段(与 system 段匹配) mic=\"{}\" system=\"{}\"",
                                    text_prefix20(&p.text),
                                    text_prefix20(&text)
                                );
                                dropped_mic = true;
                            }
                            !echoed
                        });
                        if dropped_mic {
                            on_partial(Source::Mic, String::new());
                        }
                        // system 段零延迟处理。
                        process_final(
                            job.source,
                            text.clone(),
                            job.start_ms,
                            job.end_ms,
                            job.samples.len(),
                            &job.samples,
                            seg_rms,
                            &mut embedder,
                            &mut registry,
                            &mut last_sent,
                            &mut on_final,
                            &mut on_diar,
                        );
                        recent_system.push_back(RecentSystem {
                            text,
                            norm: sys_norm,
                            start_ms: job.start_ms,
                            end_ms: job.end_ms,
                        });
                        let newest_end = job.end_ms;
                        recent_system
                            .retain(|r| newest_end.saturating_sub(r.end_ms) <= RECENT_SYSTEM_WINDOW_MS);
                    }
                    Source::Mic => {
                        // 占位文本("[识别失败]"，未归一比较)是"确有发声但识别失败"的痕迹，
                        // 不参与回声去重：双路同时识别失败时文本雷同（都是占位串）又时间
                        // 邻近，会被误判为回声互相丢弃，静默吞掉一段真实发声。跳过匹配与
                        // hold，直接走完整处理链即时处理。
                        if text == "[识别失败]" {
                            process_final(
                                job.source,
                                text,
                                job.start_ms,
                                job.end_ms,
                                job.samples.len(),
                                &job.samples,
                                seg_rms,
                                &mut embedder,
                                &mut registry,
                                &mut last_sent,
                                &mut on_final,
                                &mut on_diar,
                            );
                        } else {
                            let mic_norm = normalize_text(&text);
                            let echo = recent_system.iter().find(|r| {
                                time_near(job.start_ms, job.end_ms, r.start_ms, r.end_ms)
                                    && text_similarity(&mic_norm, &r.norm) >= ECHO_SIM_THRESHOLD
                            });
                            match echo {
                                Some(r) => {
                                    eprintln!(
                                        "回声去重: 丢弃 mic 段(与 system 段匹配) mic=\"{}\" system=\"{}\"",
                                        text_prefix20(&text),
                                        text_prefix20(&r.text)
                                    );
                                    // 命中：不 embed/不 assign/不 emit/不落盘，直接丢弃。
                                    // 同语言过滤路径：无 final 接替，主动清空该源 partial 残影。
                                    on_partial(job.source, String::new());
                                }
                                None => {
                                    pending_mic.push_back(PendingMic {
                                        text,
                                        norm: mic_norm,
                                        start_ms: job.start_ms,
                                        end_ms: job.end_ms,
                                        samples_len: job.samples.len(),
                                        embedding_input: job.samples,
                                        held_at: Instant::now(),
                                        rms: seg_rms,
                                    });
                                }
                            }
                        }
                    }
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                // 到期检查：无 final 到来时靠这个 100ms tick 兜底 release。
                while pending_mic
                    .front()
                    .is_some_and(|p| p.held_at.elapsed() >= echo_hold)
                {
                    let p = pending_mic.pop_front().unwrap();
                    release_pending!(p);
                }
                // 空闲：服务每源最新 partial（取出即清空，只识别最新一版）。
                for (src, slot) in &partial_slots {
                    let job = slot.lock().unwrap().take();
                    if let Some(job) = job {
                        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            recognizer.recognize(&job.samples)
                        })) {
                            Ok(Ok(t)) => on_partial(*src, t.text),
                            Ok(Err(_)) => {}
                            Err(_) => {
                                eprintln!(
                                    "run_asr_worker: recognize panicked on a {:?} partial; 跳过",
                                    src
                                );
                            }
                        }
                    }
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                // 排干全部 pending（无论是否到期），保持入队序，再发 Snapshot。
                while let Some(p) = pending_mic.pop_front() {
                    release_pending!(p);
                }
                on_diar(DiarEvent::Snapshot(registry.snapshot()));
                break;
            }
        }
    }
    (recognizer, embedder)
}

/// 一次录制会话的句柄：持两路 capture + 各 worker 的 join 句柄。
pub struct RecordingHandle {
    captures: Vec<Box<dyn AudioCapture>>,
    workers: Vec<std::thread::JoinHandle<()>>,
    asr: Option<std::thread::JoinHandle<(Box<dyn Recognizer>, Option<Box<dyn SpeakerEmbedder>>)>>,
    /// 各 segment_worker 共享的暂停闸（true = 丢帧，时间轴冻结）。
    paused: Arc<std::sync::atomic::AtomicBool>,
}

impl RecordingHandle {
    /// 置暂停闸。跳变瞬间的在途语句 flush 由 worker 侧完成（见 run_segment_worker）。
    pub fn set_paused(&self, v: bool) {
        self.paused.store(v, std::sync::atomic::Ordering::Relaxed);
    }

    /// 优雅停止：停各 capture（关帧通道）→ 分段 worker flush 尾段后退出并 join
    /// →（其 finals 发送端随之 drop）ASR worker 排干剩余 finals 后退出并 join，
    /// 返还 recognizer / embedder 供复用（asr 线程 panic 时均返 None，调用方现场重载兜底）。
    pub fn stop(mut self) -> (Option<Box<dyn Recognizer>>, Option<Box<dyn SpeakerEmbedder>>) {
        for c in self.captures.iter_mut() {
            c.stop();
        }
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
        match self.asr.take() {
            Some(a) => match a.join() {
                Ok((r, e)) => (Some(r), e),
                Err(_) => {
                    eprintln!("RecordingHandle::stop: asr 线程异常退出（panic），模型不回收");
                    (None, None)
                }
            },
            None => (None, None),
        }
    }
}

/// start_session 的结果：句柄 + 成功启动的源 + 失败的源（含错误串，供降级归类）。
pub struct SessionStart {
    pub handle: RecordingHandle,
    pub active: Vec<Source>,
    pub failed: Vec<(Source, String)>,
}

/// start_session 失败时携带 recognizer / embedder 返还，避免常驻模型在错误路径丢失。
pub struct StartError {
    pub error: anyhow::Error,
    pub recognizer: Box<dyn Recognizer>,
    pub embedder: Option<Box<dyn SpeakerEmbedder>>,
}

impl std::fmt::Debug for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StartError({})", self.error)
    }
}

/// 起会话：每源一条分段 worker + 单 ASR worker，接好 finals 通道与每源 partial 槽。
/// 某源 capture 启动失败 → 跳过该源并记入 failed（用于降级）；无任何源启动 → Err。
#[allow(clippy::too_many_arguments)]
pub fn start_session(
    sources: Vec<(Source, Box<dyn AudioCapture>, Box<dyn Segmenter>)>,
    recognizer: Box<dyn Recognizer>,
    embedder: Option<Box<dyn SpeakerEmbedder>>,
    registry: SpeakerRegistry,
    echo_hold: Duration,
    target_rate: u32,
    partial_interval_samples: usize,
    on_final: impl FnMut(Source, String, u64, u64, Option<String>, Option<f32>) + Send + 'static,
    on_partial: impl FnMut(Source, String) + Send + 'static,
    on_diar: impl FnMut(DiarEvent) + Send + 'static,
    on_mic_level: Option<Box<dyn Fn(f32) + Send>>,
) -> Result<SessionStart, StartError> {
    let paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut mic_level = on_mic_level;
    let (finals_tx, finals_rx) = crossbeam_channel::unbounded::<FinalJob>();
    let mut slots: Vec<(Source, Arc<Mutex<Option<PartialJob>>>)> = Vec::new();
    let mut captures: Vec<Box<dyn AudioCapture>> = Vec::new();
    let mut workers: Vec<std::thread::JoinHandle<()>> = Vec::new();
    let mut active: Vec<Source> = Vec::new();
    let mut failed: Vec<(Source, String)> = Vec::new();

    for (source, mut capture, segmenter) in sources {
        let (ftx, frx) = crossbeam_channel::bounded::<AudioFrame>(256);
        let slot = Arc::new(Mutex::new(None));
        let slot_for_worker = slot.clone();
        let final_tx = finals_tx.clone();
        // 先起 worker（消费者），再启动 capture：兼容同步灌帧的 MockCapture，
        // 且若 capture 启动失败，ftx 在 start 内被 drop → frx 关闭 → worker 立即退出。
        let level_cb = if source == Source::Mic { mic_level.take() } else { None };
        let paused_w = paused.clone();
        let w = std::thread::spawn(move || {
            run_segment_worker(
                source,
                frx,
                target_rate,
                partial_interval_samples,
                final_tx,
                slot_for_worker,
                segmenter,
                paused_w,
                level_cb,
            );
        });
        match capture.start(ftx) {
            Ok(()) => {
                active.push(source);
                slots.push((source, slot));
                captures.push(capture);
                workers.push(w);
            }
            Err(e) => {
                failed.push((source, e.to_string()));
                let _ = w.join(); // frx 已关闭，worker 已在退出
            }
        }
    }

    drop(finals_tx); // 仅剩各 worker 持有发送端 → 它们结束后 ASR 才断开

    if active.is_empty() {
        return Err(StartError {
            error: anyhow::anyhow!("没有可用音频源可启动: {failed:?}"),
            recognizer,
            embedder,
        });
    }

    let asr = std::thread::spawn(move || {
        run_asr_worker(
            recognizer, embedder, registry, finals_rx, echo_hold, slots, on_final, on_partial, on_diar,
        )
    });

    Ok(SessionStart {
        handle: RecordingHandle { captures, workers, asr: Some(asr), paused },
        active,
        failed,
    })
}

#[cfg(test)]
mod asr_worker_tests {
    use super::*;
    use crate::asr::{Recognizer, Transcript};
    use crate::audio::Source;
    use crate::diar::MockEmbedder;
    use std::sync::{Arc, Mutex};

    // 短 hold,避免慢测试;既有(非回声去重相关)测试用它即可——它们的段要么单源、
    // 要么时间戳刻意分得够开,不会被误判为回声,hold 时长本身对结果无影响。
    const TEST_ECHO_HOLD: Duration = Duration::from_millis(50);

    struct CountingRecognizer;
    impl Recognizer for CountingRecognizer {
        fn recognize(&mut self, s: &[f32]) -> anyhow::Result<Transcript> {
            Ok(Transcript { text: format!("len={}", s.len()), ..Default::default() })
        }
    }

    struct FlakyRecognizer { n: usize }
    impl Recognizer for FlakyRecognizer {
        fn recognize(&mut self, s: &[f32]) -> anyhow::Result<Transcript> {
            self.n += 1;
            if self.n == 1 {
                anyhow::bail!("boom");
            }
            Ok(Transcript { text: format!("len={}", s.len()), ..Default::default() })
        }
    }

    #[test]
    fn emits_all_finals_tagged_in_order() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        // System 先到、Mic 后到，且时间戳刻意拉开(> ECHO_WINDOW_MS 且不交叠)：System
        // 零延迟处理，Mic 因回声去重会先 hold、在 Disconnected 排干时才 release——
        // 这与本例送达顺序一致(system 先、mic 后)，故整体顺序不变，回声匹配也不误伤。
        tx.send(FinalJob { source: Source::System, samples: vec![0.0; 20], start_ms: 0, end_ms: 625 }).unwrap();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.0; 10], start_ms: 5000, end_ms: 5625 }).unwrap();
        drop(tx);

        let finals = Arc::new(Mutex::new(Vec::<(Source, String, u64, u64)>::new()));
        let f2 = finals.clone();
        let _ = run_asr_worker(
            Box::new(CountingRecognizer),
            None,
            SpeakerRegistry::new(),
            rx,
            TEST_ECHO_HOLD,
            vec![],
            move |s, t, start_ms, end_ms, _, _| f2.lock().unwrap().push((s, t, start_ms, end_ms)),
            |_, _| {},
            |_| {},
        );
        assert_eq!(
            *finals.lock().unwrap(),
            vec![
                (Source::System, "len=20".into(), 0, 625),
                (Source::Mic, "len=10".into(), 5000, 5625)
            ]
        );
    }

    #[test]
    fn failed_final_becomes_placeholder_and_worker_survives() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.0; 3], start_ms: 0, end_ms: 0 }).unwrap();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.0; 4], start_ms: 0, end_ms: 0 }).unwrap();
        drop(tx);

        let finals = Arc::new(Mutex::new(Vec::<(Source, String)>::new()));
        let f2 = finals.clone();
        let _ = run_asr_worker(
            Box::new(FlakyRecognizer { n: 0 }),
            None,
            SpeakerRegistry::new(),
            rx,
            TEST_ECHO_HOLD,
            vec![],
            move |s, t, _, _, _, _| f2.lock().unwrap().push((s, t)),
            |_, _| {},
            |_| {},
        );
        assert_eq!(
            *finals.lock().unwrap(),
            vec![(Source::Mic, "[识别失败]".into()), (Source::Mic, "len=4".into())]
        );
    }

    struct PanicRecognizer { n: usize }
    impl Recognizer for PanicRecognizer {
        fn recognize(&mut self, s: &[f32]) -> anyhow::Result<Transcript> {
            self.n += 1;
            if self.n == 1 {
                panic!("boom");
            }
            Ok(Transcript { text: format!("len={}", s.len()), ..Default::default() })
        }
    }

    #[test]
    fn recognize_panic_becomes_placeholder_worker_survives() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.0; 3], start_ms: 0, end_ms: 0 }).unwrap();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.0; 5], start_ms: 0, end_ms: 0 }).unwrap();
        drop(tx);

        let finals = Arc::new(Mutex::new(Vec::<(Source, String)>::new()));
        let f2 = finals.clone();

        // Suppress "panicked at" output so test stderr stays clean.
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let _ = run_asr_worker(
            Box::new(PanicRecognizer { n: 0 }),
            None,
            SpeakerRegistry::new(),
            rx,
            TEST_ECHO_HOLD,
            vec![],
            move |s, t, _, _, _, _| f2.lock().unwrap().push((s, t)),
            |_, _| {},
            |_| {},
        );
        std::panic::set_hook(prev);

        assert_eq!(
            *finals.lock().unwrap(),
            vec![
                (Source::Mic, "[识别失败]".into()),
                (Source::Mic, "len=5".into()),
            ]
        );
    }

    #[test]
    fn services_latest_partial_when_idle() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        let slot = Arc::new(Mutex::new(Some(PartialJob { source: Source::System, samples: vec![0.0; 7] })));
        let partials = Arc::new(Mutex::new(Vec::<(Source, String)>::new()));
        let p2 = partials.clone();
        let slot_for_worker = slot.clone();

        let worker = std::thread::spawn(move || {
            let _ = run_asr_worker(
                Box::new(CountingRecognizer),
                None,
                SpeakerRegistry::new(),
                rx,
                TEST_ECHO_HOLD,
                vec![(Source::System, slot_for_worker)],
                |_, _, _, _, _, _| {},
                move |s, t| p2.lock().unwrap().push((s, t)),
                |_| {},
            );
        });

        // 轮询等待 worker 在空闲分支服务了 partial 槽（有界，避免固定 sleep 假设）。
        let mut serviced = false;
        for _ in 0..200 {
            if !partials.lock().unwrap().is_empty() {
                serviced = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        drop(tx); // 结束 worker
        worker.join().unwrap();

        assert!(serviced, "空闲时应服务 partial 槽");
        assert_eq!(*partials.lock().unwrap(), vec![(Source::System, "len=7".into())]);
        assert!(slot.lock().unwrap().is_none(), "partial 取出后槽应清空");
    }

    #[test]
    fn finals_get_speaker_labels_and_diar_events() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        // 两段长音频:第一段 → S1;第二段正交向量 → S2。
        // 两段文本(均由 CountingRecognizer 按长度生成)恰好相似("len=32000" 相同)，
        // 时间戳特意拉开(> ECHO_WINDOW_MS 且不交叠)以隔离本用例(测说话人聚类)与
        // 回声去重逻辑,避免被误判丢弃。
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.1; 32000], start_ms: 0, end_ms: 2000 }).unwrap();
        tx.send(FinalJob { source: Source::System, samples: vec![0.1; 32000], start_ms: 10000, end_ms: 12000 }).unwrap();
        drop(tx);

        let embedder = MockEmbedder::new(vec![
            Ok(vec![1.0, 0.0, 0.0]),
            Ok(vec![0.0, 1.0, 0.0]),
        ]);
        let finals = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let diar_events = Arc::new(Mutex::new(0usize));
        let (f2, d2) = (finals.clone(), diar_events.clone());
        let (_r, e) = run_asr_worker(
            Box::new(CountingRecognizer),
            Some(Box::new(embedder)),
            SpeakerRegistry::new(),
            rx,
            TEST_ECHO_HOLD,
            vec![],
            move |_, _, _, _, spk, _| f2.lock().unwrap().push(spk),
            |_, _| {},
            move |_ev| *d2.lock().unwrap() += 1,
        );
        assert!(e.is_some(), "embedder 应返还");
        assert_eq!(
            *finals.lock().unwrap(),
            vec![Some("S1".into()), Some("S2".into())]
        );
        assert!(*diar_events.lock().unwrap() >= 2, "每个新说话人应发 SpeakersChanged");
    }

    #[test]
    fn same_speaker_growing_sources_reemits_speakers() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        // 同一说话人两段，不同 source（两次同向量 → 都归入 S1，sources 从 {mic} 增长到 {mic,system}）。
        // 时间戳拉开(> ECHO_WINDOW_MS 且不交叠)，隔离本用例与回声去重逻辑。
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.1; 32000], start_ms: 0, end_ms: 2000 }).unwrap();
        tx.send(FinalJob { source: Source::System, samples: vec![0.1; 32000], start_ms: 10000, end_ms: 12000 }).unwrap();
        drop(tx);

        let embedder = MockEmbedder::new(vec![
            Ok(vec![1.0, 0.0, 0.0]),
            Ok(vec![1.0, 0.0, 0.0]),
        ]);
        let finals = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let diar_events = Arc::new(Mutex::new(0usize));
        let (f2, d2) = (finals.clone(), diar_events.clone());
        let _ = run_asr_worker(
            Box::new(CountingRecognizer),
            Some(Box::new(embedder)),
            SpeakerRegistry::new(),
            rx,
            TEST_ECHO_HOLD,
            vec![],
            move |_, _, _, _, spk, _| f2.lock().unwrap().push(spk),
            |_, _| {},
            move |_ev| *d2.lock().unwrap() += 1,
        );
        assert_eq!(
            *finals.lock().unwrap(),
            vec![Some("S1".into()), Some("S1".into())],
            "两段同说话人"
        );
        assert!(
            *diar_events.lock().unwrap() >= 2,
            "sources 增长应再发一次 SpeakersChanged（全量比较，非仅 len）"
        );
    }

    #[test]
    fn embed_failure_degrades_to_null_speaker() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.1; 32000], start_ms: 0, end_ms: 2000 }).unwrap();
        drop(tx);
        let embedder = MockEmbedder::new(vec![Err(anyhow::anyhow!("boom"))]);
        let finals = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let f2 = finals.clone();
        let _ = run_asr_worker(
            Box::new(CountingRecognizer),
            Some(Box::new(embedder)),
            SpeakerRegistry::new(),
            rx,
            TEST_ECHO_HOLD,
            vec![],
            move |_, _, _, _, spk, _| f2.lock().unwrap().push(spk),
            |_, _| {},
            |_| {},
        );
        assert_eq!(*finals.lock().unwrap(), vec![None], "嵌入失败段 speaker 为 null,不影响文本");
    }

    #[test]
    fn no_embedder_all_speakers_null() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.1; 32000], start_ms: 0, end_ms: 2000 }).unwrap();
        drop(tx);
        let finals = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let f2 = finals.clone();
        let (_r, e) = run_asr_worker(
            Box::new(CountingRecognizer),
            None,
            SpeakerRegistry::new(),
            rx,
            TEST_ECHO_HOLD,
            vec![],
            move |_, _, _, _, spk, _| f2.lock().unwrap().push(spk),
            |_, _| {},
            |_| {},
        );
        assert!(e.is_none());
        assert_eq!(*finals.lock().unwrap(), vec![None]);
    }

    #[test]
    fn worker_emits_snapshot_exactly_once_at_end_after_other_diar_events() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.1; 32000], start_ms: 0, end_ms: 2000 }).unwrap();
        drop(tx);

        let embedder = MockEmbedder::new(vec![Ok(vec![1.0, 0.0, 0.0])]);
        let events = Arc::new(Mutex::new(Vec::<DiarEvent>::new()));
        let e2 = events.clone();
        let _ = run_asr_worker(
            Box::new(CountingRecognizer),
            Some(Box::new(embedder)),
            SpeakerRegistry::new(),
            rx,
            TEST_ECHO_HOLD,
            vec![],
            |_, _, _, _, _, _| {},
            |_, _| {},
            move |ev| e2.lock().unwrap().push(ev),
        );
        let evs = events.lock().unwrap();
        let snapshot_count = evs.iter().filter(|e| matches!(e, DiarEvent::Snapshot(_))).count();
        assert_eq!(snapshot_count, 1, "worker 结束时应恰发一次 Snapshot");
        assert!(matches!(evs.last().unwrap(), DiarEvent::Snapshot(_)), "Snapshot 应在末尾(既有 diar 事件之后)");
        match evs.last().unwrap() {
            DiarEvent::Snapshot(snaps) => {
                assert_eq!(snaps.len(), 1);
                assert_eq!(snaps[0].id, "S1");
            }
            _ => unreachable!(),
        }
    }

    /// 测试用识别器：按队列依次返回预置文本（耗尽后返回空串），供回声去重测试
    /// 精确控制每段的识别结果，而不依赖样本长度这类间接信号。
    struct ScriptedRecognizer {
        script: std::collections::VecDeque<String>,
    }
    impl ScriptedRecognizer {
        fn new(texts: &[&str]) -> Self {
            Self { script: texts.iter().map(|s| s.to_string()).collect() }
        }
    }
    impl Recognizer for ScriptedRecognizer {
        fn recognize(&mut self, _s: &[f32]) -> anyhow::Result<Transcript> {
            Ok(Transcript { text: self.script.pop_front().unwrap_or_default(), ..Default::default() })
        }
    }

    // ---- P4.5 Task 4: 跨路回声去重(mic hold-and-release + 文本相似)----

    #[test]
    fn mic_first_then_matching_system_only_system_survives() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        // 时间邻近(同区间) + 文本相同 → mic 段应被丢弃,只剩 system 一条。
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.1; 800], start_ms: 1000, end_ms: 1625 }).unwrap();
        tx.send(FinalJob { source: Source::System, samples: vec![0.1; 900], start_ms: 1000, end_ms: 1625 }).unwrap();
        drop(tx);

        let recognizer = ScriptedRecognizer::new(&["hello world", "hello world"]);
        let embedder = MockEmbedder::new(vec![Ok(vec![1.0, 0.0, 0.0])]); // 仅 system 段会 embed
        let finals = Arc::new(Mutex::new(Vec::<(Source, String)>::new()));
        let f2 = finals.clone();
        let _ = run_asr_worker(
            Box::new(recognizer),
            Some(Box::new(embedder)),
            SpeakerRegistry::new(),
            rx,
            TEST_ECHO_HOLD,
            vec![],
            move |s, t, _, _, _, _| f2.lock().unwrap().push((s, t)),
            |_, _| {},
            |_| {},
        );
        assert_eq!(
            *finals.lock().unwrap(),
            vec![(Source::System, "hello world".to_string())],
            "mic 先到、system 后到且同文本:mic 段应被回声去重丢弃,只留 system 一条"
        );
    }

    #[test]
    fn system_first_then_matching_mic_is_dropped() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        tx.send(FinalJob { source: Source::System, samples: vec![0.1; 900], start_ms: 2000, end_ms: 2625 }).unwrap();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.1; 800], start_ms: 2000, end_ms: 2625 }).unwrap();
        drop(tx);

        let recognizer = ScriptedRecognizer::new(&["foo bar", "foo bar"]);
        let embedder = MockEmbedder::new(vec![Ok(vec![1.0, 0.0, 0.0])]); // 仅 system 段会 embed
        let finals = Arc::new(Mutex::new(Vec::<(Source, String)>::new()));
        let f2 = finals.clone();
        let _ = run_asr_worker(
            Box::new(recognizer),
            Some(Box::new(embedder)),
            SpeakerRegistry::new(),
            rx,
            TEST_ECHO_HOLD,
            vec![],
            move |s, t, _, _, _, _| f2.lock().unwrap().push((s, t)),
            |_, _| {},
            |_| {},
        );
        assert_eq!(
            *finals.lock().unwrap(),
            vec![(Source::System, "foo bar".to_string())],
            "system 先到、mic 后到且同文本:mic 到达时应对照 recent_system 命中即丢"
        );
    }

    #[test]
    fn dissimilar_text_or_far_apart_time_does_not_misfire() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        // 组 1:时间邻近,但文本完全不同 → 不应误杀。
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.1; 100], start_ms: 3000, end_ms: 3625 }).unwrap();
        tx.send(FinalJob { source: Source::System, samples: vec![0.1; 100], start_ms: 3000, end_ms: 3625 }).unwrap();
        // 组 2:文本相同,但时间相距甚远 → 不应误杀。
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.1; 100], start_ms: 0, end_ms: 625 }).unwrap();
        tx.send(FinalJob { source: Source::System, samples: vec![0.1; 100], start_ms: 90_000, end_ms: 90_625 }).unwrap();
        drop(tx);

        let recognizer = ScriptedRecognizer::new(&[
            "aaaaaaaaaa",     // mic 组1
            "zzzzzzzzzz",     // system 组1:与 mic 组1 文本完全不同
            "same phrase",    // mic 组2
            "same phrase",    // system 组2:与 mic 组2 文本相同,但时间相距 90s
        ]);
        let embedder = MockEmbedder::new(vec![
            Ok(vec![1.0, 0.0, 0.0, 0.0]),
            Ok(vec![0.0, 1.0, 0.0, 0.0]),
            Ok(vec![0.0, 0.0, 1.0, 0.0]),
            Ok(vec![0.0, 0.0, 0.0, 1.0]),
        ]);
        let finals = Arc::new(Mutex::new(Vec::<(Source, String)>::new()));
        let f2 = finals.clone();
        let _ = run_asr_worker(
            Box::new(recognizer),
            Some(Box::new(embedder)),
            SpeakerRegistry::new(),
            rx,
            TEST_ECHO_HOLD,
            vec![],
            move |s, t, _, _, _, _| f2.lock().unwrap().push((s, t)),
            |_, _| {},
            |_| {},
        );
        // system 段零延迟(到达即处理);mic 段本身不匹配任何 system,最终在 Disconnected
        // 排干时按入队序 release——四段都不应被丢弃。
        let got = finals.lock().unwrap().clone();
        assert_eq!(got.len(), 4, "不相似/不邻近的两组都不应被回声去重误杀: {got:?}");
        assert!(got.contains(&(Source::System, "zzzzzzzzzz".to_string())));
        assert!(got.contains(&(Source::System, "same phrase".to_string())));
        assert!(got.contains(&(Source::Mic, "aaaaaaaaaa".to_string())));
        assert!(got.contains(&(Source::Mic, "same phrase".to_string())));
    }

    /// 回归 P4.5 终审 Finding 2：mic 与 system 两路同时识别失败时，占位文本
    /// ("[识别失败]")相同、时间邻近，若照常参与回声比对会被误判为回声、mic 段
    /// 被误杀。占位段不该参与回声去重，两条都应被 emit（内容不丢，只是都不带
    /// 有效转写文本）。
    #[test]
    fn both_sides_placeholder_text_do_not_echo_dedupe_each_other() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.1; 100], start_ms: 1000, end_ms: 1625 }).unwrap();
        tx.send(FinalJob { source: Source::System, samples: vec![0.1; 100], start_ms: 1000, end_ms: 1625 }).unwrap();
        drop(tx);

        struct AlwaysFailRecognizer;
        impl Recognizer for AlwaysFailRecognizer {
            fn recognize(&mut self, _s: &[f32]) -> anyhow::Result<Transcript> {
                anyhow::bail!("boom")
            }
        }

        let finals = Arc::new(Mutex::new(Vec::<(Source, String)>::new()));
        let f2 = finals.clone();
        let _ = run_asr_worker(
            Box::new(AlwaysFailRecognizer),
            None,
            SpeakerRegistry::new(),
            rx,
            TEST_ECHO_HOLD,
            vec![],
            move |s, t, _, _, _, _| f2.lock().unwrap().push((s, t)),
            |_, _| {},
            |_| {},
        );
        // mic 段是占位文本，跳过 hold 直接即时处理，故先于 system 段 emit（送达顺序:
        // mic 先、system 后）。
        assert_eq!(
            *finals.lock().unwrap(),
            vec![
                (Source::Mic, "[识别失败]".to_string()),
                (Source::System, "[识别失败]".to_string()),
            ],
            "双路各一段占位文本、时间邻近：两条都应 emit，不得被回声去重误杀"
        );
    }

    #[test]
    fn drain_releases_all_pending_without_loss_at_disconnect() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.1; 100], start_ms: 0, end_ms: 625 }).unwrap();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.1; 200], start_ms: 1000, end_ms: 1625 }).unwrap();
        drop(tx); // Disconnected 几乎立即到达,远早于下面刻意设的 10s hold 到期

        let recognizer = ScriptedRecognizer::new(&["first segment", "second segment"]);
        let embedder = MockEmbedder::new(vec![Ok(vec![1.0, 0.0, 0.0]), Ok(vec![0.0, 1.0, 0.0])]);
        let finals = Arc::new(Mutex::new(Vec::<(Source, String)>::new()));
        let f2 = finals.clone();
        let _ = run_asr_worker(
            Box::new(recognizer),
            Some(Box::new(embedder)),
            SpeakerRegistry::new(),
            rx,
            Duration::from_secs(10), // 故意远长于测试运行时间:证明 release 靠 Disconnected 排干,而非到期
            vec![],
            move |s, t, _, _, _, _| f2.lock().unwrap().push((s, t)),
            |_, _| {},
            |_| {},
        );
        assert_eq!(
            *finals.lock().unwrap(),
            vec![
                (Source::Mic, "first segment".to_string()),
                (Source::Mic, "second segment".to_string())
            ],
            "会话结束应排干全部 pending mic(顺序保持入队序),不丢内容"
        );
    }

    #[test]
    fn pending_mic_releases_after_hold_expires_without_matching_system() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        let recognizer = ScriptedRecognizer::new(&["lonely mic segment"]);
        let embedder = MockEmbedder::new(vec![Ok(vec![1.0, 0.0, 0.0])]);
        let finals = Arc::new(Mutex::new(Vec::<(Source, String)>::new()));
        let f2 = finals.clone();

        let worker = std::thread::spawn(move || {
            let _ = run_asr_worker(
                Box::new(recognizer),
                Some(Box::new(embedder)),
                SpeakerRegistry::new(),
                rx,
                TEST_ECHO_HOLD,
                vec![],
                move |s, t, _, _, _, _| f2.lock().unwrap().push((s, t)),
                |_, _| {},
                |_| {},
            );
        });

        tx.send(FinalJob { source: Source::Mic, samples: vec![0.1; 100], start_ms: 0, end_ms: 625 }).unwrap();

        // 有界轮询等待到期 release:此时 tx 仍未 drop(channel 未断开),证明 release 由
        // 到期检查(timeout tick / 下一条 final 前)触发,而非依赖会话结束时的排干。
        let mut released = false;
        for _ in 0..200 {
            if !finals.lock().unwrap().is_empty() {
                released = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(released, "hold 到期后应自动 release,无需等待会话结束");
        assert_eq!(*finals.lock().unwrap(), vec![(Source::Mic, "lonely mic segment".to_string())]);

        drop(tx);
        worker.join().unwrap();
    }

    /// 外语幻觉段整段丢弃:不 emit、不进说话人表;正常段带 rms 落到 on_final。
    #[test]
    fn worker_drops_foreign_final_and_reports_rms() {
        // ScriptRecognizer: 第一条返回日语标签,第二条正常中文(lang 空,兜底不命中)。
        struct ScriptRecognizer(std::collections::VecDeque<Transcript>);
        impl Recognizer for ScriptRecognizer {
            fn recognize(&mut self, _s: &[f32]) -> anyhow::Result<Transcript> {
                Ok(self.0.pop_front().unwrap_or_default())
            }
        }
        let script = vec![
            Transcript { text: "でかし".into(), lang: "<|ja|>".into() },
            Transcript { text: "正常句子".into(), lang: "<|zh|>".into() },
        ];
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.5; 1600], start_ms: 0, end_ms: 100 }).unwrap();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.5; 1600], start_ms: 200, end_ms: 300 }).unwrap();
        drop(tx);
        let mut finals: Vec<(String, Option<f32>)> = Vec::new();
        run_asr_worker(
            Box::new(ScriptRecognizer(script.into())),
            None,
            SpeakerRegistry::new(),
            rx,
            Duration::from_millis(0), // hold 归零,立即 release
            Vec::new(),
            |_src, text, _s, _e, _spk, rms| finals.push((text, rms)),
            |_, _| {},
            |_| {},
        );
        assert_eq!(finals.len(), 1, "日语幻觉段被丢弃");
        assert_eq!(finals[0].0, "正常句子");
        let rms = finals[0].1.expect("正常段必须带 rms");
        assert!((rms - 0.5).abs() < 1e-3, "全 0.5 样本的 RMS 应为 0.5,得 {rms}");
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;
    use crate::asr::{Recognizer, Transcript};
    use crate::audio::mock::MockCapture;
    use crate::audio::{AudioCapture, AudioFrame, Source};
    use crate::pipeline::segmenter::MockSegmenter;
    use crossbeam_channel::Sender;
    use std::sync::{Arc, Mutex};

    // 短 hold,避免慢测试(与本文件顶部 ECHO_HOLD_MS 的生产值区分开)。
    const TEST_ECHO_HOLD: Duration = Duration::from_millis(50);

    struct CountingRecognizer;
    impl Recognizer for CountingRecognizer {
        fn recognize(&mut self, s: &[f32]) -> anyhow::Result<Transcript> {
            Ok(Transcript { text: format!("len={}", s.len()), ..Default::default() })
        }
    }

    /// 按内容(而非仅长度)生成文本的测试识别器：定长分段器(MockSegmenter)对不同音频也可能
    /// 切出相同长度的段,若识别文本只看长度,两路不同内容的段会被回声去重误判为同一人。
    /// 真实场景该由真实 ASR 输出的转写文本自然区分,这里用内容摘要模拟“文本不同”。
    struct ContentDigestRecognizer;
    impl Recognizer for ContentDigestRecognizer {
        fn recognize(&mut self, s: &[f32]) -> anyhow::Result<Transcript> {
            let mut hash: u64 = 1469598103934665603; // FNV-1a offset basis
            for &x in s {
                hash ^= x.to_bits() as u64;
                hash = hash.wrapping_mul(1099511628211);
            }
            Ok(Transcript { text: format!("h{hash:x}n{}", s.len()), ..Default::default() })
        }
    }

    /// 发完 fixture 帧后保持通道开启，直到 stop() 被调用——用于测真停止与运行中的会话。
    struct IdlingCapture {
        frames: Vec<AudioFrame>,
        stop_tx: Option<Sender<()>>,
    }
    impl IdlingCapture {
        fn from_fixture() -> Self {
            Self::from_wav(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sample_16k.wav"))
        }
        fn from_wav(path: &str) -> Self {
            let mut cap = MockCapture::from_wav(path).expect("fixture");
            // 借 MockCapture 的分帧：把它的帧抽出来（通过一次性 start 到本地通道）。
            let (tx, rx) = crossbeam_channel::unbounded::<AudioFrame>();
            cap.start(tx).unwrap();
            Self { frames: rx.try_iter().collect(), stop_tx: None }
        }
    }
    impl AudioCapture for IdlingCapture {
        fn start(&mut self, sink: Sender<AudioFrame>) -> anyhow::Result<()> {
            let frames = std::mem::take(&mut self.frames);
            let (stx, srx) = crossbeam_channel::bounded::<()>(0);
            self.stop_tx = Some(stx);
            std::thread::spawn(move || {
                for f in frames {
                    let _ = sink.send(f);
                }
                srx.recv().ok(); // 阻塞直到 stop() drop 掉 stx
                // sink 在此 drop → 分段 worker 的 frame_rx 关闭 → flush 退出
            });
            Ok(())
        }
        fn stop(&mut self) {
            self.stop_tx = None;
        }
    }

    #[test]
    fn merges_two_sources_and_stops_cleanly() {
        let finals = Arc::new(Mutex::new(Vec::<(Source, String)>::new()));
        let f2 = finals.clone();

        // 两源用不同 fixture(内容不同):真实场景 mic/system 音频不同才是常态；本用例只测
        // 两源都能跑通落盘全链路，不是回声去重场景——用不同内容 + 按内容生成文本的识别器，
        // 避免定长分段器切出的等长段被回声去重误判为同一人而丢弃(见 ContentDigestRecognizer)。
        let sources: Vec<(Source, Box<dyn AudioCapture>, Box<dyn Segmenter>)> = vec![
            (Source::Mic, Box::new(IdlingCapture::from_fixture()), Box::new(MockSegmenter::new(2000))),
            (
                Source::System,
                Box::new(IdlingCapture::from_wav(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/sample_zh_16k.wav"
                ))),
                Box::new(MockSegmenter::new(2000)),
            ),
        ];

        let start = start_session(
            sources,
            Box::new(ContentDigestRecognizer),
            None,
            SpeakerRegistry::new(),
            TEST_ECHO_HOLD,
            16000,
            4000,
            move |s, t, _, _, _, _| f2.lock().unwrap().push((s, t)),
            |_, _| {},
            |_| {},
            None,
        )
        .expect("start_session");

        assert_eq!(start.active.len(), 2, "两源都应启动");
        assert!(start.failed.is_empty());

        // 等待两源都产出至少一个 final（有界轮询）。
        let mut ok = false;
        for _ in 0..300 {
            let g = finals.lock().unwrap();
            let has_mic = g.iter().any(|(s, _)| *s == Source::Mic);
            let has_sys = g.iter().any(|(s, _)| *s == Source::System);
            if has_mic && has_sys {
                ok = true;
                break;
            }
            drop(g);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = start.handle.stop(); // 真停止：停 capture → join workers → join asr
        assert!(ok, "两源都应产出带标记的 final");
    }

    #[test]
    fn stop_returns_recognizer_for_reuse() {
        let sources: Vec<(Source, Box<dyn AudioCapture>, Box<dyn Segmenter>)> = vec![(
            Source::Mic,
            Box::new(IdlingCapture::from_fixture()),
            Box::new(MockSegmenter::new(2000)),
        )];
        let start = start_session(
            sources,
            Box::new(CountingRecognizer),
            None,
            SpeakerRegistry::new(),
            TEST_ECHO_HOLD,
            16000,
            4000,
            |_, _, _, _, _, _| {},
            |_, _| {},
            |_| {},
            None,
        )
        .expect("start_session");
        let (r, _e) = start.handle.stop();
        assert!(r.is_some(), "停止后应返还 recognizer 供复用");
    }

    #[test]
    fn all_sources_fail_returns_recognizer_in_err() {
        struct FailingCapture;
        impl AudioCapture for FailingCapture {
            fn start(&mut self, _sink: Sender<AudioFrame>) -> anyhow::Result<()> {
                anyhow::bail!("unauthorized: nope")
            }
            fn stop(&mut self) {}
        }
        let sources: Vec<(Source, Box<dyn AudioCapture>, Box<dyn Segmenter>)> =
            vec![(Source::System, Box::new(FailingCapture), Box::new(MockSegmenter::new(8000)))];
        let r = start_session(
            sources,
            Box::new(CountingRecognizer),
            None,
            SpeakerRegistry::new(),
            TEST_ECHO_HOLD,
            16000,
            4000,
            |_, _, _, _, _, _| {},
            |_, _| {},
            |_| {},
            None,
        );
        let err = match r {
            Ok(_) => panic!("无源可启动应返回 Err"),
            Err(e) => e,
        };
        assert!(err.error.to_string().contains("没有可用音频源"));
        let _reusable: Box<dyn Recognizer> = err.recognizer; // Err 携带 recognizer 返还
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreign_final_detection() {
        // 模型标签命中(sherpa 原样格式与裸格式都认)
        assert!(is_foreign_final("<|ja|>", "任意文本"));
        assert!(is_foreign_final("ko", "任意文本"));
        assert!(!is_foreign_final("<|zh|>", "正常中文"));
        assert!(!is_foreign_final("en", "hello world"));
        // 字符占比兜底(标签缺失时)
        assert!(is_foreign_final("", "でかし"), "纯假名");
        assert!(is_foreign_final("", "美国のポ調スパ"), "假名混杂占比过阈");
        assert!(is_foreign_final("", "안녕하세요"), "谚文");
        assert!(!is_foreign_final("", "中英 mixed 正常句子 ok"), "中英混合放行");
        assert!(!is_foreign_final("", "純漢字幻覺讀作中文"), "纯汉字不拦(无损)");
        assert!(!is_foreign_final("", "[识别失败]"), "占位段绝不误杀");
        assert!(!is_foreign_final("", ""), "空串放行");
    }
}
