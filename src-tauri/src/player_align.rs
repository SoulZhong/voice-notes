//! 回放跨轨时基对齐:估出 mic 轨相对 system 轨的时间映射,并按映射重采样 mic 轨。
//!
//! **为什么需要**:采集侧若把设备实际出样速率记错(声明 48kHz、实测 ~44kHz),该轨
//! 时间轴就对墙钟线性漂移。两轨仍按各自 offset 铺进混音,于是同一句话——system 数字
//! 直采的一份 + mic 经扬声器回采的一份——被拉开成两处,间隔从 0 一路增长。2026-08-04
//! 一场 30 分钟的笔记末端拉开到 148s,听感就是「每句话说两遍、越到后面隔得越远」。
//!
//! 采集侧的时钟核对(`pipeline::frame_tap`)挡住了以后新录的场次,但已经落盘的历史
//! 录音只能在回放侧补救。补救成立的前提已被实测证实:**mic 轨的内容是完整的,只是被
//! 均匀压缩了**——把 mic 窗按候选因子重采样后与 system 做梅尔谱匹配,相关值在
//! 压缩比处出现尖峰(实测 ncc 0.617 对基线 0.247),说明按映射重采样能完全复位。
//!
//! **判据只看信号,不看转写**:转写段边界本身有 ±1s 级的抖动,且回声段常被识别成
//! 独立的 mic 段;拿它定时基会把噪声当信号(实测同句配对残差 ±1.4s,不足以支撑
//! 400ms 量级的门控)。这里直接在梅尔谱上做归一化互相关,分辨率到帧(20ms)。
//!
//! **保守优先**:漂移是逐段探针实测的,任一环节不可信(探针命中率不足、映射不单调、
//! 局部斜率越界)一律返回 None,调用方据此完全不纠正——降级即现状,绝不弄坏本来正常
//! 的轨道。
//!
//! **已知残余(未达标项)**:实测那场 30 分钟笔记,错位从 147.7s 压到中位 ~0.16s,
//! 但**没有全程压进回放门控的 400ms 回看窗**——独立短窗直测里 25 个采样点有 14 个
//! 落在窗内,t≈100~350s 一段稳定在 0.5~0.9s。那一段门控压不到位置上,可能仍有很轻的
//! 重影。要判定是否达标,先要有一套可信到 0.1s 的残余量法:目前三种量法(估计器自测、
//! 外部梅尔谱扫拉伸、短窗直测)在 0.2~0.9s 区间互不吻合,分歧本身已达阈值量级,
//! 而短窗法自己也会误配出 ±2.5s 的假点。
//!
//! 映射方向固定为 **mic 本地时间 → system 本地时间**:纠正后的 mic 轨与 system 轨
//! 共用同一条时基,故调用方应让它沿用 system 轨的 offset_ms(见 `render_aligned`)。

use realfft::RealFftPlanner;

/// canonical WAV:44 字节头 + 16bit LE 单声道 16k。与 player_gate 同一约定。
const HEADER: usize = 44;
const SR: usize = 16_000;
/// 帧步长 20ms:时间分辨率的下限,也是映射精度的下限。
const HOP: usize = 320;
/// 32ms 窗:够放下基频周期,又不至于把音节抹平。
const NFFT: usize = 512;
const N_MEL: usize = 26;
const MEL_FMIN: f32 = 80.0;
const MEL_FMAX: f32 = 7000.0;

/// 目标探针数:够拟合出拐点,又不至于让相关计算失控。
const TARGET_PROBES: f64 = 24.0;
/// 探针窗长。取值是两头挤出来的:窗内自身也在漂,若探针的拉伸因子偏差 e,窗尾会
/// 累计 e×win 的错位把峰抹平(10s 窗配 0.5% 的扫描步长 → 50ms,远小于 20ms 帧的
/// 判别力边界的几倍,可接受);窗再短则内容不够独特,容易匹配到别处。
const PROBE_WIN_S: f64 = 10.0;
/// 冷启动时的拉伸因子扫描范围与步长(还没有可信节点,只能扫)。
const BOOTSTRAP_SLOPES: (f64, f64, f64) = (0.85, 1.25, 0.005);
/// 模型已建立后,每个探针只在预测斜率附近小范围扫,兼顾"跟得上变化"与算量。
const TRACK_SLOPE_SPAN: f64 = 0.03;
const TRACK_SLOPE_STEP: f64 = 0.01;
/// 首个探针的位置:取靠前处——两轨都从录制开始的那一刻起,此处漂移必然接近 0,
/// 用它锚定起点比从中间猜起稳。
const FIRST_PROBE_S: f64 = 5.0;
/// 首探针搜索半径(未知偏移,放宽);后续探针由模型预测,收窄到 LATER_SEARCH_S。
const FIRST_SEARCH_S: f64 = 10.0;
const LATER_SEARCH_S: f64 = 3.0;
/// 探针接受门限:归一化互相关的绝对值,以及峰在整条相关曲线上的 z 分数。
/// 只看相关值会把"整段都有点像"的平坦高相关收进来,z 分数专治这种;只看 z 分数
/// 又会把一堆噪声里冒尖的那个收进来,两个一起卡。
///
/// z 的门限刻意压得低(2.0):真实录音里 mic 那份是经扬声器回采的,与 system 的
/// 数字直采隔着房间和 AEC,相关曲线本来就不尖——实测真笔记上正确命中的 z 多在
/// 2.5~5 之间,按合成信号的 z(17+)去卡会把八成正确命中全毙掉。判别力改由
/// 「两遍探测 + 稳健拟合」承担:单点错了也会在拟合阶段被邻居投票剔除。
const MIN_NCC: f32 = 0.30;
const MIN_PROMINENCE: f32 = 2.0;
/// 探针命中率下限。刻意不高:真实会议里总有整段"只有本端在说话"的时间——mic 有
/// 内容而 system 是静音,这种探针**本就无从对齐**,不是方法失效。判可信与否交给
/// 覆盖度(下面两条)、绝对节点数与邻域一致性,而不是靠抬高命中率。
///
/// 别为了提高覆盖而加密探针再放宽这条:实测 24→40 个探针时命中 12→16(比例
/// 50%→40%),放宽比例让更多边缘命中进来,而间距变小又使斜率带的容差
/// (2·MAX_RESID/间距)放宽,坏节点得以存活——残余错位从 0.50s 恶化到 20.6s。
/// 覆盖不足要靠定向补空档(见 refine_large_gaps),不是靠全局加密。
const MIN_ACCEPT_RATIO: f64 = 0.45;
/// 探针节点必须张开到整轨的多大范围,以及相邻节点之间最大允许多大的空档。
///
/// 张开度要求刻意不高(0.5):会议后半段常常整段只有本端在说话,两轨无从对齐,探针
/// 天然覆盖不到——实测那场 30 分钟笔记的探针只盖到 58%。撑住未覆盖段的是末端锚点,
/// 它是轨长差而非估计值;而跨越空档的那一段仍要过斜率带,真有第二次时基变化藏在
/// 里面也会被那条检查逮住。独立复核(外部工具、未参与本估计器构建)证实:该note
/// 未被探针覆盖的尾段,内插后在 t=1500s/1750s 处残余错位只有 0.01s/0.03s。
const MIN_SPAN_RATIO: f64 = 0.5;
const MAX_GAP_RATIO: f64 = 0.40;
/// 定稿节点数下限。
const MIN_KNOTS: usize = 8;
/// 空档超过这么长就去中点补一针;每场最多补这么多针,搜索半径这么大。
const GAP_REFINE_S: f64 = 200.0;
const GAP_REFINE_MAX: usize = 6;
const GAP_REFINE_SEARCH_S: f64 = 4.0;
/// RANSAC 判内点的容差,以及稳健拟合阶段允许的最大残差。
/// 前者宽(先把全局直线找出来),后者紧(定稿的节点必须准到听不出重影)。
const RANSAC_TOL: f64 = 2.5;
const MAX_RESID: f64 = 1.5;
/// 第二遍(以全局直线为中心重探)的搜索半径。
const REFINE_SEARCH_S: f64 = 6.0;
/// 拉伸因子扫描的外框。定稿节点另有一条更紧的、由数据自己定出来的带(见
/// `slope_band`),这里只管别让扫描跑到荒唐的地方去。
const SLOPE_RANGE: (f64, f64) = (0.80, 1.25);
/// 定稿斜率带在 [1, 轨长比] 两侧各放宽多少,容纳测量噪声与跨拐点的混合段。
const SLOPE_BAND_PAD: f64 = 0.02;

