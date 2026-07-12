# 文档布局对齐迁移方案（已执行）

> 目标：把 zipfs 现有文档布局全面对齐更新后的用户级指令 `70-project-doc-mgmt.md` 推荐的扁平根布局与 ADR/ARCH/DESIGN/TRACKING/BACKLOG 职责划分。
> 状态：**已执行**（用户选定「字面全面对齐 + 本次一并抽 CONFIG」）。两轮 subagent 评审纪要见 §4b。
> 执行结果：`decisions.md→ADR.md`、`plans/→plan/`、新建 `ARCH.md`/`DESIGN.md`/`TRACKING.md`/`BACKLOG.md`/`CONFIG.md`、`README.md` 索引重写、`ROADMAP.md` Stretch 段收敛。ARCH/DESIGN 写实质内容非索引壳（吸收评审一致意见）。
> 日期：2026-07-11。

## 1. 指令推荐的目标布局（扁平根）

```
docs/
  README.md      # 索引
  ARCH.md        # 当前架构视图（是什么/在哪里）：组件、边界、数据流、技术栈
  DESIGN.md      # 跨模块内部设计（怎么做）：算法、数据模型、内部契约
  ADR.md         # 架构决策记录（为什么）
  CHANGELOG.md   # 变更日志
  ROADMAP.md     # 未来里程碑、重大缺失、关键问题
  TRACKING.md    # 进行中工作跟踪（当前特性、跨会话 WIP）  ← 新增
  BACKLOG.md     # 推迟项（可选特性、非关键 bug 等）        ← 新增
  CONFIG.md      # 配置参考（可选）
  GUIDE.md       # 用户指南（可选）
  <topic>.md     # 领域专题知识
  archive/       # 归档
  plan/          # 计划（单数）
```

三分职责：ADR=为什么（仅随用户决策产生，改动需用户同意）；ARCH=是什么/在哪里；DESIGN=怎么做。

## 2. 现状盘点

| 现有文件 | 现职责 | 类型 |
|---|---|---|
| `ADR.md` | 决策台账（决定了什么/为什么/是否还算数，含被推翻项） | ≈ ADR |
| `ROADMAP.md` | T0–T4 优先级 + 开放决策门 G1–G3 | ROADMAP |
| `CHANGELOG.md` | 建成了什么、何时 | CHANGELOG |
| `00-overview.md` | 前期两路线对照总纲（2026-06-27 冻结快照） | 历史快照 |
| `01-zipfs-design.md` | 核心实现设计（两布局/Store 接缝/分块内核/模块布局） | ARCH+DESIGN 混合 |
| `02-layered-chunking.md` | 分层分块/head 缓存设计草案 | DESIGN 专题 |
| `03-target-data-scope.md` | 目标数据范围决策 | 范围决策 |
| `04-crash-safe-commit.md` | 崩溃安全提交协议 spec | DESIGN 专题 |
| `05-fault-injection-testing.md` | 故障注入测试规格 | DESIGN/test 专题 |
| `07-hangfree-umount.md` | 分档卸载特性设计 | DESIGN 专题 |
| `08-observability.md` | 可观测性/指标 | DESIGN 专题 + 部分 CONFIG |
| `09-session-reconcile.md` | 会话回落写重合并特性设计 | DESIGN 专题 |
| `environment-snapshot.md` | 实测环境事实 | 专题参考 |
| `plans/` | 14 个实施计划 + README 索引 | plan（复数目录名） |
| `archive/` | 06-defect-audit + reconcile 评审快照 | archive |

## 3. 迁移映射（提案）

设计原则：**低 churn、尊重既有编号资产、只做职责明确的收敛**。编号专题文档（00–09）是稳定的冻结/半冻结快照，逐一改名重写代价高、破坏大量 git 历史与交叉引用，收益低。因此采用「**新增顶层职责文档 + 保留编号专题**」的混合策略，而非推倒编号制。

| 动作 | 从 | 到 | 说明 |
|---|---|---|---|
| **重命名** | `ADR.md` | `ADR.md` | 职责已等价（决策台账=ADR）。指令用 ADR 命名，对齐。`git mv` 保历史 |
| **新建** | — | `ARCH.md` | 抽取 `01-zipfs-design.md` §2 分层架构 / §5 Store 接缝 / §11 模块布局，形成「当前架构视图」骨架页，其余细节链接回编号专题。**不复制正文，做导航 + 骨架** |
| **新建** | — | `DESIGN.md` | 作为跨模块内部设计的**索引页**，串起 01/02/04/05/07/08/09 各专题的「怎么做」。不搬运正文 |
| **新建** | — | `TRACKING.md` | 指令新增类。记录当前进行中工作 / 跨会话 WIP。初始从 ROADMAP 的 ◐ 进行中项 + 开放决策门提炼 |
| **新建** | — | `BACKLOG.md` | 从 ROADMAP 的 ☐ 且低优先/搁置项（如 BV 写尾抖动「搁置」、BV compact 自动化、物理空间回收、Stretch/研究）收敛 |
| **改名目录** | `plans/` | `plan/` | 指令用单数 `plan/`。`git mv`，同步全部交叉引用 |
| **保留** | `00`–`09` 编号专题 | 原地 | 作为 `<topic>.md` 领域知识/spec 快照存续，由 ARCH/DESIGN 索引指向 |
| **保留** | `archive/` `environment-snapshot.md` | 原地 | 已符合布局 |

