# AI 助手接入扩展 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给 CLI 加 `record` 录制控制(让 shell-only Agent 经 skill 拥有全能力),并把 WorkBuddy / OpenClaw / Hermes 三家 Agent 接入 MCP 注册器。

**Architecture:** `record` 子命令复用 `bridge::call` 这条 stdio→GUI 的 UDS 客户端(与 MCP server 同源)。注册器把 `Fmt::Json` 改造为携带「server 容器键路径」,现有五家用 `&["mcpServers"]`、OpenClaw 用嵌套 `&["mcp","servers"]`;Hermes 新增 `Fmt::Yaml`。

**Tech Stack:** Rust / Tauri、`serde_json`、`toml_edit`、`serde_yaml_ng`(新增)、`std::os::unix::net`(UDS)。

## Global Constraints

- 只动自己的键(`voice-notes`);解析失败拒写;写前 `write_with_backup`(`.vn.bak` + 权限位保留);幂等。
- 不引入除 `serde_yaml_ng` 外的新依赖;不 shell-out 到任何 Agent 的 CLI。
- 现有五家 AgentDef **配置路径/格式/键名不变**(仅随 `Fmt::Json(path)` 机械带上 `&["mcpServers"]`)。
- CLI 退出码:成功 0;运行时错误(未运行/被门控拒)1;用法错(未知子命令/未知 flag)2。未知 flag 硬报错、不静默。
- 新增条目统一 `{ "command": <当前 exe>, "args": ["mcp","serve"] }`。
- 非目标:`"type":"stdio"` 补写、`CLAUDE_CONFIG_DIR`、shell-out、把 skill 装进新家。
- 每个 Rust 改动后 `cd src-tauri && cargo test` 全绿、`cargo build` 无新增 warning。

---

### Task 1: `Fmt::Json(path)` 重构 —— JSON writer 按键路径下钻

**Files:**
- Modify: `src-tauri/src/mcp/registry.rs`(`Fmt` 枚举 :8-12、五家 `AGENTS` 行 :26-30、`read_command` :98-110、`register`/`unregister` :112-131、`upsert_json` :185-202、`remove_json` :204-215)

**Interfaces:**
- Produces:`Fmt::Json(&'static [&'static str])`(server 容器键路径);`upsert_json(&self, path: &Path, key_path: &[&str])`、`remove_json(&self, path: &Path, key_path: &[&str])`;`read_command` 的 Json 分支按路径下钻。供 Task 2/3 消费。

- [ ] **Step 1: 写失败测试(嵌套路径 upsert/remove/read)**

在 `registry.rs` 的 `#[cfg(test)] mod` 内(文件末尾测试模块)加:
```rust
    #[test]
    fn json_writer_walks_nested_key_path() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let cfg = home.join("nested.json");
        let reg = Registry::with(home.clone(), PathBuf::from("/Applications/voice-notes.app/Contents/MacOS/voice-notes"));

        // 嵌套路径 mcp.servers(OpenClaw 式):首次写建出两级容器
        reg.upsert_json(&cfg, &["mcp", "servers"]).unwrap();
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            v["mcp"]["servers"]["voice-notes"]["command"],
            "/Applications/voice-notes.app/Contents/MacOS/voice-notes"
        );
        assert_eq!(v["mcp"]["servers"]["voice-notes"]["args"][0], "mcp");

        // remove 幂等移除
        reg.remove_json(&cfg, &["mcp", "servers"]).unwrap();
        let v2: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert!(v2["mcp"]["servers"].get("voice-notes").is_none());
        reg.remove_json(&cfg, &["mcp", "servers"]).unwrap(); // 再删不报错
    }
```
（`tempfile` 已是 dev-dependency —— 现有 registry 测试在用；若报未找到,`cd src-tauri && cargo add --dev tempfile`。）

- [ ] **Step 2: 运行确认失败**

