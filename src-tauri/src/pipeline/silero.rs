use super::segmenter::{Segment, Segmenter};
use std::collections::VecDeque;
use std::path::Path;

/// 基于 sherpa-onnx Silero VAD 的语句分段器。
/// 内部维护"当前句"缓冲：只在说话时累积，VAD 切出完整段时清空，用于实时 partial。
/// 另维护近期原始样本的滚动历史(history),供段出炉时句首/句尾补帧(pre/post-roll)。
pub struct SileroSegmenter {
    vad: sherpa_onnx::VoiceActivityDetector,
    current: Vec<f32>,
    history: SampleHistory,
    /// 上一段(含补帧)的绝对末样本号:pre-roll 的不重叠钳制基准。
    padded_end: usize,
}

impl SileroSegmenter {
    pub fn new(model_path: &Path) -> anyhow::Result<Self> {
        let config = sherpa_onnx::VadModelConfig {
            silero_vad: sherpa_onnx::SileroVadModelConfig {
                model: Some(model_path.to_string_lossy().into_owned()),
                // 静音 > 此值视为一句结束。0.6s→1.2s(2026-08-04 实测):中文自然停顿
                // 频繁落在 0.6~1.2s,旧值把一句话切成 1~3s 碎片,每片单独送识别——
                // LLM 解码的 Qwen3 缺上下文就编造("类似于。""五万。""Be man, we need.")。
                // 真实笔记复现:问题区段 28 个碎段的相邻间隙最小值恰为 606ms(全部贴着
                // 旧门限切),按 1.2s 重新成段后 28→18 段、平均 3.6s→6.1s,多条句子从
                // 碎片乱码恢复成完整语义(如「我从。」+「我从这个项目上来讲。」+
                // 「我就必须把。」→「就我从我负责这个项目上来讲,我就必须把」)。
                // 代价:定稿延迟 +0.6s(预览不受影响),且段跨说话人的概率上升——
                // 实测同源相邻段在 0.6~1.2s 间隙内 mic 74% / system 90% 是同一人,
                // 跨界部分由 split_final 的滑窗声纹变更点检测兜底(模型给出 token
                // 时间戳时才能落到文本切分,Qwen3 无时间戳时保留母段)。
                min_silence_duration: 1.2,
                min_speech_duration: 0.25,
                max_speech_duration: 15.0, // 上限：超 15s 强制切，界定每次识别量
                threshold: 0.5,
                window_size: 512,
            },
            sample_rate: 16000,
            num_threads: 1,
            ..Default::default()
        };
        // buffer_size_in_seconds：内部环形缓冲容量，给足
        let vad = sherpa_onnx::VoiceActivityDetector::create(&config, 30.0)
            .ok_or_else(|| anyhow::anyhow!("加载 Silero VAD 失败(检查 {:?})", model_path))?;
        Ok(Self { vad, current: Vec::new(), history: SampleHistory::new(), padded_end: 0 })
    }
}

/// 句首/句尾补帧量。VAD 判定语音起点普遍偏晚(阈值需数帧爬升),句首辅音/弱起音
/// 被掐掉,识别端表现为漏字;句尾程度轻但同理。补帧取历史缓冲里的真实样本而非
/// 填零——被掐掉的音头就在那段"历史"里。两侧之和(0.5s)严格小于
/// min_silence_duration(1.2s):相邻两段的补帧不可能吃进对方的语音。
const PRE_ROLL_SAMPLES: usize = 16000 * 300 / 1000;
const POST_ROLL_SAMPLES: usize = 16000 * 200 / 1000;
/// 历史缓冲容量:段最长 15s + 静音判定滞后 1.2s + VAD 内部缓冲余量,取 20s
/// (~1.25MB/实例)。超范围的补帧请求按可用范围钳制(少补,不出错)。
const HISTORY_SAMPLES: usize = 20 * 16000;

/// 绝对样本号索引的滚动历史缓冲(base = buf[0] 的绝对样本号)。
struct SampleHistory {
    buf: VecDeque<f32>,
    base: usize,
}

impl SampleHistory {
    fn new() -> Self {
        Self { buf: VecDeque::new(), base: 0 }
    }

    fn push(&mut self, samples: &[f32]) {
        self.buf.extend(samples);
        if self.buf.len() > HISTORY_SAMPLES {
            let drop = self.buf.len() - HISTORY_SAMPLES;
            self.buf.drain(..drop);
            self.base += drop;
        }
    }

