# PostHog Tauri WebView 可行性 Spike 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 用一次性代码验证 `posthog-js` 能否在 Tauri WebView 中工作，逐条裁决五项判据，产出书面结论决定阶段 2~4 是否继续、以及回放要不要砍。

**Architecture:** 在 `+layout.svelte` 的 `onMount` 里初始化一个 spike 专用的 PostHog 客户端（应用是 SPA，`ssr = false`，无服务端渲染需要考虑）。全部 spike 代码集中在一个新文件加一处调用点，便于整体回滚。不改任何既有业务逻辑，不动 Rust 侧。

**Tech Stack:** SvelteKit（SPA 模式）、Tauri 2、posthog-js、PostHog Cloud 免费档。

## Global Constraints

- **本分支不合入 master。** spike 代码是一次性的，结论落在裁决文档里，代码本身丢弃。分支名 `spike/posthog-tauri`。
- **交付物是裁决文档**，不是能用的功能。文档路径 `docs/superpowers/research/2026-08-17-posthog-tauri-spike.md`。
- **判据 4（遮蔽实测）是硬门**：不通过则会话回放整块砍掉，阶段 4 取消，阶段 2/3 仍可继续。
- **遮蔽实测不得使用真实会议记录**。用自造的哨兵笔记，内容见 Task 5。
- posthog-js 的配置字段名**以官方隐私文档为准**（https://posthog.com/docs/session-replay/privacy ），本计划中给出的字段名未经联网核实，每个涉及配置的任务第一步都要先核对文档。字段名对不上以文档为准，并在裁决文档里记下实际字段。
- git 提交信息不加任何 Claude 署名尾注。
- 前端检查在仓库根跑 `npm run check`；应用启动用 `npm run tauri dev`。
- 注释风格跟随仓库现状：中文，说明「为什么」。

## 文件结构

| 文件 | 职责 |
|---|---|
| `src/lib/spikePosthog.ts`（新建） | spike 全部逻辑：读凭证、init、暴露手工触发事件的函数。一处集中，回滚时删这一个文件加一处调用 |
| `src/routes/+layout.svelte`（修改，`onMount` 内） | 唯一调用点 |
| `.env.local`（新建，已被 gitignore 覆盖需确认） | PostHog 凭证，不入库 |
| `docs/superpowers/research/2026-08-17-posthog-tauri-spike.md`（新建） | 裁决文档，唯一要保留的产物 |

---

### Task 1: PostHog 项目与凭证（需用户参与）

**Files:**
- Create: `.env.local`

**Interfaces:**
- Produces: 环境变量 `VITE_POSTHOG_KEY`（project API key）与 `VITE_POSTHOG_HOST`（区域 host），后续任务经 `import.meta.env` 消费。

- [ ] **Step 1: 用户创建 PostHog 项目**

用户在 https://posthog.com 注册免费档 → 新建 project → 在 Project Settings 复制 **Project API Key**（形如 `phc_xxxxxxxx`）。注册时会选区域（US 或 EU），对应 host 分别是 `https://us.i.posthog.com` 与 `https://eu.i.posthog.com`——**必须与注册区域一致，选错会连不上**。

此步只能由用户完成。

- [ ] **Step 2: 确认 .env.local 不会入库**

Run: `git check-ignore -v .env.local`
Expected: 输出一行 gitignore 规则（说明已被忽略）。

若无输出，说明没被忽略，先把 `.env.local` 加进 `.gitignore` 再继续：

```bash
echo ".env.local" >> .gitignore
git add .gitignore && git commit -m "gitignore 增加 .env.local"
```

Project API Key 是写进客户端的公开标识（等同前端可见的写入端点），不是机密；放 `.env.local` 只是为了让 spike 代码不带环境细节，不是保密措施。

- [ ] **Step 3: 写入凭证**

创建 `.env.local`：

```
VITE_POSTHOG_KEY=phc_替换成真实key
VITE_POSTHOG_HOST=https://us.i.posthog.com
```

- [ ] **Step 4: 建分支**

```bash
git checkout master && git pull
git checkout -b spike/posthog-tauri
```

