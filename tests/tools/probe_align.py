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

# ── 切片（T1）：负索引归一 / 越界钳制 / lo>hi 空副本 / str 不支持切片 / bytes 切片 ──
SLICE = {
    "slice_basic": 'l = [1, 2, 3, 4, 5];\nprint(to_str(l[1:3]));\n',
    "slice_lo_only": 'l = [1, 2, 3];\nprint(to_str(l[1:]));\n',
    "slice_hi_only": 'l = [1, 2, 3];\nprint(to_str(l[:2]));\n',
    "slice_neg_lo": 'l = [1, 2, 3, 4];\nprint(to_str(l[-2:]));\n',
    "slice_neg_hi": 'l = [1, 2, 3, 4];\nprint(to_str(l[:-1]));\n',
    "slice_hi_clamp": 'l = [1, 2, 3];\nprint(to_str(l[1:100]));\n',
    "slice_lo_clamp": 'l = [1, 2, 3];\nprint(to_str(l[-100:]));\n',
    "slice_empty": 'l = [1, 2];\nprint(to_str(l[3:]));\n',
    "slice_lo_gt_hi": 'l = [1, 2, 3];\nprint(to_str(l[2:1]));\n',
    "slice_neg_both": 'l = [1, 2, 3, 4, 5];\nprint(to_str(l[-3:-1]));\n',
    "slice_neg_clamp": 'l = [1, 2];\nprint(to_str(l[-5:-1]));\n',
    "slice_bytes": 'b = b"ABCDE";\nprint(to_str(b[1:3]));\n',
    "slice_bytes_neg": 'b = b"ABC";\nprint(to_str(b[-2:]));\n',
    "slice_bytes_empty": 'b = b"AB";\nprint(to_str(b[5:]));\n',
    "slice_slice_chain": 'l = [[1, 2, 3], [4, 5]];\nprint(to_str(l[0][1:]));\n',
    "slice_var_bounds": 'l = [1, 2, 3, 4];\ni = 1;\nj = 3;\nprint(to_str(l[i:j]));\n',
    "slice_expr_bounds": 'l = [1, 2, 3, 4, 5];\nprint(to_str(l[1 + 1:2 * 2]));\n',
    "slice_in_fn": 'fn f(a) { return a[1:]; }\nprint(to_str(f([7, 8, 9])));\n',
    "slice_copy_semantics": 'a = [1, 2, 3];\nb = a[0:2];\nb[0] = 9;\nprint(to_str(a));\nprint(to_str(b));\n',
    "slice_err_str": 's = "abc";\nprint(to_str(s[0:2]));\n',
    "slice_err_dict": 'd = {"a": 1};\nprint(to_str(d[0:1]));\n',
    "slice_err_int": 'x = 5;\nprint(to_str(x[0:1]));\n',
    "slice_err_float_bound": 'l = [1, 2];\nprint(to_str(l[0.5:]));\n',
    "slice_err_str_bound": 'l = [1, 2];\nprint(to_str(l["a":]));\n',
}

# ── byte 字面量（T2）：display / 严格隔离 / 容器承载 ──
BYTE = {
    "byte_lit": 'b = 0b01000001;\nprint(b);\n',
    "byte_zero": 'b = 0b0;\nprint(b);\n',
    "byte_max": 'b = 0b11111111;\nprint(b);\n',
    "byte_var": 'b = 0b1;\nprint(b);\nc = b;\nprint(c);\n',
    "byte_eq": 'b = 0b1;\nc = 0b1;\nprint(to_str(b == c));\n',
    "byte_ne": 'b = 0b1;\nc = 0b10;\nprint(to_str(b != c));\n',
    "byte_cmp": 'b = 0b1;\nc = 0b10;\nprint(to_str(b < c));\n',
    "byte_in_list": 'l = [0b1, 0b10];\nprint(to_str(l));\n',
    "byte_index_out": 'b = b"AB";\nprint(b[0]);\nprint(b[1]);\n',
    "byte_forin_bytes": 't = "";\nfor x in b"AB" { t = t + to_str(x); }\nprint(t);\n',
    "byte_err_add_int": 'b = 0b1;\nprint(b + 1);\n',
    "byte_err_cmp_int": 'b = 0b1;\nprint(to_str(b == 1));\n',
    "byte_err_overflow": 'b = 0b111111111;\nprint(b);\n',
}

