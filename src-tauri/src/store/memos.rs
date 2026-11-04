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

/// 整篇备注 + 版本号。版本号是写入序号(十进制字符串):每写一次加 1,存在 `.memo.seq`,
/// 与 memo.md 同锁读写。用序号而不是内容哈希:内容相同不代表是同一次写入(撤销回旧内容
/// 后哈希不变,编辑器会误当成旧事件丢掉,Codex 审出);序号只增不减,新旧一比就知道。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoDoc {
    pub text: String,
    pub rev: String,
}

impl MemoDoc {
    fn at(text: String, seq: u64) -> Self {
        Self { text, rev: seq.to_string() }
    }
}

pub const MEMO_SEQ_FILE: &str = ".memo.seq";

/// 当前版本号:`.memo.seq` 记着「序号 + 写入时正文的指纹」。指纹与盘上正文对不上(正文写成、
/// 序号没写成;或用户在应用外改过文件)就把这份正文算作更新的一版(序号 + 1)——正文与序号
/// 分两个文件,不能假定总是一起落盘(Codex 审出)。
fn read_seq(note_dir: &Path, text: &str) -> u64 {
    let raw = std::fs::read_to_string(note_dir.join(MEMO_SEQ_FILE)).unwrap_or_default();
    let mut it = raw.split_whitespace();
    let n: u64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    match it.next() {
        Some(h) if h == text_hash(text) => n,
        Some(_) => n + 1,
        // 没有指纹(从没写过):空文档 0,有正文(迁移前的旧格式或外部放进来的)算 1
        None if n == 0 => u64::from(!text.is_empty()),
        None => n,
    }
}

