//! 笔记目录跨进程写锁(flock 独占)。
//!
//! 动机:2026-07-13 事故——第二个应用实例整表重写 segments.jsonl,录制实例的
//! 追加句柄从此指向被替换的孤儿 inode,35 分钟转写静默丢失。进程内锁
//! (EDIT_LOCK / writer Mutex)对跨进程无效,flock 是最小充分武器。
//!
//! 语义:flock 按 open file description 计——同进程再 open 也互斥,因此
//! 「本进程录制中」与「另一进程录制中」在编辑路径上得到同一种拒绝,无需区分。
//! 锁生命周期即值生命周期:Drop 关 fd 自动释放,崩溃时内核代为释放,无残留。

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::Path;

pub const LOCK_FILE: &str = ".note.lock";

pub struct NoteLock {
    _file: File,
}

impl NoteLock {
    /// 非阻塞尝试独占。Ok(None) = 已被其他持有者(进程或本进程另一句柄)占用。
    pub fn try_exclusive(dir: &Path) -> std::io::Result<Option<NoteLock>> {
        let f = OpenOptions::new().create(true).write(true).open(dir.join(LOCK_FILE))?;
        let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            Ok(Some(NoteLock { _file: f }))
        } else {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
                Ok(None)
            } else {
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_blocks_second_holder_and_drop_releases() {
        let dir = tempfile::tempdir().unwrap();
        let l1 = NoteLock::try_exclusive(dir.path()).unwrap();
        assert!(l1.is_some(), "首个持有者应拿到锁");
        // 同进程第二个句柄也应被拒(flock 按 OFD 计)
        let l2 = NoteLock::try_exclusive(dir.path()).unwrap();
        assert!(l2.is_none(), "锁被持有时第二个句柄应拿不到");
        drop(l1);
        let l3 = NoteLock::try_exclusive(dir.path()).unwrap();
        assert!(l3.is_some(), "Drop 后应可重新获取");
    }

    #[test]
    fn lock_file_created_in_dir() {
        let dir = tempfile::tempdir().unwrap();
        let _l = NoteLock::try_exclusive(dir.path()).unwrap();
        assert!(dir.path().join(LOCK_FILE).exists());
    }
}