Run:
```bash
cd src-tauri && cargo test json_writer_walks_nested_key_path 2>&1 | tail -20
```
Expected: 编译失败 —— `upsert_json` 现签名只有 `&Path`,少一个参数。

- [ ] **Step 3: 改 `Fmt` 枚举**

把 `registry.rs:8-12` 改为:
```rust
/// 配置文件格式。Json 携带 server 容器的键路径(如 &["mcpServers"] 或嵌套 &["mcp","servers"]);
/// Codex 是 TOML 的 [mcp_servers.*];Hermes 是 YAML 的 mcp_servers:。
#[derive(Clone, Copy, PartialEq)]
pub enum Fmt {
    Json(&'static [&'static str]),
    Toml,
    Yaml,
}
```
(`Yaml` 变体本任务先加上占位,Task 3 才用;现在不加它 `register`/`unregister` 的 match 会不全,故本步一并加占位分支——见 Step 6。)

- [ ] **Step 4: 五家 `AGENTS` 行带上键路径**

把 `registry.rs:26-30` 的四个 `Fmt::Json` 改为 `Fmt::Json(&["mcpServers"])`(Codex 的 `Fmt::Toml` 不动):
```rust
    AgentDef { key: "claude-code", name: "Claude Code", detect_rel: ".claude", config_rel: ".claude.json", fmt: Fmt::Json(&["mcpServers"]) },
    AgentDef { key: "claude-desktop", name: "Claude Desktop", detect_rel: "Library/Application Support/Claude", config_rel: "Library/Application Support/Claude/claude_desktop_config.json", fmt: Fmt::Json(&["mcpServers"]) },
    AgentDef { key: "cursor", name: "Cursor", detect_rel: ".cursor", config_rel: ".cursor/mcp.json", fmt: Fmt::Json(&["mcpServers"]) },
    AgentDef { key: "codex", name: "Codex CLI", detect_rel: ".codex", config_rel: ".codex/config.toml", fmt: Fmt::Toml },
    AgentDef { key: "gemini", name: "Gemini CLI", detect_rel: ".gemini", config_rel: ".gemini/settings.json", fmt: Fmt::Json(&["mcpServers"]) },
```

- [ ] **Step 5: `read_command` Json 分支按路径下钻**

把 `registry.rs:100-109` 的 `match def.fmt { ... }` 改为下面这样(Yaml 分支本任务先写 `None` 占位——`serde_yaml_ng` 依赖 Task 3 才加,写真实现会编译失败;Task 3 再替换):
```rust
        match def.fmt {
            Fmt::Json(key_path) => {
                let v: serde_json::Value = serde_json::from_str(&text).ok()?;
                let mut cur = &v;
                for k in key_path {
                    cur = cur.get(*k)?;
                }
                Some(cur.get("voice-notes")?.get("command")?.as_str()?.to_string())
            }
            Fmt::Toml => {
                let doc: toml_edit::DocumentMut = text.parse().ok()?;
                Some(doc.get("mcp_servers")?.get("voice-notes")?.get("command")?.as_str()?.to_string())
            }
            Fmt::Yaml => None, // Task 3 替换为真实现
        }
```

- [ ] **Step 6: `register`/`unregister` 的 match 分支**

把 `register`(:115-118)与 `unregister`(:127-130)的 match 改为:
```rust
        // register:
        match def.fmt {
            Fmt::Json(key_path) => self.upsert_json(&path, key_path),
            Fmt::Toml => self.upsert_toml(&path),
            Fmt::Yaml => self.upsert_yaml(&path),
        }
        // unregister:
        match def.fmt {
            Fmt::Json(key_path) => self.remove_json(&path, key_path),
            Fmt::Toml => self.remove_toml(&path),
            Fmt::Yaml => self.remove_yaml(&path),
        }
```
`upsert_yaml`/`remove_yaml` 本任务先加最小占位(Task 3 实现真逻辑),否则不编译:
```rust
    fn upsert_yaml(&self, _path: &Path) -> anyhow::Result<()> {
        anyhow::bail!("YAML 写入未实现") // Task 3 替换
    }
    fn remove_yaml(&self, _path: &Path) -> anyhow::Result<()> {
        Ok(()) // Task 3 替换
    }
```

