//! 段内变更点检测(滑窗声纹) + token 按变更点分组。纯逻辑,无外部依赖,
//! 供 session.rs worker 在长段上做"同一 final 内部按说话人切分"。

/// 段时长门槛:短于此不跑滑窗嵌入(短段装不下两个人,diarization off 时零开销)。
pub const SPLIT_MIN_SEGMENT_MS: u64 = 3000;
/// 滑窗窗长(ms)。
pub const SPLIT_WIN_MS: u64 = 1500;
/// 滑窗步长(ms)。
pub const SPLIT_HOP_MS: u64 = 500;
/// 相邻有效窗余弦低于此值 → 候选变更点。待真实会议数据校准。
pub const CHANGE_SIM_THRESHOLD: f32 = 0.55;
/// 变更点切出的子段短于此(ms)则丢弃该变更点(短子段声纹不可靠)。待真实会议数据校准。
pub const MIN_SUBSEG_MS: u64 = 1200;

// dot/normalize 与 registry.rs 中的实现逐字重复:两处各自私有,刻意不抽公共
// 模块。6 行代码不值得为了省重复而让 split(纯逻辑、无状态)反过来依赖
// registry(有状态聚类)的内部实现,增加跨模块耦合。
fn normalize(v: &[f32]) -> Option<Vec<f32>> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if !norm.is_finite() || norm < 1e-6 {
        return None;
    }
    Some(v.iter().map(|x| x / norm).collect())
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// 窗 idx 的中心时刻(ms,相对段首)。窗 idx 起点 = idx*hop,窗长 win,
/// 中心 = idx*hop + win/2。
fn window_center_ms(idx: usize) -> u64 {
    idx as u64 * SPLIT_HOP_MS + SPLIT_WIN_MS / 2
}

/// 一个"相邻有效窗对"及其相似度、候选交界点。
struct Pair {
    sim: f32,
    /// 两窗中心的中点(ms,相对段首)。
    boundary: u64,
}

/// 相邻滑窗嵌入余弦跌破阈值处 → 变更点(ms,相对段首,升序)。
/// embs[i] 是第 i 窗(起点 i*hop_ms)的单位化嵌入;None = 该窗嵌入失败,
/// 视为与两侧相似(宁可漏切不误切)。变更点取"低谷 run 中最低的相邻对"的
/// 交界中点;切出的任一子段 < MIN_SUBSEG_MS 则该点丢弃。
pub fn detect_change_points(embs: &[Option<Vec<f32>>], total_ms: u64) -> Vec<u64> {
    // 跳过 None 与 normalize 失败(零向量)的窗,只保留有效窗及其原始下标
    // (下标用于还原真实窗中心 —— None 窗跳过后,相邻有效窗未必是连续下标)。
    let valid: Vec<(usize, Vec<f32>)> = embs
        .iter()
        .enumerate()
        .filter_map(|(i, e)| e.as_ref().and_then(|v| normalize(v)).map(|n| (i, n)))
        .collect();

    if valid.len() < 2 {
        return Vec::new();
    }

    let pairs: Vec<Pair> = valid
        .windows(2)
        .map(|w| {
            let (idx_a, emb_a) = &w[0];
            let (idx_b, emb_b) = &w[1];
            let sim = dot(emb_a, emb_b);
            let boundary = (window_center_ms(*idx_a) + window_center_ms(*idx_b)) / 2;
            Pair { sim, boundary }
        })
        .collect();

    // 低谷 run 归并:同一次说话人切换在重叠窗上会连续触发多个相邻对低于
    // 阈值;按 pair 下标是否连续分 run,每个 run 只取相似度最低的一对。
    let mut candidates: Vec<u64> = Vec::new();
    let mut run_start: Option<usize> = None;
    for (i, p) in pairs.iter().enumerate() {
        if p.sim < CHANGE_SIM_THRESHOLD {
            if run_start.is_none() {
                run_start = Some(i);
            }
        } else if let Some(start) = run_start.take() {
            candidates.push(min_boundary_in_run(&pairs, start, i));
        }
    }
    if let Some(start) = run_start {
        candidates.push(min_boundary_in_run(&pairs, start, pairs.len()));
    }

    // 短子段过滤:从左到右贪心,候选点与"上一保留边界"(初始为段首 0)间距
    // < MIN_SUBSEG_MS 则丢弃该点(不推进 prev,继续用同一 prev 比较下一候选)。
    let mut kept: Vec<u64> = Vec::new();
    let mut prev = 0u64;
    for &b in &candidates {
        if b.saturating_sub(prev) < MIN_SUBSEG_MS {
            continue;
        }
        kept.push(b);
        prev = b;
    }

    // 尾段检查:最后一个保留边界到段尾过短 → 丢弃最后一个边界。
    if let Some(&last) = kept.last() {
        if total_ms.saturating_sub(last) < MIN_SUBSEG_MS {
            kept.pop();
        }
    }

    kept
}