# ── bytes 字面量（T2）：display / 拼接 / 索引 / for-in / len / 容器承载 ──
BYTES = {
    "bytes_lit": 'b = b"hi";\nprint(b);\n',
    "bytes_empty": 'b = b"";\nprint(b);\nprint(to_str(len(b)));\n',
    "bytes_concat": 'b = b"ab" + b"cd";\nprint(b);\n',
    "bytes_index": 'b = b"A";\nprint(b[0]);\n',
    "bytes_len": 'b = b"abc";\nprint(to_str(len(b)));\n',
    "bytes_eq": 'b = b"x";\nc = b"x";\nprint(to_str(b == c));\n',
    "bytes_ne": 'b = b"x";\nc = b"y";\nprint(to_str(b != c));\n',
    "bytes_in_list": 'l = [b"a", b"b"];\nprint(to_str(l));\n',
    "bytes_nonascii": 'b = b"\\x00\\xff";\nprint(b);\n',
    "bytes_forin": 't = 0;\nfor x in b"AB" { t = t + 1; }\nprint(to_str(t));\n',
    "bytes_slice_chain": 'b = b"ABCD";\nprint(to_str(b[1:3] + b"!"));\n',
    "bytes_err_add_str": 'b = b"a";\nprint(b + "s");\n',
    "bytes_err_index_str": 'b = b"ab";\nprint(b["k"]);\n',
    "bytes_err_oob": 'b = b"a";\nprint(b[5]);\n',
}

# ── 字段赋值（T3）：dict/struct 写 / readonly / 未知字段 / 隐藏键 / 嵌套链 / 值语义 ──
FIELD = {
    "field_dict_write": 'd = {"a": 1};\nd.a = 9;\nprint(to_str(d));\n',
    "field_dict_overwrite": 'd = {"a": 1, "b": 2};\nd.b = d.a + 1;\nprint(to_str(d));\n',
    "field_struct_write": 'struct P { x: int };\np = P(1);\np.x = 99;\nprint(to_str(p.x));\n',
    "field_struct_all": 'struct P { x: int, y: int };\np = P(1, 2);\np.x = 10;\np.y = 20;\nprint(to_str(p));\n',
    "field_nested_chain": 'n = {"inner": {"v": 1}};\nn.inner.v = 7;\nprint(to_str(n));\n',
    "field_nested_struct": 'struct Q { v: int };\nstruct R { q: Q };\nr = R(Q(1));\nr.q.v = 3;\nprint(to_str(r.q.v));\n',
    "field_value_semantics": 'd = {"a": 1};\nb = d;\nd.a = 55;\nprint(to_str(b));\nprint(to_str(d));\n',
    "field_value_nested": 'n = {"inner": {"v": 1}};\nm = n;\nn.inner.v = 9;\nprint(to_str(m));\nprint(to_str(n));\n',
    "field_in_fn": 'fn f(d) { d.a = 5; return 0; }\nd = {"a": 1};\nf(d);\nprint(to_str(d));\n',
    "field_undefined_base": 'nope.f = 1;\n',
    "field_err_unknown": 'd = {"a": 1};\nd.zz = 2;\n',
    "field_err_struct_unknown": 'struct P { x: int };\np = P(1);\np.zz = 2;\n',
    "field_err_readonly": 'struct P { x: int, y: readonly int };\np = P(1, 2);\np.y = 5;\n',
    "field_err_noncontainer": 'x = 5;\nx.f = 1;\n',
    "field_err_str_base": 's = "ab";\ns.f = 1;\n',
    "field_err_list_base": 'l = [1];\nl.f = 1;\n',
    "field_err_list_mid": 'n = {"inner": [1]};\nn.inner.v = 2;\n',
    "field_err_int_mid": 'n = {"inner": 5};\nn.inner.v = 2;\n',
}

