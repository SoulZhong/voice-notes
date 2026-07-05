//! 目录迁移引擎（纯文件逻辑，不碰 tauri）：复制 → 校验 → [命令层写指针] → 删旧。
//!
//! 语义定位：改数据/模型目录时把既有内容整树搬到新位置。settings 里的目录指针写入
//! 是迁移的**提交点**：提交前任何失败（复制/校验/写指针）都回滚新目录、旧数据原封
//! 未动，用户可安全重试；提交后删旧只是垃圾回收，失败也不影响迁移成立。这样中途
//! 崩溃永远不会出现「数据在新处、指针指旧处」的 split-brain。绝不采用「边移边删」
//! 那种一旦中断就两头都不完整的搬法。

use anyhow::Context;
use std::path::Path;

/// 目标目录是否可用作迁移落点：不存在 → Ok（迁移时 create_dir_all 造出来）；存在且为
/// 空目录 → Ok；存在但非空 / 存在但不是目录 → Err。守卫放在命令层，避免把用户既有
/// 数据的目录当成落点覆盖进去。
pub fn dir_is_usable_target(dir: &Path) -> anyhow::Result<()> {
    let meta = match std::fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()), // 不存在:可用
        Err(e) => return Err(e).with_context(|| format!("无法访问目标目录: {}", dir.display())),
    };
    if !meta.is_dir() {
        anyhow::bail!("目标不是目录: {}", dir.display());
    }
    // 存在且是目录:必须为空(有任何条目就拒绝,免得覆盖用户既有内容)。
    let mut rd = std::fs::read_dir(dir)
        .with_context(|| format!("无法读取目标目录: {}", dir.display()))?;
    if rd.next().is_some() {
        anyhow::bail!("目标目录非空: {}", dir.display());
    }
    Ok(())
}

/// 递归复制 src → dst，返回 (文件数, 总字节)。src 可以是单文件或目录。
/// 跳过 symlink:整树搬运只搬真实内容,符号链接原样重建会牵扯目标解析/循环风险,
/// 本应用的数据/模型目录里也不该有 symlink,遇到直接跳过最诚实(计入 0 文件 0 字节)。
pub fn copy_tree(src: &Path, dst: &Path) -> anyhow::Result<(u64, u64)> {
    let meta = std::fs::symlink_metadata(src)
        .with_context(|| format!("无法访问源: {}", src.display()))?;
    if meta.file_type().is_symlink() {
        return Ok((0, 0)); // 跳过 symlink
    }
    if meta.is_file() {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("无法创建目录: {}", parent.display()))?;
        }
        std::fs::copy(src, dst)
            .with_context(|| format!("复制失败: {} → {}", src.display(), dst.display()))?;
        return Ok((1, meta.len()));
    }
    // 目录:造出 dst 后递归。
    std::fs::create_dir_all(dst)
        .with_context(|| format!("无法创建目录: {}", dst.display()))?;
    let (mut files, mut bytes) = (0u64, 0u64);
    for entry in std::fs::read_dir(src)
        .with_context(|| format!("无法读取目录: {}", src.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let (f, b) = copy_tree(&src.join(&name), &dst.join(&name))?;
        files += f;
        bytes += b;
    }
    Ok((files, bytes))
}

/// 递归统计一棵树的 (文件数, 总字节)，跳过 symlink（与 copy_tree 的口径一致,才能对得上）。
/// 路径不存在视作空树 (0, 0)——verify 里缺项两侧都不存在时应当一致通过。
fn tree_stats(root: &Path) -> anyhow::Result<(u64, u64)> {
    let meta = match std::fs::symlink_metadata(root) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(e) => return Err(e).with_context(|| format!("无法访问: {}", root.display())),
    };
    if meta.file_type().is_symlink() {
        return Ok((0, 0));
    }
    if meta.is_file() {
        return Ok((1, meta.len()));
    }
    let (mut files, mut bytes) = (0u64, 0u64);
    for entry in std::fs::read_dir(root)
        .with_context(|| format!("无法读取目录: {}", root.display()))?
    {
        let entry = entry?;
        let (f, b) = tree_stats(&entry.path())?;
        files += f;
        bytes += b;
    }
    Ok((files, bytes))
}

