#!/usr/bin/env python3
"""错误对齐回归：逐用例比对「解释器」与「VM」的错误输出是否逐字节一致。

用法：在仓库根目录执行 python tests/regress_err.py
约定：任何一处差异都视为失败（报错必须精准，行/列/文案/help 全对齐）。
"""
import os
import subprocess
import sys
import tempfile

# 仓库根目录 = 本脚本所在目录（tests/）的上一级，换机器/换路径均可用。
os.chdir(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BIN = "./target/debug/hone.exe"

# 每个用例：期望两侧产生完全相同的 stdout+stderr（通常是运行期错误）。
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
    # 解构
    "ds_too_few": 'a, b = [1];\nprint(a, b);\n',
    "ds_notiter": 'a, b = 5;\nprint(a, b);\n',
    "ds_dict_missing": '{m} = {"a": 1};\nprint(m);\n',
    # match / await / 控制流
    "match_no_arm": 'x = 5;\nmatch x {\n    1 => { print("one"); }\n    2 => { print("two"); }\n}\n',
    "await_non_future": 'x = 5;\ny = await x;\nprint(y);\n',
    "break_outside": 'break;\n',
    # 内置 / 转换 / 重载
    "str_to_int": 'print(to_int("abc"));\n',
    "len_bad": 'print(len(5));\n',
    "append_bad": 'print(append(1, 2));\n',
    "dict_key_bool_get": 'd = {"a": 1};\nprint(d[true]);\n',
    "dict_key_bool_set": 'd = {"a": 1};\nd[true] = 2;\nprint(d);\n',
    "enum_payload_oob": 'enum E { A(1) }\ne = E.A(9);\nprint(e[5]);\n',
    "throw_num": 'throw 5;\n',
    # try/catch 上下文
    "try_catch_ctx": (
        'fn risky(x) { return 10 / x; }\n'
        'try {\n    r = risky(0);\n} catch e {\n    print(e.code);\n    print(e.message);\n}\n'
    ),
}


def run(args):
    p = subprocess.run(args, capture_output=True, text=True, timeout=20)
    return p.stdout + p.stderr


def main():
    tmp = tempfile.mkdtemp(prefix="hone_err_")
    passed = failed = 0
    fails = []
    for name, src in CASES.items():
        path = os.path.join(tmp, name + ".hn")
        with open(path, "w", encoding="utf-8") as f:
            f.write(src)
        try:
            interp = run([BIN, "run", path])
            vm = run([BIN, "run", "--vm", path])
        except subprocess.TimeoutExpired:
            failed += 1
            fails.append((name, "TIMEOUT", "", ""))
            continue
        if interp == vm:
            passed += 1
            print(f"PASS  {name}")
        else:
            failed += 1
            fails.append((name, interp, vm))
            print(f"FAIL  {name}")

    print(f"\nPASS={passed} FAIL={failed} (共 {len(CASES)} 个错误用例)")
    for name, i, v in fails:
        print(f"\n--- {name} ---")
        print("interp:", repr(i))
        print("vm    :", repr(v))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