/// 定稿节点允许的局部斜率带。
///
/// 物理约束比"斜率落在 0.8~1.25"强得多:mic 的时钟要么是对的(斜率 1),要么被记错
/// 成某个固定的率(斜率 = 轨长比附近),中间值只会出现在跨拐点的那一段。据此把带收到
/// [min(1,k)-pad, max(1,k)+pad]——本例 k=1.0887 → [0.98, 1.109],尾部那个把 Δ 一
/// 口气推到 148s(局部斜率 1.13)的错配当场出局,而旧的宽区间放它过去了。
fn slope_band(k_global: f64) -> (f64, f64) {
    let (lo, hi) = (1.0f64.min(k_global), 1.0f64.max(k_global));
    (
        (lo - SLOPE_BAND_PAD).max(SLOPE_RANGE.0),
        (hi + SLOPE_BAND_PAD).min(SLOPE_RANGE.1),
    )
}

/// 反复剔掉使局部斜率越出带外的节点(标了 anchor 的不动),直到全部合规或节点不够。
/// 每轮只剔"最不合群"的那一个,避免一次误伤一串。
fn enforce_slope_band(
    mut knots: Vec<(f64, f64, bool)>,
    band: (f64, f64),
) -> Vec<(f64, f64, bool)> {
    // 越界幅度按间距缩放:两端节点各带 ±MAX_RESID 的定位误差,折算到斜率上就是
    // ±2·MAX_RESID/间距。间距 69s 时这一项有 0.043,比带本身的 pad 还大——不带上它
    // 就会把一堆本来正确、只是相邻噪声抵到一起的节点误杀(实测 14 个节点被砍到 7 个)。
    let excess = |s: f64, dt: f64| {
        let tol = 2.0 * MAX_RESID / dt.max(1.0);
        ((band.0 - tol) - s).max(s - (band.1 + tol)).max(0.0)
    };
    // 总越界:每次删一个节点,取"删完之后总越界最小"的那个。
    //
    // 不能按"这个节点两侧越界多大"来排:一段坏线段的两个端点罪责完全相同,并列时
    // 随手挑一个会挑中好的那端,删完坏段还在,于是一路连锁把好节点全啃掉(实测 14 个
    // 节点被啃到 7 个)。试删一遍再比总量,天然会挑中真正的那个坏点。
    let total = |ks: &[(f64, f64, bool)]| -> f64 {
        ks.windows(2)
            .map(|w| excess((w[1].1 - w[0].1) / (w[1].0 - w[0].0), w[1].0 - w[0].0))
            .sum()
    };
    loop {
        if knots.len() < 3 {
            return knots;
        }
        let cur = total(&knots);
        if cur <= 1e-9 {
            return knots;
        }
        let mut best: Option<(f64, usize)> = None;
        for i in 0..knots.len() {
            if knots[i].2 {
                continue; // 锚点不剔
            }
            let mut trial = knots.clone();
            trial.remove(i);
            let t = total(&trial);
            if best.map(|(b, _)| t < b).unwrap_or(true) {
                best = Some((t, i));
            }
        }
        match best {
            Some((t, i)) if t < cur - 1e-12 => {
                knots.remove(i);
            }
            _ => return knots,
        }
    }
}
/// 漂移小于此值不值得动:重采样本身有代价,且 0.5s 以内的错位听感上已经并成一句。
const MIN_DRIFT_SECS: f64 = 0.5;

/// mic 本地时间 → system 本地时间的分段线性映射(节点按时间升序,两轴均严格递增)。
///
/// 可序列化:估计一次要跑几秒,结果落到笔记目录的 align.json,读侧(转写段时间戳)
/// 与回放侧共用同一份,避免各算各的算出两条不一样的时基。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TimeMap {
    knots: Vec<(f64, f64)>,
}

/// 反序列化必须走 `TimeMap::new` 的同一套校验。
///
/// 直接 derive 会让手写/损坏的 align.json 造出空节点、单节点或非递增的 TimeMap,
/// 而 `apply`/`invert` 是按"至少两个节点、两轴严格递增"写的:空表索引 `pts[0]` 越界,
/// 单节点索引 `pts[1]` 越界,节点重合则相邻差为 0 → 除零。这些都是**读一个文件**就能
/// 触发的崩溃,必须在解析边界挡掉,不能指望调用方。
impl<'de> serde::Deserialize<'de> for TimeMap {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        struct Raw {
            knots: Vec<(f64, f64)>,
        }
        let raw = Raw::deserialize(d)?;
        if raw.knots.iter().any(|(a, b)| !a.is_finite() || !b.is_finite()) {
            return Err(serde::de::Error::custom("时基映射含非有限数"));
        }
        TimeMap::new(raw.knots)
            .ok_or_else(|| serde::de::Error::custom("时基映射节点不足两个或非严格递增"))
    }
}

impl TimeMap {
    /// 节点须已按 mic 时间升序且两轴严格递增;不足两个节点则无意义。
    pub fn new(knots: Vec<(f64, f64)>) -> Option<Self> {
        if knots.len() < 2 {
            return None;
        }
        if knots.windows(2).any(|w| w[1].0 <= w[0].0 || w[1].1 <= w[0].1) {
            return None;
        }
        Some(Self { knots })
    }

    fn interp(pts: &[(f64, f64)], x: f64, pick: impl Fn(&(f64, f64)) -> (f64, f64)) -> f64 {
        let first = pick(&pts[0]);
        let last = pick(&pts[pts.len() - 1]);
        // 两端按邻段斜率外推:录音首尾往往没有可锚定的内容,外推比夹住更贴近真相。
        if x <= first.0 {
            let n = pick(&pts[1]);
            let k = (n.1 - first.1) / (n.0 - first.0);
            return first.1 + (x - first.0) * k;
        }
        if x >= last.0 {
            let p = pick(&pts[pts.len() - 2]);
            let k = (last.1 - p.1) / (last.0 - p.0);
            return last.1 + (x - last.0) * k;
        }
        let i = pts.partition_point(|p| pick(p).0 <= x).max(1);
        let (a, b) = (pick(&pts[i - 1]), pick(&pts[i]));
        a.1 + (x - a.0) * (b.1 - a.1) / (b.0 - a.0)
    }

    /// mic 本地时间 → system 本地时间。
    pub fn apply(&self, mic_t: f64) -> f64 {
        Self::interp(&self.knots, mic_t, |p| (p.0, p.1))
    }

    /// system 本地时间 → mic 本地时间(两轴均严格递增,故可逆)。
    pub fn invert(&self, sys_t: f64) -> f64 {
        Self::interp(&self.knots, sys_t, |p| (p.1, p.0))
    }

    /// 相对「首节点那个固定偏移」的最大偏离——即错位随时间增长了多少。
    /// 固定偏移本身是 offset_ms 该管的事,不该由重采样来背。
    pub fn max_drift_secs(&self) -> f64 {
        let base = self.knots[0].1 - self.knots[0].0;
        self.knots
            .iter()
            .map(|(m, s)| ((s - m) - base).abs())
            .fold(0.0, f64::max)
    }

    /// 供诊断用(`estimate_on_real_note` 打印映射逐节点核对);生产路径只走 apply/invert。
    #[allow(dead_code)]
    pub fn knots(&self) -> &[(f64, f64)] {
        &self.knots
    }
}

/// 一次对齐估计的结果与它的成色(供调用方打日志/决定是否采纳)。
#[derive(Debug, Clone)]
pub struct Alignment {
    pub map: TimeMap,
    pub drift_secs: f64,
    pub probes: usize,
    pub accepted: usize,
}

/// 逐帧对数梅尔谱,按频带做过均值/方差归一(去掉两条链路的音色差,只留时间结构)。
struct Mel {
    frames: usize,
    /// 行优先:data[f * N_MEL + m]
    data: Vec<f32>,
}

