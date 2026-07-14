# 回放跨轨门控（crossgate）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 回放混音时按转写段活跃度把非活跃的 mic 轨温和压低 -15dB，消除 mic 残余与 system 同源迟到叠加的重影感。

**Architecture:** 新纯模块 `player_gate.rs`（segments 解析 + 压低区间构建 + 增益查询）+ `player.rs` 两处最小侵入（Track 增 gate 字段、mix_frames 采样乘增益、load 时构建区间表）。任何失败空表降级 = 行为等同现状。

**Tech Stack:** Rust；serde_json（既有依赖）；无新依赖。

**规格:** `docs/superpowers/specs/2026-07-14-voice-notes-playback-crossgate-design.md`
**分支:** `playback-crossgate`（基于 master，独立 PR，不依赖 #41/#43）

## Global Constraints

- 门控单向：只压 mic 轨；system 轨区间表恒空（空表 → 增益恒 1.0，混音行为与现状逐采样一致）。
- 双讲（mic 段与 system 段时间重叠）→ 该区间不压：本人声音永远优先。
- 定值（常量集中定义并注释依据）：`DUCK_GAIN = 0.178`(-15dB)、`RAMP_SAMPLES = 1280`(80ms@16k)、`MERGE_GAP_MS = 300`、`MIN_SPAN_MS = 200`。渐变沿落在区间内侧，区间外增益严格 1.0。
- 增值层哲学：segments 缺失/损坏/解析错 → 空表 + eprintln 一行，回放照常；门控永不 panic。
- 零配置：无 UI、无设置项。
- 提交信息中文、动机导向，不加任何 Co-Authored-By / Generated-with 尾注。
- 每任务结束 `cd src-tauri && cargo test` 全绿再提交（master 基线 412+ 项）。

## 文件结构

| 文件 | 职责 |
|---|---|
| Create `src-tauri/src/player_gate.rs` | 纯函数：segments.jsonl 容错解析 + 压低区间构建 + 逐采样增益查询 |
| Modify `src-tauri/src/player.rs` | Track 增 `gate: Vec<GateSpan>`;mix_frames 乘增益;load 构建区间表 |
| Modify `src-tauri/src/lib.rs` | 挂 `mod player_gate;`（与既有 `mod player;` 同处) |

## 实现者必读的既有事实

- `Track` 在 `player.rs:51`：`{ data, offset_samples, len_samples, muted: AtomicBool, source: String }`；生产组装点 `player.rs:262`（mmap 分支），测试组装点 `mem_track`（`player.rs:406`）。
- `mix_frames`（`player.rs:96`）纯函数逐帧循环，track 采样经线性插值后 `acc +=`。**cursor 是笔记时间轴全局采样数**——门控区间表也按全局时间轴构建（segments 的 ms 即全局轴），`gain_at` 直接用全局 cursor 查询，与 track 的 offset 无关。
- segments.jsonl 行形如 `{"seq":0,"source":"system","text":"...","start_ms":22118,"end_ms":23368,...}`；只需 source/start_ms/end_ms 三字段，独立小结构容错解析，不耦合 store::notes 内部。

---

### Task 1: 门控纯模块 `player_gate.rs`

**Files:**
- Create: `src-tauri/src/player_gate.rs`
- Modify: `src-tauri/src/lib.rs`（`mod player;` 旁加 `mod player_gate;`，可见性与 player 一致）

**Interfaces:**
- Produces:
  - `#[derive(Debug, Clone, PartialEq)] pub struct GateSpan { pub start: u64, pub end: u64 }`（16k 全局时间轴采样数,压低区间)
  - `pub fn parse_segments_jsonl(path: &std::path::Path) -> Vec<(String, u64, u64)>` — (source,start_ms,end_ms),坏行跳过,文件不存在返回空
  - `pub fn build_gate(segments: &[(String, u64, u64)]) -> Vec<GateSpan>` — 排序去重后的压低区间
  - `pub fn gain_at(spans: &[GateSpan], sample: u64) -> f32`
  - `pub const DUCK_GAIN: f32 = 0.178;`

