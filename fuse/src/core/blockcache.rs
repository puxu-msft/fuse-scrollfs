//! 解压块缓存（perf #1）：缓存已解压的**不可变内部块**明文，消除顺序读放大。
//!
//! ## 问题
//! `rwfs::read_range` 对每个覆盖块都 `get_block → decompress_block`。内核按 ~128KiB 粒度下发
//! read，而块默认 1MiB → resume 的整文件顺序前向扫描把**同一个 1MiB 块解压约 8 次**。本缓存
//! 按 `(ino, block_idx)` 缓存解压后的 `Arc<[u8]>` 明文，命中即免去整块解压。
//!
//! ## 只缓存「严格内部块」（杠杆 A，正确性承重）
//! 调用方（`read_range`）**只对 `idx < tail_idx` 的块** `insert`，其中
//! `tail_idx = (uncompressed_size - 1) / chunk_size`。两条已被代码实证的事实使此规则充分：
//! - `get_block` 对 `idx == chunk_count` 返回**可变的尾日志重放块**（store/shadow.rs）——缓存它
//!   会在下次 append 后读到陈旧字节。`idx < tail_idx` 排除它。
//! - append/seal/`materialize` **只写尾块索引**、从不改写内部已封存块（core/wsession.rs）。故
//!   `idx < tail_idx` 的块在 append 主负载下恒不可变；fsync/flush/release/forget 只 seal 尾块，
//!   结构上不可能影响任何被缓存的块。能改写内部块的只有 RMW write 与 truncate。
//!
//! ## 失效与并发（无需 epoch）
//! `BlockCache` 自身只有一把 `Mutex` 守护跨 inode 并发。**跨 inode 失效一致性靠调用方的
//! per-inode `RwLock`**：`get`/`insert` 只在读路径（持读锁）发生，[`BlockCache::invalidate`] 只在
//! 变更路径（持写锁）发生 → 同一 inode 二者互斥，不存在「失效后又插入陈旧块」竞态，故无需
//! per-ino epoch。**调用方必须在持该 inode 写锁时调用 `invalidate`**（不变量）。
//!
//! ## 内存压力自适应
//! 字节预算 `eff_cap = min(configured_cap, budget)`，
//! `budget = (MemAvailable + 本缓存已占字节 - RESERVE_BYTES) / 2`。把本缓存已占字节加回
//! available 消除「缓存占用压低 available → 压低 cap → 逐出」的自激震荡。探测经
//! [`AvailableMemory`] 抽象（生产读 `/proc/meminfo`），按 `probe_interval` 节流。低内存自动缩
//! `eff_cap` 直至 0（清空自身占用）。

use parking_lot::Mutex;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 默认缓存字节上限（128 MiB）。0 = 禁用。
pub const DEFAULT_CACHE_BYTES: usize = 128 * 1024 * 1024;

/// 内存压力保留量：低于此可用内存即把缓存预算压到 0，给系统留头寸。
const RESERVE_BYTES: u64 = 256 * 1024 * 1024;

/// 压力探测节流间隔：两次 `available_bytes` 探测至少相隔此时长，避免每次 insert 都读 /proc。
const PRESSURE_PROBE_INTERVAL: Duration = Duration::from_secs(1);

/// 可用内存探测抽象（便于测试注入 fake）。
pub trait AvailableMemory: Send + Sync {
    /// 当前可用内存字节；不可得返回 `None`（则不据压力缩 cap，沿用 configured_cap）。
    fn available_bytes(&self) -> Option<u64>;
}

/// 生产实现：读 `/proc/meminfo` 的 `MemAvailable`。非 Linux / 读失败 → `None`。
pub struct ProcMeminfo;

impl AvailableMemory for ProcMeminfo {
    fn available_bytes(&self) -> Option<u64> {
        let text = std::fs::read_to_string("/proc/meminfo").ok()?;
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("MemAvailable:") {
                // 形如 "MemAvailable:   12345 kB"
                let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
                return Some(kb.saturating_mul(1024));
            }
        }
        None
    }
}

/// 一个缓存条目：解压明文 + 其 LRU 次序号。
struct Node {
    bytes: Arc<[u8]>,
    seq: u64,
}

