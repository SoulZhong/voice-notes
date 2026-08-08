# 说话人上下文推断 P2b(自动应用,暗启动)Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:executing-plans。

> **rev2 状态:设计已定稿,实现推迟到评测数据临门。** Codex 审查(21 P1 + 7 P2)击穿了 rev1「不建 intent journal」的论证,修正后的必做设计如下;本轮只实现 Task 6(重新推断入口),其余任务待用户标注数据接近门槛时另开会话执行(评测数据还将反哺 AS-Norm 口径)。

## rev2 修正设计(Codex 审查结论,实现时必须遵守)

1. **必须建 auto_pending 意向日志**(spec identify_journal 的最小形态):每次自动应用先持久化 `{operation_id, fingerprint, cluster(当时 R 号), seqs, target_person, before(逐段 person_id/name 快照), stage: pending|assigned|reinforced|done}`,再推进各步,启动/重跑时恢复未完成操作。三个"幂等机制"不能替代它:回灌是先改库后写账本(崩溃重复累计);assign 会推进 revision(非幂等);崩溃后合并规则会吞掉回执让撤销入口消失。
2. **自动路径回灌必须同步**(不走 spawn_feedback 异步)——否则"assign→status→用户撤销→后台回灌污染"。且不得复用 `do_assign_refined_person`(其 `is_refining` 守卫在管线内必挡);需要绕守卫的内部变体,自动应用发生在 Aing 线程内本就无并发编辑。
3. **自动前置条件在锁内验证**:该簇全部段 `person_id == None 且 name 为空`(有用户手填 name 的簇绝不自动动——unassign 无法恢复 name);目标是库内已有人;tier High。这同时保证撤销只需覆盖 Reinforce 路径(prior 恒 None,不触 merge_journaled 不可逆分支)。
4. **撤销 = CAS + 状态机**:仅当当前关联仍等于自动目标才解除(用户已手改则拒绝);质心还原经 operation_id 对账(ledger 需记 op id,scope 键会被后续人工覆盖);撤销自身各步带状态位(undo_pending/restored/done),bool 推断不可靠;`restore_feedback` 被后续写挡下时持久化 non_revertible + 原因展示。
5. **回执永续可见**:回执渲染自 auto_pending/receipt 记录(含原 speaker/seqs/target),不依赖当前指纹可定位;重聚类后定位不到显示冲突态而非消失;不与建议共享 50 条淘汰上限。
6. **`run_identify` 合并规则保留 `auto_applied`**(现只保留 applied);自动循环后禁止再保存旧 idoc 副本;identify.json 读改写全部收进 IDENTIFY_ACT_GATE(含 run_identify/save 路径)。
7. **AS-Norm 必须与 registry 同口径**:对称 z(probe/seed 两侧)+ `SEED_ASSIGN_RAW_FLOOR=0.50` 裸分下限,不允许低余弦仅凭单侧 z 通过;`SNORM_MIN_COHORT` 等常量改 pub(crate) 共享,不写第二套相似算法;cohort 经 redirects 去重 + 维度/NaN 检查。
8. **Tier 语义不受开关污染**:自动资格另算 `auto_eligible` 字段,不改 adjudicate 的 High 定义(默认关时建议卡与评测行为必须与 P2a 完全一致);`acoustic` 字段形状不破坏兼容——旧二元组保留,另加 `acoustic_z: Option<f32>`(identify.json 不是纯可再生缓存:rejected/applied 决策在里面)。
9. 其它:`mark_decided` 用枚举限定转移(suggested→auto_applied→applied|rejected);acknowledge/undo 命令加录制中/Aing 中守卫;`decided_at` 前后端都是可空;`identify_note` 的 FEEDBACK_GATE 与同步回灌的自锁要拆锁序;ledger person 比较先经 redirect 归一。

**Goal:** 造好 P2b「high 档自动应用 + 回执可撤销」的全部机器,**默认关闭**(`identify_auto_apply = false`):spec 的评测数据门(≥20 场标注、high 档 ≥50 样本误认 ≤1%)约束的是**开启**,不是**建造**;用户标注达标后在设置页拨开关即生效。同时补上 P2a 冒烟清单的已知缺口:详情页手动「重新推断身份」入口。

