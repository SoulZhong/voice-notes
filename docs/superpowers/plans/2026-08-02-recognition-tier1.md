# 识别第一梯队实施计划(AS-Norm/信道感知/短段分层/ERes2NetV2 收尾)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 按 `docs/2026-08-02-recognition-tier1-design.md` 落地四项识别改造。

**Architecture:** 改动集中在 diar/registry.rs 的种子判定路径(信道分路+z 通道+短段门槛)与 voiceprints.rs 的 seed_clusters(信道贯通+种子侧 cohort 预计算入口);第 4 项是 models/settings 层的边界显性化。识别热路径,全部 cargo TDD。

**Tech Stack:** Rust(cargo test)/ Svelte(仅第 4 项文案)。

## Global Constraints

- 工作目录 worktree:/Users/teemo/workspace-soul/voice-notes/.claude/worktrees/speaker-tidy。
- 新常量注释一律标「待评测集校准的初值」;改 `MIN_CENTROID_UPDATE_SAMPLES` 时**保留 0.6s 首轮校准的历史注释**并追加新依据。
- 既有测试不许破坏(cargo --lib 950、vitest 176、check 0);registry 是 ASR worker 单线程热路径,不加锁不加堆分配大件。
- 快路语义不变:同信道 raw≥0.68 命中行为与现状逐位一致。
- 提交 `feat(diar):`/`feat(models):` 前缀 + `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` 落款。

---

### Task 1: 信道与 cohort 管道贯通(不改判定)

**Files:**
- Modify: `src-tauri/src/diar/registry.rs`(SeedCluster/Cluster/with_seeds)
- Modify: `src-tauri/src/store/voiceprints.rs`(seed_clusters ~L738)
- Test: 两文件 tests 模块

**Interfaces:**
- Produces:`SeedCluster { person, name, centroid, count, source: String }`;`Cluster` 新增 `seed_source: Option<String>` 与 `seed_cohort: Option<(f32, f32)>`(μb/σb);`with_seeds` 注入时按"对其他人物种子的每人最高分"预计算 seed_cohort(其他人物数 <3 时为 None)。Task 2 消费。

- [ ] **Step 1: 失败测试**

voiceprints.rs tests(参照既有 `seed_clusters_include_session_variants_and_skip_dangling` 搭建):

```rust
#[test]
fn seed_clusters_carry_channel_source() {
    // P1 有 mic 主质心 + system 会话变体 → 两个种子各带各的信道
    let seeds = seed_clusters(&vp);
    assert!(seeds.iter().any(|s| s.person == "P1" && s.source == "mic"));
    assert!(seeds.iter().any(|s| s.person == "P1" && s.source == "system"));
}
```

registry.rs tests:

```rust
#[test]
fn with_seeds_precomputes_cohort_stats_per_seed() {
    // 4 个人物的种子(cohort=3 达门槛)→ 每个种子簇 seed_cohort 是 Some 且 σ ≥ 1e-3;
    // 只有 2 个人物时 → 全部 None(小 cohort 关断)。
}
```

- [ ] **Step 2: 跑红** `cd src-tauri && cargo test --lib seed_` — source 字段不存在编译错。

- [ ] **Step 3: 实现**

seed_clusters:主质心循环改 `for (src, c) in &person.centroids`、变体循环改 `for (src, list) in &person.session_centroids { for c in list {...} }`,`source: src.clone()` 进 SeedCluster。
registry:结构体加字段;with_seeds 注入后做两两预计算——对每个种子 i:按 person 归组算「其他每个人物对 i.centroid 的最高 cos」集合,len<SNORM_MIN_COHORT(新常量 3,注释注明与 suggest 同值同理)→ None,否则 (mean, std.max(1e-3))。快照恢复簇 seed_source/seed_cohort 均 None(跨场恢复无信道保证,走保守路径,注释写明)。

- [ ] **Step 4: 跑绿+回归** `cargo test --lib 2>&1 | tail -2`(950+新增)。

- [ ] **Step 5: 提交** `feat(diar): 种子簇贯通信道来源与 cohort 统计(判定不变)`

---

### Task 2: 判定改造——短段门槛 + 信道分路 + AS-Norm z 通道

