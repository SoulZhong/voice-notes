# 一键拆分:混杂说话人的普通用户交互 + 「确认才入库」原则

日期:2026-08-22 · 状态:方向已获用户认可("可以")
前置:2026-08-20-mixed-speaker-split-design.md(机制层,本设计复用其全部阶段机)
     2026-08-21-one-speaker-set-design.md(一波说话人,胸牌即 S 本尊)

## 用户反馈(起因)

"「标记为多人混杂」整个交互过程太技术了,普通用户根本看不懂。"
现有流程把系统内政全摊给用户:隔离/声纹种子/会话质心/环形上限/加权稀释/
样本基线/回灌,还要勾「我已了解」、在表格里逐段选归属。

## 两条新原则(用户定)

1. **本篇标注宽,声纹库写入严**:用户试听一段后把整簇标成某人,只在本篇记录
   生效;声纹库只收用户**亲耳确认过的那一段**。
2. **库写入必须经用户确认**:系统猜的(声纹建议)只能当建议展示,不能自动入库。
   (范围:本轮只落在混杂簇拆分链路;正常自动注册 ≥10s 门槛与常规关联回灌不动。)

## 交互设计(甲式一键,机制全自动)

### 入口
胸牌菜单「标记为多人混杂」→ **「这不是一个人?」**。点击即执行,无面板、无勾选。

### 一键执行(后台串既有阶段机,全部取默认)
mark(隔离) → 样本自动清理(只删可归因到本篇被标簇的样本;来源未知的保留)
→ 残留默认「接受」(零损失、立即可用,偏差随后续录音稀释)
→ 声纹分组(suggest_split_groups)
→ commit:每组 → **新说话人**;判不准的段「保持不动」留在原说话人。

**分组结果只有一组(或全部判不准)时不硬拆**:自动 cancel(解除隔离恢复原状),
toast 如实说「听起来就是一个人,没有拆」——比硬拆出一个空壳新说话人诚实。
全程不出现任何术语;执行期间入口处显示「正在按声音分组…」。

### 结果
toast:「已拆成 {n} 个说话人,点胸牌试听确认;可撤销」。
- 有声纹线索的新说话人,胸牌带建议徽标:**「像是 张三?」**(SpeakerMeta.hint_person,
  仅建议,不冒充结论;关联/改名后清除)。
- **撤销**(undo_auto_split):段落归属原路搬回、空的新说话人删除、多人标记复位、
  原说话人的人物关联恢复(仅本篇表项,不触库)。可行性来源:自动流对库零写入
  (全组落新说话人 → 无回灌),撤销纯属笔记级。样本清理是唯一例外
  (可归因样本已删,不还原——它们本就是被污染的)。

### 认人(复用既有胸牌动作,新原则生效)
- 试听(现有)→ 选人关联(现有)。本篇:整组段落显示为该人。
- **库**:只把「刚试听过的那一段」音频存为该人物样本(append_confirmed_sample,
  绕过"老熟人不加样本"策略——用户确认过的样本永远可加;隔离/黑名单门禁照过),
  并以该段单独回灌质心。没试听就直接选人 → 本篇生效,库零写入。
- 拆分产物说话人(split_born 标记)**永久关闭整组批量回灌**(spawn_feedback 旁路):
  混杂簇正是批量喂库的污染源。

### 保留的旧部件
- MultiSpeakerPanel 保留,仅用于:恢复历史中断 op;(将来)高级入口。默认路径不再进入。
- 阶段机/门禁/黑名单/回执 全部复用,不改语义——变的只是"谁来按按钮"。

## 实现面

后端:
- `auto_split_speaker(note_id, speaker_id) -> AutoSplitOut{op_id, groups:[{speaker_id,
  count, dur_ms, hint:{person_id,name,sim}?}], kept}`:串 mark → confirm(空删单,
  confirm_seen=true) → residual("accept", then_split) → suggest → commit(全部
  new_speaker,undetermined=keep) → 读回 plan_groups 映射新号 → 写 hint/split_born。
- `undo_auto_split(op_id)`:op=done 且段落仍在拆分去向时,反向 batch_set_segment_speaker
  → 删空新说话人 → 复位 multi 标记 → 恢复原关联 → 修订稿同步(反向 moved)。
- SpeakerMeta 新字段(serde default/skip):`split_born: bool`、`hint_person: Option<String>`。
- `append_confirmed_sample`(voiceprints.rs):同 append_session_sample 临界区,
  免"老熟人"跳过;`assign_note_speaker_person` 增可选 `audited_seq`,split_born
  说话人跳过 spawn_feedback,audited_seq 存样本+单段回灌。
- 撤销所需快照:op 增记原说话人 person 关联(marked 时已知,进 SplitOp 新字段)。

前端:
- 菜单项文案改;点击 → 进度态 → toast(含撤销按钮;不设时限,撤销有效性由后端
  按「段落仍在拆分去向」判定);
- 胸牌徽标「像是 {name}?」(people 列表解析,解析不到不显示);
- 每说话人记录「最近试听过的段 seq」,onPick 时带上。

## 测试
- 编排:未关联/已关联说话人各一条全链路(隔离→默认清理→accept→新号→done);
- 撤销:归属还原、空号删除、multi 复位、关联恢复、二次撤销幂等拒;
- append_confirmed_sample:老熟人可加、隔离中拒、黑名单哈希拒;
- assign+audited_seq:split_born 不批量回灌、单段样本落库;无 audited_seq 零库写;
- hint:关联/改名清除。
