# 整理四项跟进实施计划(重新整理/失效目标保险/拆回原身份/多列布局)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 按 `docs/2026-08-01-speaker-tidy-followups-design.md` 落地四项:手动「重新整理」、失效目标双重保险+存档折叠(徽标不计存档)、失效回执「拆回独立说话人」、概览页自适应多列卡片流。

**Architecture:** 后端在 merge_journal/voiceprints 上加「仅恢复 loser」的拆回路径(不动 winner、不复活链上条目),命令层沿用录制中拒绝守卫;前端在 tidyQueue 纯逻辑层加失效建议过滤与待办/存档分组,概览页消费分组渲染折叠存档区与拆回按钮,布局改 CSS grid。

**Tech Stack:** Rust(Tauri 命令层+store 单测)/ Svelte 5 runes / vitest / 既有 IPC 仿真层无头回归。

## Global Constraints

- 工作目录:仓库的 `.claude/worktrees/speaker-tidy`(worktree,勿 cd 到主仓)。
- 全部 UI 文案中文;禁 emoji 图标(DESIGN.md);样式用既有 token(`--surface`/`--hairline`/`.mini` 等)。
- 代码注释风格:中文、讲约束不讲流水账(与仓库一致)。
- 既有测试不许破坏:cargo test(933+)、vitest(168)、`npm run check` 0 错误;既有测试禁止 Sidebar 出现「待整理」字样(徽标 title 用「N 项待处理」)。
- 录制中口径:合并/删除/撤销/拆回/一键确认置灰并由后端拒绝;忽略/保留/重新整理/展开收起不受限。
- 每个 Task 结尾提交,消息用 `feat(tidy):`/`refactor(tidy):` 前缀,落款 `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`。

---

### Task 1: merge_journal 仅恢复 loser 侧样本的助手

**Files:**
- Modify: `src-tauri/src/store/merge_journal.rs`(`restore_samples` 附近,~L340)
- Test: 同文件 `#[cfg(test)] mod tests`

**Interfaces:**
- Produces: `pub fn restore_loser_samples(&self, id: &str, vp_samples_dir: &Path) -> anyhow::Result<usize>` — 只拷 loser 侧快照副本回声纹样本目录;无常规槽位而有 `<loser>-cut.wav` 兜底截声时,把它改名拷成 `<loser>.wav`(槽 1),让拆回后的人有得听。Task 2 消费。

- [ ] **Step 1: 写失败测试**(加进 merge_journal.rs 的 tests 模块;`journal_with_side_sample` 若无同等助手,参照既有 `loser_sample_copies_*` 测试的搭建方式创建条目与副本文件)

```rust
#[test]
fn restore_loser_samples_copies_only_loser_side() {
    let (dir, j) = test_journal(); // 既有测试助手;没有就照 loser_sample_copies_lists_snapshot_or_empty 的搭建方式
    put_side_file(&j, "m-P1", "loser", "P1.wav");
    put_side_file(&j, "m-P1", "winner", "P9.wav");
    let out = dir.path().join("vp");
    let n = j.restore_loser_samples("m-P1", &out).unwrap();
    assert_eq!(n, 1);
    assert!(out.join("P1.wav").exists());
    assert!(!out.join("P9.wav").exists(), "winner 侧不许被拷回");
}

#[test]
fn restore_loser_samples_promotes_cut_to_slot1_when_no_regular() {
    let (dir, j) = test_journal();
    put_side_file(&j, "m-P2", "loser", "P2-cut.wav");
    let out = dir.path().join("vp");
    let n = j.restore_loser_samples("m-P2", &out).unwrap();
    assert_eq!(n, 1);
    assert!(out.join("P2.wav").exists(), "仅有兜底截声时拷成槽 1,拆回的人才有样本可听");
    assert!(!out.join("P2-cut.wav").exists());
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test restore_loser_samples`
Expected: 编译错误 `restore_loser_samples` 不存在。

- [ ] **Step 3: 最小实现**(紧邻 `restore_samples`)

