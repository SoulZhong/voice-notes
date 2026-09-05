//! 音频剪辑表(非破坏性):被"删减"的时间区间记在笔记目录的 edits.json 里,
//! 原始音频与逐字稿一个字节不动——播放跳过、导出剔除、逐字稿灰显,随时可恢复。
//! 这是本仓库"绝不毁用户录音"宪法(transcode.rs 顶注)在编辑功能上的延伸:
//! 任何"删除"都只是标记,恢复 = 从表里移除该区间。
//!
//! 区间为笔记时间轴毫秒的半开区间 [start_ms, end_ms),与圈选游标/导出范围同口径。
//! 表内恒序、恒不重叠:add 时钳位、吞并所有重叠/相邻区间;remove 按精确端点匹配
//! (前端恢复按钮拿到的就是表里的原值,不存在"差一毫秒找不到"的问题)。

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

/// 进程内写序列化(Codex 审出:add/remove 是 load→modify→save,并发会互相覆盖丢删减)。
/// 全程持锁跨 load-modify-save;跨进程场景本表只有 GUI 单写者,不另做文件锁。
static WRITE_LOCK: Mutex<()> = Mutex::new(());
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub const EDITS_FILE: &str = "edits.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CutRange {
    pub start_ms: u64,
    pub end_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EditList {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub cuts: Vec<CutRange>,
}

/// 读剪辑表:无文件/坏文件都按空表(编辑是增值层,坏档不阻断笔记打开;
/// 坏档会在下一次写入时被完整新表原子覆盖)。
pub fn load_edits(note_dir: &Path) -> EditList {
    let p = note_dir.join(EDITS_FILE);
    let Ok(bytes) = std::fs::read(&p) else {
        return EditList::default();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

/// 原子写(tmp+rename,与笔记其余落盘同纪律;tmp 名带 pid+序号防两次写争同一文件)。
fn save_edits(note_dir: &Path, list: &EditList) -> anyhow::Result<()> {
    let p = note_dir.join(EDITS_FILE);
    let tmp = note_dir.join(format!(
        ".{EDITS_FILE}.tmp-{}-{}",
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

/// 新增删减区间:钳到 [0, total_ms]、拒绝空区间,吞并一切重叠或首尾相接的既有
/// 区间(合并后表恒序不重叠)。返回更新后的整表。
pub fn add_cut(note_dir: &Path, start_ms: u64, end_ms: u64, total_ms: u64) -> anyhow::Result<EditList> {
    let (s, e) = (start_ms.min(total_ms), end_ms.min(total_ms));
    if e <= s {
        anyhow::bail!("删减区间为空");
    }
    let _g = WRITE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut list = load_edits(note_dir);
    list.schema_version = 1;
    let (mut s, mut e) = (s, e);
    list.cuts.retain(|c| {
        // 重叠或相邻(半开区间下 end==start 即相接)则吞并进新区间
        if c.end_ms >= s && c.start_ms <= e {
            s = s.min(c.start_ms);
            e = e.max(c.end_ms);
            false
        } else {
            true
        }
    });
    list.cuts.push(CutRange { start_ms: s, end_ms: e });
    list.cuts.sort_by_key(|c| c.start_ms);
    save_edits(note_dir, &list)?;
    Ok(list)
}

/// 恢复一段删减:按精确端点匹配移除。找不到报错(说明前端拿的是过期表,
/// 静默成功会让用户以为恢复了)。返回更新后的整表。
pub fn remove_cut(note_dir: &Path, start_ms: u64, end_ms: u64) -> anyhow::Result<EditList> {
    let _g = WRITE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut list = load_edits(note_dir);
    let before = list.cuts.len();
    list.cuts.retain(|c| !(c.start_ms == start_ms && c.end_ms == end_ms));
    if list.cuts.len() == before {
        anyhow::bail!("没有这段删减记录(可能已被恢复,请刷新)");
    }
    save_edits(note_dir, &list)?;
    Ok(list)
}

/// 从"保留区间"减去删减表:输入 keep 与 cuts 同为半开毫秒区间,输出有序不重叠的
/// 保留区间列表。导出音频拼接与导出文本过滤共用这一份几何,防两处口径漂移。
pub fn subtract_cuts(keep: (u64, u64), cuts: &[CutRange]) -> Vec<(u64, u64)> {
    let (mut pos, end) = keep;
    if end <= pos {
        return Vec::new();
    }
    let mut sorted: Vec<&CutRange> = cuts.iter().filter(|c| c.end_ms > pos && c.start_ms < end).collect();
    sorted.sort_by_key(|c| c.start_ms);
    let mut out = Vec::new();
    for c in sorted {
        if c.start_ms > pos {
            out.push((pos, c.start_ms.min(end)));
        }
        pos = pos.max(c.end_ms);
        if pos >= end {
            return out;
        }
    }
    if pos < end {
        out.push((pos, end));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_merges_overlapping_and_adjacent_cuts() {
        let dir = tempfile::tempdir().unwrap();
        add_cut(dir.path(), 1_000, 2_000, 100_000).unwrap();
        add_cut(dir.path(), 5_000, 6_000, 100_000).unwrap();
        // 与第一段相接(2000)且与第二段重叠(5500):三合一
        let list = add_cut(dir.path(), 2_000, 5_500, 100_000).unwrap();
        assert_eq!(list.cuts, vec![CutRange { start_ms: 1_000, end_ms: 6_000 }]);
        // 落盘可回读
        assert_eq!(load_edits(dir.path()).cuts, list.cuts);
    }

    #[test]
    fn add_clamps_to_total_and_rejects_empty() {
        let dir = tempfile::tempdir().unwrap();
        let list = add_cut(dir.path(), 90_000, 200_000, 100_000).unwrap();
        assert_eq!(list.cuts, vec![CutRange { start_ms: 90_000, end_ms: 100_000 }]);
        assert!(add_cut(dir.path(), 3_000, 3_000, 100_000).is_err(), "空区间");
        assert!(add_cut(dir.path(), 100_000, 200_000, 100_000).is_err(), "整段越界钳成空");
    }

    #[test]
    fn remove_needs_exact_match() {
        let dir = tempfile::tempdir().unwrap();
        add_cut(dir.path(), 1_000, 2_000, 100_000).unwrap();
        assert!(remove_cut(dir.path(), 1_000, 2_001).is_err(), "端点不符不删");
        let list = remove_cut(dir.path(), 1_000, 2_000).unwrap();
        assert!(list.cuts.is_empty());
    }

    #[test]
    fn corrupt_file_degrades_to_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(EDITS_FILE), b"{oops").unwrap();
        assert!(load_edits(dir.path()).cuts.is_empty());
    }

    #[test]
    fn subtract_cuts_splits_keep_interval() {
        let cuts = vec![
            CutRange { start_ms: 2_000, end_ms: 3_000 },
            CutRange { start_ms: 5_000, end_ms: 6_000 },
        ];
        assert_eq!(subtract_cuts((0, 10_000), &cuts), vec![(0, 2_000), (3_000, 5_000), (6_000, 10_000)]);
        // keep 完全落在删减内 → 空
        assert_eq!(subtract_cuts((2_100, 2_900), &cuts), Vec::<(u64, u64)>::new());
        // 边贴边:半开区间语义
        assert_eq!(subtract_cuts((3_000, 5_000), &cuts), vec![(3_000, 5_000)]);
        // 删减越过 keep 两端
        assert_eq!(subtract_cuts((2_500, 5_500), &cuts), vec![(3_000, 5_000)]);
    }
}
