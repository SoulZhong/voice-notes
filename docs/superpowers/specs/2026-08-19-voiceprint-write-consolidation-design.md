# 声纹库写入收口 设计

日期：2026-08-19
状态：待实施（用户已认可方向；Codex review 一轮 2×P1 + 2×P2 已并入，见「修订记录」）
起因：2026-08-19「取消关联」功能三轮 codex review，每轮都在新位置发现同类问题

## 背景

用户报告「逐字稿换说话人无效、说话人无法取消关联」。定位过程见对话记录，结论分两半：

- 「换说话人无效」是显示层 bug（段落徽章是 ProseMirror NodeView 里的命令式 DOM，显示名不在节点 attrs 里，节点 `eq` 时 PM 复用旧 NodeView，即便整份重建也不刷新）。已修，与本设计无关。
- 「取消关联」是缺功能。实现它的过程中，codex review 三轮共提出 10 条 P1，**每一轮修完，下一轮都在不同位置冒出同类问题**：

| 轮次 | P1 | 位置 |
|---|---|---|
| 一 | 3 | 重建的快照与单飞、撤销凭据、反向执行竞态 |
| 二 | 3 | 重建代次的提交点、`MergePrior` 漏复核、账本键 |
| 三 | 4 | 重建无界循环、旧模型在途写入、复核不原子、旧键删错 |

按 systematic-debugging 的判据，**三轮以上、每轮换个地方冒出来 = 架构问题，不是假设错了**。停止打补丁，改为收口。

## 根因

声纹库的写入**没有归属权**。四类互不相识的后台任务在改同一个 `voiceprints.json`：

1. 录制结束的 session 回写（`upsert_from_session`）
2. 指认触发的回灌与人物合并（`reinforce_feedback` / `merge_journaled`）
3. 取消关联触发的撤销（`restore_feedback`）
4. 换模型触发的重建（`rebuild_for_model`）

它们之间只隔着 `VP_LOCK`（决定谁先写文件）与 `FEEDBACK_GATE`（只锁其中第 2、3 类）。于是有两条无法靠加守卫堵死的缺口：

**缺口一：没有任何一层知道"这组向量属于哪个模型空间"。**
用户从 CAM++ 切到 ERes2NetV2，重建成功、库标签已是 ERes2NetV2；此时一个更早启动、用 CAM++ 算完的回写任务落盘，库里就混进了 CAM++ 空间的向量。库标签仍是 ERes2NetV2，**启动自愈也检测不出来**——它只比标签，不比内容。

**缺口二：复核与写入不原子。**
后台回灌任务在 `FEEDBACK_GATE` 内读笔记确认「该说话人仍关联着目标人物」，然后写库。但改关联的那条路（lifecycle actor 的 `AssignPerson`/`ClearPerson`）**不取这把锁**，复核与写入之间始终有缝。`Reinforce` 尚可由随后的撤销补救，`MergePrior`（把一整个人物并进目标）**明确不由取消关联回滚**，一旦发生就留在库里。

## 目标与非目标

**目标**

- 旧模型空间的向量永远写不进新库（从根上，不是靠调用方自觉）
- 「检查关联 → 写库」不可被关联变更插入
- 换模型重建期间，其它写入进不来

**非目标**

- 不引入 actor / 消息队列。那是更大的机制，解决同一个问题，不必要
- 不做乐观版本号（每次写带库版本、冲突重试）。同上
- 不收非向量写入（改名、邮箱、删人、合并关系的账目操作）。它们没有"模型空间"这回事，收进来不会更安全，只会让改动与审查面翻倍

## 设计

四条不变量。一、一之二、三落在 `VoiceprintStore` 这一层；二落在 lifecycle actor；四是调用方纪律。

### 一、向量写入必须声明模型空间

写向量的方法一律增加 `model: &str` 参数（调用方传"我这组向量是用哪个模型算的"）。方法内部持 `vp_guard()` 之后、动数据之前比对：

