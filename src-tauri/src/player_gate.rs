//! 回放跨轨门控(纯函数):按转写段活跃度构建 mic 轨压低区间,消混音重影。
//! 设计见 docs/superpowers/specs/2026-07-14-voice-notes-playback-crossgate-design.md。
//! 根因:软件 AEC 场次清洗后 mic 仍有 ~-25dB 对方残影,与 system 全电平同内容
//! 混播成"同源迟到复本"重影(真实笔记实测残余互相关 0.128@170ms)。
//! 门控单向只压 mic;双讲保护;一切失败空表降级=现状。

use serde::Deserialize;
use std::path::Path;

/// -15dB:残影压到混音不可闻,VAD 漏标的真人声不全丢(用户定,零配置)。
pub const DUCK_GAIN: f32 = 0.178;
/// 渐变沿 80ms@16k,落在区间内侧,防咔嗒。
const RAMP_SAMPLES: u64 = 1280;
/// 相邻压低区间间隙 <300ms 合并,防增益颤振。
const MERGE_GAP_MS: u64 = 300;
/// 孤立 <200ms 压低区间丢弃,不值得动增益。
const MIN_SPAN_MS: u64 = 200;
const SAMPLES_PER_MS: u64 = 16;

#[derive(Debug, Clone, PartialEq)]
pub struct GateSpan {
    pub start: u64,
    pub end: u64,
}

#[derive(Deserialize)]
struct SegRow {
    source: String,
    start_ms: u64,
    end_ms: u64,
}

/// 容错解析 segments.jsonl:坏行跳过,缺文件返回空(降级=现状)。
pub fn parse_segments_jsonl(path: &Path) -> Vec<(String, u64, u64)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str::<SegRow>(l).ok())
        .map(|r| (r.source, r.start_ms, r.end_ms))
        .collect()
}

/// (start_ms,end_ms) 区间列表求并集(输入无序容忍)。
fn union_ms(mut iv: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    iv.retain(|(s, e)| e > s);
    iv.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::new();
    for (s, e) in iv {
        match out.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => out.push((s, e)),
        }
    }
    out
}

/// system 活跃 ∧ mic 不活跃 → 压低区间(全局时间轴采样数)。
pub fn build_gate(segments: &[(String, u64, u64)]) -> Vec<GateSpan> {
    let sys = union_ms(segments.iter().filter(|(s, _, _)| s == "system").map(|(_, a, b)| (*a, *b)).collect());
    let mic = union_ms(segments.iter().filter(|(s, _, _)| s == "mic").map(|(_, a, b)| (*a, *b)).collect());

    // 差集:sys − mic(双讲保护)。双指针扫描。
    let mut spans: Vec<(u64, u64)> = Vec::new();
    let mut mi = 0usize;
    for (s, e) in sys {
        let mut cur = s;
        while cur < e {
            while mi < mic.len() && mic[mi].1 <= cur {
                mi += 1;
            }
            match mic.get(mi) {
                Some(&(ms, me)) if ms < e => {
                    if ms > cur {
                        spans.push((cur, ms));
                    }
                    cur = me.max(cur);
                    if me >= e {
                        break;
                    }
                    mi += 1;
                }
                _ => {
                    spans.push((cur, e));
                    break;
                }
            }
        }
        // mi 可能已越过本 sys 段末尾但下个 sys 段更靠后:差集扫描按序单调,无需回退。
    }

    // 间隙 <MERGE_GAP_MS 合并 → 短于 MIN_SPAN_MS 丢弃 → 换算采样。
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (s, e) in spans {
        match merged.last_mut() {
            Some(last) if s.saturating_sub(last.1) < MERGE_GAP_MS => last.1 = e,
            _ => merged.push((s, e)),
        }
    }
    merged
        .into_iter()
        .filter(|(s, e)| e - s >= MIN_SPAN_MS)
        .map(|(s, e)| GateSpan { start: s * SAMPLES_PER_MS, end: e * SAMPLES_PER_MS })
        .collect()
}

