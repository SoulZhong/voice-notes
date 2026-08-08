# 录音双方案可切换 · 第二期(回放切换 + 离线补生成 + UI)实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 同一篇笔记上回放可在「双轨对齐+门控(方案 A)」与「成品轨直放(方案 B)」之间当场切换;无成品轨的历史笔记可离线补生成;`mix_track` 与切换入口落进 UI。

**Architecture:** 回放切换不改 `player_load`——传单条 `source == "mixed"` 的轨时 `align_mic_track` 与 gate 构建天然旁路(已勘察确认),切换纯粹是前端换 `tracks` 数组;后端新增 ①`MixInfo` 完整性标记(正常定稿才写,回滚/panic 写不到,同时解决未转码 mixed 无时长读数的三期遗留),②`first_frame_offset` 落盘(live 混音轨的段落 seek 修正依据),③离线补生成核心(同一 `TimelineMixer`,`accept_at` 离线入口 + `player_align` 历史轨重采样,tmp+原子改名),④`mixed_playback_info` / `regenerate_mixed` 两条 IPC。

**Tech Stack:** Rust(cargo test)/ Svelte 5 runes + vitest / 既有 `TimelineMixer`、`player_align`、`NoteLock`、`TranscodeQueue`。

## Global Constraints

- 设计依据:`docs/superpowers/specs/2026-08-06-audio-scheme-ab-design.md`,冲突以 spec 为准。
- 工作目录:仓库根;cargo 在 `src-tauri/`。分支 `feature/audio-scheme-ab-phase2`。
- **硬约束:补生成/回放切换绝不改动源轨字节;mixed 仍是纯增值旁路。**
- **成品轨消费前必须过 `mixed_untrusted`(三期已建,retranscribe/input.rs:167),不另造判据。**
- 前端绝不把 mixed 与源轨同时传给 `player_load`(三重叠加),用源码护栏测试锁死。
- 新 UI 文案一律进 i18n 分片(zh/en 成对),`noHardcodedCjk.test.ts` 会卡硬编码中文。
- 代码注释风格:中文、讲约束与「为什么」,不讲流水账。
- 既有测试不许破坏:`cargo test` 全绿、`npx vitest run` 全绿、`npm run check` 0 错误。
- 每个 Task 结尾提交,消息用 `feat(mix):` / `test(mix):` / `feat(ui):` 前缀。**不加任何
  Co-Authored-By 落款**(2026-08-08 已把全史署名清除,勿再引入)。
- **勿跑全量 cargo fmt**(仓库约定,只 fmt 改动文件)。

## 非目标(本期不做,勿顺手)

