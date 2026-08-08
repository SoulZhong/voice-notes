//! 纠错回灌(spec rev2 P1-2):人工指认「这个说话人是库里的谁」之后,把该
//! 说话人的发声段重新嵌入并并入那个人的质心——让整理动作变成训练信号
//! (准确度分析文档:轻量纠错回灌文献口径相对 DER -32%)。
//! 本文件先落纯逻辑核:不碰磁盘不碰库,输入段+PCM,输出各信道嵌入统计;
//! 磁盘壳(账本/门禁/解码/入库)与 IPC 挂钩在同文件后续任务追加。

use std::collections::BTreeMap;

use crate::diar::SpeakerEmbedder;
use crate::store::SegmentRecord;

/// 单段最短 1.5s:与 registry::MIN_CENTROID_UPDATE_SAMPLES(24_000 采样)同口径,
/// 更短的段嵌入不稳定,进质心是污染不是信号。
pub const MIN_SEG_MS: u64 = 1_500;
/// 单次回灌每信道最多嵌入的段数(时长降序取):指认是用户动作触发的后台任务,
/// 超长会议不该嵌几百段——每信道最长的 30 段已足够代表这个人;分信道限额是
/// 因为全局截断会让长会议的次要信道颗粒无收。
pub const MAX_SEGS_PER_SOURCE: usize = 30;

/// 某信道的回灌统计:单位化质心 + 段数 + 实际嵌入时长。
pub struct SourceStat {
    pub centroid: Vec<f32>,
    pub count: u64,
    pub total_ms: u64,
}

