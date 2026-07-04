# 全局声纹库实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 跨会议说话人身份:库命中实时显名、停止时够料自动入库、管理页改名/合并/删除,历史笔记只读 join 库现名。

**Architecture:** S 号体系不动,person 为叠加层。registry 簇携带 `person/person_name/total_ms`;开录以库质心种子注入(复用 P4.5 快照铺底);名字经既有 SpeakersChanged→writer.speakers→前端 chips 通道零改动传播;停止时 Snapshot 闭包臂做库 upsert 并回填 person_id;`store/voiceprints.rs` 独立模块管全局库文件。

**Tech Stack:** Rust(serde 原子写/静态锁,与 notes.rs 同模式)+ SvelteKit 新路由。

**Spec:** `docs/superpowers/specs/2026-07-05-voice-notes-voiceprint-library-design.md`(数据模型/阈值/merge 规则/验收均以 spec 为准)

## Global Constraints

- 分支 `voiceprint-library`(已建,叠在 lang-filter-rms 上;push/PR 前先确认 PR #7 已合并并 rebase --onto master,由控制器处理,不在任务内)。
- 注释中文讲"为什么";cargo test 全过、npm check 0/0、双端 build,无新警告。
- 库文件任何失败不得阻塞/中断录制(降级打日志)。
- 常量 `SEED_ASSIGN_THRESHOLD = 0.68`、`AUTO_ENROLL_MS: u64 = 10_000`,注释标注"待真实会议数据校准"。
- serde 兼容:SpeakerMeta.person_id 与 voiceprints.json 均 default/skip_serializing_if,旧数据不破。
- TDD:每任务新逻辑先测后码。

---

### Task 1: registry 扩展(person/total_ms/种子注入/阈值分档/禁并规则)

**Files:**
- Modify: `src-tauri/src/diar/registry.rs`

**Interfaces:**
- Produces:
  - `Cluster`/`ClusterSnapshot` 增 `person: Option<String>`、`total_ms: u64`;`Cluster` 另增 `person_name: Option<String>`(种子携带库名,不进快照——名字真值在库/笔记表)。
  - `SpeakerInfo` 增 `person: Option<String>`、`name: Option<String>`。
  - `pub struct SeedCluster { pub person: String, pub name: String, pub centroid: Vec<f32>, pub count: u64 }`
  - `pub fn with_seeds(snaps: &[ClusterSnapshot], seeds: &[SeedCluster]) -> Self`:先铺快照(语义同 from_snapshot,person 随快照恢复),再对"快照中未出现的 person"逐种子建簇(新 S 号,person/person_name 带上,count 从库带,total_ms=0)。
  - `pub const SEED_ASSIGN_THRESHOLD: f32 = 0.68;`
  - assign:命中判定对 `person.is_some()` 簇用 SEED_ASSIGN_THRESHOLD,否则 ASSIGN_THRESHOLD;长段时 `total_ms += (num_samples / 16) as u64`(与质心更新同一分支)。
  - detect_merges:两簇 person 均 Some 且不同 → 跳过;否则 winner 继承 Some 方的 person/person_name。
  - `from_snapshot` 保留(等价 `with_seeds(snaps, &[])`,内部委托)。

- [ ] **Step 1: 写失败测试**(registry.rs tests;沿用既有 v(x,y,z) 正交基/LONG 模式)

```rust
    #[test]
    fn seeds_inject_match_and_dedup() {
        // 库里张三(P1)质心 e1;快照里已有 S2 关联 P9(续录场景)
        let snap = ClusterSnapshot {
            id: "S2".into(), centroid: v(0.0, 1.0, 0.0), count: 5,
            sources: BTreeSet::from(["mic".to_string()]),
            person: Some("P9".into()), total_ms: 0,
        };
        let seeds = vec![
            SeedCluster { person: "P1".into(), name: "张三".into(), centroid: v(1.0, 0.0, 0.0), count: 40 },
            SeedCluster { person: "P9".into(), name: "旧人".into(), centroid: v(0.0, 1.0, 0.0), count: 7 },
        ];
        let mut r = SpeakerRegistry::with_seeds(&[snap], &seeds);
        // P9 已在快照,种子去重:簇数 = 快照1 + P1 种子1
        assert_eq!(r.speakers().len(), 2);
        // 命中张三种子:0.7 余弦 > 0.68 → 返回其 S 号,speakers() 带 person/name
        let id = r.assign(&v(0.98, 0.199, 0.0), "mic", LONG).unwrap();
        let info = r.speakers().into_iter().find(|s| s.id == id).unwrap();
        assert_eq!(info.person.as_deref(), Some("P1"));
        assert_eq!(info.name.as_deref(), Some("张三"));
    }

    #[test]
    fn seed_threshold_is_stricter_than_session_threshold() {
        // 相似度 ~0.65:普通簇能命中(≥0.62),种子簇不能(<0.68)
        let seeds = vec![SeedCluster { person: "P1".into(), name: "甲".into(), centroid: v(1.0, 0.0, 0.0), count: 10 }];
        let mut seeded = SpeakerRegistry::with_seeds(&[], &seeds);
        let probe = v(0.65, (1.0f32 - 0.65 * 0.65).sqrt(), 0.0);
        let id = seeded.assign(&probe, "mic", LONG).unwrap();
        let info = seeded.speakers().into_iter().find(|s| s.id == id).unwrap();
        assert_eq!(info.person, None, "0.65 < 0.68,不得吸入种子簇,应新建普通簇");

        let mut plain = SpeakerRegistry::new();
        plain.assign(&v(1.0, 0.0, 0.0), "mic", LONG).unwrap();
        let id2 = plain.assign(&probe, "mic", LONG).unwrap();
        assert_eq!(plain.speakers().len(), 1, "0.65 ≥ 0.62,普通簇应命中: {id2}");
    }

    #[test]
    fn different_persons_never_automerge_and_winner_inherits_person() {
        let seeds = vec![
            SeedCluster { person: "P1".into(), name: "甲".into(), centroid: v(1.0, 0.0, 0.0), count: 10 },
            SeedCluster { person: "P2".into(), name: "乙".into(), centroid: v(0.9805, 0.19612, 0.0), count: 10 },
        ];
        // 两种子余弦 ~0.98 ≥ MERGE_THRESHOLD,但 person 不同 → 永不自动合并
        let mut r = SpeakerRegistry::with_seeds(&[], &seeds);
        assert!(r.take_merges().is_empty());
        assert_eq!(r.speakers().len(), 2);
        // 无 person 簇与有 person 簇可并,winner 继承 person
        let mut r2 = SpeakerRegistry::with_seeds(
            &[],
            &[SeedCluster { person: "P1".into(), name: "甲".into(), centroid: v(1.0, 0.0, 0.0), count: 1 }],
        );
        r2.assign(&v(0.9805, 0.19612, 0.0), "mic", LONG).unwrap(); // 新建普通簇(0.65<0.68 不吸入…注意构造相似度)
        // 构造:普通簇与种子簇质心余弦 ≥ MERGE 后触发合并
        // (具体向量按实现时校准,断言语义:合并发生且幸存簇 person == Some("P1"))
    }

    #[test]
    fn total_ms_accumulates_only_on_long_segments() {
        let mut r = SpeakerRegistry::new();
        r.assign(&v(1.0, 0.0, 0.0), "mic", 32000).unwrap(); // 2s
        r.assign(&v(1.0, 0.0, 0.0), "mic", 4800).unwrap();  // 0.3s 短段不计
        let snap = r.snapshot();
        assert_eq!(snap[0].total_ms, 2000);
    }
```

(第三个测试的合并向量构造,以及全部既有测试的 ClusterSnapshot 字面量补 `person: None, total_ms: 0`,实现时按实际调整;断言语义不变。)

- [ ] **Step 2: 编译确认失败** — `cd src-tauri && cargo test diar::` → E0063/E0425。

- [ ] **Step 3: 实现**(按 Interfaces 清单;关键片段)

assign 命中分档与 total_ms(替换原 `if sim >= ASSIGN_THRESHOLD` 块):

```rust
        if let Some((sim, cluster)) = best {
            // 种子簇(关联库 person)用更高阈值:跨会议信道差异大,误命名比不命名糟。
            // 待真实会议数据校准。
            let threshold = if cluster.person.is_some() { SEED_ASSIGN_THRESHOLD } else { ASSIGN_THRESHOLD };
            if sim >= threshold {
                cluster.sources.insert(source.to_string());
                if num_samples >= MIN_NEW_CLUSTER_SAMPLES {
                    let n = cluster.count as f32;
                    for (ci, ui) in cluster.centroid.iter_mut().zip(&unit) {
                        *ci = (*ci * n + ui) / (n + 1.0);
                    }
                    if let Some(renorm) = normalize(&cluster.centroid) {
                        cluster.centroid = renorm;
                    }
                    cluster.count += 1;
                    cluster.total_ms += (num_samples / 16) as u64;
                }
                return Some(cluster.id.clone());
            }
        }
```

detect_merges 禁并(内层循环判定处):

```rust
                    let (a, b) = (&self.clusters[i], &self.clusters[j]);
                    // 不同 person 的簇禁止自动互并:"库里两人实为一人"只能由用户在管理页显式合并。
                    let conflict = matches!((&a.person, &b.person), (Some(x), Some(y)) if x != y);
                    if !conflict && dot(&a.centroid, &b.centroid) >= MERGE_THRESHOLD {
```

合并处 winner 继承:`if winner.person.is_none() { winner.person = loser.person.clone(); winner.person_name = loser.person_name.clone(); }`(loser 在 remove 后仍持有字段)。

with_seeds:

```rust
    /// 库种子注入:先铺会话快照(续录),再为快照中未出现的 person 建种子簇。
    /// 快照优先:续录质心更贴近本场信道。count 从库带(权重大、漂移慢),
    /// total_ms 归 0(只统计本场,供停止时的入库门槛与库累计)。
    pub fn with_seeds(snaps: &[ClusterSnapshot], seeds: &[SeedCluster]) -> Self {
        let mut r = Self::from_snapshot_inner(snaps);
        let known: BTreeSet<&String> = r.clusters.iter().filter_map(|c| c.person.as_ref()).collect();
        for s in seeds {
            if known.contains(&s.person) || normalize(&s.centroid).is_none() {
                continue;
            }
            let id = format!("S{}", r.next_id);
            r.next_id += 1;
            r.clusters.push(Cluster {
                id,
                centroid: normalize(&s.centroid).unwrap(),
                count: s.count.max(1),
                sources: BTreeSet::new(),
                person: Some(s.person.clone()),
                person_name: Some(s.name.clone()),
                total_ms: 0,
            });
        }
        r
    }
```

(`from_snapshot` 改为薄委托,内部逻辑挪进 `from_snapshot_inner`,快照恢复 person、person_name=None——名字由 lib.rs 从 writer 表/库补,种子路径才带名。)
`speakers()`/`snapshot()` 同步带出新字段。

- [ ] **Step 4: 跑测试** — `cargo test diar::` 全 PASS;`cargo build` 无新警告(session.rs/lib.rs 若因 SpeakerInfo 字段编译失败,本任务顺带补 `..` 或字段,行为不变)。

- [ ] **Step 5: Commit** — `feat(diar): registry 支持库种子注入(person 叠加层/阈值分档/禁并/total_ms)`

---

### Task 2: store/voiceprints.rs 全局库模块

**Files:**
- Create: `src-tauri/src/store/voiceprints.rs`
- Modify: `src-tauri/src/store/mod.rs`(挂模块 + re-export)

**Interfaces:**
- Produces(spec 数据模型为准):

```rust
pub struct PersonCentroid { pub vec: Vec<f32>, pub count: u64 }
pub struct Person { pub name: String, pub centroids: BTreeMap<String, PersonCentroid>, pub total_ms: u64, pub last_seen: String }
pub struct Voiceprints { pub schema_version: u32, pub next_person: u32, pub people: BTreeMap<String, Person>, pub redirects: BTreeMap<String, String> }
pub struct VoiceprintStore { root: PathBuf } // root = app_data_dir
impl VoiceprintStore {
    pub fn new(root: PathBuf) -> Self;
    pub fn load(&self) -> Voiceprints;                       // 缺失/损坏 → 空库 + eprintln
    pub fn resolve<'a>(vp: &'a Voiceprints, id: &str) -> Option<&'a str>; // 跟随 redirects,防环(步数上限)
    pub fn rename(&self, id: &str, name: &str) -> anyhow::Result<()>;
    pub fn merge(&self, loser: &str, winner: &str) -> anyhow::Result<()>;
    pub fn delete(&self, id: &str) -> anyhow::Result<()>;
    /// 停止时:person=Some 簇加权回写;person=None 且 total_ms≥AUTO_ENROLL_MS 且质心非空 → 新建未命名 person。
    /// 返回 (会话簇 id → person id) 的新建关联,供回填本场 speakers 表。
    pub fn upsert_from_session(&self, snaps: &[ClusterSnapshot], now: &str) -> anyhow::Result<BTreeMap<String, String>>;
}
pub const AUTO_ENROLL_MS: u64 = 10_000; // 待真实会议数据校准
```

- 全部写操作:模块级 `static VP_LOCK: Mutex<()>`(毒化 into_inner,与 notes.rs EDIT_LOCK 同模式)+ 读改写 + `voiceprints.json.tmp` rename 原子落盘。
- upsert 的质心回写:per (person, source) `PersonCentroid`——会话簇 sources 可能多值,按簇 sources 里的每个 source 写同一质心?**不**:一簇一质心,写入该簇 sources 的主 source(取 BTreeSet 首个;简化,spec 允许);已有则 count 加权平均后归一。
- merge:loser centroids 逐 source 并入 winner(同 source 加权平均,不同 source 直插),total_ms 相加,name 取 winner(winner 无名而 loser 有名则继承 loser 名),redirects 插入 loser→winner 且把既有指向 loser 的项改指 winner(链条压扁,防长链)。
- delete:移除 person;指向它的 redirects 一并删除(悬空引用由 resolve 返回 None 容忍)。

- [ ] **Step 1: 写失败测试**(同文件 tests;tempfile root)

覆盖:load 缺失→空库;save/load 往返;损坏文件→空库不 panic;resolve 链式(P3→P2→P1)与自环防死循环;rename 持久化;merge(质心加权/异 source 直插/redirects 压扁/名字继承);delete(清 redirects);upsert 三分支(回写加权、够料入库返回映射、不够料忽略)+ 空质心不入库。每个行为一测,断言落盘后重 load 的状态。

- [ ] **Step 2: 编译确认失败** — `cargo test voiceprints` → 模块不存在。

- [ ] **Step 3: 实现**(serde 结构全部 `#[serde(default)]` 容忍旧/缺字段;`skip_serializing_if` 用于空 redirects/centroids 可省)

- [ ] **Step 4: 跑测试** — `cargo test store::` 全 PASS。

- [ ] **Step 5: Commit** — `feat(store): 全局声纹库模块(原子写/redirects/加权回写/够料入库)`

---

### Task 3: 笔记侧 person_id 关联 + 只读 join

**Files:**
- Modify: `src-tauri/src/store/mod.rs`(SpeakerMeta 增 person_id)
- Modify: `src-tauri/src/store/writer.rs`(store_centroids 存 person;registry_snapshot 恢复 person;新增 set_speaker_person;SpeakerMeta 构造点补默认)
- Modify: `src-tauri/src/store/notes.rs`(load 末尾 join 库名)
- Modify: `src/lib/notes.ts`(SpeakerMeta 类型加 person_id?)

**Interfaces:**
- `SpeakerMeta.person_id: Option<String>`(`#[serde(default, skip_serializing_if = "Option::is_none")]`)。
- `NoteWriter::set_speaker_person(&mut self, id: &str, person: &str)`(仅内存表,落盘走既有 persist/finalize)。
- `store_centroids`:snap.person → meta.person_id。`registry_snapshot`:meta.person_id → snap.person(total_ms 恒 0)。
- `NoteStore::load` 在返回前:对 name 为空且 person_id 可解析的 speaker,用库中该人现名填充(只读,不落盘);`VoiceprintStore` 从 `self.notes_dir.parent()`(= app_data_dir)构造,parent 不存在或库空 → 跳过。

- [ ] **Step 1: 失败测试**:writer 往返(store_centroids 带 person → finalize → resume registry_snapshot 恢复 person);notes join(造库文件含 P1"张三" + 笔记 speakers.json S1 name 空 person_id=P1 → load 后 name=="张三";本地有名不覆盖;库缺失不 panic)。
- [ ] **Step 2: 编译/断言失败确认。**
- [ ] **Step 3: 实现。**(SpeakerMeta 既有字面量构造点全仓 grep 补 `person_id: None`。)
- [ ] **Step 4: `cargo test store::` + `npm run check`。**
- [ ] **Step 5: Commit** — `feat(store): speakers.json 关联 person_id,详情页只读 join 库现名`

---

### Task 4: 会话与命令接线(lib.rs)

**Files:**
- Modify: `src-tauri/src/lib.rs`

**Interfaces:**
- Consumes: T1 `with_seeds`/`SeedCluster`/`SpeakerInfo{person,name}`;T2 `VoiceprintStore`;T3 `set_speaker_person`。
- Produces: commands `list_people`/`rename_person`/`merge_person`/`delete_person`(注册进 invoke_handler);`ipc::PersonSummary { id, name, total_ms, last_seen, sources: Vec<String> }`。

**接线点(实现按此逐一改):**

1. **种子注入**(lib.rs:283 替换):

```rust
        // 说话人编号/质心延续 + 库种子注入:快照(续录)优先,库中同 person 不重复注入。
        // 库加载失败降级为无种子,绝不挡录制。
        let seeds = load_voiceprint_seeds(&app);
        let registry = crate::diar::registry::SpeakerRegistry::with_seeds(
            &writer.lock().unwrap().registry_snapshot(),
            &seeds,
        );
```

`load_voiceprint_seeds(app) -> Vec<SeedCluster>`:app_data_dir → VoiceprintStore::load → 每 person 每 centroid 一个 SeedCluster(name 取 person.name;经 resolve 的有效 person 才导出)。

2. **SpeakersChanged 闭包臂**:sync_speakers 后,对 `infos` 中 `person.is_some()` 的项:`w.set_speaker_person(&s.id, person)`;且本地名为空而 `s.name` 有值时 `w.set_speaker_name(&s.id, name)`。SpeakerEntry 照旧从 w.speakers() 读(名字已进表,前端零改动)。

3. **Snapshot 闭包臂**(store_centroids 之后):

```rust
                session::DiarEvent::Snapshot(snaps) => {
                    let mut w = writer_d.lock().unwrap();
                    w.store_centroids(&snaps);
                    // 库回写/够料入库(spec:person 簇加权回写;无主簇 ≥10s 入库为未命名人)。
                    // 失败只降级打日志:库是增值层,绝不影响笔记落盘。
                    match vp_store_d.upsert_from_session(&snaps, &chrono::Local::now().to_rfc3339()) {
                        Ok(enrolled) => {
                            for (cluster_id, person_id) in &enrolled {
                                w.set_speaker_person(cluster_id, person_id);
                            }
                        }
                        Err(e) => eprintln!("声纹库回写失败(不影响笔记): {e}"),
                    }
                }
```

(`vp_store_d` 为闭包前 clone 的 `VoiceprintStore`;Snapshot 在 worker join 前送达,故先于 stop_recording 的 finalize,person_id 随 finalize 落盘。)

4. **四个 command**(货真价实的薄封装;merge/delete 在 `state.session` 活跃时拒绝——种子已注入,语义混乱;rename 允许):

```rust
#[tauri::command]
fn list_people(app: AppHandle) -> Result<Vec<ipc::PersonSummary>, String> { .. }
#[tauri::command]
fn rename_person(app: AppHandle, id: String, name: String) -> Result<(), String> { .. }
#[tauri::command]
fn merge_person(app: AppHandle, state: State<AppState>, loser: String, winner: String) -> Result<(), String> { .. }
#[tauri::command]
fn delete_person(app: AppHandle, state: State<AppState>, id: String) -> Result<(), String> { .. }
```

- [ ] **Step 1: 失败测试**:lib.rs 无直测惯例(State 不可造),以 `cargo build` 编译驱动;PersonSummary serde 形状加一个 ipc 单测(如 ipc.rs 有测试惯例则随之,无则跳过并在报告说明)。
- [ ] **Step 2-3: 实现接线 + commands + invoke_handler 注册。**
- [ ] **Step 4: `cargo test` 全量 + build 无新警告。**
- [ ] **Step 5: Commit** — `feat(app): 开录种子注入/命中显名/停止入库回填 + 声纹库四命令`

---

### Task 5: 前端管理页 /speakers + 侧栏入口

**Files:**
- Create: `src/routes/speakers/+page.svelte`
- Modify: 侧栏组件(含「实时转写」入口的布局文件,grep `href="/record"` 定位)加「说话人」入口
- Modify: `src/lib/notes.ts` 或新 `src/lib/people.ts`:PersonSummary 类型 + 四个 invoke 封装

**行为(spec 管理页节为准):**
- 列表:名字(空名显示 `未命名 · 最近 {formatDate(last_seen)}`)、累计发声(`formatDuration(total_ms/1000)` 复用)、信道标记;
- 改名:名字处常驻编辑态(contenteditable,与详情页段落同模式:失焦提交、Esc 还原、空文本还原);
- 合并:行内「合并到…」→ 下拉列出其它人 → 确认调 merge_person → 刷新;录制中后端会拒绝,错误文案原样展示;
- 删除:二次确认(同详情页删除段模式);
- 空库提示文案(spec 原文);加载失败显示错误 banner。
- 样式沿用 notes 列表页/详情页现有 token(背景/圆角/按钮),不引新依赖。

- [ ] **Step 1: 实现页面 + 入口 + invoke 封装。**
- [ ] **Step 2: `npm run check && npm run build` 0/0 + OK。**
- [ ] **Step 3: Commit** — `feat(ui): 说话人管理页(列表/改名/合并/删除)+ 侧栏入口`

---

### Task 6: 全量验证 + 记账

- [ ] **Step 1:** `cd src-tauri && cargo test 2>&1 | grep "test result" | head -1 && cargo build 2>&1 | tail -2 && cd .. && npm run check 2>&1 | tail -1 && npm run build 2>&1 | tail -2` — 全绿无新警告。
- [ ] **Step 2:** progress.md 加节:

```markdown
## 全局声纹库(分支 voiceprint-library,叠 lang-filter-rms)
- registry 种子注入(person 叠加层/0.68 分档/禁并) + voiceprints.json 库(原子/redirects/加权回写/10s 门槛入库) + speakers.json person_id 关联与只读 join + 四命令 + /speakers 管理页。
- spec: docs/superpowers/specs/2026-07-05-voice-notes-voiceprint-library-design.md
```

- [ ] **Step 3: Commit** — `docs(sdd): 记录声纹库执行`

(push、rebase --onto master、PR、终审由控制器收尾处理。)
