#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""bench_ci.py — Hone 多引擎基准：9 负载 × hone(interp/VM/nat) vs Python vs Lua 5.4。

CI（ci.yml bench 作业）与本机均可跑：
    HONE_BIN=target/release/hone python tests/bench_ci.py

计时口径与 bench/bench.sh 一致：子进程 wall time，默认 3 次取最快；
单次超过 SLOW_MS 的负载只跑 1 次（解释器慢负载如 str_concat 20 万拼接）。

输出：
  官网/perf-data.json —— 官网/perf.html 前端读取渲染（含机器/版本/时间戳元信息）
  stdout —— 控制台对齐表格
缺解释器（lua5.4 / python3）时跳过对应列并在 JSON 标注 null，不失败。
nat 引擎：逐负载 `hone build --exe --native`（编译耗时计入准备阶段，不计入运行计时）。
"""
import json
import os
import platform
import shutil
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
HONE = Path(os.environ.get("HONE_BIN", ROOT / "target" / "release" / "hone"))
if os.name == "nt" and HONE.suffix == "":
    HONE = HONE.with_suffix(".exe")
BENCH_DIR = ROOT / "bench"
OUT_DIR = ROOT / "target" / "bench"
SLOW_MS = 5000  # 单次超过 5s 的负载只跑 1 次

# (统一基准名, .hn 文件名, python 文件名, lua 文件名)（历史命名映射同 bench.sh）
BENCHES = [
    ("fib",         "fib.hn",         "fib.py",         "fib.lua"),
    ("loop20m",     "loop.hn",        "loop20m.py",     "loop20m.lua"),
    ("loop2m",      "loop2m.hn",      "loop2m.py",      "loop2m.lua"),
    ("str_concat",  "str.hn",         "str_concat.py",  "str_concat.lua"),
    ("pi",          "pi.hn",          "pi.py",          "pi.lua"),
    ("list_append", "list_append.hn", "list_append.py", "list_append.lua"),
    ("dict_lookup", "dict.hn",        "dict_lookup.py", "dict_lookup.lua"),
    ("str_replace", "str_replace.hn", "str_replace.py", "str_replace.lua"),
    ("sort",        "sort.hn",        "sort.py",        "sort.lua"),
    ("list_write",  "list_write.hn",  "list_write.py",  "list_write.lua"),
]


def run_best_ms(cmd, runs=3):
    """子进程 wall time（毫秒），runs 次取最快；单次超 SLOW_MS 只跑 1 次。"""
    best = None
    for i in range(runs):
        t0 = time.perf_counter()
        p = subprocess.run([str(c) for c in cmd], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        ms = int((time.perf_counter() - t0) * 1000)
        if p.returncode != 0:
            return None
        best = ms if best is None else min(best, ms)
        if ms > SLOW_MS:
            break
    return best


def engine_cmd(kind, script):
    """构造某引擎运行某脚本的命令行。"""
    if kind == "interp":
        return [HONE, str(script)]
    if kind == "vm":
        return [HONE, str(script), "--vm"]
    raise ValueError(kind)


def nat_build(script, name):
    """nat 引擎：编译一次（耗时计入准备，不计运行计时），返回可执行路径。"""
    exe = OUT_DIR / f"{name}_nat{'.exe' if os.name == 'nt' else ''}"
    p = subprocess.run(
        [str(HONE), "build", "--exe", "--native", str(script), "-o", str(exe)],
        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True,
    )
    if p.returncode != 0 or not exe.is_file():
        print(f"  nat 编译失败 {name}: {p.stderr.strip()[-300:]}", file=sys.stderr)
        return None
    return exe


def best_of_built(cmd):
    return run_best_ms(cmd)


def collect_meta():
    cpu = platform.processor() or ""
    cores = os.cpu_count()
    try:
        with open("/proc/cpuinfo", encoding="utf-8", errors="ignore") as f:
            for line in f:
                if line.startswith("model name"):
                    cpu = line.split(":", 1)[1].strip()
                    break
    except OSError:
        pass
    ram = ""
    try:
        with open("/proc/meminfo", encoding="utf-8") as f:
            for line in f:
                if line.startswith("MemTotal"):
                    ram = f"{int(line.split()[1]) // (1024 * 1024)} GB"
                    break
    except OSError:
        pass
    sha = ""
    try:
        sha = subprocess.run(["git", "rev-parse", "--short", "HEAD"], cwd=ROOT,
                             capture_output=True, text=True).stdout.strip()
    except OSError:
        pass
    return {
        "date": datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M UTC"),
        "os": f"{platform.system()} {platform.release()}",
        "cpu": cpu or platform.machine(),
        "cores": cores,
        "ram": ram,
        "git_sha": sha,
        "hone_version": detect_version([HONE, "--version"]),
        "python_version": platform.python_version(),
        "lua_version": detect_version(["lua5.4", "-v"]),
        "timing": "子进程 wall time，3 次取最快（单次 >5s 只跑 1 次）；含解释器/运行时启动开销",
    }


def detect_version(cmd):
    if shutil.which(str(cmd[0])) is None:
        return None
    try:
        p = subprocess.run([str(c) for c in cmd], capture_output=True, text=True, timeout=10)
        out = (p.stdout or p.stderr).strip().splitlines()
        return out[0] if out else None
    except (OSError, subprocess.TimeoutExpired):
        return None


def main():
    if not HONE.is_file():
        print(f"hone 二进制不存在: {HONE}（先 cargo build --release 或设 HONE_BIN）", file=sys.stderr)
        return 1
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    meta = collect_meta()
    print(f"== Hone 基准（{meta['os']} / {meta['cpu']} / {meta['cores']} 核）==")
    print(f"   {meta['hone_version']} | Python {meta['python_version']} | Lua {meta['lua_version'] or '未安装（跳过）'}")

    lua_bin = "lua5.4"
    has_lua = shutil.which(lua_bin) is not None
    results = {}
    header = f"{'基准':<14}{'interp':>10}{'VM':>10}{'nat':>10}{'python':>10}{'lua5.4':>10}"
    print(header)
    for name, hn, py, lua in BENCHES:
        row = {}
        # 三引擎
        for kind in ("interp", "vm"):
            row[kind] = run_best_ms(engine_cmd(kind, BENCH_DIR / hn))
        nat_exe = nat_build(BENCH_DIR / hn, name)
        row["nat"] = run_best_ms([nat_exe]) if nat_exe else None
        # 对比语言
        row["python"] = run_best_ms([sys.executable, BENCH_DIR / "py" / py])
        row["lua"] = run_best_ms([lua_bin, BENCH_DIR / "lua" / lua]) if has_lua else None
        results[name] = row
        cells = "".join(f"{(row[k] if row[k] is not None else '-'):>10}" for k in ("interp", "vm", "nat", "python", "lua"))
        print(f"{name:<14}{cells}")

    data = {"meta": meta, "results": results}
    out = ROOT / "官网" / "perf-data.json"
    out.write_text(json.dumps(data, ensure_ascii=False, indent=2), encoding="utf-8")
    print(f"\n结果已写入 {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
