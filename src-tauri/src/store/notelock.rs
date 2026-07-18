//! 笔记目录跨进程写锁(独占文件锁)。
//!
//! 动机:2026-07-13 事故——第二个应用实例整表重写 segments.jsonl,录制实例的
//! 追加句柄从此指向被替换的孤儿 inode,35 分钟转写静默丢失。进程内锁
//! (EDIT_LOCK / writer Mutex)对跨进程无效,文件锁是最小充分武器。
//!
//! 实现走 std `File::try_lock`(rustc 1.89 稳定):unix 底层即 flock(LOCK_EX|LOCK_NB),
//! 语义与旧 libc 直调完全一致;Windows 底层是 LockFileEx,同样按句柄独占——
//! 借此免去 libc 平台分叉,Windows 构建开箱即用。
//!
//! 语义:锁按 open file description/句柄计——同进程再 open 也互斥,因此
//! 「本进程录制中」与「另一进程录制中」在编辑路径上得到同一种拒绝,无需区分。
//! 锁生命周期即值生命周期:Drop 关 fd 自动释放,崩溃时内核代为释放,无残留。

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

pub const LOCK_FILE: &str = ".note.lock";

pub struct NoteLock {
    _file: File,
}

impl NoteLock {
    /// 有界重试获取独占锁:非阻塞 flock 在瞬时竞争下会假性 EWOULDBLOCK
    /// (同进程串行编辑的锁交接窗口、macOS 高负载下 close 释放 flock 的传播延迟),
    /// 短退避重试(~100ms 内)把瞬时占用转成短暂等待;真正被长期持有(别的进程在
    /// 录制/转码,持锁数秒~数分钟)则全部失败返回 None。生产编辑/录制路径与并发单测
    /// 都应走这里,而非裸 try_exclusive(后者留给需断言"立即拒绝"语义的场景)。
    pub fn acquire(dir: &Path) -> std::io::Result<Option<NoteLock>> {
        const RETRIES: u32 = 5;
        const BACKOFF: std::time::Duration = std::time::Duration::from_millis(20);
        for attempt in 0..RETRIES {
            match Self::try_exclusive(dir)? {
                Some(lock) => return Ok(Some(lock)),
                None if attempt + 1 < RETRIES => std::thread::sleep(BACKOFF),
                None => {}
            }
        }
        Ok(None)
    }

    /// 非阻塞尝试独占。Ok(None) = 已被其他持有者(进程或本进程另一句柄)占用。
    pub fn try_exclusive(dir: &Path) -> std::io::Result<Option<NoteLock>> {
        let f = OpenOptions::new().create(true).write(true).open(dir.join(LOCK_FILE))?;
        match f.try_lock() {
            Ok(()) => Ok(Some(NoteLock { _file: f })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(e),
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
