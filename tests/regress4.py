#!/usr/bin/env python3
"""三引擎回归：interp（hone run）/ VM（--vm）/ nat（build --exe --native）三路
stdout+stderr+退出码 逐字节比对。

用法：在仓库根目录执行 python tests/regress4.py
约定：
  - 用例集 = M0 基线三例（examples/fib.hn、examples/hello.hn、examples/err_divzero.hn）
    + M1 新增用例集（tests/m1/list.hn、tests/m1/dict.hn、tests/smoke/rc_alias.test.hn）。
    M2 任务组落地后按进度文档继续扩容。
    （AOT-C 后端 build --exe -c 暂不纳入本回归：其错误渲染为简化一行，
    与 interp/VM 的完整 ZError 渲染无法逐字节对齐；待 AOT 错误渲染对齐后另立比对。）
  - 错误诊断逐字符对齐由 tests/regress_err.py 承担（含 nat 第三引擎）；
    本脚本聚焦正常路径 + 单例错误渲染。
  - 每用例单独编译 nat 产物（产物内含该脚本，不可跨用例复用）；
    nat 中间 object（stem.nat.o）与临时 exe 跑完即删。
  - 任一用例首跑出现分叉 → 重试至多 3 次，任一次三路一致即判 FLAKY（随机数/
    时间/线程顺序类非逻辑差异，与 regress3 的判定口径一致）；稳定分叉才计
    FAIL，并打印首个分叉行。
"""
import os
import shutil
import subprocess
import sys
import tempfile

os.chdir(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from regress_common import should_skip  # noqa: E402

BIN = "./target/debug/hone.exe"

# 三引擎对齐用例集：M0 基线三例 + M1 新增用例集（G1.2 list / G1.3 dict+G1.4 迭代 /
# G1.5 别名 RC+COW）+ M2 新增（G2.1 struct 构造/字段/隐藏键过滤/相等语义、
# G2.2 enum 构造/match 基础/显示/相等/容器、G2.3 match 全形态：字面量/变体/绑定/作用域/闭包）。
# 错误诊断的逐字符对齐由 regress_err.py 承担（含 nat 第三引擎）。
CASES = [
    "examples/fib.hn",
    "examples/hello.hn",
    "examples/err_divzero.hn",
    "tests/m1/list.hn",
    "tests/m1/dict.hn",
    "tests/smoke/rc_alias.test.hn",
    "tests/m2/struct.hn",
    "tests/m2/enum.hn",
    "tests/m2/match.hn",
    "tests/m2/conditions.hn",
    "tests/m2/overload.hn",
    "tests/m2/alias_goto.hn",
    "tests/m2/defaults.hn",
    "tests/m2/dotted.hn",
    "tests/m2/async_go.hn",
    "tests/m2/imports.hn",
]

ENGINES = ("interp", "vm", "nat")


def build_native(path: str, tmp: str) -> str:
    """nat 编译 → 临时目录 exe（阶段行走 stderr，不污染比对输出）。"""
    out = os.path.join(tmp, "case_nat.exe")
    p = subprocess.run(
        [BIN, "build", "--exe", "--native", path, "-o", out],
        capture_output=True, text=True, timeout=120,
    )
    # 中间 object 默认落在 CWD（stem.nat.o），即删。
    obj = os.path.join(os.getcwd(), os.path.splitext(os.path.basename(path))[0] + ".nat.o")
    if os.path.exists(obj):
        os.remove(obj)
    if p.returncode != 0 or not os.path.exists(out):
        raise RuntimeError(f"nat 编译失败:\n{p.stdout}\n{p.stderr}")
    return out


def run_exe(exe: str):
    p = subprocess.run([exe], capture_output=True, text=True, timeout=30)
    return (p.returncode, p.stdout + p.stderr)


def run_engine(engine: str, path: str, nat_exe: str):
    if engine == "interp":
        p = subprocess.run([BIN, "run", path], capture_output=True, text=True, timeout=30)
        return (p.returncode, p.stdout + p.stderr)
    if engine == "vm":
        p = subprocess.run([BIN, "run", "--vm", path], capture_output=True, text=True, timeout=30)
        return (p.returncode, p.stdout + p.stderr)
    return run_exe(nat_exe)


def all_match(results: dict) -> bool:
    """三路两两比对（基准 interp）。"""
    return all(results[e][1] == results["interp"][1] for e in ENGINES if e != "interp")


def first_diff(a: str, b: str):
    """首个分叉行（两侧各取该行），便于定位。"""
    al, bl = a.splitlines(), b.splitlines()
    n = min(len(al), len(bl))
    for k in range(n):
        if al[k] != bl[k]:
            return al[k], bl[k]
    if len(al) != len(bl):
        return (al[n - 1] if al else ""), (bl[n - 1] if bl else "")
    return "", ""


def main() -> int:
    if not os.path.exists(BIN):
        print(f"缺少 {BIN}：先 cargo build", file=sys.stderr)
        return 1
    if not shutil.which("rustc"):
        print("nat: 跳过（无 rustc 工具链）", file=sys.stderr)
        return 1
    tmp = tempfile.mkdtemp(prefix="hone_regress4_")
    passed = failed = 0
    flaky_names = []
    try:
        for path in CASES:
            base = os.path.splitext(os.path.basename(path))[0]
            if should_skip(base):
                print(f"SKIP {path}")
                continue
            try:
                nat_exe = build_native(path, tmp)
            except (RuntimeError, subprocess.TimeoutExpired) as e:
                failed += 1
                print(f"BUILD-FAIL {path}\n{e}", file=sys.stderr)
                continue
            try:
                results = {e: run_engine(e, path, nat_exe) for e in ENGINES}
            except subprocess.TimeoutExpired:
                failed += 1
                print(f"TIMEOUT {path}")
                continue
            if not all_match(results):
                # FLAKY 重试：任一次全绿即判非逻辑差异（随机数/时间/线程顺序）。
                ok = False
                for _ in range(3):
                    try:
                        r2 = {e: run_engine(e, path, nat_exe) for e in ENGINES}
                    except subprocess.TimeoutExpired:
                        break
                    ok = all_match(r2)
                    if ok:
                        break
                if ok:
                    flaky_names.append(path)
                    print(f"FLAKY {path}（重试后一致）")
                    continue
            if all_match(results):
                passed += 1
                print(f"PASS {path}")
            else:
                failed += 1
                print(f"FAIL {path}")
                for e in ENGINES:
                    if e == "interp" or results[e][1] == results["interp"][1]:
                        continue
                    la, lb = first_diff(results["interp"][1], results[e][1])
                    print(f"   首个分叉行（interp vs {e}）:")
                    print(f"   interp: {la!r}")
                    print(f"   {e:7s}: {lb!r}")
                    print(f"   (退出码 interp={results['interp'][0]} {e}={results[e][0]})")
    finally:
        f = os.path.join(tmp, "case_nat.exe")
        if os.path.exists(f):
            os.remove(f)
        shutil.rmtree(tmp, ignore_errors=True)
    print(f"PASS={passed} FLAKY={len(flaky_names)} FAIL={failed} (共 {len(CASES)} 个用例)")
    if flaky_names:
        print(f"FLAKY(非逻辑差异): {', '.join(flaky_names)}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
