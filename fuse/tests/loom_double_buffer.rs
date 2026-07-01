//! D6 模型 1：container 双缓冲提交协议的 loom 并发证明。
//!
//! # 这是算法级证明，不是生产码插桩
//!
//! 生产实现（`src/store/container.rs`）用 `parking_lot::Mutex` + redb，二者 loom 都
//! **无法插桩**（loom 只追踪用 `loom::sync::*` 写的同步原语与用 `loom::thread` 起的线程）。
//! 故标准做法是：用 loom 自己的原语**忠实复刻被验证算法的并发骨架**，让 loom 穷举所有
//! 线程交错并断言不变量。本文件证明的是 **双缓冲提交协议这一算法**；生产实现的端到端
//! 正确性由 `commit_failure_does_not_lose_pending_blocks` / `get_block_reads_from_flushing_buffer_mid_commit`
//! 等确定性回归测试（container.rs 单测 + tests/fault_injection.rs）覆盖。
//!
//! # 复刻的算法骨架（对应 container.rs）
//!
//! - `Inner { active, flushing }` 两层暂存（`commit_pending` / `Inner` / `merge_from_flushing`）。
//! - `put` 写 `active`。
//! - `get` 在 `inner` 锁内查 **active → flushing → backend** 三层（对应 `get_block` 的
//!   read-through 双缓冲 + 落 redb）。
//! - `commit` 持 `commit_lock` 串行化 → swap active↔flushing → 释放 inner 锁 → 把 flushing
//!   写进 backend（redb 替身）→ 成功清 flushing / 失败 `merge_from_flushing` 回 active。
//!   commit 可注入失败（对应 `fault_commit` / `flush_to_redb` 的 Err 分支）。
//!
//! # 不变量
//!
//! 1. **torn-read**：已 put 且未被覆盖/删除的键，`get` 必返回它（或更新值），**绝不返回
//!    None**——即便它正处于 active→flushing→backend 的搬运中途（swap 后、写 backend 前）。
//! 2. **lost-update**：commit 失败后该键仍可 `get` 到（merge 回 active 不丢数据）。
//!
//! # 门控
//!
//! 整文件 `#![cfg(loom)]`：平时 `cargo test` 不编译；只有
//! `RUSTFLAGS="--cfg loom" cargo test --test loom_double_buffer` 才穷举运行。
//!
//! # buggy 变体（构造 loom 反例，证明断言有效）
//!
//! `commit_buggy_mem_take` 复刻旧实现：commit 在 redb 成败已知**之前**就 `mem::take`
//! 清空 active（无 flushing 二级缓冲）。把 `BUGGY` 常量改 true 即可让 loom 抓出 lost-update
//! 反例（commit 失败 → 暂存已被清空 → 数据永久丢失）。交回里贴出该反例。

#![cfg(loom)]

use loom::sync::Mutex;
use loom::thread;
use std::collections::HashMap;

/// 切 true 走 buggy 变体（旧 `mem::take` 单缓冲），让 loom 抓 lost-update 反例。
/// 切 false 走双缓冲正确实现，loom 穷举无反例。
const BUGGY: bool = false;

type K = u64;
type V = u64;

/// 一代挂起暂存（对应 container.rs 的 `Pending`，此处只保留键值映射作最小骨架）。
#[derive(Default, Clone)]
struct Pending {
    map: HashMap<K, V>,
}

impl Pending {
    fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// commit 失败回滚：把 flushing 并回 active。**active 已有键不覆盖**——active 是
    /// swap 之后的新写，优先级高于回滚的旧 flushing（对应 `merge_from_flushing` 的
    /// `entry().or_insert()` 语义，D1 lost-update 修复）。
    fn merge_from_flushing(&mut self, flushing: Pending) {
        for (k, v) in flushing.map {
            self.map.entry(k).or_insert(v);
        }
    }
}

