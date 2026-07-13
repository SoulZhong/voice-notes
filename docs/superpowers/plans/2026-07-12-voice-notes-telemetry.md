# voice-notes 产品遥测 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 接入 Aptabase 匿名遥测：6 个枚举化事件（启动/录制起止/精修/导出/MCP 控制面），设置页可关，默认开启。

**Architecture:** Rust 命令层统一埋点。新增 `telemetry` 模块作唯一上报入口：事件与属性全部用 Rust enum 建模（编译期杜绝自由文本），`track()` 内部检查 App-Key 与设置开关，短路即静默。`tauri-plugin-aptabase` 负责批量缓冲/重试，上报失败无感。前端只加一个设置开关行 + 欢迎页一句告知。

**Tech Stack:** Tauri 2（Rust 后端 + SvelteKit/Svelte 5 前端）、tauri-plugin-aptabase 1.x、serde_json。

**Spec:** `docs/superpowers/specs/2026-07-12-voice-notes-telemetry-design.md`

## Global Constraints

- **隐私红线**：事件属性只允许固定枚举与数值桶。禁止上报：会议内容、笔记标题、说话人名、文件路径、API key、模型接入点 ID、任何自由文本。
- **上报绝不影响主流程**：不阻塞、不弹错、不写用户可见日志；任何失败静默。
- git 提交信息**不加任何 Claude 署名尾注**（不加 `Co-Authored-By`、不加 "Generated with Claude Code"）。
- Rust 测试在 `/Users/teemo/workspace-soul/voice-notes/src-tauri` 下跑 `cargo test`；前端检查在仓库根跑 `npm run check`。
- 注释风格跟随仓库现状：中文、说明「为什么」。行号基于 commit 66c775f，动手前先用锚点文本确认位置。
- MCP 查询类工具（list_notes/search 等）跑在独立 stdio 子进程、无 GUI 上下文，**首批不统计**；`mcp_tool_used` 只覆盖经 UDS 分发的 6 个控制面 op（status/live/start/stop/pause/resume）。

---

### Task 1: Settings 增加 `telemetry_enabled` 字段

**Files:**
- Modify: `src-tauri/src/settings.rs`（struct 行 15-98、Default impl 行 135-165、测试 mod 行 209 起）
- Modify: `src/lib/models.ts:16-62`（`Settings` type）

**Interfaces:**
- Produces: `Settings.telemetry_enabled: bool`（serde 默认 `true`），后续所有任务经 `settings::load(&dir).telemetry_enabled` 消费。

- [ ] **Step 1: 写失败测试**

在 `src-tauri/src/settings.rs` 底部现有 `#[cfg(test)]` mod 内追加：

```rust
#[test]
fn telemetry_default_on_and_roundtrip() {
    // 新装默认开
    assert!(Settings::default().telemetry_enabled);
    // 显式关闭可往返
    let mut s = Settings::default();
    s.telemetry_enabled = false;
    let json = serde_json::to_string(&s).unwrap();
    let back: Settings = serde_json::from_str(&json).unwrap();
    assert!(!back.telemetry_enabled);
    // 旧配置文件(无此键)反序列化默认开
    let mut v: serde_json::Value = serde_json::from_str(&json).unwrap();
    v.as_object_mut().unwrap().remove("telemetry_enabled");
    let old: Settings = serde_json::from_value(v).unwrap();
    assert!(old.telemetry_enabled);
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test telemetry_default_on_and_roundtrip`
Expected: 编译错误 `no field telemetry_enabled`。

- [ ] **Step 3: 加字段**

在 `Settings` struct 末尾（`mcp_onboarded` 字段之后，行 96 附近）追加：

```rust
    /// 匿名使用统计:仅上报功能使用计数与版本信息,绝不含会议内容;默认开,设置页可关。
    #[serde(default = "default_true")]
    pub telemetry_enabled: bool,
```

