# 笔记详情页 markdown 编辑器(Milkdown)实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把笔记详情页的原始稿与精修稿统一为 Milkdown WYSIWYG 编辑器:原始稿保留段结构与段级编辑命令,精修稿从只读变为可编辑并保留实体高亮。

**Architecture:** 新增 `src/lib/editor/` 模块——`editorDoc.ts` 纯逻辑层(vitest 可测,不 import Milkdown)、`segmentSchema.ts`/`refinedSchema.ts` 两套 schema、`MarkdownEditor.svelte` 编辑器壳。原始稿零后端改动(复用 `edit_segment`);精修稿新增 `revision` 乐观并发字段与 `save_refined` 命令,后端在既有 `update_refined` 锁内骨架上实现整篇保存。

**Tech Stack:** Svelte 5 (runes) + SvelteKit、Milkdown v7(`@milkdown/kit`,含 ProseMirror 再导出)、Tauri 2、Rust(serde/anyhow)、vitest(node 环境纯逻辑单测)、cargo test。

**Spec:** `docs/superpowers/specs/2026-07-30-notes-markdown-editor-design.md`

## Global Constraints

- 新前端依赖只允许 `@milkdown/kit`(ProseMirror 通过 `@milkdown/kit/prose/*` 再导出使用,不单独安装 prosemirror 包)。
- 保持**常驻编辑态**:不做阅读/编辑两态切换,点击即打字(与现有 contenteditable 哲学一致)。
- 编辑器主题不引入 Milkdown 官方 theme;样式复用页面现有 class(`badge`、`ts`、`para`、`seg`、`entity-mention` 等)。注意:**Svelte scoped 样式不作用于 NodeView 生成的 DOM,编辑器内容样式必须写在 `:global(...)` 里**。
- 录音页(`src/routes/record/+page.svelte`)、导出、MCP、钩子外发路径不动;唯一例外是 `render_refined` 对空 speaker 段落的前缀处理(Task 4)。
- 前端单测走 vitest node 环境(见 `vitest.config.ts`),**纯逻辑一律放 `editorDoc.ts`,该文件禁止 import Milkdown/DOM API**。
- `save_refined` 命令的守卫与 `rename_refined_speaker`(`src-tauri/src/lib.rs:2030`)完全同套:Aing 中拒绝、录制中拒绝(`reject_if_active`)、`validate_note_id`。
- 提交前序列化做 parse→serialize 往返校验,不稳定则按纯文本提交(spec 的"不静默改写用户内容");上报通道降级为 `console.warn`(前端无 ailog 写入命令,新开命令超出本次范围——此为对 spec 的已知偏差,记录于此)。
- 旧笔记零迁移:段文本/精修稿 text 仍是纯字符串字段,markdown 是超集演进;`RefinedDoc.revision` 用 `#[serde(default)]`,历史文档按 0 读。
- Milkdown API 以 v7 为准;**每个用到 Milkdown 的任务开工前,先对照 `node_modules/@milkdown/kit/lib/` 的 `.d.ts` 核对本计划中的 import 名与签名,漂移时机械适配,不改架构**。
- 提交信息用中文,风格与 git log 一致(`feat:`/`fix:`/`docs:` 前缀)。

---

### Task 1: 安装 @milkdown/kit 依赖

**Files:**
- Modify: `package.json`(依赖新增,由 npm 写入)

**Interfaces:**
- Consumes: 无
- Produces: `@milkdown/kit` 可被 `src/lib/editor/*` import(后续所有前端任务的前提)

- [ ] **Step 1: 安装依赖**

```bash
cd "$(git rev-parse --show-toplevel)"
npm install @milkdown/kit@^7
```

- [ ] **Step 2: 验证模块可加载**

Run: `node -e "import('@milkdown/kit/core').then(m => console.log(typeof m.Editor.make))"`
Expected: 输出 `function`

- [ ] **Step 3: 验证现有检查不回归**

Run: `npm run check && npm test`
Expected: svelte-check 0 errors;vitest 全绿(与改动前一致)

- [ ] **Step 4: Commit**

```bash
git add package.json package-lock.json
git commit -m "chore(editor): 引入 @milkdown/kit(笔记页 WYSIWYG 编辑器内核)"
```

---

### Task 2: editorDoc.ts 纯逻辑层(TDD)

**Files:**
- Create: `src/lib/editor/editorDoc.ts`
- Test: `src/lib/editor/editorDoc.test.ts`
- Modify: `src/lib/notes.ts`(类型补充:`RefinedDoc.revision`/`graph_support_mentions`、`ParagraphPayload`、`saveRefined`)

**Interfaces:**
- Consumes: `splitMentions(text, mentions)`、`RefinedDoc`、`SegmentRecord`(`src/lib/notes.ts`)
- Produces(后续任务按这些确切签名消费):
  - `type InlineRun = { text: string; entityId: string | null }`
  - `type BlockSpec = { origIndex: number; speaker: string; name: string | null; personId: string | null; startMs: number; kind: "runs" | "markdown"; runs: InlineRun[]; markdown: string }`
  - `refinedToBlocks(doc: RefinedDoc): BlockSpec[]`
  - `type EditedBlock = { origIndex: number | null; markdown: string }`
  - `type ParagraphPayload = { orig_index: number | null; text: string; dirty: boolean }`(与 notes.ts 中同名类型是同一个,editorDoc 从 notes.ts re-export)
  - `refinedSavePayload(doc: RefinedDoc, blocks: EditedBlock[], baseline: Map<number, string>): { revision: number; paragraphs: ParagraphPayload[] }`
  - `normalizeOrigIndices(indices: (number | null)[]): (number | null)[]`
  - `type SegmentCommit = { kind: "skip" } | { kind: "commit"; newText: string; roundTripOk: boolean }`
  - `segmentCommitDecision(args: { storedText: string; baselineMd: string; currentMd: string; currentPlain: string; reparsedMd: string }): SegmentCommit`
  - `type SegSkeleton = { seq: number; speaker: string | null }`
  - `sameSegmentSkeleton(a: SegSkeleton[], b: SegSkeleton[]): boolean`
  - notes.ts 新增:`saveRefined(noteId: string, revision: number, paragraphs: ParagraphPayload[]): Promise<number>`(invoke `save_refined`,返回新 revision)

- [ ] **Step 1: notes.ts 类型补充**

在 `src/lib/notes.ts` 的 `RefinedDoc` 接口(`relations?: RelationFact[];` 之后、结束花括号前)加两个字段:

```ts
  /** 用户编辑保存的乐观并发版本号;历史文档缺省 0(后端 serde default)。 */
  revision?: number;
  /** 仅供旧图谱关系保持结构完整的 mention id;不是 live mention,UI 必须过滤。 */
  graph_support_mentions?: string[];
```

在文件末尾(`speakerIdCompare` 之后)追加:

```ts
/** save_refined 载荷段落:orig_index 指向载入时 doc.paragraphs 下标,null=用户新插入块。 */
export interface ParagraphPayload {
  orig_index: number | null;
  text: string;
  dirty: boolean;
}
/** 整篇保存精修稿(WYSIWYG 编辑),revision 乐观并发,返回新 revision。 */
export const saveRefined = (noteId: string, revision: number, paragraphs: ParagraphPayload[]) =>
  invoke<number>("save_refined", { noteId, revision, paragraphs });
```

- [ ] **Step 2: 写失败测试**

创建 `src/lib/editor/editorDoc.test.ts`:

```ts
import { describe, expect, it } from "vitest";
import type { RefinedDoc } from "../notes";
import {
  normalizeOrigIndices,
  refinedSavePayload,
  refinedToBlocks,
  sameSegmentSkeleton,
  segmentCommitDecision,
} from "./editorDoc";

function doc(partial: Partial<RefinedDoc> = {}): RefinedDoc {
  return {
    schema_version: 2,
    generated_at: "2026-07-30T00:00:00Z",
    stages: { filter: "done", recluster: "done", llm: "done" },
    discarded_seqs: [],
    paragraphs: [],
    revision: 3,
    ...partial,
  };
}

describe("refinedToBlocks", () => {
  it("有 live mention 的段产出 runs,无 mention 的段产出 markdown", () => {
    const d = doc({
      paragraphs: [
        {
          speaker: "R1", start_ms: 0, end_ms: 1000, source_seqs: [1],
          text: "张三在会上发言",
          mentions: [{ id: "m1", entity: "P1", start: 0, end: 2 }],
        },
        { speaker: "R2", start_ms: 1000, end_ms: 2000, source_seqs: [2], text: "无实体段落" },
      ],
    });
    const blocks = refinedToBlocks(d);
    expect(blocks[0].kind).toBe("runs");
    expect(blocks[0].runs).toEqual([
      { text: "张三", entityId: "P1" },
      { text: "在会上发言", entityId: null },
    ]);
    expect(blocks[0].origIndex).toBe(0);
    expect(blocks[0].speaker).toBe("R1");
    expect(blocks[1].kind).toBe("markdown");
    expect(blocks[1].markdown).toBe("无实体段落");
  });

  it("graph_support_mentions 中的 mention 被过滤(不产出实体 run)", () => {
    const d = doc({
      graph_support_mentions: ["m1"],
      paragraphs: [{
        speaker: "R1", start_ms: 0, end_ms: 1, source_seqs: [1],
        text: "张三在会上发言",
        mentions: [{ id: "m1", entity: "P1", start: 0, end: 2 }],
      }],
    });
    expect(refinedToBlocks(d)[0].kind).toBe("markdown");
  });
});

describe("refinedSavePayload", () => {
  const base = doc({
    paragraphs: [
      { speaker: "R1", start_ms: 0, end_ms: 1, source_seqs: [1], text: "第一段" },
      { speaker: "R2", start_ms: 1, end_ms: 2, source_seqs: [2], text: "第二段" },
    ],
  });
  const baseline = new Map([[0, "第一段"], [1, "第二段"]]);

  it("未改动的段 dirty=false,文本取当前序列化结果", () => {
    const p = refinedSavePayload(base, [
      { origIndex: 0, markdown: "第一段" },
      { origIndex: 1, markdown: "第二段" },
    ], baseline);
    expect(p.revision).toBe(3);
    expect(p.paragraphs).toEqual([
      { orig_index: 0, text: "第一段", dirty: false },
      { orig_index: 1, text: "第二段", dirty: false },
    ]);
  });

  it("文本变化的段与新插入块 dirty=true;空白块被丢弃", () => {
    const p = refinedSavePayload(base, [
      { origIndex: 0, markdown: "改过的第一段" },
      { origIndex: null, markdown: "## 新标题" },
      { origIndex: null, markdown: "   " },
      { origIndex: 1, markdown: "第二段" },
    ], baseline);
    expect(p.paragraphs).toEqual([
      { orig_index: 0, text: "改过的第一段", dirty: true },
      { orig_index: null, text: "## 新标题", dirty: true },
      { orig_index: 1, text: "第二段", dirty: false },
    ]);
  });

  it("revision 缺省按 0", () => {
    const d = doc({ paragraphs: [] });
    delete (d as Record<string, unknown>).revision;
    expect(refinedSavePayload(d, [], new Map()).revision).toBe(0);
  });
});

describe("normalizeOrigIndices", () => {
  it("重复 origIndex 保首个,其余置 null;null 与唯一值原样保留", () => {
    expect(normalizeOrigIndices([0, 0, 1, null, 1])).toEqual([0, null, 1, null, null]);
  });
});

describe("segmentCommitDecision", () => {
  const args = {
    storedText: "原文",
    baselineMd: "原文",
    currentMd: "改后",
    currentPlain: "改后",
    reparsedMd: "改后",
  };
  it("未变或纯空白 → skip(空文本走显式删除按钮,不隐式删段)", () => {
    expect(segmentCommitDecision({ ...args, currentMd: "原文", currentPlain: "原文" })).toEqual({ kind: "skip" });
    expect(segmentCommitDecision({ ...args, currentMd: "", currentPlain: "  " })).toEqual({ kind: "skip" });
  });
  it("变化且往返稳定 → 按 markdown 提交", () => {
    expect(segmentCommitDecision(args)).toEqual({ kind: "commit", newText: "改后", roundTripOk: true });
  });
  it("往返不稳定 → 按纯文本提交(不静默改写);纯文本也没变则 skip", () => {
    expect(
      segmentCommitDecision({ ...args, currentMd: "改后*", reparsedMd: "改后\\*", currentPlain: "改后*" }),
    ).toEqual({ kind: "commit", newText: "改后*", roundTripOk: false });
    expect(
      segmentCommitDecision({ ...args, currentMd: "原文*", reparsedMd: "原文\\*", currentPlain: "原文", storedText: "原文" }),
    ).toEqual({ kind: "skip" });
  });
});

describe("sameSegmentSkeleton", () => {
  it("段数、seq、speaker 全同才为真", () => {
    const a = [{ seq: 1, speaker: "S1" }, { seq: 2, speaker: null }];
    expect(sameSegmentSkeleton(a, [{ seq: 1, speaker: "S1" }, { seq: 2, speaker: null }])).toBe(true);
    expect(sameSegmentSkeleton(a, [{ seq: 1, speaker: "S1" }])).toBe(false);
    expect(sameSegmentSkeleton(a, [{ seq: 1, speaker: "S1" }, { seq: 3, speaker: null }])).toBe(false);
    expect(sameSegmentSkeleton(a, [{ seq: 1, speaker: "S2" }, { seq: 2, speaker: null }])).toBe(false);
  });
});
```