- [ ] **Step 7: 改 `upsert_json` / `remove_json` 按路径下钻**

`upsert_json`(:185-202)整体替换为:
```rust
    fn upsert_json(&self, path: &Path, key_path: &[&str]) -> anyhow::Result<()> {
        let mut root: serde_json::Value = match std::fs::read_to_string(path) {
            Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text).map_err(|e| {
                anyhow::anyhow!("{} 不是合法 JSON,拒绝写入(请手动修复或手动配置): {e}", path.display())
            })?,
            _ => serde_json::json!({}),
        };
        // 逐级下钻到 server 容器,缺失级建对象;任一级非对象则拒写。
        let mut cur = root
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("{} 顶层不是对象,拒绝写入", path.display()))?;
        for k in key_path {
            cur = cur
                .entry(*k)
                .or_insert_with(|| serde_json::json!({}))
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("{} 的键 {k} 不是对象,拒绝写入", path.display()))?;
        }
        cur.insert(
            "voice-notes".into(),
            serde_json::json!({ "command": self.exe.to_string_lossy(), "args": ["mcp", "serve"] }),
        );
        write_with_backup(path, &(serde_json::to_string_pretty(&root)? + "\n"))
    }
```
`remove_json`(:204-215)整体替换为:
```rust
    fn remove_json(&self, path: &Path, key_path: &[&str]) -> anyhow::Result<()> {
        let text = std::fs::read_to_string(path)?;
        let mut root: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("{} 不是合法 JSON,拒绝写入: {e}", path.display()))?;
        // 逐级下钻;任一级缺失即视为无条目(幂等)。
        let mut cur = &mut root;
        for k in key_path {
            let Some(next) = cur.get_mut(*k) else {
                return Ok(());
            };
            cur = next;
        }
        let Some(servers) = cur.as_object_mut() else {
            return Ok(());
        };
        if servers.remove("voice-notes").is_none() {
            return Ok(());
        }
        write_with_backup(path, &(serde_json::to_string_pretty(&root)? + "\n"))
    }
```

- [ ] **Step 8: 全量测试(新测试 + 现有五家回归)**

Run:
```bash
cd src-tauri && cargo test registry 2>&1 | tail -25 && cargo test json_writer_walks_nested_key_path 2>&1 | tail -8
```
Expected: 新测试 PASS;现有 registry 测试(顶层 `mcpServers` 路径、权限位、坏文件拒写、Codex TOML)全部仍 PASS。

- [ ] **Step 9: 编译**

Run:
```bash
cd src-tauri && cargo build 2>&1 | tail -10
```
Expected: 成功,无新 warning(占位 `upsert_yaml` 的 `bail!` 不产生 warning)。

- [ ] **Step 10: 提交**

```bash
git add src-tauri/src/mcp/registry.rs
git commit -m "refactor(mcp): Fmt::Json 携带键路径,JSON writer 按路径下钻"
```

---

### Task 2: WorkBuddy + OpenClaw 接入(JSON 家族)

**Files:**
- Modify: `src-tauri/src/mcp/registry.rs`(`AGENTS` 数组加两行 + 测试)

**Interfaces:**
- Consumes:Task 1 的 `Fmt::Json(&[...])` + 按路径下钻的 writer。
- Produces:`AGENTS` 含 `workbuddy`、`openclaw` 两 key。

- [ ] **Step 1: 写失败测试**