/// 双缓冲暂存（对应 container.rs 的 `Inner { active, flushing }`）。
#[derive(Default)]
struct Inner {
    active: Pending,
    flushing: Pending,
}

/// container 双缓冲提交协议的 loom 骨架。
struct Model {
    /// 暂存双缓冲（loom Mutex）。
    inner: Mutex<Inner>,
    /// 串行化 commit，与 inner 锁分离（IO 期间不阻塞读写并发）。
    commit_lock: Mutex<()>,
    /// redb 替身：已 durable 的 committed backend。
    backend: Mutex<HashMap<K, V>>,
}

impl Model {
    fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            commit_lock: Mutex::new(()),
            backend: Mutex::new(HashMap::new()),
        }
    }

    /// 写 active（对应 `put_block`）。
    fn put(&self, k: K, v: V) {
        let mut inner = self.inner.lock().unwrap();
        inner.active.map.insert(k, v);
    }

    /// 读三层：active → flushing → backend（对应 `get_block` 的 read-through 双缓冲 + 落 redb）。
    /// 在 inner 锁内查暂存两层，再查 backend——条目恒在 active∪flushing∪backend，
    /// 消灭 commit 中间窗口的 torn read。
    fn get(&self, k: K) -> Option<V> {
        let inner = self.inner.lock().unwrap();
        if let Some(&v) = inner.active.map.get(&k) {
            return Some(v);
        }
        if let Some(&v) = inner.flushing.map.get(&k) {
            return Some(v);
        }
        drop(inner);
        self.backend.lock().unwrap().get(&k).copied()
    }

    /// 提交挂起暂存。`fail` 注入 backend 写失败。返回 Ok(())/Err(())。
    fn commit(&self, fail: bool) -> Result<(), ()> {
        if BUGGY {
            self.commit_buggy_mem_take(fail)
        } else {
            self.commit_double_buffer(fail)
        }
    }

    /// 正确实现：双缓冲协议（对应 `commit_pending`）。
    ///
    /// ① 持 commit_lock 串行化；② 持 inner 锁 swap active↔flushing（active 清空，新写继续
    /// 进 active），释放 inner 锁；③ 用 flushing 写 backend（IO 期间不持 inner 锁，读路径
    /// 仍查 flushing → 无 torn read）；④ 成功清 flushing；失败 merge_from_flushing 回 active。
    fn commit_double_buffer(&self, fail: bool) -> Result<(), ()> {
        let _commit_guard = self.commit_lock.lock().unwrap();

        // swap：active → flushing，active 清空。
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.active.is_empty() {
                // 无新挂起；flushing 此刻必空（上次 commit 已清/合并）。
                return Ok(());
            }
            let Inner { active, flushing } = &mut *inner;
            std::mem::swap(active, flushing);
        }

        // IO 期间不持 inner 锁。克隆出 flushing 内容写 backend（缓冲本体留 inner 供读路径查）。
        let flushing_snapshot = {
            let inner = self.inner.lock().unwrap();
            inner.flushing.clone()
        };

        if fail {
            // 失败：把 flushing 合并回 active（已有键不覆盖）→ 数据不丢，下次重试。
            let mut inner = self.inner.lock().unwrap();
            let flushing = std::mem::take(&mut inner.flushing);
            inner.active.merge_from_flushing(flushing);
            return Err(());
        }

        // 成功：落 backend，再清空 flushing（内容已 durable）。
        {
            let mut backend = self.backend.lock().unwrap();
            for (k, v) in &flushing_snapshot.map {
                backend.insert(*k, *v);
            }
        }
        self.inner.lock().unwrap().flushing = Pending::default();
        Ok(())
    }

    /// buggy 变体：旧 `mem::take` 单缓冲（无 flushing 二级缓冲）。
    ///
    /// 在 redb 成败已知**之前**就 take 走 active（清空），若随后 backend 写失败，
    /// 暂存已永久丢失 → lost-update（下次 commit 因 active 空假成功掩盖）。loom 会抓到
    /// 「commit 失败后 get 返回 None」的反例。
    fn commit_buggy_mem_take(&self, fail: bool) -> Result<(), ()> {
        let _commit_guard = self.commit_lock.lock().unwrap();
        // 在成败已知前就清空 active（致命：暂存被 drop）。
        let taken = {
            let mut inner = self.inner.lock().unwrap();
            std::mem::take(&mut inner.active)
        };
        if fail {
            // 暂存 `taken` 在此函数返回时被 drop → 永久丢失。
            return Err(());
        }
        let mut backend = self.backend.lock().unwrap();
        for (k, v) in taken.map {
            backend.insert(k, v);
        }
        Ok(())
    }
}

