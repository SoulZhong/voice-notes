//! 录音场景传感器(2026-08-23-scene-recognition-design.md 一期)。
//!
//! 纯逻辑、流时间驱动:session::FinalSink 在段定稿/抑制点喂事件,30s 一桶,
//! 5 分钟滑窗按规则判场景;连续两窗同判才切换(防抖)。一期只判定、落盘、提示,
//! **不改任何处置行为**——先拿几场真会验证判定准确率。
//! 阈值全部标注「待校准」:按 2026-08-22 两场实测数据(71%/86% 覆盖率,erle≈0.18dB)
//! 取初值,后续以 scene.json 时间线对人工复盘校准。

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::Path;

pub const BUCKET_MS: u64 = 30_000;
/// 判定窗:最近 10 桶(5 分钟)。不足 4 桶(2 分钟)不判(冷启动)。
pub const DECIDE_BUCKETS: usize = 10;
const WARMUP_BUCKETS: usize = 4;
/// 判定阈值(待校准)。
const OVERLAP_HIGH: f32 = 0.6;
const ACTIVE_MIN: f32 = 0.05;
const ERLE_CONVERGED_DB: f32 = 3.0;
/// 回声文本/残渣门命中率(次/分钟)高于此值 → 与参考相关(外放而非同源双路)。
const ECHO_HITS_PER_MIN_HIGH: f32 = 2.0;

/// 场景标签(稳定机读码,scene.json 与前端文案键都按它)。
pub const SC_UNKNOWN: &str = "unknown";
pub const SC_HEADSET: &str = "headset";
pub const SC_SPEAKER_ECHO: &str = "speaker_echo";
pub const SC_DUAL_PATH: &str = "dual_path";
pub const SC_ONSITE: &str = "onsite";
pub const SC_LISTENING: &str = "listening";