在 registry 测试模块加:
```rust
    #[test]
    fn workbuddy_registers_top_level() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        std::fs::create_dir_all(home.join(".workbuddy")).unwrap();
        let reg = Registry::with(home.clone(), PathBuf::from("/Applications/voice-notes.app/Contents/MacOS/voice-notes"));
        reg.register("workbuddy").unwrap();
        let cfg = home.join(".workbuddy/mcp.json");
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["voice-notes"]["args"][1], "serve");
        let st = reg.status().into_iter().find(|s| s.key == "workbuddy").unwrap();
        assert!(st.installed && st.registered && !st.stale);
        reg.unregister("workbuddy").unwrap();
        let v2: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert!(v2["mcpServers"].get("voice-notes").is_none());
    }

    #[test]
    fn openclaw_registers_nested_and_rejects_json5_comments() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        std::fs::create_dir_all(home.join(".openclaw")).unwrap();
        let reg = Registry::with(home.clone(), PathBuf::from("/Applications/voice-notes.app/Contents/MacOS/voice-notes"));
        let cfg = home.join(".openclaw/openclaw.json");

        reg.register("openclaw").unwrap();
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(v["mcp"]["servers"]["voice-notes"]["command"], "/Applications/voice-notes.app/Contents/MacOS/voice-notes");
        assert_eq!(reg.status().into_iter().find(|s| s.key == "openclaw").unwrap().registered, true);

        // JSON5 注释文件:拒写不损坏(保留原文)
        let commented = "{\n  // 我的配置\n  \"mcp\": { \"servers\": {} }\n}\n";
        std::fs::write(&cfg, commented).unwrap();
        assert!(reg.register("openclaw").is_err(), "带注释的 JSON5 应拒写");
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), commented, "拒写后原文不变");
    }
```

- [ ] **Step 2: 运行确认失败**

Run:
```bash
cd src-tauri && cargo test -- workbuddy_registers_top_level openclaw_registers_nested 2>&1 | tail -15
```
Expected: FAIL —— `register("workbuddy")` / `register("openclaw")` 报「未知 Agent」(AGENTS 里还没有)。

- [ ] **Step 3: 加两行 AgentDef**

在 `AGENTS` 数组(Gemini 行之后)加:
```rust
    AgentDef { key: "workbuddy", name: "WorkBuddy", detect_rel: ".workbuddy", config_rel: ".workbuddy/mcp.json", fmt: Fmt::Json(&["mcpServers"]) },
    AgentDef { key: "openclaw", name: "OpenClaw", detect_rel: ".openclaw", config_rel: ".openclaw/openclaw.json", fmt: Fmt::Json(&["mcp", "servers"]) },
```

- [ ] **Step 4: 运行确认通过**

Run:
```bash
cd src-tauri && cargo test -- workbuddy_registers_top_level openclaw_registers_nested_and_rejects_json5_comments 2>&1 | tail -10
```
Expected: 两测试 PASS。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/mcp/registry.rs
git commit -m "feat(mcp): 接入 WorkBuddy(顶层 JSON)+ OpenClaw(嵌套 mcp.servers)"
```

---

### Task 3: Hermes 接入(`Fmt::Yaml`)

**Files:**
- Modify: `src-tauri/Cargo.toml`(加 `serde_yaml_ng` 依赖)、`src-tauri/src/mcp/registry.rs`(实现 `upsert_yaml`/`remove_yaml`、替换 `read_command` Yaml 分支、加 Hermes 行 + 测试)

**Interfaces:**
- Consumes:Task 1 的 `Fmt::Yaml` 分支 + 占位 `upsert_yaml`/`remove_yaml`。
- Produces:`AGENTS` 含 `hermes`;YAML writer 真实现。

- [ ] **Step 1: 加 YAML 依赖**

Run:
```bash
cd src-tauri && cargo add serde_yaml_ng
```
Expected: `Cargo.toml` 出现 `serde_yaml_ng`(0.10 系),编译索引更新。

- [ ] **Step 2: 写失败测试**

在 registry 测试模块加:
```rust
    #[test]
    fn hermes_registers_yaml_preserving_other_servers() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        std::fs::create_dir_all(home.join(".hermes")).unwrap();
        let reg = Registry::with(home.clone(), PathBuf::from("/Applications/voice-notes.app/Contents/MacOS/voice-notes"));
        let cfg = home.join(".hermes/config.yaml");
        // 预置一个已有 server,断言不被动
        std::fs::write(&cfg, "mcp_servers:\n  github:\n    command: npx\n    args: [\"-y\", \"srv\"]\n").unwrap();

        reg.register("hermes").unwrap();
        let v: serde_yaml_ng::Value = serde_yaml_ng::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(v["mcp_servers"]["voice-notes"]["command"].as_str().unwrap(), "/Applications/voice-notes.app/Contents/MacOS/voice-notes");
        assert_eq!(v["mcp_servers"]["voice-notes"]["args"][0].as_str().unwrap(), "mcp");
        assert_eq!(v["mcp_servers"]["github"]["command"].as_str().unwrap(), "npx", "既有 server 保留");
        assert_eq!(reg.read_command(Registry::def("hermes").unwrap()).as_deref(), Some("/Applications/voice-notes.app/Contents/MacOS/voice-notes"));

        reg.unregister("hermes").unwrap();
        let v2: serde_yaml_ng::Value = serde_yaml_ng::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert!(v2["mcp_servers"].get("voice-notes").is_none());
        assert_eq!(v2["mcp_servers"]["github"]["command"].as_str().unwrap(), "npx", "移除 voice-notes 不动其它");
    }