### 待决权衡（需在审查/用户处澄清）

1. **编号制 vs 语义名**：是否要把 `01-zipfs-design.md` 等彻底拆进 ARCH.md/DESIGN.md 正文（高 churn，编号消失），还是保留编号专题、ARCH/DESIGN 仅作骨架+索引（低 churn）。本草案取后者。
2. **ARCH/DESIGN 是否值得新建**：若只做索引页，与 `README.md` 现有分组索引职能重叠。需确认不是纯冗余。
3. **CONFIG.md / GUIDE.md**：zipfs 有 `zipfs enable` TUI + CLI，配置项散落。是否此次一并抽 CONFIG.md，还是推迟。

## 4. 交叉引用同步清单

`ADR.md`→`ADR.md`、`plans/`→`plan/` 改名后需全库改引用。已知引用点：
- `docs/README.md`（索引，多处）
- `docs/CHANGELOG.md`、`docs/ADR.md` 自身、各编号专题里指向 `ADR.md §x` 的链接
- `docs/plan/README.md` 及 `plans/` 内部相对链接
- `docs/archive/*` 内引用
- 代码/README 里对 `docs/ADR.md`、`docs/plan/` 的引用（需 grep 全仓）
- memory 索引中的路径（`workspace-restructure` 等条目提到 `plans/`）

## 4b. Subagent 评审纪要（2026-07-11，两路并行：务实 + 对抗）

两份评审结论方向相反，构成需用户拍板的硬分叉：
- **务实审查**：用户要「全面对齐」已覆盖指令「非强制」让步，草案第 3 节保守偏差过大。主张「保编号文件名 + ARCH/DESIGN 写**实质现状**内容 + 本次一并抽 CONFIG.md」。
- **对抗审查**：模板非强制、体系已成熟，应做**语义对齐非字面对齐**。主张收敛为「补一页 ARCH 骨架 + Stretch/研究段移 BACKLOG + 其余保持并 record-not-adopted」，认为改名/镜像/抽取是低 ROI 形式动作、有拆散单一信息源风险。

**两者一致（无争议，直接采纳）：**
1. **ARCH/DESIGN 不能只做索引壳** —— 会与 README 现有分组索引纯冗余，且违背指令中 ARCH/DESIGN 是「实质内容」的定义。要么写反映现状的实质内容（`01-zipfs-design.md` 是 2026-06-27 冻结快照，其后 workspace/hangfree/reconcile/enable/head-cache 全落地了，索引壳会回答「三周前的设计」），要么不建。
2. **`ADR.md → ADR.md` 简单 git mv 名不副实** —— 现状是 flat ledger（D1-D12 一表 + §3 实测推翻段）；且大量条目是**工程实测裁定**（31x 去重、字典 16x 过拟合），非用户决策，套 ADR「改动需用户同意」反降可维护性。改名要么不做、要么是「拆成 adr/NNNN-*.md 一决策一文件」的重写工程。
3. **交叉引用清单补漏**：项目根 `README.md`、`00-overview.md`、`03-target-data-scope.md`、memory 具体 5 文件（在仓库外，需标注跨边界）。**已验证事实**：代码/脚本/toml **零硬引用** `docs/plans`、`docs/decisions`，爆炸半径限于 markdown。
4. **TRACKING/BACKLOG 若做**：必须先定义与 ROADMAP 的职责边界并写入文件头，只放跨会话 WIP 指针（链接回 ROADMAP 行），不镜像/复制状态，否则撕碎 T0–T4 优先级叙事、重造 SoT 冲突。本项目短期 WIP 现由 memory 承担，是否需要 TRACKING 存疑。
5. `plans/README.md` 随机名对照表质量高，rename（若做）原样保留。

**分歧（待用户裁定）**：改动幅度——字面全面对齐 vs 语义最小对齐。

## 5. 执行顺序（分提交，每步 grep 断言无残留）

1. `git mv ADR.md ADR.md` + 全库改引用（一提交）
2. `git mv plans/ plan/` + 全库改引用（一提交）
3. 新建 ARCH.md / DESIGN.md 骨架索引（一提交）
4. 新建 TRACKING.md + BACKLOG.md，从 ROADMAP 提炼（一提交）
5. 重写 README.md 索引反映新布局（一提交）
6. 同步 memory 路径
