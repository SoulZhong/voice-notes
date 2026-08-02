# 建议卡「换个人」实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 按 `docs/2026-08-02-suggestion-switch-target-design.md`:归属建议卡右侧合并目标可就地改选(检索选人 popover),合并并入所选人,可还原建议。

**Architecture:** 纯逻辑层加 `resolveSugTarget`(覆盖优先、失效回落);概览页建议卡加会话级覆盖表 + 选人 popover(复用 PersonPickList 与详情页 menu/scrim/Esc 模式);零后端改动。

**Tech Stack:** Svelte 5 runes / vitest / IPC 仿真回归。

## Global Constraints

- 工作目录:`/Users/teemo/workspace-soul/voice-notes/.claude/worktrees/speaker-tidy`(worktree)。
- 文案中文全角;样式用既有 token;注释中文讲约束。
- 既有测试不破坏(vitest 173、check 0);录制中口径:换人/还原不置灰,合并置灰(busy||live)。
- 提交 `feat(tidy):` 前缀,落款 `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`。

---

### Task 1: resolveSugTarget 纯逻辑

**Files:**
- Modify: `src/lib/tidyQueue.ts`
- Test: `src/lib/tidyQueue.test.ts`

**Interfaces:**
- Produces: `export function resolveSugTarget(s: PersonMergeSuggestion, overrides: Record<string, string>, byId: Map<string, PersonSummary>): { id: string; name: string; overridden: boolean }`。Task 2 消费。

- [ ] **Step 1: 写失败测试**(构造器沿用文件既有 builder)

```ts
describe("resolveSugTarget", () => {
  const byId = new Map([["P1", person("P1", "张三")], ["P9", person("P9", "王五")]]);
  const s = sug("P2", "P1"); // loser P2 → 系统建议 winner P1(builder 若带名字参数,winner_name 取"张三")

  it("无覆盖时返回系统建议目标", () => {
    expect(resolveSugTarget(s, {}, byId)).toEqual({ id: "P1", name: s.winner_name, overridden: false });
  });

  it("覆盖存在于库时生效并标记 overridden", () => {
    const r = resolveSugTarget(s, { "P2>P1": "P9" }, byId);
    expect(r).toEqual({ id: "P9", name: "王五", overridden: true });
  });

  it("覆盖的人已不在库(其间被合并/删除)时回落系统建议", () => {
    expect(resolveSugTarget(s, { "P2>P1": "P404" }, byId)).toEqual({ id: "P1", name: s.winner_name, overridden: false });
  });
});
```

- [ ] **Step 2: 跑红** `npx vitest run src/lib/tidyQueue.test.ts` — resolveSugTarget 不存在。

- [ ] **Step 3: 实现**(tidyQueue.ts 尾部)

```ts
/** 建议卡合并目标解析:会话级改选覆盖优先;覆盖的人已不在库(其间被合并/删除)
    则回落系统建议目标。winner 本身不在库的建议在 buildTidyQueue 就被滤掉,无需兜底。 */
export function resolveSugTarget(
  s: PersonMergeSuggestion,
  overrides: Record<string, string>,
  byId: Map<string, PersonSummary>,
): { id: string; name: string; overridden: boolean } {
  const o = overrides[`${s.loser}>${s.winner}`];
  const p = o ? byId.get(o) : undefined;
  if (p) return { id: p.id, name: p.name, overridden: true };
  return { id: s.winner, name: s.winner_name, overridden: false };
}
```

- [ ] **Step 4: 跑绿** 同命令(13 passed),全量 `npx vitest run`(176)。

- [ ] **Step 5: 提交** `git add src/lib/tidyQueue.ts src/lib/tidyQueue.test.ts && git commit -m "feat(tidy): resolveSugTarget 建议目标覆盖解析(失效回落)"`

---

### Task 2: 建议卡换人 UI + 回归

**Files:**
- Modify: `src/routes/speakers/+page.svelte`

