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
BIN = os.environ.get(
    "HONE_BIN",
    os.path.join(os.getcwd(), "target", "release", "hone.exe" if os.name == "nt" else "hone"),
)

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
    # G2.9 后点号内置别名已可用（runtime + natgen 均有实现），见下方 DOTTED 组的 alias_dotted。
}

# ── 点号内置（G2.9）：time/path/stat/matrix/csv/diff/yaml/plot/glob/env/db/ptr/args ──
# 确定性输出为主（time.parse/format、纯计算、纯文本）；random/uuid/temp/server 等不确定
# 输出不固化（各自口径见进度文档 G2.9）。
DOTTED = {
    "time_parse": 'print(to_str(time.parse("2024-01-15T10:30:00Z")));\n',
    "time_format": 'print(time.format(1705314600, "YYYY-MM-DD HH:mm:SS"));\n',
    "time_add_diff": 'print(to_str(time.add(1000, 500)));\nprint(to_str(time.diff(1000, 400)));\n',
    "time_weekday": 'print(to_str(time.weekday(0)));\nprint(to_str(time.weekday(1705314600)));\n',
    "path_join": 'print(path.join("a", "b", "c.txt"));\n',
    "path_dirbase": 'print(path.dirname("a/b/c.txt"));\nprint(path.basename("a/b/c.txt"));\n',
    "path_nosep": 'print(path.dirname("file.txt"));\nprint(to_str(path.basename("/")));\n',
    "stat_int": 'print(to_str(stat.sum([1, 2, 3])));\nprint(to_str(stat.mean([1, 2, 3, 4])));\n',
    "stat_median": 'print(to_str(stat.median([3, 1, 2])));\nprint(to_str(stat.min([3, 1, 2])));\nprint(to_str(stat.max([3, 1, 2])));\n',
    "stat_var": 'print(to_str(stat.variance([1, 2, 3])));\nprint(to_str(stat.stddev([1, 2, 3])));\n',
    "stat_float": 'print(to_str(stat.sum([1.5, 2.5])));\n',
    "matrix_identity": 'print(to_str(matrix.identity(2)));\n',
    "matrix_transpose": 'print(to_str(matrix.transpose([[1, 2, 3], [4, 5, 6]])));\n',
    "matrix_add_scale": 'print(to_str(matrix.add([[1, 2], [3, 4]], [[10, 20], [30, 40]])));\nprint(to_str(matrix.scale([[1, 2], [3, 4]], 2)));\n',
    "matrix_mul": 'print(to_str(matrix.mul([[1, 2], [3, 4]], [[1, 0], [0, 1]])));\n',
    "csv_parse": 'print(csv.parse("a,b\n1,2\n"));\n',
    "csv_parse_dict": 'd = csv.parse_dict("h1,h2\nv1,v2\n");\nfor r in d { {h1, h2} = r; print(h1); print(h2); }\n',
    "csv_stringify": 'print(csv.stringify([["x", "y"], ["1", "2"]]));\n',
    "csv_quote": 'print(csv.stringify([["a,b", "c"]]));\n',
    "diff_lines": 'd = diff.lines("a\nb\nc", "a\nx\nc");\nfor it in d { {op, line} = it; print(op); }\n',
    "diff_unified": 'print(diff.unified("a\nb", "a\nx"));\n',
    "yaml_parse": 'y = yaml.parse("name: hone\nver: 7\nok: true\n");\n{name: n, ver, ok} = y;\nprint(n);\nprint(ver);\nprint(to_str(ok));\n',
    "yaml_stringify": 'print(yaml.stringify({"a": 1, "b": "x"}));\n',
    "plot_bar_len": 'svg = plot.bar([1, 2], ["a", "b"]);\nprint(len(svg) > 100);\n',
    "plot_line_len": 'svg = plot.line([1, 2, 3], [4, 5, 6]);\nprint(len(svg) > 100);\n',
    "glob_match": 'print(to_str(glob.match("*.hn", "main.hn")));\nprint(to_str(glob.match("src/**/*.rs", "src/sub/a.rs")));\nprint(to_str(glob.match("?.txt", "ab.txt")));\nprint(to_str(glob.match("[a-c].txt", "b.txt")));\n',
    "glob_nomatch": 'print(to_str(glob.match("*.hn", "a/b/main.hn")));\nprint(to_str(glob.match("[^a].txt", "a.txt")));\n',
    "env_set_get": 'env.set("HONE_PROBE", "v1");\nprint(env.get("HONE_PROBE"));\n',
    "env_get_missing": 'print(env.get("HONE_PROBE_MISSING_39"));\n',
    "db_set_get": 'db.set("k", "v");\nprint(db.get("k"));\n',
    "db_get_missing": 'print(to_str(db.get("missing39")));\n',
    "ptr_roundtrip": 'p = ptr.alloc(16);\nptr.write_int(p, 0, 4242);\nprint(to_str(ptr.read_int(p, 0)));\nprint(to_str(ptr.size(p)));\nprint(to_str(ptr.is_valid(p)));\nprint(to_str(ptr.free(p)));\n',
    "ptr_null": 'print(to_str(ptr.is_null(0)));\nprint(to_str(ptr.is_valid(0)));\n',
    "args_missing": 'print(to_str(args.has("nope39")));\nprint(to_str(args.get("nope39")));\nprint(to_str(args.get("nope39", "str", "dflt")));\n',
    "alias_dotted": 'alias time.weekday as wd;\nprint(to_str(wd(0)));\n',
}