- `Source` 枚举增 `Mixed` 变体 / `MIXED_TRACK` 常量归位存储层(分层待办注释保留原样)。
- click 测试标定两路端到端采集延迟差(spec §新口径的固有残余,留给实测)。
- 把 `echo_clean` 离线清洗补给 mixed(spec §对照条件 选项 3)——本期只做选项 1 的提示。
- 回放方案选择持久化(会话内状态即可,对比场景本来就是当场切)。
- 文件 ASR 切换(三期 PR#77 已交付)。

---

## File Structure

| 文件 | 职责 |
|---|---|
| `src-tauri/src/store/audio.rs`(改) | `SyncInfo` 增 `first_frame_offset_ms`;`TrackMeta` 增 `mix: Option<MixInfo>`;`set_track_mix()` |
| `src-tauri/src/lib.rs`(改) | `record_sync` 填首帧偏移;新增 `regenerate_mixed`/`mixed_regen_status`/`mixed_playback_info` 三命令 + 守卫 + 事件;注册 invoke_handler |
| `src-tauri/src/pipeline/recording_sink.rs`(改) | 混音线程正常定稿时写 `MixInfo{origin:"live"}` |
| `src-tauri/src/retranscribe/input.rs`(改) | `mixed_untrusted` 时长读数链增 `mix.track_ms` 回退 |
| `src-tauri/src/store/mix_regen.rs`(新建) | 离线补生成纯核心:字节进、WAV 出,无 Tauri 依赖,可单测 |
| `src-tauri/src/store/mod.rs`(改) | 注册 `pub mod mix_regen;` |
| `src-tauri/src/player.rs`(改) | `aligned_track_offset_ms` 提权 `pub(crate)` 供补生成复用 |
| `src/lib/notes.ts`(改) | `TrackInfo.source` 放宽;`MixedPlaybackInfo` 类型;三个 IPC 包装 |
| `src/routes/notes/[id]/+page.svelte`(改) | 回放 A/B 切换按钮、生成动作、seek 修正、口径护栏提示 |
| `src/routes/settings/+page.svelte`(改) | 录制 section 增 `mix_track` 开关 |
| `src/lib/i18n/dict/notes.ts` / `dict/settings.ts`(改) | 新键 zh/en 成对 |
| `src/lib/mixPlayback.test.ts`(新建) | 源码护栏:不得混传 mixed 与源轨;seek 修正必须在位 |

---

### Task 1: `first_frame_offset` 落盘(SyncInfo 增字段)

**Files:**
- Modify: `src-tauri/src/store/audio.rs:160`(SyncInfo)
- Modify: `src-tauri/src/lib.rs:1857-1884`(record_sync 构造 SyncInfo 处)
- Test: `src-tauri/src/store/audio.rs` 内联 `mod tests`

**Interfaces:**
- Produces: `SyncInfo.first_frame_offset_ms: Option<u64>`(None = 旧数据,按 0 消费)。
  Task 3(live MixInfo 的 seek 表)与 Task 6(mixed_playback_info)消费。

- [x] **Step 1: 写失败测试**(audio.rs tests 模块追加)

```rust
/// 旧 audio.json(无 first_frame_offset_ms)必须照常反序列化为 None;
/// 新写出的 JSON 有该字段且往返保真。字段语义:本源首个真实帧相对本场最早
/// 首帧的偏移(16k 口径换算成 ms),是 mixed 轨段落 seek 修正的数据来源。
#[test]
fn sync_first_frame_offset_roundtrip_and_backcompat() {
    let old = r#"{"wall_ms":1,"samples":2,"track_ms":3,"drift_ms":2,"silence_ms":0,"gaps":0,"rate_fixes":0}"#;
    let s: SyncInfo = serde_json::from_str(old).expect("旧数据必须能解析");
    assert_eq!(s.first_frame_offset_ms, None);

    let with = SyncInfo { first_frame_offset_ms: Some(120), ..s };
    let json = serde_json::to_string(&with).unwrap();
    let back: SyncInfo = serde_json::from_str(&json).unwrap();
    assert_eq!(back.first_frame_offset_ms, Some(120));
}
```

注意:该测试用了 `..s` 展开,`SyncInfo` 需要 `Clone`(已有 derive 则不动;没有就补)。

- [x] **Step 2: 跑测试确认失败**

`cd src-tauri && cargo test sync_first_frame_offset` — 期望编译错误(字段不存在)。

- [x] **Step 3: 实现**

`SyncInfo`(audio.rs:160)追加字段:

```rust
    /// 本源首个真实帧相对本场最早首帧的偏移(ms)。mixed 轨里该源内容整体后移
    /// 这么多(spec §口径差),段落 seek 到 mixed 时要加回去。续录每场覆盖,
    /// 与本结构其余字段同限制。旧数据无此字段 → None,消费方按 0 处理。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_frame_offset_ms: Option<u64>,
```

`lib.rs` `record_sync` 构造 `SyncInfo` 处(:1857-1884 内)加一行:

```rust
first_frame_offset_ms: Some(health.first_frame_offset_16k() / 16),
```

全仓 `grep -rn "SyncInfo {" src-tauri/src` 找出其余字面量构造点(mixed_untrusted 的测试等),逐个补 `first_frame_offset_ms: None,`(或 `..` 展开)。

- [x] **Step 4: 跑测试确认通过**

`cargo test sync_first_frame_offset` PASS;`cargo test` 全绿(字面量构造点全补齐才会绿)。

- [x] **Step 5: 提交**

`git add -A && git commit -m "feat(mix): SyncInfo 落盘首帧偏移——mixed 段落 seek 修正的数据来源"`

---

### Task 2: `MixInfo` 完整性标记 + `mixed_untrusted` 回退

**Files:**
- Modify: `src-tauri/src/store/audio.rs`(TrackMeta、新结构、set_track_mix)
- Modify: `src-tauri/src/retranscribe/input.rs:167-206`(mixed_untrusted)
- Test: 两文件各自内联 tests

**Interfaces:**
- Produces:
  - `pub struct MixInfo { pub origin: String, pub seek_offset_ms: BTreeMap<String, u64>, pub track_ms: u64 }`
  - `pub fn set_track_mix(note_dir: &Path, source: &str, mix: MixInfo) -> anyhow::Result<()>`(照 set_track_sync 模板,audio.rs:392)
  - `TrackMeta.mix: Option<MixInfo>`
  - `mixed_untrusted` 的 mixed 时长读数链:`duration_ms → sync.track_ms → mix.track_ms`
- Consumes: 无(Task 1 独立)。Task 3/4/5/6 消费。

- [x] **Step 1: 写失败测试**

audio.rs tests:

```rust
/// MixInfo 是「正常定稿」的盘上证据:回滚失败/线程 panic 两条残留路径(见
/// mixed_track 文档)都写不到它。set_track_mix 走读改写 audio.json,不碰其他字段。
#[test]
fn set_track_mix_persists_and_preserves_other_fields() {
    let dir = tempfile::tempdir().unwrap();
    set_track_sync(dir.path(), "mixed", SyncInfo {
        wall_ms: 1, samples: 0, track_ms: 5000, drift_ms: 4999,
        silence_ms: 0, gaps: 0, rate_fixes: 0, first_frame_offset_ms: None,
    }).unwrap();
    let mix = MixInfo {
        origin: "live".into(),
        seek_offset_ms: [("system".to_string(), 120u64)].into_iter().collect(),
        track_ms: 5000,
    };
    set_track_mix(dir.path(), "mixed", mix.clone()).unwrap();
    let meta = load_audio_meta(dir.path());
    let t = meta.tracks.get("mixed").expect("track 条目");
    assert_eq!(t.mix.as_ref(), Some(&mix));
    assert!(t.sync.is_some(), "既有字段不得被覆盖丢失");
}
```

input.rs tests(参照 :207 起既有用例的构造手法,复制其 meta 构造 helper):

```rust
/// 未转码的 mixed.wav(Windows 恒如此;macOS 转码失败降级)没有 duration_ms,
/// mixed 又永远没有 sync(record_sync 只遍历 mic/system)——三期在这种笔记上
/// 恒判「缺少时长读数」。二期起 MixInfo.track_ms 作第三读数来源。
#[test]
fn mix_info_track_ms_serves_as_duration_fallback() {
    let mut meta = /* 照既有测试构造:mic/system 带 offset+sync.track_ms,
                      mixed 条目存在但无 duration_ms、无 sync */;
    meta.tracks.get_mut("mixed").unwrap().mix = Some(crate::store::audio::MixInfo {
        origin: "live".into(),
        seek_offset_ms: Default::default(),
        track_ms: /* 与 max(源轨终点) 相符的值 */,
    });
    assert_eq!(mixed_untrusted(&meta), None, "有 MixInfo.track_ms 即可校验通过");
}
```

(`/* */` 处照抄同文件既有 `mixed_untrusted` 测试的构造数值,保持同一口径。)

- [x] **Step 2: 跑测试确认失败**(编译错:MixInfo 不存在)

- [x] **Step 3: 实现**

audio.rs 在 `CleanInfo` 附近新增:

```rust
/// 成品轨完整性标记 + 消费口径。只在混音**正常定稿**(实时)或补生成**原子改名
/// 成功后**(离线)写入;回滚、放弃、panic 路径全都到不了写入点——因此它的存在
/// 本身就是「这条轨是完整产物」的盘上证据,mixed_track() 文档里两条无标记残留
/// 路径自此可判定。缺失不单独定罪(一期录的 mixed 没有它),时长交叉核对仍是
/// 最终裁决,见 retranscribe::input::mixed_untrusted。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MixInfo {
    /// "live"(录制期混出,时间轴含首帧偏移)或 "regen"(离线补生成,按
    /// offset_ms 定位,段落 seek 无需修正)。
    pub origin: String,
    /// 消费 mixed 时各源段落 seek 要加的修正量(ms)。live = 各源首帧偏移
    /// (末场值;续录多场的历史场次只能近似,量级数十~数百 ms)。regen = 空表。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub seek_offset_ms: BTreeMap<String, u64>,
    /// 定稿时量出的净时长(WAV 字节口径,同 SyncInfo.track_ms 语义)。
    /// 未转码时 mixed_untrusted 的时长读数来源。
    pub track_ms: u64,
}
```

`TrackMeta` 增:

```rust
    /// 成品轨专用,见 MixInfo。源轨恒 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mix: Option<MixInfo>,
```

`set_track_mix` 照抄 `set_track_sync`(:392)逐行改字段名。

input.rs `mixed_untrusted` 里 mixed 时长解析处(duration_ms → sync.track_ms 的链尾)追加
`.or_else(|| track.mix.as_ref().map(|m| m.track_ms))`,并同步其文档注释。

- [x] **Step 4: 跑测试确认通过**(`cargo test set_track_mix` / `cargo test mix_info_track_ms` / 全量)

- [x] **Step 5: 提交** `feat(mix): MixInfo 完整性标记——定稿才写,兼作未转码 mixed 的时长读数`

---

### Task 3: 实时路径正常定稿写 `MixInfo{origin:"live"}`

**Files:**
- Modify: `src-tauri/src/pipeline/recording_sink.rs:253-352`(混音线程)
- Test: 同文件 tests(:390 起)

**Interfaces:**
- Consumes: Task 2 的 `set_track_mix`/`MixInfo`;`SourceHealth::first_frame_offset_16k()`;
  `store::audio::session_track_ms(note_dir, source, base_ms)`(audio.rs:248,以实际签名为准)。
- Produces: 正常停录后 `audio.json` 的 mixed 条目带 `mix`;所有放弃/回滚路径不带。

- [x] **Step 1: 写失败测试**

扩展既有 `with_mix_produces_mixed_track_of_equal_length`(:498)结尾:

```rust
    let meta = crate::store::audio::load_audio_meta(dir.path());
    let mix = meta.tracks.get(MIXED_TRACK).and_then(|t| t.mix.as_ref())
        .expect("正常定稿必须写 MixInfo");
    assert_eq!(mix.origin, "live");
    assert!(mix.track_ms > 0);
```

扩展 `starvation_after_partial_success_removes_already_written_mixed_track`(:716)与
`full_mixed_queue_marks_sidecar_abandoned`(:610)结尾:

```rust
    let meta = crate::store::audio::load_audio_meta(dir.path());
    assert!(meta.tracks.get(MIXED_TRACK).and_then(|t| t.mix.as_ref()).is_none(),
        "放弃/回滚路径不得留下完整性标记");
```

- [x] **Step 2: 跑测试确认失败**(第一条 expect panic)

- [x] **Step 3: 实现**

混音线程正常走完(队列自然关闭、未 abandoned、writer finalize 成功)之后、线程返回前:

```rust
// 正常定稿的唯一出口:写完整性标记。任何 abandon/rollback 分支都在此之前 return,
// 保证「有 MixInfo ⇔ 内容完整」。写失败只降级(轨本身是好的,消费方退回时长交叉核对)。
let track_ms = crate::store::audio::session_track_ms(&note_dir, MIXED_TRACK, base_ms);
let seek: std::collections::BTreeMap<String, u64> = first_offsets.iter()
    .map(|(s, h)| (s.as_str().to_string(), h.first_frame_offset_16k() / 16))
    .collect();
if let Err(e) = crate::store::audio::set_track_mix(&note_dir, MIXED_TRACK, crate::store::audio::MixInfo {
    origin: "live".into(),
    seek_offset_ms: seek,
    track_ms,
}) {
    eprintln!("[mix] 完整性标记写入失败(轨内容不受影响): {e}");
}
```

线程闭包需要 `note_dir`/`base_ms`/`first_offsets` 的克隆——`MixedSink` 已持有
(`first_offsets` :200,装配时 clone 进线程;`note_dir`/`base_ms` 从 `inner` 或装配参数取)。

- [x] **Step 4: 跑测试确认通过**(`cargo test recording_sink` 全部,尤其 :498/:610/:716/:768/:848/:916 六条)

- [x] **Step 5: 提交** `feat(mix): 实时混音正常定稿写完整性标记,放弃与回滚路径不写`

---

### Task 4: 离线补生成纯核心 `store/mix_regen.rs`

**Files:**
- Create: `src-tauri/src/store/mix_regen.rs`
- Modify: `src-tauri/src/store/mod.rs`(注册)
- Modify: `src-tauri/src/player.rs:322`(`aligned_track_offset_ms` 前加 `pub(crate)`)
- Test: 新文件内联 tests

**Interfaces:**
- Consumes: `TimelineMixer::{new, accept_at, finish, win_len}`(audio/timeline_mix.rs)、
  `player_align::{TimeMap, render_aligned_to, map_ms_signed}`。
- Produces:
  ```rust
  pub struct RegenOutcome { pub offset_ms: u64, pub track_ms: u64 }
  /// 两轨 canonical WAV(44 头、16k 单声道 s16le)字节 + 各自 offset_ms(+可选
  /// mic→system 时基映射)→ 把成品轨 WAV 写进 sink。纯函数,无盘面副作用。
  pub fn regen_mixed_to<W: std::io::Write + std::io::Seek>(
      mic: &[u8], mic_off_ms: u64,
      sys: &[u8], sys_off_ms: u64,
      map: Option<&crate::player_align::TimeMap>,
      sink: &mut W,
  ) -> anyhow::Result<RegenOutcome>
  ```
  Task 5 消费。

**实现要点(写进代码注释):**
- `map` 有值 → 先 `render_aligned_to(mic, map, &mut Vec::new())` 得重采样 mic 字节,
  并用 `aligned_track_offset_ms(mic_off_ms, map)` 的同款算式修 mic offset
  (`(mic_off_ms as i64 + map_ms_signed(map, 0)).max(0) as u64`)。对齐发生在生成期,
  产物定稿后回放不再估计(spec §离线补生成)。
- 时间轴原点 = `min(mic_off, sys_off)`;各源起点样本 = `(off − origin) * 16`。
  **按 offset_ms 定位 ⇒「文件内毫秒 + offset_ms == 时间轴毫秒」经典口径成立,
  段落 seek 不需要任何修正——这是与 live 轨的本质差异,MixInfo.origin 记 "regen"。**
- 两源交替喂 1s 块(`accept_at`),喂完 `finish()`。交替喂使混音窗上界 ≈ 两源
  offset 差 + 2s,不随轨长增长;每轮断言 `win_len()` 防回归。
- 输出 f32 和 → clamp [-1,1] → s16le(与 AudioTrackWriter 同口径);先写 44 字节占位头,
  流式写数据,末尾 Seek 回补 RIFF/data 长度。
- `track_ms = bytes_to_ms(数据字节数)`;`offset_ms = origin 对应毫秒`。

- [x] **Step 1: 写失败测试**(新文件先只写 tests)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// canonical WAV 构造器:44 头 + s16le@16k 单声道。
    fn wav(samples: &[i16]) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut v = Vec::with_capacity(44 + data.len());
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&16000u32.to_le_bytes());
        v.extend_from_slice(&32000u32.to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(&data);
        v
    }

    fn pcm_of(out: &[u8]) -> Vec<i16> {
        out[44..].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect()
    }

    /// 等 offset 两轨:输出逐样本和,offset 取公共值,track_ms 按字节量出。
    #[test]
    fn same_offset_tracks_sum_pointwise() {
        let mic = wav(&[1000, 2000, 3000]);
        let sys = wav(&[10, 20, 30]);
        let mut out = Cursor::new(Vec::new());
        let r = regen_mixed_to(&mic, 500, &sys, 500, None, &mut out).unwrap();
        assert_eq!(r.offset_ms, 500);
        assert_eq!(pcm_of(out.get_ref()), vec![1010, 2020, 3030]);
    }

    /// 不等 offset:后起源在成品轨里带前导静音区(另一源独占),位置按时间轴对齐。
    /// origin = min(offset);system 比 mic 晚 1ms(16 样本)⇒ system 内容整体右移 16。
    #[test]
    fn offset_gap_becomes_leading_solo_region() {
        let mic = wav(&[100; 32]);
        let sys = wav(&[7; 16]);
        let mut out = Cursor::new(Vec::new());
        let r = regen_mixed_to(&mic, 0, &sys, 1, None, &mut out).unwrap();
        assert_eq!(r.offset_ms, 0);
        let pcm = pcm_of(out.get_ref());
        assert_eq!(pcm.len(), 32);
        assert!(pcm[..16].iter().all(|&s| s == 100), "前 16 样本 mic 独占");
        assert!(pcm[16..].iter().all(|&s| s == 107), "后 16 样本两源叠加");
    }

    /// 和溢出 s16 → clamp,不回绕(与 AudioTrackWriter 同口径)。
    #[test]
    fn sum_clamps_instead_of_wrapping() {
        let mic = wav(&[i16::MAX]);
        let sys = wav(&[i16::MAX]);
        let mut out = Cursor::new(Vec::new());
        regen_mixed_to(&mic, 0, &sys, 0, None, &mut out).unwrap();
        assert_eq!(pcm_of(out.get_ref()), vec![i16::MAX]);
    }

    /// 恒等映射(0→0, 10→10)走对齐分支但不改内容——验证 map 管线接通且
    /// offset 修正为 0。
    #[test]
    fn identity_map_is_passthrough() {
        let map = crate::player_align::TimeMap::new(vec![(0.0, 0.0), (10.0, 10.0)]).unwrap();
        let mic = wav(&[500; 160]);
        let sys = wav(&[5; 160]);
        let mut out = Cursor::new(Vec::new());
        let r = regen_mixed_to(&mic, 0, &sys, 0, Some(&map), &mut out).unwrap();
        assert_eq!(r.offset_ms, 0);
        let pcm = pcm_of(out.get_ref());
        assert_eq!(pcm.len(), 160);
        assert!(pcm.iter().all(|&s| s == 505));
    }

    /// RIFF/data 头长度字段与实际数据一致(流式写 + Seek 回补的正确性)。
    #[test]
    fn header_lengths_match_payload() {
        let mic = wav(&[1; 100]);
        let sys = wav(&[2; 100]);
        let mut out = Cursor::new(Vec::new());
        regen_mixed_to(&mic, 0, &sys, 0, None, &mut out).unwrap();
        let b = out.get_ref();
        let riff = u32::from_le_bytes([b[4], b[5], b[6], b[7]]) as usize;
        let data = u32::from_le_bytes([b[40], b[41], b[42], b[43]]) as usize;
        assert_eq!(riff, b.len() - 8);
        assert_eq!(data, b.len() - 44);
    }
}
```

- [x] **Step 2: 跑测试确认失败**(`cargo test mix_regen` — 编译错)

- [x] **Step 3: 实现**(按上方要点;`store/mod.rs` 注册;`player.rs:322` fn 前加 `pub(crate)`
  并在 mix_regen 里复用或复刻其一行算式)

- [x] **Step 4: 跑测试确认通过**(`cargo test mix_regen`,含 render_aligned_to 分支)

- [x] **Step 5: 提交** `feat(mix): 离线补生成纯核心——同一混音器的 accept_at 离线入口`

---

### Task 5: `regenerate_mixed` 命令(守卫链 + NoteLock + 事件)

**Files:**
- Modify: `src-tauri/src/lib.rs`(状态槽、命令、worker、invoke_handler 注册)
- Modify: `src/lib/notes.ts`(IPC 包装)
- Test: lib.rs 内联守卫纯函数单测

**Interfaces:**
- Consumes: Task 4 `regen_mixed_to`;`store::align::{read, write}`;`player_align::{estimate, worth_correcting}`;
  `store::notelock::NoteLock::acquire`;`transcode::{decode_m4a_to_standard_wav, TranscodeQueue::is_busy, enqueue}`;
  `store::audio::{waveform_from_wav, set_track_waveform, set_track_mix, load_audio_meta}`;
  守卫纯函数 `recording_blocks_retranscribe`(lib.rs:847,语义通用,直接复用)。
- Produces:
  - `#[tauri::command] fn regenerate_mixed(app: AppHandle, id: String) -> Result<(), String>`
  - `#[tauri::command] fn mixed_regen_status(state: …) -> Result<Option<String>, String>`(Some = 正在跑的 note_id)
  - 事件 `"mixed_regen"`:`{ note_id: String, stage: "decode"|"align"|"mix"|"finish", status: "running"|"ok"|"error", message: Option<String> }`
  - TS:`export const regenerateMixed = (id: string) => invoke("regenerate_mixed", { id });`
    `export const mixedRegenStatus = () => invoke<string | null>("mixed_regen_status");`