**Interfaces:**
- Consumes: Task 1 `resolveSugTarget`;既有 `PersonPickList`(props: people/query/excludeIds/onpick)、`personPane` snippet、`plabel`、`loadNotes`、`act()`。

- [ ] **Step 1: 状态与导入**

script 区加:

```ts
import PersonPickList from "$lib/PersonPickList.svelte";
// (resolveSugTarget 并入既有 tidyQueue import)

/** 建议卡合并目标的会话级改选(键=sugKey);还原=删键。 */
let sugOverride = $state<Record<string, string>>({});
/** 打开选人 popover 的建议卡(sugKey);同屏至多一个。 */
let sugPickFor = $state<string | null>(null);
let sugPickQuery = $state("");
```

act() 里 `confirmClean = false;` 后加 `sugPickFor = null;`(动作开始收浮层)。

doMergeSuggestion 改为并入当前目标:

```ts
async function doMergeSuggestion(s: PersonMergeSuggestion, targetId: string, targetName: string) {
  await act(
    async () => {
      const jid = await mergePerson(s.loser, targetId);
      tidy.lastManual = {
        journalId: jid,
        label: `${plabel(s.loser, s.loser_name)} → ${plabel(targetId, targetName)}`,
      };
    },
    undefined,
    `s:${s.loser}>${s.winner}`,
  );
}
```

- [ ] **Step 2: 建议卡模板改造**(suggestion 分支整体替换)

```svelte
{:else if item.kind === "suggestion"}
  {@const s = item.suggestion}
  {@const skey = `${s.loser}>${s.winner}`}
  {@const target = resolveSugTarget(s, sugOverride, personById)}
  <section class="card">
    <div class="card-tag">归属建议</div>
    <div class="card-title">
      这两条像同一个人吗?
      {#if target.overridden}
        <!-- 相似度只对系统建议对成立,换了人再挂着就是误导 -->
        <span class="sim">手动改选</span>
        <button
          class="mini plain"
          onclick={() => {
            const { [skey]: _, ...rest } = sugOverride;
            sugOverride = rest;
          }}>还原建议</button>
      {:else}
        <span class="sim" class:strong={isStrong(s)}>
          相似度 {Math.round(s.similarity * 100)}%{isStrong(s) ? " · 很可能" : ""}
        </span>
      {/if}
    </div>
    <div class="panes">
      {@render personPane(s.loser, s.loser_name)}
      <svg class="arrow" width="18" height="18" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
        <path d="M2.5 8h10M9 4.5L13.5 8 9 11.5" />
      </svg>
      <div class="switch-anchor">
        {@render personPane(target.id, target.name)}
        <!-- 猜错人时就地换目标,不必忽略后绕去详情页「合并到…」 -->
        <button
          class="switch-btn"
          title="不是他?换成另一个已有的人"
          onclick={() => {
            confirmClean = false;
            sugPickQuery = "";
            sugPickFor = sugPickFor === skey ? null : skey;
          }}>换个人</button>
        {#if sugPickFor === skey}
          <button class="menu-scrim" aria-label="关闭菜单" onclick={() => (sugPickFor = null)}></button>
          <div class="menu">
            <div class="menu-title">把「{plabel(s.loser, s.loser_name)}」并入…</div>
            <!-- svelte-ignore a11y_autofocus -->
            <input class="pick-input" autofocus placeholder="输入名字检索" bind:value={sugPickQuery} />
            <PersonPickList
              people={people.filter((p) => p.id !== s.loser && p.id !== target.id)}
              query={sugPickQuery}
              onpick={(p) => {
                sugOverride = { ...sugOverride, [skey]: p.id };
                loadNotes(p.id);
                sugPickFor = null;
              }}
            />
          </div>
        {/if}
      </div>
    </div>
    <p class="hint">两边各听一段原声,确认是同一个人再合并;合并保留双方声纹,认得更准。</p>
    <div class="acts">
      <button class="mini accent" disabled={busy || live} onclick={() => doMergeSuggestion(s, target.id, target.name)}>合并</button>
      <!-- 忽略是本地处置(dismissed 元数据),后端录制中也放行,不必陪绑置灰 -->
      <button class="mini" disabled={busy} onclick={() => doIgnoreSuggestion(s)}>忽略</button>
    </div>
    {@render cardError(tidyItemKey(item))}
  </section>
```

