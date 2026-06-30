//! 压缩 codec + 不可压缩启发式（P1：zstd；lz4 留 TODO）。
//!
//! 设计见 docs/01-zipfs-design.md §3。每块独立压缩，记录压缩后长度；若
//! `clen >= raw * 阈值`（如 0.95）则原样存 + 置 flag，省解压成本并避免膨胀
//! （对齐 btrfs 行为）。压缩是 Core 的职责，Store 只搬运不透明字节（§2、§5）。
//!
//! ## 共享字典（T3「共享字典压缩」，研究性）
//! 目标负载 `~/.claude/projects` 的高冗余是**文件内长程自重复**（系统提示 / CLAUDE.md /
//! 工具 schema / 重读文件逐轮重录），相隔 ≫ 单块。64KiB 独立块压缩看不到它（实测仅 6x，
//! 而整流 18–21x）。**共享字典**把这些 boilerplate 常驻为「永久窗口」，让每个小块都能引用，
//! 实测 64KiB + 512K 字典 + zstd-19 → 16x，逼近 1MiB 大块（19x）却保留小块（append/RMW
//! 友好、无 redb 大 blob 膨胀）。字典经 `SharedDict` 预消化（CDict/DDict）一次，跨全部块复用。

use std::io;
use std::sync::Arc;

/// 单块解压输出上限（256 MiB），防解压炸弹 OOM（评审 H2）。
///
/// 诚实数据下单块 ≤ chunk_size（默认 1MiB、封存 8MiB、上限远低于此）；恶意/损坏块声明的解压后
/// 大小不受块内压缩字节约束，可炸成任意大。per-block CRC 挡随机翻转但不挡蓄意篡改（CRC 可重算），
/// 故再加一道输出上限把"坏块"从 OOM 降级为 `InvalidData`。256 MiB 远高于任何合理 chunk_size。
const MAX_DECOMPRESSED_BLOCK: usize = 256 * 1024 * 1024;

/// 解压 window_log 上限（27 = 128 MiB 窗口）：限制解码器内部窗口缓冲分配，挡"巨窗口"帧 DoS。
const DECOMPRESS_WINDOW_LOG_MAX: u32 = 27;

/// 压缩算法选择。`--algo` 切换，见 §13 已定项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algo {
    /// 原样存储（不压缩）—— 等价于全程 verbatim。
    None,
    /// zstd，带等级。
    Zstd,
    /// lz4_flex 速度对照（P1 暂未实现，留 TODO）。
    Lz4,
}

/// 预消化的共享 zstd 字典：原始字节 + 编码/解码侧 CDict/DDict（消化一次，跨全部块复用）。
///
/// `SharedDict` 持有不可变的已消化字典，Send+Sync 安全；压缩/解压时从中临时构造
/// `Compressor`/`Decoder`（仅引用 CDict/DDict，不重新消化）。原始字节 `raw` 留作持久化
/// （字典必须随数据一起存：解压每块都要它，见 docs 优化分析「字典工程化代价」）。
pub struct SharedDict {
    raw: Vec<u8>,
    enc: zstd::dict::EncoderDictionary<'static>,
    dec: zstd::dict::DecoderDictionary<'static>,
}

impl SharedDict {
    /// 从原始字典字节构造。`level` 决定编码侧 CDict 的压缩参数（须与挂载等级一致）。
    /// 空字典返回 None（无字典等价于走无字典路径，避免无意义的空 CDict）。
    pub fn new(raw: Vec<u8>, level: i32) -> Option<Arc<Self>> {
        if raw.is_empty() {
            return None;
        }
        let enc = zstd::dict::EncoderDictionary::copy(&raw, level);
        let dec = zstd::dict::DecoderDictionary::copy(&raw);
        Some(Arc::new(Self { raw, enc, dec }))
    }

    /// 原始字典字节（持久化 / 校验用）。
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }
}

impl std::fmt::Debug for SharedDict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedDict")
            .field("dict_bytes", &self.raw.len())
            .finish()
    }
}

