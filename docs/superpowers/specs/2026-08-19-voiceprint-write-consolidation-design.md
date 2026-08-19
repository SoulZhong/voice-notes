# 声纹库写入收口 设计

日期：2026-08-19
状态：待实施（Codex review 四轮共 17×P1 + 9×P2 已并入，见文末「修订记录」）
起因：2026-08-19「取消关联」功能三轮 codex review，每轮都在新位置发现同类问题

## 背景

用户报告「逐字稿换说话人无效、说话人无法取消关联」。结论分两半：

- 「换说话人无效」是显示层 bug（段落徽章是 ProseMirror NodeView 里的命令式 DOM，显示名不在节点 attrs 里，节点 `eq` 时 PM 复用旧 NodeView，即便整份重建也不刷新）。已修，与本设计无关。
- 「取消关联」是缺功能。实现它的过程中，codex review 三轮共 10 条 P1，**每轮修完，下一轮都在不同位置冒出同类问题**。按 systematic-debugging 的判据，**三轮以上、每轮换个地方冒出来 = 架构问题**。停止打补丁，改为收口。

## 根因

声纹库的写入**没有归属权**。互不相识的任务在改同一个 `voiceprints.json`：录制结束的 session 回写、指认触发的回灌与人物合并、取消关联触发的撤销、换模型触发的重建、identify 的自动应用与自动撤销。

它们之间只隔着 `VP_LOCK`（进程内，决定谁先写文件）与 `FEEDBACK_GATE`（只锁其中一部分）。三条缺口：

**一、没有任何一层知道"这组向量属于哪个模型空间"。**
切模型后重建成功、库标签已是新的；此时一个更早启动、用旧模型算完的写入落盘，库里就混进了旧空间的向量。库标签仍是新的，**启动自愈也检测不出来**——它只比标签，不比内容。

**二、复核与写入不原子。**
后台回灌读笔记确认「该说话人仍关联着目标人物」再写库，但复核与写入之间会被改关联的路径插入。`Reinforce` 尚可由随后的撤销补救，`MergePrior`（把一整个人物并进目标）**不由取消关联回滚**，一旦发生就留在库里。

**三、历史快照回放绕过一切检查。**
`restore_merged_person` / `undo_merge` / `restore_feedback` 把**存档里的整份质心原样插回**。而 `rebuild_for_model` 结尾会 `invalidate_all` 把全部合并日志标记失效，`restore_merged_person` **恰恰只接受已失效的日志**——重建之后任何一条老日志都能被「拆回」，把旧空间质心插进新库。

## 目标与非目标

**目标**

- 旧模型空间的向量永远写不进新库（从根上，不靠调用方自觉）
- 「检查关联 → 写库」不可被任何关联变更插入，**跨进程亦然**
- 换模型重建期间，其它写入进不来
- 用户点击不因后台重活而长时间等待

**非目标**

- 不引入 actor / 消息队列做库写入
- 不做乐观版本号（每次写带库版本、冲突重试）
- 不给非向量写入（改名、邮箱、删人）加**模型标签校验**——它们没有"模型空间"这回事。**注意这不等于不加文件锁**，见不变量五

## 设计

### 统一锁序

```
IDENTIFY_ACT_GATE → FEEDBACK_GATE → NoteLock(该笔记) → VP_LOCK → 声纹库文件锁
```

任何路径只能按此序取，不得反向。**重活（解码、嵌入、模型加载）一律在取 `FEEDBACK_GATE` 之前完成**；持锁期间只做读-改-写。

### 一、新算的向量写入必须声明模型空间

写向量的方法增加 `model: &str` 参数（调用方声明"我这组向量是用哪个模型算的"）。方法内部取锁之后、动数据之前比对；不等则丢弃、返回 mismatch、记日志与遥测（新增 `ErrorKind::VoiceprintSpaceMismatch`）。

**覆盖**：`upsert_from_session`、`reinforce_feedback`、`merge_with_embedder` / `merge_journaled`。

**不覆盖 `append_sample` / `append_sample_for_merge`**：它们写的是原始录音 WAV，与模型无关，而且正是重建赖以重算质心的素材。

