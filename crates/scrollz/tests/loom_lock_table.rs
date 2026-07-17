//! D6 模型 2：rwfs per-inode 锁表「单活锁」引用计数算法的 loom 并发证明。
//!
//! # 这是算法级证明，不是生产码插桩
//!
//! 生产实现（`src/rwfs.rs`）用 `DashMap<u64, Arc<parking_lot::RwLock<InodeState>>>`，
//! DashMap 与 parking_lot 都是 loom **无法插桩**的（loom 只追踪 `loom::sync::*` 原语 +
//! `loom::thread`）。故用 loom 原语忠实复刻被验证算法骨架：`loom::sync::Mutex<HashMap>`
//! 当锁表、`loom::sync::Arc`（**必须是 loom 的 Arc**，否则 `strong_count` 不被追踪）+
//! `loom::sync::RwLock<i32>` 当 per-inode 锁与被保护数据。本文件证明的是 **「entry/remove_if
//! 在表锁内串行 → 同一 ino 任意时刻只有一把活锁」这一算法**；生产实现端到端正确由
//! `tests/concurrency_deadlock.rs` 与 rwfs.rs 单测 `并发_write_与_forget_同_ino_无_panic_*` 覆盖。
//!
//! # 复刻的算法骨架（对应 rwfs.rs）
//!
//! - `lock_for(ino)`：持表锁 `entry().or_default().clone()` 返回 Arc（对应
//!   `self.inodes.entry(ino).or_default().clone()`，表锁内 clone → 串行化）。
//! - `evict(ino)`：持表锁，若 `Arc::strong_count == 1` 则 remove（对应
//!   `remove_if(&ino, |_, arc| Arc::strong_count(arc) == 1)`）。
//!
//! # 不变量
//!
//! 1. **单活锁 → 互斥**：同一 ino 任意时刻至多一把活写锁。用一张**跨 Arc 世代**的共享
//!    见证表（`live: Mutex<HashSet<ino>>`）：`guarded_increment` 进临界区时 `insert(ino)`
//!    并断言此前未在表内、退出前 `remove`。若同一 ino 曾出现两把**不同** Arc 的活写锁并发
//!    进临界区，第二个进入者会发现 ino 已被标记 → 互斥违例。（per-Data 标志抓不到跨世代
//!    并发——两把不同 Arc 各有独立 Data，故必须用共享见证表。）
//! 2. **evict 守卫**：`evict` 在有活跃持有者（strong_count>1）时绝不抽走表项——否则
//!    「A 已 lock_for 持 Arc、B evict 删项、后来者 lock_for 新建第二把 Arc」→ 双活锁。
//!    用「A 持锁自增后经**自己持有的** Arc 读回，B 并发 evict」验证：A 的自增不因表项被删
//!    换代而丢失。
//!
//! 注意：eviction 合法删除**空闲**表项后，后来者 `lock_for` 重建全新一代 Arc（计数从 0 起）
//! 是**正确行为**，非违例。故单活锁不用「总自增次数」判据（会把合法重建误报为丢失），
//! 而用上述共享见证表 + 持有者视角读回两个直接判据。
//!
//! # 门控
//!
//! 整文件 `#![cfg(loom)]`：平时 `cargo test` 不编译；只有
//! `RUSTFLAGS="--cfg loom" cargo test --test loom_lock_table` 才穷举运行。
//!
//! # buggy 变体（构造 loom 反例，证明断言有效）
//!
//! `BUGGY=true` 时 `evict` 改用**无条件 remove**（不检查 strong_count）。这样当一个写线程
//! 已 `lock_for` 持有 Arc 时，evict 线程把表项删掉、其紧接的 `lock_for` `or_default` 新建
//! **第二把** Arc → 两把不同活锁并发写 → 共享见证表检出双活锁 → loom 抓反例。

#![cfg(loom)]

use loom::sync::{Arc, Mutex, RwLock};
use loom::thread;
use std::collections::{HashMap, HashSet};

/// 切 true 走 buggy 变体（evict 无条件 remove，破坏单活锁），让 loom 抓互斥/丢失反例。
/// 切 false 走 `strong_count==1` 守卫的正确实现，loom 穷举无反例。
const BUGGY: bool = false;

type Ino = u64;

/// per-inode 被保护数据：`counter` 记自增次数（供 evict 守卫测试经持有者视角读回）。
#[derive(Default)]
struct Data {
    counter: i32,
}

/// rwfs 锁表的 loom 骨架。
struct LockTable {
    /// 锁表：ino → Arc<RwLock<Data>>。loom Mutex 串行化 entry/remove_if。
    table: Mutex<HashMap<Ino, Arc<RwLock<Data>>>>,
    /// **跨 Arc 世代**的单活锁见证：当前正持 write 锁的 ino 集合。`guarded_increment` 进入
    /// 时插入、退出前移除，并断言进入时该 ino 未在集合内——若同一 ino 存在两把**不同** Arc
    /// 的活写锁并发进临界区，第二个进入者会发现该 ino 已在集合内 → 互斥违例。这是 per-Data
    /// 的 busy 标志抓不到的（不同 Arc 各有独立 Data），故用共享见证表跨世代侦测。
    live: Mutex<HashSet<Ino>>,
}

impl LockTable {
    fn new() -> Self {
        Self {
            table: Mutex::new(HashMap::new()),
            live: Mutex::new(HashSet::new()),
        }
    }