/// 用样本训练一个 zstd 字典，返回原始字典字节。
///
/// `samples` 是若干样本块（如 transcript 的 64KiB 切块或整文件）；`max_size` 是字典目标
/// 上限（实测 512K 优于 112K）。样本太少 / 太同质时 zstd 可能训不出有效字典而报错，向上传递。
pub fn train_dict(samples: &[Vec<u8>], max_size: usize) -> io::Result<Vec<u8>> {
    let refs: Vec<&[u8]> = samples.iter().map(|s| s.as_slice()).collect();
    zstd::dict::from_samples(&refs, max_size)
        .map_err(|e| io::Error::other(format!("zstd 字典训练失败：{e}")))
}

/// 不可压缩启发式阈值：压缩后 >= 原始 * 该比例则放弃压缩，原样存储。
pub const INCOMPRESSIBLE_RATIO: f64 = 0.95;

/// 压缩一个逻辑块（无字典）。返回 `(存储字节, stored_verbatim)`。
///
/// `stored_verbatim == true` 表示返回的就是原始字节（要么 `algo==None`，要么
/// 触发了不可压缩启发式）。调用方据此在 archive 块 flags 里置位，读时跳过解压。
///
/// 空块直接原样返回（压缩空数据无意义，且能避免下游解压空流的边角）。
pub fn compress(raw: &[u8], algo: Algo, level: i32) -> io::Result<(Vec<u8>, bool)> {
    compress_block(raw, algo, level, None)
}

/// 压缩一个逻辑块，可选共享字典。`dict=Some` 时走字典路径（CDict 已含等级，`level` 被忽略）。
pub fn compress_block(
    raw: &[u8],
    algo: Algo,
    level: i32,
    dict: Option<&SharedDict>,
) -> io::Result<(Vec<u8>, bool)> {
    if raw.is_empty() {
        return Ok((Vec::new(), true));
    }
    match algo {
        Algo::None => Ok((raw.to_vec(), true)),
        Algo::Lz4 => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "lz4 codec 尚未实现（P1 仅 zstd），见 docs/01-zipfs-design.md §3",
        )),
        Algo::Zstd => {
            let compressed = match dict {
                // 字典路径：从预消化 CDict 临时构造 Compressor（仅引用 CDict，不重新消化）。
                Some(d) => {
                    let mut c = zstd::bulk::Compressor::with_prepared_dictionary(&d.enc)
                        .map_err(|e| io::Error::other(format!("zstd 字典压缩器构造失败：{e}")))?;
                    c.compress(raw)
                        .map_err(|e| io::Error::other(format!("zstd 字典压缩失败：{e}")))?
                }
                None => zstd::stream::encode_all(raw, level)
                    .map_err(|e| io::Error::other(format!("zstd 压缩失败：{e}")))?,
            };
            // 不可压缩启发式：压缩没省下足够空间就原样存，避免「解压成本 + 轻微膨胀」双输。
            if is_incompressible(raw.len(), compressed.len()) {
                Ok((raw.to_vec(), true))
            } else {
                Ok((compressed, false))
            }
        }
    }
}

/// 解压一个逻辑块（无字典）。`stored_verbatim` 为真时返回的就是原字节（不解压）。
pub fn decompress(stored: &[u8], algo: Algo, stored_verbatim: bool) -> io::Result<Vec<u8>> {
    decompress_block(stored, algo, stored_verbatim, None)
}

/// 解压一个逻辑块，可选共享字典。压缩时用了字典则解压必须传入**同一**字典，否则报错。
pub fn decompress_block(
    stored: &[u8],
    algo: Algo,
    stored_verbatim: bool,
    dict: Option<&SharedDict>,
) -> io::Result<Vec<u8>> {
    if stored_verbatim || algo == Algo::None {
        return Ok(stored.to_vec());
    }
    match algo {
        Algo::Zstd => match dict {
            // 字典路径：streaming Decoder + 预消化 DDict（不重新消化字典）。
            Some(d) => {
                let mut dec = zstd::stream::read::Decoder::with_prepared_dictionary(stored, &d.dec)
                    .map_err(|e| io::Error::other(format!("zstd 字典解压器构造失败：{e}")))?;
                dec.window_log_max(DECOMPRESS_WINDOW_LOG_MAX)
                    .map_err(|e| io::Error::other(format!("设 window_log_max 失败：{e}")))?;
                decode_capped(dec)
            }
            None => {
                let mut dec = zstd::stream::read::Decoder::new(stored).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("zstd 解压器构造失败：{e}"),
                    )
                })?;
                dec.window_log_max(DECOMPRESS_WINDOW_LOG_MAX)
                    .map_err(|e| io::Error::other(format!("设 window_log_max 失败：{e}")))?;
                decode_capped(dec)
            }
        },
        Algo::Lz4 => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "lz4 codec 尚未实现（P1 仅 zstd）",
        )),
        // verbatim 已在上面短路；走到这说明 algo==None 但 stored_verbatim==false，
        // 仍按原样返回（None 不压缩）。
        Algo::None => Ok(stored.to_vec()),
    }
}

