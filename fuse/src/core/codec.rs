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

/// 编码侧压缩参数（等级 + 可选长程匹配 LDM）。
///
/// **默认（[`CompressParams::plain`]）不开 LDM**：现有热路径 / 1MiB 块 / 8MiB 封存块行为零回归。
/// LDM 仅在封存块 > 8MiB（[`CompressParams::sealed`]）时开启：zstd-19 默认窗口 8MiB（windowLog 23）
/// 跨不出更大的块，>8MiB 距离的文件内长程重复吃不到，需 LDM + 更大 windowLog 才能逼近整流。
///
/// ## windowLog 硬 clamp ≤27（正确性红线）
/// `window_log` 由 [`CompressParams::sealed`] 取 `ceil(log2(chunk_size))` 并**硬 clamp 到
/// [`DECOMPRESS_WINDOW_LOG_MAX`]（27）**——解码器 [`decompress_block`] 设了 `window_log_max(27)`，
/// 编码 windowLog 超此值的帧解码器会拒绝（封存后解不出 = 数据损坏）。clamp 保证编 ≤ 解上限。
///
/// ## 与共享字典互斥
/// LDM 路径与字典路径互斥：seal 不用字典、热路径不用 LDM，二者不会同时正当出现。
/// `compress_block_full` 在 `enable_ldm && dict.is_some()` 时显式报错（见其文档），不静默走错分支。
#[derive(Debug, Clone, Copy)]
pub struct CompressParams {
    /// 无字典路径的 zstd 等级。
    pub level: i32,
    /// 是否启用长程匹配（LDM / `--long`）。
    pub enable_ldm: bool,
    /// LDM 开启时的 windowLog（已 clamp ≤27）；`enable_ldm == false` 时忽略。
    pub window_log: u32,
}

impl CompressParams {
    /// 普通路径：仅等级，不开 LDM（热路径 / ≤8MiB 块默认）。
    pub fn plain(level: i32) -> Self {
        Self {
            level,
            enable_ldm: false,
            window_log: 0,
        }
    }

    /// 封存路径：块 > 8MiB 时自动开 LDM，windowLog = `ceil(log2(chunk_size))` 硬 clamp ≤27；
    /// 块 ≤ 8MiB 落在 zstd 默认窗口内，等价于 [`plain`](Self::plain)（不开 LDM，零开销）。
    pub fn sealed(level: i32, chunk_size: u32) -> Self {
        // zstd-19 默认 windowLog=23（8MiB）。块 ≤8MiB 默认窗口已覆盖，无需 LDM。
        const LDM_THRESHOLD: u32 = 8 * 1024 * 1024;
        if chunk_size <= LDM_THRESHOLD {
            return Self::plain(level);
        }
        Self {
            level,
            enable_ldm: true,
            window_log: window_log_for(chunk_size),
        }
    }
}

/// 取覆盖 `chunk_size` 的最小 windowLog（`ceil(log2(chunk_size))`），**硬 clamp 到
/// [`DECOMPRESS_WINDOW_LOG_MAX`]（27）**。clamp 是正确性红线：编码 windowLog 不得超解码上限。
fn window_log_for(chunk_size: u32) -> u32 {
    debug_assert!(chunk_size > 0);
    // ceil(log2(n)) = 一个 ≥n 的最小 2 的幂的指数。
    let wl = 32 - (chunk_size.saturating_sub(1)).leading_zeros();
    // 硬 clamp ≤27（解码器 window_log_max）。也防 chunk_size==1 时 wl==0 这种边角（无实际意义但安全）。
    wl.min(DECOMPRESS_WINDOW_LOG_MAX)
}

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
    compress_block_full(raw, algo, &CompressParams::plain(level), dict)
}

/// 用显式 [`CompressParams`] 压缩（无字典），暴露 LDM。封存大块经此开 LDM。
///
/// 现有 [`compress`]/[`compress_block`] 仍走 `CompressParams::plain`（不开 LDM），行为零回归。
pub fn compress_with_params(
    raw: &[u8],
    algo: Algo,
    params: &CompressParams,
) -> io::Result<(Vec<u8>, bool)> {
    compress_block_full(raw, algo, params, None)
}

