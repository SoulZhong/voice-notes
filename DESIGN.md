---
version: 1
name: voice-notes-design-system
description: voice-notes 是 macOS 本地实时会议转写笔记工具。设计语言取 Notion 式温暖极简(warm minimalism):温暖中性灰的画布与表面、炭墨色文字、克制的单一互动蓝、近乎无阴影的发丝线分界——一切服务于"转写文本是主角"。界面镀铬(chrome)保持安静:操作入口悬停才显现,强调色只用于可点击与录制态,信息密度贴近原生 macOS 生产力工具而非营销页。
---

## 原则

1. **内容优先**:转写段落是页面的主角。正文行高 1.7、可读列宽、无干扰底色;一切控件视觉权重低于正文。
2. **温暖中性**:灰不是冷灰——画布纯白,表面/悬停用带暖调的 `surface` 系;文字用炭墨 `ink` 而非纯黑。
3. **单一强调**:互动蓝 `accent` 只表达"可交互/正在进行";danger 红只在确认破坏性操作时出现;录制红点是唯一的常驻彩色信号。
4. **发丝线代替阴影**:卡片、按钮、菜单一律 1px `hairline` 边界 + 悬停换底色;唯一允许的阴影是浮层菜单的 `shadow-popover`。
5. **悬停显影**:行级操作(删除/合并/改名角标)默认隐身,悬停浮现——保持列表安静。
6. **双主题同权**:每个 token 都有暗色值;暗色不是反色,是 Notion 式深暖灰(#191919 系),避免纯黑。

## colors

| token | light | dark | 用途 |
|---|---|---|---|
| canvas | `#ffffff` | `#191919` | 页面底 |
| surface | `#f6f5f4` | `#202020` | 侧栏/卡片/转写区底 |
| surface-soft | `#fafaf9` | `#252525` | 次级表面、行悬停 |
| surface-press | `#efedeb` | `#2c2c2c` | 按压/选中态底 |
| hairline | `#e5e3df` | `#373737` | 分隔线、控件边框 |
| hairline-strong | `#c8c4be` | `#4a4a4a` | 需要更清晰的边界(输入框) |
| ink | `#37352f` | `#d3d1cb` | 主文字(炭墨,非纯黑) |
| ink-secondary | `#5d5b54` | `#9b9998` | 次要文字、说明 |
| ink-faint | `#a4a097` | `#6f6e69` | 占位、时间戳、微文字 |
| accent | `#0075de` | `#529cca` | 链接、可交互、主按钮底 |
| accent-pressed | `#005bab` | `#3d85b5` | 主按钮按压 |
| accent-tint | `rgba(0,117,222,.08)` | `rgba(82,156,202,.15)` | 可编辑悬停底、选中弱底 |
| on-accent | `#ffffff` | `#ffffff` | 主按钮文字 |
| danger | `#e03131` | `#ff6b6b` | 破坏性确认、错误(按钮/图标) |
| danger-ink | `#9b1c1c` | `#ffb3b3` | 错误横幅正文(浅红底上高饱和 danger 仅 3.9:1,需专用深色达 AA) |
| danger-tint | `#fdeaea` | `#442a2a` | 错误横幅底 |
| danger-line | `#f3c6c6` | `#5a3232` | 错误横幅边 |
| record | `#eb5757` | `#eb5757` | 录制中红点/停止(双主题一致) |
| warning-tint | `#fef7d6` | `#3a3222` | 提示横幅底 |
| warning-ink | `#523410` | `#e8d49a` | 提示横幅文字 |
| warning-line | `#f0e2a8` | `#5a4d2a` | 提示横幅边 |
| success | `#1aae39` | `#4cc366` | 完成态 |

说话人徽章的粉彩系(pastel tints,配 `ink` 文字;暗色用同色相加深):

| token | light | dark |
|---|---|---|
| tint-sky | `#dcecfa` | `#2a4157` |
| tint-mint | `#d9f3e1` | `#2a4a37` |
| tint-peach | `#ffe8d4` | `#54402d` |
| tint-lavender | `#e6e0f5` | `#413a58` |
| tint-rose | `#fde0ec` | `#513546` |
| tint-yellow | `#fef7d6` | `#4d452a` |
| tint-gray | `#f0eeec` | `#3a3a3a` |

暗色徽章文字用 `#e8e6e1`。

## typography

系统字体栈不动:`-apple-system, system-ui, sans-serif`(原生 macOS 血统即产品气质,不引 webfont)。

| token | size / weight / line-height | 用途 |
|---|---|---|
| page-title | 1.45rem / 700 / 1.25, letter-spacing -0.3px | 页面 h1、笔记标题 |
| section | 1.05rem / 600 / 1.35 | 卡片标题、分组头 |
| body | 0.95rem / 400 / 1.55 | 常规 UI 文字 |
| transcript | 1.02rem / 400 / 1.7 | 转写段正文(阅读优化;列宽上限经冒烟反馈撤销,当前铺满窗口) |
| caption | 0.85rem / 400 / 1.45 | 元信息、说明 |
| micro | 0.78rem / 500 / 1.4 | 徽章、时间戳 |
| button | 0.9rem / 500 / 1.3 | 全部按钮 |

## rounded

| token | 值 | 用途 |
|---|---|---|
| sm | 4px | 徽章、行内高亮 |
| md | 6px | 按钮、输入框 |
| lg | 8px | 卡片、横幅、菜单 |
| xl | 12px | 大卡片(下载卡)、转写容器 |
| full | 9999px | 电平表、录制红点 |

## spacing

4 / 8 / 12 / 16 / 20 / 24 / 32(px)。页面内边距 24px;列表行内边距 12px 16px;控件间距 8-12px。

## components

- **button-primary**:`accent` 底、`on-accent` 字、rounded-md、padding 6px 14px、无阴影;hover `accent-pressed`。用于每页至多一个主动作(开始录制/命名/下载模型)。
- **button-secondary**:透明底、1px `hairline-strong` 边、`ink` 字;hover 底 `surface-soft`。默认按钮形态(导出/继续录制/暂停)。**去掉现有的 box-shadow。**
- **button-danger**:形态同 secondary,字与边 `danger`;仅确认态出现实底(danger 底白字)。停止录制按钮:字 `record`。
- **button-link**:无底无边,`accent` 字,0.85em;行级操作(删除/合并/取消)。悬停加下划线。
- **input**:1px `hairline-strong` 边、rounded-md、focus 边变 `accent` + 1px 同色外环(box-shadow 0 0 0 1px);底 `canvas`。
- **list-row**(笔记列表/说话人列表):透明底、行间 1px `hairline` 分隔;hover `surface-soft`;选中/活动 `surface-press`。操作按钮 hover 显影。
- **sidebar**:`surface` 底、右侧 1px `hairline`;条目 rounded-md,hover `surface-soft`,当前页 `surface-press` + `ink` 加粗;录制中条目前的红点用 `record`。
- **transcript-container**:`surface` 底、rounded-xl、padding 16-20px;段落间距 6px;正文 `transcript` 字级。
- **speaker-badge**:粉彩 tint 底 + `ink` 字、rounded-sm、`micro` 字级;哈希取色循环上表 7 色。
- **speaker-chip**(顶部说话人条):同徽章色系,rounded-full,可点击时 hover 加 `accent-tint` 外环。
- **banner**(提示/错误):`warning-tint` 底、`warning-ink` 字、1px `warning-line` 边、rounded-lg;错误横幅同形态换 danger 色系(浅红底 `#fdeaea`/dark `#442a2a`)。
- **menu / popover**(说话人选择、合并目标):`canvas` 底、1px `hairline` 边、rounded-lg、`shadow-popover: 0 4px 16px rgba(0,0,0,.08)`(暗色 `.4`)。
- **download-card**:`surface` 底 rounded-xl(大卡);compact 提示条改用 banner 形态(warning 色系)。进度条:轨 `hairline`、填充 `accent`、高 6px rounded-full。
- **timer / meter**:计时数字用等宽数字(`font-variant-numeric: tabular-nums`)、`ink-secondary`;暂停态 `ink-faint`。电平表轨 `hairline`、填充 `success`。
- **editable-text**(段落/标题/名字):静态时无边;hover `accent-tint` 底 + rounded-sm;focus `accent` 2px outline。已命名说话人的 ✎ 角标 `ink-faint`,hover 变 `accent`。

## 实施说明(给编码代理)

- token 落地为 `src/app.css` 的 CSS 自定义属性(`:root` + `@media (prefers-color-scheme: dark)` 覆盖),由 `+layout.svelte` 导入;组件样式一律引用 `var(--xxx)`,禁止新的硬编码色值。
- 现有硬编码色的对应关系:`#396cd8→accent`、`#f5f5f7→surface`、`#c0392b→danger`、`#fff4e5 系横幅→warning 系`、按钮 box-shadow 删除换 hairline 边。
- `speakerColor`(src/lib/notes.ts)的调色板换成上表粉彩 7 色(返回 CSS 变量名,徽章文字色随主题)。
- 行为零改动:只动样式与结构性 class,不碰逻辑/事件/状态。