fn text_hash(text: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

// ── 按行三方合并(diff3) ───────────────────────────────────────────────────

/// 按逻辑行切分:以 `\` 结尾的行与下一行连成一个单位(硬换行段落整体进出)。
fn units(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in s.trim_end_matches('\n').split('\n') {
        if let Some(last) = out.last_mut() {
            if last.ends_with('\\') {
                last.push('\n');
                last.push_str(line);
                continue;
            }
        }
        out.push(line.to_string());
    }
    out
}

/// 最长公共子序列的匹配对 (i, j),i/j 均严格递增。备注篇幅是几十到几百行,O(n·m) 足够。
fn lcs_pairs(a: &[String], b: &[String]) -> Vec<(usize, usize)> {
    let (n, m) = (a.len(), b.len());
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] { dp[i + 1][j + 1] + 1 } else { dp[i + 1][j].max(dp[i][j + 1]) };
        }
    }
    let (mut i, mut j, mut out) = (0, 0, Vec::new());
    while i < n && j < m {
        if a[i] == b[j] {
            out.push((i, j));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

/// `o` 是否是 `x` 的子序列(x 只是在 o 上插了行);是则返回 (插在 o 之前的行, 其余插入的行)。
fn only_inserted(o: &[String], x: &[String]) -> Option<(Vec<String>, Vec<String>)> {
    let mut k = 0;
    let mut before = Vec::new();
    let mut rest = Vec::new();
    for l in x {
        if k < o.len() && *l == o[k] {
            k += 1;
        } else if k == 0 {
            before.push(l.clone());
        } else {
            rest.push(l.clone());
        }
    }
    (k == o.len()).then_some((before, rest))
}

fn push_extra(out: &mut Vec<String>, extra: impl IntoIterator<Item = String>, have: &[String]) {
    let extra: Vec<String> = extra.into_iter().filter(|l| !l.trim().is_empty() && !have.contains(l)).collect();
    if extra.is_empty() {
        return;
    }
    if out.last().is_some_and(|l| !l.trim().is_empty()) {
        out.push(String::new());
    }
    out.extend(extra);
}

/// 一段三方都不稳定的区间怎么定:
/// - 一侧没动取另一侧;两侧一样取其一;
/// - 行数都相同:逐行按位置定(同一位置一侧没动取另一侧,都改了两边都留);
/// - 一侧只是插了行(base 是它的子序列):取另一侧的改动,再把插入的行放回原位(前/后);
/// - 其余:两边都留(本地在前,远端里本地没有的行跟在后面),不丢字。
fn resolve(o: &[String], a: &[String], b: &[String], out: &mut Vec<String>) {
    if a == o {
        out.extend_from_slice(b);
    } else if b == o || a == b {
        out.extend_from_slice(a);
    } else if !o.is_empty() && a.len() == o.len() && b.len() == o.len() {
        for k in 0..o.len() {
            if a[k] == o[k] {
                out.push(b[k].clone());
            } else if b[k] == o[k] || a[k] == b[k] {
                out.push(a[k].clone());
            } else {
                out.push(a[k].clone());
                out.push(String::new());
                out.push(b[k].clone());
            }
        }
    } else if o.is_empty() {
        out.extend_from_slice(a);
        push_extra(out, b.iter().cloned(), a);
    } else if let Some((before, rest)) = only_inserted(o, a) {
        out.extend(before);
        out.extend_from_slice(b);
        push_extra(out, rest, b);
    } else if let Some((before, rest)) = only_inserted(o, b) {
        let start = out.len();
        out.extend(before.into_iter().filter(|l| !a.contains(l)));
        if out.len() > start && a.first().is_some_and(|l| !l.trim().is_empty()) {
            out.push(String::new());
        }
        out.extend_from_slice(a);
        push_extra(out, rest, a);
    } else {
        out.extend_from_slice(a);
        push_extra(out, b.iter().cloned(), a);
    }
}

/// 三方合并:`base` 是 `local` 所依据的版本,`remote` 是盘上现状。按逻辑行做 diff3:三方都
/// 对得上的行是锚点,锚点之间的区间按 [`resolve`] 定。按位置对齐而不是按内容找行——同一时间点
/// 的两段文字可以一样,不能拿正文当段落身份(Codex 审出)。冲突时两边都留,不丢字。
/// 与前端 memoDocMerge.ts 同一算法。
pub fn merge3(base: &str, local: &str, remote: &str) -> String {
    if local == base {
        return remote.to_string();
    }
    if remote == base || remote == local {
        return local.to_string();
    }
    let (o, a, b) = (units(base), units(local), units(remote));
    let ma: std::collections::HashMap<usize, usize> = lcs_pairs(&o, &a).into_iter().collect();
    let mb: std::collections::HashMap<usize, usize> = lcs_pairs(&o, &b).into_iter().collect();
    let mut out = Vec::new();
    let (mut oi, mut ai, mut bi) = (0, 0, 0);
    loop {
        let anchor = (oi..o.len()).find(|k| {
            matches!((ma.get(k), mb.get(k)), (Some(&x), Some(&y)) if x >= ai && y >= bi)
        });
        match anchor {
            Some(k) => {
                let (x, y) = (ma[&k], mb[&k]);
                resolve(&o[oi..k], &a[ai..x], &b[bi..y], &mut out);
                out.push(o[k].clone());
                oi = k + 1;
                ai = x + 1;
                bi = y + 1;
            }
            None => {
                resolve(&o[oi..], &a[ai..], &b[bi..], &mut out);
                break;
            }
        }
    }
    let mut text = out.join("\n");
    while text.contains("\n\n\n") {
        text = text.replace("\n\n\n", "\n\n");
    }
    let text = text.trim_matches('\n');
    if text.trim().is_empty() {
        String::new()
    } else {
        format!("{text}\n")
    }
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
                let first = unescape_md(strip_md_prefix(&md));
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

/// 纯文字的一行 → 不会被当成 Markdown 结构的一行:行首的块记号(`#` `>` `-` `+` `*` `1.`)
/// 与 `<`(HTML 块)前加反斜杠、行尾反斜杠(会变成硬换行)也转义;`<!--` 换成实体,免得被当成时间注释。
/// 旧数据迁移与快捷键浮窗写入的都是用户的纯文字,不转义的话「第一行\n- 第二行」会被拆成
/// 段落 + 列表,时间点只挂到后一半,前一半下次编辑时被重新打上当前时间(Codex 审出)。
pub fn escape_plain_line(line: &str) -> String {
    let lead = line.len() - line.trim_start().len();
    let (indent, rest) = line.split_at(lead);
    let rest = rest.replace("<!--", "&lt;!--");
    let bytes = rest.as_bytes();
    // 只在真会被当成块结构时才转义:编辑器序列化不会给普通文字加反斜杠,多转义会让盘上原文
    // 与编辑器写法不一致,合并时冒出假差异(Codex 审出)。
    let spaced = |i: usize| bytes.get(i).is_none_or(|c| *c == b' ' || *c == b'\t');
    let same_all = |c: u8| !bytes.is_empty() && bytes.iter().all(|x| *x == c || *x == b' ');
    let hashes = bytes.iter().take_while(|c| **c == b'#').count();
    let digits = bytes.iter().take_while(|c| c.is_ascii_digit()).count();
    let block = match bytes.first() {
        Some(b'#') => hashes <= 6 && spaced(hashes),
        Some(b'-' | b'+' | b'*') => spaced(1) || same_all(bytes[0]),
        Some(b'=') => same_all(b'='),
        Some(b'>' | b'<') => true,
        Some(b'`') => rest.starts_with("```"),
        Some(b'~') => rest.starts_with("~~~"),
        _ => false,
    };
    let mut out = String::from(indent);
    if block {
        out.push('\\');
        out.push_str(&rest);
    } else if digits > 0 && digits <= 9 && matches!(bytes.get(digits), Some(b'.' | b')')) && spaced(digits + 1) {
        out.push_str(&rest[..digits]);
        out.push('\\');
        out.push_str(&rest[digits..]);
    } else {
        out.push_str(&rest);
    }
    if out.ends_with('\\') {
        out.push('\\');
    }
    out
}

/// [`escape_plain_line`] 的逆(只为视图文字:去掉标点前的转义反斜杠)。
pub fn unescape_md(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\\' {
            if let Some(&n) = it.peek() {
                if n.is_ascii_punctuation() {
                    out.push(n);
                    it.next();
                    continue;
                }
            }
        }
        out.push(c);
    }
    out.replace("&lt;!--", "<!--")
}

/// 一段可能多行的纯文字 → 一个 Markdown 段落(逐行转义,多行用硬换行 `\` 连接),行尾挂时间点。
fn paragraph(text: &str, at_ms: u64) -> String {
    let t = text.trim();
    let body = if t.is_empty() {
        MARKER.to_string()
    } else {
        t.lines().map(|l| escape_plain_line(l.trim_end())).collect::<Vec<_>>().join("\\\n")
    };
    format!("{body} {}", stamp(at_ms))
}

/// 导出用的一行:整篇文档按物理行,带时间注释的行 `at_ms` 是自己的时间点,其余行(代码块、
/// 表格、续行、空行……)继承上一个时间点(文档开头没有时为 0)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportLine {
    /// 去掉时间注释后的原文(保留缩进)。
    pub md: String,
    pub at_ms: u64,
    pub stamped: bool,
}