在 `impl Default for Settings`（行 135-165）末尾对应追加：

```rust
            telemetry_enabled: true,
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test telemetry_default_on_and_roundtrip`
Expected: PASS；再跑整个 `cargo test` 确认无回归。

- [ ] **Step 5: 前端类型同步**

`src/lib/models.ts` 的 `Settings` type 末尾（`mcp_onboarded: boolean;` 之后）追加：

```ts
  // 匿名使用统计:默认开,设置页可关;绝不含会议内容
  telemetry_enabled: boolean;
```

Run: `npm run check`
Expected: 0 errors（现状基线如有既存告警，不新增即可）。

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/settings.rs src/lib/models.ts
git commit -m "设置增加 telemetry_enabled:匿名使用统计开关,默认开"
```

---

### Task 2: telemetry 模块 + Aptabase 插件接入 + app_started

**Files:**
- Create: `src-tauri/src/telemetry.rs`
- Modify: `src-tauri/Cargo.toml`（[dependencies]，行 25 起插件块）
- Modify: `src-tauri/src/lib.rs`（mod 声明区；`run()` 行 2655-2686 的 Builder 链与 setup）

**Interfaces:**
- Consumes: Task 1 的 `Settings.telemetry_enabled`；`settings::load(&app_data_dir) -> Settings`（现有）。
- Produces: `telemetry::track(app: &AppHandle, event: telemetry::Event)`；`telemetry::Event`/`RecordSource`/`Provider`/`ExportFormat`/`McpOp` 枚举；`telemetry::APP_KEY` 常量。后续 Task 3/4 只依赖这些。

- [ ] **Step 1: 加依赖**

`src-tauri/Cargo.toml` 插件依赖块（`tauri-plugin-autostart = "2"` 之后）追加：

```toml
tauri-plugin-aptabase = "1"
```

Run: `cd src-tauri && cargo fetch`
Expected: 解析成功。若 1.x 与 tauri 2 出现版本冲突，查 https://github.com/aptabase/tauri-plugin-aptabase README 标注的 Tauri v2 兼容版本并改用之（调研时最新为 1.0.0，2025-03 发布，目标即 Tauri 2）。

- [ ] **Step 2: 写模块与失败测试**

创建 `src-tauri/src/telemetry.rs`，完整内容：

```rust
//! 匿名使用统计(遥测)薄封装——全应用唯一上报入口。
//!
//! 隐私红线:事件属性只允许固定枚举与数值桶。禁止上报会议内容、笔记标题、
//! 说话人名、文件路径、API key、模型接入点 ID、任何自由文本。事件与属性
//! 用枚举建模,从类型上杜绝自由字符串进属性;新增事件必须扩 Event 枚举。
//! 上报失败静默(插件内部批量缓冲+重试),绝不影响主流程。
//! 设计:docs/superpowers/specs/2026-07-12-voice-notes-telemetry-design.md

use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

/// Aptabase App-Key。空串 = 遥测整体停用(插件不注册、track 短路),
/// 代码可先合入,拿到 key 后填入即激活。形如 "A-EU-xxxxxxxxxx"。
pub const APP_KEY: &str = "";

/// 录制源类别。由设置推断而非实际启动结果:遥测只要低基数类别,不追精确。
#[derive(Debug, PartialEq)]
pub enum RecordSource {
    Mic,
    System,
    Both,
}

impl RecordSource {
    /// 仅录系统声 → system;否则 macOS 双源 both、其他平台 mic。
    pub fn from_settings(record_system_only: bool) -> Self {
        if record_system_only {
            RecordSource::System
        } else if cfg!(target_os = "macos") {
            RecordSource::Both
        } else {
            RecordSource::Mic
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            RecordSource::Mic => "mic",
            RecordSource::System => "system",
            RecordSource::Both => "both",
        }
    }
}

