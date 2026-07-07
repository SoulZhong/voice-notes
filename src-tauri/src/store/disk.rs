//! 录音音频磁盘统计与按时间清理(纯文件逻辑,供 lib.rs 两命令消费)。
//! 只清"音频"这一增值层的字节(`*.m4a`/`*.wav`/`*.m4a.bad`),从不碰 meta.json/
//! segments.jsonl/speakers.json——清理后笔记仍可正常打开、只是没有音频回放。

use crate::store::{audio, NoteMeta};
use std::path::Path;

/// 音频文件判定:文件名以这三种后缀结尾。`.m4a.bad` 是转码失败重命名保留的原始产物
/// (见 transcode.rs),同样占磁盘、同样该被计入/清理。
fn is_audio_file(name: &str) -> bool {
    name.ends_with(".m4a") || name.ends_with(".wav") || name.ends_with(".m4a.bad")
}

/// 统计 notes_root 下所有笔记目录的音频文件总字节数(一层遍历,不递归子目录)。
/// 单个笔记/文件读取失败(权限、竞态删除等)只跳过,不让整体统计因一个坏笔记而失败——
/// 磁盘用量是展示用的增值信息,不值得为它抛错打断设置页。
pub fn audio_usage_bytes(notes_root: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(notes_root) else {
        return 0;
    };
    let mut total = 0u64;
    for note_entry in rd.flatten() {
        let note_dir = note_entry.path();
        if !note_dir.is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&note_dir) else {
            continue;
        };
        for f in files.flatten() {
            let name = f.file_name();
            let name = name.to_string_lossy();
            if !is_audio_file(&name) {
                continue;
            }
            if let Ok(md) = f.metadata() {
                total += md.len();
            }
        }
    }
    total
}

/// 该笔记是否满足"可清理音频"的条件:meta.json 可解析且 state=="complete"
///(录制中的笔记音频还在写,绝不能碰),且(未指定 cutoff,或该笔记早于 cutoff)。
/// 时间比较用字符串序:ended_at(缺失/空则回退 started_at)与 cutoff 都是本仓统一生成的
/// RFC3339 本地时区字符串——同时区、同格式的 RFC3339 按字符串序等价于按时间序,
/// 这是本函数不解析时间、直接字符串比较的前提(若未来引入跨时区/UTC 混用的 meta,
/// 这个前提就不成立,需要改回解析比较)。
pub fn should_purge(note_dir: &Path, cutoff_rfc3339: Option<&str>) -> bool {
    let Ok(meta_str) = std::fs::read_to_string(note_dir.join("meta.json")) else {
        return false;
    };
    let Ok(meta) = serde_json::from_str::<NoteMeta>(&meta_str) else {
        return false;
    };
    if meta.state != "complete" {
        return false;
    }
    let Some(cutoff) = cutoff_rfc3339 else {
        return true;
    };
    let ts = match meta.ended_at.as_deref() {
        Some(s) if !s.is_empty() => s,
        _ => meta.started_at.as_str(),
    };
    ts < cutoff
}

