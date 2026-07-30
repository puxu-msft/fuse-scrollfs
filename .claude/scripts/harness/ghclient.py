"""GitHub 访问层：经 gh CLI，凭据只从控制器进程环境注入。"""

from __future__ import annotations

import json
import subprocess

from .config import GH, Config
from .outbox import ResponseLost

# 网络层不确定错误：可能已在服务端生效
_LOST_MARKERS = ("timeout", "timed out", "connection reset", "EOF",
                 "502", "503", "504", "unexpected EOF")


class GhCli:
    def __init__(self, cfg: Config):
        self.cfg = cfg
        self.slug = cfg.repo_slug

    def _run(self, args: list[str], parse: bool = True):
        env = {"GH_TOKEN": self.cfg.gh_token, "PATH": "/usr/bin:/bin",
               "HOME": str(self.cfg.repo_root)}
        proc = subprocess.run([GH, *args], capture_output=True, text=True, env=env)
        if proc.returncode != 0:
            stderr = proc.stderr.lower()
            if any(m.lower() in stderr for m in _LOST_MARKERS):
                raise ResponseLost(proc.stderr.strip())
            raise RuntimeError(f"gh {' '.join(args)} failed: {proc.stderr.strip()}")
        if not parse or not proc.stdout.strip():
            return None
        return json.loads(proc.stdout)

    def viewer_permission(self) -> str:
        data = self._run(["api", "graphql", "-f", (
            'query={repository(owner:"%s",name:"%s"){viewerPermission}}'
            % tuple(self.slug.split("/")))])
        return data["data"]["repository"]["viewerPermission"]

    def create_issue(self, title: str, body: str, labels: list[str]) -> dict:
        args = ["api", f"repos/{self.slug}/issues", "-X", "POST",
                "-f", f"title={title}", "-f", f"body={body}"]
        for label in labels:
            args += ["-f", "labels[]=" + label]
        return self._run(args)

    def find_issue_by_marker(self, marker: str) -> dict | None:
        data = self._run(["api", "-X", "GET", "search/issues", "-f",
                          f'q=repo:{self.slug} in:body "{marker}"'])
        items = data.get("items", [])
        return items[0] if items else None

    def list_labels(self) -> list[str]:
        return [l["name"] for l in self._run(["api", f"repos/{self.slug}/labels",
                                              "--paginate"])]

    def ensure_label(self, name: str, color: str, description: str) -> None:
        if name in self.list_labels():
            return
        self._run(["api", f"repos/{self.slug}/labels", "-X", "POST",
                   "-f", f"name={name}", "-f", f"color={color}",
                   "-f", f"description={description}"])

    def get_issue_labels(self, number: int) -> list[str]:
        issue = self._run(["api", f"repos/{self.slug}/issues/{number}"])
        return [l["name"] for l in issue["labels"]]

    def replace_labels(self, number: int, labels: list[str]) -> None:
        args = ["api", f"repos/{self.slug}/issues/{number}/labels", "-X", "PUT"]
        for label in labels:
            args += ["-f", "labels[]=" + label]
        self._run(args)

    def create_comment(self, number: int, body: str) -> dict:
        return self._run(["api", f"repos/{self.slug}/issues/{number}/comments",
                          "-X", "POST", "-f", f"body={body}"])

    def find_comment_by_marker(self, number: int, marker: str) -> dict | None:
        for c in self._run(["api", f"repos/{self.slug}/issues/{number}/comments",
                            "--paginate"]):
            if marker in c["body"]:
                return c
        return None

    def list_open_issues_with_label(self, label: str) -> list[dict]:
        return self._run(["api", "-X", "GET", f"repos/{self.slug}/issues",
                          "-f", f"labels={label}", "-f", "state=open",
                          "--paginate"])