#[derive(Default, Clone)]
struct Bucket {
    mic_ms: u64,
    mic_ov_ms: u64,
    sys_ms: u64,
    echo_hits: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SceneWindow {
    pub start_ms: u64,
    pub end_ms: u64,
    pub scene: String,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct SceneDoc {
    pub schema_version: u32,
    /// 场景时间线(变化点);末窗 end_ms 为停录时刻。
    pub windows: Vec<SceneWindow>,
    /// 占时最长的场景(unknown 不参与;全 unknown 则为 unknown)。
    pub final_scene: String,
    /// 事后回填标记(scene_backfill 工具,issue #169):区别于录制期活体判定——
    /// 回填缺 erle、被抑制段无时间戳,口径有已知偏差(见该 bin 模块注释)。
    /// false 不落盘,旧文件缺字段 serde default 兼容。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub backfilled: bool,
}

pub struct SceneSensor {
    sys_windows: VecDeque<(u64, u64)>,
    buckets: VecDeque<Bucket>,
    cur: Bucket,
    cur_end: u64,
    current: &'static str,
    cur_since: u64,
    pending: Option<(&'static str, u8)>,
    timeline: Vec<SceneWindow>,
    /// 刚发生的稳定切换(供调用方取走发 SceneHint)。
    changed: Option<&'static str>,
}

impl SceneSensor {
    pub fn new() -> Self {
        Self {
            sys_windows: VecDeque::new(),
            buckets: VecDeque::new(),
            cur: Bucket::default(),
            cur_end: BUCKET_MS,
            current: SC_UNKNOWN,
            cur_since: 0,
            pending: None,
            timeline: Vec::new(),
            changed: None,
        }
    }

    fn roll(&mut self, now: u64, erle_db: Option<f32>) {
        while now >= self.cur_end {
            let b = std::mem::take(&mut self.cur);
            self.buckets.push_back(b);
            while self.buckets.len() > DECIDE_BUCKETS {
                self.buckets.pop_front();
            }
            let closed_at = self.cur_end;
            self.cur_end += BUCKET_MS;
            self.evaluate(closed_at, erle_db);
        }
        let keep_from = now.saturating_sub(BUCKET_MS);
        while self.sys_windows.front().is_some_and(|(_, e)| *e < keep_from) {
            self.sys_windows.pop_front();
        }
    }

    fn evaluate(&mut self, at_ms: u64, erle_db: Option<f32>) {
        if self.buckets.len() < WARMUP_BUCKETS {
            return;
        }
        let wall = (self.buckets.len() as u64 * BUCKET_MS) as f32;
        let mic: u64 = self.buckets.iter().map(|b| b.mic_ms).sum();
        let sys: u64 = self.buckets.iter().map(|b| b.sys_ms).sum();
        let ov: u64 = self.buckets.iter().map(|b| b.mic_ov_ms).sum();
        let hits: u32 = self.buckets.iter().map(|b| b.echo_hits).sum();
        let mic_f = mic as f32 / wall;
        let sys_f = sys as f32 / wall;
        let hits_pm = hits as f32 / (wall / 60_000.0);
        let cand: &'static str = if sys_f < ACTIVE_MIN && mic_f >= 2.0 * ACTIVE_MIN {
            SC_ONSITE
        } else if mic_f < 0.6 * ACTIVE_MIN && sys_f >= 2.0 * ACTIVE_MIN {
            SC_LISTENING
        } else if mic > 10_000 && ov as f32 / mic as f32 >= OVERLAP_HIGH {
            // 与参考相关(AEC 收敛良好或文本门频繁命中)→ 外放;否则同源双路。
            // erle 读不到按不相关处理(倾向报更严重的同源双路,一期只提示无副作用)。
            if erle_db.is_some_and(|e| e >= ERLE_CONVERGED_DB) || hits_pm >= ECHO_HITS_PER_MIN_HIGH {
                SC_SPEAKER_ECHO
            } else {
                SC_DUAL_PATH
            }
        } else if mic_f >= ACTIVE_MIN || sys_f >= ACTIVE_MIN {
            SC_HEADSET
        } else {
            SC_UNKNOWN
        };
        // 防抖:连续两窗同候选才切换。
        if cand == self.current {
            self.pending = None;
            return;
        }
        match self.pending {
            Some((p, n)) if p == cand => {
                if n + 1 >= 2 {
                    self.timeline.push(SceneWindow {
                        start_ms: self.cur_since,
                        end_ms: at_ms,
                        scene: self.current.to_string(),
                    });
                    self.current = cand;
                    self.cur_since = at_ms;
                    self.pending = None;
                    self.changed = Some(cand);
                } else {
                    self.pending = Some((cand, n + 1));
                }
            }
            _ => self.pending = Some((cand, 1)),
        }
    }

    pub fn feed_system(&mut self, start_ms: u64, end_ms: u64, erle_db: Option<f32>) {
        self.cur.sys_ms += end_ms.saturating_sub(start_ms);
        self.sys_windows.push_back((start_ms, end_ms));
        self.roll(end_ms, erle_db);
    }

    /// mic 段(无论随后被抑制与否都喂:活跃度按"发声"口径)。重叠按「已喂入的
    /// system 窗口」因果计算——实时流没有未来知识,只能这么算。
    pub fn feed_mic(&mut self, start_ms: u64, end_ms: u64, erle_db: Option<f32>) {
        let dur = end_ms.saturating_sub(start_ms);
        let ov: u64 = self
            .sys_windows
            .iter()
            .map(|(a, b)| end_ms.min(*b).saturating_sub(start_ms.max(*a)))
            .sum::<u64>()
            .min(dur);
        self.feed_mic_precomputed(start_ms, end_ms, ov, erle_db);
    }

    /// mic 段,重叠量由调用方给定(scene_backfill 离线回放用,issue #169):离线有
    /// 全局视野,重叠按完整 system 时间线算,不受喂入顺序/段粒度影响——重转写过的
    /// 场次段结构与活体子段毫无对应,因果式 sys_windows 在那里会系统性漏算。
    /// feed_mic 委托到此,判定/防抖/入桶单一实现。
    pub fn feed_mic_precomputed(&mut self, start_ms: u64, end_ms: u64, ov_ms: u64, erle_db: Option<f32>) {
        let dur = end_ms.saturating_sub(start_ms);
        self.cur.mic_ms += dur;
        self.cur.mic_ov_ms += ov_ms.min(dur);
        self.roll(end_ms, erle_db);
    }

    /// 回声类命中(echo_match/echo_retract/aec_residue/residue_filler):相关性证据。
    pub fn feed_echo_hit(&mut self) {
        self.cur.echo_hits += 1;
    }

    /// 取走"刚发生的稳定切换"(SceneHint 用;取一次即清)。
    /// 当前稳定场景(实时行为门用;未稳定前 = unknown)。
    pub fn current_scene(&self) -> &'static str {
        self.current
    }

    pub fn poll_change(&mut self) -> Option<&'static str> {
        self.changed.take()
    }

