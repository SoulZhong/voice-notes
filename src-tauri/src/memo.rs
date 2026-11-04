//! 会议备注的命令壳与「按下即钉住」入口(快捷键/托盘共用)。
//!
//! 存储规则在 `store::memos`(整篇 `memo.md`);这里只做三件事:定位笔记目录、按「录制中 /
//! 会后」钳时间点、写成功后 emit `memos_changed`(行视图 + 整篇原文)。**不调 `admit_note`**:备注与录制
//! writer、Aing、重转写不共享任何文件,录制中也能加改删(设计「定死的选择 2」)。

use crate::store::{self, memos};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

#[derive(Debug, Clone, Serialize)]
pub struct MemosChangedEvent {
    pub note_id: String,
    /// 行视图(时间轴小点、浮窗等用)。
    pub memos: Vec<memos::Memo>,
    /// 整篇原文 + 版本号(编辑器合并用)。
    pub doc: memos::MemoDoc,
}

fn note_dir(app: &AppHandle, note_id: &str) -> Result<std::path::PathBuf, String> {
    store::validate_note_id(note_id).map_err(|e| e.to_string())?;
    let dir = crate::notes_dir(app).map_err(|e| e.to_string())?.join(note_id);
    if !dir.is_dir() {
        return Err(crate::tr!("笔记不存在", "Note not found"));
    }
    Ok(dir)
}

/// 本笔记是活动录制会话时返回当前录音时间(扣暂停、含续录基线);否则 None。
/// statement-scoped 取锁,不与任何别的锁嵌套。
pub(crate) fn active_elapsed(app: &AppHandle, note_id: &str) -> Option<u64> {
    let st = app.state::<crate::AppState>();
    let g = st.session.lock().unwrap();
    g.as_ref().filter(|s| s.note_id == note_id).map(|s| s.elapsed_ms())
}

/// 活动录制会话的 (note_id, elapsed_ms)。
pub(crate) fn active_now(app: &AppHandle) -> Option<(String, u64)> {
    let st = app.state::<crate::AppState>();
    let g = st.session.lock().unwrap();
    g.as_ref().map(|s| (s.note_id.clone(), s.elapsed_ms()))
}

pub(crate) fn emit_changed(app: &AppHandle, note_id: &str, doc: memos::MemoDoc) {
    let _ = app.emit(
        "memos_changed",
        MemosChangedEvent { note_id: note_id.to_string(), memos: memos::parse_lines(&doc.text), doc },
    );
}

/// 行视图(带时间点的行,时间升序)。
#[tauri::command]
pub async fn note_memos(app: AppHandle, note_id: String) -> Result<Vec<memos::Memo>, String> {
    crate::off_main(move || {
        let dir = note_dir(&app, &note_id)?;
        Ok(memos::load_memos(&dir).memos)
    })
    .await
}

/// 整篇备注(编辑器打开时取)。
#[tauri::command]
pub async fn memo_doc(app: AppHandle, note_id: String) -> Result<memos::MemoDoc, String> {
    crate::off_main(move || {
        let dir = note_dir(&app, &note_id)?;
        Ok(memos::load_doc(&dir))
    })
    .await
}

/// 编辑器保存的结果:成功带新文档;冲突带盘上现状(前端合并后重存),不当错误抛。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SaveMemoDocResult {
    Saved { doc: memos::MemoDoc },
    Conflict { doc: memos::MemoDoc },
}

/// 编辑器整篇保存。`base_rev` 是编辑器上次取到/存下的版本;与盘上不一致返回 Conflict。
/// 原样落盘、不改写内容:曾在这里把超前的时间点钳回录音时间,但「落盘的不是提交的那份」会让
/// 编辑器把时间变化和文字变化当成同一行的冲突、合并出重复段(Codex 审出)。前端走表最多超前
/// 几百毫秒,不值得为它改写用户的文档。
#[tauri::command]
pub async fn save_memo_doc(
    app: AppHandle,
    note_id: String,
    text: String,
    base_rev: String,
) -> Result<SaveMemoDocResult, String> {
    crate::off_main(move || {
        let dir = note_dir(&app, &note_id)?;
        match memos::save_doc(&dir, &text, &base_rev) {
            Ok(doc) => {
                emit_changed(&app, &note_id, doc.clone());
                Ok(SaveMemoDocResult::Saved { doc })
            }
            Err(memos::SaveError::Conflict(doc)) => Ok(SaveMemoDocResult::Conflict { doc }),
            Err(e) => Err(e.to_string()),
        }
    })
    .await
}

