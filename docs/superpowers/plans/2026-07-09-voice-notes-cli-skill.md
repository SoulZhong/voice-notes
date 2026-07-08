# 查询 CLI + Agent Skill 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `voice-notes notes/speakers` 查询 CLI(复用 mcp::tools,人读+--json)与 Claude Code Agent Skill(内嵌模板、install/uninstall/status、启动自愈、设置页行、README 双语)。

**Architecture:** main.rs 拦截扩展为 mcp|notes|speakers|skill 四词统一进 `mcp::cli_entry`;查询 CLI 是 `mcp::tools` 纯函数套参数解析(JSON 输出与 MCP 工具同形状);Skill 模板 include_str! 内嵌,受管标记(managed-by)是自愈重写前提。

**Tech Stack:** Rust(既有依赖,零新增), Svelte 5。

**Spec:** `docs/superpowers/specs/2026-07-09-voice-notes-cli-skill-design.md`

## Global Constraints

- git 提交**一律不加**署名尾注;注释中文、说明"为什么"。
- 退出码约定:0 成功 / 1 执行错(stderr 报错) / 2 用法错(打印用法)。
- CLI 的 `--json` 输出必须与对应 MCP 工具 JSON **完全同源**(直接打印 tools:: 返回值,不重组)。
- 人读输出用制表符分列,不引入宽度对齐库。
- 悬空 flag(有 flag 无值)必须报用法错,不得静默兜底(mod.rs parse_agent 的既有裁决)。
- 单测涉及 VN_APP_DATA 的,持 `crate::mcp::ENV_VAR_LOCK`(bridge.rs tests 已有同款用法,照抄其获取方式)。
- Skill 自愈:仅"已安装+受管标记+内容 stale"时重写;无标记(用户自建)绝不覆盖。显式 `skill install` 则总是覆盖(用户主动操作)。
- 工作目录仓库根 /Users/teemo/workspace-soul/voice-notes;cargo 在 `src-tauri/`。
- 分支:继续在 `mcp-service` 上提交。

## File Structure

```
src-tauri/src/main.rs                 # 拦截词扩展(改)
src-tauri/src/mcp/mod.rs              # cli_entry 分发 + 两个新 pub mod(改)
src-tauri/src/mcp/cli_query.rs        # 新:notes/speakers CLI(参数解析+渲染)
src-tauri/src/mcp/skill.rs            # 新:skill install/uninstall/status/heal + CLI
src-tauri/src/mcp/skill_template.md   # 新:SKILL.md 模板({{VERSION}} 占位)
src-tauri/src/lib.rs                  # 3 个 skill tauri 命令 + 自愈线程扩展(改)
src/lib/mcp.ts                        # skill 三 invoke 封装(改)
src/routes/settings/+page.svelte      # 「Claude Code 技能」行(改)
README.md / README.en.md              # CLI 直查 + 技能 小节(改)
```

既有接口(消费,勿重造):`mcp::tools::{resolve_roots, list_notes, search_notes, get_note, list_speakers}`(签名见 tools.rs;get_note 返回 anyhow::Result)、`mcp::cli_main`、`crate::mcp::ENV_VAR_LOCK`(测试锁)、设置页「AI 助手接入」分组与 `refreshMcp` 模式、lib.rs 的 MCP 命令区与 setup 自愈线程。

---

## Task 1: cli_entry 分发 + speakers list 打样

**Files:**
- Modify: `src-tauri/src/main.rs`、`src-tauri/src/mcp/mod.rs`
- Create: `src-tauri/src/mcp/cli_query.rs`、`src-tauri/src/mcp/skill.rs`(本任务仅占位)

**Interfaces:**
- Produces: `mcp::cli_entry(args: &[String]) -> i32`(args[0] 是顶层子命令);`cli_query::{has_flag, opt_value, opt_usize, speakers_cli}`、`cli_query::notes_cli`(占位)、`skill::cli`(占位)。Task 2/3 填充占位。

- [ ] **Step 1: 写失败测试**(cli_query.rs 全文新建,含桩与测试)