    pub fn finish(mut self, now_ms: u64) -> SceneDoc {
        if now_ms > self.cur_since || self.timeline.is_empty() {
            self.timeline.push(SceneWindow {
                start_ms: self.cur_since,
                end_ms: now_ms.max(self.cur_since),
                scene: self.current.to_string(),
            });
        }
        let mut tally: std::collections::BTreeMap<&str, u64> = Default::default();
        for w in &self.timeline {
            if w.scene != SC_UNKNOWN {
                *tally.entry(w.scene.as_str()).or_default() += w.end_ms - w.start_ms;
            }
        }
        let final_scene = tally
            .into_iter()
            .max_by_key(|(_, ms)| *ms)
            .map(|(s, _)| s.to_string())
            .unwrap_or_else(|| SC_UNKNOWN.to_string());
        SceneDoc { schema_version: 1, windows: self.timeline, final_scene, backfilled: false }
    }
}

pub const SCENE_FILE: &str = "scene.json";

pub fn save(note_dir: &Path, doc: &SceneDoc) -> anyhow::Result<()> {
    let path = note_dir.join(SCENE_FILE);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(doc)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn load(note_dir: &Path) -> Option<SceneDoc> {
    let raw = std::fs::read(note_dir.join(SCENE_FILE)).ok()?;
    serde_json::from_slice(&raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_minutes(s: &mut SceneSensor, mins: u64, mic_per_30s: u64, sys_per_30s: u64, overlapped: bool) {
        // 每 30s 一对段:sys 先落(0..sys_ms),mic 按 overlapped 决定贴着 sys 还是错开。
        for i in 0..(mins * 2) {
            let base = i * BUCKET_MS + s.cur_end.saturating_sub(BUCKET_MS) * 0; // 流时间由调用序递增
            let t0 = s.cur_end - BUCKET_MS + 0; // 当前桶起点
            let _ = (base, t0);
            let start = (s.cur_end - BUCKET_MS) + 1000;
            if sys_per_30s > 0 {
                s.feed_system(start, start + sys_per_30s, None);
            }
            if mic_per_30s > 0 {
                let m0 = if overlapped { start } else { start + sys_per_30s + 1000 };
                s.feed_mic(m0, m0 + mic_per_30s, None);
            }
            // 推进到下一桶
            s.roll(s.cur_end, None);
        }
    }

    #[test]
    fn dual_path_when_overlapped_and_no_echo_evidence() {
        let mut s = SceneSensor::new();
        feed_minutes(&mut s, 6, 15_000, 20_000, true);
        assert_eq!(s.current, SC_DUAL_PATH);
    }

    #[test]
    fn speaker_echo_when_hits_frequent() {
        let mut s = SceneSensor::new();
        for _ in 0..12 {
            let start = s.cur_end - BUCKET_MS + 1000;
            s.feed_system(start, start + 20_000, None);
            s.feed_mic(start, start + 15_000, None);
            s.feed_echo_hit();
            s.feed_echo_hit();
            s.roll(s.cur_end, None);
        }
        assert_eq!(s.current, SC_SPEAKER_ECHO);
    }

    #[test]
    fn onsite_when_system_silent_and_listening_when_mic_silent() {
        let mut s = SceneSensor::new();
        feed_minutes(&mut s, 6, 12_000, 0, false);
        assert_eq!(s.current, SC_ONSITE);
        let mut s2 = SceneSensor::new();
        feed_minutes(&mut s2, 6, 0, 20_000, false);
        assert_eq!(s2.current, SC_LISTENING);
    }

    #[test]
    fn headset_when_low_overlap_and_debounce_needs_two_windows() {
        let mut s = SceneSensor::new();
        feed_minutes(&mut s, 6, 10_000, 15_000, false);
        assert_eq!(s.current, SC_HEADSET);
        // 单窗突变不切换(防抖)
        let start = s.cur_end - BUCKET_MS + 1000;
        s.feed_system(start, start + 20_000, None);
        s.feed_mic(start, start + 15_000, None);
        s.roll(s.cur_end, None);
        assert_eq!(s.current, SC_HEADSET, "单窗不切");
    }

    #[test]
    fn finish_writes_timeline_and_majority_final() {
        let mut s = SceneSensor::new();
        feed_minutes(&mut s, 6, 15_000, 20_000, true);
        let doc = s.finish(12 * BUCKET_MS);
        assert!(!doc.windows.is_empty());
        assert_eq!(doc.final_scene, SC_DUAL_PATH);
    }
}

/// 场景二期·实时行为门(issue #162):纯函数,表驱动可测。
/// 旁听场的 backchannel(附和短语)不上屏——旁听者偶尔的「嗯/对/好的」在
/// 转写里只制造噪音;门槛按时长,≤ 该值的 mic 段进抑制而不是正文。
pub(crate) const LISTENING_BACKCHANNEL_MAX_MS: u64 = 2_000;

pub(crate) fn listening_backchannel_gate(scene: &str, dur_ms: u64) -> bool {
    scene == SC_LISTENING && dur_ms <= LISTENING_BACKCHANNEL_MAX_MS
}

/// 外放场残渣门收紧:AEC 在外放场必然收敛不足,残渣能量比耳机场高——
/// 能量上限放宽一倍,让更多真残渣进得了残渣门(overlap 判据不动)。
pub(crate) fn residue_rms_cap(scene: &str, base: f32) -> f32 {
    if scene == SC_SPEAKER_ECHO {
        base * 2.0
    } else {
        base
    }
}

/// UI「选中可疑段」同口径(src/lib/segPick.ts 的 overlappedMicSeqs 逐位镜像):
/// mic 段时长被 system 段覆盖 ≥80% 即命中。输入应为**未被抑制**的段集合
/// (与前端 displaySegments 一致)。二期自动折叠与手动选中共用一个口径,
/// 两边判出的集合永远相同。
pub fn overlapped_mic_seqs(segs: &[crate::store::SegmentRecord]) -> Vec<u64> {
    let sys: Vec<(u64, u64)> =
        segs.iter().filter(|s| s.source == "system").map(|s| (s.start_ms, s.end_ms)).collect();
    let mut out = Vec::new();
    for s in segs {
        if s.source != "mic" {
            continue;
        }
        let dur = s.end_ms.saturating_sub(s.start_ms).max(1);
        let ov: u64 = sys
            .iter()
            .map(|(a, b)| s.end_ms.min(*b).saturating_sub(s.start_ms.max(*a)))
            .sum();
        if ov as f64 / dur as f64 >= 0.8 {
            out.push(s.seq);
        }
    }
    out
}

#[cfg(test)]
mod behavior_gate_tests {
    use super::*;

    /// 旁听门:只在 listening 场、只对短段;外放门:只在 speaker_echo 场放宽一倍。
    #[test]
    fn gates_fire_only_in_their_scene() {
        assert!(listening_backchannel_gate(SC_LISTENING, 1_500));
        assert!(!listening_backchannel_gate(SC_LISTENING, 2_001), "长段是真发言,不拦");
        assert!(!listening_backchannel_gate(SC_HEADSET, 500), "非旁听场不拦");
        assert!(!listening_backchannel_gate(SC_UNKNOWN, 500), "未稳定不拦(保守)");
        assert_eq!(residue_rms_cap(SC_SPEAKER_ECHO, 0.012), 0.024);
        assert_eq!(residue_rms_cap(SC_HEADSET, 0.012), 0.012);
        assert_eq!(residue_rms_cap(SC_UNKNOWN, 0.012), 0.012);
    }
}

#[cfg(test)]
mod fold_tests {
    use super::*;

    fn seg(seq: u64, source: &str, a: u64, b: u64) -> crate::store::SegmentRecord {
        crate::store::SegmentRecord {
            seq,
            source: source.into(),
            text: "字".into(),
            start_ms: a,
            end_ms: b,
            speaker: None,
            rms: None,
        }
    }

    /// 与前端 segPick.ts 的表驱动口径一致:全覆盖命中、部分(<80%)不命中、
    /// system 段自身不入选。
    #[test]
    fn overlap_pick_mirrors_frontend_semantics() {
        let segs = vec![
            seg(0, "system", 0, 10_000),
            seg(1, "mic", 1_000, 3_000),  // 100% 覆盖 → 命中
            seg(2, "mic", 9_000, 12_000), // 1s/3s ≈ 33% → 不命中
            seg(3, "mic", 8_500, 10_400), // 1.5s/1.9s ≈ 79% → 不命中(边界下)
        ];
        assert_eq!(overlapped_mic_seqs(&segs), vec![1]);
    }
}