**Files:**
- Modify: `src-tauri/src/diar/registry.rs`(常量区 + assign 判定路径)
- Test: 同文件 tests

**Interfaces:**
- Consumes: Task 1 的 seed_source/seed_cohort。
- Produces: 常量 `SEED_MIN_SAMPLES=32_000`、`SEED_ASSIGN_Z=3.0`、`SEED_ASSIGN_RAW_FLOOR=0.50`、`SNORM_MIN_COHORT=3`(若 Task 1 未建);`MIN_CENTROID_UPDATE_SAMPLES=24_000`。

- [ ] **Step 1: 失败测试**(合成向量;构造方式参照既有 registry 测试)

```rust
#[test] fn short_segment_never_claims_seed() { /* 1.9s 段对种子 raw 0.9 → 不命中种子(可归普通簇/软归属);2.1s 同分 → 命中 */ }
#[test] fn cross_channel_raw_hit_no_longer_accepted() { /* mic 段 vs system 种子 raw 0.70、z 不达标 → 不命中(旧行为命中,收紧点) */ }
#[test] fn cross_channel_high_z_accepted() { /* mic 段 vs system 种子 raw 0.55,cohort 构造成其他人物都 ~0.1 → z≥3 → 命中 */ }
#[test] fn same_channel_fast_path_unchanged() { /* 同信道 raw 0.69 命中;0.67 且 z 不足 → 不命中(与现状一致) */ }
#[test] fn z_path_disabled_below_min_cohort() { /* 2 个人物种子,raw 0.55 z 再高也不命中 */ }
#[test] fn centroid_update_gate_raised() { /* 1.4s 段命中普通簇后质心不变;1.6s 段更新 */ }
```

- [ ] **Step 2: 跑红**(新常量/新分支不存在)。

- [ ] **Step 3: 实现**

常量区:

```rust
/// 段短于此(16kHz 样本数,2s)不参与种子命中:<2s 嵌入可靠性跳崖(文献:2s 条件
/// EER 可翻 3 倍),短段无权拍板"这是谁";仍可归场内簇/软归属。待评测集校准的初值。
pub const SEED_MIN_SAMPLES: usize = 32_000;
/// AS-Norm 增益通道:对称 z(与整理层 suggest 同式)达标且裸分不低于地板即命中种子。
/// 3.0 与自动归并 SUGGEST_STRONG_Z 同档;0.50 地板防"纯统计巧合"。待评测集校准的初值。
pub const SEED_ASSIGN_Z: f32 = 3.0;
pub const SEED_ASSIGN_RAW_FLOOR: f32 = 0.50;
```

`MIN_CENTROID_UPDATE_SAMPLES` 改 24_000,注释保留原 0.6s 历史、追加「二〇二六-〇八根因分析:短段系统性偏移非白噪声,running-mean 稀释不足以抵御,提到 1.5s;0.6~1.5s 段仍正常归簇打标签,只是无权改质心」。

assign 种子判定处(现 `is_seed()` 走 SEED_ASSIGN_THRESHOLD 的分支)改为:

```rust
// 种子命中三闸:①段长 ≥ SEED_MIN_SAMPLES;②同信道走裸分快路(阈值不变);
// ③跨信道只走 AS-Norm z 通道——裸余弦跨信道不可比,mic 段撞 system 质心
// 分数系统性走低/走高都不可信,归一化后才有资格认领。
let same_channel = cluster.seed_source.as_deref() == Some(source);
let seed_eligible = num_samples >= SEED_MIN_SAMPLES;
let fast_hit = same_channel && sim >= SEED_ASSIGN_THRESHOLD;
let z_hit = matches!(seed_z(...), Some(z) if z >= SEED_ASSIGN_Z) && sim >= SEED_ASSIGN_RAW_FLOOR;
if seed_eligible && (fast_hit || z_hit) { /* 命中 */ }
```

