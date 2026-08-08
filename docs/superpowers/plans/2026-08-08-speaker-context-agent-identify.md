# Agent identify 执行体 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:executing-plans。单任务小计划(P2a Task 10 立项的兑现)。

**Goal:** 让 `refine_provider = "agent"` 的用户也能跑 identify 身份推断。此前 identify 只有 HTTP 执行体,agent 用户静默跳过。

**Architecture:** 不解析裸 stdout、不扩 MCP 写面、不给 Agent 任何工具——identify 的输入(IdentifyContext JSON)全内嵌 prompt,输出走**哨兵标记单行 JSON**:Agent 被要求把结果 JSON 作为单独一行夹在 `<VN_IDENTIFY>` 与 `</VN_IDENTIFY>` 之间输出;Rust 侧取 stdout 中**最后一对**标记之间的内容解析。执行复用既有 `title_command`(零工具/零 MCP/一发一收,四家 CLI 全支持)+ `make_scratch` + `run_with_timeout`;解析与五道裁决完全复用 `parse_raw_identify`/`adjudicate`。

**取舍说明**(对 P2a 计划 Task 10「文件交换设计」的修正):文件交换要求给 Agent 开文件写工具(Claude `--allowedTools Write`、Gemini `write_file`、多轮),工具面反而更大;哨兵标记提取对横幅/日志免疫,且 `title_command` 的 stdout 消费在仓库已有先例(probe/标题)。风险=Agent 不遵守单行约定 → 解析失败落 identify failed 日志,无数据污染(identify 本就 best-effort)。

## Tasks

### Task 1: 哨兵提取纯函数 + AgentIdentifyExecutor

**Files:** `src-tauri/src/refine/identify.rs`(提取函数)、`src-tauri/src/refine/agent.rs`(执行体)、`src-tauri/src/lib.rs`(`identify_executor` 分派改造)

- [ ] identify.rs:

```rust
/// 从 Agent stdout 提取最后一对哨兵标记之间的载荷(对启动横幅/思考文本免疫;
/// 取最后一对——Agent 若复述了指令里的标记示例,以最终输出为准)。
pub fn extract_marked(stdout: &str) -> Option<&str> {
    let start_tag = "<VN_IDENTIFY>";
    let end_tag = "</VN_IDENTIFY>";
    let start = stdout.rfind(start_tag)?;
    let rest = &stdout[start + start_tag.len()..];
    let end = rest.find(end_tag)?;
    Some(rest[..end].trim())
}
```

  单测:横幅+多行噪音中提取;出现两对取最后一对;缺结束标记 None。
- [ ] agent.rs `AgentIdentifyExecutor { kind, bin, model }`:
  - `new(kind, bin_override, model)`:四家 kind 都放行(零工具面,无需白名单限制);model 非空、resolve_bin;
  - `impl IdentifyExecutor::infer`:`make_scratch("identify")`(用后删,失败带 stderr 尾);prompt = `IDENTIFY_SYSTEM_PROMPT` + 换行 + 「输入 JSON 如下:」 + `serde_json::to_string(ctx)` + 输出约定段(「完成推断后,把结果 JSON **压缩为单独一行**输出,并夹在 <VN_IDENTIFY> 与 </VN_IDENTIFY> 之间;除这一行外不要在任何位置输出这两个标记;不要调用任何工具」);`title_command` + `run_with_timeout(cmd, scratch, IDENTIFY_AGENT_TIMEOUT_S = 180)`;退出码非 0 → Err(带 stderr 尾);`extract_marked` None → Err("Agent 未按约定输出标记");`parse_raw_identify` 复用;
  - ailog:`kind: "identify"`, `provider: "agent"`, `model`, `endpoint: None`, request = `json!({"prompt": prompt})`,response = stdout 原文(成功)/Null(失败),与 HTTP 执行体同表可查。
- [ ] lib.rs `identify_executor` 改造:

```rust
fn identify_executor(settings) -> anyhow::Result<Box<dyn IdentifyExecutor>> {
    anyhow::ensure!(settings.refine_enabled, "identify 需要已启用精修");
    match settings.refine_provider.as_str() {
        "openai"(且三字段齐) => HttpIdentifyExecutor,
        "agent" => AgentIdentifyExecutor::new(kind_from(settings.refine_agent)?, &settings.refine_agent_bin, &settings.refine_agent_model),
        _ => bail,
    }
}
```

  (外发授权语义不变:agent 用户已同意经本机 CLI 送其 provider,identify 与精修同通道。)
- [ ] 测试:`extract_marked` 三条;`AgentIdentifyExecutor::new` 参数校验(未装 CLI 环境下用 `/bin/echo` 伪 bin 的构造分支,仿 `AgentRelationExecutor` 既有测试);prompt 构造含标记约定与 ctx JSON(把 prompt 构造抽 `identify_agent_prompt(ctx, model) -> String` 纯函数单测);分派测试:`refine_enabled=false` → Err、agent 分支构造成功(bin 用 /bin/echo)。
- [ ] `cargo test --lib` 全绿;提交。

## 收尾

- 真机冒烟(并入 P3 PR 或单独 PR 描述):agent provider 用户 Aing 后 identify.json 生成;ailog 出现 provider=agent kind=identify 记录;Agent 输出不合规时 identify 静默失败不影响精修。