**Architecture:** 自动应用挂在 `spawn_refine` 的 identify 成功之后:遍历新产出的 `status=="suggested"` 且 `tier==High` 且目标为**库内已有人**(new_name 永不自动建档)的 assignment,走与 `apply_identify_suggestion` 同一内部路径(`do_assign_refined_person` + P1 回灌),status 置 `"auto_applied"`;收件箱把 auto_applied 且未确认的条目渲染成**回执卡**(带证据引文),「好」确认、「撤销」回滚。撤销 = ① store 层新函数解除 R 簇关联;② P1 feedback 账本按 scope 条件还原质心(已有 before/after 快照,新增公开撤销入口);③ status → rejected + 拒绝键(同目标不再建议)。**不建 intent journal**:崩溃补偿由三处既有机制拼成——assign 本身幂等;回灌有账本快照且幂等;status 未落盘时,下轮 `run_identify` 的「建议目标与既有关联相同则不再生成」规则自动吞掉重复,收件箱最多短暂少一张回执卡,无数据损坏路径。AS-Norm 声学门升级为独立任务(离线 cohort z-score,High 档在开关开启时额外要求 z 达标)。

**Spec:** spec rev2「落地与反哺」「P2b」;**基线:** 分支 `feat/speaker-context-p2b` 叠在 `feat/speaker-context-p3`(PR #81)上。

## Global Constraints

- `identify_auto_apply` 默认 **false**(`#[serde(default)]` 裸默认即 false);设置页开关文案必须写明开启门槛(先用 `speaker_eval` 标注评测,high 档 ≥50 样本、误认 ≤1%),并给出模板文件路径提示。
- 自动应用**仅限**:tier==High、目标是库内已有人(resolve 通过)、簇当前无关联、非 new_name。任何一条不满足留在建议卡。
- 自动应用与人工确认共用同一内部函数(行为零分叉);写序固定:assign(锁内)→ 回灌(账本快照)→ status 落盘;每步幂等,崩溃无补偿债。
- 撤销必须完整:关联解除 + 质心还原(账本快照未被后续写覆盖才还原,否则留污染并明示)+ 拒绝键落盘。
- 收件箱回执卡必须带证据引文(「因为 TA 说:『……』」)——用户一眼可判对错。
- i18n 双语;新 IPC 过 generate_handler 源码解析测试;不跑全量 cargo fmt。

---

### Task 1: settings + 设置页开关(默认关)

- [ ] `settings.rs`:`#[serde(default)] pub identify_auto_apply: bool,` + Default 补 `false` + 旧文件缺键→false 单测。
- [ ] 设置页(AI 区块,refine 开关附近):开关行,desc 写明数据门与模板路径(`~/Documents/voice-notes/speaker_truth.jsonl` 按 data_dir 动态拼不必——文案给通用说明即可);i18n `settings.identifyAuto.{label,desc}` zh/en。
- [ ] 提交。

### Task 2: store 解除关联 + feedback 公开撤销入口

- [ ] `store/refined.rs`:`pub fn unassign_refined_person(note_dir, speaker_id) -> anyhow::Result<()>`——与 `assign_refined_person` 同锁纪律,把该 R 号段落的 `person_id`/`name` 清空。单测:赋后解除、revision 推进。
- [ ] `feedback.rs`:`pub fn undo_reinforce_for_scope(note_dir, seqs: &BTreeSet<u64>, expect_person: &str, vp) -> anyhow::Result<bool>`——按 scope 指纹查账本,条目存在且 person 匹配时走 `restore_feedback`(未被动过才还原),成功后删账本条;返回是否还原。单测:还原成功/被动过拒绝/无条目 false。
- [ ] 提交。

### Task 3: 自动应用内核 + spawn_refine 挂钩

- [ ] lib.rs 抽 `fn apply_identify_internal(app, note_id, fingerprint, mark_status: &str) -> Result<(), String>`:现 `apply_identify_suggestion` 的主体参数化(status 写 `"applied"` 或 `"auto_applied"`;IDENTIFY_ACT_GATE 内),命令壳与自动路径共用。
- [ ] `spawn_refine` identify 成功后(`save_identify` 之后、`emit("identify_done")` 之前):

```rust
if s.identify_auto_apply {
    for a in idoc.assignments.iter().filter(|a| {
        a.status == "suggested" && a.tier == refine::identify::Tier::High && a.person_id.is_some()
    }) {
        if let Err(e) = apply_identify_internal(&app, &note_id, &a.fingerprint, "auto_applied") {
            eprintln!("identify({note_id}): 自动应用 {} 失败(留建议卡): {e}", a.cluster);
        }
    }
}
```

  (apply_identify_internal 内部会重读 identify.json 并复核指纹/关联,竞态自防;失败条目保持 suggested,收件箱照常出卡。)
- [ ] `identify_note` 手动路径同样在开关开启时执行同段逻辑(行为一致)。
- [ ] 单测:`apply_identify_internal` 的 status 参数化(mark_applied 泛化为 `mark_decided(doc, fp, status, now)`,applied/auto_applied 两态单测);自动筛选条件纯函数 `auto_apply_targets(&idoc) -> Vec<&IdentifyAssignment>` 单测(High+库内人+suggested 才入选,new_name/Medium 不入)。
- [ ] 提交。

### Task 4: 撤销/确认 IPC + 收件箱回执卡

- [ ] `list_identify_suggestions` 扩展:也返回 `status=="auto_applied"` 且未确认的条目(`IdentifySuggestion` 加 `pub status: String`;新鲜度检查对 auto_applied 放宽——回执是历史事实,稿变了仍要能撤销,只按指纹仍可定位簇为条件)。`decided_at` 一并返回(回执时间)。
- [ ] 新命令:
  - `acknowledge_identify(note_id, fingerprint)`:auto_applied → applied(确认,回执消失);
  - `undo_identify_apply(note_id, fingerprint)`:IDENTIFY_ACT_GATE 内 ① `unassign_refined_person` ② `feedback::undo_reinforce_for_scope`(结果计入返回信息,还原失败=已被后续写动过,前端提示「关联已解除;声纹已并入后续数据,未回滚」)③ status → rejected + 拒绝键。返回 `bool`(质心是否还原)。
- [ ] 前端:`TidyItem` identify 支按 `status` 分流:suggested → 现有建议卡;auto_applied → 回执形态卡(标题「已自动认出:{cluster} = {name}」+ 证据引文 + 「好」/「撤销」);`tidyQueue` 排序:auto_applied 回执与 merge 回执同段(最前);i18n 补 `speakers.identifyAuto*` 键 zh/en;actions 经 `act()`。
- [ ] 测试:tidyQueue identify 双形态排序与 key;后端 mark 状态机单测(auto_applied→applied、auto_applied→rejected)。
- [ ] 提交。

### Task 5: AS-Norm 离线声学门(High 档增强)

- [ ] `identify.rs`:`fn asnorm_z(target_cos: f32, cohort: &[f32]) -> Option<f32>`(cohort=库内**其它**有名人物同信道主质心与簇质心的余弦集;均值/方差,样本 <SNORM_MIN_COHORT(3,与 registry 同值)返回 None);裁决声学门在 cohort 足够时改判:`cos >= 0.68 || z >= 3.0`(z 阈与 registry `SEED_ASSIGN_Z` 同值)不再是纯裸余弦——**且 High 档在 `acoustic` Some 的基础上要求 z 可算时 z≥3**(cohort 不足时维持裸余弦口径,不惩罚小库用户)。`Verdict.acoustic` 扩为 `(source, cos, Option<z>)`(落盘结构同步,serde 兼容:三元组改结构体 `AcousticCheck { source, cosine, z: Option<f32> }`,identify.json schema_version 升 2,读 v1 的二元组兼容——用 untagged enum 或手写 Deserialize;**简化:直接结构体+`#[serde(default)] z`,旧二元组数据丢弃重跑**(identify.json 是可再生缓存,不迁移)。
- [ ] 单测:cohort 充足时 z 判定、不足时回退裸余弦、High 档 z 门。
- [ ] 提交。

### Task 6: 详情页「重新推断身份」入口(P2a 缺口)

- [ ] `notes.ts`:`identifyNote(id)` 绑定;详情页头部(日历行同区)加小按钮「重新推断身份」(仅 `refine` 可用用户显示?后端 Err 即 toast,简单起见始终显示,失败提示原因);运行中置 busy;`identify_done` 事件已驱动收件箱刷新。i18n `notes.identify.rerun` zh/en。
- [ ] 提交。

## 收尾核对

- [ ] `cargo test --lib --bins` + mcp_stdio + `npm run check` + `npm test` 全绿
- [ ] 真机冒烟(PR 描述):① 开关默认关,identify 行为与 P2a 完全一致;② 开启后 Aing 一场含自我介绍笔记 → 收件箱出「已自动认出」回执卡带引文,R 段落已显真名;③ 「撤销」→ 关联解除、voiceprints total_ms 回落、同目标不再建议;④ 「好」→ 回执消失;⑤ 手动「重新推断身份」按钮工作;⑥ AS-Norm:库内人 ≥4 时 ailog/identify.json 出现 z 值
- [ ] PR 注明:默认关 + 开启门槛;不建 intent journal 的补偿论证(assign 幂等/回灌账本幂等/合并规则吞重复)