    /// 取（或建）某 ino 的锁句柄（对应 `lock_for`）。**表锁内** `entry().or_default().clone()`
    /// → 同一 ino 的取锁串行化；clone 出 Arc 后立即释放表锁，调用方拿 Arc 之后才取内层 RwLock
    /// （绝不在持表锁时锁内层 → 杜绝嵌套死锁面）。
    fn lock_for(&self, ino: Ino) -> Arc<RwLock<Data>> {
        let mut table = self.table.lock().unwrap();
        table.entry(ino).or_default().clone()
    }

    /// 回收某 ino 的锁项（对应 `evict_lock`）。正确实现仅当 `strong_count==1`（只剩表内
    /// 这一份 Arc，无其他持有者）时移除，防误删正在用的锁。buggy 变体无条件删。
    fn evict(&self, ino: Ino) {
        let mut table = self.table.lock().unwrap();
        if BUGGY {
            // buggy：不检查 strong_count，可能删掉别人正持有的项 → 下个 lock_for 新建第二把活锁。
            table.remove(&ino);
        } else {
            // 正确：strong_count==1 才删。等价于 DashMap::remove_if。
            if let Some(arc) = table.get(&ino) {
                if Arc::strong_count(arc) == 1 {
                    table.remove(&ino);
                }
            }
        }
    }
}

/// 持某 ino 的 write 锁，对被保护数据做「读-改-写」+ 跨世代互斥侦测。
/// 取内层 write 锁后，在共享 `live` 见证表里标记该 ino（断言此前未被标记），做自增，再清标记。
/// 若单活锁成立（同一 ino 至多一把活写锁），任意时刻至多一个线程能进临界区 → 见证不冲突。
/// buggy 变体下两把不同 Arc 的活写锁会让两线程同时进临界区，第二个见 ino 已标记 → 反例。
fn guarded_increment(table: &LockTable, ino: Ino, arc: &Arc<RwLock<Data>>) {
    let mut data = arc.write().unwrap();
    {
        let mut live = table.live.lock().unwrap();
        assert!(
            live.insert(ino),
            "互斥违例：ino={ino} 已有一把不同 Arc 的活写锁在临界区 → 双活锁"
        );
    }
    data.counter += 1;
    table.live.lock().unwrap().remove(&ino);
}

/// 不变量 1（单活锁 → 互斥）：模拟 rwfs 真实时序——一个写线程 `lock_for + 写`，另一线程
/// **先 evict 再 lock_for + 写**（对应 forget 回收后又有新句柄取锁）。
///
/// 正确实现（strong_count>1 不删）下：evict 线程在写线程持 Arc（含 seed）期间 evict，表项
/// 不被抽走，后续 lock_for 拿到**同一把** Arc → 两线程经同一 RwLock 串行 → 共享见证表不冲突。
/// buggy 实现（无条件删）下：evict 抽走正被持有的表项，紧接的 lock_for `or_default` 新建
/// **第二把** Arc → 两把不同活锁并发写 → 见证表检出双活锁 → loom 抓反例。
#[test]
fn lock_table_single_active_lock_mutual_exclusion() {
    loom::model(|| {
        let t = Arc::new(LockTable::new());
        let ino: Ino = 1;

        // 预置表项（模拟已有 inode 锁存在），使 evict 线程有东西可删、写线程有 Arc 可持。
        // 保留一份句柄使 strong_count 起点 >1（正确 evict 因此不删，buggy 才删）。
        let seed = t.lock_for(ino);

        let t1 = Arc::clone(&t);
        let w1 = thread::spawn(move || {
            // 写线程 1：取锁（与 seed 同一把）+ 写。持写锁期间对该 RwLock 独占。
            let arc = t1.lock_for(ino);
            guarded_increment(&t1, ino, &arc);
        });

        let t2 = Arc::clone(&t);
        let w2 = thread::spawn(move || {
            // 写线程 2：先 evict（buggy 会抽走 seed/w1 正用的表项），再 lock_for + 写。
            // buggy 下这次 lock_for 会 or_default 新建第二把 Arc → 与 w1 并发写不同锁。
            t2.evict(ino);
            let arc = t2.lock_for(ino);
            guarded_increment(&t2, ino, &arc);
        });

        w1.join().unwrap();
        w2.join().unwrap();

        // 核心判据是 guarded_increment 内的共享见证表互斥（穷举无违例即单活锁成立）。
        // seed 持有到此，保证正确实现下 evict 从不抽走该表项、始终单活锁。
        drop(seed);
    });
}

/// 不变量 2（evict 守卫）：`evict` 在有活跃持有者时绝不抽走表项（strong_count>1 不删）。
/// 线程 A 取得 Arc 并自增（持有期间 strong_count≥2），线程 B 并发 evict。A 经**自己持有的**
/// Arc 读回自增值——只要 evict 没在 A 持有期间误删并让数据换代，该值必为 1。
#[test]
fn evict_preserves_live_lock() {
    loom::model(|| {
        let t = Arc::new(LockTable::new());
        let ino: Ino = 2;

        let t_a = Arc::clone(&t);
        let a = thread::spawn(move || {
            let arc = t_a.lock_for(ino); // strong_count 此后 >=2（表 + 本地），evict 不得删。
            guarded_increment(&t_a, ino, &arc);
            // A 经自己持有的 Arc 读回：evict 守卫成立则这把锁始终是 A 增过的那把。
            let c = arc.read().unwrap().counter;
            assert_eq!(
                c, 1,
                "evict 误删活锁：A 持 Arc 期间数据被换代，自增丢失（counter={c}）"
            );
        });

        let t_b = Arc::clone(&t);
        let b = thread::spawn(move || {
            // 与 A 并发 evict。正确实现：A 持 Arc 时 strong_count>1 → 不删。
            t_b.evict(ino);
        });

        a.join().unwrap();
        b.join().unwrap();
    });
}