# ── 默认参数（G2.8）：被调帧序言求默认值 ──
DEFAULTS = {
    "dflt_omit": 'fn f(a, b: int = 2) { return to_str(a + b); }\nprint(f(1));\n',
    "dflt_given": 'fn f(a, b: int = 2) { return to_str(a + b); }\nprint(f(1, 5));\n',
    "dflt_ref_prev": 'fn f(a, b: int = a * 10) { return to_str(a + b); }\nprint(f(2));\n',
    "dflt_chain": 'fn f(a, b: int = 2, c: int = b * 3) { return to_str(a + b + c); }\nprint(f(1));\nprint(f(1, 10));\n',
    "dflt_call": 'fn mk() { return 5; }\nfn f(a, b: int = mk()) { return to_str(a + b); }\nprint(f(1));\n',
    "dflt_lambda": 'l = fn(x, y: int = 7) { return to_str(x * y); };\nprint(l(3));\n',
    "dflt_capture": 'fn o() {\n  k = 4;\n  l = fn(x, y: int = k) { return to_str(x + y); };\n  return l(1);\n}\nprint(o());\n',
    "dflt_recursive": 'fn fib(n: int, m: int = 1) { if (n < 2) { return m; } return fib(n - 1, fib(n - 2, m)); }\nprint(to_str(fib(10)));\n',
    "dflt_types": 'fn f(a: str = "x", b: bool = false) { return a + to_str(b); }\nprint(f());\nprint(f("y", true));\n',
    # 动态调用路径（运算符重载）——绕过 checker 的静态参数检查
    "dflt_dynamic": 'fn __add(a, b: int = 2) { return "ov-add"; }\nd = {"a": 1};\nprint(to_str(d + 1));\n',
    "dflt_dynamic_missing": 'fn __add(a, b, c) { return to_str(c); }\nd = {"a": 1};\nprint(d + 1);\n',
    # 默认值引用全局 / 后置形参：checker 静态拒（三引擎同源）
    "dflt_ref_global": 'g = 5;\nfn f(a, b: int = g) { return to_str(b); }\nprint(f(1));\n',
    "dflt_ref_later": 'fn f(a, b: int = c, c: int = 3) { return to_str(b); }\nprint(f(1));\n',
}

# ── async-await / go 多线程（G2.6）────────────────────────────
# 确定性口径：单 go 线程（多线程输出顺序不确定）；子线程先睡先打、主线程睡更久保序。
ASYNC = {
    "async_await": 'async fn f(x) { return x * 2; }\nf = f(21);\nprint("before");\nprint(await f);\n',
    "async_call_direct": 'async fn g(n) { return n + 1; }\nprint(await g(5));\n',
    "async_two_futures": 'async fn add(x, k) { return x + k; }\na = add(1, 10);\nb = add(2, 20);\nprint(to_str(await a) + "," + to_str(await b));\n',
    "async_nested_fn": 'async fn s(x) { return x * x; }\nfn t() {\n  a = s(3);\n  return await a;\n}\nprint(t());\n',
    "async_err_propagate": 'async fn bad() { return 1 / 0; }\ntry {\n  r = await bad();\n  print("no");\n} catch e {\n  print(e.code + ":" + e.message);\n}\n',
    "async_throw_str": 'async fn t() { throw "boom"; }\ntry {\n  await t();\n} catch e {\n  print(e.message);\n}\n',
    "go_fire_forget": 'fn bg(n) { time.sleep(0.05); print("bg:" + to_str(n)); }\ngo bg(7);\ntime.sleep(0.3);\nprint("main");\n',
    "go_child_error": 'fn boom() { x = [1]; return x[9]; }\ngo boom();\ntime.sleep(0.2);\nprint("survived");\n',
    "future_display": 'async fn f() { return 1; }\nfut = f();\ntime.sleep(0.2);\nprint(to_str(fut));\n',
    "await_non_future": 'x = 5;\ny = await x;\nprint(y);\n',
    "await_after_done": 'async fn f() { return 3; }\nfut = f();\ntime.sleep(0.2);\nprint(await fut);\nprint(await fut);\n',
}

