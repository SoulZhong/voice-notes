# 外部集成配置「测试」按钮 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给 HTTP 大模型精修、Agent CLI 精修、镜像加速三个外部集成配置面各加一个手动「测试」按钮,配置好即可就地验证,失败给归类原因。

**Architecture:** 后端三个 `Result<String,String>` 探测函数(复用现有 `llm.rs`/`agent.rs`/`download.rs` 生产代码)+ 三个 `spawn_blocking` 的 `#[tauri::command]`;前端照搬钩子编辑器已验证的「测试」三态内联模式(绿=通过/红=失败,改字段即清空)。

**Tech Stack:** Rust(Tauri v2、ureq 2.x、serde_json、anyhow)、SvelteKit(Svelte 5 runes、`@tauri-apps/api` invoke)。

## Global Constraints

- git 提交信息**不加** `Co-Authored-By` 或任何 Claude/Generated 署名尾注,正文直接结尾。
- Tauri v2 命令参数在 JS 侧用 **camelCase**(Rust `base_url` ↔ JS `baseUrl`),与现有 `set_settings({ newSettings })` 一致。
- 探测命令一律 `Result<String, String>`:`Ok` = 成功细节,`Err` = 归类原因(与现有 `test_hook` 同型)。
- 长耗时命令走 `tauri::async_runtime::spawn_blocking`,不占 IPC 线程(同 `test_hook`)。
- 前端组件无单测基建(仓库现状):前端任务以 `npm run check` 零错误 + 手动验证为准。

---

### Task 1: 后端 — 大模型精修探测 `test_refine_llm`

**Files:**
- Modify: `src-tauri/src/refine/llm.rs`(加 `PROBE_TIMEOUT_S`、`classify_http_status`、`probe` + 单测)
- Modify: `src-tauri/src/lib.rs`(加 `test_refine_llm` 命令 + 注册)

**Interfaces:**
- Consumes:现有 `pub struct LlmConfig { base_url, model, api_key }`(llm.rs:11)。
- Produces:`pub fn refine::llm::probe(cfg: &LlmConfig) -> Result<String, String>`;`pub fn refine::llm::classify_http_status(status: u16) -> &'static str`;Tauri 命令 `test_refine_llm(base_url, model, api_key) -> Result<String,String>`。

- [ ] **Step 1: 写失败测试(纯函数归类)**

在 `src-tauri/src/refine/llm.rs` 末尾追加(若已有 `#[cfg(test)] mod tests` 则并入):

```rust
#[cfg(test)]
mod probe_tests {
    use super::classify_http_status;

    #[test]
    fn classify_maps_status_to_reason() {
        assert!(classify_http_status(401).contains("认证"));
        assert!(classify_http_status(403).contains("认证"));
        assert!(classify_http_status(404).contains("模型不存在"));
        assert!(classify_http_status(429).contains("限流"));
        assert!(classify_http_status(500).contains("服务端"));
        assert_eq!(classify_http_status(418), "返回异常");
    }
}
```

- [ ] **Step 2: 运行测试,确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib probe_tests`
Expected: 编译失败 —— `cannot find function classify_http_status`。

- [ ] **Step 3: 实现探测与归类**

在 `src-tauri/src/refine/llm.rs` 顶部常量区(`REQ_TIMEOUT_S` 附近)加:

```rust
/// 「测试连接」探测的超时:比生产 REQ_TIMEOUT_S 短,测试不该久等。
pub const PROBE_TIMEOUT_S: u64 = 15;
```

在文件中(`chunk_indices` 之前或之后皆可,模块级)加:

```rust
/// HTTP 状态码 → 归类原因(供「测试连接」按钮显示具体原因)。纯函数,可单测。
pub fn classify_http_status(status: u16) -> &'static str {
    match status {
        401 | 403 => "认证失败(API Key 无效或无权限)",
        404 => "模型不存在或接口地址错误",
        429 => "触发限流",
        s if s >= 500 => "服务端错误",
        _ => "返回异常",
    }
}

