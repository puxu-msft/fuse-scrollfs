# zipfs 配置参考 / CONFIG

> **本文回答「有哪些配置项、默认值、怎么设」**——集中收敛散落在 CLI 参数、`enable config` 文件、编译期默认常量、环境变量四处的配置面。
>
> 权威源是代码（`crates/zipfs/src/enable/{config,model}.rs`、`main.rs`、各 `DEFAULT_*` 常量）；本文是人读汇总，代码变更时同步更新。

## 1. 配置来源与优先级

| 来源 | 位置 | 作用域 |
|---|---|---|
| CLI 参数 | `zipfs <cmd> --<flag>` | 单次命令，最高优先 |
| enable config 文件 | `$ZIPFS_HOME/config`（默认 `<projects_root>/zip/config`），`key=value` 每行 | `enable apply` 的持久默认 |
| 编译期默认 | `DEFAULT_*` 常量 | 兜底 |
| 环境变量 | `ZIPFS_HOME` | 覆盖 zipfs home 根 |

管理：`zipfs enable config show` 打印当前生效值 + 文件路径；`zipfs enable config set <key> <value>` 写入（未知键 fail-closed；含换行/回车的值拒绝，防注入伪造配置行）。

## 2. enable config 键（= `ApplyOptions` 字段）

`enable apply` 切换目录时的持久选项。键列表见 `config.rs::KEYS`。

| 键 | 类型 | 默认 | 含义 |
|---|---|---|---|
| `backend` | `shadow` \| `container` | `shadow`（`model.rs` `Backend::Shadow`） | 后端布局（S=影子树 / V=容器） |
| `chunk_size` | 字节 | `1048576`（1MiB，`DEFAULT_CHUNK_SIZE`） | 逻辑块大小，见 [ADR.md](./ADR.md) D8 |
| `level` | i32 | `3`（`DEFAULT_ZSTD_LEVEL`） | zstd 等级（1/3/9/19） |
| `dict` | 路径 \| 空 | 空（关） | 共享 zstd 字典（`zipfs train-dict` 产出），见 [ADR.md](./ADR.md) D11 |
| `threads` | usize | `0`（=CPU 数，下限 4） | FUSE 工作线程数 |
| `writeback` | bool | `false` | FUSE 写回缓存（合并小写、降写尾 p99） |
| `max_write` | 字节 | `0`（=内核默认 128KiB） | 协商最大单次 write，调大减大行 append 拆分 |
| `no_tail_buffer` | bool | `false` | 关闭未压缩开放尾块缓冲（仅基准对照用） |
| `allow_other` | bool | `false` | allow_other（需 `/etc/fuse.conf` 放行） |
| `auto_unmount` | bool | `false` | 进程退出自动卸载 |
| `metrics_file` | 路径 \| 空 | 空 | Prometheus textfile 指标输出（.prom），见 [08-observability.md](./08-observability.md) |

## 3. 其他命令的关键参数

| 命令 | 参数 | 默认 | 说明 |
|---|---|---|---|
| `zipfs umount` / `umount-managed` | `--level` | `auto` | 分档卸载 clean/lazy/abort/auto，见 [07-hangfree-umount.md](./07-hangfree-umount.md) |
| `zipfs seal` | `--seal-chunk` | `8388608`（8MiB，`DEFAULT_SEAL_CHUNK`） | 封存块大小；`>8MiB` 时 opt-in LDM，见 [ADR.md](./ADR.md) D10 |
| `zipfs seal` | `--level` | `19`（`DEFAULT_SEAL_LEVEL`） | 封存 zstd 等级 |
| 挂载/守护 | `--cache-bytes` | `134217728`（128MiB，`DEFAULT_CACHE_BYTES`） | 解压块缓存上限，见 [DESIGN.md](./DESIGN.md) §1 |

> 完整命令与子命令清单（`enable` 的 list/apply/restore/remount/status/purge/autostart 等）以 `zipfs --help` / `zipfs enable --help` 为准（权威）。enable TUI 用法与设计背景见 [plan/enable-tool.md](./plan/enable-tool.md)（非稳态计划文档，仅作背景）。

## 4. 环境变量

| 变量 | 含义 |
|---|---|
| `ZIPFS_HOME` | 覆盖 zipfs home 根（config 文件、backing 等所在），默认派生自 projects root |
