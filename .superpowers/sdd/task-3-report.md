# Task 3 报告:徽章文字色接 soft 公式

**分支**: raycast-design-system

(注:本文件路径此前存在一份内容完全无关的旧报告——audio-compression-settings 分支
"下载器装好即删 prune 项"任务的报告,已整体覆盖为本任务的报告。)

## 改动

1. **`src/lib/notes.ts`**
   - 抽出共用 `speakerIndex(speaker)`:取索引逻辑与原 `speakerColor` 完全一致(S<n> 数值
     循环 / 非 S<n> 字符串散列兜底),`speakerColor` 与新增 `speakerInk` 两处消费,不复制。
   - 新增 `SPEAKER_INKS`(与 `PALETTE` 同顺序七色 `-ink` 变量)与
     `export function speakerInk(speaker, source)`,`!speaker` 兜底与 `speakerColor` 对齐
     (mic→sky-ink,其它→mint-ink)。

2. **`src/lib/SpeakerChips.svelte:53`**:`style="background: ...; color: {speakerInk(id, 'mic')}"`,
   import 同步补 `speakerInk`。
   - 子元素检查:`.name`(标签文字)`color: inherit`,随 chip 新文字色走,正确。`.me`
     ("这是我")与 `.edit`(改名输入框)本就各自显式设了 `color: var(--accent)` /
     `color: var(--ink)`,不继承 chip 颜色——前者是链接态操作,后者浮在 `--canvas` 底而
     非徽章底,语义上应保持独立,未改动。

3. **`src/routes/notes/[id]/+page.svelte:335→336`**:同样追加
   `; color: {speakerInk(seg.speaker, seg.source)}"`,import 同步。该 `.badge.as-btn` 内
   只有纯文本(说话人标签),无继承色子元素需要处理。

4. **`src/routes/speakers/+page.svelte`**:删除本地第 29-44 行的 `TINTS` 数组与
   `avatarTint()` 函数,改为:
   ```ts
   const avatarTint = (id: string) => speakerColor(id, "mic");
   const avatarInk = (id: string) => speakerInk(id, "mic");
   ```
   `.avatar` 头像(第 187 行)追加 `color: {avatarInk(p.id)}`,内部 `.initial`(姓名首字)
   与人形轮廓 SVG(`stroke="currentColor"`)均无自持色,正确继承。合并菜单里的
   `.menu-dot`(第 285 行,纯色块无文字)不需要 ink,未改动。

   **索引逻辑差异(按要求以 notes.ts 为准统一,报告差异)**:本地原逻辑剥 `^P` 前缀做
   数值循环(P1→色0, P2→色1…);`notes.ts` 的 `speakerIndex` 剥的是 `^S` 前缀,对 `P<n>`
   形态不命中数值分支,统一后退化为字符串散列兜底。影响:声纹库头像色不再是
   P1/P2/P3… 顺序循环,而是按 id 字符串散列取色——每个人的颜色仍然确定且稳定(同一 id
   永远同色),只是不再保证相邻编号视觉相邻/循环有序。`source` 参数对该页无意义(id
   恒为真值,`!speaker` 分支不会触发),固定传 `"mic"`。

## 验证

- `grep -rn '"var(--tint-' src/routes/speakers/+page.svelte` → 0 行(本地色数组已删,确认)。
- `npm run check` → `0 ERRORS 0 WARNINGS`。
- `npm run build` → 通过(client + server 均 `✓ built`)。
- 布局/交互零改动:仅在既有 `style` 属性追加 `color:`,未碰任何 CSS class 规则、DOM
  结构、事件绑定。

## Commit

```
feat(design): 说话人徽章文字色接 soft 公式(同色相深/亮文字)
```

## Files

- Modified: `src/lib/notes.ts`、`src/lib/SpeakerChips.svelte`、
  `src/routes/notes/[id]/+page.svelte`、`src/routes/speakers/+page.svelte`

## Self-Review / 关注点

- 三个消费点(`SpeakerChips`、`notes/[id]` 转写徽章、`speakers` 头像)背景与文字色索引
  逻辑完全共用同一个 `speakerIndex`,不会出现背景色与文字色错位。
- `speakers/+page.svelte` 的索引口径差异已如上报告,按 brief 要求以 notes.ts 为准统一,
  未额外保留本地 `^P` 前缀特判(brief 明确要求删除本地数组、统一消费)。
- 超出范围未改动:`src/routes/record/+page.svelte:95` 的实时转写徽章
  (`<span class="badge" style="background: {speakerColor(...)}">`)有完全相同的 soft
  底配色缺文字色问题,但该文件不在本任务 brief 的文件清单内,未 touch。建议后续任务/
  补丁跟进,否则录制中页面的徽章仍是背景 15% alpha + 默认 `--ink`,对比度问题依旧存在。
- 工作区内 `.superpowers/sdd/task-1-report.md`、`src-tauri/gen/schemas/*.json` 三个文件
  在本任务开始前就已是未提交的修改状态,与 Task 3 无关(疑似前序任务/工具链遗留),
  本次 commit 只 `git add` 了本任务实际改动的 4 个文件。

---

## 追加:复核修复(record 页缺口 + 两条 Minor)

复核确认 record 页缺口需要补(计划遗漏,同一视觉一致性必须闭合),连同前一任务复核的
两条一行级 Minor,单提交完成:

1. **`src/routes/record/+page.svelte:95`** 转写行徽章:`style` 追加
   `; color: {speakerInk(line.speaker, line.source)}`,import 补 `speakerInk`。
2. **`src/routes/record/+page.svelte`(原约 158 行)**:`.sym.dot.on-blue` 的
   `background: var(--on-accent)` → `var(--on-primary)`。该点渲染在已迁 primary 的
   `.ctl.primary` 主按钮内(`color: var(--on-primary)`),语义对齐;现值巧合等值不可见,
   但会随 token 漂移。
3. **`src/routes/speakers/+page.svelte:346`**:`.section-title` 注释"小号加粗"改为
   "小号标题(500 字重)",与实际 `font-weight: 500` 一致。

**顺带闭合(同一缺口的另一半,超出清单但同属 record 页徽章一致性)**:
`.badge.mic` / `.badge.system`(实时转写 partial 行的"我/对方"占位徽章,原 260-261 行)
与最终行徽章共用同一 `.badge` 外观,原先靠 `.badge { color: var(--ink) }` 吃默认文字色。
若只改最终行,同一个"我"徽章会在 partial→final 转正瞬间文字变色(soft ink ↔ 默认 ink),
一致性反而更破。故:`.badge` 去掉 `color: var(--ink)`(现三处消费全部显式带色,无裸
`.badge` 用法),`.badge.mic` 补 `color: var(--tint-sky-ink)`、`.badge.system` 补
`color: var(--tint-mint-ink)`——与 `speakerInk()` 的 `!speaker` 兜底分支(mic→sky-ink/
system→mint-ink)完全一致。注释同步更新。

### 验证(追加改动后重跑)

- `npm run check` → `0 ERRORS 0 WARNINGS`。
- `npm run build` → 通过。
- 布局/交互零改动:仅 color/background 值与注释文字。

### Commit(追加)

```
fix(design): record 页徽章配对文字色,残留 on-accent/注释清理
```
