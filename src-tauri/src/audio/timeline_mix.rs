//! 时间轴混音核心:按**时间轴位置**索引累加,按水位线定稿。
//!
//! 为什么不按到达顺序:两路采集线程独立,块大小与到达时刻都不可控。按到达顺序配对
//! (meetily 的 `can_mix()` 用 `||` + 零填充)会在某路滞后时拿静音顶替一窗,等真数据
//! 到达已与更晚的对面窗错配,且错位不可恢复。这里每块样本的位置是**算出来的**——
//! `pos = 该源已接受样本数`(调用方保证喂进来的是 post-frame_tap 流,断流已补零帧,
//! 故样本数即时间轴位置),因此位置从不靠推断。
//!
//! 定稿判据:水位线 = `min(各源位置) − margin`。低于水位的位置两源都不可能再来数据,
//! 可安全定稿;margin 吸收两路到达时刻的抖动。
//!
//! 窗口大小无隐式上限:恒有 `win.len() == max(各源位置) − min(各源位置)`,这个差值
//! 是否有界由调用方保证,本模块不负责兜底。稳态(两源等速推进)下差值收敛到
//! margin,健康;但若某一源彻底停止喂料(不是流内空洞——那由上游 frame_tap 补零帧
//! 覆盖了,而是 tap 本身死了、不再产生任何帧),`min` 冻结、另一源持续追加,差值会
//! 单调增长且没有上限(实测 60s 单源停喂 ≈ 3.8MB,约 230MB/小时)。这里不加限流或
//! 丢弃逻辑去"治"这种情况:静默丢音频是比内存增长更坏的失败模式,有界性必须在
//! 更上游(tap 存活性)保证。

/// 源下标。只有两源,用定长数组避免哈希开销(混音在录制热路径的旁路上)。
pub const MIC: usize = 0;
pub const SYSTEM: usize = 1;
const NSRC: usize = 2;

/// 水位线安全余量默认值:400ms @16k。与 player_gate 的 system 回看窗同量级,
/// 覆盖实测 165~245ms 声学回路延迟与设备抖动。待实测校准的初值,不是定论。
pub const DEFAULT_MARGIN_SAMPLES: u64 = 6400;

pub struct TimelineMixer {
    /// 每源已接受样本数 = 该源在时间轴上的当前写入位置。
    pos: [u64; NSRC],
    /// 累加窗起点(时间轴样本号)。窗内 win[i] 对应位置 win_start + i。
    win_start: u64,
    win: Vec<f32>,
    margin: u64,
}

impl TimelineMixer {
    pub fn new(margin_samples: u64) -> Self {
        Self { pos: [0; NSRC], win_start: 0, win: Vec::new(), margin: margin_samples }
    }

    /// 接受某源一块样本,返回本次新定稿的连续样本(从旧 win_start 起)。
    pub fn accept(&mut self, src: usize, samples: &[f32]) -> Vec<f32> {
        debug_assert!(src < NSRC, "src 越界: {src} >= NSRC({NSRC})");
        // 下面两处 u64 减法(`end - win_start`、`start - win_start`)安全的前提是
        // win_start <= pos[src]。drain_below_watermark 严格维持这个不变式;finish
        // 也维持(见 finish 文档)。这里断言把契约钉死,一旦有代码改动破坏它,debug
        // 构建立刻炸,而不是留到 release 下减法回绕成 Vec::resize 的天文 capacity。
        debug_assert!(
            self.pos[src] >= self.win_start,
            "pos[{src}]={} < win_start={}: 不变式已被打破",
            self.pos[src],
            self.win_start
        );
        let start = self.pos[src];
        // 按位置累加进窗(窗不足则补 0.0 扩容——那些位置只是还没有任何源写过)。
        let end = start + samples.len() as u64;
        let need = (end - self.win_start) as usize;
        if self.win.len() < need {
            self.win.resize(need, 0.0);
        }
        let base = (start - self.win_start) as usize;
        for (i, s) in samples.iter().enumerate() {
            self.win[base + i] += *s;
        }
        self.pos[src] = end;
        self.drain_below_watermark()
    }

