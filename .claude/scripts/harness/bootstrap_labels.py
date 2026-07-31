"""建立 harness 所需的全部 label。幂等：已存在则跳过。"""

from __future__ import annotations

import subprocess
import sys

from harness.config import GH, load_config

LABELS = [
    ("harness", "0e8a16", "harness 自动产出"),
    ("harness:proposed", "1d76db", "候选提案，未开工"),
    ("harness:blocked", "b60205", "卡住，需人工"),
    ("harness:needs-decision", "d93f0b", "触及冻结红线，需用户裁决"),
    ("harness:rejected", "cccccc", "已否决"),
    ("harness:paused", "000000", "暂停哨兵"),
    ("lane:roadmap", "c5def5", "来源 lane"),
    ("lane:defect", "c5def5", "来源 lane"),
    ("lane:perf", "c5def5", "来源 lane"),
    ("lane:hygiene", "c5def5", "来源 lane"),
    ("size:S", "ededed", "规模"),
    ("size:M", "ededed", "规模"),
    ("size:L", "ededed", "规模"),
    ("T0", "5319e7", "ROADMAP 优先级"),
    ("T1", "5319e7", "ROADMAP 优先级"),
    ("T2", "5319e7", "ROADMAP 优先级"),
    ("T3", "5319e7", "ROADMAP 优先级"),
    ("T4", "5319e7", "ROADMAP 优先级"),
]


def main() -> int:
    cfg = load_config()
    env = {"GH_TOKEN": cfg.gh_token, "PATH": "/usr/bin:/bin"}
    listing = subprocess.run(
        [GH, "api", f"repos/{cfg.repo_slug}/labels", "--paginate",
         "--jq", ".[].name"],
        capture_output=True, text=True, env=env)
    if listing.returncode != 0:
        print("读取 label 失败：", listing.stderr.strip())
        return 1
    existing = set(listing.stdout.split())

    created, skipped, failed = [], [], []
    for name, color, desc in LABELS:
        if name in existing:
            skipped.append(name)
            continue
        proc = subprocess.run(
            [GH, "api", f"repos/{cfg.repo_slug}/labels", "-X", "POST",
             "-f", f"name={name}", "-f", f"color={color}",
             "-f", f"description={desc}"],
            capture_output=True, text=True, env=env)
        (created if proc.returncode == 0 else failed).append(
            name if proc.returncode == 0 else f"{name}: {proc.stderr.strip()[:80]}")

    # 回读校验：不信任写调用的返回，只信远端事实
    verify = subprocess.run(
        [GH, "api", f"repos/{cfg.repo_slug}/labels", "--paginate",
         "--jq", ".[].name"], capture_output=True, text=True, env=env)
    final = set(verify.stdout.split())
    missing = [n for n, _, _ in LABELS if n not in final]

    print(f"新建 {len(created)}，已存在 {len(skipped)}，失败 {len(failed)}")
    for f in failed:
        print("  FAIL", f)
    if missing:
        print("回读后仍缺失：", missing)
        return 1
    print("全部 18 个 label 就位")
    return 0


if __name__ == "__main__":
    sys.exit(main())