- [ ] **Step 3: 运行确认失败**

Run: `npx vitest run src/lib/editor/editorDoc.test.ts`
Expected: FAIL,报错 `Cannot find module './editorDoc'`(或找不到导出)

- [ ] **Step 4: 实现 editorDoc.ts**

创建 `src/lib/editor/editorDoc.ts`:

```ts
// 编辑器纯逻辑层:文档构建/保存载荷/提交决策。刻意不 import Milkdown/DOM,
// vitest node 环境可测;schema 与壳组件(有副作用)在同目录其他文件。
import { splitMentions, type RefinedDoc, type ParagraphPayload } from "../notes";

export type { ParagraphPayload };

export type InlineRun = { text: string; entityId: string | null };

/** 精修稿一个顶层块的构建说明。kind="runs":纯文本 + 实体标注(有 live mention 的
    干净段,文本按字面载入,不做 markdown 解析——mention 偏移只对字面文本有效);
    kind="markdown":文本按行内 markdown 解析(编辑过的段/无 mention 段)。 */
export type BlockSpec = {
  origIndex: number;
  speaker: string;
  name: string | null;
  personId: string | null;
  startMs: number;
  kind: "runs" | "markdown";
  runs: InlineRun[];
  markdown: string;
};

export function refinedToBlocks(doc: RefinedDoc): BlockSpec[] {
  const support = new Set(doc.graph_support_mentions ?? []);
  return doc.paragraphs.map((p, i) => {
    const live = (p.mentions ?? []).filter((m) => !m.id || !support.has(m.id));
    const base = {
      origIndex: i,
      speaker: p.speaker,
      name: p.name ?? null,
      personId: p.person_id ?? null,
      startMs: p.start_ms,
    };
    return live.length > 0
      ? { ...base, kind: "runs" as const, runs: splitMentions(p.text, live), markdown: p.text }
      : { ...base, kind: "markdown" as const, runs: [], markdown: p.text };
  });
}

export type EditedBlock = { origIndex: number | null; markdown: string };

/** 整篇保存载荷。dirty 判定基线:载入时同一块的序列化结果(baseline),没有基线
    (理论上不发生)退回存储原文;空白块直接丢弃(后端也会拒空,双保险)。 */
export function refinedSavePayload(
  doc: RefinedDoc,
  blocks: EditedBlock[],
  baseline: Map<number, string>,
): { revision: number; paragraphs: ParagraphPayload[] } {
  const paragraphs: ParagraphPayload[] = [];
  for (const b of blocks) {
    const text = b.markdown.trim();
    if (!text) continue;
    if (b.origIndex === null) {
      paragraphs.push({ orig_index: null, text, dirty: true });
    } else {
      const base = baseline.get(b.origIndex) ?? doc.paragraphs[b.origIndex]?.text ?? "";
      paragraphs.push({ orig_index: b.origIndex, text, dirty: text !== base.trim() });
    }
  }
  return { revision: doc.revision ?? 0, paragraphs };
}

/** Enter 分段/复制粘贴会复制块属性 → 同一 origIndex 出现多次。保首个,其余视为
    用户新插入块(origIndex=null,调用方同时清 speaker 属性)。 */
export function normalizeOrigIndices(indices: (number | null)[]): (number | null)[] {
  const seen = new Set<number>();
  return indices.map((i) => {
    if (i === null || seen.has(i)) return null;
    seen.add(i);
    return i;
  });
}

export type SegmentCommit =
  | { kind: "skip" }
  | { kind: "commit"; newText: string; roundTripOk: boolean };

/** 段落失焦提交决策。往返不稳定(serialize(parse(md)) ≠ md)时按纯文本提交,
    避免编辑器序列化 bug 静默改写用户内容(spec 错误处理节)。 */
export function segmentCommitDecision(args: {
  storedText: string;
  baselineMd: string;
  currentMd: string;
  currentPlain: string;
  reparsedMd: string;
}): SegmentCommit {
  const cur = args.currentMd.trim();
  const plain = args.currentPlain.trim();
  if (!plain) return { kind: "skip" };
  if (cur === args.baselineMd.trim()) return { kind: "skip" };
  if (args.reparsedMd.trim() !== cur) {
    if (plain === args.storedText.trim()) return { kind: "skip" };
    return { kind: "commit", newText: plain, roundTripOk: false };
  }
  return { kind: "commit", newText: cur, roundTripOk: true };
}

export type SegSkeleton = { seq: number; speaker: string | null };

/** 原始稿结构锁定判据:任何改变段数/顺序/seq/speaker 的事务都被拒绝
    (增删段与改说话人只走命令按钮,不走键盘)。 */
export function sameSegmentSkeleton(a: SegSkeleton[], b: SegSkeleton[]): boolean {
  return a.length === b.length && a.every((x, i) => x.seq === b[i].seq && x.speaker === b[i].speaker);
}
```

- [ ] **Step 5: 运行确认通过**

Run: `npx vitest run src/lib/editor/editorDoc.test.ts && npm run check`
Expected: 测试全 PASS;svelte-check 0 errors

- [ ] **Step 6: Commit**

```bash
git add src/lib/editor/editorDoc.ts src/lib/editor/editorDoc.test.ts src/lib/notes.ts
git commit -m "feat(editor): 编辑器纯逻辑层(块构建/保存载荷/提交决策)与 saveRefined 前端接口"
```

---

### Task 3: Rust:RefinedDoc.revision + save_refined_paragraphs(TDD)

**Files:**
- Modify: `src-tauri/src/store/refined.rs`(struct + `update_refined` + `write_refined_atomic` + 新函数 + tests)
- Modify: `src-tauri/src/store/mod.rs`(re-export)
- Modify: `RefinedDoc {` 字面量构造点(grep 所得,约 15 处,含各测试文件与 `src-tauri/src/refine/mod.rs:285`)

**Interfaces:**
- Consumes: `update_refined`、`load_aing_file`、`write_refined_atomic_locked`、`NoteLock`(均已存在于 refined.rs)
- Produces:
  - `RefinedDoc.revision: u64`(`#[serde(default)]`)
  - `pub struct ParagraphPayload { pub orig_index: Option<usize>, pub text: String, pub dirty: bool }`(derive `Debug, Clone, Deserialize`)
  - `pub fn save_refined_paragraphs(note_dir: &Path, expected_revision: u64, payload: &[ParagraphPayload]) -> anyhow::Result<u64>`(返回新 revision)
  - 语义保证:`update_refined` 每次成功落盘 revision+1;`write_refined_atomic`(管线整写)revision 永不回退

- [ ] **Step 1: 加字段并修复全部构造点**

`src-tauri/src/store/refined.rs` 的 `RefinedDoc` struct(`pub paragraphs: Vec<RefinedParagraph>,` 之前)加:

```rust
    /// 用户编辑保存的乐观并发版本号:每次锁内编辑落盘 +1,管线整写永不回退
    /// (见 write_refined_atomic)。历史文档缺省 0。
    #[serde(default)]
    pub revision: u64,
```

然后修复所有字面量构造点:

Run: `grep -rn "RefinedDoc {" src-tauri/src --include="*.rs"`

对每一处 `RefinedDoc { ... }` 字面量补 `revision: 0,`(测试 fixture 与管线新建文档一律 0;管线旧值进位由 Step 4 的 `write_refined_atomic` 统一负责,构造点不做特殊处理)。已知构造点:`graph/e2e_tests.rs:29`、`graph/mod.rs:605`、`graph/canonical.rs:1906,1981,2018`、`mcp/tools.rs:589,691,787,875`、`refine/mod.rs:285,737,757,832,1524,1562`(以 grep 实际输出为准)。

Run: `cargo check --manifest-path src-tauri/Cargo.toml`
Expected: 编译通过

- [ ] **Step 2: 写失败测试**

在 `src-tauri/src/store/refined.rs` 的 `mod tests`(文件尾部,812 行起)内追加。fixture 用 serde_json 反序列化构造,避免与 aing_graph 结构体字段耦合:

```rust
    fn editable_doc() -> RefinedDoc {
        serde_json::from_value(serde_json::json!({
            "schema_version": 2,
            "generated_at": "2026-07-30T00:00:00Z",
            "stages": { "filter": "done", "recluster": "done", "llm": "done", "entities": "done", "relations": "done" },
            "discarded_seqs": [],
            "revision": 3,
            "entities": [{ "id": "P1", "kind": "person", "name": "张三" }],
            "relations": [{
                "id": "rel1",
                "subject": "P1",
                "predicate": { "type": "mentions" },
                "object": "P1",
                "subject_mentions": ["m1"],
                "object_mentions": ["m1"],
                "confidence": 0.9,
                "evidence": [{ "id": "ev1", "paragraph_index": 0, "start": 0, "end": 2, "quote": "张三",
                               "source_seqs": [1], "source_hash": "h" }]
            }],
            "paragraphs": [
                { "speaker": "R1", "start_ms": 0, "end_ms": 1000, "text": "张三在发言", "source_seqs": [1],
                  "mentions": [{ "id": "m1", "entity": "P1", "start": 0, "end": 2 }] },
                { "speaker": "R2", "start_ms": 1000, "end_ms": 2000, "text": "第二段", "source_seqs": [2] }
            ]
        }))
        .expect("fixture 反序列化失败")
    }

    fn payload(items: &[(Option<usize>, &str, bool)]) -> Vec<ParagraphPayload> {
        items
            .iter()
            .map(|(i, t, d)| ParagraphPayload { orig_index: *i, text: t.to_string(), dirty: *d })
            .collect()
    }

    #[test]
    fn save_refined_rejects_revision_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let note = dir.path().join("n1");
        std::fs::create_dir_all(&note).unwrap();
        write_refined_atomic(&note, &editable_doc()).unwrap();
        let err = save_refined_paragraphs(&note, 999, &payload(&[(Some(0), "x", true)])).unwrap_err();
        assert!(err.to_string().contains("revision"), "错误应指明版本冲突: {err}");
    }

    #[test]
    fn save_refined_replaces_texts_and_bumps_revision() {
        let dir = tempfile::tempdir().unwrap();
        let note = dir.path().join("n1");
        std::fs::create_dir_all(&note).unwrap();
        write_refined_atomic(&note, &editable_doc()).unwrap();
        let rev = load_refined(&note).unwrap().revision; // 整写进位后的实际值
        let new_rev = save_refined_paragraphs(
            &note,
            rev,
            &payload(&[(Some(0), "张三在发言", false), (Some(1), "改过的第二段", true)]),
        )
        .unwrap();
        assert_eq!(new_rev, rev + 1);
        let doc = load_refined(&note).unwrap();
        assert_eq!(doc.revision, new_rev);
        assert_eq!(doc.paragraphs[1].text, "改过的第二段");
        // 干净段保留 speaker/时间戳/mentions
        assert_eq!(doc.paragraphs[0].speaker, "R1");
        assert_eq!(doc.paragraphs[0].mentions.len(), 1);
        assert!(doc.graph_support_mentions.is_empty());
    }

    #[test]
    fn save_refined_dirty_paragraph_moves_mentions_to_support() {
        let dir = tempfile::tempdir().unwrap();
        let note = dir.path().join("n1");
        std::fs::create_dir_all(&note).unwrap();
        write_refined_atomic(&note, &editable_doc()).unwrap();
        let rev = load_refined(&note).unwrap().revision;
        save_refined_paragraphs(&note, rev, &payload(&[(Some(0), "改写了第一段", true), (Some(1), "第二段", false)]))
            .unwrap();
        let doc = load_refined(&note).unwrap();
        // mention 仍在段上(图谱结构完整),但 id 进了 support 列表(UI/搜索不再当 live)
        assert_eq!(doc.paragraphs[0].mentions.len(), 1);
        assert!(doc.graph_support_mentions.contains(&"m1".to_string()));
        // 证据偏移随脏段失效,但关系端点仍有 mention 支撑,关系保留
        assert!(doc.relations[0].evidence.is_empty());
        assert_eq!(doc.relations.len(), 1);
    }

    #[test]
    fn save_refined_removed_paragraph_prunes_relations() {
        let dir = tempfile::tempdir().unwrap();
        let note = dir.path().join("n1");
        std::fs::create_dir_all(&note).unwrap();
        write_refined_atomic(&note, &editable_doc()).unwrap();
        let rev = load_refined(&note).unwrap().revision;
        // 只保留第二段:第一段(含 m1)被删 → 引用 m1 的关系整条剪掉
        save_refined_paragraphs(&note, rev, &payload(&[(Some(1), "第二段", false)])).unwrap();
        let doc = load_refined(&note).unwrap();
        assert_eq!(doc.paragraphs.len(), 1);
        assert!(doc.relations.is_empty());
        assert!(doc.graph_support_mentions.is_empty());
    }

    #[test]
    fn save_refined_new_block_gets_empty_speaker() {
        let dir = tempfile::tempdir().unwrap();
        let note = dir.path().join("n1");
        std::fs::create_dir_all(&note).unwrap();
        write_refined_atomic(&note, &editable_doc()).unwrap();
        let rev = load_refined(&note).unwrap().revision;
        save_refined_paragraphs(
            &note,
            rev,
            &payload(&[(None, "## 会议纪要", true), (Some(0), "张三在发言", false), (Some(1), "第二段", false)]),
        )
        .unwrap();
        let doc = load_refined(&note).unwrap();
        assert_eq!(doc.paragraphs.len(), 3);
        assert_eq!(doc.paragraphs[0].speaker, "");
        assert_eq!(doc.paragraphs[0].text, "## 会议纪要");
        assert!(doc.paragraphs[0].source_seqs.is_empty());
        // 证据 paragraph_index 随原第 0 段后移一位
        assert_eq!(doc.relations[0].evidence[0].paragraph_index, 1);
    }

    #[test]
    fn save_refined_rejects_empty_text_and_bad_index() {
        let dir = tempfile::tempdir().unwrap();
        let note = dir.path().join("n1");
        std::fs::create_dir_all(&note).unwrap();
        write_refined_atomic(&note, &editable_doc()).unwrap();
        let rev = load_refined(&note).unwrap().revision;
        assert!(save_refined_paragraphs(&note, rev, &payload(&[(Some(0), "  ", true)])).is_err());
        assert!(save_refined_paragraphs(&note, rev, &payload(&[(Some(9), "x", true)])).is_err());
        assert!(save_refined_paragraphs(&note, rev, &payload(&[(Some(0), "a", true), (Some(0), "b", true)])).is_err());
    }

    #[test]
    fn write_refined_atomic_never_regresses_revision() {
        let dir = tempfile::tempdir().unwrap();
        let note = dir.path().join("n1");
        std::fs::create_dir_all(&note).unwrap();
        let mut doc = editable_doc();
        doc.revision = 5;
        write_refined_atomic(&note, &doc).unwrap();
        // 管线重跑拿着旧内存 doc(revision 0)整写 → 落盘必须进位到 6,而不是回到 0
        let mut stale = editable_doc();
        stale.revision = 0;
        write_refined_atomic(&note, &stale).unwrap();
        assert_eq!(load_refined(&note).unwrap().revision, 6);
    }
```

注意:若 `mod tests` 尚未 `use` `tempfile`/`serde_json`,沿用该模块现有 use 风格补上(`tempfile` 已是 dev-dependency,现有测试在用)。

- [ ] **Step 3: 运行确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml store::refined 2>&1 | tail -20`
Expected: 编译错误 `cannot find function save_refined_paragraphs`(或找不到 `ParagraphPayload`)

- [ ] **Step 4: 实现**

`src-tauri/src/store/refined.rs`:

4a. `update_refined` 的落盘前加统一进位(替换 `f(&mut doc)?;` 与写盘之间):

```rust
    f(&mut doc)?;
    // 任何锁内编辑落盘都推进 revision:所有基于旧 revision 的未保存编辑器会话随之
    // 失效,防止笔记页保存悄悄盖掉改名/Agent 修订等其他 writer 的成果。
    doc.revision = doc.revision.saturating_add(1);
    write_refined_atomic_locked(note_dir, &doc, &note_lock)
```

4b. `write_refined_atomic` 改为永不回退 revision(整个函数体替换):

```rust
pub fn write_refined_atomic(note_dir: &Path, doc: &RefinedDoc) -> anyhow::Result<()> {
    let lock = NoteLock::acquire(note_dir)?
        .ok_or_else(|| anyhow::anyhow!("笔记正在被另一进程修改，请稍后重试"))?;
    // 管线拿内存旧 doc 整写时不得把用户已推进的 revision 拉回去:一律进位,
    // 让所有基于旧盘面的编辑器会话冲突失效,而不是被静默覆盖。
    let mut doc = doc.clone();
    if let Some(Some(existing)) = load_aing_file(note_dir) {
        if existing.revision >= doc.revision {
            doc.revision = existing.revision.saturating_add(1);
        }
    }
    write_refined_atomic_locked(note_dir, &doc, &lock)
}
```

4c. 在 `apply_refined_texts` 之后追加:

```rust
/// save_refined 载荷段落:orig_index 指向保存基线 doc.paragraphs 的下标(None=用户
/// 新插入块);dirty=文本相对载入基线有变(mention 偏移随之失效)。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ParagraphPayload {
    pub orig_index: Option<usize>,
    pub text: String,
    pub dirty: bool,
}

/// 笔记页 WYSIWYG 整篇保存。与 apply_refined_texts(Agent 只改文本)不同,这里允许
/// 增删段与插入无说话人块,因此要自己维护图谱一致性:
/// - 干净段:speaker/时间戳/source_seqs/mentions 原样保留,仅替换 text;
/// - 脏段:保留段但 mention 偏移失效 → mention id 移入 graph_support_mentions
///   (mention 本体留在段上,图谱关系端点不悬空;UI/搜索按 support 过滤);
/// - 被删段:mentions 随段消失 → 引用这些 mention 的关系整条剪掉;
/// - 证据:paragraph_index 按新布局重定位,落在被删/脏段上的证据丢弃(偏移无效);
/// - 新块:空 speaker + 零时间戳 + 空 source_seqs(导出侧对空 speaker 不加前缀)。
/// revision 乐观并发:不匹配即拒绝;成功后经 update_refined 统一 +1,返回新值。
pub fn save_refined_paragraphs(
    note_dir: &Path,
    expected_revision: u64,
    payload: &[ParagraphPayload],
) -> anyhow::Result<u64> {
    update_refined(note_dir, |doc| {
        anyhow::ensure!(
            doc.revision == expected_revision,
            "修订稿已在别处更新(盘上 revision {} ≠ 期望 {})",
            doc.revision,
            expected_revision
        );
        let old = std::mem::take(&mut doc.paragraphs);
        // old 下标 → (新下标, 是否脏);None = 该段被删
        let mut index_map: Vec<Option<(usize, bool)>> = vec![None; old.len()];
        let mut new_paras = Vec::with_capacity(payload.len());
        for (new_i, p) in payload.iter().enumerate() {
            anyhow::ensure!(!p.text.trim().is_empty(), "第 {new_i} 段文本为空");
            match p.orig_index {
                Some(i) => {
                    anyhow::ensure!(i < old.len(), "orig_index 越界: {i}(共 {} 段)", old.len());
                    anyhow::ensure!(index_map[i].is_none(), "orig_index 重复: {i}");
                    index_map[i] = Some((new_i, p.dirty));
                    let mut para = old[i].clone();
                    para.text = p.text.clone();
                    new_paras.push(para);
                }
                None => new_paras.push(RefinedParagraph {
                    speaker: String::new(),
                    name: None,
                    person_id: None,
                    start_ms: 0,
                    end_ms: 0,
                    text: p.text.clone(),
                    source_seqs: Vec::new(),
                    mentions: Vec::new(),
                }),
            }
        }
        doc.paragraphs = new_paras;

        // 脏段 mention 降级为 support-only
        for (old_i, slot) in index_map.iter().enumerate() {
            let Some((new_i, true)) = slot else { continue };
            for m in &doc.paragraphs[*new_i].mentions {
                if !m.id.is_empty() && !doc.graph_support_mentions.contains(&m.id) {
                    doc.graph_support_mentions.push(m.id.clone());
                }
            }
            let _ = old_i;
        }
        // 被删段的 mention 彻底消失:剪掉引用它们的关系与 support 残留
        let alive: std::collections::HashSet<&str> = doc
            .paragraphs
            .iter()
            .flat_map(|p| p.mentions.iter().map(|m| m.id.as_str()))
            .collect();
        doc.relations.retain(|r| {
            r.subject_mentions
                .iter()
                .chain(r.object_mentions.iter())
                .all(|id| alive.contains(id.as_str()))
        });
        doc.graph_support_mentions.retain(|id| alive.contains(id.as_str()));
        // 证据重定位:落在被删/脏段上的丢弃,其余 paragraph_index 重映射
        for rel in doc.relations.iter_mut() {
            rel.evidence.retain_mut(|ev| match index_map.get(ev.paragraph_index).copied().flatten() {
                Some((new_i, false)) => {
                    ev.paragraph_index = new_i;
                    true
                }
                _ => false,
            });
        }
        Ok(())
    })?;
    Ok(expected_revision.saturating_add(1))
}
```

注意:`RelationFact`/`RelationEvidence` 字段名(`subject_mentions`/`object_mentions`/`evidence`/`paragraph_index`)以 `src-tauri/src/store/aing_graph.rs` 实际定义为准;`retain_mut` 需要 `evidence` 元素可变,若类型不匹配按编译器提示微调。

4d. `src-tauri/src/store/mod.rs`:在 refined 相关 re-export 处(现有 `rename_refined_speaker` 等的同一位置)补 `save_refined_paragraphs` 与 `ParagraphPayload`(若该文件是 `pub use refined::*;` 则无需改动)。

- [ ] **Step 5: 运行确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml store::refined`
Expected: 新增 7 个测试全 PASS