```
if model != vp.embedding_model { 丢弃,返回 SkippedModelMismatch,记日志与遥测 }
```

**新算出来的向量**（调用方现场嵌入得到）:

- `upsert_from_session`
- `reinforce_feedback`
- `merge_with_embedder` / `merge_journaled`（合并会合并质心）

**不收 `append_sample` / `append_sample_for_merge`**：它们写的是原始录音 WAV，与模型无关，而且正是重建赖以重算质心的素材。按旧标签拒绝它只会丢掉有价值的原声（codex review 设计轮 P2）。

**快照回放另算，见下一节**——它们插回的是**存下来的旧向量**，光给方法加个参数证明不了那份快照来自哪个模型。

**丢弃而不是报错**：声纹是增值功能，绝不能因为它异常挡住主流程（既有哲学，见 `load_voiceprint_seeds` 注释）。但**必须留痕**——2026-08-19 的教训正是"系统连续 149 次知道自己坏了却什么都不做"，所以除日志外还上报一条 `ErrorKind::ModelLoad` 之外的遥测（新增 `ErrorKind::VoiceprintSpaceMismatch`）。

**`rebuild_for_model` 是唯一能改 `embedding_model` 的写入**，且它整段持 `vp_guard`，跑的时候别人写不进来。

### 一之二、快照回放必须带模型标签

三处会把**历史快照里的质心原样插回**库：

- `restore_feedback`（回灌账本的 before/after 快照）
- `restore_merged_person`（合并日志里的整份 loser 人物）
- `undo_merge`（同上）

其中最危险的一条已核实：`rebuild_for_model` 结尾会 `invalidate_all` 把**所有**合并日志标记失效，而 `restore_merged_person` **恰恰只接受已失效的日志**——于是重建之后，任何一条老日志都能被「拆回」，把旧模型空间的质心原样插进新库，库标签却仍是新的（codex review 设计轮 P1）。

因此**必须新增持久化字段**（推翻本文早先"不新增持久化字段"的说法）：

- `MergeJournalEntry.embedding_model`
- `LedgerEntry.embedding_model`

回放时比对：

| 快照标签 | 处置 |
|---|---|
| 等于库当前标签 | 照常整份还原 |
| 不等于，或旧条目没有该字段 | **只还原身份元数据**（名字、redirects、样本文件副本），**质心置空** |

选"质心置空"而不是"拒绝回放"：人被拆回来了、名字在、样本文件也还原了，只是暂时没有声纹——下次重建就从样本重新长出来。拒绝回放会让用户彻底拿不回这个人，代价大得多。

### 二、关联变更去排后台任务那条队

lifecycle actor 执行 `AssignPerson` / `ClearPerson` **之前先取 `FEEDBACK_GATE`**，再走原有的 `NoteStore` 方法。

于是两条路径的锁序是：

- actor：`FEEDBACK_GATE` → `EDIT_LOCK` → 笔记 flock
- 后台任务：`FEEDBACK_GATE` → 无锁读笔记 → `VP_LOCK`

「复核关联 → 写库」整段在 `FEEDBACK_GATE` 内完成，关联变更插不进来。根因说的正是"actor 不取这把门"，这就把它补上了。

**为什么不是把 `vp_guard` 塞进 `NoteStore`**（本文最初的写法）：那样后台任务会先持 `vp_guard`、再调用内部也要取 `vp_guard` 的写方法，而 `std::sync::Mutex` **不可重入**——稳定自死锁（codex review 设计轮 P1）。现有代码里只有 `merge_locked` 是"调用方已持锁"的形态，要走那条路就得给每个写方法配一套 `*_locked` 变体，改动面和出错面都大得多。改用 `FEEDBACK_GATE` 既不嵌套 `VP_LOCK`，也不动 `NoteStore` 的锁序。

**锁图已核实无环**：现有代码中没有 `VP_LOCK → EDIT_LOCK/笔记锁` 的反向路径；持 `VP_LOCK` 期间对笔记只做无锁 `load`；合并之后的笔记扫描发生在 `VP_LOCK` 释放之后；录制/重转写长期持笔记锁的路径只读声纹库、不申请 `VP_LOCK`。

