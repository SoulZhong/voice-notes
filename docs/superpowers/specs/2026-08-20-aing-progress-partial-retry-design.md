# Aing 运行可观测与失败块部分重跑 设计

日期：2026-08-20
状态：待实施(Codex 额度耗尽至 2026-09-19,经用户批准本轮跳过 Codex 评审;自查记录见「自查」节)
分支：`aing-progress-partial-retry`（基于 master，独立于 PR #136）

## 要解决的问题

用户点了 AI 按钮之后只有一个转圈图标。一场长会议的 Aing 要跑几分钟到十几分钟,期间:

1. **不知道在干什么、还要多久**——"感觉卡在这儿了"(2026-08-20 用户原话)
2. **部分失败后只能整篇重跑**——横幅说"部分段落 AI 处理失败,可重新执行",但「重新执行」
   把已成功的块也全部重发大模型,token 全部重花

## 现状事实(已核实)

- LLM 阶段按块跑:`CHUNK_CHARS = 3000` 字一块,每块最坏 `CHUNK_TIMEOUT_S×2 = 360s`;
  失败块保原文,**只计数**(`LlmOutcome::Partial(usize)`),不记哪些块失败
- `polish()` 已有逐块 `heartbeat` 回调(为 lifecycle 滞留自愈防误判卡死而生)——
  进度信号存在,只是没送到界面
- 重跑入口 `refine_note` 重跑整条流水线(filter → recluster → llm → entities)
- 关系回填已有逐步进度事件先例(`relation_backfill_progress` emit),前端消费模式现成
- 术语表(glossary)在块间**顺序前传**积累
- 每块同时抽实体/关系;实体阶段已有独立的失败态与回填机制

## 用户决定

- 部分重跑范围:**只重发失败的 LLM 块**(聚类本地几乎不败;实体有独立重跑入口)
- 进度形态:**AI 按钮旁行内进度**(「精修中 3/8 · 约剩 4 分」)

## 设计

### ① 失败块落盘

`RefinedDoc` 新增 `#[serde(default)] pub llm_failed_paragraphs: Vec<usize>`
(失败块覆盖的段落下标,升序去重)。

- `polish()` 返回值扩展:携带失败块的段落下标集合(现 `Partial(usize)` 的计数语义
  由 `len()` 派生,不破坏既有匹配处)
- 落盘时机 = 现有 `stages.llm` 写盘那一次,**不加新写点**;`Done` 时写空数组
- 下标锚定(**自查修正**,原稿假设有误):WYSIWYG 整篇保存(`save_refined_paragraphs`)
  是可以**增删段落**的(orig_index/index_map 重排),"约束式写入下下标恒稳"不成立。
  闭合方式:该保存闭包里 old→new 的 `index_map` 现成——**在同一次写盘里把
  `llm_failed_paragraphs` 一并重映射**(被删的段移出列表)。其余触点核实过不改段数
  (`apply_refined_texts` 只改文本、`sync_refined_after_split` 只改归属);整写
  (重新 Aing/重转写)重算或清空列表。retry 侧仍保留越界防御性剔除

### ② 部分重跑

命令 `retry_failed_refine(note_id)`:

- 守卫与 `refine_note` 完全同一套(Aing 中拒、与 lifecycle 的 refining 标记同进退,
  滞留自愈心跳同样上报——部分重跑也是无界网络操作)
- 读 `aing.json`:`llm_failed_paragraphs` 为空或字段缺失(旧产物)→ Err
- 只对失败下标的段落重新分块(复用 `chunk_indices` 作用于子集),逐块重发
- 成功块:文本写回 + 从失败列表移除;写回走**约束式写入 + revision 递增**
  (编辑器冲突重载机制现成);全清 → `stages.llm = "done"`,否则仍 `"partial"`
- 失败下标越界(理论上只有整写后才会,而整写会清列表)→ 防御性剔除并记日志

**明说的取舍**:

