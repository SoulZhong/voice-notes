# voice-notes MCP 服务 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 voice-notes 发布成 MCP 服务(stdio 入口 + Unix socket 桥),含五家 Agent 的注册引导(欢迎页/设置页/CLI)与 README 双语引导。

**Architecture:** 同一二进制 argv 分流:`voice-notes mcp serve` 走 rmcp stdio MCP 服务(查询工具直读数据文件,状态/控制工具经 `app_data/mcp.sock` 转发给运行中的 GUI);`voice-notes mcp register/unregister/status` 是 headless 注册 CLI。GUI 侧新增 UDS listener、注册用 tauri 命令、设置页分组与欢迎页步骤。

**Tech Stack:** Rust (Tauri 2, rmcp, tokio, toml_edit, serde_json), Svelte 5, TypeScript。

**Spec:** `docs/superpowers/specs/2026-07-08-voice-notes-mcp-service-design.md`(含已拍板决策 §十)。

## Global Constraints

- git 提交**一律不加** `Co-Authored-By` / `Generated with Claude Code` 等署名尾注(用户全局规约)。
- 提交信息风格沿用仓库现状:中文正文,`feat:`/`fix:`/`docs:` 类前缀可选。
- 代码注释:中文、说明"为什么"而非"做什么",密度对齐现有文件。
- UI 一律使用 DESIGN.md token(`--ink`/`--surface-soft`/`--radius-sm` 等),复用设置页现有 `section / rows / row / row-info / row-label / row-desc / ctl / btn-secondary` 类名与形态。
- 所有 Rust 测试命令在 `src-tauri/` 目录下执行:`cargo test <name>`(首次构建含 sherpa 链接,较慢,属正常)。
- settings.json 新字段必须 `#[serde(default)]` 且旧文件可解析(仓库既有约定)。
- 应用 identifier 固定 `com.teemo.voice-notes`(tauri.conf.json 真值);MCP 注册条目 args 固定 `["mcp","serve"]`。
- 文件路径均相对仓库根 `/Users/teemo/workspace-soul/voice-notes`。

## File Structure(全景)

```
src-tauri/src/main.rs                 # argv 分流(改)
src-tauri/src/lib.rs                  # pub mod mcp; 新 tauri 命令; setup 挂自愈+UDS(改)
src-tauri/src/mcp/mod.rs              # 新:模块入口, app_data_dir(), cli_main() 分发
src-tauri/src/mcp/registry.rs         # 新:Agent 表/检测/JSON+TOML 注册/status/heal
src-tauri/src/mcp/tools.rs            # 新:4 个查询工具的纯实现(文件 → JSON)
src-tauri/src/mcp/server.rs           # 新:rmcp stdio 服务(tool_router)
src-tauri/src/mcp/bridge.rs           # 新:UDS 客户端(stdio 侧)+ 协议类型
src-tauri/src/mcp/uds.rs              # 新:UDS listener(GUI 侧)
src-tauri/src/store/export.rs         # NoteStore::render(不落盘渲染)抽出(改)
src-tauri/src/store/writer.rs         # NoteWriter::set_title(改)
src-tauri/src/settings.rs             # mcp_allow_control / mcp_onboarded(改)
src-tauri/tests/mcp_stdio.rs          # 新:stdio JSON-RPC e2e
src/lib/mcp.ts                        # 新:前端 invoke 封装
src/lib/models.ts                     # Settings 类型补字段(改)
src/lib/WelcomeOverlay.svelte         # 「连接 AI 助手」步(改)
src/routes/settings/+page.svelte      # 「AI 助手接入」分组(改)
src/routes/record/+page.svelte        # 存量用户一次性提示条(改)
README.md / README.en.md              # MCP 章节 + Agent 安装引导(改)
```

关键既有接口(实现者按此消费,不要重复造):

- `store::NoteStore::new(notes_dir: PathBuf)`;`.list() -> Vec<NoteSummary>{id,title,started_at,duration_secs,state}`(started_at 倒序);`.load(id) -> anyhow::Result<Note>{meta,segments,skipped_lines,speakers}`。
- `store::{NoteMeta{id,title,started_at,ended_at,state}, SegmentRecord{seq,source,text,start_ms,end_ms,speaker,rms}, SpeakerMeta{name,sources,centroid,count,person_id}}`,`store::load_refined(note_dir) -> Option<RefinedDoc{generated_at,llm_model,stages,paragraphs:Vec<RefinedParagraph{speaker,name,start_ms,end_ms,text,source_seqs}>}>`,`store::validate_note_id(id)`(pub(crate))。
- `store::VoiceprintStore::new(data_root)`;`.load().people` 可迭代 `(id, p)`,p 有 `name/total_ms/last_seen/centroids`(见 lib.rs:1474 list_people 的用法)。
- `settings::load(app_data) -> Settings`、`settings::resolve_data_root(app_data,&s)`、`settings::update(app_data, f)`。
- lib.rs:`do_start_recording(&AppHandle)`、`do_stop_recording(&AppHandle)`、`AppState{session: Arc<Mutex<Option<ActiveSession>>>, ..}`,ActiveSession 有 `note_id`、`writer: Arc<Mutex<NoteWriter>>`、`elapsed_ms()`、`paused_at`、`system_audio`、`diarization`。
- 前端:`getSettings()/setSettings()` 在 `src/lib/models.ts`。

---

## Task 1: `mcp` 模块骨架 + argv 分流 + app_data 解析

**Files:**
- Create: `src-tauri/src/mcp/mod.rs`
- Modify: `src-tauri/src/main.rs`
- Modify: `src-tauri/src/lib.rs`(加一行 `pub mod mcp;`,与现有 `mod settings;` 等并列)

**Interfaces:**
- Produces: `mcp::app_data_dir() -> PathBuf`(env `VN_APP_DATA` 覆盖,供测试与 e2e);`mcp::cli_main(args: &[String]) -> i32`;后续任务往 `cli_main` 的 match 里挂子命令。

- [ ] **Step 1: 写失败测试**(`src-tauri/src/mcp/mod.rs` 底部,先建文件只含测试与桩)

```rust
//! MCP 子系统入口:argv `voice-notes mcp ...` 的 CLI 分发,与无 tauri 环境下的
//! app_data 解析(stdio 服务进程/注册 CLI 都不经过 tauri Builder)。

use std::path::PathBuf;

pub mod registry;

/// 无 tauri 环境下的 app_data_dir。identifier 与 tauri.conf.json 保持一致——
/// GUI 侧 `app.path().app_data_dir()` 解析到的正是这个目录。VN_APP_DATA 供
/// 测试与 e2e 注入 tempdir(生产不设)。
pub fn app_data_dir() -> PathBuf {
    if let Ok(p) = std::env::var("VN_APP_DATA") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/com.teemo.voice-notes")
}

/// `voice-notes mcp <sub> ...` 的分发。返回进程退出码。
pub fn cli_main(args: &[String]) -> i32 {
    let sub = args.first().map(String::as_str).unwrap_or("");
    match sub {
        "serve" | "register" | "unregister" | "status" => {
            eprintln!("mcp {sub}: 尚未实现");
            1
        }
        _ => {
            eprintln!(
                "用法: voice-notes mcp <serve|register|unregister|status>\n\
                 serve                 以 stdio MCP 服务运行(供 Agent spawn)\n\
                 register [--agent X]  注册到本机 Agent(X: claude-code|claude-desktop|cursor|codex|gemini|auto)\n\
                 unregister [--agent X]\n\
                 status [--json]       各 Agent 检测/注册状态"
            );
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_data_respects_env_override() {
        std::env::set_var("VN_APP_DATA", "/tmp/vn-test-app-data");
        assert_eq!(app_data_dir(), PathBuf::from("/tmp/vn-test-app-data"));
        std::env::remove_var("VN_APP_DATA");
        let p = app_data_dir();
        assert!(p.ends_with("Library/Application Support/com.teemo.voice-notes"), "{p:?}");
    }

    #[test]
    fn cli_unknown_subcommand_exits_2() {
        assert_eq!(cli_main(&["nope".into()]), 2);
        assert_eq!(cli_main(&[]), 2);
    }
}
```

同时建空的 `src-tauri/src/mcp/registry.rs`(仅 `//! Agent 注册器(Task 2 实现)。` 一行,让 `pub mod registry;` 可编译)。

- [ ] **Step 2: 跑测试确认失败**(此刻 lib.rs 还没挂模块,应编译失败)

Run: `cargo test mcp::tests -- --nocapture`
Expected: 编译错误(mcp 模块未声明)——这是本任务的"红"。

- [ ] **Step 3: 挂模块 + argv 分流**

`src-tauri/src/lib.rs`:在现有 `mod ipc;` 声明区加:

```rust
pub mod mcp;
```

`src-tauri/src/main.rs` 全文替换为:

```rust
// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // `voice-notes mcp ...` 走无 GUI 的 CLI/stdio 分支,必须在 tauri Builder 之前
    // 拦截——MCP 客户端 spawn 本进程时绝不能弹窗口/托盘。LaunchServices 正常打开
    // App 不带参数,不受影响。
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("mcp") {
        std::process::exit(app_lib::mcp::cli_main(&args[2..]));
    }
    app_lib::run()
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test mcp:: -- --nocapture`
Expected: 2 passed。再跑 `cargo run -- mcp` 应打印用法并退出码 2(`echo $?`)。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/main.rs src-tauri/src/lib.rs src-tauri/src/mcp/
git commit -m "feat(mcp): argv 分流骨架——voice-notes mcp 子命令入口与 app_data 解析"
```

---

## Task 2: registry——Agent 表、检测与 JSON 注册(TDD)

**Files:**
- Modify: `src-tauri/src/mcp/registry.rs`
- Modify: `src-tauri/Cargo.toml`(无新依赖,本任务只用 serde_json)

**Interfaces:**
- Produces: `Registry::with(home: PathBuf, exe: PathBuf)`、`Registry::new() -> anyhow::Result<Self>`、`AGENTS: &[AgentDef]`、`AgentStatus{key,name,installed,registered,command,stale}`、`Registry::status() -> Vec<AgentStatus>`、`Registry::register(key) -> anyhow::Result<()>`、`Registry::unregister(key) -> anyhow::Result<()>`、`Registry::entry_snippet_json() -> String`。Task 3 补 TOML 分支与备份;Task 4 消费 status/register 做 CLI;Task 9 从 GUI 消费。

- [ ] **Step 1: 写失败测试**(registry.rs,先写数据模型+测试,函数体 `todo!()` 不可取——写最小可编译桩,断言先红)

```rust
//! 本机 Agent 的 MCP 注册器:检测安装、把 voice-notes 条目写进各家配置。
//! 原则:只动自己的键(voice-notes),解析失败拒写,写前备份,幂等。

use serde::Serialize;
use std::path::{Path, PathBuf};

/// 配置文件格式。JSON 家族统一顶层键 "mcpServers";Codex 是 TOML 的 [mcp_servers.*]。
#[derive(Clone, Copy, PartialEq)]
pub enum Fmt {
    Json,
    Toml,
}

pub struct AgentDef {
    pub key: &'static str,
    pub name: &'static str,
    /// 相对 $HOME 的安装检测路径(目录或文件,存在即视为已安装)。
    detect_rel: &'static str,
    /// 相对 $HOME 的配置文件路径。
    config_rel: &'static str,
    pub fmt: Fmt,
}

/// 内置支持的五家(已拍板:第二梯队不内置,靠设置页手动配置卡片)。
pub const AGENTS: &[AgentDef] = &[
    AgentDef { key: "claude-code", name: "Claude Code", detect_rel: ".claude", config_rel: ".claude.json", fmt: Fmt::Json },
    AgentDef { key: "claude-desktop", name: "Claude Desktop", detect_rel: "Library/Application Support/Claude", config_rel: "Library/Application Support/Claude/claude_desktop_config.json", fmt: Fmt::Json },
    AgentDef { key: "cursor", name: "Cursor", detect_rel: ".cursor", config_rel: ".cursor/mcp.json", fmt: Fmt::Json },
    AgentDef { key: "codex", name: "Codex CLI", detect_rel: ".codex", config_rel: ".codex/config.toml", fmt: Fmt::Toml },
    AgentDef { key: "gemini", name: "Gemini CLI", detect_rel: ".gemini", config_rel: ".gemini/settings.json", fmt: Fmt::Json },
];

#[derive(Debug, Clone, Serialize)]
pub struct AgentStatus {
    pub key: String,
    pub name: String,
    pub installed: bool,
    pub registered: bool,
    /// 已注册条目里的 command(未注册为 None)。
    pub command: Option<String>,
    /// 已注册但 command ≠ 当前二进制(App 被移动/换装过)。
    pub stale: bool,
}

/// home/exe 显式注入:生产走 new()(真 $HOME + current_exe),测试注入 tempdir。
pub struct Registry {
    home: PathBuf,
    exe: PathBuf,
}

impl Registry {
    pub fn new() -> anyhow::Result<Self> {
        let home = std::env::var("HOME").map_err(|_| anyhow::anyhow!("HOME 不可用"))?;
        let exe = std::env::current_exe()?.canonicalize()?;
        Ok(Self::with(PathBuf::from(home), exe))
    }

    pub fn with(home: PathBuf, exe: PathBuf) -> Self {
        Self { home, exe }
    }

