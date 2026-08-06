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
    pub fn finish(&mut self) -> Vec<f32> {
        let out: Vec<f32> = self.win.drain(..).collect();
        self.win_start += out.len() as u64;
        out
    }

    fn drain_below_watermark(&mut self) -> Vec<f32> {
        let low = self.pos.iter().copied().min().unwrap_or(0);
        let watermark = low.saturating_sub(self.margin);
        if watermark <= self.win_start {
            return Vec::new();
        }
        let n = (watermark - self.win_start) as usize;
        let n = n.min(self.win.len());
        let out: Vec<f32> = self.win.drain(..n).collect();
        self.win_start += out.len() as u64;
        out
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
        m.accept(MIC, &[0.0, 0.0, 9.0]);
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
}