- [ ] **Step 6: 全量回归(update_refined 进位可能影响既有断言)**

Run: `cargo test --manifest-path src-tauri/Cargo.toml 2>&1 | tail -5`
Expected: 全 PASS。若有既有测试对整份 doc 或 revision 做了相等断言而失败,把断言更新为进位后的期望值(行为变化是本任务的目的,不是回归)。

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src
git commit -m "feat(store): 精修稿 revision 乐观并发与 save_refined_paragraphs 整篇保存"
```

---

### Task 4: Rust:导出对空 speaker 段落不加说话人前缀(TDD)

**Files:**
- Modify: `src-tauri/src/store/export.rs:88-117`(`render_refined`)
- Test: 同文件 `mod tests`

**Interfaces:**
- Consumes: `RefinedDoc`(Task 3 之后含 revision)
- Produces: `render_refined` 对 `speaker=="" && name==None && person_id==None` 的段落只输出正文(用户插入的标题/列表块导出后是纯 markdown,不带 `**** [00:00:00]` 垃圾前缀)

- [ ] **Step 1: 写失败测试**

在 `src-tauri/src/store/export.rs` 的 `mod tests` 内追加(fixture 风格沿用该模块现有测试;若无现成 RefinedDoc fixture,用 serde_json::json! 反序列化构造,参照 Task 3):

```rust
    #[test]
    fn render_refined_skips_prefix_for_speakerless_blocks() {
        let doc: RefinedDoc = serde_json::from_value(serde_json::json!({
            "schema_version": 2,
            "generated_at": "2026-07-30T00:00:00Z",
            "stages": { "filter": "done", "recluster": "done", "llm": "done" },
            "discarded_seqs": [],
            "paragraphs": [
                { "speaker": "", "start_ms": 0, "end_ms": 0, "text": "## 会议纪要", "source_seqs": [] },
                { "speaker": "R1", "start_ms": 0, "end_ms": 1000, "text": "正文", "source_seqs": [1] }
            ]
        }))
        .unwrap();
        let md = render_refined("标题", &doc, true);
        assert!(md.contains("## 会议纪要\n\n"), "无说话人块只出正文: {md}");
        assert!(!md.contains("****"), "不得出现空名加粗前缀: {md}");
        assert!(md.contains("**说话人 1** `[00:00:00]`"), "有说话人的段保持原格式: {md}");
        let txt = render_refined("标题", &doc, false);
        assert!(txt.contains("\n## 会议纪要\n"), "txt 同样跳过前缀: {txt}");
    }
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml store::export::tests::render_refined_skips_prefix_for_speakerless_blocks`
Expected: FAIL(当前实现对空 speaker 输出 `**** \`[00:00:00]\`` 前缀)

- [ ] **Step 3: 实现**

`render_refined` 的段落循环体改为(整段替换 `for p in &doc.paragraphs { ... }`):

```rust
    for p in &doc.paragraphs {
        // 用户在笔记页插入的自由 markdown 块(空 speaker、无关联人物):只出正文。
        let speakerless =
            p.speaker.is_empty() && p.name.as_deref().unwrap_or("").is_empty() && p.person_id.is_none();
        if speakerless {
            out.push_str(&format!("{}\n\n", p.text));
            continue;
        }
        let label = p
            .name
            .clone()
            .filter(|n| !n.is_empty())
            .or_else(|| {
                p.person_id
                    .as_ref()
                    .map(|pid| format!("说话人 {}", pid.trim_start_matches('P')))
            })
            .unwrap_or_else(|| match p.speaker.strip_prefix('R') {
                Some(n) if n.chars().all(|c| c.is_ascii_digit()) => format!("说话人 {n}"),
                _ => p.speaker.clone(),
            });
        let ts = format_ts(p.start_ms);
        if md {
            out.push_str(&format!("**{label}** `[{ts}]`\n\n{}\n\n", p.text));
        } else {
            out.push_str(&format!("{label} [{ts}]\n{}\n\n", p.text));
        }
    }
```

- [ ] **Step 4: 运行确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml store::export`
Expected: 全 PASS

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/store/export.rs
git commit -m "fix(export): 精修稿导出对无说话人块跳过前缀(用户插入的自由 markdown 块)"
```

---

### Task 5: Rust:save_refined 命令 + 注册

**Files:**
- Modify: `src-tauri/src/lib.rs`(命令函数放在 `assign_refined_person` 命令附近;注册进 `invoke_handler` 的 `generate_handler!` 列表,`get_refined,` 之后)

**Interfaces:**
- Consumes: `store::save_refined_paragraphs`、`store::ParagraphPayload`(Task 3);`reject_if_active`(lib.rs:2744)、`lifecycle::LifecycleHandle::is_refining`、`notes_dir`、`store::validate_note_id`(均已存在)
- Produces: Tauri 命令 `save_refined(note_id, revision, paragraphs) -> Result<u64, String>`(前端 Task 2 的 `saveRefined` 调用它)

- [ ] **Step 1: 实现命令**

在 `assign_refined_person` 命令函数之后加:

```rust
/// 笔记页 WYSIWYG 整篇保存精修稿。守卫与 rename_refined_speaker 同套:Aing 中拒绝
/// (管线随后整写会吞掉编辑),录制中拒绝;revision 乐观并发在 store 层校验。
#[tauri::command]
fn save_refined(
    app: AppHandle,
    state: State<AppState>,
    note_id: String,
    revision: u64,
    paragraphs: Vec<store::ParagraphPayload>,
) -> Result<u64, String> {
    if app.state::<lifecycle::LifecycleHandle>().is_refining(&note_id) {
        return Err("该笔记正在 Aing 中，稍后再存".into());
    }
    reject_if_active(&state, &note_id)?;
    store::validate_note_id(&note_id).map_err(|e| e.to_string())?;
    let root = notes_dir(&app).map_err(|e| e.to_string())?;
    store::save_refined_paragraphs(&root.join(&note_id), revision, &paragraphs)
        .map_err(|e| e.to_string())
}
```

- [ ] **Step 2: 注册**

`generate_handler![` 列表中 `get_refined,` 一行之后插入 `save_refined,`。

- [ ] **Step 3: 编译 + 回归**

Run: `cargo check --manifest-path src-tauri/Cargo.toml && cargo test --manifest-path src-tauri/Cargo.toml 2>&1 | tail -3`
Expected: 编译通过,测试全 PASS(命令层无单测,守卫逻辑与 rename_refined_speaker 同构,store 层已在 Task 3 覆盖)

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat(ipc): save_refined 命令(精修稿整篇保存,revision 乐观并发)"
```

---

### Task 6: 前端:refinedSchema + MarkdownEditor 壳(精修模式)

**Files:**
- Create: `src/lib/editor/refinedSchema.ts`
- Create: `src/lib/editor/MarkdownEditor.svelte`

**Interfaces:**
- Consumes: `refinedToBlocks`、`refinedSavePayload`、`normalizeOrigIndices`、`EditedBlock`、`BlockSpec`(Task 2);`@milkdown/kit`
- Produces(Task 7 按此消费):
  - `MarkdownEditor.svelte` props:`{ mode: "refined" | "segments", editable?: boolean, onSaveRefined?: (payload: { revision: number; paragraphs: ParagraphPayload[] }) => void, onBadgeClick?: (attrs: BadgeAttrs, rect: DOMRect) => void, onPlayFrom?: (startMs: number) => void, onEntityOpen?: (entityId: string) => void, onDeleteClick?: (seq: number, rect: DOMRect) => void, onEditSegment?: (seq: number, expectedText: string, newText: string) => void, speakerBadge: (attrs: BadgeAttrs) => { label: string; bg: string; ink: string } }`
  - `type BadgeAttrs = { seq?: number; origIndex?: number | null; speaker: string; name: string | null; personId: string | null; startMs: number }`(从 MarkdownEditor.svelte export)
  - 实例方法(`bind:this` 调用):`setRefined(doc: RefinedDoc): void`、`flushRefined(): void`(立即触发一次保存判定)、`markSaved(newRevision: number): void`、`hasFocus(): boolean`
  - segments 模式方法在 Task 8 补:`setSegments(...)`
- 内部约定(本任务实现,Task 8 复用):序列化辅助 `serializeBlock(ctx, node): string`(顶层块 → markdown,含往返所需的 parse/serialize);自定义块 toMarkdown 一律按 paragraph 输出行内内容

- [ ] **Step 1: refinedSchema.ts**

创建 `src/lib/editor/refinedSchema.ts`:

```ts
// 精修稿 schema:refined_paragraph 自定义块(携带说话人/时间戳/origIndex)+
// entity_mention 行内 mark(实体高亮)。markdown 无对应语法,parseMarkdown 永不
// 命中(文档只由 setRefined 程序化构建);toMarkdown 按普通段落/纯文本输出,
// 保证 serializeBlock 与保存载荷可用。
import { $markSchema, $nodeSchema, $prose } from "@milkdown/kit/utils";
import { Plugin, PluginKey } from "@milkdown/kit/prose/state";
import type { Node as PMNode } from "@milkdown/kit/prose/model";
import type { EditorView } from "@milkdown/kit/prose/view";
import { normalizeOrigIndices } from "./editorDoc";

export const refinedParagraphSchema = $nodeSchema("refined_paragraph", () => ({
  content: "inline*",
  group: "block",
  defining: true,
  attrs: {
    origIndex: { default: null },
    speaker: { default: "" },
    name: { default: null },
    personId: { default: null },
    startMs: { default: 0 },
  },
  parseDOM: [{ tag: "div[data-refined-paragraph]" }],
  toDOM: () => ["div", { "data-refined-paragraph": "", class: "md-para" }, 0],
  parseMarkdown: { match: () => false, runner: () => {} },
  toMarkdown: {
    match: (node) => node.type.name === "refined_paragraph",
    runner: (state, node) => {
      state.openNode("paragraph");
      state.next(node.content);
      state.closeNode();
    },
  },
}));

export const entityMentionSchema = $markSchema("entity_mention", () => ({
  attrs: { entityId: { default: "" } },
  inclusive: false,
  parseDOM: [
    {
      tag: "span[data-entity-id]",
      getAttrs: (dom) => ({ entityId: (dom as HTMLElement).dataset.entityId ?? "" }),
    },
  ],
  toDOM: (mark) => [
    "span",
    { "data-entity-id": mark.attrs.entityId as string, class: "entity-mention" },
  ],
  parseMarkdown: { match: () => false, runner: () => {} },
  toMarkdown: {
    // 序列化时实体标注不落盘(mentions 生命周期由后端管),文本原样透传。
    match: (mark) => mark.type.name === "entity_mention",
    runner: (state, _mark, node) => {
      state.addNode("text", undefined, node.text ?? "");
    },
  },
}));

