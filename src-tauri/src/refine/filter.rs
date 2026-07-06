//! A3 幻觉过滤:短段联合判定(时长 × 有效字符 × 语种漂移),白名单保真实短应答。
//! 常量 pub,由 scripts/refine_golden.py 对真实会议样本校准。

/// 短段判定上限(毫秒)。golden 校准发现两条边界漏杀短段(时长 2146/2466ms),
/// 原阈值 2000 覆盖不到;抬到 2500 命中,白名单保真实短应答(见
/// real_short_acks_pass)。权衡:2.0-2.5s 窗口内白名单外的 ≤2 字真实短语(如
/// "知道""谢谢")误杀面随之扩大——golden 样本验证过保护段无误杀,但后续若有
/// 误杀反馈,优先补白名单而非调回阈值。golden 校准记录不入库,关键数据已
/// 内联于本注释。
pub const SHORT_MS: u64 = 2500;
/// 语种漂移判定上限(毫秒)。
pub const DRIFT_MS: u64 = 3000;
/// 语种漂移下的有效字符上限。
pub const DRIFT_MAX_CHARS: usize = 4;

/// 真实短应答白名单(去标点后全匹配)。
const WHITELIST: &[&str] = &[
    "好", "对", "嗯", "行", "是", "好的", "对的", "嗯嗯", "行吧", "可以",
    "ok", "噢", "哦", "喔", "欸", "诶", "嗯哼", "没了", "没有",
];

/// 去标点/空白后的有效字符序列(小写)。
fn effective_chars(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .collect::<String>()
        .to_lowercase()
}

/// 幻觉判定。lang 为 SenseVoice 标签(可含 <|..|> 包裹)或空串。
pub fn is_hallucination(text: &str, dur_ms: u64, _rms: Option<f32>, lang: &str) -> bool {
    let eff = effective_chars(text);
    if WHITELIST.contains(&eff.as_str()) {
        return false;
    }
    let n = eff.chars().count();
    if dur_ms < SHORT_MS && n == 0 {
        return true;
    }
    if dur_ms < SHORT_MS && n <= 2 {
        return true;
    }
    let tag = lang.trim_start_matches("<|").trim_end_matches("|>").to_lowercase();
    if dur_ms < DRIFT_MS && matches!(tag.as_str(), "yue" | "ja" | "ko") && n <= DRIFT_MAX_CHARS {
        return true;
    }
    false
}

/// 对整份 segments 求应丢弃 seq 集(盘上无 lang,恒传空)。
pub fn discarded_seqs(segs: &[crate::store::SegmentRecord]) -> Vec<u64> {
    segs.iter()
        .filter(|s| is_hallucination(&s.text, s.end_ms.saturating_sub(s.start_ms), s.rms, ""))
        .map(|s| s.seq)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // 各测试的文本均为合成占位(纯标点/字典词除外),仅时长/rms 参数承载校准语义。
    #[test]
    fn junk_short_segments_hit() {
        assert!(is_hallucination("。", 1530, Some(0.0159), ""));
        assert!(is_hallucination("噪声。", 642, Some(0.0146), ""));
        assert!(is_hallucination("嗡嗡。", 1800, Some(0.0044), ""));
        assert!(is_hallucination("沙沙。", 1900, Some(0.0050), ""));
        assert!(is_hallucination("滋滋。", 1500, Some(0.0092), ""));
        assert!(is_hallucination("", 1000, Some(0.0005), ""));
    }

    #[test]
    fn real_short_acks_pass() {
        assert!(!is_hallucination("好。", 1400, Some(0.0050), ""));
        assert!(!is_hallucination("对.", 1200, Some(0.0048), ""));
        assert!(!is_hallucination("OK。", 900, Some(0.0100), ""));
        assert!(!is_hallucination("嗯嗯。", 800, Some(0.0060), ""));
    }

    #[test]
    fn long_segments_never_filtered() {
        assert!(!is_hallucination("噪", 2600, Some(0.001), "")); // 过 SHORT_MS(2500) 不适用短段规则
        assert!(!is_hallucination("这是一个正常长度的句子内容。", 5000, Some(0.01), ""));
    }

    /// 边界时长回归:2.0-2.5s 窗口的 ≤2 字非白名单短段必须命中(合成用例,
    /// 时长取自 golden 校准发现的漏杀边界,原 SHORT_MS=2000 覆盖不到)。
    #[test]
    fn boundary_window_junk_hits() {
        assert!(is_hallucination("杂讯。", 2466, Some(0.0050), ""));
        assert!(is_hallucination(".", 2146, Some(0.0052), ""));
    }

    #[test]
    fn lang_drift_short_hit_but_longer_pass() {
        assert!(is_hallucination("唔，啱啱啱", 2500, Some(0.008), "yue")); // ≤4 有效字 + yue 漂移命中
        assert!(!is_hallucination("唔啱唔啱唔啱唔啱唔啱", 2500, Some(0.02), "yue")); // >4 有效字,可能真粤语
    }

    #[test]
    fn discarded_seqs_maps_over_records() {
        let mk = |seq, text: &str, dur: u64, rms| crate::store::SegmentRecord {
            seq, source: "mic".into(), text: text.into(),
            start_ms: 0, end_ms: dur, speaker: None, rms: Some(rms),
        };
        let segs = vec![mk(0, "噪声。", 642, 0.0146), mk(1, "好。", 1400, 0.005), mk(2, "正常说话内容在这里。", 4000, 0.02)];
        assert_eq!(discarded_seqs(&segs), vec![0]);
    }
}
