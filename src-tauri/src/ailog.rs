//! AI 调用日志:所有对外 AI 调用(HTTP Aing/标题、Agent CLI Aing/标题)与 Agent 经
//! MCP 的 Aing 写回,请求与响应全量落盘,可查询、可导出。
//!
//! 存储形态:`data_root/ai_logs/` 下**每条一个 JSON 文件**,文件名
//! `<UTC紧凑时间戳毫秒>-<pid>-<seq>.json` 字典序即时间序。选一条一文件而不是共享
//! JSONL,是因为写入方横跨两类进程(GUI Aing 管线、`voice-notes mcp serve` 子进程,
//! 后者由外部 Agent spawn、生命周期不受我们控制),大条目(整块转写文本)的跨进程
//! append 无法保证行原子性;一条一文件天然零锁零撕裂。个人量级(每场会议约
//! 分块数+1 条)扫目录毫秒级。
//!
//! 原则:
//! - **记录必须完整可复用**:HTTP 记完整请求体与响应原文;Agent 记完整命令行参数、
//!   提示词与 stdout/stderr;写回记逐段修订全文。单字段超过 [`MAX_FIELD_CHARS`]
//!   才截断并标记 truncated,不静默丢内容。
//! - **绝不记密钥**:HTTP 的 api key 在请求头不在 body,天然不落;Agent 命令行
//!   不含凭据。新增记录点时保持这一不变量。
//! - **记录失败绝不影响业务**:所有写入错误只 eprintln,Aing 照常。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const AILOG_SCHEMA_VERSION: u32 = 1;
/// 单字段(request/response 序列化后)上限,超出截断保头部。转写分块 ~3k 字、
/// Agent stdout 信封 ~10k 字,正常远低于此;上限只防病态输出撑爆磁盘。
pub const MAX_FIELD_CHARS: usize = 200_000;