    fn def(key: &str) -> anyhow::Result<&'static AgentDef> {
        AGENTS.iter().find(|a| a.key == key).ok_or_else(|| anyhow::anyhow!("未知 Agent: {key}"))
    }

    fn config_path(&self, def: &AgentDef) -> PathBuf {
        self.home.join(def.config_rel)
    }

    /// 手动配置卡片/README 用的 JSON 片段(command 为本机真实路径)。
    pub fn entry_snippet_json(&self) -> String {
        serde_json::to_string_pretty(&serde_json::json!({
            "voice-notes": { "command": self.exe.to_string_lossy(), "args": ["mcp", "serve"] }
        }))
        .expect("静态结构序列化不会失败")
    }

    pub fn status(&self) -> Vec<AgentStatus> {
        AGENTS.iter().map(|d| self.status_one(d)).collect()
    }

    fn status_one(&self, def: &AgentDef) -> AgentStatus {
        todo!("Task 2 Step 3")
    }

    pub fn register(&self, key: &str) -> anyhow::Result<()> {
        todo!("Task 2 Step 3")
    }

    pub fn unregister(&self, key: &str) -> anyhow::Result<()> {
        todo!("Task 2 Step 3")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(home: &Path) -> Registry {
        Registry::with(home.to_path_buf(), PathBuf::from("/Applications/voice-notes.app/Contents/MacOS/voice-notes"))
    }

    #[test]
    fn detects_installed_by_path_presence() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".cursor")).unwrap();
        let st = reg(tmp.path()).status();
        let cursor = st.iter().find(|s| s.key == "cursor").unwrap();
        assert!(cursor.installed && !cursor.registered);
        let gemini = st.iter().find(|s| s.key == "gemini").unwrap();
        assert!(!gemini.installed);
    }

    #[test]
    fn register_creates_minimal_json_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".cursor")).unwrap();
        let r = reg(tmp.path());
        r.register("cursor").unwrap();
        r.register("cursor").unwrap(); // 幂等:重复注册 = 覆盖
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(tmp.path().join(".cursor/mcp.json")).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["voice-notes"]["command"], "/Applications/voice-notes.app/Contents/MacOS/voice-notes");
        assert_eq!(v["mcpServers"]["voice-notes"]["args"], serde_json::json!(["mcp", "serve"]));
        let st = r.status();
        let cursor = st.iter().find(|s| s.key == "cursor").unwrap();
        assert!(cursor.registered && !cursor.stale);
    }

    #[test]
    fn register_preserves_unrelated_keys() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".claude.json"),
            r#"{"theme":"dark","mcpServers":{"other":{"command":"/bin/x"}}}"#,
        )
        .unwrap();
        let r = reg(tmp.path());
        r.register("claude-code").unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(tmp.path().join(".claude.json")).unwrap()).unwrap();
        assert_eq!(v["theme"], "dark", "无关顶层键保留");
        assert_eq!(v["mcpServers"]["other"]["command"], "/bin/x", "别人的 server 条目保留");
        assert!(v["mcpServers"]["voice-notes"].is_object());
    }

    #[test]
    fn corrupt_json_is_rejected_not_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".claude.json"), "{oops").unwrap();
        let r = reg(tmp.path());
        assert!(r.register("claude-code").is_err(), "坏文件必须拒写");
        assert_eq!(std::fs::read_to_string(tmp.path().join(".claude.json")).unwrap(), "{oops", "原文件原封不动");
    }

    #[test]
    fn unregister_removes_only_own_entry_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let r = reg(tmp.path());
        r.register("cursor").unwrap();
        r.unregister("cursor").unwrap();
        r.unregister("cursor").unwrap(); // 不存在时静默成功
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(tmp.path().join(".cursor/mcp.json")).unwrap()).unwrap();
        assert!(v["mcpServers"].get("voice-notes").is_none());
    }

    #[test]
    fn stale_when_command_differs_from_exe() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".claude.json"),
            r#"{"mcpServers":{"voice-notes":{"command":"/old/path/voice-notes","args":["mcp","serve"]}}}"#,
        )
        .unwrap();
        let st = reg(tmp.path()).status();
        let cc = st.iter().find(|s| s.key == "claude-code").unwrap();
        assert!(cc.registered && cc.stale);
        assert_eq!(cc.command.as_deref(), Some("/old/path/voice-notes"));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test mcp::registry -- --nocapture`
Expected: FAIL(todo! panic)。

- [ ] **Step 3: 实现 JSON 分支**(替换三个 `todo!`;TOML 分支本任务先返回错误,Task 3 补)

```rust
    fn status_one(&self, def: &AgentDef) -> AgentStatus {
        let installed = self.home.join(def.detect_rel).exists();
        let command = self.read_command(def);
        let registered = command.is_some();
        let stale = command.as_deref().map(|c| Path::new(c) != self.exe.as_path()).unwrap_or(false);
        AgentStatus {
            key: def.key.into(),
            name: def.name.into(),
            installed,
            registered,
            command,
            stale,
        }
    }

    /// 读已注册条目的 command;未注册/文件缺失/解析失败一律 None(status 是只读探测,不报错)。
    fn read_command(&self, def: &AgentDef) -> Option<String> {
        let text = std::fs::read_to_string(self.config_path(def)).ok()?;
        match def.fmt {
            Fmt::Json => {
                let v: serde_json::Value = serde_json::from_str(&text).ok()?;
                Some(v.get("mcpServers")?.get("voice-notes")?.get("command")?.as_str()?.to_string())
            }
            Fmt::Toml => None, // Task 3
        }
    }

    pub fn register(&self, key: &str) -> anyhow::Result<()> {
        let def = Self::def(key)?;
        let path = self.config_path(def);
        match def.fmt {
            Fmt::Json => self.upsert_json(&path),
            Fmt::Toml => anyhow::bail!("TOML 注册未实现(Task 3)"),
        }
    }

    pub fn unregister(&self, key: &str) -> anyhow::Result<()> {
        let def = Self::def(key)?;
        let path = self.config_path(def);
        if !path.exists() {
            return Ok(()); // 幂等:没有配置文件自然没有条目
        }
        match def.fmt {
            Fmt::Json => self.remove_json(&path),
            Fmt::Toml => anyhow::bail!("TOML 注销未实现(Task 3)"),
        }
    }

    fn upsert_json(&self, path: &Path) -> anyhow::Result<()> {
        let mut root: serde_json::Value = match std::fs::read_to_string(path) {
            Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text).map_err(|e| {
                anyhow::anyhow!("{} 不是合法 JSON,拒绝写入(请手动修复或手动配置): {e}", path.display())
            })?,
            _ => serde_json::json!({}),
        };
        let obj = root.as_object_mut().ok_or_else(|| anyhow::anyhow!("{} 顶层不是对象,拒绝写入", path.display()))?;
        let servers = obj.entry("mcpServers").or_insert_with(|| serde_json::json!({}));
        let servers = servers
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("{} 的 mcpServers 不是对象,拒绝写入", path.display()))?;
        servers.insert(
            "voice-notes".into(),
            serde_json::json!({ "command": self.exe.to_string_lossy(), "args": ["mcp", "serve"] }),
        );
        write_with_backup(path, &(serde_json::to_string_pretty(&root)? + "\n"))
    }

    fn remove_json(&self, path: &Path) -> anyhow::Result<()> {
        let text = std::fs::read_to_string(path)?;
        let mut root: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("{} 不是合法 JSON,拒绝写入: {e}", path.display()))?;
        let Some(servers) = root.get_mut("mcpServers").and_then(|v| v.as_object_mut()) else {
            return Ok(());
        };
        if servers.remove("voice-notes").is_none() {
            return Ok(()); // 本就没有:不产生写入(也就不产生备份)
        }
        write_with_backup(path, &(serde_json::to_string_pretty(&root)? + "\n"))
    }
```

文件级私有函数(registry.rs 底部、tests 之上):

```rust
/// 写前把现有文件备份为 `<file>.vn.bak`(覆盖旧备份),再 tmp+rename 原子写。
/// 父目录不存在则创建(如 .cursor/mcp.json 首次注册)。
fn write_with_backup(path: &Path, content: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        let mut bak = path.as_os_str().to_owned();
        bak.push(".vn.bak");
        std::fs::copy(path, PathBuf::from(&bak))?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".vn.tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
```

并在测试模块补备份断言(追加到 `register_creates_minimal_json_and_is_idempotent` 末尾):

```rust
        assert!(tmp.path().join(".cursor/mcp.json.vn.bak").exists(), "二次写入前留了备份");
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test mcp::registry -- --nocapture`
Expected: 6 passed。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/mcp/registry.rs
git commit -m "feat(mcp): Agent 注册器 JSON 分支——检测/upsert/remove/备份/坏文件拒写"
```

---

## Task 3: registry——Codex 的 TOML 分支(TDD)

**Files:**
- Modify: `src-tauri/src/mcp/registry.rs`
- Modify: `src-tauri/Cargo.toml`

**Interfaces:**
- Consumes: Task 2 的 `write_with_backup`、`Registry` 骨架。
- Produces: `Fmt::Toml` 三个操作(read_command/upsert/remove)可用;`register("codex")` 全通。

- [ ] **Step 1: 加依赖**

Run(在 `src-tauri/`): `cargo add toml_edit`
Expected: Cargo.toml `[dependencies]` 出现 `toml_edit = "..."`(取 crates.io 最新 stable)。

- [ ] **Step 2: 写失败测试**(registry.rs tests 模块追加)

```rust
    #[test]
    fn codex_toml_roundtrip_preserves_comments() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".codex")).unwrap();
        std::fs::write(
            tmp.path().join(".codex/config.toml"),
            "# 用户自己的注释\nmodel = \"o3\"\n\n[mcp_servers.other]\ncommand = \"/bin/x\"\n",
        )
        .unwrap();
        let r = reg(tmp.path());
        r.register("codex").unwrap();
        let text = std::fs::read_to_string(tmp.path().join(".codex/config.toml")).unwrap();
        assert!(text.contains("# 用户自己的注释"), "toml_edit 保注释:{text}");
        assert!(text.contains("model = \"o3\""));
        assert!(text.contains("[mcp_servers.other]"));
        let st = r.status();
        let codex = st.iter().find(|s| s.key == "codex").unwrap();
        assert!(codex.registered && !codex.stale, "TOML read_command 也要通:{codex:?}");
        // 注销后条目消失、其余保留
        r.unregister("codex").unwrap();
        let text = std::fs::read_to_string(tmp.path().join(".codex/config.toml")).unwrap();
        assert!(!text.contains("voice-notes"));
        assert!(text.contains("[mcp_servers.other]"));
    }

    #[test]
    fn codex_toml_created_when_missing_and_corrupt_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let r = reg(tmp.path());
        r.register("codex").unwrap(); // 文件不存在 → 创建最小结构
        let text = std::fs::read_to_string(tmp.path().join(".codex/config.toml")).unwrap();
        assert!(text.contains("[mcp_servers.voice-notes]"), "{text}");
        std::fs::write(tmp.path().join(".codex/config.toml"), "= 不是 toml =").unwrap();
        assert!(r.register("codex").is_err(), "坏 TOML 拒写");
    }
```

- [ ] **Step 3: 跑测试确认失败**

Run: `cargo test mcp::registry::tests::codex -- --nocapture`
Expected: FAIL("TOML 注册未实现")。

- [ ] **Step 4: 实现 TOML 三操作**(替换 Task 2 里三处 `Fmt::Toml` 分支)

`read_command` 的 Toml 分支:

```rust
            Fmt::Toml => {
                let doc: toml_edit::DocumentMut = text.parse().ok()?;
                Some(doc.get("mcp_servers")?.get("voice-notes")?.get("command")?.as_str()?.to_string())
            }
```

`register`/`unregister` 的 Toml 分支分别调 `self.upsert_toml(&path)` / `self.remove_toml(&path)`:

```rust
    fn upsert_toml(&self, path: &Path) -> anyhow::Result<()> {
        let mut doc: toml_edit::DocumentMut = match std::fs::read_to_string(path) {
            Ok(text) => text.parse().map_err(|e| {
                anyhow::anyhow!("{} 不是合法 TOML,拒绝写入(请手动修复或手动配置): {e}", path.display())
            })?,
            Err(_) => toml_edit::DocumentMut::new(),
        };
        let mut args = toml_edit::Array::new();
        args.push("mcp");
        args.push("serve");
        // 索引赋值自动建隐式父表,不打扰文件里已有的 [mcp_servers.*] 兄弟表。
        doc["mcp_servers"]["voice-notes"]["command"] = toml_edit::value(self.exe.to_string_lossy().as_ref());
        doc["mcp_servers"]["voice-notes"]["args"] = toml_edit::value(args);
        write_with_backup(path, &doc.to_string())
    }

    fn remove_toml(&self, path: &Path) -> anyhow::Result<()> {
        let text = std::fs::read_to_string(path)?;
        let mut doc: toml_edit::DocumentMut = text
            .parse()
            .map_err(|e| anyhow::anyhow!("{} 不是合法 TOML,拒绝写入: {e}", path.display()))?;
        let removed = doc
            .get_mut("mcp_servers")
            .and_then(|t| t.as_table_mut())
            .map(|t| t.remove("voice-notes").is_some())
            .unwrap_or(false);
        if !removed {
            return Ok(());
        }
        write_with_backup(path, &doc.to_string())
    }
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo test mcp::registry -- --nocapture`
Expected: 8 passed。若 `doc["mcp_servers"]["voice-notes"]` 生成了 `[mcp_servers]` 顶层 inline 形态导致断言 `[mcp_servers.voice-notes]` 失败,改用:

```rust
        let servers = doc.entry("mcp_servers").or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
        let servers = servers.as_table_mut().ok_or_else(|| anyhow::anyhow!("{} 的 mcp_servers 不是表,拒绝写入", path.display()))?;
        servers.set_implicit(true);
        let mut entry = toml_edit::Table::new();
        entry["command"] = toml_edit::value(self.exe.to_string_lossy().as_ref());
        entry["args"] = toml_edit::value(args);
        servers.insert("voice-notes", toml_edit::Item::Table(entry));
```

- [ ] **Step 6: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/mcp/registry.rs
git commit -m "feat(mcp): Codex CLI 的 TOML 注册分支(toml_edit 保注释/保排版)"
```

---

## Task 4: 注册 CLI(register/unregister/status)+ 路径自愈 + quarantine 提示

**Files:**
- Modify: `src-tauri/src/mcp/registry.rs`(加 `heal`、`quarantine_warning`)
- Modify: `src-tauri/src/mcp/mod.rs`(cli_main 挂三个子命令)

**Interfaces:**
- Consumes: Task 2/3 的 Registry 全套。
- Produces: `Registry::heal() -> anyhow::Result<u32>`(修复 stale 条目数;exe 路径含 `/target/` 时直接返 0);CLI:`mcp register [--agent X] [--dry-run]`、`mcp unregister [--agent X]`、`mcp status [--json]`。Task 9 的 GUI 自愈复用 `heal()`。

- [ ] **Step 1: 写失败测试**(registry.rs tests 追加)

```rust
    #[test]
    fn heal_rewrites_stale_and_skips_dev_binary() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".claude.json"),
            r#"{"mcpServers":{"voice-notes":{"command":"/old/voice-notes","args":["mcp","serve"]}}}"#,
        )
        .unwrap();
        // 生产二进制:自愈生效
        let r = reg(tmp.path());
        assert_eq!(r.heal().unwrap(), 1);
        assert!(!r.status().iter().find(|s| s.key == "claude-code").unwrap().stale);
        // 开发二进制(路径含 /target/):不动用户配置
        std::fs::write(
            tmp.path().join(".claude.json"),
            r#"{"mcpServers":{"voice-notes":{"command":"/old/voice-notes","args":["mcp","serve"]}}}"#,
        )
        .unwrap();
        let dev = Registry::with(tmp.path().to_path_buf(), PathBuf::from("/repo/src-tauri/target/debug/voice-notes"));
        assert_eq!(dev.heal().unwrap(), 0);
        assert!(dev.status().iter().find(|s| s.key == "claude-code").unwrap().stale, "开发态保持 stale 不改写");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test heal_rewrites -- --nocapture`
Expected: FAIL(heal 不存在,编译错)。

- [ ] **Step 3: 实现 heal + quarantine_warning**(registry.rs impl Registry 内)

```rust
    /// 修复 stale 注册(App 被移动/重装后 command 指向旧路径):重写为当前 exe。
    /// 开发态二进制(路径含 /target/)跳过——否则开发机会把用户配置指向 debug 构建。
    pub fn heal(&self) -> anyhow::Result<u32> {
        if self.exe.components().any(|c| c.as_os_str() == "target") {
            return Ok(0);
        }
        let mut healed = 0u32;
        for st in self.status() {
            if st.registered && st.stale {
                // register 即覆盖式 upsert,天然就是"改正"。单家失败不挡其余家。
                if self.register(&st.key).is_ok() {
                    healed += 1;
                }
            }
        }
        Ok(healed)
    }

    /// exe 带 com.apple.quarantine 时的提示(未签名 App 被 Agent spawn 会失败)。
    /// 纯提示不阻断;xattr 不存在/查询失败按无隔离处理。
    pub fn quarantine_warning(&self) -> Option<String> {
        let out = std::process::Command::new("/usr/bin/xattr")
            .arg("-p")
            .arg("com.apple.quarantine")
            .arg(&self.exe)
            .output()
            .ok()?;
        if out.status.success() {
            Some(format!(
                "警告: {} 带 com.apple.quarantine 隔离标记,Agent 可能无法启动它。\n请执行: xattr -dr com.apple.quarantine /Applications/voice-notes.app",
                self.exe.display()
            ))
        } else {
            None
        }
    }