(与现状差异:card-title 分支、winner pane 外包 `.switch-anchor`、换人钮+popover、doMergeSuggestion 传参。其余原样保留。)

- [ ] **Step 3: 样式与 Esc**

styles 加(menu/pick-input/menu-scrim 抄详情页同名规则,保持一致):

```css
/* 换目标:锚定 winner 面板,按钮悬浮右上角(faint 小字,hover 显色) */
.switch-anchor {
  position: relative;
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
}
.switch-btn {
  position: absolute;
  top: 0.45rem;
  right: 0.55rem;
  border: none;
  background: none;
  color: var(--ink-faint);
  font-size: 0.75rem;
  padding: 0.1em 0.3em;
  border-radius: var(--radius-sm);
  cursor: pointer;
}
.switch-btn:hover {
  color: var(--accent);
  background: var(--accent-tint);
}
.menu-scrim {
  position: fixed;
  inset: 0;
  z-index: 9;
  border: 0;
  padding: 0;
  background: transparent;
  cursor: default;
}
.menu {
  position: absolute;
  top: 2rem;
  right: 0.4rem;
  z-index: 10;
  min-width: 16rem;
  max-height: 16rem;
  overflow-y: auto;
  background: var(--surface-press);
  border: 1px solid var(--hairline);
  border-radius: var(--radius-lg);
  box-shadow: var(--shadow-popover);
  padding: 0.35rem;
}
.menu-title {
  color: var(--ink-secondary);
  font-size: 0.78rem;
  line-height: 1.45;
  padding: 0.25rem 0.5rem 0.35rem;
}
.pick-input {
  width: 100%;
  box-sizing: border-box;
  padding: 0.35rem 0.5rem;
  margin-bottom: 0.25rem;
  background: transparent;
  border: none;
  border-bottom: 1px solid var(--hairline);
  outline: none;
  font: inherit;
  font-size: 0.85rem;
  color: var(--ink);
}
.pick-input::placeholder {
  color: var(--ink-faint);
}
```

文件尾(main 之后)加:

```svelte
<svelte:window
  onkeydown={(e) => {
    if (e.key !== "Escape") return;
    if (sugPickFor) sugPickFor = null;
    else if (confirmClean) confirmClean = false;
  }}
/>
```

- [ ] **Step 4: 验证** `npm run check 2>&1 | tail -2`(0 errors)、`npx vitest run 2>&1 | tail -3`(176)。

- [ ] **Step 5: IPC 仿真回归**(browse + scratchpad/mock-ipc.js,流程同前:goto→eval→点会议搭子)

场景:①建议卡(说话人 2→张三)点「换个人」→ popover 出现,输入 `li`/`李` 命中李四 → 选中后右侧变李四、标题变「手动改选」+「还原建议」;②点合并 → mock 日志 merge_person args=[P2, P3(李四)],撤销条「说话人 2 → 李四」,撤销复原;③再换人后删掉所选人(改 mock people)+ 重新整理 → 卡片回落系统建议目标张三;④Esc 与点外部关闭 popover;⑤「还原建议」恢复相似度标。截图归档 33-*.png,console 0 错误。

- [ ] **Step 6: 提交** `git add src/routes/speakers/+page.svelte && git commit -m "feat(tidy): 建议卡就地换合并目标(检索选人/手动改选标/还原建议)"`

## Self-Review

规格覆盖:入口/popover/改选态/合并/还原/兜底/Esc 全对应;无占位;resolveSugTarget 签名两任务一致;doMergeSuggestion 新签名仅本文件调用(模板同步改)。
