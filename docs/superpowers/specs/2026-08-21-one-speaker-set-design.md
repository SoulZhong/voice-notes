# 一波说话人:修订稿身份统一到原始稿说话人表

日期:2026-08-21 · 状态:已获用户方向认可("可以")

## 原则(用户定)

一场会议只有一套说话人名单。修订稿不得另立一套说话人/人物关联;
发现一个说话人里混了多人,用拆分把人拆出来(写回唯一名单)。

## 问题

修订稿(aing.json)每段自存 speaker(R 域)+ name + person_id,生成时从源段落
**逐段**抄旧关联(refine/mod.rs build_paragraphs 的 old_meta 兜底)。一个 R 簇底下
能混着多个人的暗账,已实测炸出:

- 同名胸牌出现两次(R1 与 R2 都解析成"郎佳奇")
- 取消关联被"当前关联已被修改,拒绝撤销覆盖"误拦(段落间 person_id 不一致,
  CAS 全等检查必败;真实数据:R2 = 214 段 P455 + 2 段空,R4 = P454/P455/空三种混)
- 拆分入口点"说话人 5"弹窗却叫"新说话人 2"(R↔S 两套编号互译)

## 方案

### 1. 数据:修订稿段落只存"来自哪些源段落",不存身份

- `RefinedParagraph.speaker` 存 **S 域 id**(生成时取源段落的 `segment.speaker`;
  连续同 speaker 段合并,沿用 MAX_PARA_MS 上限)。无 speaker 的源段落归入 `""`。
- `name`/`person_id` 字段**停止写入**(struct 保留,反序列化旧文档兼容;显示端忽略)。

### 2. 显示:身份一律现场查唯一的说话人表

修订稿胸牌与段落标签从 `note.speakers` 解析(speakerLabel/speakerColor 同一套,
全局编号、颜色与原始稿一致)。原始稿改名/关联/拆分,修订稿立刻跟着变。

### 3. 操作:修订稿视图直接用 S 域命令

| 修订稿操作 | 现在 | 改为 |
|---|---|---|
| 改名 | rename_refined_speaker | rename_speaker |
| 选人关联 | assign_refined_person | assign_note_speaker_person |
| 取消关联 | clear_refined_speaker_person | clear_note_speaker_person |
| 标记多人 | openMultiForRefined(R→S 映射) | openMultiForRaw(直接就是 S) |

三个 refined 专用命令与 store 层 assign_refined_person / unassign_refined_person_if /
rename_refined_speaker 全部删除;拆分面板的 sourceLabel/candidateCounts 映射解释
(mapExplain)随之删除。

### 4. 说话人上下文推断(identify):目标改为 speakers.json

- 簇来源 `cluster_members_from_doc` 不变——文档换 S 键后自动按 S 分簇。
- 自动应用:改用 notes.rs 既有的 `assign_speaker_person_if`(期望"当前未关联"
  才写,与"自动应用前置=簇原本无关联"的既有语义严丝合缝)。**不复用
  assign_note_speaker_person 命令**——命令自带 spawn_feedback,自动路径有自己的
  同步回灌,复用会双重回灌(自查发现)。
- 建议确认(手动)则复用命令本体(EditOp::AssignPerson + spawn_feedback),
  与用户在胸牌上手选人走同一条路。
- 撤销:新增 `clear_speaker_person_if(speaker, expect_person)`——speakers.json 每
  说话人单值 person_id,CAS 天然不会再有"段落间不一致"这一类误拦。
- 建议确认(apply_identify_suggestion)同样走 S 域。回灌/撤销回灌账本逻辑不动
  (它们键的是 seqs 集合与人物,与 R/S 无关)。

### 5. 重聚类降级为顾问

pipeline 的 recluster 阶段不再决定段落分组(build_paragraphs 忽略其标签);
embedding 与种子近邻继续喂 identify。"自动检测某说话人混了多人并建议拆分"
是它的下一个顾问职责,**列为后续,不在本期**。

### 6. 拆分同步简化

sync_refined_after_split 保留 speaker 改写(dest 本来就是 S id)与跨组标 stale;
person/name 改写删除(字段已不承载语义,统一清 None)。

### 7. 旧修订稿(R 键存量)

盘上不迁移。显示端兜底:段落 speaker 不在 note.speakers 时,按其 source_seqs 的
**多数源段落 speaker** 映射到 S;操作作用于映射后的 S。重新 Aing 后自然写成新格式。
空 source_seqs 的用户插入块无说话人(现状不变)。

### 8. 后端出口跟着查表(自查发现)

凡读段落 `name`/`person_id` 的出口改为按 note.speakers 解析:markdown 导出
(export.rs 的 R 前缀特判一并删除)、MCP get_note 修订稿载荷。图谱(aing_graph)
若引用段落身份,同口径处理。

### 9. 不动的部分

LLM 润色/部分重试/WYSIWYG 保存/实体图谱只关心文本与下标,全部不动。
声纹回灌"不连带撤销"的 2026-08-19 范围决定不动。

## 代价(已在方向确认时说明)

实时录制产生的碎片说话人("说话人467"这类)不再被重聚类悄悄归并,修订稿
原样露出;清理路径 = 识别建议 / 手动关联 / 拆分。两个视图永远对得上。

## 测试

- build_paragraphs:S 分组合并、无 speaker 段、MAX_PARA_MS 边界
- identify:自动应用走 assign_speaker_person_if(已关联则跳过);撤销 CAS
  (被人工改过则拒绝且状态回退)
- 旧 R 文档:显示映射到多数 S;操作作用于映射后的 S(前端测试)
- sync_refined_after_split:整段改派原位改 speaker、跨组标 stale(既有测试改口径)