**丢弃而不是报错**：声纹是增值功能，绝不能因它异常挡住主流程。但**必须留痕**——2026-08-19 的教训正是"系统连续 149 次知道自己坏了却什么都不做"。

**mismatch 必须清理占位账**：`reinforce_feedback` 若因 mismatch 返回，此前写下的 `complete=false` 占位条目必须删除，否则该 scope 会被永久判为"已经回灌过"，此后永不重试。测试须同时断言库与 ledger 都不变。

`rebuild_for_model` 是唯一能改 `embedding_model` 的写入，整段持锁。

### 二、历史快照回放必须带模型标签

新增两个持久化字段（均 `#[serde(default)]`）：`MergeJournalEntry.embedding_model`、`LedgerEntry.embedding_model`。

**标签来源**：journal 标签在 VP 锁内取自**被快照那一刻库的 `embedding_model`**（`MergePrior` 走 `merge_journaled(..., None, ...)`，没有 `TaggedEmbedder` 可用）；ledger 标签取自**通过模型比对并成功提交**的那次写入。

回放时比对：

| 快照标签 | 处置 |
|---|---|
| 等于库当前标签 | 照常整份还原 |
| 不等于，或旧条目缺该字段 | 只还原身份元数据（名字、redirects、样本文件副本），**`centroids` 与 `session_centroids` 两张表都清空** |

**两张表都要清**：种子注入与自动归并同时消费这两张表，只清主质心仍会把旧空间的会话质心注入新模型。测试须断言两张表皆空，且 `seed_clusters` 不再为该人产出种子。

**`restore_feedback` 的执行顺序**（避免实现者绕过 CAS）：

1. 先做原有 CAS：库当前状态必须等于账本记的 `after`；不等则不动，返回 `touched-since`
2. CAS 通过后：同模型 → 恢复完整 `before`；异模型 → 恢复去掉两类质心的 `before`

**样本恢复是硬条件，`restore_merged_person` 与 `undo_merge` 都算**：两者现在都是"先恢复人物 → best-effort 复制样本 → 删 journal"。清空质心之后**样本是唯一可跨模型恢复的依据**，一次复制失败就是永久丢失。改为：样本恢复失败 → **保留 journal 不删**；「人物已存在」的重试分支要**继续补样本**再收尾，且补样本必须幂等、不得覆盖第一次失败之后发生的新变化。

**清空质心之后必须排一次重建**，否则那个人永远长不回来（库标签已是当前模型，启动自愈不会再动它；`append_sample` 只写 WAV 不产生质心）。做法：

- store 层的回放方法返回 `needs_rebuild: bool`
- **命令/后台层在释放 VP 锁之后**调用 `spawn_voiceprint_rebuild`
- **不是**直接置 `REBUILD_PENDING`：那不是队列，只有已有重建在跑时置位才有意义，空闲时置位不会启动任何东西
- 适用于**所有异模型回放**，不只"样本刚恢复"那一种

### 三、后台任务在「复核 → 写库」全程持有该笔记的 NoteLock

后台任务（回灌、合并、撤销、identify 自动应用/撤销）：

1. **门外**完成解码、嵌入、模型加载
2. 取 `FEEDBACK_GATE`
3. 取**该笔记的 `NoteLock`**（跨进程）
4. 在锁内重读该笔记的当前关联并复核（原始稿读 `speakers.json`，修订稿读修订稿说话人表）
5. 复核通过才写 ledger 与声纹库
6. 逆序释放

**大部分关联写入者不动。** Aing 提交、重转写提交、活动 writer 的 `set_speaker_person`——它们**本来就持 `NoteLock`**，与上面的复核段天然互斥，无须改造、也不会再漏掉谁。

**但 assign / clear / 修订稿 assign 三处必须改**：后台任务需要知道"改之前关联的是谁"（`prior`）才能决定走 `MergePrior` 还是 `Reinforce`，而那个值在写入完成之后就**没了**——锁内重读只能读到新值。所以这三处要在**自己的 `NoteLock` 内**产出一份 transition receipt 并经 actor 回执带出来：

```
{ before_person: Option<String>, after_person: Option<String>, scope 指纹 }
```

它们仍然不碰 `FEEDBACK_GATE`。后台任务不再"锁内重读 prior"，而是：**prior 取自 receipt；锁内复核的是"当前关联是否仍等于 receipt.after_person"**。

