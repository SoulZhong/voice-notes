//! 历史同源双路场次批量折叠(devtools,issue #169):对全部 scene 终判为 dual_path 的
//! 笔记调用与停录自动折叠**同一实现**(NoteStore::fold_dual_path_echo)——可逆抑制,
//! 笔记页「展开全部」可找回;已有修订稿的场随之标 stale,走既有「重新 Aing」提示。
//! 幂等:已折叠过的场再跑为 0。
//!
//! 用法:scene_fold <notes_dir> [--dry-run] [--since YYYY-MM-DD]
//! 与应用并行运行安全:每篇经同一把 .note.lock;正在录制/占用的篇会报忙跳过。

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let notes_dir = std::path::PathBuf::from(
        args.next().ok_or_else(|| anyhow::anyhow!("用法: scene_fold <notes_dir> [--dry-run] [--since YYYY-MM-DD]"))?,
    );
    let mut dry = false;
    let mut since = String::new();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--dry-run" => dry = true,
            "--since" => since = args.next().unwrap_or_default().replace('-', ""),
            other => anyhow::bail!("未知参数: {other}"),
        }
    }
    let store = app_lib::store::NoteStore::new(notes_dir.clone());
    let mut ids: Vec<String> = std::fs::read_dir(&notes_dir)?
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    ids.sort();
    let (mut scanned, mut folded_notes, mut folded_segs, mut busy) = (0, 0, 0usize, 0);
    for id in ids {
        if !since.is_empty() && id.as_str() < since.as_str() {
            continue;
        }
        let dir = notes_dir.join(&id);
        let Some(sc) = app_lib::scene::load(&dir) else { continue };
        if sc.final_scene != app_lib::scene::SC_DUAL_PATH {
            continue;
        }
        scanned += 1;
        if dry {
            // 干跑:按可见段算候选数,不写盘
            match store.load(&id) {
                Ok(n) => {
                    let c = app_lib::scene::overlapped_mic_seqs(&n.segments).len();
                    println!("{id}: 候选 {c} 段(dry-run)");
                }
                Err(e) => eprintln!("{id}: 读取失败 {e}"),
            }
            continue;
        }
        match store.fold_dual_path_echo(&id) {
            Ok(0) => println!("{id}: 无新候选(已折叠或无重叠)"),
            Ok(n) => {
                println!("{id}: 折叠 {n} 段");
                folded_notes += 1;
                folded_segs += n;
            }
            Err(e) if e.to_string().contains("占用") => {
                eprintln!("{id}: 笔记被占用,跳过");
                busy += 1;
            }
            Err(e) => eprintln!("{id}: 失败 {e}"),
        }
    }
    println!("dual_path 场次 {scanned};折叠 {folded_notes} 场共 {folded_segs} 段;忙跳过 {busy}");
    Ok(())
}