---

### Task 2: 最小接入，裁决判据 1 与判据 2

**判据 1**：posthog-js 能在 WebView 里初始化并成功发出事件。
**判据 2**：请求能到达 PostHog 域（当前 `tauri.conf.json` 的 `csp` 为 `null`，预期无 CSP 阻挡，但自定义协议来源是否被接受必须实测）。

**Files:**
- Create: `src/lib/spikePosthog.ts`
- Modify: `src/routes/+layout.svelte`（`onMount` 内）

**Interfaces:**
- Consumes: Task 1 的 `VITE_POSTHOG_KEY` / `VITE_POSTHOG_HOST`。
- Produces: `initSpikePosthog(): void`、`spikeCapture(name: string): void`，Task 3~5 直接复用。

- [ ] **Step 1: 装依赖**

Run: `npm i posthog-js`
Expected: 安装成功，`package.json` 出现 `posthog-js`。

- [ ] **Step 2: 核对配置字段名**

打开 https://posthog.com/docs/session-replay/privacy 与 https://posthog.com/docs/libraries/js ，核对下一步代码里用到的字段：`api_host`、`autocapture`、`capture_pageview`、`disable_session_recording`、`session_recording.maskAllInputs`、`session_recording.maskTextSelector`、控制台日志采集开关。**以文档为准**，与下面代码不一致时改代码，并把实际字段名记进裁决文档草稿。

- [ ] **Step 3: 写 spike 模块**

创建 `src/lib/spikePosthog.ts`：

```ts
/** PostHog 可行性 spike 的全部逻辑,一次性代码,不合入 master。
 *
 *  集中在单文件是为了回滚干净:删本文件 + +layout.svelte 里那一处调用即可。
 *  阶段 1 只回答「在 Tauri WebView 里能不能跑」,不做任何产品埋点设计。 */
import posthog from "posthog-js";

const KEY = import.meta.env.VITE_POSTHOG_KEY as string | undefined;
const HOST = import.meta.env.VITE_POSTHOG_HOST as string | undefined;

let started = false;

export function initSpikePosthog(): void {
  if (started) return;
  if (!KEY || !HOST) {
    console.warn("[spike] 缺 VITE_POSTHOG_KEY / VITE_POSTHOG_HOST,跳过");
    return;
  }
  posthog.init(KEY, {
    api_host: HOST,
    // 判据 5 要看点击能否带出元素选择器,故开 autocapture
    autocapture: true,
    capture_pageview: true,
    // 回放在 Task 4 才打开,先确认基础事件通路
    disable_session_recording: true,
    loaded: () => console.log("[spike] posthog loaded"),
  });
  started = true;
}

/** 手工事件:用于确认「确实是我们发出去的」,而不是被 autocapture 撞对。 */
export function spikeCapture(name: string): void {
  posthog.capture(name, { spike: true });
}
```

- [ ] **Step 4: 接线**

`src/routes/+layout.svelte`：在 `<script lang="ts">` 的 import 区末尾加：

```ts
  import { initSpikePosthog, spikeCapture } from "$lib/spikePosthog";
```

在该文件已有的 `onMount(...)` 回调体的**最开头**插入：

```ts
    initSpikePosthog();
    spikeCapture("spike_app_started");
```

- [ ] **Step 5: 类型检查**

Run: `npm run check`
Expected: 0 errors。（基线是 0 error / 0 warning，不得新增。）

- [ ] **Step 6: 跑起来看事件**

Run: `npm run tauri dev`

在应用里点几下、切两个页面，然后打开 PostHog 后台 → **Activity**（实时事件流），观察 1~2 分钟。

Expected：
- 出现 `spike_app_started` 事件 → **判据 1 通过**
- 事件属性里 `$current_url` 有值（可能形如 `tauri://localhost/...` 或 `http://localhost:1420/...`），且事件确实到达 → **判据 2 通过**

若一条都没有：打开 WebView 开发者工具的 Network 面板，看到 PostHog 域的请求了吗？