**守卫链(照抄 do_retranscribe :2143-2251 的顺序纪律,doc :2134-2142):**

1. `validate_note_id(&id)`
2. 迁移/下载中拒(`download_running`)
3. 录制中快拒 —— **`let session_active = state.session.lock().unwrap().is_some();` 独立成句**
   (lib.rs:2167 的 ABBA 锁序纪律),再 `recording_blocks_retranscribe(&state.running, session_active)`
4. 重转写占槽中拒(retranscribe slot 非空 → 拒:两者都重读盘上音频)
5. 转码中拒 `state.transcode.is_busy(&dir)`
6. 双轨可用校验:audio.json 里 mic 与 system 都有条目且 wav/m4a 至少一形态在盘上,
   否则 `Err(tr!("需要 mic 与 system 双轨才能补生成成品轨", "Regenerating requires both mic and system tracks"))`
7. 占槽 `*state.mixed_regen.lock().unwrap() = Some(id.clone())`
8. **Dekker 写后读**:复查 3/2/4,任一命中 → 清槽 + Err
9. spawn worker 线程

**Worker(线程内,全程持锁):**

```text
NoteLock::acquire(&dir) —— None → emit error + 清槽 + return(文案照 lib.rs:2287 模板,
  注意「不写'或转码中'」的既有措辞纪律)
emit(stage=decode, running)
两源各取 canonical WAV 字节:wav 在 → fs::read;仅 m4a → decode_m4a_to_standard_wav
  到 note_dir/.mixregen_{src}.tmp.wav 再读再删
emit(stage=align, running)
map = store::align::read(&dir)  —— 回放侧已估过就复用
  .or_else(|| estimate(mic, mic_off, sys, sys_off)
      .filter(worth_correcting)
      .inspect(|a| store::align::write(&dir, &a.map) 忽略失败)
      .map(|a| a.map))
emit(stage=mix, running)
File::create(dir/"mixed.wav.tmp") → regen_mixed_to(...) → flush
校验:tmp 长度 > 44,否则删 tmp + emit error
删旧 mixed.m4a(有则);audio.json 里 mixed 条目清 codec/duration_ms/waveform(过期读数)
rename(mixed.wav.tmp → mixed.wav)   —— 原子切换,半成品永不可见
set offset_ms(写 meta:tracks["mixed"].offset_ms = outcome.offset_ms)
set_track_mix(&dir, "mixed", MixInfo{ origin:"regen", seek_offset_ms: 空表, track_ms: outcome.track_ms })
waveform_from_wav(dir/"mixed.wav") → set_track_waveform(失败只降级)
macOS:state.transcode.enqueue(dir)
emit(stage=finish, ok)
清槽 —— 必须在终态 emit 之后(照 spawn_retranscribe Fix 3,:2343-2349)
```

