# 回放增益归一化(A1)Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 打开录得很轻的老笔记时,回放侧用一个可关的开关把整条录音响度抬到接近正常水平。

**Architecture:** 纯前端。`el.volume` 封顶 1.0 无法放大,故在 `AudioPlayer.svelte` 里让每轨 `<audio>` 经 `MediaElementAudioSourceNode` 汇入一个共享 `GainNode` 再到扬声器;增益由纯函数 `computeNoteGain(tracks)` 从各轨 `waveform`(0..255 绝对峰值桶)算出,只增不减。开关默认开、状态存 localStorage。

**Tech Stack:** Svelte 5 (runes)、Web Audio API、TypeScript、vitest(本功能新引入,给纯函数兜底)。

## Global Constraints

- 零后端改动;不改磁盘音频/波形;不做数据回填。
- 只增不减:好录音(峰值近满幅)增益必须钳到 `1.0`。
- 增益量纲 = 0..255 绝对峰值桶(`v = |i16sample| >> 7`,255=满幅)。
- 增益常量集中在 `src/lib/gain.ts` 顶部:`TARGET = 170`、`CEILING = 250`、`MAX_BOOST = 8`(初拟,冒烟可调)。
- 不引入除 vitest 外的新依赖;`computeNoteGain` 只用 `import type`,不产生 `$lib` 运行时解析。
- UI 复用现有 `.track-toggle` 胶囊样式与 DESIGN.md 现有 token,不引新组件、不使用 Unicode 符号字符。
- 类型检查:每个改动后 `npm run check` 必须 0 error。

---

### Task 1: vitest 基座 + `computeNoteGain` 纯函数

**Files:**
- Create: `src/lib/gain.ts`
- Create: `src/lib/gain.test.ts`
- Create: `vitest.config.ts`
- Modify: `package.json`(devDependency `vitest` + `scripts.test`)

**Interfaces:**
- Consumes: `TrackInfo`(来自 `src/lib/notes.ts`,字段 `waveform?: number[] | null`)。
- Produces:
  - `computeNoteGain(tracks: TrackInfo[]): number` —— 返回应用到共享 GainNode 的增益,恒 `≥ 1`。
  - `export const TARGET: number`、`export const CEILING: number`、`export const MAX_BOOST: number`。

- [ ] **Step 1: 安装 vitest**

Run:
```bash
npm install -D vitest
```
Expected: `package.json` devDependencies 出现 `vitest`;无 peer 冲突报错(Vite 6 / Node 20 兼容 vitest 3)。

- [ ] **Step 2: 建 vitest 配置**

Create `vitest.config.ts`:
```ts
import { defineConfig } from "vitest/config";

// 纯函数单测:node 环境即可,无需 jsdom / SvelteKit 插件链。
export default defineConfig({
  test: {
    include: ["src/**/*.test.ts"],
    environment: "node",
  },
});
```

- [ ] **Step 3: 加 test 脚本**

Modify `package.json` 的 `scripts`,在 `"check"` 旁加一行:
```json
"test": "vitest run",
```

- [ ] **Step 4: 写失败测试**

Create `src/lib/gain.test.ts`:
```ts
import { describe, it, expect } from "vitest";
import { computeNoteGain, MAX_BOOST, CEILING } from "./gain";
import type { TrackInfo } from "./notes";

const track = (waveform: number[] | null): TrackInfo => ({
  source: "mic",
  path: "x",
  offset_ms: 0,
  duration_ms: 1000,
  waveform,
});

describe("computeNoteGain", () => {
  it("放大很轻的笔记:gain>1 且放大后峰值不削波", () => {
    const g = computeNoteGain([track(new Array(260).fill(20))]);
    expect(g).toBeGreaterThan(1);
    expect(20 * g).toBeLessThanOrEqual(CEILING);
  });

  it("已够响的笔记:gain 钳到 1(只增不减)", () => {
    const g = computeNoteGain([track(new Array(260).fill(250))]);
    expect(g).toBe(1);
  });

  it("全静音笔记:gain=1(除零守卫)", () => {
    expect(computeNoteGain([track(new Array(260).fill(0))])).toBe(1);
  });

  it("某轨无波形:gain=1(不猜)", () => {
    expect(computeNoteGain([track(null)])).toBe(1);
  });

  it("空 tracks:gain=1", () => {
    expect(computeNoteGain([])).toBe(1);
  });

  it("极轻音频:gain 不超过 MAX_BOOST", () => {
    const g = computeNoteGain([track(new Array(260).fill(1))]);
    expect(g).toBeLessThanOrEqual(MAX_BOOST);
  });

  it("整条笔记一个增益:一条近满幅的轨把整条钳到 1", () => {
    const g = computeNoteGain([
      track(new Array(260).fill(20)),
      track(new Array(260).fill(250)),
    ]);
    expect(g).toBe(1);
  });
});
```

