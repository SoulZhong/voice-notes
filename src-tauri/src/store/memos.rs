//! 会议备注(memo):整场一篇 Markdown 文档,每行(段落/标题/列表项)带一个笔记时间轴上的时间点。
//!
//! 存在笔记目录里独立的 `memo.md`,**不进**原始稿、修订稿、meta——那几份文件各有会整份
//! 重写它们的人(重转写换 segments、重新 Aing 重建 aing.json、Aing 写 meta),而备注永远
//! 属于用户:谁都不改写它。唯一的「系统改备注」是续录时 [`clamp_after`] 把超出上一场末尾
//! 的时间点钳回末尾(只改时间不改文字)。
//!
//! ## 文件格式
//!
//! 普通 Markdown。带时间点的行在**行尾**挂一个 HTML 注释 `<!-- mm:ss -->`(满一小时
//! `h:mm:ss`):别的 Markdown 软件打开看不见它,在应用外改文件也不会弄丢。
//!
//! ```text
//! ## 决议 <!-- 00:16 -->
//!
//! - 周二上线 <!-- 00:16 -->
//! - 回滚由 **李四** 负责 <!-- 00:31 -->
//!
//! 📍 <!-- 01:02 -->
//! ```
//!
//! `📍` 一行 = 时间标记(快捷键按下即钉、还没补文字)。段落里的硬换行(`\` 行尾)把多行
//! 连成一段,时间点挂在这段的最后一行。
//!
//! ## 两种读法
//!
//! - [`load_doc`]:整篇原文 + 版本号(编辑器用,保存时带回版本号做乐观校验)。
//! - [`load_memos`]:按行拆成 [`Memo`] 列表(只含带时间点的行,按时间升序)——导出、
//!   Aing 提示、MCP、搜索、时间轴小点都读这个视图,不必懂 Markdown。
//!
//! ## 旧数据
//!
//! v0.22.0 的条目式 `memos.json` 在第一次读到这篇笔记时自动转成 `memo.md`(每条一段,
//! 带原时间点),旧文件改名 `memos.json.bak` 留底,不删。
//!
//! 并发:进程内 `MEMO_LOCK` 串行读改写,另加跨进程文件锁 `.memos.lock`(MCP 是独立进程)。
//! 不经 lifecycle actor、不碰 `.note.lock`——录制中、Aing 中、重转写中都能写。

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

static MEMO_LOCK: Mutex<()> = Mutex::new(());
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub const MEMO_DOC_FILE: &str = "memo.md";
/// v0.22.0 的条目式存储;只在迁移时读。
pub const MEMOS_FILE: &str = "memos.json";
pub const MEMOS_LOCK_FILE: &str = ".memos.lock";
/// 快捷键/托盘一次追加的文字上限(按字符计)。
pub const MAX_TEXT_CHARS: usize = 2000;
/// 整篇备注上限(按字符计)。前端先拦,后端兜底拒绝。
pub const MAX_DOC_CHARS: usize = 200_000;
/// 时间标记的占位文字。视图里读成空串(与旧版「空文字 = 时间标记」同义)。
pub const MARKER: &str = "📍";

/// 备注的行视图(只读)。`id` 是行号 `L<n>`(1 起),文档一改就会变,只在一次读取内有意义。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memo {
    pub id: String,
    pub at_ms: u64,
    /// 去掉 Markdown 行首记号(`#`/`-`/`1.`/`>`/`- [ ]`)后的文字;时间标记为空串。
    #[serde(default)]
    pub text: String,
    /// 这一行的 Markdown 原文(去掉时间注释)。导出按它原样排版。
    #[serde(default)]
    pub md: String,
    /// 这一行在文档里的行号(1 起)。导出按文档顺序排。
    #[serde(default)]
    pub line: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoList {
    #[serde(default)]
    pub memos: Vec<Memo>,
}

/// 整篇备注 + 版本号。版本号是内容的 64 位 FNV-1a,空文档为 `"0"`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoDoc {
    pub text: String,
    pub rev: String,
}