清 stale meta + 写 offset 若无现成 store API,则在 audio.rs 加一个私有性质的
`pub fn reset_mixed_meta(note_dir: &Path, offset_ms: u64) -> anyhow::Result<()>`
(读改写模板同 set_track_sync:清 codec/duration_ms/waveform/mix,写 offset_ms)。

- [x] **Step 1: 写失败测试**(lib.rs tests,守卫纯函数级)

```rust
/// 补生成与录制/重转写互斥:占槽判定是纯函数,三态各自可测。
#[test]
fn mixed_regen_slot_blocks_and_clears() {
    let slot: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    assert!(!mixed_regen_busy(&slot));
    *slot.lock().unwrap() = Some("n1".into());
    assert!(mixed_regen_busy(&slot));
}
```

(`mixed_regen_busy` 是新的一行纯函数,与 `retranscribe_blocks_recording` :840 同款形态,
录制启动侧与重转写侧的守卫都要接它——录制 spawn 前、do_retranscribe 守卫链里各加一查,
反向互斥闭环。)

- [x] **Step 2: 跑测试确认失败**

- [x] **Step 3: 实现**(命令 + worker + 三处反向互斥接线 + invoke_handler 注册 + notes.ts 包装)

- [x] **Step 4: 跑测试确认通过**(`cargo test mixed_regen` + 全量;`npm run check`)

