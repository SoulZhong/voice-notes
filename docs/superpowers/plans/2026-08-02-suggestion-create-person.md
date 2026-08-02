# 换人弹层「新建」实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 按 `docs/2026-08-02-suggestion-create-person-design.md`:换人弹层检索无同名人时给「新建并命名」行,点击=就地命名左侧说话人。

**Architecture:** 纯前端单文件改动(+page.svelte):popover 内追加 create-row + `doNameAsNew` 动作(act 包裹 renamePerson,乐观移除建议)。零后端。

**Tech Stack:** Svelte 5 runes / IPC 仿真回归。

## Global Constraints

- 工作目录 worktree;文案中文全角;token 样式;`feat(tidy):` 提交前缀 + Co-Authored-By 落款;vitest 176 / check 0 不破坏。

---

### Task 1: create-row + doNameAsNew

**Files:**
- Modify: `src/routes/speakers/+page.svelte`

**Interfaces:**
- Consumes: 既有 `renamePerson`(src/lib/people.ts)、`act()`、`tidy.suggestions`(store $state,可直接过滤重赋)、`sugKey`(从 $lib/tidy.svelte 导入,已有 import isStrong/tidy——补 sugKey)。

- [ ] **Step 1: script 区**

people.ts import 行补 `renamePerson`;tidy.svelte import 行补 `sugKey`。动作函数区加:

```ts
/** 换人弹层「新建」:库里没这个人=左侧说话人就是他,就地命名(声纹/样本/笔记
    原地不动,无合并日志);已命名的人不再进归属建议,这张卡随重算消失——乐观
    先移除,不写 dismissed(命名后建议本就不再生成,不该占一条落盘忽略)。 */
async function doNameAsNew(s: PersonMergeSuggestion, name: string) {
  await act(
    async () => {
      await renamePerson(s.loser, name);
    },
    () => {
      tidy.suggestions = tidy.suggestions.filter((x) => sugKey(x) !== sugKey(s));
    },
    `s:${s.loser}>${s.winner}`,
  );
}
```

- [ ] **Step 2: 模板**(换人 popover 内,`<PersonPickList ... />` 之后)

```svelte
{@const q = sugPickQuery.trim()}
{#if q && !people.some((p) => p.name === q)}
  <!-- 库里没这个人:就地命名,不必出弹层绕去详情页 -->
  <button class="create-row" disabled={busy} onclick={() => doNameAsNew(s, q)}>
    新建「{q}」并命名这个说话人
  </button>
{/if}
```

注意 Svelte 5 中 `{@const}` 需挂在块级标签内——若直接放 `{#if sugPickFor === skey}` 块中报错,改写为把 `{@const q = ...}` 提到该 `{#if}` 块顶部(紧随 `<div class="menu">` 前后皆可,以能编译为准)。

- [ ] **Step 3: 样式**

```css
/* 新建行:menu 行形态,accent 色标识"这是创建动作"而非候选 */
.create-row {
  display: block;
  width: 100%;
  padding: 0.38rem 0.55rem;
  background: none;
  border: none;
  border-top: 1px solid var(--hairline);
  border-radius: 0 0 var(--radius-md) var(--radius-md);
  color: var(--accent);
  font: inherit;
  font-size: 0.85rem;
  text-align: left;
  cursor: pointer;
}
.create-row:hover:not(:disabled) {
  background: var(--surface-soft);
}
.create-row:disabled {
  color: var(--ink-faint);
  cursor: default;
}
```

- [ ] **Step 4: 验证** `npm run check 2>&1 | tail -2`(0 errors)、`npx vitest run 2>&1 | tail -3`(176)。

- [ ] **Step 5: 仿真回归**(browse+mock,流程同前;截图 35-*.png)

①打开换人弹层输入「朱伟杰」→ 列表空态 + create-row 出现;②点击 → mock 日志 `rename_person {id:"P2",name:"朱伟杰"}`、建议卡消失、侧栏出现「朱伟杰」;③输入「李四」(库中有精确同名)→ 不出 create-row;④failNext rename_person → 卡片级错误就地。mock 已有 rename_person 处理,无需改。console 0 错误。

- [ ] **Step 6: 提交** `git add src/routes/speakers/+page.svelte && git commit -m "feat(tidy): 换人弹层检索无此人时可新建并就地命名"`

## Self-Review

规格全覆盖(条件/动作/门禁/不写 dismissed/失败路径);无占位;消费接口均既有。
