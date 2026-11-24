# 应用内大模型调用全景与优化清单

日期:2026-11-24 · 关联任务:AxiomOS #848 · 代码基线:master `952502e`(v1.0.3)

这份文档回答三个问题:应用在哪些地方调用了大模型、每一处现在是怎么做的、实测效果如何以及该先优化什么。

数据来源:

- 代码:`src-tauri/src/refine/`(全部提示词集中在 `refine/prompts.rs`)、`src-tauri/src/lib.rs` 的 `spawn_refine`。
- 实测:本机 `<数据目录>/ai_logs/` 的 3120 条调用日志(2026-07 至 2026-11),以及 338 篇笔记盘上的 `aing.json` / `identify.json`。统计脚本见文末。

范围说明:本文只覆盖**文本大模型**调用。语音识别(本地 sherpa-onnx 各引擎、Apple 原生、`asr/cloud/` 下的火山与阿里云云端识别)不在此列,见 `docs/asr-pipeline.md`。

---

## 1. 一页总览

应用里的大模型调用全部发生在「Aing」(会后 AI 整理)这条线上,共 **5 类业务调用 + 2 类连通性测试**。

| # | 调用 | 做什么 | 每篇笔记调几次 | 日志 kind | 实测成功率(全部 / 11 月) |
|---|---|---|---|---|---|
| 1 | 分块精修 | 改错字、统一写法、删口头语、排版;顺带抽实体 | 每 3000 字一次(中位 4 次,最多 47 次) | `refine_chunk` | 62% / 73% |
| 2 | 关系抽取 | 从定稿正文里抽「谁负责什么」这类关系 | 1 次 | `refine_relations` | 12% / 9%,**且入库 0 条** |
| 3 | 说话人身份推断 | 从对话内容推断「R2 是张伟」 | 1 次 | `identify` | 调用 90% 成功,**但产出的建议几乎全被校验丢弃** |
| 4 | 起标题 | 给笔记起一个 12 字以内的标题 | 0 或 1 次 | `title` | 93% / 93% |
| 5 | Agent 整篇 Aing | 用本机 Agent CLI 代替 1+2,经 MCP 读写 | 1 次 | `agent_refine` + `mcp_apply` | 53%(仅 7 月用过) |
| 6 | HTTP 测试连接 | 设置页按钮 | 手动 | 不记日志 | — |
| 7 | Agent 测试运行 | 设置页按钮 | 手动 | 不记日志 | — |

最要紧的三个结论(详见第 5、6 节):

1. **关系抽取事实上从未成功过**:172 次调用,150 次失败;22 次「调用成功」里有内容的那几次也在后续校验中整批丢弃。338 篇笔记里关系为 0 条。
2. **身份推断在白花钱**:268 次成功调用花了 135 万输入 token,只有 42 次给出了推断(共 57 条),其中至少 30 条因「引文位置对不上」被丢弃;226 份 `identify.json` 里总共只剩 1 条建议。
3. **2 和 3 是同一个病根**:提示词要求模型给出引文在段落里的**字符起止位置**并且逐字符相等,模型数不准字数。校验又是「一处错,整批(或整条)丢」。

---

## 2. 两条通道与公共机制

### 2.1 执行体:HTTP 或 Agent

每个功能用哪条通道由设置决定(`settings::resolve_executor`):

- **HTTP**:OpenAI 兼容的 `POST {base_url}/chat/completions`,`refine/llm.rs`。配置项是 `llm_profiles` 里的一条(base_url / model / api_key)。
- **Agent**:在本机起一个 Agent CLI 子进程(Claude Code / Codex / Gemini / Pi;Cursor 只能测试和起标题),`refine/agent.rs`。Agent 通过应用自带的 MCP 服务(`voice-notes mcp serve`)读写笔记。

功能与执行体的对应:

| 功能 | 取哪个设置 | 说明 |
|---|---|---|
| 精修、起标题、身份推断 | `refine_executor` | 三者共用;`refine_enabled` 关掉则全部不跑 |
| 关系补建(批量回填) | `relations_executor`,空则回落 `refine_executor` | 仅回填路径可单独指定 |