- 有请求但失败 → 记下 HTTP 状态与错误信息，多半是来源/CSP 问题，**判据 2 失败**
- 连请求都没发出 → 多半是 init 没跑或 key 没读到，先看 Console 里有没有 `[spike] posthog loaded`

把观察到的现象（成功或失败的具体表现）记进裁决文档草稿，不要只记「通过/不通过」。

- [ ] **Step 7: Commit**

```bash
git add package.json package-lock.json src/lib/spikePosthog.ts src/routes/+layout.svelte
git commit -m "spike: posthog-js 最小接入,验证 WebView 内初始化与事件投递"
```

---

### Task 3: 裁决判据 5（热图所需的点击数据）

**判据 5**：热图能拿到点击坐标与元素。

**Files:**
- Modify: 无（Task 2 已开 `autocapture`）

**Interfaces:**
- Consumes: Task 2 的 `initSpikePosthog`。

- [ ] **Step 1: 产生点击**

Run: `npm run tauri dev`

依次点击：侧边栏三个入口、录制页的开录按钮（不必真录）、设置页的任一开关。

- [ ] **Step 2: 在 PostHog 里查验**

PostHog 后台 → Activity，筛选事件类型 `$autocapture`。

Expected：每次点击对应一条 `$autocapture`，展开后属性里有 `$element_selector`（或 `$elements` 数组，含 tag/class/text 等）。

- [ ] **Step 3: 判定并记录**

- 有 `$autocapture` 且带元素信息 → **判据 5 通过**
- 有事件但元素信息为空 → 热图会拿到点击数但定位不到元素，**判据 5 部分通过**，在裁决文档里如实写「热图价值受限」，不要含糊记成通过
- 完全没有 → **判据 5 失败**

**注意**：此处 `$autocapture` 的元素属性可能带按钮上的**文字**。这与阶段 2 的隐私红线相关（按钮文案是界面文本，不是会议内容，可接受；但若某个 chip 的文字是**说话人真实姓名**，就是内容泄漏）。实测时特意点一下说话人 chip，看属性里有没有出现人名，结论记进裁决文档——这一条会直接影响阶段 2 的 autocapture 配置。

- [ ] **Step 4: 建裁决文档草稿并提交**

此时创建 `docs/superpowers/research/2026-08-17-posthog-tauri-spike.md`，先落判据 1/2/5 的实测结果（Task 7 再成文）：

```markdown
# PostHog Tauri WebView Spike 裁决

日期：2026-08-17
状态：进行中

## 判据实测记录

### 判据 1 · posthog-js 能否初始化并发出事件
结论：<通过 / 失败>
证据：<观察到的事件名、时间、或失败时的 Console/Network 具体表现>

### 判据 2 · 请求能否到达 PostHog 域
结论：<通过 / 失败>
证据：<$current_url 的实际取值；失败时记 HTTP 状态与错误信息>

### 判据 5 · 点击能否带出元素信息（热图原料）
结论：<通过 / 部分通过 / 失败>
证据：<$autocapture 属性里 $element_selector 的实际内容>
人名泄漏检查：点击说话人 chip 后，属性里<是否>出现了人名 → 对阶段 2 的影响：<...>

### 判据 3 · 回放是否可播放
（Task 4 填写）

### 判据 4 · 全文本遮蔽是否生效（硬门）
（Task 5 填写）
```

```bash
git add docs/superpowers/research/2026-08-17-posthog-tauri-spike.md
git commit -m "spike: 记录判据 1/2/5 实测结果"
```

---

### Task 4: 裁决判据 3（回放可播放）

**判据 3**：能产出一条可播放的回放。

**Files:**
- Modify: `src/lib/spikePosthog.ts`

**Interfaces:**
- Consumes: Task 2 的模块。

- [ ] **Step 1: 打开回放**

`src/lib/spikePosthog.ts` 的 `posthog.init` 配置中，把

```ts
    disable_session_recording: true,
```

改为

```ts
    disable_session_recording: false,
```

- [ ] **Step 2: 产生一段会话**

Run: `npm run tauri dev`

连续操作约 60 秒：切换页面、滚动列表、打开一篇已有笔记、点开设置页。然后**关闭应用**（回放通常要会话结束或达到一定时长才完整上传）。