/// `Mutex` 保护的可变状态。
struct Inner {
    map: HashMap<(u64, u64), Node>,
    /// LRU 次序：seq → key。最小 seq = 最久未用。`Node.seq` 与之互为反向索引；替换/淘汰时
    /// 必须同步维护，逐出时校验 `map[key].seq == 弹出的 seq` 才删（防悬空 seq 致记账漂移）。
    order: BTreeMap<u64, (u64, u64)>,
    /// ino → 该 ino 已缓存的 idx 集合。使 `invalidate(ino)` 退化为 O(该 ino 块数)，避免全表
    /// 线性扫描——`invalidate` 在 append 热路径（每次 write/fsync）触发，全表扫会随缓存增大变贵。
    by_ino: HashMap<u64, HashSet<u64>>,
    cur_bytes: usize,
    eff_cap: usize,
    seq: u64,
    last_probe: Option<Instant>,
}

/// 解压块缓存。`configured_cap == 0` 时全 no-op。
pub struct BlockCache {
    inner: Mutex<Inner>,
    configured_cap: usize,
    probe: Box<dyn AvailableMemory>,
    probe_interval: Duration,
}

impl std::fmt::Debug for BlockCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let g = self.inner.lock();
        f.debug_struct("BlockCache")
            .field("configured_cap", &self.configured_cap)
            .field("eff_cap", &g.eff_cap)
            .field("cur_bytes", &g.cur_bytes)
            .field("entries", &g.map.len())
            .finish()
    }
}

impl BlockCache {
    /// 生产构造：读 `/proc/meminfo` 探测、默认节流间隔。
    pub fn new(configured_cap: usize) -> Self {
        Self::with_probe_and_interval(
            configured_cap,
            Box::new(ProcMeminfo),
            PRESSURE_PROBE_INTERVAL,
        )
    }

    /// 注入探测 + 节流间隔（测试用：interval=0 则每次 insert 重探测，便于验证压力动态）。
    pub fn with_probe_and_interval(
        configured_cap: usize,
        probe: Box<dyn AvailableMemory>,
        probe_interval: Duration,
    ) -> Self {
        Self {
            inner: Mutex::new(Inner {
                map: HashMap::new(),
                order: BTreeMap::new(),
                by_ino: HashMap::new(),
                cur_bytes: 0,
                // 初始 = 配置上限；首次 insert 探测修正。
                eff_cap: configured_cap,
                seq: 0,
                last_probe: None,
            }),
            configured_cap,
            probe,
            probe_interval,
        }
    }

    /// 是否启用（configured_cap > 0）。
    pub fn enabled(&self) -> bool {
        self.configured_cap > 0
    }

    /// 取缓存块明文（命中则更新 LRU 次序）。未启用 / 未命中 → `None`。
    pub fn get(&self, ino: u64, idx: u64) -> Option<Arc<[u8]>> {
        if self.configured_cap == 0 {
            return None;
        }
        let mut g = self.inner.lock();
        let g = &mut *g;
        let key = (ino, idx);
        if !g.map.contains_key(&key) {
            return None;
        }
        g.seq += 1;
        let new_seq = g.seq;
        let (old_seq, bytes) = {
            let node = g.map.get_mut(&key).unwrap();
            let old = node.seq;
            node.seq = new_seq;
            (old, node.bytes.clone())
        };
        g.order.remove(&old_seq);
        g.order.insert(new_seq, key);
        Some(bytes)
    }

    /// 存入块明文。调用方须保证 `idx < tail_idx`（只缓存不可变内部块，见模块文档）。
    /// 触发节流压力探测；超 `eff_cap` 的单块不缓存；按字节预算 LRU 逐出。
    pub fn insert(&self, ino: u64, idx: u64, bytes: Arc<[u8]>) {
        if self.configured_cap == 0 {
            return;
        }
        let mut g = self.inner.lock();
        self.refresh_effective_cap(&mut g);
        let key = (ino, idx);
        let blen = bytes.len();
        // 单块超 eff_cap（含 eff_cap==0）：不缓存，但须移除该键既有旧值，避免残留陈旧明文。
        if g.eff_cap == 0 || blen > g.eff_cap {
            remove_key(&mut g, key);
            return;
        }
        // 替换已存在键：先减旧字节、删旧 seq（防双计 + 悬空 seq）。
        remove_key(&mut g, key);
        // 逐出最旧直到放得下。
        while g.cur_bytes + blen > g.eff_cap {
            let Some((&old_seq, &old_key)) = g.order.iter().next() else {
                break;
            };
            // 仅当该 seq 仍是 map 当前条目（非悬空）才完整移除；否则只清悬空 order 条目推进循环。
            if g.map.get(&old_key).map(|n| n.seq) == Some(old_seq) {
                remove_key(&mut g, old_key);
            } else {
                g.order.remove(&old_seq);
            }
        }
        g.seq += 1;
        let new_seq = g.seq;
        g.order.insert(new_seq, key);
        g.map.insert(
            key,
            Node {
                bytes,
                seq: new_seq,
            },
        );
        g.by_ino.entry(ino).or_default().insert(idx);
        g.cur_bytes += blen;
    }