/// 「测试连接」:发一条最小 chat/completions,验证端点可达 + 鉴权通过 + 模型可用。
/// 成功返回简短摘要;失败返回归类原因。不落 AI 日志(测试噪音不入库)。
pub fn probe(cfg: &LlmConfig) -> Result<String, String> {
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let body = json!({
        "model": cfg.model,
        "max_tokens": 1,
        "messages": [{ "role": "user", "content": "回复 OK" }],
    })
    .to_string();
    let resp = ureq::post(&url)
        .timeout(std::time::Duration::from_secs(PROBE_TIMEOUT_S))
        .set("authorization", &format!("Bearer {}", cfg.api_key))
        .set("content-type", "application/json")
        .send_string(&body);
    match resp {
        Ok(r) => {
            let txt = r.into_string().map_err(|e| format!("读取响应失败: {e}"))?;
            let v: Value = serde_json::from_str(&txt)
                .map_err(|_| "返回非 JSON,可能不是 OpenAI 兼容接口".to_string())?;
            if v["choices"][0]["message"]["content"].is_string() {
                Ok(format!("连接正常,模型 {} 可用", cfg.model))
            } else {
                Err("返回内容异常(缺 choices[0].message.content)".to_string())
            }
        }
        Err(ureq::Error::Status(code, _)) => {
            Err(format!("{}(HTTP {code})", classify_http_status(code)))
        }
        Err(ureq::Error::Transport(t)) => {
            let s = t.to_string();
            if s.contains("timed out") || s.contains("timeout") {
                Err("连接超时".to_string())
            } else {
                Err(format!("连不上端点:{s}"))
            }
        }
    }
}
```

- [ ] **Step 4: 运行测试,确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib probe_tests`
Expected: PASS。

- [ ] **Step 5: 加 Tauri 命令并注册**

在 `src-tauri/src/lib.rs` 的 `test_hook`(约 2388 行)之后加:

```rust
/// 配置页「测试连接」:发一条最小 chat/completions 验证大模型精修配置。
#[tauri::command]
async fn test_refine_llm(base_url: String, model: String, api_key: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        refine::llm::probe(&refine::llm::LlmConfig { base_url, model, api_key })
    })
    .await
    .map_err(|e| format!("执行线程失败: {e}"))?
}
```

在 `tauri::generate_handler![` 列表里 `test_hook,`(约 2921 行)之后加一行:

```rust
            test_refine_llm,
```

- [ ] **Step 6: 编译确认**

Run: `cargo check --manifest-path src-tauri/Cargo.toml`
Expected: 无 error(允许既有 warning)。

- [ ] **Step 7: 提交**

```bash
git add src-tauri/src/refine/llm.rs src-tauri/src/lib.rs
git commit -m "大模型精修加测试连接:probe 发最小请求验证端点/鉴权/模型,失败归类为认证/404/限流/超时/连不上"
```

---

### Task 2: 后端 — Agent CLI 精修探测 `test_refine_agent`

**Files:**
- Modify: `src-tauri/src/refine/agent.rs`(加 `PROBE_TIMEOUT_S`、`probe_run` + 单测)
- Modify: `src-tauri/src/lib.rs`(加 `test_refine_agent` 命令 + 注册)

**Interfaces:**
- Consumes:现有 `AgentKind::from_key`(agent.rs:36)、`resolve_bin`(agent.rs:70)、`make_scratch`(agent.rs:353)、`run_with_timeout`(agent.rs:321)、`title_command`(同模块私有)。
- Produces:`pub fn refine::agent::probe_run(provider: &str, bin_override: &str, model: &str) -> Result<String, String>`;Tauri 命令 `test_refine_agent(provider, bin, model) -> Result<String,String>`。

- [ ] **Step 1: 写失败测试(不触发真实进程的错误路径)**

在 `src-tauri/src/refine/agent.rs` 的 `#[cfg(test)] mod tests`(约 506 行)内追加:

```rust
    #[test]
    fn probe_run_unknown_provider_errs() {
        let e = super::probe_run("nope", "", "").unwrap_err();
        assert!(e.contains("未知 Agent"), "得到: {e}");
    }

    #[test]
    fn probe_run_missing_bin_errs() {
        // override 指向不存在路径 → resolve_bin 返回 None,不 spawn 任何进程。
        let e = super::probe_run("claude", "/definitely/not/here/claude", "").unwrap_err();
        assert!(e.contains("未找到"), "得到: {e}");
    }
```