# ── 值语义 / 别名（引用模型重构基线）：变异前隔离必须保值语义 ──
# 锁定「b = a 共享后写 a 不影响 b」在三个引擎下逐字节一致——Value 换 Arc + 写点
# make_unique 门控（REFACTOR_PLAN.md）前后本组输出必须零 diff。
# dict 索引写被 checker 静态拒（同源），bytes 字面量 VM 报 H999，均不入本组；
# cow 声明专项已由 tests/smoke/rc_alias.test.hn 覆盖（且阶段 5 将废弃 cow）。
VALUE = {
    "val_alias_write": 'a = [1, 2];\nb = a;\na[0] = 9;\nprint(to_str(a));\nprint(to_str(b));\n',
    "val_alias_nested": 'm = [[1, 2], [3, 4]];\nn = m;\nm[0][1] = 9;\nprint(to_str(m));\nprint(to_str(n));\n',
    "val_alias_chain3": 't = [[[1]]];\nu = t;\nt[0][0][0] = 5;\nprint(to_str(t));\nprint(to_str(u));\n',
    "val_fn_pass": 'fn f(l) { l[0] = 9; return 0; }\na = [1, 2];\nf(a);\nprint(to_str(a));\n',
    "val_fn_pass_nested": 'fn f(l) { l[0][0] = 9; return 0; }\na = [[1]];\nf(a);\nprint(to_str(a));\n',
    "val_append_new": 'a = [1];\nb = append(a, 2);\nprint(to_str(a));\nprint(to_str(b));\n',
    "val_rebind_isolate": 'a = [1];\nb = a;\na = [9];\nprint(to_str(a));\nprint(to_str(b));\n',
    "val_loop_mutate_self": 'a = [1, 2, 3];\nfor (i = 0; i < len(a); i = i + 1) { a[i] = a[i] * 2; }\nprint(to_str(a));\n',
    "val_lambda_mutate_capture": 'fn mk() {\n  l = [1];\n  f = fn() { l[0] = 9; return l[0]; };\n  print(to_str(f()));\n  return l;\n}\nprint(to_str(mk()));\n',
    "val_two_alias_alternate": 'a = [1];\nb = a;\na[0] = 2;\nb[0] = 3;\nprint(to_str(a));\nprint(to_str(b));\n',
    "val_str_alias": 's = "ab";\nt = s;\ns = s + "c";\nprint(s);\nprint(t);\n',
    "val_elem_alias_write": 'a = [[1], [2]];\ne = a[1];\na[1][0] = 9;\nprint(to_str(e));\nprint(to_str(a));\n',
    "val_alias_read_only": 'a = [1, 2, 3];\nb = a;\nprint(to_str(b[0] + b[2]));\nprint(to_str(len(b)));\n',
    "val_deep_mixed": 'm = {"k": [1, 2]};\nv = 0;\nfor x in m { {k} = x; v = x; }\n{k2: lst} = v;\nlst[0] = 9;\nprint(to_str(m));\n',
}

GROUPS = {
    "defaults": DEFAULTS,
    "match": MATCH,
    "cond": COND,
    "logic": LOGIC,
    "destruct": DESTRUCT,
    "overload": OVERLOAD,
    "alias": ALIAS,
    "value": VALUE,
    "dotted": DOTTED,
    "async": ASYNC,
}

def run(cmd, timeout=30):
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=timeout)
    except subprocess.TimeoutExpired:
        return "<HANG: 死循环>", 99
    return (p.stdout or "") + (p.stderr or ""), p.returncode


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--only", choices=sorted(GROUPS), help="只跑某一组")
    args = ap.parse_args()

    if not os.path.exists(BIN):
        print(f"缺少 {BIN}：先 cargo build", file=sys.stderr)
        return 1

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
                res = {"interp": i, "vm": v}
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