- 术语表顺序前传拿不到:补跑块以**空术语表**起步,质量可能略逊于一次跑通——
  失败块本来保的是原文,仍是净改善。不做"从成功块重建术语表"(那要缓存每块术语表,
  复杂度不值)
- **只补文本润色,不补实体/关系**:失败块当初没抽到的实体不在本次补;实体阶段
  有独立机制,两边搅在一起会把图谱治理拖进来
- 部分重跑期间的 AI 日志(ailog)照常逐块记,标注 retry 来源

### ③ 进度事件

`aing_progress` (Tauri emit,复用 relation_backfill_progress 的消费模式):

```
{ note_id, stage: "recluster" | "llm" | "entities" | "llm_retry",
  done: u32, total: u32, avg_chunk_ms: u64 }
```

- LLM 阶段:`polish` 的 heartbeat 回调扩展为带 `(done, total, avg_ms)`;每块开工前发
- recluster / entities:各在阶段开始时发一次(total=0 表示"不可分块的阶段")
- 平均耗时由后端算(前端只做乘法):`avg = 已完成块总耗时 / 已完成块数`
- 事件是 best-effort:发失败不影响流水线(与既有 emit 同哲学)

### ④ 前端呈现

- `aiState === "running"` 时按钮旁行内一条:
  - LLM 阶段:`精修中 {done}/{total}` + 当 `done ≥ 2` 时追加 `· 约剩 {ceil(avg×(total-done))}`
    (前两块没完成不显示时间——没数据不瞎估)
  - 其它阶段:`聚类中…` / `抽取实体…`
- 部分失败横幅:加**「只重试失败段落({n} 段)」**按钮;原「重新执行」保留;
  旧产物(无失败列表)只显示全量入口
- 部分重跑运行中复用同一条行内进度(stage = llm_retry,文案「重试失败段 {done}/{total}」)
- 事件跨页残留防御:进度状态随 note id 切换清零;`all` 终态事件到达时清行内进度

## 数据与兼容

- `llm_failed_paragraphs`:`#[serde(default)]`,旧文件可读;旧产物无部分重跑入口
- 无新增锁;写回走既有 NoteLock/约束式写入
- 事件新增一个,无 IPC 破坏

## 自查(替代本轮 Codex 设计审,2026-08-21)

- a. 下标稳定性:**发现原稿假设错误**(WYSIWYG 可增删段)→ 已改为 save 内 index_map
  同步重映射(见上)
- b. 并发与守卫:retry 走与 refine_note 完全相同的 lifecycle 消息骨架
  (RefineRequest 拒重入 / RefineProgress 心跳 / RefineFinished 收尾),无新锁
- c. polish 返回值:`Partial(usize)` 改 `Partial(Vec<usize>)`(len 即旧计数),
  调用点一处 + 测试三处,无外部消费者
- d. 事件:照搬 relation_backfill_progress 的 struct+emit+listen 模式(含终态清理)

## 测试

- polish 返回失败下标:多块混合成败 → 下标集合正确;全成 → 空
- 部分重跑:只有失败块的段落文本变化,成功块一字不动;全清后 stages.llm=done;
  再跑一次报"没有失败段落"
- 越界下标被防御性剔除
- WYSIWYG 保存增/删/重排段后,失败列表随 index_map 正确重映射(删的段消失,余下指向原段)
- 约束保持:部分重跑不改段数/说话人/时间戳(apply 同款断言)
- 进度事件序:llm 阶段 done 单调递增至 total;avg 只在 done≥1 时非零
- 前端:done<2 不显示 ETA(组件测试);切笔记清进度
- 现有 1439 条 Rust 测试不得变红

## 未做的(明确划出范围)

- 不做全阶段断点续跑(聚类本地几乎不败;实体有独立回填)
- 不做 LLM 中途 checkpoint 落盘(想要时是独立增量,不阻塞本设计)
- 不做完成块文本实时刷进编辑器(动落盘时序与编辑冲突边界,用户已选行内进度形态)
- 不重建术语表前传