/// 压缩内核：统一处理 zstd 等级 / LDM / 字典三条路径与不可压缩启发式。
///
/// **LDM 与字典互斥**：`params.enable_ldm && dict.is_some()` 时显式返回 `InvalidInput`——
/// seal 路径不用字典、热路径不用 LDM，二者不会同时正当出现；同传说明调用方逻辑错乱，
/// 绝不静默走某一条分支产出"看似成功"的错配帧。
pub fn compress_block_full(
    raw: &[u8],
    algo: Algo,
    params: &CompressParams,
    dict: Option<&SharedDict>,
) -> io::Result<(Vec<u8>, bool)> {
    if raw.is_empty() {
        return Ok((Vec::new(), true));
    }
    if params.enable_ldm && dict.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "LDM 与共享字典互斥：seal 不用字典、热路径不用 LDM，二者不可同传",
        ));
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
                None if params.enable_ldm => compress_ldm(raw, params)?,
                None => zstd::stream::encode_all(raw, params.level)
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

/// LDM 路径：bulk Compressor + `EnableLongDistanceMatching` + `WindowLog`（已 clamp ≤27）。
///
/// windowLog 取自 `params.window_log`（[`CompressParams::sealed`] 已硬 clamp 到
/// [`DECOMPRESS_WINDOW_LOG_MAX`]）；这里再 `debug_assert` 一道，确保任何调用方传入的 window_log
/// 都不超解码上限——超了的帧解码器会拒绝 = 封存后解不出 = 损坏。
fn compress_ldm(raw: &[u8], params: &CompressParams) -> io::Result<Vec<u8>> {
    use zstd::zstd_safe::CParameter;
    debug_assert!(
        params.window_log <= DECOMPRESS_WINDOW_LOG_MAX,
        "编码 windowLog {} 超解码上限 {DECOMPRESS_WINDOW_LOG_MAX}（会损坏）",
        params.window_log
    );
    let mut c = zstd::bulk::Compressor::new(params.level)
        .map_err(|e| io::Error::other(format!("zstd LDM 压缩器构造失败：{e}")))?;
    c.set_parameter(CParameter::EnableLongDistanceMatching(true))
        .map_err(|e| io::Error::other(format!("启用 LDM 失败：{e}")))?;
    c.set_parameter(CParameter::WindowLog(params.window_log))
        .map_err(|e| io::Error::other(format!("设 LDM windowLog 失败：{e}")))?;
    c.compress(raw)
        .map_err(|e| io::Error::other(format!("zstd LDM 压缩失败：{e}")))
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
    fn decompression_bomb_oversize_block_rejected() {
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
    fn zstd_compresses_compressible_data_round_trip() {
        // 高度可压缩：重复字节。
        let raw = vec![b'a'; 64 * 1024];
        let (stored, verbatim) = compress(&raw, Algo::Zstd, 3).unwrap();
        assert!(!verbatim, "高度可压缩数据不应触发 verbatim");
        assert!(stored.len() < raw.len(), "压缩应显著缩小");
        let back = decompress(&stored, Algo::Zstd, verbatim).unwrap();
        assert_eq!(back, raw, "解压必须 round-trip 一致");
    }

    #[test]
    fn incompressible_data_triggers_verbatim_flag() {
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
    fn algo_none_always_verbatim() {
        let raw = vec![b'x'; 1000];
        let (stored, verbatim) = compress(&raw, Algo::None, 0).unwrap();
        assert!(verbatim);
        assert_eq!(stored, raw);
        assert_eq!(decompress(&stored, Algo::None, verbatim).unwrap(), raw);
    }

    #[test]
    fn empty_block_round_trip() {
        let (stored, verbatim) = compress(&[], Algo::Zstd, 3).unwrap();
        assert!(verbatim);
        assert!(stored.is_empty());
        assert_eq!(
            decompress(&stored, Algo::Zstd, verbatim).unwrap(),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn incompressible_threshold_integer_boundary() {
        // clen == raw*0.95 正好触发（>=）。
        assert!(is_incompressible(100, 95));
        // clen 略低于阈值则不触发。
        assert!(!is_incompressible(100, 94));
        // 压缩到一半显然可压缩。
        assert!(!is_incompressible(1000, 500));
    }

    #[test]
    fn lz4_returns_unsupported() {
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
    fn dictionary_round_trip_consistent() {
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
    fn dictionary_saves_more_than_no_dict_on_high_redundancy_small_block() {
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
    fn empty_dictionary_returns_none() {
        assert!(SharedDict::new(Vec::new(), 3).is_none(), "空字典不应构造");
    }

    /// 构造一个含 >8MiB 距离自重复的样本：两段相同的伪随机块，中间塞不可压缩填充，
    /// 使两段相同内容相隔超过 zstd-19 默认窗口（8MiB）。无 LDM 时第二段引用不到第一段；
    /// 开 LDM + 大 windowLog 才能跨窗口命中，压缩显著更小。
    fn long_range_dup_sample() -> Vec<u8> {
        // 4MiB 伪随机段（不可压缩，确定性 LCG）。
        let seg_len = 4 * 1024 * 1024;
        let mut seg = Vec::with_capacity(seg_len);
        let mut x: u64 = 0xdead_beef_1234_5678;
        for _ in 0..seg_len {
            x = x
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            seg.push((x >> 56) as u8);
        }
        // 10MiB 不可压缩填充（另一条 LCG 流），把两段 seg 推到 >8MiB 距离。
        let fill_len = 10 * 1024 * 1024;
        let mut fill = Vec::with_capacity(fill_len);
        let mut y: u64 = 0x0f0f_0f0f_a5a5_a5a5;
        for _ in 0..fill_len {
            y = y
                .wrapping_mul(2_862_933_555_777_941_757)
                .wrapping_add(3_037_000_493);
            fill.push((y >> 56) as u8);
        }
        // 布局：seg | fill | seg  → 两个 seg 相隔 ~14MiB > 8MiB 默认窗口。
        let mut out = Vec::with_capacity(seg_len * 2 + fill_len);
        out.extend_from_slice(&seg);
        out.extend_from_slice(&fill);
        out.extend_from_slice(&seg);
        out
    }

    #[test]
    fn ldm_saves_significantly_on_beyond_window_long_range_repeats() {
        // RED→GREEN：>8MiB 距离的自重复，开 LDM 应显著小于不开 LDM。
        let raw = long_range_dup_sample();
        let chunk = raw.len() as u32;

        let (no_ldm, _) =
            compress_with_params(&raw, Algo::Zstd, &CompressParams::plain(19)).unwrap();
        let (with_ldm, _) =
            compress_with_params(&raw, Algo::Zstd, &CompressParams::sealed(19, chunk)).unwrap();

        // 第二段 4MiB 应几乎被 LDM 整段消除；保守断言至少省 20%。
        assert!(
            (with_ldm.len() as f64) < (no_ldm.len() as f64) * 0.80,
            "LDM 应显著更省：no_ldm={} with_ldm={}",
            no_ldm.len(),
            with_ldm.len()
        );
    }

    #[test]
    fn ldm_large_block_round_trip_byte_for_byte_consistent() {
        // 正确性红线：LDM + 大 windowLog 压缩的块必须能被 decompress_block 逐字节解回。
        let raw = long_range_dup_sample();
        let chunk = raw.len() as u32;
        let (stored, verbatim) =
            compress_with_params(&raw, Algo::Zstd, &CompressParams::sealed(19, chunk)).unwrap();
        assert!(!verbatim, "高冗余大块不应触发 verbatim");
        let back = decompress_block(&stored, Algo::Zstd, verbatim, None).unwrap();
        assert_eq!(back, raw, "LDM 大块解压必须逐字节 round-trip");
    }

    #[test]
    fn windowlog_hard_clamp_not_exceeding_27() {
        // 即便块大小 > 128MiB（windowLog 28+），编码 windowLog 必须 clamp 到 ≤27（=解码上限），
        // 否则封存后帧窗口超解码器 window_log_max → 解不出 = 损坏。
        // 直接验证：用一个 >128MiB 声明的 chunk_size 构造 params，对一个可解的小样本压缩→解压成功。
        let params = CompressParams::sealed(19, 256 * 1024 * 1024);
        assert!(
            params.window_log <= DECOMPRESS_WINDOW_LOG_MAX,
            "windowLog 必须 clamp ≤27"
        );
        // round-trip 一个含长程重复的样本，确保 clamp 后帧仍可解码。
        let raw = long_range_dup_sample();
        let (stored, verbatim) = compress_with_params(&raw, Algo::Zstd, &params).unwrap();
        let back = decompress_block(&stored, Algo::Zstd, verbatim, None).unwrap();
        assert_eq!(back, raw, "clamp 后帧必须仍可解码");
    }

    #[test]
    fn ldm_mutually_exclusive_with_dictionary_explicitly_rejected() {
        // 安全：LDM 与共享字典路径互斥（seal 不用字典，热路径不用 LDM），同传必须显式拒绝而非静默走错。
        let samples = boilerplate_samples(16);
        let dict = SharedDict::new(train_dict(&samples, 8 * 1024).unwrap(), 3).unwrap();
        let params = CompressParams {
            level: 19,
            enable_ldm: true,
            window_log: 25,
        };
        let err = compress_block_full(b"hello world", Algo::Zstd, &params, Some(&dict));
        assert!(err.is_err(), "LDM + 字典同传必须报错");
    }

    #[test]
    fn dict_frame_decompress_without_dict_must_error_not_silent_garbage() {
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
