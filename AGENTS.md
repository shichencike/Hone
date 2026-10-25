# AGENTS.md — Hone 项目代理指南

Hone：Rust 实现的脚本语言（单一可执行 `hone`）。主 crate 在 `src/`；`hone_lib/` 是 Hone 标准库（.hn 脚本，非 Rust）。

## 构建与测试

- `cargo check` 只查类型、不产二进制；**改代码后验证行为必须先 `cargo build`**，`target/` 旧二进制会静默沿用。
- **dev profile 无调试符号**（Cargo.toml `[profile.dev] debug = 0`）：debuginfo=2 时 rustc
  链接 200MB 级产物，本机与 CI runner 均内存耗尽（exit 143 / STATUS_STACK_BUFFER_OVERRUN）；
  CI 全部走 `cargo build --release`。
- 回归脚本依赖 `target/release/hone.exe`（`HONE_BIN` 可覆盖定位），先 build 再跑：
  - `python tests/regress3.py` — 解释器 vs VM 输出逐字节对齐
  - `python tests/regress_err.py` — 双引擎（interp/VM）错误诊断逐字符对齐（行/列/文案/help）
  - `python tests/regress4.py` — 双引擎正常路径逐字节对齐
  - `python tests/regress_ir.py` — 文本 IR 往返
  - `python tests/tools/probe_align.py` — 开发期扫雷：271 例 match / 条件 / 逻辑 / 解构 / 重载 / alias+goto / 值语义 / 切片 / byte / bytes / 字段赋值 / with / type / import / 默认参数 / 点号内置 / async+go 形态的 interp-vs-VM 逐字节比对。动态类型值（enum/struct）下的条件与逻辑分叉、重载回退边界、以及新增 VM 特性的对齐靠它扫出。import 组自带本地 http server fixture（`tests/fixtures/remote_mod/`，避免 CI 联网），每例在 interp/VM 之间清 `~/.hone/cache/` 确保两路都重新下载。
- CI（.github/workflows/ci.yml）：ubuntu/windows 两个 test job 做 `cargo build --release --locked` + `cargo test --release --locked` + hello/fib smoke + `tests/regress4.py`；无 rustfmt/clippy 配置，格式化用语言自带 `hone fmt`（作用于 .hn，非 Rust）。

## 架构

- 单 crate 流水线：lexer → parser → ast → checker → 双后端：`interp.rs`（AST 解释器）、`vm.rs`（字节码 VM）。（AOT 与原生编译（nat）后端已移除。）
- `Value` 定义在 `interp.rs`，VM 与全部 builtins 复用之——改 Value/字典行为要同时顾及双引擎。
- **引用模型**：List/Dict/Str/Bytes 四变体为 Arc 共享句柄（List/Dict 带 RwLock），`Value::clone` = O(1) 句柄克隆，值语义由统一写屏障保证（interp `ensure_unique` / VM `cow_handle`：独占原地写、共享先隔离）。别名不可见，可观察行为与深拷贝逐字节一致。`cow` 声明语法已废弃（BREAKING，遇 `cow` 报 H005）；`as_plain` 保留为恒等（历史读点兼容）。
- 铁律：**interp 与 VM 输出（含报错）必须逐字节一致**；错误码 Hxxx 集中在 `error.rs`，`H999`=功能未实现（T1-T9 后 VM 已无特性 H999 兜底：load/FFI、import 远程、with、type 实例、byte/bytes 字面量、字段赋值、切片、默认参数均已在 VM 落地；`Instr::LoadLib` 携带 `ffi::LoadDesc` 走共用 `ffi::load`）。byte/bytes **值**（read_bytes/base64_decode/zip_read/tgz_read_bytes/http_get_bytes/http_post_bytes/http.request binary/zlib.* 四函数/ws.request 消息列表/ptr.read_byte/索引产出）interp/VM 双引擎全支持：VM 侧执行臂集中在 `vm.rs` 的 `v_add`/`v_eq`/`v_cmp`/`v_index`/`v_len`/`IterCheck`（按 is_comp 站点分派：推导式拒 bytes、for-in 收）与 `value_to_str`（display 基准 = 解释器：Byte 为 `0bNNNNNNNN`、Bytes 为可打印原样 + `\xNN`），改任一处须与解释器 `interp.rs` 对应函数（`arith`/`values_eq`/`values_cmp`/`index_value`）逐字对齐；for-in 双变量 list/bytes 报错走 `IsBytes` 指令 + `Raise`（文案含 list/bytes 两分支）。
- 运算符重载（G2.4）：`__op` 回退链在 `vm.rs` exec 臂（触发条件表以此为准：算术：非数字/str 组合 + H001；比较：非数字对、错误码不限；一元：H001）。
- 别名（G2.5）：语义基准：VM `do_call`（先 resolve_alias → 再查局部变量 → 函数 → 内置）。
- 参数个数校验双引擎统一到解释器文案（`expects at most N arguments, got M` /
  ``missing argument `c` of `f` `` + 定位 + help）——**不要**改回中文硬编码消息。
