//! 样本溯源:哪个笔记的哪个簇写下了哪份样本文件。
//!
//! 为什么必须有:打标(多人混杂)要删"这一簇贡献的样本",而样本身份只是槽位名
//! (`P15.wav`/`P15-2.wav`),删除、补空槽、人物合并都会让路径漂移;样本又是在质心
//! 入库成功之后单独追加的,两者之间没有关联记录——没有溯源就只能瞎猜
//! (2026-08-20 设计,codex 轮二 P1④)。
//!
//! 崩溃原子靠**同锁 WAL**:VP 锁内先记 intent(fsync)→ 写 WAV(临时文件+rename,
//! fsync 父目录)→ 记 complete(收进 receipts,fsync)。"写完 WAV、还没写溯源"之间
//! 崩溃会留下永远无法归因的样本,WAL 把这个窗口闭合。启动恢复三分支(codex 轮三 P2):
//! 文件在且 hash 符 → 补 complete;不在 → 丢 intent;在但 hash 不符 → 记 conflict,
//! 不认领不删除,留人工。
//!
//! 质心贡献 receipt 也落在这里:每次 upsert 提交后追加一行(jsonl)。定位是
//! **辅助诊断证据**——出问题时能回答"这个人物历史上被哪些笔记的哪些簇写过",
//! 不支撑任何撤回承诺(真精确化需要向量取回+合并血缘+共同提交,见设计文档)。
//!
//! 并发:所有读写都要求调用方已持 vp_guard(与 voiceprints.json 同一把锁)。
//! 文件与声纹库同根(app-data),**不写 note_dir**——录制 writer 整场持 NoteLock,
//! 实时入库在 worker 里持 VP 锁,写 note_dir 是绕过笔记单写者(codex 轮一 P1⑤)。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 一份样本文件的出生证明。path 相对 root(如 "voiceprints/P15-2.wav"):
/// 绝对路径会因数据目录迁移整体失效。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SampleReceipt {
    pub receipt_id: String,
    pub note_id: String,
    pub cluster_id: String,
    /// resolve 后的人物 id。合并迁移时随文件更新。
    pub person_id: String,
    pub path: String,
    /// 文件字节的 sha256 hex。删除前 path+hash 双校验的依据。
    pub content_hash: String,
    pub at: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SampleTrace {
    #[serde(default)]
    pub receipts: Vec<SampleReceipt>,
    /// 启动恢复发现"文件在但 hash 不符"的 intent:不认领不删除,留人工。
    #[serde(default)]
    pub conflicts: Vec<SampleReceipt>,
}

fn trace_path(root: &Path) -> PathBuf {
    root.join("sample_trace.json")
}
fn wal_path(root: &Path) -> PathBuf {
    root.join("sample_trace.wal.jsonl")
}

/// 缺失/损坏 → 空(溯源是保护层;坏了等于"这些样本无溯源",按不可自动删处理)。
pub fn load(root: &Path) -> SampleTrace {
    std::fs::read_to_string(trace_path(root))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// 原子写。调用方须已持 vp_guard。
pub fn save(root: &Path, t: &SampleTrace) -> anyhow::Result<()> {
    let path = trace_path(root);
    let tmp = path.with_extension("json.tmp");
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(serde_json::to_string_pretty(t)?.as_bytes())?;
        f.sync_all()?; // 断电级:rename 前内容必须先落稳(codex 实现轮一 P2)
    }
    std::fs::rename(&tmp, &path)?;
    if let Ok(d) = std::fs::File::open(root) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// WAL 第一步:落 intent 并 fsync。**必须先于样本文件落地**。
pub fn wal_intent(root: &Path, r: &SampleReceipt) -> anyhow::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(wal_path(root))?;
    let mut line = serde_json::to_string(r)?;
    line.push('\n');
    f.write_all(line.as_bytes())?;
    f.sync_all()?;
    Ok(())
}

/// WAL 第三步:intent 转正。收进 receipts、清掉 WAL 里已完成的行。
/// (WAL 行数≤在途写入数,通常 0-2 行,整文件重写成本可忽略。)
pub fn wal_complete(root: &Path, r: SampleReceipt) -> anyhow::Result<()> {
    let mut t = load(root);
    t.receipts.push(r.clone());
    save(root, &t)?;
    remove_wal_line(root, &r.receipt_id)?;
    Ok(())
}