- [ ] **Step 3: 查验**

PostHog 后台 → Session Replay，等待最多 5 分钟。

Expected：出现一条录制，能播放，能看到页面布局与鼠标移动。

- [ ] **Step 4: 判定并记录**

- 能播且画面正常 → **判据 3 通过**
- 有记录但播放空白/严重错位 → **判据 3 失败**，记下具体表现（WebView 的 rrweb 兼容问题多半出在这里）
- 压根没有记录 → 先确认是不是没等够时间，再判失败

- [ ] **Step 5: Commit**

```bash
git add src/lib/spikePosthog.ts
git commit -m "spike: 打开会话回放,验证 WebView 内能否产出可播放记录"
```

---

### Task 5: 裁决判据 4（遮蔽实测）——硬门

**判据 4**：故意录一段含笔记正文的界面，回放里必须一个字都看不到。

**这是整个 spike 的关键任务。判据 4 不过，阶段 4（回放与热图）取消。**

**Files:**
- Modify: `src/lib/spikePosthog.ts`

**Interfaces:**
- Consumes: Task 4 的回放已打开。

- [ ] **Step 1: 核对遮蔽字段名**

再次打开 https://posthog.com/docs/session-replay/privacy ，确认下一步用到的字段名与写法。**PostHog 默认不遮普通文本、只遮输入框**——这一点必须在文档里得到确认，它是阶段 2 全部遮蔽设计的前提。若文档显示默认行为已变，如实记进裁决文档。

- [ ] **Step 2: 配上全遮蔽**

`src/lib/spikePosthog.ts` 的 init 配置里，把 `disable_session_recording: false,` 那行之后补上：

```ts
    session_recording: {
      // 默认只遮输入框、不遮普通文本——本应用满屏都是会议内容,必须全遮
      maskAllInputs: true,
      maskTextSelector: "*",
    },
    // 控制台日志可能打印笔记内容,关掉
    enable_recording_console_log: false,
```

字段名以 Step 1 核对结果为准。

- [ ] **Step 3: 造哨兵笔记**

在应用里新建（或改名）一篇笔记，让界面上同时出现三类哨兵串，**不要用真实会议内容**：

| 哨兵串 | 放在哪 | 模拟什么 |
|---|---|---|
| `ZZSENTINELTITLEZZ 季度复盘会` | 笔记标题 | 会议标题 |
| `ZZSENTINELNAMEZZ 张伟` | 说话人名 | 真实姓名 |
| `ZZSENTINELBODYZZ 这段话是用来验证回放遮蔽是否生效的正文` | 正文段落 | 会议逐字稿 |

用 `ZZ...ZZ` 前缀是为了能在回放和网络请求里**精确搜索**，而不是靠肉眼扫——肉眼容易漏。

- [ ] **Step 4: 录一段并检查网络请求体**

Run: `npm run tauri dev`

打开那篇哨兵笔记，停留 30 秒，滚动几下。同时打开 WebView 开发者工具 Network 面板，找到发往 PostHog 的回放上传请求，在请求体里搜 `ZZSENTINEL`。

Expected：**一次都搜不到**。

若请求体被压缩看不到明文，跳过这一步，以 Step 5 的回放实测为准，并在裁决文档里注明「请求体已压缩，未能在传输层验证」。

- [ ] **Step 5: 回放里逐屏检查**

关闭应用 → PostHog 后台 → Session Replay → 播放刚才那条。

Expected：
- 标题、说话人名、正文全部显示为遮蔽块（通常是 `*` 或色块）
- 用回放界面的搜索/DOM 探查功能搜 `ZZSENTINEL`，**零命中**

- [ ] **Step 6: 判定（硬门）**

- 三个哨兵串一个都没出现 → **判据 4 通过**，阶段 4 可以做
- **任何一个出现** → **判据 4 失败**。记下是哪一类漏的（标题/人名/正文）、在什么场景漏的。**结论是：会话回放整块砍掉，阶段 4 取消**，阶段 2/3 不受影响。不要尝试「再调调看能不能遮住」——遮蔽规则要靠一次实测建立信心，反复试出来的配置在下个版本的 UI 上一样会漏。