`seed_z`:测试侧 μa/σa 用扫描期收集的「每个其他人物种子的最高分」(排除候选人物全部种子;len<SNORM_MIN_COHORT → None;σ 下限 1e-3),种子侧 (μb,σb)=cluster.seed_cohort(None → 整体 None);`z=((s−μa)/σa+(s−μb)/σb)/2`。实现时按 assign 现有扫描结构落位:先一遍扫完收集各种子分数与最优普通簇,再做种子判定——避免双重循环。快照恢复簇 seed_source=None:按「跨信道」对待(只 z 通道,而其 seed_cohort=None → 实际只剩普通簇路径?不——续录恢复簇 is_seed() 且无信道:给它保留旧行为(裸 0.68,不分信道),注释写明「续录恢复簇信道未知,维持原语义,待后续快照带信道再收紧」。这条在测试里锁住:恢复簇 raw 0.70 仍命中。)

- [ ] **Step 4: 跑绿+全量** `cargo test --lib 2>&1 | tail -2` 全绿;既有种子测试(SEED_ASSIGN_THRESHOLD 相关)如因信道字段缺省而红,按"测试搭建补 source"修测试而非改语义。

- [ ] **Step 5: 提交** `feat(diar): 种子命中三闸——短段禁入/同信道快路/跨信道走 AS-Norm z 通道`

---

### Task 3: ERes2NetV2 切换边界显性化

**Files:**
- Modify: `src-tauri/src/store/voiceprints.rs`(rebuild_for_model 返回值或新查询)
- Modify: `src-tauri/src/lib.rs`(设置切换命令处 ~L3934 或新命令)
- Modify: `src/routes/settings/+page.svelte`(~L472 声纹模型选型区)

**Interfaces:**
- Produces: 前端可取「无样本人物数」;切换确认提示;文案更新。

- [ ] **Step 1: 先读现状**:`rebuild_for_model`(voiceprints.rs:577-631)对无样本人物做什么(跳过?清质心?)、设置页切换交互现状(有无确认步)。**以读到的为准**调整下述实现,行为红线:无样本人物的记录/名字/笔记关联绝不能被删——只是换模型后认不出。
- [ ] **Step 2: 失败测试**(cargo):`rebuild_for_model` 场景——2 人有样本 1 人无样本 → 返回值(或新查询 `count_people_without_samples`)能区分出 1;无样本者 people 记录仍在。
- [ ] **Step 3: 实现**:后端透出无样本计数(优先复用 rebuild 返回,或加只读查询命令 `count_people_without_samples() -> usize`,注册 invoke_handler);设置页切换确认文案:「切换后将用录音样本为每人重算声纹;库内 N 人无样本,重建前无法自动认出(名字与历史笔记不受影响)。」;选型行说明补:「ERes2NetV2:中文基准更准(CN-Celeb EER 6.14% vs 6.78%),模型更大速度稍慢。」
- [ ] **Step 4: 验证**:`cargo test --lib` 全绿、`npm run check` 0;设置页仿真截图(browse+mock:mock 需给新命令返回个数字)。
- [ ] **Step 5: 提交** `feat(models): 声纹模型切换显性化无样本人物边界+选型依据文案`

---

### Task 4: 文档同步 + 全量回归

**Files:**
- Modify: `docs/speaker-identification-architecture.md`(阈值速查表:MIN_CENTROID 1.5s、新增 SEED_MIN_SAMPLES/SEED_ASSIGN_Z/RAW_FLOOR;§④ 补信道分路与 z 通道一句)
- Modify: `docs/2026-08-02-speaker-recognition-accuracy-analysis.md`(第一梯队四项标「已实施 2026-08-02」,注明常量为待评测初值)

- [ ] **Step 1: 全量验证**:`cd src-tauri && cargo test 2>&1 | tail -3`、`npx vitest run 2>&1 | tail -3`、`npm run check 2>&1 | tail -2` 全绿。
- [ ] **Step 2: 文档两处更新**(表格与标注,不重写)。
- [ ] **Step 3: 提交** `docs: 识别第一梯队落地同步(阈值表+实施标注)`

## Self-Review

规格覆盖:设计 §一→T1/T2;§二→T1/T2;§三→T2;§四→T3;测试与文档→各任务+T4。类型一致:seed_cohort `Option<(f32,f32)>`、SeedCluster.source `String` 两任务同名。快照恢复簇的旧语义保留点已在 T2 显式锁测试。无占位(T3 Step1 的"先读现状"是任务内指令,红线已给)。
