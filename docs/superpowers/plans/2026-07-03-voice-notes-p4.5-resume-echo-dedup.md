# P4.5 会议续录 + 跨路回声去重 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 任何非活动笔记可续录(seq/时间轴/说话人编号全部连续);他人电脑外放导致的跨路同句重复被自动去除。

**Architecture:** ①质心快照:registry `snapshot/from_snapshot` ↔ `DiarEvent::Snapshot` ↔ speakers.json(`SpeakerMeta.centroid/count`);`NoteWriter::resume` + `base_ms` 偏移 + `NoteTarget` 会话启动重构 + `resume_recording` command;前端 store `resume()` 灌历史段。②worker 内 mic 段 hold-and-release 去重(`pending_mic`/`recent_system`,文本归一+Levenshtein)。

**Tech Stack:** 无新依赖。

**Spec:** `docs/superpowers/specs/2026-07-03-voice-notes-p4.5-resume-echo-dedup-design.md`

## Global Constraints

- **不丢内容**:去重只丢「判定为回声的 mic 段」;会话结束 pending_mic 全部 release 后才发 Snapshot、才 break;续录失败路径沿用 abort_or_finalize。
- 时间戳偏移:`on_final` 落盘/emit 前 `start_ms/end_ms + base_ms`(New 路径 base_ms=0,逻辑统一)。
- `SpeakerMeta` 新字段 serde 向后兼容:`#[serde(default, skip_serializing_if = "Option::is_none")] centroid: Option<Vec<f32>>`、`#[serde(default)] count: u64`——旧 speakers.json 照常解析。
- `from_snapshot` 编号续接:next_id = 表中最大 `S{n}` 的 n + 1(含无质心项);质心缺失项不建簇。
- 去重常量:`ECHO_HOLD_MS: u64 = 2500`、`ECHO_WINDOW_MS: u64 = 2500`、`ECHO_SIM_THRESHOLD: f32 = 0.6`;文本归一 = 去空白/标点 + ASCII 小写;相似度 = max(1 − Levenshtein/较长串字符数, 完全包含 ? 1.0 : 0);仅跨源(mic↔system)比对。
- system 段零延迟;mic 段 hold ≤ 2.5s(partial 不受影响);`recent_system` 按 end_ms 裁剪保留 10s。
- 事件契约:新增 `DiarEvent::Snapshot`(仅 worker→lib,不进 ipc);无新前端事件。
- 测试命令:`cargo test --manifest-path src-tauri/Cargo.toml`;前端 `npm run check` 0 errors + `npm run build`。
- 每 Task 一个 commit,末尾 `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`。

---

### Task 1: 质心快照(registry ↔ DiarEvent ↔ speakers.json)

**Files:** Modify `src-tauri/src/diar/registry.rs`、`src-tauri/src/session.rs`、`src-tauri/src/store/mod.rs`、`src-tauri/src/store/writer.rs`、`src-tauri/src/lib.rs`(on_diar Snapshot 分支)

**Interfaces(Produces):**
- `registry::ClusterSnapshot { pub id: String, pub centroid: Vec<f32>, pub count: u64, pub sources: BTreeSet<String> }`(Clone/Debug/PartialEq)
- `SpeakerRegistry::snapshot(&self) -> Vec<ClusterSnapshot>`;`SpeakerRegistry::from_snapshot(snaps: &[ClusterSnapshot]) -> Self`(next_id 续接规则见 Global Constraints;空切片 ≡ new())
- `DiarEvent::Snapshot(Vec<ClusterSnapshot>)`——worker 在 finals 通道 Disconnected 后、break 前发一次
- `SpeakerMeta` 新字段(serde 兼容,见 Global Constraints);`NoteWriter::store_centroids(&mut self, snaps: &[ClusterSnapshot])`(内存表 merge,不落盘——finalize/persist_speakers 落);`NoteWriter::registry_snapshot(&self) -> Vec<ClusterSnapshot>`(从表中有质心的项构造;sources 从 SpeakerMeta.sources)
- lib.rs on_diar 增 `DiarEvent::Snapshot(snaps) => writer 锁内 store_centroids`(不 emit)

