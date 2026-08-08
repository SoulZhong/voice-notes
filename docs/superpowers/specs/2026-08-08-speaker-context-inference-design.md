# 说话人上下文推断增强 — 设计文档

日期:2026-08-08
状态:已与用户逐节确认,待实施计划

## 背景与问题

当前说话人识别是纯声学链路:CAM++ 声纹嵌入 → 在线聚类(S 号)→ 种子簇跨会认人 → 会后 AHC 重聚类(R 号)。文本内容对说话人判定零参与——refine 管线的 LLM 甚至看不到 speaker 标签。声纹能分清「说话人 1/2/3」,但不知道他们是谁;无名簇只能靠用户手动改名。

同时,库内已有大量未利用的上下文原料:转写文本中的自我介绍与称呼、知识图谱的 person 实体(带逐字证据区间)、历史同场记录;`docs/2026-08-02-speaker-recognition-accuracy-analysis.md` 点名的「用户纠错回灌质心」(文献相对 DER -32%)至今未实施;日历/参会人概念完全不存在。

## 目标

在声学链路之上叠加「上下文身份推断」层:

1. **给无名簇起真名**(主攻):用自我介绍、称呼、会议元数据推断簇的真实身份;
2. **跨会议认人更准**:推断确认的身份分级反哺声纹质心,老熟人下次直接命中;
3. **纠正声纹分错的段**(远期):用对话逻辑反过来质疑声学分段。

**非目标(本期不做)**:实时推断显名;Windows 日历;云端 ASR 厂商 diarization 接入;跨设备声纹同步。

## 总体架构

```
录音 → VAD → ASR → 声纹聚类(S1..Sn) → 会后 AHC 重聚类(R1..Rk)
                                              ↓
                              【identify 阶段】← 声纹库人名
                               LLM 推断簇→身份  ← 历史同场记录
                               + 逐字证据区间    ← 【日历当场事件+参会人】
                                              ↓
                    高置信 → 自动应用+回执(可撤销) → 分级反哺质心
                    中置信 → 整理收件箱建议卡 → 用户拍板 → 回灌
                    低置信 → 丢弃(仅留 ailog)
```

架构选型结论:**refine 管线内新增 identify 阶段为主干**(复用分块/重试/ailog/双 provider 基础设施),**MCP 工具为副产品**(同一份裁决逻辑暴露给外部 Agent);实时推断进远期路线图。

## 分期路线

每期独立可交付、可验证:

- **P1 — 地基**:① speaker 标签传入现有 refine LLM prompt(`refine/mod.rs` `run_local` 已持有 `speakers`,只差拼进 `format_chunk_paragraphs`);② 纠错回灌:`assign_note_speaker_person` 与建议卡确认后,把人工指认段的嵌入回灌质心。
- **P2 — 主体**:identify 阶段 + 置信度分级落地 + 收件箱建议卡 + MCP 工具 + 评测集。
- **P3 — 日历**:macOS EventKit 接入,时间窗匹配 + 参会人闭集先验;Windows 留 trait 接口。
- **P4 — 声学-文本互纠(远期)**:文本证据质疑声纹分段,生成「这段可能归错人」建议;实时推断在此重新评估。

## identify 阶段设计(P2)

### 管线位置

`filter → recluster → identify → llm → entities → relations`。

- 在 recluster 之后:R 簇是会后稳定单元;
- 在 llm 精修之前:精修 prompt 从此可带真名,指代消解顺带受益;
- `RefineStages` 增加独立 `identify` 状态位,失败不阻塞其余阶段,可单独重试。

### 输入打包

单次请求(不走 3000 字分块——身份推断需要全局视野),靠采样控制长度:

- 每个 R 簇:总时长、段数、信道(mic/system)、现有关联(人名或「无名」);
- 采样文本:每簇开场若干段 + 含人名/称呼的段(用图谱 person 实体证据区间预筛)+ 簇切换边界前后段,总量封顶;
- 声纹库候选:全量人名 + `last_seen` + 历史同场共现(`person_notes` 反查);
- mic 信道先验:mic 簇默认候选是「我」;
- P3 起:日历事件标题 + 参会人名单。

### 输出 schema

复用 `response_format: json_object`。每条 assignment:

```json
{
  "assignments": [
    {
      "cluster": "R2",
      "person": { "kind": "existing", "person_id": "P3" },
      "confidence": "high | medium | low",
      "evidence": [
        { "seq": 41, "quote": "我是张伟", "type": "self_intro" }
      ]
    }
  ]
}
```

`person.kind` 为 `existing`(库内已有人,给 person_id)或 `new`(新名字)。证据类型五档:

| type | 说明 | 强度 |
|---|---|---|
| `self_intro` | 自我介绍(「我是张伟」) | 铁证 |
| `addressed_reply` | 称呼 + 下一话轮应答(「小王你说说」→ 下一簇是王) | 强 |
| `third_person_exclusion` | 第三人称排除(簇内说「张伟他昨天…」→ 此簇不是张伟) | 排除 |
| `role_topic` | 角色/主题匹配 | 弱 |
| `calendar` | 日历参会人闭集 | 先验 |

### 程序侧裁决(不只信 LLM 自报)

防幻觉关键防线,四道:

1. **逐字校验**:每条 evidence 的 `quote` 必须真实出现在对应 seq 段文本中;不过则整条 assignment 降级丢弃;
2. **冲突检测**:两簇指向同一人、或与已有高置信关联矛盾 → 全部降为建议卡;
3. **声学否决权**:目标人在同信道已有质心时,若该簇声纹与其余弦低于软阈值(初始值复用 `SOFT_ASSIGN_THRESHOLD = 0.45`,后续由评测集调优),文本证据再硬也只出建议卡,不自动应用——声学与文本互为制衡;跨信道余弦不可比,此关不适用,自动应用要求同时满足其余三关;
4. **裁决分档**:high(自我介绍级证据 + 前三关全过)自动应用;medium 进收件箱建议卡;low 丢弃仅留 ailog。