- [ ] **Step 1: 写失败测试**

```rust
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
    fn parse_tolerates_garbage_and_missing_file(){
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("segments.jsonl");
        assert!(parse_segments_jsonl(&p).is_empty(), "缺文件→空");
        std::fs::write(&p, "{\"source\":\"system\",\"start_ms\":1,\"end_ms\":2}\ngarbage\n{\"source\":\"mic\",\"start_ms\":3,\"end_ms\":4,\"text\":\"x\"}\n").unwrap();
        let v = parse_segments_jsonl(&p);
        assert_eq!(v, vec![("system".into(),1,2),("mic".into(),3,4)], "坏行跳过");
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test player_gate -- --nocapture`
Expected: 编译错误（模块不存在）

- [ ] **Step 3: 实现**

```rust
//! 回放跨轨门控(纯函数):按转写段活跃度构建 mic 轨压低区间,消混音重影。
//! 设计见 specs/2026-07-14-voice-notes-playback-crossgate-design.md。
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
```

差集扫描的 `mi` 指针注意:实现如上按 sys 段顺序推进,mic 并集与 sys 并集均已排序;若测试暴露跨段回退问题(某 mic 区间横跨两个 sys 段),把 `mi += 1` 的推进改为局部变量从 `mi` 起扫、不动全局指针——以测试全绿为准。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test player_gate -- --nocapture`
Expected: 5 个测试 PASS

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/player_gate.rs src-tauri/src/lib.rs
git commit -m "回放门控纯模块:segments活跃度→压低区间,双讲保护+间隙合并+渐变沿"
```

---

### Task 2: 混音接入 `player.rs`

**Files:**
- Modify: `src-tauri/src/player.rs`

**Interfaces:**
- Consumes: `crate::player_gate::{GateSpan, gain_at, build_gate, parse_segments_jsonl}`
- Produces: `Track` 增 `gate: Vec<GateSpan>` 字段;混音行为——空表轨逐采样与现状一致

- [ ] **Step 1: 写失败测试**

在 `player.rs` 测试模块追加（`mem_track` 造轨后手动放 gate）：

```rust
    /// 门控混音:mic 轨在压低区间内乘 DUCK_GAIN,区间外全量;system 轨(空表)不受影响。
    #[test]
    fn gated_mic_is_ducked_in_span_and_full_outside() {
        use crate::player_gate::{GateSpan, DUCK_GAIN};
        // mic 全程常值 8000;区间 [16000,48000) 压低(带 1280 渐变沿)。
        let mut mic = mem_track(&vec![8000i16; 64_000], 0, "mic");
        mic.gate = vec![GateSpan { start: 16_000, end: 48_000 }];
        let core = core_of(vec![mic]);
        let mut out = vec![0f32; 2]; // 单帧双声道,逐点采样
        let probe = |core: &Core, at: u64, out: &mut Vec<f32>| -> f32 {
            core.set_cursor(at as f64);
            mix_frames(core, out, 2, 1.0);
            out[0]
        };
        let full = 8000f32 / 32768.0;
        assert!((probe(&core, 1000, &mut out) - full).abs() < 1e-4, "区间外全量");
        let ducked = probe(&core, 30_000, &mut out);
        assert!((ducked - full * DUCK_GAIN).abs() < 1e-3, "腹地=DUCK: {ducked}");
        let edge = probe(&core, 16_000 + 640, &mut out);
        assert!(edge > ducked && edge < full, "渐变沿介于两者之间: {edge}");
    }

    /// 空 gate 表 = 现状:与未加门控的输出逐采样一致(既有测试的行为锚)。
    #[test]
    fn empty_gate_is_identity() {
        let a = mem_track(&[1000, 2000, 3000], 0, "mic");
        let core = core_of(vec![a]);
        let mut out = vec![0f32; 6];
        mix_frames(&core, &mut out, 2, 1.0);
        let expect = [1000f32, 1000., 2000., 2000., 3000., 3000.].map(|v| v / 32768.0);
        for (o, e) in out.iter().zip(expect) {
            assert!((o - e).abs() < 1e-6, "空表必须逐采样等于现状");
        }
    }
```