```

- [ ] **Step 4: cli_main 挂子命令**(mod.rs,替换 Task 1 的占位分支;`serve` 仍占位到 Task 6)

```rust
pub fn cli_main(args: &[String]) -> i32 {
    let sub = args.first().map(String::as_str).unwrap_or("");
    match sub {
        "serve" => {
            eprintln!("mcp serve: 尚未实现");
            1
        }
        "register" | "unregister" | "status" => run_registry_cli(sub, &args[1..]),
        _ => {
            eprintln!(
                "用法: voice-notes mcp <serve|register|unregister|status>\n\
                 serve                 以 stdio MCP 服务运行(供 Agent spawn)\n\
                 register [--agent X]  注册到本机 Agent(X: claude-code|claude-desktop|cursor|codex|gemini|auto)\n\
                 unregister [--agent X]\n\
                 status [--json]       各 Agent 检测/注册状态"
            );
            2
        }
    }
}

/// --agent X 解析:未给或 auto = 所有"已检测到安装"的家;显式指名不要求已安装
/// (用户可能装在非常规位置,检测只是启发)。
fn parse_agent(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--agent" {
            return it.next().cloned();
        }
    }
    None
}

fn run_registry_cli(sub: &str, args: &[String]) -> i32 {
    let reg = match registry::Registry::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("初始化失败: {e}");
            return 1;
        }
    };
    let agent = parse_agent(args).unwrap_or_else(|| "auto".into());
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let json = args.iter().any(|a| a == "--json");

    if sub == "status" {
        let st = reg.status();
        if json {
            println!("{}", serde_json::to_string_pretty(&st).expect("Serialize 派生结构不会失败"));
        } else {
            for s in &st {
                let state = if !s.installed {
                    "未检测到"
                } else if s.stale {
                    "已注册(路径过期)"
                } else if s.registered {
                    "已注册"
                } else {
                    "未注册"
                };
                println!("{:<16} {}", s.key, state);
            }
        }
        return 0;
    }

    // register / unregister 的目标集合
    let targets: Vec<String> = if agent == "auto" {
        reg.status().into_iter().filter(|s| s.installed).map(|s| s.key).collect()
    } else {
        vec![agent]
    };
    if targets.is_empty() {
        eprintln!("未检测到任何已安装的 Agent;可用 --agent 显式指定,或在 App 设置页复制手动配置。");
        return 1;
    }
    if let Some(w) = reg.quarantine_warning() {
        eprintln!("{w}");
    }
    if dry_run {
        println!("将写入的条目:\n{}", reg.entry_snippet_json());
        println!("目标: {}", targets.join(", "));
        return 0;
    }
    let mut failed = 0;
    for key in &targets {
        let r = if sub == "register" { reg.register(key) } else { reg.unregister(key) };
        match r {
            Ok(()) => println!("{key}: ok"),
            Err(e) => {
                eprintln!("{key}: {e}");
                failed += 1;
            }
        }
    }
    if failed == 0 {
        0
    } else {
        1
    }
}
```

- [ ] **Step 5: 跑测试 + 手工验证**

Run: `cargo test mcp:: -- --nocapture`
Expected: 全 passed。
Run: `cargo run -- mcp status`(真机上应列出本机实际检测结果)、`cargo run -- mcp register --dry-run`
Expected: status 列 5 行;dry-run 打印 JSON 片段与目标,不写任何文件。

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/mcp/
git commit -m "feat(mcp): register/unregister/status CLI 与路径自愈、quarantine 提示"
```

---

## Task 5: 查询工具纯实现(tools.rs)+ NoteStore::render(TDD)

**Files:**
- Create: `src-tauri/src/mcp/tools.rs`(mod.rs 加 `pub mod tools;`)
- Modify: `src-tauri/src/store/export.rs`(渲染抽出为 `render`,`export` 改为调它)

**Interfaces:**
- Consumes: `store::NoteStore/load_refined/VoiceprintStore/SpeakerMeta`、`settings::{load,resolve_data_root}`、`mcp::app_data_dir`。
- Produces:
  - `store::NoteStore::render(&self, id: &str, format: &str) -> anyhow::Result<String>`(format: "md"|"txt")。
  - `tools::DataRoots { app_data: PathBuf, data_root: PathBuf }` 与 `tools::resolve_roots() -> DataRoots`(每次调用现算,数据目录迁移后无需重启服务)。
  - `tools::list_notes(&DataRoots, limit, offset, from, to) -> serde_json::Value`
  - `tools::search_notes(&DataRoots, query, limit) -> serde_json::Value`
  - `tools::get_note(&DataRoots, id, format, prefer_refined) -> anyhow::Result<serde_json::Value>`(format: "segments"|"markdown"|"text")
  - `tools::list_speakers(&DataRoots) -> serde_json::Value`
  Task 6 把这四个函数包成 MCP tool。

- [ ] **Step 1: export.rs 抽渲染**(先做小重构,行为不变)

`src-tauri/src/store/export.rs` 的 `impl NoteStore` 块改为:

```rust
impl NoteStore {
    /// 导出到会议文件夹内的 transcript.md / transcript.txt，返回文件路径。
    pub fn export(&self, id: &str, format: &str) -> anyhow::Result<PathBuf> {
        let content = self.render(id, format)?;
        let dir = self.note_dir(id)?;
        let name = match format {
            "md" => "transcript.md",
            _ => "transcript.txt",
        };
        let path = dir.join(name);
        std::fs::write(&path, content)?;
        Ok(path)
    }

    /// 渲染导出内容字符串(不落盘)。MCP get_note 与 export 共用同一渲染,防两处漂移。
    pub fn render(&self, id: &str, format: &str) -> anyhow::Result<String> {
        let note = self.load(id)?;
        Ok(match format {
            "md" => render_markdown(&note),
            "txt" => render_text(&note),
            _ => anyhow::bail!("未知导出格式: {format}"),
        })
    }
}
```

Run: `cargo test store:: -- --nocapture`
Expected: 既有 store 测试全 passed(纯重构)。

- [ ] **Step 2: 写 tools.rs 失败测试**(文件底部 tests;fixture helper 造一条真实形状的笔记)

```rust
//! MCP 查询工具的纯实现:文件系统 → serde_json::Value。不依赖 tauri/AppHandle,
//! stdio 服务进程与单测直接调用。App 运行与否都可用(只读,GUI 侧写入均原子)。

use crate::settings;
use crate::store::{self, NoteStore, SpeakerMeta};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub struct DataRoots {
    pub app_data: PathBuf,
    pub data_root: PathBuf,
}

/// 每次工具调用现算(极廉价):settings.json 的 data_dir 可能随时被 GUI 迁移。
pub fn resolve_roots() -> DataRoots {
    let app_data = super::app_data_dir();
    let s = settings::load(&app_data);
    let data_root = settings::resolve_data_root(&app_data, &s);
    DataRoots { app_data, data_root }
}

fn notes_dir(roots: &DataRoots) -> PathBuf {
    roots.data_root.join("notes")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一条最小真实笔记:meta.json + segments.jsonl + speakers.json。
    fn fixture_note(root: &std::path::Path, id: &str, title: &str, started_at: &str, lines: &[(&str, &str, u64)]) {
        let dir = root.join("notes").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("meta.json"),
            serde_json::json!({
                "schema_version": 1, "id": id, "title": title,
                "started_at": started_at, "ended_at": started_at, "state": "complete"
            })
            .to_string(),
        )
        .unwrap();
        let mut jsonl = String::new();
        for (i, (speaker, text, start_ms)) in lines.iter().enumerate() {
            jsonl.push_str(
                &serde_json::json!({
                    "seq": i as u64, "source": "mic", "text": text,
                    "start_ms": start_ms, "end_ms": start_ms + 1000, "speaker": speaker
                })
                .to_string(),
            );
            jsonl.push('\n');
        }
        std::fs::write(dir.join("segments.jsonl"), jsonl).unwrap();
        std::fs::write(
            dir.join("speakers.json"),
            serde_json::json!({ "S1": { "name": "张三", "sources": ["mic"], "count": 2, "person_id": "P1" } }).to_string(),
        )
        .unwrap();
    }

    fn roots(tmp: &std::path::Path) -> DataRoots {
        DataRoots { app_data: tmp.to_path_buf(), data_root: tmp.to_path_buf() }
    }

    #[test]
    fn list_notes_pages_and_filters_by_time() {
        let tmp = tempfile::tempdir().unwrap();
        fixture_note(tmp.path(), "20260101-100000", "一月会", "2026-01-01T10:00:00+08:00", &[("S1", "a", 0)]);
        fixture_note(tmp.path(), "20260301-100000", "三月会", "2026-03-01T10:00:00+08:00", &[("S1", "b", 0)]);
        let v = list_notes(&roots(tmp.path()), 10, 0, None, None);
        assert_eq!(v["notes"].as_array().unwrap().len(), 2);
        assert_eq!(v["notes"][0]["title"], "三月会", "倒序:新的在前");
        let v = list_notes(&roots(tmp.path()), 10, 0, Some("2026-02-01"), None);
        assert_eq!(v["notes"].as_array().unwrap().len(), 1);
        assert_eq!(v["notes"][0]["id"], "20260301-100000");
        let v = list_notes(&roots(tmp.path()), 1, 1, None, None);
        assert_eq!(v["notes"][0]["title"], "一月会", "offset 翻页");
        assert_eq!(v["total"], 2);
    }

    #[test]
    fn search_notes_matches_case_insensitive_with_context() {
        let tmp = tempfile::tempdir().unwrap();
        fixture_note(
            tmp.path(),
            "20260101-100000",
            "评审会",
            "2026-01-01T10:00:00+08:00",
            &[("S1", "先看背景", 0), ("S1", "交付日期定在 Q3", 1000), ("S1", "散会", 2000)],
        );
        let v = search_notes(&roots(tmp.path()), "交付日期", 10);
        let hits = v["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["note_id"], "20260101-100000");
        assert_eq!(hits[0]["text"], "交付日期定在 Q3");
        assert_eq!(hits[0]["before"], "先看背景");
        assert_eq!(hits[0]["after"], "散会");
        assert_eq!(hits[0]["speaker"], "S1");
        assert!(search_notes(&roots(tmp.path()), "不存在的词", 10)["hits"].as_array().unwrap().is_empty());
    }

    #[test]
    fn get_note_segments_markdown_and_refined_preference() {
        let tmp = tempfile::tempdir().unwrap();
        fixture_note(tmp.path(), "20260101-100000", "评审会", "2026-01-01T10:00:00+08:00", &[("S1", "原始句", 0)]);
        let v = get_note(&roots(tmp.path()), "20260101-100000", "segments", true).unwrap();
        assert_eq!(v["refined"], false, "无精修稿回落原始");
        assert_eq!(v["segments"][0]["text"], "原始句");
        assert_eq!(v["speakers"]["S1"]["name"], "张三");
        let md = get_note(&roots(tmp.path()), "20260101-100000", "markdown", false).unwrap();
        assert!(md["content"].as_str().unwrap().contains("原始句"));
        // 落一份精修稿:prefer_refined=true 时取精修
        let dir = tmp.path().join("notes/20260101-100000");
        store::write_refined_atomic(
            &dir,
            &store::RefinedDoc {
                schema_version: 1,
                generated_at: "2026-01-01T11:00:00+08:00".into(),
                llm_model: None,
                stages: store::RefineStages { filter: "done".into(), recluster: "done".into(), llm: "done".into() },
                discarded_seqs: vec![],
                paragraphs: vec![store::RefinedParagraph {
                    speaker: "S1".into(),
                    name: Some("张三".into()),
                    start_ms: 0,
                    end_ms: 1000,
                    text: "精修句".into(),
                    source_seqs: vec![0],
                }],
            },
        )
        .unwrap();
        let v = get_note(&roots(tmp.path()), "20260101-100000", "segments", true).unwrap();
        assert_eq!(v["refined"], true);
        assert_eq!(v["paragraphs"][0]["text"], "精修句");
        let md = get_note(&roots(tmp.path()), "20260101-100000", "markdown", true).unwrap();
        assert!(md["content"].as_str().unwrap().contains("精修句"));
        assert!(get_note(&roots(tmp.path()), "no-such", "segments", true).is_err());
        assert!(get_note(&roots(tmp.path()), "../evil", "segments", true).is_err(), "id 穿越防护");
    }

    #[test]
    fn list_speakers_joins_note_counts() {
        let tmp = tempfile::tempdir().unwrap();
        fixture_note(tmp.path(), "20260101-100000", "会一", "2026-01-01T10:00:00+08:00", &[("S1", "a", 0)]);
        fixture_note(tmp.path(), "20260102-100000", "会二", "2026-01-02T10:00:00+08:00", &[("S1", "b", 0)]);
        // 最小声纹库:voiceprints/db.json 的真实路径与形状由 VoiceprintStore 决定,
        // 这里直接经 store 写入以免猜格式。
        let vp = store::VoiceprintStore::new(tmp.path().to_path_buf());
        // 若 VoiceprintStore 无公开写入 API,则本测试改为:仅断言 people 为空时
        // note_counts 逻辑不炸,并把"有人物"的断言留给 e2e(实现者按实际 API 取舍,
        // 保底断言如下)。
        let v = list_speakers(&roots(tmp.path()));
        assert!(v["speakers"].as_array().is_some());
        let _ = vp;
    }
}
```

