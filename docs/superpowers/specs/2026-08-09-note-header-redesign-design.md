# 笔记头部操作区重设计(质感统一)

日期:2026-08-09
状态:已批准(用户逐项拍板:统一控件语言+重排结构 / 低频动作全部可见 / 二段确认保留行内展开只美化 / 切换控件用凹槽式 segmented)
同日追加(用户中途提出):除导出 MD 外,还要能**导出音频**与**打开存储目录**——用户拍板:音频=成品轨单文件(无则置灰);头部两个导出合并为一个「导出」菜单钮。见 §5。

## 背景与问题

笔记详情页 topbar(`src/routes/notes/[id]/+page.svelte` 的 `.topbar` 区域)是操作密度最高的区域,现状有四处质感短板:

1. **两套切换控件视觉语言不一致**:双轨/成品轨(pill + 灰底)与修订稿/原始逐字稿(圆角矩形 + accent-tint)是同类交互两种长相。
2. **按钮权重混乱**:右上角"重新推断身份/导出 MD"是描边按钮,工具行"重转写"是裸链接,同为低频动作却不同族。
3. **破坏性二段确认行内散排**:重新 Aing / 重转写的确认态是警示文字+裸链接直接挤进行内,排版跳、不像成体系的设计。
4. **AI 魔杖按钮(英雄件)与裸链接并排**,层级对比失衡。

## 设计

### 1. 控件三级体系

- **英雄级**:AI 魔杖按钮**原样保留**(彩色魔杖、hover 星火、casting 动效均为用户拍板的签名件,不动),维持描边按钮形态,是工具行唯一"重"元素。录音红点键(`.rec-btn`)不变。
- **Segmented 切换**:新组件 `src/lib/Segmented.svelte`,双轨/成品轨与修订稿/原始逐字稿共用。
  - 轨道:`surface-soft` 底 + 1px `hairline` 边,`radius-md`(内陷凹槽感);
  - 选中滑块:`surface-press` 底 + 1px `hairline-strong` 边 + `shadow-btn`,绝对定位,按选中段的 offsetLeft/offsetWidth 定位,切换时 120ms ease 平滑滑动;`prefers-reduced-motion` 时无动画直接跳位;
  - 文字:选中 `ink`,未选中 `ink-secondary`,hover 提亮;
  - disabled 段:opacity 减淡 + `title` tooltip 给原因(成品轨不可信/无修订稿——现有行为全部保留);
  - 语义:`role="tablist"` + `role="tab"` + `aria-selected`,方向键左右切换,焦点环 `accent`;
  - 特例(mix-switch):成品轨缺失时该段显示"生成",点击触发补生成(动作而非切换,不移动滑块),进行中显示阶段文字并禁用——沿用现有 `startRegen`/`regenStage` 逻辑,只换壳。
- **幽灵按钮(quiet)**:重新推断身份、导出 MD、重转写统一为无边框幽灵态——`ink-secondary` 文字 + 16px 线性图标(stroke=currentColor),透明底,hover 浮现 `surface-soft` 底并提亮为 `ink`(悬停显影原则)。新增图标:重新推断身份(人形+循环箭头)、重转写(文档+循环箭头),与现有导出 MD 图标同语言。禁 emoji(DESIGN.md 原则 7)。

### 2. 布局(结构微调,原功能行位置不动)

```
标题 / 时间·时长 / 日历行      [◉↻ 重新推断身份] [⎘ 导出 ▾] [▤ 打开目录]   ← 笔记级幽灵组(右上)
▶ 波形━━━━━━━━━━ 00:03:54  ╭双轨│成品轨╮  ●rec              ← transport 行
说话人 chips……
╭█修订稿█│原始逐字稿╮        (spacer)   [✦AI ✓]  [↻ 重转写]      ← 工具行
```

分组原则:右上角是**笔记级**输出/元数据动作;工具行右侧是**转写内容级**动作(AI 精修、重转写);工具行左侧是"看哪份稿"。各行内部对齐与间距按 4/8/12px 栅格收紧。

### 3. 二段确认:警示胶囊

确认态(confirmRefine / retransConfirm)整体包进**警示胶囊条**:`warning-tint` 底 + 1px `warning-line` 边 + `radius-lg`,内含:

- 警示文案(`warning-ink`,caption 字级);
- 确认按钮(`danger` 色文字;重转写场景是"双轨重转/成品轨重转"两个并排,成品轨不可用时置灰+tooltip——现有逻辑保留);
- 取消按钮(幽灵态)。

出现时 120ms 淡入 + 下移 2px,占据工具行右侧原按钮位置,不换行不跳版。两处确认共用同一形态(可抽 CSS class,不必抽组件)。

### 4. 微交互统一

- 全部按钮 `transition: 120ms ease`(background/color/transform);按压态下沉 0.5px;
- disabled 一律 opacity + tooltip 给原因(现有 title 全部保留);
- 焦点可见性:`:focus-visible` accent 焦点环。

### 5. 新功能:导出音频 + 打开存储目录(同日追加)

**头部右上角三个幽灵钮**:`[重新推断身份] [导出 ▾] [打开目录]`。

- **导出菜单**:「导出」幽灵钮(带下箭头 chevron)点开小浮层菜单(DESIGN.md 浮层规范:`surface-press` 底 + `hairline` 边 + `shadow-popover`,`radius-lg`),两项:
  - **导出 MD**:走现有 `doExport("md")` 流程,不变;
  - **导出音频**:成品轨单文件。无成品轨时该项置灰 + tooltip 提示先在播放器区生成(与回放侧「生成成品轨」语义一致)。流程与导出 MD 同款:保存对话框(默认名=标题+时间,扩展名取成品轨实际文件后缀 m4a/wav)→ 后端落盘 → 复用 `notes.export.done/failed` 提示。
  - 菜单 Esc/点外部关闭,`role="menu"` 语义。
- **打开目录**:幽灵钮,点击在系统文件管理器中打开该笔记的存储目录。后端照 `open_models_dir` 先例加 `open_note_dir(id)`(Rust 侧 opener,不依赖前端路径白名单)。

**后端新增**:
- `NoteStore::export_audio_to(id, dest)`(store/export.rs):复用 `export_to` 的 dest 守卫(绝对路径、不落数据目录内、同目录 tmp+rename 原子替换),源文件由 `store::audio::mixed_track(note_dir)` 解析;无成品轨报错。
- `#[tauri::command] export_note_audio(app, id, dest)` 与 `#[tauri::command] open_note_dir(app, id)`,注册进 generate_handler。

**新 i18n key**(zh/en 双语,过 parity 测试):`notes.export.button`(导出/Export)、`notes.export.audio`(导出音频/Export audio)、`notes.export.audioNone`(无成品轨置灰原因)、`notes.dir.open`(打开目录/Open folder)、`notes.dir.openHint`(tooltip)、`notes.dir.openFailed`(失败提示)。

`exportFileName(title, startedAt)` 加可选 `ext` 参数(默认 "md"),音频导出复用同一命名纪律。

## 范围与非目标

**动**:`src/routes/notes/[id]/+page.svelte` 的 topbar 区域(header 动作组、transport 的 mix-switch、view-switch 行、二段确认)样式与结构;新增 `src/lib/Segmented.svelte`;§5 的两个新功能(前端 `notes.ts` 包装 + 后端两个命令 + i18n key)。

**不动**:AudioPlayer 内部、SpeakerChips、转写区、既有业务逻辑/状态机/事件(rerunIdentify、doExport、switchScheme、startRegen、rerunRefine、startRetranscribe 的行为与禁用条件一字不改);AI 魔杖动效;日历行(mini plain 链接维持现状);既有 i18n key 不改语义(新 key 只增)。

## 测试

- 现有 vitest 交互测试不涉及此区域样式,预计零改动;若 view 切换测试选择器依赖 `.link` class 需同步更新选择器(跑 `npx vitest run` 验证)。
- 新纯逻辑单测:segmented 键盘导航函数(vitest)、`exportFileName` 的 ext 参数(vitest)、`export_audio_to` 的守卫与拷贝(Rust,与 export_to 既有测试同款 tempdir 样板)。
- 人眼冒烟(真机):segmented 滑块滑动、双主题(暗/亮)下凹槽与滑块对比度、成品轨置灰 tooltip、二段确认胶囊不跳版、reduced-motion 下无滑动动画、导出菜单开合与音频落盘可播、打开目录落在正确笔记目录。