- [x] **Step 5: 提交** `feat(mix): 离线补生成命令——守卫链/NoteLock/原子改名/事件,照重转写纪律`

---

### Task 6: `mixed_playback_info` 命令(消费侧一站式读数)

**Files:**
- Modify: `src-tauri/src/lib.rs`(命令 + 波形懒回填)
- Modify: `src/lib/notes.ts`(类型 + 包装;`TrackInfo.source` 放宽)
- Test: lib.rs 内联(store 层构造临时笔记目录)

**Interfaces:**
- Consumes: `store::audio::{mixed_track, load_audio_meta, backfill_wav_waveform}`;
  `retranscribe::input::mixed_untrusted`;Task 2 的 `MixInfo`。
- Produces:
  ```rust
  #[derive(Serialize)]
  pub struct MixedPlaybackInfo {
      pub track: Option<crate::store::audio::TrackInfo>, // None = 无成品轨(前端给「生成」动作)
      pub untrusted: Option<String>,                     // Some = 有轨但不可信(置灰 + tooltip 原因)
      pub seek_offset_ms: std::collections::BTreeMap<String, u64>, // 空表 = 无需修正(regen 轨/旧轨)
      pub ab_caveat: bool,       // mic 轨带 CleanInfo:A 侧多一级离线清洗,听感不可直比
  }
  #[tauri::command]
  fn mixed_playback_info(app: AppHandle, id: String) -> Result<MixedPlaybackInfo, String>
  ```
  - TS:
  ```ts
  export interface MixedPlaybackInfo {
    track: (Omit<TrackInfo, "source"> & { source: string }) | null;
    untrusted: string | null;
    seek_offset_ms: Record<string, number>;
    ab_caveat: boolean;
  }
  export const mixedPlaybackInfo = (id: string) =>
    invoke<MixedPlaybackInfo>("mixed_playback_info", { id });
  ```
  另:`src/lib/notes.ts:74-82` 的 `TrackInfo.source` 由 `Source` 放宽为 `Source | "mixed"`
  (mixed 轨要过 `player_load`;`AudioPlayer` 对未知 source 的每轨静音菜单只在
  `tracks.length > 1` 时出现,单轨装载不受影响)。

