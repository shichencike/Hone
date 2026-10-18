#!/usr/bin/env python3
"""错误对齐审计探针：批量比对 nat 与 interp 的错误输出（文案 + help 行 + 退出码）。

覆盖 runtime 各错误臂（算术 / 比较 / 索引 / len / append / keys / values / has_key /
to_int / to_float / abs / assert / 类型谓词 / 字段 / 解构 / 迭代 / 溢出 / throw / 跨帧），
一次性暴露 help 缺失与文案偏差。

用法：在仓库根目录执行 python tests/tools/audit_err.py
定位：属**开发期扫雷**工具（比regress_err 覆盖更广、但每次要编译 55 个 nat 产物，慢），
不进常规回归；改动 runtime 错误臂 / natgen lowering 后跑一次，确认无新增差异。
"""
import os
import subprocess
import sys
import tempfile

# 仓库根目录 = 本脚本（tests/tools/）所在目录的上两级，换机器/换路径均可用。
os.chdir(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))
BIN = os.environ.get(
    "HONE_BIN",
    os.path.abspath("target/release/hone.exe" if os.name == "nt" else "target/release/hone"),
)

CASES = {
    # 算术 / 除零 / 溢出
    "div_zero": 'x = 0;\nprint(10 / x);\n',
    "fdiv_zero": 'print(1.0 / 0.0);\n',
    "mod_zero": 'print(7 % 0);\n',
    "int_ovf_add": 'print(9223372036854775807 + 1);\n',
    "int_ovf_sub": 'print(-9223372036854775807 - 2);\n',
    "int_ovf_mul": 'print(4611686018427387904 * 4);\n',
    "abs_ovf": 'print(abs(-9223372036854775807 - 1));\n',
    # 类型错误
    "add_int_str": 'print(1 + "a");\n',
    "sub_bool": 'print(true - false);\n',
    "neg_str": 'x = "a";\nprint(-x);\n',
    "not_int": 'print(!5);\n',
    "cmp_str_lt": 'print("a" < "b");\n',
    "cmp_bool_lt": 'print(true < false);\n',
    # 索引
    "idx_list_oob": 'l = [1, 2];\nprint(l[9]);\n',
    "idx_list_neg": 'l = [1, 2];\nprint(l[-1]);\n',
    "idx_str_oob": 's = "ab";\nprint(s[5]);\n',
    "idx_str_neg": 's = "ab";\nprint(s[-1]);\n',
    "idx_int": 'n = 5;\nprint(n[0]);\n',
    "idx_set_oob": 'l = [1];\nl[7] = 9;\n',
    "idx_dict_bool": 'd = {"a": 1};\nprint(d[true]);\n',
    # 内置
    "len_int": 'print(len(5));\n',
    "len_null": 'print(len(null));\n',
    "append_int": 'print(append(5, 1));\n',
    "keys_list": 'print(keys([1, 2]));\n',
    "values_list": 'print(values([1, 2]));\n',
    "has_key_list": 'print(has_key([1], "a"));\n',
    "to_int_bad": 'print(to_int("abc"));\n',
    "to_int_null": 'print(to_int(null));\n',
    "to_float_bad": 'print(to_float("zz"));\n',
    "to_str_none": 'print(to_str(null));\n',
    "abs_str": 'print(abs("x"));\n',
    "max_mixed": 'print(max(1, "a"));\n',
    "min_mixed": 'print(min("a", 1));\n',
    "assert_fail": 'assert(1 == 2, "must be equal");\n',
    "assert_eq_fail": 'assert_eq(1, 2);\n',
    "assert_str_cond": 'assert("x");\n',
    "type_of_ok": 'print(type_of(1));\n',
    # 字段
    "field_dict_missing": 'd = {"a": 1};\nprint(d.zzz);\n',
    "field_dict_ok": 'd = {"a": 1};\nprint(d.a);\n',
    "field_int": 'n = 5;\nprint(n.a);\n',
    "field_list": 'l = [1];\nprint(l.a);\n',
    "field_err_missing": 'try { x = 1 / 0; } catch e { print(e.zzz); }\n',
    # 解构
    "ds_list_short": 'a, b = [1];\n',
    "ds_list_got_dict": 'a, b = {"x": 1};\n',
    "ds_dict_got_list": '{c, d} = [1, 2];\n',
    "ds_dict_missing": '{m} = {"a": 1};\n',
    "ds_int": 'a, b = 5;\n',
    "ds_key_int": '{c, d} = {1: 2};\n',
    # 迭代 / 推导
    "for_int": 'for v in 5 { print(v); }\n',
    "comp_int": 'z = [i for i in 5];\n',
    "for_dict_onevar": 'for k in {"a": 1} { print(k); }\n',
    # throw / 跨帧
    "throw_str": 'throw "boom";\n',
    "throw_rethrow": 'fn f() { throw "inner"; }\ntry { f(); } catch e { print(e.message); }\n',
    "fn_div_cross_frame": 'fn f(x) { return 10 / x; }\nprint(f(0));\n',
    # 溢出边界
    "int_min_div": 'x = -9223372036854775807 - 1;\nprint(-x);\n',
}


def run(args, t=30):
    p = subprocess.run(args, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=t)
    return ((p.stdout or "") + (p.stderr or ""), p.returncode)


def strip_stage(text):
    out = [ln for ln in text.splitlines() if not __import__("re").match(r"^\[\d/4\]", ln)]
    return "\n".join(out) + ("\n" if text.endswith("\n") else "")


tmp = tempfile.mkdtemp(prefix="hone_audit_")
same = diff = unsup = 0
diffs = []
for name, src in CASES.items():
    p = os.path.join(tmp, name + ".hn")
    open(p, "w", encoding="utf-8", newline="\n").write(src)
    exe = os.path.join(tmp, "a.exe")
    if os.path.exists(exe):
        os.remove(exe)
    ip = run([BIN, "run", os.path.abspath(p)])
    bp = run([BIN, "build", "--exe", "--native", os.path.abspath(p), "-o", exe], t=120)
    txt, rc = bp
    if rc == 0 and os.path.exists(exe):
        nat = run([exe])
        os.remove(exe)
    elif "error[H99" in txt and "natgen:" in txt:
        unsup += 1
        diffs.append((name, "UNSUPPORTED", txt.strip().splitlines()[-2] if txt.strip() else ""))
        continue
    else:
        nat = (strip_stage(txt), rc)
    if nat == ip:
        same += 1
    else:
        diff += 1
        diffs.append((name, "DIFF", (ip, nat)))

print(f"一致 {same} / 差异 {diff} / 未支持 {unsup}  共 {len(CASES)}\n")
for name, kind, payload in diffs:
    print(f"== {name}  [{kind}]")
    if kind == "DIFF":
        i, n = payload
        il, nl = i[0].splitlines(), n[0].splitlines()
        for k in range(max(len(il), len(nl))):
            a = il[k] if k < len(il) else "<缺>"
            b = nl[k] if k < len(nl) else "<缺>"
            if a != b:
                print(f"   interp: {a!r}")
                print(f"   nat   : {b!r}")
        if i[1] != n[1]:
            print(f"   退出码 interp={i[1]} nat={n[1]}")
    else:
        print(f"   {payload}")