/// 从解码器读出全部明文，但在 [`MAX_DECOMPRESSED_BLOCK`] 处封顶——超限即判定解压炸弹/损坏。
fn decode_capped(mut dec: impl io::Read) -> io::Result<Vec<u8>> {
    use io::Read;
    let mut out = Vec::new();
    // 至多读 cap+1 字节：若真有 cap+1 字节，说明解压输出超限。
    dec.by_ref()
        .take(MAX_DECOMPRESSED_BLOCK as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("zstd 解压失败：{e}")))?;
    if out.len() > MAX_DECOMPRESSED_BLOCK {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "解压输出超上限，疑似解压炸弹或损坏块",
        ));
    }
    Ok(out)
}

/// 不可压缩判定：`clen >= raw * INCOMPRESSIBLE_RATIO`。
///
/// 用整数比较避免浮点抖动：`clen * 100 >= raw * 95`（阈值 0.95 化为整数）。
fn is_incompressible(raw_len: usize, clen: usize) -> bool {
    // INCOMPRESSIBLE_RATIO 当前为 0.95；以 *100 / *95 表达，避免浮点边界误判。
    debug_assert!((INCOMPRESSIBLE_RATIO - 0.95).abs() < f64::EPSILON);
    (clen as u128) * 100 >= (raw_len as u128) * 95
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解压炸弹_超上限块被拒() {
        // 评审 H2：构造一个解压后远超 MAX_DECOMPRESSED_BLOCK 的合法 zstd 帧（全零高压缩比），
        // decompress_block 必须返回 InvalidData 而非 OOM。诚实块（≤ chunk_size）不受影响。
        let huge = vec![0u8; MAX_DECOMPRESSED_BLOCK + 1024];
        let stored = zstd::stream::encode_all(&huge[..], 3).unwrap();
        let res = decompress_block(&stored, Algo::Zstd, false, None);
        assert!(
            res.as_ref().map_err(|e| e.kind()) == Err(io::ErrorKind::InvalidData),
            "超上限解压应 InvalidData，实际：{:?}",
            res.map(|v| v.len())
        );
        // 上限内的诚实块仍正常 round-trip。
        let ok = vec![7u8; 1024 * 1024];
        let s = zstd::stream::encode_all(&ok[..], 3).unwrap();
        assert_eq!(decompress_block(&s, Algo::Zstd, false, None).unwrap(), ok);
    }

    #[test]
    fn zstd_压缩可压缩数据_round_trip() {
        // 高度可压缩：重复字节。
        let raw = vec![b'a'; 64 * 1024];
        let (stored, verbatim) = compress(&raw, Algo::Zstd, 3).unwrap();
        assert!(!verbatim, "高度可压缩数据不应触发 verbatim");
        assert!(stored.len() < raw.len(), "压缩应显著缩小");
        let back = decompress(&stored, Algo::Zstd, verbatim).unwrap();
        assert_eq!(back, raw, "解压必须 round-trip 一致");
    }

    #[test]
    fn 不可压缩数据触发_verbatim_flag() {
        // 伪随机不可压缩数据（线性同余，确定性，避免依赖 rand）。
        let mut raw = Vec::with_capacity(4096);
        let mut x: u32 = 0x1234_5678;
        for _ in 0..4096 {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            raw.push((x >> 24) as u8);
        }
        let (stored, verbatim) = compress(&raw, Algo::Zstd, 3).unwrap();
        assert!(verbatim, "不可压缩数据应触发 verbatim 原样存储");
        assert_eq!(stored, raw, "verbatim 时存储字节即原始字节");
        let back = decompress(&stored, Algo::Zstd, verbatim).unwrap();
        assert_eq!(back, raw, "verbatim 解压仍 round-trip");
    }

    #[test]
    fn algo_none_总是_verbatim() {
        let raw = vec![b'x'; 1000];
        let (stored, verbatim) = compress(&raw, Algo::None, 0).unwrap();
        assert!(verbatim);
        assert_eq!(stored, raw);
        assert_eq!(decompress(&stored, Algo::None, verbatim).unwrap(), raw);
    }

    #[test]
    fn 空块_round_trip() {
        let (stored, verbatim) = compress(&[], Algo::Zstd, 3).unwrap();
        assert!(verbatim);
        assert!(stored.is_empty());
        assert_eq!(
            decompress(&stored, Algo::Zstd, verbatim).unwrap(),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn 不可压缩阈值_整数边界() {
        // clen == raw*0.95 正好触发（>=）。
        assert!(is_incompressible(100, 95));
        // clen 略低于阈值则不触发。
        assert!(!is_incompressible(100, 94));
        // 压缩到一半显然可压缩。
        assert!(!is_incompressible(1000, 500));
    }

    #[test]
    fn lz4_返回_unsupported() {
        let err = compress(b"hello", Algo::Lz4, 0).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }

    /// 生成一批共享 boilerplate + 每块独有尾巴的样本，模拟 transcript 的「每行重录系统提示」。
    fn boilerplate_samples(n: usize) -> Vec<Vec<u8>> {
        let boiler = b"SYSTEM-PROMPT: you are a helpful assistant. CLAUDE.md rules: no-hard-wrap, user-first, test-driven. tool schema: {read,write,bash,grep}. ".repeat(8);
        (0..n)
            .map(|i| {
                let mut s = boiler.clone();
                s.extend_from_slice(
                    format!("\nEVENT {i}: unique tail payload number {i}\n").as_bytes(),
                );
                s
            })
            .collect()
    }

    #[test]
    fn 字典_round_trip_一致() {
        let samples = boilerplate_samples(64);
        let dict_bytes = train_dict(&samples, 16 * 1024).unwrap();
        let dict = SharedDict::new(dict_bytes, 3).expect("非空字典");
        // 用字典压一个含 boilerplate 的块，再用同字典解压，必须一致。
        let raw = samples[0].clone();
        let (stored, verbatim) = compress_block(&raw, Algo::Zstd, 3, Some(&dict)).unwrap();
        let back = decompress_block(&stored, Algo::Zstd, verbatim, Some(&dict)).unwrap();
        assert_eq!(back, raw, "字典压缩/解压必须 round-trip 一致");
    }

    #[test]
    fn 字典对高冗余小块比无字典更省() {
        // boilerplate 占主体的小块：字典把 boilerplate 当「永久窗口」，应显著优于无字典独立压缩。
        let samples = boilerplate_samples(64);
        let dict_bytes = train_dict(&samples, 16 * 1024).unwrap();
        let dict = SharedDict::new(dict_bytes, 3).unwrap();
        let raw = samples[7].clone();
        let (no_dict, _) = compress_block(&raw, Algo::Zstd, 3, None).unwrap();
        let (with_dict, _) = compress_block(&raw, Algo::Zstd, 3, Some(&dict)).unwrap();
        assert!(
            with_dict.len() < no_dict.len(),
            "字典应让高冗余小块更省：no_dict={} with_dict={}",
            no_dict.len(),
            with_dict.len()
        );
    }

    #[test]
    fn 空字典返回_none() {
        assert!(SharedDict::new(Vec::new(), 3).is_none(), "空字典不应构造");
    }

    #[test]
    fn 字典帧_无字典解压_必须报错而非静默脏数据() {
        // 安全属性：用字典压的块，解压若忘了传字典，zstd 帧带 dictID 找不到字典 → 必须 Err，
        // 绝不静默返回脏数据（错误显式处理）。反向（无字典帧 + 带字典解压）zstd 会正常解出原文，
        // 字典未被引用，是安全的，不在此断言。
        let samples = boilerplate_samples(32);
        let dict = SharedDict::new(train_dict(&samples, 8 * 1024).unwrap(), 3).unwrap();
        let raw = samples[3].clone();
        let (stored, verbatim) = compress_block(&raw, Algo::Zstd, 3, Some(&dict)).unwrap();
        assert!(!verbatim, "高冗余样本不应触发 verbatim");
        let err = decompress_block(&stored, Algo::Zstd, verbatim, None);
        assert!(
            err.is_err(),
            "字典压缩的块用无字典解压必须报错，实际得到 {:?}",
            err.map(|v| v.len())
        );
    }
}