/// 样本源抽象:让梅尔谱既能直接读 WAV 字节,也能读内存里的小片 f32。
///
/// 为什么不一律先 decode 成 `Vec<f32>`:一小时的轨解出来是 230MB,两轨就是 460MB,
/// 而此刻两份 WAV 字节本身还在(再 230MB)。梅尔谱只有 ~19MB——按需逐样本解码,
/// 峰值就从 ~700MB 掉到只剩两份字节。事后 drop 没用:峰值发生在算梅尔谱**期间**。
trait Samples {
    fn len(&self) -> usize;
    fn at(&self, i: usize) -> f32;
}

impl Samples for &[f32] {
    fn len(&self) -> usize {
        <[f32]>::len(self)
    }
    fn at(&self, i: usize) -> f32 {
        self[i]
    }
}

/// canonical WAV 字节上的只读样本视图(44 字节头 + 16bit LE 单声道)。
#[derive(Clone, Copy)]
struct Pcm<'a>(&'a [u8]);

impl Pcm<'_> {
    fn slice_to_vec(&self, a: usize, b: usize) -> Vec<f32> {
        (a..b.min(self.len())).map(|i| self.at(i)).collect()
    }
}

impl Samples for Pcm<'_> {
    fn len(&self) -> usize {
        self.0.len().saturating_sub(HEADER) / 2
    }
    fn at(&self, i: usize) -> f32 {
        let at = HEADER + i * 2;
        if at + 1 >= self.0.len() {
            return 0.0;
        }
        i16::from_le_bytes([self.0[at], self.0[at + 1]]) as f32 / 32768.0
    }
}

fn hz_to_mel(f: f32) -> f32 {
    2595.0 * (1.0 + f / 700.0).log10()
}
fn mel_to_hz(m: f32) -> f32 {
    700.0 * (10f32.powf(m / 2595.0) - 1.0)
}

/// 三角梅尔滤波器组,稠密存放(N_MEL × (NFFT/2+1))。
fn mel_filterbank() -> Vec<f32> {
    let bins = NFFT / 2 + 1;
    let mut fb = vec![0.0f32; N_MEL * bins];
    let (lo, hi) = (hz_to_mel(MEL_FMIN), hz_to_mel(MEL_FMAX));
    let pts: Vec<usize> = (0..N_MEL + 2)
        .map(|i| {
            let m = lo + (hi - lo) * i as f32 / (N_MEL + 1) as f32;
            (((NFFT + 1) as f32 * mel_to_hz(m) / SR as f32).floor() as usize).min(bins - 1)
        })
        .collect();
    for i in 0..N_MEL {
        let (l, mut c, mut r) = (pts[i], pts[i + 1], pts[i + 2]);
        c = c.max(l + 1).min(bins - 2);
        r = r.max(c + 1).min(bins - 1);
        for k in l..c {
            fb[i * bins + k] = (k - l) as f32 / (c - l) as f32;
        }
        for k in c..r {
            fb[i * bins + k] = (r - k) as f32 / (r - c) as f32;
        }
    }
    fb
}

fn log_mel<S: Samples>(samples: &S) -> Mel {
    let bins = NFFT / 2 + 1;
    let fb = mel_filterbank();
    let window: Vec<f32> = (0..NFFT)
        .map(|i| {
            0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / NFFT as f32).cos()
        })
        .collect();
    let frames = if samples.len() >= NFFT { 1 + (samples.len() - NFFT) / HOP } else { 0 };
    let mut data = vec![0.0f32; frames * N_MEL];
    if frames == 0 {
        return Mel { frames, data };
    }
    let mut planner = RealFftPlanner::<f32>::new();
    let r2c = planner.plan_fft_forward(NFFT);
    let mut buf = r2c.make_input_vec();
    let mut spec = r2c.make_output_vec();
    let mut power = vec![0.0f32; bins];
    for f in 0..frames {
        for (i, (d, w)) in buf.iter_mut().zip(window.iter()).enumerate() {
            *d = samples.at(f * HOP + i) * w;
        }
        if r2c.process(&mut buf, &mut spec).is_err() {
            return Mel { frames: 0, data: Vec::new() };
        }
        for (p, c) in power.iter_mut().zip(spec.iter()) {
            *p = c.re * c.re + c.im * c.im;
        }
        for m in 0..N_MEL {
            let e: f32 = fb[m * bins..(m + 1) * bins]
                .iter()
                .zip(power.iter())
                .map(|(w, p)| w * p)
                .sum();
            data[f * N_MEL + m] = (e + 1e-8).ln();
        }
    }
    normalize_bands(&mut data, frames);
    Mel { frames, data }
}

/// 逐频带减均值除标准差:两条链路(数字直采 vs 经扬声器回采)的音色/增益差异集中在
/// 频带的常数偏置上,归一后剩下的才是可比的时间结构。
fn normalize_bands(data: &mut [f32], frames: usize) {
    if frames == 0 {
        return;
    }
    let mut mean = [0.0f32; N_MEL];
    let mut sd = [0.0f32; N_MEL];
    for m in 0..N_MEL {
        mean[m] = (0..frames).map(|f| data[f * N_MEL + m]).sum::<f32>() / frames as f32;
        let var: f32 = (0..frames)
            .map(|f| (data[f * N_MEL + m] - mean[m]).powi(2))
            .sum::<f32>()
            / frames as f32;
        sd[m] = var.sqrt();
    }
    // 标准差夹板:某频带整段几乎没内容时(高频带在窄带素材里常见),除以它自己的
    // 微小标准差等于把量化噪声放大成满量程,一条噪声频带就能盖过真正有信息的频带。
    // 以各频带标准差的中位数为尺子,低于其 20% 的频带按 0 处理——不贡献,也不捣乱。
    let mut med = sd;
    med.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let floor = med[N_MEL / 2] * 0.2;
    for m in 0..N_MEL {
        if sd[m] <= floor || sd[m] <= 1e-6 {
            for f in 0..frames {
                data[f * N_MEL + m] = 0.0;
            }
        } else {
            for f in 0..frames {
                data[f * N_MEL + m] = (data[f * N_MEL + m] - mean[m]) / sd[m];
            }
        }
    }
}

/// 线性插值重采样一段音频:用于把 mic 探针窗按候选斜率拉伸后再比对。
/// 拉伸音频(而非只拉伸梅尔帧)是刻意的:时基压缩同时也把频谱整体搬了家,
/// 只在时间轴上插值会留下这段频率失配,峰值会被压低。
fn stretch(samples: &[f32], factor: f64) -> Vec<f32> {
    let out_len = ((samples.len() as f64) * factor) as usize;
    if samples.len() < 2 || out_len < 2 {
        return samples.to_vec();
    }
    (0..out_len)
        .map(|j| {
            let x = j as f64 / factor;
            let i = x.floor() as usize;
            let frac = (x - i as f64) as f32;
            let a = samples.get(i).copied().unwrap_or(0.0);
            let b = samples.get(i + 1).copied().unwrap_or(a);
            a + (b - a) * frac
        })
        .collect()
}

/// 把探针窗(已归一)在 system 梅尔谱的 [lo,hi) 起点范围内滑动,取归一化互相关最大处。
/// 返回 (最佳起点帧, ncc, 显著度)。
fn best_match(probe: &Mel, sys: &Mel, lo: usize, hi: usize) -> Option<(usize, f32, f32)> {
    let alen = probe.frames;
    if alen == 0 || sys.frames <= alen {
        return None;
    }
    let hi = hi.min(sys.frames - alen);
    if hi < lo {
        return None;
    }
    let anorm = probe.data.iter().map(|v| v * v).sum::<f32>().sqrt();
    if anorm <= 1e-6 {
        return None;
    }
    // system 每帧的能量前缀和 → 任意窗的范数 O(1)。
    let mut prefix = vec![0.0f64; sys.frames + 1];
    for f in 0..sys.frames {
        let e: f32 = sys.data[f * N_MEL..(f + 1) * N_MEL].iter().map(|v| v * v).sum();
        prefix[f + 1] = prefix[f] + e as f64;
    }
    let mut scores = Vec::with_capacity(hi - lo + 1);
    for k in lo..=hi {
        let bnorm = ((prefix[k + alen] - prefix[k]).max(1e-12)).sqrt() as f32;
        let dot: f32 = probe
            .data
            .iter()
            .zip(sys.data[k * N_MEL..(k + alen) * N_MEL].iter())
            .map(|(a, b)| a * b)
            .sum();
        scores.push(dot / (anorm * bnorm));
    }
    let (bi, &best) = scores
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))?;
    // 显著度用 z 分数而不是"峰/中位":无关窗的归一化相关值本来就散布在 0 附近,
    // 中位数随时可能穿过 0,比值会炸成任意大,失去判别力。
    let n = scores.len() as f32;
    let mean = scores.iter().sum::<f32>() / n;
    let sd = (scores.iter().map(|s| (s - mean).powi(2)).sum::<f32>() / n).sqrt();
    let z = if sd > 1e-6 { (best - mean) / sd } else { 0.0 };
    Some((lo + bi, best, z))
}