/// 精修 provider 类别。预设 base_url 前缀与前端 REFINE_PRESETS
/// (src/routes/ai/+page.svelte)对齐;匹配不上一律 custom,绝不报原始 URL。
#[derive(Debug, PartialEq)]
pub enum Provider {
    Deepseek,
    Qwen,
    Doubao,
    Kimi,
    Openai,
    Agent,
    Custom,
}

impl Provider {
    pub fn classify(refine_provider: &str, base_url: &str) -> Self {
        if refine_provider == "agent" {
            return Provider::Agent;
        }
        let u = base_url.trim();
        if u.starts_with("https://api.deepseek.com") {
            Provider::Deepseek
        } else if u.starts_with("https://dashscope.aliyuncs.com") {
            Provider::Qwen
        } else if u.starts_with("https://ark.cn-beijing.volces.com") {
            Provider::Doubao
        } else if u.starts_with("https://api.moonshot.cn") {
            Provider::Kimi
        } else if u.starts_with("https://api.openai.com") {
            Provider::Openai
        } else {
            Provider::Custom
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Provider::Deepseek => "deepseek",
            Provider::Qwen => "qwen",
            Provider::Doubao => "doubao",
            Provider::Kimi => "kimi",
            Provider::Openai => "openai",
            Provider::Agent => "agent",
            Provider::Custom => "custom",
        }
    }
}

/// 导出格式。命令层收到的是字符串,先 parse 成枚举再进属性。
#[derive(Debug, PartialEq)]
pub enum ExportFormat {
    Md,
    Txt,
}

impl ExportFormat {
    pub fn parse(format: &str) -> Option<Self> {
        match format {
            "md" => Some(ExportFormat::Md),
            "txt" => Some(ExportFormat::Txt),
            _ => None,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            ExportFormat::Md => "md",
            ExportFormat::Txt => "txt",
        }
    }
}

/// MCP 控制面 op(经 GUI 进程 UDS 分发的 6 个)。查询类工具跑在独立
/// stdio 子进程、无 GUI 上下文,首批不统计。
#[derive(Debug, PartialEq)]
pub enum McpOp {
    Status,
    Live,
    Start,
    Stop,
    Pause,
    Resume,
}

impl McpOp {
    pub fn parse(op: &str) -> Option<Self> {
        match op {
            "status" => Some(McpOp::Status),
            "live" => Some(McpOp::Live),
            "start" => Some(McpOp::Start),
            "stop" => Some(McpOp::Stop),
            "pause" => Some(McpOp::Pause),
            "resume" => Some(McpOp::Resume),
            _ => None,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            McpOp::Status => "status",
            McpOp::Live => "live",
            McpOp::Start => "start",
            McpOp::Stop => "stop",
            McpOp::Pause => "pause",
            McpOp::Resume => "resume",
        }
    }
}

/// 全部遥测事件(首批 6 个,见设计文档事件清单)。
pub enum Event {
    AppStarted,
    RecordingStarted { source: RecordSource },
    RecordingStopped { duration_ms: u64 },
    NoteRefined { provider: Provider },
    NoteExported { format: ExportFormat },
    McpToolUsed { op: McpOp },
}

/// 时长桶:精确时长不上报,只报 4 档。
fn duration_bucket(ms: u64) -> &'static str {
    let min = ms / 60_000;
    if min < 5 {
        "<5m"
    } else if min < 30 {
        "5-30m"
    } else if min < 60 {
        "30-60m"
    } else {
        ">1h"
    }
}