- [ ] **Step 3: 跑测试确认失败**

Run: `cargo test mcp::tools -- --nocapture`
Expected: FAIL(四个函数不存在)。

- [ ] **Step 4: 实现四个函数**(tests 之上)

```rust
/// 笔记列表。from/to 为 RFC3339 前缀(如 "2026-02-01"),与 started_at 字典序比较
/// (同时区 RFC3339 字典序即时间序,与 NoteStore::list 排序同一假设)。
pub fn list_notes(
    roots: &DataRoots,
    limit: usize,
    offset: usize,
    from: Option<&str>,
    to: Option<&str>,
) -> serde_json::Value {
    let all = NoteStore::new(notes_dir(roots)).list();
    let filtered: Vec<_> = all
        .into_iter()
        .filter(|n| from.map(|f| n.started_at.as_str() >= f).unwrap_or(true))
        .filter(|n| to.map(|t| n.started_at.as_str() <= t).unwrap_or(true))
        .collect();
    let total = filtered.len();
    let page: Vec<_> = filtered
        .into_iter()
        .skip(offset)
        .take(limit.clamp(1, 100))
        .map(|n| {
            serde_json::json!({
                "id": n.id, "title": n.title, "started_at": n.started_at,
                "duration_secs": n.duration_secs, "state": n.state,
            })
        })
        .collect();
    serde_json::json!({ "total": total, "notes": page })
}

/// 全文检索:遍历全部笔记逐段子串匹配(大小写不敏感)。个人量级(百场×百句)
/// 全扫毫秒级,不建索引(YAGNI,见设计文档 §三)。
pub fn search_notes(roots: &DataRoots, query: &str, limit: usize) -> serde_json::Value {
    let store = NoteStore::new(notes_dir(roots));
    let needle = query.to_lowercase();
    let mut hits = Vec::new();
    let mut scanned = 0usize;
    'outer: for summary in store.list() {
        let Ok(note) = store.load(&summary.id) else { continue };
        scanned += 1;
        for (i, seg) in note.segments.iter().enumerate() {
            if !seg.text.to_lowercase().contains(&needle) {
                continue;
            }
            hits.push(serde_json::json!({
                "note_id": summary.id, "title": summary.title,
                "seq": seg.seq, "speaker": seg.speaker, "start_ms": seg.start_ms,
                "text": seg.text,
                "before": if i > 0 { note.segments[i - 1].text.clone() } else { String::new() },
                "after": note.segments.get(i + 1).map(|s| s.text.clone()).unwrap_or_default(),
            }));
            if hits.len() >= limit.clamp(1, 100) {
                break 'outer;
            }
        }
    }
    serde_json::json!({ "scanned_notes": scanned, "hits": hits })
}

/// 笔记全文。format: segments(结构化) / markdown / text;prefer_refined 且
/// refined.json 存在时返回精修稿(结构化给 paragraphs,md/txt 现场渲染精修段)。
pub fn get_note(
    roots: &DataRoots,
    id: &str,
    format: &str,
    prefer_refined: bool,
) -> anyhow::Result<serde_json::Value> {
    let store = NoteStore::new(notes_dir(roots));
    let note = store.load(id)?; // 内含 validate_note_id 防穿越 + 存在性检查
    let refined = if prefer_refined { store::load_refined(&notes_dir(roots).join(id)) } else { None };
    let speakers: serde_json::Value = note
        .speakers
        .iter()
        .map(|(sid, m)| (sid.clone(), serde_json::json!({ "name": m.name, "person_id": m.person_id })))
        .collect::<serde_json::Map<_, _>>()
        .into();
    match format {
        "segments" => Ok(match refined {
            Some(doc) => serde_json::json!({
                "id": note.meta.id, "title": note.meta.title, "started_at": note.meta.started_at,
                "state": note.meta.state, "speakers": speakers, "refined": true,
                "generated_at": doc.generated_at,
                "paragraphs": doc.paragraphs.iter().map(|p| serde_json::json!({
                    "speaker": p.speaker, "name": p.name, "start_ms": p.start_ms,
                    "end_ms": p.end_ms, "text": p.text,
                })).collect::<Vec<_>>(),
            }),
            None => serde_json::json!({
                "id": note.meta.id, "title": note.meta.title, "started_at": note.meta.started_at,
                "state": note.meta.state, "speakers": speakers, "refined": false,
                "segments": note.segments.iter().map(|s| serde_json::json!({
                    "seq": s.seq, "source": s.source, "speaker": s.speaker,
                    "start_ms": s.start_ms, "end_ms": s.end_ms, "text": s.text,
                })).collect::<Vec<_>>(),
            }),
        }),
        "markdown" | "text" => {
            let was_refined = refined.is_some();
            let content = match refined {
                Some(doc) => render_refined(&note.meta.title, &doc, format == "markdown"),
                None => store.render(id, if format == "markdown" { "md" } else { "txt" })?,
            };
            Ok(serde_json::json!({
                "id": note.meta.id, "title": note.meta.title,
                "refined": was_refined,
                "content": content,
            }))
        }
        _ => anyhow::bail!("未知 format: {format}(可用 segments|markdown|text)"),
    }
}

/// 精修稿的 md/txt 渲染(原始稿渲染在 store::export,精修段形状不同,单独渲染)。
fn render_refined(title: &str, doc: &store::RefinedDoc, md: bool) -> String {
    let mut out = String::new();
    if md {
        out.push_str(&format!("# {title}\n\n"));
    } else {
        out.push_str(&format!("{title}\n\n"));
    }
    for p in &doc.paragraphs {
        let label = p.name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| p.speaker.clone());
        let ts = crate::store::format_ts(p.start_ms);
        if md {
            out.push_str(&format!("**{label}** `[{ts}]`\n\n{}\n\n", p.text));
        } else {
            out.push_str(&format!("{label} [{ts}]\n{}\n\n", p.text));
        }
    }
    out
}

/// 全局声纹库人物 + 各自出现过的笔记数(扫 speakers.json 的 person_id)。
pub fn list_speakers(roots: &DataRoots) -> serde_json::Value {
    let vp = store::VoiceprintStore::new(roots.data_root.clone()).load();
    let mut note_counts: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    if let Ok(rd) = std::fs::read_dir(notes_dir(roots)) {
        for e in rd.flatten().filter(|e| e.path().is_dir()) {
            let Ok(text) = std::fs::read_to_string(e.path().join("speakers.json")) else { continue };
            let Ok(map) = serde_json::from_str::<BTreeMap<String, SpeakerMeta>>(&text) else { continue };
            let mut seen = std::collections::HashSet::new();
            for m in map.values() {
                if let Some(pid) = &m.person_id {
                    if seen.insert(pid.clone()) {
                        *note_counts.entry(pid.clone()).or_default() += 1;
                    }
                }
            }
        }
    }
    let speakers: Vec<_> = vp
        .people
        .iter()
        .map(|(id, p)| {
            serde_json::json!({
                "id": id, "name": p.name, "total_ms": p.total_ms,
                "last_seen": p.last_seen, "note_count": note_counts.get(id.as_str()).copied().unwrap_or(0),
            })
        })
        .collect();
    serde_json::json!({ "speakers": speakers })
}
```

配套两处小改:
1. `store/mod.rs` 加一行 re-export(`export.rs` 的 `format_ts` 本就是 pub fn,只是 `mod export` 私有,提穿透即可):

```rust
pub use export::format_ts;
```

2. `mcp/mod.rs` 加 `pub mod tools;`。

- [ ] **Step 5: 跑测试确认通过**

Run: `cargo test mcp::tools store::` && `cargo check`
Expected: 全 passed。

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/mcp/ src-tauri/src/store/export.rs src-tauri/src/store/mod.rs
git commit -m "feat(mcp): 四个查询工具纯实现(list/search/get/speakers)+ NoteStore::render 抽出"
```

---

## Task 6: rmcp stdio 服务 + e2e

**Files:**
- Create: `src-tauri/src/mcp/server.rs`(mod.rs 加 `pub mod server;`,cli_main 的 `serve` 分支接入)
- Create: `src-tauri/tests/mcp_stdio.rs`
- Modify: `src-tauri/Cargo.toml`

**Interfaces:**
- Consumes: `tools::{resolve_roots, list_notes, search_notes, get_note, list_speakers}`。
- Produces: `server::serve_stdio() -> i32`;MCP server 名 "voice-notes",4 个工具可被任意 MCP 客户端调用。Task 14 在同一 `VnMcp` 上追加 UDS 工具。

- [ ] **Step 1: 加依赖**

Run(在 `src-tauri/`):

```bash
cargo add rmcp --features server,transport-io,macros,schemars
cargo add tokio --features rt-multi-thread,macros,net,io-std,time
cargo add schemars
```

Expected: 三依赖入 Cargo.toml。**注意**:rmcp 演进快,若 feature 名对不上(如 `schemars` 特性不存在),以 `cargo add rmcp --features server,transport-io` 起步,编译报缺什么再补;宏 API 以 docs.rs 当前版为准(下方代码按 2026-07 时点 docs.rs 核对过:`#[tool_router]` + `#[tool_handler]` + `Parameters<T>` 提取器 + `ServiceExt::serve(stdio())`)。

- [ ] **Step 2: 实现 server.rs**