实测 3120 条日志里 HTTP 占 3039 条(火山方舟两个接入点),Agent 路径只在 7 月用过 63 条。**当前生产路径就是 HTTP + 火山方舟(豆包)。**

### 2.2 HTTP 通道的公共参数

| 项 | 现状 | 位置 |
|---|---|---|
| 温度 | 精修 / 关系 / 身份 0.1;标题 0.3 | 各调用点 |
| `response_format: json_object` | 精修、身份有;**关系没有** | `llm.rs` |
| `max_tokens` | **所有业务调用都没设**,用服务端默认值(实测输出顶在约 4096) | — |
| 关闭思考 | base_url 含 `volces.com` 时注入 `thinking: {type: disabled}` | `apply_thinking_off` |
| 超时 | 小请求(标题、身份)60 秒;长请求(精修、关系)180 秒 | `REQ_TIMEOUT_S` / `CHUNK_TIMEOUT_S` |
| 代理 | 走 `netproxy::agent_for`,跟随系统代理与绕过表 | `netproxy.rs` |
| 提示词缓存 | 未利用。11 月精修输入 96 万 token,命中缓存的只有 8 千 | — |
| 并发 | 全部串行 | — |

### 2.3 重试规则

| 调用 | 重试 |
|---|---|
| 分块精修 | 传输层错误、408、429、5xx:最多重试 2 次,间隔 3 秒、30 秒。其余 4xx 与内容错误不重试。输出被截断(`finish_reason=length`)不重试,改为**对半拆块** |
| 关系抽取 | 传输层类错误重试 1 次,间隔 3 秒。内容错误不重试 |
| 身份推断、标题 | 不重试 |

### 2.4 日志

`ailog.rs`:每次调用一个 JSON 文件,落在 `<数据目录>/ai_logs/`,记完整请求体和响应原文,不含密钥。

已知缺口:

- 关系抽取、身份推断、标题在**解析失败时响应记 `null`**,原文丢失。精修已修过这个问题(`do_call_chunk` 的 `raw_out`),其余三处没跟上。这直接导致关系抽取的格式错误至今无法从日志确认。
- 日志没有 token 与费用的汇总口径,要靠脚本从响应的 `usage` 里重算。
- 「测试连接」不记日志。

---

## 3. Aing 主流程

入口是 `lib.rs` 的 `spawn_refine`,一篇笔记一个后台线程。触发时机:

- 停止录音后自动触发(`lifecycle/actor.rs` 的收尾动作);
- 「中断笔记就此结束」(`finalize_interrupted_note`);
- 重转写或导入音频完成后(`spawn_retranscribe`);
- 笔记页手动点 Aing(`refine_note`)。

顺序(HTTP 路径):

```
[可选] 云端二遍识别(语音,不是文本大模型)
  ↓
本地两步:过滤无效段 → 说话人重聚类 → 合并成段落(每段 ≤60 秒)      不调大模型
  ↓
① 分块精修(每 3000 字一块,串行,术语表逐块前传)                 N 次调用
  ↓
② 关系抽取(整篇一次)                                          1 次调用
  ↓ 写盘 aing.json,触发图谱索引重建
③ 说话人身份推断                                              1 次调用
  ↓ 写 identify.json
④ 起标题(仅当标题还是默认样式)                                 0~1 次调用
```

旁路入口:

- **失败段重跑** `retry_failed_refine`:只把上次失败的段落重新分块精修,成功的不重发。
- **单独重跑身份推断** `identify_note`。
- **关系批量回填** `refine/backfill.rs`(图谱页「分析」按钮):对已精修的笔记只重跑关系抽取,带预览和用户确认。
- **命令行批量起标题** `mcp/cli_query.rs`。

Agent 路径下,①② 合并为一次 Agent 运行(见 4.5),③④ 各自单独起一次 Agent。

---

## 4. 逐个调用点

### 4.1 分块精修 `refine_chunk`

