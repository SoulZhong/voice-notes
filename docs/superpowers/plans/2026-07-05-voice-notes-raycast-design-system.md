# Raycast 化设计系统(DESIGN.md v2)实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 全应用视觉层切换为 Raycast 语言:近黑表面阶梯、白/黑药丸主按钮、交互蓝、饱和语义色、紧圆角、标题 500 字重;双主题(暗色取原值、亮色反推)。

**Architecture:** 纯 token 驱动重皮肤——app.css 是唯一取值点(PR #9 已收敛硬编码),token 名不换只换值;新增 primary 三 token(主按钮从 accent 实底解耦)与七个徽章文字色 token。组件层只有三类点改:主按钮换 primary、字重 600/700→500、徽章补文字色。布局/交互/行为零改动。

**Tech Stack:** CSS 自定义属性(双主题 prefers-color-scheme)+ Svelte 组件微调。

**Spec:** `docs/superpowers/specs/2026-07-05-voice-notes-raycast-design-system-design.md`(token 全表以 spec §二 为真值源,本计划 Task 1 代码块与其一致)

## Global Constraints

- 分支 `raycast-design-system`(已建,基 master 8a43877),每任务一提交,最终 push→PR→squash。
- token 名不改(组件 diff 最小化);唯一例外是**新增** `--primary/--primary-pressed/--on-primary` 与 `--tint-*-ink`×7。
- 亮色全套是反推值:实现照抄本计划,值的好坏由冒烟裁决(改值只动 app.css+DESIGN.md,不返工组件)。
- 不引 webfont;字号不动,只动字重/注释;`npm run check` 0/0、`npm run build` 通过;无浏览器可截图时目检交给用户冒烟。
- 前端无单测框架:每任务验证 = check+build+grep 断言(计划里给出具体 grep)。

---

### Task 1: app.css 全量 token 重定值 + 新增 token

**Files:**
- Modify: `src/app.css`

**Interfaces:**
- Produces:新增 `--primary/--primary-pressed/--on-primary`(Task 2 消费)、`--tint-sky-ink` 等七个徽章文字色(Task 3 消费);全部既有 token 新值。

- [ ] **Step 1: 用下列内容整体替换 app.css 的 `:root` 与 `@media (prefers-color-scheme: dark)` 两块**(文件头注释同步改写:「数值来自 DESIGN.md(Raycast 化命令面板质感)…」;质感层的 h1/transition/active 规则保留,仅 h1 `font-weight: 700` → `500`):

```css
:root {
  /* 亮色 = Raycast 暗色灰阶的极性反推(Raycast 无官方亮色),冒烟后可调 */
  --canvas: #fafafa;
  --surface: #f1f1f2;
  --surface-soft: #ebebec;
  --surface-press: #e3e3e5;
  --hairline: #e4e5e6;
  --hairline-strong: #c9cacc;
  --ink: #18191a;
  --ink-secondary: #5c5d5f;
  --ink-faint: #949597;

  /* accent 只表达链接/焦点/选中;主按钮走 primary 药丸(Raycast 签名) */
  --accent: #0f7fd1;
  --accent-pressed: #0c6ab0;
  --accent-tint: rgba(15, 127, 209, 0.1);
  --on-accent: #ffffff;

  /* 主按钮药丸:亮色黑底白字(暗色白底黑字的极性翻转) */
  --primary: #18191a;
  --primary-pressed: #2c2d2f;
  --on-primary: #ffffff;

  --danger: #d63a3a;
  --danger-tint: #fdecec;
  --danger-line: #f3c6c6;
  --danger-ink: #9b1c1c;

  /* 录制红点:Raycast accent-red,双主题一致,仍是唯一常驻彩色信号 */
  --record: #ff6161;

  --warning-tint: #fef6de;
  --warning-ink: #7a5a0e;
  --warning-line: #f0e0ac;

  --success: #1d9e63;

  /* 说话人徽章:饱和色 15% alpha soft 底 + 同色相深文字(soft 公式,双主题同底)。
     红色不进池(独占录制/danger 语义);tint-peach 名下语义改为青色(改名牵连组件,保名加注)。 */
  --tint-sky: rgba(87, 193, 255, 0.15);
  --tint-sky-ink: #0b6bb8;
  --tint-mint: rgba(89, 212, 153, 0.15);
  --tint-mint-ink: #157a4c;
  --tint-peach: rgba(79, 210, 201, 0.15);
  --tint-peach-ink: #0e7d74;
  --tint-lavender: rgba(178, 139, 244, 0.15);
  --tint-lavender-ink: #6d3fc2;
  --tint-rose: rgba(255, 122, 194, 0.15);
  --tint-rose-ink: #b8347e;
  --tint-yellow: rgba(255, 197, 51, 0.15);
  --tint-yellow-ink: #8a6510;
  --tint-gray: rgba(156, 156, 157, 0.15);
  --tint-gray-ink: #55565a;

  /* 圆角:Raycast 紧圆角(6-10px);药丸仅主按钮与录制点 */
  --radius-sm: 4px;
  --radius-md: 6px;
  --radius-lg: 8px;
  --radius-xl: 10px;
  --radius-full: 9999px;

  /* 深度靠表面阶梯不靠投影;浮层深阴影;shadow-btn 为主按钮药丸专用 */
  --shadow-popover: 0 8px 24px rgba(0, 0, 0, 0.16);
  --shadow-btn: inset 0 0 0 1px rgba(255, 255, 255, 0.12), 0 1px 2px rgba(0, 0, 0, 0.18);
}

@media (prefers-color-scheme: dark) {
  :root {
    /* 暗色是第一公民:Raycast 原值(近黑画布 + 表面阶梯 + 发丝线) */
    --canvas: #07080a;
    --surface: #0d0d0d;
    --surface-soft: #121212;
    --surface-press: #1a1b1c;
    --hairline: #242728;
    --hairline-strong: #3a3d40;
    --ink: #f4f4f6;
    --ink-secondary: #9c9c9d;
    --ink-faint: #6a6b6c;

    --accent: #57c1ff;
    --accent-pressed: #3fa9e8;
    --accent-tint: rgba(87, 193, 255, 0.15);
    --on-accent: #07080a;

    --primary: #ffffff;
    --primary-pressed: #e8e8e8;
    --on-primary: #18191a;

    --danger: #ff6161;
    --danger-tint: rgba(255, 97, 97, 0.12);
    --danger-line: rgba(255, 97, 97, 0.3);
    --danger-ink: #ffb4b4;

    --record: #ff6161;

    --warning-tint: rgba(255, 197, 51, 0.1);
    --warning-ink: #ffd980;
    --warning-line: rgba(255, 197, 51, 0.3);

    --success: #59d499;

    /* 徽章 soft 底双主题同值(15% alpha 对暗底同样成立),只切文字色 */
    --tint-sky-ink: #57c1ff;
    --tint-mint-ink: #59d499;
    --tint-peach-ink: #4fd2c9;
    --tint-lavender-ink: #b28bf4;
    --tint-rose-ink: #ff7ac2;
    --tint-yellow-ink: #ffc533;
    --tint-gray-ink: #c9c9ca;

    --shadow-popover: 0 8px 24px rgba(0, 0, 0, 0.5);
    --shadow-btn: inset 0 0 0 1px rgba(0, 0, 0, 0.2), 0 1px 2px rgba(0, 0, 0, 0.4);
  }
}
```

注意:暗色块里**不再重复** `--tint-sky` 等背景值与 `--radius-*`(双主题同值的 token 只在 `:root` 定义一次;现文件暗色块里有 7 个 tint 背景重定义,删除)。

- [ ] **Step 2: 验证**:`npm run check` 0/0、`npm run build` 通过;`grep -c "prefers-color-scheme" src/app.css` = 1;`grep -c -- "--primary" src/app.css` = 6(三 token × 双主题)。
- [ ] **Step 3: Commit**:`feat(design): app.css 全量切换 Raycast token(双主题+primary 药丸+徽章 soft 公式)`

---

### Task 2: 主按钮改 primary 药丸 + 字重全面降 500

**Files:**
- Modify: `src/lib/ModelDownloadCard.svelte`、`src/lib/Sidebar.svelte`、`src/routes/record/+page.svelte`、`src/routes/settings/+page.svelte`、`src/routes/speakers/+page.svelte`、`src/routes/notes/[id]/+page.svelte`

**Interfaces:**
- Consumes: Task 1 的 `--primary/--primary-pressed/--on-primary`。

- [ ] **Step 1: 主按钮换 token**。四个文件(`ModelDownloadCard`、`settings`、`record`、`speakers`)中所有 `background: var(--accent)` 的实底按钮改为:

```css
background: var(--primary);
color: var(--on-primary);
border-color: transparent;      /* 原样保留此行者不动 */
border-radius: var(--radius-full);  /* 药丸:Raycast 主按钮签名 */
```

hover/pressed 态 `var(--accent-pressed)` → `var(--primary-pressed)`。链接式/secondary 按钮不动(它们引用 hairline/accent 文本色,值换即适配)。

- [ ] **Step 2: 字重排查**。六个文件共 23 处 `font-weight: 600/700` 全部降为 `500`(Raycast 层级靠亮度对比不靠重字重;`app.css` 的 h1 已在 Task 1 降)。逐处核对无"降后与周边同重失层级"——列表标题/当前页条目原 600/700 与正文 400 之间仍差 100,层级保持。
- [ ] **Step 3: 验证**:`grep -rn "font-weight: [67]00" src --include="*.svelte"` 返回 0 行;`grep -rn "background: var(--accent)" src --include="*.svelte"` 返回 0 行;`npm run check` 0/0、`npm run build` 通过。
- [ ] **Step 4: Commit**:`feat(design): 主按钮 primary 药丸化,标题字重全面降 500`

---

### Task 3: 徽章文字色接 soft 公式

**Files:**
- Modify: `src/lib/notes.ts`、`src/lib/SpeakerChips.svelte`、`src/routes/notes/[id]/+page.svelte`、`src/routes/speakers/+page.svelte`

**Interfaces:**
- Consumes: Task 1 的 `--tint-*-ink`×7。
- Produces: `notes.ts` 新导出 `speakerInk(speaker, source): string`(与 `speakerColor` 同索引逻辑,返回 `var(--tint-X-ink)`)。

- [ ] **Step 1: notes.ts**。在既有色数组旁加平行文字色数组与函数(索引逻辑与 `speakerColor` 完全一致,含 `!speaker` 时 mic→sky/system→mint 的兜底):

```ts
const SPEAKER_INKS = [
  "var(--tint-sky-ink)",
  "var(--tint-mint-ink)",
  "var(--tint-peach-ink)",
  "var(--tint-lavender-ink)",
  "var(--tint-rose-ink)",
  "var(--tint-yellow-ink)",
  "var(--tint-gray-ink)",
];

/** 徽章文字色:与 speakerColor 同索引(soft 底配同色相文字,Raycast soft 公式)。 */
export function speakerInk(speaker: string | null, source: Source): string {
  if (!speaker) return source === "mic" ? "var(--tint-sky-ink)" : "var(--tint-mint-ink)";
  return SPEAKER_INKS[hashIndex(speaker)];
}
```

(`hashIndex` 指 `speakerColor` 现用的取索引逻辑;若它是内联表达式,抽成共用小函数,两处消费,不复制。)

- [ ] **Step 2: 三个消费点补 `color:`**。`SpeakerChips.svelte:53` 与 `notes/[id]/+page.svelte:335` 的 `style="background: {speakerColor(...)}"` 追加 `; color: {speakerInk(...)}"`(import 同步);`speakers/+page.svelte` 自持的色数组(第 30-36 行)改为从 `$lib/notes` import `speakerColor/speakerInk` 消费,删除本地重复数组(如其索引逻辑与 notes.ts 不同,以 notes.ts 为准统一)。
- [ ] **Step 3: 验证**:`grep -rn '"var(--tint-' src/routes/speakers/+page.svelte` 返回 0 行(本地数组已删);`npm run check` 0/0、`npm run build` 通过。
- [ ] **Step 4: Commit**:`feat(design): 说话人徽章文字色接 soft 公式(同色相深/亮文字)`

---

### Task 4: DESIGN.md v2 全文重写

**Files:**
- Modify: `DESIGN.md`(仓库根)

- [ ] **Step 1: 重写**。以 spec §一~五 为内容真值:版头 `version: 2`、name 不变、description 改写为 Raycast 化叙事;「原则」节按 spec §一(保留内容优先/悬停显影/录制红点唯一常驻彩色/禁 emoji/双主题同权;「温暖中性」→「冷黑阶梯」、「单一强调」→「白药丸+交互蓝」);colors 表 = Task 1 两块 CSS 的值(light/dark 两列,含新增 token 与徽章 ink 列);typography 节:系统字体栈不动 + 标题 500 字重 + 正文 1.6 行高 + ≤14px 小字 0.2px 字距;shapes/elevation 按 spec §四;组件规范逐条改写按 spec §五(button-primary 药丸/录制按钮/输入框/列表行/浮层/横幅/进度条/徽章)。保留既有文件里与视觉无关的流程性注记(若有)。**亮色列标注"反推值,冒烟后可调"**。
- [ ] **Step 2: 自查**:文中每个色值与 app.css 逐一相符(抽 5 个 spot check);无 Notion 残留叙述;无 emoji。
- [ ] **Step 3: Commit**:`docs(design): DESIGN.md v2——Raycast 化命令面板质感(暗色第一公民)`

---

### Task 5: 全量验证与 PR

- [ ] **Step 1**: `npm run check` 0/0、`npm run build`、`cargo build`(确认 Rust 侧无涉动)全过;全仓 `grep -rn "#[0-9a-fA-F]\{6\}" src --include="*.svelte" | grep -v "app.css"` 抽查无新硬编码色值逃逸(既有豁免除外,如 svg currentColor 无匹配即最好)。
- [ ] **Step 2**: push 分支,`gh pr create`:标题「Raycast 化设计系统(DESIGN.md v2)」;描述含变更概览、双主题逐页冒烟清单(侧栏/录制页/详情页含播放器/声纹库/设置页 × 亮暗)、已知取舍(亮色反推可调/tint-peach 名实不符加注/字重降 500 的中文渲染观察点)。

---

## Self-Review 记录

- Spec 覆盖:§二 token 表→T1;主按钮/字重→T2;徽章→T3;§一/三/四/五 文档化→T4;验收→T5+用户冒烟。spec §五 其余组件(输入框/列表行/浮层/横幅/进度条)靠 token 值替换自动适配,无任务是有意的(结构零改动)。
- 占位符扫描:无 TBD;T3 的 `hashIndex` 指向现有逻辑并给出抽取指令,非悬空引用。
- 类型一致:`speakerInk` 签名在 Interfaces 与代码块一致;`--tint-*-ink` 命名在 T1/T3/T4 一致。
- 已知顺序:T2/T3 依赖 T1 token;T4 依赖 T1 定稿值;顺序执行无并行冲突。