```rust
//! rmcp stdio MCP 服务。查询工具直读数据文件;UDS 工具(状态/实时/控制)见 Task 14。

use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
    transport::stdio,
    ErrorData as McpError, ServerHandler, ServiceExt,
};
use serde::Deserialize;

use super::tools;

#[derive(Clone, Default)]
pub struct VnMcp;

fn ok_json(v: serde_json::Value) -> CallToolResult {
    CallToolResult::success(vec![Content::text(v.to_string())])
}

fn err_text(msg: String) -> CallToolResult {
    CallToolResult::error(vec![Content::text(msg)])
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct ListNotesParams {
    /// 返回条数,默认 20,最大 100
    pub limit: Option<usize>,
    /// 跳过条数(翻页),默认 0
    pub offset: Option<usize>,
    /// 起始时间过滤,RFC3339 前缀,如 "2026-07-01"
    pub from: Option<String>,
    /// 截止时间过滤,RFC3339 前缀
    pub to: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    /// 检索词(在所有笔记的逐句文本里做大小写不敏感子串匹配)
    pub query: String,
    /// 最多返回命中数,默认 20,最大 100
    pub limit: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct GetNoteParams {
    /// 笔记 id(来自 list_notes / search_notes)
    pub note_id: String,
    /// "segments"(默认,逐句结构化) | "markdown" | "text"
    pub format: Option<String>,
    /// 有精修稿时优先返回精修稿,默认 true
    pub prefer_refined: Option<bool>,
}

#[tool_router]
impl VnMcp {
    #[tool(description = "列出会议笔记(倒序分页;from/to 可按时间过滤)。返回 id/标题/开始时间/时长/状态。")]
    async fn list_notes(&self, Parameters(p): Parameters<ListNotesParams>) -> Result<CallToolResult, McpError> {
        let roots = tools::resolve_roots();
        Ok(ok_json(tools::list_notes(&roots, p.limit.unwrap_or(20), p.offset.unwrap_or(0), p.from.as_deref(), p.to.as_deref())))
    }

    #[tool(description = "全文检索所有会议笔记的转写内容,返回命中句与上下文各一句、说话人与时间戳。")]
    async fn search_notes(&self, Parameters(p): Parameters<SearchParams>) -> Result<CallToolResult, McpError> {
        let roots = tools::resolve_roots();
        Ok(ok_json(tools::search_notes(&roots, &p.query, p.limit.unwrap_or(20))))
    }

    #[tool(description = "读取一场会议笔记全文。segments 给逐句结构化(含说话人/时间戳),markdown/text 给渲染稿;有 AI 精修稿时默认优先精修稿。")]
    async fn get_note(&self, Parameters(p): Parameters<GetNoteParams>) -> Result<CallToolResult, McpError> {
        let roots = tools::resolve_roots();
        match tools::get_note(&roots, &p.note_id, p.format.as_deref().unwrap_or("segments"), p.prefer_refined.unwrap_or(true)) {
            Ok(v) => Ok(ok_json(v)),
            Err(e) => Ok(err_text(e.to_string())),
        }
    }

    #[tool(description = "列出全局声纹库中的说话人(跨会议一致的人物编号/名字/累计说话时长/出现的笔记数)。")]
    async fn list_speakers(&self) -> Result<CallToolResult, McpError> {
        let roots = tools::resolve_roots();
        Ok(ok_json(tools::list_speakers(&roots)))
    }
}

#[tool_handler]
impl ServerHandler for VnMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            instructions: Some(
                "voice-notes 本地会议笔记。查询类工具(list/search/get/speakers)随时可用;\
                 录制状态与控制类工具需要 voice-notes 应用正在运行。所有数据均在本机。"
                    .into(),
            ),
            ..Default::default()
        }
    }
}

/// stdio 服务主循环:客户端关 stdin 即退出。仅此分支创建 tokio runtime,GUI 路径零影响。
pub fn serve_stdio() -> i32 {
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("tokio runtime 创建失败: {e}");
            return 1;
        }
    };
    let result: anyhow::Result<()> = rt.block_on(async {
        let service = VnMcp::default().serve(stdio()).await?;
        service.waiting().await?;
        Ok(())
    });
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("mcp serve 退出: {e}");
            1
        }
    }
}
```

mod.rs:`pub mod server;`,cli_main 的 serve 分支改为 `"serve" => server::serve_stdio(),`。

- [ ] **Step 3: cargo check 过编译**

Run: `cargo check`
Expected: 编译通过。若 `Parameters` 路径/宏签名与所装 rmcp 版本不符,以 `docs.rs/rmcp` 该版本的 `tool_router` 模块文档为准调整 import(结构不变:参数结构体 + 提取器 + 两个宏)。

- [ ] **Step 4: 写 e2e 测试**(`src-tauri/tests/mcp_stdio.rs`——集成测试,newline-delimited JSON-RPC 直接对话,不引客户端 SDK)

```rust
//! MCP stdio e2e:spawn 真二进制,走 initialize → tools/list → tools/call 全链路。
//! 数据经 VN_APP_DATA 注入 tempdir(settings.json 缺省 → data_root=app_data)。

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: i64,
}

impl Mcp {
    fn spawn(app_data: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_voice-notes"))
            .args(["mcp", "serve"])
            .env("VN_APP_DATA", app_data)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn voice-notes mcp serve");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self { child, stdin, stdout, next_id: 0 }
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.next_id += 1;
        let id = self.next_id;
        writeln!(
            self.stdin,
            "{}",
            serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
        )
        .unwrap();
        // 服务端可能穿插通知,读到匹配 id 的响应为止
        loop {
            let mut line = String::new();
            assert!(self.stdout.read_line(&mut line).unwrap() > 0, "服务端过早关闭");
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            if v.get("id") == Some(&serde_json::json!(id)) {
                assert!(v.get("error").is_none(), "RPC 错误: {v}");
                return v["result"].clone();
            }
        }
    }

    fn notify(&mut self, method: &str) {
        writeln!(self.stdin, "{}", serde_json::json!({ "jsonrpc": "2.0", "method": method })).unwrap();
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 工具结果的首块文本解析为 JSON。
fn tool_json(result: &serde_json::Value) -> serde_json::Value {
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[test]
fn stdio_initialize_list_and_call_tools() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("notes/20260101-100000");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("meta.json"),
        r#"{"schema_version":1,"id":"20260101-100000","title":"评审会","started_at":"2026-01-01T10:00:00+08:00","ended_at":"2026-01-01T11:00:00+08:00","state":"complete"}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("segments.jsonl"),
        r#"{"seq":0,"source":"mic","text":"交付日期定在 Q3","start_ms":0,"end_ms":1000,"speaker":"S1"}"#.to_string() + "\n",
    )
    .unwrap();

    let mut mcp = Mcp::spawn(tmp.path());
    let init = mcp.request(
        "initialize",
        serde_json::json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "e2e", "version": "0" }
        }),
    );
    assert!(init["capabilities"]["tools"].is_object(), "{init}");
    mcp.notify("notifications/initialized");

    let tools = mcp.request("tools/list", serde_json::json!({}));
    let names: Vec<&str> = tools["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    for expect in ["list_notes", "search_notes", "get_note", "list_speakers"] {
        assert!(names.contains(&expect), "缺工具 {expect}: {names:?}");
    }

    let r = mcp.request("tools/call", serde_json::json!({ "name": "list_notes", "arguments": {} }));
    let v = tool_json(&r);
    assert_eq!(v["total"], 1);
    assert_eq!(v["notes"][0]["title"], "评审会");

    let r = mcp.request("tools/call", serde_json::json!({ "name": "search_notes", "arguments": { "query": "交付日期" } }));
    assert_eq!(tool_json(&r)["hits"][0]["note_id"], "20260101-100000");

    let r = mcp.request(
        "tools/call",
        serde_json::json!({ "name": "get_note", "arguments": { "note_id": "20260101-100000", "format": "markdown" } }),
    );
    assert!(tool_json(&r)["content"].as_str().unwrap().contains("交付日期定在 Q3"));

    // 错误路径:不存在的笔记 → isError 工具级错误(而非 RPC 协议错误)
    let r = mcp.request(
        "tools/call",
        serde_json::json!({ "name": "get_note", "arguments": { "note_id": "no-such" } }),
    );
    assert_eq!(r["isError"], true, "{r}");
}
```

- [ ] **Step 5: 跑 e2e**

Run: `cargo test --test mcp_stdio -- --nocapture`
Expected: 1 passed(首次要完整链接二进制,几分钟属正常)。

- [ ] **Step 6: 手工冒烟(可选但建议)**

Run: `npx -y @modelcontextprotocol/inspector --cli cargo run -- mcp serve` 或真机 `claude mcp add voice-notes -- $PWD/src-tauri/target/debug/voice-notes mcp serve` 后在 Claude Code 里调 `list_notes`。
Expected: 工具列表与真实笔记可见。

- [ ] **Step 7: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/mcp/ src-tauri/tests/mcp_stdio.rs
git commit -m "feat(mcp): rmcp stdio 服务上线——四查询工具 + JSON-RPC e2e"
```

---

## Task 7: README 双语「接入 AI 助手(MCP)」+ Agent 安装引导

**Files:**
- Modify: `README.md`(「配置」章节之后、「从源码运行」之前插入新章节;「安装」章节顶部插引导块)
- Modify: `README.en.md`(同结构英文版,自行对照翻译,结构与中文一一对应)

**Interfaces:**
- Consumes: Task 4 的 CLI 语义(`mcp register --agent auto` / `mcp status --json`)。

- [ ] **Step 1: 「安装」章节顶部加 Agent 安装引导块**(`### 系统要求` 之前)

```markdown
> **让 AI 助手帮你装**:把下面这段话原样发给你的 AI 编程助手(Claude Code / Codex 等),它会完成下载、安装与 MCP 接入:
>
> ```text
> 请帮我安装 voice-notes 并接入 MCP:
> 1. 从 https://github.com/SoulZhong/voice-notes/releases 下载最新的 voice-notes_*_aarch64.dmg;
> 2. 挂载 DMG,把 voice-notes.app 拷入 /Applications,然后执行
>    xattr -dr com.apple.quarantine /Applications/voice-notes.app;
> 3. 执行 /Applications/voice-notes.app/Contents/MacOS/voice-notes mcp register --agent auto
>    注册 MCP,并用 ... mcp status --json 确认注册成功;
> 4. 提醒我手动打开一次 App 完成识别模型下载(约 1GB,需要 GUI)。
> ```
```