/// 纯逻辑核:给定待回灌段 + 各轨 PCM(16kHz f32,带轨起点 offset_ms),逐段
/// 切片→嵌入→按信道求单位均值。嵌入/切片/非有限值失败一律静默跳段——回灌是
/// 增值层,任何失败都不该冒泡成用户可见错误。
pub fn build_source_stats(
    segs: &[&SegmentRecord],
    pcm_by_source: &BTreeMap<String, (u64, Vec<f32>)>,
    embedder: &mut dyn SpeakerEmbedder,
) -> BTreeMap<String, SourceStat> {
    // 分信道分组,各自(时长降序,seq 升序)稳定排序后限额——排序键里带 seq
    // 是为了让选择结果与调用方的加载顺序无关(NoteStore::load 会按时间重排)。
    let mut by_source_segs: BTreeMap<&str, Vec<&SegmentRecord>> = BTreeMap::new();
    for s in segs {
        if s.end_ms.saturating_sub(s.start_ms) >= MIN_SEG_MS {
            by_source_segs.entry(s.source.as_str()).or_default().push(s);
        }
    }

    struct Acc {
        sum: Vec<f32>,
        count: u64,
        total_ms: u64,
    }
    let mut out_acc: BTreeMap<String, Acc> = BTreeMap::new();
    for (source, mut list) in by_source_segs {
        let Some((offset_ms, pcm)) = pcm_by_source.get(source) else {
            continue;
        };
        list.sort_by_key(|s| {
            (
                std::cmp::Reverse(s.end_ms.saturating_sub(s.start_ms)),
                s.seq,
            )
        });
        list.truncate(MAX_SEGS_PER_SOURCE);
        for s in list {
            let start = (s.start_ms.saturating_sub(*offset_ms) as usize).saturating_mul(16);
            let end =
                ((s.end_ms.saturating_sub(*offset_ms) as usize).saturating_mul(16)).min(pcm.len());
            if start >= end {
                continue;
            }
            // 实际切片时长再过一次门:账面 2s 但轨尾截断到几十 ms 的段,嵌入
            // 不稳定且会虚报 total_ms。
            let actual_ms = ((end - start) / 16) as u64;
            if actual_ms < MIN_SEG_MS {
                continue;
            }
            let Ok(vec) = embedder.embed(&pcm[start..end]) else {
                continue;
            };
            if vec.is_empty() || vec.iter().any(|x| !x.is_finite()) {
                continue;
            }
            let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm <= f32::EPSILON {
                continue;
            }
            let acc = out_acc.entry(source.to_string()).or_insert_with(|| Acc {
                sum: vec![0.0; vec.len()],
                count: 0,
                total_ms: 0,
            });
            if acc.sum.len() != vec.len() {
                continue; // 维度漂移只可能是模型异常,弃段
            }
            for (a, b) in acc.sum.iter_mut().zip(&vec) {
                *a += b / norm; // 逐向量归一化后累加(registry 同口径)
            }
            acc.count += 1;
            acc.total_ms += actual_ms;
        }
    }

    out_acc
        .into_iter()
        .filter_map(|(source, acc)| {
            let norm: f32 = acc.sum.iter().map(|x| x * x).sum::<f32>().sqrt();
            if !norm.is_finite() || norm <= f32::EPSILON || acc.count == 0 {
                return None;
            }
            Some((
                source,
                SourceStat {
                    centroid: acc.sum.iter().map(|x| x / norm).collect(),
                    count: acc.count,
                    total_ms: acc.total_ms,
                },
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diar::MockEmbedder;

    fn seg(seq: u64, source: &str, start_ms: u64, end_ms: u64) -> SegmentRecord {
        SegmentRecord {
            seq,
            source: source.into(),
            text: String::new(),
            start_ms,
            end_ms,
            speaker: Some("S1".into()),
            rms: Some(0.0),
        }
    }

    /// 16kHz:1ms = 16 采样。
    fn pcm_ms(ms: u64) -> Vec<f32> {
        vec![0.1; (ms * 16) as usize]
    }

    fn unit(i: usize) -> Vec<f32> {
        let mut v = vec![0.0; 4];
        v[i] = 1.0;
        v
    }

    #[test]
    fn short_and_truncated_segments_are_skipped() {
        let s_short = seg(0, "mic", 0, 800); // 账面 <1.5s
                                             // 账面 2s 但 PCM 只覆盖到 2200ms:切到 1200ms,实际 <1.5s,同样跳过——
                                             // 账面时长骗不过实际样本数。
        let s_trunc = seg(1, "mic", 1000, 3000);
        let s_ok = seg(2, "mic", 0, 2000);
        let mut pcm = BTreeMap::new();
        pcm.insert("mic".to_string(), (0u64, pcm_ms(2200)));
        let mut emb = MockEmbedder::new(vec![Ok(unit(0))]);
        let stats = build_source_stats(&[&s_short, &s_trunc, &s_ok], &pcm, &mut emb);
        let mic = stats.get("mic").expect("s_ok 应产出 mic 统计");
        assert_eq!(mic.count, 1);
        assert_eq!(mic.total_ms, 2000, "只计实际嵌入段的实际切片时长");
    }

    #[test]
    fn stats_split_by_source_and_centroid_is_unit_mean_of_unit_vectors() {
        let m = seg(0, "mic", 0, 2000);
        let s = seg(1, "system", 0, 2000);
        let mut pcm = BTreeMap::new();
        pcm.insert("mic".to_string(), (0u64, pcm_ms(2000)));
        pcm.insert("system".to_string(), (0u64, pcm_ms(2000)));
        // 故意给未归一化向量:实现必须先逐向量归一化再累加。
        let mut emb = MockEmbedder::new(vec![
            Ok(vec![3.0, 0.0, 0.0, 0.0]),
            Ok(vec![0.0, 5.0, 0.0, 0.0]),
        ]);
        let stats = build_source_stats(&[&m, &s], &pcm, &mut emb);
        assert_eq!(stats.keys().collect::<Vec<_>>(), vec!["mic", "system"]);
        for st in stats.values() {
            let norm: f32 = st.centroid.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((norm - 1.0).abs() < 1e-5, "质心必须单位化");
        }
        assert!((stats["mic"].centroid[0] - 1.0).abs() < 1e-5);
    }

    #[test]
    fn non_finite_embeddings_and_errors_degrade_to_skip() {
        let a = seg(0, "mic", 0, 2000);
        let b = seg(1, "mic", 2000, 4000);
        let c = seg(2, "system", 0, 2000); // system 轨 PCM 缺失
        let mut pcm = BTreeMap::new();
        pcm.insert("mic".to_string(), (0u64, pcm_ms(4000)));
        let mut emb = MockEmbedder::new(vec![
            Ok(vec![f32::NAN, 1.0, 0.0, 0.0]),
            Err(anyhow::anyhow!("boom")),
        ]);
        let stats = build_source_stats(&[&a, &b, &c], &pcm, &mut emb);
        assert!(stats.is_empty(), "NaN/报错/缺轨全部静默跳过");
    }

    #[test]
    fn per_source_cap_with_stable_order() {
        // mic 造 MAX_SEGS_PER_SOURCE+2 个等长段:限额按信道各自生效,
        // 等长时按 seq 升序稳定取。
        let segs: Vec<SegmentRecord> = (0..(MAX_SEGS_PER_SOURCE as u64 + 2))
            .map(|i| seg(i, "mic", i * 2000, i * 2000 + 2000))
            .collect();
        let refs: Vec<&SegmentRecord> = segs.iter().collect();
        let mut pcm = BTreeMap::new();
        pcm.insert(
            "mic".to_string(),
            (0u64, pcm_ms((MAX_SEGS_PER_SOURCE as u64 + 2) * 2000)),
        );
        let mut emb = MockEmbedder::new(vec![Ok(unit(0))]);
        let stats = build_source_stats(&refs, &pcm, &mut emb);
        assert_eq!(stats["mic"].count as usize, MAX_SEGS_PER_SOURCE);
    }

    #[test]
    fn offset_is_respected_when_slicing() {
        // 轨 offset 1000ms:段 [1000,3000)ms 应切 PCM [0,32000) 采样;
        // PCM 只有 2000ms 长,不减 offset 会切出空区间。
        let m = seg(0, "mic", 1000, 3000);
        let mut pcm = BTreeMap::new();
        pcm.insert("mic".to_string(), (1000u64, pcm_ms(2000)));
        let mut emb = MockEmbedder::new(vec![Ok(unit(0))]);
        let stats = build_source_stats(&[&m], &pcm, &mut emb);
        assert_eq!(stats["mic"].total_ms, 2000);
    }
}