```rust
//! notes/speakers 查询 CLI:mcp::tools 纯函数套参数解析。--json 直接打印
//! tools:: 返回值(与 MCP 工具同源,防两处漂移);人读输出制表符分列
//! (CJK 定宽对齐不值得引库)。渲染抽成纯函数返回 String,单测不必捕获 stdout。

use super::tools;

pub(crate) fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

/// 取 `--key value`。悬空(有 flag 无值)必须报错——静默兜底会产生意外语义
/// (与 mod.rs parse_agent 同一裁决)。
pub(crate) fn opt_value(args: &[String], name: &str) -> Result<Option<String>, String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == name {
            return it.next().cloned().map(Some).ok_or_else(|| format!("{name} 需要一个值"));
        }
    }
    Ok(None)
}

pub(crate) fn opt_usize(args: &[String], name: &str, default: usize) -> Result<usize, String> {
    match opt_value(args, name)? {
        Some(v) => v.parse().map_err(|_| format!("{name} 需要整数,得到 {v:?}")),
        None => Ok(default),
    }
}

pub fn notes_cli(args: &[String]) -> i32 {
    let _ = args;
    eprintln!("notes: 尚未实现(Task 2)");
    1
}

pub fn speakers_cli(args: &[String]) -> i32 {
    if args.first().map(String::as_str) != Some("list") {
        eprintln!("用法: voice-notes speakers list [--json]");
        return 2;
    }
    let v = tools::list_speakers(&tools::resolve_roots());
    if has_flag(&args[1..], "--json") {
        println!("{}", serde_json::to_string_pretty(&v).expect("静态结构序列化不会失败"));
    } else {
        print!("{}", render_speakers_human(&v));
    }
    0
}

/// 人读渲染。name 空串是「未改名」的存储语义,显示端兜底「(未命名)」。
fn render_speakers_human(v: &serde_json::Value) -> String {
    let speakers = v["speakers"].as_array().cloned().unwrap_or_default();
    if speakers.is_empty() {
        return "声纹库暂无人物。\n".into();
    }
    let mut out = String::from("id\t名字\t累计说话\t出现笔记数\t最近出现\n");
    for s in speakers {
        let name = s["name"].as_str().unwrap_or("");
        out.push_str(&format!(
            "{}\t{}\t{} 分钟\t{}\t{}\n",
            s["id"].as_str().unwrap_or(""),
            if name.is_empty() { "(未命名)" } else { name },
            s["total_ms"].as_u64().unwrap_or(0) / 60000,
            s["note_count"].as_u64().unwrap_or(0),
            s["last_seen"].as_str().unwrap_or(""),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_helpers_parse_and_reject_dangling() {
        let args: Vec<String> = ["--limit", "5", "--json"].iter().map(|s| s.to_string()).collect();
        assert!(has_flag(&args, "--json"));
        assert_eq!(opt_usize(&args, "--limit", 20).unwrap(), 5);
        assert_eq!(opt_usize(&args, "--offset", 7).unwrap(), 7, "缺省用默认值");
        let dangling: Vec<String> = ["--limit"].iter().map(|s| s.to_string()).collect();
        assert!(opt_usize(&dangling, "--limit", 20).is_err(), "悬空必须报错");
        let bad: Vec<String> = ["--limit", "abc"].iter().map(|s| s.to_string()).collect();
        assert!(opt_usize(&bad, "--limit", 20).is_err(), "非整数必须报错");
    }

    #[test]
    fn render_speakers_human_formats_and_handles_empty() {
        let v = serde_json::json!({ "speakers": [] });
        assert!(render_speakers_human(&v).contains("暂无"));
        let v = serde_json::json!({ "speakers": [
            { "id": "P1", "name": "张三", "total_ms": 180000, "note_count": 3, "last_seen": "2026-07-01T10:00:00+08:00" },
            { "id": "P2", "name": "", "total_ms": 0, "note_count": 0, "last_seen": "" }
        ]});
        let out = render_speakers_human(&v);
        assert!(out.contains("P1\t张三\t3 分钟\t3\t2026-07-01T10:00:00+08:00"));
        assert!(out.contains("P2\t(未命名)"), "空名兜底:{out}");
    }

    #[test]
    fn speakers_cli_usage_and_empty_store() {
        assert_eq!(speakers_cli(&[]), 2, "裸命令给用法");
        assert_eq!(speakers_cli(&["nope".into()]), 2);
        // 空数据目录:list 正常返回 0(不因库空而失败)
        let _g = crate::mcp::ENV_VAR_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("VN_APP_DATA", tmp.path());
        assert_eq!(speakers_cli(&["list".into()]), 0);
        assert_eq!(speakers_cli(&["list".into(), "--json".into()]), 0);
        std::env::remove_var("VN_APP_DATA");
    }
}
```

(若 `ENV_VAR_LOCK` 的实际可见性/获取方式与上式不符,照 bridge.rs tests 的现成写法改,不改语义。)

`src-tauri/src/mcp/skill.rs` 占位:

```rust
//! Claude Code Agent Skill 安装管理(Task 3 实现)。

pub fn cli(args: &[String]) -> i32 {
    let _ = args;
    eprintln!("skill: 尚未实现(Task 3)");
    1
}
```

- [ ] **Step 2: 跑测试确认失败**

Run(在 src-tauri/): `cargo test mcp::cli_query -- --nocapture`
Expected: 编译错误(mod 未挂)——本任务的红。

- [ ] **Step 3: 挂模块 + cli_entry + main.rs 扩展**

mod.rs:`pub mod bridge;` 声明区加 `pub mod cli_query;`、`pub mod skill;`;`cli_main` 之前加:

```rust
/// 顶层 CLI 分发:main.rs 把 mcp|notes|speakers|skill 都送到这里(args[0] 即
/// 顶层词)。集中一处是为了 main.rs 的拦截集合与分发表永远一致。
pub fn cli_entry(args: &[String]) -> i32 {
    match args.first().map(String::as_str).unwrap_or("") {
        "mcp" => cli_main(&args[1..]),
        "notes" => cli_query::notes_cli(&args[1..]),
        "speakers" => cli_query::speakers_cli(&args[1..]),
        "skill" => skill::cli(&args[1..]),
        _ => {
            eprintln!("用法: voice-notes <mcp|notes|speakers|skill> ...");
            2
        }
    }
}
```

main.rs 拦截改为:

```rust
    // CLI 词表与 mcp::cli_entry 的分发表一一对应;新增子命令两处同改。
    if matches!(
        args.get(1).map(String::as_str),
        Some("mcp" | "notes" | "speakers" | "skill")
    ) {
        std::process::exit(app_lib::mcp::cli_entry(&args[1..]));
    }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test mcp:: -- --nocapture` && `cargo check`
Expected: 全 passed(含既有 21 项),无新警告。手工:`cargo run -- speakers list` 列出真实声纹库(或"暂无");`cargo run -- notes` 打印"尚未实现"退出 1。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/main.rs src-tauri/src/mcp/
git commit -m "feat(cli): cli_entry 顶层分发 + speakers list(人读/--json 同源输出)"
```

---

## Task 2: notes list / search / get

**Files:**
- Modify: `src-tauri/src/mcp/cli_query.rs`

**Interfaces:**
- Consumes: Task 1 的 helpers 与 `tools::{list_notes, search_notes, get_note}`。
- Produces: 完整 `notes_cli`。

- [ ] **Step 1: 写失败测试**(tests 模块追加;渲染纯函数可测,数据路径复用 tools 测试的 fixture 手法)

```rust
    /// 与 tools::tests::fixture_note 同构的最小笔记(此处独立复制,模块私有互不可见)。
    fn fixture_note(root: &std::path::Path, id: &str, title: &str, started_at: &str, text: &str) {
        let dir = root.join("notes").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("meta.json"),
            serde_json::json!({ "schema_version": 1, "id": id, "title": title,
                "started_at": started_at, "ended_at": started_at, "state": "complete" }).to_string(),
        ).unwrap();
        std::fs::write(
            dir.join("segments.jsonl"),
            serde_json::json!({ "seq": 0, "source": "mic", "text": text,
                "start_ms": 0, "end_ms": 1000, "speaker": "S1" }).to_string() + "\n",
        ).unwrap();
    }

    #[test]
    fn render_notes_human_lists_rows_and_total() {
        let v = serde_json::json!({ "total": 2, "notes": [
            { "id": "20260101-100000", "title": "评审会", "started_at": "2026-01-01T10:00:00+08:00",
              "duration_secs": 3600, "state": "complete", "speaker_count": 2, "has_refined": true }
        ]});
        let out = render_notes_human(&v);
        assert!(out.contains("20260101-100000\t2026-01-01T10:00:00+08:00\t60 分钟\t2\t有\t评审会"), "{out}");
        assert!(out.contains("共 2 条"));
    }

    #[test]
    fn render_hits_human_shows_context() {
        let v = serde_json::json!({ "scanned_notes": 1, "hits": [
            { "note_id": "20260101-100000", "title": "评审会", "seq": 1, "speaker": "S1",
              "start_ms": 5000, "text": "交付日期定在 Q3", "before": "先看背景", "after": "散会" }
        ]});
        let out = render_hits_human(&v);
        assert!(out.contains("20260101-100000"));
        assert!(out.contains("交付日期定在 Q3"));
        assert!(out.contains("先看背景") && out.contains("散会"), "上下文要展示:{out}");
    }

    #[test]
    fn notes_cli_end_to_end_over_fixture() {
        let _g = crate::mcp::ENV_VAR_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir().unwrap();
        fixture_note(tmp.path(), "20260101-100000", "评审会", "2026-01-01T10:00:00+08:00", "交付日期定在 Q3");
        std::env::set_var("VN_APP_DATA", tmp.path());
        assert_eq!(notes_cli(&["list".into()]), 0);
        assert_eq!(notes_cli(&["list".into(), "--json".into()]), 0);
        assert_eq!(notes_cli(&["search".into(), "交付日期".into()]), 0);
        assert_eq!(notes_cli(&["get".into(), "20260101-100000".into()]), 0);
        assert_eq!(notes_cli(&["get".into(), "20260101-100000".into(), "--format".into(), "json".into()]), 0);
        // 错误面
        assert_eq!(notes_cli(&[]), 2, "裸命令用法错");
        assert_eq!(notes_cli(&["search".into()]), 2, "缺关键词用法错");
        assert_eq!(notes_cli(&["get".into()]), 2, "缺 note-id 用法错");
        assert_eq!(notes_cli(&["get".into(), "no-such".into()]), 1, "不存在执行错");
        assert_eq!(notes_cli(&["get".into(), "x".into(), "--format".into(), "bogus".into()]), 2, "未知格式用法错");
        assert_eq!(notes_cli(&["list".into(), "--limit".into()]), 2, "悬空 flag 用法错");
        std::env::remove_var("VN_APP_DATA");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test mcp::cli_query -- --nocapture`
Expected: FAIL(渲染函数不存在/notes_cli 是占位)。

- [ ] **Step 3: 实现**(替换 notes_cli 占位;渲染纯函数与子命令实现)

```rust
/// 第一个位置参数(跳过 flag 及其值;--json 是无值 flag)。
fn first_positional(args: &[String]) -> Option<String> {
    let mut skip = false;
    for a in args {
        if skip {
            skip = false;
            continue;
        }
        if a == "--json" {
            continue;
        }
        if a.starts_with("--") {
            skip = true; // 其余 flag 均带值
            continue;
        }
        return Some(a.clone());
    }
    None
}

const NOTES_USAGE: &str = "用法: voice-notes notes <list|search|get> ...\n\
  list   [--limit N] [--offset N] [--from RFC3339] [--to RFC3339] [--json]\n\
  search <关键词> [--limit N] [--json]\n\
  get    <note-id> [--format md|txt|json]   # 默认 md;json = 逐句结构化";

pub fn notes_cli(args: &[String]) -> i32 {
    let sub = args.first().map(String::as_str).unwrap_or("");
    let rest = args.get(1..).unwrap_or(&[]);
    let result = match sub {
        "list" => run_list(rest),
        "search" => run_search(rest),
        "get" => run_get(rest),
        _ => {
            eprintln!("{NOTES_USAGE}");
            return 2;
        }
    };
    match result {
        Ok(code) => code,
        Err(msg) => {
            // Err = 用法错(参数形状不对);执行错在 run_* 内打印并 Ok(1)。
            eprintln!("{msg}\n{NOTES_USAGE}");
            2
        }
    }
}

fn run_list(args: &[String]) -> Result<i32, String> {
    let limit = opt_usize(args, "--limit", 20)?;
    let offset = opt_usize(args, "--offset", 0)?;
    let from = opt_value(args, "--from")?;
    let to = opt_value(args, "--to")?;
    let v = tools::list_notes(&tools::resolve_roots(), limit, offset, from.as_deref(), to.as_deref());
    if has_flag(args, "--json") {
        println!("{}", serde_json::to_string_pretty(&v).expect("静态结构序列化不会失败"));
    } else {
        print!("{}", render_notes_human(&v));
    }
    Ok(0)
}

fn run_search(args: &[String]) -> Result<i32, String> {
    let Some(query) = first_positional(args) else {
        return Err("search 需要一个关键词".into());
    };
    let limit = opt_usize(args, "--limit", 20)?;
    let v = tools::search_notes(&tools::resolve_roots(), &query, limit);
    if has_flag(args, "--json") {
        println!("{}", serde_json::to_string_pretty(&v).expect("静态结构序列化不会失败"));
    } else {
        print!("{}", render_hits_human(&v));
    }
    Ok(0)
}

fn run_get(args: &[String]) -> Result<i32, String> {
    let Some(id) = first_positional(args) else {
        return Err("get 需要一个 note-id(来自 notes list/search)".into());
    };
    let format = opt_value(args, "--format")?.unwrap_or_else(|| "md".into());
    // CLI 格式名 → MCP get_note 格式名(md/txt 是 CLI 的口语层,内部只有一套)。
    let inner = match format.as_str() {
        "md" => "markdown",
        "txt" => "text",
        "json" => "segments",
        other => return Err(format!("未知格式 {other:?}(可用 md|txt|json)")),
    };
    match tools::get_note(&tools::resolve_roots(), &id, inner, true) {
        Ok(v) => {
            if inner == "segments" {
                println!("{}", serde_json::to_string_pretty(&v).expect("静态结构序列化不会失败"));
            } else {
                println!("{}", v["content"].as_str().unwrap_or(""));
            }
            Ok(0)
        }
        Err(e) => {
            eprintln!("{e}");
            Ok(1)
        }
    }
}

fn render_notes_human(v: &serde_json::Value) -> String {
    let notes = v["notes"].as_array().cloned().unwrap_or_default();
    if notes.is_empty() {
        return "没有匹配的笔记。\n".into();
    }
    let mut out = String::from("id\t开始时间\t时长\t说话人\t精修\t标题\n");
    for n in &notes {
        out.push_str(&format!(
            "{}\t{}\t{} 分钟\t{}\t{}\t{}\n",
            n["id"].as_str().unwrap_or(""),
            n["started_at"].as_str().unwrap_or(""),
            n["duration_secs"].as_u64().unwrap_or(0) / 60,
            n["speaker_count"].as_u64().unwrap_or(0),
            if n["has_refined"].as_bool().unwrap_or(false) { "有" } else { "无" },
            n["title"].as_str().unwrap_or(""),
        ));
    }
    out.push_str(&format!("共 {} 条(本页 {} 条)\n", v["total"].as_u64().unwrap_or(0), notes.len()));
    out
}

fn render_hits_human(v: &serde_json::Value) -> String {
    let hits = v["hits"].as_array().cloned().unwrap_or_default();
    if hits.is_empty() {
        return "没有命中。\n".into();
    }
    let mut out = String::new();
    for h in &hits {
        out.push_str(&format!(
            "{} {} [{}ms] {}\n  … {} → 【{}】 → {}\n",
            h["note_id"].as_str().unwrap_or(""),
            h["title"].as_str().unwrap_or(""),
            h["start_ms"].as_u64().unwrap_or(0),
            h["speaker"].as_str().unwrap_or("-"),
            h["before"].as_str().unwrap_or(""),
            h["text"].as_str().unwrap_or(""),
            h["after"].as_str().unwrap_or(""),
        ));
    }
    out.push_str(&format!("共 {} 处命中\n", hits.len()));
    out
}
```

注意:`get --format bogus` 走 Err → 退出码 2(测试如此断言);`get` 不存在的 id 走 Ok(1)。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test mcp:: -- --nocapture` && `cargo check`
Expected: 全 passed。手工:`cargo run -- notes list`、`cargo run -- notes search 会议`、`cargo run -- notes get <真实id>` 对真实数据观感检查。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/mcp/cli_query.rs
git commit -m "feat(cli): notes list/search/get——人读与 --json 同源、退出码 0/1/2"
```

---

## Task 3: skill 模块(模板/安装/状态/自愈)+ CLI

**Files:**
- Create: `src-tauri/src/mcp/skill_template.md`
- Modify: `src-tauri/src/mcp/skill.rs`(替换占位)

**Interfaces:**
- Produces: `skill::{rendered, status_in, install_in, uninstall_in, heal_in, cli}` 与 `SkillState{NotInstalled,Current,Stale,Unmanaged}`;`heal() -> bool`、`status()/install()/uninstall()`(真实 $HOME 包装)。Task 4 GUI 消费。

- [ ] **Step 1: 写 skill_template.md**(全文;{{VERSION}} 由安装时渲染)

```markdown
---
name: voice-notes
description: 查询本机 voice-notes 会议笔记(实时转写+说话人识别)。当用户询问会议内容、要会议纪要、写周报/日报需要汇总会议、找会上的决议/待办/承诺/时间点时使用。支持全文检索、读取全文(优先 AI 精修稿)、录制状态查询与(需用户开启)录制控制。
---

<!-- managed-by: voice-notes v{{VERSION}} —— 本文件由 voice-notes 自动安装,应用升级时自动更新;手工修改会被覆盖。如需自定义,请删除本行受管标记(将不再自动更新)。 -->

# voice-notes 会议笔记

## 工具与降级路径

优先用 MCP 工具(server 名 `voice-notes`)。MCP 工具不可用时改用 CLI,输出与 MCP 同一 JSON 形状:

```bash
/Applications/voice-notes.app/Contents/MacOS/voice-notes notes list --json
/Applications/voice-notes.app/Contents/MacOS/voice-notes notes search "关键词" --json
/Applications/voice-notes.app/Contents/MacOS/voice-notes notes get <note-id> --format md
/Applications/voice-notes.app/Contents/MacOS/voice-notes speakers list --json
```

MCP 未注册时可代用户注册:`/Applications/voice-notes.app/Contents/MacOS/voice-notes mcp register --agent auto`。

## 使用策略

- **先定位再取全文**:`search_notes`(大小写不敏感子串,试关键词的多个说法)拿 note_id,再 `get_note`;不要 list 全部后逐个 get。
- `get_note` 默认 prefer_refined=true:有 AI 精修稿(错字修正/段落归并)时返回精修稿,响应的 `refined` 字段标注来源;需要逐句时间戳或原始逐字稿时用 format="segments"、prefer_refined=false。
- 查询类(list/search/get/speakers)无需 App 运行;`recording_status`/`get_live_transcript` 需要 App 正在运行;`start/stop/pause/resume_recording` 还需用户在「设置 → AI 助手接入」开启「允许 AI 控制录制」——被拒时把这句指引转告用户,不要自行重试。
- 说话人:人名以响应里的 `speakers` 表(name/person_id)为准;P 号是跨会议一致的人物编号;`speaker_count` 是聚类结果仅供参考。

## 常用工作流

1. **会议纪要**:`get_note(note_id, format="markdown")` → 按「主题 / 结论与决议 / 待办(负责人+时限)/ 遗留问题」归纳;引用原话时带说话人名与时间戳。
2. **周报/日报汇总**:`list_notes(from=<周一日期>)` → 逐条 `get_note` 提取 1-3 个要点合并;标题与时长直接用 list 字段。
3. **找决议/待办/承诺**:`search_notes` 用关键词族(决定/定了/负责/下周/deadline/跟进),命中自带前后一句上下文,必要时 get 全文核对。

## 隐私

会议笔记是用户的本机隐私数据,内容进入你的上下文即离开本机。仅在任务需要时检索;引用大段原文前先确认用户意图。
```

- [ ] **Step 2: 写失败测试**(skill.rs 重写为实现骨架+tests;先让断言红)

```rust
//! Claude Code Agent Skill 的安装/卸载/状态/自愈。模板 include_str! 内嵌,
//! 安装时渲染 {{VERSION}}。受管标记(managed-by)是自愈重写的前提:无标记
//! 视为用户自有文件,自愈绝不覆盖;显式 install 是用户主动操作,总是覆盖。

use std::path::{Path, PathBuf};

const TEMPLATE: &str = include_str!("skill_template.md");
const MANAGED_MARK: &str = "managed-by: voice-notes";

/// 渲染当前版本的 SKILL.md 内容(也是 status 判 stale 的比较基准)。
pub fn rendered() -> String {
    TEMPLATE.replace("{{VERSION}}", env!("CARGO_PKG_VERSION"))
}

fn skill_file(home: &Path) -> PathBuf {
    home.join(".claude/skills/voice-notes/SKILL.md")
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SkillState {
    NotInstalled,
    /// 受管且与当前版本渲染结果一致。
    Current,
    /// 受管但内容过期(旧版本装的/模板改了)——自愈可重写。
    Stale,
    /// 存在同名文件但无受管标记(用户自建/删标记自定义)——一律不动。
    Unmanaged,
}

pub fn status_in(home: &Path) -> SkillState {
    let Ok(text) = std::fs::read_to_string(skill_file(home)) else {
        return SkillState::NotInstalled;
    };
    if !text.contains(MANAGED_MARK) {
        return SkillState::Unmanaged;
    }
    if text == rendered() {
        SkillState::Current
    } else {
        SkillState::Stale
    }
}

pub fn install_in(home: &Path) -> anyhow::Result<()> {
    let file = skill_file(home);
    let dir = file.parent().expect("skill_file 恒有父目录");
    std::fs::create_dir_all(dir)?;
    // tmp+rename 原子写:Agent 可能随时读这个文件,不能让它看到半写状态。
    let tmp = dir.join("SKILL.md.tmp");
    std::fs::write(&tmp, rendered())?;
    std::fs::rename(&tmp, &file)?;
    Ok(())
}

pub fn uninstall_in(home: &Path) -> anyhow::Result<()> {
    let file = skill_file(home);
    if file.exists() {
        std::fs::remove_file(&file)?;
    }
    // 目录只剩壳则顺手删;非空(用户放了别的)会失败,忽略即可。
    if let Some(dir) = file.parent() {
        let _ = std::fs::remove_dir(dir);
    }
    Ok(())
}

/// GUI 启动自愈:仅「受管 + stale」重写。返回是否发生了重写。
pub fn heal_in(home: &Path) -> bool {
    status_in(home) == SkillState::Stale && install_in(home).is_ok()
}

fn real_home() -> anyhow::Result<PathBuf> {
    std::env::var("HOME").map(PathBuf::from).map_err(|_| anyhow::anyhow!("HOME 不可用"))
}

pub fn status() -> anyhow::Result<SkillState> {
    Ok(status_in(&real_home()?))
}

pub fn install() -> anyhow::Result<()> {
    install_in(&real_home()?)
}

pub fn uninstall() -> anyhow::Result<()> {
    uninstall_in(&real_home()?)
}

pub fn heal() -> bool {
    real_home().map(|h| heal_in(&h)).unwrap_or(false)
}

pub fn cli(args: &[String]) -> i32 {
    match args.first().map(String::as_str).unwrap_or("") {
        "install" => match install() {
            Ok(()) => {
                println!("已安装到 ~/.claude/skills/voice-notes/SKILL.md");
                0
            }
            Err(e) => {
                eprintln!("安装失败: {e}");
                1
            }
        },
        "uninstall" => match uninstall() {
            Ok(()) => {
                println!("已移除");
                0
            }
            Err(e) => {
                eprintln!("移除失败: {e}");
                1
            }
        },
        "status" => {
            match status() {
                Ok(SkillState::NotInstalled) => println!("未安装"),
                Ok(SkillState::Current) => println!("已安装(当前版本)"),
                Ok(SkillState::Stale) => println!("已安装(旧版,应用启动时会自动更新)"),
                Ok(SkillState::Unmanaged) => println!("存在自定义同名 skill(无受管标记,不自动管理)"),
                Err(e) => {
                    eprintln!("查询失败: {e}");
                    return 1;
                }
            }
            0
        }
        _ => {
            eprintln!(
                "用法: voice-notes skill <install|uninstall|status>\n\
                 install     安装 Claude Code 技能(~/.claude/skills/voice-notes/)\n\
                 uninstall   移除\n\
                 status      安装状态"
            );
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_substitutes_version_and_keeps_mark() {
        let r = rendered();
        assert!(!r.contains("{{VERSION}}"), "占位必须被替换");
        assert!(r.contains(env!("CARGO_PKG_VERSION")));
        assert!(r.contains(MANAGED_MARK));
        assert!(r.starts_with("---\nname: voice-notes"), "frontmatter 形状");
    }

    #[test]
    fn install_status_uninstall_roundtrip_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(status_in(tmp.path()), SkillState::NotInstalled);
        install_in(tmp.path()).unwrap();
        install_in(tmp.path()).unwrap(); // 幂等
        assert_eq!(status_in(tmp.path()), SkillState::Current);
        uninstall_in(tmp.path()).unwrap();
        uninstall_in(tmp.path()).unwrap(); // 幂等
        assert_eq!(status_in(tmp.path()), SkillState::NotInstalled);
        assert!(!tmp.path().join(".claude/skills/voice-notes").exists(), "空壳目录一并清掉");
    }

    #[test]
    fn stale_is_healed_but_unmanaged_is_never_touched() {
        let tmp = tempfile::tempdir().unwrap();
        // stale:受管标记在,但内容是旧版
        let dir = tmp.path().join(".claude/skills/voice-notes");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), format!("old content\n<!-- {MANAGED_MARK} v0.0.0 -->\n")).unwrap();
        assert_eq!(status_in(tmp.path()), SkillState::Stale);
        assert!(heal_in(tmp.path()), "stale 应被自愈重写");
        assert_eq!(status_in(tmp.path()), SkillState::Current);
        // unmanaged:无标记,自愈绝不动;显式 install 才覆盖
        std::fs::write(dir.join("SKILL.md"), "用户自己的 skill,没有标记").unwrap();
        assert_eq!(status_in(tmp.path()), SkillState::Unmanaged);
        assert!(!heal_in(tmp.path()), "无标记不得自愈");
        assert_eq!(
            std::fs::read_to_string(dir.join("SKILL.md")).unwrap(),
            "用户自己的 skill,没有标记",
            "内容原封不动"
        );
        install_in(tmp.path()).unwrap(); // 显式安装=用户主动,覆盖
        assert_eq!(status_in(tmp.path()), SkillState::Current);
    }
}
```

- [ ] **Step 3: 跑测试**

Run: `cargo test mcp::skill -- --nocapture`
Expected: 若骨架即完整实现则直接绿(本任务代码即实现,红步由 Task 1 占位保证过链路);重点确认三个测试全过、`cargo check` 无新警告。

- [ ] **Step 4: 手工验证**

Run: `cargo run -- skill status` → 未安装;`cargo run -- skill install` → 检查 `~/.claude/skills/voice-notes/SKILL.md` 内容含真实版本号;`cargo run -- skill status` → 当前版本;`cargo run -- skill uninstall` 还原。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/mcp/skill.rs src-tauri/src/mcp/skill_template.md
git commit -m "feat(skill): Claude Code 技能安装管理——模板内嵌/受管标记/stale 自愈/CLI"
```

---

## Task 4: GUI——tauri 命令 + 自愈线程扩展 + 设置页行

**Files:**
- Modify: `src-tauri/src/lib.rs`、`src/lib/mcp.ts`、`src/routes/settings/+page.svelte`

**Interfaces:**
- Consumes: `mcp::skill::{status, install, uninstall, heal, SkillState}`。
- Produces: tauri 命令 `mcp_skill_status() -> String`("not_installed"|"current"|"stale"|"unmanaged")、`mcp_skill_install()`、`mcp_skill_uninstall()`;前端 `mcpSkillStatus/mcpSkillInstall/mcpSkillUninstall`。

- [ ] **Step 1: lib.rs 命令**(MCP 命令区追加)

```rust
#[tauri::command]
fn mcp_skill_status() -> Result<String, String> {
    use mcp::skill::SkillState::*;
    Ok(match mcp::skill::status().map_err(|e| e.to_string())? {
        NotInstalled => "not_installed",
        Current => "current",
        Stale => "stale",
        Unmanaged => "unmanaged",
    }
    .into())
}

#[tauri::command]
fn mcp_skill_install() -> Result<(), String> {
    mcp::skill::install().map_err(|e| e.to_string())
}

#[tauri::command]
fn mcp_skill_uninstall() -> Result<(), String> {
    mcp::skill::uninstall().map_err(|e| e.to_string())
}
```

invoke_handler 列表在 `mcp_healed_count` 后追加三个命令名。setup 的自愈线程内(registry heal 旁)加:

```rust
                    // Skill 同步:受管且过期(应用升级后)静默重写为当前版本。
                    let _ = crate::mcp::skill::heal();
```

(具体放置:自愈线程闭包里 registry heal 之后;若该闭包结构不同,保持"同一后台线程、GUI 启动即跑"语义。)

- [ ] **Step 2: mcp.ts 封装**(文件尾追加)

```ts
/** Claude Code 技能状态:not_installed | current | stale | unmanaged。 */
export const mcpSkillStatus = () => invoke<string>("mcp_skill_status");
export const mcpSkillInstall = () => invoke<void>("mcp_skill_install");
export const mcpSkillUninstall = () => invoke<void>("mcp_skill_uninstall");
```

- [ ] **Step 3: 设置页行**(「AI 助手接入」分组,「允许 AI 控制录制」行之前插入)

script 区(MCP 状态块旁):

```ts
  // Claude Code 技能:与 Agent 注册同理,真值源是磁盘文件,现查现示。
  import { mcpSkillStatus, mcpSkillInstall, mcpSkillUninstall } from "$lib/mcp";
  let skillState = $state<string | null>(null);
  let skillBusy = $state(false);

  async function refreshSkill() {
    try {
      skillState = await mcpSkillStatus();
    } catch (e) {
      mcpError = String(e);
    }
  }

  async function toggleSkill() {
    skillBusy = true;
    try {
      if (skillState === "not_installed") {
        await mcpSkillInstall();
      } else {
        await mcpSkillUninstall();
      }
      await refreshSkill();
    } catch (e) {
      mcpError = String(e);
    } finally {
      skillBusy = false;
    }
  }
```

(import 合并进该文件已有的 $lib/mcp import 行;onMount 里 refreshMcp() 旁加 `refreshSkill();`。)

模板(行结构与 Agent 行同形态):

```svelte
      <div class="row">
        <div class="row-info">
          <span class="row-label">Claude Code 技能</span>
          <span class="row-desc">
            {#if skillState === "current"}已安装:Claude 掌握会议纪要/周报/检索工作流
            {:else if skillState === "stale"}已安装(旧版,应用启动时自动更新)
            {:else if skillState === "unmanaged"}检测到自定义同名技能,不自动管理
            {:else}让 Claude Code 掌握会议纪要/周报/检索工作流(写入 ~/.claude/skills)
            {/if}
          </span>
        </div>
        {#if skillState !== null && skillState !== "unmanaged"}
          <button class="btn-secondary" disabled={skillBusy} onclick={toggleSkill}>
            {skillState === "not_installed" ? "安装" : "移除"}
          </button>
        {/if}
      </div>
```

- [ ] **Step 4: 验证**

Run: `cargo check` && `cargo test mcp::` && `npm run check` && `npm run build`
Expected: 全绿、无新警告/错误。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/lib.rs src/lib/mcp.ts src/routes/settings/+page.svelte
git commit -m "feat(skill): GUI 接入——三命令/启动自愈同步/设置页「Claude Code 技能」行"
```

---

## Task 5: README 双语 + 对账回归

**Files:**
- Modify: `README.md`、`README.en.md`

- [ ] **Step 1: README.md**(「接入 AI 助手(MCP)」章节内、工具表之后追加两小节)

````markdown
### 命令行直查(无需 MCP)

脚本或未配置 MCP 的 Agent 可以直接用 CLI 查询,输出与 MCP 工具同一 JSON 形状(`--json`),默认人读:

```bash
VN=/Applications/voice-notes.app/Contents/MacOS/voice-notes
"$VN" notes list --limit 10          # 最近笔记(id/时间/时长/说话人数/精修/标题)
"$VN" notes search "交付日期" --json  # 全文检索
"$VN" notes get <note-id>            # 全文(默认 Markdown,--format md|txt|json)
"$VN" speakers list                  # 声纹库人物
```

### Claude Code 技能

一行命令让 Claude Code 掌握会议纪要、周报汇总、决议检索等工作流(也可在 设置 → AI 助手接入 一键安装):

```bash
/Applications/voice-notes.app/Contents/MacOS/voice-notes skill install
```
````

- [ ] **Step 2: README.en.md 同步英文版**(同位置、同结构)。

- [ ] **Step 3: spec 对账 + 全量回归**

对照 `docs/superpowers/specs/2026-07-09-voice-notes-cli-skill-design.md` 逐节核对(命令面/退出码/skill 生命周期/设置页/README/真值源纪律);Run: `cargo test`(src-tauri/)、`npm run check`、`npm run build`——全绿。发现缺口最小修复。

- [ ] **Step 4: Commit**

```bash
git add README.md README.en.md
git commit -m "docs(README): CLI 直查与 Claude Code 技能小节(中英)"
```
