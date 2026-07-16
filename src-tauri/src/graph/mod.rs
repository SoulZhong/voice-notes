//! Aing 全局知识图谱:从所有 aing.json 派生的 SQLite 索引(实体注册表 + 笔记↔实体边)。
//! 纯增值产物——任何失败只降级,绝不打断 Aing;丢了可从 aing.json 整库重建。

use std::path::Path;

pub(crate) const GRAPH_FILE: &str = "graph.sqlite";

/// 写路径进程内串行(rusqlite::Connection 非 Sync;沿用全仓"每次操作现开连接 + Mutex 门禁"惯例)。
pub(crate) static GRAPH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS entities (
  id         TEXT PRIMARY KEY,
  kind       TEXT NOT NULL,
  name       TEXT NOT NULL,
  aliases    TEXT NOT NULL DEFAULT '[]',
  is_person  INTEGER NOT NULL DEFAULT 0,
  updated_at TEXT
);
CREATE TABLE IF NOT EXISTS note_entities (
  note_id       TEXT NOT NULL,
  entity_id     TEXT NOT NULL,
  mention_count INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (note_id, entity_id)
);
CREATE INDEX IF NOT EXISTS idx_ne_entity ON note_entities(entity_id);
";

/// 打开(必要时创建)图谱库,建表 + 设 WAL/busy_timeout。幂等。
pub(crate) fn open(data_root: &Path) -> anyhow::Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open(data_root.join(GRAPH_FILE))?;
    conn.busy_timeout(std::time::Duration::from_millis(3000))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_creates_tables_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(dir.path()).unwrap();
        // 两表存在
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('entities','note_entities')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2);
        drop(conn);
        // 再开一次不报错(IF NOT EXISTS)
        let _ = open(dir.path()).unwrap();
        assert!(dir.path().join("graph.sqlite").exists());
    }
}