/// 删除该笔记目录下的音频文件(m4a/wav/m4a.bad),并清掉 audio.json 里对应 track 的
/// codec/duration_ms(回落到"无压缩产物"的记录形状,offset_ms 保留——它是时间轴对齐
/// 信息,和音频是否还在磁盘上无关)。返回实际删除的字节数。
/// 单个文件删除失败(权限/竞态)只 eprintln continue,不让一个坏文件挡住其它文件与
/// 其它笔记的清理——这是增值层的清理操作,不是必须原子成功的事务。
pub fn purge_note_audio(note_dir: &Path) -> u64 {
    let mut freed = 0u64;
    if let Ok(rd) = std::fs::read_dir(note_dir) {
        for f in rd.flatten() {
            let name = f.file_name();
            let name = name.to_string_lossy().into_owned();
            if !is_audio_file(&name) {
                continue;
            }
            let path = f.path();
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            match std::fs::remove_file(&path) {
                Ok(()) => freed += len,
                Err(e) => eprintln!("清理音频文件失败,跳过({}): {e}", path.display()),
            }
        }
    }
    let meta = audio::load_audio_meta(note_dir);
    for source in meta.tracks.keys() {
        if let Err(e) = audio::clear_track_compressed(note_dir, source) {
            eprintln!("清理 audio.json 压缩记录失败,跳过({source}): {e}");
        }
    }
    freed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_meta(note_dir: &Path, state: &str, started_at: &str, ended_at: Option<&str>) {
        let meta = NoteMeta {
            schema_version: 1,
            id: "n".into(),
            title: "t".into(),
            started_at: started_at.into(),
            ended_at: ended_at.map(|s| s.to_string()),
            state: state.into(),
        };
        std::fs::write(note_dir.join("meta.json"), serde_json::to_string(&meta).unwrap()).unwrap();
    }

    #[test]
    fn usage_sums_audio_files_only_across_notes() {
        let tmp = tempfile::tempdir().unwrap();
        let n1 = tmp.path().join("n1");
        let n2 = tmp.path().join("n2");
        std::fs::create_dir_all(&n1).unwrap();
        std::fs::create_dir_all(&n2).unwrap();
        std::fs::write(n1.join("mic.wav"), vec![0u8; 100]).unwrap();
        std::fs::write(n1.join("system.m4a"), vec![0u8; 50]).unwrap();
        std::fs::write(n1.join("mic.m4a.bad"), vec![0u8; 10]).unwrap();
        std::fs::write(n1.join("meta.json"), b"{}").unwrap(); // 非音频文件不计入
        std::fs::write(n2.join("mic.wav"), vec![0u8; 20]).unwrap();
        assert_eq!(audio_usage_bytes(tmp.path()), 180);
    }

    #[test]
    fn usage_ignores_non_directory_entries_and_missing_root() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("stray.wav"), vec![0u8; 999]).unwrap(); // 不是笔记目录
        assert_eq!(audio_usage_bytes(tmp.path()), 0);
        assert_eq!(audio_usage_bytes(&tmp.path().join("nonexistent")), 0);
    }

    #[test]
    fn should_purge_true_when_complete_and_past_cutoff() {
        let tmp = tempfile::tempdir().unwrap();
        write_meta(tmp.path(), "complete", "2026-01-01T00:00:00+08:00", None);
        assert!(should_purge(tmp.path(), Some("2026-06-01T00:00:00+08:00")));
    }

    #[test]
    fn should_purge_false_when_recording() {
        let tmp = tempfile::tempdir().unwrap();
        write_meta(tmp.path(), "recording", "2026-01-01T00:00:00+08:00", None);
        assert!(!should_purge(tmp.path(), Some("2026-06-01T00:00:00+08:00")), "录制中绝不清理");
        assert!(!should_purge(tmp.path(), None), "即使无 cutoff 也不清理录制中的笔记");
    }

    #[test]
    fn should_purge_false_when_not_past_cutoff() {
        let tmp = tempfile::tempdir().unwrap();
        write_meta(tmp.path(), "complete", "2026-06-15T00:00:00+08:00", None);
        assert!(!should_purge(tmp.path(), Some("2026-06-01T00:00:00+08:00")), "未过期不清理");
    }

    #[test]
    fn should_purge_false_when_meta_corrupt_or_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!should_purge(tmp.path(), None), "meta.json 缺失");
        std::fs::write(tmp.path().join("meta.json"), "not json {{").unwrap();
        assert!(!should_purge(tmp.path(), None), "meta.json 损坏");
    }

    #[test]
    fn should_purge_true_when_no_cutoff_and_complete() {
        let tmp = tempfile::tempdir().unwrap();
        write_meta(tmp.path(), "complete", "2026-01-01T00:00:00+08:00", None);
        assert!(should_purge(tmp.path(), None), "无 cutoff 即清理所有已完成笔记");
    }

    #[test]
    fn should_purge_uses_ended_at_falling_back_to_started_at() {
        let tmp = tempfile::tempdir().unwrap();
        // ended_at 在 cutoff 之后(晚于) → 不清理,即使 started_at 更早。
        write_meta(
            tmp.path(),
            "complete",
            "2026-01-01T00:00:00+08:00",
            Some("2026-07-01T00:00:00+08:00"),
        );
        assert!(!should_purge(tmp.path(), Some("2026-06-01T00:00:00+08:00")), "以 ended_at 为准");

        // ended_at 为空串 → 回退 started_at。
        let tmp2 = tempfile::tempdir().unwrap();
        write_meta(tmp2.path(), "complete", "2026-01-01T00:00:00+08:00", Some(""));
        assert!(should_purge(tmp2.path(), Some("2026-06-01T00:00:00+08:00")), "空 ended_at 回退 started_at");
    }

    #[test]
    fn purge_deletes_audio_files_keeps_meta_and_clears_compression_record() {
        let tmp = tempfile::tempdir().unwrap();
        write_meta(tmp.path(), "complete", "2026-01-01T00:00:00+08:00", None);
        std::fs::write(tmp.path().join("segments.jsonl"), "{}\n").unwrap();
        std::fs::write(tmp.path().join("mic.wav"), vec![0u8; 30]).unwrap();
        std::fs::write(tmp.path().join("system.m4a"), vec![0u8; 40]).unwrap();
        std::fs::write(tmp.path().join("mic.m4a.bad"), vec![0u8; 5]).unwrap();
        audio::set_track_compressed(tmp.path(), "system", 1234, None).unwrap();

        let freed = purge_note_audio(tmp.path());
        assert_eq!(freed, 75, "wav+m4a+m4a.bad 三类都计入释放字节"); // 30+40+5=75

        assert!(!tmp.path().join("mic.wav").exists());
        assert!(!tmp.path().join("system.m4a").exists());
        assert!(!tmp.path().join("mic.m4a.bad").exists());
        assert!(tmp.path().join("meta.json").exists(), "meta.json 完好");
        assert!(tmp.path().join("segments.jsonl").exists(), "segments.jsonl 完好");

        let meta = audio::load_audio_meta(tmp.path());
        assert!(meta.tracks["system"].codec.is_none(), "codec 记录被清除");
        assert!(meta.tracks["system"].duration_ms.is_none(), "duration 记录被清除");
    }

    #[test]
    fn purge_tolerates_missing_files_and_no_audio_json() {
        let tmp = tempfile::tempdir().unwrap();
        write_meta(tmp.path(), "complete", "2026-01-01T00:00:00+08:00", None);
        // 无任何音频文件、无 audio.json:应安全返回 0,不 panic。
        assert_eq!(purge_note_audio(tmp.path()), 0);
    }
}