/// 在一组候选拉伸因子上找最佳匹配,返回最好的 (起点帧, ncc, z)。
///
/// 探针窗必须先按候选因子重采样再比对:窗内自身也在漂,拿原速的窗去比等于让窗尾
/// 错开 e×win,峰直接被抹平——实测 9% 压缩下 ncc 从 0.99 掉到 0.22,且最大值落在
/// 无关位置。拉伸音频而不是只拉伸梅尔帧,是因为时基压缩同时把频谱整体搬了家,
/// 只在时间轴上插值会留下这段频率失配。
///
/// `reacquire`(冷启动或上一个探针失配)时全范围粗扫再细化,否则只在预测斜率附近
/// 小范围扫。分两段是为了算量:全范围按细步长直扫要几十倍的相关计算。
fn scan_slopes(
    win: &[f32],
    sys: &Mel,
    lo: usize,
    hi: usize,
    predicted: f64,
    reacquire: bool,
) -> Option<(usize, f32, f32, f64)> {
    let try_one = |c: f64| -> Option<(usize, f32, f32)> {
        let stretched = stretch(win, c);
        let probe = log_mel(&stretched.as_slice());
        if probe.frames == 0 {
            return None;
        }
        best_match(&probe, sys, lo, hi)
    };
    let pick = |cands: Vec<f64>| -> Option<(f64, (usize, f32, f32))> {
        cands
            .into_iter()
            .filter_map(|c| try_one(c).map(|h| (c, h)))
            .max_by(|a, b| a.1 .1.partial_cmp(&b.1 .1).unwrap_or(std::cmp::Ordering::Equal))
    };
    let (c, h) = if reacquire {
        let (a, b, fine) = BOOTSTRAP_SLOPES;
        let coarse = fine * 4.0;
        let n = ((b - a) / coarse).round() as usize;
        let (c0, _) = pick((0..=n).map(|i| a + i as f64 * coarse).collect())?;
        // 细化:粗扫命中的邻域按细步长再来一遍。
        let k = (coarse / fine).round() as i64;
        pick((-k..=k).map(|i| c0 + i as f64 * fine).collect())?
    } else {
        let n = (TRACK_SLOPE_SPAN / TRACK_SLOPE_STEP).round() as i64;
        pick(
            (-n..=n)
                .map(|i| {
                    (predicted + i as f64 * TRACK_SLOPE_STEP)
                        .clamp(SLOPE_RANGE.0, SLOPE_RANGE.1)
                })
                .collect(),
        )?
    };
    Some((h.0, h.1, h.2, c))
}

/// 一次命中:(mic 时间, system 时间, 相关值, 该处最佳拉伸因子)。
type Hit = (f64, f64, f32, f64);

/// 在 t 处探一针。位置搜索以 `pred` 为中心 ±`search`,拉伸因子以 `slope` 为中心。
fn probe_at(
    mic: Pcm<'_>,
    sys: &Mel,
    t: f64,
    win: f64,
    pred: f64,
    slope: f64,
    search: f64,
    reacquire: bool,
) -> Option<Hit> {
    let a0 = (t * SR as f64) as usize;
    let a1 = (((t + win) * SR as f64) as usize).min(mic.len());
    if a1 <= a0 {
        return None;
    }
    let lo = (((pred - search) * SR as f64 / HOP as f64) as i64).max(0) as usize;
    let hi = (((pred + search) * SR as f64 / HOP as f64) as i64).max(0) as usize;
    let (k, ncc, z, c) = scan_slopes(&mic.slice_to_vec(a0, a1), sys, lo, hi, slope, reacquire)?;
    if ncc < MIN_NCC || z < MIN_PROMINENCE {
        return None;
    }
    Some((t, k as f64 * HOP as f64 / SR as f64, ncc, c))
}

#[derive(Clone, Copy, Debug)]
struct Line {
    slope: f64,
    intercept: f64,
}
impl Line {
    fn at(&self, t: f64) -> f64 {
        self.slope * t + self.intercept
    }
}

/// RANSAC 找贯穿全程的直线。第一遍的链式预测会把单点误差一路传下去,拿这条直线
/// 当第二遍的锚,误差就不再累积。要求内点占比 ≥40%,否则说明命中根本不共线。
fn ransac_line(hits: &[Hit]) -> Option<Line> {
    if hits.len() < 4 {
        return None;
    }
    let count = |l: &Line| hits.iter().filter(|h| (h.1 - l.at(h.0)).abs() <= RANSAC_TOL).count();
    let mut best: Option<(usize, Line)> = None;
    for i in 0..hits.len() {
        for j in i + 1..hits.len() {
            let dt = hits[j].0 - hits[i].0;
            if dt < 1e-6 {
                continue;
            }
            let slope = (hits[j].1 - hits[i].1) / dt;
            if slope < SLOPE_RANGE.0 || slope > SLOPE_RANGE.1 {
                continue;
            }
            let line = Line { slope, intercept: hits[i].1 - slope * hits[i].0 };
            let n = count(&line);
            if best.map(|(bn, _)| n > bn).unwrap_or(true) {
                best = Some((n, line));
            }
        }
    }
    let (n, line) = best?;
    if (n as f64) < 0.4 * hits.len() as f64 {
        return None;
    }
    // 内点最小二乘精修。
    let inl: Vec<&Hit> =
        hits.iter().filter(|h| (h.1 - line.at(h.0)).abs() <= RANSAC_TOL).collect();
    let n = inl.len() as f64;
    let (sx, sy) = inl.iter().fold((0.0, 0.0), |(x, y), h| (x + h.0, y + h.1));
    let (mx, my) = (sx / n, sy / n);
    let num: f64 = inl.iter().map(|h| (h.0 - mx) * (h.1 - my)).sum();
    let den: f64 = inl.iter().map(|h| (h.0 - mx).powi(2)).sum();
    if den <= 1e-9 {
        return Some(line);
    }
    let slope = (num / den).clamp(SLOPE_RANGE.0, SLOPE_RANGE.1);
    Some(Line { slope, intercept: my - slope * mx })
}

/// 邻域稳健拟合:每个命中取自身与前后各 2 个命中,用两两斜率中位数(Theil–Sen)
/// 定局部直线,再以中位截距定位。残差超 MAX_RESID 的点直接丢掉,留下的点取拟合值
/// 而非原始值——单个探针有 ±0.5s 级抖动,邻域投票能把它压下去,又不会像全局直线
/// 那样抹掉真实的拐点。
fn robust_smooth(hits: &[Hit]) -> Vec<(f64, f64)> {
    let n = hits.len();
    let mut out: Vec<(f64, f64)> = Vec::new();
    for i in 0..n {
        let w = &hits[i.saturating_sub(2)..(i + 3).min(n)];
        if w.len() < 3 {
            continue;
        }
        let mut slopes: Vec<f64> = Vec::new();
        for a in 0..w.len() {
            for b in a + 1..w.len() {
                let dt = w[b].0 - w[a].0;
                if dt > 1e-6 {
                    slopes.push((w[b].1 - w[a].1) / dt);
                }
            }
        }
        slopes.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
        let m = slopes[slopes.len() / 2];
        if !(SLOPE_RANGE.0..=SLOPE_RANGE.1).contains(&m) {
            continue;
        }
        let mut ic: Vec<f64> = w.iter().map(|h| h.1 - m * h.0).collect();
        ic.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
        let fit = m * hits[i].0 + ic[ic.len() / 2];
        if (hits[i].1 - fit).abs() > MAX_RESID {
            continue;
        }
        // 单调性是硬约束:时间不能倒流。
        if out.last().map(|l: &(f64, f64)| fit > l.1 && hits[i].0 > l.0).unwrap_or(true) {
            out.push((hits[i].0, fit));
        }
    }
    out
}