这条判定要写进裁决文档的显眼位置，不能只留一句「通过」。

- [ ] **Step 7: Commit**

```bash
git add src/lib/spikePosthog.ts
git commit -m "spike: 全文本遮蔽实测(判据 4 硬门)"
```

---

### Task 6: Windows 平台复验

macOS 用 WKWebView、Windows 用 WebView2，是两套完全不同的引擎，**在一个平台通过不能推断另一个平台**。

**Files:** 无

- [ ] **Step 1: 判断是否具备条件**

有可用的 Windows 机器吗？

- 有 → 走 Step 2
- 没有 → **跳到 Step 3**，不要假装验过

- [ ] **Step 2: 在 Windows 上重跑判据 1/2/3/4**

在 Windows 机器上拉 `spike/posthog-tauri` 分支，补一份 `.env.local`（内容同 Task 1），跑 `npm run tauri dev`，重复 Task 2 Step 6、Task 4 Step 3、Task 5 Step 5。

Expected：四条判据结论与 macOS 一致。任何一条不一致都要单独记录——尤其判据 4，遮蔽在两个引擎上的表现可能不同。

- [ ] **Step 3: 无 Windows 机器时的处理**

在裁决文档里写明：**本次裁决只对 macOS/WKWebView 有效，Windows/WebView2 未验证**，并把它列为阶段 2 上线前必须补的一项。

不要因为「大概也能跑」就把结论写成全平台通过——这正是当初 `hw_gaps` 被误读成断流计数的同一类错误：把未经验证的推断当成已知事实往下传。

---

### Task 7: 写裁决文档，清理 spike 代码

**Files:**
- Create: `docs/superpowers/research/2026-08-17-posthog-tauri-spike.md`

- [ ] **Step 1: 成文**

创建裁决文档，必须包含：

1. **五条判据逐条结论**：通过/部分通过/失败，各附实测证据（事件名、回放链接、错误信息、截图路径）。
2. **总裁决**：阶段 2/3 是否继续、阶段 4（回放热图）做还是砍。
3. **平台覆盖**：验了哪些平台，没验的明确写出来。
4. **实际配置字段名**：与本计划中未核实的写法有出入的，记下正确的，供阶段 2 直接抄。
5. **autocapture 的人名泄漏结论**（Task 3 Step 3 那条），直接影响阶段 2 的配置。
6. **未解决的疑问**：spike 只回答被问到的问题，途中冒出的新问题列出来，不要装作没看见。

- [ ] **Step 2: 提交文档**

```bash
git add docs/superpowers/research/2026-08-17-posthog-tauri-spike.md
git commit -m "spike 裁决:posthog-js 在 Tauri WebView 的五条判据结论"
```

- [ ] **Step 3: 把裁决文档挪到 master**

spike 代码丢弃，但结论要留下：

```bash
git checkout master
git checkout spike/posthog-tauri -- docs/superpowers/research/2026-08-17-posthog-tauri-spike.md
git add docs/superpowers/research/2026-08-17-posthog-tauri-spike.md
git commit -m "记录 PostHog Tauri spike 裁决"
```

- [ ] **Step 4: 确认 master 干净**

Run: `git status --short && git log --oneline -1`
Expected：工作区干净；master 上**只有裁决文档**这一个提交，没有 `spike/posthog-tauri` 的代码（`src/lib/spikePosthog.ts` 不应存在于 master）。

Run: `ls src/lib/spikePosthog.ts`
Expected: `No such file or directory`。

- [ ] **Step 5: 保留分支备查**

`spike/posthog-tauri` 分支保留不删，将来阶段 2 实施时可回看当时的具体配置。不推送到 origin（含 `.env.local` 的风险不值得冒，虽然它已被 gitignore）。

---

## 完成判据

本计划完成 = master 上有一份裁决文档，文档能回答：**阶段 2/3 能不能开始做，阶段 4 是做还是砍。**

代码交付为零，这是预期结果。spike 的产出是决策依据，不是功能。