- **代码**:`llm::polish_with_memos` → `call_chunk` → `do_call_chunk`;提示词 `prompts::REFINE_SYSTEM` + `refine_user_with_memos`。
- **输入**:
  - system:四类修订规则 + 实体抽取要求 + 输出 JSON 契约。
  - user:累计术语表(JSON)+ 该时间窗内的用户备注(≤800 字)+ 本块段落,每段一行 `paragraph_index=N speaker=人名或簇号: 正文`。
- **分块**:按累计字数切,每块 ≤3000 字(`CHUNK_CHARS`),单段超长独占一块。
- **输出契约**:`{"glossary":{…},"texts":[…],"entities":[…]}`,`texts` 条数必须等于输入段落数。
- **术语表前传**:上一块返回的 glossary 累进后随下一块发出。单条超过 24 字丢弃,总数上限 200 条。
- **失败处理**:
  - JSON 整体坏了:尝试只抠出 `texts` 数组(`salvage_texts`),术语表和实体放弃。
  - 输出被截断且块里不止一段:对半拆开重排队。
  - 其余失败:该块保留原文,段落下标记进 `llm_failed_paragraphs`,可事后「失败段重跑」。
  - 全部块都是网络类失败 → 整体 `failed`;有成有败 → `partial`。
- **实测**(全部 2236 次):
  - 成功 1393 次(62%)。成功调用耗时中位 40 秒、p95 66 秒、最长 179 秒。
  - 每次输入中位约 3500 token、输出约 2400 token。累计输入 507 万、输出 411 万 token,占全部用量的绝大部分。
  - 失败调用累计等待 36626 秒(约 10 小时)。
  - 130 次输出撞上限被截断。
  - 11 月 379 次里失败 101 次:**401 鉴权失败 47 次**(11/13–11/16 三天密钥失效)、TLS 连接失败 22 次、`texts` 条数不符 21 次、截断 4 次、读响应超时 4 次。
  - 盘上结果:338 篇里 `llm` 阶段 done 164、partial 133、failed 19、off 22;90 篇仍有失败段未重跑。

### 4.2 关系抽取 `refine_relations`

- **代码**:`llm::extract_relations` → `parse_relation_only_payload` → `relations::materialize`;提示词 `prompts::RELATION_ONLY_SYSTEM`。
- **输入**:`{"entities": 本篇实体, "paragraphs": [{paragraph_index, text, source_seqs}]}`,**整篇一次发完,不分块**。
- **输出契约**:`{"relations":[{subject, predicate:{type,label}, object, confidence, valid_from, valid_to, evidence:[{paragraph_index,start,end,quote}]}]}`。
  - 谓词限 8 种核心类型或带标签的 custom。
  - 证据的 `start/end` 是段内 Unicode 字符半开区间,`quote` 必须逐字符等于该区间。
- **解析**:`serde` 严格反序列化(`deny_unknown_fields`),任何字段形状不对整份失败。
- **校验** `materialize`:主宾语必须能对上实体名或别名;证据的段落下标、区间、引文逐字符相等。**任何一条关系有问题,整篇的关系全部作废**,回退到上一版图谱,阶段标 `failed`。
- **实测**(全部 172 次):
  - 调用成功 22 次(12%)。11 月 73 次里成功 7 次。
  - 11 月失败原因:`invalid type: map, expected a sequence` 55 次、401 鉴权 5 次、JSON 断尾 5 次、其它 1 次。
  - 「map, expected a sequence」的含义是:代码要一个数组,模型给了一个对象。报错位置在响应的第 120–180 个字符附近,正好是第一条关系的 `evidence` 字段出现的位置。**这是推断**:模型把 `evidence` 写成了单个对象而不是数组。失败时响应原文没有记进日志,无法直接确认。
  - 调用成功且返回了关系的那几次,应用日志(`logs/stderr.log`)里是 `evidence quote must exactly match the Unicode scalar span` 22 次、`evidence span must be a non-empty … range` 4 次、`object entity is missing` 1 次——都在校验环节整批丢弃。
  - 盘上结果:338 篇里 `relations` 阶段 failed 259、off 44、done 9;**done 的 9 篇也都是空数组**,全库关系 0 条。
  - 失败调用累计等待 6833 秒。