/// 逐采样增益:区间外 1.0;区间内 DUCK_GAIN,边沿 80ms 线性渐变(落区间内侧)。
/// 二分定位,回放热路径每帧每轨一次,开销可忽略。
pub fn gain_at(spans: &[GateSpan], sample: u64) -> f32 {
    let idx = spans.partition_point(|sp| sp.end <= sample);
    let Some(sp) = spans.get(idx) else {
        return 1.0;
    };
    if sample < sp.start {
        return 1.0;
    }
    let into = sample - sp.start;
    let left = sp.end - sample; // sample < sp.end 由 partition_point 保证
    let depth = 1.0 - DUCK_GAIN;
    let g_attack = 1.0 - depth * (into.min(RAMP_SAMPLES) as f32 / RAMP_SAMPLES as f32);
    let g_release = 1.0 - depth * (left.min(RAMP_SAMPLES) as f32 / RAMP_SAMPLES as f32);
    g_attack.max(g_release)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 16; // 1ms = 16 采样

    fn sys(s: u64, e: u64) -> (String, u64, u64) {
        ("system".into(), s, e)
    }
    fn mic(s: u64, e: u64) -> (String, u64, u64) {
        ("mic".into(), s, e)
    }

    #[test]
    fn system_only_interval_is_ducked() {
        let g = build_gate(&[sys(1000, 3000)]);
        assert_eq!(g, vec![GateSpan { start: 1000 * MS, end: 3000 * MS }]);
    }

    #[test]
    fn double_talk_interval_is_protected() {
        // system 1000..3000,mic 2000..2500 重叠 → 压低区间挖掉双讲部分
        let g = build_gate(&[sys(1000, 3000), mic(2000, 2500)]);
        assert_eq!(
            g,
            vec![
                GateSpan { start: 1000 * MS, end: 2000 * MS },
                GateSpan { start: 2500 * MS, end: 3000 * MS },
            ]
        );
    }

    #[test]
    fn gaps_under_300ms_merge_and_short_spans_drop() {
        // 两段 system 间隙 250ms → 合并为一段
        let g = build_gate(&[sys(0, 1000), sys(1250, 2000)]);
        assert_eq!(g, vec![GateSpan { start: 0, end: 2000 * MS }]);
        // 孤立 150ms(<200ms) → 丢弃
        let g = build_gate(&[sys(5000, 5150)]);
        assert!(g.is_empty());
    }

    #[test]
    fn gain_ramps_inside_span_edges() {
        let spans = vec![GateSpan { start: 16_000, end: 48_000 }]; // 1s..3s
        assert_eq!(gain_at(&spans, 0), 1.0, "区间外恒 1.0");
        assert_eq!(gain_at(&spans, 15_999), 1.0);
        // 进沿 80ms(1280 采样)线性 1.0→0.178
        let mid_in = gain_at(&spans, 16_000 + 640);
        assert!((mid_in - (1.0 + DUCK_GAIN) / 2.0).abs() < 0.02, "进沿中点≈均值: {mid_in}");
        assert!((gain_at(&spans, 30_000) - DUCK_GAIN).abs() < 1e-6, "区间腹地=DUCK");
        // 出沿对称
        let mid_out = gain_at(&spans, 48_000 - 640);
        assert!((mid_out - (1.0 + DUCK_GAIN) / 2.0).abs() < 0.02);
        assert_eq!(gain_at(&spans, 48_000), 1.0);
    }

    #[test]
    fn parse_tolerates_garbage_and_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("segments.jsonl");
        assert!(parse_segments_jsonl(&p).is_empty(), "缺文件→空");
        std::fs::write(&p, "{\"source\":\"system\",\"start_ms\":1,\"end_ms\":2}\ngarbage\n{\"source\":\"mic\",\"start_ms\":3,\"end_ms\":4,\"text\":\"x\"}\n").unwrap();
        let v = parse_segments_jsonl(&p);
        assert_eq!(v, vec![("system".into(),1,2),("mic".into(),3,4)], "坏行跳过");
    }

    /// 回归锁:mic 区间横跨两个 sys 段(brief 点名的 mi 指针风险场景)。
    /// 数值刻意 ≥200ms,避开 MIN_SPAN 过滤,直接检验差集扫描本身。
    #[test]
    fn mic_spanning_two_sys_segments_carves_both() {
        let g = build_gate(&[sys(0, 1000), sys(2000, 3000), mic(900, 2100)]);
        assert_eq!(
            g,
            vec![
                GateSpan { start: 0, end: 900 * MS },
                GateSpan { start: 2100 * MS, end: 3000 * MS },
            ],
            "mic 跨段必须同时挖到两个 sys 段"
        );
    }
}