    /// 收尾:两源都不再来数据,窗内剩余全部定稿。
    ///
    /// 终态语义:这是给"两源都停了"用的收尾调用,但调用后并不禁止再 `accept`——
    /// 录制停止时两源位置通常不齐(一源比另一源多攒了几帧),硬性禁止只是把问题挪给
    /// 调用方。这里选择留活口:再 accept 不会 panic,但那些迟到样本会退化为**追加
    /// 在窗尾**,不再具备原来的时间轴位置语义(因为对应的位置窗口已经关闭并吐出)。
    ///
    /// 实现上,这要求把所有源的 `pos` 推到 `max(pos)`,以维持 `win_start <= pos[src]`
    /// 这一 `accept` 依赖的不变式——否则落后源下次 accept 时 `pos[src] - win_start`
    /// 直接下溢(release 下 `need` 回绕成 ~2^64,`Vec::resize` capacity overflow)。
    pub fn finish(&mut self) -> Vec<f32> {
        let out: Vec<f32> = self.win.drain(..).collect();
        self.win_start += out.len() as u64;
        // 恢复不变式:把落后源的位置拉平到 max(pos),而不是让它继续 < win_start。
        let max_pos = self.pos.iter().copied().max().expect("pos 长度固定为 NSRC,max 不可能为 None");
        for p in self.pos.iter_mut() {
            *p = max_pos;
        }
        out
    }

    fn drain_below_watermark(&mut self) -> Vec<f32> {
        let low = self.pos.iter().copied().min().expect("pos 长度固定为 NSRC,min 不可能为 None");
        let watermark = low.saturating_sub(self.margin);
        if watermark <= self.win_start {
            return Vec::new();
        }
        let n = (watermark - self.win_start) as usize;
        // n <= win.len() 恒成立:watermark <= low <= max(pos) == win_start + win.len()
        // (窗口右端恒等于目前见过的最大位置)。之前的 `.min(win.len())` 是不可达的
        // 死防御,换成断言把它从"掩盖"变成"锁定"。
        debug_assert!(n <= self.win.len(), "watermark 越过窗口右端,不变式被打破: n={n} win.len()={}", self.win.len());
        let out: Vec<f32> = self.win.drain(..n).collect();
        self.win_start += out.len() as u64;
        out
    }
}