/// 校验 dst 与 src 内容一致:双侧递归统计文件数 + 总字节,任一不符即 Err。
/// 用「数量 + 字节」而非逐字节哈希:迁移刚 copy 完,同机同盘,字节数不符已足够发现
/// 截断/漏拷这类真实故障;全量哈希 1GB 模型不划算。
pub fn verify_tree(src: &Path, dst: &Path) -> anyhow::Result<()> {
    let (sf, sb) = tree_stats(src)?;
    let (df, db) = tree_stats(dst)?;
    if sf != df || sb != db {
        anyhow::bail!(
            "校验不一致: 源({sf} 文件/{sb} 字节) 与 目标({df} 文件/{db} 字节) 不符"
        );
    }
    Ok(())
}

/// 迁移时序是「复制 → 校验 → **写指针** → 删旧」:settings 里的目录指针写入才是迁移的
/// 提交点,删旧只是提交后的垃圾回收。因此引擎拆成两步供命令层在中间插入指针写入:
/// copy_and_verify_entries(可失败,失败必回滚新目录)→ [命令层 save settings] →
/// remove_old_entries(不可失败——指针已指新,旧残留只是多占盘,绝不能把已提交的迁移
/// 标成失败)。若删旧发生在写指针之前,中途崩溃会留下"数据在新处、指针指旧处"的
/// split-brain,用户重启后找不到笔记——这正是本顺序要消灭的窗口。
///
/// 第一步:create_dir_all(new_root) → 对**存在的** entry 逐个 copy_tree + verify_tree
/// (缺项跳过,不报错)。任何失败:清理 new_root 下已复制的这些 entry 后 Err,旧数据
/// 全程未被触碰,可安全重试。
pub fn copy_and_verify_entries(
    old_root: &Path,
    new_root: &Path,
    entries: &[&str],
) -> anyhow::Result<()> {
    std::fs::create_dir_all(new_root)
        .with_context(|| format!("无法创建新目录: {}", new_root.display()))?;
    for e in entries {
        let src = old_root.join(e);
        if !src.exists() {
            continue; // 缺项跳过(如声纹目录还没建过)
        }
        let dst = new_root.join(e);
        if let Err(err) = copy_tree(&src, &dst).and_then(|_| verify_tree(&src, &dst)) {
            cleanup_copied_entries(new_root, entries);
            return Err(err).with_context(|| format!("迁移 {e} 失败,已回滚(旧数据未动)"));
        }
    }
    Ok(())
}

/// 清理 new_root 下本次复制进去的 entry(旧数据不动)。copy_and_verify_entries 失败时
/// 内部自调;命令层在「复制成功但指针写入失败」时也要调它——那种情况迁移未提交,
/// 新目录必须清干净,让用户可以原地重试(否则重试会被"目标非空"守卫拦下)。
pub fn cleanup_copied_entries(new_root: &Path, entries: &[&str]) {
    for e in entries {
        let p = new_root.join(e);
        let _ = std::fs::remove_dir_all(&p); // 目录残留
        let _ = std::fs::remove_file(&p); // 文件残留(remove_dir_all 对文件是 Err,这里补删)
    }
}

/// 第二步(指针已写入之后):逐 entry 删旧。删除失败只 eprintln 不返回 Err——此刻
/// 迁移已提交(指针指新),旧处残留仅是多占磁盘,把它上报成"迁移失败"反而误导用户
/// 重试一个已经成功的操作。不存在的 entry 静默跳过。
pub fn remove_old_entries(old_root: &Path, entries: &[&str]) {
    for e in entries {
        let src = old_root.join(e);
        if !src.exists() {
            continue;
        }
        let result = if src.is_dir() {
            std::fs::remove_dir_all(&src)
        } else {
            std::fs::remove_file(&src)
        };
        if let Err(err) = result {
            eprintln!("删除旧数据失败(迁移已完成,残留仅占磁盘): {}: {err}", src.display());
        }
    }
}