/// 不变量 1（torn-read）：线程 A 反复 put+commit，线程 B 反复 get；已 put 的键在搬运
/// 途中也绝不读到 None。穷举 swap 后/写 backend 前等所有中间态。
#[test]
fn double_buffer_no_torn_read() {
    loom::model(|| {
        let m = std::sync::Arc::new(Model::new());

        // 预置一个 committed 键，保证 backend 非空、读路径要穿三层。
        m.commit(false).ok(); // 空 commit（active 空）→ Ok。
        m.put(1, 100);
        m.commit(false).unwrap(); // 键 1 落 backend。

        let m_a = std::sync::Arc::clone(&m);
        let a = thread::spawn(move || {
            // 线程 A：put 键 1 的新值 + commit（成功），制造 active→flushing→backend 搬运。
            m_a.put(1, 101);
            m_a.commit(false).unwrap();
        });

        let m_b = std::sync::Arc::clone(&m);
        let b = thread::spawn(move || {
            // 线程 B：读键 1。无论交错到 swap 前/中/后，键 1 恒可见（torn-read 不变量）。
            let got = m_b.get(1);
            assert!(
                got.is_some(),
                "torn-read 违例：已 put 的键 1 在搬运途中读到 None（got={got:?}）"
            );
            // 值要么旧 100、要么新 101，绝不是别的。
            let v = got.unwrap();
            assert!(v == 100 || v == 101, "键 1 读到非法值 {v}");
        });

        a.join().unwrap();
        b.join().unwrap();

        // 终态：键 1 必为 101（A 的写已 commit 成功）。
        assert_eq!(m.get(1), Some(101), "终态键 1 应为已提交的新值 101");
    });
}

/// 不变量 2（lost-update）：commit 失败后该键仍可 get 到（merge 回 active 不丢）。
/// 线程 A put 后 commit 注入失败，线程 B 并发 get；断言键恒可见。
#[test]
fn double_buffer_no_lost_update_on_commit_failure() {
    loom::model(|| {
        let m = std::sync::Arc::new(Model::new());

        let m_a = std::sync::Arc::clone(&m);
        let a = thread::spawn(move || {
            m_a.put(7, 700);
            // 注入失败：双缓冲实现把 flushing merge 回 active，数据不丢。
            let r = m_a.commit(true);
            assert_eq!(r, Err(()), "注入故障 commit 应返回 Err");
        });

        let m_b = std::sync::Arc::clone(&m);
        let b = thread::spawn(move || {
            // 并发读：键 7 一旦被 put，任何时刻（含 commit 失败搬运中）都不该消失。
            // 注意：B 可能在 A put 之前就读 → 那时 None 合法；只断言「读到 Some 时值合法」，
            // 终态断言放 join 之后（彼时 A 的 put 必已发生）。
            if let Some(v) = m_b.get(7) {
                assert_eq!(v, 700, "键 7 读到非法值 {v}");
            }
        });

        a.join().unwrap();
        b.join().unwrap();

        // 终态：A 的 put + 失败 commit 之后，键 7 仍可见（lost-update 不变量）。
        assert_eq!(
            m.get(7),
            Some(700),
            "lost-update 违例：commit 失败后键 7 丢失（双缓冲应 merge 回 active）"
        );
    });
}