    fn end(&self) -> usize {
        self.base + self.buf.len()
    }

    /// 取绝对样本区间 [from, to),越界部分钳制,返回 (实际起点, 样本)。
    /// 区间完全不可得时返回 (from, 空)。
    fn range(&self, from: usize, to: usize) -> (usize, Vec<f32>) {
        let lo = from.max(self.base);
        let hi = to.min(self.end());
        if hi <= lo {
            return (from, Vec::new());
        }
        (lo, self.buf.range(lo - self.base..hi - self.base).copied().collect())
    }
}

/// 用历史缓冲给 VAD 原始段补 pre/post-roll。prev_end 是上一段(含补帧)的绝对末
/// 样本号:pre-roll 不越过它。按当前参数(0.5s < 1.2s 静音门)本不会相撞,钳制是
/// 参数改动/极端切分下的不变式兜底——任何情况下输出段之间零样本重叠,时间轴单调。
fn pad_segment(seg: Segment, hist: &SampleHistory, prev_end: usize) -> Segment {
    let want_from = seg.start.saturating_sub(PRE_ROLL_SAMPLES).max(prev_end);
    let (pre_from, pre) = hist.range(want_from, seg.start);
    let seg_end = seg.start + seg.samples.len();
    let (_, post) = hist.range(seg_end, seg_end + POST_ROLL_SAMPLES);
    if pre.is_empty() && post.is_empty() {
        return seg;
    }
    let mut samples = Vec::with_capacity(pre.len() + seg.samples.len() + post.len());
    samples.extend_from_slice(&pre);
    samples.extend_from_slice(&seg.samples);
    samples.extend_from_slice(&post);
    let start = if pre.is_empty() { seg.start } else { pre_from };
    Segment { samples, start }
}

/// 段长硬上限(样本数,16kHz × 15s),与 config.max_speech_duration 对齐。
/// 该配置一路透传 sherpa,但当前版本实测不强制(冒烟见 36s 超长段照常产出),
/// 故在分段器出口自兜底:超长段按此硬切。混说场景一段多人,段越长声纹污染越重,
/// 识别也劣化——上限必须真实生效。
const MAX_SEGMENT_SAMPLES: usize = 15 * 16000;

/// 找切点的搜索窗长(100ms @ 16kHz)。
const QUIET_WIN_SAMPLES: usize = 1600;
/// 找切点的滑窗步长(25ms @ 16kHz)。
const QUIET_STEP_SAMPLES: usize = 400;

/// 在 `[lo, hi)` 内找能量(均方)最低的 100ms 窗,返回其中心样本 idx——
/// 硬切尽量落在停顿/低能量处,盲切在词中间会让切口两侧识别都出错。
/// 区间放不下一整窗(或为空/越界)时退化返回 lo,由调用方兜底。
fn quietest_cut(samples: &[f32], lo: usize, hi: usize) -> usize {
    let hi = hi.min(samples.len());
    if hi <= lo || hi - lo < QUIET_WIN_SAMPLES {
        return lo;
    }
    let mut best_start = lo;
    let mut best_energy = f32::INFINITY;
    let mut s = lo;
    while s + QUIET_WIN_SAMPLES <= hi {
        let energy: f32 =
            samples[s..s + QUIET_WIN_SAMPLES].iter().map(|x| x * x).sum::<f32>() / QUIET_WIN_SAMPLES as f32;
        if energy < best_energy {
            best_energy = energy;
            best_start = s;
        }
        s += QUIET_STEP_SAMPLES;
    }
    best_start + QUIET_WIN_SAMPLES / 2
}

/// 超长段按 MAX_SEGMENT_SAMPLES 找静点切,子段 start 顺延样本偏移(时间轴连续)。
/// 切点不再固定在 MAX 处盲切(会切在词中间,切口两侧识别都出错),改在
/// `[0.7*MAX, MAX]` 区间内找能量最低的 100ms 窗,尽量落在停顿处。
fn split_long(samples: Vec<f32>, start: usize) -> Vec<Segment> {
    if samples.len() <= MAX_SEGMENT_SAMPLES {
        return vec![Segment { samples, start }];
    }
    let mut out = Vec::new();
    let mut off = 0;
    while samples.len() - off > MAX_SEGMENT_SAMPLES {
        let lo = off + MAX_SEGMENT_SAMPLES * 7 / 10;
        let hi = off + MAX_SEGMENT_SAMPLES;
        let cut = quietest_cut(&samples, lo, hi);
        // 不变式:cut 必须严格大于 off,否则子段长度为 0 / 死循环;区间异常时
        // 回退原来的盲切边界(off + MAX)。
        let cut = if cut > off { cut } else { off + MAX_SEGMENT_SAMPLES };
        out.push(Segment { samples: samples[off..cut].to_vec(), start: start + off });
        off = cut;
    }
    out.push(Segment { samples: samples[off..].to_vec(), start: start + off });
    out
}

