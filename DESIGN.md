---
version: 2
name: voice-notes-design-system
description: voice-notes 是 macOS 本地实时会议转写笔记工具。设计语言取 Raycast 化命令面板质感:近黑阶梯的画布与表面、发丝线分界、主 CTA 用极性药丸(暗色白底/亮色黑底)、交互蓝只表达链接与焦点、饱和彩色只出现在语义信号与说话人徽章——一切服务于"转写文本是主角"。暗色为第一公民(取 Raycast 原值),亮色按同一灰阶纪律极性反推;界面镀铬(chrome)读起来像命令面板:安静、克制、层级靠亮度阶梯而非投影。
---

## 原则

1. **内容优先**:转写段落是页面的主角。正文行高 1.6、可读列宽、无干扰底色;一切控件视觉权重低于正文。
2. **冷黑阶梯**:表面层级靠亮度阶梯而非投影——`canvas` 到 `surface` 到 `surface-soft` 到 `surface-press` 逐级抬升,配 1px `hairline` 发丝线分界;chrome 读起来像命令面板。暗色不是纯黑反色,是 Raycast 近黑(#07080a 画布 + #0d0d0d 表面)。
3. **白药丸 + 交互蓝**:主 CTA 用极性药丸(暗色白底黑字 / 亮色黑底白字,radius-full),是 Raycast 签名;`accent` 蓝只表达链接 / 焦点 / 选中,不再做按钮实底;饱和彩色只出现在语义信号(录制红 / 警示黄 / 成功绿)与说话人徽章。
4. **录制红点是唯一常驻彩色信号**:`record` 红点是界面上唯一长期在场的高饱和色;`danger` 红只在确认破坏性操作时出现。
5. **发丝线代替阴影**:卡片、列表、菜单一律 1px `hairline` 边界 + 表面阶梯换底色;浮层菜单用 `shadow-popover` 加深。**唯一例外**:主按钮药丸用 `shadow-btn`(1px 内描边 + 2px 微投影),按压下沉 0.5px;链接式按钮不加。全部交互控件 120ms 缓动过渡。
6. **悬停显影**:行级操作(删除 / 合并 / 改名角标)默认隐身,悬停浮现——保持列表安静。
7. **禁 emoji 与 Unicode 符号图标**:录制 / 停止等符号用 CSS 图形(圆点 / 圆角方块)或 16px 线性 SVG(stroke currentColor),**禁用 emoji 与 Unicode 符号字符**(●■▶⏸👤)——各平台字形与基线不一,是质感杀手。
8. **双主题同权**:每个 token 都有双主题值;暗色为第一公民(Raycast 原值),亮色为同一灰阶纪律下的极性反推。落地上,`src/app.css` 用 CSS `light-dark()` 把每个 token 的两个值合并到 `:root` 一处声明,不再拆 `@media (prefers-color-scheme)` 两块;手动指定主题时只覆盖根元素的 `color-scheme`(`src/lib/theme.ts` 的 `applyTheme`),不改任何 token,跟随系统与手动切换共用同一套取值。

## colors

token 值以 `src/app.css` 为唯一真值源(下表逐一相符)。**亮色列为反推值(Raycast 无官方亮色参照),冒烟后可调**;暗色列为 Raycast 原值。

| token | dark(主) | light(反推,冒烟后可调) | 用途 |
|---|---|---|---|
| canvas | `#07080a` | `#fafafa` | 页面底 |
| surface | `#0d0d0d` | `#f1f1f2` | 侧栏 / 卡片 / 转写区底 |
| surface-soft | `#121212` | `#ebebec` | 次级表面、行悬停 |
| surface-press | `#1a1b1c` | `#e3e3e5` | 按压 / 选中态底、输入框底、浮层底 |
| hairline | `#242728` | `#e4e5e6` | 分隔线、控件边框 |
| hairline-strong | `#3a3d40` | `#c9cacc` | 需要更清晰的边界(次要按钮边) |
| ink | `#f4f4f6` | `#18191a` | 主文字 |
| ink-secondary | `#9c9c9d` | `#5c5d5f` | 次要文字、说明 |
| ink-faint | `#6a6b6c` | `#737476` | 占位、时间戳、微文字 |
| accent | `#57c1ff` | `#0f7fd1` | 链接、可交互、焦点环、选中 |
| accent-pressed | `#3fa9e8` | `#0c6ab0` | 交互按压 |
| accent-tint | `rgba(87,193,255,.15)` | `rgba(15,127,209,.1)` | 可编辑悬停底、选中弱底 |
| on-accent | `#07080a` | `#ffffff` | accent 实底上的文字(极少用) |
| **primary**(新增) | `#ffffff` | `#18191a` | 主按钮药丸底(Raycast 签名) |
| **primary-pressed**(新增) | `#e8e8e8` | `#2c2d2f` | 主按钮按压 |
| **on-primary**(新增) | `#18191a` | `#ffffff` | 主按钮文字 |
| danger | `#ff6161` | `#d63a3a` | 破坏性确认、错误(按钮 / 图标) |
| danger-ink | `#ffb4b4` | `#9b1c1c` | 错误横幅正文 |
| danger-tint | `rgba(255,97,97,.12)` | `#fdecec` | 错误横幅底 |
| danger-line | `rgba(255,97,97,.3)` | `#f3c6c6` | 错误横幅边 |
| record | `#ff6161` | `#ff6161` | 录制中红点 / 停止(双主题一致) |
| **on-record**(新增) | `#ffffff` | `#ffffff` | record / danger 实底上的文字(双主题同值,勿与 on-accent 混用) |
| warning-tint | `rgba(255,197,51,.1)` | `#fef6de` | 提示横幅底 |
| warning-ink | `#ffd980` | `#7a5a0e` | 提示横幅文字 |
| warning-line | `rgba(255,197,51,.3)` | `#f0e0ac` | 提示横幅边 |
| success | `#59d499` | `#1d9e63` | 完成态、电平表填充 |

主按钮组件从 `accent / on-accent` 改引 `primary / on-primary / primary-pressed`;`accent` 不再做按钮实底,只用于链接 / 焦点环 / 选中。

### 说话人徽章(soft 公式)

徽章走 **soft 公式**:饱和色 15% alpha 作底(双主题同底——15% alpha 对暗底同样成立),只切文字色。红色 `#ff6161` **不进徽章池**——它独占录制 / danger 语义,进池会造成"说话人 = 错误"的误读。七色:

| token | 底(双主题同公式) | dark 文字 | light 文字(反推 AA) |
|---|---|---|---|
| tint-sky | `rgba(87,193,255,.15)` | `#57c1ff` | `#0b6bb8` |
| tint-mint | `rgba(89,212,153,.15)` | `#59d499` | `#157a4c` |
| tint-yellow | `rgba(255,197,51,.15)` | `#ffc533` | `#8a6510` |
| tint-lavender | `rgba(178,139,244,.15)` | `#b28bf4` | `#6d3fc2` |
| tint-rose | `rgba(255,122,194,.15)` | `#ff7ac2` | `#b8347e` |
| tint-peach | `rgba(79,210,201,.15)` | `#4fd2c9` | `#0e7d74` |
| tint-gray | `rgba(156,156,157,.15)` | `#c9c9ca` | `#55565a` |

**tint-peach 名实注记**:token 名沿用现有七个,但 `tint-peach` 的实际语义已从桃色改为**青色**(值即青色 `rgba(79,210,201,.15)`)——改名会牵连 `speakerColor`(src/lib/notes.ts)与各引用组件的 diff,故保名加注,不改标识符。徽章文字色以双主题变量(`--tint-*-ink`)承载,随主题切换。

## typography

系统字体栈不动:`-apple-system, system-ui, sans-serif`(原生 macOS 血统即产品气质,SF Pro 亦是 Raycast 真实应用字体;只采其字级 / 字重 / 行高体系,**不引 webfont**)。

层级采 Raycast 纪律:**标题一律 500 字重**(告别 600/700 的重标题,层级靠 `ink` 亮度对比而非重字重),**正文行高 1.6**,**≤14px 小字加 0.2px 字距**。字号级差沿现有应用内页面骨架,不引营销页 display 字号。

| token | size / weight / line-height | 用途 |
|---|---|---|
| page-title | 1.45rem / 500 / 1.25, letter-spacing -0.3px | 页面 h1、笔记标题 |
| section | 1.05rem / 500 / 1.35 | 卡片标题、分组头 |
| body | 0.95rem / 400 / 1.6 | 常规 UI 文字 |
| transcript | 1.02rem / 400 / 1.6 | 转写段正文(铺满窗口,不设列宽上限) |
| caption | 0.85rem / 400 / 1.45, letter-spacing 0.2px | 元信息、说明(≤14px) |
| micro | 0.78rem / 500 / 1.4, letter-spacing 0.2px | 徽章、时间戳(≤14px) |
| button | 0.9rem / 500 / 1.3 | 全部按钮 |

h1 若不定字级则回退浏览器默认 2em、页面标题失控巨大——app.css 已用元素级规则统一为 page-title(1.45rem / 500 / 1.25 / -0.3px)。

## shapes(rounded)

圆角收紧为 Raycast 紧圆角;药丸仅主按钮与录制点。

| token | 值 | 用途 |
|---|---|---|
| radius-sm | 4px | 徽章、行内高亮 |
| radius-md | 6px | 按钮、输入框 |
| radius-lg | 8px | 卡片、横幅、菜单 |
| radius-xl | 10px | 大卡片(下载卡)、转写容器 |
| radius-full | 9999px | 主按钮药丸、录制红点、进度条 |

## elevation(深度)

深度靠**表面阶梯**不靠投影:

- **卡片 / 列表**:surface 阶梯 + 1px `hairline`,无投影。
- **浮层菜单 / 弹出**:`surface-press` 底 + `hairline` 边 + `shadow-popover` 加深(dark `0 8px 24px rgba(0,0,0,.5)` / light `0 8px 24px rgba(0,0,0,.16)`)。
- **主按钮药丸专用** `shadow-btn`:1px 内描边(dark `inset 0 0 0 1px rgba(0,0,0,.2)` / light `inset 0 0 0 1px rgba(255,255,255,.12)`)+ 2px 微投影(dark `0 1px 2px rgba(0,0,0,.4)` / light `0 1px 2px rgba(0,0,0,.18)`);按压下沉 0.5px。链接式与次要按钮不加。

## spacing

4 / 8 / 12 / 16 / 20 / 24 / 32(px)。页面内边距 24px;列表行内边距 12px 16px;控件间距 8-12px。

## 语义知识图谱（Phase 1 已实现）

### 数据真值与稳定标识

- 系统分为三层真值：笔记目录中的原始稿与 `aing.json` 是可审计事实；数据根目录的 `knowledge-overrides.json` 是操作只追加、整体原子替换的人为治理账本；图谱目录中的版本化 SQLite 文件是可删除、可重建的派生语义索引。任何索引错误都不得反写或阻断笔记核心生命周期。
- `aing.json` schema v2 为 mention、evidence、relation 提供稳定 ID。旧 schema v1 的 `e:<name>` 只作为迁移输入，经解析器映射为稳定 `kg_` 实体 ID；它不是当前实体身份，也不能成为跨次 Aing 的 canonical ID。
- Resolver 先归一化显式稳定 ID，再应用人工 rename/alias/create/merge/split/suppress/restore/end/create relation 操作，最后才使用模型抽取结果。merge/split 必须同时处理 mention、relation 与 evidence 的归属；治理账本损坏时保留上一版可读图谱并拒绝新的治理或补建写入。

### 派生索引与执行体一致性

- 重建写入版本化 `.next` 索引，完整校验后原子发布；失败删除候选并继续服务上一版。调度器按 generation 合并重复请求，构建状态事件携带对应 generation；关系补建另以精确 `run_id` 关联进度和取消，前端只在收到该 run 的完成态及其精确 generation 的索引 ready 后刷新一次，旧 run/旧 generation 事件都不能提交当前结果。笔记处理结果与索引发布结果分别记录；索引失败只重试 durable dirty 索引，不重新调用关系执行体，也不把已成功写入的笔记误列为失败。重试请求本身会重新持久化 dirty marker，即使旧 marker 缺失也不会让恢复入口失效。
- HTTP 与本机 Agent 共用同一归一化、证据校验与物化规则：相同关系载荷生成同等的实体引用、关系和 evidence。`aing.json` 的 `graph_extraction` 保存 provider、精确 model、run、contract、source hash、mode 和生成时间，SQLite 关系/证据保存可查询的 provider、model、原文区间、source sequences/hash 与 mention 绑定；完整 AI 请求/响应只进入独立 AI 日志，不伪装成关系自身的 provenance 字段。
- 图查询以语义关系为前景，共现只是一种可关闭的弱信号。最短解释路径按关系类型、置信度与治理状态计算确定性成本，使用稳定 ID 作为并列裁决，且每条路径边都可回到证据与原笔记。

### 补建、隐私与失败边界

- 历史补建必须先展示待处理笔记数量、完整笔记 ID、当前执行体/provider、精确 model、契约版本与发送内容说明，用户明确确认后才启动；文案明确告知「将把修订稿发送给当前配置的执行体」。预览生成的 consent token 绑定有序笔记选择、每篇 source hash、provider、model 和 contract，启动前重新计算，任一漂移都拒绝。界面不猜测价格，也不伪造当前接口并未计算的段落总数。
- 关系进度与图索引状态两条监听都必须先建立再启动任务；取消携带精确 `run_id`，是请求态而非伪造成功，终态与关闭时只清理一次监听。失败、部分完成或取消若携带已提交数据的 generation，必须先等该精确 generation 发布，再暴露原终态；共享弹窗对每个不同的已发布 generation 恰好刷新一次，同一弹窗继续未完成任务后可上报下一 generation，但不会重复上报旧 generation。未完成笔记重新预览当前范围后继续，索引失败则使用独立「重试索引」动作。迟到事件以会话标识、`run_id` 和 generation 丢弃。主界面只显示可行动的脱敏错误摘要，原始后端错误仅在用户主动展开“技术详情”时出现。补建只能新增/更新关系派生物，不改变段落文本、顺序或原始稿。
- 模型不可用、账本损坏、候选索引发布失败或前端刷新失败时，录音、保存、精修、阅读与既有图谱读取仍可继续；图谱明确展示降级原因和可恢复动作。

### 探索与治理体验

- 顶点和关系边展示完整名称，长文本在其视觉载体内换行或随语义缩放逐级出现，不用省略号代替内容，也不把标签拆成占空间的常驻外框。
- 搜索、关系类型/kind 筛选、一跳展开、收起、路径探索与 evidence 回看共同服务探索；rename、alias、merge、split、suppress、restore、end/create relation 与 undo 共同服务可逆治理。
- 大图默认呈现语义骨架并允许逐层展开；弱共现可选且视觉降权。键盘焦点、触控目标、窄窗口、亮暗主题与减少动态效果遵循全局无障碍约束。
- 开发版 `debugFixture=semantic-large` 从路由首帧即进入隔离模式，不等待异步夹具会话；它同步清除实体、关系与审核深链参数，禁止真实图谱、详情、审核、治理与补建副作用。调试关系详情只可通过服务端生成的不透明临时会话读取，替换或释放会话时删除旧临时目录，且该能力不进入 release 构建。

## components

- **button-primary**:`primary` 药丸底(radius-full)、`on-primary` 字、`shadow-btn`;hover `primary-pressed`,按压下沉 0.5px。用于每页至多一个主动作(开始录制 / 命名 / 下载模型)。暗色即白底黑字,亮色即黑底白字。
- **button-secondary**:透明底、1px `hairline-strong` 边、`ink` 字、radius-md;hover 底 `surface-soft`,不变形、无阴影。默认按钮形态(导出 / 继续录制 / 暂停)。
- **button-danger**:形态同 secondary,字与边 `danger`;仅确认态出现实底(danger 底白字)。停止录制按钮:字 `record`。
- **button-link**:无底无边,`accent` 字,0.85em;行级操作(删除 / 合并 / 取消)。悬停加下划线。
- **录制按钮**:保持"白底(dark 下即 primary 白药丸)+ 红点"结构——大面积强调蓝在侧栏太吵,彩色由 `record` 红点承担。录制中红点变圆角方块(CSS 图形,非 Unicode),字色 `record`。
- **input**:`surface-press` 底、无边、radius-md;聚焦换 `canvas` 底 + `accent` 1px 环(box-shadow `0 0 0 1px`)。侧栏过滤框同款内嵌式。
- **list-row**(笔记列表 / 说话人列表):透明底、行间 1px `hairline` 分隔;hover `surface-soft`;选中 / 活动 `surface-press`;整行可点。操作按钮 hover 显影。
- **settings-row**(设置页,macOS 系统设置式):`surface` 卡片承载多行,行间 1px `hairline`;每行=左「标题(0.92rem `ink`)+一行大白话说明(0.8rem `ink-secondary`)」右控件;纯开关行整行可点(label);行级按钮 hover 显影。说明文案禁术语,一行说清。
- **switch**(拨动开关,`input[type=checkbox].switch`,取代原生方框 checkbox):全应用布尔行的统一右侧控件。轨道 34x20px `radius-full`,关 `hairline-strong` 底、开 `accent` 底;滑块 16px 白圆、2px 内缩,选中态位移 14px,120ms 过渡;`:disabled` 透明度 .45 且指针恢复默认;`:focus-visible` 2px `accent` 外描边、2px 偏移。纯 CSS 实现,token 化,不改变原生 checkbox 的绑定/事件语义。
- **AI 助手接入**(设置页分组,settings-row 卡片内):Agent 列表逐行——row-label 是名字,row-desc 是状态(未检测到安装/未注册/已注册/已注册(路径已由自愈修复或待修复)),行尾 `button-secondary`(注册/移除)按钮;下方「允许 AI 控制录制」toggle 行(纯开关行,整行可点);再下「手动配置」折叠卡——`button-secondary` 展开/收起,展开后 `.snippet` 等宽代码块(JSON 片段)+ `button-secondary`「复制」按钮。
- **智能精修配置**(AI 页首卡,section-title「智能精修」,settings-row 语言与同页「AI 助手接入」卡同构):首行 settings-row「精修方式」——row-desc 按选择说明成本(本机 Agent=不需要 API Key / 在线接口=需要 API Key),右控件 segmented 二选一。本机 Agent 态三行 settings-row:①「Agent」row-desc=当前选中家的探测状态(已找到 <~缩写路径> / `warning-ink`「未找到命令行工具:请先安装并登录,或在下方指定路径」/ 填了路径时「使用指定路径 <路径>」),右控件 segmented 四选一(分段项只放名字,不带状态记号);②「模型」row-desc「留空使用 <家> 的默认模型」,右控件 `row-input`(input token 的行内版:surface-press 底无边 radius-md,聚焦 canvas 底+accent 环,占位符按所选家给示例);③「CLI 路径」row-desc「自动探测不到时,手动指定可执行文件」,右控件 row-input.wide,占位符「自动探测」——不得把探测结果放占位符伪装成已填值,路径只在 ①row-desc 出现一次;卡底 config-hint 一行失败语义(精修失败保留原文,不影响笔记)。在线接口态同为四行 settings-row:「一键填充」右侧 `button-secondary` 预设簇(preset-btns,窄窗换行右对齐)、「接口地址」row-input.wide、「模型」row-input、「API Key」row-input.wide(password);「模型」行文案随接口地址命中的预设定制(标签/说明/占位符三件套)——豆包(火山方舟)命中时整行变「接入点」(说明给「方舟控制台在线推理创建的 ep- 接入点 ID」,预设不预填模型值),手改过地址不再套预设文案;卡底 config-hint「三项配齐后精修生效」仅未配齐时出现。启用总开关仍在设置页「录制」区。
- **AI 调用日志**:AI 页末卡只留一行入口 settings-row(row-label「调用记录」+row-desc 一句话说明与总条数,行尾 `button-secondary`「查看」),浏览在独立页 **/ai/logs**(侧栏 AI 页签按前缀 `/ai` 高亮)。独立页:topbar=标题+右侧 `button-secondary`「打开日志目录」(访达 reveal,目录不存在先建)与「导出 JSONL」(空库禁用);topbar 下工具条=类别 segmented 五选一(全部/精修分块/标题生成/Agent 精修/精修写回)+右侧 caption 计数「共 N 条,已加载 M 条」或导出结果路径。列表 settings-row:row-label=类别中文名,失败尾随 `pill.warn`「失败」、超长截断尾随中性 pill;row-desc 一行元信息「YYYY-MM-DD HH:mm · 执行方 · 模型 · 耗时 · 笔记 id」+失败时 `warning-ink` 错误摘要(≤80 字);行尾 `button-link`「详情/收起」展开 `.snippet` 同族 JSON 全文(限高 24rem 内滚,pre-wrap)。分页=底部居中「加载更多(剩 N 条)」,50 条一页;空态居中一行说明留痕机制。
- **钩子配置**(侧栏第三页签,主从结构):侧栏=固定「新建钩子」行(+号线性图标,同「概览与整理」形态)+按事件分组的钩子列表(row=名称+「Shell 命令/Webhook·已停用」meta,停用行整行 `ink-faint` 淡显);主区 **/hooks** 概览页=一句话引导+三个 section(事件白名单与环境变量用 .rows/.row 列表,webhook 载荷用 .snippet 代码块);**/hooks/[id]** 编辑页(`new`=新建)=settings-row 卡六行:名称 row-input、触发事件下拉(六事件白名单)、执行方式 segmented 二选一(说明随选择切换)、命令 textarea 全宽等宽字(shell 态,说明列出 $VN_* 变量与 30s 超时)或 URL row-input.wide(webhook 态)、附带笔记内容 switch 行(说明:详情与全文交给命令/接口,精修稿优先)、启用 switch 行(整行可点);卡下动作行=保存(button-primary)+测试一次(button-secondary)+删除(button-danger 右贴,仅编辑态,走系统原生确认对话框);测试结果一行文字(成功=退出码/HTTP 状态,失败 `danger-ink` 原样错误),改名称/事件/方式/命令或 URL/附带笔记内容 即清空防旧结果背书新命令(启用开关不影响测试语义,不清空)。
- **segmented**(分段选择,设置行内多选一):`surface-press` 槽(radius-md、2px 内距),选中项 `canvas` 底浮起 + `shadow-btn`,未选中 `ink-secondary` 字 hover 变 `ink`;radio 视觉隐藏。用于外观主题/识别引擎等 2-4 项互斥选择。
- **sidebar**:`surface` 底、右侧 1px `hairline`;条目 radius-md,hover `surface-soft`,当前页 `surface-press` + `ink` 加重;行级操作悬停显影;行间不画分隔线(靠间距与 hover)。页签轨道为三页签「录音 / 会议搭子 / 钩子」,钩子页签按前缀 `/hooks` 高亮,点当前页签回根 `/hooks`。录制按钮见上;过滤框内嵌式(`surface-press` 底、无边,聚焦浮出 `canvas` 底 + accent 环)。
- **状态行**:辅助状态文字降为 caption 级 `ink-faint`,前缀 7px 状态点(活跃 `record`,空闲 `ink-faint`)。录制页把状态映射成友好短标签(录制中/已暂停/就绪)并进控制行右簇、不单挂一行;仅出错时在控制行下方展开完整 `danger` 错误详情行(文案可能较长)。空态文案在容器内居中,不左对齐孤行。
- **transcript-container**:`surface` 底、radius-xl、padding 16-20px;段落间距 6px;正文 `transcript` 字级(行高 1.6)。
- **speaker-badge**:soft tint 底 + `--tint-*-ink` 文字、radius-sm、`micro` 字级;哈希取色循环上表 7 色。
- **speaker-chip**(顶部说话人条):同徽章色系,radius-full,可点击时 hover 加 `accent-tint` 外环。录制页说话人条并入 topbar 随头部整体吸顶(滚到会中段落仍可对照辨认/改名);空说话人不渲染不占高。可编辑时点击在 chip 下缘 6px 处展开**编辑面板**(标准 menu/popover 语言,120ms 缓动浮现,贴视口右缘按实测尺寸左收留 8px)——chip 本身不变形,展开期间保持 `accent-tint` 外环;「这是我」不再作 chip 常驻钮,收进面板。面板结构:①首行无框改名输入(预填现名并全选,回车提交/Esc 关闭/失焦提交,下缘全出血发丝线);①′「试听他的声音」快捷行(▶ 图标;仅笔记详情页且有音轨时出现——不听声音没法确认「说话人 N」是谁):点击经页面播放器跳播该说话人**时长最长**的一段(高亮跟随连带滚到该段文本),重复点击按时长降序换下一段(前 5 循环),单段最多 15s 段尾自动停,试听中行尾 `ink-faint`「播放中,点击换一段」,用户手动暂停/拖走即退出试听态;面板保持展开,听完可直接改名/选人;②「这是我」快捷行(人形图标+文字);③人物区=`ink-faint` 小标题「会议搭子」+人物行列表(9px 色点用调色板 **ink 变体**——soft 底 15% alpha 做小色点不可见;未命名显示「说话人 N」;已关联行尾 `accent` SVG 勾),输入即按包含匹配过滤(过滤中隐藏快捷行),人多限高 13rem 内滚。精修稿视图:改名同步声纹库现名、选人关联库人物,会议搭子里改名经只读 join 反映回历史精修稿;精修进行中 chip 退回只读。原始稿视图同一面板,改名仍是笔记内本地名,选人区在非录制时可用(关联写 speakers.json);录制页无人物区。**重名拦截**:提交的新名撞库中他人现名时面板转确认条(`warning-ink` 提示语)——未关联说话人给 [是,关联他](accent 主推)/[不是,保留同名],已关联说话人给「可能是重复条目」提示+详情页链接+[仍要改名];人物行的未命名/重名条目行尾补「最近 MM-DD」`ink-faint` 副文案。
- **tidy-card**(会议搭子概览页「整理」卡):`surface` 底 rounded-lg,头部=标题「整理」+一行摘要(N 个待辨认可尝试自动归属 · N 个条目没有录音样本)+右侧 `button-secondary` 展开钮;展开分两节——①「可归属建议」:行=色点+可点名字+**行内试听小钮**(圆形 hairline 图标钮,播该人第一份样本,播放中 accent 描边+停止方块;无样本不出钮;单实例互斥,收起/合并/清理即停)+SVG 箭头+目标侧同构+相似度(≥74% 加 accent「很可能」)+行尾 [合并](mini accent)/[忽略];②「无样本条目」:checkbox 行(accent-color)+最近/累计 meta,出现在建议里的行加 `warning-ink`「有归属建议,先合并更好」且默认不勾,行尾「清理选中 N 项」走行内二段确认(warning 后果语+danger 确认)。录制中合并/清理禁用。**可发现性三件套**:①页签点击=回该页签根(已在页签内再点回 `/` 或 `/speakers`,iOS 式);②侧栏人物列表顶部固定「概览与整理」行(线性图标代色点,同 item 形态,`/speakers` 时 current 高亮)+`warning` 色药丸徽标=可归属建议数+同名组数(0 隐藏),像收件箱未读;③**建议跟人走**——详情页头部 ctx-card(warning 横幅家族)就地给当前这个人的归属建议(行内 [合并]/[忽略])与同名重复([查看对方]),对方名后带同款行内试听钮(与本人样本试听单实例互斥)——不听原声没法拍板该不该合。三处建议/忽略集同源(`src/lib/tidy.svelte.ts` 会话态,忽略不落盘)。
- **menu / popover**(右键菜单、说话人选择、合并目标):`surface-press` 底、1px `hairline` 边、radius-lg、`shadow-popover`(dark `.5` / light `.16`)。光标处展开,靠近视口右/下缘按实测尺寸整体收回(留 8px 边距);**菜单项不承载确认态**(不原地变形成「确认删除/取消」)——破坏性菜单动作一律弹系统原生确认对话框(plugin-dialog `ask`,warning 档,正文写清删什么、不可恢复);页面内(非菜单)的破坏性按钮仍可用行内二段式确认(清理/删段落既有模式)。
- **banner**(横幅三件套):tint 底 + line 边 + ink 文字。提示横幅 `warning-tint` / `warning-line` / `warning-ink`;错误横幅同形态换 `danger-tint` / `danger-line` / `danger-ink`;radius-lg。
- **progress**(进度条):轨 `hairline`、填充 `accent`、高 6px、radius-full。
- **waveform-track**(播放器音轨,即进度条):260 桶等宽细条(flex 等分 + 1px gap、radius-full),条高优先取**真实音频波形**(转码时从 WAV 预计算峰值桶存 audio.json,旧笔记打开时从 m4a 懒回填;多轨按时间轴对位取 max,按本条峰值归一 γ0.7 拉动态,不加抖动——真实数据自带起伏。有声音就有波形,与录音机直觉一致,说话稀疏不再近乎空白);无波形数据回退旧「段落 rms 包络 × ±18% 确定性抖动」;未播 `hairline-strong`、已播 `accent`;整条可点击/拖拽定位、方向键 ±5s。空数据退化为平轨。
- **waveform-live**(录制实时音轨,兼任电平表):2px 细条自右缘进入左移滚动(120ms 采样、保留约 29s),条色 `record`(录制中是唯一常驻彩色信号的延伸),暂停冻结退 `ink-faint`;空闲时容器空置占位保行高。
- **transport**(控制行,录音机式):录音/播放控制整合一行——笔记页 `[▶ 播放/暂停][时间][waveform-track][总时长][🔊 音频▾][⏺]`(音频菜单=单个「🔊 音频」胶囊按钮,喇叭图标+文字+chevron;把回放相关低频设置收进弹出面板,主控制行保持干净、每项用途一句话只在点开时出现。面板分两组:①**静音**——双轨笔记才有,一句「回放有回音?静掉一轨」+ 每轨复选框(静音的加「已静音」标);②**响度归一化**——本条真能被放大(`noteGain>1`)时才有,复选框 + 副说明「把偏轻的录音抬到正常响度」,默认开、存 `localStorage`。按钮仅在有任一可用设置时出现(`tracks>1 || noteGain>1`);改过任一默认(静音某轨/关响度)时点亮 `accent`、静音时喇叭换静音图标,收起也看得出动过。面板走改说话人菜单同款 popover 语言,向上弹避开视口顶,点面板外/Esc 关闭)(行尾续录键=圆形纯红点录音键,圆环+12px `record` 点,禁用点退 `ink-faint`;「图标必带文字」原则的**用户拍板特例**——录音红点是录音机通识符号,加文字反而挤占音轨,以 title/aria 兜底),录制页与之同构 `[控制钮组][waveform-live 全宽][计时+状态]`(波形 `flex:1` 占满控制与右簇之间整行,与笔记页 waveform-track 一致的单行整合;计时与状态短标签并进右簇、不再单挂一行);其余动作按钮一律图标+文字(纯图标看不出功能,冒烟反馈)。
- **download-card**:`surface` 底 radius-xl(大卡);compact 提示条改用 banner 形态(warning 色系)。
- **welcome-overlay**(首启引导):`canvas` 底全屏覆盖(z-index 置顶),居中 30rem 面板。品牌记号=录制按钮同构「primary 药丸 + 14px `record` 红点」;标题下一句话简介(`ink-secondary`);下载流整体复用 download-card(主按钮文案换「开始使用」),不另造进度 UI;权限预告与匿名统计告知共三行 caption 级 `ink-faint`;右下「高级设置 →」文字链接(`ink-secondary`,hover `ink` + `surface-soft`)为唯一逃生口。仅当未 onboarded 且识别模型未就绪时出现;下载完成后若检测到本机已装 Agent,先进「连接 AI 助手」步(勾选列表默认全选,可跳过),完成/跳过后再进录制页;未检测到任何 Agent 时直接进录制页。
- **timer / meter**:计时数字用等宽数字(`font-variant-numeric: tabular-nums`)、`ink-secondary`;暂停态 `ink-faint`。电平表轨 `hairline`、填充 `success`、radius-full。
- **editable-text**(段落 / 标题 / 名字):静态时无边;hover `accent-tint` 底 + radius-sm;focus `accent` 2px outline。已命名说话人的改名角标(线性 SVG,非 Unicode)`ink-faint`,hover 变 `accent`。
- **图谱页**（侧栏第三页签「图谱」，消费 Aing 语义知识图谱）：实体视角默认以**语义关系为前景**，弱共现关系仅作为可关闭的探索线索；文章视角继续用共享实体表达笔记之间的相似性。支持实体/文章切换、kind 与关系类型筛选、搜索定位、一跳展开、收起、全量浏览、两点间最短解释路径，以及实体详情与证据回看。语义索引不可用或待补建时明确显示降级原因，并提供进入共享「补建语义关系」确认流程的入口；失败不会阻断笔记录制、保存、精修或阅读。
- **force-graph**（图谱页主区，d3-force + SVG 自绘，`ForceGraph.svelte`）：顶点和边直接承载**完整可读标签**，不使用省略号或脱离顶点的常驻说明方框；长名称在顶点内多行排布，完整关系名沿边显示，缩放时按语义层级逐步显露别名、关系、证据数量和治理状态。顶点可拖拽，空白处平移，滚轮围绕光标缩放；hover 聚焦邻域，点击导航，键盘可达，`prefers-reduced-motion` 时直接定格。路径计算使用确定性的关系成本、稳定并列顺序，并始终允许从路径边进入原始证据。大图通过骨架、逐层展开和显式「显示全部」控制密度，不以截断文本换取空间。
- **entity-mention 可点击**(笔记页正文实体高亮,Phase2a 静态高亮的可导航升级):`note_entity_links` 拉本篇局部 ent_N→全局 id 映射;有映射的 span 加 `.linkable`(hover 变 `accent` 下划线,`role=link`+键盘可达)点击导航(人→会议搭子、非人→`/graph?e=` 深链);无映射(旧笔记/图谱未就绪)保持 Phase2a 原有纯 tooltip 不可点,不报错、不改变外观。

## 实施说明(给编码代理)

- token 落地为 `src/app.css` 的 CSS 自定义属性(`:root` 亮色 + `@media (prefers-color-scheme: dark)` 暗色覆盖),由 `+layout.svelte` 导入;组件样式一律引用 `var(--xxx)`,禁止新的硬编码色值。**文档与代码不一致时以 app.css 为准。**
- 主按钮改引 `primary / on-primary / primary-pressed`(不再用 accent 实底);全组件字重 600/700 处降至 500,层级靠 `ink` 亮度对比保持。
- `speakerColor`(src/lib/notes.ts)返回上表 7 个 tint token 名(不含红);徽章文字色随主题走 `--tint-*-ink` 变量。`tint-peach` 语义为青色,标识符名不动。
- 行为零改动:只动样式与结构性 class,不碰逻辑 / 事件 / 状态;不引 webfont;不做 Raycast 营销页组件。
