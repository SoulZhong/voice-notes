# 混杂说话人的隔离与拆分 实施计划

设计：`2026-08-20-mixed-speaker-split-design.md`（Codex 五轮评审后收窄版）
分支：`mixed-speaker-quarantine`

## 阶段划分（每阶段独立提交、独立可测）

### A｜标记字段与隔离消费面（纯后端）
1. `SpeakerMeta.multi_speaker`、`Person.voiceprint_quarantined`（均 `#[serde(default)]`）
2. 读路径过滤：`seed_clusters`；自动归并(目标/候选/S-Norm cohort)；identify 四类召回;
   identify 应用的 MergePrior 与自动回灌
3. 写路径拒绝：`append_sample`；`merge_person`/`merge_journaled` 任一方；兜底截声;
   重转写/再入库对 `multi_speaker` 簇不入库不写样本
4. 回放保护：三条 restore 保留隔离、隔离人物不恢复质心表；`rebuild_for_model` 对隔离
   人物只清空；封禁样本 hash 名单(回放过滤用,Phase C 落地删除时写入)
5. 每条一个测试

### B｜溯源（后端）
1. 样本 receipt + 同锁 WAL（app-data/VP 根;intent→WAV→complete;启动三分支恢复）
2. 接入写点：停录 `append_sample`（带 note_id/cluster_id）;合并样本迁移时同步迁移 receipt
3. 顺带修 resolve 既有 bug：存在性检查先 resolve、与 append 同临界区
4. 质心贡献 receipt：`upsert_from_session` 两条路径提交时追加（只记不用）
5. 测试：WAL 三分支、hash 拒删、迁移后可定位、resolve 回归

### C｜打标流程 quarantine_only（后端命令 + 前端）
1. 阶段机存储（app-data/VP 根,per-op 文件）:plan→marked→samples_handled→residual_decided→released→done
2. 命令：`mark_speaker_multi`（计算受影响人物、落 plan、置两标记、作废 identify 建议）;
   `speaker_multi_impact`（count 占比估算、会话质心存在性、元数据说明、样本清单含来源标注）;
   `resolve_multi_residual`（accept / baseline;baseline 走逐人重算含 total_ms/last_seen 重置）
3. 逐人重算 `rebuild_person_from_samples`（设计列明的不变量;与全库 rebuild 共用调度）
4. 前端：chip 菜单加「标记为多人混杂」;影响面板（诚实呈现:全部历史样本+来源未知+默认不勾）;
   试听勾删;二选一
5. 启动恢复：quarantine_only 各阶段续跑;done 终态

### D｜拆分 split_commit（后端 + 前端,量最大）
1. 拆分专用聚类：`recluster_split`（关碎片吞并;无嵌入统一进无法判定桶;种子给建议去处）
2. 建议分组命令（解码+嵌入+聚类,spawn_blocking）
3. reserved 占号（NoteLock 内写空预留表项,带 split_op_id）;取消协议 cancel_requested→cancelled
4. 批量改派（NoteLock,显式 ID）;修订稿处理（全组同去向原位改,否则标 stale;先排空未保存编辑）
5. 受权回灌（split_op_id 校验）;released
6. 前端：段落表(每段下拉+试听)、组去处选择器、singleton 折叠
7. 恢复三态判别测试、占号并发测试、取消边界测试

## 交付口径

A+B+C 合起来兑现「打标即止血」（用户诉求 2 的全部 + 诉求 1 的基础）;D 兑现拆分（诉求 1 完整）。
每阶段完成即跑全量测试;全部完成后 Codex 实现 review(多轮至收敛),提 PR。
若 D 在实现中暴露超预期复杂度,A+B+C 可先行成 PR,D 独立成第二个 PR——届时明说,不静默缩水。
