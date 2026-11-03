//! 会议备注(memo):用户亲手写下的一句话 + 笔记时间轴上的一个毫秒点。
//!
//! 存在笔记目录里独立的 `memos.json`,**不进**原始稿、修订稿、meta——那几份文件各有
//! 会整份重写它们的人(重转写换 segments、重新 Aing 重建 aing.json、Aing 写 meta),
//! 而备注永远属于用户:谁都不改写它。唯一的「系统改备注」是续录时 [`clamp_after`]
//! 把超出上一场末尾的时间点钳回末尾(只改时间不改文字)。
//!
//! 结构照 `edits.rs`(音频删减)的先例:命令壳直调、不经 lifecycle actor、不碰
//! `.note.lock`——录制中、Aing 中、重转写中都能写。并发:进程内 `MEMO_LOCK` 串行
//! load-modify-save,另加跨进程文件锁 `.memos.lock`(MCP 是独立进程)。
//!
//! 坏档与 edits.json 不同:按空表读,但**写入前先把坏档改名留底**再写新表——
//! 备注是手写内容,丢了无法再生。
//!
//! 设计:docs/superpowers/specs/2026-11-03-meeting-memo-design.md

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

static MEMO_LOCK: Mutex<()> = Mutex::new(());
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub const MEMOS_FILE: &str = "memos.json";
pub const MEMOS_LOCK_FILE: &str = ".memos.lock";
/// 单条备注文字上限(按字符计)。前端先拦,后端兜底拒绝。
pub const MAX_TEXT_CHARS: usize = 2000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memo {
    pub id: String,
    pub at_ms: u64,
    /// 纯文本,可多行;空串 = 时间标记。
    #[serde(default)]
    pub text: String,
    /// `composer` / `hotkey` / `tray` / `note`。只用于诊断,不影响行为。
    #[serde(default)]
    pub origin: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoList {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub memos: Vec<Memo>,
}

enum Loaded {
    Missing,
    Ok(MemoList),
    Corrupt,
}

fn read_raw(note_dir: &Path) -> Loaded {
    let Ok(bytes) = std::fs::read(note_dir.join(MEMOS_FILE)) else {
        return Loaded::Missing;
    };
    match serde_json::from_slice::<MemoList>(&bytes) {
        Ok(l) => Loaded::Ok(l),
        Err(_) => Loaded::Corrupt,
    }
}

/// 读备注表(升序)。无文件/坏档都按空表——备注是增值层,不阻断笔记打开。
pub fn load_memos(note_dir: &Path) -> MemoList {
    match read_raw(note_dir) {
        Loaded::Ok(mut l) => {
            sort(&mut l.memos);
            l
        }
        _ => MemoList::default(),
    }
}

fn sort(memos: &mut [Memo]) {
    memos.sort_by(|a, b| a.at_ms.cmp(&b.at_ms).then_with(|| a.created_at.cmp(&b.created_at)));
}

fn save(note_dir: &Path, list: &MemoList) -> anyhow::Result<()> {
    let p = note_dir.join(MEMOS_FILE);
    let tmp = note_dir.join(format!(
        ".{MEMOS_FILE}.tmp-{}-{}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let bytes = serde_json::to_vec_pretty(list)?;
    if let Err(e) = std::fs::write(&tmp, &bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    if let Err(e) = std::fs::rename(&tmp, &p) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

/// 读-改-写的唯一入口:进程内锁 + 跨进程文件锁内重读,交给 `f` 修改后原子落盘。
/// 读到坏档时先把它改名为 `memos.json.corrupt-<时间>` 留底,再在空表上修改。
fn modify<T>(note_dir: &Path, f: impl FnOnce(&mut MemoList) -> anyhow::Result<T>) -> anyhow::Result<T> {
    if !note_dir.is_dir() {
        anyhow::bail!("笔记不存在");
    }
    let _g = MEMO_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _flock = super::notelock::NoteLock::acquire_opened(|| {
        std::fs::OpenOptions::new().create(true).write(true).open(note_dir.join(MEMOS_LOCK_FILE))
    })
    .map_err(|e| anyhow::anyhow!("备注锁不可用: {e}"))?
    .ok_or_else(|| anyhow::anyhow!("另一处正在修改这篇笔记的备注,请重试"))?;
    let mut list = match read_raw(note_dir) {
        Loaded::Ok(l) => l,
        Loaded::Missing => MemoList::default(),
        Loaded::Corrupt => {
            let stamp = chrono::Local::now().format("%Y%m%dT%H%M%S");
            let bak = note_dir.join(format!("{MEMOS_FILE}.corrupt-{stamp}"));
            std::fs::rename(note_dir.join(MEMOS_FILE), &bak)?;
            eprintln!("memos.json 解析失败,已改名留底: {}", bak.display());
            MemoList::default()
        }
    };
    list.schema_version = 1;
    let out = f(&mut list)?;
    sort(&mut list.memos);
    save(note_dir, &list)?;
    Ok(out)
}

fn now_rfc3339() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false)
}

fn new_id() -> String {
    let u = uuid::Uuid::new_v4().simple().to_string();
    format!("m_{}_{}", chrono::Local::now().format("%Y%m%dT%H%M%S"), &u[..6])
}

fn check_text(text: &str) -> anyhow::Result<()> {
    if text.chars().count() > MAX_TEXT_CHARS {
        anyhow::bail!("备注太长(上限 {MAX_TEXT_CHARS} 字)");
    }
    Ok(())
}

/// 备注 id 只允许 `[A-Za-z0-9_-]`:撤销删除时由前端带回,不信任外来字符串。
fn check_id(id: &str) -> anyhow::Result<()> {
    if id.is_empty() || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
        anyhow::bail!("备注 id 不合法");
    }
    Ok(())
}

/// 新增。`id` 只在「撤销删除」时带回原 id(连同原 created_at),已存在同 id 报错。
/// `at_ms` 由调用方先钳到合法范围(见 [`clamp_at`])。返回新备注与整表。
pub fn add_memo(
    note_dir: &Path,
    at_ms: u64,
    text: &str,
    origin: &str,
    restore: Option<(&str, &str)>,
) -> anyhow::Result<(Memo, MemoList)> {
    check_text(text)?;
    let now = now_rfc3339();
    let (id, created_at) = match restore {
        Some((id, created)) => {
            check_id(id)?;
            (id.to_string(), if created.is_empty() { now.clone() } else { created.to_string() })
        }
        None => (new_id(), now.clone()),
    };
    let memo = Memo { id, at_ms, text: text.to_string(), origin: origin.to_string(), created_at, updated_at: now };
    let m = memo.clone();
    let list = modify(note_dir, move |l| {
        if l.memos.iter().any(|x| x.id == m.id) {
            anyhow::bail!("备注已存在");
        }
        l.memos.push(m);
        Ok(l.clone())
    })?;
    let mut list = list;
    sort(&mut list.memos);
    Ok((memo, list))
}

/// 改文字和/或时间点。`expected_updated_at` 做乐观校验:与盘上不一致说明别处改过。
pub fn edit_memo(
    note_dir: &Path,
    id: &str,
    expected_updated_at: &str,
    text: Option<&str>,
    at_ms: Option<u64>,
) -> anyhow::Result<(Memo, MemoList)> {
    if let Some(t) = text {
        check_text(t)?;
    }
    let mut list = modify(note_dir, |l| {
        let m = l.memos.iter_mut().find(|m| m.id == id).ok_or_else(|| anyhow::anyhow!("备注不存在(可能已被删除,请刷新)"))?;
        if m.updated_at != expected_updated_at {
            anyhow::bail!("备注已被修改,请刷新");
        }
        if let Some(t) = text {
            m.text = t.to_string();
        }
        if let Some(a) = at_ms {
            m.at_ms = a;
        }
        m.updated_at = now_rfc3339();
        Ok(l.clone())
    })?;
    sort(&mut list.memos);
    let memo = list.memos.iter().find(|m| m.id == id).cloned().expect("刚改过的备注必在表内");
    Ok((memo, list))
}

/// 删除,返回被删的那条(供撤销)与整表。
pub fn delete_memo(note_dir: &Path, id: &str) -> anyhow::Result<(Memo, MemoList)> {
    let mut list_out = None;
    let removed = modify(note_dir, |l| {
        let i = l.memos.iter().position(|m| m.id == id).ok_or_else(|| anyhow::anyhow!("备注不存在(可能已被删除,请刷新)"))?;
        let m = l.memos.remove(i);
        list_out = Some(l.clone());
        Ok(m)
    })?;
    let mut list = list_out.unwrap_or_default();
    sort(&mut list.memos);
    Ok((removed, list))
}

/// 续录专用:把 `at_ms > base_ms` 的备注钳到 `base_ms`(上一场末尾之后的静音被切掉了,
/// 不钳就会落进第二场开头的内容里)。只动时间,不动文字与 updated_at 之外的字段。
/// 没有要钳的就不写盘。返回被钳的条数。
pub fn clamp_after(note_dir: &Path, base_ms: u64) -> anyhow::Result<usize> {
    if !load_memos(note_dir).memos.iter().any(|m| m.at_ms > base_ms) {
        return Ok(0);
    }
    modify(note_dir, |l| {
        let mut n = 0;
        let now = now_rfc3339();
        for m in l.memos.iter_mut().filter(|m| m.at_ms > base_ms) {
            m.at_ms = base_ms;
            m.updated_at = now.clone();
            n += 1;
        }
        Ok(n)
    })
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

/// `mm:ss`(满一小时 `h:mm:ss`)。导出、Aing 提示共用。
pub fn fmt_clock(ms: u64) -> String {
    let s = ms / 1000;
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m:02}:{s:02}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_sorts_by_time_and_persists() {
        let d = tempfile::tempdir().unwrap();
        add_memo(d.path(), 5_000, "后", "composer", None).unwrap();
        let (_, list) = add_memo(d.path(), 1_000, "前", "note", None).unwrap();
        let texts: Vec<_> = list.memos.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["前", "后"]);
        assert_eq!(load_memos(d.path()).memos, list.memos);
    }

    #[test]
    fn missing_dir_is_note_not_found() {
        let d = tempfile::tempdir().unwrap();
        let err = add_memo(&d.path().join("gone"), 0, "x", "note", None).unwrap_err();
        assert!(err.to_string().contains("笔记不存在"));
    }

    #[test]
    fn edit_checks_expected_updated_at() {
        let d = tempfile::tempdir().unwrap();
        let (m, _) = add_memo(d.path(), 1_000, "a", "composer", None).unwrap();
        let err = edit_memo(d.path(), &m.id, "stale", Some("b"), None).unwrap_err();
        assert!(err.to_string().contains("已被修改"));
        let (m2, _) = edit_memo(d.path(), &m.id, &m.updated_at, Some("b"), Some(500)).unwrap();
        assert_eq!((m2.text.as_str(), m2.at_ms), ("b", 500));
        assert_eq!(m2.created_at, m.created_at);
    }

    #[test]
    fn delete_then_restore_with_same_id() {
        let d = tempfile::tempdir().unwrap();
        let (m, _) = add_memo(d.path(), 1_000, "a", "composer", None).unwrap();
        let (gone, list) = delete_memo(d.path(), &m.id).unwrap();
        assert!(list.memos.is_empty());
        let (back, _) = add_memo(d.path(), gone.at_ms, &gone.text, &gone.origin, Some((&gone.id, &gone.created_at))).unwrap();
        assert_eq!((back.id.as_str(), back.created_at.as_str()), (m.id.as_str(), m.created_at.as_str()));
        let err = add_memo(d.path(), 0, "", "note", Some((&m.id, ""))).unwrap_err();
        assert!(err.to_string().contains("已存在"));
        assert!(delete_memo(d.path(), "nope").is_err());
    }

    #[test]
    fn rejects_long_text_and_bad_restore_id() {
        let d = tempfile::tempdir().unwrap();
        let long = "字".repeat(MAX_TEXT_CHARS + 1);
        assert!(add_memo(d.path(), 0, &long, "note", None).is_err());
        assert!(add_memo(d.path(), 0, &"字".repeat(MAX_TEXT_CHARS), "note", None).is_ok());
        assert!(add_memo(d.path(), 0, "x", "note", Some(("../evil", ""))).is_err());
    }

    #[test]
    fn corrupt_file_is_kept_aside_before_rewrite() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(MEMOS_FILE), b"{not json").unwrap();
        assert!(load_memos(d.path()).memos.is_empty());
        add_memo(d.path(), 0, "新", "note", None).unwrap();
        let baks: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("memos.json.corrupt-"))
            .collect();
        assert_eq!(baks.len(), 1);
        assert_eq!(std::fs::read(baks[0].path()).unwrap(), b"{not json");
        assert_eq!(load_memos(d.path()).memos.len(), 1);
    }

    #[test]
    fn clamp_after_only_moves_later_memos_and_keeps_text() {
        let d = tempfile::tempdir().unwrap();
        add_memo(d.path(), 1_000, "早", "composer", None).unwrap();
        add_memo(d.path(), 600_000, "晚", "hotkey", None).unwrap();
        assert_eq!(clamp_after(d.path(), 570_000).unwrap(), 1);
        let l = load_memos(d.path());
        assert_eq!(l.memos.iter().map(|m| (m.at_ms, m.text.as_str())).collect::<Vec<_>>(), [(1_000, "早"), (570_000, "晚")]);
        // 无可钳则不写盘
        let before = std::fs::metadata(d.path().join(MEMOS_FILE)).unwrap().modified().unwrap();
        assert_eq!(clamp_after(d.path(), 570_000).unwrap(), 0);
        assert_eq!(std::fs::metadata(d.path().join(MEMOS_FILE)).unwrap().modified().unwrap(), before);
    }

    #[test]
    fn clamp_and_cut_helpers() {
        assert_eq!(clamp_at(5_000, 3_000), 3_000);
        assert_eq!(clamp_at(1_000, 3_000), 1_000);
        let cuts = [super::super::edits::CutRange { start_ms: 1_000, end_ms: 2_000 }];
        assert!(in_cut(1_000, &cuts));
        assert!(!in_cut(2_000, &cuts));
        assert_eq!(fmt_clock(754_000), "12:34");
        assert_eq!(fmt_clock(3_754_000), "1:02:34");
    }
}