**逻辑:**

```text
dir 解析 + validate → meta = load_audio_meta
track = mixed_track(&dir)
untrusted = track.is_some().then(|| mixed_untrusted(&meta)).flatten()
seek_offset_ms = meta.tracks["mixed"].mix.map(|m| m.seek_offset_ms).unwrap_or_default()
ab_caveat = meta.tracks.get("mic").map(|t| t.clean.is_some()).unwrap_or(false)
波形懒回填:track 有值 && waveform 为 None && mixed.wav 在盘 → 照 note_audio_info 的
  INFLIGHT 去重样板(lib.rs:4429-4470)后台 spawn backfill_wav_waveform(&dir, "mixed"),
  完成后发 transcode_done 同款重拉信号(直接 emit "transcode_done" 即可复用前端既有订阅,:521-534)
```

- [x] **Step 1: 写失败测试**(lib.rs tests;构造 tempdir 笔记:双轨 sync + mixed wav + MixInfo)

```rust
/// mixed_playback_info 的读数拼装:轨在+可信 → track Some/untrusted None;
/// seek 表原样透传;mic 带 clean → ab_caveat。
#[test]
fn mixed_playback_info_assembles_readings() { /* 构造 → 调内部拼装函数(把纯逻辑抽成
    fn assemble_mixed_playback(meta, track) 便于单测,命令壳只做 dir 解析) */ }
```

- [x] **Step 2: 跑失败** → **Step 3: 实现**(拼装抽纯函数)→ **Step 4: 全绿 + npm run check**

- [x] **Step 5: 提交** `feat(mix): mixed_playback_info——回放消费侧一站式读数(可信性/seek 表/AB 口径告警)`

---

### Task 7: 详情页回放 A/B 切换 + 生成动作 + seek 修正

**Files:**
- Modify: `src/routes/notes/[id]/+page.svelte`(transport 行 + state + seek)
- Modify: `src/lib/i18n/dict/notes.ts`(新键 zh/en)
- Create: `src/lib/mixPlayback.test.ts`(源码护栏)

**Interfaces:**
- Consumes: Task 5/6 的 TS 包装与事件。
- Produces: UI 行为;`playbackScheme` 会话内状态。

**新 i18n 键(zh 区与 en 区各一份,键名相同):**

