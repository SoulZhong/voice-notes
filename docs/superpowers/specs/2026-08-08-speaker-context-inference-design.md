# 说话人上下文推断增强 — 设计文档

日期:2026-08-08(rev2,消化 Codex 审查 18 P1 + 10 P2 后修订)
状态:已与用户逐节确认;经 Codex 交叉审查后重写关键机制;待用户复审

## 背景与问题

当前说话人识别是纯声学链路:CAM++ 声纹嵌入 → 在线聚类(S 号)→ 种子簇跨会认人 → 会后 AHC 重聚类(R 号)。文本内容对说话人判定零参与。声纹能分清「说话人 1/2/3」,但不知道他们是谁;无名簇只能靠用户手动改名。

库内已有未利用的上下文原料:转写文本中的自我介绍与称呼、历史同场记录;`docs/2026-08-02-speaker-recognition-accuracy-analysis.md` 点名的「用户纠错回灌质心」至今未实施;日历/参会人概念完全不存在。

## 目标

1. **给无名簇起真名**(主攻):用自我介绍、称呼、会议元数据推断簇的真实身份;
2. **跨会议认人更准**:确认的身份分级反哺声纹质心;
3. **纠正声纹分错的段**(远期):用对话逻辑反过来质疑声学分段。

**非目标(本期不做)**:实时推断显名;Windows 日历;云端 ASR 厂商 diarization 接入;跨设备声纹同步;merge_journal 通用事务日志化。

## 总体架构

```
录音 → VAD → ASR → 声纹聚类(S1..Sn) → 会后 AHC 重聚类(R1..Rk,导出簇级统计)
                                              ↓
                              【identify 阶段】← 候选召回 Top-K(声纹库/日历/历史共现)
                               LLM 推断簇→身份  ← 轻量人名预筛(非图谱依赖)
                               + 逐字证据区间    ← 【日历当场事件+参会人】
                                              ↓
                         程序侧四道裁决 → identify.json(带簇指纹版本绑定)
                                              ↓
              【P2a dry-run 期】全部只出建议卡/报告,积累评测数据
              【P2b 达标后】高置信自动应用+回执(可撤销)→ 分级反哺质心
                            中置信 → 收件箱建议卡 → 用户拍板 → 回灌
                            低置信 → 丢弃(仅留 ailog)
```

架构选型结论:**refine 管线内新增 identify 阶段为主干**;**MCP 工具为副产品且仅只读(dry-run)**;实时推断进远期路线图。

## 分期路线

每期独立可交付、可验证。**评测先行是硬门:自动写入(自动应用+自动回灌)只在 P2b、且评测数据达标后开启。**

- **P1 — 地基**:
  1. speaker 标签进入精修上下文,**双路径都要做**:HTTP 路径扩展 `format_chunk_paragraphs` 调用链与 prompt 契约(函数签名从 `(index, text)` 扩展,含测试与契约更新,非一行改动);Agent 路径经 `get_note` 本已可见 speaker/name,在指令模板中明确要求利用,消除双 provider 行为分叉;
  2. **纠错回灌模块(新建,无现成路径)**:`assign_note_speaker_person` 与 `assign_refined_person` 目前只改关联、不碰质心。新建 `feedback` 模块:从笔记音频按段重嵌入(复用 retranscribe 的音频读取+嵌入路径),把人工指认段的嵌入并入目标人质心,受模型门禁约束;
  3. **评测集与标注工具**:从真实笔记标注「簇→真人」ground truth 的最小工具(可以是 MCP/脚本驱动),评测脚本按档位算查准/查全。这是 P2b 开闸的前置。
- **P2a — identify 只读期**:identify 阶段落地,但**一切结论只出建议卡或 dry-run 报告,零自动写入**;收件箱建议卡上线(人工拍板后才落地+回灌——人工确认即真值,回灌安全);期间每场推断结果自动积累为评测样本。
- **P2b — 自动应用期**:评测数据达标后(初始验收线:累计 ≥20 场标注、high 档样本 ≥50 条且误认 ≤1%,阈值随数据调整),开启 high 档自动应用+回灌。
- **P3 — 日历**:macOS EventKit 接入(兼容 macOS 13),时间窗匹配 + 参会人闭集先验;Windows 留 trait 接口。P3 可与 P2a 并行开发,产出先喂 dry-run。
- **P4 — 声学-文本互纠(远期)**:文本证据质疑声纹分段;实时推断在此重新评估。

## identify 阶段设计(P2)

### 管线位置与调度