impl Event {
    /// (事件名, 属性)。属性值只可能来自各枚举 as_str 与 duration_bucket
    /// ——隐私红线的类型化落实,测试锁形状防回归。
    fn payload(&self) -> (&'static str, Option<Value>) {
        match self {
            Event::AppStarted => ("app_started", None),
            Event::RecordingStarted { source } => {
                ("recording_started", Some(json!({ "source": source.as_str() })))
            }
            Event::RecordingStopped { duration_ms } => (
                "recording_stopped",
                Some(json!({ "duration_bucket": duration_bucket(*duration_ms) })),
            ),
            Event::NoteRefined { provider } => {
                ("note_refined", Some(json!({ "provider": provider.as_str() })))
            }
            Event::NoteExported { format } => {
                ("note_exported", Some(json!({ "format": format.as_str() })))
            }
            Event::McpToolUsed { op } => {
                ("mcp_tool_used", Some(json!({ "tool": op.as_str() })))
            }
        }
    }
}

/// 上报门:key 未配置或用户关闭开关 → 不发。settings 读取失败按关闭处理
/// (宁可丢数据,不可在异常路径上报)。
fn gate(app_key: &str, telemetry_enabled: bool) -> bool {
    !app_key.is_empty() && telemetry_enabled
}

/// 唯一上报入口。每次现读设置(与 spawn_session/spawn_refine 同哲学,
/// 事件稀疏、读盘便宜),开关翻转即时生效、无需重启。
pub fn track(app: &AppHandle, event: Event) {
    let enabled = app
        .path()
        .app_data_dir()
        .map(|d| crate::settings::load(&d).telemetry_enabled)
        .unwrap_or(false);
    if !gate(APP_KEY, enabled) {
        return;
    }
    use tauri_plugin_aptabase::EventTracker;
    let (name, props) = event.payload();
    app.track_event(name, props);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_bucket_boundaries() {
        assert_eq!(duration_bucket(0), "<5m");
        assert_eq!(duration_bucket(5 * 60_000 - 1), "<5m");
        assert_eq!(duration_bucket(5 * 60_000), "5-30m");
        assert_eq!(duration_bucket(30 * 60_000), "30-60m");
        assert_eq!(duration_bucket(60 * 60_000), ">1h");
        assert_eq!(duration_bucket(3 * 60 * 60_000), ">1h");
    }

    #[test]
    fn provider_classify_presets() {
        assert_eq!(Provider::classify("agent", "https://api.openai.com/v1"), Provider::Agent);
        assert_eq!(Provider::classify("openai", "https://api.deepseek.com/v1"), Provider::Deepseek);
        assert_eq!(
            Provider::classify("openai", "https://dashscope.aliyuncs.com/compatible-mode/v1"),
            Provider::Qwen
        );
        assert_eq!(
            Provider::classify("openai", "https://ark.cn-beijing.volces.com/api/v3"),
            Provider::Doubao
        );
        assert_eq!(Provider::classify("openai", "https://api.moonshot.cn/v1"), Provider::Kimi);
        assert_eq!(Provider::classify("openai", "https://api.openai.com/v1"), Provider::Openai);
        // 自定义/未知 base_url 绝不透出原文
        assert_eq!(Provider::classify("openai", "https://my-private-gw.example.com"), Provider::Custom);
        assert_eq!(Provider::classify("openai", ""), Provider::Custom);
    }

    #[test]
    fn mcp_op_parse_known_only() {
        assert_eq!(McpOp::parse("start"), Some(McpOp::Start));
        assert_eq!(McpOp::parse("live"), Some(McpOp::Live));
        assert_eq!(McpOp::parse("drop_table"), None);
    }

    #[test]
    fn export_format_parse_known_only() {
        assert_eq!(ExportFormat::parse("md"), Some(ExportFormat::Md));
        assert_eq!(ExportFormat::parse("txt"), Some(ExportFormat::Txt));
        assert_eq!(ExportFormat::parse("../etc/passwd"), None);
    }

    /// 锁全部事件的序列化形状:事件名、属性键、属性值均为受控枚举输出。
    /// 若有人往属性里塞新字段/自由文本,此测试必须跟着改——强制走一次红线审视。
    #[test]
    fn payload_shape_locked() {
        let cases: Vec<(Event, &str, Option<Value>)> = vec![
            (Event::AppStarted, "app_started", None),
            (
                Event::RecordingStarted { source: RecordSource::Both },
                "recording_started",
                Some(json!({ "source": "both" })),
            ),
            (
                Event::RecordingStopped { duration_ms: 10 * 60_000 },
                "recording_stopped",
                Some(json!({ "duration_bucket": "5-30m" })),
            ),
            (
                Event::NoteRefined { provider: Provider::Doubao },
                "note_refined",
                Some(json!({ "provider": "doubao" })),
            ),
            (
                Event::NoteExported { format: ExportFormat::Md },
                "note_exported",
                Some(json!({ "format": "md" })),
            ),
            (
                Event::McpToolUsed { op: McpOp::Stop },
                "mcp_tool_used",
                Some(json!({ "tool": "stop" })),
            ),
        ];
        for (event, name, props) in cases {
            let (n, p) = event.payload();
            assert_eq!(n, name);
            assert_eq!(p, props);
        }
    }

    #[test]
    fn gate_blocks_without_key_or_consent() {
        assert!(!gate("", true)); // 未配 key:整体停用
        assert!(!gate("A-EU-123", false)); // 用户关闭
        assert!(!gate("", false));
        assert!(gate("A-EU-123", true));
    }
}
```

- [ ] **Step 3: 挂模块并跑测试**

`src-tauri/src/lib.rs` 顶部 mod 声明区（与 `mod settings;` 等并列处）加：

```rust
mod telemetry;
```

Run: `cd src-tauri && cargo test telemetry`
Expected: 上述 6 个测试全 PASS。

- [ ] **Step 4: Builder 注册插件（条件） + setup 里发 app_started**

`src-tauri/src/lib.rs` `run()`（行 2655 起）。现状是一条链式表达式；改为在现有 `.plugin(...)` 块（行 2657-2667）结束后断链插入条件注册：

```rust
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        // …… 其余既有 .plugin(...) 原样保留 ……
        ;
    // 遥测插件:未配 App-Key 时不注册,track 亦会短路,双保险。
    let builder = if telemetry::APP_KEY.is_empty() {
        builder
    } else {
        builder.plugin(tauri_plugin_aptabase::Builder::new(telemetry::APP_KEY).build())
    };
    builder
        .manage(/* 既有 .manage 起的链原样接续 */)
