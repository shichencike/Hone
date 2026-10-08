# AGENTS.md — Hone 项目代理指南

Hone：Rust 实现的脚本语言（单一可执行 `hone`）。主 crate 在 `src/`；`hone_lib/` 是 Hone 标准库（.hn 脚本，非 Rust）。

## 构建与测试

- `cargo check` 只查类型、不产二进制；**改代码后验证行为必须先 `cargo build`**，`target/` 旧二进制会静默沿用。
- 回归脚本依赖 `target/debug/hone.exe`，先 build 再跑：
  - `python tests/regress3.py` — 解释器 vs VM 输出逐字节对齐
  - `python tests/regress_err.py` — 三引擎（interp/VM/nat）错误诊断逐字符对齐（行/列/文案/help）
  - `python tests/regress4.py` — 三引擎正常路径逐字节对齐（需本机 rustc）
  - `python tests/regress_ir.py` — 文本 IR 往返
  - `python tests/tools/audit_err.py` — 开发期扫雷：55 例错误文案 + help 行 nat-vs-interp 批量比对（慢，不入常规回归）
  - `python tests/tools/probe_align.py` — 开发期扫雷：93 例 match / 条件 / 逻辑 / 解构 / 重载 / alias+goto 形态的 interp-vs-VM 逐字节比对；`--nat` 追加第三引擎（每例单独 build，慢）。动态类型值（enum/struct）下的条件与逻辑分叉、以及重载回退边界靠它扫出。
- CI（.github/workflows/ci.yml）只做 `cargo build/test --locked` + hello/fib smoke；无 rustfmt/clippy 配置，格式化用语言自带 `hone fmt`（作用于 .hn，非 Rust）。

## 架构

- 单 crate 流水线：lexer → parser → ast → checker → 四后端：`interp.rs`（AST 解释器）、`vm.rs`（字节码 VM）、`aot.rs`+`codegen.rs`（C 生成）、`natgen.rs`+`runtime/`（Cranelift 原生编译）。
- `Value` 定义在 `interp.rs`，VM 与全部 builtins 复用之——改 Value/字典行为要同时顾及三引擎。
- 铁律：**interp 与 VM 输出（含报错）必须逐字节一致**；错误码 Hxxx 集中在 `error.rs`，`H999`=功能未实现（VM/AOT/DLL 不支持 with、type 实例、byte/bytes、字段赋值等解释器特性时统一报 H999）。
- 原生后端与 VM **共享同一套 `vm::Instr`**：加指令时 `vm.rs` 的执行臂 / `instr_text` / `parse_instr`（文本 IR 往返）与 `natgen.rs` 的 lowering 臂、`main.rs` 的 strtab 收集必须同步；漏一处会被 `regress_ir.py` / `regress4.py` 抓住。
  - 指令带**字符串操作数**（Call 名 / Field 名 / Raise 三段文本 / ThrowStr 文案 / enum 构造名等）才需 `main.rs` strtab 收集；纯数值操作数指令（如 `CondCheck` / `IsBoolVal`）不需要。
  - `Instr::Raise` 的**错误码**另有两处白名单，漏改会让 nat 静默渲染成 H990 或 IR 往返失败：`runtime/src/lib.rs` 的 `rt_raise` 码表、`vm.rs` `parse_instr` 的 `RAISE` 码表。
  - runtime 侧「解释器显式无 help」的分支必须用 `err_h_explicit_no_help`（哨兵 `Some("")`），否则渲染层 `hn_help_for` 会凭空补一行 help。
- 运算符重载（G2.4）：`__op` 回退链**全部在 runtime**（`rt_binop` / `rt_unop` / `rt_index` /
  `call_builtin` 的 `len` 臂），字节码与 natgen 无需改动。触发条件表以 `vm.rs` exec 臂为准
  （算术：非数字/str 组合 + H001；比较：非数字对、错误码不限；一元：H001）。
- 别名（G2.5）：三处同步——runtime `ALIASES` 表 + `call_global` 入口解析、natgen Call lowering
  编译期解析（决定 rt_callv / rt_call）、wrapper `NAT_ALIASES` **且原名与新名都要进 strtab**
  （原名可能从不作为 Call 名出现，漏收集会静默解析成 strtab[0]，报错指向无关函数名）。
  语义基准：VM `do_call`（先 resolve_alias → 再查局部变量 → 函数 → 内置）。
- 参数个数校验三引擎统一到解释器文案（`expects at most N arguments, got M` /
  ``missing argument `c` of `f` `` + 定位 + help）——**不要**改回中文硬编码消息。
- 条件与逻辑语义（改 `vm.rs` 编译臂时注意，解释器为准）：`if`/`while`/`do-while`/`for`/三元/推导式 `if` 过滤六处条件站点**运行期严格 bool**（`Instr::CondCheck`），`&&`/`||` 短路求值且右侧操作数严格 bool（`Instr::IsBoolVal` 判「恰为该布尔值」，不是 truthy）。

## 项目特有约定

- struct 实例内部携带 `__struct__` 隐藏键（`Value::Str` 标记）：新增/改动任何用户可见 dict 路径（display/len/keys/values/has_key/for-in/推导式/解构/==/json）必须过滤该键，否则泄漏。
- 无 null 字面量（null 仅 void 占位，用 `""` 哨兵）；变量块级作用域。
- `match` 绑定变量**仅在分支体内可见**（编译期用 `decl_tmp` + 分支作用域，不推进 `var_next`）；lambda 捕获按**内层优先**（与 `lookup` 同序）。改这两处前先跑 `tests/tools/probe_align.py`。
- AOT `build --exe -c` 在 Windows 的 `-o` 必须显式带 `.exe`，否则产物校验误报失败。
- `d["k"]` 形式的 dict 索引**读**在 checker 层被拒（索引须 int）；dict 取值只能走 for-in / `keys` / `values` / `has_key` / 解构。别按JS/Python 直觉写 dict 读写。
- 新功能需同步：`hone.md`（语言规格）、`CHANGELOG.md`、`官网/` 站点页、README。

## 维护规则

当项目结构、构建/测试命令、架构边界、开发约定，或本文件记录的其他事实发生变化时，必须在同一次改动中同步更新本文件。