```rust
/// 只把 loser 侧快照副本拷回声纹样本目录(拆回用;undo 用 restore_samples 双侧)。
/// 无常规槽位而有 `<loser>-cut.wav` 兜底截声时,改名拷成 `<loser>.wav`(槽 1)——
/// sample_slot_path 不识别 -cut 后缀,原名拷回等于拆回的人"无样本"。
pub fn restore_loser_samples(&self, id: &str, vp_samples_dir: &Path) -> anyhow::Result<usize> {
    std::fs::create_dir_all(vp_samples_dir)?;
    let copies = self.sample_copies(id, "loser");
    let is_cut = |p: &PathBuf| {
        p.file_stem().and_then(|s| s.to_str()).is_some_and(|s| s.ends_with("-cut"))
    };
    let mut n = 0usize;
    for p in copies.iter().filter(|p| !is_cut(p)) {
        std::fs::copy(p, vp_samples_dir.join(p.file_name().unwrap()))?;
        n += 1;
    }
    if n == 0 {
        if let Some(cut) = copies.iter().find(|p| is_cut(p)) {
            let stem = cut.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
            let loser = stem.trim_end_matches("-cut");
            std::fs::copy(cut, vp_samples_dir.join(format!("{loser}.wav")))?;
            n = 1;
        }
    }
    Ok(n)
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test restore_loser_samples`
Expected: 2 passed;`cargo test merge_journal` 全绿。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/store/merge_journal.rs
git commit -m "feat(tidy): merge_journal 仅恢复 loser 侧样本(兜底截声升槽1)"
```

---

### Task 2: VoiceprintStore::restore_merged_person(拆回原身份)

**Files:**
- Modify: `src-tauri/src/store/voiceprints.rs`(`undo_merge` 之后,~L403)
- Test: 同文件 tests 模块(参照 `undo_merge_restores_records_redirects_samples_and_denylists` 的搭建)

**Interfaces:**
- Consumes: Task 1 的 `restore_loser_samples`。
- Produces: `pub fn restore_merged_person(&self, journal_id: &str) -> anyhow::Result<String>` — 返回恢复的 person id。Task 3 消费。

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn restore_merged_person_rebuilds_loser_without_touching_winner() {
    // 搭建:P1(loser) 并入 P2(winner),再让条目失效(如把 P2 并入 P3,或直接
    // journal.invalidate(&["P2"], "相关人物随后又被合并", None))。
    let (store, journal_id) = setup_invalidated_merge(); // 参照 undo_merge 测试的搭建函数拆出
    let winner_before = store.load().people.get("P2").cloned();
    let pid = store.restore_merged_person(&journal_id).unwrap();
    assert_eq!(pid, "P1");
    let vp = store.load();
    assert!(vp.people.contains_key("P1"), "loser 按快照重建");
    assert_eq!(vp.people.get("P2").cloned(), winner_before, "winner 不动");
    assert!(!vp.redirects.contains_key("P1"), "loser 不再被重定向");
    let journal = MergeJournal::new(store_root(&store));
    assert!(journal.entry(&journal_id).is_err(), "条目删除");
    assert!(journal.auto_denylist().iter().any(|p| p == "P1>P2"), "pair 进拒绝名单");
}

#[test]
fn restore_merged_person_rejects_valid_entry() {
    let (store, journal_id) = setup_valid_merge(); // 未失效条目
    let err = store.restore_merged_person(&journal_id).unwrap_err().to_string();
    assert!(err.contains("撤销"), "有效条目应引导走撤销而非拆回: {err}");
}

#[test]
fn restore_merged_person_does_not_revive_chain() {
    // P1→P2 落条目 A;P2→P3 落条目 B(A 被 B 连带失效,invalidated_by=B)。
    // 拆回 B 的 loser(P2)后,A 不得复活为可撤销——B 那次合并并未被撤销。
    let (store, entry_a_id, entry_b_id) = setup_chained_merges();
    store.restore_merged_person(&entry_b_id).unwrap();
    let journal = MergeJournal::new(store_root(&store));
    let a = journal.entry(&entry_a_id).unwrap();
    assert!(a.invalid_reason.is_some(), "链上条目不复活");
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test restore_merged_person`
Expected: 编译错误,方法不存在。

- [ ] **Step 3: 最小实现**