`filter → recluster → identify → llm → entities → relations`(逻辑顺序;HTTP 路径的 llm/entities/relations 实际是同一次分块调用,identify 是**独立于它的另一次调用**,不混入分块)。

- 在 recluster 之后:R 簇是会后稳定单元;**recluster 需扩展导出簇级统计(质心、段数、总时长、信道构成)**——当前只返回逐段 assignment,`embed_all` 结果是私有临时值,此为新增工作项;
- 在 llm 精修之前:精修 prompt 从此可带真名;
- `RefineStages` 增加 `identify` 状态位;**单独重试的调度入口就是 IPC `identify_note`**(不重跑整个 refine);`run_local` 重跑导致重聚类时,旧 identify 结果按簇指纹迁移,迁移失败则标失效待重跑(见「版本绑定」)。
- 增改 `NoteMeta`/`RefineStages` 字段用 serde default 保持向后兼容;仓库内数十处结构体字面量的编译修复列入实施计划工作量,不视为"小改动"。

### 执行器(双 provider,两套实现)

「复用双 provider」指复用**调度、配置、探测、ailog、超时/重试**基础设施;执行逻辑需要两套新实现,仿照 `RelationExecutor` 模式新建 `IdentifyExecutor` trait:

- HTTP 实现:单次请求 + `response_format: json_object`;
- Agent 实现:扩展 Agent 沙箱 MCP 白名单加入 identify 所需只读工具,更新 Claude/Codex/Gemini 三家的命令构造与 prompt 模板;**Agent 只产出建议(等价 dry-run),一切写入统一回到 Rust 侧裁决层执行**,不扩大 Agent 写面。

### 输入打包与候选召回

单次请求,靠采样与召回控制长度:

- **候选召回 Top-K(约 30,可调)**,不送全量人名:按 ① 日历参会人 ② 历史同场共现 ③ 声学近邻(库内质心与各簇余弦 top)三路召回取并集;共现统计需同时扫原始 `speakers.json` 关联与 refined 稿关联(`person_notes` 现状只看前者,需补);
- **采样预筛不依赖知识图谱**(图谱实体在 identify 之后的阶段才生成,首轮无米;旧稿实体可能对应旧文本):改用轻量检测——候选人名/日历参会人名的字符串命中 + 称呼/自报句式正则(「我是/我叫/这边是」「X 你说/X 总/X 老师」等),命中段 + 每簇开场段 + 簇边界前后段,总量封顶;
- 每簇附:总时长、段数、信道构成、现有关联;
- mic 信道先验:mic 主导的簇默认候选是「我」;
- P3 起:日历事件标题 + 参会人名单。

### 输出 schema 与证据模型

每条 assignment:`cluster` + `person`(`existing` 给库 person_id;`new` 给名字字符串——LLM 绝不引用图谱 `ent_N`,图谱实体只是输入线索,与库 person 无映射关系)+ 自报置信度 + 证据列表。

证据条目**锚定 identify 输入所用的文本版本**:`{ paragraph_index, char_start, char_end, quote, type }`,配合本次输入的 revision hash,消除「R 段落拼接多 seq、quote 跨段/重复/只存在于精修稿」的歧义。证据类型:

| type | 说明 | 强度 |
|---|---|---|
| `self_intro` | 自我介绍(「我是张伟」) | 强(非铁证:可能是引用/复述/ASR 错字) |
| `addressed_reply` | 称呼 + 下一话轮应答 | 中(下一话轮可能是插话/重叠) |
| `third_person_exclusion` | 第三人称排除 | 排除 |
| `role_topic` | 角色/主题匹配 | 弱 |
| `calendar` | 日历参会人闭集 | 先验 |

### 程序侧裁决(不只信 LLM 自报)

1. **区间+逐字校验**:按 `paragraph_index + char_start/char_end` 在对应版本文本上取子串与 `quote` 比对;不过则整条 assignment 降级丢弃;
2. **冲突检测**:两簇指向同一人、或与已有高置信关联矛盾 → 全部降为建议卡;
3. **声学门(取代原「0.45 否决」设计)**:自动应用要求声学**正向确认**——簇主导信道与目标人同信道质心裸余弦 ≥ `SEED_ASSIGN_THRESHOLD`(0.68)或 AS-Norm z 通道通过(复用种子认人三闸口径);簇为跨信道混合、目标人无同信道质心、或未达标 → 最高只到建议卡。原 0.45 软阈值是在线聚类软归属下限,不是身份验证阈值,弃用;
4. **裁决分档**:high 要求「self_intro 级证据 + 前三关全过」;由于 self_intro 并非铁证,**high 档自动应用只在 P2b 且评测达标后启用**;P2a 期间 high 与 medium 都只出建议卡(high 排前并标注);low 丢弃仅留 ailog。