```

- [ ] **Step 3: 运行确认失败**

Run:
```bash
cd src-tauri && cargo test hermes_registers_yaml 2>&1 | tail -15
```
Expected: FAIL —— `register("hermes")` 走占位 `upsert_yaml` 的 `bail!("YAML 写入未实现")`。

- [ ] **Step 4: 实现 YAML writer + 替换 read_command Yaml 分支**

把 Task 1 加的占位 `upsert_yaml`/`remove_yaml` 替换为:
```rust
    fn upsert_yaml(&self, path: &Path) -> anyhow::Result<()> {
        use serde_yaml_ng::Value;
        let mut root: Value = match std::fs::read_to_string(path) {
            Ok(text) if !text.trim().is_empty() => serde_yaml_ng::from_str(&text).map_err(|e| {
                anyhow::anyhow!("{} 不是合法 YAML,拒绝写入(请手动修复或手动配置): {e}", path.display())
            })?,
            _ => Value::Mapping(Default::default()),
        };
        let map = root
            .as_mapping_mut()
            .ok_or_else(|| anyhow::anyhow!("{} 顶层不是映射,拒绝写入", path.display()))?;
        let servers = map
            .entry(Value::String("mcp_servers".into()))
            .or_insert_with(|| Value::Mapping(Default::default()));
        let servers = servers
            .as_mapping_mut()
            .ok_or_else(|| anyhow::anyhow!("{} 的 mcp_servers 不是映射,拒绝写入", path.display()))?;
        let mut entry = serde_yaml_ng::Mapping::new();
        entry.insert(Value::String("command".into()), Value::String(self.exe.to_string_lossy().into_owned()));
        entry.insert(
            Value::String("args".into()),
            Value::Sequence(vec![Value::String("mcp".into()), Value::String("serve".into())]),
        );
        servers.insert(Value::String("voice-notes".into()), Value::Mapping(entry));
        write_with_backup(path, &serde_yaml_ng::to_string(&root)?)
    }

    fn remove_yaml(&self, path: &Path) -> anyhow::Result<()> {
        use serde_yaml_ng::Value;
        let text = std::fs::read_to_string(path)?;
        let mut root: Value = serde_yaml_ng::from_str(&text)
            .map_err(|e| anyhow::anyhow!("{} 不是合法 YAML,拒绝写入: {e}", path.display()))?;
        let Some(servers) = root.get_mut("mcp_servers").and_then(|v| v.as_mapping_mut()) else {
            return Ok(());
        };
        if servers.remove(Value::String("voice-notes".into())).is_none() {
            return Ok(());
        }
        write_with_backup(path, &serde_yaml_ng::to_string(&root)?)
    }
