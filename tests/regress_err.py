#!/usr/bin/env python3
"""错误对齐回归：逐用例比对「解释器」/「VM」/「nat 原生编译」的错误输出是否逐字节一致。

用法：在仓库根目录执行 python tests/regress_err.py
约定：任何一处差异都视为失败（报错必须精准，行/列/文案/help 全对齐）。

nat（build --exe --native）作为第三引擎接入（G1.6），两条比对路径：
  1. 编译成功 → 跑产物，比对 (stdout+stderr, 退出码)。
  2. 编译期拒 → 分两类：
     a. checker 静态拦截：checker 是解释器/VM/AOT/nat 共用的前端，属同源行为，
        剥离 build 的 [N/4] 阶段进度行后，报错文本应与 `hone run` 逐字节一致；
     b. natgen 自身能力边界（lowering 阶段 H990/H999，M2 待落地）→ 列入
        NAT_UNSUPPORTED 显式登记，不静默跳过（每项须写明待落地任务号）。
"""
import os
import re
import shutil
import subprocess
import sys
import tempfile

# 仓库根目录 = 本脚本所在目录（tests/）的上一级，换机器/换路径均可用。
os.chdir(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BIN = os.environ.get(
    "HONE_BIN",
    "./target/release/hone.exe" if os.name == "nt" else "./target/release/hone",
)

# 每个用例：期望各引擎产生完全相同的 (stdout+stderr, 退出码)。
CASES = {
    # 变量 / 函数
    "undef_var": 'x = 1;\nprint(y);\n',
    "undef_fn": 'print(nosuchfn(1, 2));\n',
    "call_int": 'x = 5;\nprint(x(1));\n',
    "method_missing": 'x = 5;\nprint(x.nosuch());\n',
    # 算术
    "div_zero": 'x = 10;\ny = 0;\nz = x / y;\n',
    "fdiv_zero": 'print(1.0 / 0.0);\n',
    "mod_zero": 'print(1 % 0);\n',
    "overflow": 'print(9223372036854775807 + 1);\n',
    "add_mixed": 'print(1 + "a");\n',
    "str_plus_int": 'print("a" + 1);\n',
    "char_plus_int": "c = 'a';\nprint(c + 1);\n",
    "neg_bool": 'print(-true);\n',
    "not_int": 'print(!5);\n',
    # 比较
    "cmp_str": 'print("a" < "b");\n',
    "cmp_mixed": 'print(true < 1);\n',
    # 索引 / 字段
    "idx_oob_list": 'l = [1, 2];\nprint(l[9]);\n',
    "idx_oob_str": 's = "ab";\nprint(s[9]);\n',
    "idx_notindex": 'n = 5;\nprint(n[0]);\n',
    "idx_set_oob": 'l = [1];\nl[5] = 9;\n',
    "field_missing": 'd = {"a": 1};\nprint(d.missing);\n',
    "null_field": 'd = {};\nk = d["x"];\nprint(k.nope);\n',
    # 迭代 / 推导式
    "for_notiter": 'x = 1;\nfor t in x { print(t); }\n',
    "comp_notiter": 'y = 3;\nz = [i for i in y];\nprint(z);\n',
    # 解构（M1：解构错误文案已对齐解释器形态/变量数，见 G1.2/G1.3）
    "ds_too_few": 'a, b = [1];\nprint(a, b);\n',
    "ds_notiter": 'a, b = 5;\nprint(a, b);\n',
    "ds_dict_missing": '{m} = {"a": 1};\nprint(m);\n',
    "ds_list_got_dict": 'a, b = {"x": 1, "y": 2};\n',
    "ds_dict_got_list": '{c, d} = [1, 2];\n',
    "ds_short": 'x, y = [1];\nprint(y);\n',
    "ds_key_not_str": '{c, d} = {1: 2, 3: 4};\n',
    # match / await / 控制流
    # （match 是表达式、不能作语句：解析期 H005「expected a statement」——三引擎同源。）
    "match_as_stmt": 'x = 5;\nmatch x {\n    1 => { print("one"); }\n    2 => { print("two"); }\n}\n',
    # （match 无匹配分支：G2.3 由「抛字面串 → H600」改为与解释器逐字节的 H005 + help；
    #  用例用表达式形态，真走运行期 Raise 路径。）
    "match_no_arm": 'x = 5;\ny = match x {\n    1 => "one",\n    2 => "two",\n};\nprint(y);\n',
    "match_no_arm_enum": 'enum C { A, B };\nc = C.A;\ny = match c { C.B => 1 };\nprint(y);\n',
    # G2.3 引擎对齐：条件严格 bool / 逻辑操作数严格 bool 的运行期 H001（此前 VM 按 truthy
    # 静默放行，enum / struct 等动态类型值上分叉）。逐站点冻结消息/定位。
    "cond_if_enum": 'enum C { A, B };\nc = C.A;\nif (c) { print("x"); }\n',
    "cond_while_enum": 'enum C { A, B };\nc = C.A;\nwhile (c) { break; }\n',
    "cond_dowhile_enum": 'enum C { A, B };\nc = C.A;\ndo { break; } while (c);\n',
    "cond_for_enum": 'enum C { A, B };\nc = C.A;\nfor (i = 0; c; i = i + 1) { break; }\n',
    "cond_ternary_enum": 'enum C { A, B };\nc = C.A;\nx = (c ? 1 : 2);\nprint(to_str(x));\n',
    "cond_struct_if": 'struct P { x: int };\np = P(1);\nif (p) { print("x"); }\n',
    "logic_and_enum": 'enum C { A, B };\nc = C.A;\nprint(to_str(c && c));\n',
    "logic_or_enum": 'enum C { A, B };\nc = C.A;\nprint(to_str(c || c));\n',
    "logic_and_rhs": 'enum C { A, B };\nc = C.A;\nprint(to_str(true && c));\n',
    "comp_if_enum": 'enum C { A, B };\nc = C.A;\nl = [x for x in [1, 2] if c];\nprint(to_str(l));\n',
    # （解构 other 臂文案：interp 报类型名、VM 曾报值显示；G2.3 修 VM 为 type_name。
    #  既有 ds_notiter 用例被 print 的 H011 静态错误掩盖，此处单语句真走运行期。）
    "ds_notiter_run": 'a, b = 5;\n',
    "await_non_future": 'x = 5;\ny = await x;\nprint(y);\n',
    "break_outside": 'break;\n',
    # 内置 / 转换 / 重载
    "str_to_int": 'print(to_int("abc"));\n',
    "len_bad": 'print(len(5));\n',
    "append_bad": 'print(append(5, 1));\n',
    "dict_key_bool_get": 'd = {"a": 1};\nprint(d[true]);\n',
    "dict_key_bool_set": 'd = {"a": 1};\nd[true] = 2;\nprint(d);\n',
    "enum_payload_oob": 'enum E { A(1) }\ne = E.A(9);\nprint(e[5]);\n',
    "enum_for_in": 'enum E { A, B }\ne = E.A;\nfor x in e { print(x); }\n',
    "enum_len": 'enum E { A(float) }\ne = E.A(1.0);\nprint(len(e));\n',
    "enum_destruct": 'enum E { A(int) }\ne = E.A(1);\na, b = e;\n',
    # （enum 解构 `x, y = e`：G2.3 已修 VM 解构 other 臂文案（改 type_name），三引擎一致，
    #  故可固化——ds_notiter_run 覆盖同类非容器解构。）
    # G2.4 运算符重载错误路径：重载参数个数不匹配 / 重载体内报错 / native 错误不回退。
    # （这些路径只能经重载动态到达——普通调用的参数个数由 checker 静态拦。）
    "ov_arity_many": 'fn __add(a) { return "x"; }\nd = {"a": 1};\nprint(to_str(d + 1));\n',
    "ov_arity_missing": 'fn __add(a, b, c) { return to_str(c); }\nd = {"a": 1};\nprint(d + 1);\n',
    "ov_body_error": 'fn __add(a, b) { return 1 / 0; }\nd = {"a": 1};\nprint(to_str(d + 1));\n',
    "ov_native_div0": 'fn __div(a, b) { return "d"; }\nprint(1 / 0);\n',
    "lambda_arity": 'g = fn(a, b) { return a; };\nprint(to_str(g(1, 2, 3)));\n',
    # G2.5 别名：解析到不存在的名字（三引擎统一报 H002，指名道姓解析后的原名）
    "alias_undef": 'alias nope as nn;\nnn();\n',
    "alias_undef_builtin": 'alias nope2 as nn2;\nprint(to_str(nn2([1])));\n',
    "throw_num": 'throw 5;\n',
    # G3.1 构建期 import：本地模块缺失 → 三引擎 H404（VM/nat 构建期、解释器运行期，
    # 文案/定位（--> 行 + caret）一致；nat 的 [1/4] 阶段行由 strip_stage_lines 剥离）。
    "import_missing": 'import "nope" from "./no_such_mod.hn";\nprint(1);\n',
    # G2.9 点号内置族 H999 登记（仅 nat 产生，解释器/VM 正常执行——能力边界口径）
    "dotted_h999_regex": 'print(to_str(regex.match("a+", "aaa")));\n',
    "dotted_h999_sqlite": 'db = sqlite.open(":memory:");\nprint(to_str(db != 0));\n',
    "dotted_h999_http": 'print(http_get("http://127.0.0.1:1/x"));\n',
    # try/catch 上下文
    "try_catch_ctx": (
        'fn risky(x) { return 10 / x; }\n'
        'try {\n    r = risky(0);\n} catch e {\n    print(e.code);\n    print(e.message);\n}\n'
    ),
}

# natgen 能力边界：lowering 阶段报 H990/H999（M2 任务组待落地），无法比对运行期错误。
# 显式登记而非静默跳过——新增条目必须写明待落地任务号，避免「漏实现被当成通过」。
NAT_UNSUPPORTED = {
    # G2.9 点号内置族 H999 登记（std-only runtime 无法承载外部 crate 依赖的家族）：
    # interp/VM 正常执行、nat 有意报 H999——能力边界口径，跳过 nat 比对而非静默漏测。
    "dotted_h999_regex": "H999 原生编译后端暂不支持 regex.*（见 M2 G2.9 点号内置族：需 regex crate）",
    "dotted_h999_sqlite": "H999 原生编译后端暂不支持 sqlite.*（见 M2 G2.9 点号内置族：需 FFI libsqlite3）",
    "dotted_h999_http": "H999 原生编译后端暂不支持 http_get（见 M2 G2.9 点号内置族：需 rustls/TLS 栈）",
}


def run_hone(args, timeout=20):
    p = subprocess.run(args, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=timeout)
    return ((p.stdout or "") + (p.stderr or ""), p.returncode)


def strip_stage_lines(text: str) -> str:
    """剥掉 build 的 [N/4] 阶段进度行（余下报错本体应与 `hone run` 一致）。"""
    out = [ln for ln in text.splitlines() if not re.match(r"^\[\d/4\]", ln)]
    return "\n".join(out) + ("\n" if text.endswith("\n") else "")


def nat_run(path: str, tmp: str):
    """nat 第三引擎。返回 (结果, 状态)；结果 = (stdout+stderr, 退出码)。
    状态：ok（编译成功已跑产物）/ build_reject（checker 同源拒）/ unsupported（natgen 边界）
    """
    exe = os.path.join(tmp, "nat_case.exe")
    if os.path.exists(exe):
        os.remove(exe)
    obj = os.path.join(os.getcwd(), os.path.splitext(os.path.basename(path))[0] + ".nat.o")
    text, rc = run_hone([BIN, "build", "--exe", "--native", path, "-o", exe], timeout=120)
    if os.path.exists(obj):
        os.remove(obj)
    if rc == 0 and os.path.exists(exe):
        res = run_hone([exe])
        os.remove(exe)
        return res, "ok"
    if re.search(r"error\[H99[09]\]: natgen:", text):
        return (text, rc), "unsupported"
    return (strip_stage_lines(text), rc), "build_reject"


def main():
    have_nat = shutil.which("rustc") is not None
    if not have_nat:
        print("nat: 无 rustc，跳过第三引擎（仅比对 interp / VM）", file=sys.stderr)
    # 可选子集过滤：python tests/regress_err.py dotted_ 只跑名字含该子串的用例
    cases = CASES
    if len(sys.argv) > 1:
        cases = {k: v for k, v in CASES.items() if any(s in k for s in sys.argv[1:])}
    tmp = tempfile.mkdtemp(prefix="hone_err_")
    passed = failed = 0
    fails = []
    counts = {"ok": 0, "build_reject": 0, "unsupported": 0, "off": 0}
    try:
        for name, src in cases.items():
            path = os.path.join(tmp, name + ".hn")
            with open(path, "w", encoding="utf-8", newline="\n") as f:
                f.write(src)
            try:
                base = run_hone([BIN, "run", path])
                vm = run_hone([BIN, "run", "--vm", path])
            except subprocess.TimeoutExpired:
                failed += 1
                fails.append((name, "TIMEOUT", []))
                print(f"TIMEOUT  {name}")
                continue
            results = [("interp", base), ("vm", vm)]
            if have_nat:
                if name in NAT_UNSUPPORTED:
                    counts["off"] += 1
                else:
                    try:
                        nat, status = nat_run(path, tmp)
                    except subprocess.TimeoutExpired:
                        nat, status = ("[TIMEOUT]", -1), "ok"
                    counts[status] = counts.get(status, 0) + 1
                    results.append(("nat", nat))
            diffs = [(e, r[0]) for e, r in results[1:] if r != base]
            if not diffs:
                passed += 1
                print(f"PASS  {name}")
            else:
                failed += 1
                fails.append((name, base[0], diffs))
                print(f"FAIL  {name}")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)

    engines = "interp/VM" + ("/nat" if have_nat else "")
    print(f"\nPASS={passed} FAIL={failed} (共 {len(CASES)} 个错误用例, 引擎 {engines})")
    if have_nat:
        print(f"nat 路径分布: 运行期(ok)={counts['ok']} "
              f"checker同源拒(build_reject)={counts['build_reject']} "
              f"能力边界(unsupported/登记跳过)={counts['unsupported']} "
              f"显式跳过={counts['off']}")
    if NAT_UNSUPPORTED:
        print("nat 能力边界（显式登记，待落地后移除）:")
        for n, why in NAT_UNSUPPORTED.items():
            print(f"  {n}: {why}")
    for name, i, diffs in fails:
        print(f"\n--- {name} ---")
        print("interp:", repr(i))
        for eng, val in diffs:
            print(f"{eng:5s}:", repr(val))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())