/** Enter 分段/粘贴复制块属性 → origIndex 重复。规整:保首个,其余降级为用户新块
    (origIndex=null 且清空说话人属性)。纯判定在 editorDoc.normalizeOrigIndices。 */
export const refinedNormalizePlugin = $prose(
  () =>
    new Plugin({
      key: new PluginKey("refined-normalize"),
      appendTransaction(trs, _old, state) {
        if (!trs.some((tr) => tr.docChanged)) return null;
        const entries: { node: PMNode; pos: number; idx: number | null }[] = [];
        state.doc.forEach((node, pos) => {
          entries.push({
            node,
            pos,
            idx: node.type.name === "refined_paragraph" ? (node.attrs.origIndex as number | null) : null,
          });
        });
        const fixed = normalizeOrigIndices(entries.map((e) => e.idx));
        let tr: typeof state.tr | null = null;
        entries.forEach((e, i) => {
          if (e.node.type.name !== "refined_paragraph" || fixed[i] === e.idx) return;
          tr = (tr ?? state.tr).setNodeMarkup(e.pos, undefined, {
            ...e.node.attrs,
            origIndex: null,
            speaker: "",
            name: null,
            personId: null,
            startMs: 0,
          });
        });
        return tr;
      },
    }),
);

export type BadgeCallbacks = {
  speakerBadge: (attrs: Record<string, unknown>) => { label: string; bg: string; ink: string };
  formatTs: (ms: number) => string;
  onBadgeClick: (attrs: Record<string, unknown>, rect: DOMRect) => void;
  onPlayFrom: (startMs: number) => void;
};

/** refined_paragraph NodeView:说话人徽章 + 时间戳按钮为不可编辑前缀,正文是
    contentDOM。徽章/时间戳交互经回调抛给页面(Svelte 渲染浮层),不在 PM 里做 UI。 */
export function makeRefinedParagraphView(cb: BadgeCallbacks) {
  return (node: PMNode, _view: EditorView, _getPos: () => number | undefined) => {
    const dom = document.createElement("div");
    dom.className = "md-para";
    dom.dataset.refinedParagraph = "";
    if (node.attrs.speaker || node.attrs.name || node.attrs.personId) {
      const { label, bg, ink } = cb.speakerBadge(node.attrs);
      const badge = document.createElement("button");
      badge.type = "button";
      badge.className = "badge as-btn";
      badge.contentEditable = "false";
      badge.textContent = label;
      badge.style.background = bg;
      badge.style.color = ink;
      badge.onclick = (e) => {
        e.preventDefault();
        cb.onBadgeClick(node.attrs, badge.getBoundingClientRect());
      };
      const ts = document.createElement("button");
      ts.type = "button";
      ts.className = "ts ts-btn";
      ts.contentEditable = "false";
      ts.textContent = cb.formatTs(node.attrs.startMs as number);
      ts.title = "从此处播放";
      ts.onclick = () => cb.onPlayFrom(node.attrs.startMs as number);
      dom.append(badge, ts);
    }
    const contentDOM = document.createElement("span");
    contentDOM.className = "para-text";
    dom.append(contentDOM);
    return {
      dom,
      contentDOM,
      stopEvent: (e: Event) =>
        e.target instanceof HTMLElement && e.target.closest("button") !== null,
      ignoreMutation: (m: MutationRecord) => !contentDOM.contains(m.target),
    };
  };
}
```

- [ ] **Step 2: MarkdownEditor.svelte(精修模式)**

创建 `src/lib/editor/MarkdownEditor.svelte`:

```svelte
<script lang="ts" module>
  export type BadgeAttrs = {
    seq?: number;
    origIndex?: number | null;
    speaker: string;
    name: string | null;
    personId: string | null;
    startMs: number;
  };
</script>

<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import {
    Editor,
    defaultValueCtx,
    editorViewCtx,
    parserCtx,
    rootCtx,
    schemaCtx,
    serializerCtx,
  } from "@milkdown/kit/core";
  import type { Ctx } from "@milkdown/kit/ctx";
  import { commonmark } from "@milkdown/kit/preset/commonmark";
  import { history } from "@milkdown/kit/plugin/history";
  import { $prose } from "@milkdown/kit/utils";
  import { Plugin, PluginKey } from "@milkdown/kit/prose/state";
  import type { Node as PMNode, Fragment } from "@milkdown/kit/prose/model";
  import { formatTs, type RefinedDoc, type ParagraphPayload } from "../notes";
  import {
    refinedSavePayload,
    refinedToBlocks,
    type EditedBlock,
  } from "./editorDoc";
  import {
    entityMentionSchema,
    makeRefinedParagraphView,
    refinedNormalizePlugin,
    refinedParagraphSchema,
  } from "./refinedSchema";

  let {
    mode,
    editable = true,
    onSaveRefined,
    onBadgeClick,
    onPlayFrom,
    onEntityOpen,
    speakerBadge,
  }: {
    mode: "refined" | "segments";
    editable?: boolean;
    onSaveRefined?: (payload: { revision: number; paragraphs: ParagraphPayload[] }) => void;
    onBadgeClick?: (attrs: BadgeAttrs, rect: DOMRect) => void;
    onPlayFrom?: (startMs: number) => void;
    onEntityOpen?: (entityId: string) => void;
    speakerBadge: (attrs: BadgeAttrs) => { label: string; bg: string; ink: string };
  } = $props();

  let rootEl: HTMLDivElement;
  let editor: Editor | null = null;
  let ctxRef: Ctx | null = null;
  // 精修稿保存状态:载入 doc + 各 origIndex 的序列化基线 + 上次已保存载荷指纹
  let loadedDoc: RefinedDoc | null = null;
  let baseline = new Map<number, string>();
  let lastSaved = "";
  let idleTimer: ReturnType<typeof setTimeout> | null = null;
  const IDLE_SAVE_MS = 2000;

  /** 顶层块 → markdown。自定义块(refined_paragraph/transcript_segment)按段落
      输出行内内容;标准块直接序列化。往返兜底:结果必须 parse 回同构文本,
      失败(理论不发生)退回 textContent,绝不静默改写。 */
  function serializeBlock(ctx: Ctx, node: PMNode): string {
    const schema = ctx.get(schemaCtx);
    const serializer = ctx.get(serializerCtx);
    try {
      const doc = schema.topNodeType.createChecked(null, [node]);
      return serializer(doc).trim();
    } catch (err) {
      console.warn("serializeBlock 失败,退回纯文本", err);
      return node.textContent.trim();
    }
  }

  /** 行内 markdown → Fragment。解析出多块(理论上段内不该有)时退回字面文本。 */
  function parseInline(ctx: Ctx, text: string): Fragment | PMNode[] {
    const schema = ctx.get(schemaCtx);
    if (!text) return [];
    try {
      const parsed = ctx.get(parserCtx)(text);
      if (parsed && parsed.childCount === 1 && parsed.firstChild!.isTextblock) {
        return parsed.firstChild!.content;
      }
    } catch {
      /* 落到字面文本 */
    }
    return [schema.text(text)];
  }

  export function setRefined(doc: RefinedDoc) {
    if (!ctxRef) return;
    const ctx = ctxRef;
    loadedDoc = doc;
    baseline = new Map();
    const view = ctx.get(editorViewCtx);
    const schema = ctx.get(schemaCtx);
    const paras = refinedToBlocks(doc).map((b) => {
      const content =
        b.kind === "runs"
          ? b.runs
              .filter((r) => r.text)
              .map((r) =>
                r.entityId
                  ? schema.text(r.text, [schema.marks.entity_mention.create({ entityId: r.entityId })])
                  : schema.text(r.text),
              )
          : parseInline(ctx, b.markdown);
      return schema.nodes.refined_paragraph.createChecked(
        { origIndex: b.origIndex, speaker: b.speaker, name: b.name, personId: b.personId, startMs: b.startMs },
        content,
      );
    });
    const docNode = schema.topNodeType.createChecked(
      null,
      paras.length ? paras : [schema.nodes.paragraph.createAndFill()!],
    );
    view.dispatch(
      view.state.tr
        .replaceWith(0, view.state.doc.content.size, docNode.content)
        .setMeta("addToHistory", false)
        .setMeta("external-load", true),
    );
    paras.forEach((p) => {
      if (p.attrs.origIndex !== null) baseline.set(p.attrs.origIndex as number, serializeBlock(ctx, p));
    });
    lastSaved = JSON.stringify(refinedSavePayload(doc, collectBlocks(), baseline).paragraphs);
  }

  function collectBlocks(): EditedBlock[] {
    if (!ctxRef) return [];
    const ctx = ctxRef;
    const view = ctx.get(editorViewCtx);
    const blocks: EditedBlock[] = [];
    view.state.doc.forEach((node) => {
      blocks.push({
        origIndex: node.type.name === "refined_paragraph" ? (node.attrs.origIndex as number | null) : null,
        markdown: serializeBlock(ctx, node),
      });
    });
    return blocks;
  }

  export function flushRefined() {
    if (idleTimer) clearTimeout(idleTimer);
    idleTimer = null;
    if (!loadedDoc || !onSaveRefined) return;
    const payload = refinedSavePayload(loadedDoc, collectBlocks(), baseline);
    const fingerprint = JSON.stringify(payload.paragraphs);
    if (fingerprint === lastSaved) return;
    onSaveRefined(payload);
  }

  /** 保存成功回执:当前内容成为新基线(origIndex 重排为保存后的段序)。 */
  export function markSaved(newRevision: number) {
    if (!ctxRef || !loadedDoc) return;
    const ctx = ctxRef;
    const view = ctx.get(editorViewCtx);
    let tr = view.state.tr;
    let nextIndex = 0;
    baseline = new Map();
    view.state.doc.forEach((node, pos) => {
      const md = serializeBlock(ctx, node);
      if (!md) return;
      if (node.type.name === "refined_paragraph") {
        if (node.attrs.origIndex !== nextIndex) {
          tr = tr.setNodeMarkup(pos, undefined, { ...node.attrs, origIndex: nextIndex });
        }
        baseline.set(nextIndex, md);
      }
      nextIndex += 1;
    });
    tr = tr.setMeta("addToHistory", false).setMeta("external-load", true);
    if (tr.docChanged || tr.steps.length > 0) view.dispatch(tr);
    loadedDoc = { ...loadedDoc, revision: newRevision };
    lastSaved = JSON.stringify(refinedSavePayload(loadedDoc, collectBlocks(), baseline).paragraphs);
  }

  export function hasFocus(): boolean {
    if (!ctxRef) return false;
    return ctxRef.get(editorViewCtx).hasFocus();
  }

  function scheduleIdleSave() {
    if (idleTimer) clearTimeout(idleTimer);
    idleTimer = setTimeout(() => flushRefined(), IDLE_SAVE_MS);
  }

  const uiPlugins = $prose(
    () =>
      new Plugin({
        key: new PluginKey("md-editor-ui"),
        props: {
          editable: () => editable,
          nodeViews: {
            refined_paragraph: makeRefinedParagraphView({
              speakerBadge: (attrs) => speakerBadge(attrs as BadgeAttrs),
              formatTs,
              onBadgeClick: (attrs, rect) => onBadgeClick?.(attrs as BadgeAttrs, rect),
              onPlayFrom: (ms) => onPlayFrom?.(ms),
            }),
          },
          handleDOMEvents: {
            // 实体高亮:悬浮弹层的打开走页面回调(点击进编辑光标,不抢手势)
            mouseover: (_view, event) => {
              const t = event.target as HTMLElement;
              const span = t.closest?.("span[data-entity-id]") as HTMLElement | null;
              if (span) rootEl.dispatchEvent(new CustomEvent("entityhover", { detail: { entityId: span.dataset.entityId, rect: span.getBoundingClientRect() } }));
              return false;
            },
            focusout: () => {
              if (mode === "refined") flushRefined();
              return false;
            },
          },
        },
        view: () => ({
          update: (view, prev) => {
            if (mode === "refined" && !view.state.doc.eq(prev.doc)) scheduleIdleSave();
          },
        }),
      }),
  );

  onMount(async () => {
    editor = await Editor.make()
      .config((ctx) => {
        ctx.set(rootCtx, rootEl);
        ctx.set(defaultValueCtx, "");
      })
      .use(commonmark)
      .use(history)
      .use(refinedParagraphSchema)
      .use(entityMentionSchema)
      .use(refinedNormalizePlugin)
      .use(uiPlugins)
      .create();
    editor.action((ctx) => {
      ctxRef = ctx;
    });
    rootEl.addEventListener("entityhover", ((e: CustomEvent) => {
      // 页面若关心实体悬浮,经 onEntityOpen 之外的浮层处理;此处只转发打开回调的数据源
      void e;
    }) as EventListener);
  });

  onDestroy(() => {
    if (idleTimer) clearTimeout(idleTimer);
    editor?.destroy();
  });