**已知代价**：后台任务正持着 `FEEDBACK_GATE` 时（回灌/合并/撤销，中间含嵌入，实测秒级），用户点「选人」/「取消关联」会等到它做完。这是用户主动点击路径上的一次可感知等待，换来的是库不被悄悄污染——判断为划算。

**纪律要求（写进注释）**：`FEEDBACK_GATE` 的持有面已从"后台任务之间"扩大到"用户点击也排这条队"。此后任何往这把门里添加更长耗时操作的改动，都会直接表现为点按钮卡顿。解码/嵌入这类重活能挪到取门之前的就要挪。

### 三、重建的触发保持一次性 + 排队

沿用已实现的 `REBUILD_RUNNING` 单飞 + `REBUILD_PENDING` 排队，**不再因"库标签仍不一致"自动重跑**（那会在永久失败时变成无界循环）。真正的模型切换必然经过 `set_settings`，那条路会置 `PENDING`；其余"卡住"情形由下次启动的自愈兜，一次启动最多一轮，天然有界。

### 四、`model` 参数的来源必须与嵌入器同源

声明的标签和实际加载的权重必须来自**同一次设置读取**，否则声明与事实可以分叉（设置在两次读取之间被改）。规则：谁创建 embedder，谁在同一处定下标签，一路传下去，中途不得再读设置。`speaker_model_path_for(&tag)` 就是为此存在的。

## 数据与兼容

- 新增两个持久化字段：`MergeJournalEntry.embedding_model`、`LedgerEntry.embedding_model`，均 `#[serde(default)]`
- 旧条目无此字段 → 视为来源不明，回放时按上表只还原身份、清空质心
- `voiceprints.json` 的 `embedding_model` 语义不变
- 无破坏性迁移

## 测试

- 旧模型的向量写入被拒绝：库标签为 B，以 A 调 `upsert_from_session` / `reinforce_feedback` / `restore_feedback`，断言库内容不变且返回 mismatch
- 重建独占：重建持 `vp_guard` 期间的写入不得穿插
- 关联取消后回灌不落库：取消关联后再跑回灌，断言库不变
- `MergePrior` 同样受复核保护：取消关联后不得再发生人物合并
- 锁序无环：以固定顺序的双路径压测（或以代码审查+注释固化，node 环境无法真并发时退化为审查）
- **快照回放**：库标签为 B，回放一条标着 A（或无标签）的合并日志/回灌账，断言人物身份回来了、质心是空的
- **调用链级**：构造"任务用 A 创建 embedder → 中途设置切到 B → 任务提交"，断言写入被拒（只测 store 层收不到调用点传错标签的问题）
- 现有 1393 条 Rust 测试不得变红

## 已知不做

- 非向量写入不收口（见非目标）
- 不检测"库里已经混进了异空间向量"这一历史污染。本设计只保证此后不再发生；已有污染需要一次重建才能清掉，而重建本就是用户可触发的动作

## 修订记录

**2026-08-19，Codex review 设计轮**（2×P1 + 2×P2，全部并入上文）：

- P1 快照回放漏网：`restore_merged_person` / `undo_merge` / `restore_feedback` 会把旧空间质心原样插回，且重建会把全部合并日志标记失效、正好让它们变得可拆回。→ 新增两个持久化字段，标签不符只还原身份、清空质心。本文早先"不新增持久化字段"的说法作废。
- P1 自死锁：原设计要求"写方法内部持 `vp_guard`"+"后台任务先持 `vp_guard` 再调这些方法"，而 `std::sync::Mutex` 不可重入。→ 不变量二改为让 actor 取 `FEEDBACK_GATE`，不再把 `vp_guard` 塞进 `NoteStore`。
- P2 `model` 来源未固化 → 新增不变量四。
- P2 `append_sample*` 不该受门禁（写的是原始 WAV，与模型无关，且是重建素材）→ 移出范围。