    /// 失效某 inode 的全部缓存条目、回收字节。经 `by_ino` 索引做 O(该 ino 块数)，不全表扫描
    /// （`invalidate` 在 append 热路径触发）。**必须在持该 inode 写锁时调用**（见模块文档）。
    pub fn invalidate(&self, ino: u64) {
        if self.configured_cap == 0 {
            return;
        }
        let mut g = self.inner.lock();
        let Some(idxs) = g.by_ino.remove(&ino) else {
            return;
        };
        for idx in idxs {
            if let Some(node) = g.map.remove(&(ino, idx)) {
                g.order.remove(&node.seq);
                debug_assert!(
                    g.cur_bytes >= node.bytes.len(),
                    "cur_bytes 记账漂移：cur_bytes={} < node={}",
                    g.cur_bytes,
                    node.bytes.len()
                );
                g.cur_bytes = g.cur_bytes.saturating_sub(node.bytes.len());
            }
        }
    }

    /// 按节流间隔刷新 `eff_cap`。把本缓存已占字节加回 available 消除自激反馈。
    fn refresh_effective_cap(&self, g: &mut Inner) {
        let now = Instant::now();
        let due = match g.last_probe {
            Some(t) => now.duration_since(t) >= self.probe_interval,
            None => true,
        };
        if !due {
            return;
        }
        g.last_probe = Some(now);
        g.eff_cap = match self.probe.available_bytes() {
            None => self.configured_cap,
            Some(avail) => {
                let effective_avail = avail.saturating_add(g.cur_bytes as u64);
                let budget = effective_avail.saturating_sub(RESERVE_BYTES) / 2;
                budget.min(self.configured_cap as u64) as usize
            }
        };
    }

    #[cfg(test)]
    fn cur_bytes(&self) -> usize {
        self.inner.lock().cur_bytes
    }

    #[cfg(test)]
    fn eff_cap(&self) -> usize {
        self.inner.lock().eff_cap
    }

    #[cfg(test)]
    fn entry_count(&self) -> usize {
        self.inner.lock().map.len()
    }

    /// 测试用：`by_ino` 索引覆盖的 ino 数（验证索引不泄漏、与 map 一致）。
    #[cfg(test)]
    fn ino_index_len(&self) -> usize {
        self.inner.lock().by_ino.len()
    }
}