```
并把 Task 1 Step 5 里 `Fmt::Yaml => None,` 换成真实现:
```rust
            Fmt::Yaml => {
                let v: serde_yaml_ng::Value = serde_yaml_ng::from_str(&text).ok()?;
                Some(v.get("mcp_servers")?.get("voice-notes")?.get("command")?.as_str()?.to_string())
            }
```

- [ ] **Step 5: 加 Hermes 行**

在 `AGENTS` 数组(OpenClaw 行之后)加:
```rust
    AgentDef { key: "hermes", name: "Hermes Agent", detect_rel: ".hermes", config_rel: ".hermes/config.yaml", fmt: Fmt::Yaml },
```

- [ ] **Step 6: 运行确认通过 + 全量回归**

Run:
```bash
cd src-tauri && cargo test hermes_registers_yaml 2>&1 | tail -8 && cargo test registry 2>&1 | tail -15
```
Expected: Hermes 测试 PASS;全部 registry 测试仍 PASS。

- [ ] **Step 7: 编译无新 warning**

Run:
```bash
cd src-tauri && cargo build 2>&1 | tail -8
```
Expected: 成功,无新 warning。

- [ ] **Step 8: 提交**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/mcp/registry.rs
git commit -m "feat(mcp): 接入 Hermes(Fmt::Yaml + serde_yaml_ng)"
```

---

### Task 4: CLI `record` 录制控制

**Files:**
- Create: `src-tauri/src/mcp/cli_control.rs`
- Modify: `src-tauri/src/mcp/mod.rs`(`mod cli_control;` + `cli_entry` 分发 + 词表注释)、`src-tauri/src/main.rs`(拦截词表加 `record`)

**Interfaces:**
- Consumes:`super::bridge::call(op: &str, extra: serde_json::Value) -> Result<serde_json::Value, String>`(现有)。
- Produces:`pub fn record_cli(args: &[String]) -> i32`。

- [ ] **Step 1: 写失败测试(参数解析纯逻辑 + 用法错退出码)**

新建 `src-tauri/src/mcp/cli_control.rs`,先只放测试骨架 + 待实现签名:
```rust
//! record 控制 CLI:start/stop/pause/resume/status/live 经 super::bridge::call 打到运行中的 GUI。

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_helpers() {
        let args = vec!["--title".to_string(), "评审会".to_string(), "--json".to_string()];
        assert_eq!(flag_value(&args, "--title").as_deref(), Some("评审会"));
        assert_eq!(flag_value(&args, "--tail"), None);
        assert!(reject_unknown(&args, &["--title", "--json"]).is_ok());
        assert!(reject_unknown(&args, &["--json"]).is_err(), "未知 --title 应报错");
    }

    #[test]
    fn usage_errors_exit_2_without_touching_bridge() {
        assert_eq!(record_cli(&["bogus".into()]), 2, "未知子命令");
        assert_eq!(record_cli(&[]), 2, "缺子命令");
        assert_eq!(record_cli(&["start".into(), "--nope".into()]), 2, "未知 flag");
        assert_eq!(record_cli(&["live".into(), "--tail".into(), "abc".into()]), 2, "--tail 非整数");
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run:
```bash
cd src-tauri && cargo test cli_control 2>&1 | tail -15
```
Expected: 编译失败(`record_cli`/`flag_value`/`reject_unknown` 未定义)。

- [ ] **Step 3: 实现 `cli_control.rs`**

在测试模块之上写:
```rust
use super::bridge;

const USAGE: &str = "用法: voice-notes record <start|stop|pause|resume|status|live> [选项]\n  \
start [--title 标题] | stop | pause | resume | status | live [--tail N]\n  通用: --json 输出原始 JSON";