### 落地、原子性与回执

- 目标是库内已有人 → 簇关联 person_id;新名字 → **新增 `create_person_from_cluster(name, note_id, cluster)` API**(现库只能经 `upsert_from_session` 自动建无名人):同名不静默合并——建同名新人并在收件箱提示疑似重复(复用重名拦截思路),ID 分配在 VP_LOCK 内完成;
- **回执/撤销走新建的 `identify_journal`**(借鉴 merge_journal 的快照模式,不复用其结构——现有条目硬编码 loser/winner/redirect/样本目录,非通用事务日志):每条回执记录簇指纹、关联前状态、回灌净增量(质心/计数/时长/last_seen 各真值源的前值),支持完整撤销;
- **提交顺序与崩溃恢复**(关联与回灌跨 `aing.json`/`speakers.json`/`voiceprints.json` 三个真值源,锁彼此独立):固定顺序 ① 先写 identify_journal 意向条目(intent)→ ② 写关联 → ③ 写质心 → ④ 标记 intent 完成;锁顺序固定为 note → refined → VP_LOCK;启动时扫描未完成 intent,按记录的前值补偿回滚;
- 建议卡确认 → 同一条落地路径(含回灌);忽略 → 拒绝记录写入 identify_journal(带簇指纹),**不用 `dismiss_tidy_item`**(其为前端字符串 best-effort 列表、容量 500、损坏归空,不可作身份真值);
- 每条自动应用回执带证据引文,用户一眼可判;
- 回灌受模型门禁约束:`Voiceprints.embedding_model` 与当前设置不一致时禁回灌,只做显示层关联。

### 版本绑定(簇指纹)

R 号在重跑 `run_local` 后会重新编号;重转写、过滤变化、用户编辑会改变段落。因此:

- `identify.json` 记录 `revision`:输入文本版本 hash + 每簇指纹(簇成员原始 seq 集合的 hash);
- assignments、回执、拒绝记录全部绑定**簇指纹**而非 R 号;重聚类后按指纹迁移(成员集合 Jaccard ≥ 阈值视为同簇),迁移失败标失效;
- 杜绝旧结论静默作用到重编号后的另一个人。

### MCP 副产品(仅只读)

新增工具 `identify_speakers(note_id)`:**只做 dry-run**,返回 assignments + 证据 + 裁决结果,不提供写开关。理由:独立 MCP 进程不掌握 GUI 录制/重转写/refine 状态,直接写入会绕过命令层守卫,且需另建幂等/并发边界,收益不抵风险。外部 Agent 想落地结论,引导用户走 GUI 收件箱,或后续再评估带幂等键的写接口。

## 日历集成设计(P3)

### 接入与权限

- Rust 侧经 `objc2-event-kit` 直调 EventKit,抽象为 `CalendarProvider` trait(macOS 实现,Windows 留空);
- **兼容 macOS 13**(应用最低支持 13):14+ 用 `requestFullAccessToEvents`,13 回退 `requestAccess(to:)`;Info.plist 同时声明 `NSCalendarsUsageDescription`(13)与 `NSCalendarsFullAccessUsageDescription`(14+),entitlement 加 `com.apple.security.personal-information.calendars`;`scripts/check_macos_entitlements.py` 校验清单同步扩展,拒权路径纳入自动化测试;
- 设置页「日历匹配」开关,**默认开**;真正触发系统授权前先弹应用内说明卡(为什么需要日历:自动关联会议日程、参会人帮助认人;日程数据不离本机语境见下),点「继续」才拉起系统弹窗;系统层被拒 → 开关回关 + 指引去系统设置;授权失败绝不阻塞录制。

### 匹配逻辑与边界

- 时间窗:`started_at` 到 `ended_at`(`ended_at` 为空——录制中/崩溃残留——用最后一段时间戳兜底);与当天事件求重叠,**排除全天事件**;取重叠比例最高者;并列平手不自动绑定,列候选待用户选;跨午夜按实际区间;时区取系统本地(EventKit 返回绝对时间,DST 无需特判);
- 匹配是**可修正的弱绑定**:详情页头部显示事件标题+参会人,可改选/清除;
- **落盘即快照**:`NoteMeta.calendar` 存 `{ event_id, title, attendees: [{name, email, is_me}], matched_at }`;title/attendees 是当时快照,**不依赖 event_id 活性**——事件被改/删/重复拆分后快照仍然自洽,event_id 仅用于用户改选时重新定位,失效则提示「原事件已变更」;
- 批量回填命令给历史笔记补匹配(同样落快照)。