# ── with（T5）：Int 资源句柄 / 非上下文报错 / return / break / 穿透 ──
# __exit__ 抛错与嵌套 type 实例路径依赖 T6（new Type 构造），T6 落地后扩容本组。
WITH = {
    "with_int_resource": 'with 5 as x {\n  print(x);\n}\nprint("done");\n',
    "with_no_bind": 'with 5 {\n  print("in");\n}\nprint("out");\n',
    "with_int_nonresource": 'with 999 as h {\n  print(h);\n}\nprint("closed-silently");\n',
    "with_nested": 'with 1 as a {\n  with 2 as b {\n    print(a + b);\n  }\n  print("inner-done");\n}\nprint("outer-done");\n',
    "with_body_error": 'with 5 as x {\n  x = x[0];\n}\n',
    "with_err_str_target": 'with "s" as x {\n  print("in");\n}\n',
    "with_err_list_target": 'with [1] as x {\n  print("in");\n}\n',
    "with_err_dict_target": 'with {"a": 1} as x {\n  print("in");\n}\n',
    "with_return_passthrough": 'fn f() {\n  with 7 as x {\n    print("pre");\n    return 1;\n  }\n}\nprint(f());\nprint("after");\n',
    "with_break_passthrough": 'for (i = 0; i < 3; i = i + 1) {\n  with 8 as x {\n    print(i);\n    if (i == 1) { break; }\n  }\n}\nprint("out");\n',
    "with_continue_passthrough": 'for (i = 0; i < 3; i = i + 1) {\n  with 8 as x {\n    if (i == 0) { continue; }\n    print(i);\n  }\n}\nprint("out");\n',
    "with_in_loop_multi": 't = 0;\nfor (i = 0; i < 3; i = i + 1) {\n  with i as x {\n    t = t + x;\n  }\n}\nprint(t);\n',
    "with_try_inside": 'with 5 as x {\n  try {\n    throw "boom";\n  } catch e {\n    print(e.message);\n  }\n}\nprint("done");\n',
    "with_throw_inside": 'with 5 as x {\n  throw "inner";\n}\n',
    "with_lambda_inside": 'with 5 as x {\n  f = fn() { return x; };\n  print(f());\n}\n',
}

# ── type 实例（T6）：构造 / init / self / extends / 覆盖 / 引用语义 / readonly / 错误路径 ──
TYPE = {
    "type_init_method": 'type Counter {\n  n: int,\n  fn init(self, start: int) { self.n = start; }\n  fn inc(self) { self.n = self.n + 1; return self.n; }\n  fn get(self) { return self.n; }\n};\nc = new Counter(10);\nprint(c.get());\nc.inc();\nprint(c.get());\nprint(c.n);\nprint(c);\n',
    "type_field_write": 'type P { x: int };\np = new P(1);\np.x = 9;\nprint(p.x);\n',
    "type_no_init": 'type Shape { name: str, sides: int, area: int,\n  fn desc(self) { return "shape " + self.name; } };\ns = new Shape("tri", 3, 9);\nprint(s.desc());\nprint(s.area);\nprint(s);\n',
    "type_extends": 'type Shape { name: str, sides: int, area: int };\ntype Square extends Shape {\n  side: int,\n  fn init(self, side: int) {\n    self.name = "square";\n    self.sides = 4;\n    self.side = side;\n    self.area = side * side;\n  }\n  fn perim(self) { return self.side * 4; }\n};\ns = new Square(5);\nprint(s.perim());\nprint(s.area);\nprint(s.sides);\nprint(s);\n',
    "type_override": 'type Shape { name: str, sides: int, area: int,\n  fn desc(self) { return "shape " + self.name; } };\ntype Square extends Shape {\n  side: int,\n  fn init(self, side: int) { self.name = "sq"; self.side = side; self.sides = 4; self.area = side * side; }\n  fn desc(self) { return "square " + self.name; }\n};\ns = new Square(5);\nprint(s.desc());\nsh = new Shape("tri", 3, 9);\nprint(sh.desc());\n',
    "type_ref_semantics": 'type P { x: int };\nfn bump(g) { g.x = g.x + 1; return 0; }\np = new P(1);\nbump(p);\nprint(p.x);\n',
    "type_with_method": 'type Guard {\n  active: int,\n  fn init(self) { self.active = 0; }\n  fn __enter__(self) { self.active = 1; return self; }\n  fn __exit__(self) { self.active = 0; return 0; }\n};\ng = new Guard();\nwith g {\n  print(g.active);\n}\nprint(g.active);\n',
    "type_field_read": 'type P { x: int, y: int };\np = new P(3, 4);\nprint(p.x + p.y);\n',
    "type_eq_identity": 'type P { x: int };\na = new P(1);\nb = a;\nc = new P(1);\nprint(to_str(a == b));\nprint(to_str(a == c));\n',
    "type_display": 'type P { x: int, tag: str };\np = new P(7, "hi");\nprint(p);\n',
    "type_err_unknown_method": 'type P { x: int };\np = new P(1);\np.nope();\n',
    "type_err_unknown_field": 'type P { x: int };\np = new P(1);\nprint(p.nope);\n',
    "type_err_write_unknown": 'type P { x: int };\np = new P(1);\np.nope = 2;\n',
    "type_err_readonly": 'type P { a: int, b: readonly int };\np = new P(1, 2);\np.b = 5;\n',
    "type_err_too_many": 'type P { x: int };\np = new P(1, 2);\n',
    "type_err_method_on_int": 'x = 5;\nx.foo();\n',
    "type_err_new_unknown": 'p = new Nope(1);\n',
    "type_in_fn_arg": 'type P { x: int };\nfn getx(p) { return p.x; }\nprint(getx(new P(42)));\n',
}

