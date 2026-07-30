"""发布工作区：detached worktree + push origin HEAD:main（spec §四、§6.1）。

detached 的原因：git 不允许两个 worktree 同时检出 main，而用户主工作区已占用它。
"""

from __future__ import annotations

import subprocess
from pathlib import Path

from .config import GIT

TRAILER = "HARNESS-OP:"
MAX_PUSH_RETRY = 3
# 固定身份：commit 与 cherry-pick 都必须用它。否则重放后的 committer 会变成仓库
# 本地配置里的人类身份（本机实测为 Pu Xu <puxu@microsoft.com>），
# 「哪些提交是 harness 做的」就不再可查。
IDENT = ("-c", "user.name=scrollz-harness", "-c", "user.email=harness@localhost")


class NonFastForward(Exception):
    pass


class ReplayConflict(Exception):
    """重放时与他人改动冲突：本轮判失败，不静默重试。"""


class PublishWorktree:
    def __init__(self, repo_root: Path, worktree_path: Path,
                 remote: str = "origin", branch: str = "main"):
        self.repo_root = Path(repo_root)
        self.path = Path(worktree_path)
        self.remote = remote
        self.branch = branch
        # 本轮 operation 绑定的提案卡提交；重放只允许动它
        self.operation_sha: str | None = None
        self.operation_path: str | None = None

    def _assert_single_path(self, sha: str) -> None:
        changed = [f for f in self._git(
            "show", "--name-only", "--format=", sha).splitlines() if f.strip()]
        if changed != [self.operation_path]:
            raise ReplayConflict(
                f"提交 {sha[:8]} 改动了 {changed}，预期只有 {self.operation_path}")

    def _git(self, *args: str, cwd: Path | None = None) -> str:
        proc = subprocess.run([GIT, *args], cwd=cwd or self.path,
                              capture_output=True, text=True)
        if proc.returncode != 0:
            raise RuntimeError(f"git {' '.join(args)}: {proc.stderr.strip()}")
        return proc.stdout.strip()

    def ensure(self, allow_reset: bool = True) -> None:
        """allow_reset=False 时只保证工作区存在，**绝不 reset**（评审 C-04）。

        崩溃在「本地 commit 已完成、尚未 push」时，若预检无脑 reset --hard，
        会先把待恢复的提交删掉，再去发现有未决 operation——
        §5.0 的 proposal-committed-local 恢复态在真实 round 里将永远到不了。
        """
        self._git("fetch", self.remote, self.branch, cwd=self.repo_root)
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

    def _abort_in_progress(self) -> None:
        """清掉上一轮遗留的 cherry-pick / merge 半途状态。"""
        for sub in ("cherry-pick", "merge"):
            subprocess.run([GIT, sub, "--abort"], cwd=self.path,
                           capture_output=True, text=True)

    def is_clean(self) -> bool:
        return self._git("status", "--porcelain") == ""

    def write_proposal(self, rel_path: str, content: str) -> None:
        target = self.path / rel_path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content, encoding="utf-8")

    def commit(self, message: str, operation_id: str, rel_path: str) -> str:
        self._git("add", "--", rel_path)
        full = f"{message}\n\n{TRAILER}{operation_id}\n"
        self._git(*IDENT, "commit", "-m", full, "--", rel_path)
        sha = self._git("rev-parse", "HEAD")
        self.operation_sha = sha
        self.operation_path = rel_path
        return sha

    def local_has_operation(self, operation_id: str) -> bool:
        return self.operation_commit_sha(operation_id) is not None

    def operation_commit_sha(self, operation_id: str) -> str | None:
        """本地已存在的、属于该 operation 的提交 SHA（崩溃恢复要靠它认出提交）。"""
        out = self._git("log", "--grep", TRAILER + operation_id, "--format=%H")
        lines = [l for l in out.splitlines() if l.strip()]
        return lines[0] if lines else None

    def remote_has_operation(self, operation_id: str, rel_path: str) -> bool:
        self._git("fetch", self.remote, self.branch, cwd=self.repo_root)
        out = self._git("log", f"{self.remote}/{self.branch}", "--grep",
                        TRAILER + operation_id, "--format=%H", cwd=self.repo_root)
        if not out.strip():
            return False
        proc = subprocess.run(
            [GIT, "cat-file", "-e", f"{self.remote}/{self.branch}:{rel_path}"],
            cwd=self.repo_root, capture_output=True, text=True)
        return proc.returncode == 0

    def push(self) -> None:
        for _ in range(MAX_PUSH_RETRY):
            proc = subprocess.run(
                [GIT, "push", self.remote, f"HEAD:{self.branch}"],
                cwd=self.path, capture_output=True, text=True)
            if proc.returncode == 0:
                return
            if "non-fast-forward" not in proc.stderr and \
                    "fetch first" not in proc.stderr and \
                    "rejected" not in proc.stderr:
                raise RuntimeError(f"git push: {proc.stderr.strip()}")
            self._replay_onto_remote()
        raise NonFastForward("push 重试耗尽")

    def _replay_onto_remote(self) -> None:
        """只重放**本 operation 绑定的那一个提交**（评审 C-05）。

        不能重放 merge-base..HEAD 的全部提交：`_publish` 里可能因上一轮异常或
        人工操作残留其它提交，那样会把不属于本 operation 的改动推上 main。
        """
        if self.operation_sha is None:
            raise ReplayConflict("未绑定 operation commit SHA，拒绝重放")
        self._assert_single_path(self.operation_sha)
        self._git("fetch", self.remote, self.branch, cwd=self.repo_root)
        target = self._git("rev-parse", f"{self.remote}/{self.branch}",
                           cwd=self.repo_root)
        commits = [self.operation_sha]
        self._git("reset", "--hard", target)
        for commit in commits:
            proc = subprocess.run([GIT, *IDENT, "cherry-pick", commit],
                                  cwd=self.path, capture_output=True, text=True)
            if proc.returncode != 0:
                subprocess.run([GIT, "cherry-pick", "--abort"], cwd=self.path,
                               capture_output=True, text=True)
                raise ReplayConflict(
                    f"重放 {commit[:8]} 与远端冲突：{proc.stderr.strip()[:200]}")