/// 用已接受的节点外推 t 处的 system 时间与局部斜率(最近 5 个节点最小二乘)。
fn predict(knots: &[(f64, f64)], fallback_slope: f64, t: f64) -> (f64, f64) {
    let last = *knots.last().expect("至少有锚点");
    if knots.len() < 3 {
        return (last.1 + (t - last.0) * fallback_slope, fallback_slope);
    }
    let tail = &knots[knots.len().saturating_sub(5)..];
    let n = tail.len() as f64;
    let (sx, sy) = tail.iter().fold((0.0, 0.0), |(x, y), p| (x + p.0, y + p.1));
    let (mx, my) = (sx / n, sy / n);
    let num: f64 = tail.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
    let den: f64 = tail.iter().map(|p| (p.0 - mx).powi(2)).sum();
    let slope = if den > 1e-9 {
        (num / den).clamp(SLOPE_RANGE.0, SLOPE_RANGE.1)
    } else {
        fallback_slope
    };
    (last.1 + (t - last.0) * slope, slope)
}


/// 在过大的节点间隔中点补探针。返回补过的映射;补不出任何有效点则返回 None。
fn refine_large_gaps(
    map: &TimeMap,
    mic: &Pcm<'_>,
    sys: &Mel,
    win: f64,
    mic_dur: f64,
    band: (f64, f64),
    debug: bool,
) -> Option<TimeMap> {
    let mut knots: Vec<(f64, f64)> = map.knots().to_vec();
    let mut added = 0;
    loop {
        // 每轮补当前最大的那个空档,补完重排再看下一个,直到都不超阈值。
        let (i, gap) = knots
            .windows(2)
            .enumerate()
            .map(|(i, w)| (i, w[1].0 - w[0].0))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))?;
        if gap <= GAP_REFINE_S || added >= GAP_REFINE_MAX {
            break;
        }
        let t = (knots[i].0 + knots[i + 1].0) / 2.0;
        if t + win > mic_dur {
            break;
        }
        // 局部斜率取该空档两端的连线,预测中心取当前映射——两者都比第一遍准。
        let slope = (knots[i + 1].1 - knots[i].1) / gap;
        let hit = probe_at(*mic, sys, t, win, map.apply(t), slope, GAP_REFINE_SEARCH_S, false);
        if debug {
            eprintln!(
                "补档 t={t:7.1} 空档={gap:5.0}s pred={:7.1} -> {}",
                map.apply(t),
                hit.map(|h| format!("sys={:.2} ncc={:.3}", h.1, h.2)).unwrap_or_else(|| "无".into())
            );
        }
        let Some(h) = hit else { break };
        // 新点必须让**两侧**的局部斜率都留在带内,否则它比空档更坏。
        let ok = [(knots[i], (h.0, h.1)), ((h.0, h.1), knots[i + 1])].iter().all(|(a, b)| {
            let k = (b.1 - a.1) / (b.0 - a.0);
            let tol = 2.0 * MAX_RESID / (b.0 - a.0).max(1.0);
            k >= band.0 - tol && k <= band.1 + tol
        });
        if !ok {
            break;
        }
        knots.insert(i + 1, (h.0, h.1));
        added += 1;
    }
    if added == 0 {
        return None;
    }
    if debug {
        eprintln!("补档新增 {added} 个节点");
    }
    TimeMap::new(knots)
}