**Steps(TDD):**
- [ ] registry 测试:snapshot→from_snapshot roundtrip(簇/质心/count/sources 一致;继续 assign 相同向量归入原簇);from_snapshot 编号续接(表含 S3 → 新簇为 S4);空快照 ≡ new。
- [ ] writer 测试:store_centroids + persist_speakers → 重读 speakers.json 质心在;旧格式(无 centroid 字段)speakers.json 可解析。
- [ ] session 测试:worker 结束时收到恰一次 Snapshot 事件(在既有 diar 事件之后)。
- [ ] 实现全部 → 全量测试绿 → commit:`P4.5 Task 1: 说话人质心快照(registry↔DiarEvent↔speakers.json)`

---

### Task 2: 续录(NoteWriter::resume + base_ms + NoteTarget + command)

**Files:** Modify `src-tauri/src/store/writer.rs`、`src-tauri/src/session.rs`(start_session/run_asr_worker 增 registry 参数)、`src-tauri/src/lib.rs`

**Interfaces(Produces):**
- `NoteWriter::resume(notes_dir: &Path, id: &str) -> anyhow::Result<NoteWriter>`(spec §1 语义;meta 损坏 → Err;id 校验防路径穿越——复用/复制 NoteStore 的规则)
- `NoteWriter::base_ms(&self) -> u64`(create 路径恒 0)
- `run_asr_worker(..., registry: SpeakerRegistry, ...)`/`start_session(..., registry: SpeakerRegistry, ...)`(替换 worker 内部 `SpeakerRegistry::new()`;既有调用点传 `SpeakerRegistry::new()`)
- lib.rs:提取 `fn spawn_session(app: AppHandle, running/generation/session_slot/recognizer_cache/embedder_cache: Arc<...>, target: NoteTarget)`(start_recording 与 resume_recording 的加载线程共用;`enum NoteTarget { New, Resume(String) }`);writer 分支 create/resume;registry = `SpeakerRegistry::from_snapshot(&writer.registry_snapshot())`;on_final 闭包捕获 `base_ms` 并偏移
- `#[tauri::command] fn resume_recording(app, state, note_id: String) -> Result<(), String>`:running/generation 守卫与 start_recording 完全一致(注意锁序注释同步),然后 spawn_session(Resume);注册 generate_handler

**Steps(TDD):**
- [ ] writer 测试:create+append+finalize → resume → meta 回 recording/ended_at None;next_seq 续接(含截断尾行容忍);base_ms = 最大 end_ms;speakers 表加载;resume 后 append 的 SegmentRecord seq 正确;不存在的 id / 损坏 meta → Err。
- [ ] 集成测试(writer tests 内,仿 `full_session_persists_every_final`):第一场会话落 N 段 → finalize → resume + 新会话再落 M 段(on_final 中 + base_ms)→ load:N+M 段、seq 单调、后 M 段 start_ms ≥ base_ms。
- [ ] lib.rs 重构 spawn_session(行为不变——start_recording 走 New 路径,全量测试与既有竞态注释保持)+ resume_recording。
- [ ] 全量绿 + `cargo build` 无新 warning → commit:`P4.5 Task 2: 会议续录(resume/base_ms/NoteTarget/resume_recording)`

---

### Task 3: 前端续录入口

**Files:** Modify `src/lib/notes.ts`、`src/lib/recording.svelte.ts`、`src/routes/notes/[id]/+page.svelte`

**Interfaces(Produces):**
- `notes.ts`:`resumeRecording(noteId: string)` invoke 封装
- store:`resume(noteId: string): Promise<boolean>`——pending 守卫;`getNote` 灌 `finals`(map SegmentRecord→Line,含 speaker)与 `speakers`;置一次性 `resuming` 标志;invoke;失败清标志并报 error。`onStatus` 的 `"recording"` 分支:`resuming` 时**不清** finals/speakers(仍清 partial/storageDegraded、bump statusVersion),用后复位
- 详情页:头部按钮「继续录制」(`recording.isRecording` 时 disabled;点击 `await recording.resume(id)` 成功 `goto('/record')`);中断横幅文案加「可点击上方『继续录制』接着记」