```ts
// notes.mix.* —— 回放方案切换(二期)
"notes.mix.dual": "双轨",
"notes.mix.mixed": "成品轨",
"notes.mix.title": "回放方案:双轨对齐(方案 A)或成品轨直放(方案 B)",
"notes.mix.none": "此笔记还没有成品轨",
"notes.mix.generate": "生成成品轨",
"notes.mix.generating": "生成中({stage})…",
"notes.mix.genFailed": "成品轨生成失败:{message}",
"notes.mix.abCaveat": "此场次 mic 轨经过离线回声清洗,A/B 听感不可直比(A 侧多一级抑制)",
```

(en 对应:"Dual tracks" / "Mixed track" / "Playback scheme: dual-track aligned (A) or mixed direct (B)" /
"No mixed track yet" / "Generate mixed track" / "Generating ({stage})…" /
"Failed to generate mixed track: {message}" /
"The mic track was echo-cleaned offline; A/B listening comparison is not apples-to-apples (A has one extra suppression pass)")

**页面改动要点:**

```svelte
// state(:106 附近)
let playbackScheme = $state<"dual" | "mixed">("dual");
let mixedInfo = $state<MixedPlaybackInfo | null>(null);
let regenStage = $state<string | null>(null);   // null = 未在生成

// 拉取:与 tracks 拉取同一 effect 组(:521-534 后追加),依赖 id + tracksVersion
$effect(() => { void noteId; void tracksVersion;
  mixedPlaybackInfo(noteId).then((i) => (mixedInfo = i)).catch(() => (mixedInfo = null));
});

// 事件:mixed_regen → 更新 regenStage;终态 ok 时 bump tracksVersion 重拉
// (订阅样板照 :663-710 重转写事件)

// 装载数组:二选一,绝不 concat —— mixPlayback.test.ts 锁死
const playerTracks = $derived(
  playbackScheme === "mixed" && mixedInfo?.track && !mixedInfo.untrusted
    ? [mixedInfo.track]
    : tracks,
);

// mixed 不可用时強制回落 dual(轨被删/变不可信)
$effect(() => { if (playbackScheme === "mixed" && (!mixedInfo?.track || mixedInfo.untrusted)) playbackScheme = "dual"; });
```

`<AudioPlayer {tracks}` 处换成 `tracks={playerTracks}`;waveform:mixed 态且
`mixedInfo.track.waveform` 有值时优先用它(`Array.from` 转 number[]),否则沿用现状聚合。

**切换按钮**(`.transport` 行内、player-slot 之后,样式随 `.view-switch` 的 pill 风格):

```svelte
{#if canEdit && tracks.length > 0}
  <div class="mix-switch" title={t("notes.mix.title")}>
    <button class="link" class:active={playbackScheme === "dual"}
            onclick={() => (playbackScheme = "dual")}>{t("notes.mix.dual")}</button>
    {#if mixedInfo?.track}
      <button class="link" class:active={playbackScheme === "mixed"}
              disabled={mixedInfo.untrusted !== null}
              title={mixedInfo.untrusted ?? t("notes.mix.title")}
              onclick={() => (playbackScheme = "mixed")}>{t("notes.mix.mixed")}</button>
    {:else}
      <button class="link"
              disabled={regenStage !== null || recording.isLive}
              title={t("notes.mix.none")}
              onclick={startRegen}>
        {regenStage ? t("notes.mix.generating", { stage: regenStage }) : t("notes.mix.generate")}
      </button>
    {/if}
    {#if playbackScheme === "mixed" && mixedInfo?.ab_caveat}
      <span class="refine-warn">{t("notes.mix.abCaveat")}</span>
    {/if}
  </div>
{/if}
```

`startRegen` 照 `startRetranscribe`(:1116-1128)的快失败竞态样板:调 `regenerateMixed(noteId)`,
catch → 立即恢复按钮态并示错。

**seek 修正:** 找到段落点击→seek 的调用点(`grep -n "seek(" src/routes/notes/[id]/+page.svelte`
与 `playerMs =` 赋值处),对 seek 目标毫秒数应用:

```ts
const seekFix = (ms: number, source: string) =>
  playbackScheme === "mixed" ? ms + (mixedInfo?.seek_offset_ms[source] ?? 0) : ms;
```

段落条目携带其 `source`;regen 轨 seek 表为空 ⇒ 修正恒 0,天然正确。

**源码护栏 `src/lib/mixPlayback.test.ts`**(editorReactivity.test.ts:10-14 同款 ?raw 手法):

```ts
import { describe, expect, it } from "vitest";

const source = import.meta.glob(["../routes/notes/[id]/+page.svelte"], {
  eager: true, query: "?raw", import: "default",
}) as Record<string, string>;
const page = source["../routes/notes/[id]/+page.svelte"];

// mixed 与源轨同时装载 = 三重叠加(mixed 本就是 mic+system 混出),这条约束
// 在 node 环境跑不起组件,靠读源码守住(手法同 editorReactivity.test.ts)。
describe("mixed 回放装载纪律", () => {
  it("装载数组是二选一表达式,不存在 mixed 与源轨的拼接", () => {
    expect(page).toMatch(/playbackScheme === "mixed"[\s\S]{0,200}\[mixedInfo\.track\]\s*:\s*tracks/);
    expect(page).not.toMatch(/tracks\.concat|\.\.\.tracks,\s*mixedInfo|mixedInfo\.track,\s*\.\.\.tracks/);
  });
  it("mixed 态 seek 必须带 seek_offset_ms 修正", () => {
    expect(page).toMatch(/seek_offset_ms\[/);
  });
  it("不可信成品轨不得进装载数组", () => {
    expect(page).toMatch(/!mixedInfo\.untrusted|mixedInfo\.untrusted\s*!==\s*null/);
  });
});
```

