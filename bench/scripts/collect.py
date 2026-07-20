#!/usr/bin/env python3
"""collect.py — 解析 fio JSON 输出，汇总成一张 CSV。

把 run-suite.sh 产出的 results/<run>/<条件>/<job>.json 全部读出，
按 (条件, fio-job 名) 展平为行，提取吞吐 / IOPS / 延迟 p50,p99 / CPU。

用法:
    python3 bench/scripts/collect.py <results 目录> [-o summary.csv]
    # <results 目录> 既可指向某一次 run（含若干 <条件>/ 子目录），
    # 也可指向 results/ 根（会递归找所有 *.json）。

仅用 Python 标准库（json/csv/argparse/pathlib）。

fio JSON 结构要点:
    top["jobs"] 是列表，每个元素一个 job，含:
        jobname, read{bw,iops,clat_ns{percentile{...}}}, write{...},
        usr_cpu, sys_cpu
    bw 单位 KiB/s；clat 百分位单位 ns。我们统一换算: bw->MiB/s, lat->us。
"""
from __future__ import annotations

import argparse
import csv
import json
import sys
from pathlib import Path

# ── 常量（避免魔数）────────────────────────────────────────────
KIB_PER_MIB = 1024.0
NS_PER_US = 1000.0
# fio 百分位键名（fio 用字符串键，含一位小数）。
P50_KEY = "50.000000"
P99_KEY = "99.000000"

CSV_COLUMNS = [
    "run",          # 上层 run 目录名（日期 tag）
    "condition",    # 条件名（C0/A/B0/B2/...）
    "job",          # fio job 名（seq-write / rand-read-4k / ...）
    "rw",           # 主导方向: read / write（按哪边有 IO 判定）
    "bw_MiBps",     # 吞吐 MiB/s
    "iops",         # IOPS
    "lat_p50_us",   # 完成延迟 p50（微秒）
    "lat_p99_us",   # 完成延迟 p99（微秒）
    "usr_cpu_pct",  # 用户态 CPU %
    "sys_cpu_pct",  # 内核态 CPU %
    "source",       # 来源 json 相对路径，便于追溯
]


def _percentile(clat: dict, key: str):
    """从 clat_ns.percentile 取某百分位（ns）→ 换算为 us；缺失返回 None。"""
    pct = clat.get("percentile") or {}
    val = pct.get(key)
    if val is None:
        return None
    return round(val / NS_PER_US, 3)


def _side_metrics(side: dict):
    """从 read{} 或 write{} 子结构提取 (bw_MiBps, iops, p50_us, p99_us)。"""
    bw_kib = side.get("bw", 0) or 0
    iops = side.get("iops", 0) or 0
    clat = side.get("clat_ns") or {}
    return (
        round(bw_kib / KIB_PER_MIB, 3),
        round(iops, 2),
        _percentile(clat, P50_KEY),
        _percentile(clat, P99_KEY),
    )


def rows_from_fio_json(path: Path, run: str, condition: str):
    """解析单个 fio JSON 文件 → 行列表。容错: 坏文件告警并跳过。"""
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (json.JSONDecodeError, OSError) as exc:
        print(f"[collect] WARN: 跳过无法解析的 JSON: {path} ({exc})", file=sys.stderr)
        return []

    rows = []
    for job in data.get("jobs", []):
        jobname = job.get("jobname", path.stem)
        read = job.get("read") or {}
        write = job.get("write") or {}

        # 判定主导方向: 哪一侧有非零 IO 就报哪侧（fio 单方向 job 另一侧全 0）。
        read_io = (read.get("io_bytes", 0) or 0) > 0 or (read.get("iops", 0) or 0) > 0
        write_io = (write.get("io_bytes", 0) or 0) > 0 or (write.get("iops", 0) or 0) > 0

        if read_io and not write_io:
            sides = [("read", read)]
        elif write_io and not read_io:
            sides = [("write", write)]
        elif read_io and write_io:
            sides = [("read", read), ("write", write)]  # 混合负载，分两行
        else:
            sides = [("write", write)]  # 兜底（罕见: 全 0）

        for direction, side in sides:
            bw, iops, p50, p99 = _side_metrics(side)
            rows.append(
                {
                    "run": run,
                    "condition": condition,
                    "job": jobname,
                    "rw": direction,
                    "bw_MiBps": bw,
                    "iops": iops,
                    "lat_p50_us": p50,
                    "lat_p99_us": p99,
                    "usr_cpu_pct": round(job.get("usr_cpu", 0) or 0, 2),
                    "sys_cpu_pct": round(job.get("sys_cpu", 0) or 0, 2),
                    "source": str(path),
                }
            )
    return rows


def discover(results_dir: Path):
    """递归找出所有 fio json，推断 (run, condition)。

    约定布局: <results_dir>/<run>/<condition>/<job>.json
    若深度不足（直接传入某个 run 目录），用回退推断。
    """
    rows = []
    json_files = sorted(results_dir.rglob("*.json"))
    if not json_files:
        print(f"[collect] WARN: 在 {results_dir} 下未找到任何 *.json", file=sys.stderr)

    for jf in json_files:
        rel = jf.relative_to(results_dir)
        parts = rel.parts
        # 期望 .../<run>/<condition>/<job>.json
        if len(parts) >= 3:
            run, condition = parts[-3], parts[-2]
        elif len(parts) == 2:
            run, condition = results_dir.name, parts[-2]
        else:
            run, condition = results_dir.name, "unknown"
        rows.extend(rows_from_fio_json(jf, run, condition))
    return rows


def main(argv=None):
    ap = argparse.ArgumentParser(description="汇总 fio JSON 结果为 CSV")
    ap.add_argument("results_dir", type=Path, help="results 目录（某次 run 或 results 根）")
    ap.add_argument("-o", "--output", type=Path, default=None,
                    help="输出 CSV 路径（默认 <results_dir>/summary.csv）")
    args = ap.parse_args(argv)

    results_dir = args.results_dir
    if not results_dir.is_dir():
        print(f"[collect] ERROR: 不是目录: {results_dir}", file=sys.stderr)
        return 2

    out_path = args.output or (results_dir / "summary.csv")
    rows = discover(results_dir)

    with out_path.open("w", newline="", encoding="utf-8") as fh:
        writer = csv.DictWriter(fh, fieldnames=CSV_COLUMNS)
        writer.writeheader()
        for r in rows:
            writer.writerow(r)

    print(f"[collect] 写出 {len(rows)} 行 -> {out_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
