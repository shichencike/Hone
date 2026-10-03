# AGENTS.md — Hone 项目代理指南

Hone：Rust 实现的脚本语言（单一可执行 `hone`）。主 crate 在 `src/`；`hone_lib/` 是 Hone 标准库（.hn 脚本，非 Rust）。

## 构建与测试

- `cargo check` 只查类型、不产二进制；**改代码后验证行为必须先 `cargo build`**，`target/` 旧二进制会静默沿用。
- 回归脚本依赖 `target/debug/hone.exe`，先 build 再跑：
  - `python tests/regress3.py` — 解释器 vs VM 输出逐字节对齐
  - `python tests/regress_err.py` — 双引擎错误诊断逐字符对齐（行/列/文案/help）
  - `python tests/regress_ir.py` — 文本 IR 往返
- CI（.github/workflows/ci.yml）只做 `cargo build/test --locked` + hello/fib smoke；无 rustfmt/clippy 配置，格式化用语言自带 `hone fmt`（作用于 .hn，非 Rust）。

## 架构

- 单 crate 流水线：lexer → parser → ast → checker → 三后端：`interp.rs`（AST 解释器）、`vm.rs`（字节码 VM）、`aot.rs`+`codegen.rs`（C 生成）。
- `Value` 定义在 `interp.rs`，VM 与全部 builtins 复用之——改 Value/字典行为要同时顾及三引擎。
- 铁律：**interp 与 VM 输出（含报错）必须逐字节一致**；错误码 Hxxx 集中在 `error.rs`，`H999`=功能未实现（VM/AOT/DLL 不支持 with、type 实例、byte/bytes、字段赋值等解释器特性时统一报 H999）。

## 项目特有约定

- struct 实例内部携带 `__struct__` 隐藏键（`Value::Str` 标记）：新增/改动任何用户可见 dict 路径（display/len/keys/values/has_key/for-in/推导式/解构/==/json）必须过滤该键，否则泄漏。
- 无 null 字面量（null 仅 void 占位，用 `""` 哨兵）；变量块级作用域。
- AOT `build --exe -c` 在 Windows 的 `-o` 必须显式带 `.exe`，否则产物校验误报失败。
- 新功能需同步：`hone.md`（语言规格）、`CHANGELOG.md`、`官网/` 站点页、README。

## 维护规则

当项目结构、构建/测试命令、架构边界、开发约定，或本文件记录的其他事实发生变化时，必须在同一次改动中同步更新本文件。