这一条同时解决跨进程问题：`NoteLock` 是文件锁，另一个实例改不进来。

**门外准备的数据必须与锁内真值绑定。** `NoteLock` 只保证"复核之后不再被插入"，不保证门外那段计算所依据的版本还是当前版本——重转写会在任务计算期间整体替换 segments 与 speakers，即便最终仍关联同一个人，簇成员、source、起止时间都可能变了。因此准备阶段要产出 `PreparedFeedback`，至少绑定：

- 原始稿：speaker + 排序后的 `(seq, source, start_ms, end_ms)` 指纹
- 修订稿：revision + speaker + source_seqs 指纹
- 音频轨道元数据签名

锁内比对，不一致则丢弃或重新排队——**绝不拿旧段算出的嵌入去写当前 scope**。

**`NoteLock` 的获取必须是非阻塞 + 退避，不能持 gate 干等。** 已核实：`NoteLock::acquire()` 只尝试约 100ms 就返回 `None`，且**同进程不可重入**；而长期持锁者包括录制 writer（整场）、普通转码（转码 + 删 WAV 全程）、重转写（解码 + 识别 + 提交全程，分钟级）、mixed regen（全程）。协议：

1. 取 `FEEDBACK_GATE`
2. **非阻塞**试取该笔记 `NoteLock`
3. 失败 → **连 gate 一起释放**，退避后重新准备（含重新算指纹）再来；不得持 gate 等待
4. 成功 → 复核 → 写库 → 逆序释放

只调一次 `acquire()` 就放弃会静默丢掉回灌；持着全局 gate 循环等，则一篇正在录制的笔记会把所有笔记的回灌全堵死。两者都不可接受。

**identify 的两条路径要改成"已持锁"变体。** `auto_apply_one` 与自动撤销当前是「`IDENTIFY_ACT_GATE` → 自己取 `NoteLock` 的 assign → 释放 → `FEEDBACK_GATE`」。`NoteLock` 不可重入，新顺序下不能在外层锁住之后再调这些公共 writer。需新增"调用方已持 `NoteLock`"的 refined assign/unassign 变体，改为：

```
IDENTIFY_ACT_GATE → FEEDBACK_GATE → NoteLock → locked refined edit → VP_LOCK
```

`recover_identify_ops`（崩溃恢复）当前持 ACT 之后直接动反馈账与声纹库，既不取 `FEEDBACK_GATE` 也不取 `NoteLock`——**必须一并纳入协议**，否则崩溃恢复仍能与人工回灌并发。

**actor 与命令都不碰 `FEEDBACK_GATE`。** 早先的方案是让 lifecycle actor 等门，那会阻塞整个录制控制面（actor 是单线程串行信箱，开录/停录/实时段落写入全从它过）；改为命令线程持门也只是把等待挪到用户点击上。用 `NoteLock` 之后两者都不需要。

**复核所需的读取必须在锁内**：`prior`、`linked`、`seqs`、当前关联，都要在取到 `NoteLock` 之后读。锁外先读会拿到过期值，随后错误地 `MergePrior` 或撤错账。

### 三之二、所有 Reinforce 路径的重活移出门

`reinforce_person` 目前在门内解码全场音轨并逐段嵌入；`identify_note` 同样。长会议是分钟级——不移出去，用户点「选人」就会等数十秒到数分钟。

拆成：**门外**解码 + 嵌入 → **锁内**复核 + 账本 + 库提交。identify 与普通回灌都要拆，不是只拆 identify。

**解码移出门会暴露两处竞态**：

1. `track_pcm` 对同一笔记/信道固定用 `.<source>.refine.wav.tmp` 且开头先删它，并发解码会互删 → 改唯一临时名
2. 唯一临时名只解决"解码者之间"。转码持 `NoteLock` 会 rename M4A、删除 WAV，而门外解码不持锁，仍可能在 `exists` 与 `read` 之间踩空 → 准备阶段**短持 `NoteLock` 取一次稳定的音频快照**（取完即放，重活在锁外做）

### 四、模型标签与声纹计算器绑死

`model` 参数若只靠调用方自觉传对等于没有保证：现有缓存槽存的是无身份的 `Box<dyn SpeakerEmbedder>`，trait 本身也没有标签。