/// 估计 mic → system 的时基映射。任一环节不可信即返回 None(调用方据此不纠正)。
///
/// `mic_off_ms`/`sys_off_ms` 只用来给首个探针一个起点猜测;返回的映射是两轨**本地**
/// 时间之间的关系。
pub fn estimate(
    mic_bytes: &[u8],
    mic_off_ms: u64,
    sys_bytes: &[u8],
    sys_off_ms: u64,
) -> Option<Alignment> {
    // 两轨都按字节视图读,不整轨解码成 f32(见 `Samples` 的注释:那一步是内存峰值的
    // 大头,而且事后 drop 也降不下来——峰值就发生在算梅尔谱期间)。
    let mic = Pcm(mic_bytes);
    let sys_pcm = Pcm(sys_bytes);
    let (mic_dur, sys_dur) = (mic.len() as f64 / SR as f64, sys_pcm.len() as f64 / SR as f64);
    if mic_dur < 30.0 || sys_dur < 30.0 {
        return None;
    }
    let sys = log_mel(&sys_pcm);
    if sys.frames == 0 {
        return None;
    }

    let step = (mic_dur / TARGET_PROBES).clamp(6.0, 90.0);
    let win = PROBE_WIN_S.min(step);
    let times: Vec<f64> = {
        let mut v = Vec::new();
        let mut t = FIRST_PROBE_S;
        while t + win <= mic_dur {
            v.push(t);
            t += step;
        }
        v
    };
    if times.len() < 5 {
        return None;
    }
    let debug = cfg!(test) && std::env::var("VN_ALIGN_DEBUG").is_ok();

    // ---- 第一遍:链式预测,粗定位 ----
    // 起始斜率猜测取轨长比:纯线性漂移时它就是真值,有拐点时也是个够近的起点。
    let mut slope = (sys_dur / mic_dur).clamp(SLOPE_RANGE.0, SLOPE_RANGE.1);
    // 两轨都从录制开始那一刻起,首探针处漂移接近 0,用 offset 差做猜测。
    // 方向由全局时间轴定死:global = mic_off + mic_local = sys_off + sys_local
    //   ⇒ sys_local = mic_local + (mic_off - sys_off)
    // 故 mic_local=0 处的 sys_local 是 **mic_off - sys_off**。续录、某一路授权延迟
    // 等场景会真产生非零 offset,方向写反会把整条映射平移两倍的 offset 差。
    let seed = (mic_off_ms as f64 - sys_off_ms as f64) / 1000.0;
    let mut coarse: Vec<Hit> = Vec::new();
    // 上一个探针失配后必须重新捕获:模型可能刚好在此处失效(进程重启换了时基,
    // 局部斜率一步跳 9%,远超跟踪时的小范围扫描),继续窄扫只会一路失配到底。
    let mut missed = true;
    for &t in &times {
        let knots: Vec<(f64, f64)> = coarse.iter().map(|h| (h.0, h.1)).collect();
        let (pred, local_slope) = if knots.is_empty() {
            (t + seed, slope)
        } else {
            predict(&knots, slope, t)
        };
        let reacquire = missed || knots.len() < 3;
        let search = if reacquire { FIRST_SEARCH_S } else { LATER_SEARCH_S };
        let hit = probe_at(mic, &sys, t, win, pred, local_slope, search, reacquire);
        if debug {
            eprintln!(
                "粗探 t={t:7.1} pred={pred:7.1} -> {}",
                hit.map(|h| format!("sys={:.2} ncc={:.3} c={:.4}", h.1, h.2, h.3))
                    .unwrap_or_else(|| "无".into())
            );
        }
        missed = true;
        if let Some(h) = hit {
            if coarse.last().map(|l| h.1 > l.1).unwrap_or(true) {
                coarse.push(h);
                missed = false;
                if coarse.len() >= 3 {
                    let k: Vec<(f64, f64)> = coarse.iter().map(|h| (h.0, h.1)).collect();
                    slope = predict(&k, slope, t).1;
                }
            }
        }
    }

    // ---- 第二遍:以全局直线为中心重探 ----
    // 位置锚在 RANSAC 直线上(不再链式累积误差),拉伸因子沿用该点第一遍自己选出的
    // 那个(它是局部真值,跨拐点也对;用全局直线的斜率反而会在拐点前把窗内抹平)。
    let line = ransac_line(&coarse)?;
    if debug {
        eprintln!("RANSAC 直线: sys = {:.5}·t {:+.2}", line.slope, line.intercept);
    }
    let mut refined: Vec<Hit> = Vec::new();
    for &t in &times {
        // 该点第一遍自己有命中就以它为中心窄搜:它是局部实测,比全局直线准
        // (直线在拐点附近能偏出 6s 以上,窄搜会把正确命中挡在窗外);单点万一
        // 错了,后面的邻域稳健拟合会把它剔掉。没命中的点才退回直线,并放宽搜索。
        let own = coarse.iter().find(|h| (h.0 - t).abs() < 1e-6);
        let (center, search, local_slope) = match own {
            Some(h) => (h.1, REFINE_SEARCH_S, h.3),
            None => (line.at(t), REFINE_SEARCH_S * 2.0, line.slope),
        };
        let hit = probe_at(mic, &sys, t, win, center, local_slope, search, false);
        if debug {
            eprintln!(
                "精探 t={t:7.1} pred={:7.1} -> {}",
                line.at(t),
                hit.map(|h| format!("sys={:.2} ncc={:.3}", h.1, h.2))
                    .unwrap_or_else(|| "无".into())
            );
        }
        if let Some(h) = hit {
            refined.push(h);
        }
    }

    // ---- 定稿:邻域稳健拟合 + 首尾锚点 + 斜率带 ----
    let smoothed = robust_smooth(&refined);
    let (probes, hits) = (times.len(), smoothed.len());
    if debug {
        eprintln!("命中 {hits}/{probes}");
    }
    if (hits as f64) < MIN_ACCEPT_RATIO * probes as f64 || hits < MIN_KNOTS {
        return None;
    }

    // 首尾锚点:两条轨由同一次会话开启、同一次停止关闭,覆盖同一段墙钟——因此
    // 起点错位 ≈ offset 差,终点错位 ≈ 轨长差,两者都不必估。这是整个估计里最硬的
    // 两条信息,尤其是终点:它把"末端漂移到底是多少"钉死,尾段证据薄弱时(会议后半
    // 段常常只有本端在说话,两轨无从对齐)不至于让个别错配把映射带跑。
    let mut knots: Vec<(f64, f64, bool)> = vec![(0.0, seed, true)];
    knots.extend(smoothed.iter().map(|&(m, s)| (m, s, false)));
    knots.push((mic_dur, sys_dur, true));
    knots.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    knots.dedup_by(|a, b| (a.0 - b.0).abs() < 1e-6);

    let band = slope_band(sys_dur / mic_dur);
    let knots = enforce_slope_band(knots, band);
    if debug {
        eprintln!("斜率带 [{:.3},{:.3}] 后余 {} 个节点", band.0, band.1, knots.len());
    }
    let kept = knots.iter().filter(|k| !k.2).count();
    if kept < MIN_KNOTS {
        return None;
    }
    // 覆盖度:探针节点之间(含锚点)不能留下大空档——空档只能线性内插,一旦其中
    // 又发生一次时基变化就插错了。
    let max_gap = knots.windows(2).map(|w| w[1].0 - w[0].0).fold(0.0, f64::max);
    // 张开度只算探针节点:锚点必然落在首尾,把它们算进来这条检查就永远成立。
    let probe_ts: Vec<f64> = knots.iter().filter(|k| !k.2).map(|k| k.0).collect();
    let span = probe_ts.last().copied().unwrap_or(0.0) - probe_ts.first().copied().unwrap_or(0.0);
    if span < MIN_SPAN_RATIO * mic_dur || max_gap > MAX_GAP_RATIO * mic_dur {
        if debug {
            eprintln!("覆盖不足: span={span:.0}s max_gap={max_gap:.0}s (轨长 {mic_dur:.0}s)");
        }
        return None;
    }
    let accepted = kept;
    // 定向补空档:大空档只能靠线性内插撑,而真实漂移曲线在其中未必线性——实测那场
    // 1045→1665s 的 619s 空档里,残余错位攀到 0.50s(其余各处 0.1~0.31s)。此时已有
    // 一张相当准的映射,拿它当预测中心重探空档中点,比第一遍的链式/直线预测准得多,
    // 之前失败的点这次往往能命中。全局加密探针不行:它同时放宽了斜率带的容差,
    // 坏节点会存活(实测残余从 0.50s 恶化到 20.6s)。
    let map = TimeMap::new(knots.iter().map(|&(m, s, _)| (m, s)).collect())?;
    let map = refine_large_gaps(&map, &mic, &sys, win, mic_dur, band, debug).unwrap_or(map);
    let drift_secs = map.max_drift_secs();
    Some(Alignment { map, drift_secs, probes, accepted })
}

