---
name: zipfs-reconcile-ops
description: 操作 zipfs 的停用期回落写重合并——诊断 NEEDS-RECONCILE、跑 reconcile/reconcile-undo、处理 memory split-brain。当某 Claude project 停用期被直接写进裸挂载点、remount 被守卫拒、或需回退一次 reconcile 重选时用。
---

# zipfs enable reconcile / reconcile-undo 操作

设计与内部机制见 [docs/09-session-reconcile.md](../../../docs/09-session-reconcile.md)。本 skill 只讲**怎么操作**。

## 何时需要

影子挂载**停用期**（守护死 / 维护 / 崩溃卸载）里，Claude Code 会把会话直接写进裸挂载点 underlay，与压缩 backing 分歧。症状：

- `zipfs enable list` 里该项目标 **`NEEDS-RECONCILE`**（STOPPED 且 underlay 有回落写）。
- `enable remount` 被守卫拒（"underlay 含停用期回落写…先 reconcile"）——**这是保护，不是 bug**：直接挂会静默盖住回落写。

## 铁律 / 陷阱

- **项目名前导 `-` 须用 `--` 分隔**：`enable reconcile -- -home-xp-src-foo`（不加 `--` 会被 clap 当选项）。
- **须先卸载**：reconcile/undo 只对未挂载项目有效（挂载态读挂载点是 FUSE 视图而非 underlay，会误判 / 在活挂载上改写）。已挂载会被拒。
- **策略 B——绝不自动落盘**：实跑是**逐条交互确认**，stdin 非 tty（脚本/管道）且非 `--dry-run` 会被**拒绝**。必须在真终端跑。
- **零丢失**：全程 stash-backed。冷备份 golden 在 `~/.claude/projects/<name>.zipfs-orig`；reconcile 每代次 stash 在 `~/.claude-zip/reconcile-stash/<name>/<ts>/`。

## 标准流程

```bash
cd /path/to/zipfs   # 或用已装好的 zipfs
# 1) 先 dry-run 看建议单（零改动，只读）
./target/release/zipfs enable reconcile -- <name> --dry-run
# 2) 逐条确认实跑。每条打印 推荐动作/置信度/理由：
#    a=accept（采纳推荐）  k=keep-both（都留、不动，延后）  s=skip（跳过）
#    高置信度条目【回车】即 accept；拿不准的选 s/k 延后（underlay 不动、可后续再来）
./target/release/zipfs enable reconcile -- <name>
# 3) underlay 清空后重挂 + 确认
./target/release/zipfs enable remount -- <name>
./target/release/zipfs enable list | grep <name>     # 期望 ZIPFS（Active）
```

## 逐条动作语义

| 建议动作 | 含义 |
|---|---|
| **Union**（log-only / incremental） | base 正文全保留 + incoming 日志/记录无损并入。安全，通常 accept。 |
| **KeepSeparate**（疑 session-id 重用） | base 不动；重用那段会话隔离到 `~/.claude-zip/reconcile-quarantine/<name>/<ts>/<uuid>.jsonl`（保原名，可手动取回）。两段都留。 |
| **Passthrough**（memory 外链） | memory 新文件复原到 canonical 目标（项目自己的 `docs/memory` 等），**绝不落 orig**；冲突文件改名 `<f>.underlay-<crc>` 保两份。 |
| **New** | orig 没有的新会话，直接并入。 |

`accept` = 采纳推荐；`keep-both` = 什么都不动（该条留待处理，remount 仍会因它被拒）；`skip` = 跳过。

## 重选：reconcile-undo

reconcile 后想换选择 → 回退最近一次：
```bash
./target/release/zipfs enable reconcile-undo -- <name>   # 须未挂载
# 之后可换选项重跑 enable reconcile
```
- **陈旧门**：若 reconcile 之后 Claude 又往该项目写了新内容（live 与快照不同），undo 会**整体拒绝**、报告哪些条目已变——绝不用旧快照覆盖新写。此时先想清楚：要么手动处理，要么再 `reconcile` 收编新写。
- **memory 例外**：undo **不触碰**外部 memory 目标（那是你项目的真实 git 仓，如 `~/src/<proj>/docs/memory`），只在报告里列出本次往目标写过的文件——用你自己的 `git checkout` / `git clean` 回退。
- 只回退**最近一代**；`.undone` 幂等，二次 undo 是 no-op。

## memory split-brain 注意

memory 是**透传软链**（挂载时 `projects/<name>/memory` → 项目 `docs/memory`）。停用期软链缺失时 Claude 会在 underlay 写出真实 `memory/` 目录（split-brain）。reconcile 的 Passthrough 把这些**写进你项目的真实 git 仓**。跑前建议该仓 commit/clean 干净，事后 `git diff docs/memory` 核对复原了哪些。若 underlay 的 memory 整体只是个 symlink（无 split-brain），reconcile 自动识别、零数据操作、只清冗余软链。

## 排障

- **remount 一直被拒、reconcile 也说 underlay 空**：可能有非常规残留（fifo/socket）或某条目被 `keep-both`/`skip` 留着——`enable list` 看是否仍 `NEEDS-RECONCILE`，把剩余条目处理掉。
- **list 里进行中显示 `RECONCILING`**：`.reconciling` 标记在（reconcile/undo 半改写窗口），正常；崩溃留标记会挡维护/自启，重跑 reconcile/undo 会自恢复清标记。
- **测试 backing 用 tempdir 子目录**（非 temp 根），否则 `.zipfs.lock` flock 跨测试碰撞 flaky。
