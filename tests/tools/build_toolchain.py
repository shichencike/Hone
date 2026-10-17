#!/usr/bin/env python3
"""G3.4 工具链打包：targets/ 运行时归档 + lld + CRT（设计 §5.1）。

产出布局（随 hone 发行包分发；目标机无需 rustc/gcc）：
  targets/<triple>/libhoneruntime.a   该目标的运行时 staticlib
                                      （runtime/nat_main.rs：runtime + 固定 main，
                                      --cfg hone_boot 启用 hn_boot 静态表自举）
  targets/lld/rust-lld(.exe)          链接器（宿主工具，来自 rustc sysroot）
  targets/crt/<triple>/               CRT + 导入库（rustc sysroot self-contained 拷贝）

用法：python tests/tools/build_toolchain.py [--target <triple>]（默认宿主目标；
可多次传入 --target 或 --all-common 构建三目标矩阵）。
交叉目标的 staticlib 编译需要对应 target std（rustup target add ...）。
"""

import argparse
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
NAT_MAIN = ROOT / "runtime" / "nat_main.rs"

# 三目标矩阵（设计 §6.1）
COMMON_TARGETS = [
    "x86_64-pc-windows-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
]


def rustc_sysroot() -> str:
    out = subprocess.run(["rustc", "--print", "sysroot"], capture_output=True, text=True, encoding="utf-8", errors="replace")
    if out.returncode != 0:
        sys.exit("rustc 不可用：工具链打包需要 rustc（打包一次，分发后目标机无需）")
    return (out.stdout or "").strip()


def build_staticlib(target: str, sysroot: str) -> Path:
    out_dir = ROOT / "targets" / target
    out_dir.mkdir(parents=True, exist_ok=True)
    out = out_dir / "libhoneruntime.a"
    host = subprocess.run(["rustc", "-vV"], capture_output=True, text=True, encoding="utf-8", errors="replace").stdout
    host_triple = next(l.split(": ")[1].strip() for l in host.splitlines() if l.startswith("host:"))
    cmd = ["rustc", "--edition", "2021", "--cfg", "hone_boot",
           "--crate-type", "staticlib",
           "-C", "opt-level=z", "-C", "codegen-units=1"]
    if target != host_triple:
        cmd += ["--target", target]
    cmd += [str(NAT_MAIN), "-o", str(out)]
    print(f"[staticlib] {target} -> {out}")
    r = subprocess.run(cmd, capture_output=True, text=True, encoding="utf-8", errors="replace")
    if r.returncode != 0:
        sys.exit(f"staticlib 编译失败 ({target}):\n{(r.stderr or "")[-3000:]}")
    return out


def lld_runtime_deps(src: Path, sysroot: str) -> list:
    """列出 lld 的动态依赖中**位于 sysroot 内**的库。

    Linux/macOS 的 rust-lld 是动态链接的（典型：sysroot 的 libLLVM.so），目标机没有 rustc 就找不到
    → 打包路径不可用。Windows 侧 rust-lld.exe 的依赖是 bin/ 下的 DLL（已随包拷贝），无需此步。
    """
    if sys.platform == "win32":
        return []
    try:
        r = subprocess.run(["ldd", str(src)], capture_output=True, text=True,
                           encoding="utf-8", errors="replace")
    except OSError:
        return []
    if r.returncode != 0:
        return []
    root = Path(sysroot).resolve()
    deps = []
    for line in (r.stdout or "").splitlines():
        if "=>" not in line:
            continue
        path = line.split("=>", 1)[1].strip().split(" ")[0]
        if not path.startswith("/"):
            continue
        p = Path(path)
        try:
            p.relative_to(root)
        except ValueError:
            continue          # 系统库（libc/libstdc++ 等）：目标机自带，不随包
        if p.is_file():
            deps.append(p)
    return deps


def package_lld(sysroot: str) -> Path:
    host = subprocess.run(["rustc", "-vV"], capture_output=True, text=True, encoding="utf-8", errors="replace").stdout
    host_triple = next(l.split(": ")[1].strip() for l in host.splitlines() if l.startswith("host:"))
    lld_dir = ROOT / "targets" / "lld"
    lld_dir.mkdir(parents=True, exist_ok=True)
    exe = "rust-lld.exe" if sys.platform == "win32" else "rust-lld"
    src = Path(sysroot) / "lib" / "rustlib" / host_triple / "bin" / exe
    if not src.exists():
        sys.exit(f"rust-lld 不存在: {src}")
    dst = lld_dir / exe
    shutil.copy2(src, dst)
    # rust-lld.exe 的依赖 DLL（最小 PATH 下 STATUS_DLL_NOT_FOUND）：随包分发。
    for dll in (Path(sysroot) / "lib" / "rustlib" / host_triple / "bin").glob("*.dll"):
        shutil.copy2(dll, lld_dir / dll.name)
        print(f"[lld] 依赖 DLL {dll.name}")
    print(f"[lld] {src} -> {dst}")
    # 非 Windows：lld 动态依赖 sysroot 内的共享库（libLLVM.so 等），**未**随包分发
    # ——目标机无 rustc 时该平台打包链接暂不可用（G3.8）。此处如实打印体积，供打包决策。
    deps = lld_runtime_deps(src, sysroot)
    if deps:
        total = sum(d.stat().st_size for d in deps) / (1024 * 1024)
        print(f"[lld] 告警：{exe} 动态依赖 sysroot 内 {len(deps)} 个库（合计 {total:.1f} MB），"
              f"尚未随包分发：")
        for d in deps:
            print(f"[lld]   {d.name}  {d.stat().st_size / (1024 * 1024):.1f} MB")
        print("[lld] → 目标机无 rustc 时该平台打包链接不可用（hone 会自动改用 rustc 链接路径）；"
              "自足化见进度文档 G3.8")
    return dst


def package_crt(target: str, sysroot: str) -> Path:
    """CRT + 导入库：来自 <sysroot>/lib/rustlib/<target>/lib/self-contained
    （windows-gnu 与 musl 目标的 rustc std 组件均自带）。"""
    self_contained = Path(sysroot) / "lib" / "rustlib" / target / "lib" / "self-contained"
    dst = ROOT / "targets" / "crt" / target
    dst.mkdir(parents=True, exist_ok=True)
    if self_contained.is_dir():
        for f in self_contained.iterdir():
            if f.is_file():
                shutil.copy2(f, dst / f.name)
        print(f"[crt] {target}: self-contained 全量拷贝（{len(list(dst.iterdir()))} 个文件）")
    else:
        sys.exit(f"目标 {target} 的 self-contained CRT 不存在: {self_contained}（先 rustup target add {target}）")
    return dst


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--target", action="append", default=[], help="目标三元组（可多次）")
    ap.add_argument("--all-common", action="store_true", help="构建三目标矩阵")
    args = ap.parse_args()
    sysroot = rustc_sysroot()
    package_lld(sysroot)
    targets = COMMON_TARGETS if args.all_common else args.target
    if not targets:
        host = subprocess.run(["rustc", "-vV"], capture_output=True, text=True, encoding="utf-8", errors="replace").stdout
        targets = [next(l.split(": ")[1].strip() for l in host.splitlines() if l.startswith("host:"))]
    for t in targets:
        build_staticlib(t, sysroot)
        package_crt(t, sysroot)
    print("工具链打包完成：targets/")


if __name__ == "__main__":
    main()