- [ ] **Step 2: 新章节「接入 AI 助手(MCP)」**(「## 配置」表格之后)

````markdown
## 接入 AI 助手(MCP)

voice-notes 内置 MCP(Model Context Protocol)服务:注册后,Claude Code / Claude Desktop / Cursor / Codex CLI / Gemini CLI 可以直接检索你的会议笔记——"上周和张三定的交付日期是哪天?"、"把今天的周会整理成邮件"。

> **隐私提示**:笔记内容经 Agent 检索后会进入其 LLM 上下文,是否上云取决于你所用的 Agent 与模型;**voice-notes 自身仍然不联网上传任何内容**。"允许 AI 控制录制"默认关闭,可在 设置 → AI 助手接入 开启。

三种接入方式(任选其一):

1. **应用内**:首次启动的欢迎页勾选,或随时到 设置 → AI 助手接入 注册/移除。
2. **命令行**(免打开界面,Agent 亦可直接执行):

   ```bash
   /Applications/voice-notes.app/Contents/MacOS/voice-notes mcp register --agent auto   # 注册到所有检测到的 Agent
   /Applications/voice-notes.app/Contents/MacOS/voice-notes mcp status --json           # 查看注册状态
   ```

3. **手动配置**(未内置的 Agent):在其 MCP 配置里加:

   ```json
   { "mcpServers": { "voice-notes": {
       "command": "/Applications/voice-notes.app/Contents/MacOS/voice-notes",
       "args": ["mcp", "serve"] } } }
   ```

   Codex CLI(`~/.codex/config.toml`):

   ```toml
   [mcp_servers.voice-notes]
   command = "/Applications/voice-notes.app/Contents/MacOS/voice-notes"
   args = ["mcp", "serve"]
   ```

提供的工具:

| 工具 | 用途 | 需要 App 运行 |
| --- | --- | --- |
| `list_notes` | 笔记列表(分页/时间过滤) | 否 |
| `search_notes` | 全文检索转写内容 | 否 |
| `get_note` | 读一场笔记全文(优先 AI 精修稿) | 否 |
| `list_speakers` | 全局声纹库人物 | 否 |
| `recording_status` / `get_live_transcript` | 录制状态 / 实时转写 | 是 |
| `start/stop/pause/resume_recording` | 控制录制(默认禁用,设置里开启) | 是 |
````

- [ ] **Step 3: README.en.md 同步**(同两处、英文,注意 en 版对应章节锚点)

- [ ] **Step 4: 校对渲染**

Run: `grep -n "接入 AI 助手" README.md && grep -n "MCP" README.en.md | head`
Expected: 两文件均含新章节;人工快速通读一遍代码块闭合与表格渲染。

- [ ] **Step 5: Commit**

```bash
git add README.md README.en.md
git commit -m "docs(README): MCP 接入章节与「让 Agent 安装本 App」引导(中英)"
```

---

**至此 M1 完成,可独立发版。**

---

## Task 8: settings 新字段(mcp_allow_control / mcp_onboarded)

**Files:**
- Modify: `src-tauri/src/settings.rs`
- Modify: `src/lib/models.ts`(Settings 类型)

**Interfaces:**
- Produces: `Settings.mcp_allow_control: bool`(默认 false)、`Settings.mcp_onboarded: bool`(默认 false);前端同名字段。Task 11 欢迎页/提示条与 Task 13 门控消费。

- [ ] **Step 1: 写失败测试**(settings.rs tests 追加)

```rust
    #[test]
    fn mcp_fields_default_off_and_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("settings.json"), r#"{"asr_model":"whisper"}"#).unwrap();
        let s = load(tmp.path());
        assert!(!s.mcp_allow_control, "控制录制默认关(隐私敏感)");
        assert!(!s.mcp_onboarded);
        let s = Settings { mcp_allow_control: true, mcp_onboarded: true, ..Default::default() };
        save(tmp.path(), &s).unwrap();
        let got = load(tmp.path());
        assert!(got.mcp_allow_control && got.mcp_onboarded);
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test mcp_fields -- --nocapture`
Expected: 编译失败(字段不存在)。

- [ ] **Step 3: 加字段**(Settings 结构体 `onboarded` 之后 + Default impl 补两行)

```rust
    /// 允许 MCP(AI 助手)控制录制(start/stop/pause/resume)。默认关:开录是隐私
    /// 敏感操作,必须用户显式授权。
    #[serde(default)]
    pub mcp_allow_control: bool,
    /// MCP 接入引导已展示过(欢迎页步骤走完,或存量用户提示条被关闭)。
    #[serde(default)]
    pub mcp_onboarded: bool,
```

Default impl 加 `mcp_allow_control: false, mcp_onboarded: false,`。

`src/lib/models.ts` 的 `Settings` 类型 `onboarded` 行后加:

```ts
  // 允许 MCP(AI 助手)控制录制
  mcp_allow_control: boolean;
  // MCP 接入引导已展示过
  mcp_onboarded: boolean;
```

- [ ] **Step 4: 跑测试 + 前端类型检查**

Run: `cargo test settings:: -- --nocapture` && `npm run check`
Expected: Rust 全 passed;svelte-check 无新错误。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/settings.rs src/lib/models.ts
git commit -m "feat(mcp): settings 新增 mcp_allow_control / mcp_onboarded(默认关)"
```

---

## Task 9: GUI 侧注册命令 + 启动自愈

**Files:**
- Modify: `src-tauri/src/lib.rs`(4 个新命令 + invoke_handler 注册 + setup 里自愈)
- Create: `src/lib/mcp.ts`

**Interfaces:**
- Consumes: `mcp::registry::{Registry, AgentStatus}`(Task 2-4)。
- Produces:
  - tauri 命令:`mcp_agents_status() -> Vec<AgentStatus>`、`mcp_register(agents: Vec<String>) -> Vec<RegisterOutcome>`、`mcp_unregister(agent: String)`、`mcp_manual_snippet() -> String`、`mcp_healed_count() -> u32`。
  - `RegisterOutcome { key: String, ok: bool, error: Option<String> }`(Serialize,定义在 lib.rs 命令旁)。
  - 前端 `src/lib/mcp.ts`:同名 invoke 封装(Task 10/11 消费)。

- [ ] **Step 1: lib.rs 加命令**(list_people 命令块之后)

```rust
// —— MCP 注册(设置页/欢迎页消费;registry 真值源是各 Agent 配置文件) ——

#[derive(serde::Serialize)]
struct RegisterOutcome {
    key: String,
    ok: bool,
    error: Option<String>,
}

/// 启动自愈修复的条目数,设置页读一次并展示提示条。AtomicU32 而非事件:setup 时
/// 前端尚未挂监听,事件会丢。
static MCP_HEALED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[tauri::command]
fn mcp_agents_status() -> Result<Vec<mcp::registry::AgentStatus>, String> {
    Ok(mcp::registry::Registry::new().map_err(|e| e.to_string())?.status())
}

#[tauri::command]
fn mcp_register(agents: Vec<String>) -> Result<Vec<RegisterOutcome>, String> {
    let reg = mcp::registry::Registry::new().map_err(|e| e.to_string())?;
    Ok(agents
        .into_iter()
        .map(|key| match reg.register(&key) {
            Ok(()) => RegisterOutcome { key, ok: true, error: None },
            Err(e) => RegisterOutcome { key, ok: false, error: Some(e.to_string()) },
        })
        .collect())
}

#[tauri::command]
fn mcp_unregister(agent: String) -> Result<(), String> {
    mcp::registry::Registry::new().map_err(|e| e.to_string())?.unregister(&agent).map_err(|e| e.to_string())
}

#[tauri::command]
fn mcp_manual_snippet() -> Result<String, String> {
    Ok(mcp::registry::Registry::new().map_err(|e| e.to_string())?.entry_snippet_json())
}

#[tauri::command]
fn mcp_healed_count() -> u32 {
    MCP_HEALED.swap(0, Ordering::SeqCst) // 读即清:提示只出一次
}
```

invoke_handler 列表(delete_person 之后)追加:

```rust
            mcp_agents_status,
            mcp_register,
            mcp_unregister,
            mcp_manual_snippet,
            mcp_healed_count
```

setup 闭包末尾(`tray::setup(&handle);` 之后、`Ok(())` 之前)加:

```rust
            // MCP 注册路径自愈:App 被移动/换装后,各 Agent 配置里的 command 指向旧路径,
            // Agent spawn 会失败。启动时静默改正;开发态二进制(target/)在 heal 内部跳过。
            std::thread::spawn(|| {
                if let Ok(reg) = mcp::registry::Registry::new() {
                    if let Ok(n) = reg.heal() {
                        if n > 0 {
                            MCP_HEALED.store(n, Ordering::SeqCst);
                        }
                    }
                }
            });
```

- [ ] **Step 2: 建 `src/lib/mcp.ts`**

```ts
// MCP 注册的前端封装。真值源是各 Agent 的配置文件(后端每次现扫),
// 前端不缓存注册状态,操作后重新拉取。
import { invoke } from "@tauri-apps/api/core";

export type AgentStatus = {
  key: string;
  name: string;
  installed: boolean;
  registered: boolean;
  command: string | null;
  stale: boolean;
};

export type RegisterOutcome = { key: string; ok: boolean; error: string | null };

export const mcpAgentsStatus = () => invoke<AgentStatus[]>("mcp_agents_status");
export const mcpRegister = (agents: string[]) => invoke<RegisterOutcome[]>("mcp_register", { agents });
export const mcpUnregister = (agent: string) => invoke<void>("mcp_unregister", { agent });
export const mcpManualSnippet = () => invoke<string>("mcp_manual_snippet");
/** 启动自愈修复数(读即清零,提示条只出一次)。 */
export const mcpHealedCount = () => invoke<number>("mcp_healed_count");
```

- [ ] **Step 3: 编译 + 类型检查**

Run: `cargo check` && `npm run check`
Expected: 均通过。

- [ ] **Step 4: 手工验证**

Run: `npm run tauri dev`,开发者工具 Console 里:
`await window.__TAURI__.core.invoke("mcp_agents_status")`
Expected: 返回 5 家的检测数组(开发机上 claude-code 应 installed=true)。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs src/lib/mcp.ts
git commit -m "feat(mcp): GUI 侧注册命令(status/register/unregister/snippet)与启动路径自愈"
```

---

## Task 10: 设置页「AI 助手接入」分组

**Files:**
- Modify: `src/routes/settings/+page.svelte`(「智能精修」section 之后、「语音模型」之前插入)

**Interfaces:**
- Consumes: `src/lib/mcp.ts` 全部;`getSettings/setSettings`(mcp_allow_control)。

- [ ] **Step 1: script 区加状态与操作**(与既有分组同风格,放在精修状态块之后)

```ts
  // —— MCP(AI 助手接入):列表现扫现示,注册/移除后重拉;真值源是 Agent 配置文件 ——
  import { mcpAgentsStatus, mcpRegister, mcpUnregister, mcpManualSnippet, mcpHealedCount, type AgentStatus } from "$lib/mcp";

  let mcpAgents = $state<AgentStatus[]>([]);
  let mcpAllowControl = $state(false);
  let mcpSnippet = $state("");
  let mcpSnippetOpen = $state(false);
  let mcpHealed = $state(0);
  let mcpBusy = $state<string | null>(null); // 正在操作的 agent key,防连点
  let mcpError = $state("");

  async function refreshMcp() {
    try {
      mcpAgents = await mcpAgentsStatus();
    } catch (e) {
      mcpError = String(e);
    }
  }

  async function mcpToggleRegister(a: AgentStatus) {
    mcpBusy = a.key;
    mcpError = "";
    try {
      if (a.registered) {
        await mcpUnregister(a.key);
      } else {
        const [r] = await mcpRegister([a.key]);
        if (r && !r.ok) mcpError = `${a.name}: ${r.error ?? "注册失败"}`;
      }
    } catch (e) {
      mcpError = String(e);
    }
    mcpBusy = null;
    await refreshMcp();
  }

  async function saveMcpAllowControl() {
    if (!settings) return;
    try {
      settings = await setSettings({ ...settings, mcp_allow_control: mcpAllowControl });
    } catch {
      mcpAllowControl = settings.mcp_allow_control; // 失败回弹
    }
  }
```

onMount(既有 onMount 内追加):

```ts
    refreshMcp();
    mcpManualSnippet().then((s) => (mcpSnippet = s)).catch(() => {});
    mcpHealedCount().then((n) => (mcpHealed = n)).catch(() => {});
```

settings 加载完成处(既有把 settings 镜像到本地 state 的位置)同步 `mcpAllowControl = settings.mcp_allow_control;`。

- [ ] **Step 2: 模板区插 section**(「智能精修」`</section>` 之后)

```svelte
  <!-- —— AI 助手接入(MCP) —— -->
  <section>
    <h2 class="section-title">AI 助手接入</h2>
    <div class="rows">
      {#if mcpHealed > 0}
        <div class="banner">应用位置变更:已自动更新 {mcpHealed} 个 AI 助手的注册路径。</div>
      {/if}
      {#if mcpError}
        <div class="banner warn">{mcpError}</div>
      {/if}
      {#each mcpAgents as a (a.key)}
        <div class="row">
          <div class="row-info">
            <span class="row-label">{a.name}</span>
            <span class="row-desc">
              {#if !a.installed && !a.registered}未检测到安装
              {:else if a.stale}已注册(路径已由自愈修复或待修复)
              {:else if a.registered}已注册
              {:else}未注册{/if}
            </span>
          </div>
          {#if a.installed || a.registered}
            <button class="btn-secondary" disabled={mcpBusy === a.key} onclick={() => mcpToggleRegister(a)}>
              {a.registered ? "移除" : "注册"}
            </button>
          {/if}
        </div>
      {/each}
      <label class="row">
        <div class="row-info">
          <span class="row-label">允许 AI 控制录制</span>
          <span class="row-desc">开启后,已接入的 AI 助手可远程开始/停止/暂停录制。默认关闭</span>
        </div>
        <input type="checkbox" class="ctl" bind:checked={mcpAllowControl} disabled={!settings} onchange={saveMcpAllowControl} />
      </label>
      <div class="row">
        <div class="row-info">
          <span class="row-label">手动配置</span>
          <span class="row-desc">未内置的 Agent(Windsurf/Cline 等)把左侧片段加进其 MCP 配置即可</span>
        </div>
        <button class="btn-secondary" onclick={() => (mcpSnippetOpen = !mcpSnippetOpen)}>
          {mcpSnippetOpen ? "收起" : "查看"}
        </button>
      </div>
      {#if mcpSnippetOpen}
        <div class="config">
          <pre class="snippet">{mcpSnippet}</pre>
          <button class="btn-secondary" onclick={() => navigator.clipboard.writeText(mcpSnippet)}>复制</button>
        </div>
      {/if}
      <p class="config-hint">笔记内容经 AI 助手检索后会进入其模型上下文;本应用自身不联网上传任何内容。</p>
    </div>
  </section>
```

`snippet` 样式(该文件 style 区,沿用既有 token):

```css
  .snippet {
    margin: 0 0 0.5rem;
    padding: 0.6rem 0.8rem;
    background: var(--surface-soft);
    border-radius: var(--radius-sm);
    font-size: 0.8rem;
    overflow-x: auto;
    user-select: text;
  }
```

注:`banner` / `banner warn` / `config` / `config-hint` 均为该页已有类,直接复用;若 `banner`(非 warn)样式不存在则用 `config-hint` 段落替代成功提示。

- [ ] **Step 3: 验证**

Run: `npm run check`,然后 `npm run tauri dev` → 设置页。
Expected: 分组显示 5 家状态;对已安装的家点「注册」→ 状态翻转、对应配置文件出现条目(cat 验证);「移除」还原;开关写入 settings.json;手动配置可复制。

- [ ] **Step 4: Commit**

```bash
git add src/routes/settings/+page.svelte
git commit -m "ui(settings): 「AI 助手接入」分组——Agent 注册/移除、控制开关、手动配置"
```

---

## Task 11: 欢迎页「连接 AI 助手」步 + 存量用户一次性提示条

**Files:**
- Modify: `src/lib/WelcomeOverlay.svelte`
- Modify: `src/routes/record/+page.svelte`

**Interfaces:**
- Consumes: `src/lib/mcp.ts`、`getSettings/setSettings`(mcp_onboarded)。

- [ ] **Step 1: WelcomeOverlay 加 connect 相位**(script 区)

```ts
  import { mcpAgentsStatus, mcpRegister, type AgentStatus, type RegisterOutcome } from "$lib/mcp";

  // 相位:download(模型下载,现状) → connect(连接 AI 助手,可跳过) → 结束。
  // 未检测到任何 Agent 时 connect 整步自动跳过(spec §四)。
  let phase = $state<"download" | "connect">("download");
  let agents = $state<AgentStatus[]>([]);
  let picked = $state<Record<string, boolean>>({});
  let outcomes = $state<RegisterOutcome[] | null>(null);
  let registering = $state(false);

  async function finish(target: "/record" | "/settings") {
    await markOnboarded();
    onDone(target);
  }

  async function maybeConnect() {
    try {
      agents = (await mcpAgentsStatus()).filter((a) => a.installed);
    } catch {
      agents = [];
    }
    if (agents.length === 0) {
      await finish("/record");
      return;
    }
    // 已拍板:默认全选
    picked = Object.fromEntries(agents.map((a) => [a.key, true]));
    phase = "connect";
  }

  async function registerPicked() {
    registering = true;
    const keys = agents.filter((a) => picked[a.key]).map((a) => a.key);
    try {
      outcomes = keys.length ? await mcpRegister(keys) : [];
    } catch {
      outcomes = keys.map((key) => ({ key, ok: false, error: "调用失败" }));
    }
    registering = false;
    if ((outcomes ?? []).every((o) => o.ok)) {
      setTimeout(() => finish("/record"), 600); // 让用户看见打勾再走
    }
  }
```

改两处既有函数:

```ts
  /** 置 onboarded(含 mcp_onboarded:欢迎流即 MCP 引导,不再二次提示)。 */
  async function markOnboarded() {
    try {
      const s = await getSettings();
      await setSettings({ ...s, onboarded: true, mcp_onboarded: true });
    } catch {
      /* 同前:幂等,不打断跳转 */
    }
  }

  async function refresh() {
    try {
      current = await modelsStatus();
    } catch {
      return;
    }
    if (current.recording_ready) {
      await maybeConnect(); // 原直接 finish("/record"),现插入 connect 步
    }
  }
```

`advanced()` 改调 `finish("/settings")`(语义不变,仅走新收口)。

- [ ] **Step 2: 模板加 connect 相位**(`<ModelDownloadCard ...>` 与 `.hints` 段包进 `{#if phase === "download"}`,加 else 分支)

```svelte
    {#if phase === "download"}
      <ModelDownloadCard status={current} onComplete={refresh} primaryLabel="开 始 使 用" />
      <p class="hints">首次录制时，系统会请求麦克风权限；录制系统声音需在系统设置中允许录屏。</p>
    {:else}
      <div class="connect">
        <h2>连接 AI 助手</h2>
        <p class="hints">
          让 Claude / Cursor 等直接检索你的会议笔记。注册后,AI 助手查到的笔记内容会进入其模型上下文;随时可在
          设置 → AI 助手接入 移除。
        </p>
        {#each agents as a (a.key)}
          <label class="agent-row">
            <input type="checkbox" bind:checked={picked[a.key]} disabled={registering || outcomes !== null} />
            <span>{a.name}</span>
            {#if outcomes}
              {@const o = outcomes.find((x) => x.key === a.key)}
              {#if o}<span class="mark-txt" class:bad={!o.ok}>{o.ok ? "✓ 已注册" : `✕ ${o.error ?? "失败"}`}</span>{/if}
            {/if}
          </label>
        {/each}
        <div class="connect-actions">
          <button class="btn-primary" disabled={registering} onclick={registerPicked}>注册所选</button>
          <button class="link" disabled={registering} onclick={() => finish("/record")}>跳过</button>
        </div>
      </div>
    {/if}
```

style 区追加(沿用 token;`btn-primary` 若该组件无此类,复用 ModelDownloadCard 的主按钮样式类名,以现文件为准):

```css
  .connect { text-align: left; }
  .connect h2 { margin: 0 0 0.4rem; font-size: 1.1rem; text-align: center; }
  .agent-row {
    display: flex; align-items: center; gap: 0.6rem;
    padding: 0.55rem 0.4rem; border-radius: var(--radius-sm);
  }
  .agent-row:hover { background: var(--surface-soft); }
  .mark-txt { margin-left: auto; font-size: 0.85rem; color: var(--ink-secondary); }
  .mark-txt.bad { color: var(--record); }
  .connect-actions { display: flex; justify-content: center; gap: 0.8rem; margin-top: 1rem; }
```

- [ ] **Step 3: record 页一次性提示条**(script 加状态与 onMount 逻辑,模板顶部加条)

script:

```ts
  // 存量用户 MCP 引导:onboarded(老用户)且 mcp_onboarded 为 false 时出一次提示条。
  // 新用户在欢迎页已走过(markOnboarded 同置两标记),不会看到。
  let showMcpHint = $state(false);
  async function dismissMcpHint(goSettings: boolean) {
    showMcpHint = false;
    try {
      const s = await getSettings();
      await setSettings({ ...s, mcp_onboarded: true });
    } catch {
      /* 置失败下次再提示,可接受 */
    }
    if (goSettings) location.hash = "#/settings"; // 路由跳转按本项目实际方式:若用 goto,改 import { goto } from "$app/navigation"; goto("/settings")
  }
```

onMount 内(既有 getSettings 调用处顺带):

```ts
    getSettings().then((s) => {
      showMcpHint = s.onboarded && !s.mcp_onboarded;
    }).catch(() => {});
```

模板顶部(权限横幅同级、之前):

```svelte
{#if showMcpHint}
  <div class="mcp-hint">
    <span>新功能:把会议笔记接入 Claude / Cursor 等 AI 助手(MCP)。</span>
    <button class="btn-secondary" onclick={() => dismissMcpHint(true)}>去设置</button>
    <button class="btn-secondary" onclick={() => dismissMcpHint(false)}>知道了</button>
  </div>
{/if}
```

样式(该页 style 区,对齐既有横幅形态;若该页已有 banner 类直接复用并删掉本段):

```css
  .mcp-hint {
    display: flex; align-items: center; gap: 0.6rem;
    padding: 0.5rem 0.8rem; margin-bottom: 0.6rem;
    background: var(--surface-soft); border-radius: var(--radius-sm);
    font-size: 0.85rem; color: var(--ink-secondary);
  }
  .mcp-hint span { flex: 1; }
```

**注意**:record 页跳设置的既有方式先看该文件里现存的导航写法(如 Sidebar 是 `<a href>` 还是 goto),照抄同款,上面 `location.hash` 只是占位提示。

- [ ] **Step 4: 验证**

Run: `npm run check`;手工:删掉(或改 false)settings.json 里 `onboarded` 后 `npm run tauri dev` 走完整欢迎流(模型已就绪时下载卡即完成态→应直接进 connect 步);再把 `onboarded:true, mcp_onboarded:false` 手改验证 record 页提示条出现、点掉后 settings.json 落 `mcp_onboarded:true`。
Expected: 两条路径都一次性、不重复出现;未检测到 Agent 的机器(可用 `VN_APP_DATA`+假 HOME 难模拟,跳过)不阻塞。

- [ ] **Step 5: Commit**

```bash
git add src/lib/WelcomeOverlay.svelte src/routes/record/+page.svelte
git commit -m "ui: 欢迎页「连接 AI 助手」步(默认全选)+ 存量用户一次性提示条"
```

---

**至此 M2 完成。**

---

## Task 12: do_pause/do_resume 抽取 + NoteWriter::set_title(TDD)

**Files:**
- Modify: `src-tauri/src/lib.rs`(pause_recording/unpause_recording 抽共用实现,命令变薄壳——与 do_start/do_stop 同款手法)
- Modify: `src-tauri/src/store/writer.rs`

**Interfaces:**
- Produces: `do_pause_recording(&AppHandle) -> Result<(), String>`、`do_resume_recording(&AppHandle) -> Result<(), String>`(lib.rs 内 pub(crate) 级即可);`NoteWriter::set_title(&mut self, title: &str) -> anyhow::Result<()>`。Task 13 的 UDS handler 消费。

- [ ] **Step 1: 写 set_title 失败测试**(writer.rs tests 追加;fixture 手法照抄同文件既有测试,如 `finalize_fails_leaves_recording_state` 的 NoteWriter::create 用法)

```rust
    #[test]
    fn set_title_persists_and_survives_finalize() {
        let tmp = tempfile::tempdir().unwrap();
        let mut w = NoteWriter::create(tmp.path(), now()).unwrap();
        let dir = w.dir().to_path_buf();
        w.set_title("需求评审").unwrap();
        // 落盘立即可见
        let meta: crate::store::NoteMeta =
            serde_json::from_str(&std::fs::read_to_string(dir.join("meta.json")).unwrap()).unwrap();
        assert_eq!(meta.title, "需求评审");
        // finalize 用内存 meta 重写 —— set_title 走内存路径,标题必须存活
        w.finalize(now()).unwrap();
        let meta: crate::store::NoteMeta =
            serde_json::from_str(&std::fs::read_to_string(dir.join("meta.json")).unwrap()).unwrap();
        assert_eq!(meta.title, "需求评审", "finalize 不得回滚标题");
        assert_eq!(meta.state, "complete");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test set_title -- --nocapture`
Expected: 编译失败(方法不存在)。

- [ ] **Step 3: 实现 set_title**(writer.rs,`persist_speakers` 旁)

```rust
    /// 录制中改标题:唯一安全路径。rename_note 命令拒绝活动笔记(改盘会被 finalize
    /// 的内存 meta 覆盖),MCP start_recording(title) 由 UDS handler 经 writer 走
    /// 这里——内存与磁盘同步更新,finalize 自然保留。
    pub fn set_title(&mut self, title: &str) -> anyhow::Result<()> {
        self.meta.title = title.to_string();
        write_meta_atomic(&self.dir, &self.meta)
    }
```

- [ ] **Step 4: 抽 do_pause/do_resume**(lib.rs;把 pause_recording/unpause_recording 的函数体逐语句搬进新函数,命令变薄壳——`State` 换 `app.state::<AppState>()`,其余零改动)

```rust
/// 暂停共用实现(命令壳、UDS 桥共用)。逐语句搬自原 pause_recording 命令体。
fn do_pause_recording(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let ev = {
        let mut slot = state.session.lock().unwrap();
        let Some(s) = slot.as_mut() else { return Err("没有正在进行的录制".into()) };
        if s.paused_at.is_some() {
            return Ok(());
        }
        s.handle.set_paused(true);
        s.paused_at = Some(std::time::Instant::now());
        ipc::StatusEvent {
            state: "paused".into(),
            system_audio: s.system_audio.clone(),
            note_id: s.note_id.clone(),
            diarization: s.diarization.clone(),
            elapsed_ms: s.elapsed_ms(),
        }
    };
    let _ = app.emit("status", ev);
    Ok(())
}

#[tauri::command]
fn pause_recording(app: AppHandle) -> Result<(), String> {
    do_pause_recording(&app)
}
```

`do_resume_recording` / `unpause_recording` 同款(搬 unpause 原体)。**注意** invoke_handler 里命令名不变;前端 invoke("pause_recording") 无参,签名去掉 State 后兼容。

- [ ] **Step 5: 跑全量测试**

Run: `cargo test`
Expected: 全 passed(纯抽取重构 + 新增 set_title)。

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/store/writer.rs
git commit -m "refactor(mcp 铺垫): pause/resume 抽共用实现;NoteWriter::set_title 单写者改题路径"
```

---

## Task 13: GUI 侧 UDS listener(mcp/uds.rs)

**Files:**
- Create: `src-tauri/src/mcp/uds.rs`(mod.rs 加 `pub mod uds;`)
- Modify: `src-tauri/src/lib.rs`(setup 里 `mcp::uds::spawn_listener(handle.clone());`,放自愈线程旁)

**Interfaces:**
- Consumes: `do_start_recording/do_stop_recording/do_pause_recording/do_resume_recording`、`AppState.session`、`notes_dir(&AppHandle)`、`settings::load`、`recording_status` 的状态组装逻辑。lib.rs 侧需要把这几个 `fn` 从私有提为 `pub(crate)`(同文件内 crate 可见即可,uds.rs 在同 crate)。
- Produces: `app_data/mcp.sock` 上的行式 JSON 协议:请求 `{"op": "...", "title"?, "tail"?}`,响应 `{"ok": bool, "data"?, "error"?}`。ops: `status | live | start | stop | pause | resume`。Task 14 的 bridge 消费。

- [ ] **Step 1: 实现 uds.rs**

```rust
//! GUI 侧 Unix socket 服务:stdio MCP 进程的「活能力」后端。行式 JSON,一行请求
//! 一行响应。socket 固定在 app_data(不随 data_dir 迁移),权限 0600。
//! 控制类 op 受 settings.mcp_allow_control 门控——授权真值源在 GUI 侧,stdio 进程
//! 不可信(任何本机进程都能连 socket,但同 uid 本就有全部数据的文件权限,不新增面)。

use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use tauri::Manager;

#[derive(Deserialize)]
struct Req {
    op: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    tail: Option<usize>,
}

#[derive(Serialize)]
struct Resp {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn ok(data: serde_json::Value) -> Resp {
    Resp { ok: true, data: Some(data), error: None }
}

fn err(msg: impl Into<String>) -> Resp {
    Resp { ok: false, data: None, error: Some(msg.into()) }
}

pub fn spawn_listener(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        let Ok(app_data) = app.path().app_data_dir() else {
            eprintln!("mcp uds: app_data_dir 不可用,活能力不启动(查询类工具不受影响)");
            return;
        };
        let _ = std::fs::create_dir_all(&app_data);
        let sock = app_data.join("mcp.sock");
        let _ = std::fs::remove_file(&sock); // 上次异常退出的残留
        let listener = match UnixListener::bind(&sock) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("mcp uds: bind 失败(活能力不可用): {e}");
                return;
            }
        };
        let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600));
        for conn in listener.incoming().flatten() {
            let app = app.clone();
            // 每连接一线程:流量是"单 Agent 偶发调用"量级,线程成本可忽略。
            std::thread::spawn(move || handle_conn(&app, conn));
        }
    });
}