</script>

<div class="md-editor" bind:this={rootEl} data-mode={mode}></div>

<style>
  .md-editor :global(.ProseMirror) {
    outline: none;
    white-space: pre-wrap;
  }
  .md-editor :global(.md-para) {
    margin: 0 0 6px;
  }
  .md-editor :global(.md-para .para-text) {
    display: inline;
  }
</style>
```

说明(执行者须知):
- 徽章/时间戳/实体样式复用页面 `.transcript` 下的既有 class;Task 7 会把页面样式改成 `:global` 选择器,本组件只带最小结构样式。
- 实体悬浮浮层由 Task 7 在页面层实现(监听编辑器容器上的 `entityhover` CustomEvent,渲染含"打开知识图谱"按钮的浮层,按钮回调 `onEntityOpen`)。本任务先把事件派发通道铺好。
- Milkdown `$nodeSchema`/`$markSchema` 返回值是数组式插件切片,`.use(...)` 直接可用;若 `.d.ts` 显示需要 `.use(refinedParagraphSchema)` 拆开(如 `[schema.ctx, schema.node]`),按类型提示适配。

- [ ] **Step 3: 类型检查**

Run: `npm run check`
Expected: 0 errors(允许 unused export 类 warning)

- [ ] **Step 4: Commit**

```bash
git add src/lib/editor/refinedSchema.ts src/lib/editor/MarkdownEditor.svelte
git commit -m "feat(editor): Milkdown 编辑器壳与精修稿 schema(段属性块+实体 mark+规整插件)"
```

---

### Task 7: 笔记页精修稿接线(可编辑 + 自动保存 + 冲突处理)

**Files:**
- Modify: `src/routes/notes/[id]/+page.svelte`
  - 精修稿渲染分支(`{#if effectiveView === "refined" && refined}` 内 `{#each refined.paragraphs ...}` 整段,约 757-785 行)→ MarkdownEditor
  - script 区新增保存/冲突/实体浮层逻辑
  - style 区:`.transcript` 内与徽章/正文/实体相关的规则改为 `:global` 形式使 NodeView DOM 命中

**Interfaces:**
- Consumes: `MarkdownEditor.svelte`(`setRefined`/`flushRefined`/`markSaved`/`hasFocus`、props)、`saveRefined`(Task 2)、页面既有 `speakerLabel/speakerColor/speakerInk/refinedSpeakers/playFrom/gotoEntity/entityLinks/entityName/getRefined`
- Produces: 精修稿视图 = 可编辑 WYSIWYG;失焦立即保存 + 停顿 2s 自动保存;revision 冲突自动重载并提示

- [ ] **Step 1: script 区新增状态与处理函数**

imports 增加:

```ts
  import MarkdownEditor, { type BadgeAttrs } from "$lib/editor/MarkdownEditor.svelte";
  import { saveRefined, type ParagraphPayload } from "$lib/notes";
```

状态与函数(放在 refined 相关状态之后):

```ts
  let refinedEditor = $state<ReturnType<typeof MarkdownEditor> | null>(null);
  let savingRefined = $state(false);
  // 实体悬浮浮层:{ entityId, rect };离开/点击其他区域即收起
  let entityPop = $state<{ entityId: string; rect: DOMRect } | null>(null);
  // 精修稿说话人浮层(徽章点击):沿用说话人条改名/选人入口,弹层只做跳转提示
  let refinedBadgePop = $state<{ attrs: BadgeAttrs; rect: DOMRect } | null>(null);

  function refinedBadge(attrs: BadgeAttrs): { label: string; bg: string; ink: string } {
    const sid = attrs.speaker;
    return {
      label: speakerLabel(sid, "mic", refinedSpeakers),
      bg: speakerColor(sid, "mic", refinedSpeakers),
      ink: speakerInk(sid, "mic", refinedSpeakers),
    };
  }

  async function doSaveRefined(payload: { revision: number; paragraphs: ParagraphPayload[] }) {
    if (savingRefined) return;
    savingRefined = true;
    try {
      const newRev = await saveRefined(id, payload.revision, payload.paragraphs);
      refinedEditor?.markSaved(newRev);
      if (refined) refined = { ...refined, revision: newRev };
    } catch (err) {
      error = `精修稿保存失败: ${err}`;
      // 乐观冲突/其他失败:重载盘上最新内容重建文档(与段编辑同哲学)
      try {
        refined = await getRefined(id);
        if (refined) refinedEditor?.setRefined(refined);
      } catch {
        /* 重载失败保持错误横幅 */
      }
    } finally {
      savingRefined = false;
    }
  }
```

同步编辑器内容的 effect(靠近现有 refined 加载逻辑):

```ts
  // refined 变化(载入/精修完成/冲突重载)→ 重建编辑器文档;输入中不打断
  $effect(() => {
    const doc = refined;
    const ed = refinedEditor;
    if (!ed || !doc || effectiveView !== "refined") return;
    if (ed.hasFocus()) return; // 常驻编辑态:正在打字时外部刷新不吹掉输入
    ed.setRefined(doc);
  });
```

离开页面/切换笔记前落盘(在现有 id-effect 复位处与 `onDestroy` 处调用):

```ts
  // id 切换与组件销毁前:把未保存的精修稿编辑冲出去
  refinedEditor?.flushRefined();
```

(若页面没有现成 onDestroy,新增 `import { onDestroy } from "svelte";` 并 `onDestroy(() => refinedEditor?.flushRefined());`。)

- [ ] **Step 2: 模板替换精修稿分支**

`{#if effectiveView === "refined" && refined}` 分支内,把 `{#each refined.paragraphs as p, i (i)} ... {/each}` 与空稿提示整段替换为:

```svelte
        <MarkdownEditor
          bind:this={refinedEditor}
          mode="refined"
          editable={canEdit}
          speakerBadge={refinedBadge}
          onSaveRefined={doSaveRefined}
          onBadgeClick={(attrs, rect) => (refinedBadgePop = { attrs, rect })}
          onPlayFrom={(ms) => playFrom({ start_ms: ms })}
          onEntityOpen={(eid) => gotoEntity(eid)}
        />
        {#if refined.paragraphs.length === 0}
          <p class="hint">（修订稿为空，可直接输入补充内容）</p>
        {/if}
```

编辑器容器的实体悬浮事件桥接:在 MarkdownEditor 外层包一个监听(entityhover 自编辑器根冒泡):

```svelte
        <div
          class="refined-editor-host"
          onentityhover={(e: CustomEvent) => (entityPop = e.detail)}
          onmouseleave={() => (entityPop = null)}
        >
          ...上面的 <MarkdownEditor ... /> 移入此容器...
        </div>
```

注:Svelte 5 对自定义事件属性若类型报错,退化为 `onMount` 里 `host.addEventListener("entityhover", ...)`,行为等价。

实体浮层与徽章浮层(transcript 容器之后,复用页面浮层样式风格):

```svelte
    {#if entityPop}
      <div
        class="entity-pop"
        style="position: fixed; left: {entityPop.rect.left}px; top: {entityPop.rect.bottom + 4}px;"
        onmouseleave={() => (entityPop = null)}
      >
        <span>{entityName(entityPop.entityId)}</span>
        {#if entityLinks[entityPop.entityId]}
          <button class="link" onclick={() => { gotoEntity(entityPop!.entityId); entityPop = null; }}>打开知识图谱</button>
        {/if}
      </div>
    {/if}
    {#if refinedBadgePop}
      <div
        class="entity-pop"
        style="position: fixed; left: {refinedBadgePop.rect.left}px; top: {refinedBadgePop.rect.bottom + 4}px;"
      >
        <span>{refinedBadge(refinedBadgePop.attrs).label}</span>
        <button class="link" onclick={() => (refinedBadgePop = null)}>关闭</button>
        <!-- 改名/选人沿用页面顶部说话人条(SpeakerChips);弹层只提示身份 -->
      </div>
    {/if}
```

- [ ] **Step 3: 样式 :global 化**

页面 style 区:凡是要作用于编辑器 NodeView DOM 的规则(`.badge`、`.ts`、`.ts-btn`、`.para-text`、`.entity-mention`、`.entity-mention.linkable`)复制/改写为:

```css
  .transcript :global(.md-para) { /* 原 .para 规则 */ }
  .transcript :global(.md-para .badge) { /* 原 .para .badge 规则 */ }
  .transcript :global(.md-para .ts) { /* 原 .ts 规则 */ }
  .transcript :global(.entity-mention) { /* 原 .entity-mention 规则 */ }
```

新增浮层样式:

```css
  .entity-pop {
    z-index: 30;
    display: flex;
    gap: 8px;
    align-items: center;
    padding: 6px 10px;
    background: var(--surface);
    border-radius: 8px;
    box-shadow: 0 4px 16px rgba(0, 0, 0, 0.12);
    font-size: 0.85rem;
  }
```

(具体 token 名以页面现有样式为准,保持同一视觉体系;DESIGN.md 的 editable-text 规范:focus 态 `accent` 2px outline 加在 `:global(.ProseMirror:focus-visible)` 上。)

- [ ] **Step 4: 验证**

Run: `npm run check && npm test`
Expected: 0 errors,vitest 全绿

- [ ] **Step 5: 手工冒烟(tauri dev)**

Run: `npm run tauri dev`,打开一条已精修笔记:
1. 精修稿正常渲染:说话人徽章、时间戳、实体高亮齐全;点击时间戳跳播。
2. 直接点进段落打字 → 停 2 秒或点击别处 → 无报错;重进该笔记,编辑已持久化。
3. 编辑含实体高亮的段落 → 保存后该段高亮消失(mention 失效),其他段高亮保留。
4. 敲 `## 标题` + Enter 新起一行输入 → 出现无徽章标题块;导出 md 检查该块无 `****` 前缀。
5. 悬浮实体 → 浮层出现,"打开知识图谱"可跳转。
6. 两个窗口同开一条笔记各自编辑 → 后保存的一方出现"精修稿已在别处更新/保存失败"并自动重载。
7. 对录音中的笔记(若可复现)确认精修稿编辑被拒且有报错提示。

- [ ] **Step 6: Commit**

```bash
git add src/routes/notes/[id]/+page.svelte
git commit -m "feat(notes): 精修稿改为可编辑 WYSIWYG(自动保存+revision 冲突重载+实体浮层)"
```

---

### Task 8: 前端:segmentSchema(结构锁定的原始稿模式)

**Files:**
- Create: `src/lib/editor/segmentSchema.ts`
- Modify: `src/lib/editor/MarkdownEditor.svelte`(segments 模式:setSegments、提交流程、结构锁定接入)

**Interfaces:**
- Consumes: `sameSegmentSkeleton`、`segmentCommitDecision`(Task 2);Task 6 的 `serializeBlock`/`parseInline`/uiPlugins 骨架
- Produces:
  - `segmentSchema.ts`:`transcriptSegmentSchema`($nodeSchema,attrs `{ seq, source, speaker, startMs }`)、`segmentLockPlugin`($prose filterTransaction)、`makeSegmentView(cb)`(NodeView:徽章+时间戳+删除按钮+data-seq)
  - MarkdownEditor 实例新方法:`setSegments(segments: SegmentRecord[], speakers: Note["speakers"]): void`
  - 提交流程:段失焦/Enter → `segmentCommitDecision` → 有变化时调 props `onEditSegment(seq, expectedText, newText)`;Escape → 还原该段为基线内容并失焦

- [ ] **Step 1: segmentSchema.ts**

创建 `src/lib/editor/segmentSchema.ts`:

```ts
// 原始稿 schema:transcript_segment 自定义块,一段=一节点,attrs 锁定段身份。
// 结构锁定:任何改变段骨架(段数/顺序/seq/speaker)的事务一律拒绝——增删段与
// 改说话人只走命令按钮(delete_segment/set_segment_speaker),键盘只能改段内文本。
import { $nodeSchema, $prose } from "@milkdown/kit/utils";
import { Plugin, PluginKey } from "@milkdown/kit/prose/state";
import type { Node as PMNode } from "@milkdown/kit/prose/model";
import type { EditorView } from "@milkdown/kit/prose/view";
import { sameSegmentSkeleton, type SegSkeleton } from "./editorDoc";

export const transcriptSegmentSchema = $nodeSchema("transcript_segment", () => ({
  content: "inline*",
  group: "block",
  defining: true,
  attrs: {
    seq: { default: 0 },
    source: { default: "mic" },
    speaker: { default: null },
    startMs: { default: 0 },
  },
  parseDOM: [{ tag: "div[data-seq]" }],
  toDOM: (node) => ["div", { "data-seq": String(node.attrs.seq), class: "md-seg" }, 0],
  parseMarkdown: { match: () => false, runner: () => {} },
  toMarkdown: {
    match: (node) => node.type.name === "transcript_segment",
    runner: (state, node) => {
      state.openNode("paragraph");
      state.next(node.content);
      state.closeNode();
    },
  },
}));

export function docSkeleton(doc: PMNode): SegSkeleton[] {
  const out: SegSkeleton[] = [];
  doc.forEach((n) => {
    if (n.type.name === "transcript_segment") {
      out.push({ seq: n.attrs.seq as number, speaker: n.attrs.speaker as string | null });
    } else {
      out.push({ seq: -1, speaker: null }); // 非段节点混入 = 骨架已破坏,必然拒绝
    }
  });
  return out;
}

export const segmentLockPlugin = $prose(
  () =>
    new Plugin({
      key: new PluginKey("segment-lock"),
      filterTransaction(tr, state) {
        if (!tr.docChanged || tr.getMeta("external-load")) return true;
        return sameSegmentSkeleton(docSkeleton(state.doc), docSkeleton(tr.doc));
      },
    }),
);

export type SegmentViewCallbacks = {
  speakerBadge: (attrs: Record<string, unknown>) => { label: string; bg: string; ink: string };
  formatTs: (ms: number) => string;
  canEdit: () => boolean;
  onBadgeClick: (seq: number, rect: DOMRect) => void;
  onPlayFrom: (startMs: number) => void;
  onDeleteClick: (seq: number, rect: DOMRect) => void;
};

/** 段 NodeView:徽章(点击→说话人菜单浮层)、时间戳(跳播)、正文 contentDOM、
    行尾删除按钮(点击→页面确认浮层)。data-seq 保留给播放高亮/滚动跟随定位。 */
export function makeSegmentView(cb: SegmentViewCallbacks) {
  return (node: PMNode, _view: EditorView, _getPos: () => number | undefined) => {
    const dom = document.createElement("div");
    dom.className = "md-seg";
    dom.dataset.seq = String(node.attrs.seq);
    const { label, bg, ink } = cb.speakerBadge(node.attrs);
    const badge = document.createElement("button");
    badge.type = "button";
    badge.className = "badge as-btn";
    badge.contentEditable = "false";
    badge.textContent = label;
    badge.style.background = bg;
    badge.style.color = ink;
    badge.disabled = !cb.canEdit();
    badge.title = cb.canEdit() ? "点击改说话人" : "";
    badge.onclick = (e) => {
      e.preventDefault();
      if (cb.canEdit()) cb.onBadgeClick(node.attrs.seq as number, badge.getBoundingClientRect());
    };
    const ts = document.createElement("button");
    ts.type = "button";
    ts.className = "ts ts-btn";
    ts.contentEditable = "false";
    ts.title = "从此处播放";
    ts.textContent = cb.formatTs(node.attrs.startMs as number);
    ts.onclick = () => cb.onPlayFrom(node.attrs.startMs as number);
    const content = document.createElement("span");
    content.className = "seg-text";
    const actions = document.createElement("span");
    actions.className = "seg-actions";
    actions.contentEditable = "false";
    if (cb.canEdit()) {
      const del = document.createElement("button");
      del.type = "button";
      del.className = "link";
      del.textContent = "删除";
      del.onclick = () => cb.onDeleteClick(node.attrs.seq as number, del.getBoundingClientRect());
      actions.append(del);
    }
    dom.append(badge, ts, content, actions);
    return {
      dom,
      contentDOM: content,
      stopEvent: (e: Event) =>
        e.target instanceof HTMLElement && e.target.closest("button") !== null,
      ignoreMutation: (m: MutationRecord) => !content.contains(m.target),
    };
  };
}
```

- [ ] **Step 2: MarkdownEditor 扩展 segments 模式**

`MarkdownEditor.svelte` 修改:

2a. imports 增加:

```ts
  import type { Note, SegmentRecord } from "../notes";
  import { segmentCommitDecision } from "./editorDoc";
  import { makeSegmentView, segmentLockPlugin, transcriptSegmentSchema } from "./segmentSchema";
  import { keymap } from "@milkdown/kit/prose/keymap";
```

props 增加(签名见 Task 6 Interfaces):`onEditSegment`、`onBadgeClick` 复用(segments 模式回调参数为 `(attrs, rect)`,attrs 内含 seq)、`onDeleteClick`。

2b. segments 状态与方法:

```ts
  // segments 模式基线:seq → { storedText(后端 expectedText), baselineMd(载入序列化) }
  let segBase = new Map<number, { storedText: string; baselineMd: string }>();
  let focusedSeqNo: number | null = null;

  export function setSegments(segments: SegmentRecord[], _speakers: Note["speakers"]) {
    if (!ctxRef) return;
    const ctx = ctxRef;
    const view = ctx.get(editorViewCtx);
    const schema = ctx.get(schemaCtx);
    segBase = new Map();
    const nodes = segments.map((s) =>
      schema.nodes.transcript_segment.createChecked(
        { seq: s.seq, source: s.source, speaker: s.speaker, startMs: s.start_ms },
        parseInline(ctx, s.text),
      ),
    );
    const docNode = schema.topNodeType.createChecked(
      null,
      nodes.length ? nodes : [schema.nodes.paragraph.createAndFill()!],
    );
    view.dispatch(
      view.state.tr
        .replaceWith(0, view.state.doc.content.size, docNode.content)
        .setMeta("addToHistory", false)
        .setMeta("external-load", true),
    );
    segments.forEach((s, i) => {
      segBase.set(s.seq, { storedText: s.text, baselineMd: serializeBlock(ctx, nodes[i]) });
    });
  }

  /** 焦点段变化/失焦时评估提交。往返校验:serialize→parse→serialize 必须稳定。 */
  function commitSegment(seq: number) {
    if (!ctxRef || !onEditSegment) return;
    const ctx = ctxRef;
    const view = ctx.get(editorViewCtx);
    let target: PMNode | null = null;
    view.state.doc.forEach((n) => {
      if (n.type.name === "transcript_segment" && n.attrs.seq === seq) target = n;
    });
    const base = segBase.get(seq);
    if (!target || !base) return;
    const currentMd = serializeBlock(ctx, target);
    let reparsedMd = currentMd;
    try {
      const schema = ctx.get(schemaCtx);
      const reNode = schema.nodes.transcript_segment.createChecked(target.attrs, parseInline(ctx, currentMd));
      reparsedMd = serializeBlock(ctx, reNode);
    } catch {
      reparsedMd = `${currentMd}#roundtrip-error`;
    }
    const decision = segmentCommitDecision({
      storedText: base.storedText,
      baselineMd: base.baselineMd,
      currentMd,
      currentPlain: (target as PMNode).textContent,
      reparsedMd,
    });
    if (decision.kind === "commit") {
      if (!decision.roundTripOk) console.warn(`段 ${seq} markdown 序列化往返不稳定,按纯文本提交`);
      onEditSegment(seq, base.storedText, decision.newText);
    }
  }

  function currentSeq(): number | null {
    if (!ctxRef) return null;
    const view = ctxRef.get(editorViewCtx);
    const $from = view.state.selection.$from;
    for (let d = $from.depth; d >= 0; d--) {
      const n = $from.node(d);
      if (n.type.name === "transcript_segment") return n.attrs.seq as number;
    }
    return null;
  }

  export function focusedSegment(): number | null {
    return hasFocus() ? currentSeq() : null;
  }
