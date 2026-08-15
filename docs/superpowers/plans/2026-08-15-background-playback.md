# 切页继续播放 + 全局迷你播放条 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 播放笔记音频时切到其他页面不再中断,底部出现可暂停/关闭的全局迷你播放条。

**Architecture:** 播放引擎本就在 Rust(单条 cpal 输出流,WebView 只画 UI),中断源是
`AudioPlayer.svelte` 卸载时调 `player_stop`。引入一个**会话(session)**概念:由用户
真正开始播放建立、独立于笔记页组件存活,持有后端内核的所有权。后端 `player_pos` 事件
加上装载代次 `gen`,前端 store 只接受属于当前会话的事件,杜绝"停 A 装 B"窗口里的串台。

**Tech Stack:** Svelte 5 runes(`$state`/`$derived`)、Tauri v2 invoke/event、vitest、Rust。

## Global Constraints

- 规格来源:`docs/superpowers/specs/2026-08-15-background-playback-design.md`(第二版)。
- **会话 ≠ 已装载内核**:进笔记页会自动装载,但只有用户点了播放才建立会话。只看不播,离页后不得出现迷你条。
- **事件必须带代次**:store 只接受 `gen === session.gen` 的 `player_pos`,其余丢弃。这是竞态防线,不是可选优化。
- **换代守卫语义不得改动**:在途装载返回后 `gen !== loadGen` 的条件停止逻辑一行不动(它防的是 2026-08-10 排障过的"迟到装载掐掉新页面")。本计划只改**卸载时**要不要停。
- **浮层形态(2026-08-15 改版)**:右下角圆形浮层(直径约 56px),圆环周长表示播放进度,圆心为播放/暂停按钮(左键),右键菜单含「回到笔记」「关闭播放」。进度环**只读不可拖拽**。此前的底部通栏形态已废弃——实测会遮挡页面底部内容,且 padding 补偿对内部 `height:100%`/`overflow:hidden` 的页面无效。
- 中文注释与既有代码风格一致;新文件放 `src/lib/`。
- 前端测试用 vitest(`npm test`),Rust 用 `cargo test --lib`。

---

### Task 1: 后端 —— `player_pos` 事件带上装载代次

**Files:**
- Modify: `src-tauri/src/player.rs`(`Core` 结构、`PosEvent`、`emit_pos`、`player_load` 里的 Core 构造)

**Interfaces:**
- Produces: `player_pos` 事件负载新增 `gen: u64`,即 `{ pos_ms, playing, gen }`。前端 Task 2 依赖它做会话过滤。

- [ ] **Step 1: 写失败测试**

在 `src-tauri/src/player.rs` 的 `#[cfg(test)] mod tests` 里加:

```rust
/// 位置事件必须带装载代次:store 靠它把"停 A 装 B"窗口里 A 的排队事件丢掉,
/// 否则 A 的位置会写进 B 的界面(spec 第二版 P1)。
#[test]
fn pos_event_carries_load_generation() {
    let ev = PosEvent { pos_ms: 1234, playing: true, gen: 7 };
    let v = serde_json::to_value(&ev).unwrap();
    assert_eq!(v["pos_ms"], 1234);
    assert_eq!(v["playing"], true);
    assert_eq!(v["gen"], 7, "缺 gen 前端就无法辨认事件归属");
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test --lib pos_event_carries_load_generation`
Expected: 编译失败,`PosEvent` 无 `gen` 字段。

- [ ] **Step 3: 实现**

`PosEvent` 加字段:

```rust
#[derive(Debug, Clone, Serialize)]
struct PosEvent {
    pos_ms: u64,
    playing: bool,
    /// 装载代次:前端会话据此辨认事件归属,丢弃属于旧内核的排队事件。
    gen: u64,
}
```

`Core` 加字段(紧跟 `playing`):

```rust
    /// 装载本核时的代次,随位置事件发给前端做归属判定。
    gen: u64,
```

`emit_pos` 带上它:

```rust
fn emit_pos(app: &AppHandle, core: &Core) {
    let _ = app.emit(
        "player_pos",
        PosEvent {
            pos_ms: core.pos_ms(),
            playing: core.playing.load(Ordering::Relaxed),
            gen: core.gen,
        },
    );
}
```

在 `player_load` 构造 `Core` 处(`Arc::new(Core { ... })`)补 `gen,`——该函数入口已有
`let gen = ...`,直接用它。若测试模块另有构造 `Core` 的地方,一并补 `gen: 0`。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd src-tauri && cargo test --lib player`
Expected: 全部 PASS,无编译警告。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/player.rs
git commit -m "feat(player): 位置事件带上装载代次

前端会话据此辨认事件归属。没有它,停 A 装 B 的窗口里 A 的排队位置事件
会写进 B 的界面(spec 第二版 P1 竞态)。"
```

---

### Task 2: 播放会话 store(纯逻辑 + 全局订阅)

**Files:**
- Create: `src/lib/playback.svelte.ts`
- Create: `src/lib/playback.test.ts`

**Interfaces:**
- Consumes: Task 1 的 `player_pos` 事件负载 `{ pos_ms, playing, gen }`。
- Produces:
  - `playback.session: { gen: number; noteId: string; title: string; totalMs: number } | null`
  - `playback.currentMs: number`、`playback.playing: boolean`
  - `playback.begin(s: { gen: number; noteId: string; title: string; totalMs: number }): void`
  - `playback.clear(): void`
  - `playback.applyPos(e: { pos_ms: number; playing: boolean; gen: number }): void`
  - `shouldShowMiniPlayer(noteId: string | null, pathname: string): boolean`
  - `shouldStopOnCleanup(lastBackendGen: number | null, sessionGen: number | null): boolean`

- [ ] **Step 1: 写失败测试**

创建 `src/lib/playback.test.ts`:

```ts
import { describe, expect, it } from "vitest";
import { shouldShowMiniPlayer, shouldStopOnCleanup } from "./playback.svelte";

describe("shouldShowMiniPlayer", () => {
  it("无会话一律不显示", () => {
    expect(shouldShowMiniPlayer(null, "/settings")).toBe(false);
    expect(shouldShowMiniPlayer(null, "/notes/20260815-072046")).toBe(false);
  });

  it("有会话且在其他页 → 显示", () => {
    expect(shouldShowMiniPlayer("20260815-072046", "/settings")).toBe(true);
    expect(shouldShowMiniPlayer("20260815-072046", "/")).toBe(true);
    expect(shouldShowMiniPlayer("20260815-072046", "/speakers/S1")).toBe(true);
  });

  it("正在播放的那篇笔记页上隐藏(完整播放器已在页面里)", () => {
    expect(shouldShowMiniPlayer("20260815-072046", "/notes/20260815-072046")).toBe(false);
    // 尾斜杠、查询串、hash 都不该让它误判成"别的页"
    expect(shouldShowMiniPlayer("20260815-072046", "/notes/20260815-072046/")).toBe(false);
    expect(shouldShowMiniPlayer("20260815-072046", "/notes/20260815-072046?tab=refined")).toBe(false);
  });

  it("在别的笔记页上要显示(那篇尚未接管播放)", () => {
    expect(shouldShowMiniPlayer("20260815-072046", "/notes/20260814-180747")).toBe(true);
  });
});

describe("shouldStopOnCleanup", () => {
  it("从未成功装载 → 无核可收,不发停止", () => {
    expect(shouldStopOnCleanup(null, null)).toBe(false);
    expect(shouldStopOnCleanup(null, 5)).toBe(false);
  });

  it("本组件装的核正是活动会话 → 不停,会话接管所有权", () => {
    expect(shouldStopOnCleanup(5, 5)).toBe(false);
  });

  it("无会话,或会话已换到别的代次 → 停,语义同现状", () => {
    expect(shouldStopOnCleanup(5, null)).toBe(true);
    expect(shouldStopOnCleanup(5, 9)).toBe(true);
  });
});
```

- [ ] **Step 2: 运行测试确认失败**

