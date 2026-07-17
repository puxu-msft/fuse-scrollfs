# zipfs → scrollz 改名迁移计划（rev3 · 可转执行）

> 类型:实施计划(how)· 状态:**代码与文档阶段已完成；真实数据迁移待主会话授权执行**(rev1 needs-rework → rev2 闭合 2 Critical → rev3 补 3 Important[I-A/I-B/I-C]+3 Minor;reviewer 复核确认 0 Critical、修完转执行)。
> 决策来源:用户 2026-07-18 定名 `scrollz` + 六项分叉决策(见 §0.2)。

## 0. 定名、定位与决策台账

### 0.1 定名
- 新名 **`scrollz`**(`scroll` 会话长卷 + `z` 压缩;z 收尾不撞 ZFS;crates.io 空闲)。
- 副标题:*Transparent zstd-chunked compression for Claude Code session transcripts (and any append-only log).*

### 0.2 已决策分叉(用户 2026-07-18)
| # | 分叉 | 决策 |
|---|---|---|
| 1 | 磁盘 `.zipfs.*` 后缀 | 改 `.scrollz.*` + 一次性迁移现存产物 |
| 2 | Prometheus 指标前缀 | `zipfs_*` → `scrollz_*` |
| 3 | 文档 | 全量改名 + `git mv` 设计文档 + ADR 记一笔 |
| 4 | 仓库根目录 | **保留** `/home/xp/src/zipfs`;并**解耦 systemd** 模板对硬编码绝对路径的依赖 |
| 5 | 环境变量 | `ZIPFS_HOME`→`SCROLLZ_HOME`(留旧名兼容读+弃用提示);`CLAUDE_PROJECTS` 保留 |
| 6 | backing 家目录 | `~/.claude-zip` → `~/.local/claude-scrollz`(整树 mv + 改默认) |

## 1. 关键前提(已核实,含 rev1 错误更正)

1. **当前无 live FUSE 挂载**(仅 `fusectl`)。→ 磁盘迁移安全窗口。
2. **【rev1 更正】磁盘文件名/后缀不含 zipfs;但归档魔数含 `ZIPFS` 字节**:
   - `archive::MAGIC = *b"ZIPFSAR\x01"`(`archive/mod.rs:52`)、`SB_MAGIC = *b"ZSB2"`(`superblock.rs:12`)。`xxd` 实证 neighbors 每个 per-uuid 归档文件开头即此魔数。
   - `reader.rs:45` / `updater.rs:80` 以 `header[..8]!=MAGIC` 校验。**改魔数 = 存量 shadow 归档全部拒读 = 数据全损**。
   - → **结论修正**:改名**不重写已压缩数据**,当且仅当**冻结魔数**(见 §2.G 不可改清单)。rev1「格式不含 zipfs」措辞错误,已废。
3. 磁盘现存待迁移产物(执行时以 `find` + 明示清单精确重列):
   - shadow(neighbors,backing **外** sidecar):`~/.claude-zip/back/-home-xp-src-neighbors.zipfs.meta`、`.zipfs.lock`;备份 `~/.claude/projects/-home-xp-src-neighbors.zipfs-orig`
   - container(ghc2api-go,backing **内** sidecar):`~/.claude-zip/back/-home-xp-src-ghc2api-go/.zipfs.meta`;备份 `~/.claude/projects/-home-xp-src-ghc2api-go.zipfs-orig`
   - **`.zipfs-orig` 是用户真实原始 transcript 备份**(enable 时移到一边),零丢失硬红线。
   - **无 zipfs、不迁**:`.needs-reconcile`、`.reconcile.lock`(neighbors 现带此二者 → **处于待 reconcile 态**,重挂验证需留意,见 §4)。
