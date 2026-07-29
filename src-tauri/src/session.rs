use crate::asr::{Recognizer, Transcript};
use crate::audio::{AudioCapture, AudioFrame, Source};
use crate::diar::registry::SpeakerRegistry;
use crate::diar::split::{
    detect_change_points, group_tokens_by_boundaries, SPLIT_HOP_MS, SPLIT_MIN_SEGMENT_MS,
    SPLIT_WIN_MS,
};
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
/// 自适应 hold 的延长倍数:system 侧有在途语句(last_system_partial 非空)时,mic 段
/// 的 hold 延长为 echo_hold × 此倍数(默认 2.5s×6=15s,覆盖 VAD 15s 硬切上限)。
/// 外放场景 mic 回声段常先于 system 长句定稿,固定 2.5s 一到就放行,等 system 句
/// 落地时 pending 里已无从比对——冒烟实锤的主要漏杀形态。真实发言被误延时预览
/// 仍即时可见,且 system 句一定稿(last_system_partial 清空)下个 tick 就放行。
const ECHO_HOLD_EXTEND_FACTOR: u32 = 6;
/// 已放行 mic 段的追溯窗口(ms):system 段定稿时回查最近这么久内已发出的 mic 段,
/// 命中回声即撤回(DiarEvent::EchoRetract)。兜住自适应 hold 也没罩住的极端时序
/// (system 无 partial 期间 mic 到期放行等)。按流时间裁剪。
const RETRACT_WINDOW_MS: u64 = 30_000;

// AEC 残渣抑制(冒烟反馈)：外放场景下 mic 收到的 AEC 消除残渣被识别成垃圾中文/
// 标点("大"/"。"/"The.")，文本与 system 段不相似躲过上面的文本回声去重，污染
// 转写与说话人表。残渣必然与外放(system 路)同时发生，能量却远低于近场真人声，
// 故用「时间重叠比例 + rms 上界」双条件识别，与文本回声去重同一批检查点。
/// AEC 残渣判定:mic 段与 system 段时间重叠比例下限。
/// 残渣必然与外放(system 路)同时发生;真人插话即使重叠,近场 rms 也远高于下面的上界。
/// 待校准。
pub(crate) const RESIDUE_OVERLAP_MIN: f32 = 0.8;
/// AEC 残渣 rms 上界。2026-07-05 外放数据: 残渣 ≤0.0091,近场人声典型 ≥0.02,取 30% 余量。待校准。
pub(crate) const RESIDUE_RMS_MAX: f32 = 0.012;

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

/// contains 满分捷径的最短长度门槛（归一化后按 char 计）：子段化后短句（语气词
/// "嗯"/"对"之类）更容易被任意长文本"完全包含"而误拿满分，是段内切分带来的
/// 连带效应，P4.5 校准时的素材里还没有这类超短子段。较短一方低于此长度时改走
/// levenshtein 距离分（仍可能命中，但不再是 contains 的无条件 1.0）。阈值 4
/// 待真实回声素材复核校准。
const ECHO_CONTAINS_MIN_LEN: usize = 4;

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
    let shorter_len = ca.len().min(cb.len());
    let contains_score = if shorter_len < ECHO_CONTAINS_MIN_LEN {
        0.0
    } else if ca.len() <= cb.len() {
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

/// `[a_start,a_end)` 与 `[b_start,b_end)` 的重叠时长占 a 段时长的比例(0~1)。
/// a 段时长为 0 时返回 0(理论不出现零时长段,防御性避免除零)。
fn overlap_fraction(a_start: u64, a_end: u64, b_start: u64, b_end: u64) -> f32 {
    let a_dur = a_end.saturating_sub(a_start);
    if a_dur == 0 {
        return 0.0;
    }
    let overlap_start = a_start.max(b_start);
    let overlap_end = a_end.min(b_end);
    let overlap = overlap_end.saturating_sub(overlap_start);
    overlap as f32 / a_dur as f32
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

/// 已放行(on_final 已发出)的 mic 段的轻量记录:system 段定稿时回查,命中回声即
/// 通过 DiarEvent::EchoRetract 撤回(调用方负责从落盘/UI 移除)。
struct EmittedMic {
    text: String,
    norm: String,
    start_ms: u64,
    end_ms: u64,
}

/// 已处理的 system 段的轻量记录，供后续到达的 mic 段比对（回声去重）。
struct RecentSystem {
    text: String,
    norm: String,
    start_ms: u64,
    end_ms: u64,
}

/// AEC 残渣判定的原子条件：一对(mic, system)段的 rms + 时间重叠是否命中残渣特征。
/// 供两个检查点共用（mic 到达时对照 recent_system；system 到达时对照 pending_mic）。
fn is_residue_pair(rms: f32, a_start: u64, a_end: u64, b_start: u64, b_end: u64) -> bool {
    rms < RESIDUE_RMS_MAX && overlap_fraction(a_start, a_end, b_start, b_end) >= RESIDUE_OVERLAP_MIN
}

/// AEC 残渣判定：mic 段 rms 低于上界，且与某个最近处理过的 system 段有足够比例的
/// 时间重叠——残渣必然与外放(system 路)同时发生，能量却达不到近场真人声门槛。
fn is_aec_residue(sub_start: u64, sub_end: u64, rms: f32, recent_system: &VecDeque<RecentSystem>) -> bool {
    recent_system
        .iter()
        .any(|r| is_residue_pair(rms, sub_start, sub_end, r.start_ms, r.end_ms))
}

/// 完整处理链：embed → assign → take_merges/SpeakersChanged → on_final。
/// 即时路径（system 段、无匹配的 mic 段）与 release 路径（hold 到期/排干的 mic 段）共用，
/// 保证「被丢弃段零副作用、被处理段处理逻辑同源」。
/// sample_store 维护「簇 id → 该簇迄今最长段样本(截 SPEAKER_SAMPLE_CAP)」,
/// 簇合并时随之迁移(loser 样本更长则归 winner),停止时随 Snapshot 导出供声纹库试听。
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
    sample_store: &mut std::collections::HashMap<String, Vec<f32>>,
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
    // 样本更新先于合并处理:若本段归属簇随后被合并,下面的迁移会把样本一并带走。
    // 已存样本 ≥ ENOUGH 即定格:更长不再有试听增益,不值一次整块克隆。
    if let Some(id) = &speaker {
        let keep = embedding_input.len().min(SPEAKER_SAMPLE_CAP);
        let cur = sample_store.get(id).map(|v| v.len()).unwrap_or(0);
        if keep > cur && cur < SPEAKER_SAMPLE_ENOUGH {
            sample_store.insert(id.clone(), embedding_input[..keep].to_vec());
        }
    }
    for (loser, winner) in registry.take_merges() {
        if let Some(ls) = sample_store.remove(&loser) {
            let wl = sample_store.get(&winner).map(|v| v.len()).unwrap_or(0);
            if ls.len() > wl {
                sample_store.insert(winner.clone(), ls);
            }
        }
        on_diar(DiarEvent::Merged { loser, winner });
    }
    // 实时全局入库：够料的无主簇当场经回调入库拿全局 person id(mark_enrolled),
    // 新说话人不必等停止就获得全局唯一身份;person 变化随下方 speakers 差分广播。
    registry.enroll_pending();
    let speakers = registry.speakers();
    if speakers != *last_sent {
        *last_sent = speakers.clone();
        on_diar(DiarEvent::SpeakersChanged(speakers));
    }
    on_final(source, text, start_ms, end_ms, speaker, Some(rms));
}

/// 毫秒(相对段首)→ 样本下标：16kHz 单声道，1ms = 16 样本。
fn ms_to_sample_idx(ms: u64) -> usize {
    (ms * 16) as usize
}

/// 一个母段切出的子段:等价一个独立 final。
struct SubFinal {
    text: String,
    samples: Vec<f32>,
    start_ms: u64,
    end_ms: u64,
}

/// 母段 → 子段列表(len ≥ 1)。任何"装不下/跑不动/切不出/切了也没内容"的情形都
/// 回退单元素原段——不丢内容是本函数唯一不可违反的不变式，宁可不切也不能丢。
///
/// 失败/跳过路径：无 embedder；段时长 < SPLIT_MIN_SEGMENT_MS；变更点检测无点；
/// 全部子段文本 trim 后为空。
fn split_final(
    job_samples: Vec<f32>,
    job_start_ms: u64,
    job_end_ms: u64,
    transcript: &Transcript,
    embedder: &mut Option<Box<dyn SpeakerEmbedder>>,
    // 开关透传：子段重识别复核（见下方 is_foreign_final 调用）需与整段判定用
    // 同一个开关状态，否则关闭过滤后仍会在回退路径误丢子段。
    _language_filter: bool,
) -> Vec<SubFinal> {
    let whole_segment = |job_samples: Vec<f32>| {
        vec![SubFinal {
            text: transcript.text.clone(),
            samples: job_samples,
            start_ms: job_start_ms,
            end_ms: job_end_ms,
        }]
    };

    let total_ms = job_end_ms.saturating_sub(job_start_ms);
    if embedder.is_none() || total_ms < SPLIT_MIN_SEGMENT_MS {
        return whole_segment(job_samples);
    }

    // 计时:滑窗嵌入 + 分组/重识别总耗时,仅在确有切分发生时随子段数一并打印
    // (性能可观测;不影响回退路径——那些路径本身就没有这条日志)。
    let split_started_at = Instant::now();

    // 滑窗嵌入：窗起点 idx*hop，窗长 win；末窗不足窗长则止（不足一窗的尾音直接
    // 不再开窗，其内容仍归属最后一个子段——不会丢样本，只是不参与切分判定）。
    let mut embs: Vec<Option<Vec<f32>>> = Vec::new();
    let mut win_start_ms = 0u64;
    while win_start_ms + SPLIT_WIN_MS <= total_ms {
        let start_idx = ms_to_sample_idx(win_start_ms).min(job_samples.len());
        let end_idx = ms_to_sample_idx(win_start_ms + SPLIT_WIN_MS).min(job_samples.len());
        let window = &job_samples[start_idx..end_idx];
        // 与既有 embed 防护同款：panic 视同失败，该窗记 None（"与两侧都相似"，
        // 宁可漏切不误切），绝不让滑窗嵌入的异常波及整段处理。
        let emb = embedder.as_mut().and_then(|e| {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| e.embed(window))) {
                Ok(Ok(v)) => Some(v),
                Ok(Err(err)) => {
                    eprintln!("段内切分: 滑窗嵌入失败: {err}");
                    None
                }
                Err(_) => {
                    eprintln!("段内切分: 滑窗嵌入 panic,该窗视为与两侧相似");
                    None
                }
            }
        });
        embs.push(emb);
        win_start_ms += SPLIT_HOP_MS;
    }

    let boundaries = detect_change_points(&embs, total_ms);
    if boundaries.is_empty() {
        return whole_segment(job_samples);
    }

    // 文本只能按模型给出的 token 时间戳切。若时间戳缺失或不可信，保留母段：
    // 说话人边界属于弱证据，不能触发第二次 ASR 覆盖已有文本，否则同一音频会因
    // 上下文缩短产生不同字词，造成内容层准确率倒退。
    let groups = group_tokens_by_boundaries(&transcript.tokens, &transcript.timestamps, &boundaries);
    if groups.is_none() {
        eprintln!("段内切分: 时间戳缺失,保留母段文本");
        return whole_segment(job_samples);
    }

    let mut subs: Vec<SubFinal> = Vec::new();
    let seg_count = boundaries.len() + 1;
    for i in 0..seg_count {
        let seg_start_ms = if i == 0 { 0 } else { boundaries[i - 1] };
        let seg_end_ms = if i < boundaries.len() { boundaries[i] } else { total_ms };
        let start_idx = ms_to_sample_idx(seg_start_ms).min(job_samples.len());
        // 末子段直接取到样本末尾：ms 换算下取整会丢掉 <1ms 的尾部样本，母段
        // 最后一个子段没有下一边界兜底，用真实样本长度消除这点误差。
        let end_idx = if i == seg_count - 1 {
            job_samples.len()
        } else {
            ms_to_sample_idx(seg_end_ms).min(job_samples.len())
        };
        let sub_samples = job_samples[start_idx..end_idx].to_vec();

        let text = groups.as_ref().expect("groups checked above")[i].clone();

        // 子段文本 trim 后为空 → 丢弃该子段（与空白段过滤同哲学：无文本内容
        // 不产 final）。是否全部被丢在循环结束后统一检查、回退整段。
        if text.trim().is_empty() {
            continue;
        }
        subs.push(SubFinal {
            text,
            samples: sub_samples,
            start_ms: job_start_ms + seg_start_ms,
            end_ms: job_start_ms + seg_end_ms,
        });
    }

    if subs.is_empty() {
        // 全部子段文本被丢弃：不丢内容不变式 → 回退单元素原段。
        return whole_segment(job_samples);
    }

    eprintln!(
        "段内切分: 母段 {total_ms}ms 切为 {} 子段,耗时 {}ms",
        subs.len(),
        split_started_at.elapsed().as_millis()
    );
    subs
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
/// Snapshot 额外携带各簇的代表性音频样本(≤ SPEAKER_SAMPLE_CAP,声纹库试听用):
/// 与质心同一时刻导出,消费方(lib.rs)在入库后为人物落样本 WAV。
#[derive(Debug, Clone)]
pub enum DiarEvent {
    SpeakersChanged(Vec<crate::diar::registry::SpeakerInfo>),
    Merged { loser: String, winner: String },
    Snapshot {
        snaps: Vec<crate::diar::registry::ClusterSnapshot>,
        samples: Vec<(String, Vec<f32>)>,
    },
    /// 追溯回声撤回:一条已放行(on_final 已发出)的 mic 段事后被确认是 system 段的
    /// 回声。调用方须把这条段从落盘与 UI 中移除(按 start_ms/end_ms/text 精确匹配,
    /// 时间戳为会话相对值,消费方自行加续录 base_ms)。挂在 DiarEvent 总线上是复用
    /// 既有事件通道(避免为单一事件再扩 run_asr_worker 签名),并非声纹语义。
    EchoRetract { start_ms: u64, end_ms: u64, text: String },
    /// 自动规则判为不可见的识别结果。消费方必须先写原始段，再以 reason 写入抑制
    /// sidecar；不得直接丢弃，以便离线评测、误杀诊断和恢复。
    SuppressedFinal {
        source: Source,
        text: String,
        start_ms: u64,
        end_ms: u64,
        rms: Option<f32>,
        reason: String,
    },
}

/// 声纹样本上限:15s。超长截头 15s——试听确认身份用不着更长,也把 worker 内存
/// 占用限制在每簇 <1MB。
pub(crate) const SPEAKER_SAMPLE_CAP: usize =
    15 * crate::store::audio::AUDIO_SAMPLE_RATE as usize;
/// 样本「够好」阈值:已存样本达到 10s 后不再升级替换——试听没有增益,却省掉长会中
/// 每逢更长段就整块克隆(≤1MB)的 ASR 热路径开销。
pub(crate) const SPEAKER_SAMPLE_ENOUGH: usize =
    10 * crate::store::audio::AUDIO_SAMPLE_RATE as usize;

/// 定稿处理链的持有者：回声去重(hold/追溯撤回/AEC 残渣)、语言过滤、段内切分、
/// 声纹归簇与 emit 全在这里。从 run_asr_worker 抽出是为了让本机 worker 与云端
/// worker 喂同一条链——链一旦有两份实现，回声/声纹的判据迟早会分叉。
/// 生命周期 'a 借用 worker 持有的 embedder 与 registry(worker 结束后要把 embedder
/// 还给调用方复用)；三个回调按值持有，随 sink 一起活到 finish。
pub(crate) struct FinalSink<'a, F, P, D>
where
    F: FnMut(Source, String, u64, u64, Option<String>, Option<f32>),
    P: FnMut(Source, String),
    D: FnMut(DiarEvent),
{
    embedder: &'a mut Option<Box<dyn SpeakerEmbedder>>,
    registry: &'a mut SpeakerRegistry,
    echo_hold: Duration,
    /// 语言幻觉过滤总开关，见 run_asr_worker 同名参数注释。
    language_filter: bool,
    on_final: F,
    on_partial: P,
    on_diar: D,
    /// 与上次发送的完整说话人表比较（非仅 len）：同段内「合并-1+新建+1」净零、
    /// 已有簇 sources 增长等变化都能被捕获并同步。
    last_sent: Vec<crate::diar::registry::SpeakerInfo>,
    // 回声去重状态：hold 中的 mic 段（入队序）+ 最近处理过的 system 段（供 mic 端比对）
    // + 已放行的 mic 段（供 system 定稿后的追溯撤回）。
    pending_mic: VecDeque<PendingMic>,
    recent_system: VecDeque<RecentSystem>,
    recent_mic: VecDeque<EmittedMic>,
    /// system 路当前在途预览文本(预览级回声抑制用):随 system partial 更新,该句
    /// 定稿(进 recent_system)即清——staleness 被限制在"当前在途一句"内,不会拿几分钟前
    /// 的旧预览误杀 mic 新语句。
    last_system_partial: String,
    /// 各簇代表性样本(声纹库试听),随 process_final 更新、Snapshot 导出。
    sample_store: std::collections::HashMap<String, Vec<f32>>,
}