改为 `TaggedEmbedder { model: String, inner: Box<dyn SpeakerEmbedder> }`：标签在**构造实例时绑定**，无法脱离。缓存槽与会话载体都存它；取用时若 `model` ≠ 当前选型则丢弃重建。写库时的 `model` 一律取自该结构。

### 五、声纹库需要跨进程文件锁，覆盖**全部** RMW 入口

`VP_LOCK` 是模块静态 `Mutex`，只在进程内有效。抄 `store::notelock` 的模式加一把跨进程文件锁。

**覆盖范围是所有现有 `VP_LOCK` 写入口**，包括改名、邮箱、删除、样本与 journal——**不能只覆盖向量写入**。理由：声纹库的每次变更都是整份 `load → save`，改名同样是完整的读-改-写，用旧快照 save 一次就能把刚完成的模型重建整体盖回去。「非向量不收口」只表示它们不做模型标签校验，不表示不取文件锁。

**必须按"逻辑事务"划边界，而不是机械包住每个现有 `vp_guard()`。** 反例：`append_sample()` 的 `VP_LOCK` 在 inner 返回时就释放了，**之后**才写 journal 失效——只在原 guard 外面套一层文件锁的话，"样本写"与"journal 失效"之间仍能被另一进程插入。定义统一的 `voiceprint_write_guard = VP_LOCK + 文件锁`，由公共 mutator 持有到 **voiceprints + 样本 + journal 三者的整个逻辑事务结束**。

**当前实际暴露面**：已核实 MCP 子进程只 `load()` 声纹库、不写（`mcp/tools.rs:551/565`），眼下能撞上的只有"开两个 GUI 实例"，macOS 由 LaunchServices 天然拦着。**是真漏洞但眼下打不到**——仍要修，因为不变量一的承诺是"永远写不进"。

### 六、重建的触发保持一次性 + 排队

沿用 `REBUILD_RUNNING` 单飞 + `REBUILD_PENDING` 排队，**不因"库标签仍不一致"自动重跑**（永久失败时会变成无界循环）。发起重建一律走 `spawn_voiceprint_rebuild`（空闲即启动、忙则置 pending），不得直接操作 `REBUILD_PENDING`。

## 数据与兼容

- 新增两个持久化字段：`MergeJournalEntry.embedding_model`、`LedgerEntry.embedding_model`，均 `#[serde(default)]`
- 旧条目无该字段 → 视为来源不明，按不变量二只还原身份、清空两张质心表
- `voiceprints.json` 的 `embedding_model` 语义不变
- 新增声纹库跨进程锁文件（位置与命名比照 `NoteLock`）
- 无破坏性迁移

## 测试

- **旧模型写入被拒**：库标签 B，以 A 调 `upsert_from_session` / `reinforce_feedback` / `merge_journaled`，断言库**与 ledger**都不变且返回 mismatch
- **原始录音不受门禁**：以任意标签调 `append_sample`，断言正常写入
- **快照回放**：库标签 B，回放标着 A（或无标签）的合并日志与回灌账，断言身份回来了、两张质心表皆空、`seed_clusters` 不再产出该人
- **`restore_feedback` 顺序**：CAS 失败时库一字不变；CAS 通过且异模型时恢复的 `before` 不含两类质心
- **样本恢复是硬条件**：`restore_merged_person` 与 `undo_merge` 各一条——样本复制失败时 journal 必须保留；重试路径补齐样本后才收尾且不覆盖新变化
- **清空后有触发点**：异模型回放返回 `needs_rebuild=true`；调用方在释放锁后发起重建
- **复核在锁内**：关联在复核之后、写库之前被改，写入必须被拒（以可注入的时序钩子构造）
- **receipt 提供 prior**：assign/clear 返回的 receipt 里 `before_person` 必须是写入前的真值（写入后已不可恢复）
- **准备版本绑定**：门外准备完成后段落被重转写替换，锁内比对必须判不一致并丢弃
- **`NoteLock` 拿不到时不丢任务**：长期持锁者在场时，回灌应退避重排而非静默消失；且不得持 `FEEDBACK_GATE` 等待
- **identify 的 locked 变体**：外层已持 `NoteLock` 时调用不得再次取锁（不可重入会死锁）
- **`recover_identify_ops` 走同一协议**
- **声纹写事务完整**：样本写与 journal 失效之间不得被另一进程插入
- **`MergePrior` 同样受复核保护**
- **修订稿路径同样受保护**：修订稿改派后旧任务不得写入旧人物
- **`TaggedEmbedder`**：标签≠当前选型的实例不得被取用
- **临时文件不互删**：并发两路解码同一笔记同一信道，两边都拿到完整 PCM
- 现有 1393 条 Rust 测试不得变红