/// 取 `--flag 值` 的值(未出现返回 None)。
fn flag_value(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

/// 拒绝未知 flag(以 -- 开头且不在白名单;白名单里带值的 flag 其值不算 flag)。
fn reject_unknown(args: &[String], allowed: &[&str]) -> Result<(), String> {
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a.starts_with("--") {
            if !allowed.contains(&a.as_str()) {
                return Err(format!("未知选项 {a}"));
            }
            // 带值 flag(--title/--tail)跳过其值
            if a == "--title" || a == "--tail" {
                i += 1;
            }
        }
        i += 1;
    }
    Ok(())
}

fn usage_err(msg: &str) -> i32 {
    eprintln!("{msg}\n{USAGE}");
    2
}

/// 人读渲染:status 显状态,start/stop 显 note_id,其余回退紧凑 JSON。
fn render_human(sub: &str, data: &serde_json::Value) -> String {
    match sub {
        "status" => format!(
            "状态: {} | note: {} | 时长: {}ms\n",
            data.get("state").and_then(|v| v.as_str()).unwrap_or("?"),
            data.get("note_id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).unwrap_or("-"),
            data.get("elapsed_ms").and_then(|v| v.as_u64()).unwrap_or(0),
        ),
        "start" | "stop" => format!(
            "note_id: {}\n",
            data.get("note_id").and_then(|v| v.as_str()).unwrap_or("-"),
        ),
        _ => format!("{data}\n"),
    }
}

pub fn record_cli(args: &[String]) -> i32 {
    let Some((sub, rest)) = args.split_first() else {
        eprintln!("{USAGE}");
        return 2;
    };
    let json_out = rest.iter().any(|a| a == "--json");
    let result = match sub.as_str() {
        "start" => {
            if let Err(m) = reject_unknown(rest, &["--title", "--json"]) {
                return usage_err(&m);
            }
            let extra = match flag_value(rest, "--title") {
                Some(t) => serde_json::json!({ "title": t }),
                None => serde_json::json!({}),
            };
            bridge::call("start", extra)
        }
        "stop" | "pause" | "resume" | "status" => {
            if let Err(m) = reject_unknown(rest, &["--json"]) {
                return usage_err(&m);
            }
            bridge::call(sub, serde_json::json!({}))
        }
        "live" => {
            if let Err(m) = reject_unknown(rest, &["--tail", "--json"]) {
                return usage_err(&m);
            }
            let tail = match flag_value(rest, "--tail") {
                Some(v) => match v.parse::<u64>() {
                    Ok(n) => n,
                    Err(_) => return usage_err("--tail 需要整数"),
                },
                None => 20,
            };
            bridge::call("live", serde_json::json!({ "tail": tail }))
        }
        _ => {
            eprintln!("{USAGE}");
            return 2;
        }
    };
    match result {
        Ok(data) => {
            if json_out {
                println!("{}", serde_json::to_string_pretty(&data).unwrap_or_default());
            } else {
                print!("{}", render_human(sub, &data));
            }
            0
        }
        Err(msg) => {
            eprintln!("{msg}");
            1
        }
    }
}
```

- [ ] **Step 4: 接线到 cli_entry + main.rs**

`src-tauri/src/mcp/mod.rs`:在 `mod` 声明区加 `mod cli_control;`;把 `cli_entry` 的 match(:31-34)加一臂:
```rust
        "record" => cli_control::record_cli(&args[1..]),
```
并把该文件里「CLI 词表与 mcp::cli_entry 的分发表一一对应」相关注释里的词表补上 `record`(若有列举)。

`src-tauri/src/main.rs:12`:把
```rust
        Some("mcp" | "notes" | "speakers" | "skill")
```
改为
```rust
        Some("mcp" | "notes" | "speakers" | "skill" | "record")
```

- [ ] **Step 5: 运行确认通过**

Run:
```bash
cd src-tauri && cargo test cli_control 2>&1 | tail -10
```
Expected: `parse_helpers`、`usage_errors_exit_2_without_touching_bridge` PASS。
(注:`usage_errors...` 用例全在 bridge 调用前返回 2,不触网/不连 socket,无 flake。)

- [ ] **Step 6: 编译**

Run:
```bash
cd src-tauri && cargo build 2>&1 | tail -8
```
Expected: 成功,无新 warning。

- [ ] **Step 7: 提交**

```bash
git add src-tauri/src/mcp/cli_control.rs src-tauri/src/mcp/mod.rs src-tauri/src/main.rs
git commit -m "feat(cli): record 录制控制子命令(复用 UDS 桥)"
```

---

### Task 5: 文档 —— skill 控制段 + README 八家 + 前端文案

**Files:**
- Modify: `src-tauri/src/mcp/skill_template.md`、`README.md`、`README.en.md`
- 核对:`src/routes/record/+page.svelte`、`src/routes/settings/+page.svelte`、`src/lib/WelcomeOverlay.svelte`、`src/lib/mcp.ts`(前端是否硬编码代理数量/名单)

**Interfaces:** 无(文档 + 文案)。

- [ ] **Step 1: skill 模板加「控制录制(CLI)」**

在 `src-tauri/src/mcp/skill_template.md` 的「## 工具与降级路径」代码块(现有 `{{BINARY}} notes ...` 那段)之后、「需要原始逐字稿时加 --raw」之前,插入:
```markdown

控制录制(需 App 运行;`start/stop/pause/resume` 还需用户在设置开启「允许 AI 控制录制」):

    {{BINARY}} record status
    {{BINARY}} record start --title "评审会"
    {{BINARY}} record stop
    {{BINARY}} record live --tail 20

被门控拒绝或 App 未运行时命令会返回指引原文,把它转告用户、不要自行重试。
```

- [ ] **Step 2: README(中/英)改「五家」→「八家」+ 补代理 + 控制命令**

在 `README.md` 与 `README.en.md`:
1. 把「五家」/「five agents」等数量表述改为「八家」/「eight agents」。
2. 在支持的 Agent 列表补 `WorkBuddy`、`OpenClaw`、`Hermes Agent`(中文名/英文名各自对应)。
3. 在「命令行直查」小节补录制控制示例(与 skill 一致的四条 `record` 命令)。
（实现者用 `grep -n "五家\|five\|Gemini" README.md README.en.md` 定位需改处。)

- [ ] **Step 3: 前端硬编码核对**

Run:
```bash
cd /Users/teemo/workspace-soul/voice-notes && grep -rn "五家\|5 家\|Gemini\|claude-desktop\|接入" src/ | grep -iv "node_modules"
```
若命中**硬编码的代理数量或写死的五家名单**(而非从后端 `status()` 渲染),把相关文案/列表改为不写死数量或补齐八家;若前端纯粹遍历后端返回的列表(无硬编码),则无需改,记录「已核对,前端由 AGENTS 驱动」。

- [ ] **Step 4: 类型检查**

Run:
```bash
npm run check
```
Expected: 0 error / 0 warning。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/mcp/skill_template.md README.md README.en.md src/
git commit -m "docs(mcp): skill 控制命令 + README 八家 + 前端文案核对"
```

---

## 收尾(控制器执行)

- [ ] `cd src-tauri && cargo test` 全绿;`npm run check` 0/0;`cargo build` 无新 warning。
- [ ] `git log --oneline` 复核五个提交。
- [ ] **真机冒烟(macOS,发版前)**:对本机装了的新家 `voice-notes mcp register --agent workbuddy|openclaw|hermes` → 打开对应工具确认 voice-notes 在其 MCP 列表;`voice-notes record status`(App 运行)→ 返回状态;开「允许 AI 控制录制」后 `record start`/`record stop` → 驱动录制。清理:`voice-notes mcp unregister --agent <key>`。
- [ ] 推分支 `agent-access-expansion` → 开 PR(用户冒烟后 squash 合入 master)。
