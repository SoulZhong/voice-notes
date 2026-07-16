# Aing Phase 2a — 笔记页实体高亮 + 相关笔记 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: 用 superpowers:subagent-driven-development 逐任务实现。步骤用 `- [ ]` 勾选。

**Goal:** 把 Aing 已产出的实体在笔记详情页**变得看得见**:①修订稿正文里对实体提及区间做高亮(读 aing.json 的 mentions/entities,accent-tint 底 + tooltip 显实体名);②笔记底部新增「相关笔记」区(经知识图谱的 `related_notes` 查共享实体的其他笔记)。纯展示增量,缺数据/旧笔记优雅隐藏,不影响现有页面。

**Architecture:** 后端加一个薄 Tauri 命令 `note_related`(包 `graph::related_notes` + 联查 `NoteStore::list()` 拼标题/时间,失败返回空不 Err)。前端:①补齐 `src/lib/notes.ts` 的 `RefinedDoc`/`RefinedParagraph`/`RefineStages` TS 类型缺的 `entities`/`mentions`/`stages.entities` + 新 `Entity`/`Mention` 接口;②纯函数 `splitMentions(text, mentions)`(按 char 下标把段落文本切成高亮/普通片段,`Array.from` 规避 astral 坑,重叠跳过,vitest 测);③笔记页把 `.para-text` 单串插值改成 `{#each}` 片段渲染,实体片段 accent-tint 高亮 + title=实体名;④转写容器后加「相关笔记」`.card.col`(照抄会议搭子详情页现成样式)。**实体点击导航(人→会议搭子、非人→实体页)延后 Phase 2b**(需全局 id 解析 + 图谱页作跳转目标);本期高亮只作视觉 + tooltip,不作链接。

**Tech Stack:** SvelteKit(Svelte/TS)+ vitest(已是本仓 JS 单测基座);Rust(一个薄命令 + 一个 ipc struct)。

## Global Constraints

- **纯增量、优雅降级**:`note_related` 查询失败/图谱空 → 返回空数组,前端 `.catch(() => [])`;段落 `mentions` 为空或字段缺失(旧 aing.json)→ 正文按纯文本渲染,不报错。相关笔记为空 → 该区块整块不渲染。绝不因图谱/实体拖垮笔记详情页加载。
- **配色克制(DESIGN.md 硬约束)**:实体高亮**只用单色** `accent` / `accent-tint`(不按 entity.kind 分七彩,避免正文视觉噪音);hover 态仿 `editable-text`(hover `accent-tint` 底 + radius-sm);禁 emoji / Unicode 符号图标(要图标用 16px 线性 SVG)。
- **char 下标 = Unicode scalar**(非 UTF-16 code unit):`splitMentions` 用 `Array.from(text)` 切分再 `join`,对 BMP 中文一致、对 astral 字符也安全;mentions 可能重叠/乱序,函数需排序 + 跳过重叠不崩。
- **`data_root` 不是 `notes_dir`**:`graph::related_notes` 吃 `data_root`(graph.sqlite 与 notes/ 平级),命令层用 `data_root(&app)`。
- **不改契约**:hook key、其它命令/事件名、MCP 工具名、aing.json 结构、既有 TS 类型的现有字段——只**新增**字段/类型/命令。
- **实体点击导航不在本期**:本期实体高亮不是链接,只 tooltip;人/非人实体详情跳转归 Phase 2b。
- git 提交不加 `Co-Authored-By`/Claude/Generated 署名。
- 验证:`npm run check` 0/0;`npm run test`(vitest)绿;`cargo check` 无 error;`cargo test --lib` 全绿。

## File Structure

- **Modify** `src-tauri/src/ipc.rs` — 加 `pub struct RelatedNote { id, title, started_at, shared_entities }`(Serialize)。
- **Modify** `src-tauri/src/lib.rs` — 加 `#[tauri::command] fn note_related(...)`;注册进 `generate_handler!`。
- **Modify** `src/lib/notes.ts` — 补 `Entity`/`Mention` 接口 + `RefinedParagraph.mentions` + `RefinedDoc.entities` + `RefineStages.entities`;加纯函数 `splitMentions`;加 `RelatedNote` 类型 + `noteRelated` 绑定。
- **Create** `src/lib/notes.test.ts` — vitest 测 `splitMentions`。
- **Modify** `src/routes/notes/[id]/+page.svelte` — 段落渲染改片段化高亮 + 底部相关笔记区 + 少量 CSS。