Run: `npm test -- playback`
Expected: FAIL,`./playback.svelte` 不存在。

- [ ] **Step 3: 实现纯函数与状态**

创建 `src/lib/playback.svelte.ts`:

```ts
import { onPlayerPos } from "$lib/events";

/** 活动播放会话。由用户**真正开始播放**建立,不是装载建立——播放器进笔记页就会
    自动装载,若用装载判定,只看过没播过的笔记离页后也会冒出迷你条。 */
export type PlaybackSession = {
  /** 后端装载代次:位置事件靠它辨认归属。 */
  gen: number;
  noteId: string;
  title: string;
  totalMs: number;
};

/** 迷你条显示判定:有会话,且当前不在这篇笔记自己的详情页上。
    路径比较只看 pathname 的第一段笔记 id,尾斜杠/查询串/hash 不参与。 */
export function shouldShowMiniPlayer(noteId: string | null, pathname: string): boolean {
  if (!noteId) return false;
  const path = pathname.split(/[?#]/)[0].replace(/\/+$/, "");
  return path !== `/notes/${noteId}`;
}

/** 组件卸载时要不要停内核。所有权模型:内核归**会话**所有,不再归组件所有。
    - 从未成功装载 → 无核可收
    - 本组件装的核正是活动会话 → 不停(会话接管)
    - 其余 → 停,与本功能引入前语义一致 */
export function shouldStopOnCleanup(
  lastBackendGen: number | null,
  sessionGen: number | null,
): boolean {
  if (lastBackendGen === null) return false;
  return lastBackendGen !== sessionGen;
}

class Playback {
  session = $state<PlaybackSession | null>(null);
  currentMs = $state(0);
  playing = $state(false);

  begin(s: PlaybackSession) {
    this.session = s;
    this.currentMs = 0;
    this.playing = true;
  }

  clear() {
    this.session = null;
    this.currentMs = 0;
    this.playing = false;
  }

  /** 位置事件入口:只接受属于当前会话代次的事件。丢弃的是"停 A 装 B"窗口里
      A 的排队事件——它们若被采纳,A 的位置会写进 B 的界面。 */
  applyPos(e: { pos_ms: number; playing: boolean; gen: number }) {
    if (!this.session || e.gen !== this.session.gen) return;
    this.currentMs = Math.min(e.pos_ms, this.session.totalMs);
    this.playing = e.playing;
  }

  /** 后台播放期间笔记被改名 → 迷你条标题跟着更新,否则会一直显示旧名。 */
  rename(noteId: string, title: string) {
    if (this.session?.noteId === noteId) this.session = { ...this.session, title };
  }

  /** 同篇重装后恢复会话:代次换新,位置与播放态沿用重装前的现场。
      不能用 begin ——它把 currentMs 归零、playing 置真,那是「新开一段播放」的语义,
      重装场景下会把迷你条进度打回 0。 */
  restore(s: PlaybackSession, atMs: number, playing: boolean) {
    this.session = s;
    this.currentMs = atMs;
    this.playing = playing;
  }
}

export const playback = new Playback();

/** 全局位置订阅:必须放在 store 而不是 AudioPlayer 组件里——组件一卸载订阅就没了,
    迷你条的进度会僵住。应用启动时由 +layout.svelte 调一次。 */
export function startPlaybackSubscriptions(): () => void {
  return onPlayerPos((e) => playback.applyPos(e));
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `npm test -- playback`
Expected: PASS,10 个断言全绿。

- [ ] **Step 5: 补会话状态机的测试并通过**

在 `src/lib/playback.test.ts` 追加:

```ts
import { playback } from "./playback.svelte";

