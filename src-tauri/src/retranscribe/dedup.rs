//! 离线回声去重:实时链路的回声去重靠墙钟 hold 计时(session.rs pending/recent 环),
//! 离线时全部数据已知,直接按时间轴重叠 + 文本相似判定跨轨重复。清洗后的 mic 轨
//! 大多已无回声,此层是兜底不是主力(spec §管线)。判中弃 mic 侧:system 路是
//! 数字信号原文,mic 路的重复必是声学回灌。

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
pub fn echo_discards(segs: &[DedupSeg]) -> Vec<usize> {
    let systems: Vec<&DedupSeg> = segs.iter().filter(|s| s.source == "system").collect();
    if systems.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (i, seg) in segs.iter().enumerate() {
        if seg.source != "mic" {
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
}
