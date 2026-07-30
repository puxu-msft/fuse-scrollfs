from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path("/home/xp/src/zipfs")
DEFAULT_ENV_FILE = Path.home() / ".config/scrollz-harness/env"

PYTHON = "/home/linuxbrew/.linuxbrew/bin/python3"
CLAUDE = "/home/xp/.local/bin/claude"
GH = "/usr/bin/gh"
GIT = "/usr/bin/git"
FLOCK = "/home/linuxbrew/.linuxbrew/bin/flock"


@dataclass(frozen=True)
class Config:
    repo_root: Path
    state_db: Path
    publish_worktree: Path
    repo_slug: str
    gh_token: str
    round_budget_usd: float
    daily_budget_usd: float
    max_turns: int
    proposed_cap: int
    lane_cap: int


def _read_env_file(path: Path) -> dict[str, str]:
    out: dict[str, str] = {}
    if not path.exists():
        return out
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, _, value = line.partition("=")
        out[key.strip()] = value.strip().strip("'\"")
    return out


def load_config(env_file: Path | None = None, repo_root: Path | None = None) -> Config:
    env_file = env_file or DEFAULT_ENV_FILE
    root = repo_root or REPO_ROOT
    env = _read_env_file(env_file)
    token = env.get("GH_TOKEN") or os.environ.get("GH_TOKEN", "")
    return Config(
        repo_root=root,
        state_db=root / ".claude/state/harness.db",
        publish_worktree=root / ".worktree/_publish",
        repo_slug=env.get("HARNESS_REPO", "puxu-msft/fuse-scrollfs"),
        gh_token=token,
        round_budget_usd=float(env.get("HARNESS_ROUND_BUDGET_USD", "1.50")),
        daily_budget_usd=float(env.get("HARNESS_DAILY_BUDGET_USD", "20.00")),
        max_turns=int(env.get("HARNESS_MAX_TURNS", "60")),
        proposed_cap=int(env.get("HARNESS_PROPOSED_CAP", "20")),
        lane_cap=int(env.get("HARNESS_LANE_CAP", "6")),
    )
