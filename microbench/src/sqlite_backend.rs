//! rusqlite 后端：布局 V 备选形态（设计 §6.1 表末行）。表 `blocks(ino, idx, blob)`。
//!
//! 公平性对照说明：
//! - 为与 redb 默认 `Durability::Immediate`（每 commit 一 fsync）对齐，这里设
//!   `synchronous = FULL` 且默认 rollback journal（每事务 fsync）。这样「每块一事务」
//!   在两后端语义一致：都是「每写一次 fsync」。
//! - 不开 WAL：WAL 会把多次 commit 的 fsync 摊到 checkpoint，语义偏离 redb 的同步落盘，
//!   反而不公平。报告里会注明这一口径选择。

use rusqlite::{Connection, OptionalExtension};

use crate::backend::{Backend, CommitPolicy};

pub struct SqliteBackend {
    conn: Connection,
    path: std::path::PathBuf,
}

impl SqliteBackend {
    pub fn create(path: &std::path::Path) -> Self {
        let conn = Connection::open(path).expect("打开 sqlite 失败");
        // 与 redb Immediate 对齐：每事务同步落盘。
        conn.pragma_update(None, "journal_mode", "DELETE")
            .expect("设置 journal_mode 失败");
        conn.pragma_update(None, "synchronous", "FULL")
            .expect("设置 synchronous 失败");
        conn.execute(
            "CREATE TABLE IF NOT EXISTS blocks (
                ino  INTEGER NOT NULL,
                idx  INTEGER NOT NULL,
                blob BLOB NOT NULL,
                PRIMARY KEY (ino, idx)
            )",
            [],
        )
        .expect("建表失败");
        SqliteBackend {
            conn,
            path: path.to_path_buf(),
        }
    }
}

impl Backend for SqliteBackend {
    fn name(&self) -> &'static str {
        "sqlite"
    }

    fn bulk_insert(&mut self, blocks: &[(u64, u64, Vec<u8>)], policy: CommitPolicy) -> u64 {
        let mut bytes = 0u64;
        match policy {
            CommitPolicy::PerBlock => {
                for (ino, idx, blob) in blocks {
                    let tx = self.conn.transaction().expect("begin tx 失败");
                    tx.execute(
                        "INSERT OR REPLACE INTO blocks (ino, idx, blob) VALUES (?1, ?2, ?3)",
                        rusqlite::params![*ino as i64, *idx as i64, blob.as_slice()],
                    )
                    .expect("insert 失败");
                    tx.commit().expect("commit 失败");
                    bytes += blob.len() as u64;
                }
            }
            CommitPolicy::Batched(k) => {
                for chunk in blocks.chunks(k) {
                    let tx = self.conn.transaction().expect("begin tx 失败");
                    {
                        let mut stmt = tx
                            .prepare_cached(
                                "INSERT OR REPLACE INTO blocks (ino, idx, blob) VALUES (?1, ?2, ?3)",
                            )
                            .expect("prepare 失败");
                        for (ino, idx, blob) in chunk {
                            stmt.execute(rusqlite::params![
                                *ino as i64,
                                *idx as i64,
                                blob.as_slice()
                            ])
                            .expect("insert 失败");
                            bytes += blob.len() as u64;
                        }
                    }
                    tx.commit().expect("commit 失败");
                }
            }
        }
        bytes
    }

    fn get_block(&self, ino: u64, idx: u64, out: &mut Vec<u8>) -> Option<usize> {
        let res: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT blob FROM blocks WHERE ino = ?1 AND idx = ?2",
                rusqlite::params![ino as i64, idx as i64],
                |row| row.get(0),
            )
            .optional()
            .expect("查询失败");
        match res {
            Some(v) => {
                out.clear();
                out.extend_from_slice(&v);
                Some(v.len())
            }
            None => None,
        }
    }

    fn put_block_committed(&mut self, ino: u64, idx: u64, blob: &[u8]) {
        let tx = self.conn.transaction().expect("begin tx 失败");
        tx.execute(
            "INSERT OR REPLACE INTO blocks (ino, idx, blob) VALUES (?1, ?2, ?3)",
            rusqlite::params![ino as i64, idx as i64, blob],
        )
        .expect("insert 失败");
        tx.commit().expect("commit 失败");
    }

    fn put_batch_committed(&mut self, items: &[(u64, u64, Vec<u8>)]) {
        let tx = self.conn.transaction().expect("begin tx 失败");
        {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT OR REPLACE INTO blocks (ino, idx, blob) VALUES (?1, ?2, ?3)",
                )
                .expect("prepare 失败");
            for (ino, idx, blob) in items {
                stmt.execute(rusqlite::params![*ino as i64, *idx as i64, blob.as_slice()])
                    .expect("insert 失败");
            }
        }
        tx.commit().expect("commit 失败");
    }

    fn file_size(&self) -> u64 {
        crate::backend::path_size(&self.path)
    }

    fn compact(&mut self) -> Option<u64> {
        self.conn.execute("VACUUM", []).expect("VACUUM 失败");
        Some(self.file_size())
    }

    fn sync(&mut self) {
        // FULL 同步下每 commit 已落盘；执行一次 checkpoint/no-op 保证元数据刷新。
        self.conn
            .pragma_update(None, "wal_checkpoint", "TRUNCATE")
            .ok();
    }
}