```

在 `.setup(|app| { ... })`（行 2686 起）内、既有初始化完成处追加：

```rust
            telemetry::track(&app.handle().clone(), telemetry::Event::AppStarted);
```

（若上下文已有 `let handle = app.handle().clone();`，直接 `telemetry::track(&handle, telemetry::Event::AppStarted);`。）

- [ ] **Step 5: 编译 + 全量测试**

Run: `cd src-tauri && cargo build && cargo test`
Expected: 编译通过、全部测试 PASS。APP_KEY 为空时运行不注册插件、不上报——行为等同现状。

- [ ] **Step 6: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/telemetry.rs src-tauri/src/lib.rs
git commit -m "telemetry 模块:Aptabase 接入+枚举化事件,属性红线类型化"
```

---

### Task 3: 命令层埋点（录制/精修/导出/MCP）

**Files:**
- Modify: `src-tauri/src/lib.rs`（`do_start_recording` 行 1127-1147、`resume_recording` 行 1157-1181、`do_stop_recording` 行 1186-1236、`spawn_refine` 行 266-469、`export_note` 行 1692-1713）
- Modify: `src-tauri/src/mcp/uds.rs`（`dispatch` 行 140 附近）

**Interfaces:**
- Consumes: Task 2 的 `telemetry::track` 与各枚举（签名见 Task 2 Produces）。
- Produces: 无新接口，纯接线。

- [ ] **Step 1: 录制开始（两处入口）**

`do_start_recording`（行 1127-1147）：在 `spawn_session(...)` 调用成功之后、函数返回 `Ok` 之前插入（`app: &AppHandle` 就在参数里）：