describe("会话状态机", () => {
  const s = { gen: 3, noteId: "n1", title: "会议", totalMs: 60000 };

  it("begin 建立会话,clear 清空", () => {
    playback.begin(s);
    expect(playback.session?.noteId).toBe("n1");
    expect(playback.playing).toBe(true);
    playback.clear();
    expect(playback.session).toBe(null);
    expect(playback.playing).toBe(false);
  });

  it("只接受本会话代次的位置事件", () => {
    playback.begin(s);
    playback.applyPos({ pos_ms: 1000, playing: true, gen: 3 });
    expect(playback.currentMs).toBe(1000);
    // 旧内核的排队事件:必须丢弃,否则会写进当前会话
    playback.applyPos({ pos_ms: 55555, playing: false, gen: 2 });
    expect(playback.currentMs).toBe(1000);
    expect(playback.playing).toBe(true);
    playback.clear();
  });

  it("位置不超过总时长", () => {
    playback.begin(s);
    playback.applyPos({ pos_ms: 999999, playing: true, gen: 3 });
    expect(playback.currentMs).toBe(60000);
    playback.clear();
  });

  it("播放自然结束保留会话(可重播)", () => {
    playback.begin(s);
    playback.applyPos({ pos_ms: 60000, playing: false, gen: 3 });
    expect(playback.session).not.toBe(null);
    expect(playback.playing).toBe(false);
    playback.clear();
  });

  it("改名只影响同一篇笔记", () => {
    playback.begin(s);
    playback.rename("other", "别的");
    expect(playback.session?.title).toBe("会议");
    playback.rename("n1", "新名字");
    expect(playback.session?.title).toBe("新名字");
    playback.clear();
  });

  it("同篇重装 → restore 换新代次且不归零现场,新代次事件被接受", () => {
    playback.begin(s);
    playback.applyPos({ pos_ms: 5000, playing: true, gen: 3 });
    playback.restore({ ...s, gen: 8 }, 5000, true);
    expect(playback.session?.gen).toBe(8);
    expect(playback.currentMs).toBe(5000);
    expect(playback.playing).toBe(true);
    playback.applyPos({ pos_ms: 2000, playing: true, gen: 8 });
    expect(playback.currentMs).toBe(2000);
    // 旧代次事件仍要丢弃
    playback.applyPos({ pos_ms: 9000, playing: true, gen: 3 });
    expect(playback.currentMs).toBe(2000);
    playback.clear();
  });
});
```

Run: `npm test -- playback`
Expected: PASS。

- [ ] **Step 6: 提交**

```bash
git add src/lib/playback.svelte.ts src/lib/playback.test.ts
git commit -m "feat(playback): 播放会话 store

会话由用户真正开始播放建立(不是装载建立——播放器进页就自动装载,
用装载判定会让只看过没播过的笔记也冒出迷你条)。位置事件按装载代次
过滤,丢弃停 A 装 B 窗口里 A 的排队事件。"
```

---

### Task 3: AudioPlayer 交出所有权

**Files:**
- Modify: `src/lib/AudioPlayer.svelte`

**Interfaces:**
- Consumes: Task 2 的 `playback`、`shouldStopOnCleanup`。
- Produces: 新 props `noteId?: string`、`title?: string`;`play()` 成功后建立会话。

- [ ] **Step 1: 加 props 与 import**

在 `<script lang="ts">` 顶部 import 处加:

```ts
  import { playback, shouldStopOnCleanup } from "$lib/playback.svelte";
```

在 `$props()` 的类型与解构里加两个可选字段(放在 `onLoaded` 之前):

```ts
    /** 本播放器所属笔记的身份:建立播放会话用(迷你条要显示标题、点标题要能跳回)。
        不传则不建立会话——本组件也被别处复用时保持现状行为。 */
    noteId?: string;
    title?: string;
```

- [ ] **Step 2: cleanup 改为按所有权决策**

把装载 `$effect` 的 cleanup 里那段条件停止:

```ts
      const g = lastBackendGen;
      lastBackendGen = null;
      if (g !== null) void invoke("player_stop", { ifGen: g }).catch(() => {});
```

替换成:

```ts
      // 所有权:内核归**会话**所有,不再归组件所有。本组件装的核若正是活动会话,
      // 卸载不停它——那正是"切页继续播放"。其余情形语义同现状。
      // 注意 cleanup 不等于组件卸载:本 effect 依赖 tracks,转码完成重拉、续录、
      // A/B 切换都会重跑它,所以这里绝不能做"登记会话"之类的副作用。
      const g = lastBackendGen;
      lastBackendGen = null;
      if (shouldStopOnCleanup(g, playback.session?.gen ?? null)) {
        void invoke("player_stop", { ifGen: g }).catch(() => {});
      }
