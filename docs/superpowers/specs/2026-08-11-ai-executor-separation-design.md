# AI 执行体分层:模型/Agent 资源与功能项解耦

> 状态:设计稿(待用户确认后实施)
> 日期:2026-08-11
> 动机(用户原话):把在线大模型的配置、Agent(AI 助手)配置与注册跟功能项(AI 整理等)
> 分离开,让大模型和 Agent 可以在不同的功能中复用。

## 1. 现状与病灶

当前 settings 只有**一套** `refine_*` 执行体配置(provider/base_url/model/api_key/
agent/agent_bin/agent_model),被五处共享消费:

| 消费方 | 位置 | 用途 |
|---|---|---|
| AI 整理(Aing) | lib.rs spawn_refine → refine/llm.rs·agent.rs | 会后整理主流程 |
| 标题生成 | refine 流程内 | 跟随整理执行体 |
| 关系分析回填 | refine/backfill.rs(RelationExecutor) | 历史语义关系 |
| MCP CLI 查询 | mcp/cli_query.rs | Agent 反向调用 |
| 遥测 | telemetry.rs Provider::classify | 匿名分类统计 |

病灶:
1. **资源与功能一对一锁死**:想让"整理用 DeepSeek、关系分析用本机 Claude"做不到;
   未来功能(GER 纠错、热词提取)只能继续挤同一套配置。
2. **换功能配置会踩掉另一功能**:所有功能共享一份 base_url/key,改一处全体跟着变。
3. **UI 上资源与功能糅在一起**:/ai 页「AI 整理」区里既有功能开关又有服务商表单,
   「AI 助手接入」(MCP 注册)又是另一区,用户建立不起"配置一次、处处可用"的心智。
4. 今天(2026-08-11)刚做的服务商 tab + 草稿记忆已经是"多份模型配置"的雏形,
   但草稿只存在前端 localStorage,后端仍只认一份生效值——半步状态,应当转正。

## 2. 目标架构:三层

```text
资源层(可复用,配置一次)          功能层(引用资源)
┌────────────────────────┐      ┌──────────────────────────┐
│ 在线模型档案 LlmProfile │◄─────│ AI 整理    refine_executor│
│  DeepSeek / 豆包 / 自定义…│      │ (标题生成跟随整理)         │
├────────────────────────┤      ├──────────────────────────┤
│ 本机 Agent 档案          │◄─────│ 关系分析 relations_executor│
│  claude/codex/gemini/cursor│    ├──────────────────────────┤
│  (含 MCP 注册状态)       │◄─────│ (未来)GER 纠错 / 热词提取 │
└────────────────────────┘      └──────────────────────────┘
```

### 2.1 数据模型(settings.json)

```jsonc
{
  // —— 资源层 ——
  "llm_profiles": [
    { "id": "p-8f2a", "label": "DeepSeek", "base_url": "https://api.deepseek.com/v1",
      "model": "deepseek-chat", "api_key": "sk-…" }
  ],
  // Agent 档案:每种 CLI 至多一条(kind 即身份),bin/model 是既有字段的搬家。
  // 探测状态(装没装/在哪)是运行时信息,不落盘。
  "agent_profiles": [
    { "kind": "claude", "bin": "", "model": "" }
  ],

  // —— 功能层:executor 引用,格式 "llm:<profile_id>" | "agent:<kind>" | "" ——
  "refine_enabled": true,            // 功能开关,不变
  "refine_executor": "llm:p-8f2a",
  "relations_executor": ""           // 空 = 跟随 refine_executor(默认,免二次配置)
}
```

要点:
- **id 稳定**:profile 增删改不影响引用它的功能;删除被引用的 profile 时 UI 阻止
  (或引导改指)。id 用短随机串,label 用户可改。
- **relations_executor 默认跟随**:关系分析今天就复用整理配置,保持零配置升级;
  用户显式选择后才独立。
- **api_key 仍明文存本机** settings.json,与现状一致(单机应用,设置页已注明)。
- 未来新功能只需新增一个 `<feature>_executor` 字段 + UI 一个选择器。

### 2.2 迁移(一次性,向后兼容)

`SettingsRepr → Settings` 的 From 转换里做(该处已有 mix_track 迁移先例):

- 旧 `refine_base_url` 非空 → 生成一条 LlmProfile(label 按 base 猜厂商:命中预设
  表用其名,否则 "自定义";id 新造),`refine_executor = "llm:<id>"`。
- 旧 `refine_provider == "agent"` → `agent_profiles` 造对应 kind 条目(搬
  refine_agent_bin/refine_agent_model),`refine_executor = "agent:<kind>"`。