---

### Task 1: 后端 `note_related` 命令(图谱相关笔记 → 笔记摘要)

**Files:**
- Modify: `src-tauri/src/ipc.rs`(ipc 输出 struct;参照 `ipc::PersonSummary` `ipc.rs:104-116`)
- Modify: `src-tauri/src/lib.rs`(命令 + `generate_handler!` 注册 `lib.rs:2934` 一带;参照 `person_notes` `lib.rs:1491-1520`)

**Interfaces:**
- Produces:
  - `pub struct ipc::RelatedNote { pub id: String, pub title: String, pub started_at: String, pub shared_entities: i64 }`（`#[derive(Debug, Clone, Serialize)]`）
  - Tauri 命令 `note_related(app, id: String) -> Result<Vec<ipc::RelatedNote>, String>`（查 `graph::related_notes` 得 `(note_id, shared)`,用 `NoteStore::list()` 联查标题/时间;图谱查询失败 → 返回空 `vec![]` 不 Err)
- Consumes: `graph::related_notes`、`store::NoteStore`、`store::validate_note_id`、`data_root`/`notes_dir`。

- [ ] **Step 1: 加 ipc struct**

在 `src-tauri/src/ipc.rs`(`PersonSummary` 定义附近)加:

```rust
/// 相关笔记(笔记详情页「相关笔记」区):与当前笔记共享 Aing 实体的其他笔记 + 共享实体数。
#[derive(Debug, Clone, serde::Serialize)]
pub struct RelatedNote {
    pub id: String,
    pub title: String,
    pub started_at: String,
    pub shared_entities: i64,
}
```

- [ ] **Step 2: 加命令**

在 `src-tauri/src/lib.rs`(`person_notes` 附近,读操作扎堆处)加:

```rust
/// 相关笔记:与该笔记共享 Aing 实体的其他笔记(经知识图谱),按共享实体数降序。
/// 纯增值:图谱缺失/查询失败 → 返回空列表(前端据此隐藏该区块),绝不 Err 拖垮详情页。
#[tauri::command]
fn note_related(app: AppHandle, id: String) -> Result<Vec<ipc::RelatedNote>, String> {
    store::validate_note_id(&id).map_err(|e| e.to_string())?;
    let Ok(root) = data_root(&app) else { return Ok(vec![]) };
    let pairs = match graph::related_notes(&root, &id) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("note_related: 图谱查询失败,返回空: {e}");
            return Ok(vec![]);
        }
    };
    if pairs.is_empty() {
        return Ok(vec![]);
    }
    let notes_root = notes_dir(&app).map_err(|e| e.to_string())?;
    let summaries = store::NoteStore::new(notes_root).list();
    let by_id: std::collections::HashMap<String, &store::NoteSummary> =
        summaries.iter().map(|n| (n.id.clone(), n)).collect();
    let out = pairs
        .into_iter()
        .filter_map(|(nid, shared)| {
            by_id.get(&nid).map(|n| ipc::RelatedNote {
                id: n.id.clone(),
                title: n.title.clone(),
                started_at: n.started_at.clone(),
                shared_entities: shared,
            })
        })
        .collect();
    Ok(out)
}
```

（注:`NoteSummary` 字段名以实际为准——`ipc`/`store::mod.rs:89-96` 有 `id/title/started_at/...`;若 `title` 是 `Option`,按实际取 `.clone().unwrap_or_default()`。实现者先看 `store::NoteSummary` 定义对齐字段。)

- [ ] **Step 3: 注册命令**

在 `lib.rs` 的 `tauri::generate_handler![ ... ]` 数组里,`get_refined,` / `person_notes,` 附近加一行 `note_related,`。

- [ ] **Step 4: 验证**

Run: `cargo check --manifest-path src-tauri/Cargo.toml 2>&1 | tail -3` → 无 error。
Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib 2>&1 | tail -2` → 全绿(命令是薄包装,逻辑由既有 `graph::related_notes` 测试覆盖;命令本身无独立单测,与本仓 `get_refined`/`person_notes` 等命令一致不单测)。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/ipc.rs src-tauri/src/lib.rs
git commit -m "note_related 命令:知识图谱查共享实体的相关笔记 + 联查笔记摘要;图谱失败/空返回空列表不 Err;ipc::RelatedNote"
```