### 4.3 说话人身份推断 `identify`

- **代码**:`identify::run_identify` → `build_context` → `HttpIdentifyExecutor::infer` → `adjudicate`;提示词 `prompts::IDENTIFY_SYSTEM`。
- **输入**(一个 JSON):
  - `clusters`:本场各说话人簇(簇号、时长、是否来自麦克风、已确认身份)。
  - `candidates`:候选人名单,最多 30 人。来源是日历参会人与声纹库的交集,再并入声学近邻(每簇前 5)和最近共同出现的人(前 10)。
  - `sampled`:抽样的会议片段,总预算 6000 字,单段截到 200 字。抽样优先级:每簇开场 2 段 → 含候选人名的段 → 自报句式段 → 簇切换边界前后段。
  - `calendar`:当场日历事件(标题、参会人,最多 30 人)。
- **输出契约**:`{"assignments":[{cluster, person_id 或 new_name, confidence, evidence:[{paragraph_index,start,end,quote,type}]}]}`。证据类型四种:自我介绍、称呼应答配对、第三人称排除、角色主题弱线索。证据同样要求字符区间与逐字引文。
- **裁决** `adjudicate`:逐条核对证据是否逐字存在、结合声学分数定档;低档不进收件箱;用户拒绝过的不再提。通过的写进 `identify.json` 成为建议卡,默认不自动应用(`identify_auto_apply` 默认关)。
- **实测**(HTTP 268 次成功调用):
  - 耗时中位 2.4 秒,很快。
  - 每次输入约 5000 token,输出约 28 token。累计输入 135 万 token。
  - 只有 42 次给出了推断,共 57 条(51 条自报「高」置信度)。
  - 应用日志里 `条目被裁决丢弃(evidence-mismatch)` 30 条。
  - 盘上结果:226 份 `identify.json` 里总共 1 条建议(状态是已拒绝)。**没有一条被采纳。**
  - 11 月失败 8 次:401 鉴权 7 次、响应含控制字符 1 次。

### 4.4 起标题 `title`

- **代码**:`llm::gen_title_with_memos`(HTTP)/ `agent::gen_title_with_memos`(Agent);提示词 `TITLE_SYSTEM` / `agent_title`。
- **触发条件**:执行体就绪,且标题仍是默认样式。写入时再判一次(`rename_if_default`),用户中途手动改过名就放弃。
- **输入**:用户备注(≤500 字)+ 正文开头约 1500 字。
- **输出**:一行标题。去掉首尾引号和句号后,为空、超过 20 字或含换行即放弃。
- **失败处理**:不重试,保留默认名。
- **实测**(373 次):成功 347 次(93%),耗时中位 2.2 秒。每次输入约 830 token。11 月 4 次失败全是 401。
- 已知局限:只看开头 1500 字,长会议的主题如果在后面就概括不到(备注可以部分弥补)。

### 4.5 Agent 整篇 Aing `agent_refine`

- **代码**:`agent::run_refine`;提示词 `prompts::agent_refine`。
- **做法**:起一个 Agent CLI 子进程,工作目录是一次性临时目录,只挂应用自己的 MCP 服务(`VN_MCP_AGENT_MODE=1` 时只暴露 4 个工具)。提示词要求它按 6 步走:`get_note` 读稿 → 逐段修订 → `apply_refined_texts` 写回 → `get_aing_context` 重读定稿 → 抽实体与关系 → `apply_aing_graph` 提交。
- **成败判定**:不信 Agent 的任何输出,只看盘上 `aing.json` 的 `stages.llm` 是否变成 `done`。
- **超时**:整轮 900 秒;标题 120 秒;身份推断 180 秒;测试运行 60 秒。
- **各家差异**:Claude 用内联 `--mcp-config --strict-mcp-config`;Codex 用 `exec --sandbox read-only` 加 `-c` 覆盖;Gemini、Pi 把 MCP 配置写进临时工作区;Cursor 因无法关掉写文件与 shell,完整 Aing 直接拒绝。
- **配套**:
  - 关系补建 `prompts::agent_relation`(`AgentRelationExecutor`):只允许 `get_aing_context` 与 `apply_aing_graph`。
  - 身份推断 `prompts::agent_identify`:输入全部内嵌,不给任何工具,输出用一对随机标记包住。