### 落地与反哺

- 目标是库内已有人 → 等价 `assign_note_speaker_person`(簇关联 person_id);新名字 → 建新 Person;
- **高置信自动应用:关联 + 质心回灌同时生效**,沿 #66 merge_journal 净增量快照记回执(新类型 `identify_assign`),收件箱可一键撤销并完整回滚质心。取舍说明:相比「回执期满才回灌」少一套挂起队列,撤销能力等价(已确认);
- 建议卡被确认 → 同样回灌(顺带落地「纠错回灌」高杠杆项);被忽略 → 进拒绝名单,同簇同人不再重复建议;
- 每条自动应用回执带证据引文(「因为他说了:我是张伟」),用户一眼可判;
- 回灌受既有**模型门禁**约束:`Voiceprints.embedding_model` 与当前设置不一致时禁止回灌,只做显示层关联。

### MCP 副产品

新增工具 `identify_speakers(note_id, dry_run)`:返回 assignments + 证据;`dry_run=false` 时走同一套程序侧裁决与分级落地。外部 Agent 与管线共享一份逻辑。

## 日历集成设计(P3)

### 接入与权限

- Rust 侧经 `objc2-event-kit` 直调 EventKit,抽象为 `CalendarProvider` trait(macOS 实现 EventKit,Windows 留空实现待后补);
- macOS 14+ 日历完整访问:Info.plist 用途声明 + entitlement;EventKit 可读本机已登录的 iCloud/Google/Exchange 日程,无需自建 OAuth;
- 设置页新增「日历匹配」开关,**默认开**;
- **授权前置说明**:真正触发系统授权前,先弹应用内说明卡,讲清「为什么需要日历」(自动把录音关联到会议日程、用参会人名单帮助认出说话人)与「日程数据只在本机使用」,用户点「继续」才拉起系统弹窗;系统层被拒 → 开关自动回关,并给出去系统设置重开的指引;授权失败绝不阻塞录制。

### 匹配逻辑

- 录制停止时,以 `started_at/ended_at` 与当天日历事件求时间窗重叠,取重叠最长者;无重叠则不匹配;
- 匹配结果是**可修正的弱绑定**:笔记详情页头部显示事件标题 + 参会人,可手动改选其它事件或清除;
- 提供批量回填命令,给历史笔记补匹配。

### 落盘与供给

- `NoteMeta.calendar` 可选字段:`{ event_id, title, attendees: [{ name, email, is_me }] }`(向后兼容,旧笔记无感);`is_me` 用 EKParticipant 的 isCurrentUser 映射「我」;
- 供给 identify:标题并入主题上下文;参会人名单作**闭集先验**——prompt 明示「本场参会人大概率在此名单内」,但不设硬墙(临时加入/代参会存在);参会人名与声纹库人名模糊匹配,库内已有的人直接给 person_id 候选;email 落盘保留用于同名区分,不参与展示;
- 隐私边界:日历数据全本地;送入 LLM 的内容(标题 + 人名)与转写文本同级敏感度,跟随用户已选 refine provider,不引入新外发面。

## 数据结构与接口汇总

**数据结构**:

- `RefineStages` + `identify` 状态位;
- 推断产物落笔记目录 `identify.json`(assignments + 证据 + 裁决结果);
- merge_journal 新增回执类型 `identify_assign`,复用快照/撤销/失效钩子;
- `NoteMeta.calendar` 可选字段;
- `settings.calendar_match_enabled`(默认 true);
- 声纹库结构零改动(回灌走现有质心更新路径)。

**IPC 新命令**:`identify_note`、`apply_identify_suggestion`、`set_note_calendar_event`、`backfill_calendar_matches`。建议卡并入现有整理收件箱数据源;忽略复用 `dismiss_tidy_item` 拒绝名单。

**MCP 新工具**:`identify_speakers(note_id, dry_run)`。

## 错误处理

- LLM 请求失败 → identify 阶段标失败,其余阶段照跑,下次精修可重试;
- 逐字校验不过 → 该条静默丢弃,ailog 留痕;
- 日历无权限/EventKit 异常 → 跳过匹配,录制与精修完全不受影响;
- 所有 LLM 交互进现有 ailog 全量日志。

## 测试与评测

准确度分析文档将「自建评测集」列为所有优化的前置,本期一并落地:

- **单元测试**:逐字校验、冲突检测、声学否决、回灌撤销的质心回滚、日历时间窗匹配;
- **评测集**:从现有真实笔记挑若干场,人工标注「簇→真人」ground truth;评测脚本按置信档位算查准/查全;
- **验收线**:high 档查准率优先,目标接近 100%(错认比不认糟);medium 档看查全率;每次调 prompt/阈值都跑同一评测集防回归。

## 已确认的关键取舍记录

| 决策点 | 结论 |
|---|---|
| 核心目标 | 全都要,按阶段推进;主攻给无名簇起真名 |
| 落地策略 | 置信度分级:高自动应用+回执,中建议卡,低丢弃 |
| 推断引擎 | 复用现有双 provider(HTTP / Agent CLI),零新设置 |
| 信号源范围 | 日历纳入本期;先 macOS EventKit,Windows 留接口 |
| 声纹反哺 | 分级反哺:高置信自动应用与人工确认均回灌,快照可撤销 |
| 架构位置 | A 主干(refine 管线 identify 阶段)+ B 副产品(MCP 工具) |
| 日历开关 | 默认开;系统授权前先弹应用内说明卡 |