impl<'a, F, P, D> FinalSink<'a, F, P, D>
where
    F: FnMut(Source, String, u64, u64, Option<String>, Option<f32>),
    P: FnMut(Source, String),
    D: FnMut(DiarEvent),
{
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        embedder: &'a mut Option<Box<dyn SpeakerEmbedder>>,
        registry: &'a mut SpeakerRegistry,
        echo_hold: Duration,
        language_filter: bool,
        on_final: F,
        on_partial: P,
        on_diar: D,
    ) -> Self {
        Self {
            embedder,
            registry,
            echo_hold,
            language_filter,
            on_final,
            on_partial,
            on_diar,
            last_sent: Vec::new(),
            pending_mic: VecDeque::new(),
            recent_system: VecDeque::new(),
            recent_mic: VecDeque::new(),
            last_system_partial: String::new(),
            sample_store: std::collections::HashMap::new(),
        }
    }

    /// release 一个到期/排干的 pending mic 段：走完整处理链，与即时路径同源。
    fn release_pending(&mut self, p: PendingMic) {
        // 记录已放行 mic 段(占位段除外),供 system 定稿后的追溯回声撤回。
        if p.text != "[识别失败]" {
            self.recent_mic.push_back(EmittedMic {
                text: p.text.clone(),
                norm: p.norm.clone(),
                start_ms: p.start_ms,
                end_ms: p.end_ms,
            });
        }
        process_final(
            Source::Mic,
            p.text,
            p.start_ms,
            p.end_ms,
            p.samples_len,
            &p.embedding_input,
            p.rms,
            self.embedder,
            self.registry,
            &mut self.sample_store,
            &mut self.last_sent,
            &mut self.on_final,
            &mut self.on_diar,
        );
    }

    /// 周期性的 pending mic 到期检查。调用方每收到一条 final 前调一次(让长时间
    /// 空转但持续来 final 的场景也能及时 release)，空闲 tick 也调一次兜底。
    pub(crate) fn tick(&mut self) {
        // 自适应 hold:system 侧有在途语句(预览未定稿)时延长——mic 回声段常先于
        // system 长句定稿,固定 2.5s 放行就错过比对(冒烟实锤主漏杀形态);system
        // 句一定稿 last_system_partial 即清,下个检查点就按普通 hold 放行。
        let hold_now = if self.last_system_partial.is_empty() {
            self.echo_hold
        } else {
            self.echo_hold * ECHO_HOLD_EXTEND_FACTOR
        };
        while self
            .pending_mic
            .front()
            .is_some_and(|p| p.held_at.elapsed() >= hold_now)
        {
            let p = self.pending_mic.pop_front().unwrap();
            self.release_pending(p);
        }
    }

    /// 一条已识别的定稿段进入完整处理链(语言过滤 → 段内切分 → 回声 hold/声纹/emit)。
    /// `samples` 为该段原始音频，语言过滤丢弃时用于算 rms、正常路径进切分与声纹嵌入。
    pub(crate) fn push(
        &mut self,
        source: Source,
        t: Transcript,
        start_ms: u64,
        end_ms: u64,
        samples: &[f32],
    ) {
        // 语言白名单:外语幻觉段与 ECHO 命中同待遇——不 embed/不 assign/
        // 不 emit/不落盘,从源头杜绝垃圾段污染说话人表。占位段占比 0 天然放行。
        if self.language_filter && is_foreign_final(&t.lang, &t.text) {
            eprintln!(
                "语言过滤: 丢弃 {:?} 段 lang=\"{}\" text=\"{}\"",
                source,
                t.lang,
                text_prefix20(&t.text)
            );
            // 被丢段无 final 接替，前端只在收到 final 时清 partial 预览，
            // 幻觉文本会残留成 UI 残影；主动推空 partial 顶掉它。
            (self.on_partial)(source, String::new());
            (self.on_diar)(DiarEvent::SuppressedFinal {
                source,
                text: t.text,
                start_ms,
                end_ms,
                rms: Some(rms_of(samples)),
                reason: "foreign_language".into(),
            });
            return;
        }

        // 占位段("[识别失败]")没有时间戳也没有切分意义,不进滑窗切分,
        // 沿既有专用路径原样走一个"子段"。真实段交给 split_final:装不下/
        // 跑不动/切不出/切了也没内容 → 内部各失败路径回退单元素原段，
        // 下面循环体在"无变更点"的绝大多数段上退化为原逻辑，零行为变化。
        let subs: Vec<SubFinal> = if t.text == "[识别失败]" {
            vec![SubFinal {
                text: t.text,
                samples: samples.to_vec(),
                start_ms,
                end_ms,
            }]
        } else {
            split_final(
                samples.to_vec(),
                start_ms,
                end_ms,
                &t,
                self.embedder,
                self.language_filter,
            )
        };

        for sub in subs {
            let seg_rms = rms_of(&sub.samples);
            match source {
                Source::System => self.push_system_sub(sub, seg_rms),
                Source::Mic => self.push_mic_sub(sub, seg_rms),
            }
        }
    }

    /// system 侧子段：先用它击杀/撤回 mic 侧回声，再零延迟走处理链并记入 recent_system。
    fn push_system_sub(&mut self, sub: SubFinal, seg_rms: f32) {
        let sys_norm = normalize_text(&sub.text);
        // 先对照 pending_mic：命中即丢弃（零副作用），不进入处理链。
        // 占位文本("[识别失败]"，未归一比较)是"确有发声但识别失败"的
        // 痕迹，不参与回声比对：双路同时识别失败时文本雷同（都是占位串）
        // 又时间邻近，若照常比对会把 mic 占位段误判为回声丢弃，静默吞掉
        // 一段真实发声。故遇到占位段的 pending 直接跳过匹配，原样保留。
        // retain 闭包内不能直接调用 on_partial（借用冲突：on_partial 与
        // pending_mic 同属 self，闭包已独占 pending_mic 的可变借用）；改用
        // 局部 flag，retain 结束后统一补一次空 partial，清掉被丢 mic 段的 UI 残影。
        let mut dropped_mic: Vec<(String, u64, u64, f32, &'static str)> = Vec::new();
        self.pending_mic.retain(|p| {
            if p.text == "[识别失败]" {
                return true;
            }
            // AEC 残渣抑制:与文本回声去重镜像的第二个检查点——新到 system
            // 段与某 pending mic 段重叠且 mic 段 rms 低,视为残渣,先于文本
            // 相似度判定丢弃(残渣文本本就与 system 段不相似,躲不过下面的
            // echoed 判定,须单独拦)。
            if is_residue_pair(p.rms, p.start_ms, p.end_ms, sub.start_ms, sub.end_ms) {
                eprintln!(
                    "残渣抑制: 丢弃 mic 段 rms={:.4} \"{}\"",
                    p.rms,
                    text_prefix20(&p.text)
                );
                dropped_mic.push((p.text.clone(), p.start_ms, p.end_ms, p.rms, "aec_residue"));
                return false;
            }
            let echoed = time_near(p.start_ms, p.end_ms, sub.start_ms, sub.end_ms)
                && text_similarity(&p.norm, &sys_norm) >= ECHO_SIM_THRESHOLD;
            if echoed {
                eprintln!(
                    "回声去重: 丢弃 mic 段(与 system 段匹配) mic=\"{}\" system=\"{}\"",
                    text_prefix20(&p.text),
                    text_prefix20(&sub.text)
                );
                dropped_mic.push((p.text.clone(), p.start_ms, p.end_ms, p.rms, "echo_match"));
            }
            !echoed
        });
        if !dropped_mic.is_empty() {
            (self.on_partial)(Source::Mic, String::new());
        }
        for (text, start_ms, end_ms, rms, reason) in dropped_mic {
            (self.on_diar)(DiarEvent::SuppressedFinal {
                source: Source::Mic,
                text,
                start_ms,
                end_ms,
                rms: Some(rms),
                reason: reason.into(),
            });
        }
        // system 段零延迟处理。
        process_final(
            Source::System,
            sub.text.clone(),
            sub.start_ms,
            sub.end_ms,
            sub.samples.len(),
            &sub.samples,
            seg_rms,
            self.embedder,
            self.registry,
            &mut self.sample_store,
            &mut self.last_sent,
            &mut self.on_final,
            &mut self.on_diar,
        );
        // 追溯回声撤回:该 system 句可能对应一条已放行的 mic 回声段
        // (hold 到期先落盘了)。命中(时间邻近+文本高相似,与击杀
        // pending 同一套判据)即发 EchoRetract,由调用方从落盘与 UI
        // 移除。占位段不参与(双路同时识别失败文本雷同,会互相误杀)。
        if sub.text != "[识别失败]" {
            let mut i = 0;
            while i < self.recent_mic.len() {
                let hit = time_near(
                    self.recent_mic[i].start_ms,
                    self.recent_mic[i].end_ms,
                    sub.start_ms,
                    sub.end_ms,
                ) && text_similarity(&self.recent_mic[i].norm, &sys_norm) >= ECHO_SIM_THRESHOLD;
                if hit {
                    let m = self.recent_mic.remove(i).unwrap();
                    eprintln!(
                        "追溯回声撤回: mic=\"{}\" system=\"{}\"",
                        text_prefix20(&m.text),
                        text_prefix20(&sub.text)
                    );
                    (self.on_diar)(DiarEvent::EchoRetract {
                        start_ms: m.start_ms,
                        end_ms: m.end_ms,
                        text: m.text,
                    });
                } else {
                    i += 1;
                }
            }
            let newest_end = sub.end_ms;
            self.recent_mic
                .retain(|m| newest_end.saturating_sub(m.end_ms) <= RETRACT_WINDOW_MS);
        }
        // 该句已定稿:预览级抑制改由 recent_system(带 10s 窗)接力,
        // 清掉在途预览文本,防其无限期滞留误杀后续 mic 预览。
        self.last_system_partial.clear();
        self.recent_system.push_back(RecentSystem {
            text: sub.text,
            norm: sys_norm,
            start_ms: sub.start_ms,
            end_ms: sub.end_ms,
        });
        let newest_end = sub.end_ms;
        self.recent_system
            .retain(|r| newest_end.saturating_sub(r.end_ms) <= RECENT_SYSTEM_WINDOW_MS);
    }

    /// mic 侧子段：占位段直通、残渣/回声命中即丢，其余进 hold 等 system 侧比对。
    fn push_mic_sub(&mut self, sub: SubFinal, seg_rms: f32) {
        // 占位文本("[识别失败]"，未归一比较)是"确有发声但识别失败"的痕迹，
        // 不参与回声去重：双路同时识别失败时文本雷同（都是占位串）又时间
        // 邻近，会被误判为回声互相丢弃，静默吞掉一段真实发声。跳过匹配与
        // hold，直接走完整处理链即时处理。
        if sub.text == "[识别失败]" {
            process_final(
                Source::Mic,
                sub.text,
                sub.start_ms,
                sub.end_ms,
                sub.samples.len(),
                &sub.samples,
                seg_rms,
                self.embedder,
                self.registry,
                &mut self.sample_store,
                &mut self.last_sent,
                &mut self.on_final,
                &mut self.on_diar,
            );
        } else if is_aec_residue(sub.start_ms, sub.end_ms, seg_rms, &self.recent_system) {
            // AEC 残渣抑制:与文本回声去重镜像的第一个检查点——rms 低且与
            // 某最近 system 段高度重叠,视为外放残渣,不进 hold/不处理,与
            // ECHO 命中同待遇。
            eprintln!(
                "残渣抑制: 丢弃 mic 段 rms={:.4} \"{}\"",
                seg_rms,
                text_prefix20(&sub.text)
            );
            (self.on_partial)(Source::Mic, String::new());
            (self.on_diar)(DiarEvent::SuppressedFinal {
                source: Source::Mic,
                text: sub.text,
                start_ms: sub.start_ms,
                end_ms: sub.end_ms,
                rms: Some(seg_rms),
                reason: "aec_residue".into(),
            });
        } else {
            let mic_norm = normalize_text(&sub.text);
            let echo = self.recent_system.iter().find(|r| {
                time_near(sub.start_ms, sub.end_ms, r.start_ms, r.end_ms)
                    && text_similarity(&mic_norm, &r.norm) >= ECHO_SIM_THRESHOLD
            });
            match echo {
                Some(r) => {
                    eprintln!(
                        "回声去重: 丢弃 mic 段(与 system 段匹配) mic=\"{}\" system=\"{}\"",
                        text_prefix20(&sub.text),
                        text_prefix20(&r.text)
                    );
                    // 命中：不 embed/不 assign/不 emit/不落盘，直接丢弃。
                    // 同语言过滤路径：无 final 接替，主动清空该源 partial 残影。
                    (self.on_partial)(Source::Mic, String::new());
                    (self.on_diar)(DiarEvent::SuppressedFinal {
                        source: Source::Mic,
                        text: sub.text,
                        start_ms: sub.start_ms,
                        end_ms: sub.end_ms,
                        rms: Some(seg_rms),
                        reason: "echo_match".into(),
                    });
                }
                None => {
                    self.pending_mic.push_back(PendingMic {
                        text: sub.text,
                        norm: mic_norm,
                        start_ms: sub.start_ms,
                        end_ms: sub.end_ms,
                        samples_len: sub.samples.len(),
                        embedding_input: sub.samples,
                        held_at: Instant::now(),
                        rms: seg_rms,
                    });
                }
            }
        }
    }

    /// system 侧在途预览文本更新(预览级回声抑制依赖)：只记状态，不 emit。
    pub(crate) fn note_system_partial(&mut self, text: &str) {
        self.last_system_partial.clear();
        self.last_system_partial.push_str(text);
    }

    /// 一版预览文本进入预览链：system 侧记在途文本，mic 侧按回声抑制，再统一 emit。
    pub(crate) fn push_partial(&mut self, source: Source, text: String) {
        // 预览级回声抑制:外放场景 mic 路会实时"跟读"system 路
        // 正在说的话——定稿级去重(hold/recent_system)不管预览,
        // UI 上会出现「我」「对方」两行同字齐蹦的回音观感。mic
        // 预览与 system 在途预览或最近 system 定稿(10s 窗)高相似
        // 即按回声压掉(推空清残影)。只影响预览:若真是本人发言,
        // 定稿仍走完整判定链(hold+时间邻近+rms),不丢内容。
        let text = match source {
            Source::System => {
                self.note_system_partial(&text);
                text
            }
            Source::Mic => {
                let echoed = text_similarity(&text, &self.last_system_partial) >= ECHO_SIM_THRESHOLD
                    || self
                        .recent_system
                        .iter()
                        .any(|r| text_similarity(&text, &r.text) >= ECHO_SIM_THRESHOLD);
                if echoed {
                    eprintln!("预览回声抑制: 隐藏 mic 预览 \"{}\"", text_prefix20(&text));
                    String::new()
                } else {
                    text
                }
            }
        };
        (self.on_partial)(source, text);
    }

    /// 收尾：排干全部 pending（无论是否到期），保持入队序，再发 Snapshot。
    pub(crate) fn finish(mut self) {
        while let Some(p) = self.pending_mic.pop_front() {
            self.release_pending(p);
        }
        let snaps = self.registry.snapshot();
        let samples = self.sample_store.drain().collect();
        (self.on_diar)(DiarEvent::Snapshot { snaps, samples });
    }
}

