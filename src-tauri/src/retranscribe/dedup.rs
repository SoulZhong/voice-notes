//! 离线回声去重:实时链路的回声去重靠墙钟 hold 计时(session.rs pending/recent 环),
//! 离线时全部数据已知,直接按时间轴重叠 + 文本相似判定跨轨重复。清洗后的 mic 轨
//! 大多已无回声,此层是兜底不是主力(spec §管线)。判中弃 mic 侧:system 路是
//! 数字信号原文,mic 路的重复必是声学回灌。

use crate::player_gate::GateSpan;
use crate::session::{overlap_fraction, text_similarity};

/// 时间重叠占比下限。`overlap_fraction` 的分母是第一个参数(此处即 mic 段)自身
/// 时长,不是较短段/并集——实读 session.rs:138-149 确认。初值,单测锁死;实测
/// 误杀/漏杀后与 SIM 一并校准。
pub const ECHO_OVERLAP_MIN: f32 = 0.5;
/// 归一化文本相似度下限。`text_similarity` 内部自行 `normalize_text`,故此处
/// 直接传原文,不预归一化(实读 session.rs:104-129 确认)。
pub const ECHO_SIM_MIN: f32 = 0.75;

pub struct DedupSeg<'a> {
    pub source: &'a str,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: &'a str,
}

/// 返回应弃用的段下标(恒为 mic 段)。O(mic × system),会议级段数(千段内)无压力。
///
/// 占位段("[识别失败]",`super::ASR_FAILED_PLACEHOLDER`)不参与回声比对,双向,
/// 语义对齐实时链路 session.rs push_system_sub 的同款判据(约 687-690 行、
/// 753-757 行注释:「占位段不参与——双路同时识别失败文本雷同,会互相误
/// 杀」):
/// 1) mic 侧占位段本身就不可能是"回声"(它是识别失败的痕迹,不是复述内容),
///    直接跳过,不可能被弃;
/// 2) system 侧占位段不能作为"这是回声"的证据——构建参考列表时先排除,
///    避免 mic 侧一段恰好与占位串"相似"的正常文本被误判为回声丢弃。
pub fn echo_discards(segs: &[DedupSeg]) -> Vec<usize> {
    let systems: Vec<&DedupSeg> = segs
        .iter()
        .filter(|s| s.source == "system" && s.text != super::ASR_FAILED_PLACEHOLDER)
        .collect();
    if systems.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (i, seg) in segs.iter().enumerate() {
        if seg.source != "mic" {
            continue;
        }
        // mic 侧占位段不可能被判定为回声:双路同时识别失败时文本雷同(都是
        // 占位串)又时间邻近,照常比对会把它误判为回声弃用,静默吞掉一段
        // 真实发声。
        if seg.text == super::ASR_FAILED_PLACEHOLDER {
            continue;
        }
        let hit = systems.iter().any(|sys| {
            overlap_fraction(seg.start_ms, seg.end_ms, sys.start_ms, sys.end_ms) >= ECHO_OVERLAP_MIN
                && text_similarity(seg.text, sys.text) >= ECHO_SIM_MIN
        });
        if hit {
            out.push(i);
        }
    }
    out
}

/// 电平门覆盖率阈值:mic 段落在压低区间内的时长占比 ≥ 此值即弃。初值 0.6:
/// 压低区间经 300ms 合并 + 200ms 短区丢弃,粒度粗于段,阈值过高会漏(单测锁死,
/// 沙箱实测校准)。
pub const GATE_COVER_MIN: f32 = 0.6;