- [ ] **Step 5: 运行测试确认失败**

Run:
```bash
npm test
```
Expected: FAIL —— 无法解析 `./gain`(模块不存在)。

- [ ] **Step 6: 写 `computeNoteGain`**

Create `src/lib/gain.ts`:
```ts
import type { TrackInfo } from "./notes";

// 增益常量(0..255 绝对峰值桶量纲;冒烟可调)。
export const TARGET = 170; // 目标响度代理:≈良好录音的常态电平
export const CEILING = 250; // 放大后峰值上限,留余量不削顶(<255)
export const MAX_BOOST = 8; // 最大放大倍数:防把噪声地板轰起来

// 响度代理取非零桶的 90 百分位,避开单个瞬态尖峰。
const LOUDNESS_PERCENTILE = 0.9;

/**
 * 回放响度归一化增益:整条笔记一个增益,只增不减。
 * 输入各轨 waveform(0..255 绝对峰值桶)。数据不足 / 已够响时返回 1(不归一)。
 */
export function computeNoteGain(tracks: TrackInfo[]): number {
  const buckets: number[] = [];
  for (const t of tracks) {
    if (!t.waveform) continue;
    for (const v of t.waveform) buckets.push(v);
  }
  const nonzero = buckets.filter((v) => v > 0);
  if (nonzero.length === 0) return 1; // 无有效样本 / 全静音:不归一

  const peak = Math.max(...buckets); // 绝对峰值(0..255)
  if (peak <= 0) return 1;

  const sorted = [...nonzero].sort((a, b) => a - b);
  const idx = Math.min(sorted.length - 1, Math.floor(sorted.length * LOUDNESS_PERCENTILE));
  const loud = sorted[idx]; // 响度代理

  // CEILING/peak 保证放大后峰值 < 255,构造上不削波,无需限幅器。
  const gain = Math.min(TARGET / loud, CEILING / peak, MAX_BOOST);
  return Math.max(1, gain); // 只增不减
}
```

- [ ] **Step 7: 运行测试确认通过**

Run:
```bash
npm test
```
Expected: PASS(7 passed)。

- [ ] **Step 8: 类型检查**

Run:
```bash
npm run check
```
Expected: 0 error, 0 warning。

- [ ] **Step 9: 提交**

```bash
git add src/lib/gain.ts src/lib/gain.test.ts vitest.config.ts package.json package-lock.json
git commit -m "feat(playback): computeNoteGain 纯函数 + vitest 基座"
```

---

### Task 2: AudioPlayer 接 WebAudio 增益链

**Files:**
- Modify: `src/lib/AudioPlayer.svelte`(`<script>` 段 + `play()`/`pause()` + 卸载 effect)
- 依赖运行:`localhost:1420`(`npm run tauri dev` 或 `npm run dev`)

**Interfaces:**
- Consumes: `computeNoteGain`、`TARGET/CEILING/MAX_BOOST`(Task 1);组件既有 `tracks: TrackInfo[]`、`els: (HTMLAudioElement|null)[]`、`play()`、`pause()`。
- Produces(供 Task 3 消费):
  - `let normalize = $state<boolean>(…)` —— 归一化开关状态(默认 true,读 localStorage)。
  - `const noteGain = $derived<number>(computeNoteGain(tracks))` —— 本条增益。
  - `function setNormalize(on: boolean): void` —— 切换并持久化,内部调 `applyGain()`。

- [ ] **Step 1: 引入依赖与状态**