- **实测**(仅 7 月,Claude,26 次):成功 14 次(53%),成功耗时中位 208 秒、最长 689 秒;失败 12 次里 8 次是跑满 900 秒被杀。`mcp_apply` 写回 18 次成功 15 次。Agent 起标题 37 次成功 32 次,2 次因输出夹带 HTML 或 JSON 前缀被长度守卫拦下。

### 4.6 连通性测试

- HTTP:`llm::probe`,发一条「回复 OK」,`max_tokens: 1`,超时 15 秒,按状态码给出归类原因。
- Agent:`agent::probe_run`,提示词「只回复两个字:正常」,超时 60 秒。
- 两者都不记日志。

### 4.7 外部 Agent 经 MCP 的写入

不是应用主动发起的调用,但同样是大模型产物进入数据的入口:任何连上应用 MCP 服务的外部 Agent(比如 Claude Code)都可以调 `apply_refined_texts`、`apply_aing_graph` 写回精修稿与图谱(`identify_speakers` 只读)。写回走与 4.5 相同的校验,并记 `mcp_apply` 日志。

---

## 5. 实测汇总

### 5.1 用量分布(2026-07 至 11,HTTP)

| 调用 | 次数 | 输入 token | 输出 token | 失败调用累计等待 |
|---|---|---|---|---|
| 分块精修 | 2236 | 507 万 | 411 万 | 36626 秒 |
| 身份推断 | 295 | 135 万 | 0.8 万 | 217 秒 |
| 起标题 | 373 | 31 万 | 1.1 万 | 113 秒 |
| 关系抽取 | 172 | 14 万 | 0.6 万 | 6833 秒 |

- 输出 token 几乎全在精修上:它要把正文整段重写一遍。
- 身份推断的输入占第二位,是关系抽取的近 10 倍,而产出接近零。
- 失败的 HTTP 调用拿不到 `usage`,所以失败烧掉的 token 没法从日志精确统计,上表的 token 只来自成功调用。

### 5.2 失败类型(11 月,589 条)

| 类型 | 次数 | 涉及调用 | 性质 |
|---|---|---|---|
| 关系格式不符(对象 vs 数组) | 55 | 关系 | 提示词 / 解析问题 |
| 401 鉴权失败 | 63 | 全部四类 | 配置问题,集中在 11/13、11/14、11/16 |
| TLS 连接失败 | 22 | 精修 | 网络(Clash 环境) |
| `texts` 条数不符 | 21 | 精修 | 模型合并或拆分了段落 |
| JSON 断尾 | 5 | 关系 | 输出被截断,关系调用没有判 `finish_reason` |
| 输出截断 | 4 | 精修 | 撞输出上限 |
| 读响应超时 | 4 | 精修 | 网络 |

---

## 6. 问题与优化建议

按「投入小、收益大」排序。P0 是现在就坏着的功能,P1 是明显的浪费,P2 是进一步的质量与成本优化。

### P0-1 关系抽取:让它先能产出

现状:全库 0 条关系,每篇笔记仍在为它付一次调用。

建议:

1. **不再让模型报字符位置**。只要 `paragraph_index` 和 `quote`,由程序在该段里查找引文来确定 `start/end`;找不到再做一次宽松匹配(忽略空白与标点差异),仍找不到才丢这条证据。
2. **坏一条丢一条**。`materialize` 改为逐条关系裁决,合格的入库,不合格的记原因,不再整篇作废。
3. **解析放宽**。`evidence` 接受单个对象或数组;`predicate` 接受纯字符串;去掉 `deny_unknown_fields`。
4. **请求补齐**:加 `response_format: json_object`;判断 `finish_reason=length`;显式设 `max_tokens`。
5. **失败时记原文**:解析失败把响应原文写进日志(照 `do_call_chunk` 的做法),先确认 55 次格式错误到底长什么样。
6. 提示词里给一条完整的输出示例(现在只有字段说明,没有样例)。
7. 长会议整篇一次发完会超长,补上分块或按实体密度抽样。

验收口径:拿盘上已有的 22 次成功响应和若干失败请求回放,入库关系数从 0 变为可用。

### P0-2 身份推断:同一个病,同一副药

现状:57 条推断至少 30 条死于 `evidence-mismatch`,最终采纳 0 条。

建议:证据定位改成与 P0-1 相同的「程序按引文查找」。改完先用日志里 42 次有内容的响应离线回放,看能救回多少条,再决定是否值得继续投入。

如果回放后仍然很少:考虑改成**按需触发**(用户点「帮我认人」才跑),而不是每篇自动跑。它每次要发约 5000 token 的上下文。

### P0-3 鉴权失败要立刻停,并告诉用户

现状:11/13–11/16 密钥失效期间,63 次调用照发不误——一篇 20 块的笔记会连吃 20 次 401,用户看到的只是「部分段落未精修」。

建议:

- 第一次遇到 401 / 403 就终止整轮 Aing(精修、关系、身份、标题都不再发)。
- 界面明确提示「API Key 无效」,并提供跳转到 AI 设置页的入口。
- 同理,模型不存在(404)也按配置错误处理。

### P1-1 显式设置输出上限

所有业务调用都没传 `max_tokens`,输出被服务端默认值(约 4096)卡住:累计 130 次截断,每次截断都是等了几十秒之后白跑。

建议:精修按输入字数估算后显式给足(前提是所用模型允许更大的输出上限,需查方舟文档确认);或者把 `CHUNK_CHARS` 从 3000 降到约 2000,让输出稳稳落在上限内。两者选一,用回放数据定。

### P1-2 精修只输出改动,不重写全文

精修占了几乎全部输出 token,而四类修订实际改动的字很少。

建议方向(需要评测后选):

- 让模型输出「改动列表」(段落下标 + 原片段 + 新片段),程序应用补丁。输出量可降一个数量级,耗时同比下降,截断基本消失。
- 或者先用本地规则处理确定性的部分(删「嗯」「呃」、紧邻重复、中英文之间加空格),模型只负责错字与实体归一。

风险:补丁式输出的定位又是「引文查找」问题,但这里可以用原片段精确匹配,比字符下标可靠。

### P1-3 `texts` 条数不符的兜底

11 月 21 次。模型把相邻段落合并或拆开了,整块作废。

建议:输出改成带下标的对象数组(`[{"i": 12, "t": "…"}]`),按下标对齐,缺的段保留原文,多的丢弃,不再整块失败。

### P1-4 利用提示词缓存

每块的 system 提示词完全相同,但缓存命中几乎为零(11 月 96 万输入 token 中 8 千)。

建议:确认方舟接入点的缓存开关与计费方式;把稳定内容(system、输出契约)放最前,变化内容(术语表、备注、段落)放最后。术语表目前排在 user 消息开头,每块都在变,需要挪到段落之后或并入尾部。

### P1-5 并发

分块精修完全串行,一篇 20 块的会议要等十几分钟。术语表前传是串行的唯一理由。

建议:先串行跑第 1 块建立术语表,其余块带着这份术语表并发(限 3–4 路)。全文写法一致性略降,换来几倍的速度。并发数做成可配置。

### P2-1 标题只看开头

只取前 1500 字。建议改成「开头 + 中段 + 结尾」各取一段,或直接复用精修阶段已抽出的高频实体做输入。

### P2-2 身份推断的输入瘦身