/// 录制页编辑器取「此刻」的录音时间(后端为准)。
#[tauri::command]
pub fn memo_anchor_now(app: AppHandle, note_id: String) -> Result<u64, String> {
    active_elapsed(&app, &note_id).ok_or_else(|| crate::tr!("这篇笔记不在录制中", "This note is not recording"))
}

/// 浮窗补字:改「时间点 + 原文字」对得上的那一段;找不到就追加一段(不丢字)。
#[tauri::command]
pub async fn edit_memo_line(
    app: AppHandle,
    note_id: String,
    at_ms: u64,
    old_text: String,
    text: String,
) -> Result<(), String> {
    crate::off_main(move || {
        let dir = note_dir(&app, &note_id)?;
        let doc = memos::edit_line(&dir, at_ms, &old_text, &text).map_err(|e| e.to_string())?;
        emit_changed(&app, &note_id, doc);
        Ok(())
    })
    .await
}

/// 快捷键 / 托盘共用:不在录音中什么都不做;否则在按下的这一刻立即在文档末尾追加一行
/// 时间标记(`📍`)、emit,再弹浮窗让用户补文字。写盘放后台线程——调用方可能在主线程
/// (托盘菜单、快捷键回调),不能在那里做文件 IO 与锁等待。
pub fn mark_now(app: &AppHandle, _origin: &'static str) {
    let Some((note_id, at_ms)) = active_now(app) else {
        return;
    };
    let app = app.clone();
    std::thread::spawn(move || {
        let dir = match note_dir(&app, &note_id) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("mark_now: {e}");
                return;
            }
        };
        match memos::append(&dir, at_ms, "") {
            Ok((memo, doc)) => {
                emit_changed(&app, &note_id, doc);
                crate::memo_panel::show(&app, &note_id, &memo);
            }
            Err(e) => eprintln!("mark_now 写入失败: {e}"),
        }
    });
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoHit {
    pub note_id: String,
    pub memo_id: String,
    pub at_ms: u64,
    pub text: String,
}

/// 侧栏搜索用:扫全部笔记的备注,文字含 `query`(大小写不敏感)的行。只读 memo.md,
/// 不 load 整篇笔记;个人量级(百场 × 几十条)一次扫毫秒级,不建索引。
#[tauri::command]
pub async fn search_memos(app: AppHandle, query: String) -> Result<Vec<MemoHit>, String> {
    crate::off_main(move || {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        let root = crate::notes_dir(&app).map_err(|e| e.to_string())?;
        Ok(search_dir(&root, &needle, 200))
    })
    .await
}

fn search_dir(root: &std::path::Path, needle: &str, cap: usize) -> Vec<MemoHit> {
    let mut hits = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else { return hits };
    for e in rd.flatten() {
        if !e.path().join(memos::MEMO_DOC_FILE).is_file() && !e.path().join(memos::MEMOS_FILE).is_file() {
            continue;
        }
        let note_id = e.file_name().to_string_lossy().into_owned();
        for m in memos::load_memos(&e.path()).memos {
            if m.text.to_lowercase().contains(needle) {
                hits.push(MemoHit { note_id: note_id.clone(), memo_id: m.id, at_ms: m.at_ms, text: m.text });
                if hits.len() >= cap {
                    return hits;
                }
            }
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_dir_finds_memos_case_insensitively() {
        let d = tempfile::tempdir().unwrap();
        for id in ["a", "b"] {
            std::fs::create_dir(d.path().join(id)).unwrap();
        }
        memos::append(&d.path().join("a"), 1_000, "Ship 封版").unwrap();
        memos::append(&d.path().join("b"), 2_000, "别的").unwrap();
        let hits = search_dir(d.path(), "ship", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].note_id.as_str(), hits[0].at_ms), ("a", 1_000));
    }

}