### 参会人与声纹库对接

- `Person` 增加可选 `emails: Vec<String>`(serde default,schema_version 递增;此为声纹库唯一结构改动):用户确认「参会人 X = 库内 P3」时把 email 记入,下一场直接精确匹配,不再每场模糊猜;无 email 记录时按名字模糊匹配;
- 参会人名单作**闭集先验**,不设硬墙(临时加入/代参会存在);`is_me` 映射「我」。

### 隐私边界(如实表述)

- 日历数据**不新增外发面**:标题+人名会作为 identify 输入发给用户已选的 refine provider,与转写文本同级、同通道;
- 完整请求进 ailog(现状即全量、不脱敏),日历字段随之入日志——与转写在 ailog 中的处理一致;
- 删除笔记时连带删除其 calendar 快照;导出/备份包含 calendar 字段,文档中向用户说明。

## 数据结构与接口汇总

**数据结构**:`RefineStages.identify` 状态位;`identify.json`(assignments+证据+裁决+revision/簇指纹);新建 `identify_journal`(意向/回执/拒绝,含撤销前值);`NoteMeta.calendar` 可选字段;`Person.emails` 可选字段(schema_version 递增);`settings.calendar_match_enabled`(默认 true)。凡增字段均 serde default 向后兼容;结构体字面量修复范围计入实施计划。

**IPC 新命令**:`identify_note`(触发/重试)、`apply_identify_suggestion`、`undo_identify_assign`、`set_note_calendar_event`、`backfill_calendar_matches`、`create_person_from_cluster`(内部)、评测标注命令(P1)。建议卡并入整理收件箱数据源(拒绝走 identify_journal)。

**MCP 新工具**:`identify_speakers(note_id)`(仅 dry-run)。

## 错误处理

- LLM 请求失败 → identify 阶段标失败,其余照跑,`identify_note` 单独重试;
- 校验不过 → 该条静默丢弃,ailog 留痕;
- 落地中途失败 → 按 identify_journal intent 于启动时补偿回滚;
- 日历无权限/EventKit 异常 → 跳过匹配,录制与精修不受影响;
- 所有 LLM 交互进 ailog。

## 测试与评测

- **单元测试**:区间+逐字校验、冲突检测、声学门(0.68/AS-Norm 口径)、intent 补偿回滚、簇指纹迁移、日历时间窗边界(空 ended_at/全天/平手/跨午夜)、macOS 13/14 授权分支、拒权路径;
- **评测集(P1 建,P2b 的门)**:真实笔记人工标注「簇→真人」;评测脚本按档位算查准/查全;P2a 每场 dry-run 结果自动积累样本;
- **P2b 开闸验收线(初始)**:累计 ≥20 场标注、high 档样本 ≥50 条且误认 ≤1%;数据不足前自动应用保持关闭;此后每次调 prompt/阈值跑同一评测集防回归。

## 已确认的关键取舍记录

| 决策点 | 结论 |
|---|---|
| 核心目标 | 全都要,按阶段推进;主攻给无名簇起真名 |
| 落地策略 | 置信度分级;**自动应用推迟到 P2b 评测达标后**(Codex 审查后收紧) |
| 推断引擎 | 复用双 provider 的调度/日志底座;执行器两套新实现 |
| 信号源范围 | 日历纳入本期;先 macOS EventKit(兼容 13),Windows 留接口 |
| 声纹反哺 | 分级反哺:人工确认即回灌;自动回灌仅 P2b;identify_journal 可撤销 |
| 架构位置 | A 主干(refine 管线 identify 阶段)+ B 副产品(MCP 仅 dry-run) |
| 日历开关 | 默认开;系统授权前先弹应用内说明卡 |
| 声学门口径 | 弃 0.45 否决,改 0.68/AS-Norm 正向确认(种子认人同口径) |

## 修订记录

- rev2(2026-08-08):经 Codex 交叉审查(18 P1 + 10 P2)重写:评测先行拆 P2a/P2b;采样预筛去图谱依赖改轻量人名检测;声学门改 0.68/AS-Norm;回执改新建 identify_journal + intent 补偿;证据模型改区间锚定+版本/簇指纹绑定;MCP 收为仅 dry-run;新增 create_person_from_cluster、Person.emails、候选 Top-K 召回;日历边界/13 兼容/快照化;纠错回灌与 AHC 簇统计如实列为新建工作项;隐私表述修正。
- rev1(2026-08-08):初版,与用户逐节确认。