```

- [ ] **Step 3: 会话跟着内核走(发起前作废,成功后恢复)**

后端 `player_load` 在**函数入口**就 `begin_load()+stop_stream()` 杀掉旧核。所以会话必须
在**发起装载之前**同步作废——放在成功之后同步的话,装载失败 / 被 SUPERSEDED / 在途
离页三条路径都会漏掉,留下一个指着已死内核的会话:迷你条挂着旧笔记、进度僵住、
按钮点不动。

先在 `const res = await invoke<...>("player_load", ...)` 这一句**之前**插入(变量名必须叫
`prevSession`,不能叫 `prev`——外层 `loadChain` 已有一个 `prev`,重名会触发 TDZ 错误
而且被 `try { await prev } catch {}` 静默吞掉,变成并发装载时的串行链失效):

```ts
        const prevSession = playback.session
          ? { session: playback.session, atMs: playback.currentMs, playing: playback.playing }
          : null;
        playback.clear();
```

再在 `lastBackendGen = res.gen;` 这一行**之后**插入:

```ts
        // 同一篇笔记重装(转码完成重拉/续录/A-B 切换):按新代次恢复会话,位置与
        // 播放态沿用重装前的现场;装的是别的笔记则不恢复——发起装载前已作废。
        // totalMs 用本次装载返回的 res.total_ms:续录后笔记变长,沿用旧值会让
        // 迷你条进度卡在旧总长。
        if (prevSession && noteId && prevSession.session.noteId === noteId) {
          playback.restore(
            { ...prevSession.session, gen: res.gen, totalMs: res.total_ms },
            prevSession.atMs,
            prevSession.playing,
          );
        }
```

- [ ] **Step 4: play 成功后建立会话**

把 `play()` 里的:

```ts
      .then(() => invoke("player_play"))
```

替换成:

```ts
      .then(() => invoke("player_play"))
      .then(() => {
        // 会话由**真正开始播放**建立(spec:装载不算)。缺身份则不建会话。
        if (noteId && lastBackendGen !== null) {
          playback.begin({ gen: lastBackendGen, noteId, title: title ?? "", totalMs });
        }
      })
```

- [ ] **Step 5: 运行既有测试确认无回归**

Run: `npm test && npm run check`
Expected: 全部 PASS;类型检查 0 错 0 警。

- [ ] **Step 6: 提交**

```bash
git add src/lib/AudioPlayer.svelte
git commit -m "feat(player): 卸载时把内核所有权交给会话

cleanup 不再无条件停内核:本组件装的核若正是活动会话就留着,那正是
切页继续播放。换代守卫(在途装载的条件停止)一行未动——它防的是迟到
装载掐掉新页面,与所有权无关。"
```

---

### Task 4: 迷你条组件 + 挂进 layout

**Files:**
- Create: `src/lib/MiniPlayer.svelte`
- Modify: `src/routes/+layout.svelte`

**Interfaces:**
- Consumes: Task 2 的 `playback`、`shouldShowMiniPlayer`、`startPlaybackSubscriptions`。

- [ ] **Step 1: 写组件**

创建 `src/lib/MiniPlayer.svelte`:

```svelte
<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { goto } from "$app/navigation";
  import { page } from "$app/stores";
  import { playback, shouldShowMiniPlayer } from "$lib/playback.svelte";
  import { formatTs } from "$lib/notes";
  import { t } from "$lib/i18n/index.svelte";

  /* 全局迷你播放条:切到非笔记页后仍能看到并控制正在播放的笔记。
     进度只读——要精确跳转就点标题回笔记页,那里有波形和逐句时间戳。 */
  const show = $derived(shouldShowMiniPlayer(playback.session?.noteId ?? null, $page.url.pathname));
  const pct = $derived(
    playback.session && playback.session.totalMs > 0
      ? Math.min(100, (playback.currentMs / playback.session.totalMs) * 100)
      : 0,
  );

  function toggle() {
    if (playback.playing) {
      playback.playing = false;
      void invoke("player_pause").catch(() => {});
    } else {
      playback.playing = true;
      void invoke("player_play").catch(() => {});
    }
  }

  function close() {
    void invoke("player_stop", {}).catch(() => {});
    playback.clear();
  }