// 调用类别(kind 取值):refine_chunk=HTTP Aing 分块;title=标题生成(HTTP 或 Agent);
// agent_refine=Agent CLI Aing 一整轮;mcp_apply=Agent 经 MCP 写回 Aing 稿。

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiLogEntry {
    pub schema_version: u32,
    /// 文件名去扩展名,全局唯一、字典序即时间序。
    pub id: String,
    /// 本地时区 RFC3339。
    pub ts: String,
    pub kind: String,
    /// openai | claude | codex | gemini | cursor | mcp
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// HTTP 完整 URL,或 Agent CLI 可执行路径。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// 请求全量(HTTP body / {args,prompt} / {updates})。
    pub request: serde_json::Value,
    /// 响应全量(HTTP 响应原文 / {exit_ok,stdout,stderr_tail} / {updated,paragraphs})。
    pub response: serde_json::Value,
    /// "ok" | "error"
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub duration_ms: u64,
    /// request/response 有字段被截断时为 true。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// 写入方携带的上下文:数据根 + 归属笔记。业务层拿不到时传 None 即静默不记
/// (单测里的 polish/gen_title 不受影响)。
#[derive(Debug, Clone)]
pub struct Ctx {
    pub data_root: PathBuf,
    pub note_id: String,
}

/// 一次调用的记录素材(id/ts/截断由 record 统一生成)。
pub struct Draft {
    pub kind: &'static str,
    pub provider: String,
    pub model: Option<String>,
    pub endpoint: Option<String>,
    pub request: serde_json::Value,
    pub response: serde_json::Value,
    pub status: &'static str,
    pub error: Option<String>,
    pub duration_ms: u64,
}

pub fn log_dir(data_root: &Path) -> PathBuf {
    data_root.join("ai_logs")
}

/// 序列化后超上限的 Value 截断为字符串(保头部+标记),并报告是否截断。
fn cap_value(v: serde_json::Value) -> (serde_json::Value, bool) {
    let s = v.to_string();
    if s.chars().count() <= MAX_FIELD_CHARS {
        return (v, false);
    }
    let head: String = s.chars().take(MAX_FIELD_CHARS).collect();
    (serde_json::Value::String(format!("{head}…[超长已截断]")), true)
}

/// 落一条日志。任何失败只留 stderr,绝不向调用方冒泡——日志是旁路,不许影响 Aing。
pub fn record(ctx: &Ctx, draft: Draft) {
    if let Err(e) = record_inner(ctx, draft) {
        eprintln!("ailog: 记录失败(不影响业务): {e}");
    }
}

fn record_inner(ctx: &Ctx, draft: Draft) -> anyhow::Result<()> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = log_dir(&ctx.data_root);
    std::fs::create_dir_all(&dir)?;
    let now = chrono::Local::now();
    let id = format!(
        "{}-{}-{}",
        now.with_timezone(&chrono::Utc).format("%Y%m%dT%H%M%S%3f"),
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let (request, t1) = cap_value(draft.request);
    let (response, t2) = cap_value(draft.response);
    let entry = AiLogEntry {
        schema_version: AILOG_SCHEMA_VERSION,
        id: id.clone(),
        ts: now.to_rfc3339(),
        kind: draft.kind.to_string(),
        provider: draft.provider,
        note_id: Some(ctx.note_id.clone()).filter(|s| !s.is_empty()),
        model: draft.model,
        endpoint: draft.endpoint,
        request,
        response,
        status: draft.status.to_string(),
        error: draft.error,
        duration_ms: draft.duration_ms,
        truncated: t1 || t2,
    };
    // 同目录 tmp+rename:查询扫描永远不会读到半个文件。
    let tmp = dir.join(format!("{id}.json.tmp"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(&entry)?)?;
    std::fs::rename(&tmp, dir.join(format!("{id}.json")))?;
    Ok(())
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct Filter {
    pub kind: Option<String>,
    pub note_id: Option<String>,
    /// RFC3339 前缀,与 ts 字典序比较(同 list_notes 的 from/to 假设)。
    pub from: Option<String>,
    pub to: Option<String>,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

/// 倒序(新在前)分页查询。损坏文件跳过不炸(与 load_refined 同一容忍哲学)。
pub fn query(data_root: &Path, f: &Filter) -> serde_json::Value {
    let mut names: Vec<String> = list_entry_names(data_root);
    names.sort_unstable_by(|a, b| b.cmp(a)); // 文件名字典序=时间序,倒排
    let entries: Vec<AiLogEntry> = names
        .iter()
        .filter_map(|n| load_entry(data_root, n))
        .filter(|e| f.kind.as_deref().map(|k| e.kind == k).unwrap_or(true))
        .filter(|e| f.note_id.as_deref().map(|id| e.note_id.as_deref() == Some(id)).unwrap_or(true))
        .filter(|e| f.from.as_deref().map(|v| e.ts.as_str() >= v).unwrap_or(true))
        .filter(|e| f.to.as_deref().map(|v| e.ts.as_str() <= v).unwrap_or(true))
        .collect();
    let total = entries.len();
    let page: Vec<&AiLogEntry> = entries
        .iter()
        .skip(f.offset.unwrap_or(0))
        .take(f.limit.unwrap_or(50).clamp(1, 500))
        .collect();
    serde_json::json!({ "total": total, "entries": page })
}

fn list_entry_names(data_root: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(log_dir(data_root)) else { return Vec::new() };
    rd.flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".json").map(str::to_string)
        })
        .collect()
}

fn load_entry(data_root: &Path, name: &str) -> Option<AiLogEntry> {
    let bytes = std::fs::read(log_dir(data_root).join(format!("{name}.json"))).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// 全量导出为单个 JSONL(升序,一行一条),写进 ai_logs/ 下带时间戳的文件名,
/// 返回路径(与 NoteStore::export 同一「写数据目录、把路径给用户」约定)。
/// 扫描只认 `.json` 后缀,导出的 `.jsonl` 不会被后续查询当日志读回。
pub fn export_jsonl(data_root: &Path, dest: Option<&Path>) -> anyhow::Result<(PathBuf, usize)> {
    let mut names = list_entry_names(data_root);
    names.sort_unstable();
    let mut out = String::new();
    let mut count = 0usize;
    for n in &names {
        if let Some(e) = load_entry(data_root, n) {
            out.push_str(&serde_json::to_string(&e)?);
            out.push('\n');
            count += 1;
        }
    }
    let path = match dest {
        Some(p) => p.to_path_buf(),
        None => {
            let dir = log_dir(data_root);
            std::fs::create_dir_all(&dir)?;
            dir.join(format!("export-{}.jsonl", chrono::Local::now().format("%Y%m%d-%H%M%S")))
        }
    };
    std::fs::write(&path, out)?;
    Ok((path, count))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(root: &Path, note: &str) -> Ctx {
        Ctx { data_root: root.to_path_buf(), note_id: note.into() }
    }

    fn draft(kind: &'static str, status: &'static str) -> Draft {
        Draft {
            kind,
            provider: "openai".into(),
            model: Some("m".into()),
            endpoint: Some("https://x/v1/chat/completions".into()),
            request: serde_json::json!({"messages":[{"role":"user","content":"段落"}]}),
            response: serde_json::json!("{\"choices\":[]}"),
            status,
            error: if status == "error" { Some("boom".into()) } else { None },
            duration_ms: 42,
        }
    }

    #[test]
    fn record_then_query_roundtrip_with_filters() {
        let tmp = tempfile::tempdir().unwrap();
        record(&ctx(tmp.path(), "n1"), draft("refine_chunk", "ok"));
        record(&ctx(tmp.path(), "n1"), draft("title", "error"));
        record(&ctx(tmp.path(), "n2"), draft("refine_chunk", "ok"));
        let all = query(tmp.path(), &Filter::default());
        assert_eq!(all["total"], 3);
        let e0 = &all["entries"][0];
        assert!(e0["ts"].as_str().unwrap() >= all["entries"][2]["ts"].as_str().unwrap(), "倒序:新在前");
        assert_eq!(e0["schema_version"], 1);
        // kind 过滤
        let chunks = query(tmp.path(), &Filter { kind: Some("refine_chunk".into()), ..Default::default() });
        assert_eq!(chunks["total"], 2);
        // note 过滤 + 请求响应完整回读
        let n1 = query(tmp.path(), &Filter { note_id: Some("n1".into()), ..Default::default() });
        assert_eq!(n1["total"], 2);
        let ok = n1["entries"].as_array().unwrap().iter().find(|e| e["status"] == "ok").unwrap();
        assert_eq!(ok["request"]["messages"][0]["content"], "段落");
        assert_eq!(ok["model"], "m");
        assert_eq!(ok["duration_ms"], 42);
        let err = n1["entries"].as_array().unwrap().iter().find(|e| e["status"] == "error").unwrap();
        assert_eq!(err["error"], "boom");
        // 分页
        let page = query(tmp.path(), &Filter { limit: Some(1), offset: Some(1), ..Default::default() });
        assert_eq!(page["total"], 3);
        assert_eq!(page["entries"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn oversized_field_is_capped_and_flagged() {
        let tmp = tempfile::tempdir().unwrap();
        let mut d = draft("agent_refine", "ok");
        d.response = serde_json::Value::String("长".repeat(MAX_FIELD_CHARS + 10));
        record(&ctx(tmp.path(), "n1"), d);
        let v = query(tmp.path(), &Filter::default());
        let e = &v["entries"][0];
        assert_eq!(e["truncated"], true);
        let resp = e["response"].as_str().unwrap();
        assert!(resp.ends_with("…[超长已截断]"));
        assert!(resp.chars().count() <= MAX_FIELD_CHARS + 20);
    }

    #[test]
    fn corrupt_entry_skipped_and_empty_dir_ok() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(query(tmp.path(), &Filter::default())["total"], 0, "无目录不炸");
        record(&ctx(tmp.path(), "n1"), draft("refine_chunk", "ok"));
        std::fs::write(log_dir(tmp.path()).join("zzz-bad.json"), "{broken").unwrap();
        let v = query(tmp.path(), &Filter::default());
        assert_eq!(v["total"], 1, "坏文件跳过");
    }

    #[test]
    fn export_merges_ascending_jsonl_and_excludes_itself() {
        let tmp = tempfile::tempdir().unwrap();
        record(&ctx(tmp.path(), "n1"), draft("refine_chunk", "ok"));
        record(&ctx(tmp.path(), "n1"), draft("title", "ok"));
        let (path, count) = export_jsonl(tmp.path(), None).unwrap();
        assert_eq!(count, 2);
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let a: AiLogEntry = serde_json::from_str(lines[0]).unwrap();
        let b: AiLogEntry = serde_json::from_str(lines[1]).unwrap();
        assert!(a.id < b.id, "升序");
        // 导出文件不会被再次查询/导出读回
        assert_eq!(query(tmp.path(), &Filter::default())["total"], 2);
        let (_, count2) = export_jsonl(tmp.path(), None).unwrap();
        assert_eq!(count2, 2);
        // 指定目标路径
        let dest = tmp.path().join("out.jsonl");
        let (p2, _) = export_jsonl(tmp.path(), Some(&dest)).unwrap();
        assert_eq!(p2, dest);
        assert!(dest.exists());
    }
}