/// 单识别 worker：串行消费 finals（不丢、优先），空闲时取每源最新 partial（best-effort）。
/// finals_rx 关闭且排干后返回。识别失败的完成句 emit "[识别失败]" 占位，worker 不退出。
/// 每条 final 定稿时额外提声纹嵌入并归簇（嵌入失败/无 embedder/panic 均降级为 None，绝不影响文本）；
/// 归簇产生的簇合并 / 说话人表变化通过 on_diar 通知（顺序：先 Merged 后 SpeakersChanged）。
/// 识别得到的语言标签命中外语白名单过滤（`is_foreign_final`）的整段直接丢弃，
/// 与 ECHO 命中同待遇；未被丢弃的段额外算出段级 rms，随 `on_final` 尾参
/// `Option<f32>` 透传给调用方落盘（partial 路径不参与语言过滤，也不算 rms）。
///
/// 本函数只负责「取 job → 识别 → 交给 FinalSink」：定稿的全部判定与副作用都在
/// `FinalSink` 里，云端 worker 复用同一个 sink，两条来源的处理链永远一致。
#[allow(clippy::too_many_arguments)]
pub fn run_asr_worker(
    mut recognizer: Box<dyn Recognizer>,
    mut embedder: Option<Box<dyn SpeakerEmbedder>>,
    mut registry: SpeakerRegistry,
    finals_rx: Receiver<FinalJob>,
    echo_hold: Duration,
    // 语言幻觉过滤总开关：会议场景默认开(过滤中日韩误判幻觉段)，多语会议可关闭
    // 以保留外语真实发言；关闭时 FinalSink 内的 is_foreign_final 判定整体短路为不丢弃。
    language_filter: bool,
    partial_slots: Vec<(Source, Arc<Mutex<Option<PartialJob>>>)>,
    on_final: impl FnMut(Source, String, u64, u64, Option<String>, Option<f32>),
    on_partial: impl FnMut(Source, String),
    on_diar: impl FnMut(DiarEvent),
) -> (Box<dyn Recognizer>, Option<Box<dyn SpeakerEmbedder>>) {
    let mut sink = FinalSink::new(
        &mut embedder,
        &mut registry,
        echo_hold,
        language_filter,
        on_final,
        on_partial,
        on_diar,
    );

    loop {
        match finals_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(job) => {
                // 到期检查(先于本条 final 的处理)：让长时间空转但持续来 final 的场景
                // 也能及时 release，不必等到 timeout tick。
                sink.tick();

                let t: Transcript = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    recognizer.recognize(&job.samples)
                })) {
                    Ok(Ok(t)) => t,
                    Ok(Err(_)) => Transcript { text: "[识别失败]".to_string(), ..Default::default() },
                    Err(_) => {
                        eprintln!(
                            "run_asr_worker: recognize panicked on a {:?} final; 以占位继续",
                            job.source
                        );
                        Transcript { text: "[识别失败]".to_string(), ..Default::default() }
                    }
                };
                sink.push(job.source, t, job.start_ms, job.end_ms, &job.samples);
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                // 到期检查：无 final 到来时靠这个 100ms tick 兜底 release。
                sink.tick();
                // 空闲：服务每源最新 partial（取出即清空，只识别最新一版）。
                for (src, slot) in &partial_slots {
                    let job = slot.lock().unwrap().take();
                    if let Some(job) = job {
                        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            recognizer.recognize(&job.samples)
                        })) {
                            Ok(Ok(t)) => sink.push_partial(*src, t.text),
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
                sink.finish();
                break;
            }
        }
    }
    (recognizer, embedder)
}

// ===== 云端流式识别 worker(spec §4)=====
//
// 与本机 worker 的分工:本机侧「切段 → 识别 → FinalSink」,云端侧把切段与识别都
// 交给厂商,worker 只做三件本机才知道的事——(1) 把厂商的流内时钟映射回本源时间轴,
// (2) 断连期间的音频记账 + 重连后批式补识,(3) 把结果喂进同一个 FinalSink。
// 处理链复用 FinalSink 是硬要求:回声去重/声纹/语言过滤一旦有两份实现必然分叉。

/// ring 容量:5 分钟(spec §4)。断网缺口能补回的上限,也是 worker 内存占用上限
/// (5min × 16kHz × 4B ≈ 19MB/源)。
pub(crate) const CLOUD_RING_CAP: usize = 5 * 60 * 16_000;
/// 重连退避基数(ms):1s→2s→4s…上限 30s。测试注入 1ms,避免单测空等真实退避。
#[cfg(not(test))]
const CLOUD_BACKOFF_BASE_MS: u64 = 1000;
#[cfg(test)]
const CLOUD_BACKOFF_BASE_MS: u64 = 1;
/// 退避上限(ms):长时间断网不再无限拉长,保证网络恢复后 30s 内必然重试。
const CLOUD_BACKOFF_CAP_MS: u64 = 30_000;
/// 「这条流算活过」的门槛(ms):活够这么久才把退避清回基数。低于门槛的流是
/// 「开起来就死」的抖动流(鉴权过期/服务端持续拒绝),此时清零退避会退化成
/// 忙重连风暴——每次都 1s 后再来一次,既打厂商也烧电。测试注入 5ms 免空等。
#[cfg(not(test))]
const CLOUD_BACKOFF_STABLE_MS: u64 = 5000;
#[cfg(test)]
const CLOUD_BACKOFF_STABLE_MS: u64 = 5;
/// 停录后排干厂商剩余定稿的窗口(ms)。生产值是边界层的单一真源
/// (`asr::cloud::CLOUD_DRAIN_MS`),适配层的内部排干预算据它收窄,防止两边错位丢句。
/// 测试用短窗:mock 流在 finish 后即断开通道,排干靠断开结束,短窗只是兜底。
#[cfg(not(test))]
use crate::asr::cloud::CLOUD_DRAIN_MS;
#[cfg(test)]
const CLOUD_DRAIN_MS: u64 = 200;
/// 主循环空闲 tick(ms):驱动 FinalSink 的 hold 到期检查,与本机 worker 同节奏。
const CLOUD_TICK_MS: u64 = 100;

/// 缺口切段器:缺口音频 → (段偏移样本, 段) 列表。由调用方注入本机 VAD
/// (厂商批式接口有单请求时长上限),worker 只把偏移叠回绝对时间轴。
pub type BackfillSegmenter = dyn FnMut(&[f32]) -> Vec<(u64, Vec<f32>)> + Send;

/// 云端识别的连接状态,供 UI 提示「重连中/补识中/已恢复」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloudAsrStatus {
    Reconnecting { source: Source },
    Recovered { source: Source },
    Backfilling { source: Source },
    BackfillFailed { source: Source },
}

/// 每源音频账本:推流样本计数(=流内时钟)+ 环形缓冲 + 断连缺口。
///
/// 为什么要自己记账:厂商给的时间戳是「本条流内的相对 ms」,重连后从 0 重计,
/// 而落盘/UI 要的是「本源会话时间轴」。fed 只在真正喂给识别的帧上前进(暂停期
/// 的帧在 segment_worker 就被丢了,压根到不了这里),因此与本机路的时间轴同语义。
pub(crate) struct SourceFeed {
    /// 已推样本(16kHz),暂停不计 → 与本地时间轴同语义。
    fed: u64,
    /// 当前流打开时的 fed(重连后厂商 ms 从 0 重计,靠它加回绝对位置)。
    stream_base: u64,
    ring: VecDeque<f32>,
    /// ring[0] 的绝对样本号(掐头后前进)。
    ring_start: u64,
    /// 断连起点(绝对样本号);None = 无未补缺口。
    gap_from: Option<u64>,
    /// 最后一条厂商定稿的绝对结束样本号(开流时以 fed 为起点)。缺口起点靠它,
    /// 而不是靠 fed —— 见 gap_start。
    last_final_end: u64,
}

impl SourceFeed {
    fn new() -> Self {
        Self {
            fed: 0,
            stream_base: 0,
            ring: VecDeque::new(),
            ring_start: 0,
            gap_from: None,
            last_final_end: 0,
        }
    }

    /// 缺口起点:已定稿的末尾与已推样本取小者。
    /// 厂商定稿滞后于推流(推进去的最后一两秒往往还没吐出定稿就断了),若拿 fed
    /// 当起点,这段"推了但没回话"的音频会被算作已识别而永久丢失;拿定稿末尾当
    /// 起点最多让接缝处重复识别一小段。接缝宁重复不丢(spec §4)。
    /// min(fed) 是护栏:定稿 end 常越过已喂样本(服务端补白),缺口起点不该跑到
    /// 未来去,否则 gap_to <= gap_from 会把整个缺口判成零长。
    fn gap_start(&self) -> u64 {
        self.last_final_end.min(self.fed)
    }

    /// 起一个缺口(若尚无)。get_or_insert:重连成功后立刻再断的场景,缺口起点仍
    /// 是最早那次,中间这段音频不能被后一次断连"吃掉"。
    fn begin_gap(&mut self) {
        let from = self.gap_start();
        self.gap_from.get_or_insert(from);
    }

    /// 开流(首开或重连成功)时重基:新流的厂商时钟从 0 重计,此刻的 fed 既是
    /// stream_base 也是"尚未定稿的起点"。
    fn rebase(&mut self, at: u64) {
        self.stream_base = at;
        self.last_final_end = at;
    }

    /// 记一帧账:fed 前进、入环、超 CAP 掐头。断连与否都要记——缺口的音频全靠
    /// 这份 ring 补回来。
    fn account(&mut self, samples: &[f32]) {
        self.fed += samples.len() as u64;
        self.ring.extend(samples.iter().copied());
        if self.ring.len() > CLOUD_RING_CAP {
            let overflow = self.ring.len() - CLOUD_RING_CAP;
            self.ring.drain(..overflow);
            self.ring_start += overflow as u64;
        }
    }

    /// `[from,to)` 的环内音频(声纹样本用)。
    /// 段首已被环挤掉 → 整段给空:宁可没有声纹,也不能拿一段错位的音频去归簇。
    /// 段尾越过环末 → 截到环末:厂商定稿的 end 常比我们已喂的样本多出几十毫秒
    /// (服务端补白),为这点尾巴丢掉整段声纹得不偿失。
    fn slice(&self, from: u64, to: u64) -> Vec<f32> {
        let ring_end = self.ring_start + self.ring.len() as u64;
        if to <= from || from < self.ring_start || from >= ring_end {
            return Vec::new();
        }
        let lo = (from - self.ring_start) as usize;
        let hi = (to.min(ring_end) - self.ring_start) as usize;
        self.ring.iter().copied().skip(lo).take(hi - lo).collect()
    }

    /// `[from,to)` 与环的交集,零拷贝借出(补识切段用,可能是几分钟的大数组)。
    fn contiguous(&mut self, from: u64, to: u64) -> &[f32] {
        let lo = from.max(self.ring_start);
        let hi = to.min(self.ring_start + self.ring.len() as u64);
        if hi <= lo {
            return &[];
        }
        let (lo, hi) = ((lo - self.ring_start) as usize, (hi - self.ring_start) as usize);
        &self.ring.make_contiguous()[lo..hi]
    }
}

/// 样本数 → ms(16kHz)。
fn samples_to_ms(n: u64) -> u64 {
    n * 1000 / crate::store::audio::AUDIO_SAMPLE_RATE as u64
}

/// ms → 样本数(16kHz)。
fn ms_to_samples(ms: u64) -> u64 {
    ms * crate::store::audio::AUDIO_SAMPLE_RATE as u64 / 1000
}

/// 厂商定稿 → Transcript。逐词表映射成 tokens + 相对段首的秒时间戳,喂 FinalSink
/// 的段内切分(与本机 Qwen3 的 token 时间戳同形);无词表则两者皆空,自然降级为段级。
fn utterance_to_transcript(u: &crate::asr::cloud::DefiniteUtterance) -> Transcript {
    Transcript {
        text: u.text.clone(),
        lang: u.lang.clone(),
        tokens: u.words.iter().map(|w| w.text.clone()).collect(),
        timestamps: u
            .words
            .iter()
            .map(|w| w.start_ms.saturating_sub(u.start_ms) as f32 / 1000.0)
            .collect(),
    }
}

/// 云端 worker 的每源运行态:账本 + 当前流 + 重连节奏。
struct CloudSource {
    source: Source,
    feed: SourceFeed,
    /// None = 当前无可用流(断连或已正常关闭)。
    stream: Option<crate::asr::cloud::CloudStream>,
    /// 音频通道是否仍开着(全关 = 停录)。
    audio_open: bool,
    /// 当前流的开流时刻;None = 无活流。用于判断这条流"活够久"没有(退避防抖)。
    stream_opened_at: Option<Instant>,
    /// 下次重连时刻(退避);None = 不安排自动重连(正常关闭 / 从未断连)。
    retry_at: Option<Instant>,
    backoff_ms: u64,
}

/// 下一次重连退避:活够 STABLE 才算这条流站住过,退避清回基数;否则继续翻倍到上限。
/// 纯函数(不看时钟)以便直接单测。
fn next_backoff(prev_ms: u64, lived_ms: u64) -> u64 {
    if lived_ms >= CLOUD_BACKOFF_STABLE_MS {
        CLOUD_BACKOFF_BASE_MS
    } else {
        (prev_ms * 2).min(CLOUD_BACKOFF_CAP_MS)
    }
}

/// 标记一次断连:记缺口起点、丢弃死流、通知 UI、起退避。
fn mark_disconnected<S: FnMut(CloudAsrStatus)>(cs: &mut CloudSource, on_status: &mut S) {
    cs.feed.begin_gap();
    cs.stream = None;
    // 退避按"这条流活了多久"推进:开起来就死的抖动流必须继续拉长间隔,
    // 无条件清零会在鉴权过期/服务端持续拒绝时变成忙重连风暴。
    let lived_ms = cs
        .stream_opened_at
        .take()
        .map_or(0, |t| t.elapsed().as_millis() as u64);
    cs.backoff_ms = next_backoff(cs.backoff_ms, lived_ms);
    cs.retry_at = Some(Instant::now() + Duration::from_millis(cs.backoff_ms));
    on_status(CloudAsrStatus::Reconnecting { source: cs.source });
}

/// 厂商定稿的绝对样本区间 `[start, end)`:流内 ms 叠加本流基准。
/// 单独成函数,是因为这段换算同时决定两件事——取哪段音频做声纹、缺口从哪里起
/// (last_final_end),两处必须用同一把尺子,否则接缝会悄悄错位。
fn definite_abs_samples(
    stream_base: u64,
    u: &crate::asr::cloud::DefiniteUtterance,
) -> (u64, u64) {
    (
        stream_base + ms_to_samples(u.start_ms),
        stream_base + ms_to_samples(u.end_ms),
    )
}

/// 一条厂商定稿落进 FinalSink:流内 ms → 绝对 ms,并按绝对区间从 ring 取声纹样本。
fn push_definite<F, P, D>(
    source: Source,
    feed: &mut SourceFeed,
    u: &crate::asr::cloud::DefiniteUtterance,
    sink: &mut FinalSink<'_, F, P, D>,
) where
    F: FnMut(Source, String, u64, u64, Option<String>, Option<f32>),
    P: FnMut(Source, String),
    D: FnMut(DiarEvent),
{
    let base_ms = samples_to_ms(feed.stream_base);
    let start_ms = base_ms + u.start_ms;
    let end_ms = base_ms + u.end_ms;
    let (from, to) = definite_abs_samples(feed.stream_base, u);
    let samples = feed.slice(from, to);
    // 定稿推进"已识别水位":后面若断连,缺口就从这里起(见 SourceFeed::gap_start)。
    feed.last_final_end = to;
    // 到期检查先于本条定稿(与本机 worker 同款):持续来定稿时也能及时放行 hold 中的段。
    sink.tick();
    sink.push(source, utterance_to_transcript(u), start_ms, end_ms, &samples);
}

/// 一条占位段:确有发声但没能识别回来的区间,留痕不静默丢(与本机路占位同语义)。
fn push_placeholder<F, P, D>(
    source: Source,
    start_ms: u64,
    end_ms: u64,
    sink: &mut FinalSink<'_, F, P, D>,
) where
    F: FnMut(Source, String, u64, u64, Option<String>, Option<f32>),
    P: FnMut(Source, String),
    D: FnMut(DiarEvent),
{
    sink.push(
        source,
        Transcript { text: "[识别失败]".to_string(), ..Default::default() },
        start_ms,
        end_ms,
        &[],
    );
}

/// 缺口补识:ring 未覆盖的头部直接占位,覆盖部分切段后逐段批式识别。
/// 任一段失败 → 整缺口一条占位段(spec §4):成功段先攒着不发,避免同一区间既有
/// 文本段又盖一条占位段,时间轴上自相矛盾。
#[allow(clippy::too_many_arguments)]
fn backfill_gap<F, P, D, S>(
    source: Source,
    feed: &mut SourceFeed,
    gap_from: u64,
    gap_to: u64,
    cloud: &Arc<dyn crate::asr::cloud::CloudAsr>,
    backfill_segmenter: &mut BackfillSegmenter,
    sink: &mut FinalSink<'_, F, P, D>,
    on_status: &mut S,
) where
    F: FnMut(Source, String, u64, u64, Option<String>, Option<f32>),
    P: FnMut(Source, String),
    D: FnMut(DiarEvent),
    S: FnMut(CloudAsrStatus),
{
    if gap_to <= gap_from {
        return; // 零长缺口(断连期间一帧没来):无音频可补,也不必惊动 UI
    }
    on_status(CloudAsrStatus::Backfilling { source });

    // ring 之外的区间音频已被覆盖,补无可补 → 先落一条占位,再处理覆盖部分,
    // 保证时间轴顺序(占位在前)。
    // .min(gap_to):占位段绝不许越过缺口末尾。ring_start > gap_to 在当前调用路径下
    // 走不到(缺口闭合时 gap_to = fed ≥ ring 覆盖范围),一行钳制换掉一类"占位段
    // 盖到未来"的时间轴错乱。
    let covered_from = gap_from.max(feed.ring_start).min(gap_to);
    if covered_from > gap_from {
        push_placeholder(source, samples_to_ms(gap_from), samples_to_ms(covered_from), sink);
    }
    let chunks = {
        let audio = feed.contiguous(covered_from, gap_to);
        if audio.is_empty() {
            return;
        }
        backfill_segmenter(audio)
    };

    let mut done: Vec<(Transcript, u64, u64, Vec<f32>)> = Vec::new();
    let mut failed = false;
    for (offset, chunk) in chunks {
        match cloud.transcribe_batch(&chunk) {
            Ok(utts) => {
                for u in utts {
                    // 绝对时间 = 缺口可补起点 + 段偏移 + 段内相对时间。
                    let seg_base = covered_from + offset;
                    let start_ms = samples_to_ms(seg_base) + u.start_ms;
                    let end_ms = samples_to_ms(seg_base) + u.end_ms;
                    let samples = feed.slice(
                        seg_base + ms_to_samples(u.start_ms),
                        seg_base + ms_to_samples(u.end_ms),
                    );
                    done.push((utterance_to_transcript(&u), start_ms, end_ms, samples));
                }
            }
            Err(e) => {
                eprintln!("云端补识失败({:?} 源): {e};整缺口按占位段处理", source);
                failed = true;
                break;
            }
        }
    }

    if failed {
        // 覆盖整个缺口的占位;若头部已单独占位过,就从可补起点开始,免得两条
        // 占位段互相包含。
        push_placeholder(source, samples_to_ms(covered_from), samples_to_ms(gap_to), sink);
        on_status(CloudAsrStatus::BackfillFailed { source });
        return;
    }
    for (t, start_ms, end_ms, samples) in done {
        sink.push(source, t, start_ms, end_ms, &samples);
    }
}