候选人最多 30 个、片段 6000 字。在 P0-2 修好之后,按实际命中的证据类型(称呼应答 37、自我介绍 28)收紧抽样策略,优先保留含人名与自报句式的段落,削减「每簇开场 2 段」这类低产出的填充。

### P2-3 观测口径

- 每次 Aing 结束写一条汇总(各阶段调用数、成功数、token、耗时),现在只能靠脚本扫日志。
- 把「失败调用烧掉的 token」估出来(按请求体字数折算)。
- 设置页或 AI 页显示最近一周的成功率,让配置失效这类问题当天可见。

### P2-4 Agent 路径的定位

实测只在 7 月用过,成功率 53%,一篇要 3 到 11 分钟,且 8 次跑满 15 分钟被杀。建议明确它的定位(没有 API Key 时的备选),不再与 HTTP 路径同步演进每一项功能;或者在 P1-2 的补丁式输出落地后重新评估。

---

## 7. 代码索引

| 内容 | 位置 |
|---|---|
| 全部提示词 | `src-tauri/src/refine/prompts.rs` |
| HTTP 调用、分块、重试、标题、测试连接 | `src-tauri/src/refine/llm.rs` |
| Aing 编排(HTTP 路径写盘) | `src-tauri/src/refine/mod.rs` 的 `run_llm` |
| 主流程入口 | `src-tauri/src/lib.rs` 的 `spawn_refine` |
| 关系校验 | `src-tauri/src/refine/relations.rs` 的 `materialize` |
| 身份推断上下文、裁决 | `src-tauri/src/refine/identify.rs` |
| Agent 子进程 | `src-tauri/src/refine/agent.rs` |
| 关系批量回填 | `src-tauri/src/refine/backfill.rs` |
| 执行体解析 | `src-tauri/src/settings.rs` 的 `resolve_executor` |
| 调用日志 | `src-tauri/src/ailog.rs` |
| 代理 | `src-tauri/src/netproxy.rs` |

关键常量:

| 常量 | 值 | 含义 |
|---|---|---|
| `llm::CHUNK_CHARS` | 3000 | 精修每块字数 |
| `llm::REQ_TIMEOUT_S` | 60 | 标题、身份推断超时 |
| `llm::CHUNK_TIMEOUT_S` | 180 | 精修、关系超时 |
| `llm::RETRY_BACKOFF_S` | 3, 30 | 精修重试间隔 |
| `llm::GLOSSARY_MAX_ENTRIES` | 200 | 术语表条数上限 |
| `llm::MEMO_CHUNK_CHARS` / `MEMO_TITLE_CHARS` | 800 / 500 | 备注进提示词的字数上限 |
| `identify::MAX_CANDIDATES` | 30 | 候选人上限 |
| `identify::SAMPLE_CHAR_BUDGET` | 6000 | 抽样片段总字数 |
| `agent::REFINE_TIMEOUT_S` | 900 | Agent 整轮超时 |

## 8. 复算统计

日志每条一个 JSON 文件,字段见 `ailog::AiLogEntry`。HTTP 调用的 token 在 `response`(字符串)解析后的 `usage` 里。按 `kind` 与 `status` 分组即可复算第 5 节的数字:

```python
import json, glob, collections
stat = collections.defaultdict(lambda: [0, 0, 0, 0])   # 次数, 成功, 输入token, 输出token
for f in glob.glob("<数据目录>/ai_logs/*.json"):
    d = json.load(open(f))
    s = stat[d["kind"]]
    s[0] += 1
    if d["status"] == "ok":
        s[1] += 1
        if isinstance(d["response"], str):
            try:
                u = json.loads(d["response"]).get("usage") or {}
                s[2] += u.get("prompt_tokens", 0)
                s[3] += u.get("completion_tokens", 0)
            except ValueError:
                pass
for kind, s in stat.items():
    print(kind, *s)
```

注意两点:日志文件名里的时间是 UTC;2026-11-12 数据目录迁移时 `ai_logs` 被漏搬,本机已于 11-24 补回(见 #847),别的机器上如果做过迁移,旧日志可能还在旧目录里。