/// mic 段在 spans(时间轴样本域,player_gate::build_gate_from_pcm 产出)内的覆盖
/// 占比。纯函数:段 [start_ms*16, end_ms*16) 与各 span 求交集样本和 / 段长。依赖
/// spans 有序不重叠——player_gate 的合并逻辑保证这一点,此处不再重新排序/去重。
pub fn gate_coverage(spans: &[GateSpan], start_ms: u64, end_ms: u64) -> f32 {
    let seg_start = start_ms * 16;
    let seg_end = end_ms * 16;
    if seg_end <= seg_start {
        return 0.0;
    }
    let covered: u64 = spans
        .iter()
        .map(|sp| {
            let lo = sp.start.max(seg_start);
            let hi = sp.end.min(seg_end);
            hi.saturating_sub(lo)
        })
        .sum();
    covered as f32 / (seg_end - seg_start) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg<'a>(source: &'a str, start: u64, end: u64, text: &'a str) -> DedupSeg<'a> {
        DedupSeg { source, start_ms: start, end_ms: end, text }
    }

    /// 时间重叠 + 文本相同的 mic/system 对 → 弃 mic 侧。
    #[test]
    fn overlapping_identical_pair_drops_mic() {
        let segs = [
            seg("system", 1000, 5000, "今天讨论发布计划的三个风险点"),
            seg("mic", 1200, 5200, "今天讨论发布计划的三个风险点"),
        ];
        assert_eq!(echo_discards(&segs), vec![1]);
    }

    /// 文本相似但时间不重叠(隔了很久重复同一句)→ 不弃。
    #[test]
    fn similar_text_without_overlap_kept() {
        let segs = [
            seg("system", 1000, 3000, "今天讨论发布计划的三个风险点"),
            seg("mic", 60_000, 62_000, "今天讨论发布计划的三个风险点"),
        ];
        assert!(echo_discards(&segs).is_empty());
    }

    /// 时间重叠但文本不同(真双讲)→ 不弃。
    #[test]
    fn overlapping_different_text_kept() {
        let segs = [
            seg("system", 1000, 5000, "今天讨论发布计划的三个风险点"),
            seg("mic", 1200, 5200, "我觉得数据库迁移得再排期"),
        ];
        assert!(echo_discards(&segs).is_empty());
    }

    /// mic/mic 或 system/system 同源相似不互杀;mixed 模式(无 system 段)天然零弃用。
    #[test]
    fn same_source_and_mixed_never_dropped() {
        let segs = [
            seg("mic", 1000, 3000, "同一句话"),
            seg("mic", 1100, 3100, "同一句话"),
            seg("mixed", 1000, 3000, "同一句话"),
        ];
        assert!(echo_discards(&segs).is_empty());
    }

    /// 占位段("[识别失败]")不参与回声比对,双向:
    /// 1) mic 侧为占位段、system 侧也是占位段(双路同时识别失败,文本雷同
    ///    但都是"确有发声但识别失败"的痕迹,不是回声)→ 不弃,两段都保留;
    /// 2) system 侧为占位段、mic 侧是与占位串"相似"的正常文本 → 占位段
    ///    不能作为回声证据,同样不弃。
    /// 语义对齐实时链路 session.rs push_system_sub 的同款判据(694-701 行 /
    /// 753-757 行注释:「占位段不参与——双路同时识别失败文本雷同,会互相
    /// 误杀」)。
    #[test]
    fn placeholder_segments_excluded_from_echo_match() {
        // 双路同时识别失败:时间重叠 + 文本同为占位符,不得互杀。
        let both_failed = [
            seg("system", 1000, 5000, super::super::ASR_FAILED_PLACEHOLDER),
            seg("mic", 1200, 5200, super::super::ASR_FAILED_PLACEHOLDER),
        ];
        assert!(echo_discards(&both_failed).is_empty());

        // system 侧占位、mic 侧是正常文本(非占位符本身)但与占位串"[识别
        // 失败]"高度相似(此处去掉方括号后即被 contains 捷径判为 1.0)——
        // 占位段不作参考证据,不弃。
        let sys_placeholder = [
            seg("system", 1000, 5000, super::super::ASR_FAILED_PLACEHOLDER),
            seg("mic", 1200, 5200, "识别失败"),
        ];
        assert!(echo_discards(&sys_placeholder).is_empty());
    }

    fn span(start_ms: u64, end_ms: u64) -> GateSpan {
        GateSpan { start: start_ms * 16, end: end_ms * 16 }
    }

    /// 空 spans → 覆盖率恒 0(与 player_gate 降级口径一致:门不开就不弃)。
    #[test]
    fn gate_coverage_empty_spans_is_zero() {
        assert_eq!(gate_coverage(&[], 1000, 2000), 0.0);
    }

    /// 段完全落在单个 span 内 → 覆盖率 1。
    #[test]
    fn gate_coverage_full_overlap_is_one() {
        let spans = [span(500, 3000)];
        assert_eq!(gate_coverage(&spans, 1000, 2000), 1.0);
    }

    /// 段一半落在 span 内 → 覆盖率 0.5。
    #[test]
    fn gate_coverage_half_overlap_is_half() {
        let spans = [span(1000, 1500)];
        assert_eq!(gate_coverage(&spans, 1000, 2000), 0.5);
    }

    /// 段跨多个不相邻 span,覆盖率为各交集之和 / 段长。
    #[test]
    fn gate_coverage_sums_across_multiple_spans() {
        // 段 [1000,2000)(长 1000ms),span1 覆盖 [1000,1300)=300ms,span2 覆盖
        // [1700,2000)=300ms,中间 [1300,1700) 不覆盖 → 合计 600/1000=0.6。
        let spans = [span(1000, 1300), span(1700, 2000)];
        assert_eq!(gate_coverage(&spans, 1000, 2000), 0.6);
    }

    /// 零长段(start_ms == end_ms)防 0 除,返回 0 而非 NaN/panic。
    #[test]
    fn gate_coverage_zero_length_segment_is_zero() {
        let spans = [span(500, 3000)];
        assert_eq!(gate_coverage(&spans, 1500, 1500), 0.0);
    }
}