```rust
/// 从失效日志条目拆回被并入方:按快照把 loser 重建为原编号独立说话人,还原
/// 指向他的 redirects(历史笔记段落重新归他),pair 进自动归并拒绝名单,条目
/// 删除。与 undo_merge 的区别:不动 winner(那次合并未被撤销,质心里已混入的
/// 贡献不抽回,后续录制自然纠正),也不 revive 链上条目(它们的前置状态未还原,
/// 复活会造成"可撤销"假象)。仅失效条目可拆;有效条目走 undo_merge。
pub fn restore_merged_person(&self, journal_id: &str) -> anyhow::Result<String> {
    let _guard = vp_guard();
    let journal = super::merge_journal::MergeJournal::new(self.root.clone());
    let entry = journal.entry(journal_id)?;
    if entry.invalid_reason.is_none() {
        anyhow::bail!("该条仍可直接撤销,不需要拆回");
    }
    let mut vp = self.load();
    if vp.people.contains_key(&entry.loser) {
        anyhow::bail!("原编号 {} 已存在,无法拆回", entry.loser);
    }
    vp.people.insert(entry.loser.clone(), entry.loser_person.clone());
    vp.redirects.remove(&entry.loser);
    for k in &entry.redirects_to_loser {
        vp.redirects.insert(k.clone(), entry.loser.clone());
    }
    self.save(&vp)?;
    if let Err(e) = journal.restore_loser_samples(journal_id, &self.root.join("voiceprints")) {
        eprintln!("拆回说话人:样本副本还原失败(不影响库): {e}");
    }
    journal.deny_auto(&format!("{}>{}", entry.loser, entry.winner));
    journal.remove(journal_id)?;
    Ok(entry.loser.clone())
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test restore_merged_person && cargo test undo_merge`
Expected: 新 3 个通过,undo 既有 4 个不回归。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/store/voiceprints.rs
git commit -m "feat(store): restore_merged_person 从失效日志拆回原身份(不动 winner/不复活链)"
```

---

### Task 3: 命令层 restore_merged_person + 前端 API

**Files:**
- Modify: `src-tauri/src/lib.rs`(`undo_merge` 命令之后 ~L3208;invoke_handler 注册表 ~L4647)
- Modify: `src/lib/people.ts`(文件尾)

**Interfaces:**
- Consumes: Task 2 的 store 方法。
- Produces: IPC 命令 `restore_merged_person(journalId) -> String`;TS `restoreMergedPerson(journalId: string): Promise<string>`。Task 5 消费。

- [ ] **Step 1: lib.rs 加命令**(录制中拒绝口径同 undo_merge;拆回改库,重建人物图谱)

```rust
/// 失效回执「拆回独立说话人」:按快照重建被并入方。录制中拒绝:理由同 merge_person。
#[tauri::command]
fn restore_merged_person(
    app: AppHandle,
    state: State<AppState>,
    journal_id: String,
) -> Result<String, String> {
    if state.session.lock().unwrap().is_some() {
        return Err("录制中不能拆回说话人".into());
    }
    let root = data_root(&app).map_err(|e| e.to_string())?;
    let pid = store::VoiceprintStore::new(root.clone())
        .restore_merged_person(&journal_id)
        .map_err(|e| e.to_string())?;
    queue_person_graph_rebuild(&app, root, "拆回说话人")?;
    Ok(pid)
}
```

并在 invoke_handler 列表 `undo_merge,` 之后加一行 `restore_merged_person,`。

- [ ] **Step 2: people.ts 加 API**(`dismissTidyItem` 之后)

```ts
/** 失效回执「拆回独立说话人」:按合并时快照把被并入方重建为原编号独立说话人,
    返回其 id;录制中后端拒绝。 */
export const restoreMergedPerson = (journalId: string) =>
  invoke<string>("restore_merged_person", { journalId });