```rust
    if let Ok(dir) = app.path().app_data_dir() {
        let source =
            telemetry::RecordSource::from_settings(settings::load(&dir).record_system_only);
        telemetry::track(app, telemetry::Event::RecordingStarted { source });
    }
```

（若该函数作用域尚未 `use tauri::Manager;`，lib.rs 顶部已有则不必动。）`resume_recording`（行 1157-1181）在其 `spawn_session(..., NoteTarget::Resume(..))` 成功后插入完全相同的片段——续录也是一次录制使用。

- [ ] **Step 2: 录制停止 + 时长桶**

`do_stop_recording`（行 1186-1236）：在 `if let Some(s) = sess`（行 1200 附近，session 被 take 出来的分支）内、finalize 之前插入：

```rust
        telemetry::track(app, telemetry::Event::RecordingStopped { duration_ms: s.elapsed_ms() });
```

`s.elapsed_ms()` 即 `ActiveSession::elapsed_ms`（行 77-86），已扣除暂停时间。

- [ ] **Step 3: 精修触发**

`spawn_refine`（行 266-469）：其内部已有 `let s = settings::load(...)` 读设置处（行 377 附近组装 `LlmConfig` 之前的设置读取点；auto/手动两条路径共用此函数体）。在读到 `s` 后插入：

```rust
    telemetry::track(
        &app,
        telemetry::Event::NoteRefined {
            provider: telemetry::Provider::classify(&s.refine_provider, &s.refine_base_url),
        },
    );
```

注意 `spawn_refine` 里 `app` 的所有权形态（owned `AppHandle`，可能已 move 进线程闭包）——把埋点放在与设置读取同一作用域，借用即可。若该函数分 openai/agent 两个分支各自读设置（行 377-381 与 413-417），在**两个分支各插一次**，`classify` 会分别得出预设类别与 `agent`。

- [ ] **Step 4: 导出**

`export_note`（行 1692-1713）：在 `NoteStore::export(...)` 成功返回之后（只统计成功导出）插入：

```rust
    if let Some(fmt) = telemetry::ExportFormat::parse(&format) {
        telemetry::track(&app, telemetry::Event::NoteExported { format: fmt });
    }
```

- [ ] **Step 5: MCP 控制面**

`src-tauri/src/mcp/uds.rs` 的生产入口 `dispatch(app, req)`（行 140，包 `AppBackend` 调 `dispatch_with` 的那个函数）：在调 `dispatch_with` **之前**插入（拿得到 `app` 与 `req.op`）：

```rust
    if let Some(op) = crate::telemetry::McpOp::parse(&req.op) {
        crate::telemetry::track(app, crate::telemetry::Event::McpToolUsed { op });
    }
```

未知 op 被 `parse` 过滤，不上报。注意 `dispatch_with` 是泛型纯函数（测试在打它的桩），埋点**不要**放进 `dispatch_with`，否则单测会带上 AppHandle 依赖。

- [ ] **Step 6: 编译 + 全量测试**

Run: `cd src-tauri && cargo build && cargo test`
Expected: 通过。埋点全部是 fire-and-forget，不改变任何函数的返回值与错误路径。

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/lib.rs src-tauri/src/mcp/uds.rs
git commit -m "命令层埋点:录制起止/精修/导出/MCP 控制面走 telemetry::track"
```

---

### Task 4: 设置页开关 + 欢迎页告知 + DESIGN.md 同步

**Files:**
- Modify: `src/routes/settings/+page.svelte`（state 行 68-72、`syncLocalFromSettings` 行 139-148、「通用」区行 460-520）
- Modify: `src/lib/WelcomeOverlay.svelte`（行 114-116 download 相位 hints）
- Modify: `DESIGN.md`（行 139 welcome-overlay 约定）

**Interfaces:**
- Consumes: Task 1 的 `Settings.telemetry_enabled`（TS 类型已加）；现有 `saveSetting(mut)`（行 286-299）。
- Produces: 无新接口。

- [ ] **Step 1: 设置页开关行**

`src/routes/settings/+page.svelte` 三处，完全仿照 `keep_audio` 的既有模式：

state 声明（行 72 附近）：

```svelte
  let telemetryOn = $state(true);