```

2c. uiPlugins 扩展(同一 Plugin 内):
- `nodeViews` 增加 `transcript_segment: makeSegmentView({ speakerBadge: (a) => speakerBadge(a as BadgeAttrs), formatTs, canEdit: () => editable, onBadgeClick: (seq, rect) => onBadgeClick?.({ seq, speaker: null, name: null, personId: null, startMs: 0 }, rect), onPlayFrom: (ms) => onPlayFrom?.(ms), onDeleteClick: (seq, rect) => onDeleteClick?.(seq, rect) })`
- `handleDOMEvents.focusout` 在 segments 模式下:`if (mode === "segments" && focusedSeqNo !== null) { const seq = focusedSeqNo; focusedSeqNo = null; commitSegment(seq); }`
- `view.update` 里 segments 模式跟踪焦点段切换:

```ts
            if (mode === "segments") {
              const seq = currentSeq();
              if (focusedSeqNo !== null && seq !== focusedSeqNo) commitSegment(focusedSeqNo);
              focusedSeqNo = view.hasFocus() ? seq : focusedSeqNo;
            }
```

2d. segments 模式键位($prose keymap,仅 mode==="segments" 时 use):

```ts
  const segmentKeys = $prose(() =>
    keymap({
      Enter: (state, _dispatch, view) => {
        // Enter=提交当前段(不分段;分段事务本就被 segmentLockPlugin 拒绝)
        const seq = focusedSeqNo;
        if (seq !== null) commitSegment(seq);
        view?.dom.blur();
        return true;
      },
      Escape: (_state, _dispatch, view) => {
        // Esc=还原当前段为基线并失焦(重建走页面 refresh → setSegments)
        const seq = focusedSeqNo;
        focusedSeqNo = null;
        view?.dom.blur();
        if (seq !== null) rootEl.dispatchEvent(new CustomEvent("segescape", { detail: { seq } }));
        return true;
      },
    }),
  );
