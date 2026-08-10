//! 笔记的跨轨时基映射(`align.json`)的读写。
//!
//! 存在的理由:受时钟漂移影响的笔记,mic 轨的时间戳整条是错的——回放侧把音频重采样
//! 回 system 时基后,转写段的 start_ms/end_ms 仍停在旧时基上,于是高亮跟不上、点段落
//! 跳错位置,连原始稿里 mic 行与 system 行的**先后次序**都是错的(mic 行最多早插
//! 148s)。映射估计一次要跑几秒,所以由回放侧算完落盘,读侧直接取用,两边共用同一条
//! 时基,不会各算各的算出两条。
//!
//! 只改读出来的值,`segments.jsonl` 原样不动:估计是启发式的,不该把它写死进单一
//! 真值源;删掉 align.json 即完全回到未纠正状态。

use crate::player_align::TimeMap;
use std::path::Path;

pub const ALIGN_FILE: &str = "align.json";
/// align.json 及关联产物(对齐音轨缓存/跳过标记)的**全写删方共用**串行锁:
/// 写入者不止回放侧(mix_regen 也直写),各持各的锁等于没锁——过期清理可在
/// regen 刚发布有效映射后把它删掉,映射却已烘进随后落盘的 mixed(Codex 十七轮)。
/// 约定:任何 align.json 的写/删,以及需要与之原子的产物操作,统一持本锁;
/// `write` 不自取锁(调用方可能要把它与相邻操作圈进同一临界区,嵌套自锁会死锁)。
pub static ALIGN_FS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// 对齐"估不出/不值得纠正"的负结果标记(空文件,mtime 即判据):没有它,大笔记每次
/// 装载都会重跑 60-100s 的时基估计。比两条源轨都新才有效,源轨更新(续录)即重估;
/// 删掉它即强制下次装载重估。
pub const ALIGN_SKIP_FILE: &str = "align.skip";

#[derive(serde::Serialize, serde::Deserialize)]
struct AlignDoc {
    schema_version: u32,
    /// 被纠正的轨(目前恒为 "mic";留着是为了将来 system 轨出问题时不必改格式)。
    track: String,
    map: TimeMap,
}

/// 读映射。文件缺失/损坏一律 None——调用方据此按未纠正处理。
pub fn read(note_dir: &Path) -> Option<TimeMap> {
    let bytes = std::fs::read(note_dir.join(ALIGN_FILE)).ok()?;
    let doc: AlignDoc = serde_json::from_slice(&bytes).ok()?;
    if doc.schema_version != 1 || doc.track != "mic" {
        return None;
    }
    Some(doc.map)
}

pub fn write(note_dir: &Path, map: &TimeMap) -> anyhow::Result<()> {
    use std::io::Write;
    let doc = AlignDoc { schema_version: 1, track: "mic".into(), map: map.clone() };
    let bytes = serde_json::to_vec_pretty(&doc)?;
    // 临时文件名唯一 + `create_new`:固定名 + `fs::write` 会**跟随**目录里预先放好的
    // 同名符号链接,把内容写到链接指向的任意可写文件上;create_new 遇到已存在的路径
    // (含符号链接本身)直接失败,不跟随。唯一后缀另外避免并发写互相踩。
    // (store 层其余几处原子写仍是旧的固定名写法,同样值得收口,但不在本次改动范围。)
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = note_dir.join(format!(
        "{ALIGN_FILE}.{}-{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    let write_then_rename = (|| -> std::io::Result<()> {
        f.write_all(&bytes)?;
        f.sync_all()?;
        drop(f);
        // std::fs::rename 在 Windows 走 MoveFileEx + MOVEFILE_REPLACE_EXISTING,
        // 覆盖已有目标是它的既定语义,无需另写平台分支。
        std::fs::rename(&tmp, note_dir.join(ALIGN_FILE))
    })();
    if write_then_rename.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(write_then_rename?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> TimeMap {
        TimeMap::new(vec![(0.0, 0.0), (60.0, 60.0), (600.0, 649.0)]).unwrap()
    }

    /// 临时文件不得跟随预先放好的同名符号链接:否则一份构造过的笔记目录能让对齐
    /// 把内容写到 notes 之外的任意可写文件上。唯一名 + create_new 双保险。
    #[cfg(unix)]
    #[test]
    fn temp_file_never_follows_a_planted_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("victim.txt");
        std::fs::write(&outside, b"must survive").unwrap();
        let note = tmp.path().join("note");
        std::fs::create_dir(&note).unwrap();
        // 把所有可预测的 .tmp 名都指向受害者文件
        for name in [format!("{ALIGN_FILE}.tmp"), format!("{ALIGN_FILE}.0.tmp")] {
            let _ = std::os::unix::fs::symlink(&outside, note.join(name));
        }
        write(&note, &map()).unwrap();
        assert_eq!(std::fs::read(&outside).unwrap(), b"must survive", "外部文件不得被改写");
        assert_eq!(read(&note), Some(map()), "映射本身正常落盘");
    }

    /// 重复写不得因临时名冲突而失败(唯一名要真的唯一)。
    #[test]
    fn repeated_writes_succeed() {
        let tmp = tempfile::tempdir().unwrap();
        for _ in 0..3 {
            write(tmp.path(), &map()).unwrap();
        }
        assert_eq!(read(tmp.path()), Some(map()));
    }

    #[test]
    fn round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read(tmp.path()).is_none(), "无文件即未纠正");
        write(tmp.path(), &map()).unwrap();
        assert_eq!(read(tmp.path()), Some(map()));
    }

    /// 合法 JSON 也可能编码出一个"不可用"的映射(空节点/单节点/非递增/NaN)。
    /// 这些都是读一个文件就能触发 apply() 越界或除零的形状,必须在解析边界挡掉。
    #[test]
    fn structurally_invalid_maps_are_rejected_at_parse_time() {
        let tmp = tempfile::tempdir().unwrap();
        for bad in [
            r#"{"schema_version":1,"track":"mic","map":{"knots":[]}}"#,
            r#"{"schema_version":1,"track":"mic","map":{"knots":[[0.0,0.0]]}}"#,
            // 非递增:相邻差为 0 → 插值时除零
            r#"{"schema_version":1,"track":"mic","map":{"knots":[[0.0,0.0],[0.0,1.0]]}}"#,
            // 时间倒流
            r#"{"schema_version":1,"track":"mic","map":{"knots":[[0.0,5.0],[10.0,1.0]]}}"#,
            r#"{"schema_version":1,"track":"mic","map":{"knots":[[0.0,0.0],[1e999,1.0]]}}"#,
        ] {
            std::fs::write(tmp.path().join(ALIGN_FILE), bad).unwrap();
            assert!(read(tmp.path()).is_none(), "应拒绝: {bad}");
        }
    }

    #[test]
    fn corrupt_or_foreign_file_degrades_to_none() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(ALIGN_FILE), b"{ not json").unwrap();
        assert!(read(tmp.path()).is_none());
        std::fs::write(
            tmp.path().join(ALIGN_FILE),
            br#"{"schema_version":99,"track":"mic","map":{"knots":[[0.0,0.0],[1.0,1.0]]}}"#,
        )
        .unwrap();
        assert!(read(tmp.path()).is_none(), "未来版本不猜,按未纠正处理");
    }
}
