//! 会议备注的命令壳与「按下即钉住」入口(快捷键/托盘共用)。
//!
//! 存储规则在 `store::memos`;这里只做三件事:定位笔记目录、按「录制中 / 会后」
//! 钳时间点、写成功后 emit `memos_changed`(整表)。**不调 `admit_note`**:备注与录制
//! writer、Aing、重转写不共享任何文件,录制中也能加改删(设计「定死的选择 2」)。

use crate::store::{self, memos};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

#[derive(Debug, Clone, Serialize)]
pub struct MemosChangedEvent {
    pub note_id: String,
    pub memos: Vec<memos::Memo>,
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

/// 时间点上限:录制中 = 当前录音时间(只能往前),会后 = 笔记时间轴终点。
fn max_at(app: &AppHandle, note_id: &str, dir: &std::path::Path) -> u64 {
    active_elapsed(app, note_id).unwrap_or_else(|| store::timeline_end_ms(dir))
}

fn emit_changed(app: &AppHandle, note_id: &str, list: memos::MemoList) {
    let _ = app.emit("memos_changed", MemosChangedEvent { note_id: note_id.to_string(), memos: list.memos });
}

#[tauri::command]
pub async fn note_memos(app: AppHandle, note_id: String) -> Result<Vec<memos::Memo>, String> {
    crate::off_main(move || {
        let dir = note_dir(&app, &note_id)?;
        Ok(memos::load_memos(&dir).memos)
    })
    .await
}

/// 录制页输入框第一次按键时调用:时间点一律由后端取,不信前端走表。
#[tauri::command]
pub fn memo_anchor_now(app: AppHandle, note_id: String) -> Result<u64, String> {
    active_elapsed(&app, &note_id).ok_or_else(|| crate::tr!("这篇笔记不在录制中", "This note is not recording"))
}

#[tauri::command]
pub async fn add_memo(
    app: AppHandle,
    note_id: String,
    at_ms: u64,
    text: String,
    origin: String,
    restore_id: Option<String>,
    restore_created_at: Option<String>,
) -> Result<memos::Memo, String> {
    crate::off_main(move || {
        let dir = note_dir(&app, &note_id)?;
        let at = memos::clamp_at(at_ms, max_at(&app, &note_id, &dir));
        let restore = restore_id.as_deref().map(|id| (id, restore_created_at.as_deref().unwrap_or("")));
        let (memo, list) = memos::add_memo(&dir, at, &text, &origin, restore).map_err(|e| e.to_string())?;
        emit_changed(&app, &note_id, list);
        Ok(memo)
    })
    .await
}

#[tauri::command]
pub async fn edit_memo(
    app: AppHandle,
    note_id: String,
    id: String,
    expected_updated_at: String,
    text: Option<String>,
    at_ms: Option<u64>,
) -> Result<memos::Memo, String> {
    crate::off_main(move || {
        let dir = note_dir(&app, &note_id)?;
        let at = at_ms.map(|a| memos::clamp_at(a, max_at(&app, &note_id, &dir)));
        let (memo, list) =
            memos::edit_memo(&dir, &id, &expected_updated_at, text.as_deref(), at).map_err(|e| e.to_string())?;
        emit_changed(&app, &note_id, list);
        Ok(memo)
    })
    .await
}

#[tauri::command]
pub async fn delete_memo(app: AppHandle, note_id: String, id: String) -> Result<memos::Memo, String> {
    crate::off_main(move || {
        let dir = note_dir(&app, &note_id)?;
        let (memo, list) = memos::delete_memo(&dir, &id).map_err(|e| e.to_string())?;
        emit_changed(&app, &note_id, list);
        Ok(memo)
    })
    .await
}

/// 快捷键 / 托盘共用:不在录音中什么都不做;否则在按下的这一刻立即写一条时间标记
/// (空文字备注)、emit,再弹浮窗让用户补文字。写盘放后台线程——调用方可能在主线程
/// (托盘菜单、快捷键回调),不能在那里做文件 IO 与锁等待。
pub fn mark_now(app: &AppHandle, origin: &'static str) {
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
        match memos::add_memo(&dir, at_ms, "", origin, None) {
            Ok((memo, list)) => {
                emit_changed(&app, &note_id, list);
                crate::memo_panel::show(&app, &note_id, &memo);
            }
            Err(e) => eprintln!("mark_now 写入失败: {e}"),
        }
    });
}