impl Segmenter for SileroSegmenter {
    fn accept(&mut self, samples: &[f32]) {
        // 非有限值消毒:AEC/AGC/重采样链的数值边界可能产出 NaN/Inf,喂进 ONNX 轻则
        // 概率失真,重则 ORT 报错抛 C++ 异常——sherpa C 接口不接异常,直接 SIGABRT
        // 全进程闪退(2026-07-07 两例,栈在 SileroVadModel::RunV4)。在唯一入口消毒。
        let samples: Vec<f32> = samples
            .iter()
            .map(|&x| if x.is_finite() { x } else { 0.0 })
            .collect();
        let samples = samples.as_slice();
        // 历史缓冲与 VAD 喂同一份消毒后样本:补帧取到的内容与 VAD 判定所见严格同源。
        self.history.push(samples);
        self.vad.accept_waveform(samples);
        if self.vad.detected() {
            self.current.extend_from_slice(samples);
        } else {
            // 静音期清空预览缓冲：避免噪声导致 is_speech 抖动却不成段时，
            // current 里残留过时片段被当成 partial 显示。
            self.current.clear();
        }
    }

    fn take_finished(&mut self) -> Vec<Segment> {
        let mut out = Vec::new();
        while !self.vad.is_empty() {
            let Some(seg) = self.vad.front() else { break };
            let raw = Segment { samples: seg.samples().to_vec(), start: seg.start().max(0) as usize };
            drop(seg); // front 是借用视图,pop 前先归还
            self.vad.pop();
            // 先补帧再硬切:split_long 的内部切点两侧是连续语音,不补(补了会在
            // 切口两侧重复 0.5s 内容);补帧只发生在 VAD 判定的真实句边界。
            let padded = pad_segment(raw, &self.history, self.padded_end);
            self.padded_end = padded.start + padded.samples.len();
            out.extend(split_long(padded.samples, padded.start));
        }
        if !out.is_empty() {
            // 已完成的语句对应的"当前句"已结束，清空预览缓冲。
            self.current.clear();
        }
        out
    }

    fn current_partial(&mut self) -> Option<Vec<f32>> {
        if self.current.is_empty() { None } else { Some(self.current.clone()) }
    }

    fn flush(&mut self) {
        self.vad.flush();
        self.current.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::segmenter::Segmenter;

    /// 硬切纯逻辑:边界内不切、超长切块、尾块与时间轴偏移正确。
    /// 全零样本时各窗能量恒等,quietest_cut 退化为搜索区间([0.7*MAX, MAX))首窗
    /// 中心——不再断言精确 15s 边界,改断言:每子段 ≤ 上限、非末块不短于搜索
    /// 下界(0.7*MAX)、偏移单调衔接、总样本量守恒(不丢内容)。
    #[test]
    fn split_long_caps_segment_length_and_keeps_offsets() {
        // 恰好上限:原样一段
        let one = split_long(vec![0.0; MAX_SEGMENT_SAMPLES], 100);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].samples.len(), MAX_SEGMENT_SAMPLES);
        assert_eq!(one[0].start, 100);

