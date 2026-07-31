"""发布工作区：detached worktree + push origin HEAD:main（spec §四、§6.1）。

detached 的原因：git 不允许两个 worktree 同时检出 main，而用户主工作区已占用它。
"""

from __future__ import annotations

import re
import subprocess
from pathlib import Path

from .config import GIT

TRAILER_KEY = "HARNESS-OP"
TRAILER = f"{TRAILER_KEY}:"
MAX_PUSH_RETRY = 3
# 无人值守（systemd 定时器）下没有 ssh-agent 可解锁密钥，SSH remote 会永远
# push 失败；PAT 走 HTTPS 是本就为此设计的凭据形式。host 固定为 github.com，
# 因为 GH_TOKEN 是 GitHub PAT，与 SSH remote 的目标服务一致。
GITHUB_HOST = "github.com"
# 固定身份：commit 与 cherry-pick 都必须用它。否则重放后的 committer 会变成仓库
# 本地配置里的人类身份（本机实测为 Pu Xu <puxu@microsoft.com>），
# 「哪些提交是 harness 做的」就不再可查。
IDENT = ("-c", "user.name=scrollz-harness", "-c", "user.email=harness@localhost")

# 提案卡路径由 Issue 号和模型生成的 slug 拼出，路径可信度低。只允许这一种严格形状，
# 拒绝绝对路径、含 ".." 的段、空 segment ——评审 Critical。
PROPOSAL_PATH_RE = re.compile(r"^docs/proposals/(\d+)-([a-z0-9-]+)\.md$")


class NonFastForward(Exception):
    """push 重试耗尽（真实竞态但对手一直在赢）。"""

    def __init__(self, message: str, *, attempts: int, last_stderr: str,
                 remote_sha: str, head_sha: str):
        super().__init__(message)
        self.attempts = attempts
        self.last_stderr = last_stderr
        self.remote_sha = remote_sha
        self.head_sha = head_sha


class PushRejected(Exception):
    """push 被拒绝，但 fetch 后确认远端并未领先本地 HEAD——不是真实竞态。

    典型原因：branch protection / pre-receive hook / server policy。这类拒绝
    没有重放收益，重放反而会悄悄改变本地 commit SHA 并掩盖原始错误。
    """


class ReplayConflict(Exception):
    """重放时与他人改动冲突：本轮判失败，不静默重试。"""


class AmbiguousOperation(Exception):
    """同一 operation_id 命中多个提交：拒绝静默取第一条。"""


class InvalidProposalPath(Exception):
    """提案卡路径不合法或试图逃出发布工作区。"""


