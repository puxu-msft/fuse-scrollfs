//! redb 后端：布局 V「全包」形态。表 `blocks: (u64 ino, u64 idx) -> &[u8] blob`。
//!
//! 关键建模决策：
//! - redb 4.x 默认 `Durability::Immediate`：每次 `commit` 都 fsync。这正是设计 §6.1
//!   担忧的「每写一事务 = 每写一 fsync」的来源。我们保持默认，因为真实 fsync 才能
//!   暴露「每块一事务」陷阱的真实代价（用 `Durability::None` 会把陷阱测没了）。
//! - 批量策略 = 一个 write txn 内塞 K 块再 commit，对照单块 commit。

use redb::{Database, ReadableDatabase, TableDefinition};

use crate::backend::{Backend, CommitPolicy};

const BLOCKS: TableDefinition<(u64, u64), &[u8]> = TableDefinition::new("blocks");

pub struct RedbBackend {
    db: Database,
    path: std::path::PathBuf,
}

impl RedbBackend {
    pub fn create(path: &std::path::Path) -> Self {
        let db = Database::create(path).expect("创建 redb 数据库失败");
        // 预先建表，保证后续读事务能打开。
        {
            let wtxn = db.begin_write().expect("begin_write 失败");
            {
                let _t = wtxn.open_table(BLOCKS).expect("open_table 失败");
            }
            wtxn.commit().expect("初始建表 commit 失败");
        }
        RedbBackend {
            db,
            path: path.to_path_buf(),
        }
    }
}

impl Backend for RedbBackend {
    fn name(&self) -> &'static str {
        "redb"
    }

    fn bulk_insert(&mut self, blocks: &[(u64, u64, Vec<u8>)], policy: CommitPolicy) -> u64 {
        let mut bytes = 0u64;
        match policy {
            CommitPolicy::PerBlock => {
                for (ino, idx, blob) in blocks {
                    let wtxn = self.db.begin_write().expect("begin_write 失败");
                    {
                        let mut t = wtxn.open_table(BLOCKS).expect("open_table 失败");
                        t.insert(&(*ino, *idx), blob.as_slice())
                            .expect("insert 失败");
                    }
                    wtxn.commit().expect("commit 失败");
                    bytes += blob.len() as u64;
                }
            }
            CommitPolicy::Batched(k) => {
                for chunk in blocks.chunks(k) {
                    let wtxn = self.db.begin_write().expect("begin_write 失败");
                    {
                        let mut t = wtxn.open_table(BLOCKS).expect("open_table 失败");
                        for (ino, idx, blob) in chunk {
                            t.insert(&(*ino, *idx), blob.as_slice())
                                .expect("insert 失败");
                            bytes += blob.len() as u64;
                        }
                    }
                    wtxn.commit().expect("commit 失败");
                }
            }
        }
        bytes
    }

    fn get_block(&self, ino: u64, idx: u64, out: &mut Vec<u8>) -> Option<usize> {
        let rtxn = self.db.begin_read().expect("begin_read 失败");
        let t = rtxn.open_table(BLOCKS).expect("open_table 失败");
        match t.get(&(ino, idx)).expect("get 失败") {
            Some(guard) => {
                let v = guard.value();
                out.clear();
                out.extend_from_slice(v);
                Some(v.len())
            }
            None => None,
        }
    }

    fn put_block_committed(&mut self, ino: u64, idx: u64, blob: &[u8]) {
        let wtxn = self.db.begin_write().expect("begin_write 失败");
        {
            let mut t = wtxn.open_table(BLOCKS).expect("open_table 失败");
            t.insert(&(ino, idx), blob).expect("insert 失败");
        }
        wtxn.commit().expect("commit 失败");
    }

    fn put_batch_committed(&mut self, items: &[(u64, u64, Vec<u8>)]) {
        let wtxn = self.db.begin_write().expect("begin_write 失败");
        {
            let mut t = wtxn.open_table(BLOCKS).expect("open_table 失败");
            for (ino, idx, blob) in items {
                t.insert(&(*ino, *idx), blob.as_slice())
                    .expect("insert 失败");
            }
        }
        wtxn.commit().expect("commit 失败");
    }

    fn file_size(&self) -> u64 {
        crate::backend::path_size(&self.path)
    }

    fn compact(&mut self) -> Option<u64> {
        // redb compact 需要 &mut Database，且要求没有未完成事务。
        match self.db.compact() {
            Ok(_freed) => Some(self.file_size()),
            Err(e) => {
                eprintln!("[warn] redb compact 失败: {e}");
                None
            }
        }
    }

    fn sync(&mut self) {
        // redb 在 Immediate 下每次 commit 已落盘；这里发一个空事务作 barrier 并刷新元数据。
        let wtxn = self.db.begin_write().expect("begin_write 失败");
        wtxn.commit().expect("barrier commit 失败");
    }
}