        // 2.5 倍上限:应切成多块,偏移顺延,总样本无增无减。
        let n = MAX_SEGMENT_SAMPLES * 5 / 2;
        let parts = split_long(vec![0.0; n], 1000);
        assert!(parts.len() >= 2, "超长段应被切分为多块: {}", parts.len());
        let mut prev_end = 1000usize;
        for p in &parts {
            assert_eq!(p.start, prev_end, "偏移应与前一块末尾无缝衔接");
            assert!(p.samples.len() <= MAX_SEGMENT_SAMPLES, "每子段不应超过上限");
            prev_end = p.start + p.samples.len();
        }
        assert_eq!(prev_end, 1000 + n, "总样本量应与母段一致,不丢内容");
        for p in &parts[..parts.len() - 1] {
            assert!(
                p.samples.len() >= MAX_SEGMENT_SAMPLES * 7 / 10,
                "被切开的子段不应短于搜索下界(0.7*MAX)"
            );
        }
    }

    /// quietest_cut 纯函数:全等能量时退化为区间起点窗;能找到明显静音时应精确命中。
    #[test]
    fn quietest_cut_finds_lowest_energy_window() {
        // 全零区间:能量恒为 0,退化返回 lo(首窗中心 = lo + WIN/2,这里只需 ≥ lo)。
        let flat = vec![0.0f32; 20_000];
        let cut = quietest_cut(&flat, 1000, 18_000);
        assert!(cut >= 1000 && cut < 18_000, "退化情形切点应落在搜索区间内: {cut}");

        // 区间中段插入一段明显更安静的窗口,应被精确找到。
        let mut samples = vec![0.5f32; 20_000];
        for s in &mut samples[10_000..11_600] {
            *s = 0.0;
        }
        let cut = quietest_cut(&samples, 0, 20_000);
        assert!(
            (10_000..=11_600).contains(&cut),
            "应命中静音窗口中心附近: cut={cut}"
        );

        // 区间放不下一整窗:退化返回 lo。
        assert_eq!(quietest_cut(&flat, 100, 100 + QUIET_WIN_SAMPLES - 1), 100);
    }

    /// 20s 母段中段(12s 处)埋 200ms 静音,其余为响亮语音——首个切点应落在静音区
    /// (±100ms 容差内),而非盲切在词中间。
    #[test]
    fn split_long_cuts_at_quiet_point_when_silence_present() {
        const SR: usize = 16000;
        let total = 20 * SR; // 20s,触发切分(> 15s 上限)
        let mut samples = vec![0.5f32; total];
        let silence_start = 12 * SR;
        let silence_len = SR / 5; // 200ms
        for s in &mut samples[silence_start..silence_start + silence_len] {
            *s = 0.0;
        }
        let parts = split_long(samples, 0);
        assert!(parts.len() >= 2, "20s 母段应触发切分");
        let cut = parts[0].samples.len(); // 母段起点为 0,首子段长度即首个切点样本位置
        let tol = SR / 10; // ±100ms
        assert!(
            cut + tol >= silence_start && cut <= silence_start + silence_len + tol,
            "首个切点应落在静音区(±100ms)附近: cut={cut}, silence=[{silence_start},{})",
            silence_start + silence_len
        );
    }

    /// 全响亮均匀段(无静音可找)、非上限整数倍:各子段仍应 ≤ 上限,总样本守恒。
    #[test]
    fn split_long_uniform_loud_segment_capped_and_conserves_total() {
        let n = MAX_SEGMENT_SAMPLES * 5 / 2 + 12_345; // 非整数倍,含零头
        let parts = split_long(vec![0.5f32; n], 500);
        let total: usize = parts.iter().map(|p| p.samples.len()).sum();
        assert_eq!(total, n, "总样本量应守恒,不丢内容");
        for p in &parts {
            assert!(p.samples.len() <= MAX_SEGMENT_SAMPLES, "每子段不应超过上限");
        }
        let mut prev_end = 500usize;
        for p in &parts {
            assert_eq!(p.start, prev_end, "偏移应单调衔接");
            prev_end = p.start + p.samples.len();
        }
    }

    /// 构造内容可验证的历史:绝对样本号 i 的值为 i as f32,补进来的每个样本
    /// 都能对出它来自哪个绝对位置。
    fn indexed_history(total: usize) -> SampleHistory {
        let mut h = SampleHistory::new();
        let samples: Vec<f32> = (0..total).map(|i| i as f32).collect();
        h.push(&samples);
        h
    }

    /// 补帧基本形:前后各补足额,start 前移 PRE_ROLL,内容与历史逐样本一致。
    #[test]
    fn pad_segment_prepends_and_appends_from_history() {
        let hist = indexed_history(100_000);
        let seg = Segment { samples: (20_000..40_000).map(|i| i as f32).collect(), start: 20_000 };
        let padded = pad_segment(seg, &hist, 0);
        assert_eq!(padded.start, 20_000 - PRE_ROLL_SAMPLES);
        assert_eq!(padded.samples.len(), 20_000 + PRE_ROLL_SAMPLES + POST_ROLL_SAMPLES);
        // 逐样本核对:整段应等于历史区间 [start, start+len)
        for (k, &v) in padded.samples.iter().enumerate() {
            assert_eq!(v, (padded.start + k) as f32, "样本 {k} 应来自绝对位置 {}", padded.start + k);
        }
    }

    /// pre-roll 不越过上一段末尾:即使请求区间更早,也从 prev_end 起补。
    #[test]
    fn pad_segment_clamps_pre_roll_at_previous_end() {
        let hist = indexed_history(100_000);
        let prev_end = 19_000;
        let seg = Segment { samples: (20_000..30_000).map(|i| i as f32).collect(), start: 20_000 };
        let padded = pad_segment(seg, &hist, prev_end);
        assert_eq!(padded.start, prev_end, "pre-roll 应被钳制在上一段末尾");
        assert_eq!(padded.samples[0], prev_end as f32);
    }

    /// 尾段 post-roll 不足(flush 时流已尽):按历史可用范围截断,不填零不越界。
    #[test]
    fn pad_segment_truncates_post_roll_at_stream_end() {
        let total = 30_000 + POST_ROLL_SAMPLES / 2; // 段后只剩半个 post-roll
        let hist = indexed_history(total);
        let seg = Segment { samples: (20_000..30_000).map(|i| i as f32).collect(), start: 20_000 };
        let padded = pad_segment(seg, &hist, 0);
        let end = padded.start + padded.samples.len();
        assert_eq!(end, total, "post-roll 应止于流末尾");
        assert_eq!(*padded.samples.last().unwrap(), (total - 1) as f32);
    }

    /// 历史已滚动淘汰的区间取不到:pre-roll 钳制到缓冲底,start 相应少前移。
    #[test]
    fn pad_segment_degrades_when_history_evicted() {
        let mut hist = SampleHistory::new();
        let total = HISTORY_SAMPLES + 50_000; // 触发淘汰,base = 50_000
        let samples: Vec<f32> = (0..total).map(|i| i as f32).collect();
        hist.push(&samples);
        assert_eq!(hist.base, 50_000);
        // 段起点恰在缓冲底附近,请求的 pre-roll 大半已被淘汰。
        let start = 50_000 + PRE_ROLL_SAMPLES / 4;
        let seg = Segment { samples: (start..start + 8_000).map(|i| i as f32).collect(), start };
        let padded = pad_segment(seg, &hist, 0);
        assert_eq!(padded.start, 50_000, "只能补到缓冲底,不得虚构更早样本");
        assert_eq!(padded.samples[0], 50_000.0);
    }

    /// 历史完全不可得(退化):原段原样返回。
    #[test]
    fn pad_segment_noop_without_history() {
        let hist = SampleHistory::new();
        let seg = Segment { samples: vec![0.5; 1000], start: 5_000 };
        let padded = pad_segment(seg, &hist, 0);
        assert_eq!(padded.start, 5_000);
        assert_eq!(padded.samples.len(), 1000);
    }

    /// 暂停功能依赖：flush 之后继续 accept，段的 start 样本偏移必须延续而非归零。
    /// 需要真实模型：cargo test -- --ignored（或 VN_MODELS 指向模型目录）。
    #[test]
    #[ignore]
    fn flush_midstream_keeps_timeline_monotonic() {
        let model = crate::models::root().join("silero_vad.onnx");
        let mut seg = SileroSegmenter::new(&model).expect("加载 VAD");
        let wav = {
            let mut r = hound::WavReader::open(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/sample_16k.wav"
            ))
            .expect("fixture");
            r.samples::<i16>().map(|s| s.unwrap() as f32 / 32768.0).collect::<Vec<f32>>()
        };
        seg.accept(&wav);
        seg.flush();
        let a = seg.take_finished();
        assert!(!a.is_empty(), "fixture 是真实语音，flush 应产段");
        seg.accept(&wav);
        seg.flush();
        let b = seg.take_finished();
        assert!(!b.is_empty());
        let last_a = a.last().unwrap();
        assert!(
            b[0].start >= last_a.start + last_a.samples.len(),
            "flush 后时间轴延续不重叠: b.start={} vs a.end={}",
            b[0].start,
            last_a.start + last_a.samples.len()
        );
    }
}