///
/// 硬换行(行尾 `\`)连起来的多行段落是一个整体:前几行与末行同一时间点、同进同出(按时间
/// 范围圈选导出时不会只剩半段,Codex 审出)。只有末行带时间注释,前几行 `stamped=false`。
pub fn export_lines(doc: &str) -> Vec<ExportLine> {
    let raws: Vec<&str> = doc.lines().collect();
    let mut out = Vec::with_capacity(raws.len());
    let mut last = 0u64;
    let mut i = 0;
    while i < raws.len() {
        // 一个逻辑块:连续以 `\` 结尾的行 + 其后一行
        let mut j = i;
        while j + 1 < raws.len() && raws[j].trim_end().ends_with('\\') && !raws[j].trim().is_empty() {
            j += 1;
        }
        let (_, block_at) = split_stamp(raws[j]);
        let at = block_at.unwrap_or(last);
        for (k, raw) in raws.iter().enumerate().take(j + 1).skip(i) {
            let (body, own) = split_stamp(raw);
            out.push(ExportLine { md: body.trim_end().to_string(), at_ms: at, stamped: k == j && own.is_some() });
        }
        if let Some(a) = block_at {
            last = a;
        }
        i = j + 1;
    }
    out
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
                write_doc(note_dir, &doc, 0)?;
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

/// 锁内读:正文与版本号是同一版(编辑器取盘用)。锁拿不到返回错误,由调用方重试——
/// 无锁拼出来的「旧正文 + 新序号」会让编辑器把真正的新版本当成旧的丢掉(Codex 审出)。
pub fn load_doc_strict(note_dir: &Path) -> anyhow::Result<MemoDoc> {
    locked(note_dir, || {
        let text = read_locked(note_dir)?;
        let seq = read_seq(note_dir, &text);
        Ok(MemoDoc::at(text, seq))
    })
}

/// 读整篇备注(导出、Aing、MCP、搜索等只读方)。无文件为空文档;有旧 `memos.json` 时先迁移
/// (失败按空文档读并打日志——备注是增值层,不阻断笔记打开;旧文件原样留在盘上,下次再试)。
/// 锁拿不到时退回无锁读正文:这些读方只用正文,不用版本号。
pub fn load_doc(note_dir: &Path) -> MemoDoc {
    match load_doc_strict(note_dir) {
        Ok(d) => d,
        Err(e) => {
            if note_dir.is_dir() {
                eprintln!("备注读取走无锁路径({}): {e}", note_dir.display());
            }
            let text = std::fs::read_to_string(note_dir.join(MEMO_DOC_FILE)).unwrap_or_default();
            let seq = read_seq(note_dir, &text);
            MemoDoc::at(text, seq)
        }
    }
}

/// 行视图(带时间点的行,时间升序)。
pub fn load_memos(note_dir: &Path) -> MemoList {
    MemoList { memos: parse_lines(&load_doc(note_dir).text) }
}

/// 写正文并记下新版本号(锁内调用;`cur_seq` 是写之前的版本号)。全空白删掉正文文件。
/// 正文写成即算成功:序号文件写失败只打日志——读方会按指纹不符把这份正文算作新版本。
fn write_doc(note_dir: &Path, text: &str, cur_seq: u64) -> anyhow::Result<u64> {
    write_text(note_dir, text)?;
    let next = cur_seq + 1;
    let on_disk = if text.trim().is_empty() { "" } else { text };
    if let Err(e) = write_atomic(note_dir, MEMO_SEQ_FILE, format!("{next} {}", text_hash(on_disk)).as_bytes()) {
        eprintln!("备注版本号写入失败(正文已写成,读方会按指纹自愈): {e}");
    }
    Ok(next)
}

fn write_text(note_dir: &Path, text: &str) -> anyhow::Result<()> {
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

/// 编辑器整篇保存。`base` = 编辑器这份 `text` 所依据的盘面原文:盘面没变就原样写;变了
/// (快捷键追加/浮窗补字/编辑器前一份保存合进了别处改动)就在锁内三方合并 `merge3(base, text,
/// 盘面)` 再写——合并在后端锁内做,前端不用处理冲突与重试,时序问题收敛到一处。
/// 合并结果与盘面相同则不写(序号不变)。全空白的文档删掉文件。
pub fn save_doc(note_dir: &Path, text: &str, base: &str) -> anyhow::Result<MemoDoc> {
    if text.chars().count() > MAX_DOC_CHARS {
        anyhow::bail!("备注太长(上限 {MAX_DOC_CHARS} 字)");
    }
    locked(note_dir, || {
        let cur = read_locked(note_dir)?;
        let cur_seq = read_seq(note_dir, &cur);
        let next = if cur == base { text.to_string() } else { merge3(base, text, &cur) };
        let next = if next.trim().is_empty() { String::new() } else { next };
        if next == cur {
            return Ok(MemoDoc::at(cur, cur_seq));
        }
        if next.chars().count() > MAX_DOC_CHARS {
            anyhow::bail!("备注太长(上限 {MAX_DOC_CHARS} 字)");
        }
        let seq = write_doc(note_dir, &next, cur_seq)?;
        Ok(MemoDoc::at(next, seq))
    })
}

/// 读改写的通用入口(追加/浮窗改行/续录钳位)。`f` 返回 None 表示不必写盘。
fn modify(note_dir: &Path, f: impl FnOnce(&str) -> anyhow::Result<Option<String>>) -> anyhow::Result<MemoDoc> {
    locked(note_dir, || {
        let cur = read_locked(note_dir)?;
        let cur_seq = read_seq(note_dir, &cur);
        match f(&cur)? {
            Some(next) => {
                if next.chars().count() > MAX_DOC_CHARS {
                    anyhow::bail!("备注太长(上限 {MAX_DOC_CHARS} 字)");
                }
                let seq = write_doc(note_dir, &next, cur_seq)?;
                Ok(MemoDoc::at(next, seq))
            }
            None => Ok(MemoDoc::at(cur, cur_seq)),
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
    fn save_doc_merges_on_server_and_bumps_seq() {
        let d = tempfile::tempdir().unwrap();
        let empty = load_doc(d.path());
        assert_eq!((empty.text.as_str(), empty.rev.as_str()), ("", "0"));
        let s1 = save_doc(d.path(), "a <!-- 00:01 -->\n", "").unwrap();
        assert_eq!(s1.rev, "1");
        assert_eq!(load_doc(d.path()), s1);
        // 快捷键在别处追加 → 盘面变了;编辑器基于旧盘面改了第一行 → 后端合并,两边都在
        let (_, after_append) = append(d.path(), 40_000, "").unwrap();
        assert_eq!(after_append.rev, "2");
        let s2 = save_doc(d.path(), "a 改 <!-- 00:01 -->\n", "a <!-- 00:01 -->\n").unwrap();
        assert_eq!(s2.text, "a 改 <!-- 00:01 -->\n\n📍 <!-- 00:40 -->\n");
        assert_eq!(s2.rev, "3");
        // 合并结果与盘面相同:不写、序号不变
        assert_eq!(save_doc(d.path(), &s2.text, &s2.text).unwrap().rev, "3");
        // 撤销回旧内容也是新的一次写入(序号增,不靠内容判新旧)
        let s4 = save_doc(d.path(), "a <!-- 00:01 -->\n", &s2.text).unwrap();
        assert_eq!((s4.text.as_str(), s4.rev.as_str()), ("a <!-- 00:01 -->\n", "4"));
        save_doc(d.path(), "  \n", &s4.text).unwrap();
        assert!(!d.path().join(MEMO_DOC_FILE).exists());
        assert!(save_doc(d.path(), &"字".repeat(MAX_DOC_CHARS + 1), "").is_err());
    }

    #[test]
    fn seq_heals_when_text_changed_without_seq() {
        let d = tempfile::tempdir().unwrap();
        let s1 = save_doc(d.path(), "a\n", "").unwrap();
        assert_eq!(s1.rev, "1");
        // 正文被改、序号没跟上(写到一半失败 / 应用外编辑):读方算作新一版
        std::fs::write(d.path().join(MEMO_DOC_FILE), "b\n").unwrap();
        assert_eq!(load_doc(d.path()).rev, "2");
        let s3 = save_doc(d.path(), "c\n", "b\n").unwrap();
        assert_eq!((s3.text.as_str(), s3.rev.as_str()), ("c\n", "3"));
        // 旧格式没有序号文件但有正文:算 1
        let e = tempfile::tempdir().unwrap();
        std::fs::write(e.path().join(MEMO_DOC_FILE), "x\n").unwrap();
        assert_eq!(load_doc(e.path()).rev, "1");
    }

    #[test]
    fn merge3_aligns_by_position_not_content() {
        // 同一时间点的两段可以文字相同:base=[A,B] local=[B,C] remote=[A,C] → [B,C](Codex 反例)
        assert_eq!(merge3("A\n\nB\n", "B\n\nC\n", "A\n\nC\n"), "B\n\nC\n");
        // 两边都改了同一行:都留,不丢字
        assert_eq!(merge3("a\n", "a1\n", "a2\n"), "a1\n\na2\n");
    }

    #[test]
    fn merge3_cases() {
        let base = "决议 <!-- 00:16 -->\n\n- 周二上线 <!-- 00:16 -->\n";
        assert_eq!(merge3(base, base, "x\n"), "x\n");
        assert_eq!(merge3(base, "y\n", base), "y\n");
        // 远端追加 + 本地改
        let local = "决议改了 <!-- 00:16 -->\n\n- 周二上线 <!-- 00:16 -->\n";
        assert_eq!(
            merge3(base, local, &format!("{base}\n📍 <!-- 00:40 -->\n")),
            "决议改了 <!-- 00:16 -->\n\n- 周二上线 <!-- 00:16 -->\n\n📍 <!-- 00:40 -->\n"
        );
        // 浮窗补字:本地那一行原地替换
        assert_eq!(
            merge3("a\n\n📍 <!-- 00:40 -->\n", "a 改\n\n📍 <!-- 00:40 -->\n", "a\n\n补 <!-- 00:40 -->\n"),
            "a 改\n\n补 <!-- 00:40 -->\n"
        );
        // 本地已删的行被远端改了:新行追加
        assert_eq!(merge3("a\n\n📍\n", "a\n\nb\n", "a\n\n补\n"), "a\n\nb\n\n补\n");
        // 远端删掉的行本地还在 → 删
        assert_eq!(merge3("a\n\nb\n", "a\n\nb\n\nc\n", "a\n"), "a\n\nc\n");
        // 硬换行段落整体追加
        assert_eq!(
            merge3("a\n", "a 改\n", "a\n\n第一\\\n第二 <!-- 00:20 -->\n"),
            "a 改\n\n第一\\\n第二 <!-- 00:20 -->\n"
        );
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
    fn plain_text_is_escaped_and_round_trips_in_view() {
        let d = tempfile::tempdir().unwrap();
        let (_, doc) = append(d.path(), 5_000, "第一行\n- 第二行\n1. 三\n# 四 <!-- x -->").unwrap();
        assert_eq!(doc.text, "第一行\\\n\\- 第二行\\\n1\\. 三\\\n\\# 四 &lt;!-- x --> <!-- 00:05 -->\n");
        let v = load_memos(d.path()).memos;
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].text, "第一行 - 第二行 1. 三 # 四 <!-- x -->");
        assert_eq!(escape_plain_line("结尾反斜杠\\"), "结尾反斜杠\\\\");
        assert_eq!(escape_plain_line("12) 编号"), "12\\) 编号");
        assert_eq!(escape_plain_line("<div>"), "\\<div>");
        // 不构成块结构的不转义(与编辑器写法一致)
        assert_eq!(escape_plain_line("-item"), "-item");
        assert_eq!(escape_plain_line("#话题"), "#话题");
        assert_eq!(escape_plain_line("2026.11 计划"), "2026.11 计划");
    }

    #[test]
    fn export_lines_inherit_last_stamp() {
        let doc = "  - 子项 <!-- 00:03 -->\n```\ncode\n```\n\n后 <!-- 00:09 -->\n";
        let v = export_lines(doc);
        assert_eq!(v[0], ExportLine { md: "  - 子项".into(), at_ms: 3_000, stamped: true });
        assert_eq!((v[2].md.as_str(), v[2].at_ms, v[2].stamped), ("code", 3_000, false));
        assert_eq!((v[5].at_ms, v[5].stamped), (9_000, true));
        // 硬换行段落:前半行跟末行同一时间
        let v = export_lines("早 <!-- 00:05 -->\n\n第一行\\\n第二行 <!-- 00:20 -->\n");
        assert_eq!((v[2].md.as_str(), v[2].at_ms, v[2].stamped), ("第一行\\", 20_000, false));
        assert_eq!((v[3].at_ms, v[3].stamped), (20_000, true));
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