</script>

{#if show && playback.session}
  <div class="mini">
    <button class="icon" onclick={toggle} aria-label={playback.playing ? t("notes.player.pause") : t("notes.player.play")}>
      {playback.playing ? "⏸" : "▶"}
    </button>
    <button class="title" onclick={() => goto(`/notes/${playback.session?.noteId}`)}>
      {playback.session.title}
    </button>
    <div class="bar"><div class="fill" style:width="{pct}%"></div></div>
    <span class="ts">{formatTs(playback.currentMs)} / {formatTs(playback.session.totalMs)}</span>
    <button class="icon" onclick={close} aria-label={t("common.close")}>✕</button>
  </div>
{/if}

<style>
  .mini {
    position: fixed;
    left: 0;
    right: 0;
    bottom: 0;
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 8px 14px;
    background: var(--surface-raised, #1c1c1e);
    border-top: 1px solid var(--border, #2c2c2e);
    z-index: 40;
  }
  .icon {
    background: none;
    border: none;
    color: inherit;
    cursor: pointer;
    font-size: 14px;
    padding: 4px 6px;
  }
  .title {
    background: none;
    border: none;
    color: inherit;
    cursor: pointer;
    font-size: 13px;
    max-width: 32ch;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    text-align: left;
  }
  .title:hover { text-decoration: underline; }
  .bar { flex: 1; height: 3px; background: var(--border, #2c2c2e); border-radius: 2px; }
  .fill { height: 100%; background: var(--accent, #0a84ff); border-radius: 2px; }
  .ts { font-size: 12px; opacity: 0.7; font-variant-numeric: tabular-nums; }
</style>
```

- [ ] **Step 2: 挂进 layout 并起全局订阅**

在 `src/routes/+layout.svelte` 的 `<script>` 里加:

```ts
  import MiniPlayer from "$lib/MiniPlayer.svelte";
  import { startPlaybackSubscriptions } from "$lib/playback.svelte";

  // 位置事件的全局订阅:放这里而不是 AudioPlayer 里——组件一卸载订阅就没了,
  // 迷你条的进度会僵住。
  $effect(() => startPlaybackSubscriptions());
```

在模板末尾(现有布局内容之后)加:

```svelte
<MiniPlayer />
```

- [ ] **Step 3: 确认 i18n 键存在**

Run: `rg -n "notes.player.pause|notes.player.play|common.close" src/lib/i18n/dict/*.ts | head`
Expected: 三个键都有中英两份。若缺,在 `src/lib/i18n/dict/` 对应文件补上
(中文 `暂停`/`播放`/`关闭`,英文 `Pause`/`Play`/`Close`),再继续。

- [ ] **Step 4: 类型检查与测试**

Run: `npm run check && npm test`
Expected: 0 错 0 警;测试全绿。

- [ ] **Step 5: 提交**

```bash
git add src/lib/MiniPlayer.svelte src/routes/+layout.svelte src/lib/i18n/dict/
git commit -m "feat(playback): 全局迷你播放条

挂在 layout,切到非笔记页时显示正在播放的笔记。位置事件的全局订阅也放
layout——挂在 AudioPlayer 里的话组件一卸载进度就僵住。"
```

---

### Task 5: 笔记页传身份 + 回页续播

**Files:**
- Modify: `src/routes/notes/[id]/+page.svelte`

**Interfaces:**
- Consumes: Task 3 的 `noteId`/`title` props;Task 2 的 `playback`。

- [ ] **Step 1: 传身份给播放器**

找到(约 1548 行):

```svelte
            <AudioPlayer bind:this={player} tracks={playerTracks} {waveform} bind:currentMs={playerMs} bind:playing={playerPlaying} onLoaded={onPlayerLoaded} />
```

改为(补两个 prop;`note.meta.id`/`note.meta.title` 是该页既有的笔记数据字段,
若本页变量名不同则用本页实际持有笔记元数据的变量):

```svelte
            <AudioPlayer bind:this={player} tracks={playerTracks} {waveform} bind:currentMs={playerMs} bind:playing={playerPlaying} onLoaded={onPlayerLoaded} noteId={note?.meta.id} title={note?.meta.title} />
```

- [ ] **Step 2: onLoaded 里按会话续播**

在 `onPlayerLoaded` 函数内、既有 `pendingResume` 处理**之前**插入:

```ts
    // 从迷你条点标题回到本页:后端新内核从 0/paused 起(player.rs 里 Core 写死
    // cursor=0),位置不会自动续上,必须显式 seek。复用 pendingResume 通路,
    // 不新造机制。只在会话确实是本篇笔记时才接管。
    if (!pendingResume && playback.session?.noteId === note?.meta.id) {
      pendingResume = { ms: playback.currentMs, playing: playback.playing };
    }
```

并在该文件 import 处加:

```ts
  import { playback } from "$lib/playback.svelte";
```

- [ ] **Step 3: 类型检查**

Run: `npm run check`
Expected: 0 错 0 警。

- [ ] **Step 4: 真机验证续播**

Run: `npm run tauri dev`

操作:进一篇笔记 → 播放 → 切到设置页(迷你条出现)→ 点迷你条标题回笔记页。
Expected: 迷你条隐藏,完整播放器接管,**播放位置从离开时的位置继续**,不是从 0。

- [ ] **Step 5: 提交**

```bash
git add "src/routes/notes/[id]/+page.svelte"
git commit -m "feat(playback): 笔记页传身份并在回页时续播

后端新内核写死 cursor=0/playing=false,回页位置不会自动续上——复用
pendingResume 通路显式 seek 回会话位置。"
```

---

### Task 6: 录制与删除两条退出路径

**Files:**
- Modify: `src/lib/recording.svelte.ts`
- Modify: `src/lib/Sidebar.svelte`

**Interfaces:**
- Consumes: Task 2 的 `playback`。

- [ ] **Step 1: 开录前停播放**

在 `src/lib/recording.svelte.ts` 的 import 处加:

```ts
import { playback } from "$lib/playback.svelte";
import { invoke as invokeCore } from "@tauri-apps/api/core";
```

(若该文件已 import 了 `invoke`,复用它,不要重复引入。)

在 `async start()` 内、`await invoke("start_recording")` **之前**插入:

```ts
    // 开录即停回放:两条 cpal 链路技术上独立,但正在播放的旧笔记会被 system 轨
    // 录进去,还可能经扬声器串进 mic——是内容污染,不是"互不干扰"。
    if (playback.session) {
      await invoke("player_stop", {}).catch(() => {});
      playback.clear();
    }
```

- [ ] **Step 2: 删除笔记前停播放**

在 `src/lib/Sidebar.svelte` 的 import 处加:

```ts
  import { playback } from "$lib/playback.svelte";
```

在删除确认(`const yes = await ask(...)`)通过之后、调用删除命令**之前**插入:

```ts
    // 删除前必须先停:音轨是 mmap,Unix 上已删文件会继续播,Windows 上活动映射
    // 还可能让删除本身失败。必须覆盖"从侧栏删除非当前页笔记"这条路径。
    if (playback.session?.noteId === id) {
      await invoke("player_stop", {}).catch(() => {});
      playback.clear();
    }
```

(`id` 用该删除处理函数实际持有的笔记 id 变量名。)

- [ ] **Step 3: 类型检查与测试**

Run: `npm run check && npm test`
Expected: 0 错 0 警;测试全绿。

- [ ] **Step 4: 真机验证两条路径**

Run: `npm run tauri dev`

1. 播放一篇笔记 → 切到别的页(迷你条在)→ 开始录制。
   Expected: 播放停止、迷你条消失、录制正常开始。
2. 播放一篇笔记 → 切到别的页 → 从侧栏删除**正在播放的那篇**。
   Expected: 播放先停、迷你条消失、删除成功无报错。

- [ ] **Step 5: 提交**

```bash
git add src/lib/recording.svelte.ts src/lib/Sidebar.svelte
git commit -m "feat(playback): 开录与删除前停止后台播放

开录:播放内容会被 system 轨录进去(内容污染,不是互不干扰)。
删除:音轨是 mmap,Unix 上已删文件继续可播,Windows 上活动映射可能
让删除失败;必须覆盖从侧栏删除非当前页笔记的路径。"
```

---

### Task 7: 改名同步 + 全量冒烟

**Files:**
- Modify: `src/lib/playback.svelte.ts`(订阅 `note_renamed`)

**Interfaces:**
- Consumes: 既有 `note_renamed` 事件。

- [ ] **Step 1: 在 store 里订阅改名事件**

事件已确认存在:`src/lib/events.ts` 的 `onNoteRenamed`,负载
`{ note_id: string; title: string }`。

在 `src/lib/playback.svelte.ts` 顶部 import 处改为:

```ts
import { onNoteRenamed, onPlayerPos } from "$lib/events";
```

把 `startPlaybackSubscriptions` 改为同时订阅两个事件:

```ts
export function startPlaybackSubscriptions(): () => void {
  const unPos = onPlayerPos((e) => playback.applyPos(e));
  const unRename = onNoteRenamed((e) => playback.rename(e.note_id, e.title));
  return () => {
    void Promise.resolve(unPos).then((f) => f());
    void Promise.resolve(unRename).then((f) => f());
  };
}
```

- [ ] **Step 2: 类型检查与测试**

Run: `npm run check && npm test`
Expected: 0 错 0 警;测试全绿(Task 2 已有 `rename` 的单测)。

- [ ] **Step 3: 全量冒烟**

Run: `npm run tauri dev`,逐条走 spec 的冒烟清单:

- [ ] 笔记页播放 → 切设置页,音频继续,底部出现迷你条
- [ ] 迷你条 ⏸/▶ 有效,进度在走
- [ ] 点迷你条标题 → 跳回笔记,迷你条隐藏,**播放位置续上**
- [ ] **只进笔记页不点播放,离开后不出现迷你条**
- [ ] 播放中打开另一篇笔记 → 旧的停止,迷你条消失,新笔记未播放
- [ ] 播放中**快速连点两篇笔记** → 不出现"迷你条显示 A 但按钮控制 B"
- [ ] **后台播放时切换 A/B 音轨方案** → 播放不中断、迷你条进度不僵住(Task 3 实现者提出:装载 effect 的 cleanup 在切轨时也会跑,该路径只做过静态推演,未真机验证)
- [ ] 迷你条 ✕ → 停止且消失
- [ ] 播到结尾 → 迷你条仍在且可重播
- [ ] 播放中开始录制 → 播放停止、迷你条消失
- [ ] 播放中从侧栏删除这篇笔记 → 先停播放,删除成功
- [ ] 后台播放期间改名 → 迷你条标题跟着更新

- [ ] **Step 4: 提交**

```bash
git add src/lib/playback.svelte.ts
git commit -m "feat(playback): 迷你条标题跟随笔记改名

后台播放期间改名,迷你条不订阅就会一直显示旧名。"
```

---

## 已知不做(spec 明确列出)

- **托盘常驻下关主窗口**:窗口只隐藏、进程还在,音频继续播。这是刻意的(与音乐 App
  一致),用户重开窗口即可用迷你条停止。托盘菜单加"停止播放"项列为后续。
- **迷你条进度拖拽**:刻意不做,要精确跳转就点标题回笔记页。
- **并发路径的自动化测试**:前端无组件测试基建,不为此新建;A→B 快速切换等靠真机
  冒烟(已列进 Task 7)。