/// 尝试重连并补识缺口。`force` 忽略退避(停录收尾时只此一次)。
/// 顺序:重开成功 → 重基 stream_base → 补识缺口 → Recovered 殿后,「已恢复」才
/// 真的意味着这段时间没留窟窿。
#[allow(clippy::too_many_arguments)]
fn try_recover<F, P, D, S>(
    cs: &mut CloudSource,
    force: bool,
    cloud: &Arc<dyn crate::asr::cloud::CloudAsr>,
    backfill_segmenter: &mut BackfillSegmenter,
    sink: &mut FinalSink<'_, F, P, D>,
    on_status: &mut S,
) where
    F: FnMut(Source, String, u64, u64, Option<String>, Option<f32>),
    P: FnMut(Source, String),
    D: FnMut(DiarEvent),
    S: FnMut(CloudAsrStatus),
{
    let Some(gap_from) = cs.feed.gap_from else {
        return;
    };
    // retry_at=None 且非 force:这个缺口不是断连留下的(厂商正常关闭,见
    // handle_cloud_event),没有重连语义——正常关闭再自动开一条流只是凭空多一次
    // 握手。缺口留到停录收尾时统一处理(force 一次:能补则补,补不回就占位)。
    if !force {
        match cs.retry_at {
            None => return,
            Some(t) if Instant::now() < t => return,
            Some(_) => {}
        }
    }
    match cloud.open_stream() {
        Ok(stream) => {
            cs.stream = Some(stream);
            cs.stream_opened_at = Some(Instant::now());
            // 退避不在这里清零:一条流"开起来了"不代表它站得住,清零交给
            // mark_disconnected 按存活时长判定(next_backoff)。
            cs.retry_at = None;
            // 新流的厂商时钟从 0 重计:此刻的 fed 就是它的基准。缺口在此闭合。
            let gap_to = cs.feed.fed;
            cs.feed.rebase(gap_to);
            cs.feed.gap_from = None;
            backfill_gap(
                cs.source,
                &mut cs.feed,
                gap_from,
                gap_to,
                cloud,
                backfill_segmenter,
                sink,
                on_status,
            );
            on_status(CloudAsrStatus::Recovered { source: cs.source });
        }
        Err(e) => {
            eprintln!("云端重连失败({:?} 源): {e}", cs.source);
            cs.backoff_ms = (cs.backoff_ms * 2).min(CLOUD_BACKOFF_CAP_MS);
            cs.retry_at = Some(Instant::now() + Duration::from_millis(cs.backoff_ms));
        }
    }
}

/// 云端识别 worker：每源一条厂商流，音频记账 + 推流，事件回灌同一条 FinalSink。
///
/// 断连不丢内容是本函数的核心不变式：流一死就记下缺口起点(取"最后一条定稿的末尾"
/// 而非"已推样本",厂商定稿滞后于推流,接缝宁重复不丢)，其间的音频照常记账进
/// ring；重连成功后把缺口音频切段批式补回来，补不回来(超出 ring / 批式失败 / 停录
/// 前始终连不上)也要留一条 `[识别失败]` 占位段，绝不静默吞掉一段发声。
/// 厂商正常关闭(Closed{error:None})同样起缺口——只要音频还在来，那之后喂进来的
/// 每一帧都无人识别，差别只在不重连、不报"重连中"，缺口留到停录收尾统一了结。
///
/// audio_rxs 全部关闭 = 停录：对活流 finish 并排干厂商最后几句，再 sink.finish()，
/// 返还 embedder 供调用方复用。
#[allow(clippy::too_many_arguments)]
pub fn run_cloud_asr_worker(
    cloud: Arc<dyn crate::asr::cloud::CloudAsr>,
    mut embedder: Option<Box<dyn SpeakerEmbedder>>,
    mut registry: SpeakerRegistry,
    audio_rxs: Vec<(Source, Receiver<Vec<f32>>)>,
    echo_hold: Duration,
    // 语言幻觉过滤总开关，见 run_asr_worker 同名参数注释。
    language_filter: bool,
    // 缺口音频 → (段偏移样本, 段) 列表。由调用方注入本机 VAD 切段(厂商批式接口
    // 有单请求时长上限),worker 只负责把偏移叠回绝对时间轴。
    mut backfill_segmenter: Box<BackfillSegmenter>,
    on_final: impl FnMut(Source, String, u64, u64, Option<String>, Option<f32>),
    on_partial: impl FnMut(Source, String),
    on_diar: impl FnMut(DiarEvent),
    mut on_status: impl FnMut(CloudAsrStatus),
) -> Option<Box<dyn SpeakerEmbedder>> {
    let mut sink = FinalSink::new(
        &mut embedder,
        &mut registry,
        echo_hold,
        language_filter,
        on_final,
        on_partial,
        on_diar,
    );

    let mut srcs: Vec<CloudSource> = audio_rxs
        .iter()
        .map(|(source, _)| CloudSource {
            source: *source,
            feed: SourceFeed::new(),
            stream: None,
            audio_open: true,
            stream_opened_at: None,
            retry_at: None,
            backoff_ms: CLOUD_BACKOFF_BASE_MS,
        })
        .collect();

    // 开流。首次就开不起来的源直接进重连状态机(缺口从 0 起),不放弃该源:
    // 网络在录制中恢复的场景下,这段音频还能靠补识捞回来。
    for cs in srcs.iter_mut() {
        match cloud.open_stream() {
            Ok(s) => {
                cs.stream = Some(s);
                cs.stream_opened_at = Some(Instant::now());
                cs.feed.rebase(0); // 首流:基准与"已识别水位"都在 0
            }
            Err(e) => {
                eprintln!("云端开流失败({:?} 源): {e};进入重连", cs.source);
                mark_disconnected(cs, &mut on_status);
            }
        }
    }

    /// 主循环一次唤醒的来源(usize 为源下标)。
    enum Woke {
        Audio(usize, Result<Vec<f32>, crossbeam_channel::RecvError>),
        Event(usize, Result<crate::asr::cloud::CloudEvent, crossbeam_channel::RecvError>),
        Tick,
    }

    loop {
        if srcs.iter().all(|cs| !cs.audio_open) {
            break; // 全部音频通道关闭 = 停录
        }

        // 1) 事件优先:先把已到的识别事件处理干净再收音频。断连时刻决定缺口起点,
        //    若让事件落后于音频,断连前已成功推送的音频会被错算进缺口(重复识别)。
        for cs in srcs.iter_mut() {
            while let Some(stream) = cs.stream.as_ref() {
                match stream.events.try_recv() {
                    Ok(e) => handle_cloud_event(cs, e, &mut sink, &mut on_status),
                    Err(crossbeam_channel::TryRecvError::Empty) => break,
                    Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        // 事件通道无声无息断开(适配层线程异常退出):视同带错关闭,
                        // 否则这条流会变成"活着但永不出字"的黑洞。
                        eprintln!("云端事件通道断开({:?} 源);按断连处理", cs.source);
                        mark_disconnected(cs, &mut on_status);
                        break;
                    }
                }
            }
        }

        // 2) 重连 + 补识。只在所有源都无音频积压时做:开流与批式识别都要走网络,
        //    可能阻塞数秒,占着主循环就会把各源的记账一起堵死。
        let idle = audio_rxs
            .iter()
            .zip(srcs.iter())
            .all(|((_, rx), cs)| !cs.audio_open || rx.is_empty());
        if idle {
            for cs in srcs.iter_mut() {
                try_recover(
                    cs,
                    false,
                    &cloud,
                    backfill_segmenter.as_mut(),
                    &mut sink,
                    &mut on_status,
                );
            }
        }

        // 3) 等音频帧 / 事件 / tick。Select 借着 audio_rxs 与各流的 events,
        //    借用必须在改状态之前结束,故收完值就出块。
        let woke = {
            let mut sel = crossbeam_channel::Select::new();
            let mut ops: Vec<(usize, bool, usize)> = Vec::new(); // (op, 是音频, 源下标)
            for (i, cs) in srcs.iter().enumerate() {
                if cs.audio_open {
                    ops.push((sel.recv(&audio_rxs[i].1), true, i));
                }
                if let Some(s) = cs.stream.as_ref() {
                    ops.push((sel.recv(&s.events), false, i));
                }
            }
            match sel.select_timeout(Duration::from_millis(CLOUD_TICK_MS)) {
                Err(_) => Woke::Tick,
                Ok(oper) => {
                    let idx = oper.index();
                    let (_, is_audio, i) = *ops
                        .iter()
                        .find(|(op, _, _)| *op == idx)
                        .expect("select 只会返回上面注册过的操作");
                    if is_audio {
                        Woke::Audio(i, oper.recv(&audio_rxs[i].1))
                    } else {
                        let rx = &srcs[i].stream.as_ref().expect("事件操作只对活流注册").events;
                        Woke::Event(i, oper.recv(rx))
                    }
                }
            }
        };

        match woke {
            Woke::Audio(i, Ok(frame)) => {
                let cs = &mut srcs[i];
                // 先推后记账:推失败时缺口起点正好落在本帧之前(本帧没送达厂商,
                // 必须算进缺口)。断连期间流为 None → 只记账,不推(规约 7)。
                let push_failed = match cs.stream.as_mut() {
                    Some(s) => (s.push)(&frame).is_err(),
                    None => false,
                };
                if push_failed {
                    eprintln!("云端推流失败({:?} 源);按断连处理", cs.source);
                    mark_disconnected(cs, &mut on_status);
                }
                cs.feed.account(&frame);
            }
            Woke::Audio(i, Err(_)) => srcs[i].audio_open = false,
            Woke::Event(i, Ok(e)) => handle_cloud_event(&mut srcs[i], e, &mut sink, &mut on_status),
            Woke::Event(i, Err(_)) => {
                eprintln!("云端事件通道断开({:?} 源);按断连处理", srcs[i].source);
                mark_disconnected(&mut srcs[i], &mut on_status);
            }
            Woke::Tick => sink.tick(),
        }
    }

    // 停录收尾。仍挂着缺口的源走同一条恢复路径(忽略退避,只此一次):多开一条随
    // 即 finish 的流,换取"补识只有一条实现"的确定性;连不上就落占位段留痕。
    for cs in srcs.iter_mut() {
        try_recover(
            cs,
            true,
            &cloud,
            backfill_segmenter.as_mut(),
            &mut sink,
            &mut on_status,
        );
        // 始终连不上:缺口里的发声补不回来了,落一条占位段留痕(零长缺口除外)。
        if let Some(gap_from) = cs.feed.gap_from.take() {
            if cs.feed.fed > gap_from {
                push_placeholder(
                    cs.source,
                    samples_to_ms(gap_from),
                    samples_to_ms(cs.feed.fed),
                    &mut sink,
                );
                on_status(CloudAsrStatus::BackfillFailed { source: cs.source });
            }
        }
    }

    // 对活流 finish 并排干:厂商常在 finish 之后才吐最后一句。push 闭包在此丢弃,
    // 适配层的发送端随之释放,排干靠通道断开自然结束,不必空等满窗。
    let mut drains: Vec<(usize, Receiver<crate::asr::cloud::CloudEvent>)> = Vec::new();
    for (i, cs) in srcs.iter_mut().enumerate() {
        if let Some(stream) = cs.stream.take() {
            let crate::asr::cloud::CloudStream { push, finish, events } = stream;
            drop(push);
            if let Err(e) = finish() {
                eprintln!("云端流 finish 失败({:?} 源): {e}", cs.source);
            }
            drains.push((i, events));
        }
    }
    let deadline = Instant::now() + Duration::from_millis(CLOUD_DRAIN_MS);
    while !drains.is_empty() {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            eprintln!("云端排干超时,放弃剩余事件");
            break;
        };
        let woke = {
            let mut sel = crossbeam_channel::Select::new();
            for (_, rx) in drains.iter() {
                sel.recv(rx);
            }
            match sel.select_timeout(left) {
                Err(_) => None,
                Ok(oper) => {
                    let slot = oper.index();
                    Some((slot, oper.recv(&drains[slot].1)))
                }
            }
        };
        let Some((slot, ev)) = woke else { break };
        match ev {
            // 排干阶段只捞定稿:预览已无处可去,断连也无需再重连(录制已结束)。
            Ok(crate::asr::cloud::CloudEvent::Definite(u)) => {
                let cs = &mut srcs[drains[slot].0];
                push_definite(cs.source, &mut cs.feed, &u, &mut sink);
            }
            Ok(crate::asr::cloud::CloudEvent::Interim { .. }) => {}
            Ok(crate::asr::cloud::CloudEvent::Closed { .. }) | Err(_) => {
                drains.remove(slot);
            }
        }
    }

    sink.finish();
    embedder
}

/// 主循环里的一条厂商事件:预览走 push_partial(system 记账与 mic 预览回声抑制
/// 都在 sink 内部),定稿走时间映射,带错关闭进重连状态机。
fn handle_cloud_event<F, P, D, S>(
    cs: &mut CloudSource,
    event: crate::asr::cloud::CloudEvent,
    sink: &mut FinalSink<'_, F, P, D>,
    on_status: &mut S,
) where
    F: FnMut(Source, String, u64, u64, Option<String>, Option<f32>),
    P: FnMut(Source, String),
    D: FnMut(DiarEvent),
    S: FnMut(CloudAsrStatus),
{
    match event {
        crate::asr::cloud::CloudEvent::Interim { text } => sink.push_partial(cs.source, text),
        crate::asr::cloud::CloudEvent::Definite(u) => {
            push_definite(cs.source, &mut cs.feed, &u, sink)
        }
        crate::asr::cloud::CloudEvent::Closed { error: None } => {
            // 正常关闭(厂商侧收尾):不自动重开——重开是断连语义,正常关闭再开一条
            // 流只会凭空多一次握手,也不该报"重连中"惊扰 UI。
            cs.stream = None;
            // 但音频通道还开着就意味着还在录:此后喂进来的每一帧都无人识别,
            // 必须记成缺口,否则这段发声被静默吞掉(与断连同罪,只是没有告警)。
            // 缺口留给停录收尾的 force 恢复:能补则补,补不回落占位段留痕。
            if cs.audio_open {
                cs.feed.begin_gap();
            }
        }
        crate::asr::cloud::CloudEvent::Closed { error: Some(e) } => {
            eprintln!("云端连接断开({:?} 源): {e}", cs.source);
            mark_disconnected(cs, on_status);
        }
    }
}

/// 识别 worker 线程的返还物:(recognizer, embedder)。recognizer 为 Option——
/// 云端模式没有本机识别器可还(识别在厂商侧),本地模式恒 Some,调用方的 stash
/// 语义因此两边一致(stash_model 对 None 是空操作)。
type AsrJoin =
    std::thread::JoinHandle<(Option<Box<dyn Recognizer>>, Option<Box<dyn SpeakerEmbedder>>)>;