class PublishWorktree:
    def __init__(self, repo_root: Path, worktree_path: Path,
                 remote: str = "origin", branch: str = "main",
                 gh_token: str = "", repo_slug: str = ""):
        self.repo_root = Path(repo_root)
        self.path = Path(worktree_path)
        self.remote = remote
        self.branch = branch
        # 无人值守场景下的凭据：显式注入，不在本模块内读环境/配置文件，
        # 保持凭据来源单一、本类可测（spec 要求 4）。二者都非空时才启用
        # HTTPS+PAT 路径；否则回退到 `remote`（本地/测试场景，例如
        # `file://`、路径式 bare repo）。
        self.gh_token = gh_token
        self.repo_slug = repo_slug
        # 本轮 operation 绑定的提案卡提交；重放只允许动它
        self.operation_sha: str | None = None
        self.operation_path: str | None = None

    # ------------------------------------------------------------- 凭据/URL

    def _use_https_auth(self) -> bool:
        return bool(self.gh_token and self.repo_slug)

    def _https_url(self) -> str:
        return (f"https://x-access-token:{self.gh_token}@"
                f"{GITHUB_HOST}/{self.repo_slug}.git")

    def _redact(self, text: str) -> str:
        """把 token 明文从任何将要落进异常/日志的文本里去掉。

        token 一旦出现在 `RuntimeError` 里，会随异常冒到日志和 Issue 评论中——
        等同于把 PAT 写进公开仓库（spec 要求 3）。"""
        if self.gh_token:
            return text.replace(self.gh_token, "<REDACTED>")
        return text

    # ---------------------------------------------------------------- 路径校验

    def _validate_rel_path(self, rel_path: str) -> Path:
        """校验 rel_path 是规范的严格提案路径，返回未 resolve 的 target。

        只接受 `docs/proposals/<issue>-<slug>.md`（issue 为数字，slug 为
        `[a-z0-9-]+`）。拒绝绝对路径、含 ".." 的段、空 segment——这些在正则
        锚定匹配下天然被排除，不需要单独再判断。
        """
        if not PROPOSAL_PATH_RE.match(rel_path):
            raise InvalidProposalPath(
                f"非法提案路径：{rel_path!r}；必须匹配 "
                f"docs/proposals/<issue>-<slug>.md（issue 为数字，"
                f"slug 为 [a-z0-9-]+）")
        target = self.path / rel_path
        self._assert_within_root(target)
        return target

    def _assert_within_root(self, target: Path) -> None:
        """解析 symlink 后确认 target 仍在工作区根目录之内。

        正则只锁死了字面路径的形状，挡不住中间目录（如 `docs/` 或
        `docs/proposals/`）本身是指向工作区外的 symlink。`resolve(strict=False)`
        会解析已存在的 symlink 部分，未存在的尾部分原样拼接，因此足以在
        创建目录/写文件之前发现这类逃逸。
        """
        resolved = target.resolve(strict=False)
        root = self.path.resolve(strict=False)
        if not resolved.is_relative_to(root):
            raise InvalidProposalPath(
                f"路径 {str(target)!r} 解析后为 {resolved}，"
                f"逃出发布工作区 {root}")

    # -------------------------------------------------------------------- git

    def _assert_single_path(self, sha: str) -> None:
        changed = self._changed_paths(sha)
        if changed != [self.operation_path]:
            raise ReplayConflict(
                f"提交 {sha[:8]} 改动了 {changed}，预期只有 {self.operation_path}")

    def _changed_paths(self, sha: str, cwd: Path | None = None) -> list[str]:
        return [f for f in self._git(
            "show", "--name-only", "--format=", sha, cwd=cwd).splitlines()
            if f.strip()]

    def _git(self, *args: str, cwd: Path | None = None) -> str:
        proc = subprocess.run([GIT, *args], cwd=cwd or self.path,
                              capture_output=True, text=True)
        if proc.returncode != 0:
            # args 本身可能含 HTTPS token URL（fetch 走 HTTPS 路径时）；
            # stderr 与 argv 拼接前都必须脱敏，否则 token 会随异常冒到日志
            # 和 Issue 评论里（spec 要求 3）。
            raise RuntimeError(self._redact(
                f"git {' '.join(args)}: {proc.stderr.strip()}"))
        return proc.stdout.strip()

    def _fetch(self) -> None:
        """fetch `self.branch`，确保 `refs/remotes/{remote}/{branch}` 被更新。

        配置了 token 时直接用 HTTPS URL 拉取——无人值守场景下没有 ssh-agent，
        SSH remote 永远失败（spec 要求 5）。此时 git 不会像按已配置 remote
        名字 fetch 那样自动写 tracking ref，需要显式 refspec 补上；未配置
        token（本地测试场景）时保留原有的按 remote 名 fetch，不改变既有
        20 条 gitops 测试的行为（spec 要求 6）。
        """
        if self._use_https_auth():
            self._git("-c", "credential.helper=", "fetch", self._https_url(),
                      f"+{self.branch}:refs/remotes/{self.remote}/{self.branch}",
                      cwd=self.repo_root)
        else:
            self._git("fetch", self.remote, self.branch, cwd=self.repo_root)

    def _push_argv(self) -> list[str]:
        if self._use_https_auth():
            return [GIT, "-c", "credential.helper=", "push",
                    self._https_url(), f"HEAD:{self.branch}"]
        return [GIT, "push", self.remote, f"HEAD:{self.branch}"]

    def ensure(self, allow_reset: bool = True) -> None:
        """allow_reset=False 时只保证工作区存在，**绝不 reset**（评审 C-04）。

        崩溃在「本地 commit 已完成、尚未 push」时，若预检无脑 reset --hard，
        会先把待恢复的提交删掉，再去发现有未决 operation——
        §5.0 的 proposal-committed-local 恢复态在真实 round 里将永远到不了。
        """
        self._fetch()
        target = self._git("rev-parse", f"{self.remote}/{self.branch}",
                           cwd=self.repo_root)
        # 崩溃或人工清理会删掉目录却留下 worktree 注册；不 prune 会导致
        # `worktree add` 报「already registered」而永久卡死（已实测）
        self._git("worktree", "prune", cwd=self.repo_root)
        if not (self.path / ".git").exists():
            self.path.parent.mkdir(parents=True, exist_ok=True)
            self._git("worktree", "add", "--detach", str(self.path), target,
                      cwd=self.repo_root)
        elif allow_reset:
            self._abort_in_progress()
            self._git("reset", "--hard", target)
            self._git("clean", "-fd")
        else:
            self._abort_in_progress()

    def _git_dir(self) -> Path:
        return Path(self._git("rev-parse", "--absolute-git-dir"))

    def _abort_in_progress(self) -> None:
        """清掉上一轮遗留的 cherry-pick / merge 半途状态。

        只有当对应的 marker 文件（`CHERRY_PICK_HEAD` / `MERGE_HEAD`）真实存在
        时才 abort——否则 `git <sub> --abort` 在无操作时仍可能改动 index/HEAD，
        且原实现完全忽略退出码，无法区分「本来无操作」与「abort 失败仍有残留」
        （评审 Important #2）。
        """
        gitdir = self._git_dir()
        markers = (("cherry-pick", gitdir / "CHERRY_PICK_HEAD"),
                   ("merge", gitdir / "MERGE_HEAD"))
        for sub, marker in markers:
            if not marker.exists():
                continue
            proc = subprocess.run([GIT, sub, "--abort"], cwd=self.path,
                                  capture_output=True, text=True)
            if proc.returncode != 0 or marker.exists():
                raise RuntimeError(
                    f"清理 {sub} 残留失败（退出码 {proc.returncode}）："
                    f"{proc.stderr.strip()}")

    def is_clean(self) -> bool:
        return self._git("status", "--porcelain") == ""

    def write_proposal(self, rel_path: str, content: str) -> None:
        target = self._validate_rel_path(rel_path)
        target.parent.mkdir(parents=True, exist_ok=True)
        # mkdir 与校验之间存在 TOCTOU 窗口（例如并发写入插入了 symlink）；
        # 创建父目录后再次确认边界（评审 Critical 修法 3）。
        self._assert_within_root(target)
        target.write_text(content, encoding="utf-8")

    def commit(self, message: str, operation_id: str, rel_path: str) -> str:
        self._validate_rel_path(rel_path)
        self._git("add", "--", rel_path)
        full = f"{message}\n\n{TRAILER}{operation_id}\n"
        self._git(*IDENT, "commit", "-m", full, "--", rel_path)
        sha = self._git("rev-parse", "HEAD")
        self.operation_sha = sha
        self.operation_path = rel_path
        return sha

    def local_has_operation(self, operation_id: str) -> bool:
        return self.operation_commit_sha(operation_id) is not None

    def _trailer_log(self, *log_args: str, cwd: Path | None = None) -> list[tuple[str, str]]:
        """返回 `(sha, trailer值)` 列表；trailer 值为空串表示该提交没有该 trailer。

        用 `%(trailers:key=...,valueonly)` 精确提取 trailer 字段值，而不是对
        commit message 做子串 grep——后者会让 `op12` 命中 `op123`（评审
        Important #3）。
        """
        fmt = "%H\x1f%(trailers:key=" + TRAILER_KEY + ",valueonly,unfold,separator=)"
        out = self._git("log", *log_args, f"--format={fmt}", cwd=cwd)
        result = []
        for line in out.splitlines():
            if not line:
                continue
            sha, _, value = line.partition("\x1f")
            result.append((sha, value))
        return result

    def operation_commit_sha(self, operation_id: str) -> str | None:
        """本地已存在的、属于该 operation 的提交 SHA（崩溃恢复要靠它认出提交）。

        命中 0 个返回 None；命中超过 1 个视为一致性错误，拒绝静默取第一条。
        """
        matches = [sha for sha, value in self._trailer_log()
                   if value == operation_id]
        if len(matches) > 1:
            raise AmbiguousOperation(
                f"operation {operation_id} 在本地命中 {len(matches)} 个提交："
                f"{matches}；拒绝静默取第一条")
        return matches[0] if matches else None

    def remote_has_operation(self, operation_id: str, rel_path: str) -> bool:
        """远端 tip 上是否存在该 operation 的提交，且该提交恰好只改了 rel_path。

        只验证「历史里有个含 marker 的 commit」不足以证明「那个 commit 改过
        那个 path」——评审 Important #3 要求同时核验改动路径。
        """
        self._fetch()
        matches = [sha for sha, value in self._trailer_log(
            f"{self.remote}/{self.branch}", cwd=self.repo_root)
            if value == operation_id]
        if len(matches) > 1:
            raise AmbiguousOperation(
                f"operation {operation_id} 在远端命中 {len(matches)} 个提交："
                f"{matches}；拒绝静默取第一条")
        if not matches:
            return False
        changed = self._changed_paths(matches[0], cwd=self.repo_root)
        return changed == [rel_path]

    def _looks_like_ref_rejection(self, stderr: str) -> bool:
        """粗筛：这类失败是不是「ref 更新被拒」（区别于网络/权限等无关错误）。

        真正的竞态 vs 策略拒绝判定不靠这个文本，靠 `_is_real_race()`——hook
        的拒绝文案是任意的，不能作为分类依据（评审 Important #1）。
        """
        return ("non-fast-forward" in stderr or "fetch first" in stderr or
                "rejected" in stderr)

    def _is_real_race(self) -> bool:
        """fetch 后检查远端 branch 是否真的不再是本地 HEAD 的祖先。

        返回 True：远端有本地看不到的提交（真实竞态，重放有意义）。
        返回 False：本地 HEAD 已包含远端全部历史（我们领先或持平），
        此时的拒绝只能是 branch protection / pre-receive hook 等策略拒绝，
        重放没有收益且会掩盖原始错误。
        """
        self._fetch()
        proc = subprocess.run(
            [GIT, "merge-base", "--is-ancestor", f"{self.remote}/{self.branch}", "HEAD"],
            cwd=self.path, capture_output=True, text=True)
        if proc.returncode not in (0, 1):
            raise RuntimeError(
                f"git merge-base --is-ancestor: {proc.stderr.strip()}")
        return proc.returncode != 0

    def push(self) -> None:
        last_stderr = ""
        for attempt in range(1, MAX_PUSH_RETRY + 1):
            proc = subprocess.run(self._push_argv(), cwd=self.path,
                                  capture_output=True, text=True)
            if proc.returncode == 0:
                return
            last_stderr = self._redact(proc.stderr.strip())
            if not self._looks_like_ref_rejection(last_stderr):
                raise RuntimeError(f"git push: {last_stderr}")
            if not self._is_real_race():
                raise PushRejected(
                    f"push 被拒绝但远端并未领先本地 HEAD，非真实竞态，"
                    f"疑似分支保护/hook 策略拒绝：{last_stderr}")
            if attempt == MAX_PUSH_RETRY:
                # 第三次失败后直接抛出，不再执行一次没有后续 push 的重放
                # （评审 Important #1）。
                break
            self._replay_onto_remote()
        remote_sha = self._git("rev-parse", f"{self.remote}/{self.branch}",
                               cwd=self.repo_root)
        head_sha = self._git("rev-parse", "HEAD")
        raise NonFastForward(
            f"push 重试耗尽（{MAX_PUSH_RETRY} 次尝试）：{last_stderr}",
            attempts=MAX_PUSH_RETRY, last_stderr=last_stderr,
            remote_sha=remote_sha, head_sha=head_sha)

    def _replay_onto_remote(self) -> None:
        """只重放**本 operation 绑定的那一个提交**（评审 C-05）。

        不能重放 merge-base..HEAD 的全部提交：`_publish` 里可能因上一轮异常或
        人工操作残留其它提交，那样会把不属于本 operation 的改动推上 main。
        """
        if self.operation_sha is None:
            raise ReplayConflict("未绑定 operation commit SHA，拒绝重放")
        self._assert_single_path(self.operation_sha)
        self._fetch()
        target = self._git("rev-parse", f"{self.remote}/{self.branch}",
                           cwd=self.repo_root)
        self._git("reset", "--hard", target)
        # 单提交不变量：直接 cherry-pick 一个 SHA，不需要用列表包装成多提交抽象
        # （评审 Minor）。
        proc = subprocess.run([GIT, *IDENT, "cherry-pick", self.operation_sha],
                              cwd=self.path, capture_output=True, text=True)
        if proc.returncode != 0:
            subprocess.run([GIT, "cherry-pick", "--abort"], cwd=self.path,
                           capture_output=True, text=True)
            raise ReplayConflict(
                f"重放 {self.operation_sha[:8]} 与远端冲突："
                f"{proc.stderr.strip()[:200]}")
