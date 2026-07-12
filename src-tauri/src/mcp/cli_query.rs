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

/// 校验 args 中没有已知集合之外的 flag。CLI 的调用方主要是 Agent:手误
/// (--form)或惯性(--json 给 get)若被静默忽略,会拿到形状正确内容错误的
/// 输出而不自知——必须硬报用法错。
fn reject_unknown_flags(args: &[String], known_with_value: &[&str], known_bare: &[&str]) -> Result<(), String> {
    let mut skip = false;
    for a in args {
        if skip {
            skip = false;
            continue;
        }
        if !a.starts_with("--") {
            continue;
        }
        if known_bare.contains(&a.as_str()) {
            continue;
        }
        if known_with_value.contains(&a.as_str()) {
            skip = true;
            continue;
        }
        return Err(format!("未知参数 {a}"));
    }
    Ok(())
}

/// 第一个位置参数(跳过 flag 及其值;--json/--raw 是无值 flag)。
fn first_positional(args: &[String]) -> Option<String> {
    let mut skip = false;
    for a in args {
        if skip {
            skip = false;
            continue;
        }
        if a == "--json" || a == "--raw" {
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
  get    <note-id> [--format md|txt|json] [--json]   # 默认 md;json = 逐句结构化;--json 是 --format json 的别名\n\
  get    … [--raw]  # 忽略精修稿,取原始逐字稿";

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
    reject_unknown_flags(args, &["--limit", "--offset", "--from", "--to"], &["--json"])?;
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
    reject_unknown_flags(args, &["--limit"], &["--json"])?;
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
    reject_unknown_flags(args, &["--format"], &["--json", "--raw"])?;
    let Some(id) = first_positional(args) else {
        return Err("get 需要一个 note-id(来自 notes list/search)".into());
    };
    let format_opt = opt_value(args, "--format")?;
    let json_flag = has_flag(args, "--json");
    // --json 是 --format json 的别名;两者同时给出且矛盾时,静默择一比报错更容易
    // 让 Agent 误以为拿到的是自己要的格式——必须报用法错。
    let format = match (&format_opt, json_flag) {
        (Some(f), true) if f != "json" => return Err(format!("--json 与 --format {f} 冲突")),
        (Some(f), _) => f.clone(),
        (None, true) => "json".into(),
        (None, false) => "md".into(),
    };
    // CLI 格式名 → MCP get_note 格式名(md/txt 是 CLI 的口语层,内部只有一套)。
    let inner = match format.as_str() {
        "md" => "markdown",
        "txt" => "text",
        "json" => "segments",
        other => return Err(format!("未知格式 {other:?}(可用 md|txt|json)")),
    };
    let prefer_refined = !has_flag(args, "--raw");
    match tools::get_note(&tools::resolve_roots(), &id, inner, prefer_refined) {
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

pub fn speakers_cli(args: &[String]) -> i32 {
    if args.first().map(String::as_str) != Some("list") {
        eprintln!("用法: voice-notes speakers list [--json]");
        return 2;
    }
    if let Err(msg) = reject_unknown_flags(&args[1..], &[], &["--json"]) {
        eprintln!("{msg}\n用法: voice-notes speakers list [--json]");
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

const AILOG_USAGE: &str = "用法: voice-notes ailog <list|export> ...\n\
  list   [--limit N] [--offset N] [--kind refine_chunk|title|agent_refine|mcp_apply] [--note ID] [--from RFC3339] [--to RFC3339] [--json]\n\
  export [--out 文件路径]   # 全量合并为 JSONL;缺省写数据目录 ai_logs/export-<时间>.jsonl";

/// AI 调用日志 CLI:查询与导出,与 GUI 命令同源(crate::ailog 纯函数)。
pub fn ailog_cli(args: &[String]) -> i32 {
    let sub = args.first().map(String::as_str).unwrap_or("");
    let rest = args.get(1..).unwrap_or(&[]);
    let result = match sub {
        "list" => run_ailog_list(rest),
        "export" => run_ailog_export(rest),
        _ => {
            eprintln!("{AILOG_USAGE}");
            return 2;
        }
    };
    match result {
        Ok(code) => code,
        Err(msg) => {
            eprintln!("{msg}\n{AILOG_USAGE}");
            2
        }
    }
}

fn run_ailog_list(args: &[String]) -> Result<i32, String> {
    reject_unknown_flags(args, &["--limit", "--offset", "--kind", "--note", "--from", "--to"], &["--json"])?;
    let filter = crate::ailog::Filter {
        kind: opt_value(args, "--kind")?,
        note_id: opt_value(args, "--note")?,
        from: opt_value(args, "--from")?,
        to: opt_value(args, "--to")?,
        offset: Some(opt_usize(args, "--offset", 0)?),
        limit: Some(opt_usize(args, "--limit", 20)?),
    };
    let v = crate::ailog::query(&tools::resolve_roots().data_root, &filter);
    if has_flag(args, "--json") {
        println!("{}", serde_json::to_string_pretty(&v).expect("静态结构序列化不会失败"));
    } else {
        print!("{}", render_ailog_human(&v));
    }
    Ok(0)
}

fn run_ailog_export(args: &[String]) -> Result<i32, String> {
    reject_unknown_flags(args, &["--out"], &[])?;
    let out = opt_value(args, "--out")?;
    match crate::ailog::export_jsonl(&tools::resolve_roots().data_root, out.as_deref().map(std::path::Path::new)) {
        Ok((path, count)) => {
            println!("已导出 {count} 条 → {}", path.display());
            Ok(0)
        }
        Err(e) => {
            eprintln!("导出失败: {e}");
            Ok(1)
        }
    }
}

/// 人读渲染:列表只给概览列(请求/响应全文用 --json 或导出取)。
fn render_ailog_human(v: &serde_json::Value) -> String {
    let entries = v["entries"].as_array().cloned().unwrap_or_default();
    if entries.is_empty() {
        return "暂无 AI 调用日志。\n".into();
    }
    let mut out = String::from("时间\t类别\t执行方\t模型\t状态\t耗时\t笔记\n");
    for e in &entries {
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}ms\t{}\n",
            e["ts"].as_str().unwrap_or(""),
            e["kind"].as_str().unwrap_or(""),
            e["provider"].as_str().unwrap_or(""),
            e["model"].as_str().unwrap_or("-"),
            e["status"].as_str().unwrap_or(""),
            e["duration_ms"].as_u64().unwrap_or(0),
            e["note_id"].as_str().unwrap_or("-"),
        ));
    }
    out.push_str(&format!("共 {} 条(本页 {} 条)\n", v["total"].as_u64().unwrap_or(0), entries.len()));
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
    fn ailog_cli_lists_queries_and_exports() {
        let _guard = crate::mcp::ENV_VAR_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("VN_APP_DATA", tmp.path());
        // 用法错/未知 flag → 2
        assert_eq!(ailog_cli(&[]), 2);
        assert_eq!(ailog_cli(&["bogus".into()]), 2);
        assert_eq!(ailog_cli(&["list".into(), "--bogus".into()]), 2);
        // 空库 list 可用
        assert_eq!(ailog_cli(&["list".into()]), 0);
        // 落两条再查
        let ctx = crate::ailog::Ctx { data_root: tmp.path().to_path_buf(), note_id: "n1".into() };
        for kind in ["refine_chunk", "title"] {
            crate::ailog::record(
                &ctx,
                crate::ailog::Draft {
                    kind,
                    provider: "openai".into(),
                    model: Some("m".into()),
                    endpoint: None,
                    request: serde_json::json!({}),
                    response: serde_json::json!({}),
                    status: "ok",
                    error: None,
                    duration_ms: 1,
                },
            );
        }
        assert_eq!(ailog_cli(&["list".into(), "--kind".into(), "title".into(), "--json".into()]), 0);
        let out = tmp.path().join("logs.jsonl");
        assert_eq!(ailog_cli(&["export".into(), "--out".into(), out.to_string_lossy().into_owned()]), 0);
        assert_eq!(std::fs::read_to_string(&out).unwrap().lines().count(), 2, "导出全量 2 条");
        std::env::remove_var("VN_APP_DATA");
    }

    #[test]
    fn render_ailog_human_formats_and_handles_empty() {
        assert!(render_ailog_human(&serde_json::json!({ "entries": [] })).contains("暂无"));
        let v = serde_json::json!({ "total": 1, "entries": [{
            "ts": "2026-07-12T09:00:00+08:00", "kind": "agent_refine", "provider": "claude",
            "model": "haiku", "status": "ok", "duration_ms": 1234, "note_id": "n1"
        }]});
        let out = render_ailog_human(&v);
        assert!(out.contains("agent_refine\tclaude\thaiku\tok\t1234ms\tn1"), "{out}");
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
        // --json 是 --format json 的别名(渲染层已按 inner=="segments" 走 JSON 分支;
        // 这里只能断言退出码,行为一致性由 run_get 的 format 归一化逻辑保证)。
        assert_eq!(notes_cli(&["get".into(), "20260101-100000".into(), "--json".into()]), 0, "--json 别名");
        // --raw:忽略精修稿取原始逐字稿,fixture 无精修稿故仅验证不因新 flag 报错
        assert_eq!(notes_cli(&["get".into(), "20260101-100000".into(), "--raw".into()]), 0, "--raw 取原始稿");
        // 错误面
        assert_eq!(notes_cli(&[]), 2, "裸命令用法错");
        assert_eq!(notes_cli(&["search".into()]), 2, "缺关键词用法错");
        assert_eq!(notes_cli(&["get".into()]), 2, "缺 note-id 用法错");
        assert_eq!(notes_cli(&["get".into(), "no-such".into()]), 1, "不存在执行错");
        assert_eq!(notes_cli(&["get".into(), "x".into(), "--format".into(), "bogus".into()]), 2, "未知格式用法错");
        assert_eq!(notes_cli(&["list".into(), "--limit".into()]), 2, "悬空 flag 用法错");
        // 未知 flag:静默忽略会系统性误导 Agent,必须硬报用法错
        assert_eq!(notes_cli(&["list".into(), "--form".into(), "2026-01-01".into()]), 2, "未知 flag --form 拒绝");
        assert_eq!(
            notes_cli(&["get".into(), "20260101-100000".into(), "--json".into(), "--format".into(), "md".into()]),
            2,
            "--json 与 --format 矛盾时报用法错"
        );
        std::env::remove_var("VN_APP_DATA");
    }

    #[test]
    fn speakers_cli_rejects_unknown_flag() {
        let _g = crate::mcp::ENV_VAR_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("VN_APP_DATA", tmp.path());
        assert_eq!(speakers_cli(&["list".into(), "--bogus".into()]), 2, "未知 flag 拒绝而非静默忽略");
        std::env::remove_var("VN_APP_DATA");
    }
}