fn min_boundary_in_run(pairs: &[Pair], start: usize, end: usize) -> u64 {
    pairs[start..end]
        .iter()
        .min_by(|a, b| a.sim.partial_cmp(&b.sim).expect("相似度不应为 NaN"))
        .map(|p| p.boundary)
        .expect("run 非空")
}

/// 按变更点把 tokens 分组拼接为子文本(变更点 n 个 → n+1 段;token 时刻(秒)
/// 换算 ms 后 < 边界归前段)。返回 None 表示时间戳不可用(空/与 tokens 不等长),
/// 调用方走"子段重识别"回退。子文本 trim 后可为空,由调用方丢弃该子段。
pub fn group_tokens_by_boundaries(
    tokens: &[String],
    timestamps: &[f32],
    boundaries_ms: &[u64],
) -> Option<Vec<String>> {
    if timestamps.is_empty() || timestamps.len() != tokens.len() {
        return None;
    }

    let mut groups: Vec<String> = vec![String::new(); boundaries_ms.len() + 1];
    for (tok, &ts) in tokens.iter().zip(timestamps) {
        let ts_ms = (ts * 1000.0).round() as u64;
        // 第一个满足 ts_ms < 边界的下标即所属段;越过全部边界则归最后一段。
        let seg = boundaries_ms.iter().position(|&b| ts_ms < b).unwrap_or(boundaries_ms.len());
        groups[seg].push_str(tok);
    }
    Some(groups)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(v: [f32; 3]) -> Option<Vec<f32>> {
        Some(v.to_vec())
    }

    #[test]
    fn no_change_when_all_embeddings_identical() {
        let e1 = [1.0, 0.0, 0.0];
        let embs = vec![unit(e1), unit(e1), unit(e1), unit(e1), unit(e1)];
        assert_eq!(detect_change_points(&embs, 5000), Vec::<u64>::new());
    }

    #[test]
    fn single_change_point_at_orthogonal_switch() {
        let e1 = [1.0, 0.0, 0.0];
        let e2 = [0.0, 1.0, 0.0];
        // idx0..3 = e1, idx4..7 = e2;唯一低谷对在 idx3-idx4 之间。
        let embs = vec![
            unit(e1),
            unit(e1),
            unit(e1),
            unit(e1),
            unit(e2),
            unit(e2),
            unit(e2),
            unit(e2),
        ];
        // 交界中点 = 3*hop + (win+hop)/2 = 3*500 + 1000 = 2500ms。
        // total_ms 取 5000,两侧子段均 >= MIN_SUBSEG_MS,不被过滤。
        assert_eq!(detect_change_points(&embs, 5000), vec![2500]);
    }

    #[test]
    fn continuous_valley_run_takes_only_the_lowest_pair() {
        // 5 个有效窗在 e1-e2 平面渐变,相邻对夹角依次为 40°/70°/65°/40°,
        // 对应相似度 0.766/0.342/0.423/0.766 —— 中间两对连续低于阈值(0.55),
        // 只应产生 1 个点,取相似度最低的一对(idx1-idx2,0.342)。
        let v0 = [1.0_f32, 0.0, 0.0];
        let v1 = [40f32.to_radians().cos(), 40f32.to_radians().sin(), 0.0];
        let v2 = [110f32.to_radians().cos(), 110f32.to_radians().sin(), 0.0];
        let v3 = [175f32.to_radians().cos(), 175f32.to_radians().sin(), 0.0];
        let v4 = [215f32.to_radians().cos(), 215f32.to_radians().sin(), 0.0];
        let embs = vec![unit(v0), unit(v1), unit(v2), unit(v3), unit(v4)];

        // 交界中点 = (window_center(1) + window_center(2)) / 2
        //          = ((500+750) + (1000+750)) / 2 = (1250+1750)/2 = 1500ms。
        assert_eq!(detect_change_points(&embs, 5000), vec![1500]);
    }

    #[test]
    fn change_point_dropped_when_tail_subsegment_too_short() {
        let e1 = [1.0, 0.0, 0.0];
        let e2 = [0.0, 1.0, 0.0];
        // idx0,1 = e1;idx2,3 = e2 → 唯一低谷对在 idx1-idx2 之间。
        let embs = vec![unit(e1), unit(e1), unit(e2), unit(e2)];
        // 交界中点 = 1*500 + 1000 = 1500ms。
        // total_ms 恰好卡在尾段 < MIN_SUBSEG_MS(1200) 的一侧:1500+1199=2699。
        assert_eq!(detect_change_points(&embs, 2699), Vec::<u64>::new());
        // 对照:尾段 = 1200(不小于阈值)时应保留该点。
        assert_eq!(detect_change_points(&embs, 2700), vec![1500]);
    }

    #[test]
    fn none_window_is_skipped_not_a_change_point() {
        let e1 = [1.0, 0.0, 0.0];
        let e2 = [0.0, 1.0, 0.0];

        // e1, None, e1 → 有效窗都是 e1,无变更点。
        let embs_same = vec![unit(e1), None, unit(e1)];
        assert_eq!(detect_change_points(&embs_same, 3000), Vec::<u64>::new());

        // e1, None, e2 → 有效对是 (idx0, idx2),交界中点用两个有效窗的
        // 实际位置:(window_center(0) + window_center(2)) / 2
        //         = ((0+750) + (1000+750)) / 2 = (750+1750)/2 = 1250ms。
        let embs_switch = vec![unit(e1), None, unit(e2)];
        assert_eq!(detect_change_points(&embs_switch, 3000), vec![1250]);
    }

    #[test]
    fn empty_or_single_window_has_no_change_point() {
        let embs_empty: Vec<Option<Vec<f32>>> = Vec::new();
        assert_eq!(detect_change_points(&embs_empty, 0), Vec::<u64>::new());

        let embs_single = vec![unit([1.0, 0.0, 0.0])];
        assert_eq!(detect_change_points(&embs_single, 1500), Vec::<u64>::new());
    }

    #[test]
    fn group_tokens_splits_and_concats_without_separator() {
        let tokens: Vec<String> =
            ["Hi", "there", "how", "are", "you"].iter().map(|s| s.to_string()).collect();
        let timestamps = vec![0.1, 0.6, 1.1, 1.6, 2.1];
        // 边界 1000ms:0.1s/0.6s(<1.0s)归前段,1.1s/1.6s/2.1s 归后段。
        let boundaries = vec![1000u64];
        let groups = group_tokens_by_boundaries(&tokens, &timestamps, &boundaries);
        assert_eq!(groups, Some(vec!["Hithere".to_string(), "howareyou".to_string()]));
    }

    #[test]
    fn group_tokens_none_when_timestamps_empty() {
        let tokens: Vec<String> = vec!["a".to_string()];
        let timestamps: Vec<f32> = Vec::new();
        assert_eq!(group_tokens_by_boundaries(&tokens, &timestamps, &[1000]), None);
    }

    #[test]
    fn group_tokens_none_when_length_mismatch() {
        let tokens: Vec<String> = vec!["a".to_string(), "b".to_string()];
        let timestamps: Vec<f32> = vec![0.1];
        assert_eq!(group_tokens_by_boundaries(&tokens, &timestamps, &[1000]), None);
    }

    #[test]
    fn group_tokens_tail_segment_empty_when_no_token_after_last_boundary() {
        let tokens: Vec<String> = vec!["a".to_string(), "b".to_string()];
        let timestamps = vec![0.1, 0.2];
        // 边界远超所有 token 时刻 → 前段拿到全部 token,尾段空串(调用方丢弃)。
        let groups = group_tokens_by_boundaries(&tokens, &timestamps, &[10_000]);
        assert_eq!(groups, Some(vec!["ab".to_string(), String::new()]));
    }
}