/// 一次录制会话的句柄：持两路 capture + 各 worker 的 join 句柄。
pub struct RecordingHandle {
    captures: Vec<Box<dyn AudioCapture>>,
    workers: Vec<std::thread::JoinHandle<()>>,
    /// 识别 worker 的 join 句柄(本地 / 云端同一个槽,见 AsrJoin)。
    asr: Option<AsrJoin>,
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
    /// 云端模式下 recognizer 恒为 None（识别在厂商侧,本机无模型可还）；
    /// 其收尾链完全同构:CloudForwarder 随分段 worker drop → 音频通道关闭 →
    /// 云端 worker finish 厂商流、排干末尾定稿后退出。
    pub fn stop(mut self) -> (Option<Box<dyn Recognizer>>, Option<Box<dyn SpeakerEmbedder>>) {
        for c in self.captures.iter_mut() {
            c.stop();
        }
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
        match self.asr.take() {
            Some(a) => match a.join() {
                Ok((r, e)) => (r, e),
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
/// recognizer 为 Option:云端模式压根没有本机识别器,无可返还(None)。
pub struct StartError {
    pub error: anyhow::Error,
    pub recognizer: Option<Box<dyn Recognizer>>,
    pub embedder: Option<Box<dyn SpeakerEmbedder>>,
}

impl std::fmt::Debug for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StartError({})", self.error)
    }
}

/// 本场录制的识别引擎:本机常驻识别器,或云端厂商流。
///
/// 云端所需的两件"只有调用方知道"的东西随变体一起传进来,而不是加到 start_session
/// 的参数表里——本地路的调用点(既有测试/store 测试/lib.rs)因此一字不改,云端专属
/// 的装配也不会漏进本地分支:
///  - `backfill_segmenter`:缺口补识的本机 VAD 切段器(模型路径归 lib.rs 管,
///    session 层不碰 models::root);
///  - `on_status`:重连/补识状态回调(session 层保持无 UI 依赖,emit 留给 lib.rs)。
pub enum AsrEngine {
    Local(Box<dyn Recognizer>),
    Cloud {
        asr: Arc<dyn crate::asr::cloud::CloudAsr>,
        backfill_segmenter: Box<BackfillSegmenter>,
        on_status: Box<dyn FnMut(CloudAsrStatus) + Send>,
    },
}

/// 起会话：每源一条分段 worker + 单 ASR worker，接好 finals 通道与每源 partial 槽。
/// 某源 capture 启动失败 → 跳过该源并记入 failed（用于降级）；无任何源启动 → Err。
/// audio_sinks:按源可选的音频旁路(录音保留),worker 在暂停闸后把重采样样本喂给它;
/// 未提供的源不落音频。capture 启动失败的源其 sink 随 worker 一起丢弃(惰性建档不留空文件)。
/// aec_roles:按源可选的软件回声消除角色(System=Render 参考,Mic=Capture 消回声,
/// 见 audio::aec);capture 启动失败的源其角色随 worker 一起丢弃。
/// engine:本机识别器 或 云端厂商流(见 AsrEngine)。云端分支下 sources 里带的本机
/// 分段器整段让位给 CloudForwarder——断句由厂商服务端 VAD 负责,本机只做转发,
/// run_segment_worker(AEC/暂停闸/电平/音频旁路)一行不改地复用。
#[allow(clippy::too_many_arguments)]
pub fn start_session(
    sources: Vec<(Source, Box<dyn AudioCapture>, Box<dyn Segmenter>)>,
    engine: AsrEngine,
    embedder: Option<Box<dyn SpeakerEmbedder>>,
    registry: SpeakerRegistry,
    echo_hold: Duration,
    // 语言幻觉过滤总开关，见 run_asr_worker 同名参数注释；会议场景默认开(true)，
    // 多语会议可在设置里关闭。
    language_filter: bool,
    target_rate: u32,
    partial_interval_samples: usize,
    mut audio_sinks: Vec<(Source, Box<dyn FnMut(&[f32]) + Send>)>,
    mut aec_roles: Vec<(Source, crate::audio::aec::AecRole)>,
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
    // 云端:每源一条「分段 worker → 云端 worker」的音频通道。只收录 capture 启动成功
    // 的源(与 slots 同节奏),否则会为一条根本没音频的源白开一次厂商流。
    let cloud_mode = matches!(engine, AsrEngine::Cloud { .. });
    let mut audio_rxs: Vec<(Source, Receiver<Vec<f32>>)> = Vec::new();

    for (source, mut capture, segmenter) in sources {
        let (ftx, frx) = crossbeam_channel::bounded::<AudioFrame>(256);
        // 云端:本机分段器让位给 CloudForwarder(只转发,不断句)。被替下的 silero
        // 在此 drop——多花一次构造是本地/云端共用同一段源装配代码的代价,换来 lib.rs
        // 侧的源构建(采集栈/AEC/健康统计)零分叉。
        let (segmenter, cloud_rx): (Box<dyn Segmenter>, Option<Receiver<Vec<f32>>>) = if cloud_mode {
            let (atx, arx) = crossbeam_channel::unbounded::<Vec<f32>>();
            (Box::new(crate::pipeline::cloud_forward::CloudForwarder::new(atx)), Some(arx))
        } else {
            (segmenter, None)
        };
        let slot = Arc::new(Mutex::new(None));
        let slot_for_worker = slot.clone();
        let final_tx = finals_tx.clone();
        // 先起 worker（消费者），再启动 capture：兼容同步灌帧的 MockCapture，
        // 且若 capture 启动失败，ftx 在 start 内被 drop → frx 关闭 → worker 立即退出。
        let level_cb = if source == Source::Mic { mic_level.take() } else { None };
        let audio_sink = audio_sinks
            .iter()
            .position(|(s, _)| *s == source)
            .map(|i| audio_sinks.swap_remove(i).1);
        let aec_role = aec_roles
            .iter()
            .position(|(s, _)| *s == source)
            .map(|i| aec_roles.swap_remove(i).1);
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
                audio_sink,
                aec_role,
            );
        });
        match capture.start(ftx) {
            Ok(()) => {
                active.push(source);
                slots.push((source, slot));
                if let Some(rx) = cloud_rx {
                    audio_rxs.push((source, rx));
                }
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
            recognizer: match engine {
                AsrEngine::Local(r) => Some(r),
                AsrEngine::Cloud { .. } => None,
            },
            embedder,
        });
    }

    let asr = match engine {
        AsrEngine::Local(recognizer) => std::thread::spawn(move || {
            let (r, e) = run_asr_worker(
                recognizer,
                embedder,
                registry,
                finals_rx,
                echo_hold,
                language_filter,
                slots,
                on_final,
                on_partial,
                on_diar,
            );
            (Some(r), e)
        }),
        AsrEngine::Cloud { asr, backfill_segmenter, on_status } => {
            // finals_rx / slots 在云端不参与:CloudForwarder 既不产段也不产 partial,
            // 通道就此空转(发送端随分段 worker 一起 drop)。
            drop(finals_rx);
            drop(slots);
            std::thread::spawn(move || {
                let e = run_cloud_asr_worker(
                    asr,
                    embedder,
                    registry,
                    audio_rxs,
                    echo_hold,
                    language_filter,
                    backfill_segmenter,
                    on_final,
                    on_partial,
                    on_diar,
                    on_status,
                );
                (None, e)
            })
        }
    };

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
            true, // language_filter: 既有测试语义不变(过滤开)
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
            true, // language_filter: 既有测试语义不变(过滤开)
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
            true, // language_filter: 既有测试语义不变(过滤开)
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
                true, // language_filter: 既有测试语义不变(过滤开)
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

    /// 有界轮询直到谓词为真(避免固定 sleep 假设),超时返回 false。
    fn poll_until(mut pred: impl FnMut() -> bool) -> bool {
        for _ in 0..300 {
            if pred() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        false
    }

    /// 预览级回声抑制:mic 预览与 system 在途预览同文本 → 压成空串(UI 不出现
    /// 「我」「对方」同字齐蹦);不相似的 mic 预览原样透传。
    #[test]
    fn mic_partial_echoing_system_partial_is_suppressed_in_preview() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        let sys_slot = Arc::new(Mutex::new(Some(PartialJob { source: Source::System, samples: vec![0.0; 7] })));
        let mic_slot = Arc::new(Mutex::new(None::<PartialJob>));
        let partials = Arc::new(Mutex::new(Vec::<(Source, String)>::new()));
        let (p2, sys2, mic2) = (partials.clone(), sys_slot.clone(), mic_slot.clone());

        let worker = std::thread::spawn(move || {
            let _ = run_asr_worker(
                Box::new(CountingRecognizer),
                None,
                SpeakerRegistry::new(),
                rx,
                TEST_ECHO_HOLD,
                true,
                vec![(Source::System, sys2), (Source::Mic, mic2)],
                |_, _, _, _, _, _| {},
                move |s, t| p2.lock().unwrap().push((s, t)),
                |_| {},
            );
        });

        // 1) system 预览先被服务,in-flight 文本 = "len=7"。
        assert!(
            poll_until(|| partials.lock().unwrap().contains(&(Source::System, "len=7".into()))),
            "system 预览应被服务"
        );
        // 2) mic 预览同文本("len=7")→ 判回声,压成空串。
        *mic_slot.lock().unwrap() = Some(PartialJob { source: Source::Mic, samples: vec![0.0; 7] });
        assert!(
            poll_until(|| partials.lock().unwrap().contains(&(Source::Mic, String::new()))),
            "同文本 mic 预览应被压成空串"
        );
        assert!(
            !partials.lock().unwrap().contains(&(Source::Mic, "len=7".into())),
            "被抑制的 mic 预览文本不得透出"
        );
        // 3) mic 预览不相似("len=1234" vs "len=7",编辑距离分 0.5 < 0.6)→ 原样透传。
        *mic_slot.lock().unwrap() = Some(PartialJob { source: Source::Mic, samples: vec![0.0; 1234] });
        assert!(
            poll_until(|| partials.lock().unwrap().contains(&(Source::Mic, "len=1234".into()))),
            "不相似的 mic 预览应原样透传"
        );

        drop(tx);
        worker.join().unwrap();
    }

    /// 追溯回声撤回:mic 段 hold 到期先放行(on_final 已发出),随后 system 同句定稿
    /// → 应发 EchoRetract(带被撤段的时间戳与文本),供调用方从落盘/UI 移除。
    #[test]
    fn late_system_final_retracts_already_emitted_mic_echo() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        let finals = Arc::new(Mutex::new(Vec::<(Source, String)>::new()));
        let diar = Arc::new(Mutex::new(Vec::<DiarEvent>::new()));
        let (f2, d2) = (finals.clone(), diar.clone());

        let worker = std::thread::spawn(move || {
            let _ = run_asr_worker(
                Box::new(CountingRecognizer),
                None,
                SpeakerRegistry::new(),
                rx,
                TEST_ECHO_HOLD, // 50ms:mic 段快速到期放行,制造"先放行后定稿"时序
                true,
                vec![],
                move |s, t, _, _, _, _| f2.lock().unwrap().push((s, t)),
                |_, _| {},
                move |ev| d2.lock().unwrap().push(ev),
            );
        });

        // mic 回声段先到,hold 到期(50ms)放行上屏。
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.1; 4000], start_ms: 0, end_ms: 250 })
            .unwrap();
        assert!(
            poll_until(|| finals.lock().unwrap().contains(&(Source::Mic, "len=4000".into()))),
            "mic 段应先被放行"
        );
        // system 同句(同文本、时间邻近)晚定稿 → 追溯撤回。
        tx.send(FinalJob { source: Source::System, samples: vec![0.1; 4000], start_ms: 0, end_ms: 250 })
            .unwrap();
        assert!(
            poll_until(|| {
                diar.lock().unwrap().iter().any(|e| matches!(
                    e,
                    DiarEvent::EchoRetract { start_ms: 0, end_ms: 250, text } if text == "len=4000"
                ))
            }),
            "system 定稿后应追溯撤回已放行的 mic 回声段"
        );

        drop(tx);
        worker.join().unwrap();
    }

    /// 自适应 hold:system 侧有在途预览时,mic 段 hold 延长(×ECHO_HOLD_EXTEND_FACTOR),
    /// 等到 system 同句定稿仍在 pending 中被击杀——mic 回声段从头到尾不上屏。
    #[test]
    fn pending_mic_hold_extends_while_system_partial_in_flight() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        let sys_slot = Arc::new(Mutex::new(Some(PartialJob { source: Source::System, samples: vec![0.0; 7] })));
        let finals = Arc::new(Mutex::new(Vec::<(Source, String)>::new()));
        let partials = Arc::new(Mutex::new(Vec::<(Source, String)>::new()));
        let (f2, p2, sys2) = (finals.clone(), partials.clone(), sys_slot.clone());

        let worker = std::thread::spawn(move || {
            let _ = run_asr_worker(
                Box::new(CountingRecognizer),
                None,
                SpeakerRegistry::new(),
                rx,
                Duration::from_millis(100), // 普通 hold 100ms,延长后 600ms
                true,
                vec![(Source::System, sys2)],
                move |s, t, _, _, _, _| f2.lock().unwrap().push((s, t)),
                move |s, t| p2.lock().unwrap().push((s, t)),
                |_| {},
            );
        });

        // 1) system 预览被服务 → last_system_partial = "len=7"(在途语句标志)。
        assert!(
            poll_until(|| partials.lock().unwrap().contains(&(Source::System, "len=7".into()))),
            "system 预览应被服务"
        );
        // 2) mic 同文本段进 hold;普通 hold(100ms)过后远未到延长档(600ms),不得放行。
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.0; 7], start_ms: 0, end_ms: 250 })
            .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !finals.lock().unwrap().iter().any(|(s, _)| *s == Source::Mic),
            "system 在途语句期间 mic 段不应放行(hold 已延长)"
        );
        // 3) system 同句定稿 → pending 中的 mic 回声被击杀,永不上屏。
        tx.send(FinalJob { source: Source::System, samples: vec![0.0; 7], start_ms: 0, end_ms: 250 })
            .unwrap();
        assert!(
            poll_until(|| finals.lock().unwrap().contains(&(Source::System, "len=7".into()))),
            "system 段应定稿"
        );
        drop(tx);
        worker.join().unwrap();
        assert!(
            !finals.lock().unwrap().iter().any(|(s, _)| *s == Source::Mic),
            "mic 回声段应在 pending 中被击杀,从未上屏"
        );
    }

    /// 预览级回声抑制的第二判据:system 句已定稿(进 recent_system,在途预览已清)后,
    /// mic 路仍在"跟读"同句 → 预览同样被压掉。
    #[test]
    fn mic_partial_echoing_recent_system_final_is_suppressed_in_preview() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        let mic_slot = Arc::new(Mutex::new(None::<PartialJob>));
        let partials = Arc::new(Mutex::new(Vec::<(Source, String)>::new()));
        let finals = Arc::new(Mutex::new(Vec::<String>::new()));
        let (p2, f2, mic2) = (partials.clone(), finals.clone(), mic_slot.clone());

        let worker = std::thread::spawn(move || {
            let _ = run_asr_worker(
                Box::new(CountingRecognizer),
                None,
                SpeakerRegistry::new(),
                rx,
                TEST_ECHO_HOLD,
                true,
                vec![(Source::Mic, mic2)],
                move |_, t, _, _, _, _| f2.lock().unwrap().push(t),
                move |s, t| p2.lock().unwrap().push((s, t)),
                |_| {},
            );
        });

        // system 段定稿 → 文本 "len=4000" 进 recent_system。
        tx.send(FinalJob { source: Source::System, samples: vec![0.1; 4000], start_ms: 0, end_ms: 250 })
            .unwrap();
        assert!(
            poll_until(|| finals.lock().unwrap().contains(&"len=4000".to_string())),
            "system 段应先定稿"
        );
        // mic 预览同文本 → 被 recent_system 判据压掉。
        *mic_slot.lock().unwrap() = Some(PartialJob { source: Source::Mic, samples: vec![0.0; 4000] });
        assert!(
            poll_until(|| partials.lock().unwrap().contains(&(Source::Mic, String::new()))),
            "与最近 system 定稿同文本的 mic 预览应被压掉"
        );
        assert!(
            !partials.lock().unwrap().contains(&(Source::Mic, "len=4000".into())),
            "被抑制的 mic 预览文本不得透出"
        );

        drop(tx);
        worker.join().unwrap();
    }

    #[test]
    fn finals_get_speaker_labels_and_diar_events() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        // 两段长音频:第一段 → S1;第二段正交向量 → S2。
        // 两段文本(均由 CountingRecognizer 按长度生成)恰好相似("len=32000" 相同)，
        // 时间戳特意拉开(> ECHO_WINDOW_MS 且不交叠)以隔离本用例(测说话人聚类)与
        // 回声去重逻辑,避免被误判丢弃。
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.1; 44800], start_ms: 0, end_ms: 2800 }).unwrap();
        tx.send(FinalJob { source: Source::System, samples: vec![0.1; 44800], start_ms: 10000, end_ms: 12800 }).unwrap();
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
            true, // language_filter: 既有测试语义不变(过滤开)
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
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.1; 44800], start_ms: 0, end_ms: 2800 }).unwrap();
        tx.send(FinalJob { source: Source::System, samples: vec![0.1; 44800], start_ms: 10000, end_ms: 12800 }).unwrap();
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
            true, // language_filter: 既有测试语义不变(过滤开)
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
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.1; 44800], start_ms: 0, end_ms: 2800 }).unwrap();
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
            true, // language_filter: 既有测试语义不变(过滤开)
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
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.1; 44800], start_ms: 0, end_ms: 2800 }).unwrap();
        drop(tx);
        let finals = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
        let f2 = finals.clone();
        let (_r, e) = run_asr_worker(
            Box::new(CountingRecognizer),
            None,
            SpeakerRegistry::new(),
            rx,
            TEST_ECHO_HOLD,
            true, // language_filter: 既有测试语义不变(过滤开)
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
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.1; 44800], start_ms: 0, end_ms: 2800 }).unwrap();
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
            true, // language_filter: 既有测试语义不变(过滤开)
            vec![],
            |_, _, _, _, _, _| {},
            |_, _| {},
            move |ev| e2.lock().unwrap().push(ev),
        );
        let evs = events.lock().unwrap();
        let snapshot_count = evs.iter().filter(|e| matches!(e, DiarEvent::Snapshot { .. })).count();
        assert_eq!(snapshot_count, 1, "worker 结束时应恰发一次 Snapshot");
        assert!(matches!(evs.last().unwrap(), DiarEvent::Snapshot { .. }), "Snapshot 应在末尾(既有 diar 事件之后)");
        match evs.last().unwrap() {
            DiarEvent::Snapshot { snaps, samples } => {
                assert_eq!(snaps.len(), 1);
                assert_eq!(snaps[0].id, "S1");
                // 样本随 Snapshot 导出:该簇唯一一段(44800 样本,超 15s 上限则截断)。
                assert_eq!(samples.len(), 1);
                assert_eq!(samples[0].0, "S1");
                assert_eq!(samples[0].1.len(), 44800.min(super::SPEAKER_SAMPLE_CAP));
            }
            _ => unreachable!(),
        }
    }

    /// 样本上限真实生效:送一段超过 SPEAKER_SAMPLE_CAP 的 final(15.625s),
    /// Snapshot 导出的样本必须恰被截断到上限——防止 worker 内存随长独白无界增长。
    /// (MockEmbedder 脚本耗尽后重复最后一个向量:滑窗嵌入全相同 → 无变更点 → 整段。)
    #[test]
    fn snapshot_sample_truncated_to_cap_for_long_segment() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        let n = SPEAKER_SAMPLE_CAP + 10_000;
        tx.send(FinalJob {
            source: Source::Mic,
            samples: vec![0.1; n],
            start_ms: 0,
            end_ms: (n / 16) as u64,
        })
        .unwrap();
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
            true, // language_filter: 既有测试语义不变(过滤开)
            vec![],
            |_, _, _, _, _, _| {},
            |_, _| {},
            move |ev| e2.lock().unwrap().push(ev),
        );
        let evs = events.lock().unwrap();
        let DiarEvent::Snapshot { samples, .. } = evs.last().unwrap() else {
            panic!("末事件应为 Snapshot");
        };
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].1.len(), SPEAKER_SAMPLE_CAP, "超长段样本截断到上限");
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
            true, // language_filter: 既有测试语义不变(过滤开)
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
            true, // language_filter: 既有测试语义不变(过滤开)
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
            true, // language_filter: 既有测试语义不变(过滤开)
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
            true, // language_filter: 既有测试语义不变(过滤开)
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
            true, // language_filter: 既有测试语义不变(过滤开)
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
                true, // language_filter: 既有测试语义不变(过滤开)
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
            Transcript { text: "でかし".into(), lang: "<|ja|>".into(), ..Default::default() },
            Transcript { text: "正常句子".into(), lang: "<|zh|>".into(), ..Default::default() },
        ];
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.5; 1600], start_ms: 0, end_ms: 100 }).unwrap();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.5; 1600], start_ms: 200, end_ms: 300 }).unwrap();
        drop(tx);
        let mut finals: Vec<(String, Option<f32>)> = Vec::new();
        let mut suppressed = Vec::new();
        run_asr_worker(
            Box::new(ScriptRecognizer(script.into())),
            None,
            SpeakerRegistry::new(),
            rx,
            Duration::from_millis(0), // hold 归零,立即 release
            true, // language_filter: 既有测试语义不变(过滤开)
            Vec::new(),
            |_src, text, _s, _e, _spk, rms| finals.push((text, rms)),
            |_, _| {},
            |event| {
                if let DiarEvent::SuppressedFinal { text, reason, .. } = event {
                    suppressed.push((text, reason));
                }
            },
        );
        assert_eq!(finals.len(), 1, "日语幻觉段被丢弃");
        assert_eq!(finals[0].0, "正常句子");
        let rms = finals[0].1.expect("正常段必须带 rms");
        assert!((rms - 0.5).abs() < 1e-3, "全 0.5 样本的 RMS 应为 0.5,得 {rms}");
        assert_eq!(suppressed, vec![("でかし".into(), "foreign_language".into())]);
    }

    /// language_filter=false:多语会议场景显式关闭本过滤后,即便命中中日韩白名单
    /// 判定,也不应丢弃——应与"未过滤"路径行为一致,两段都正常落 final。
    #[test]
    fn worker_language_filter_disabled_keeps_foreign_final() {
        // ScriptRecognizer: 第一条日语标签(若开关生效本应被丢弃),第二条正常中文。
        struct ScriptRecognizer(std::collections::VecDeque<Transcript>);
        impl Recognizer for ScriptRecognizer {
            fn recognize(&mut self, _s: &[f32]) -> anyhow::Result<Transcript> {
                Ok(self.0.pop_front().unwrap_or_default())
            }
        }
        let script = vec![
            Transcript { text: "でかし".into(), lang: "<|ja|>".into(), ..Default::default() },
            Transcript { text: "正常句子".into(), lang: "<|zh|>".into(), ..Default::default() },
        ];
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.5; 1600], start_ms: 0, end_ms: 100 }).unwrap();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.5; 1600], start_ms: 200, end_ms: 300 }).unwrap();
        drop(tx);
        let mut finals: Vec<String> = Vec::new();
        run_asr_worker(
            Box::new(ScriptRecognizer(script.into())),
            None,
            SpeakerRegistry::new(),
            rx,
            Duration::from_millis(0), // hold 归零,立即 release
            false, // language_filter 关:日语标签段也应正常落 final,不被丢弃
            Vec::new(),
            |_src, text, _s, _e, _spk, _rms| finals.push(text),
            |_, _| {},
            |_| {},
        );
        assert_eq!(finals.len(), 2, "关闭语言过滤后两段都应正常落 final,不丢日语段: {finals:?}");
        assert_eq!(finals[0], "でかし");
        assert_eq!(finals[1], "正常句子");
    }

    // ---- AEC 残渣抑制(冒烟反馈):能量+重叠双条件,与文本回声去重两个检查点镜像 ----

    /// 检查点(a):mic 段到达时对照 recent_system——system 先到、已入 recent_system,
    /// 随后到达的 mic 段幅度低(rms 低)且与该 system 段 90% 重叠 → 判定残渣丢弃。
    #[test]
    fn aec_residue_dropped_when_rms_low_and_overlap_high() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        // system: 100..3000ms;mic: 0..1000ms → overlap_fraction(mic,system) = 900/1000 = 0.9。
        tx.send(FinalJob { source: Source::System, samples: vec![0.2; 100], start_ms: 100, end_ms: 3000 })
            .unwrap();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.005; 100], start_ms: 0, end_ms: 1000 })
            .unwrap();
        drop(tx);

        let recognizer = ScriptedRecognizer::new(&["system speech here", "残渣文本大。"]);
        let finals = Arc::new(Mutex::new(Vec::<(Source, String)>::new()));
        let f2 = finals.clone();
        let _ = run_asr_worker(
            Box::new(recognizer),
            None,
            SpeakerRegistry::new(),
            rx,
            TEST_ECHO_HOLD,
            true, // language_filter: 既有测试语义不变(过滤开)
            vec![],
            move |s, t, _, _, _, _| f2.lock().unwrap().push((s, t)),
            |_, _| {},
            |_| {},
        );
        assert_eq!(
            *finals.lock().unwrap(),
            vec![(Source::System, "system speech here".to_string())],
            "rms 低且与 system 段高度重叠的 mic 段应被判定为 AEC 残渣丢弃"
        );
    }

    /// 同样 90% 重叠,但 mic 段幅度高(rms 高,近场真人声典型值)→ 不应误杀,应保留。
    #[test]
    fn aec_residue_kept_when_overlap_high_but_rms_high() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        tx.send(FinalJob { source: Source::System, samples: vec![0.2; 100], start_ms: 100, end_ms: 3000 })
            .unwrap();
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.1; 100], start_ms: 0, end_ms: 1000 }).unwrap();
        drop(tx);

        let recognizer = ScriptedRecognizer::new(&["system speech here", "真人插话内容"]);
        let finals = Arc::new(Mutex::new(Vec::<(Source, String)>::new()));
        let f2 = finals.clone();
        let _ = run_asr_worker(
            Box::new(recognizer),
            None,
            SpeakerRegistry::new(),
            rx,
            TEST_ECHO_HOLD,
            true, // language_filter: 既有测试语义不变(过滤开)
            vec![],
            move |s, t, _, _, _, _| f2.lock().unwrap().push((s, t)),
            |_, _| {},
            |_| {},
        );
        let got = finals.lock().unwrap().clone();
        assert_eq!(got.len(), 2, "rms 高的真人插话不应被残渣抑制误杀: {got:?}");
        assert!(got.contains(&(Source::System, "system speech here".to_string())));
        assert!(got.contains(&(Source::Mic, "真人插话内容".to_string())));
    }

    /// rms 低但与 system 段无时间重叠 → 不应误杀,应保留。
    #[test]
    fn aec_residue_kept_when_rms_low_but_no_overlap() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        tx.send(FinalJob { source: Source::System, samples: vec![0.2; 100], start_ms: 100, end_ms: 3000 })
            .unwrap();
        // mic 段远早于 system 段,零重叠。
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.005; 100], start_ms: 90_000, end_ms: 91_000 })
            .unwrap();
        drop(tx);

        let recognizer = ScriptedRecognizer::new(&["system speech here", "远处安静片段"]);
        let finals = Arc::new(Mutex::new(Vec::<(Source, String)>::new()));
        let f2 = finals.clone();
        let _ = run_asr_worker(
            Box::new(recognizer),
            None,
            SpeakerRegistry::new(),
            rx,
            TEST_ECHO_HOLD,
            true, // language_filter: 既有测试语义不变(过滤开)
            vec![],
            move |s, t, _, _, _, _| f2.lock().unwrap().push((s, t)),
            |_, _| {},
            |_| {},
        );
        let got = finals.lock().unwrap().clone();
        assert_eq!(got.len(), 2, "无时间重叠的低 rms 段不应被残渣抑制误杀: {got:?}");
        assert!(got.contains(&(Source::System, "system speech here".to_string())));
        assert!(got.contains(&(Source::Mic, "远处安静片段".to_string())));
    }

    /// 检查点(b):mic 段先到、进 pending_mic hold 中,随后到达的 system 段与其 90% 重叠
    /// 且 mic rms 低 → retain 闭包内判定残渣丢弃(mic 段不会走到 release 那一步)。
    #[test]
    fn aec_residue_dropped_via_pending_mic_when_system_arrives_later() {
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        // mic: 0..1000ms,先到,rms 低;system: 100..3000ms,后到,与 mic 重叠 90%。
        tx.send(FinalJob { source: Source::Mic, samples: vec![0.005; 100], start_ms: 0, end_ms: 1000 }).unwrap();
        tx.send(FinalJob { source: Source::System, samples: vec![0.2; 100], start_ms: 100, end_ms: 3000 })
            .unwrap();
        drop(tx);

        let recognizer = ScriptedRecognizer::new(&["残渣文本大。", "system speech here"]);
        let finals = Arc::new(Mutex::new(Vec::<(Source, String)>::new()));
        let f2 = finals.clone();
        let _ = run_asr_worker(
            Box::new(recognizer),
            None,
            SpeakerRegistry::new(),
            rx,
            TEST_ECHO_HOLD,
            true, // language_filter: 既有测试语义不变(过滤开)
            vec![],
            move |s, t, _, _, _, _| f2.lock().unwrap().push((s, t)),
            |_, _| {},
            |_| {},
        );
        assert_eq!(
            *finals.lock().unwrap(),
            vec![(Source::System, "system speech here".to_string())],
            "pending 中 rms 低的 mic 段应在 system 到达时经 retain 闭包判定残渣丢弃"
        );
    }

    // ---- 段内说话人分离(Task 3):滑窗嵌入 → 变更点 → 切子段,各自走既有链 ----

    /// 双说话人混说段被切成两个 final,各自说话人;单说话人段不乱切。
    #[test]
    fn worker_splits_mixed_segment_into_two_finals() {
        // ContentEmbedder: 按窗样本均值返回 e1(<0.5) / e2(≥0.5)——前半 0.1、后半 0.9
        // 的 8s 段,滑窗序列前半 e1 后半 e2,应检出 1 个变更点。
        struct ContentEmbedder;
        impl SpeakerEmbedder for ContentEmbedder {
            fn embed(&mut self, s: &[f32]) -> anyhow::Result<Vec<f32>> {
                let mean = s.iter().sum::<f32>() / s.len() as f32;
                Ok(if mean < 0.5 { vec![1.0, 0.0, 0.0] } else { vec![0.0, 1.0, 0.0] })
            }
        }
        // TimedRecognizer: 8 个 token,时间戳均匀分布 0..8s,文本 t0..t7
        struct TimedRecognizer;
        impl Recognizer for TimedRecognizer {
            fn recognize(&mut self, _s: &[f32]) -> anyhow::Result<Transcript> {
                Ok(Transcript {
                    text: "t0t1t2t3t4t5t6t7".into(),
                    tokens: (0..8).map(|i| format!("t{i}")).collect(),
                    timestamps: (0..8).map(|i| i as f32).collect(),
                    ..Default::default()
                })
            }
        }
        let mut samples = vec![0.1f32; 4 * 16000];
        samples.extend(vec![0.9f32; 4 * 16000]);
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        tx.send(FinalJob { source: Source::System, samples, start_ms: 0, end_ms: 8000 }).unwrap();
        drop(tx);
        let mut finals: Vec<(String, u64, u64, Option<String>)> = Vec::new();
        run_asr_worker(
            Box::new(TimedRecognizer),
            Some(Box::new(ContentEmbedder)),
            SpeakerRegistry::new(),
            rx,
            Duration::from_millis(0),
            true, // language_filter: 既有测试语义不变(过滤开)
            Vec::new(),
            |_src, text, s, e, spk, _rms| finals.push((text, s, e, spk)),
            |_, _| {},
            |_| {},
        );
        assert_eq!(finals.len(), 2, "混说段应切成两个 final: {finals:?}");
        assert!(finals[0].3 != finals[1].3, "两子段说话人应不同");
        assert_eq!(finals[0].1, 0);
        assert_eq!(finals[1].2, 8000, "时间轴首尾衔接母段");
        assert!(finals[0].2 == finals[1].1, "子段边界无缝");
        assert_eq!(format!("{}{}", finals[0].0, finals[1].0), "t0t1t2t3t4t5t6t7", "文本无损");
    }

    /// 单说话人长段:嵌入恒同 → 不切,单 final(现状不回归)。
    #[test]
    fn worker_keeps_uniform_segment_whole() {
        struct ContentEmbedder;
        impl SpeakerEmbedder for ContentEmbedder {
            fn embed(&mut self, s: &[f32]) -> anyhow::Result<Vec<f32>> {
                let mean = s.iter().sum::<f32>() / s.len() as f32;
                Ok(if mean < 0.5 { vec![1.0, 0.0, 0.0] } else { vec![0.0, 1.0, 0.0] })
            }
        }
        struct TimedRecognizer;
        impl Recognizer for TimedRecognizer {
            fn recognize(&mut self, _s: &[f32]) -> anyhow::Result<Transcript> {
                Ok(Transcript {
                    text: "t0t1t2t3t4t5t6t7".into(),
                    tokens: (0..8).map(|i| format!("t{i}")).collect(),
                    timestamps: (0..8).map(|i| i as f32).collect(),
                    ..Default::default()
                })
            }
        }
        // 全段样本恒为 0.1 → 每窗嵌入均为 e1,detect_change_points 无变更点。
        let samples = vec![0.1f32; 8 * 16000];
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        tx.send(FinalJob { source: Source::System, samples, start_ms: 0, end_ms: 8000 }).unwrap();
        drop(tx);
        let mut finals: Vec<(String, u64, u64, Option<String>)> = Vec::new();
        run_asr_worker(
            Box::new(TimedRecognizer),
            Some(Box::new(ContentEmbedder)),
            SpeakerRegistry::new(),
            rx,
            Duration::from_millis(0),
            true, // language_filter: 既有测试语义不变(过滤开)
            Vec::new(),
            |_src, text, s, e, spk, _rms| finals.push((text, s, e, spk)),
            |_, _| {},
            |_| {},
        );
        assert_eq!(finals.len(), 1, "单说话人段不应被乱切: {finals:?}");
        assert_eq!(finals[0].0, "t0t1t2t3t4t5t6t7");
        assert_eq!((finals[0].1, finals[0].2), (0, 8000));
    }

    /// 时间戳缺失时，说话人切分不得触发子段重识别覆盖母段文本。
    #[test]
    fn worker_split_without_timestamps_preserves_original_transcript() {
        struct ContentEmbedder;
        impl SpeakerEmbedder for ContentEmbedder {
            fn embed(&mut self, s: &[f32]) -> anyhow::Result<Vec<f32>> {
                let mean = s.iter().sum::<f32>() / s.len() as f32;
                Ok(if mean < 0.5 { vec![1.0, 0.0, 0.0] } else { vec![0.0, 1.0, 0.0] })
            }
        }
        // 第 1 次 recognize：整段初次识别，故意不带 tokens/timestamps → group_tokens
        // 返回 None，split_final 走子段重识别回退。lang 标为中文（整段判定放行）。
        // 第 2/3 次：两个子段各自的重识别结果——第一段正常中文，第二段纯假名幻觉。
        struct ScriptedFallbackRecognizer {
            calls: usize,
        }
        impl Recognizer for ScriptedFallbackRecognizer {
            fn recognize(&mut self, _s: &[f32]) -> anyhow::Result<Transcript> {
                self.calls += 1;
                Ok(match self.calls {
                    1 => Transcript {
                        text: "占位母段文本".into(),
                        lang: "<|zh|>".into(),
                        ..Default::default()
                    },
                    2 => Transcript { text: "第一部分内容".into(), ..Default::default() },
                    _ => {
                        Transcript { text: "でかしでかしでかしでかしでかし".into(), ..Default::default() }
                    }
                })
            }
        }

        let mut samples = vec![0.1f32; 4 * 16000];
        samples.extend(vec![0.9f32; 4 * 16000]);
        let (tx, rx) = crossbeam_channel::unbounded::<FinalJob>();
        tx.send(FinalJob { source: Source::System, samples, start_ms: 0, end_ms: 8000 }).unwrap();
        drop(tx);
        let mut finals: Vec<(String, u64, u64, Option<String>)> = Vec::new();
        run_asr_worker(
            Box::new(ScriptedFallbackRecognizer { calls: 0 }),
            Some(Box::new(ContentEmbedder)),
            SpeakerRegistry::new(),
            rx,
            Duration::from_millis(0),
            true, // language_filter: 既有测试语义不变(过滤开)
            Vec::new(),
            |_src, text, s, e, spk, _rms| finals.push((text, s, e, spk)),
            |_, _| {},
            |_| {},
        );
        assert_eq!(finals.len(), 1, "无词级时间戳时应保留母段: {finals:?}");
        assert_eq!(finals[0].0, "占位母段文本");
        assert_eq!((finals[0].1, finals[0].2), (0, 8000));
    }

    /// 回归终审 Finding：末子段边界按 ms_to_sample_idx(total_ms) 换算会因 ms 记账
    /// 与实际样本数不能整除而丢掉 <1ms 的尾部样本（真实采集里 ms 时长与样本数
    /// 并非总能整除，是常见的记账误差来源，非本模块内部计算引入）。修复后末子段
    /// 直接取到 job_samples 末尾，样本总量与母段完全一致。
    #[test]
    fn split_final_last_subsegment_keeps_full_sample_tail() {
        struct ContentEmbedder;
        impl SpeakerEmbedder for ContentEmbedder {
            fn embed(&mut self, s: &[f32]) -> anyhow::Result<Vec<f32>> {
                let mean = s.iter().sum::<f32>() / s.len() as f32;
                Ok(if mean < 0.5 { vec![1.0, 0.0, 0.0] } else { vec![0.0, 1.0, 0.0] })
            }
        }
        // 名义时长 8000ms 对应 128000 样本，实际样本多出 7 个(<1ms 的尾巴)。
        let mut samples = vec![0.1f32; 4 * 16000];
        samples.extend(vec![0.9f32; 4 * 16000 + 7]);
        let total_len = samples.len();
        let transcript = Transcript {
            text: "t0t1t2t3t4t5t6t7".into(),
            tokens: (0..8).map(|i| format!("t{i}")).collect(),
            timestamps: (0..8).map(|i| i as f32).collect(),
            ..Default::default()
        };
        let mut embedder: Option<Box<dyn SpeakerEmbedder>> = Some(Box::new(ContentEmbedder));
        let subs = split_final(samples, 0, 8000, &transcript, &mut embedder, true);
        assert_eq!(subs.len(), 2, "应切成两个子段");
        let total_sub_samples: usize = subs.iter().map(|s| s.samples.len()).sum();
        assert_eq!(total_sub_samples, total_len, "子段样本总长应等于母段样本总长,不丢尾部样本");
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
            AsrEngine::Local(Box::new(ContentDigestRecognizer)),
            None,
            SpeakerRegistry::new(),
            TEST_ECHO_HOLD,
            true, // language_filter: 既有测试语义不变(过滤开)
            16000,
            4000,
            vec![],
            vec![],
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

    /// 音频保留接线:sink 按源路由,写出的 WAV 与段时间轴按构造对齐——
    /// wav 样本数 == 全部 final 的样本数之和(CountingRecognizer 文本即段长),
    /// 且 == 最大 end_ms 对应的样本数(MockSegmenter 不留尾巴时)。
    #[test]
    fn audio_sinks_route_per_source_and_wav_aligns_with_segments() {
        use crate::store::audio::AudioTrackWriter;
        let tmp = tempfile::tempdir().unwrap();
        let mut track = AudioTrackWriter::new(tmp.path(), "mic", 0);
        let sinks: Vec<(Source, Box<dyn FnMut(&[f32]) + Send>)> =
            vec![(Source::Mic, Box::new(move |s: &[f32]| track.append(s)))];

        let finals = Arc::new(Mutex::new(Vec::<(String, u64)>::new()));
        let f2 = finals.clone();
        let sources: Vec<(Source, Box<dyn AudioCapture>, Box<dyn Segmenter>)> = vec![(
            Source::Mic,
            Box::new(IdlingCapture::from_fixture()),
            Box::new(MockSegmenter::new(2000)),
        )];
        let start = start_session(
            sources,
            AsrEngine::Local(Box::new(CountingRecognizer)),
            None,
            SpeakerRegistry::new(),
            TEST_ECHO_HOLD,
            true, // language_filter: 既有测试语义不变(过滤开)
            16000,
            4000,
            sinks,
            vec![],
            move |_, t, _, end_ms, _, _| f2.lock().unwrap().push((t, end_ms)),
            |_, _| {},
            |_| {},
            None,
        )
        .expect("start_session");
        // 等 fixture 全部产出(有界轮询):fixture 417ms,至少一个 final。
        for _ in 0..300 {
            if !finals.lock().unwrap().is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = start.handle.stop(); // 停止排干尾段;sink 随 worker 退出 Drop 收尾

        let g = finals.lock().unwrap();
        assert!(!g.is_empty());
        let total_final_samples: usize =
            g.iter().map(|(t, _)| t.strip_prefix("len=").unwrap().parse::<usize>().unwrap()).sum();
        let mut r = hound::WavReader::open(tmp.path().join("mic.wav")).expect("mic.wav 应存在");
        let wav_samples = r.samples::<i16>().count();
        assert_eq!(wav_samples, total_final_samples, "wav 样本数 == 各 final 样本数之和");
        let max_end_ms = g.iter().map(|(_, e)| *e).max().unwrap();
        // end_ms 由样本数换算毫秒时向下取整,允许 <1ms(16 样本)的舍入差。
        let diff = wav_samples as u64 - max_end_ms * 16;
        assert!(diff < 16, "最大 end_ms 指向 wav 末尾(容忍 <1ms 取整): diff={diff}");
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
            AsrEngine::Local(Box::new(CountingRecognizer)),
            None,
            SpeakerRegistry::new(),
            TEST_ECHO_HOLD,
            true, // language_filter: 既有测试语义不变(过滤开)
            16000,
            4000,
            vec![],
            vec![],
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
            AsrEngine::Local(Box::new(CountingRecognizer)),
            None,
            SpeakerRegistry::new(),
            TEST_ECHO_HOLD,
            true, // language_filter: 既有测试语义不变(过滤开)
            16000,
            4000,
            vec![],
            vec![],
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
        // Err 携带 recognizer 返还(本地引擎恒 Some)
        let _reusable: Box<dyn Recognizer> = err.recognizer.expect("本地模式应返还识别器");
    }

    /// 云端装配冒烟:AsrEngine::Cloud 下,源自带的本机分段器让位给 CloudForwarder,
    /// 采集帧一路进厂商流(pushed_samples > 0),厂商定稿经同一条 FinalSink 出到
    /// on_final;停录返还 recognizer=None(云端无本机识别器)、embedder 照常返还。
    #[test]
    fn cloud_engine_forwards_audio_and_emits_vendor_finals() {
        use crate::asr::cloud::{CloudEvent, DefiniteUtterance, MockCloudAsr};
        use crate::diar::MockEmbedder;

        let mock = MockCloudAsr::new(
            vec![vec![CloudEvent::Definite(DefiniteUtterance {
                text: "云端定稿".into(),
                start_ms: 0,
                end_ms: 500,
                words: vec![],
                lang: String::new(),
            })]],
            vec![],
        );
        let pushed = mock.pushed_samples.clone();

        let finals = Arc::new(Mutex::new(Vec::<(Source, String)>::new()));
        let f2 = finals.clone();
        let sources: Vec<(Source, Box<dyn AudioCapture>, Box<dyn Segmenter>)> = vec![(
            Source::Mic,
            Box::new(IdlingCapture::from_fixture()),
            // 这只分段器在云端分支被 CloudForwarder 顶替,断句归厂商。
            Box::new(MockSegmenter::new(2000)),
        )];
        let start = start_session(
            sources,
            AsrEngine::Cloud {
                asr: Arc::new(mock),
                // 本用例不制造断连,补识桩不会被调用。
                backfill_segmenter: Box::new(|gap: &[f32]| vec![(0u64, gap.to_vec())]),
                on_status: Box::new(|_| {}),
            },
            Some(Box::new(MockEmbedder::new(vec![])) as Box<dyn SpeakerEmbedder>),
            SpeakerRegistry::new(),
            TEST_ECHO_HOLD,
            false, // language_filter:与本用例无关,关掉隔离变量
            16000,
            4000,
            vec![],
            vec![],
            move |s, t, _, _, _, _| f2.lock().unwrap().push((s, t)),
            |_, _| {},
            |_| {},
            None,
        )
        .expect("start_session");
        assert_eq!(start.active, vec![Source::Mic]);

        // 有界轮询：等采集帧真的经 CloudForwarder 推进厂商流。
        let mut fed = false;
        for _ in 0..300 {
            if *pushed.lock().unwrap() > 0 {
                fed = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let (r, e) = start.handle.stop();
        assert!(fed, "采集帧应经 CloudForwarder 推进厂商流");
        assert!(r.is_none(), "云端模式没有本机识别器可返还");
        assert!(e.is_some(), "声纹嵌入器照常返还复用");
        let g = finals.lock().unwrap();
        assert!(
            g.iter().any(|(s, t)| *s == Source::Mic && t == "云端定稿"),
            "厂商定稿应经同一条 FinalSink 出到 on_final: {g:?}"
        );
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

    #[test]
    fn overlap_fraction_basic_cases() {
        assert_eq!(overlap_fraction(0, 1000, 100, 3000), 0.9, "90% 重叠");
        assert_eq!(overlap_fraction(0, 1000, 2000, 3000), 0.0, "无重叠");
        assert_eq!(overlap_fraction(0, 1000, 0, 1000), 1.0, "完全重叠");
        assert_eq!(overlap_fraction(5, 5, 0, 100), 0.0, "a 时长为 0,防御性返回 0");
    }

    #[test]
    fn text_similarity_contains_shortcut_needs_minimum_length() {
        // 短语气词被长文本"完全包含"不应再拿满分捷径(子段化后短子段更容易撞上)。
        assert!(
            text_similarity("嗯", "嗯,今天我们讨论的议题是这样的") < 1.0,
            "过短的一方不应触发 contains 满分"
        );
        // 双方都够长时,contains 捷径照常命中(不收窄正常场景)。
        assert_eq!(
            text_similarity("今天我们讨论的议题", "今天我们讨论的议题以及后续安排"),
            1.0,
            "较长文本仍应正常触发 contains 满分"
        );
    }
}

#[cfg(test)]
mod cloud_worker_tests {
    use super::*;
    use crate::asr::cloud::{CloudAsr, CloudEvent, CloudStream, CloudWord, DefiniteUtterance, MockCloudAsr};
    use crate::audio::Source;
    use std::sync::{Arc, Mutex};

    // 短 hold:云端 worker 的 mic 段同样进回声 hold,测试里只关心停录排干后的结果,
    // hold 时长本身对断言无影响(单源、无 system 段)。
    const TEST_ECHO_HOLD: Duration = Duration::from_millis(50);

    fn utter(text: &str, s: u64, e: u64) -> DefiniteUtterance {
        DefiniteUtterance { text: text.into(), start_ms: s, end_ms: e, words: vec![], lang: String::new() }
    }

    /// 状态事件 → 短名,便于按序断言。
    fn status_name(s: &CloudAsrStatus) -> String {
        match s {
            CloudAsrStatus::Reconnecting { .. } => "Reconnecting",
            CloudAsrStatus::Recovered { .. } => "Recovered",
            CloudAsrStatus::Backfilling { .. } => "Backfilling",
            CloudAsrStatus::BackfillFailed { .. } => "BackfillFailed",
        }
        .to_string()
    }

    type Collected = (Vec<(Source, String, u64, u64)>, Vec<String>, Vec<String>);

    /// 驱动骨架:单源 mic。帧全部预先入队后关掉发送端(= 停录),worker 就地跑完返回,
    /// 三路回调收进 Vec 供断言。补识切段桩默认「整缺口一段、偏移 0」。
    fn drive(mock: MockCloudAsr, frames: Vec<Vec<f32>>) -> Collected {
        drive_with(Arc::new(mock), frames, Box::new(|gap: &[f32]| vec![(0u64, gap.to_vec())]))
    }

    fn drive_with(
        cloud: Arc<dyn CloudAsr>,
        frames: Vec<Vec<f32>>,
        backfill_segmenter: Box<BackfillSegmenter>,
    ) -> Collected {
        let (tx, rx) = crossbeam_channel::unbounded::<Vec<f32>>();
        for f in frames {
            tx.send(f).unwrap();
        }
        drop(tx); // 停录:音频通道关闭即触发 worker 收尾

        let finals = Arc::new(Mutex::new(Vec::<(Source, String, u64, u64)>::new()));
        let partials = Arc::new(Mutex::new(Vec::<String>::new()));
        let statuses = Arc::new(Mutex::new(Vec::<String>::new()));
        let (f2, p2, s2) = (finals.clone(), partials.clone(), statuses.clone());

        let _ = run_cloud_asr_worker(
            cloud,
            None,
            SpeakerRegistry::new(),
            vec![(Source::Mic, rx)],
            TEST_ECHO_HOLD,
            false, // language_filter:测试文本与语言过滤无关,关掉隔离变量
            backfill_segmenter,
            move |s, t, start_ms, end_ms, _, _| f2.lock().unwrap().push((s, t, start_ms, end_ms)),
            move |s, t| p2.lock().unwrap().push(format!("{}:{}", s.as_str(), t)),
            |_| {},
            move |st| s2.lock().unwrap().push(status_name(&st)),
        );

        let finals = finals.lock().unwrap().clone();
        let partials = partials.lock().unwrap().clone();
        let statuses = statuses.lock().unwrap().clone();
        (finals, partials, statuses)
    }

    #[test]
    fn definite_maps_stream_ms_to_source_timeline_and_reaches_final() {
        // 脚本不带 Closed{None}:MockCloudAsr 在开流瞬间就投递全部事件,写在这里
        // 等于"厂商刚开流就收尾、而录音还在继续",按 F1 会给尾部音频留占位段
        // (那条语义由 normal_close_mid_session_leaves_placeholder_for_tail_audio 专测)。
        // 本例只关心时间映射,让流一直活到停录排干为止。
        let mock = MockCloudAsr::new(
            vec![vec![CloudEvent::Definite(utter("你好", 0, 500))]],
            vec![],
        );
        let (finals, _, _) = drive(mock, vec![vec![0.0; 16000]]);
        assert_eq!(finals.len(), 1, "一条 Definite → 一条 final");
        assert_eq!(finals[0].1, "你好");
        assert_eq!((finals[0].2, finals[0].3), (0, 500), "首流 stream_base=0,厂商 ms 即绝对 ms");
    }

    #[test]
    fn interim_feeds_partial_slot() {
        let mock = MockCloudAsr::new(
            vec![vec![
                CloudEvent::Interim { text: "你".into() },
                CloudEvent::Closed { error: None },
            ]],
            vec![],
        );
        let (_, partials, _) = drive(mock, vec![vec![0.0; 1600]]);
        assert_eq!(partials, vec!["mic:你".to_string()], "Interim 经 push_partial 出预览");
    }

    #[test]
    fn reconnect_records_gap_and_backfills_via_batch() {
        // 流1 立刻带错关闭(缺口起点 fed=0);流2 重连成功并再吐一句(验证 stream_base 重基)。
        let mock = MockCloudAsr::new(
            vec![
                vec![CloudEvent::Closed { error: Some("net".into()) }],
                vec![
                    CloudEvent::Definite(utter("续", 0, 200)),
                    CloudEvent::Closed { error: None },
                ],
            ],
            vec![Ok(vec![utter("补", 0, 1000)])],
        );
        // 1s 音频 → 缺口 [0, 16000) = [0ms, 1000ms)
        let (finals, _, statuses) = drive(mock, vec![vec![0.1; 16000]]);
        assert_eq!(
            statuses,
            vec!["Reconnecting", "Backfilling", "Recovered"],
            "重连→补识→恢复:Recovered 殿后(缺口处理完才算恢复)"
        );
        assert!(
            finals.contains(&(Source::Mic, "补".into(), 0, 1000)),
            "补识结果落在缺口起点 + 段偏移 + 段内相对时间上: {finals:?}"
        );
        assert!(
            finals.contains(&(Source::Mic, "续".into(), 1000, 1200)),
            "重连后厂商 ms 从 0 重计,须叠加 stream_base(1000ms): {finals:?}"
        );
    }

    #[test]
    fn batch_failure_yields_placeholder_covering_gap() {
        let mock = MockCloudAsr::new(
            vec![
                vec![CloudEvent::Closed { error: Some("net".into()) }],
                vec![CloudEvent::Closed { error: None }],
            ],
            vec![Err(anyhow::anyhow!("batch boom"))],
        );
        let (finals, _, statuses) = drive(mock, vec![vec![0.1; 16000]]);
        assert_eq!(
            statuses,
            vec!["Reconnecting", "Backfilling", "BackfillFailed", "Recovered"],
            "补识失败也要走完恢复流程"
        );
        assert_eq!(
            finals,
            vec![(Source::Mic, "[识别失败]".to_string(), 0, 1000)],
            "整缺口一条占位段,不静默吞掉这段发声"
        );
    }

    #[test]
    fn gap_beyond_ring_yields_placeholder_for_uncovered_head() {
        // 缺口 400s,ring 只留最近 5 分钟(300s):前 100s 无音频可补 → 占位;
        // 其余走批式补识。
        let mock = MockCloudAsr::new(
            vec![
                vec![CloudEvent::Closed { error: Some("net".into()) }],
                vec![CloudEvent::Closed { error: None }],
            ],
            vec![Ok(vec![utter("尾", 0, 500)])],
        );
        let cloud: Arc<dyn CloudAsr> = Arc::new(mock);
        // 切段桩不搬运大数组:只回一个「偏移 0 的小段」,时间映射只看偏移。
        let (finals, _, statuses) = drive_with(
            cloud,
            vec![vec![0.1; 3_200_000], vec![0.1; 3_200_000]], // 共 400s
            Box::new(|_gap: &[f32]| vec![(0u64, vec![0.1; 16])]),
        );
        let ring_ms = (CLOUD_RING_CAP as u64) * 1000 / 16_000; // 300_000ms
        let head_end = 400_000 - ring_ms; // 100_000ms:ring 覆盖不到的头部
        assert_eq!(
            finals,
            vec![
                (Source::Mic, "[识别失败]".to_string(), 0, head_end),
                (Source::Mic, "尾".to_string(), head_end, head_end + 500),
            ],
            "先占位未覆盖头部,再落覆盖部分的补识结果"
        );
        assert_eq!(statuses, vec!["Reconnecting", "Backfilling", "Recovered"]);
    }

    /// finish 之后才吐最后一句的厂商流(排干路径专用)。
    struct LateFinishCloud;
    impl CloudAsr for LateFinishCloud {
        fn open_stream(&self) -> anyhow::Result<CloudStream> {
            let (tx, rx) = crossbeam_channel::unbounded();
            let tx_finish = tx.clone();
            Ok(CloudStream {
                // tx 挂在 push 上:流活着期间事件通道不断开(与真实适配层一致)。
                push: Box::new(move |_s: &[f32]| {
                    let _ = &tx;
                    Ok(())
                }),
                finish: Box::new(move || {
                    let _ = tx_finish.send(CloudEvent::Definite(DefiniteUtterance {
                        text: "尾句".into(),
                        start_ms: 0,
                        end_ms: 300,
                        words: vec![],
                        lang: String::new(),
                    }));
                    Ok(())
                }),
                events: rx,
            })
        }
        fn transcribe_batch(&self, _s: &[f32]) -> anyhow::Result<Vec<DefiniteUtterance>> {
            anyhow::bail!("本例不走批式")
        }
    }

    #[test]
    fn definite_arriving_after_finish_still_lands_during_drain() {
        let (finals, _, _) = drive_with(
            Arc::new(LateFinishCloud),
            vec![vec![0.1; 16000]],
            Box::new(|gap: &[f32]| vec![(0u64, gap.to_vec())]),
        );
        assert_eq!(
            finals,
            vec![(Source::Mic, "尾句".to_string(), 0, 300)],
            "停录 finish 后才到的定稿必须排干落地"
        );
    }

    #[test]
    fn audio_during_disconnect_is_accounted_but_not_pushed() {
        // 只给一条流脚本:断连后重连必然失败,整段音频都在断连期内。
        let mock = MockCloudAsr::new(
            vec![vec![CloudEvent::Closed { error: Some("net".into()) }]],
            vec![],
        );
        let pushed = mock.pushed_samples.clone();
        let cloud: Arc<dyn CloudAsr> = Arc::new(mock);
        let (finals, _, statuses) = drive_with(
            cloud,
            vec![vec![0.1; 16000]],
            Box::new(|gap: &[f32]| vec![(0u64, gap.to_vec())]),
        );
        assert_eq!(*pushed.lock().unwrap(), 0, "流已死:断连期间的音频一个样本都不许推");
        assert_eq!(
            finals,
            vec![(Source::Mic, "[识别失败]".to_string(), 0, 1000)],
            "音频仍在记账,停录时补不回来 → 整缺口占位(不静默丢)"
        );
        assert_eq!(statuses, vec!["Reconnecting", "BackfillFailed"]);
    }

    #[test]
    fn each_source_opens_its_own_stream_on_its_own_timeline() {
        // 两源各开一条流(mock 按 open 顺序发脚本),各记各的账。system 侧预览还要
        // 经 push_partial 进 sink 的在途文本记账(mic 预览级回声抑制依赖它)。
        // 脚本不带 Closed{None}:mock 会在开流瞬间就投递它,那等于"厂商刚开流就收尾
        // 而录音继续",按 F1 两源都会多出一条尾部占位段,与本例要验的多源时间轴无关。
        let mock = MockCloudAsr::new(
            vec![
                vec![CloudEvent::Definite(utter("我这边", 0, 500))],
                vec![
                    CloudEvent::Interim { text: "对方在讲".into() },
                    CloudEvent::Definite(utter("对方在讲话", 3000, 3600)),
                ],
            ],
            vec![],
        );
        let (mic_tx, mic_rx) = crossbeam_channel::unbounded::<Vec<f32>>();
        let (sys_tx, sys_rx) = crossbeam_channel::unbounded::<Vec<f32>>();
        mic_tx.send(vec![0.1; 16000]).unwrap();
        sys_tx.send(vec![0.1; 32000]).unwrap();
        drop(mic_tx);
        drop(sys_tx);

        let finals = Arc::new(Mutex::new(Vec::<(Source, String, u64, u64)>::new()));
        let partials = Arc::new(Mutex::new(Vec::<String>::new()));
        let (f2, p2) = (finals.clone(), partials.clone());
        let _ = run_cloud_asr_worker(
            Arc::new(mock),
            None,
            SpeakerRegistry::new(),
            vec![(Source::Mic, mic_rx), (Source::System, sys_rx)],
            TEST_ECHO_HOLD,
            false,
            Box::new(|gap: &[f32]| vec![(0u64, gap.to_vec())]),
            move |s, t, a, b, _, _| f2.lock().unwrap().push((s, t, a, b)),
            move |s, t| p2.lock().unwrap().push(format!("{}:{}", s.as_str(), t)),
            |_| {},
            |st| panic!("不该有状态事件: {st:?}"),
        );
        let finals = finals.lock().unwrap().clone();
        assert!(finals.contains(&(Source::Mic, "我这边".into(), 0, 500)), "{finals:?}");
        assert!(
            finals.contains(&(Source::System, "对方在讲话".into(), 3000, 3600)),
            "{finals:?}"
        );
        assert_eq!(*partials.lock().unwrap(), vec!["system:对方在讲".to_string()]);
    }

    #[test]
    fn ring_trims_to_cap_and_slices_only_from_covered_start() {
        let mut feed = SourceFeed::new();
        feed.account(&vec![0.5; CLOUD_RING_CAP]);
        feed.account(&vec![0.25; 16_000]);
        assert_eq!(feed.fed, CLOUD_RING_CAP as u64 + 16_000);
        assert_eq!(feed.ring.len(), CLOUD_RING_CAP, "环容量掐头到 CAP");
        assert_eq!(feed.ring_start, 16_000, "掐掉多少,ring_start 就前进多少");
        assert!(feed.slice(0, 1600).is_empty(), "段首已被挤掉 → 整段给空");
        assert_eq!(feed.slice(16_000, 17_600).len(), 1600, "环内区间照常取样");
        // 段尾越过已喂样本(厂商补白):截到环末而不是整段丢弃。
        let tail = feed.slice(feed.fed - 1600, feed.fed + 1600);
        assert_eq!(tail.len(), 1600);
        assert!(tail.iter().all(|&x| x == 0.25));
    }

    /// 按推流进度投递事件的桩:脚本 `(after_samples, events)` —— 本流累计推入
    /// after_samples 个样本后事件才进通道(0 = 开流即发)。
    /// MockCloudAsr 在开流瞬间就把脚本灌满通道,复现不出"先喂音频、定稿/关闭随后
    /// 才到"的真实次序,而接缝类缺陷(F1 正常关闭吞尾音、F2 定稿滞后)恰恰只在这
    /// 种次序下才暴露。
    struct PushGatedCloud {
        scripts: Mutex<std::collections::VecDeque<(usize, Vec<CloudEvent>)>>,
        batches: Mutex<std::collections::VecDeque<anyhow::Result<Vec<DefiniteUtterance>>>>,
    }

    impl PushGatedCloud {
        fn new(
            scripts: Vec<(usize, Vec<CloudEvent>)>,
            batches: Vec<anyhow::Result<Vec<DefiniteUtterance>>>,
        ) -> Self {
            Self { scripts: Mutex::new(scripts.into()), batches: Mutex::new(batches.into()) }
        }
    }

    impl CloudAsr for PushGatedCloud {
        fn open_stream(&self) -> anyhow::Result<CloudStream> {
            let (after, mut pending) = self
                .scripts
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("PushGatedCloud 流脚本耗尽"))?;
            let (tx, rx) = crossbeam_channel::unbounded();
            if after == 0 {
                for e in pending.drain(..) {
                    let _ = tx.send(e);
                }
            }
            let mut pushed = 0usize;
            Ok(CloudStream {
                // tx 随闭包存活:流活着期间事件通道不断开(与真实适配层一致)。
                push: Box::new(move |s: &[f32]| {
                    pushed += s.len();
                    if pushed >= after {
                        for e in pending.drain(..) {
                            let _ = tx.send(e);
                        }
                    }
                    Ok(())
                }),
                finish: Box::new(|| Ok(())),
                events: rx,
            })
        }
        fn transcribe_batch(&self, _s: &[f32]) -> anyhow::Result<Vec<DefiniteUtterance>> {
            self.batches
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(anyhow::anyhow!("PushGatedCloud 批式脚本耗尽")))
        }
    }

    #[test]
    fn normal_close_mid_session_leaves_placeholder_for_tail_audio() {
        // 厂商在 0.5s 处正常收尾,而录音还在继续:此后 1.5s 音频无流可推 → 必须留痕。
        let cloud: Arc<dyn CloudAsr> = Arc::new(PushGatedCloud::new(
            vec![(
                8_000,
                vec![
                    CloudEvent::Definite(utter("短句", 0, 500)),
                    CloudEvent::Closed { error: None },
                ],
            )],
            vec![],
        ));
        let (finals, _, statuses) = drive_with(
            cloud,
            vec![vec![0.1; 8_000]; 4], // 4 × 0.5s = 2s
            Box::new(|gap: &[f32]| vec![(0u64, gap.to_vec())]),
        );
        assert!(finals.contains(&(Source::Mic, "短句".into(), 0, 500)), "{finals:?}");
        assert!(
            finals.contains(&(Source::Mic, "[识别失败]".into(), 500, 2000)),
            "正常关闭之后仍在喂的音频没人识别,必须占位留痕: {finals:?}"
        );
        assert!(
            !statuses.contains(&"Reconnecting".to_string()),
            "正常关闭不是断连:不重连也不该报重连中: {statuses:?}"
        );
    }

    #[test]
    fn gap_starts_at_last_definite_end_not_at_fed() {
        // 定稿滞后推流:句子只到 500ms,断连时已推到 1000ms。缺口若从 fed 起,
        // [500ms,1000ms) 这段"推了但没回话"的音频就永久消失。
        let cloud: Arc<dyn CloudAsr> = Arc::new(PushGatedCloud::new(
            vec![
                (
                    16_000,
                    vec![
                        CloudEvent::Definite(utter("甲", 0, 500)),
                        CloudEvent::Closed { error: Some("net".into()) },
                    ],
                ),
                (0, vec![CloudEvent::Closed { error: None }]),
            ],
            vec![Ok(vec![utter("补", 0, 1500)])],
        ));
        let gap_lens = Arc::new(Mutex::new(Vec::<usize>::new()));
        let seen = gap_lens.clone();
        let (finals, _, statuses) = drive_with(
            cloud,
            vec![vec![0.1; 8_000]; 4], // 2s
            Box::new(move |gap: &[f32]| {
                seen.lock().unwrap().push(gap.len());
                vec![(0u64, gap.to_vec())]
            }),
        );
        assert_eq!(
            *gap_lens.lock().unwrap(),
            vec![24_000],
            "缺口 = [最后定稿末尾 500ms, 停录 2000ms) = 1.5s;旧口径 gap_from=fed 只会补回 1s"
        );
        assert!(
            finals.contains(&(Source::Mic, "补".into(), 500, 2000)),
            "补识结果贴回缺口起点(定稿末尾): {finals:?}"
        );
        assert_eq!(statuses, vec!["Reconnecting", "Backfilling", "Recovered"]);
    }

    #[test]
    fn next_backoff_doubles_until_cap_and_only_resets_after_stable_run() {
        assert_eq!(next_backoff(CLOUD_BACKOFF_BASE_MS, 0), CLOUD_BACKOFF_BASE_MS * 2, "秒死流:翻倍");
        assert_eq!(next_backoff(4, CLOUD_BACKOFF_STABLE_MS - 1), 8, "差一点站住也不算站住");
        assert_eq!(next_backoff(20_000, 0), CLOUD_BACKOFF_CAP_MS, "翻倍不越上限");
        assert_eq!(next_backoff(CLOUD_BACKOFF_CAP_MS, 0), CLOUD_BACKOFF_CAP_MS, "封顶后停在上限");
        assert_eq!(
            next_backoff(CLOUD_BACKOFF_CAP_MS, CLOUD_BACKOFF_STABLE_MS),
            CLOUD_BACKOFF_BASE_MS,
            "活够门槛 → 退避清回基数"
        );
        assert_eq!(
            next_backoff(CLOUD_BACKOFF_CAP_MS, CLOUD_BACKOFF_STABLE_MS * 100),
            CLOUD_BACKOFF_BASE_MS
        );
    }

    #[test]
    fn definite_abs_samples_anchors_stream_ms_onto_absolute_timeline() {
        let u = utter("一句", 500, 1200);
        assert_eq!(definite_abs_samples(0, &u), (8_000, 19_200), "首流:500ms/1200ms → 8000/19200 样本");
        // 重连后 stream_base 重基到 2s 处,厂商 ms 仍从 0 重计。
        assert_eq!(definite_abs_samples(32_000, &u), (40_000, 51_200));
    }

    #[test]
    fn words_become_tokens_with_relative_second_timestamps() {
        let mut u = utter("你好世界", 1000, 2000);
        u.words = vec![
            CloudWord { text: "你好".into(), start_ms: 1000, end_ms: 1500 },
            CloudWord { text: "世界".into(), start_ms: 1500, end_ms: 2000 },
        ];
        let t = utterance_to_transcript(&u);
        assert_eq!(t.tokens, vec!["你好".to_string(), "世界".to_string()]);
        assert_eq!(t.timestamps, vec![0.0, 0.5], "时间戳相对段首、单位秒");
        let empty = utterance_to_transcript(&utter("无词", 0, 100));
        assert!(empty.tokens.is_empty() && empty.timestamps.is_empty(), "无词表 → 两者皆空");
    }
}