4. **systemd 托管单元(rev1 完全漏,Critical#1)**:`~/.config/systemd/user/` 下有 `zipfs@.service`(模板,`autostart.rs:74` 生成)、`zipfs@<neighbors>` 实例符号链接、遗留 `zipfs-neighbors.service`。模板 ExecCondition/ExecStart/ExecStop 硬编码 `/home/xp/src/zipfs/target/release/zipfs`。
   - **【rev2 二次更正 I-C】实测**:实例 `zipfs@<neighbors>`(经 `default.target.wants/` 符号链接)= **enabled**(`is-enabled` exit 0);独立单元 `zipfs-neighbors.service` = disabled。(rev1 观察对,rev2 反向改错,现更正回。)
   - **影响**:实例 enabled → **迁移未完成前若发生登录**,systemd 拉起旧模板(硬编码 `…/src/zipfs/target/release/zipfs`)→ 档 A 改名后旧二进制不存在 → 启动失败(无损坏,但打断一个 enabled 产品托管态)。→ **§3 第 0 步 disable 实例为强制、非可选**。
   - 另:二进制改名 + `template_installed()` 查 `scrollz@.service` 会使托管挂载代码路径静默回落 `RealMounter`,旧单元成孤儿。
5. **核心格式经确认不因改名重写**(仅魔数冻结前提下)。

## 2. 改名映射

### 档 A — 构建标识(编译期可抓)
- crate/包/lib/bin `zipfs`→`scrollz`;`zipfs-bench`→`scrollz-bench`;`git mv crates/zipfs crates/scrollz`、`crates/zipfs-bench crates/scrollz-bench`;根 `Cargo.toml` members/default-members;bench 依赖键 `zipfs = { path = "../zipfs" }`→`scrollz`、各 `use zipfs::`→`use scrollz::`;`Cargo.lock`。`mkfixture` bin 不变。
- 内部标识符 `Paths.zipfs_home` 字段(`model.rs`,`p.zipfs_home` 等)→ `scrollz_home`(编译期抓)。
- **仓库根目录保留**(决策4):systemd 硬编码路径改由安装时解析(见档 F),不因保留根目录而残留旧路径依赖。

### 档 B — CLI + FUSE 标识(运行期字符串)
- 二进制名随 bin;`scrollz enable` 等子命令由 clap 继承,核对 help/about 无 `zipfs`。
- **FUSE FSName/subtype**(`main.rs:681-686`):`FSName("zipfs")`→`"scrollz"`;subtype `zipfs-passthrough/shadow/container`→`scrollz-*`。纳入 §6 重挂 `/proc/mounts` 检查点。

### 档 C — 磁盘后缀常量(运行期字符串,编译期抓不到)
`enable/model.rs`、`store/lock.rs`:`ORIG_SUFFIX .zipfs-orig`→`.scrollz-orig`、`PID_SUFFIX .zipfs.pid`→`.scrollz.pid`、`META_SUFFIX .zipfs.meta`→`.scrollz.meta`、lock `.zipfs.lock`→`.scrollz.lock`。`NEEDS_RECONCILE_SUFFIX .needs-reconcile` 不变。连带 model.rs 单测路径断言同步。
- **Minor**:`reconcile/orchestrator/routes/memory_passthrough.rs:168` 探针名 `.zipfs-memory-write-probe`→`.scrollz-memory-write-probe`。

### 档 D — Prometheus 指标前缀(运行期字符串 + 测试断言)
- `zipfs_*`(115 处字面量)→ `scrollz_*`;连带测试断言 `out.contains("zipfs_seals_total …")` 等同步(测试是此项 oracle,漏改即红)。

### 档 E — 环境变量(决策5)
- `ZIPFS_HOME`→`SCROLLZ_HOME`(`config.rs`、`model.rs:48-51`、`main.rs:233/249`、`mod.rs` 及提示文本)。
- **向后兼容**:读取顺序 `SCROLLZ_HOME` → 回落 `ZIPFS_HOME`(命中时 `tracing::warn` 弃用提示)。`CLAUDE_PROJECTS` 保留不动。
- **用户须知**:若你在 shell rc 里 `export ZIPFS_HOME`,建议改 `SCROLLZ_HOME`(旧名仍可用但会告警)。
- **footgun(决策5×决策6 交叉)**:home 从 `~/.claude-zip` 搬走后,若曾 `export ZIPFS_HOME=~/.claude-zip`,回落会指向**已搬空的旧路径** → 项目全判未迁移/Plain。**本机已核实 env 与各 rc 无 `ZIPFS_HOME`/`claude-zip`,无此风险**;须知里附「迁移后请 unset 旧 export 或改指 `~/.local/claude-scrollz`」。

### 档 F — systemd 托管(决策4;Critical#1)
- **模板解耦**:`autostart.rs` 生成 `scrollz@.service` 时,ExecCondition/ExecStart/ExecStop 的二进制路径改为**安装时解析的 exe 绝对路径**(`std::env::current_exe()`),不再硬编码仓库根。**注**:须从稳定产物(`target/release/scrollz`)执行 `autostart install`,勿从 `cargo run`/临时路径,否则会把非稳定路径烤进 ExecStart。
- **代码自改名迁移**:`autostart.rs` 补一段(比照现有 `zipfs-projects.service` 清理 `autostart.rs:113-116`):`systemctl --user disable --now` 旧 `zipfs@<esc>` 实例 → 删旧模板 `zipfs@.service` / 实例符号链接 / 遗留 `zipfs-neighbors.service`(best-effort,记 manifest)。
- `template_installed()`(`systemd.rs:240`)查 `scrollz@.service`。

### 档 G — **代码层不可改清单(compat-frozen;Critical#2)**
以下含 `zipfs`/`ZIPFS` 字节但**禁止改**(改即存量归档不可读 = 数据丢失),`rg zipfs` 收口须**显式豁免**:
- `archive::MAGIC` = `b"ZIPFSAR\x01"`(`archive/mod.rs:52`)及其上方注释(`mod.rs:51`)。
- `archive::SB_MAGIC` = `b"ZSB2"`(不含 zipfs 字面但同属魔数冻结,`superblock.rs:12`)。
- 在这两处旁加注释 `// COMPAT-FROZEN: 改字节=存量归档不可读,禁止改。品牌无关。`

### 档 H — 文档(决策3)
- `git mv docs/01-zipfs-design.md docs/01-scrollz-design.md`;docs/ 全文 `zipfs`→`scrollz`;代码注释里 `docs/01-zipfs-design.md` 路径引用同步。
- ADR 记一条改名决策(为何弃 zipfs、定 scrollz、六分叉、魔数冻结)。
- **例外**:CHANGELOG/ADR **历史叙述**中 `zipfs` 作为历史事实保留(标注「原名 zipfs」),不抹历史真相。

### 档 I — bench/scripts + 默认 backing + `claude-zip` 落点(决策6;I-A)
- `bench/scripts/*.sh`、`zipfs.service`:既有 `ln`/`zipfs` 混用 → 统一 `scrollz`;`git mv zipfs-*.sh scrollz-*.sh`;脚本内后缀 `.ln-orig`/`.zipfs.pid` 对齐 `.scrollz.*`;env `ZIPFS_BIN`/`ZIPFS_LEVEL` → `SCROLLZ_*`。
- **`claude-zip` 字符串专项**(不含 "zipfs" 子串,`rg zipfs` 抓不到,须单独列改):
  - 代码/测试(编译期抓):`model.rs:53` 默认 `home.join(".claude-zip")`→`.local/claude-scrollz`、`model.rs:37` doc 注释、`model.rs:428/439/443/447/455` 单测断言、`tests/systemd_mount.rs:91`。
  - live 文档:`README.md`、`docs/09-session-reconcile.md:93`(quarantine 路径)。
  - 历史 plan 文档(`docs/plan/*.md`):按档 H 例外——历史叙述/旧路径保留并标注,不强改;若含 live runbook 命令(如 `docs/plan/session-reconcile.md:1035` 的 `ls ~/.claude-zip/…`)则更新为新家或标注旧路径。
- 默认 backing 家目录 `model.rs:53` `~/.claude-zip`→`~/.local/claude-scrollz`。

## 3. 执行顺序(git 可提交增量)

> 执行同步（2026-07-17）：步骤 1、2、3、5、6 已完成并通过 `cargo test --release`；步骤 0、4 与 §6 的真实挂载/数据迁移验收按授权边界留给主会话。

0. **前置(迁移第 0 步)**:`systemctl --user disable --now` 现有 `zipfs@<neighbors>` 实例;声明「本次改名完成前禁止 enable/重挂」以消除中间不一致窗口。
1. **档 A 构建标识** → `cargo build --release` 绿。提交。
2. **档 D 指标 + 档 B FSName + 档 C 后缀常量 + 档 E env + 档 G 冻结注释**(纯代码,一并改)→ `cargo test --release` 绿。提交(可拆 2–3 提交)。
3. **档 F systemd 代码**(模板解耦 + 自改名迁移)→ `cargo test` 绿。提交。
4. **磁盘 + systemd + backing 家目录迁移**(见 §4:guarded 脚本、pre-image manifest、用户确认后执行)。提交脚本。
5. **档 H 文档** + `git mv` 设计文档 + ADR + 注释路径。提交。
6. **档 I bench/scripts + systemd 模板文件**归一 + `git mv`。提交。
7. **收口**:`rg -n zipfs`(排除 `target/`、档 G 冻结清单、ADR/CHANGELOG 历史叙述)= 0;合并态复审;(强化)neighbors 以 scrollz 重挂读校验。

> 代码改动(步 1–3)全程 `cargo test`(188+)兜底运行期遗漏(指标/后缀断言、model 单测)。

## 4. 磁盘迁移子方案(唯一真风险点)

**目标**:原子迁移 §1.3 sidecar/备份 + §1.4 systemd 单元 + 决策6 backing 家目录,零丢失、可回滚。

**步骤(全部在无挂载窗口)**:
1. **前置断言**:无 live 挂载(`mountpoint -q` 各点 / 无 `scrollz|zipfs` FUSE conn);否则中止。
2. **pre-image manifest**:合并三个来源落一份「旧→新」映射(既是执行输入也是回滚依据):
   - `find ~/.claude ~/.claude-zip -name '*.zipfs*'`(sidecar/备份;shadow backing 外 + container backing 内两位置)
   - `~/.config/systemd/user/{zipfs@.service,zipfs@*,zipfs-neighbors.service}`(名不匹配 `*.zipfs*`,单列)
   - backing 家目录 `~/.claude-zip` → `~/.local/claude-scrollz`(整树)
3. **执行(顺序关键,I-B):sidecar 改名必须在 home 整树 mv 之前**——sidecar `.zipfs.meta`/`.zipfs.lock` 住在 backing home **内**(`~/.claude-zip/back/…`),先 mv home 会使 manifest 里 sidecar 旧路径全失效:
   - ① systemd:disable 实例 → 删旧单元/链接(见档 F 代码迁移;脚本仅兜底)。
   - ② sidecar/备份改后缀(仍在旧 home / `~/.claude/projects` 下,manifest 路径有效):逐个原子 `mv .zipfs.* → .scrollz.*`;`.zipfs.lock` 陈旧可直接删(重挂重建);`.zipfs-orig`(在 `~/.claude/projects/`,home 外)只改名不改内容不删。
   - ③ backing 家目录整树 mv(此时其内已全是 `.scrollz.*`):断言 `[ ! -e ~/.local/claude-scrollz ]`(否则 mv 会嵌套)→ `mkdir -p ~/.local` → `mv ~/.claude-zip ~/.local/claude-scrollz`(整树原子 rename,同 FS,已核实同 `/dev/sdd`)。
4. **验证**:`find ~/.claude ~/.local/claude-scrollz -name '*.zipfs*'`=空;`ls ~/.config/systemd/user|rg zipfs`=空;`.scrollz-orig`/`.scrollz.meta` 计数==pre-image;抽验 `.scrollz-orig` 大小/首字节与迁移前一致;抽验一个 neighbors per-uuid 归档首 8 字节仍 `ZIPFSAR\x01`(证魔数未动)。
5. **回滚**:manifest 反向 `mv`(含 backing 家目录整树 mv 回、systemd 重装旧态)。脚本入库。

**授权边界**:操作 `~/.claude`/`~/.local` 内真实用户数据(`no-accidental-data-loss`)。执行前把 pre-image manifest **明示用户、确认后**再 `mv`;不无声改真实备份。

**neighbors 待 reconcile 注意**:neighbors 带 `.needs-reconcile` → 重挂验证(§6 强化)可能触发 reconcile 守卫;若触发,先按 `zipfs-reconcile-ops` 处置,不阻塞改名本身。

## 5. 风险与缓解

| # | 风险 | 缓解 |
|---|---|---|
| R1 | 迁移碰真实 transcript 备份 `.zipfs-orig` | 未挂载窗口 + 原子 mv + manifest 回滚 + 只改名不改内容 + 字节抽验 |
| R2 | **误改归档魔数致存量全损**(Critical#2) | 档 G 冻结清单 + `rg` 收口豁免 + 迁移后抽验首 8 字节 |
| R3 | systemd 孤儿 + 托管路径静默回落(Critical#1) | 档 F 代码自改名迁移 + 模板解耦 + §6 验收 `rg zipfs`=空 |
| R4 | 运行期字符串遗漏(FSName/env/探针/子命令) | 档 B/C/E/I 逐列 + `cargo test` + 收口 rg=0 + 重挂 `/proc/mounts` 检查 |
| R5 | bench 既有 `ln` 残留掩盖遗漏 | 档 I 整体归一,提交前 `rg 'ln\b'` 核误伤 |
| R6 | 历史文档被过度抹改 | 档 H 例外:历史叙述保留「原名 zipfs」 |

## 6. 验收

- `cargo build --release && cargo test --release` 全绿。
- `rg -n zipfs`(排除 `target/`、档 G 冻结清单、ADR/CHANGELOG 历史叙述)= 0。
- `rg -n 'claude-zip'`(排除历史 plan 叙述)= 0(I-A:该串不含 zipfs 子串,须独立扫)。
- `find ~/.claude ~/.local/claude-scrollz -name '*.zipfs*'`=空;`.scrollz-*` 计数==pre-image;`ls ~/.config/systemd/user|rg zipfs`=空。
- 归档首 8 字节仍 `ZIPFSAR\x01`(魔数未动)。
- (强化)neighbors 以 scrollz 重挂 + 读校验通过;`/proc/mounts` 显示 `scrollz` fsname/subtype。
- ADR 有改名决策;README 副标题更新;`scrollz@…neighbors` 托管态与迁移前对齐(或按需重 enable)。
