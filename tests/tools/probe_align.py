#!/usr/bin/env python3
"""三引擎语义对齐探针：match / 条件 / 逻辑 / 解构 四组形态的 interp-vs-VM(-nat) 逐字节比对。

用法（仓库根目录执行）：
    python tests/tools/probe_align.py                # interp vs VM（快，无构建）
    python tests/tools/probe_align.py --nat          # 追加 nat 第三引擎（每例单独 build，慢）
    python tests/tools/probe_align.py --only logic   # 只跑某一组（match / cond / logic / destruct）

用途：G2.3 审计据此一次性扫出 6 处 interp-VM 分歧（match 无匹配错误码 / 绑定变量泄漏 /
lambda 捕获取外层 / 条件 truthy 放行 / && || 无短路 / 解构文案），与 audit_err.py 互补——
本探针盯「正常路径输出 + 动态类型值下的条件与逻辑」，audit_err.py 盯「错误文案 + help 行」。

约定：任一组出现 DIVERGE 即为引擎缺陷（违反「interp 与 VM 逐字节一致」铁律），
修完再跑；新增形态用例请追加到对应组，勿删既有用例（它们是回归基线）。
"""
import argparse
import os
import re
import subprocess
import sys
import tempfile

os.chdir(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))
BIN = os.path.join(os.getcwd(), "target", "debug", "hone.exe")

ENUM_H = "enum C{A,B}\nc=C.A;\n"

# ── match：字面量 / 变体 / 绑定 / 通配 / 作用域 / 闭包 ──
MATCH = {
    "lit_int": 'x = 5;\nr = match x { 1 => "one", 5 => "five", _ => "other" };\nprint(r);\n',
    "lit_str": 's = "b";\nr = match s { "a" => "A", "b" => "B", _ => "?" };\nprint(r);\n',
    "lit_bool": 'b = true;\nprint(to_str(match b { true => 1, false => 0 }));\n',
    "lit_float": 'f = 1.5;\nprint(to_str(match f { 1.5 => 1, _ => 0 }));\n',
    "lit_char": "c = 'y';\nprint(to_str(match c { 'x' => 1, 'y' => 2, _ => 3 }));\n",
    "wild_first": 'x = 5;\nprint(match x { _ => "wild", 5 => "five" });\n',
    "no_wild_hit": 'x = 5;\nprint(to_str(match x { 4 => 1, 5 => 2 }));\n',
    "wild_dup": 'x = 5;\nprint(to_str(match x { _ => 1, _ => 2 }));\n',
    "nomatch_lit": 'x = 3;\nr = match x { 1 => "a", 2 => "b" };\nprint(r);\n',
    "nomatch_enum": ENUM_H + 'x = match c { C.B => 1 };\nprint(to_str(x));\n',
    "bind_two": 'enum Sh { Rect(float, float) };\nsh = Sh.Rect(2.0, 3.0);\nprint(to_str(match sh { Sh.Rect(w, h) => w + h, _ => 0.0 }));\n',
    "bind_ignore": 'enum Sh { Rect(float, float) };\nsh = Sh.Rect(2.0, 3.0);\nprint(to_str(match sh { Sh.Rect(_, h) => h, _ => 0.0 }));\n',
    "bind_shadow": 'enum S { C(int) };\nrr = 9;\ns = S.C(5);\nprint(to_str(match s { S.C(rr) => rr, _ => 0 }));\nprint(to_str(rr));\n',
    "bind_then_outer": 'enum S { C(int) };\nrr = 9;\ns = S.C(5);\nprint(to_str((match s { S.C(rr) => rr, _ => 0 }) + rr));\n',
    "lambda_cap_bind": 'enum S { C(int) };\nouter = 7;\ns = S.C(5);\nf = match s { S.C(outer) => (fn() { return outer; }), _ => (fn() { return 0; }) };\nprint(to_str(f()));\n',
    "nested": 'enum O { S(int), N };\nprint(match O.S(3) { O.S(n) => match n { 3 => "three", _ => "other" }, O.N => "none" });\n',
    "cross_enum": 'enum C { A }\nenum D { A }\nc = C.A;\nprint(to_str(match c { D.A => 1, _ => 0 }));\n',
    "in_fn": 'enum O { S(int), N };\nfn f(o) { return match o { O.S(n) => n * 2, O.N => 0 }; }\nprint(to_str(f(O.S(4))));\n',
    "in_loop": 't = 0;\nfor (i = 0; i < 4; i = i + 1) { t = t + match i { 0 => 10, 1 => 20, _ => 1 }; }\nprint(to_str(t));\n',
    "as_arg": 'x = 1;\nprint(to_str(len(match x { 1 => [1, 2], _ => [] })));\n',
}