---

### Task 2: 前端类型补齐 + splitMentions 纯函数 + 绑定

**Files:**
- Modify: `src/lib/notes.ts`(类型 `notes.ts:64-88`;绑定风格 `notes.ts:103`)
- Create: `src/lib/notes.test.ts`

**Interfaces:**
- Produces:
  - `Entity { id, kind, name, aliases }`、`Mention { entity, start, end }` TS 接口;`RefinedParagraph.mentions?: Mention[]`、`RefinedDoc.entities?: Entity[]`、`RefineStages.entities?: string`
  - `export function splitMentions(text: string, mentions?: Mention[]): { text: string; entityId: string | null }[]`
  - `RelatedNote` 类型 + `export const noteRelated = (id) => invoke<RelatedNote[]>("note_related", { id })`
- Consumes: Task 1 命令。

- [ ] **Step 1: 写 vitest 失败测试**

新建 `src/lib/notes.test.ts`:

```ts
import { describe, it, expect } from "vitest";
import { splitMentions } from "./notes";

describe("splitMentions", () => {
  it("splits a paragraph into plain + entity segments by char offset", () => {
    // "灯塔计划下周启动":实体在 char 0..4
    const segs = splitMentions("灯塔计划下周启动", [{ entity: "ent_1", start: 0, end: 4 }]);
    expect(segs).toEqual([
      { text: "灯塔计划", entityId: "ent_1" },
      { text: "下周启动", entityId: null },
    ]);
  });
  it("handles a mention in the middle (中英混排 char 下标)", () => {
    // "我们叫它 Lighthouse 吧":Lighthouse 在 char 5..15
    const segs = splitMentions("我们叫它 Lighthouse 吧", [{ entity: "e1", start: 5, end: 15 }]);
    expect(segs.map((s) => s.text).join("")).toBe("我们叫它 Lighthouse 吧");
    expect(segs.find((s) => s.entityId === "e1")?.text).toBe("Lighthouse");
  });
  it("empty / missing mentions → single plain segment", () => {
    expect(splitMentions("你好", [])).toEqual([{ text: "你好", entityId: null }]);
    expect(splitMentions("你好", undefined)).toEqual([{ text: "你好", entityId: null }]);
  });
  it("sorts and skips overlapping mentions without crashing", () => {
    const segs = splitMentions("ABCDEF", [
      { entity: "b", start: 3, end: 5 },
      { entity: "a", start: 0, end: 2 },
      { entity: "x", start: 1, end: 4 }, // 与 a、b 重叠 → 跳过
    ]);
    expect(segs.filter((s) => s.entityId).map((s) => s.entityId)).toEqual(["a", "b"]);
  });
  it("ignores out-of-range mentions", () => {
    expect(splitMentions("AB", [{ entity: "z", start: 0, end: 99 }])).toEqual([{ text: "AB", entityId: null }]);
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `npm run test -- --run src/lib/notes.test.ts 2>&1 | tail -15`
Expected: 失败(`splitMentions` 未导出)。（若 `npm run test` 脚本名不同,看 package.json scripts,用其 vitest run 入口。）

- [ ] **Step 3: 补类型 + 实现 splitMentions + 绑定**

在 `src/lib/notes.ts`,给 `RefinedParagraph`/`RefineStages`/`RefinedDoc` 补字段,并加新类型与函数(放 `speakerLabel` 等纯函数附近):

```ts
export interface Mention {
  entity: string;
  start: number;
  end: number;
}
export interface Entity {
  id: string;
  kind: string;
  name: string;
  aliases?: string[];
}
```

`RefinedParagraph` 追加 `mentions?: Mention[];`;`RefineStages` 追加 `entities?: string;`;`RefinedDoc` 追加 `entities?: Entity[];`。

```ts
/** 按 char 下标把段落文本切成 { 普通片段 | 实体片段 } 序列(实体片段 entityId 非空)。
 *  用 Array.from 按 code point 切分(BMP 中文一致、astral 安全);mentions 排序 + 跳过重叠/越界。 */
