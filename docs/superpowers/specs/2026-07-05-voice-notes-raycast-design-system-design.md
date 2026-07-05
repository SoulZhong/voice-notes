# Raycast 化设计系统(DESIGN.md v2)设计

日期:2026-07-05。**排队等 PR #13(音频压缩+设置页)squash 合并后从新 master 开分支实施**(用户拍板,避免堆叠 PR)。风格源:VoltAgent/awesome-design-md 收录的 Raycast DESIGN.md(getdesign.md 分析稿,本地参考副本 ~/.claude/raycast/DESIGN.md);本 spec 已把所需值全部内联,实施不依赖外部文件。

## 已确认决策

- 视觉方向整体切换为 Raycast:近黑画布、表面阶梯、发丝线、白色 CTA 药丸、饱和 accent 点缀、6-10px 紧圆角、命令面板质感。
- **双主题**:暗色为第一公民(取 Raycast 原值);亮色按同一灰阶纪律极性反推(Raycast 文件无亮色,反推值标注"冒烟后可调")。
- **系统字体栈不动**(-apple-system;SF Pro 即 Raycast 真实应用字体),只采其字级/字重/行高体系;不引 Inter webfont。
- 纯视觉层重皮肤:布局结构、交互逻辑、组件行为全部不动。
- 现有 CSS 变量 **token 名不换、只换值**(组件已全量引用 var(--token),改名会放大 diff);仅新增主按钮三 token(见下)。

## 一、原则(DESIGN.md v2 保留/改写)

保留(与 Raycast 气质相容,只换视觉值):**内容优先**(转写文本是主角)、**悬停显影**(行级操作默认隐身)、**录制红点是唯一常驻彩色信号**、**禁 emoji/Unicode 符号图标**(线框 svg)、**双主题同权**。
改写:「温暖中性」→「**冷黑阶梯**:表面层级靠亮度阶梯而非投影,chrome 读起来像命令面板」;「单一强调」→「**白药丸 + 交互蓝**:主 CTA 用极性药丸(暗色白底/亮色黑底),accent 蓝只表达链接/焦点/选中;饱和彩色只出现在语义信号(录制红/警示黄/成功绿)与说话人徽章」。

## 二、colors(token 名沿用 app.css 现名)

| token | dark(主,Raycast 原值) | light(反推,可调) | 用途 |
|---|---|---|---|
| canvas | `#07080a` | `#fafafa` | 页面底 |
| surface | `#0d0d0d` | `#f1f1f2` | 侧栏/卡片/转写区底 |
| surface-soft | `#121212` | `#ebebec` | 次级表面、行悬停 |
| surface-press | `#1a1b1c` | `#e3e3e5` | 按压/选中态底 |
| hairline | `#242728` | `#e4e5e6` | 分隔线、控件边框 |
| hairline-strong | `#3a3d40` | `#c9cacc` | 输入框等更清晰边界 |
| ink | `#f4f4f6` | `#18191a` | 主文字 |
| ink-secondary | `#9c9c9d` | `#5c5d5f` | 次要文字 |
| ink-faint | `#6a6b6c` | `#949597` | 占位、时间戳 |
| accent | `#57c1ff` | `#0f7fd1` | 链接、可交互、焦点环、选中 |
| accent-pressed | `#3fa9e8` | `#0c6ab0` | 交互按压 |
| accent-tint | `rgba(87,193,255,.15)` | `rgba(15,127,209,.10)` | 可编辑悬停底、选中弱底 |
| on-accent | `#07080a` | `#ffffff` | accent 实底上的文字(极少用) |
| **primary(新增)** | `#ffffff` | `#18191a` | 主按钮药丸底(Raycast 签名) |
| **primary-pressed(新增)** | `#e8e8e8` | `#2c2d2f` | 主按钮按压 |
| **on-primary(新增)** | `#18191a` | `#ffffff` | 主按钮文字 |
| danger | `#ff6161` | `#d63a3a` | 破坏性确认、错误 |
| danger-ink | `#ffb4b4` | `#9b1c1c` | 错误横幅正文 |
| danger-tint | `rgba(255,97,97,.12)` | `#fdecec` | 错误横幅底 |
| danger-line | `rgba(255,97,97,.30)` | `#f3c6c6` | 错误横幅边 |
| record | `#ff6161` | `#ff6161` | 录制红点/停止(双主题一致) |
| warning-tint | `rgba(255,197,51,.10)` | `#fef6de` | 提示横幅底 |
| warning-ink | `#ffd980` | `#7a5a0e` | 提示横幅文字 |
| warning-line | `rgba(255,197,51,.30)` | `#f0e0ac` | 提示横幅边 |
| success | `#59d499` | `#1d9e63` | 完成态 |

主按钮组件从 `accent/on-accent` 改引 `primary/on-primary/primary-pressed`;accent 不再做按钮实底。