fn remove_wal_line(root: &Path, receipt_id: &str) -> anyhow::Result<()> {
    let p = wal_path(root);
    let Ok(s) = std::fs::read_to_string(&p) else { return Ok(()) };
    let kept: Vec<&str> = s
        .lines()
        .filter(|l| {
            serde_json::from_str::<SampleReceipt>(l)
                .map(|r| r.receipt_id != receipt_id)
                .unwrap_or(false) // 解析不动的行是垃圾,一并清
        })
        .collect();
    if kept.is_empty() {
        let _ = std::fs::remove_file(&p);
    } else {
        // 原地重写会在崩溃时殃及其他在途行:走 tmp+rename+fsync(codex 实现轮一 P2)。
        let tmp = p.with_extension("jsonl.tmp");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all((kept.join("\n") + "\n").as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &p)?;
        if let Ok(d) = std::fs::File::open(root) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

/// 启动恢复(三分支)。返回 (转正数, 丢弃数, 冲突数)。调用方须已持 vp_guard。
pub fn recover(root: &Path) -> (usize, usize, usize) {
    let p = wal_path(root);
    let Ok(s) = std::fs::read_to_string(&p) else { return (0, 0, 0) };
    let mut t = load(root);
    let (mut done, mut dropped, mut conflicted) = (0usize, 0usize, 0usize);
    for line in s.lines() {
        let Ok(r) = serde_json::from_str::<SampleReceipt>(line) else {
            dropped += 1;
            continue;
        };
        if t.receipts.iter().any(|x| x.receipt_id == r.receipt_id) {
            continue; // complete 写成了、WAL 行没删掉:已转正,直接清
        }
        let abs = root.join(&r.path);
        match super::voiceprints::sample_content_hash(&abs) {
            Some(h) if h == r.content_hash => {
                t.receipts.push(r);
                done += 1;
            }
            Some(_) => {
                eprintln!("样本溯源恢复:{} 内容与 intent 不符,记 conflict 留人工", r.path);
                t.conflicts.push(r);
                conflicted += 1;
            }
            None => {
                dropped += 1; // 文件没写成:丢弃 intent(顺带清临时半成品)
                let _ = std::fs::remove_file(abs.with_extension("wav.tmp"));
            }
        }
    }
    if let Err(e) = save(root, &t) {
        eprintln!("样本溯源恢复:落盘失败(下次启动重试): {e}");
        return (0, 0, 0); // trace 没写成就不能删 WAL,否则 intent 丢失
    }
    let _ = std::fs::remove_file(&p);
    (done, dropped, conflicted)
}

/// 样本文件被合并迁移(rename from→to,归属改为 new_person)时同步溯源。
/// 调用方须已持 vp_guard。best-effort:溯源丢了等于该样本回到"无溯源"态(不可自动删),
/// 不影响库一致性。
pub fn on_sample_moved(root: &Path, from: &Path, to: &Path, new_person: &str) {
    let rel_from = rel(root, from);
    let rel_to = rel(root, to);
    let mut t = load(root);
    let mut hit = false;
    for r in t.receipts.iter_mut() {
        if r.path == rel_from {
            r.path = rel_to.clone();
            r.person_id = new_person.to_string();
            hit = true;
        }
    }
    if hit {
        if let Err(e) = save(root, &t) {
            eprintln!("样本溯源迁移失败(该样本退化为无溯源): {e}");
        }
    }
}

/// 样本文件被删除(合并淘汰/用户删样本)时清掉对应 receipt。调用方须已持 vp_guard。
pub fn on_sample_deleted(root: &Path, path: &Path) {
    let rel_p = rel(root, path);
    let mut t = load(root);
    let before = t.receipts.len();
    t.receipts.retain(|r| r.path != rel_p);
    if t.receipts.len() != before {
        if let Err(e) = save(root, &t) {
            eprintln!("样本溯源清理失败(残留一条指向已删文件的 receipt,无害): {e}");
        }
    }
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).to_string_lossy().into_owned()
}

// ── 质心贡献 receipt(append-only jsonl,只记不用) ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CentroidReceipt {
    pub note_id: String,
    pub cluster_id: String,
    pub resolved_person: String,
    pub source: String,
    pub count: u64,
    pub total_ms: u64,
    /// 本次是否新建了该人物(A 类)。
    pub created_person: bool,
    /// 本次写入向量的 sha256 hex(f32 LE 字节)。
    pub vec_hash: String,
    pub at: String,
}

pub fn append_centroid_receipts(root: &Path, rows: &[CentroidReceipt]) {
    use std::io::Write;
    if rows.is_empty() {
        return;
    }
    let res = (|| -> anyhow::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("centroid_receipts.jsonl"))?;
        for r in rows {
            let mut line = serde_json::to_string(r)?;
            line.push('\n');
            f.write_all(line.as_bytes())?;
        }
        Ok(())
    })();
    if let Err(e) = res {
        eprintln!("质心贡献 receipt 追加失败(诊断证据缺一段,不影响库): {e}");
    }
}

/// 全量读质心贡献 receipt(诊断/打标影响面用)。损坏行跳过。
pub fn read_centroid_receipts(root: &Path) -> Vec<CentroidReceipt> {
    std::fs::read_to_string(root.join("centroid_receipts.jsonl"))
        .map(|s| s.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
        .unwrap_or_default()
}

pub fn vec_hash(v: &[f32]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for x in v {
        h.update(x.to_le_bytes());
    }
    hex::encode(h.finalize())
}