/// 按映射重采样 mic 轨,输出 (canonical WAV 字节, 该轨起点在 system 本地时基上的秒数)。
///
/// 输出铺在 **system 的本地时基**上。起点不写死成 0:mic 若比 system 先开录
/// (mic_off < sys_off),它开头那段对应的 sys_local 是负的,从 0 起渲染会把这段内容
/// 整个丢掉。故起点取 `map.apply(0)`(可为负),调用方据此把轨的 offset_ms 相应前移
/// ——渲染后的轨在**全局**时间轴上的起点仍恰好是 mic 原来的起点,内容一个不少。
pub fn render_aligned_to<W: std::io::Write>(
    mic_bytes: &[u8],
    map: &TimeMap,
    sink: &mut W,
) -> std::io::Result<(u64, f64)> {
    let mic = Pcm(mic_bytes);
    let mic_dur = mic.len() as f64 / SR as f64;
    let start = map.apply(0.0);
    let out_len = (((map.apply(mic_dur) - start).max(0.0)) * SR as f64) as usize;
    let data_len = (out_len * 2) as u32;
    sink.write_all(&crate::store::audio::wav_header(data_len))?;
    // 分块写而不是先攒成一整个 Vec:一小时的轨 PCM 就是 115MB,旧写法还要再复制进
    // 带头的输出缓冲(共 230MB),四小时录音光这两份就近 1GB,而此刻两条源轨 mmap
    // 还活着。头的长度提前算得出,所以完全可以边算边写。
    const CHUNK: usize = 1 << 16;
    let mut buf = Vec::with_capacity(CHUNK * 2);
    for j in 0..out_len {
        let t = map.invert(start + j as f64 / SR as f64) * SR as f64;
        let v = if t < 0.0 || t >= (mic.len() as f64 - 1.0) {
            0.0
        } else {
            let i = t.floor() as usize;
            let frac = (t - i as f64) as f32;
            mic.at(i) + (mic.at(i + 1) - mic.at(i)) * frac
        };
        buf.extend_from_slice(&((v.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
        if buf.len() >= CHUNK * 2 {
            sink.write_all(&buf)?;
            buf.clear();
        }
    }
    if !buf.is_empty() {
        sink.write_all(&buf)?;
    }
    Ok((crate::store::audio::wav_header(0).len() as u64 + data_len as u64, start))
}

/// 渲染进内存(测试与诊断用;生产路径走 `render_aligned_to` 直接落盘,不留整轨副本)。
#[cfg(test)]
pub fn render_aligned(mic_bytes: &[u8], map: &TimeMap) -> (Vec<u8>, f64) {
    let mut out = Vec::new();
    let (_, start) = render_aligned_to(mic_bytes, map, &mut out).expect("写内存不会失败");
    (out, start)
}

/// 漂移是否大到值得纠正。
pub fn worth_correcting(a: &Alignment) -> bool {
    a.drift_secs >= MIN_DRIFT_SECS
}

/// 把 mic 轨的一个时间戳(毫秒)映射到 system 时基,负值夹到 0。
/// 给转写段/修订段落改时间戳用——段的时间戳是 u64,且负时间对展示无意义。
pub fn map_ms(map: &TimeMap, ms: u64) -> u64 {
    map_ms_signed(map, ms).max(0) as u64
}

/// 同上但保留负号。铺轨要用它:mic 比 system 先开录时,mic_local=0 对应的
/// sys_local 是负的,夹到 0 会把轨整体后移、开头那段被顶掉。
pub fn map_ms_signed(map: &TimeMap, ms: u64) -> i64 {
    (map.apply(ms as f64 / 1000.0) * 1000.0).round() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(samples: &[f32]) -> Vec<u8> {
        let mut pcm = Vec::with_capacity(samples.len() * 2);
        for s in samples {
            pcm.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
        }
        let mut out = crate::store::audio::wav_header(pcm.len() as u32).to_vec();
        out.extend_from_slice(&pcm);
        out
    }

    /// 造一段"像会议"的信号:频谱随时间不断变化的音节串 + 静默间隔。
    /// 纯噪声/纯定频都不行——前者互相关无峰,后者到处都是峰。
    fn speechy(secs: f64, seed: u64) -> Vec<f32> {
        let n = (secs * SR as f64) as usize;
        let mut out = vec![0.0f32; n];
        let mut rng = seed | 1;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let mut i = 0usize;
        while i < n {
            let syl = (SR / 20) + (next() % (SR as u64 / 4)) as usize; // 50~300ms
            let f0 = 90.0 + (next() % 120) as f32; // 基频
            // 两个共振峰在 300~3200Hz 间随音节游走:能量必须铺满整个梅尔带宽,
            // 否则高频带全是噪声,逐带归一后反而盖过真正有信息的低频带。
            let f1 = 300.0 + (next() % 700) as f32;
            let f2 = 1200.0 + (next() % 2000) as f32;
            let bw = 120.0 + (next() % 300) as f32;
            let end = (i + syl).min(n);
            for j in i..end {
                let t = (j - i) as f32 / SR as f32;
                let env = (std::f32::consts::PI * (j - i) as f32 / (end - i) as f32).sin();
                let mut v = 0.0;
                let mut h = 1;
                while (f0 * h as f32) < 7000.0 {
                    let f = f0 * h as f32;
                    // 共振峰包络:离 f1/f2 越近越响,给每个音节一张独特的频谱指纹。
                    let g = (-((f - f1) / bw).powi(2)).exp() + 0.7 * (-((f - f2) / bw).powi(2)).exp();
                    if g > 1e-3 {
                        v += g * (2.0 * std::f32::consts::PI * f * t).sin();
                    }
                    h += 1;
                }
                out[j] = 0.3 * env * v;
            }
            i = end + (next() % (SR as u64 / 8)) as usize; // 静默间隔
        }
        out
    }

    /// 匹配器自洽性:拿 system 自己的一段去 system 里找,必须找回原位。
    /// 这条不过就不必谈漂移估计了。
    #[test]
    fn matcher_finds_identical_window_at_its_own_position() {
        let s = speechy(120.0, 41);
        let sys = log_mel(&s.as_slice());
        for at in [12.0f64, 47.0, 88.0] {
            let a0 = (at * SR as f64) as usize;
            let a1 = a0 + 10 * SR;
            let probe = log_mel(&&s[a0..a1]);
            let (k, ncc, z) = best_match(&probe, &sys, 0, sys.frames).expect("应有匹配");
            let got = k as f64 * HOP as f64 / SR as f64;
            eprintln!("at={at} -> got={got:.2} ncc={ncc:.3} z={z:.2}");
            assert!((got - at).abs() < 0.1, "at={at} 实得 {got}");
            assert!(ncc > 0.8, "同一内容的相关值应接近 1,实得 {ncc}");
        }
    }

    #[test]
    fn timemap_interpolates_and_extrapolates() {
        let m = TimeMap::new(vec![(0.0, 0.0), (10.0, 11.0), (20.0, 22.0)]).unwrap();
        assert!((m.apply(5.0) - 5.5).abs() < 1e-9);
        assert!((m.apply(15.0) - 16.5).abs() < 1e-9);
        // 外推沿用邻段斜率(1.1)
        assert!((m.apply(30.0) - 33.0).abs() < 1e-9);
        assert!((m.apply(-10.0) + 11.0).abs() < 1e-9);
        // 往返
        for t in [0.5, 7.0, 13.0, 25.0] {
            assert!((m.invert(m.apply(t)) - t).abs() < 1e-6, "t={t}");
        }
        assert!((m.max_drift_secs() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn timemap_rejects_non_monotonic() {
        assert!(TimeMap::new(vec![(0.0, 0.0)]).is_none());
        assert!(TimeMap::new(vec![(0.0, 0.0), (10.0, 5.0), (5.0, 9.0)]).is_none());
        assert!(TimeMap::new(vec![(0.0, 0.0), (10.0, 11.0), (20.0, 10.0)]).is_none());
    }

    /// 全程线性压缩(采样率被谎报的形态):映射应把 mic 时间还原到 system 时间。
    #[test]
    fn recovers_linear_time_compression() {
        const K: f64 = 1.09; // system 时间 = 1.09 × mic 时间
        let sys = speechy(180.0, 7);
        let mic = stretch(&sys, 1.0 / K); // mic 被压短
        let a = estimate(&wav(&mic), 0, &wav(&sys), 0).expect("应估出映射");
        assert!(
            a.accepted as f64 >= 0.8 * a.probes as f64,
            "命中率 {}/{}",
            a.accepted,
            a.probes
        );
        for t in [20.0, 60.0, 100.0, 150.0] {
            let got = a.map.apply(t);
            assert!((got - t * K).abs() < 0.5, "t={t} 期望 {} 实得 {got}", t * K);
        }
        assert!(a.drift_secs > 10.0, "末端漂移应显著: {}", a.drift_secs);
        assert!(worth_correcting(&a));
    }

    /// 前段正常、中途才开始漂(进程重启换了时基的形态):映射必须是折线而不是一条直线。
    #[test]
    fn recovers_drift_that_starts_midway() {
        const KNEE: f64 = 60.0;
        const K: f64 = 1.09;
        let sys = speechy(240.0, 11);
        let knee_i = (KNEE * SR as f64) as usize;
        let mut mic = sys[..knee_i].to_vec();
        mic.extend(stretch(&sys[knee_i..], 1.0 / K));
        let a = estimate(&wav(&mic), 0, &wav(&sys), 0).expect("应估出映射");
        // 拐点前基本恒等
        for t in [20.0, 45.0] {
            assert!((a.map.apply(t) - t).abs() < 0.5, "t={t} 实得 {}", a.map.apply(t));
        }
        // 拐点后按 K 增长
        for t in [120.0, 200.0] {
            let want = KNEE + (t - KNEE) * K;
            assert!((a.map.apply(t) - want).abs() < 0.8, "t={t} 期望 {want} 实得 {}", a.map.apply(t));
        }
    }

    /// 非零 offset:映射的起点必须是 `mic_off - sys_off`(全局时间轴推出来的方向),
    /// 写反会把整条映射平移两倍 offset 差。两个方向都测。
    #[test]
    fn nonzero_offsets_seed_the_map_in_the_right_direction() {
        const K: f64 = 1.09;
        const LEAD: f64 = 8.0;
        // 造一条"全局"信号,两轨各截取它的一段——这样 offset 与音频内容才自洽
        // (直接给同一份内容配非零 offset 是自相矛盾的:内容说对齐在本地 0)。
        let g = speechy(200.0, 31);
        let cut = (LEAD * SR as f64) as usize;
        for (mic_off, sys_off, want) in
            [(8_000u64, 0u64, LEAD), (0u64, 8_000u64, -LEAD)]
        {
            // mic 晚开录 → mic 本地 0 落在 system 本地 +LEAD;反之落在 -LEAD。
            let (mic_src, sys_src): (&[f32], &[f32]) = if want > 0.0 {
                (&g[cut..], &g[..])
            } else {
                (&g[..], &g[cut..])
            };
            let mic = stretch(mic_src, 1.0 / K); // mic 轨被时基压缩
            let a = estimate(&wav(&mic), mic_off, &wav(sys_src), sys_off)
                .expect("应估出映射");
            let got = a.map.apply(0.0);
            assert!(
                (got - want).abs() < 0.6,
                "mic_off={mic_off} sys_off={sys_off}: 起点应为 {want:+.1}s,实得 {got:+.2}s"
            );
        }
    }

    /// mic 比 system 先开录时,mic 开头那段对应的 sys_local 是负的。渲染必须把它
    /// 渲进去并把起点如实报回(调用方据此前移 offset),否则这段内容整个丢掉。
    #[test]
    fn render_keeps_content_that_starts_before_the_system_track() {
        let map = TimeMap::new(vec![(0.0, -8.0), (100.0, 92.0)]).unwrap();
        let mic = speechy(100.0, 17);
        let (out, start) = render_aligned(&wav(&mic), &map);
        assert!((start + 8.0).abs() < 1e-6, "起点应如实报 -8s,实得 {start}");
        let out_secs = (out.len() - HEADER) as f64 / 2.0 / SR as f64;
        assert!(
            (out_secs - 100.0).abs() < 0.05,
            "整条 mic 都该渲进去(100s),实得 {out_secs:.2}s"
        );
        // 开头不该是静音:先开录的那段内容必须还在。
        let head: Vec<i16> = out[HEADER..HEADER + 2 * SR]
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert!(
            head.iter().any(|s| s.abs() > 200),
            "起点前移后开头应是真内容,不是被顶掉留下的静音"
        );
    }

    /// 两轨内容无关时必须返回 None——错误的"纠正"比不纠正坏得多。
    #[test]
    fn refuses_unrelated_tracks() {
        let sys = speechy(180.0, 3);
        let mic = speechy(180.0, 999);
        assert!(estimate(&wav(&mic), 0, &wav(&sys), 0).is_none());
    }

    /// 太短的轨不估(探针不够,结论不可信)。
    #[test]
    fn refuses_short_tracks() {
        let s = speechy(10.0, 5);
        assert!(estimate(&wav(&s), 0, &wav(&s), 0).is_none());
    }

    /// 真实笔记上的诊断:估出映射并打印各处漂移。合成信号只能证明算法自洽,
    /// 真录音才能证明它在真实声学条件(mic 那份是经扬声器回采的,音色/带宽都不同)下
    /// 也站得住。手动运行:
    /// `VN_ALIGN_NOTE=~/Documents/voice-notes/notes/20260804-180309 \
    ///  cargo test --lib player_align::tests::estimate_on_real_note -- --ignored --nocapture`
    #[test]
    #[ignore = "需要真实笔记目录与 afconvert;手动运行"]
    fn estimate_on_real_note() {
        let dir = std::path::PathBuf::from(
            std::env::var("VN_ALIGN_NOTE").expect("需设 VN_ALIGN_NOTE 指向笔记目录"),
        );
        let tmp = std::env::temp_dir().join("vn-align-probe");
        std::fs::create_dir_all(&tmp).unwrap();
        let decode = |src: &str| -> Vec<u8> {
            let dest = tmp.join(format!("{src}.wav"));
            crate::store::transcode::decode_m4a_to_standard_wav(
                &dir.join(format!("{src}.m4a")),
                &dest,
            )
            .expect("解码");
            std::fs::read(&dest).unwrap()
        };
        let (mic, sys) = (decode("mic"), decode("system"));
        let dur = |b: &[u8]| (b.len() - HEADER) as f64 / 2.0 / SR as f64;
        eprintln!("mic {:.1}s  system {:.1}s", dur(&mic), dur(&sys));

        let t0 = std::time::Instant::now();
        let a = estimate(&mic, 0, &sys, 0).expect("应估出映射");
        eprintln!(
            "估计耗时 {:?} | 探针 {}/{} 命中 | 最大漂移 {:.1}s",
            t0.elapsed(),
            a.accepted,
            a.probes,
            a.drift_secs
        );
        for (m, s) in a.map.knots() {
            eprintln!("  mic {m:7.1}s -> system {s:7.1}s   (Δ {:+6.2}s)", s - m);
        }
        let (fixed, _) = render_aligned(&mic, &a.map);
        eprintln!("纠正后 mic 轨 {:.1}s", dur(&fixed));
        // 落盘供外部工具独立复核(用估计器自己的指标验收是循环论证)。
        if let Ok(out) = std::env::var("VN_ALIGN_OUT") {
            std::fs::write(&out, &fixed).expect("写出纠正后音轨");
            eprintln!("纠正后音轨已写出: {out}");
        }
        // 验收必须用**独立**量法,不能再跑一遍 estimate:同一套探针几何会偏向自己的
        // 模型——实测它自测残余 0.31s,而下面这种短窗直测给出 0.86s,差了近 3 倍。
        //
        // 短窗(4s)+ 不扫拉伸:窗内残余速率误差最多贡献 0.03s,量到的就是纯滞后。
        let sysm = log_mel(&Pcm(&sys));
        let mut worst: f64 = 0.0;
        let mut lags: Vec<f64> = Vec::new();
        eprintln!("独立复核(短窗直测残余滞后):");
        let mut t = 60.0;
        while t < dur(&fixed) - 10.0 {
            const W: f64 = 4.0;
            let m = Pcm(&fixed);
            let (a0, a1) = ((t * SR as f64) as usize, ((t + W) * SR as f64) as usize);
            if a1 < m.len() {
                let probe = log_mel(&m.slice_to_vec(a0, a1).as_slice());
                let lo = (((t - 3.0) * SR as f64 / HOP as f64) as i64).max(0) as usize;
                let hi = (((t + 3.0) * SR as f64 / HOP as f64) as i64).max(0) as usize;
                if let Some((k, ncc, _)) = best_match(&probe, &sysm, lo, hi) {
                    if ncc >= 0.35 {
                        let lag = k as f64 * HOP as f64 / SR as f64 - t;
                        eprintln!("  t={t:7.1}s  残余 {lag:+6.2}s  ncc {ncc:.2}");
                        worst = worst.max(lag.abs());
                        lags.push(lag);
                    }
                }
            }
            t += 50.0;
        }
let mut sorted: Vec<f64> = lags.iter().map(|l| l.abs()).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = sorted.get(sorted.len() / 2).copied().unwrap_or(0.0);
        let within_gate = sorted.iter().filter(|l| **l < 0.4).count();
        eprintln!(
            "独立复核: n={} 中位残余 {:.2}s 最大 {:.2}s;落在门控 400ms 内 {}/{}",
            sorted.len(),
            median,
            worst,
            within_gate,
            sorted.len()
        );
        // 用中位数而不是最大值:这套短窗量法自身会误配(4s 语音很容易与别处相关,
        // 数据里那几个 ±2.5s 的点 ncc 并不低却明显不是真残余),拿最大值设线等于把
        // 量法的噪声当被测量。中位数对误配稳健,又能在映射真坏掉时立刻抬起来。
        //
        // 诚实交代:纠正前是 147.7s,现在中位 ~0.16s;但**全程压进门控 400ms 这个
        // 目标尚未被证实**——三种量法(本法、估计器自测、外部梅尔谱)在 0.2~0.9s
        // 区间互不吻合,分歧本身已达阈值量级。要判定达标,得先有一套可信到 0.1s
        // 的量法,那是另一件事。
        assert!(median < 0.3, "中位残余应 <0.3s,实得 {median:.2}s");
        assert!(
            within_gate * 2 >= sorted.len(),
            "过半采样点应落在门控 400ms 内,实得 {within_gate}/{}",
            sorted.len()
        );
    }

    /// 端到端:按估出的映射重采样后,两轨应当真的对齐——再估一次漂移必须塌到 ~0。
    #[test]
    fn render_aligned_actually_removes_the_drift() {
        const K: f64 = 1.09;
        let sys = speechy(180.0, 23);
        let mic = stretch(&sys, 1.0 / K);
        let (mic_wav, sys_wav) = (wav(&mic), wav(&sys));
        let a = estimate(&mic_wav, 0, &sys_wav, 0).expect("应估出映射");
        assert!(a.drift_secs > 10.0);

        let (fixed, _) = render_aligned(&mic_wav, &a.map);
        let b = estimate(&fixed, 0, &sys_wav, 0).expect("纠正后仍应能估");
        assert!(
            b.drift_secs < 0.5,
            "纠正后残余漂移应塌到 0 附近,实得 {}",
            b.drift_secs
        );
        assert!(!worth_correcting(&b), "纠正后不应再被判为值得纠正");
    }
}