在 `src/lib/AudioPlayer.svelte` 顶部 `import { formatTs … }` 之后加:
```ts
  import { computeNoteGain } from "$lib/gain";
```

在状态声明区(`let els = $state(...)` 附近)加:
```ts
  // ── 回放响度归一化(A1):el.volume 封顶 1.0 无法放大老笔记,经共享 GainNode 提升 ──
  const NORMALIZE_KEY = "vn.playbackNormalize";
  // 默认开(「自动」语义):localStorage 存 "0" 才关。SSR/无 localStorage 时按开。
  let normalize = $state(
    typeof localStorage !== "undefined" ? localStorage.getItem(NORMALIZE_KEY) !== "0" : true,
  );
  const noteGain = $derived(computeNoteGain(tracks));

  let audioCtx: AudioContext | null = null;
  let gainNode: GainNode | null = null;
  // 每个 <audio> 只能建一次 MediaElementSource;按元素缓存,tracks 变化时增量重连。
  const srcNodes = new Map<HTMLAudioElement, MediaElementAudioSourceNode>();

  function ensureGraph() {
    if (typeof AudioContext === "undefined") return;
    if (!audioCtx) {
      audioCtx = new AudioContext();
      gainNode = audioCtx.createGain();
      gainNode.connect(audioCtx.destination);
    }
    for (const el of els) {
      if (el && !srcNodes.has(el)) {
        const node = audioCtx.createMediaElementSource(el);
        node.connect(gainNode!);
        srcNodes.set(el, node);
      }
    }
  }

  function applyGain() {
    if (!audioCtx || !gainNode) return;
    const g = normalize ? noteGain : 1;
    // 平滑过渡防咔哒(~20ms 时间常数)。
    gainNode.gain.setTargetAtTime(g, audioCtx.currentTime, 0.02);
  }

  export function setNormalize(on: boolean) {
    normalize = on;
    if (typeof localStorage !== "undefined") localStorage.setItem(NORMALIZE_KEY, on ? "1" : "0");
    applyGain();
  }
```

- [ ] **Step 2: 首次播放建图 + 唤醒 context**

修改 `export function play()`:在函数体开头 `if (tracks.length === 0) return;` 之后插入:
```ts
    ensureGraph();
    if (audioCtx?.state === "suspended") void audioCtx.resume();
    applyGain();
```

- [ ] **Step 3: tracks/增益变化时重连并重算增益**

在卸载 effect(`$effect(() => { void tracks; return () => pause(); })`)之后加一个 effect:
```ts
  // tracks 变化(续录/transcode_done 重拉音轨)后:已建图则增量接新 <audio> 并重算增益。
  $effect(() => {
    void noteGain; // 追踪 tracks → noteGain
    if (audioCtx) {
      ensureGraph();
      applyGain();
    }
  });
```

- [ ] **Step 4: 卸载时关闭 AudioContext**

修改卸载 effect,把清理从 `return () => pause();` 改为:
```ts
  $effect(() => {
    void tracks;
    return () => {
      pause();
      srcNodes.clear();
      void audioCtx?.close();
      audioCtx = null;
      gainNode = null;
    };
  });
```

- [ ] **Step 5: 类型检查**

Run:
```bash
npm run check
```
Expected: 0 error, 0 warning。

- [ ] **Step 6: 浏览器冒烟——增益已施加**

启动 `npm run tauri dev`,打开一条**已知很轻**的老笔记(输入音量 30 时代;若不确定哪条,挑 `duration` 有值且回放偏小声的早期笔记),点击播放。

在应用内(devtools console 或 Playwright `evaluate`)验证:
```js
// 页面里应有唯一 AudioContext 的 gainNode 值 ≈ computeNoteGain 结果
// 通过听感确认:明显比未接线时响,且不破音/不失真。
```
Expected: 老笔记明显变响、不失真;播放/暂停/拖拽定位/多轨同步一切照旧。

- [ ] **Step 7: 浏览器冒烟——静音仍生效(关键回归)**