# ── import 远程模块（T7）：编译期下载 / 别名前缀 / 本地路径 ──
# 远程用例经本地 http server（fixture tests/fixtures/remote_mod/）下载，避免 CI 联网。
# {port} 运行时替换为 httpd 实际端口；每例 interp 与 VM 之间清理缓存，确保两路都重新下载
# （否则 interp 下载后 VM 缓存命中，进度条输出分叉）。
IMPORT = {
    "remote_download": 'import "probe_remote_mod" from "http://127.0.0.1:{port}/probe_remote_mod.hn";\nprint(probe_remote_mod_add(2, 3));\nprint(probe_remote_mod_mul(4, 5));\n',
    "remote_alias": 'import "probe_remote_mod" from "http://127.0.0.1:{port}/probe_remote_mod.hn" as rm;\nprint(rm_add(2, 3));\nprint(rm_mul(4, 5));\n',
    "local_path": 'import "probe_remote_mod" from "tests/fixtures/remote_mod/probe_remote_mod.hn";\nprint(probe_remote_mod_add(2, 3));\n',
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
    "slice": SLICE,
    "byte": BYTE,
    "bytes": BYTES,
    "field": FIELD,
    "with": WITH,
    "type": TYPE,
    "import": IMPORT,
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

    # IMPORT 组本地 http server（fixture 目录）+ 缓存路径。
    httpd = None
    port = None
    cache_file = os.path.join(
        os.environ.get("USERPROFILE", os.path.expanduser("~")),
        ".hone", "cache", "probe_remote_mod.hn",
    )

    def clear_import_cache():
        try:
            if os.path.exists(cache_file):
                os.remove(cache_file)
        except OSError:
            pass

    try:
        need_http = (not args.only and "import" in GROUPS) or args.only == "import"
        if need_http:
            import functools
            import http.server
            import socketserver
            import threading
            fixture_dir = os.path.join(os.getcwd(), "tests", "fixtures", "remote_mod")

            class _QuietHandler(http.server.SimpleHTTPRequestHandler):
                def log_message(self, format, *args):
                    pass

            handler = functools.partial(_QuietHandler, directory=fixture_dir)
            httpd = socketserver.TCPServer(("127.0.0.1", 0), handler)
            port = httpd.server_address[1]
            threading.Thread(target=httpd.serve_forever, daemon=True).start()
            clear_import_cache()
        for gname, cases in GROUPS.items():
            if args.only and gname != args.only:
                continue
            print(f"── {gname} ──")
            for name, src in cases.items():
                total += 1
                path = os.path.join(tmp, "t.hn")
                if gname == "import" and port is not None:
                    src = src.replace("{port}", str(port))
                with open(path, "w", encoding="utf-8", newline="\n") as f:
                    f.write(src)
                is_import = gname == "import"
                if is_import:
                    clear_import_cache()
                i = run([BIN, "run", path])
                if is_import:
                    clear_import_cache()
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

        if httpd is not None:
            httpd.shutdown()
        shutil.rmtree(tmp, ignore_errors=True)
    print(f"\n共 {total} 例，分歧 {bad}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