- FFI（typed 签名块 / 头文件绑定 / 变参 / 回调）：动态调用与回调闭包统一走 **libffi**
  （`libffi-sys`，默认从源码静态构建、无系统动态库依赖；MSVC 目标经预处理头 + cc，GNU/ELF 目标需
  make + C 编译器）——`ffi_prep_cif` / `ffi_prep_cif_var` / `ffi_call` + `ffi_closure`；旧的
  `ffi_dispatch!` / `ffi_variadic!` 穷举宏已删（8190 类型实例化内存峰值）。T8 后 FFI 实现
  **拆出 `src/ffi.rs` 供双引擎共用**（`ffi::load` / `ffi::call` / `FfiState` / `CbEngine::Interp|Vm`：
  错误文案只有一处定义，回调宿主经 `VmCbEnv` Arc 快照在新实例中执行）；VM 编译臂把
  `Stmt::Load` 产为 `Instr::LoadLib(LoadDesc)`（描述内联在指令里，文本 IR 可往返）、
  点号未解析调用落到 `call_lib_fn`。`int32` / `float32` 是
  **FFI 专有类型名**（Hone 值按 int/float 走，checker 映射 `Ty::Int` / `Ty::Float`，调用时校验
  32 位范围）；头文件绑定把 C `int32_t`/`uint32_t` 映射 `int32`、C `float` 映射 `float32`；
  回调限 0-8 个标量参数且仅同步。改 FFI 类型/文案须同步 `ast.rs`(FfiTy) / `parser.rs`(ffi_ty_from_tok) /
  `header.rs`(map_type) / `ffi.rs` 四处。
- 点号内置族（G2.9）：interp/VM 共用 `builtins.rs` + 各 `*mod.rs`（基于 `Value`）。未登记点号名保留 H404（load 库语义）。
- import（G3.1）：解释器**运行期**处理（`interp::exec_import`，`hone run` 语义基准）；VM（`--vm`）
  在**编译期**经 `src/imports.rs::resolve_imports` 递归展开顶层 import、
  合并为单编译单元（本地路径相对 CWD / 远程缓存 `~/.hone/cache/`，模块前缀改名 + 顶层语句入 Block）。
  改 import 语义需同时顾及运行期与构建期两处；`hone check`/`fmt`/LSP 不经过构建期展开。
  **VM 路径 `run_script` 中 `checker` 必须在 `resolve_imports` 之前运行**（只静态检查主模块
  源码，import 模块函数按「外部动态加载」`has_external` 放行）——与解释器 / `hone check` /
  `hone prof` 一致；若在展开后检查，展开进来的模块函数体（未被调用、参数类型无法从调用点
  推断者）会被 strict 阶段误报 H003，破坏铁律。
- CI/发布：`ci.yml` test job 含 `regress4.py`（二进制定位支持 `HONE_BIN` 覆盖）；
  `release.yml` 构建三目标发布产物（linux tar.gz / windows zip，hone + install.sh/bat）。
  安装脚本 install.sh（Linux/Termux）与 install.bat（Windows）随发布上传；Windows 安装 = 双击
  install.bat（与 full zip 同目录）。Windows 构建产物在 release.yml 经 signtool 签名
  （secrets WINDOWS_CERT_PFX / WINDOWS_CERT_PASSWORD；缺失时跳过签名不失败）。
  签名后仅用 `Get-AuthenticodeSignature` 确认产物带代码签名证书（NotSigned 即失败），
  不做链信任校验（runner 根存储可能不含签名证书的根 CA：signtool verify /pa 报链不信任、
  Get-AuthenticodeSignature 返回 UnknownError、certutil 预信任方案也有坑）。
- 性能基准（`bench.yml`，push master + 路径过滤 src/bench/tests/bench_ci.py/Cargo.lock）：
  `tests/bench_ci.py` 跑 9 负载 × hone interp/VM vs CPython vs Lua 5.4（子进程 wall time
  3 次取最快、单次 >5s 只跑 1 次；VM 口径 `--vm`），结果写 `官网/perf-data.json` 自动提交回仓库
  （提交信息带 **[skip ci]** 且该文件不在触发路径内，双保险防递归）。`bench/lua/` 为九项 Lua 5.4
  等价实现——**改任一语言基准需三语言等价同步**（.hn / py / lua），fib26 与 loop 计数不得变。
- 条件与逻辑语义（改 `vm.rs` 编译臂时注意，解释器为准）：`if`/`while`/`do-while`/`for`/三元/推导式 `if` 过滤六处条件站点**运行期严格 bool**（`Instr::CondCheck`），`&&`/`||` 短路求值且右侧操作数严格 bool（`Instr::IsBoolVal` 判「恰为该布尔值」，不是 truthy）。

## 项目特有约定

- struct 实例内部携带 `__struct__` 隐藏键（`Value::Str` 标记）：新增/改动任何用户可见 dict 路径（display/len/keys/values/has_key/for-in/推导式/解构/==/json）必须过滤该键，否则泄漏。
- 无 null 字面量（null 仅 void 占位，用 `""` 哨兵）；变量块级作用域。
- `match` 绑定变量**仅在分支体内可见**（编译期用 `decl_tmp` + 分支作用域，不推进 `var_next`）；lambda 捕获按**内层优先**（与 `lookup` 同序）。改这两处前先跑 `tests/tools/probe_align.py`。
- `d["k"]` 形式的 dict 索引**读**在 checker 层被拒（索引须 int）；dict 取值只能走 for-in / `keys` / `values` / `has_key` / 解构。别按JS/Python 直觉写 dict 读写。
- 新功能需同步：`hone.md`（语言规格）、`CHANGELOG.md`、`官网/` 站点页、README。

## 维护规则

当项目结构、构建/测试命令、架构边界、开发约定，或本文件记录的其他事实发生变化时，必须在同一次改动中同步更新本文件。