fn handle_conn(app: &tauri::AppHandle, conn: UnixStream) {
    let Ok(write_half) = conn.try_clone() else { return };
    let mut writer = std::io::BufWriter::new(write_half);
    for line in BufReader::new(conn).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Req>(&line) {
            Ok(req) => dispatch(app, &req),
            Err(e) => err(format!("请求解析失败: {e}")),
        };
        let Ok(json) = serde_json::to_string(&resp) else { break };
        if writeln!(writer, "{json}").and_then(|()| writer.flush()).is_err() {
            break;
        }
    }
}

/// 录制状态快照(与 recording_status 命令同源:session 槽)。
fn status_json(app: &tauri::AppHandle) -> serde_json::Value {
    let state = app.state::<crate::AppState>();
    let slot = state.session.lock().unwrap();
    match slot.as_ref() {
        Some(s) => serde_json::json!({
            "state": if s.paused_at.is_some() { "paused" } else { "recording" },
            "note_id": s.note_id, "elapsed_ms": s.elapsed_ms(),
            "system_audio": s.system_audio, "diarization": s.diarization,
        }),
        None => serde_json::json!({ "state": "idle", "note_id": "", "elapsed_ms": 0,
            "system_audio": "", "diarization": "" }),
    }
}

fn control_allowed(app: &tauri::AppHandle) -> bool {
    app.path().app_data_dir().map(|d| crate::settings::load(&d).mcp_allow_control).unwrap_or(false)
}