- 两者都可能存在(用户配过 HTTP 又切到 agent):都迁,executor 按旧 provider 定向。
- 旧键从 `Settings` 结构移除(SettingsRepr 保留反序列化兼容,写出时只写新键);
  迁移幂等:新键已存在则跳过旧键解析。
- 前端 localStorage 草稿(vn.refineDrafts,今天刚加)**废弃**:有 key 的草稿在首次
  打开 /ai 页时一次性转正为 llm_profiles(前端做,settings 写回),然后清掉草稿键。
  转正而不是丢弃——用户今天可能已经存了多家 key。

### 2.3 后端执行体解析(单一真源)

```rust
/// 功能层引用 → 可执行配置。所有 AI 功能(整理/标题/关系/未来纠错)一律经此,
/// 不再各自读散字段。
pub enum Executor {
    Http(LlmConfig),                       // 复用 refine/llm.rs 现有结构
    Agent { kind: AgentKind, bin: String, model: String },
}
/// which: Refine | Relations(空引用时回落 Refine)
pub fn resolve_executor(s: &Settings, which: Feature) -> Option<Executor>;
/// 就绪判定(refine_llm_ready / refine_agent_ready 收编于此):
pub fn executor_ready(s: &Settings, which: Feature) -> bool;
```

改造消费方(等价替换,行为不变):
- lib.rs spawn_refine / 标题:resolve_executor(Refine)
- refine/backfill.rs:resolve_executor(Relations)
- mcp/cli_query.rs:resolve_executor(Refine)(现状语义)
- telemetry.rs:classify 改收 Executor

### 2.4 UI(/ai 页重排)

```text
┌ 模型与助手(资源区) ─────────────────────────────┐
│ ▸ 在线模型                                        │
│   [DeepSeek ●][通义千问][豆包 ●][自定义…][ + ]      │  ← 今天的 tab 条直接升级:
│   base / model / key / 测试连接                    │    tab=真实 profile,+ 可新增,
│                                                   │    预设变为「新建时的模板」
│ ▸ 本机 Agent                                      │
│   claude  已找到 /usr/local/bin/claude  [注册 MCP] │  ← 「AI 助手接入」并入:
│   codex   未检测到安装                             │    探测/bin/model/测试运行/
│   …                                               │    MCP 注册状态同屏
└───────────────────────────────────────────────────┘
┌ 功能区 ───────────────────────────────────────────┐
│ AI 整理   [开关]  执行体: [DeepSeek ▾]             │  ← ExecutorPicker 复用组件:
│           (标题生成跟随此执行体)                    │    下拉列出 profiles(绿点=有 key)
│ 关系分析  执行体: [跟随 AI 整理 ▾]                  │    与已就绪 Agents
└───────────────────────────────────────────────────┘
「Agent 能调用什么」「AI 调用日志」两区不动。
```

交互原则(延续今天两轮冒烟的反馈):
- 资源区只谈"连接是否可用"(测试连接/测试运行),不谈功能;
- 功能区只做"开关 + 选执行体",选择器里直接显示就绪状态,不引导用户跳来跳去;
- 不展示用户拿它没用的状态标签;删除被引用的 profile → 就地提示哪个功能在用。

### 2.5 不做什么(YAGNI)

- 不做多 Agent 同 kind 多实例(每种 CLI 一条档案够用);
- 不做功能级模型参数覆盖(温度等)——需要时再加 per-feature options;
- 不做云端 ASR 凭证并入(那是识别链路的资源,语义不同,放设置页现位);
- MCP 注册机制本身(写配置文件/自愈)零改动,只挪 UI 归属。

## 3. 实施拆分(预估 2-3 个工作日)

1. **后端 schema + 迁移 + resolve_executor**(settings.rs/lib.rs/refine/backfill/
   cli_query/telemetry;迁移与就绪判定单测)——功能行为逐位等价。
2. **前端资源区**(在线模型 profile 管理 + Agent 卡合并 MCP 注册;草稿转正)。
3. **前端功能区**(ExecutorPicker + AI 整理/关系分析改造;删除被引用保护)。
4. 回归:Aing 全流程、关系分析回填、MCP CLI 查询、遥测分类、老配置升级迁移。

## 4. 风险

- 迁移是最大风险面:老用户三种形态(纯 HTTP/纯 agent/两者都配过)必须各有单测;
  迁移失败的兜底是"读出默认值",绝不丢 key(旧键在 repr 层仍可读)。
- backfill/cli_query 有各自的 provider 校验逻辑(request.provider 对账),需同步改;
- 前端 Settings 类型变化波及 /ai 与设置页两处。