## 已知不做

- 非向量写入不做模型标签校验（但取文件锁，见不变量五）
- 不检测"库里已经混进了异空间向量"这一历史污染。本设计只保证此后不再发生；已有污染需一次重建清掉

## 修订记录

**Codex 设计轮一**（2×P1 + 2×P2）：快照回放漏网 → 新增两个持久化字段；原方案"写方法内部持 `vp_guard`" + "后台任务先持 `vp_guard` 再调它们"会自死锁 → 改用 `FEEDBACK_GATE`；`model` 来源未固化 → 不变量四；`append_sample*` 移出门禁范围。

**Codex 设计轮二**（5×P1 + 3×P2）：actor 等门会阻塞整个录制控制面，`identify_note` 在门内做分钟级重活；修订稿关联路径与 identify 自动路径未纳入协议；不变量四只是纪律；`VP_LOCK` 只有进程内原子性；样本恢复失败仍删 journal；质心置空须清两张表；清空后无重新长出的触发点；`restore_feedback` 的 CAS 与回放顺序自相矛盾。

**Codex 设计轮三**（6×P1 + 2×P2）：

- 枚举"所有改关联的路径"这条路本身走不通——这一轮又漏了 Aing 提交、重转写提交、活动 writer 三个写入者，且进程内的门挡不住另一个实例 → **改为后台任务在「复核 → 写库」全程持该笔记的跨进程 `NoteLock`**，关联写入者一个都不用改，跨进程一并解决
- 复核所需的读取（prior/linked/seqs）必须在锁内，锁外先读会拿到过期值
- 跨进程文件锁必须覆盖**全部** RMW 入口而非仅向量写入——改名同样是整份读-改-写，能把重建结果整体盖回去
- `REBUILD_PENDING` 不是队列，空闲时置位不启动任何东西 → 改为 store 返回 `needs_rebuild`、调用方释放锁后调 `spawn_voiceprint_rebuild`；且适用于所有异模型回放
- `undo_merge` 同样要纳入"样本恢复是硬条件"，并定义部分成功后的幂等重试
- 普通回灌的解码/嵌入同样在门内 → 所有 Reinforce 路径都要拆，不只 identify
- 解码移出门后 `track_pcm` 的固定临时名会互删 → 唯一临时名或独立解码锁
- `MergePrior` 无 `TaggedEmbedder`，journal 标签应取自被快照库的 `embedding_model`；mismatch 时必须清理 `complete=false` 占位账

**Codex 设计轮四**（4×P1 + 2×P2）：

- 「关联写入者一律不动」**撤回**：`prior` 在写入完成后不可恢复，后台任务锁内只能读到新值 → assign/clear/修订稿 assign 必须在自己的 `NoteLock` 内产出 transition receipt 并经 actor 回执带出
- 门外算的嵌入未与锁内真值绑定（重转写会整体替换 segments/speakers）→ 新增 `PreparedFeedback` 版本指纹，锁内比对不一致即丢弃或重排
- `NoteLock::acquire()` 只试约 100ms、同进程不可重入，而录制/转码/重转写/mixed regen 都是长期持锁者 → 明确"非阻塞试锁、失败连 gate 一起释放、退避重排"的协议
- identify 的 `auto_apply_one` 与自动撤销会自取 `NoteLock`，新顺序下不可重入 → 新增 locked 变体；`recover_identify_ops` 也必须纳入协议
- 唯一临时名只解决解码者互删，与转码 rename/删 WAV 的竞争仍在 → 准备阶段短持 `NoteLock` 取稳定音频快照
- 文件锁要按逻辑事务划边界（`append_sample` 的样本写与 journal 失效当前分处两段临界区）