### 说话人徽章(soft 公式:饱和色 15% alpha 底)

红 `#ff6161` **不进徽章池**(独占录制/danger 语义,避免"说话人=错误"误读)。七色:

| token | 底(双主题同公式) | dark 文字 | light 文字 |
|---|---|---|---|
| tint-sky | `rgba(87,193,255,.15)` | `#57c1ff` | `#0b6bb8` |
| tint-mint | `rgba(89,212,153,.15)` | `#59d499` | `#157a4c` |
| tint-yellow | `rgba(255,197,51,.15)` | `#ffc533` | `#8a6510` |
| tint-lavender | `rgba(178,139,244,.15)` | `#b28bf4` | `#6d3fc2` |
| tint-rose | `rgba(255,122,194,.15)` | `#ff7ac2` | `#b8347e` |
| tint-peach(→青) | `rgba(79,210,201,.15)` | `#4fd2c9` | `#0e7d74` |
| tint-gray | `rgba(156,156,157,.15)` | `#c9c9ca` | `#55565a` |

token 名沿用现有七个(tint-peach 语义改青色,名不动,注释说明);light 文字为反推 AA 值。

## 三、typography

系统字体栈不动。层级采 Raycast:**标题一律 500 字重**(告别 600/700 的重标题),正文行高 1.6,≤14px 小字加 0.2px 字距;字号级差沿现有页面骨架(应用内层级,不引营销页 64px display)。落地为 app.css 注释规范 + 各组件字重排查(现 600/700 处降到 500;列表标题 600→500 后靠 ink 对比保持层级)。

## 四、shapes 与 elevation

- 圆角收紧:`radius-md 6px / radius-lg 8px / radius-xl 10px / radius-full 999px`(药丸仅主按钮与录制点)。
- 深度靠**表面阶梯**不靠投影:卡片/列表 = surface 阶梯 + 1px hairline;浮层菜单/弹出 = surface-press 底 + `shadow-popover` 加深(`0 8px 24px rgba(0,0,0,.5)` dark / `.16` light);`shadow-btn` 改为主按钮专用:1px 内描边(dark `rgba(0,0,0,.2)`,light `rgba(255,255,255,.12)`)+ 2px 微投影,按压下沉 0.5px 保留。

## 五、组件规范要点(全组件按此核对)

- **button-primary**:primary 药丸(radius-full)、on-primary 文字、shadow-btn;**button-secondary**:透明底 + hairline-strong 边 + hover surface-soft(不变形,只换 token 值);链接式按钮无边无影。
- **录制按钮**:保持"白底(dark 下即 primary 白药丸)+ 红点"结构,录制中红点变方块语义不动。
- **输入框**:surface-press 底、无边;聚焦 canvas 底 + accent 1px 环(现有形态,换值即成立)。
- **列表行/侧栏条目**:hover surface-soft、选中 surface-press、整行可点、操作悬停显影——结构零改动。
- **右键菜单/浮层**:surface-press 底 + hairline 边 + 深 shadow-popover。
- **横幅**:tint 底 + line 边 + ink 文字三件套(danger/warning 新值)。
- **进度条**:轨 hairline、填充 accent、radius-full。
- **播放器/chips/徽章**:引用 token 自动适配;徽章文字色按 §二表补双主题变量。

## 六、施工范围

1. `DESIGN.md` 全文重写(v2,本 spec §一~五为内容基础;版头 version: 2)。
2. `src/app.css`:全 token 重定值 + 三个新增 primary token + 徽章文字色变量补齐 + radius 重定值。
3. 全组件排查:主按钮改引 primary、字重 600/700→500 处、硬编码残留(应无,PR #9 已收敛)、徽章文字色接新变量。
4. 逐页双主题目检:侧栏、录制页、详情页(含播放器)、声纹库、设置页。

**不动**:布局/交互/组件行为/文案;不引 webfont;不做 Raycast 营销页组件(hero 条纹、定价卡等)。

## 验收冒烟

1. 暗色逐页走查:命令面板质感成立(黑阶梯+发丝线+白药丸),无残留暖灰;
2. 亮色逐页走查:反推值可读性(正文/次要/徽章文字对比度 AA 抽查),不顺眼的值现场调并回写 DESIGN.md;
3. 录制红点/danger/warning/success 语义信号在两主题下清晰;
4. `npm run check` 0/0、build 通过;截图对比留档 PR。

## 已知取舍

- 亮色全套为反推值(Raycast 无亮色官方参照),预期需要一轮冒烟微调;spec 表即初始真值,调后回写 DESIGN.md。
- tint-peach token 名与其新语义(青色)不一致:改名会牵连组件 diff,保名加注释。
- 标题字重全面降到 500 是气质关键,若冒烟发现中文渲染层级不足,允许正文加大 ink 对比而非回调字重。