# ── 条件：六处站点（if/while/do-while/for/ternary/推导式 if）的严格 bool ──
COND = {
    "if_enum": ENUM_H + 'if (c) { print("x"); }\n',
    "if_int": 'x = 5;\nif (x) { print("x"); }\n',          # checker 静态拒（三引擎同源）
    "if_struct": 'struct P { x: int };\np = P(1);\nif (p) { print("x"); }\n',
    "if_str": 's = "a";\nif (s) { print("x"); }\n',         # checker 静态拒
    "while_enum": ENUM_H + 'while (c) { break; }\n',
    "dowhile_enum": ENUM_H + 'do { break; } while (c);\n',
    "for_enum": ENUM_H + 'for (i = 0; c; i = i + 1) { break; }\n',
    "ternary_enum": ENUM_H + 'x = (c ? 1 : 2);\nprint(to_str(x));\n',
    "comp_if_enum": ENUM_H + 'l = [x for x in [1, 2] if c];\nprint(to_str(l));\n',
    "not_enum": ENUM_H + 'print(!c);\n',
    "if_bool_ok": 'x = 5;\nif (x > 1) { print("ok"); }\n',
}

# ── 逻辑：&&/|| 短路求值 + 操作数严格 bool ──
LOGIC = {
    "short_and": 'fn f() { print("side"); return true; }\nprint(to_str(false && f()));\n',
    "short_or": 'fn f() { print("side"); return true; }\nprint(to_str(true || f()));\n',
    "eval_and": 'fn f() { print("side"); return true; }\nprint(to_str(true && f()));\n',
    "eval_or": 'fn f() { print("side"); return true; }\nprint(to_str(false || f()));\n',
    "short_div0": 'print(to_str(false && (1 / 0 == 0)));\n',
    "short_or_div0": 'print(to_str(true || (1 / 0 == 0)));\n',
    "and_enum_enum": ENUM_H + 'print(to_str(c && c));\n',
    "or_enum_enum": ENUM_H + 'print(to_str(c || c));\n',
    "and_rhs_enum": ENUM_H + 'print(to_str(true && c));\n',
    "or_rhs_enum": ENUM_H + 'print(to_str(false || c));\n',
    "and_bool_rhs": ENUM_H + 'print(to_str(c && true));\n',
    "or_bool_rhs": ENUM_H + 'print(to_str(c || false));\n',
    "prec": 't = true;\nf = false;\nprint(to_str(t || f && f));\nprint(to_str(f && t || t));\nprint(to_str((t || f) && f));\n',
    "normal": 'x = 5;\nprint(to_str(x > 1 && x < 10));\nprint(to_str(x < 1 || x == 5));\nprint(to_str(!(x == 4)));\n',
}

# ── 解构：other 臂文案（type_name vs value_to_str）──
DESTRUCT = {
    "ds_int": 'a, b = 5;\n',
    "ds_enum": ENUM_H + 'a, b = c;\n',
    "ds_static_masked": 'a, b = 5;\nprint(a, b);\n',   # print H011 静态错误掩盖运行期路径
    "ds_short": 'x, y = [1];\nprint(to_str(y));\n',
    "ds_ok": 'a, b = [1, 2];\nprint(to_str(a + b));\n',
}