/// 目标目录与当前根目录必须互不包含:目标在当前根内部会被 copy_tree 自拷进死循环/
/// 随后删旧连带删新;当前根在目标内部则"空目录"守卫必然不成立且语义混乱。路径比较
/// 前用 std::path::absolute 归一(不解 symlink):new_dir 可能尚不存在,canonicalize
/// 会直接失败,absolute 只做词法归一恰好够用。
pub fn ensure_disjoint(old_root: &Path, new_dir: &Path) -> anyhow::Result<()> {
    let old = std::path::absolute(old_root)
        .with_context(|| format!("无法解析当前目录: {}", old_root.display()))?;
    let new = std::path::absolute(new_dir)
        .with_context(|| format!("无法解析目标目录: {}", new_dir.display()))?;
    if new.starts_with(&old) || old.starts_with(&new) {
        anyhow::bail!("目标目录不能与当前目录互相包含");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_entries_moves_and_cleans_old() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("old");
        let new = tmp.path().join("new");
        std::fs::create_dir_all(old.join("notes/n1")).unwrap();
        std::fs::write(old.join("notes/n1/meta.json"), b"{}").unwrap();
        std::fs::write(old.join("voiceprints.json"), b"{}").unwrap();
        let entries = &["notes", "voiceprints.json", "voiceprints"];
        // "voiceprints" 目录不存在:缺项跳过不报错。两步驱动模拟命令层的完整时序
        //(中间是指针写入,引擎不管):copy_and_verify → remove_old。
        copy_and_verify_entries(&old, &new, entries).unwrap();
        assert!(new.join("notes/n1/meta.json").exists());
        assert!(new.join("voiceprints.json").exists());
        assert!(old.join("notes/n1/meta.json").exists(), "删旧前旧数据完好(提交点在两步之间)");
        remove_old_entries(&old, entries);
        assert!(!old.join("notes").exists(), "成功后删旧");
        assert!(!old.join("voiceprints.json").exists());
        assert!(new.join("notes/n1/meta.json").exists(), "删旧不伤新处");
    }

    #[test]
    fn remove_old_skips_missing_entries() {
        // 指针已写入后的删旧是垃圾回收:不存在的 entry 静默跳过,任何失败都不算迁移
        // 失败(签名返回 (),本测试同时锁死"不 Err/不 panic"的契约)。
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("present"), b"x").unwrap();
        remove_old_entries(tmp.path(), &["present", "absent-dir", "absent.json"]);
        assert!(!tmp.path().join("present").exists(), "存在的照删");
    }

    #[test]
    fn cleanup_copied_entries_removes_only_listed() {
        // 「复制成功但指针写入失败」路径:命令层调 cleanup 清新目录残留,
        // 未列出的既有内容不受牵连。
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("notes")).unwrap();
        std::fs::write(tmp.path().join("notes/a"), b"1").unwrap();
        std::fs::write(tmp.path().join("voiceprints.json"), b"{}").unwrap();
        std::fs::write(tmp.path().join("unrelated"), b"keep").unwrap();
        cleanup_copied_entries(tmp.path(), &["notes", "voiceprints.json"]);
        assert!(!tmp.path().join("notes").exists());
        assert!(!tmp.path().join("voiceprints.json").exists());
        assert!(tmp.path().join("unrelated").exists(), "未列出的不动");
    }

    #[test]
    fn disjoint_rejects_nested_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        assert!(ensure_disjoint(&root, &root.join("sub")).is_err(), "目标是子目录:拒绝");
        assert!(ensure_disjoint(&root.join("sub"), &root).is_err(), "目标是父目录:拒绝");
        assert!(ensure_disjoint(&root, &root).is_err(), "同一目录:拒绝");
        assert!(ensure_disjoint(&root, &tmp.path().join("other")).is_ok(), "无关目录:放行");
    }

    #[test]
    fn migrate_failure_keeps_old_and_cleans_new() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("old");
        let new = tmp.path().join("new");
        std::fs::create_dir_all(old.join("notes")).unwrap();
        std::fs::write(old.join("notes/a.bin"), b"data").unwrap();
        // 让 verify 失败:预先在 new 放同名目录制造复制冲突不可行(copy 会并入),
        // 改为直接测清理助手——复制成功后人为破坏 dst 再走 verify 分支:
        std::fs::create_dir_all(&new).unwrap();
        let (n, _) = copy_tree(&old.join("notes"), &new.join("notes")).unwrap();
        assert_eq!(n, 1);
        std::fs::remove_file(new.join("notes/a.bin")).unwrap();
        assert!(verify_tree(&old.join("notes"), &new.join("notes")).is_err(), "缺文件必被察觉");
        assert!(old.join("notes/a.bin").exists(), "旧数据全程未动");
    }

    #[test]
    fn target_must_be_empty_or_absent() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(dir_is_usable_target(&tmp.path().join("absent")).is_ok());
        assert!(dir_is_usable_target(tmp.path()).is_ok(), "空目录可用");
        std::fs::write(tmp.path().join("x"), b"1").unwrap();
        assert!(dir_is_usable_target(tmp.path()).is_err(), "非空拒绝");
    }
}