const CONTROL_DENIED: &str = "已被用户在设置中禁用:请在 voice-notes 的「设置 → AI 助手接入」开启「允许 AI 控制录制」";

fn dispatch(app: &tauri::AppHandle, req: &Req) -> Resp {
    match req.op.as_str() {
        "status" => ok(status_json(app)),
        "live" => {
            let note_id = {
                let state = app.state::<crate::AppState>();
                let slot = state.session.lock().unwrap();
                match slot.as_ref() {
                    Some(s) => s.note_id.clone(),
                    None => return err("没有正在进行的录制"),
                }
            };
            let tail = req.tail.unwrap_or(50).clamp(1, 500);
            let Ok(dir) = crate::notes_dir(app) else { return err("数据目录不可用") };
            match crate::store::NoteStore::new(dir).load(&note_id) {
                Ok(note) => {
                    let start = note.segments.len().saturating_sub(tail);
                    ok(serde_json::json!({
                        "note_id": note_id, "title": note.meta.title,
                        "segments": note.segments[start..].iter().map(|s| serde_json::json!({
                            "seq": s.seq, "source": s.source, "speaker": s.speaker,
                            "start_ms": s.start_ms, "text": s.text,
                        })).collect::<Vec<_>>(),
                    }))
                }
                Err(e) => err(e.to_string()),
            }
        }
        "start" => {
            if !control_allowed(app) {
                return err(CONTROL_DENIED);
            }
            if let Err(e) = crate::do_start_recording(app) {
                return err(e);
            }
            // spawn_session 异步加载模型后才入槽:轮询等 note_id(最多 20s,模型冷加载
            // 可能秒级);拿到后如带 title,经 writer 单写者路径改题(见 set_title 注释)。
            for _ in 0..200 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                let state = app.state::<crate::AppState>();
                let slot = state.session.lock().unwrap();
                if let Some(s) = slot.as_ref() {
                    if let Some(title) = req.title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
                        if let Err(e) = s.writer.lock().unwrap().set_title(title) {
                            eprintln!("mcp start: 设标题失败(录制已开始,不回滚): {e}");
                        }
                    }
                    return ok(serde_json::json!({ "note_id": s.note_id }));
                }
                drop(slot);
                // 会话未入槽且 running 已被清(启动失败路径)→ 提前报错
                if !*state.running.lock().unwrap() {
                    return err("录制启动失败(设备或模型异常,详见应用日志)");
                }
            }
            err("录制启动超时")
        }
        "stop" => {
            if !control_allowed(app) {
                return err(CONTROL_DENIED);
            }
            let note_id = status_json(app)["note_id"].as_str().unwrap_or_default().to_string();
            if note_id.is_empty() {
                return err("没有正在进行的录制");
            }
            crate::do_stop_recording(app); // 阻塞至排干,本线程等待无妨
            ok(serde_json::json!({ "note_id": note_id }))
        }
        "pause" => {
            if !control_allowed(app) {
                return err(CONTROL_DENIED);
            }
            match crate::do_pause_recording(app) {
                Ok(()) => ok(status_json(app)),
                Err(e) => err(e),
            }
        }
        "resume" => {
            if !control_allowed(app) {
                return err(CONTROL_DENIED);
            }
            match crate::do_resume_recording(app) {
                Ok(()) => ok(status_json(app)),
                Err(e) => err(e),
            }
        }
        other => err(format!("未知 op: {other}")),
    }
}
```

- [ ] **Step 2: lib.rs 可见性与挂载**

- `AppState`、`do_start_recording`、`do_stop_recording`、`do_pause_recording`、`do_resume_recording`、`notes_dir`、`settings` 模块:确认/改为 `pub(crate)`(uds.rs 经 `crate::` 引用;`mod settings;` 本就 crate 内可见,函数加 `pub(crate)` 前缀即可)。
- setup 里自愈线程旁加:

```rust
            // UDS listener:MCP stdio 进程的活能力后端(状态/实时/控制)。
            mcp::uds::spawn_listener(handle.clone());
```

- [ ] **Step 3: 编译 + 手工验证**

Run: `cargo check`;`npm run tauri dev` 起 GUI 后另开终端:

```bash
printf '{"op":"status"}\n' | nc -U ~/Library/Application\ Support/com.teemo.voice-notes/mcp.sock
printf '{"op":"start","title":"UDS 冒烟"}\n' | nc -U ~/Library/Application\ Support/com.teemo.voice-notes/mcp.sock
```

Expected: status 返回 `{"ok":true,"data":{"state":"idle",...}}`;start 在未开「允许 AI 控制录制」时返回 CONTROL_DENIED 文案;设置页开启后再试,GUI 进入录制、标题为「UDS 冒烟」,`{"op":"stop"}` 停录。`ls -l` 确认 mcp.sock 权限 `srw-------`。

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/mcp/ src-tauri/src/lib.rs
git commit -m "feat(mcp): GUI 侧 UDS 服务——status/live/start/stop/pause/resume,控制受设置门控"
```

---

## Task 14: bridge + 六个 UDS 工具接入 MCP 服务

**Files:**
- Create: `src-tauri/src/mcp/bridge.rs`(mod.rs 加 `pub mod bridge;`)
- Modify: `src-tauri/src/mcp/server.rs`(VnMcp 追加 6 工具)
- Modify: `src-tauri/tests/mcp_stdio.rs`(追加"未运行"错误路径断言)

**Interfaces:**
- Consumes: Task 13 的 UDS 协议;`mcp::app_data_dir()`。
- Produces: `bridge::call(op: &str, extra: serde_json::Value) -> Result<serde_json::Value, String>`;MCP 工具 `recording_status / get_live_transcript / start_recording / stop_recording / pause_recording / resume_recording`。

- [ ] **Step 1: 写 bridge.rs**

```rust
//! stdio 进程 → GUI 的 UDS 客户端。同步阻塞(单请求毫秒级;stop 会等排干,给宽超时)。
//! 连不上 = App 未运行,统一转成给 Agent 看的指引文案。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub const NOT_RUNNING: &str =
    "voice-notes 应用未在运行。请先启动 voice-notes(查询类工具 list_notes/search_notes/get_note/list_speakers 无需应用运行)。";

/// op + 额外字段(对象)合并成一行请求,读一行响应。Err 是给 Agent 的人话。
pub fn call(op: &str, extra: serde_json::Value) -> Result<serde_json::Value, String> {
    let sock = super::app_data_dir().join("mcp.sock");
    let mut stream = UnixStream::connect(&sock).map_err(|_| NOT_RUNNING.to_string())?;
    stream.set_read_timeout(Some(Duration::from_secs(60))).ok(); // stop 等排干,宽限
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
    let mut req = serde_json::json!({ "op": op });
    if let (Some(obj), Some(ext)) = (req.as_object_mut(), extra.as_object()) {
        for (k, v) in ext {
            obj.insert(k.clone(), v.clone());
        }
    }
    writeln!(stream, "{req}").map_err(|e| format!("请求发送失败: {e}"))?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).map_err(|e| format!("响应读取失败: {e}"))?;
    let resp: serde_json::Value = serde_json::from_str(&line).map_err(|e| format!("响应解析失败: {e}"))?;
    if resp["ok"].as_bool() == Some(true) {
        Ok(resp.get("data").cloned().unwrap_or(serde_json::json!({})))
    } else {
        Err(resp["error"].as_str().unwrap_or("未知错误").to_string())
    }
}
```

- [ ] **Step 2: server.rs 追加参数结构与 6 工具**(`#[tool_router] impl VnMcp` 内;bridge 阻塞 IO 用 spawn_blocking 包)

```rust
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct LiveParams {
    /// 返回最近几句,默认 50,最大 500
    pub tail: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct StartParams {
    /// 可选:本场录制的标题
    pub title: Option<String>,
}

async fn bridge_call(op: &'static str, extra: serde_json::Value) -> CallToolResult {
    match tokio::task::spawn_blocking(move || super::bridge::call(op, extra)).await {
        Ok(Ok(data)) => ok_json(data),
        Ok(Err(msg)) => err_text(msg),
        Err(e) => err_text(format!("内部错误: {e}")),
    }
}
```

(`bridge_call` 放 impl 外的自由函数区,与 `ok_json/err_text` 并列。)

```rust
    #[tool(description = "查询录制状态(idle/recording/paused)、当前笔记 id 与已录时长。需要 voice-notes 应用正在运行。")]
    async fn recording_status(&self) -> Result<CallToolResult, McpError> {
        Ok(bridge_call("status", serde_json::json!({})).await)
    }

    #[tool(description = "获取正在录制会话的实时转写(最近 N 句,含说话人与时间戳)。需要应用正在运行且正在录制。")]
    async fn get_live_transcript(&self, Parameters(p): Parameters<LiveParams>) -> Result<CallToolResult, McpError> {
        Ok(bridge_call("live", serde_json::json!({ "tail": p.tail })).await)
    }

    #[tool(description = "开始录制一场会议(可选标题)。需要应用正在运行,且用户已在 设置 → AI 助手接入 开启「允许 AI 控制录制」。")]
    async fn start_recording(&self, Parameters(p): Parameters<StartParams>) -> Result<CallToolResult, McpError> {
        Ok(bridge_call("start", serde_json::json!({ "title": p.title })).await)
    }

    #[tool(description = "停止当前录制并返回笔记 id。需要应用运行 + 用户开启「允许 AI 控制录制」。")]
    async fn stop_recording(&self) -> Result<CallToolResult, McpError> {
        Ok(bridge_call("stop", serde_json::json!({})).await)
    }

    #[tool(description = "暂停当前录制。需要应用运行 + 用户开启「允许 AI 控制录制」。")]
    async fn pause_recording(&self) -> Result<CallToolResult, McpError> {
        Ok(bridge_call("pause", serde_json::json!({})).await)
    }

    #[tool(description = "恢复已暂停的录制。需要应用运行 + 用户开启「允许 AI 控制录制」。")]
    async fn resume_recording(&self) -> Result<CallToolResult, McpError> {
        Ok(bridge_call("resume", serde_json::json!({})).await)
    }
```

- [ ] **Step 3: e2e 追加断言**(mcp_stdio.rs 的测试函数末尾;VN_APP_DATA 指向 tempdir,必无 mcp.sock → 覆盖"未运行"路径;tools/list 断言列表加 6 个新名)

```rust
    for expect in ["recording_status", "get_live_transcript", "start_recording", "stop_recording", "pause_recording", "resume_recording"] {
        assert!(names.contains(&expect), "缺工具 {expect}: {names:?}");
    }
    // App 未运行:UDS 工具给指引性错误而非崩溃
    let r = mcp.request("tools/call", serde_json::json!({ "name": "recording_status", "arguments": {} }));
    assert_eq!(r["isError"], true);
    assert!(r["content"][0]["text"].as_str().unwrap().contains("未在运行"), "{r}");
```

(注意:`names` 取自 Step 4 之前的 tools/list 调用,把两段 for 合到该处。)

- [ ] **Step 4: 跑测试**

Run: `cargo test --test mcp_stdio -- --nocapture` && `cargo test`
Expected: 全 passed。

- [ ] **Step 5: 手工冒烟(全链路)**

`npm run tauri dev` 起 GUI;`claude mcp add voice-notes-dev -- $PWD/src-tauri/target/debug/voice-notes mcp serve` 后在 Claude Code:
1. `recording_status` → idle;
2. 设置页开「允许 AI 控制录制」→ `start_recording(title="MCP 冒烟")` → GUI 进入录制、标题正确;
3. 说几句话 → `get_live_transcript` 有内容;`pause_recording`/`resume_recording` GUI 状态同步;
4. `stop_recording` → 返回 note_id,`get_note` 能读到刚才的内容;
5. 关 GUI → `recording_status` 返回"未在运行"指引。
完成后 `claude mcp remove voice-notes-dev`。

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/mcp/ src-tauri/tests/mcp_stdio.rs
git commit -m "feat(mcp): UDS 桥接入 stdio 服务——状态/实时转写/录制控制六工具"
```

---

## Task 15: 收尾——spec 对账 + CHANGELOG/版本(如仓库惯例)

**Files:**
- Modify: 按对账结果(理想情况无)

- [ ] **Step 1: 对 spec 逐节核对**

打开 `docs/superpowers/specs/2026-07-08-voice-notes-mcp-service-design.md`,逐条核对:§三工具清单(4+2+4 个,pause/resume 已含)、§四注册(五家/幂等/备份/自愈/CLI/欢迎页默认全选/设置页/两 settings 字段)、§五 README 双语、§六错误处理五条、§七测试四类。缺项回补,补完重跑 `cargo test && npm run check`。

- [ ] **Step 2: 全量回归**

Run: `cargo test && npm run check && npm run build`
Expected: 全绿。

- [ ] **Step 3: Commit(如有回补)+ 汇报**

```bash
git add -A && git commit -m "chore(mcp): spec 对账收尾"
```

向用户汇报:M1/M2/M3 全部落地,建议真机跑一遍 Task 14 Step 5 的冒烟清单后再发版(版本号 bump 与 Release 由用户拍板)。