```

- [ ] **Step 3: 验证编译**

Run: `cd src-tauri && cargo test --no-run 2>&1 | tail -3 && cd .. && npm run check 2>&1 | tail -2`
Expected: 编译通过;svelte-check 0 错误。

- [ ] **Step 4: 提交**

```bash
git add src-tauri/src/lib.rs src/lib/people.ts
git commit -m "feat(ipc): restore_merged_person 命令与前端 API(录制中拒绝)"
```

---

### Task 4: tidyQueue 纯逻辑——失效建议过滤 + 待办/存档分组

**Files:**
- Modify: `src/lib/tidyQueue.ts`
- Test: `src/lib/tidyQueue.test.ts`

**Interfaces:**
- Produces:
  - `buildTidyQueue(...)` 行为变化:loser 或 winner 不在 people 的建议被滤掉(签名不变)。
  - `export function splitArchive(items: TidyItem[]): { pending: TidyItem[]; archived: TidyItem[] }` — archived=`invalid_reason ≠ null` 的回执,pending=其余(队列序不变)。Sidebar 徽标与概览页均消费 pending。

- [ ] **Step 1: 写失败测试**(加进 tidyQueue.test.ts;`person`/`receipt`/`suggestion` 构造器沿用该文件既有测试的写法,没有就照 MergeReceipt/PersonMergeSuggestion 类型手写字面量)

```ts
describe("buildTidyQueue 失效目标过滤", () => {
  it("winner 不在库的建议被滤掉(已合并/已删除的人不能再作合并目标)", () => {
    const people = [person("P1")];
    const sug = [suggestion("P1", "P9")]; // P9 已不在库
    const q = buildTidyQueue(people, sug, [], new Set());
    expect(q.filter((i) => i.kind === "suggestion")).toEqual([]);
  });

  it("loser 不在库的建议同样滤掉", () => {
    const people = [person("P9")];
    const q = buildTidyQueue(people, [suggestion("P1", "P9")], [], new Set());
    expect(q.filter((i) => i.kind === "suggestion")).toEqual([]);
  });

  it("双方都在库的建议保留", () => {
    const people = [person("P1"), person("P9")];
    const q = buildTidyQueue(people, [suggestion("P1", "P9")], [], new Set());
    expect(q.filter((i) => i.kind === "suggestion")).toHaveLength(1);
  });
});