```

2e. `onMount` 的 `.use(...)` 链按 mode 组装:

```ts
    let builder = Editor.make()
      .config((ctx) => {
        ctx.set(rootCtx, rootEl);
        ctx.set(defaultValueCtx, "");
      })
      .use(commonmark)
      .use(history)
      .use(uiPlugins);
    builder =
      mode === "refined"
        ? builder.use(refinedParagraphSchema).use(entityMentionSchema).use(refinedNormalizePlugin)
        : builder.use(transcriptSegmentSchema).use(segmentLockPlugin).use(segmentKeys);
    editor = await builder.create();
```

- [ ] **Step 3: 验证**

Run: `npm run check && npx vitest run src/lib/editor/editorDoc.test.ts`
Expected: 0 errors;测试全绿

- [ ] **Step 4: Commit**

```bash
git add src/lib/editor/segmentSchema.ts src/lib/editor/MarkdownEditor.svelte
git commit -m "feat(editor): 原始稿 segments 模式(结构锁定+段级提交+Enter/Esc 键位)"
```

---

### Task 9: 笔记页原始稿接线

**Files:**
- Modify: `src/routes/notes/[id]/+page.svelte`
  - 原始稿渲染分支(`{:else}` 内 `{#each displaySegments as seg (seg.seq)} ... {/each}`,约 787-860 行)→ MarkdownEditor
  - `segFocus`/`segBlur` 删除,换 `onEditSegment` 处理器;说话人菜单/删除确认改为浮层
  - playing/discarded 类同步 effect

**Interfaces:**
- Consumes: MarkdownEditor segments 模式(Task 8)、既有 `editSegment/deleteSegment/setSegmentSpeaker/refresh/playFrom/activeSeqs/discardedSeqs/speakerIds/note.speakers/canEdit`
- Produces: 原始稿视图 = 段结构锁定的 WYSIWYG;段内失焦/Enter 提交走 `edit_segment`,冲突回滚重载;说话人/删除操作走浮层 + 既有命令

- [ ] **Step 1: script 区改造**

删除 `segFocus`、`segBlur` 函数与 `focusedSeq` 状态(常驻编辑守卫改由编辑器 `hasFocus()` 承担)。新增:

```ts
  let segEditor = $state<ReturnType<typeof MarkdownEditor> | null>(null);
  // 原始稿浮层:说话人菜单 / 删除确认(锚定 NodeView 内按钮的屏幕矩形)
  let segMenuPop = $state<{ seq: number; rect: DOMRect } | null>(null);
  let segDeletePop = $state<{ seq: number; rect: DOMRect } | null>(null);

  function segBadge(attrs: BadgeAttrs): { label: string; bg: string; ink: string } {
    // segments 模式徽章按段属性取既有配色(note.speakers 命名/关联人物一致)
    const seg = note?.segments.find((s) => s.seq === attrs.seq);
    const speaker = seg?.speaker ?? null;
    const source = seg?.source ?? "mic";
    return {
      label: speakerLabel(speaker, source, note?.speakers ?? {}),
      bg: speakerColor(speaker, source, note?.speakers),
      ink: speakerInk(speaker, source, note?.speakers),
    };
  }

  async function doEditSegment(seq: number, expectedText: string, newText: string) {
    try {
      await editSegment(id, seq, expectedText, newText);
      await refresh();
    } catch (err) {
      error = `编辑失败: ${err}`;
      await refresh(); // 乐观冲突:重载最新内容(setSegments 由下方 effect 触发)
    }
  }
```

`doDeleteSeg`/`doSetSpeaker` 改为按 seq 查段(浮层没有整个 seg 对象):

```ts
  async function doDeleteSeg(seq: number) {
    segDeletePop = null;
    const seg = note?.segments.find((s) => s.seq === seq);
    if (!seg) return;
    try {
      await deleteSegment(id, seg.seq, seg.text);
      await refresh();
    } catch (e) {
      error = `删除失败: ${e}`;
      await refresh();
    }
  }

  async function doSetSpeaker(seq: number, speakerId: string) {
    segMenuPop = null;
    const seg = note?.segments.find((s) => s.seq === seq);
    if (!seg) return;
    try {
      await setSegmentSpeaker(id, seg.seq, seg.text, speakerId);
      await refresh();
    } catch (e) {
      error = `修改说话人失败: ${e}`;
      await refresh();
    }
  }
```

同步 effect 与类高亮 effect:

```ts
  // note.segments 变化(载入/提交回执/冲突重载)→ 重建编辑器文档;输入中不打断
  $effect(() => {
    const segs = displaySegments;
    const ed = segEditor;
    const speakers = note?.speakers;
    if (!ed || !note || effectiveView === "refined") return;
    if (ed.hasFocus()) return;
    ed.setSegments(segs, speakers ?? {});
  });

  // 播放高亮/AI 过滤灰显:NodeView DOM 不吃 Svelte class 指令,直接按 data-seq 贴类
  $effect(() => {
    const el = transcriptEl;
    const active = activeSeqs;
    const discarded = discardedSeqs;
    if (!el || effectiveView === "refined") return;
    for (const node of el.querySelectorAll<HTMLElement>(".md-seg")) {
      const seq = Number(node.dataset.seq);
      node.classList.toggle("playing", active.has(seq));
      node.classList.toggle("discarded", discarded.has(seq));
      node.title = discarded.has(seq) ? "已被 AI 过滤" : "";
    }
  });
```

Escape 还原桥接(编辑器容器上监听 `segescape` → `refresh()` 重建即还原)。

- [ ] **Step 2: 模板替换原始稿分支**

`{:else}` 分支整段替换为:

```svelte
        <MarkdownEditor
          bind:this={segEditor}
          mode="segments"
          editable={canEdit}
          speakerBadge={segBadge}
          onEditSegment={doEditSegment}
          onBadgeClick={(attrs, rect) => (segMenuPop = { seq: attrs.seq!, rect })}
          onDeleteClick={(seq, rect) => (segDeletePop = { seq, rect })}
          onPlayFrom={(ms) => playFrom({ start_ms: ms })}
        />
        {#if displaySegments.length === 0}
          <p class="hint">（这场会议没有转写内容）</p>
        {/if}
```

浮层(transcript 之后;菜单项复用原 badge-menu 的按钮结构与样式):

```svelte
    {#if segMenuPop && note}
      <div class="badge-menu floating" style="position: fixed; left: {segMenuPop.rect.left}px; top: {segMenuPop.rect.bottom + 4}px;">
        {#each speakerIds as sid (sid)}
          <button class="menu-item" onclick={() => doSetSpeaker(segMenuPop!.seq, sid)}>
            {speakerLabel(sid, "mic", note.speakers)}
          </button>
        {/each}
        <button class="menu-item new" onclick={() => doSetSpeaker(segMenuPop!.seq, "new")}>＋ 新说话人</button>
        <button class="menu-item" onclick={() => (segMenuPop = null)}>取消</button>
      </div>
    {/if}
    {#if segDeletePop}
      <div class="badge-menu floating" style="position: fixed; left: {segDeletePop.rect.left}px; top: {segDeletePop.rect.bottom + 4}px;">
        <button class="menu-item" onclick={() => doDeleteSeg(segDeletePop!.seq)}>确认删除</button>
        <button class="menu-item" onclick={() => (segDeletePop = null)}>取消</button>
      </div>
    {/if}
```

样式:`.badge-menu.floating { z-index: 30; }` 补充;`.md-seg` 相关规则按 Task 7 同法 `:global` 化(`.seg` 原规则复制为 `.transcript :global(.md-seg)`,含 `.playing`/`.discarded` 态)。

- [ ] **Step 3: 验证**

Run: `npm run check && npm test`
Expected: 0 errors,全绿

- [ ] **Step 4: 手工冒烟(tauri dev)**

1. 原始稿逐段渲染与旧版一致:徽章配色、时间戳、AI 过滤灰显、播放跟随高亮都在。
2. 点段内文字直接编辑,失焦/Enter 提交;重进笔记编辑已落盘。
3. 段内加粗(`**x**` 输入或 Cmd+B)→ 提交后重进,渲染为粗体;导出 md 含 `**x**`。
4. Enter 不会拆段;段首 Backspace 不会并段;跨段选中删除被拒(骨架锁)。
5. Esc 还原当前段。
6. 徽章点击 → 说话人浮层,改说话人生效;删除按钮 → 确认浮层,删段生效。
7. 时间戳点击跳播;播放中当前段高亮跟随、滚动跟随不回退。
8. 录音中的笔记(recording.noteId === id)整个编辑器只读、徽章禁用。
9. 双窗口同段编辑 → 后提交方看到"编辑失败"并自动重载。

- [ ] **Step 5: Commit**

```bash
git add src/routes/notes/[id]/+page.svelte
git commit -m "feat(notes): 原始稿接入结构锁定 WYSIWYG(段级提交/浮层菜单/播放高亮保留)"
```

---

### Task 10: 收尾:全量回归 + DESIGN.md 增补 + 死代码清理

**Files:**
- Modify: `DESIGN.md`(components 段)
- Modify: `src/routes/notes/[id]/+page.svelte`(删除不再命中的旧样式:`.seg-text.editable`、旧 `.para`/`.seg` 中已被 `:global` 版本取代的规则)

**Interfaces:**
- Consumes: Tasks 1-9 全部产物
- Produces: 可合入的完整分支

- [ ] **Step 1: 死代码清理**

`src/routes/notes/[id]/+page.svelte`:
- 删除 `contenteditable` 相关残留样式(`.seg-text.editable` 及其 hover/focus 规则——若 DESIGN.md editable-text 规范已迁移到 `:global(.ProseMirror)` 上)。
- 确认 `splitMentions` 在页面已无引用则从 import 列表移除(editorDoc.ts 仍用它,notes.ts 导出保留)。
- `grep -n "segFocus\|segBlur\|focusedSeq\|confirmSeq\|speakerMenuSeq" src/routes/notes/[id]/+page.svelte` 应无残留(浮层状态已改名 segMenuPop/segDeletePop)。

- [ ] **Step 2: DESIGN.md 增补**

components 段(`**transcript-container**` 条目之后)追加:

```markdown
- **markdown-editor**(原始稿/精修稿共用 Milkdown 壳):常驻编辑态,无阅读/编辑两态。原始稿=段结构锁定(徽章+时间戳为不可编辑前缀,Enter 提交不分段);精修稿=完整 markdown 块编辑,实体高亮为行内 mark(悬浮浮层跳图谱)。focus 态沿用 editable-text 规范:`accent` 2px outline 加在 ProseMirror 根上。
```

- [ ] **Step 3: 全量回归**

Run:
```bash
npm run check && npm test && cargo test --manifest-path src-tauri/Cargo.toml 2>&1 | tail -5
```
Expected: 三者全绿

- [ ] **Step 4: 完整手工冒烟**

`npm run tauri dev`:按 Task 7 Step 5 与 Task 9 Step 4 清单各过一遍;另外确认:
1. 原始稿↔精修稿视图切换往返,两个编辑器互不串内容。
2. 切换笔记 id、返回列表再进入,无残留内容/无控制台报错。
3. 触发一次重新 Aing → 完成后精修稿刷新为新内容(revision 进位,旧编辑会话不覆盖)。
4. 导出 md/txt:精修稿(含用户插入块)与原始稿(含行内格式)内容正确。

- [ ] **Step 5: Commit**

```bash
git add DESIGN.md src/routes/notes/[id]/+page.svelte
git commit -m "docs(design): markdown-editor 组件规范;清理 contenteditable 残留样式"
```