- [x] **Step 1: 写护栏测试并跑失败**(`npx vitest run src/lib/mixPlayback.test.ts`)
- [x] **Step 2: 实现页面改动 + i18n 键**
- [x] **Step 3: `npx vitest run` 全绿**(含 i18n 键集一致/无硬编码中文两道既有护栏)
- [x] **Step 4: `npm run check` 0 错误**
- [x] **Step 5: 提交** `feat(ui): 详情页回放 A/B 切换与成品轨生成——装载二选一,seek 带首帧偏移修正`

---

### Task 8: 设置页 `mix_track` 开关

**Files:**
- Modify: `src/routes/settings/+page.svelte`(录制 section,:843 `keep_audio` 行之后)
- Modify: `src/lib/i18n/dict/settings.ts`

**Interfaces:**
- Consumes: settings 模型已有 `mix_track: boolean`(models.ts:83),`saveSetting` helper(:390)。

**i18n 键:**

```ts
"settings.record.mixTrack.label": "录制期混出成品轨(方案 B)",
"settings.record.mixTrack.desc": "把两路声音在录制时混成第三条轨,回放可直放对比。仅影响新录制;每分钟约多占 1.9MB(转码后大幅缩小)。",
// en:
"settings.record.mixTrack.label": "Mix a combined track while recording (scheme B)",
"settings.record.mixTrack.desc": "Blend both audio sources into a third track during recording for direct playback comparison. New recordings only; ~1.9MB/min extra (much smaller after transcoding).",
```

**行样板**(照 `keep_audio` :843 逐行抄,绑定变量 `mixTrack`,onchange
`saveSetting((s) => (s.mix_track = mixTrack))`);`settings.rs:139` 注释里的
「实验字段,无 UI,手改 settings.json」一句同步删掉(名不副实即改)。

- [x] **Step 1: 实现**(此 Task 纯声明式 UI,无先行失败测试;护栏是既有 i18n 双测)
- [x] **Step 2: `npx vitest run` + `npm run check` 全绿**
- [x] **Step 3: 提交** `feat(ui): 设置页开放 mix_track 开关`

---

### Task 9: 端到端验收 + 全套自动化 + PR

**Files:**
- Modify: `src-tauri/src/pipeline/recording_sink.rs` tests(若 Task 3 未覆盖端到端断言)
- Test: 全仓

- [x] **Step 1: 补端到端断言**(recording_sink 既有 drain 流水线上,mix 开):
  正常停录后 `load_audio_meta`:mixed 有 `mix.origin == "live"`、`mix.track_ms` 与
  `session_track_ms` 一致、`seek_offset_ms` 键集 ⊆ {mic, system};两条源轨 `sync.first_frame_offset_ms`
  为 Some 且其中至少一个为 0(最早源)。

- [x] **Step 2: 补 regen→可信闭环单测**(lib.rs 或 input.rs):tempdir 笔记造双轨(wav)+
  sync → 直接调 worker 内层函数(把 worker 主体抽成 `fn regen_mixed_for_dir(dir) -> anyhow::Result<()>`
  便于免 Tauri 单测)→ 断言 `mixed_untrusted == None`、`mix.origin == "regen"`、
  `seek_offset_ms` 空、`offset_ms == min(源 offset)`。

- [x] **Step 3: 全套验证**

```bash
cd src-tauri && cargo test            # 全绿
cd .. && npx vitest run               # 全绿
npm run check                         # 0 错误
```

- [x] **Step 4: 提交 + PR**

PR 描述含**真机冒烟清单**(Chromium/无头假通过前科,必须真机):

1. 设置页开 `mix_track` → 录一场(外放)→ 停录:详情页出现「双轨/成品轨」切换;
   切到成品轨,听感无重影、音量不翻倍(播放器仍单轨装载)。
2. 成品轨态点段落跳转:system 段落落点准确(首帧偏移修正生效)。
3. 关 `mix_track` 的历史双轨笔记 → 「生成成品轨」→ 完成后可切换;时长与双轨一致
   (±0.5s);align.json 已存在的笔记复用映射(日志无二次 estimate)。
4. 生成过程中开始录制 → 被拒;录制中点生成 → 置灰/被拒(互斥双向)。
5. mic 轨带 clean 记录的场次切到成品轨 → 出现「不可直比」提示。
6. 中断残留:手工截断 mixed.wav(去尾 + 删 audio.json 的 mix 字段)→ 切换按钮置灰,
   tooltip 给出不可信原因。
7. Windows:录制期混音 + 切换回放(未转码 WAV 路径,MixInfo.track_ms 读数生效)。

---

## Self-Review 记录(成文时已跑)

- spec 覆盖:目标 1(Task 3/6/7)、目标 2(Task 4/5)、目标 3(Task 6/7/8;ASR 切换三期已交付)、
  目标 4(一期已交付,Task 1 补首帧偏移落盘、Task 9 端到端断言)。§口径差 三条后果 →
  Task 1/6/7(seek)、mixed_untrusted 既有(duration 交叉)、Task 1(落盘)。§错误处理
  「二期消费前必须自行校验或补标记」→ Task 2/3。§对照条件 选项 1 → Task 6/7 ab_caveat。
- 已知裂缝五条(勘察报告)全部有归属:①TS 类型 → Task 6;②未转码无读数 → Task 2;
  ③首帧偏移不落盘 → Task 1;④波形懒回填不含 mixed → Task 6;⑤accept_at 离线入口 → Task 4。
- 类型一致性:`MixInfo`/`set_track_mix`/`mixed_playback_info`/`regen_mixed_to` 的签名在
  Interfaces 与代码块间已核对一致。
