# 外部集成配置「测试」按钮 设计

日期:2026-07-16

## 背景与问题

用户需求原话:「所有调用外部的功能,配置好后需要有测试功能。」

触发场景:一篇长会议笔记的 Agent 精修连续两次超时被杀(`Agent 进程超时(900s),已杀`),但界面只显示笼统的「LLM 精修失败,当前展示本地精修结果」,用户无法在配置阶段预先确认外部集成是否可用,只能等真实任务失败后才发现。

现状盘点(外部集成配置面 × 是否已有测试):

| 配置面 | 是否外部调用 | 现有测试 |
|---|---|---|
| 钩子(webhook/shell) | 是 | **已有**:`test_hook` + 「测试一次」完整流程 |
| HTTP 大模型精修 | 是(chat/completions) | 无 |
| Agent CLI 精修 | 是(拉起 CLI) | 仅 `refine_agents_probe` 文件存在性检查,非真跑 |
| 镜像加速 / 模型下载 | 是(下载 URL) | 无(仅运行时镜像→源站回退) |
| 遥测 / MCP | 外部性弱或本机 | 无(不纳入,见下) |

## 目标

给三个**尚无测试**的外部集成配置面各加一个手动「测试」按钮,配置好后即可就地验证连通性/可用性,失败时给出**归类原因**而非笼统提示。

范围(用户确认):**HTTP 大模型精修 + Agent CLI 精修 + 镜像加速**。
不纳入:钩子(已有测试)、遥测(自托管 Aptabase,隐私敏感、fire-and-forget,测试价值低)、MCP(本机 Unix socket,非外部端点)、更新检查(「检查更新」按钮本身即实时探测)。

## 方案选择

**方案 A(采纳):照搬钩子已验证的测试模式,逐面加。** 每个配置面加手动「测试」按钮,结果三态内联显示(绿=通过/红=失败),改相关字段即清空结果。后端三个专用探测命令,复用现有生产代码。

- 方案 B(通用「连接测试」框架):三个面调用形态差异大(HTTP / 拉起 CLI / 镜像 HEAD),抽象收益低,过度设计,否。
- 方案 C(保存时自动测):LLM 测试是付费 API 调用,失焦即打一枪既费钱又意外,否。手动、显式、便宜。

理由:`test_hook` 的交互(手动按钮 + 内联三态 + 改字段清结果)已在钩子编辑器验证,复用它 UX 统一、无新概念、实现风险最低。

## 详细设计

### 1. 后端探测(复用现有生产代码)

三个探测函数均返回 `Result<String, String>`:`Ok` = 成功细节,`Err` = **归类原因**。与 `test_hook` 同型,前端无需新类型。

**`refine/llm.rs` 加 `probe(base_url, model, api_key) -> Result<String,String>`**
- 复用现成的 `ureq` POST + `Authorization: Bearer` 头 + JSON 解析,向 `{base_url}/chat/completions` 发一条最小请求(极短提示,如「回复 OK」),约 15s 超时。
- 通过判据:HTTP 2xx **且** 能解析出 `choices[0].message.content`。
- 归类原因:`连不上端点`(传输错)/ `认证失败(401/403)` / `模型不存在(404)` / `超时` / `返回内容异常`(2xx 但 JSON/字段不对)。
- 「HTTP 状态码 → 归类原因」抽成纯函数,便于单测。

**`refine/agent.rs` 加 `probe_run(provider, bin, model) -> Result<String,String>`**
- 复用已有 `build_cmd` + `run_with_timeout`(即「生成标题」同一条路),跑一句极短提示。
- 专设 `PROBE_TIMEOUT_S`(约 30–60s,远小于精修的 900s):测试只验「能启动+能回写」,不验长任务耗时。
- 通过判据:退出码 0 **且** stdout 非空。
- 归类原因:`未找到 CLI` / `退出码非0` / `超时` / `无输出`。

**`models/download.rs` 加 `probe_mirror(prefix) -> Result<String,String>`**
- 取一个已知模型资源 URL,走 `apply_mirror(url, true, prefix)` 后发 **HEAD**(只探可达、不下载正文),约 10s 超时。
- 通过判据:经镜像返回 2xx。
- 归类原因:`镜像不可达` / `超时` / `非2xx`。

### 2. Tauri 命令

`lib.rs` 加三个 `#[tauri::command]` 并注册进 `generate_handler!`:
- `test_refine_llm(base_url, model, api_key)`
- `test_refine_agent(provider, bin, model)`
- `test_mirror(prefix)`

均接收**当前表单值作参数**(仿 `test_hook` 收 `cfg`),测「屏上当前值」而非仅已存盘值——即使刚敲完未失焦也测得准。长耗时命令用 `spawn_blocking`(同 `test_hook`)避免阻塞。

### 3. 前端界面与交互(仿 `hooks/[id]/+page.svelte`)

**位置**
- AI 页 · HTTP 大模型区:api_key 字段下方加「测试连接」按钮 + 内联结果,仅 `refine_provider = openai` 时显示。
- AI 页 · Agent 区:加「测试运行」按钮 + 内联结果,仅 `refine_provider = agent` 时显示;现有文件存在性提示保留作即时反馈。
- 设置页 · 镜像加速行:开关旁加「测试」按钮 + 行内状态(仿「检查更新」行),仅 `mirror_enabled` 时可点。

**交互**
- 按钮文案 `测试` →(进行中)`测试中…`,期间禁用。
- 结果三态 `{ ok: boolean, msg: string } | null`,就地内联:成功绿字「测试成功(<细节>)」,失败红字「测试失败:<归类原因>」。
- **改任一相关字段即把结果清回 `null`**(旧「通过」不给改过的配置背书)。
- 配置不全(如 base_url/model/key 有空)时按钮禁用。

**invoke 包装**:`src/lib/models.ts` 加三个 typed 包装(`testRefineLlm` / `testRefineAgent` / `testMirror`),对齐现有 `getSettings/setSettings` 风格。

## 文件改动清单

后端:
- `src-tauri/src/refine/llm.rs` — 加 `probe` + 纯化的状态码归类函数
- `src-tauri/src/refine/agent.rs` — 加 `probe_run` + `PROBE_TIMEOUT_S`
- `src-tauri/src/models/download.rs` — 加 `probe_mirror`
- `src-tauri/src/lib.rs` — 3 个 `#[command]` + `generate_handler!` 注册

前端:
- `src/routes/ai/+page.svelte` — 大模型 + Agent 两个测试按钮 / 三态 / 改字段清结果
- `src/routes/settings/+page.svelte` — 镜像测试按钮 / 行内状态
- `src/lib/models.ts` — 3 个 `invoke` 包装

## 测试

- Rust 单测:LLM 的「HTTP 状态码 → 归类原因」纯函数逐项断言;Agent 的「未找到 CLI」路径。真实网络调用不进单测(不稳定),只测可纯化的映射逻辑。
- `npm run check` + `cargo check` / `cargo test` 全绿。
- 手动验证:配坏 key / 错地址 / 不可达镜像,点测试看是否显示对应归类原因;配正确值看是否通过。

## 非目标 / YAGNI

- 不做通用测试框架、不做测试历史/日志留存、不做自动定时健康检查。
- 不改真实精修失败态的界面(把失败归类透出到精修结果 banner 是**相关但独立**的增强,另议)。
- 不纳入遥测 / MCP / 更新检查的测试按钮。