- [ ] **Step 2: 运行测试,确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib probe_run`
Expected: 编译失败 —— `cannot find function probe_run`。

- [ ] **Step 3: 实现探测**

在 `src-tauri/src/refine/agent.rs` 常量区(`TITLE_TIMEOUT_S` 附近)加:

```rust
/// 「测试运行」探测的超时:只验能启动+能产出,远短于精修的 REFINE_TIMEOUT_S。
pub const PROBE_TIMEOUT_S: u64 = 60;
```

在 `probe_all`(约 110 行)之后加:

```rust
/// 「测试运行」:用配好的 CLI 跑一句极短提示,验证能启动并产出。不依赖任何笔记,
/// 不落 AI 日志。成功返回 stdout 摘要;失败返回归类原因。
pub fn probe_run(provider: &str, bin_override: &str, model: &str) -> Result<String, String> {
    let kind = AgentKind::from_key(provider).ok_or_else(|| format!("未知 Agent: {provider}"))?;
    let bin = resolve_bin(kind, bin_override)
        .ok_or_else(|| format!("未找到 {} 命令行工具:请先安装并登录,或指定 CLI 路径", kind.bin_name()))?;
    let scratch = make_scratch("probe").map_err(|e| format!("建工作区失败: {e}"))?;
    let prompt = "只回复两个字:正常。不要任何解释。";
    let run = (|| -> anyhow::Result<(bool, String, String)> {
        let cmd = title_command(kind, &bin, model, prompt, &scratch);
        run_with_timeout(cmd, &scratch, PROBE_TIMEOUT_S)
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    match run {
        Err(e) => {
            let s = e.to_string();
            if s.contains("超时") {
                Err(format!("测试超时({PROBE_TIMEOUT_S}s):CLI 可能未登录或卡住"))
            } else {
                Err(format!("启动失败:{s}"))
            }
        }
        Ok((exit_ok, stdout, err_tail)) => {
            if !exit_ok {
                Err(format!("退出码非 0;stderr 尾部:\n{err_tail}"))
            } else if stdout.trim().is_empty() {
                Err("无输出:CLI 可能未登录或未产出结果".to_string())
            } else {
                let first: String = stdout.trim().lines().next().unwrap_or("").chars().take(30).collect();
                Ok(format!("CLI 可用,返回:{first}"))
            }
        }
    }
}
```

- [ ] **Step 4: 运行测试,确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib probe_run`
Expected: 2 passed(两个错误路径都不 spawn 进程,秒级返回)。

- [ ] **Step 5: 加 Tauri 命令并注册**

在 `src-tauri/src/lib.rs` 的 `test_refine_llm` 之后加:

```rust
/// 配置页「测试运行」:用配好的 Agent CLI 跑一句极短提示验证可用。
#[tauri::command]
async fn test_refine_agent(provider: String, bin: String, model: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || refine::agent::probe_run(&provider, &bin, &model))
        .await
        .map_err(|e| format!("执行线程失败: {e}"))?
}
```

在 `generate_handler![` 列表里 `test_refine_llm,` 之后加:

```rust
            test_refine_agent,
```

- [ ] **Step 6: 编译确认**

Run: `cargo check --manifest-path src-tauri/Cargo.toml`
Expected: 无 error。

- [ ] **Step 7: 提交**

```bash
git add src-tauri/src/refine/agent.rs src-tauri/src/lib.rs
git commit -m "Agent 精修加测试运行:probe_run 真跑一句极短提示验证 CLI 能启动+产出,取代仅查文件存在的弱探测;失败归类为未找到/退出非0/超时/无输出"
```

---

### Task 3: 后端 — 镜像加速探测 `test_mirror`

**Files:**
- Modify: `src-tauri/src/models/download.rs`(加 `probe_mirror` + 单测)
- Modify: `src-tauri/src/lib.rs`(加 `test_mirror` 命令 + 注册)

**Interfaces:**
- Consumes:现有 `apply_mirror`(download.rs:14)、`crate::models::ARTIFACTS`(有 `.url` 字段,见 download.rs 测试用法)。
- Produces:`pub fn models::download::probe_mirror(prefix: &str) -> Result<String, String>`;Tauri 命令 `test_mirror(prefix) -> Result<String,String>`。

- [ ] **Step 1: 写失败测试(空前缀守卫,无网络)**

在 `src-tauri/src/models/download.rs` 的 `#[cfg(test)] mod tests` 内追加:

```rust
    #[test]
    fn probe_mirror_empty_prefix_errs() {
        assert!(probe_mirror("   ").unwrap_err().contains("为空"));
    }
```

- [ ] **Step 2: 运行测试,确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib probe_mirror_empty_prefix`
Expected: 编译失败 —— `cannot find function probe_mirror`。

- [ ] **Step 3: 实现探测**

在 `src-tauri/src/models/download.rs` 的 `download_urls`(约 33 行)之后加:

```rust
/// 「测试」镜像:经前缀对一个已知模型资源发 Range 探测请求(只取 1 字节,不拉正文),
/// 验证镜像可达。空前缀直接报错(未启用/未填无可测)。成功返回 HTTP 状态。
pub fn probe_mirror(prefix: &str) -> Result<String, String> {
    let p = prefix.trim();
    if p.is_empty() {
        return Err("镜像前缀为空".to_string());
    }
    // 取注册表里一个稳定的小资源(vad,~1MB)做探测;Range 只要头 1 字节。
    let origin = crate::models::ARTIFACTS
        .iter()
        .find(|a| a.id == "vad")
        .map(|a| a.url)
        .unwrap_or("https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx");
    let url = apply_mirror(origin, true, p);
    match ureq::get(&url)
        .timeout(Duration::from_secs(10))
        .set("Range", "bytes=0-0")
        .call()
    {
        Ok(r) => Ok(format!("镜像可达(HTTP {})", r.status())),
        Err(ureq::Error::Status(code, _)) if (200..400).contains(&code) => {
            Ok(format!("镜像可达(HTTP {code})"))
        }
        Err(ureq::Error::Status(code, _)) => Err(format!("镜像返回 HTTP {code}")),
        Err(ureq::Error::Transport(t)) => {
            let s = t.to_string();
            if s.contains("timed out") || s.contains("timeout") {
                Err("镜像连接超时".to_string())
            } else {
                Err(format!("镜像不可达:{s}"))
            }
        }
    }
}
```

- [ ] **Step 4: 运行测试,确认通过**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --lib probe_mirror_empty_prefix`
Expected: PASS。

- [ ] **Step 5: 加 Tauri 命令并注册**

在 `src-tauri/src/lib.rs` 的 `test_refine_agent` 之后加:

```rust
/// 设置页「测试」镜像:经镜像前缀探一个已知资源验证可达。
#[tauri::command]
async fn test_mirror(prefix: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || models::download::probe_mirror(&prefix))
        .await
        .map_err(|e| format!("执行线程失败: {e}"))?
}
```

在 `generate_handler![` 列表里 `test_refine_agent,` 之后加:

```rust
            test_mirror,
```

- [ ] **Step 6: 编译确认**

Run: `cargo check --manifest-path src-tauri/Cargo.toml`
Expected: 无 error。

- [ ] **Step 7: 提交**

```bash
git add src-tauri/src/models/download.rs src-tauri/src/lib.rs
git commit -m "镜像加速加测试:probe_mirror 经前缀对已知资源发 Range 探测验证可达,空前缀直接报错"
```

---

### Task 4: 前端 — models.ts 三个 invoke 包装

**Files:**
- Modify: `src/lib/models.ts`(加三个导出)

**Interfaces:**
- Consumes:Task 1–3 的三个 Tauri 命令。
- Produces:`testRefineLlm(baseUrl, model, apiKey) => Promise<string>`;`testRefineAgent(provider, bin, model) => Promise<string>`;`testMirror(prefix) => Promise<string>`。

- [ ] **Step 1: 加包装**

在 `src/lib/models.ts` 末尾(现有导出之后)加:

```ts
// —— 外部集成配置测试(失败时命令 reject,前端 catch 出归类原因) ——
export const testRefineLlm = (baseUrl: string, model: string, apiKey: string) =>
  invoke<string>("test_refine_llm", { baseUrl, model, apiKey });
export const testRefineAgent = (provider: string, bin: string, model: string) =>
  invoke<string>("test_refine_agent", { provider, bin, model });
export const testMirror = (prefix: string) => invoke<string>("test_mirror", { prefix });
```

- [ ] **Step 2: 类型检查**

Run: `npm run check`
Expected: 0 errors 0 warnings。

- [ ] **Step 3: 提交**

```bash
git add src/lib/models.ts
git commit -m "前端加三个测试命令 invoke 包装:testRefineLlm/testRefineAgent/testMirror"
```

---

### Task 5: 前端 — AI 页大模型 + Agent 测试按钮

**Files:**
- Modify: `src/routes/ai/+page.svelte`(script 加状态/处理函数;两处 markup 加按钮+结果;三处输入加清结果;style 加 `.test-result`)

**Interfaces:**
- Consumes:Task 4 的 `testRefineLlm` / `testRefineAgent`;现有状态 `refineBaseUrl/refineModel/refineKey/refineProvider/refineAgent/refineAgentBin/refineAgentModel/agentProbe`、`saveRefine`/`saveRefineAgent`、`.btn-secondary` 样式。

- [ ] **Step 1: script 加导入与状态**

在 `src/routes/ai/+page.svelte` 顶部 `import { getSettings, setSettings, type Settings } from "$lib/models";`(约第 4 行)改为:

```ts
  import { getSettings, setSettings, testRefineLlm, testRefineAgent, type Settings } from "$lib/models";
```

在 `let refineAgentModel = $state("");`(约 64 行)之后加:

```ts
  // 测试三态(null=没测过);改相关字段即清空,防旧「通过」给改过的配置背书。
  let llmTest = $state<{ ok: boolean; msg: string } | null>(null);
  let llmTesting = $state(false);
  let agentTest = $state<{ ok: boolean; msg: string } | null>(null);
  let agentTesting = $state(false);
  const llmMissing = $derived(!refineBaseUrl.trim() || !refineModel.trim() || !refineKey.trim());
  const agentMissing = $derived(!refineAgentBin.trim() && !agentProbe[refineAgent]);
  async function runLlmTest() {
    llmTesting = true;
    llmTest = null;
    try {
      llmTest = { ok: true, msg: await testRefineLlm(refineBaseUrl.trim(), refineModel.trim(), refineKey.trim()) };
    } catch (e) {
      llmTest = { ok: false, msg: String(e) };
    } finally {
      llmTesting = false;
    }
  }
  async function runAgentTest() {
    agentTesting = true;
    agentTest = null;
    try {
      agentTest = { ok: true, msg: await testRefineAgent(refineAgent, refineAgentBin.trim(), refineAgentModel.trim()) };
    } catch (e) {
      agentTest = { ok: false, msg: String(e) };
    } finally {
      agentTesting = false;
    }
  }
```

- [ ] **Step 2: 保存时清测试结果(改字段失效)**

在 `saveRefine()`(约 171 行)函数体最前面加一行 `llmTest = null;`;在 `saveRefineAgent()`(约 178 行)函数体最前面加一行 `agentTest = null;`。改后示例:

```ts
  function saveRefine() {
    llmTest = null;
    saveSetting((s) => {
      s.refine_base_url = refineBaseUrl.trim();
      s.refine_model = refineModel.trim();
      s.refine_api_key = refineKey.trim();
    });
  }
  function saveRefineAgent() {
    agentTest = null;
    saveSetting((s) => {
      s.refine_provider = refineProvider;
      s.refine_agent = refineAgent;
      s.refine_agent_bin = refineAgentBin.trim();
      s.refine_agent_model = refineAgentModel.trim();
    });
  }
```

- [ ] **Step 3: Agent 区加测试按钮**

在 CLI 路径行(约 380–386 行,`placeholder="自动探测"` 那个 `.row` 的 `</div>`)之后、`<p class="config-hint">`(约 387 行)之前插入:

```svelte
        <div class="row">
          <div class="row-info">
            <span class="row-label">测试运行</span>
            <span class="row-desc">用该 CLI 跑一句极短提示,验证能启动并产出(约 1 分钟内)</span>
          </div>
          <button class="btn-secondary" onclick={runAgentTest} disabled={agentTesting || agentMissing}>
            {agentTesting ? "测试中…" : "测试"}
          </button>
        </div>
        {#if agentTest}
          <p class="test-result" class:ok={agentTest.ok} class:err={!agentTest.ok}>
            {agentTest.ok ? `测试成功(${agentTest.msg})` : `测试失败: ${agentTest.msg}`}
          </p>
        {/if}
```

- [ ] **Step 4: 大模型区加测试按钮 + 输入清结果**

在 API Key 行(约 419–425 行 `type="password"` 那个 `.row` 的 `</div>`)之后、`{#if !refineBaseUrl || !refineModel || !refineKey}`(约 426 行)之前插入:

```svelte
        <div class="row">
          <div class="row-info">
            <span class="row-label">测试连接</span>
            <span class="row-desc">发一条最小请求,验证接口地址 / 密钥 / 模型可用</span>
          </div>
          <button class="btn-secondary" onclick={runLlmTest} disabled={llmTesting || llmMissing}>
            {llmTesting ? "测试中…" : "测试"}
          </button>
        </div>
        {#if llmTest}
          <p class="test-result" class:ok={llmTest.ok} class:err={!llmTest.ok}>
            {llmTest.ok ? `测试成功(${llmTest.msg})` : `测试失败: ${llmTest.msg}`}
          </p>
        {/if}
```

给大模型三个输入各加 `oninput={() => (llmTest = null)}`(键入即清,不必等失焦)。分别把:
- `<input class="row-input wide" placeholder="https://api.deepseek.com/v1" bind:value={refineBaseUrl} onblur={saveRefine} />`
- 模型 `<input class="row-input" placeholder={...} bind:value={refineModel} onblur={saveRefine} />`
- API Key `<input class="row-input wide" type="password" placeholder="sk-..." bind:value={refineKey} onblur={saveRefine} />`

改为在 `onblur={saveRefine}` 后各补 `oninput={() => (llmTest = null)}`。例如第一个:

```svelte
          <input class="row-input wide" placeholder="https://api.deepseek.com/v1" bind:value={refineBaseUrl} onblur={saveRefine} oninput={() => (llmTest = null)} />
```

同理给 Agent 的模型 / CLI 路径两个输入(约 378、385 行)各补 `oninput={() => (agentTest = null)}`。

- [ ] **Step 5: style 加 `.test-result`**

在 `src/routes/ai/+page.svelte` 的 `<style>` 内(任意位置)加(照搬钩子编辑器):

```css
  .test-result { font-size: 0.85rem; margin: 0.4rem 0 0.2rem; }
  .test-result.ok { color: var(--success, var(--ink-secondary)); }
  .test-result.err { color: var(--danger-ink); }
```

- [ ] **Step 6: 类型检查**

Run: `npm run check`
Expected: 0 errors 0 warnings。

- [ ] **Step 7: 手动验证(记录结果)**

`npm run tauri dev` 起应用 → AI 页:①在线接口填错 base_url,点「测试」应显示红字「连不上端点…」;②填对配置点「测试」应显示绿字「连接正常…」;③切本机 Agent,点「测试运行」,已装 CLI 应绿字返回、未装应红字「未找到…」。

- [ ] **Step 8: 提交**

```bash
git add src/routes/ai/+page.svelte
git commit -m "AI 页大模型/Agent 精修加测试按钮:三态内联结果(绿通过/红失败带归类原因),改字段即清空"
```

---

### Task 6: 前端 — 设置页镜像加速测试按钮

**Files:**
- Modify: `src/routes/settings/+page.svelte`(script 加状态/处理;镜像行 markup 加按钮+行内状态;`toggleMirror` 清结果;style 加 ok/err)

**Interfaces:**
- Consumes:Task 4 的 `testMirror`;现有 `settings.mirror_prefix`/`mirror_enabled`、`toggleMirror`(约 432 行)、`.btn-secondary` 样式。

- [ ] **Step 1: script 加导入与状态**

确认 `src/routes/settings/+page.svelte` 顶部已从 `$lib/models` 导入其它命令;在该 import 里补 `testMirror`。若无现成从 `$lib/models` 的导入行,则新增:

```ts
  import { testMirror } from "$lib/models";
```

在 `let updateError = $state("");`(约 36 行)之后加:

```ts
  let mirrorTest = $state<{ ok: boolean; msg: string } | null>(null);
  let mirrorTesting = $state(false);
  async function runMirrorTest() {
    if (!settings) return;
    mirrorTesting = true;
    mirrorTest = null;
    try {
      mirrorTest = { ok: true, msg: await testMirror(settings.mirror_prefix) };
    } catch (e) {
      mirrorTest = { ok: false, msg: String(e) };
    } finally {
      mirrorTesting = false;
    }
  }
```

- [ ] **Step 2: 切换镜像开关时清结果**

在 `toggleMirror`(约 432 行)函数体最前面加一行 `mirrorTest = null;`。

- [ ] **Step 3: 镜像行 markup 加按钮 + 行内状态**

把镜像行(约 789–802 行)整体替换为:

```svelte
      <div class="row">
        <div class="row-info">
          <span class="row-label">镜像加速</span>
          <span class="row-desc">
            {#if mirrorTest}
              <span class={mirrorTest.ok ? "mtest-ok" : "mtest-err"}>
                {mirrorTest.ok ? `测试成功(${mirrorTest.msg})` : `测试失败: ${mirrorTest.msg}`}
              </span>
            {:else}
              国内网络下载模型更快
            {/if}
          </span>
        </div>
        {#if settings?.mirror_enabled}
          <button class="btn-secondary" onclick={runMirrorTest} disabled={mirrorTesting}>
            {mirrorTesting ? "测试中…" : "测试"}
          </button>
        {/if}
        <input
          type="checkbox"
          class="ctl switch"
          aria-label="使用镜像加速"
          checked={settings?.mirror_enabled ?? false}
          disabled={!settings}
          onchange={toggleMirror}
        />
      </div>
```

- [ ] **Step 4: style 加状态色**

在 `src/routes/settings/+page.svelte` 的 `<style>` 内加:

```css
  .mtest-ok { color: var(--success, var(--ink-secondary)); }
  .mtest-err { color: var(--danger-ink); }
```

- [ ] **Step 5: 类型检查**

Run: `npm run check`
Expected: 0 errors 0 warnings。

- [ ] **Step 6: 手动验证**

`npm run tauri dev` → 设置页:开镜像加速 → 出现「测试」按钮;点它,可达显示绿字「镜像可达(HTTP …)」,把镜像前缀改成不可达地址再测应红字「镜像不可达…」。

- [ ] **Step 7: 提交**

```bash
git add src/routes/settings/+page.svelte
git commit -m "设置页镜像加速加测试按钮:经镜像探已知资源,行内绿/红显示可达或失败原因"
```

---

## Self-Review

- **Spec 覆盖**:大模型测试(Task 1+5)、Agent 测试(Task 2+5)、镜像测试(Task 3+6)、失败归类原因(各后端 probe)、三态内联+改字段清空(Task 5/6)、复用 `test_hook` 模式(全程)、不纳入遥测/MCP(未建任务)——逐项有对应任务。
- **占位符扫描**:无 TBD/TODO;每个改动步骤都给了完整代码与确切文件位置。
- **类型一致**:`probe`/`classify_http_status`/`probe_run`/`probe_mirror` 命名在后端定义与命令调用处一致;`testRefineLlm/testRefineAgent/testMirror` 在 models.ts 定义与 AI/设置页调用处一致;命令名 `test_refine_llm/test_refine_agent/test_mirror` 在 Rust `#[command]`、`generate_handler!`、JS `invoke` 三处一致;JS 参数 camelCase(`baseUrl/apiKey`)对齐 Rust snake_case。