describe("splitArchive", () => {
  it("失效回执进 archived,其余进 pending,各自保持队列序", () => {
    const items = buildTidyQueue(
      [person("P1")],
      [],
      [receipt("J1", null), receipt("J2", "相关人物随后又被合并"), receipt("J3", null)],
      new Set(),
    );
    const { pending, archived } = splitArchive(items);
    expect(archived.map((i) => i.kind === "receipt" && i.receipt.journal_id)).toEqual(["J2"]);
    expect(pending.filter((i) => i.kind === "receipt")).toHaveLength(2);
  });

  it("空输入两侧都空", () => {
    expect(splitArchive([])).toEqual({ pending: [], archived: [] });
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `npx vitest run src/lib/tidyQueue.test.ts`
Expected: splitArchive 不存在 + 过滤断言失败。

- [ ] **Step 3: 最小实现**

buildTidyQueue 中建议追加处改为:

```ts
const ids = new Set(people.map((p) => p.id));
// 失效目标保险:后端建议按当前库现算,但拉取与消费之间有时序窗口——已合并/
// 已删除的人不能再作合并目标(或来源),否则点「合并」必报「人物不存在」。
for (const s of suggestions) {
  if (ids.has(s.loser) && ids.has(s.winner)) items.push({ kind: "suggestion", suggestion: s });
}
```

文件尾追加:

```ts
/** 待办/存档分组:失效回执(不能再撤销)只剩回看价值,折叠进存档区,不算待办
    ——徽标与「N 件待处理」按 pending 计。 */
export function splitArchive(items: TidyItem[]): { pending: TidyItem[]; archived: TidyItem[] } {
  const archived = items.filter((i) => i.kind === "receipt" && i.receipt.invalid_reason !== null);
  const pending = items.filter((i) => !archived.includes(i));
  return { pending, archived };
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `npx vitest run src/lib/tidyQueue.test.ts`
Expected: 新 5 个通过,既有全绿。

- [ ] **Step 5: 提交**

```bash
git add src/lib/tidyQueue.ts src/lib/tidyQueue.test.ts
git commit -m "feat(tidy): 队列滤掉失效目标建议,失效回执分组进存档"
```

---

### Task 5: 概览页——重新整理按钮/存档折叠组/拆回动作/徽标口径 + 详情页对方存在校验

**Files:**
- Modify: `src/routes/speakers/+page.svelte`
- Modify: `src/lib/Sidebar.svelte:153`(tidyBadge)
- Modify: `src/routes/speakers/[id]/+page.svelte`(related 派生)

**Interfaces:**
- Consumes: Task 3 `restoreMergedPerson`、Task 4 `splitArchive`。

- [ ] **Step 1: 徽标与队列改用 pending**

Sidebar.svelte(import 处加 `splitArchive`):

```ts
/** 「概览与整理」徽标:待拍板的活(有效回执+建议+同名组+无样本);失效存档不计。 */
const tidyBadge = $derived(
  splitArchive(buildTidyQueue(people, tidy.visible, tidy.receipts, tidy.dismissed)).pending.length,
);
```

+page.svelte 派生区(替换原 `const queue = $derived(buildTidyQueue(...))`):

```ts
const queueParts = $derived(splitArchive(buildTidyQueue(people, tidy.visible, tidy.receipts, tidy.dismissed)));
const queue = $derived(queueParts.pending);
const archived = $derived(queueParts.archived);
```

`invalidReceiptsN` 删除,引用处改 `archived.length`;`receiptsN` 语义自动变为"有效回执数"(queue 里已无失效条),摘要文案不动。`let archiveOpen = $state(false);` 加进状态区。

- [ ] **Step 2: 重新整理按钮**(`.tidy-head` 内,`refreshing` 之前)

```svelte
<span class="head-spacer"></span>
<button class="mini plain" disabled={tidy.loading} onclick={() => void tidy.refresh()}>重新整理</button>
```

样式:`.head-spacer { flex: 1; }`(tidy-head 已是 flex)。原「一键确认全部」`.tools` 块整体删除(挪进存档组头)。

- [ ] **Step 3: 回执卡抽 snippet,供队列与存档区共用**

把 `{#if item.kind === "receipt"}` 分支的整段 `<section class="card">…</section>` 抽成顶层 `{#snippet receiptCard(r: MergeReceipt)}`(内部 `class:archived={r.invalid_reason}` 等原样保留;`{@render cardError(\`r:${r.journal_id}\`)}` 键改为字符串模板,不再依赖 item)。失效侧动作区改为:

```svelte
{#if r.invalid_reason}
  <button class="mini accent" disabled={busy || live} onclick={() => doRestore(r)}>拆回独立说话人</button>
  <button class="mini" disabled={busy || live} onclick={() => doAck(r)}>知道了</button>
{:else}
  <!-- 好/撤销 两钮原样 -->
{/if}
```

失效卡尾注 hint 改为:

```
不能撤销时:先听「合并时的原声」核对;确认并错可「拆回独立说话人」——按合并时快照
恢复原编号,历史笔记段落重新归他,之后录制也能重新认出;当时的合并对象不受影响。
```

队列分支变成 `{#if item.kind === "receipt"}{@render receiptCard(item.receipt)}{:else if ...}`。

动作函数区加:

```ts
async function doRestore(r: MergeReceipt) {
  await act(
    async () => {
      await restoreMergedPerson(r.journal_id);
    },
    () => tidy.removeReceipt(r.journal_id),
    `r:${r.journal_id}`,
  );
}
```

(people.ts import 行加 `restoreMergedPerson`。)

- [ ] **Step 4: 存档折叠组**(`.stack` 的 `{#each}` 之后、`</section>` 之前;空队列 hint 的判断改为 `queue.length === 0 && archived.length === 0` 才显示「都整理完了」)

```svelte
{#if archived.length > 0}
  <div class="archive">
    <div class="archive-head">
      <button class="archive-toggle" onclick={() => (archiveOpen = !archiveOpen)}>
        {archiveOpen ? "收起" : "展开"} {archived.length} 条已存档
      </button>
      <span class="hint">相关人物已再次合并等原因,不能再撤销;可逐条核对或拆回。</span>
      <span class="spacer"></span>
      <button class="mini plain" disabled={busy || live} onclick={ackAllInvalid}>一键确认全部</button>
    </div>
    {#if archiveOpen}
      <div class="stack">
        {#each archived as item (tidyItemKey(item))}
          {#if item.kind === "receipt"}{@render receiptCard(item.receipt)}{/if}
        {/each}
      </div>
    {/if}
  </div>
{/if}
```

`ackAllInvalid` 内部改为遍历 `archived`(原 `queue.filter(invalid)` 已拿不到失效条)。样式:

```css
.archive { margin-top: 1rem; }
.archive-head { display: flex; align-items: center; gap: 0.6rem; margin-bottom: 0.8rem; }
.archive-toggle {
  border: 1px solid var(--hairline-strong); background: transparent; color: var(--ink);
  border-radius: var(--radius-md); font-size: 0.82rem; padding: 0.25em 0.75em; cursor: pointer;
}
.archive-toggle:hover { background: var(--surface-soft); }
```

- [ ] **Step 5: 详情页对方存在校验**([id]/+page.svelte `related` 派生替换)

```ts
/** 上下文建议:对方必须仍在库(失效目标保险,与概览队列同口径)。 */
const related = $derived(
  tidy
    .involving(personId)
    .filter((s) => people.some((p) => p.id === (s.loser === personId ? s.winner : s.loser))),
);
```

- [ ] **Step 6: 验证**

Run: `npm run check 2>&1 | tail -2 && npx vitest run 2>&1 | tail -4`
Expected: 0 错误、全绿(Sidebar 文案测试不破:徽标 title 仍「N 项待处理」)。

- [ ] **Step 7: 提交**

```bash
git add src/routes/speakers/+page.svelte src/routes/speakers/[id]/+page.svelte src/lib/Sidebar.svelte
git commit -m "feat(tidy): 重新整理按钮+失效回执折叠存档(徽标不计)+拆回独立说话人入口"
```

---

### Task 6: 概览页自适应多列卡片流

**Files:**
- Modify: `src/routes/speakers/+page.svelte`(styles)

- [ ] **Step 1: 布局改造**

`.container` 的 `max-width: 44rem;` 删除(padding 1.5rem 保留);`.desc` 的 `max-width: 40rem` 保留。`.stack` 改为:

```css
/* 自适应多列:宽窗 2-3 列一屏多卡,窄窗自动退单列;卡片顶对齐各保己高,
   DOM 序即队列序(逐行阅读)。存档组/横幅/撤销条在 .stack 外,天然整行。 */
.stack {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(26rem, 1fr));
  gap: 0.8rem;
  align-items: start;
}
```

(原 `flex-direction: column` 声明删除。)

- [ ] **Step 2: 无头回归截图验证**

Run(browse + IPC 仿真层,复用 scratchpad/mock-ipc.js 流程):1280 与 2000 视口各截一张 `/speakers`,确认宽窗 ≥2 列、窄窗单列、存档组整行、无水平滚动条。
Expected: 布局符合;console 0 错误。

- [ ] **Step 3: 提交**

```bash
git add src/routes/speakers/+page.svelte
git commit -m "feat(tidy): 概览页自适应多列卡片流,放开 44rem 列宽"
```

---

### Task 7: 端到端回归(仿真层)+ 真机核对 + 文档同步

**Files:**
- Modify: `/private/tmp/.../scratchpad/mock-ipc.js`(加 `restore_merged_person` 处理:失效条目→恢复 undoRestore 快照/删条目/返 id;有效条目→抛「该条仍可直接撤销」)
- Modify: `README.md` / `README.en.md`(整理段:补一句「重新整理/存档折叠/拆回」;沿用全角标点)

- [ ] **Step 1: 仿真层全流程回归**

场景:① 点「重新整理」→ `apply_confident_merges` 再次调用、loading 态出现;② 存档组默认收起、徽标只计待办、展开后逐卡渲染;③ 失效卡「拆回独立说话人」→ 卡收起、人物回侧栏、`restore_merged_person` 调用带对 journalId;④ 拆回失败(failNext)→ 卡片级错误就地显示;⑤ 建议 winner 被删(直接改 mock people)后重新整理 → 该建议卡消失;⑥ 既有主干闭环(合并/撤销/好/忽略)复测。
Expected: 全过,console 0 错误,截图归档 `.gstack/qa-reports/screenshots/3x-*.png`。

- [ ] **Step 2: 全量验证**

Run: `cd src-tauri && cargo test 2>&1 | tail -3 && cd .. && npx vitest run 2>&1 | tail -3 && npm run check 2>&1 | tail -2`
Expected: cargo 全绿(933+新增)、vitest 全绿(168+新增)、check 0 错误。

- [ ] **Step 3: 真机核对**(tauri dev 已跑,vite 热载)

整屏截图确认:多列布局生效、存档折叠行在、16 条不再铺首屏、徽标数下降;请用户点一例「拆回」。

- [ ] **Step 4: 提交**

```bash
git add README.md README.en.md
git commit -m "docs: 整理段同步重新整理/存档折叠/拆回说明"
```

---

## Self-Review 结果

- 规格覆盖:§1→Task 5 Step2;§2 过滤→Task 4+5(Step5);§2 折叠+徽标→Task 4+5;§3→Task 1-3+5(Step3);§4→Task 6;测试→各 Task 内+Task 7。无缺口。
- 占位扫描:测试搭建助手(`test_journal`/`setup_invalidated_merge` 等)标注了"参照既有测试搭建方式",属指路非占位;其余步骤均含完整代码。
- 类型一致:`restore_merged_person` Rust 返回 `String`/TS `Promise<string>`;`splitArchive` 两端消费名一致;`doRestore`/`receiptCard` 签名前后一致。