# ── 运算符重载（G2.4）：各 __op 回退 + native 组合/错误不回退 + 重载内报错 ──
_OV = """fn __add(a, b) { return "add"; }
fn __sub(a, b) { return "sub"; }
fn __mul(a, b) { return "mul"; }
fn __div(a, b) { return "div"; }
fn __mod(a, b) { return "mod"; }
fn __eq(a, b) { return true; }
fn __ne(a, b) { return false; }
fn __lt(a, b) { return true; }
fn __le(a, b) { return true; }
fn __gt(a, b) { return true; }
fn __ge(a, b) { return true; }
fn __neg(a) { return "neg"; }
fn __not(a) { return "not"; }
fn __len(a) { return 42; }
fn __index(a, i) { return "idx"; }
"""
OVERLOAD = {
    "bin_add": _OV + 'd = {"a": 1};\nprint(to_str(d + 1));\n',
    "bin_sub": _OV + 'd = {"a": 1};\nprint(to_str(d - 1));\n',
    "bin_mul": _OV + 'd = {"a": 1};\nprint(to_str(d * 2));\n',
    "bin_div": _OV + 'd = {"a": 1};\nprint(to_str(d / 2));\n',
    "bin_mod": _OV + 'd = {"a": 1};\nprint(to_str(d % 2));\n',
    "cmp_eq": _OV + 'print(to_str(1 == "a"));\n',
    "cmp_ne": _OV + 'print(to_str(1 != "a"));\n',
    "cmp_mix": _OV + 'print(to_str("a" < 1));\nprint(to_str(1 >= "a"));\n',
    "un_neg": _OV + 'print(-"a");\n',
    "un_not": _OV + 'print(!5);\n',
    "b_len": _OV + 'print(to_str(len(5)));\n',
    "b_index": _OV + 'print("a"[0]);\n',
    # native 组合 / native 错误**不**回退
    "native_int_add": _OV + 'print(1 + 2);\n',
    "native_str_add": _OV + 'print("a" + "b");\n',
    "native_str_mul": _OV + 'print("a" * "b");\n',
    "native_div0": _OV + 'print(1 / 0);\n',
    "native_mod0": _OV + 'print(1 % 0);\n',
    "native_overflow": _OV + 'print(9223372036854775807 + 1);\n',
    # 越界索引是 H001 → 仍回退（VM 语义），三引擎须一致
    "oob_list_index": _OV + 'l = [1, 2];\nprint(l[9]);\n',
    "oob_str_index": _OV + 'print("ab"[9]);\n',
    # 未定义 __op 时错误原样
    "no_ov_add": 'd = {"a": 1};\nprint(to_str(d + 1));\n',
    "no_ov_len": 'print(to_str(len(5)));\n',
    "no_ov_index": 'print("a")[0];\n',
    "no_ov_neg": 'print(-"a");\n',
    "no_ov_eq": 'print(to_str(1 == "a"));\n',
    # 重载体内报错 / 参数个数不匹配（span 与文案三引擎对齐）
    "ov_raises": 'fn __add(a, b) { return 1 / 0; }\nd = {"a": 1};\nprint(to_str(d + 1));\n',
    "ov_too_many": 'fn __add(a) { return "x"; }\nd = {"a": 1};\nprint(to_str(d + 1));\n',
    "ov_missing": 'fn __add(a, b, c) { return to_str(c); }\nd = {"a": 1};\nprint(d + 1);\n',
    # 重载参与条件 / 短路 / struct / enum
    "ov_in_cond": _OV + 'd = {"a": 1};\nif (d == 1) { print("eq-true"); }\n',
    "ov_in_short": _OV + 'd = {"a": 1};\nprint(to_str(false && (d == 1)));\n',
    "ov_struct": 'struct P { x: int };\nfn __add(a, b) { return "p-add"; }\np = P(1);\nprint(to_str(p + 1));\n',
    "ov_enum": 'enum E { A, B }\nfn __add(a, b) { return "e-add"; }\ne = E.A;\nprint(to_str(e + 1));\n',
}