```

`syncLocalFromSettings`（行 139-148）内追加：

```svelte
    telemetryOn = s.telemetry_enabled;
```

「通用」区末（行 518「菜单栏常驻」行之后、`</div>` 之前）追加：

```svelte
      <label class="row">
        <div class="row-info">
          <span class="row-label">匿名使用统计</span>
          <span class="row-desc">仅上报功能使用次数与版本信息，绝不包含任何会议内容</span>
        </div>
        <input
          type="checkbox"
          class="ctl switch"
          bind:checked={telemetryOn}
          disabled={!settings}
          onchange={() => saveSetting((s) => (s.telemetry_enabled = telemetryOn))}
        />
      </label>
```

- [ ] **Step 2: 欢迎页告知**

`src/lib/WelcomeOverlay.svelte` download 相位、现有权限提示 `p.hints`（行 116）之后追加一行（**纯文本，不加链接**——DESIGN.md 约定「高级设置 →」是唯一逃生口，不新增出口）：

```svelte
      <p class="hints">已开启匿名使用统计（仅功能使用次数与版本，绝不含会议内容），可在设置中关闭。</p>
```

已知取舍：老用户升级不再见欢迎页，其告知靠 Release 说明与设置页文案兜底。

- [ ] **Step 3: DESIGN.md 同步**

`DESIGN.md` 行 139 welcome-overlay 约定中「权限预告两行 caption 级 `ink-faint`」改为「权限预告与匿名统计告知共三行 caption 级 `ink-faint`」（其余原文不动）。

- [ ] **Step 4: 检查 + 手工验证**

Run: `npm run check`
Expected: 0 errors。

Run: `npm run tauri dev`，验证：设置页「通用」区出现开关且默认开；点击关→重进设置页仍关（落盘生效）；欢迎页（可临时把 settings.json 的 `onboarded` 置 false 复现）download 相位出现告知行。

- [ ] **Step 5: Commit**

```bash
git add src/routes/settings/+page.svelte src/lib/WelcomeOverlay.svelte DESIGN.md
git commit -m "设置页匿名统计开关+欢迎页告知,DESIGN.md 同步"
```

---

### Task 5: App-Key 激活与真机冒烟（需用户参与）

**Files:**
- Modify: `src-tauri/src/telemetry.rs`（`APP_KEY` 常量）

**Interfaces:**
- Consumes: Task 2 的 `APP_KEY` 空串停用机制。

- [ ] **Step 1: 用户创建 Aptabase 应用**

用户在 https://aptabase.com 注册（免费档即可）→ 新建 app → 复制 App-Key（形如 `A-EU-xxxxxxxxxx`）。此步只能由用户完成。

- [ ] **Step 2: 填入 key**

把 `telemetry.rs` 的 `pub const APP_KEY: &str = "";` 改为实际 key。App-Key 是公开写进客户端的标识（Aptabase 设计如此，等同前端可见的写入端点），不是机密，可入库。

- [ ] **Step 3: 真机冒烟**

Run: `npm run tauri dev`，依次触发：启动、开录→停录、精修一篇、导出 MD、（可选）经 MCP 调 `recording_status`。
Expected: Aptabase 看板（Debug 模式过滤器）几分钟内出现 `app_started`/`recording_started`/`recording_stopped`/`note_refined`/`note_exported`/`mcp_tool_used`，属性值均为枚举/桶。再把设置开关关掉重复操作，确认**无新事件**。

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/telemetry.rs
git commit -m "填入 Aptabase App-Key,遥测激活"
```