impl MemoDoc {
    fn of(text: String) -> Self {
        let rev = rev_of(&text);
        Self { text, rev }
    }
}

#[derive(Debug)]
pub enum SaveError {
    /// 盘上版本与 `base_rev` 不一致:别处(快捷键/浮窗/另一个窗口/MCP)刚改过。附盘上现状。
    Conflict(MemoDoc),
    Other(anyhow::Error),
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveError::Conflict(_) => write!(f, "备注已在别处被修改"),
            SaveError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl From<anyhow::Error> for SaveError {
    fn from(e: anyhow::Error) -> Self {
        SaveError::Other(e)
    }
}

pub fn rev_of(text: &str) -> String {
    if text.is_empty() {
        return "0".into();
    }
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

// ── 时间注释 ────────────────────────────────────────────────────────────

/// `mm:ss`(满一小时 `h:mm:ss`)。导出、Aing 提示、文件格式共用。
pub fn fmt_clock(ms: u64) -> String {
    let s = ms / 1000;
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m:02}:{s:02}") }
}

/// [`fmt_clock`] 的逆:`mm:ss` / `h:mm:ss`,秒与分两位以内。
pub fn parse_clock(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.trim().split(':').collect();
    if !(2..=3).contains(&parts.len()) || parts.iter().any(|p| p.is_empty() || p.len() > 3 || !p.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let n: Vec<u64> = parts.iter().map(|p| p.parse().unwrap_or(0)).collect();
    let (h, m, s) = if n.len() == 3 { (n[0], n[1], n[2]) } else { (0, n[0], n[1]) };
    if s >= 60 || (n.len() == 3 && m >= 60) {
        return None;
    }
    Some(((h * 3600) + m * 60 + s) * 1000)
}

pub fn stamp(ms: u64) -> String {
    format!("<!-- {} -->", fmt_clock(ms))
}

/// 拆出行尾的时间注释:`("正文", Some(ms))`;没有则 `(原行, None)`。
pub fn split_stamp(line: &str) -> (&str, Option<u64>) {
    let t = line.trim_end();
    if let Some(body) = t.strip_suffix("-->") {
        if let Some(i) = body.rfind("<!--") {
            if let Some(ms) = parse_clock(&body[i + 4..]) {
                return (t[..i].trim_end(), Some(ms));
            }
        }
    }
    (line, None)
}

/// 去掉一行的 Markdown 行首记号,得到给人/给 AI 看的文字。
pub fn strip_md_prefix(md: &str) -> &str {
    let mut s = md.trim_start();
    // 引用可叠在列表前,先剥引用
    while let Some(r) = s.strip_prefix('>') {
        s = r.trim_start();
    }
    if let Some(r) = s.strip_prefix('#') {
        let r = r.trim_start_matches('#');
        if r.starts_with(' ') || r.is_empty() {
            return r.trim();
        }
    }
    for p in ["- [ ] ", "- [x] ", "- [X] ", "* [ ] ", "* [x] ", "- ", "* ", "+ "] {
        if let Some(r) = s.strip_prefix(p) {
            return r.trim();
        }
    }
    let digits = s.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits > 0 && digits <= 9 {
        let rest = &s[digits..];
        if let Some(r) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return r.trim();
        }
    }
    s.trim()
}

/// 把文档拆成行视图(只含带时间点的行),按时间升序、同时间按行号。硬换行(行尾 `\`)
/// 续起来的前几行并进时间点所在的那一段。
pub fn parse_lines(doc: &str) -> Vec<Memo> {
    let mut out = Vec::new();
    let mut carry: Vec<String> = Vec::new();
    for (i, raw) in doc.lines().enumerate() {
        let (body, at) = split_stamp(raw);
        match at {
            Some(at_ms) => {
                let mut md = String::new();
                for c in carry.drain(..) {
                    md.push_str(&c);
                    md.push(' ');
                }
                md.push_str(body);
                let first = strip_md_prefix(&md).to_string();
                let text = if first == MARKER { String::new() } else { first };
                out.push(Memo { id: format!("L{}", i + 1), at_ms, text, md: md.trim_end().to_string(), line: i + 1 });
            }
            None => {
                let t = raw.trim_end();
                match t.strip_suffix('\\') {
                    Some(head) if !t.is_empty() => carry.push(head.trim_end().to_string()),
                    _ => carry.clear(),
                }
            }
        }
    }
    out.sort_by(|a, b| a.at_ms.cmp(&b.at_ms).then(a.line.cmp(&b.line)));
    out
}

/// 一段可能多行的纯文字 → 一个 Markdown 段落(多行用硬换行 `\` 连接),行尾挂时间点。
fn paragraph(text: &str, at_ms: u64) -> String {
    let t = text.trim();
    let body = if t.is_empty() {
        MARKER.to_string()
    } else {
        t.lines().map(str::trim_end).collect::<Vec<_>>().join("\\\n")
    };
    format!("{body} {}", stamp(at_ms))
}

fn append_block(doc: &mut String, block: &str) {
    let trimmed_len = doc.trim_end().len();
    doc.truncate(trimmed_len);
    if !doc.is_empty() {
        doc.push_str("\n\n");
    }
    doc.push_str(block);
    doc.push('\n');
}

// ── 盘 ─────────────────────────────────────────────────────────────────

fn write_atomic(note_dir: &Path, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
    let p = note_dir.join(name);
    let tmp = note_dir.join(format!(
        ".{name}.tmp-{}-{}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    if let Err(e) = std::fs::write(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    if let Err(e) = std::fs::rename(&tmp, &p) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

#[derive(Deserialize)]
struct LegacyMemo {
    at_ms: u64,
    #[serde(default)]
    text: String,
    #[serde(default)]
    created_at: String,
}

#[derive(Deserialize)]
struct LegacyList {
    #[serde(default)]
    memos: Vec<LegacyMemo>,
}

/// 旧条目表 → 文档。按时间升序,每条一段。
fn legacy_to_doc(list: &LegacyList) -> String {
    let mut ms: Vec<&LegacyMemo> = list.memos.iter().collect();
    ms.sort_by(|a, b| a.at_ms.cmp(&b.at_ms).then_with(|| a.created_at.cmp(&b.created_at)));
    let mut doc = String::new();
    for m in ms {
        append_block(&mut doc, &paragraph(&m.text, m.at_ms));
    }
    doc
}

/// 持锁(进程内 + 跨进程)执行 `f`。
fn locked<T>(note_dir: &Path, f: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
    if !note_dir.is_dir() {
        anyhow::bail!("笔记不存在");
    }
    let _g = MEMO_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _flock = super::notelock::NoteLock::acquire_opened(|| {
        std::fs::OpenOptions::new().create(true).write(true).open(note_dir.join(MEMOS_LOCK_FILE))
    })
    .map_err(|e| anyhow::anyhow!("备注锁不可用: {e}"))?
    .ok_or_else(|| anyhow::anyhow!("另一处正在修改这篇笔记的备注,请重试"))?;
    f()
}

/// 锁内读当前文档;需要时先做旧数据迁移。
fn read_locked(note_dir: &Path) -> anyhow::Result<String> {
    let doc_path = note_dir.join(MEMO_DOC_FILE);
    if doc_path.is_file() {
        return Ok(std::fs::read_to_string(&doc_path)?);
    }
    let legacy = note_dir.join(MEMOS_FILE);
    let Ok(bytes) = std::fs::read(&legacy) else {
        return Ok(String::new());
    };
    match serde_json::from_slice::<LegacyList>(&bytes) {
        Ok(list) => {
            let doc = legacy_to_doc(&list);
            if !doc.is_empty() {
                write_atomic(note_dir, MEMO_DOC_FILE, doc.as_bytes())?;
            }
            std::fs::rename(&legacy, note_dir.join(format!("{MEMOS_FILE}.bak")))?;
            eprintln!("备注已从 memos.json 转成 memo.md({} 条): {}", list.memos.len(), note_dir.display());
            Ok(doc)
        }
        Err(_) => {
            let stamp = chrono::Local::now().format("%Y%m%dT%H%M%S");
            let bak = note_dir.join(format!("{MEMOS_FILE}.corrupt-{stamp}"));
            std::fs::rename(&legacy, &bak)?;
            eprintln!("memos.json 解析失败,已改名留底: {}", bak.display());
            Ok(String::new())
        }
    }
}

fn needs_migration(note_dir: &Path) -> bool {
    !note_dir.join(MEMO_DOC_FILE).is_file() && note_dir.join(MEMOS_FILE).is_file()
}

/// 读整篇备注。无文件为空文档;有旧 `memos.json` 时先迁移(迁移失败按空文档读并打日志——
/// 备注是增值层,不阻断笔记打开;旧文件原样留在盘上,下次再试)。
pub fn load_doc(note_dir: &Path) -> MemoDoc {
    if needs_migration(note_dir) {
        match locked(note_dir, || read_locked(note_dir)) {
            Ok(t) => return MemoDoc::of(t),
            Err(e) => {
                eprintln!("备注迁移失败({}): {e}", note_dir.display());
                return MemoDoc::of(String::new());
            }
        }
    }
    MemoDoc::of(std::fs::read_to_string(note_dir.join(MEMO_DOC_FILE)).unwrap_or_default())
}

/// 行视图(带时间点的行,时间升序)。
pub fn load_memos(note_dir: &Path) -> MemoList {
    MemoList { memos: parse_lines(&load_doc(note_dir).text) }
}

fn write_doc(note_dir: &Path, text: &str) -> anyhow::Result<()> {
    if text.trim().is_empty() {
        match std::fs::remove_file(note_dir.join(MEMO_DOC_FILE)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    } else {
        write_atomic(note_dir, MEMO_DOC_FILE, text.as_bytes())
    }
}

/// 编辑器整篇保存。`base_rev` 必须等于盘上版本,否则返回 [`SaveError::Conflict`] 附现状,
/// 由调用方合并后重试。全空白的文档删掉文件。
pub fn save_doc(note_dir: &Path, text: &str, base_rev: &str) -> Result<MemoDoc, SaveError> {
    if text.chars().count() > MAX_DOC_CHARS {
        return Err(SaveError::Other(anyhow::anyhow!("备注太长(上限 {MAX_DOC_CHARS} 字)")));
    }
    let mut conflict = None;
    let out = locked(note_dir, || {
        let cur = read_locked(note_dir)?;
        if rev_of(&cur) != base_rev {
            conflict = Some(MemoDoc::of(cur));
            return Ok(None);
        }
        let next = if text.trim().is_empty() { String::new() } else { text.to_string() };
        write_doc(note_dir, &next)?;
        Ok(Some(MemoDoc::of(next)))
    })?;
    match (out, conflict) {
        (Some(d), _) => Ok(d),
        (None, Some(c)) => Err(SaveError::Conflict(c)),
        (None, None) => unreachable!("save_doc 锁内要么写成要么冲突"),
    }
}

/// 读改写的通用入口(追加/浮窗改行/续录钳位)。`f` 返回 None 表示不必写盘。
fn modify(note_dir: &Path, f: impl FnOnce(&str) -> anyhow::Result<Option<String>>) -> anyhow::Result<MemoDoc> {
    locked(note_dir, || {
        let cur = read_locked(note_dir)?;
        match f(&cur)? {
            Some(next) => {
                if next.chars().count() > MAX_DOC_CHARS {
                    anyhow::bail!("备注太长(上限 {MAX_DOC_CHARS} 字)");
                }
                write_doc(note_dir, &next)?;
                Ok(MemoDoc::of(next))
            }
            None => Ok(MemoDoc::of(cur)),
        }
    })
}

fn check_text(text: &str) -> anyhow::Result<()> {
    if text.chars().count() > MAX_TEXT_CHARS {
        anyhow::bail!("备注太长(上限 {MAX_TEXT_CHARS} 字)");
    }
    Ok(())
}

/// 在文档末尾追加一段(快捷键/托盘/MCP)。空文字 = 时间标记 `📍`。返回新文档与新行的视图。
pub fn append(note_dir: &Path, at_ms: u64, text: &str) -> anyhow::Result<(Memo, MemoDoc)> {
    check_text(text)?;
    let block = paragraph(text, at_ms);
    let doc = modify(note_dir, |cur| {
        let mut next = cur.to_string();
        append_block(&mut next, &block);
        Ok(Some(next))
    })?;
    let line = doc.text.trim_end().lines().count();
    let memo = parse_lines(&doc.text)
        .into_iter()
        .find(|m| m.line == line)
        .expect("刚追加的行必带时间点");
    Ok((memo, doc))
}

/// 浮窗补字:把「时间点 = `at_ms` 且文字 = `old_text`」的那一段(从后往前找第一处)换成
/// `new_text`;找不到(用户已在编辑器里改过/删了)就追加一段新的,不丢字。
pub fn edit_line(note_dir: &Path, at_ms: u64, old_text: &str, new_text: &str) -> anyhow::Result<MemoDoc> {
    check_text(new_text)?;
    let new_block = paragraph(new_text, at_ms);
    let old_view = if old_text.trim() == MARKER { "" } else { old_text.trim() };
    modify(note_dir, |cur| {
        let views = parse_lines(cur);
        let hit = views.iter().rev().filter(|m| m.at_ms == at_ms && m.text == old_view).max_by_key(|m| m.line);
        let mut lines: Vec<&str> = cur.lines().collect();
        let next = match hit {
            Some(m) => {
                // 连同硬换行续起来的前几行一起替换
                let end = m.line - 1;
                let mut start = end;
                while start > 0 && lines[start - 1].trim_end().ends_with('\\') {
                    start -= 1;
                }
                let replaced: Vec<&str> = new_block.lines().collect();
                lines.splice(start..=end, replaced);
                let mut s = lines.join("\n");
                s.push('\n');
                s
            }
            None => {
                let mut s = cur.to_string();
                append_block(&mut s, &new_block);
                s
            }
        };
        Ok(Some(next))
    })
}

/// 续录专用:把 `at_ms > base_ms` 的时间点钳到 `base_ms`(上一场末尾之后的静音被切掉了,
/// 不钳就会落进第二场开头的内容里)。只动时间,不动文字。没有要钳的就不写盘。返回被钳的行数。
pub fn clamp_after(note_dir: &Path, base_ms: u64) -> anyhow::Result<usize> {
    if !load_memos(note_dir).memos.iter().any(|m| m.at_ms > base_ms) {
        return Ok(0);
    }
    let mut n = 0;
    modify(note_dir, |cur| {
        let mut out = String::with_capacity(cur.len());
        for raw in cur.split_inclusive('\n') {
            let (line, nl) = match raw.strip_suffix('\n') {
                Some(l) => (l, "\n"),
                None => (raw, ""),
            };
            match split_stamp(line) {
                (body, Some(at)) if at > base_ms => {
                    n += 1;
                    out.push_str(&format!("{body} {}{nl}", stamp(base_ms)));
                }
                _ => {
                    out.push_str(line);
                    out.push_str(nl);
                }
            }
        }
        Ok(Some(out))
    })?;
    Ok(n)
}

/// 时间点的统一钳位:下限 0(入参已是 u64),上限 `max_ms`(录制中 = 当前 elapsed,
/// 会后 = 笔记时长)。超出钳位而非报错——拖时间轴拖过头是常态。
pub fn clamp_at(at_ms: u64, max_ms: u64) -> u64 {
    at_ms.min(max_ms)
}

/// 半开区间判定:落在任一删减区间 `[start, end)` 内。
pub fn in_cut(at_ms: u64, cuts: &[super::edits::CutRange]) -> bool {
    cuts.iter().any(|c| c.start_ms <= at_ms && at_ms < c.end_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(d: &Path) -> Vec<(u64, String)> {
        load_memos(d).memos.into_iter().map(|m| (m.at_ms, m.text)).collect()
    }

    #[test]
    fn clock_round_trip() {
        assert_eq!(fmt_clock(754_000), "12:34");
        assert_eq!(fmt_clock(3_754_000), "1:02:34");
        assert_eq!(parse_clock("12:34"), Some(754_000));
        assert_eq!(parse_clock("1:02:34"), Some(3_754_000));
        assert_eq!(parse_clock(" 00:05 "), Some(5_000));
        assert_eq!(parse_clock("1:60"), None);
        assert_eq!(parse_clock("abc"), None);
        assert_eq!(parse_clock("12"), None);
    }

    #[test]
    fn parses_lines_with_markdown_prefixes() {
        let doc = "## 决议 <!-- 00:16 -->\n\n- 周二上线 <!-- 00:16 -->\n- [ ] 回滚由 **李四** 负责 <!-- 00:31 -->\n\n没有时间的一行\n\n📍 <!-- 01:02 -->\n> 引用 <!-- 00:05 -->\n1. 第一 <!-- 00:40 -->\n";
        let v = parse_lines(doc);
        let got: Vec<_> = v.iter().map(|m| (m.at_ms, m.text.as_str())).collect();
        assert_eq!(
            got,
            [(5_000, "引用"), (16_000, "决议"), (16_000, "周二上线"), (31_000, "回滚由 **李四** 负责"), (40_000, "第一"), (62_000, "")]
        );
        assert_eq!(v[1].md, "## 决议");
        assert_eq!(v[1].line, 1);
    }

    #[test]
    fn hard_break_lines_join_into_one_memo() {
        let doc = "第一行\\\n第二行 <!-- 00:03 -->\n";
        let v = parse_lines(doc);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].text, "第一行 第二行");
    }

    #[test]
    fn append_and_edit_line_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let (m, doc) = append(d.path(), 7_000, "").unwrap();
        assert_eq!((m.at_ms, m.text.as_str()), (7_000, ""));
        assert_eq!(doc.text, "📍 <!-- 00:07 -->\n");
        append(d.path(), 9_000, "第二\n两行").unwrap();
        let doc = edit_line(d.path(), 7_000, "", "补上的字").unwrap();
        assert_eq!(doc.text, "补上的字 <!-- 00:07 -->\n\n第二\\\n两行 <!-- 00:09 -->\n");
        // 找不到原行(被用户删了)→ 追加,不丢字
        let doc = edit_line(d.path(), 1_000, "不存在", "新的").unwrap();
        assert!(doc.text.ends_with("新的 <!-- 00:01 -->\n"));
        // 多行段落也能整段替换
        let doc = edit_line(d.path(), 9_000, "第二 两行", "改好").unwrap();
        assert!(doc.text.contains("\n\n改好 <!-- 00:09 -->\n"), "{}", doc.text);
        assert!(!doc.text.contains("两行"));
    }

    #[test]
    fn save_doc_checks_rev_and_empty_removes_file() {
        let d = tempfile::tempdir().unwrap();
        let empty = load_doc(d.path());
        assert_eq!(empty.rev, "0");
        let saved = save_doc(d.path(), "a <!-- 00:01 -->\n", &empty.rev).unwrap();
        assert_eq!(load_doc(d.path()), saved);
        match save_doc(d.path(), "b", "stale") {
            Err(SaveError::Conflict(cur)) => assert_eq!(cur, saved),
            other => panic!("应冲突: {other:?}"),
        }
        save_doc(d.path(), "  \n", &saved.rev).unwrap();
        assert!(!d.path().join(MEMO_DOC_FILE).exists());
        assert!(save_doc(d.path(), &"字".repeat(MAX_DOC_CHARS + 1), "0").is_err());
    }

    #[test]
    fn missing_dir_is_note_not_found() {
        let d = tempfile::tempdir().unwrap();
        let err = append(&d.path().join("gone"), 0, "x").unwrap_err();
        assert!(err.to_string().contains("笔记不存在"));
    }

    #[test]
    fn legacy_json_migrates_once_and_is_kept_as_bak() {
        let d = tempfile::tempdir().unwrap();
        let legacy = r#"{"schema_version":1,"memos":[
            {"id":"m2","at_ms":65000,"text":"后","origin":"note","created_at":"2","updated_at":"2"},
            {"id":"m1","at_ms":5000,"text":"定了\n周一封版","origin":"composer","created_at":"1","updated_at":"1"},
            {"id":"m3","at_ms":9000,"text":"","origin":"hotkey","created_at":"3","updated_at":"3"}]}"#;
        std::fs::write(d.path().join(MEMOS_FILE), legacy).unwrap();
        assert_eq!(texts(d.path()), [(5_000, "定了 周一封版".into()), (9_000, String::new()), (65_000, "后".into())]);
        assert!(!d.path().join(MEMOS_FILE).exists());
        assert!(d.path().join("memos.json.bak").is_file());
        let doc = std::fs::read_to_string(d.path().join(MEMO_DOC_FILE)).unwrap();
        assert_eq!(doc, "定了\\\n周一封版 <!-- 00:05 -->\n\n📍 <!-- 00:09 -->\n\n后 <!-- 01:05 -->\n");
        // 第二次读不再迁移、内容不变
        assert_eq!(load_doc(d.path()).text, doc);
    }

    #[test]
    fn corrupt_legacy_is_kept_aside() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(MEMOS_FILE), b"{not json").unwrap();
        assert!(load_memos(d.path()).memos.is_empty());
        let baks: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("memos.json.corrupt-"))
            .collect();
        assert_eq!(baks.len(), 1);
        assert_eq!(std::fs::read(baks[0].path()).unwrap(), b"{not json");
    }

    #[test]
    fn clamp_after_only_moves_later_stamps_and_keeps_text() {
        let d = tempfile::tempdir().unwrap();
        append(d.path(), 1_000, "早").unwrap();
        append(d.path(), 600_000, "晚").unwrap();
        assert_eq!(clamp_after(d.path(), 570_000).unwrap(), 1);
        assert_eq!(texts(d.path()), [(1_000, "早".into()), (570_000, "晚".into())]);
        let before = std::fs::metadata(d.path().join(MEMO_DOC_FILE)).unwrap().modified().unwrap();
        assert_eq!(clamp_after(d.path(), 570_000).unwrap(), 0);
        assert_eq!(std::fs::metadata(d.path().join(MEMO_DOC_FILE)).unwrap().modified().unwrap(), before);
    }

    #[test]
    fn clamp_and_cut_helpers() {
        assert_eq!(clamp_at(5_000, 3_000), 3_000);
        assert_eq!(clamp_at(1_000, 3_000), 1_000);
        let cuts = [super::super::edits::CutRange { start_ms: 1_000, end_ms: 2_000 }];
        assert!(in_cut(1_000, &cuts));
        assert!(!in_cut(2_000, &cuts));
    }

    #[test]
    fn strip_prefix_cases() {
        assert_eq!(strip_md_prefix("### 标题"), "标题");
        assert_eq!(strip_md_prefix("#话题"), "#话题");
        assert_eq!(strip_md_prefix("- [x] 做完"), "做完");
        assert_eq!(strip_md_prefix("12. 第十二"), "第十二");
        assert_eq!(strip_md_prefix("> - 引用里的列表"), "引用里的列表");
        assert_eq!(strip_md_prefix("普通"), "普通");
    }
}