export function splitMentions(
  text: string,
  mentions?: Mention[],
): { text: string; entityId: string | null }[] {
  const chars = Array.from(text);
  const valid = (mentions ?? [])
    .filter((m) => Number.isInteger(m.start) && Number.isInteger(m.end) && m.start >= 0 && m.end <= chars.length && m.start < m.end)
    .sort((a, b) => a.start - b.start || b.end - a.end);
  const out: { text: string; entityId: string | null }[] = [];
  let cur = 0;
  for (const m of valid) {
    if (m.start < cur) continue; // 与已产出区间重叠 → 跳过
    if (m.start > cur) out.push({ text: chars.slice(cur, m.start).join(""), entityId: null });
    out.push({ text: chars.slice(m.start, m.end).join(""), entityId: m.entity });
    cur = m.end;
  }
  if (cur < chars.length) out.push({ text: chars.slice(cur).join(""), entityId: null });
  if (out.length === 0) out.push({ text, entityId: null });
  return out;
}

export interface RelatedNote {
  id: string;
  title: string;
  started_at: string;
  shared_entities: number;
}
export const noteRelated = (id: string) => invoke<RelatedNote[]>("note_related", { id });
```

- [ ] **Step 4: 跑测试确认通过**

Run: `npm run test -- --run src/lib/notes.test.ts 2>&1 | tail -10` → 5 测过。
Run: `npm run check 2>&1 | tail -2` → 0/0(补了类型不该引入错误)。

- [ ] **Step 5: 提交**

```bash
git add src/lib/notes.ts src/lib/notes.test.ts
git commit -m "前端补 Aing 实体类型(RefinedDoc.entities/RefinedParagraph.mentions/RefineStages.entities + Entity/Mention)+ splitMentions 纯函数(char 下标切片段,vitest 测)+ noteRelated 绑定"
```

---

### Task 3: 笔记页实体高亮 + 相关笔记区

**Files:**
- Modify: `src/routes/notes/[id]/+page.svelte`（段落渲染 `+page.svelte:678-700`;转写容器之后加相关笔记区;`<style>`）

**Interfaces:**
- Consumes: Task 2 的 `splitMentions`、`noteRelated`、`RelatedNote`、`Entity`。

- [ ] **Step 1: 实体名查表 + 相关笔记数据**

在 `<script>` 里加:

```ts
import { splitMentions, noteRelated, type RelatedNote } from "$lib/notes";
// ...
let related = $state<RelatedNote[]>([]);
// 相关笔记:增值层,取失败静默按空。id 切换即重取(与 refresh 同步)。
$effect(() => {
  const forId = id;
  noteRelated(forId)
    .then((r) => { if (forId === id) related = r; })
    .catch(() => { if (forId === id) related = []; });
});
/** 段落内实体片段 → 实体名(tooltip 用):从本篇 refined.entities 按局部 id 查。 */
const entityName = $derived.by(() => {
  const m: Record<string, string> = {};
  for (const e of refined?.entities ?? []) m[e.id] = e.name;
  return (eid: string) => m[eid] ?? "";
});
```

- [ ] **Step 2: 段落渲染改片段化高亮**

把段落里的 `<span class="para-text">{p.text}</span>`(`+page.svelte:695` 一带)替换为:

```svelte
<span class="para-text">{#each splitMentions(p.text, p.mentions) as seg}{#if seg.entityId}<span class="entity-mention" title={entityName(seg.entityId)}>{seg.text}</span>{:else}{seg.text}{/if}{/each}</span>
```

（注:`{#each}` 之间不留空白,免在正文里插入多余空格——Svelte 对 `{#each}...{/each}` 内的换行/缩进会当文本渲染,务必写成紧凑一行或用 Svelte 的空白控制;若排版工具强制格式化换行,改用一个纯函数在 script 里先算好 segments 数组再渲染,渲染块保持无空白。)

- [ ] **Step 3: 转写容器后加相关笔记区**

在 `.transcript` 容器 `</div>` 之后(仍在主内容区)加:

```svelte
{#if related.length > 0}
  <section class="card col related">
    <div class="card-title">相关笔记</div>
    <ul class="appear-list">
      {#each related as n (n.id)}
        <li class="appear-row">
          <a href="/notes/{n.id}">{n.title}</a>
          <span class="appear-meta">共享 {n.shared_entities} 个实体</span>
        </li>
      {/each}
    </ul>
  </section>
{/if}
```

- [ ] **Step 4: CSS(实体高亮 + 相关笔记卡,照 DESIGN.md 克制)**

在 `<style>` 加(相关笔记照抄会议搭子详情页 `.card.col`/`.appear-*`;实体高亮单色 accent-tint):

```css
/* 实体提及高亮:正文单色,静态无底(不染正文),hover 才浮 accent-tint 底 + accent 字 */
.entity-mention {
  border-radius: var(--radius-sm);
  cursor: default;
  transition: background 120ms ease, color 120ms ease;
}
.entity-mention:hover {
  background: var(--accent-tint);
  color: var(--accent);
}
/* 相关笔记卡(照会议搭子详情页 .card.col/.appear-list) */
.related.card.col {
  display: block;
  background: var(--surface);
  border: 1px solid var(--hairline);
  border-radius: var(--radius-lg);
  padding: 14px 16px;
  margin: 0.75rem 0 0;
}
.related .card-title {
  font-size: 0.85rem;
  font-weight: 500;
  margin-bottom: 0.45rem;
}
.related .appear-list { list-style: none; margin: 0; padding: 0; max-height: 14rem; overflow-y: auto; }
.related .appear-row { display: flex; align-items: baseline; gap: 0.6rem; padding: 0.28rem 0; }
.related .appear-row a { color: var(--ink); text-decoration: none; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.related .appear-row a:hover { color: var(--accent); text-decoration: underline; }
.related .appear-meta { color: var(--ink-faint); font-size: 0.78rem; flex: none; }
```

（若组件 `<style>` 已有 `.card`/`.card-title`/`.appear-*` 定义则复用,不重复定义;先 grep 本文件是否已有。）

- [ ] **Step 5: 验证**

Run: `npm run check 2>&1 | tail -2` → 0/0。
Run: `npm run test -- --run 2>&1 | tail -3` → 全绿(含 Task 2 的 splitMentions)。
手工/截图:构造一份带 entities/mentions 的 aing.json(或用 playwright shim + 假数据)渲染 notes/[id],确认实体片段 hover 浮 accent-tint、相关笔记区在有数据时出现、无数据时整块隐藏、旧无 mentions 笔记正文正常。（截图归控制器最后统一做。）

- [ ] **Step 6: 提交**

```bash
git add src/routes/notes/\[id\]/+page.svelte
git commit -m "笔记页实体高亮(修订稿正文按 mention 区间片段化,accent-tint hover + 实体名 tooltip,单色克制)+ 底部相关笔记区(noteRelated,共享实体数;空则隐藏);旧无实体笔记正文正常"
```

---

## Self-Review
- **Spec 覆盖**:覆盖架构 spec Phase 2 的「合一笔记页:正文实体高亮 + 底部相关笔记」的**展示部分**。**不做**(归后续):实体点击导航(人→会议搭子、非人→实体页,需全局 id 解析 + 图谱页,Phase 2b)、图谱页与力导向可视化(Phase 2b)、会议搭子增补相关实体/笔记(Phase 2c)、录制中实时实体 chips(Phase 3)、Aing 状态 banner(已存在)。
- **红线**:所有实体/图谱表面缺数据/失败优雅隐藏,不影响笔记详情页与本地稿;`note_related` 失败返回空不 Err;`splitMentions` 对空/越界/重叠 mentions 不崩;旧 aing.json 无 mentions 字段照常渲染。
- **占位符**:命令/类型/纯函数/渲染/CSS/测试均给完整代码;唯 `NoteSummary` 字段名与「本组件是否已有 .card 样式」要求实现者对齐现有代码——非 TBD。
- **类型一致**:`Mention{entity,start,end}`/`Entity{id,kind,name,aliases}`(TS,Task 2)镜像 Rust `store::{Mention,Entity}`;`splitMentions` 签名/返回(Task 2)= Task 3 消费一致;`ipc::RelatedNote`(Task 1)= 前端 `RelatedNote`(Task 2)字段一致(id/title/started_at/shared_entities)。
- **契约不变**:只新增命令 `note_related` / ipc struct / TS 类型字段 / 纯函数 / 一个 CSS 类;不改任何现有命令、事件、hook key、aing.json 结构、既有 TS 字段。