前置:`mem_track`/`core_of`/`mix_frames` 均既有;`mem_track` 需补 `gate: Vec::new()` 字段初始化(编译期强制所有构造点补齐,不会漏)。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test player:: -- --nocapture`
Expected: 编译错误(`Track` 无 gate 字段)

- [ ] **Step 3: 实现**

1. `Track` 增字段(`player.rs:51` 结构体):

```rust
    /// 回放压低区间(player_gate 构建;system/无段数据轨为空表=行为同现状)。
    gate: Vec<crate::player_gate::GateSpan>,
```

2. `mix_frames` 内轨采样乘增益(`player.rs:96` 循环体,替换 `acc +=` 一行):

```rust
                    let g = if t.gate.is_empty() {
                        1.0
                    } else {
                        crate::player_gate::gain_at(&t.gate, cursor as u64)
                    };
                    acc += (a + (b - a) * frac) * g;
```

(空表快路径避免无谓函数调用;`gain_at` 空表本身也返回 1.0,双保险。)

3. load 组装点(`player.rs:262` mmap 分支)构建区间表——在 `let mut loaded = Vec::new();` 之前读一次 segments:

```rust
    // 回放门控:按转写段活跃度构建 mic 轨压低区间(任何失败空表降级=现状)。
    let seg_path = note_dir.join("segments.jsonl");
    let gate_spans = {
        let segs = crate::player_gate::parse_segments_jsonl(&seg_path);
        if segs.is_empty() {
            Vec::new()
        } else {
            crate::player_gate::build_gate(&segs)
        }
    };
    if !gate_spans.is_empty() {
        eprintln!("回放门控: {} 个压低区间(mic 轨,-15dB,双讲保护)", gate_spans.len());
    }
```

Track 构造处:

```rust
            gate: if source == "mic" { gate_spans.clone() } else { Vec::new() },
```

`note_dir` 变量名以该函数实际为准(load 函数已有笔记目录路径,沿用同名变量;若为 `dir` 等按实际)。`mem_track`(测试)与其它 Track 构造点补 `gate: Vec::new()`。

- [ ] **Step 4: 跑测试确认通过 + 全量回归**

Run: `cd src-tauri && cargo test`
Expected: 新增 2 个 PASS;player 既有测试全绿(空表路径逐采样等价是硬要求)

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/player.rs
git commit -m "回放混音接入门控:mic轨按压低区间乘增益,空表路径与现状逐采样一致"
```

---

### Task 3: 数值验收与真机试听

**Files:** 无代码改动(验证任务;发现问题回 systematic-debugging)

- [ ] **Step 1: 全量回归**

Run: `cd src-tauri && cargo test`
Expected: 全绿

- [ ] **Step 2: 真实笔记数值验收(控制者执行)**

对笔记 20260714-170547(实测残余 0.128@170ms):解析其 segments.jsonl → `build_gate` → 统计:压低区间数、覆盖时长占比、双讲保护区间数;抽查 3 个压低区间与转写文本对照(该区间确实只有对方在说)。压低区间应覆盖 system 单独活跃的大部分时段。

- [ ] **Step 3: 真机试听(用户执行)**

分支构建后回放该笔记:重影感应消失;双讲段本人声音无衰减;渐变沿无咔嗒。

- [ ] **Step 4: 收尾**

```bash
git add -u
git commit -m "回放门控收尾:验收记录与注释零星修正"
```

(若无改动则跳过本提交。)
