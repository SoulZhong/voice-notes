# 笔记详情页 markdown 编辑器改造(Milkdown)设计

日期:2026-07-30
状态:已评审通过,待实施计划

## 背景与目标

笔记详情页(`src/routes/notes/[id]/+page.svelte`)当前的转写内容是纯文本方案:原始稿用段级 `contenteditable="plaintext-only"` 编辑(失焦保存、`expectedText` 乐观并发),精修稿只读并做实体高亮(`splitMentions()`)。仓库中不存在任何 markdown 解析/编辑依赖,markdown 只出现在导出/MCP/钩子的输出端(后端手拼字符串)。

目标:将笔记详情页的**原始稿与精修稿统一为所见即所得(WYSIWYG)markdown 编辑器**,同时:

- 原始稿保留段结构(说话人、时间戳、音频对齐、段级编辑命令);
- 精修稿从只读变为可编辑,并保留实体高亮(可跳转知识图谱);
- 录音页实时转写区、导出、MCP、钩子外发路径不动。

## 选型

**Milkdown**(`@milkdown/kit`,ProseMirror 系、markdown 原生、框架无关)。一套内核跑两种配置:原始稿受限 schema,精修稿完整 commonmark schema。

备选对比:TipTap 生态最好但 markdown 序列化非原生、包体大;ProseMirror 直用依赖最轻但所有编辑器 UI 自建、工作量最大。

## 1. 架构与组件

新增 `src/lib/editor/` 模块,页面层只换渲染壳,数据层命令尽量复用:

- **`MarkdownEditor.svelte`** — Milkdown 壳组件,接收 `mode: 'segments' | 'refined'`。主题按 DESIGN.md 现有 token 定制(沿用 `editable-text`、`transcript-container` 规范),不引入 Milkdown 官方主题。
- **`segmentSchema.ts`** — 原始稿受限 schema:文档 = 自定义 `transcriptSegment` 节点列表,节点属性携带 `seq/source/speaker/startMs`。说话人徽章和时间戳用 NodeView 渲染为不可编辑前缀;点击徽章弹现有说话人菜单(走 `set_segment_speaker`);删除段仍走行侧按钮。段内允许行内 markdown(加粗、斜体、行内代码、链接)。**结构锁定插件**拦截 Enter(不分段,改为提交当前段)与段首 Backspace(不合并段)。
- **`refinedSchema.ts`** — 精修稿完整 commonmark schema + 自定义 `entityMention` mark。实体高亮悬浮时出小弹层提供"打开知识图谱"跳转(避免单击跳转与编辑光标抢手势)。
- **`editorDoc.ts`** — 纯逻辑层(可单测):segments/RefinedDoc ↔ 编辑器文档的双向构建与序列化、按 mention 字符偏移打 mark、变更 diff 出提交载荷。
- 保持**常驻编辑态**(与现有交互哲学一致):不做阅读/编辑两态切换,点击即打字。

## 2. 数据流与持久化

### 原始稿:零后端改动

编辑器按段序列化;某段失焦且内容变化 → 复用现有 `edit_segment { noteId, seq, expectedText, newText }`(乐观并发校验保留)。段文本从此可能含行内 markdown 源码(如 `**重点**`),存储字段仍是纯字符串,属于超集演进;导出/MCP 侧无需迁移。

### 精修稿:新增后端能力(现为只读)

新增 Tauri 命令 `save_refined { noteId, revision, paragraphs }`:

- 前端整篇序列化为段落数组:保留原段落的 `speaker/ts` 属性;用户新插入的标题/列表等块为无属性块;
- 后端在 flock 保护下原子重写 refined 存储并递增 `revision`;`revision` 不匹配即拒绝(与段编辑的乐观并发同风格);
- 保存时机:防抖自动保存(失焦 + 停顿 2s),失败 toast 并回滚。

### 实体高亮生命周期

mentions 按字符偏移存储。前端序列化时对比段落文本与载入时原文,文本有变的段落在载荷中标记为 `dirty`;后端保存时丢弃 dirty 段落的 mentions(该处高亮失效),下次精修/实体对齐时重建。

## 3. 错误处理与兼容

- 段编辑冲突(`expectedText` 不符)→ 与现状一致:回滚该节点文本 + `refresh()` 重建文档 + 提示。
- `save_refined` 冲突 → 拉取最新精修稿重建文档,提示"内容已在别处更新"。
- 序列化往返兜底:提交前做 parse→serialize round-trip 校验,不一致则按原文提交并上报 ailog,避免编辑器 bug 静默改写用户内容。
- 旧笔记无需数据迁移;录音页、导出、钩子外发路径全部不变。

## 4. 测试

沿用仓库现有 vitest 纯逻辑单测风格(不引入组件测试框架):

- `editorDoc.test.ts`:segments↔doc 往返、mention 偏移打标、结构锁定边界(Enter/段首退格)、变更 diff 载荷;
- Rust 侧:`save_refined` 并发校验(revision 不匹配拒绝)与原子重写测试,与 `edit_segment` 现有测试对齐。

## 依赖增量

`@milkdown/kit`(ProseMirror 系),无其他新依赖。