**Steps:**
- [ ] 实现 → `npm run check` 0 errors + `npm run build` → commit:`P4.5 Task 3: 详情页续录入口 + store 灌注历史段`

---

### Task 4: 跨路回声去重(worker hold-and-release)

**Files:** Modify `src-tauri/src/session.rs`

**Interfaces(Produces):**
- 常量(session.rs 顶部,注释校准来源):`ECHO_HOLD_MS/ECHO_WINDOW_MS/ECHO_SIM_THRESHOLD`(值见 Global Constraints)
- worker 内部(不改公共签名):
  - `fn normalize_text(s: &str) -> String`(去空白+标点[Unicode 常见中英标点]+ASCII 小写)
  - `fn text_similarity(a: &str, b: &str) -> f32`(归一化 Levenshtein 与完全包含取 max;空串相似度 0)
  - `struct PendingMic { text: String, norm: String, start_ms: u64, end_ms: u64, samples_len: usize, embedding_input: Vec<f32>, held_at: std::time::Instant }`(实现可按需增减字段——目标:release 时能完成 embed/assign/on_final 全流程)
  - 流程:Mic final 识别后 → 对照 `recent_system`(时间邻近:`[start,end]` 区间交叠或起点差 < ECHO_WINDOW_MS;文本相似 ≥ 阈值)命中即丢(eprintln 一行含两段文本前缀便于校准);未命中入 `pending_mic`。System final → 先对照 `pending_mic` 丢匹配项 → 处理自身(embed/assign/on_final)→ 入 `recent_system`(按 end_ms 裁剪 10s)。每次循环迭代(含 100ms timeout tick)release 到期 pending(顺序保持)。Disconnected → release 全部 pending → Snapshot → break。
- 注意:被丢弃段不 embed/不 assign/不 on_final(声纹与转写双清);release 时才走完整处理链。

**Steps(TDD):**
- [ ] 新增 `ScriptedRecognizer`(测试用,按队列返回指定文本)+ 5 个测试(spec §3 去重清单:mic先/system先/不相似不误杀/结束排干/到期 release)。时间控制:hold 到期用 `held_at` + 测试里 sleep >2.5s 不可取——**把 hold 时长做成 worker 参数**(`echo_hold: Duration`,生产调用点传 `Duration::from_millis(ECHO_HOLD_MS)`,测试传短值如 50ms)以免慢测试;`start_session` 同步增参或用 `#[cfg(test)]` 构造器,选前者(调用点少)。
- [ ] 实现 → 全量绿(既有测试因增参适配)→ commit:`P4.5 Task 4: 跨路回声去重(mic hold-and-release + 文本相似)`

---

### Task 5: 全量验证 + 冒烟清单

- [ ] `cargo test` 全量 + `npm run check`/`build`;账本记录。
- [ ] 人工冒烟:①中断笔记详情「继续录制」→ 接着录,seq/时间轴/说话人编号连续,停止后 complete;②已完成笔记同上;③他人电脑外放场景:同句只出一条(system 侧)、单说话人;④本人正常说话(无外放)不被误杀,final 延迟 ~2.5s 内可接受;⑤P4 主链不回归(多方区分/AEC/chips)。

## Self-Review 记录

- Spec 覆盖:续录语义/质心连续性/前端灌注(T1-T3)、去重双向+排干+零 system 延迟(T4)、局限与常量(T4/Global)。
- 类型一致性:ClusterSnapshot 在 registry 定义、session(DiarEvent)与 writer(store_centroids/registry_snapshot)引用同一类型;base_ms 由 writer 单一来源。
- 风险:spawn_session 重构触碰竞态协议——T2 明确「行为不变+注释同步」,审查按锁序核。