/// 移除一个键：同步回收字节、LRU 次序与 ino 索引。`&mut Inner` 自由函数，避免借用 self 与 guard 冲突。
fn remove_key(g: &mut Inner, key: (u64, u64)) {
    if let Some(node) = g.map.remove(&key) {
        g.order.remove(&node.seq);
        debug_assert!(
            g.cur_bytes >= node.bytes.len(),
            "cur_bytes 记账漂移：cur_bytes={} < node={}",
            g.cur_bytes,
            node.bytes.len()
        );
        g.cur_bytes = g.cur_bytes.saturating_sub(node.bytes.len());
        if let Some(set) = g.by_ino.get_mut(&key.0) {
            set.remove(&key.1);
            if set.is_empty() {
                g.by_ino.remove(&key.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 可注入可用内存的 fake，记录探测次数。clone 共享内部状态，便于测试事后断言。
    #[derive(Clone)]
    struct FakeMem {
        avail: Arc<Mutex<Option<u64>>>,
        calls: Arc<AtomicUsize>,
    }

    impl FakeMem {
        fn new(avail: Option<u64>) -> Self {
            Self {
                avail: Arc::new(Mutex::new(avail)),
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
        fn set(&self, v: Option<u64>) {
            *self.avail.lock() = v;
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl AvailableMemory for FakeMem {
        fn available_bytes(&self) -> Option<u64> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.avail.lock()
        }
    }

    /// 充裕内存（available=None → 直接用 configured_cap），无压力扰动，每次 insert 重探测。
    fn cache_uncapped(cap: usize) -> BlockCache {
        BlockCache::with_probe_and_interval(cap, Box::new(FakeMem::new(None)), Duration::ZERO)
    }

    fn blk(byte: u8, len: usize) -> Arc<[u8]> {
        Arc::from(vec![byte; len].into_boxed_slice())
    }

    #[test]
    fn 命中返回同字节_未命中_none() {
        let c = cache_uncapped(1 << 20);
        assert!(c.get(1, 0).is_none(), "空缓存未命中");
        let b = blk(7, 100);
        c.insert(1, 0, b.clone());
        let got = c.get(1, 0).expect("应命中");
        assert_eq!(&*got, &*b, "命中返回同字节");
        assert!(c.get(1, 1).is_none(), "他块未命中");
    }

    #[test]
    fn 字节预算_lru_逐出最旧() {
        // cap 容 2 块（每块 100B），插第 3 块应逐出最久未用。
        let c = cache_uncapped(250);
        c.insert(1, 0, blk(0, 100));
        c.insert(1, 1, blk(1, 100));
        // 触碰块 0 使其更新（块 1 变最旧）。
        assert!(c.get(1, 0).is_some());
        c.insert(1, 2, blk(2, 100)); // 超 250 → 逐出最旧（块 1）。
        assert!(c.get(1, 1).is_none(), "最旧的块 1 应被逐出");
        assert!(c.get(1, 0).is_some(), "刚触碰的块 0 应保留");
        assert!(c.get(1, 2).is_some(), "新块 2 应在");
        assert_eq!(c.cur_bytes(), 200);
        assert_eq!(c.entry_count(), 2);
    }

    #[test]
    fn invalidate_只清该ino_回收字节_他ino保留() {
        let c = cache_uncapped(1 << 20);
        c.insert(1, 0, blk(0, 100));
        c.insert(1, 1, blk(1, 100));
        c.insert(2, 0, blk(2, 100));
        assert_eq!(c.cur_bytes(), 300);
        c.invalidate(1);
        assert!(c.get(1, 0).is_none() && c.get(1, 1).is_none(), "ino=1 全清");
        assert!(c.get(2, 0).is_some(), "ino=2 保留");
        assert_eq!(c.cur_bytes(), 100, "回收 ino=1 的字节");
        assert_eq!(c.entry_count(), 1);
    }

    #[test]
    fn by_ino_索引随逐出与失效保持一致_不泄漏() {
        // 索引项数应随 map 收敛：逐出、替换、失效后 by_ino 不残留空 ino。
        let c = cache_uncapped(250); // 容 2 块。
        c.insert(7, 0, blk(0, 100));
        c.insert(7, 1, blk(1, 100));
        c.insert(8, 0, blk(2, 100)); // 逐出 ino=7 的最旧块（idx 0）。
        assert_eq!(c.entry_count(), 2);
        assert_eq!(c.ino_index_len(), 2, "ino 7、8 各有条目");
        c.invalidate(7); // 清掉 ino=7 仅剩的块。
        assert_eq!(c.ino_index_len(), 1, "ino=7 索引项随末块清除而移除");
        c.invalidate(8);
        assert_eq!(c.ino_index_len(), 0, "全清后索引不泄漏空 ino");
        assert_eq!(c.cur_bytes(), 0);
    }

    #[test]
    fn 同键重复insert_记账不漂移_无悬空seq() {
        // 评审 MEDIUM-1：同键 insert 两次（并发双读各 miss 各 insert 的串行等价），
        // cur_bytes 不得双计，order 不得残留悬空 seq。
        let c = cache_uncapped(1 << 20);
        c.insert(5, 9, blk(0, 100));
        c.insert(5, 9, blk(1, 100)); // 替换同键。
        assert_eq!(c.cur_bytes(), 100, "同键替换不双计字节");
        assert_eq!(c.entry_count(), 1);
        // 再多插若干不同键并逐出，验证无悬空 seq 致逐出 panic / 记账错。
        let c2 = cache_uncapped(250);
        c2.insert(0, 0, blk(0, 100));
        c2.insert(0, 0, blk(0, 100)); // 同键替换，旧 seq 必须从 order 删除。
        c2.insert(0, 1, blk(1, 100));
        c2.insert(0, 2, blk(2, 100)); // 触发逐出，若旧 seq 悬空会记账漂移。
        assert!(c2.cur_bytes() <= 250, "逐出后不超预算");
        assert_eq!(c2.cur_bytes() % 100, 0, "记账整齐，无漂移");
    }

    #[test]
    fn cap0_全no_op() {
        let c =
            BlockCache::with_probe_and_interval(0, Box::new(FakeMem::new(None)), Duration::ZERO);
        assert!(!c.enabled());
        c.insert(1, 0, blk(0, 100));
        assert!(c.get(1, 0).is_none(), "禁用时不缓存");
        c.invalidate(1); // 不 panic。
    }

    #[test]
    fn 单块超eff_cap_不缓存_且清旧值() {
        let c = cache_uncapped(150);
        c.insert(1, 0, blk(0, 100)); // 占 100。
        c.insert(1, 0, blk(0, 200)); // 200 > eff_cap=150 → 不缓存，且须清掉旧的 100B 值。
        assert!(c.get(1, 0).is_none(), "超块不缓存");
        assert_eq!(c.cur_bytes(), 0, "旧值被清，无残留");
    }

    #[test]
    fn 压力低_eff_cap缩到0_不缓存() {
        // available == RESERVE → budget = (RESERVE + 0 - RESERVE)/2 = 0 → eff_cap 0。
        let fake = FakeMem::new(Some(RESERVE_BYTES));
        let c = BlockCache::with_probe_and_interval(1 << 30, Box::new(fake), Duration::ZERO);
        c.insert(1, 0, blk(0, 4096));
        assert_eq!(c.eff_cap(), 0);
        assert!(c.get(1, 0).is_none(), "压力下不缓存");
    }

    #[test]
    fn 压力高_用configured_cap() {
        // available 远超 RESERVE → budget 远超 cap → 取 min = configured_cap。
        let fake = FakeMem::new(Some(RESERVE_BYTES + 100 * (1 << 30)));
        let cap = 64 * 1024;
        let c = BlockCache::with_probe_and_interval(cap, Box::new(fake), Duration::ZERO);
        c.insert(1, 0, blk(0, 1024));
        assert_eq!(c.eff_cap(), cap, "充裕内存用配置上限");
        assert!(c.get(1, 0).is_some());
    }

    #[test]
    fn 探测按间隔节流() {
        // 默认 1s 间隔：两次快速 insert 只探测一次（首次 last_probe=None 必探，第二次在窗口内跳过）。
        let fake = FakeMem::new(Some(RESERVE_BYTES + 100 * (1 << 30)));
        let c = BlockCache::with_probe_and_interval(
            1 << 20,
            Box::new(fake.clone()),
            Duration::from_secs(1),
        );
        c.insert(1, 0, blk(0, 100));
        c.insert(1, 1, blk(1, 100));
        assert_eq!(fake.calls(), 1, "1s 窗口内第二次 insert 不重探测");
    }

    #[test]
    fn 压力缩cap后逐出已缓存_含cur_bytes加回() {
        // interval=0 每次重探测：先充裕填 2 块，再把 available 降到只够 ~1 块，下次 insert 逐出。
        let fake = FakeMem::new(Some(RESERVE_BYTES + 100 * (1 << 30)));
        let c =
            BlockCache::with_probe_and_interval(1 << 30, Box::new(fake.clone()), Duration::ZERO);
        c.insert(1, 0, blk(0, 1000));
        c.insert(1, 1, blk(1, 1000));
        assert_eq!(c.cur_bytes(), 2000);
        // 降压：effective_avail = avail + cur_bytes(2000)。令 budget ≈ 1000：
        // (avail + 2000 - RESERVE)/2 = 1000 → avail = RESERVE + 0  → budget = 1000。
        fake.set(Some(RESERVE_BYTES));
        c.insert(1, 2, blk(2, 1000)); // eff_cap≈1000(+cur 反馈)；逐出至预算内。
        assert!(
            c.cur_bytes() <= 1500,
            "压力下逐出收敛，cur_bytes={}",
            c.cur_bytes()
        );
    }
}