# ── alias（别名解析）与 goto（标签跳转）：G2.5 ──
ALIAS = {
    "alias_builtin": 'alias print as p;\np("hi");\n',
    "alias_len": 'alias len as mylen;\nprint(to_str(mylen([1, 2, 3])));\n',
    "alias_user": 'fn f(x) { return x * 2; }\nalias f as g;\nprint(to_str(g(5)));\n',
    "alias_chain": 'fn f(x) { return x * 2; }\nalias f as g;\nalias g as h;\nprint(to_str(h(5)));\n',
    "alias_struct": 'struct P { x: int };\nalias P as Pt;\np = Pt(7);\nprint(to_str(p.x));\n',
    # 别名指向持有 lambda 的变量（G2.5：解释器原先报 H002、VM 可用，nat 需编译期解析）
    "alias_var": 'gvar = fn(k) { return k + 1; };\nalias gvar as g2;\nprint(to_str(g2(1)));\n',
    "alias_undef": 'alias nope as nn;\nnn();\n',
    # goto：向后跳 / 跳出循环 / 跳出嵌套循环 / 跳出 try
    "goto_back": 'i = 0;\nagain:\nprint(to_str(i));\ni = i + 1;\nif (i < 3) { goto again; }\nprint("done");\n',
    "goto_out_loop": 'for (i = 0; i < 5; i = i + 1) {\n  if (i == 2) { goto out; }\n  print(to_str(i));\n}\nout:\nprint("out");\n',
    "goto_nested": 'j = 0;\nfor (a = 0; a < 3; a = a + 1) {\n  for (b = 0; b < 3; b = b + 1) {\n    j = j + 1;\n    if (b == 1) { goto done; }\n  }\n}\ndone:\nprint(to_str(j));\n',
    "goto_in_try": 'try {\n  goto skip;\n  print("nope");\n} catch e {\n  print("caught");\n}\nskip:\nprint("after");\n',
    # 注：`alias time.now as t`（点号内置别名）不固化——runtime 尚无点号内置实现（G2.9），
    # 直接写 time.now() 也是 H404，与 alias 无关。
}

GROUPS = {
    "match": MATCH,
    "cond": COND,
    "logic": LOGIC,
    "destruct": DESTRUCT,
    "overload": OVERLOAD,
    "alias": ALIAS,
}

def run(cmd, timeout=30):
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        return "<HANG: 死循环>", 99
    return p.stdout + p.stderr, p.returncode


def strip_stage_lines(text: str) -> str:
    """剥掉 build 的 [N/4] 阶段进度行。

    checker 静态拒的脚本三引擎都会拒（checker 是共享前端），但 nat 走 build 管线，
    报错前后会夹阶段进度行——不剥就会把「同源拒」误判成分歧（与 regress_err.py 同口径）。
    """
    out = [ln for ln in text.splitlines() if not re.match(r"^\[\d/4\]", ln)]
    return "\n".join(out) + ("\n" if text.endswith("\n") else "")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--nat", action="store_true", help="追加 nat 第三引擎（慢：每例单独 build）")
    ap.add_argument("--only", choices=sorted(GROUPS), help="只跑某一组")
    args = ap.parse_args()

    if not os.path.exists(BIN):
        print(f"缺少 {BIN}：先 cargo build", file=sys.stderr)
        return 1
    if args.nat and not any(
        os.path.exists(os.path.join(p, "rustc.exe")) for p in os.environ.get("PATH", "").split(os.pathsep)
    ):
        print("nat: 无 rustc，跳过第三引擎", file=sys.stderr)

    tmp = tempfile.mkdtemp(prefix="hone_probe_")
    bad = total = 0
    try:
        for gname, cases in GROUPS.items():
            if args.only and gname != args.only:
                continue
            print(f"── {gname} ──")
            for name, src in cases.items():
                total += 1
                path = os.path.join(tmp, "t.hn")
                with open(path, "w", encoding="utf-8", newline="\n") as f:
                    f.write(src)
                i = run([BIN, "run", path])
                v = run([BIN, "run", "--vm", path])
                n = None
                if args.nat:
                    exe = os.path.join(tmp, "nat.exe")
                    b = run([BIN, "build", "--exe", "--native", path, "-o", exe], timeout=180)
                    if b[1] == 0 and os.path.exists(exe):
                        n = run([exe])
                        os.path.exists(exe) and os.remove(exe)
                    else:
                        # 编译期拒（含 checker 同源拒与 natgen H990 边界）：剥阶段行后比对。
                        n = (strip_stage_lines(b[0]), b[1])
                    obj = os.path.join(
                        os.getcwd(), os.path.splitext(os.path.basename(path))[0] + ".nat.o"
                    )
                    os.path.exists(obj) and os.remove(obj)
                res = {"interp": i, "vm": v}
                if n is not None:
                    res["nat"] = n
                diffs = [(e, r) for e, r in res.items() if e != "interp" and r != i]
                if diffs:
                    bad += 1
                    print(f"[DIVERGE] {gname}.{name}")
                    print(f"    interp: {i!r}")
                    for e, r in diffs:
                        print(f"    {e:7s}: {r!r}")
                else:
                    print(f"[SAME] {gname}.{name}")
    finally:
        import shutil

        shutil.rmtree(tmp, ignore_errors=True)
    print(f"\n共 {total} 例，分歧 {bad}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