在一条**双轨**笔记上,点某轨「麦克风/系统声」静音胶囊,播放。
Expected: 被静音的轨真的没声。
**若仍有声**(说明 `el.muted` 经 WebAudio 路由失效),应用以下退化补丁——每轨串一个 muteGain:
```ts
  // 退化:el.muted 经图路由失效时,用每轨 gain 节点做静音。
  const muteNodes = new Map<HTMLAudioElement, GainNode>();
  // ensureGraph 内把 node.connect(gainNode!) 改为:
  //   const mg = audioCtx.createGain();
  //   node.connect(mg); mg.connect(gainNode!);
  //   muteNodes.set(el, mg);
  // 并在 toggleMute()/syncTracks() 里对每轨:
  //   muteNodes.get(el)?.gain.setValueAtTime(muted[source] ? 0 : 1, audioCtx.currentTime);
```
(el.muted 生效则无需此补丁。)

- [ ] **Step 8: 提交**

```bash
git add src/lib/AudioPlayer.svelte
git commit -m "feat(playback): AudioPlayer 接 WebAudio 共享增益链"
```

---

### Task 3: 「响度」开关 UI + 可见性门控

**Files:**
- Modify: `src/lib/AudioPlayer.svelte`(模板控制行 + `<style>`)
- 依赖运行:`localhost:1420`

**Interfaces:**
- Consumes: `normalize`、`noteGain`、`setNormalize()`(Task 2)。
- Produces: 无(终端 UI)。

- [ ] **Step 1: 加开关按钮**

在模板里,`{#if tracks.length > 1} …track-toggles… {/if}` 块**之后**、`<span class="time">` **之前**,加:
```svelte
  {#if noteGain > 1}
    <!-- 响度归一化开关:仅当本条真能被放大时出现,避免死开关。默认开。 -->
    <button
      class="norm-toggle"
      class:off={!normalize}
      onclick={() => setNormalize(!normalize)}
      title={normalize ? "关闭响度归一化(听原始电平)" : "打开响度归一化(把偏轻的录音抬到正常响度)"}
    >响度</button>
  {/if}
```

- [ ] **Step 2: 加样式**

在 `<style>` 里 `.track-toggle.off { … }` 规则之后加:
```css
  /* 响度开关:复用 track-toggle 胶囊语言;off=划线退灰 */
  .norm-toggle {
    border: 1px solid var(--hairline-strong);
    background: transparent;
    color: var(--ink-secondary);
    border-radius: var(--radius-full);
    padding: 0.15em 0.7em;
    font-size: 0.75rem;
    cursor: pointer;
    white-space: nowrap;
    flex: none;
  }
  .norm-toggle:hover {
    background: var(--surface-soft);
    color: var(--ink);
  }
  .norm-toggle.off {
    text-decoration: line-through;
    color: var(--ink-faint);
    border-style: dashed;
  }
```

- [ ] **Step 3: 类型检查**

Run:
```bash
npm run check
```
Expected: 0 error, 0 warning。

- [ ] **Step 4: 浏览器冒烟——开关行为**

`npm run tauri dev`:
- 打开**很轻的老笔记**:开关出现且默认「开」(未划线);播放确认响;点开关变「关」(划线)→ 播放回原始小声;刷新页面 → 仍「关」(localStorage 记忆)。
- 打开**AGC 之后的正常笔记**:`noteGain` 钳到 1 → 开关**不出现**。
- 切换开关时无咔哒。

Expected: 全部符合。

- [ ] **Step 5: 真机冒烟(权威验证)**

在真实 app 里复核 Task 2/3 全链:老笔记变响不失真、正常笔记无变化无开关、静音仍生效、续录一条停录(音轨重建)后归一化仍正确。

- [ ] **Step 6: 提交**

```bash
git add src/lib/AudioPlayer.svelte
git commit -m "feat(playback): 响度归一化开关 UI + 可见性门控"
```

---

## 收尾

- [ ] 全量测试:`npm test` 通过、`npm run check` 0/0。
- [ ] `git log --oneline` 复核三个提交。
- [ ] 若开关样式值得登记,DESIGN.md 补一行(复用现有胶囊则免)。
- [ ] 推分支 `playback-gain-normalization` → 开 PR(等用户真机冒烟后 squash 合入 master)。