#[cfg(test)]
impl TimelineMixer {
    /// 仅测试可见:当前窗口长度。用于锁定"稳态下窗口大小恒为 margin,不会偷偷
    /// 变大"这条性质,不为此放开生产 API。
    fn win_len(&self) -> usize {
        self.win.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// f32 逐元素容差比较。不用 assert_eq! 精确比:和式(如 1.0+0.1)是否恰好等于
    /// 字面量 1.1f32 取决于舍入,断言不该赌这个。
    fn assert_close(got: &[f32], want: &[f32]) {
        assert_eq!(got.len(), want.len(), "长度不符: got {got:?} want {want:?}");
        for (g, w) in got.iter().zip(want) {
            assert!((g - w).abs() < 1e-6, "got {got:?} want {want:?}");
        }
    }

    /// 两源等速到达:定稿部分应是逐样本和。
    #[test]
    fn equal_rate_sources_sum_pointwise() {
        let mut m = TimelineMixer::new(0); // margin=0 便于逐样本断言
        assert!(m.accept(MIC, &[1.0, 1.0, 1.0]).is_empty(), "只有一源时水位线为 0,不该定稿");
        assert_close(&m.accept(SYSTEM, &[0.5, 0.5, 0.5]), &[1.5, 1.5, 1.5]);
    }

    /// 一源滞后:滞后期间不定稿;补上后按位置对齐,不与更晚的对面窗错配。
    #[test]
    fn lagging_source_aligns_by_position_not_arrival() {
        let mut m = TimelineMixer::new(0);
        // mic 先跑 4 个样本,system 一个都没来
        assert!(m.accept(MIC, &[1.0, 2.0, 3.0, 4.0]).is_empty());
        // system 追上前 2 个 → 只定稿前 2 个位置
        assert_close(&m.accept(SYSTEM, &[0.1, 0.2]), &[1.1, 2.2]);
        // system 再追 2 个 → 定稿第 3、4 个位置(而非和 mic 后来的样本错配)
        assert_close(&m.accept(SYSTEM, &[0.3, 0.4]), &[3.3, 4.4]);
    }

    /// 缺口:某源在时间轴上有空洞(frame_tap 已补零帧,此处等价于喂 0.0),
    /// 另一源内容原样保留,位置不漂移。
    #[test]
    fn silent_fill_does_not_shift_positions() {
        let mut m = TimelineMixer::new(0);
        assert!(m.accept(MIC, &[0.0, 0.0, 9.0]).is_empty(), "只有一源时水位线为 0,不该定稿");
        assert_close(&m.accept(SYSTEM, &[1.0, 1.0, 1.0]), &[1.0, 1.0, 10.0]);
    }

    /// 不等长块 + 交替到达:定稿序列与位置严格对应。
    #[test]
    fn uneven_chunks_keep_positional_correspondence() {
        let mut m = TimelineMixer::new(0);
        let mut all = Vec::new();
        all.extend(m.accept(MIC, &[0.1])); // 水位线仍为 0,空
        all.extend(m.accept(SYSTEM, &[0.01, 0.02, 0.03])); // 水位线到 1 → 定稿位置 0
        all.extend(m.accept(MIC, &[0.2, 0.3])); // 水位线到 3 → 定稿位置 1、2
        all.extend(m.finish());
        // 位置 0 = 0.1+0.01,位置 1 = 0.2+0.02,位置 2 = 0.3+0.03
        assert_eq!(all.len(), 3);
        for (got, want) in all.iter().zip([0.11_f32, 0.22, 0.33]) {
            assert!((got - want).abs() < 1e-6, "got {all:?}");
        }
    }

    /// 水位线余量:margin 之内的位置不定稿,留给尚未到达的样本。
    #[test]
    fn margin_holds_back_recent_positions() {
        let mut m = TimelineMixer::new(2);
        m.accept(MIC, &[1.0, 1.0, 1.0, 1.0]);
        // 两源 min 位置 = 4,减 margin 2 → 只定稿位置 0、1
        assert_close(&m.accept(SYSTEM, &[1.0, 1.0, 1.0, 1.0]), &[2.0, 2.0]);
        // finish 把剩下的全部吐出
        assert_close(&m.finish(), &[2.0, 2.0]);
    }

    /// 核心**不**钳制:溢出交给落盘侧的 f32_to_s16(既有,已 clamp)。
    /// 混音器是纯加法,钳制是存储层关注点;两处都钳会让核心的单测无法用直观数值断言,
    /// 也掩盖"两路相加真的触顶了"这一诊断信号。
    #[test]
    fn sum_is_not_clamped_here() {
        let mut m = TimelineMixer::new(0);
        m.accept(MIC, &[0.9, -0.9]);
        let out = m.accept(SYSTEM, &[0.9, -0.9]);
        assert_eq!(out.len(), 2);
        assert!((out[0] - 1.8).abs() < 1e-6, "got {out:?}");
        assert!((out[1] + 1.8).abs() < 1e-6, "got {out:?}");
    }

    /// Critical 回归:`finish()` 之后落后源再 accept 不能 panic。修复前,若某源在
    /// finish 时落后(pos < 即将成为 win_start 的位置),它下次 accept 会在 u64 减法
    /// 上下溢(debug panic / release capacity overflow)。这里复现"mic 领先、system
    /// 完全没喂过就停录"的场景,然后让 system 迟到——迟到样本不再具备原时间轴位置
    /// 语义,只是安全地追加到窗尾。
    #[test]
    fn accept_after_finish_appends_at_tail_without_panicking() {
        let mut m = TimelineMixer::new(0);
        assert!(m.accept(MIC, &[1.0, 2.0, 3.0]).is_empty(), "system 未喂过,水位线为 0");
        assert_close(&m.finish(), &[1.0, 2.0, 3.0]);

        // system 落后 3 个位置迟到:修复前这里必 panic。
        assert!(m.accept(SYSTEM, &[9.0]).is_empty(), "margin=0 且 mic 未再推进,不会立即定稿");
        assert_close(&m.finish(), &[9.0]);
    }

    /// 空切片不是特殊情况,不该 panic、也不该产生任何定稿输出。
    #[test]
    fn accept_empty_slice_does_not_panic() {
        let mut m = TimelineMixer::new(0);
        assert!(m.accept(MIC, &[]).is_empty());
        assert!(m.accept(SYSTEM, &[]).is_empty());
    }

    /// Important 回归:稳态(两源等速推进)下窗口大小应恒等于 margin,不会无界增长。
    /// 这条锁住的是"看似无害的改动让窗口开始偷偷攒料"这一类未来回归。
    #[test]
    fn steady_state_window_len_stays_at_margin() {
        let margin = 100u64;
        let mut m = TimelineMixer::new(margin);
        let chunk = [0.1_f32; 10];
        for round in 0..1000u64 {
            m.accept(MIC, &chunk);
            m.accept(SYSTEM, &chunk);
            // 前几轮窗口还在从 0 填到 margin,尚未进入稳态,不适用这条断言。
            if (round + 1) * chunk.len() as u64 >= margin {
                assert_eq!(
                    m.win_len(),
                    margin as usize,
                    "稳态下 win 长度应恒为 margin,round={round}"
                );
            }
        }
    }
}
