# Hone 原生编译后端 — 进度清单

> 配套：`Hone原生编译后端设计.md`（设计）；本文件跟踪落地进度，按可独立验收的任务拆分。
> 更新：2026-10-07
> 图例：`[x]` 完成 · `[ ]` 待办 · `[~]` 进行中 · `[!]` 已按用户指令裁剪/延后

## 总览

| 里程碑 | 状态 | 说明 |
|---|---|---|
| **M0 管线打通** | ✅ 完成 | T1–T10 全完成（三引擎逐字节对齐；`build --exe --native` + `natgen` 共用管线；文档已同步） |
| **M1 核心语义** | ✅ 完成 | G1.1 RC+COW / G1.2 list / G1.3 dict / G1.4 for-in / G1.5 别名用例集 / G1.6 全量回归（三引擎）；审计暴露并修复 6 处引擎缺陷 |
| **M2 功能对齐** | 🔵 进行中 | G2.1 struct 完成（构造 / Field / `__struct__` 全路径过滤 / 相等语义；字段写入为 VM 编译期 H999 同源）；G2.2 enum 完成（构造 / match / 显示 / 相等 / 容器传参）；**G2.3 match 完成**（字面量 / 变体 / 载荷绑定 / 通配 / 无匹配 H005 / 绑定作用域 / 闭包捕获），并连带修掉 6 处既有 interp-VM 分歧（match 无匹配错误码、绑定变量泄漏、lambda 捕获取外层、`&&`/`||` 无短路、条件 truthy 放行、解构文案）；G2.4–G2.7 待办 |
| **M3 集成与优化** | ⬜ 未开始 | import 合并/交叉编译/体积裁剪/工具链打包/CI；任务组 G3.1–G3.7 |

**当前可运行产物**：`tests/m1/list.hn` / `tests/m1/dict.hn` / `tests/smoke/rc_alias.test.hn` / `tests/m2/struct.hn` / `tests/m2/enum.hn` / `tests/m2/match.hn` / `tests/m2/conditions.hn` → nat exe，三引擎逐字节对齐（含 list/dict 读写、for-in 迭代、别名 RC+COW 值语义、struct 构造/字段/隐藏键过滤/相等、enum 构造/match/显示/相等/容器与传参、match 全形态与绑定作用域、lambda 捕获、条件严格 bool 与 `&&`/`||` 短路）。
**复现**：`hone build --exe --native tests/m2/match.hn -o match.exe && ./match.exe`
**回归入口**：`python tests/regress4.py`（正常路径三引擎，10 例全绿）、`python tests/regress_err.py`（错误诊断三引擎逐字符，57 例全绿）、`python tests/regress3.py`（interp vs VM 全 examples，真实逻辑差异 0）、`python tests/regress_ir.py`（文本 IR 往返，BAD=0）。
**审计工具**：`python tests/tools/audit_err.py`（55 例错误文案 + help 行 nat-vs-interp 批量比对）；`python tests/tools/probe_align.py`（50 例 match / 条件 / 逻辑 / 解构形态的 interp-VM[-nat] 逐字节比对，**G2.3 的 6 处分歧即由它扫出**；加 `--nat` 追加第三引擎）。
**验证前置**：改代码先 `cargo build`（`target/` 旧二进制会静默沿用），回归脚本依赖 `target/debug/hone.exe`。

---

## M0 — 管线打通（任务分解）

> 已完成部分（整数子集）见「M0 已完成基线」节。以下为剩余任务的执行顺序（T2 依赖 T1，T3–T6 依赖 T2/T4 渐进，T7–T9 依赖 T6）。

### T1 工程地基：runtime 独立 crate + 链接管线升级 ✅

- **目标**：建立 `honeruntime` 独立 crate（std-only）与「object + runtime rlib + 生成式 wrapper」链接管线；引入 frame 指针 ABI（替换 M0 基线的无帧 i64 调用约定）。
- **范围**：
  - 根 `Cargo.toml` 加 `[workspace] members = ["runtime"]`（`tests/hone_lib` 显式 exclude）；`runtime/` 为独立 crate（rlib，无外部依赖，edition 2021）。
  - `runtime/src/lib.rs`（单文件）骨架：`hn_entry` 产物入口 + `rt_print`；约束「无内属属性 / 无 `//!`」（因被 `include!` 内联进 wrapper）。`Hnv` / 静态表 / ZError 渲染随 T2/T4 增量落地。
  - `cmd_natgen` 链接改造：runtime 源码经 `include_str!` 嵌入 hone 二进制（`NAT_RUNTIME_SRC`）→ 临时目录（`rt.rs` + 生成式 wrapper `main.rs`，`include!` 内联合并单文件）→ 一次 `rustc -C link-arg=obj.o` → exe。干净构建机自足（不依赖仓库存在）。
- **实现期决策（2026-10-05）**：
  - **不引入 frame 指针 ABI**：`__hn_main() -> i64` 保持基线签名，寄存器文件维持「函数内显式栈槽」，参数走调用 ABI 寄存器。理由：24B Hnv 可直接按值传参（设计 §4.3「3 寄存器装得下」），引入 frame 指针会迫使所有 rt_* 变槽指针形态、lowering 全面重写，且 M0 无性能诉求（性能优化归 M1 RC+COW）。T2 起 `rt_*` 为 Hnv 按值形态（`rt_binop(op, a, b) -> Hnv`）。
  - **链接形态用单次 rustc（嵌入源码）而非 rlib**：规避 link.exe 对 rlib 扩展名/命名约定（`hon_runtime.lib`）的边角，且产物构建零中间物。M3 工具链打包时再换预编译 staticlib（G3.4）。
  - wrapper 的 `main` 恒 `std::process::exit(hn_entry())`：为 T4「错误 → exit(1)」预留语义占位（T4 前 hn_entry 恒 0）。
- **退出标准（已全部达成）**：`cargo build -p honeruntime` 与 `cargo test -p honeruntime` 通过；fib15 经新管线（嵌入 runtime + 生成式 wrapper + rustc 链接）重建仍输出 610。
- **涉及文件**：`Cargo.toml`、`runtime/Cargo.toml`、`runtime/src/lib.rs`、`src/main.rs`（cmd_natgen + NAT_RUNTIME_SRC）。

### T2 Hnv 值模型 + 常量 + print 全类型 ✅

- **目标**：落地设计 §4.3 值模型：寄存器槽 8B → 24B `Hnv{tag,a,b}`；str/char/float/null 常量承载；`print` 全类型输出。
- **范围**：
  - 标签：`INT / FLOAT / BOOL / CHAR / NULL / STR / ERR / FN`；标量内联（a 位），`STR / ERR / FN` 为 `*const HnBox`（a 指针）。
  - `HnBox{rc: AtomicUsize, kind, payload}`：**M0 只做 retain/release 引用计数**（COW 惰性深拷留 M1，见 G1.1）；rodata 常量串同样走 box（rt_init 时建箱，LoadK = retain）。
  - 槽写统一 `rt_slot_store(dst, val)`：释放旧堆值、存新值（所有权转移语义：load 为借用、store/call 实参/返回值转移）。
  - 字符串静态表：全部 `Const::Str` 去重后编入生成式 wrapper，object 经 import 函数取指针（M3 移入 object rodata，见 G3.5）。
  - `Hnv::display` 对齐 `interp::Value::display` 的 M0 子集（int/float/bool/char/str/null；float 用 std Display 天然一致）。
- **退出标准**：`examples/hello.hn` + float/bool/char/null 打印脚本，nat 产物与 interp / VM 逐字节一致；fib 仍对齐；debug 构建下出口断言无 box 泄漏（rc 归零）。
- **涉及文件**：`runtime/src/lib.rs`、`src/natgen.rs`、`src/main.rs`（wrapper 生成）。
- **实现进度（2026-10-05）**：
  - **ABI 关键决策**：设计 §4.3 原设想「Hnv{t,a,b}=24B 内联寄存器」，但 rustc windows-gnu 反汇编实测：24 字节（>16B）的 `repr(C)` 结构体在 Windows x64 走**隐藏 sret 指针**（返回值写进调用方缓冲区、参数经指针），与 cranelift 多结果函数 RAX/RDX/RCX 返回**不匹配**。故 T2 起改**句柄式值模型**：寄存器（cranelift i64 SSA 值）持 u64 句柄指向堆上 `HnCell{rc,t,a,b,pkind,payload}`，跨 object↔rustc 只传 i64（T1 已验证的干净 SysV 寄存器 ABI）。这是「24B 内联」不可行后的唯一 ABI 安全路径。
  - **runtime 已落地，7 项单测全过**：句柄 retain/drop 引用计数（HEAP_LIVE 基线差值断言出口归零、无泄漏）、错误码胖指针还原（`&'static str` 无 NUL 结尾不能用 CStr）、strtab 建箱/取值往返、算术/比较/一元/显示逐字对齐 VM 的 M0 子集（int/float 溢出 H010、除零 H009、类型不匹配 H001、char 比较、display 全标量）。FFI 全 u64 进出：`rt_i64/rt_f64/rt_bool/rt_char/rt_null/rt_str`（建句柄）、`rt_retain/rt_drop`、`rt_binop`（消费实参镜像 VM clone）、`rt_unop`、`rt_print`（借用）、`rt_truthy/rt_is_err`、`hn_entry`。
  - **natgen 24B lowering 重建中**：T1 版 `src/natgen.rs` 因误操作丢失（从未提交），正在按句柄式模型重建（内存帧槽：每 VM 寄存器一个 8B 句柄槽，递归经 cranelift 栈帧独立）。fib.hn 真实形态已确认：递归函数 + if/return + 字符串拼接（无 while 循环）。
- **实现进度（2026-10-06）—— T2 完成**：
  - **内存帧槽定型**：重建版曾用 SSA `Value` 直接当槽（`slots: Vec<Value>`），verifier 报「值来自非支配块」——`err_ret` 块有 N 个前驱（每处 fallible 检查都跳去），末尾 drop 全部槽的 SSA 值只在最后前驱定义，必然违反支配（if-else 合并点 / 循环回边同理）。改回设计文档既定方案：每 VM 寄存器一个 8B `ExplicitSlot`（`create_sized_stack_slot` + `stack_load`/`stack_store`，offset 0）；槽是内存，任意多前驱块在自己块内 load 再 drop，无跨前驱 SSA 依赖。`stack_store` 为带副作用指令（can_store），DCE 不删，所有权转移安全。
  - **check_err 必须切 cont 块**：尝试过「brif(ok, 当前块, err_ret)」自分支免切块（块数减半），但 cranelift 前端禁止对已被分支回指的块继续发射指令（"cannot add an instruction to a block already filled"），故 cont 块保留——与内存槽正交（cont 单前驱接收 r，err_ret 走内存槽）。
  - **JmpIfFalse 方向修复**：重建版 `brif(is_false, next, dest)` 把 then/else 接反（fib 实际走 fall-through 分支，输出 15 而非 610），改为 `brif(is_false, dest, next)`。
  - **runtime 收尾**：`hn_entry` 加 debug 出口泄漏断言（HEAP_LIVE 归零，strtab 基引用常驻不计）；`cmpv_inner` 补 bool 异值相等臂（`true == false` → false，镜像 vm.rs `v_eq` 的 `Bool==Bool → Ok(x==y)`，此前误报 H001）。
  - **退出标准达成**：`bench/t2_values.hn`（int/float/bool/char/null/str 常量、算术/比较、void→null）+ `examples/hello.hn` + fib15，nat 产物与 interp / VM 逐字节一致；debug 构建泄漏断言无触发。
  - **T2 范围外（T3/T5 落地）**：str+str 拼接、str 比较（需句柄字节比较）、`to_str` 等 builtin、f-string——runtime 已留 `make_cell_str` / str 字节访问路径。

### T3 运算与比较（rt_binop / rt_cmp / rt_unop）✅

- **目标**：算术/比较/一元运算全部改为运行时分发，逐条镜像 `vm.rs` 的 `v_add/v_sub/v_mul/v_div/v_mod/v_eq/v_cmp/v_neg/v_not`。
- **范围**：
  - `rt_binop(op, xs, ys, ds)`：int/float 全组合（含 int×float 提升）、str+str 拼接、除零 H009、整数溢出 H010、类型不匹配 H001。
  - `rt_cmp`：`==`/`!=`（int/float/bool/char/str/null 等值类型，镜像 `v_eq` 的 M0 可等集合）；`<`/`<=`/`>`/`>=`（按 `v_cmp` 实际语义，含 str 字典序）。
  - `rt_unop`：`Neg` / `Not`（含类型错误）。
  - lowering：每个 fallible 指令后 `tag==ERR` 检查 → 无 handler 则原样 return 跨帧传播（T6 补 handler 跳转）。
- **退出标准**：`tests/smoke/arithmetic.test.hn` 数值/比较/类型谓词子集 nat 对齐；除零/溢出/类型错误文案与 VM 一致（完整渲染 T4）。
- **涉及文件**：`runtime/src/lib.rs`、`src/natgen.rs`。
- **实现进度（2026-10-06）—— T3 完成**：
  - **str 句柄路径**：`rt_binop` 入口先取 tag 再分发——`str+str`（op 0）走 `str_concat`（合并字节 `make_cell_str`），`str ==/!=`（op 7/8）走 `str_eq`（直接比字节；Hone 字符串恒为合法 UTF-8，字节相等 ⇔ 码点相等，且绕开 wrapper 侧裸 rustc 下 `std::str::from_utf8_lossy` 的路径解析问题）。其余组合照旧走 Hnv 快照路径。
  - **混合臂按 VM 实际语义修正**：对照 `v_add/v_sub/v_mul/v_div/v_mod` 发现三处偏差并修正——①`v_mod` **无** int×float 混合臂（旧 runtime 多给，已移除）；②`v_sub`/`v_mul`/`v_div` **有**混合臂（旧 runtime 缺，已补，div 除零检查镜像 v_div）；③`v_cmp` **无 str 臂**（范围条「含 str 字典序」与设计文档不符，VM 实际 `str < str` → H001，runtime 保持 H001，**范围条以 VM 为准修正**）。注：混合运算被 checker 静态拒绝（no implicit conversion），运行期混合臂为防御性对齐，脚本不可达。
  - **验证**：`bench/t3_ops.hn`（str 拼接 8 例 / str ==/!= 8 例 / int float 运算 / char bool 比较 / 一元）nat 与 interp / VM 逐字节一致；错误消息文本（`error[H009]: division by zero`、`error[H010]: integer overflow`）与 VM 一致（行/列/caret 完整渲染 T4）；`cargo test -p honeruntime` 9/9 过（新增 `str_concat_and_eq`：拼接/相等/空串/str<str→H001 + HEAP_LIVE 归零断言）。
  - **范围外（T4/T5 落地）**：错误完整渲染（caret/help）、`to_int`/`to_float`（H006/H007）、bytes 拼接（VM 支持，natgen 无 byte 字面量 lowering）。

### T4 错误渲染 + 静态表 + rt_unwind ✅

- **目标**：错误输出与双引擎逐字节一致（行/列/caret/help/exit code）。
- **范围**：
  - Program 静态表（经 wrapper）：`file` / `src` 全文、每 chunk 的 span 表 `(line,col,len)×code_len`、每 chunk 内联字符串表（`ThrowStr` 等）。
  - `HnErr{code,msg,span,help}`：rt 函数取 pc 查 span；help 文本镜像 `vm_help_for`（覆盖 M0 可达码：H001/H002/H006/H007/H009/H010/H011/H600/H700）。
  - `rt_unwind(err)`：复刻 `ZError::Display`（含 Tab→4 空格 caret 对齐）→ stderr → `exit(1)`。
  - 内置参数错误（arg_err 文案）、`to_int`/`to_float` 失败（H006/H007）纳入。
- **退出标准**：`examples/err_divzero.hn` 等错误用例，nat 与 interp / VM 逐字节一致（含 `--> file:line:col`、caret 行、`help:` 行、退出码 1）。
- **涉及文件**：`runtime/src/lib.rs`、`src/natgen.rs`（span 表收集）、`src/main.rs`（wrapper 生成）。
- **实现进度（2026-10-06）—— T4 完成**：
  - **wrapper 程序静态表落地**：`hn_rt_prog_init(file, src, spans_ptrs, spans_lens, nchunks)` —— wrapper 生成 `NAT_FILE` / `NAT_SRC` 全文 + 每 chunk `NAT_SPAN{ci}: [u32; n*3]`（line,col,len 交错，与 `Chunk.spans` 一一对应）+ `NAT_SPANS: [&[u32]; C]`；file/src 经 `rust_str_lit` 转义（非 ASCII 原样、控制字符 `\u{}`）。
  - **ci/pc 接线**：`rt_binop` 改 5 参（op,x,y,ci,pc）、`rt_unop` 改 4 参（op,x,ci,pc）——`RtImports` 新增 sig5_1/sig4_1 声明，`ChunkLower` 带 `ci`，`lower_simple` 带 `pc`（指令在 chunk.code 的下标）；出错指令位置随错误句柄跨帧传播，`rt_unwind` 按 (ci,pc) 查 span 表渲染。
  - **渲染对齐**：`err_render` / `render_error` 复刻 `ZError::Display`（error 行 / `--> file:line:col` / 行号行 / Tab→4 空格 caret 对齐 / help 行）；`rt_unwind` 用 `eprintln!`（镜像 main.rs 的 `eprintln!("{}", e)`——Display 各行已含 `\n`，eprintln! 再补一个，stderr 以空行结尾）→ `exit(1)`。
  - **潜伏编译错误修复**：runtime 上一轮骨架把 `HnErr` 加了 ci/pc 字段但 `err_new` 未同步（主 crate `cargo build` 不编译 honeruntime，natgen 链接时 rustc 编译 wrapper 才暴露）——补字段；`err_render` 的 span 由 `Option<[u32;3]>` 转 `Option<(usize,usize,usize)>`。
  - **验证**：错误用例 6 组 nat / interp / VM 逐字节对齐——`1/0`（1:7）、溢出 `9223372036854775807+1`（1:25）、多行脚本（3:7）、**函数内错误跨帧传播**（div 函数第 2 行 2:14，ci 指向函数 chunk）、嵌套表达式（1:11）、**Tab 缩进 caret 展开**（4:11）；`examples/err_divzero.hn` 对齐（4:7，退出码 1）；runtime 单测 11/11 过（新增 `error_render_with_span_caret` / `error_render_tab_caret_align`，含 HEAP_LIVE 归零断言）；regress3 47/47、regress_err 37/37 无回归。
  - **范围外（T5/T6 落地）**：H011 arg_err 文案、`to_int`/`to_float` H006/H007（`err_new` 已留 ci/pc 形参）、H600 throw 渲染（T6）。

### T5 rt_call 完整分发 + lambda ✅

- **目标**：设计 §4.4/§4.6 —— 所有 `Call` 统一走 `rt_call` 名分发（M0 不做直调内联），支持用户函数递归、lambda、内置注册表（M0 子集）。
- **范围**：
  - `rt_call` 解析顺序镜像 `Vm::do_call`：当前帧局部变量（lambda 闭包 / 函数名字符串）→ 点号名（用户函数 / 模块内置 → H999 / 库未加载 H404）→ 用户函数 → struct 构造（M0 报 H999）→ 内置表 → `H002 undefined function`。
  - 内置表 M0 子集在 runtime 内逐条镜像 `builtins.rs`：`print / len / to_str / to_int / to_float / is_int / is_float / is_bool / is_str / type_of / abs / max / min / assert / assert_eq`（assert 失败 H700 文案 + 计数对齐）；表外但 VM 支持的内置 → 显式 H999。
  - 函数注册表（经 wrapper）：每 chunk `{nregs, locals: [(name,reg)], param_regs, cap_regs}`。
  - `MakeLambda` → `rt_makelambda`：`HnFn{ci, caps: Vec<Hnv>}`（捕获值整表 retain，按值捕获语义）；lambda 调用 → `rt_call_fn`（捕获填 cap_regs、实参填 param_regs、alloc frame 调 `__hn_f<ci>`）；lambda 块 lowering 为独立导出函数 `hn_f{ci}`。
- **退出标准**：`tests/smoke/functions.test.hn`（递归 / lambda / 向前引用）与 f-string 用例（`to_str` + 拼接）nat 对齐；lambda 捕获变量外层再修改不影响闭包（值语义）。
- **涉及文件**：`runtime/src/lib.rs`、`src/natgen.rs`、`src/main.rs`（wrapper 生成）。
- **实现进度（2026-10-06）—— T5 完成**：
  - **chunk 统一签名**（natgen + runtime）：所有 chunk 函数改为 `(argptr, nargs, capsptr, ncap) -> u64`——入口从 argptr 内存读参数（按 params 名 → locals 寄存器号，与 VM 帧填充一致）、从 capsptr 读捕获（按 captured_regs 元组）、其余寄存器 rt_null；main chunk `__hn_main` 同步 4 参，hn_entry 以全 0 调用。wrapper 生成 `extern "C"` 声明 + `NAT_FN_PTRS` 函数指针表（fn → usize 经 `hn_rt_fn_ptrs_init` 传入 runtime）。
  - **注册表**（wrapper 生成 + runtime init）：`hn_rt_funcs_init`（全局函数表 (strtab_idx, ci, nparams)，镜像 func_map）、`hn_rt_chunk_nparams`（每 chunk 形参数）、`hn_rt_locals_init`（每 chunk 局部变量表 (strtab_idx, reg)，扁平 + 计数）、`hn_rt_fn_ptrs_init`。strtab 收集扩展为去重全集：Const::Str + Call 函数名 + locals 名 + func_map 名。
  - **rt_call / rt_callv 名分发**：`rt_callv(v, sidx, argptr, nargs, ci, pc)` 带局部变量优先（func 名 ∈ 当前 chunk locals 的 Call 走此路径）——变量值为 HnFn → `call_fn_h`（捕获 retain 独立引用给 callee 帧，值语义）；为 Str → 字符串内容递归分发；否则按 strtab 名走全局分发。`rt_call` 纯全局分发（点号名 → 用户函数 / 库未加载 H404 带显式 help → 用户函数 H011 plain 校验 → 内置表 → H002）。
  - **内置表 M0 子集**：print/len（str 字节长）/to_str/to_int（H006 两变体文案）/to_float（H007）/is_int/is_float/is_bool/is_str/type_of/abs（i64::MIN 溢出 H010）/max/min/assert/assert_eq（H700 文案 + values_eq 标量集）；参数校验镜像 `arg_err`（H011 + "check the function signature"）；内置错误带调用点 (ci,pc) span。assert 计数（ASSERT_OK/FAIL）在 hone test 流程，nat 产物无此路径，M0 不做。
  - **Call / MakeLambda lowering**：Call 实参 retain 后打包进 param_ss 连续栈区（stack_addr 取指针传 runtime），所有权由 rt_call 消费（callee 帧槽接管 / builtin 借用后释放）；MakeLambda 捕获句柄 retain 后打包 cap_ss → `rt_makelambda(ci, capsptr, ncap)` 建 `HnFnBox{ci, caps}`（P_FN kind，释放时逐项 release）。移除 gen_object 对 lambda chunk（captured_regs 非空）的 H999 拒绝。
  - **顺手修复 VM 捕获 bug（违反 interp/VM 一致铁律）**：`call_lambda` 原按 `ncap+i` 偏移绑实参，但 lambda chunk 编译期继承外层 var_next，捕获/参数寄存器不从 0 起排（如 base@r1、x@r2）——实参填进捕获寄存器、参数寄存器留空（`base + x` 报 int+null）。改为按 `chunk.locals[params[i]]` 精确绑定实参寄存器（与捕获填装一致）。修复后捕获用例双引擎从「interp 15 / VM 报错」变为一致。
  - **验证**：递归 fib(10)/fib(15)、lambda 直调、捕获（值语义：外层改 n 后闭包输出不变 15/15）、内置集 29 项、向前引用（dbl 后置定义）、lambda 作参（apply(fn, 7)）、嵌套 lambda（twice）——全部 nat / interp / VM 逐字节对齐；错误渲染 to_int H006（2:7 caret 覆盖整个 `to_int` 名）与 assert H700（含自定义消息）对齐；bench 7 用例（fib15/t2/t3/hello/err_divzero/t4e/t4f）+ regress3 47/47 + regress_err 37/37 + honeruntime 11/11 全绿。
  - **范围外（T6 / 防御性保留）**：运行期 H002/H011/字符串函数名调用被 checker 静态拦截（编译期报错，三引擎同源一致；runtime 实现为防御性对齐）；assert 计数；`tests/smoke/functions.test.hn` 中 make_counter / apply(triple,7) / Calc 等「函数名作值」用例被 checker 拒绝（非运行期路径）。

### T6 最小 try-catch / throw ✅

- **目标**：设计 §4.5 无 setjmp 方案 —— lowering 期静态 catch 映射 + ERR 值跨帧传播。
- **范围**：
  - 每 chunk 预计算：每个 pc 的「最近活跃 handler」`TryBegin(handler, catch_reg)`（TryPop 出栈；嵌套取最内层）。
  - fallible 指令后：`tag==ERR` → 有 handler：err 值移入 catch_reg 槽、跳 handler 块；无 handler：return 传播（T3 已有）。
  - `Throw` / `ThrowStr` lowering：镜像 `normalize_throw`（str→H600、error 原样、其余→H600+value_to_str）；`TryBegin`/`TryPop` 为 lowering 期元指令（生成代码无运行时痕迹）。
  - 跨帧语义对齐 `Vm::try_catch`：catch 只发生在建立 try 的帧（天然由「函数返回 ERR、调用方检查」实现）。
- **退出标准**：`examples/try_catch.hn` 及「错误被 catch 后继续执行」用例 nat 与 interp / VM 逐字节一致（H600 文案、catch 变量值、后续输出）。
- **涉及文件**：`src/natgen.rs`（catch 映射 + Throw lowering）、`runtime/src/lib.rs`（normalize 镜像）。
- **实现进度（2026-10-06）—— T6 完成**：
  - **catch 静态映射**：`compute_catch_map` 线性扫描已 resolve 的 `chunk.code`，`TryBegin` 压栈 `(handler_pc, catch_reg)`、`TryPop` 出栈，每 pc 记最近活跃 handler（嵌套取最内层），与 VM 帧 `try_stack` 语义同构。`check_err(r, pc)` 改带 pc：`tag==ERR` 时查 `catch_map[pc]`——有 handler → `brif` 跳该 handler 的 catch 块（err 句柄作块参数、`store_slot` 入 catch_reg 后跳 handler 块）；无 handler → 原 `err_ret`（drop 全部槽后 return 跨帧传播）。handler 块是既有 leader（`TryBegin` 的 handler_pc），catch 块懒建并补为 BFS 可达起点（BFS 只跟显式跳转边，catch 边须显式补入，否则 handler 段不被 lower）。
  - **Throw / ThrowStr / Field lowering**：`rt_throw(v,ci,pc)`（str→H600、error 重抛同文件保留原始 `(ci,pc)` 且 len 归 1、其余→H001 类型不匹配——以 VM 实际语义为准，范围条「其余→H600+value_to_str」按 VM 修正）、`rt_throwstr(sidx,ci,pc)`（字面→H600）、`rt_field(obj,fsidx,ci,pc)`（e.code/.message/.file/.line/.col/.context，`(ci,pc)` 查 span 表按需推导、unknown→H002）。`Throw` 被抛值先 `retain` 再交归一器（镜像 VM clone：槽保留原引用、错误句柄经 catch 槽 / err_ret 转移）。`TryBegin`/`TryPop` lowering 为空操作（catch 路由全在 check_err 处，无运行时痕迹）。
  - **read_file 内置**：runtime `call_builtin` 补 `read_file` 臂（细分 H401/H402/H403/H404，文案镜像 builtins.rs）并加入 `is_m0_builtin` 白名单（否则落 H002 undefined function）。strtab 收集补 `Field` 字段名 + `ThrowStr` 文案。
  - **验证**：`examples/try_catch.hn` + 10 个 `.t6*` 边界用例（见总览「T6 验证」）nat 与 interp / VM 逐字节一致；`.t6_vart.hn` 为既有 fixture 第 7 行缺 `;` 的 H104 语法错误（三引擎 parse 同源一致，非 nat 差异；补 `;` 后 `throw <变量>` 三引擎逐字节一致）。回归无回退：regress3 47/47、regress_err 37/37、regress_ir 39 OK、M0 五例（fib15/t2/t3/hello/err_divzero）nat 对齐；honeruntime 11/11、debug 出口泄漏断言无触发。
  - **范围外（M2/M3）**：`ThrowStr` 的 context 字段（VM 恒空串 vs 按 `(ci,pc)` 推导该行）仅 match 无匹配分支产生，留 M2；跨文件错误退化（`{} (at {}:{}:{})`）依赖 import 多模块，M3 起。

### T7 命令集成：`hone build --exe --native` ✅

- **目标**：设计 §7.1 命令统一 —— natgen 管线并入 `hone build --exe`。
- **范围**：`hone build --exe --native <file.hn> [-o] [--obj]` 与 `hone natgen` 共用同一管线函数（重构 `cmd_natgen` 为共享入口）；help 文案同步（含 `[1/4]` 阶段输出风格）；`hone natgen` 保留为别名。`-c`（C 后端）行为不变，无 C 编译器自动切换留 M3。
- **退出标准**：两命令产物等价；`hone --help` 文案准确；fib/hello 经新命令复跑对齐。
- **涉及文件**：`src/main.rs`。
- **实现进度（2026-10-06）—— T7 完成**：
  - **共享管线**：`cmd_natgen` 的管线体（解析+类型检查 → VM 字节码 → Cranelift object → 生成式 wrapper + rustc 链接）抽为 `build_native(path, out, obj, label)`；`cmd_natgen` 保留参数解析后转调（label=`"natgen"`），`cmd_build_exe --native` 转调（label=`"build"`，obj 取管线默认名）——两命令同一路径，无逻辑分叉。
  - **参数集成**：`cmd_build_exe` 增 `--native` 标志，与 `-c` 互斥（显式 H005 报错 + help 提示二选一）；`-c`/`--keep-c` 与默认 bundle 路径行为不变。范围条中 `build --exe --native` 侧的 `[--obj]` 未暴露——obj 是 natgen 调试用中间物，build 侧默认名足够；natgen 侧 `--obj` 保留。
  - **输出风格**：阶段输出统一 `[1/4] label 解析 + 类型检查` / `[2/4] label 生成 object` / `[3/4] label 链接原生 exe` / `[4/4] label 完成`（stderr，与其他 build 阶段一致）；末行 `natgen OK:` 由 stdout 提升为 `[4/4]` 阶段行（stderr），stdout 留给产物自身。
  - **help 同步**：新增 `hone build --exe --native <file.hn>` 行；natgen 行去除过期的「[M0] 整数子集」描述（T3–T7 已落地），标注为 `build --exe --native` 别名。
  - **验证**：fib15 / hello 两命令产物运行输出 + 退出码与 interp 逐字节一致（产物二进制差异仅 rustc/lld 链接元数据非确定性，object/wrapper/rustc 命令行完全相同）；默认 bundle 模式复跑无回归；`-c --native` 互斥报错文案验证通过。
  - **范围外（M3）**：无 C 编译器自动切换（G3 任务组）、交叉目标编译、CI 接入（G3.6）。

### T8 三引擎回归 `regress4.py` ✅

- **目标**：设计 §十 / 进度「回归接入」—— 新后端成为第四引擎（三引擎回归：interp / VM / nat）。
- **范围**：`tests/regress4.py`：interp（`hone run`）/ VM（`--vm`）/ nat（`build --exe --native`）三路 stdout+stderr 逐字节比对；用例集 = `examples/fib.hn`、`examples/hello.hn` + 错误用例（`err_divzero` 等 M0 可对齐项）；SKIP/FLAKY 复用 `regress_common.py`；nat 产物写临时目录、跑完即删。AOT-C（`build --exe -c`）不纳入：错误渲染为简化一行（`uncaught error: Hxxx msg`），无完整 ZError 渲染（`-->` / 行号行 / caret / help），无法逐字节对齐（用户 2026-10-06 决定移除；AOT 后端功能本身保留）。
- **退出标准**：`python tests/regress4.py` 全绿（M0 用例集）；失败时打印首个分叉行。
- **涉及文件**：`tests/regress4.py`、`tests/regress_common.py`（如需扩 FLAKY 表）。
- **实现进度（2026-10-06）—— T8 完成**：
  - **regress4.py**：interp（`hone run`）/ VM（`--vm`）/ nat（`build --exe --native`）三路 stdout+stderr 逐字节比对；SKIP/FLAKY 复用 `regress_common.py`；FLAKY 重试至多 3 次（任一次全绿即非逻辑差异）；失败打印首个分叉行（interp vs 分叉引擎各一行）。
  - **工具链探测**：nat 需 rustc（无则退出）。
  - **产物管理**：每用例单独编译 nat 产物（产物内含该脚本不可复用）；nat 中间 object（CWD 下 `stem.nat.o`）与临时 exe 跑完即删，`finally` 清理临时目录。
  - **AOT 移除**：初版为四引擎（含 AOT-C + 用例级排除表），因 AOT 错误渲染为简化一行无法逐字节对齐、正常路径之外无稳定对齐面，按用户决定移除 AOT 路（`has_cc`/`build_aot`/`AOT_EXCLUDE` 全部删除），回归聚焦三引擎。
  - **顺手修复 AOT Windows CRLF**（初版四引擎首跑暴露，AOT 移除后修复仍保留）：C 运行时默认文本模式把 `\n` 转 CRLF，与 Rust 侧（interp/VM/nat 二进制写 LF）逐字节不一致。RUNTIME_C 增 `hn_init_line_endings()`（`_setmode` 切 stdout/stderr 二进制模式，非 Windows 空实现），生成的 `main()` 首行调用。
  - **验证**：`python tests/regress4.py` PASS=3 FLAKY=0 FAIL=0；`cargo build` / `cargo test` 无回归。

### T9 冒烟用例扩展（M0 对齐子集）✅

- **目标**：M0 退出标准「三引擎对齐」从 fib/hello 扩到冒烟子集。
- **范围**：筛选 `tests/smoke/` 中 M0 指令集可覆盖的用例入 regress4；不可对齐项在本文档登记原因（留 M1/M2）。
- **退出标准**：子集三引擎全绿；登记完整。
- **涉及文件**：`tests/regress4.py`、本文档。
- **实现进度（2026-10-06）—— T9 完成（结论：无可对齐扩容项）**：
  - **探测方法**：对 `tests/smoke/` 全部 8 个用例跑四路（interp/VM/AOT/nat）stdout+stderr+退出码比对。
  - **结果：8/8 全部在 interp 层即 exit=1，无一可入 regress4**。逐例原因（均为「fixture 陈旧 / 非 M0 指令集」，非 nat 缺陷；7 例 interp==VM 同源一致，1 例 interp 与 VM 分叉）：
    | 用例 | 失败点（interp） | 原因归类 |
    |---|---|---|
    | arithmetic.test.hn | `4.0 / 2` → H001 禁隐式转换 | fixture 陈旧（写于允许隐式转换时期） |
    | collections.test.hn | `expected )` 语法错 | fixture 陈旧 |
    | controls.test.hn | `continue` 断言 H700 失败 | 语义与现行语言不符（continue 跳过计数不符） |
    | datatypes.test.hn | `goto` 越过声明 → H105 | 非 M0（goto 属 M2） |
    | errors.test.hn | `null` 未定义 → H002 | fixture 陈旧（Hone 无 null 字面量，见 AGENTS.md） |
    | functions.test.hn | 函数名作值 → H002 | checker 静态拒绝（T5 已登记同源） |
    | strings.test.hn | `str_upper` 未定义 → H002 | fixture 陈旧（内置名不符） |
    | cow_read.hn | interp 过、VM 报 H999 `cow` 形参 | interp/VM 分叉（非 nat 路径；VM 按设计 H999 兜底） |
  - **结论**：smoke 用例集整体陈旧，当前语言语义下无一可对齐；T9 扩容项为空。修复这批 fixture 属「测试资产更新」而非 M0 管线任务，建议另立任务（M1 起 list/dict/迭代落地后顺带重制）。regress4 用例集维持 fib/hello/err_divzero 三例全绿。
  - **验证**：`python tests/regress4.py` PASS=3 FLAKY=0 FAIL=0。

### T10 文档同步 ✅

- **目标**：项目约定「新功能需同步四处文档」。
- **范围**：`hone.md`（原生编译章节：值模型/命令/限制）、`CHANGELOG.md`、`README.md`（`build --exe --native`）、官网 changelog（如存在对应页）；本进度文档状态收尾。
- **退出标准**：文档与实现一致；M0 退出标准（设计 §八）逐条核销。
- **涉及文件**：`hone.md`、`CHANGELOG.md`、`README.md`、`官网/`、本文件。
- **实现进度（2026-10-07）—— T10 完成**：
  - **hone.md**：命令表补 `hone build --exe --native`（含 natgen 别名说明）；新增 4.7「原生编译后端（hone build --exe --native）」章节（管线 / 句柄式值模型 / rt_call 分发 / 静态 catch 映射 / 三引擎逐字节对齐 / M0 支持范围 / 限制——子集外指令编译期 H990、依赖本机 rustc、与 -c 互斥 / 与 AOT 分工 / 验证基线），原 4.7 AOT 性能实测顺延 4.8。
  - **CHANGELOG.md**：顶部新增 [Unreleased] 段记录原生后端 M0（命令 / 值模型 / 调用模型 / 语义对齐 / 支持范围 / rustc 依赖 / 顺手修复 VM lambda 捕获寄存器绑定 bug 与 AOT Windows CRLF）。
  - **README.md**：用法清单补 `build --exe --native` 行；AOT 段后新增「原生编译后端」小节（语义对齐铁律 / 支持范围 / rustc 依赖 / 与 -c 互斥）。
  - **官网**：changelog.html 顶部插入 Unreleased 区块（含原生后端条目）；docs.html 命令表补 `--native` 行、AOT 章节改写为「AOT 原生编译与原生编译后端」并更新「三个后端」为四个执行后端 + M0 子集说明；index.html 命令表补 `--native` 行。
  - **退出标准达成**：四处文档与实现一致；M0 退出标准（设计 §八）逐条核销见「验收清单」。

---

## M0 已完成基线（整数子集）

- [x] `src/natgen.rs` lowering：leader 基本块切分（0 + 分支目标 + 条件 fall-through）
- [x] `block_succs()` 统一 terminator/后继判定，BFS 可达性与 emit 共用
- [x] 寄存器文件落地（M0 基线为每寄存器 8B i64 槽；T1 起 frame 内槽数组，T2 起句柄式 8B 句柄槽）
- [x] 整数算术 Add / Sub / Mul / Div / Mod（iadd / isub / imul / sdiv / srem）
- [x] 整数比较 Eq / Ne / Lt / Le / Gt / Ge（icmp + `select` 归一 i64 0/1）
- [x] 常量 LoadK / LoadBool / LoadNull（int/bool/null）
- [x] 控制流 Jmp / JmpIfFalse / JmpIfTrue + Label fall-through
- [x] 原生 Call：用户函数递归 + 内置（基线 3 个，T5 起统一走 rt_call）
- [x] `__hn_main` 导出（避 Rust `main` 符号冲突）；`rt_*` 声明 `Linkage::Import`
- [x] 链接：rustc `-C link-arg=<obj>` + 临时运行时
- [x] `hone natgen <file> [-o] [--obj]` 命令（parse → check → VM 字节码 → object → exe）
- [x] 错误码 H990（internal）/ H999（not implemented）；verifier 明细解包
- [x] **fib(15)=610，interp / VM / nat 三引擎逐字节对齐**

> T1 起 frame ABI 与 Hnv 模型替换基线的无帧 i64 约定，上表中「寄存器文件」「原生 Call」「链接」三项在新管线下重写，但语义结论（三引擎对齐）不变并持续验证。

---

## M1 — 核心语义（任务组）

- [x] **G1.1 RC+COW 值模型**（设计 §4.8）：`cow()` 门 + 惰性 path 深拷（list 写槽只拷外层 + 槽位、dict 同理）；debug 断言（rc 下溢 / 泄漏 / cow 后独占）
- [x] **G1.2 list 承载**：`HnList` + `NewList` / `Index` / `IndexSet` / `append` / `len` / 解构 `DestructGet`
- [x] **G1.3 dict 承载**：`HnDict` + 键值读写 / `keys` / `len`（`__struct__` 隐藏键过滤约定在 G2.1 生效）
- [x] **G1.4 for-in 迭代**：`IterCheck` + list/dict 迭代协议（与 VM 迭代序逐位一致）
- [x] **G1.5 别名用例集**：`tests/smoke/rc_alias.test.hn`（`b=a; a[i]=x; print(b)` 类，retain/cow/release 三路径）
- [x] **G1.6 全量回归**：`regress3` 无回退 + `regress_err` 接入nat 第三引擎（41 例）+ `regress4` 扩容至 6 例

### G1.1–G1.6 实现进度（2026-10-07）—— M1 完成

- **句柄式容器落地**：新增 `T_LIST` / `T_DICT` 标签 + `P_LIST`（`*mut HnList{items: Vec<u64>}`）/ `P_DICT`（`*mut HnDict{pairs: Vec<(String, u64)>}`）payload kind；`cell_release` 的 kind 分派补两个分支（逐项 `cell_release` 元素引用）。元素/值存**句柄引用**（进容器时 retain），故嵌套容器天然共享而不深拷。
- **COW 门 `cow_handle(h)`**：独占（rc==1）原样返回；共享（rc>1）按 kind 浅拷一份独占副本（`make_cell_list`/`make_cell_dict`逐项 retain，兄弟元素不碰——设计 §4.8 惰性 path 深拷的「只拷外层」形态）。**拷贝分支不动原 cell 的 rc**：各持有者仍各自持一份引用，rc 守恒。
- **写路径协议（rt_indexset）**：错误检查在cow **之前**（越界不应触发拷贝）；成功后若 `cow_handle` 返回原句柄（原地写分支）则 `rt_retain` 补一引用交还调用方，`store_slot` 替换槽时 drop 旧引用恰好守恒；拷贝分支则返回新句柄，槽的旧引用被 drop 后 rc 仍 ≥1（别名安全）。natgen 侧 `IndexSet` lowering 用「无条件 `store_slot(o, r)`」匹配该协议。
- **for-in 无专用指令**：VM 编译器已把 `for k[, v] in src` 降解为 `IterCheck / IsDict / 跳转 + Len / Index / Keys / Index` 序列（G1.4 只需 `rt_itercheck` 补齐 runtime 一端，迭代序天然与 VM 逐位一致）。推导式同理（`[x for x in list]` / `{k: v for k, v in dict}`）。
- **dict 无脚本层写入语法**：`d["k"]` 被checker 静态拒（索引须 int，而 dict 键恒为字符串字面量），故 dict 的 COW 写路径**脚本层不可达**，只有 list 写路径是 COW 主战场；`cow_handle` 的 P_DICT 分支仍保留（G2.1 struct 写入落地后启用）。取值走 for-in / `keys` / `values` / `has_key`。
- **G1.5 别名用例集**：`tests/smoke/rc_alias.test.hn` 11 组 35 断言——基础别名 / 三层别名链 / 反复写（独占原地分支）/ 交替写两别名 / 嵌套容器深索引 / 槽位整体替换 / 函数传参按值 / append 返回新容器 / 循环内改写自身 / dict 只读别名 / str 不可变别名。interp / VM / nat 三引擎逐字节一致（含 11 组标签输出）。
- **G1.6 回归接入**：
  - `regress4.py` 用例集3 → 6（增`tests/m1/list.hn`、`tests/m1/dict.hn`、`tests/smoke/rc_alias.test.hn`），退出码纳入比对。
  - `regress_err.py` 增 nat 第三引擎，两条路径：**编译成功**→跑产物比(stdout+stderr, 退出码)；**编译期拒**→剥离`[N/4]` 阶段进度行后与 `hone run` 比对（checker 是四引擎共享前端，静态拒属同源行为，必须逐字节对齐）。用例 37 → 41（补M1 解构形态/ 变量数 / 键类型 4 例）。能力边界（`await_non_future` H990）经`NAT_UNSUPPORTED` 显式登记待落地任务号，不静默跳过。
  - 新增 `tests/tools/audit_err.py`：55 例错误文案 + help 行批量比对探针（本轮据此发现 6 处缺陷，见下）。
  - `tests/m1/{list,dict}.hn` 正式用例集（各 13 / 12 组语义断言）。
- **审计暴露并修复的 6 处引擎缺陷**（均属「文档/直觉」与实现不一致，必须实地验证才能发现）：
  | # | 缺陷 | 位置 | 性质 |
  |---|---|---|---|
  | 1 | 解释器 `-x`遇 `i64::MIN` 直接 panic（`attempt to negate with overflow`），VM/nat 均报 H010 | `interp.rs` UnOp::Neg | **违反 interp/VM 一致铁律**；改 `checked_neg` + H010 + help |
  | 2 | VM 推导式双变量（`{k: v for k, v in dict}`）在**列表源**上静默退化为单变量，且编译期因列表路径未声明 `var2` 报「未定义变量」 | `vm.rs compile_comp` | interp/VM 分叉；新增 `Instr::Raise` 指令复现解释器「列表源 + 双变量」报错（natgen 同步 `rt_raise`） |
  | 3 | nat `rt_field` 只处理 error 对象，dict 点号访问报「requires an `error` value」 | `runtime rt_field` | 补 dict/struct 按键查找 + H002「unknown field（列出实有键）」，镜像 `interp field_value` 分支顺序 |
  | 4 | nat `rt_field` / `len` / `to_int` / `to_float` / `abs` 多处 help 缺失或文案漂移 | `runtime` 各错误臂 | 以 `builtins.rs` 为权威补help；对「解释器显式无 help」分支（如 `abs` 溢出）引入 `err_h_explicit_no_help`哨兵（`Some("")`），阻止渲染层 `hn_help_for` 凭空补行 |
  | 5 | nat 解构错误文案用值显示（`got 5`）而非类型名（`got int`） | `runtime rt_destructget` | 改 `type_name(c.t)` |
  | 6 | `raise` 时 `Hnv` 的 `error_p`存局部 `String` 指针会悬垂 | `runtime rt_raise` | 错误码归一为 `&'static str`（`HnErr.code_p` 契约要求静态生命周期） |
- **验证结论**：`cargo build` 通过；`cargo test` 4/4、`cargo test -p honeruntime` **18/18**（M0 11 + M1 7）；`regress3` **FAIL=0**（PASS≈46–47 / FLAKY 4–5 随机的即 alias_demo / server_demo / threads / time_random / uuid_demo 五例，真实逻辑差异恒为 0）；`regress_ir` **BAD=0**（OK≈38–39，FLAKY 同理）；`regress4`（三引擎）**6/6 全绿**；`regress_err`（三引擎）**41/41 全绿**（nat：运行期 18 / checker 同源拒 22 / 显式登记跳过 1）；`audit_err.py` **55/55 一致**（修复前 50/55）。

## M2 — 功能对齐（任务组）

- [x] **G2.1 struct**：构造 / `Field` / readonly 拦截 / `__struct__` 标记键全路径过滤（display/len/keys/for-in/解构/==/json）
- [x] **G2.2 enum**：`NewEnum` / `EnumElem` / `IsEnumVariant` + 点号变体构造（`Color.Red` 零参调用形态）
- [x] **G2.3 match**：模式匹配 lowering（字面量 / 变体载荷绑定 / 通配 / 无匹配 H005 / 绑定作用域 / 闭包捕获）；连带修复 6 处既有 interp-VM 分歧（见实现进度）
- [ ] **G2.4 运算符重载**：`__add` / `__eq` / `__neg` / `__len` 等回退链（镜像 `Vm::overload` 的触发条件：仅非 native 组合 + TYPE_MISMATCH）
- [ ] **G2.5 alias / goto**：alias 解析表接入 rt_call；goto 已是 Label+Jmp（基线覆盖，补用例）
- [ ] **G2.6 async-await**：`GoCall` / `Await` 线程 lowering（future 值 + wait，与 VM 线程语义对齐；跨线程值传递按 M2 边界）
- [ ] **G2.7 H999 边界对齐**：lowering 显式全覆盖单测（`Instr` 全变体断言）+ 新后端 H999 集合 = VM H999 集合 + `.t_neg` / `examples` 全过

### G2.1 实现进度（2026-10-07）—— 完成

- **范围界定（以 VM 实际语义为准）**：struct 实例 = 带 `\u{0}__struct__` 隐藏标记键的 dict（标记值 = 结构体名，与 VM/interp 构造臂一致）。字段写入 `p.f = x`（`FieldAssign`）在 **VM 编译期 H999**（`fail(NOT_IMPLEMENTED, "VM: 字段赋值 obj.field = x 暂不支持")`，`compile_module` 即返回 Err），natgen 管线先走 `compile_module` 故**同源被拦**，lowering 不会见到该指令——readonly 字段的运行时拦截在 VM 侧不可达（VM `struct_defs` 只存字段名不存 readonly 标志），G2.1 无需运行时拦截路径。范围条的「readonly 拦截」按此登记为「编译期 H999 同源」核销。
- **构造（runtime）**：`call_global` 在 func_map 之后、内置之前插 struct 臂（逐位镜像 VM `do_call` 顺序：局部变量 → async → 点号名 → func_map → struct → 内置 → H002）。`STRUCT_DEFS`/`STRUCT_FIELDS` 注册表经 wrapper `hn_rt_structs_init` 注入（`(strtab_idx, 字段数)` 交错 + 字段名 strtab 索引按定义序扁平）；实例首键 `__struct__` → 结构体名 strtab 句柄，其余键按字段序取值（缺省补 null；多余实参释放——checker 已静态拒绝，防御性对齐）。
- **相等语义修正（三引擎分叉点）**：原 `values_eq_h` 的 dict 臂对两侧做隐藏键过滤后逐对比较，导致**异名 struct 同值误判相等**（VM/interp 对 `Point(3,4) == Vec2(3,4)` 均判 false——VM 的 Rust `Vec==` 含标记键、interp `dict_values_eq` 显式同名校验）。改 `dict_values_eq_h`：双侧标记键均存在 → 同名且可见字段逐对相等；否则全键值逐对相等（普通 dict 无隐藏键走 else，行为不变）。该修正同时覆盖 `==`/`!=`（rt_binop 7/8）与 `assert_eq`（共用 `values_eq_h`）。
- **隐藏键过滤复用 M1 钩子**：display / len / keys / values / has_key / for-in / 解构 / 字段访问（unknown field 列键）各路径 M1 已带 `is_hidden_key` 过滤，本轮 struct 实例携带标记键后全部自动生效；`type_of(struct)` 报 `dict`（与 interp/VM 一致，struct 无独立类型名）。
- **wrapper（main.rs）**：strtab 收集补 struct 定义名 + 字段名 + `\u{0}__struct__` 标记键（隐藏键脚本不可书写，须显式收集）；生成 `NAT_STRUCT_DEFS` / `NAT_STRUCT_FIELDS` / `NAT_STRUCT_FIELD_COUNTS` 静态表并接 `hn_rt_structs_init`。
- **用例**：`tests/m2/struct.hn`（10 组：构造+字段读 / readonly 字段读取 / 隐藏键过滤 7 路径 / 相等四象限（同名同值、同名异值、**异名同值 false**、struct vs 普通 dict）/ 解构+推导式 / 嵌套 struct（Box{inner: Point}）/ list of struct / 别名+函数传参 / dict 值为 struct / 单字段边界）；错误用例 `struct_err_field.hn`（unknown field H002 列实有键）/ `struct_err_undef.hn`（checker 同源拒）。
- **用例侧语言约束（非 nat 差异）**：`pts[1].x` 语法不可写（字段链基只认标识符，parser 约束）；字段访问结果为 `Unknown` 类型，双 Unknown 的 `*` 报 H003（四引擎共享 checker 规则，一臂 `to_int` 即可）；函数内引用 struct 类型需 `[Point]` 类型参数声明（编译期擦除）。
- **验证**：`tests/m2/struct.hn` 47 行输出三引擎逐字节一致；两错误用例三引擎一致；`regress4` **7/7 全绿**（6 → 7，新增 struct.hn）；`regress_err` 41/41 无回退；`regress3` 真实逻辑差异 = 0；`regress_ir` BAD=0；`cargo test -p honeruntime` 18/18。

### G2.2 实现进度（2026-10-07）—— 完成

- **范围界定（以 VM 实际语义为准）**：enum 值 = 三字段结构（枚举名 `ty` + 变体 `variant` + 载荷 `payload` 句柄数组），运行时以 `T_ENUM`/`P_ENUM` 类型与 `HnEnum{payload}` 载荷建模，`a`/`b` 复用句柄 cell 槽存枚举名/变体名的 strtab 索引（索引相等 ⇔ 名字相等，主程序侧 strtab 去重）。
- **两条构造入口（镜像 VM `do_call` 分发序：enum → func → 内置 → 库未加载）**：
  1. **点号零参形态 `Color.Red`**：VM 编译臂把它编成 `Call("Color.Red", 0)`（`enum_defs.contains_key(head)` 命中），运行时经 `rt_call` → `call_global` 点号分支的 **enum 臂**构造（`ENUM_DEFS`/`ENUM_VARIANTS` 注册表查变体，无实参）。
  2. **带载荷形态 `Shape.Circle(1.5)`**：VM 把带参构造 `Expr::Call(Ident "Shape.Circle")` 也走点号 Call，实参打包入 `param_ss`（`NewEnum` lowering 臂同 `Call` 约定：retain 入连续区，runtime 消费并 `drop_args` 释放）。
- **三个指令 lowering（镜像 vm.rs 执行臂 2058–2085）**：`rt_newenum(n_sidx,v_sidx,argptr,nargs)`（无错误路径，逐项 retain 载荷入 box）；`rt_enumelem(src,idx,ci,pc)`（借 src 返回载荷新句柄，非枚举/越界 → H001）；`rt_isenumvariant(src,n_sidx,v_sidx)`（恒返 bool，供 match 模式分派）。均入 `RtImports`/`RtRefs` 并同步 `max_pack`（`NewEnum` 占 `n` 个打包槽）。
- **语义臂复用 M1 钩子**：`display`（`P_ENUM`：`Name.Variant` 无载荷 / `Name.Variant(v, …)` 有载荷，镜像 interp `display` 的 Enum 臂）；`==`/`!=`（`values_eq_h` 的 `P_ENUM` 臂：枚举名 + 变体 + 载荷逐项递归相等，镜像 interp `Value::eq` 的 Enum 臂；`rt_binop` 7/8 的容器守卫补 `T_ENUM`）；`type_of` → `enum`；`len` 的 else 臂经 `type_name(T_ENUM)` 报 `enum`（与 builtins.rs `len` 文案同源）。
- **wrapper（main.rs）**：strtab 收集补 enum 定义名 + 各变体名；生成 `NAT_ENUM_DEFS`（`(strtab_idx,变体数)` 交错）/ `NAT_ENUM_VARIANTS`（`(变体名 sidx,载荷数)` 扁平）/ `NAT_ENUM_VARIANT_COUNTS` 静态表并接 `hn_rt_enums_init`。
- **用例**：`tests/m2/enum.hn`（8 组：简单变体构造+显示 / 带载荷变体 / match 无载荷+载荷绑定+通配+作函数参数 / 相等四象限（同名同值、同名异值、**跨枚举同名变体 false**、载荷递归）/ type_of / enum 进 list（显示/索引/相等）+ dict 值 / 别名+函数传参 / 多类型载荷（str+int）显示与相等）；错误用例 `regress_err.py` 新增 `enum_for_in`（运行期 H001 `for in` 非 list/dict，nat 真走 `rt_itercheck`）/ `enum_len`（运行期 H001 `len` 非 string/list/dict/bytes，nat 真走 `call_builtin` 的 len else 臂）/ `enum_payload_oob`（checker 同源拒 H005）。
- **审计暴露 2 处既有引擎分歧**（非 G2.2 引入，本轮 diff 未触碰 vm.rs/interp.rs，登记待修；**已在 G2.3 一并修复**，详见 G2.3 进度）：
  1. **`if (enum值)` truthy 判定**：interp 报 `H001 if condition must be bool, got enum`（拒绝），VM 却当 truthy 放行（`if(enum)` 走真分支）。探针确认 `if(5)`/`if("a")` 两引擎一致报 H008，说明 `if` 本应严格 bool，enum 应同型拒绝——**VM 侧漏了 enum 类型的 bool 校验**。`enum.hn` 不固化该路径。
  2. **解构 other 臂文案 `type_name` vs `value_to_str`**：`x, y = 非容器值` 时 interp 报 `got <type_name>`（如 `enum`/`int`），VM 报 `got <value_to_str>`（如 `E.A(1)`/`5`）。探针确认**纯 `a, b = 5` 单语句即分叉**（与 enum 无关），且既有用例 `ds_notiter`（`a,b=5; print(a,b)`）因 `print` 的 H011 参数数错误**掩盖**了该解构分歧从未被暴露。**建议修 VM `v_destruct_get` 的 other 臂改用 `other.type_name()` 与 interp 对齐。**
- **验证**：`tests/m2/enum.hn` 三引擎（interp/VM/nat）逐字节一致；3 个新错误用例三引擎对齐全 PASS（`enum_for_in`/`enum_len` 走 nat 运行期、`enum_payload_oob` 走 checker 同源拒）；`regress4` **8/8 全绿**（7 → 8，新增 enum.hn）；`regress_err` **43/43 全绿**（41 → 43，新增 2 运行期 + 1 同源拒）；`regress3` 真实逻辑差异 = 0；`regress_ir` BAD=0；`cargo build` 通过（7 条存量警告，非本轮引入）。

### G2.3 实现进度（2026-10-07）—— 完成

- **范围界定（以解释器实际语义为准）**：`match` 在 VM 编译器里降解为 `Eq` / `IsEnumVariant` / `EnumElem` +
  分支跳转，指令集 G2.2 已全覆盖，**natgen 无需新增 lowering**；本任务实际工作是
  ①把 match 的运行期错误路径与作用域语义对齐解释器 ②补正式用例集 ③连带修掉审计暴露的既有引擎分歧。
- **探针先行（`tmp_probe/match_probe*.py`，36 例形态矩阵）**：先跑 interp vs VM 形态比对再动手，
  一次性扫出 3 处 match 侧分叉（无匹配错误码、绑定变量泄漏、lambda 捕获取外层）；
  再用条件/逻辑/解构探针扫出 3 类更严重的既有分叉（见下「连带修复」）。
- **无匹配分支错误码对齐**：VM 原发 `ThrowStr("match failed: 无匹配分支 (H005)")` → 运行期 H600，
  与解释器 H005「no match arm matched the value」+ help 不一致——改发 `Instr::Raise(codes::SYNTAX, …)`。
  连带两处同步：runtime `rt_raise` 的错误码白名单（未命中会静默归 H990，nat 曾渲染成 H990）、
  文本 IR 装配的 `RAISE` 码表（缺 H005 会导致 `--disasm` 往返失败）。nat 三引擎逐字节一致。
- **绑定变量作用域**：绑定原用 `decl()` 插进**所在作用域且从不弹出** → 绑定寄存器顶掉外层同名变量
  （`rr` 绑定后外层 `rr` 在 VM 侧被改写，解释器分支结束即 `pop_scope`）。改为
  「`scopes.push` + `decl_tmp` + 分支末 `scopes.pop`」（与推导式循环变量同口径）：不推进 `var_next`、
  不写 `chunk.locals`，对外层完全无痕。
- **lambda 捕获改内层优先**：`Expr::Lambda` 捕获表按「外→内」遍历且首见即取，遮蔽场景下捕获到被遮蔽的
  外层值（`match` 绑定遮蔽外层名时，闭包拿到外层值而非绑定值）——改为 `.rev()`（与 `lookup` 同序）。
- **连带修复的 3 类既有引擎分歧（G2.2 登记 2 处 + 新发现 1 类）**：
  | # | 分歧 | 位置 | 性质 |
  |---|---|---|---|
  | 1 | 条件 truthy 放行：6 处站点（if/while/do-while/for/ternary/推导式 `if` 过滤）非 bool 值当真 | `vm.rs` JmpIfFalse/JmpIfTrue 执行臂 | G2.2 登记项；`do {} while(enum)` 原先**死循环**；enum/struct 等动态值全部放行 |
  | 2 | `&&`/`||` **无短路**：右侧在分支前无条件求值；且操作数非 bool 时当 truthy 并直接返回该值 | `vm.rs` `Expr::Binary` And/Or 臂 | 新发现（**可见副作用的正确性 bug**）：`false && f()` 仍调用 f、`true \|\| (1/0==0)` 触发除零 |
  | 3 | 解构非容器文案：VM 用值显示 `got 5`，解释器用类型名 `got int` | `vm.rs` `v_destruct_get` other 臂 | G2.2 登记项（被 `print` H011 静态错误掩盖，从未暴露） |
- **新指令两条（VM / natgen / runtime / 文本 IR 四处同步）**：
  - `Instr::CondCheck(reg, kind)`：非 bool → H001，消息按 kind 分站点（0=if 1=while 2=do-while
    3=for 4=ternary 5=逻辑运算 6=推导式 if），**均无 help 行**（解释器七处均传 `None`，runtime 用
    `err_h_explicit_no_help` 哨兵阻渲染层回退补行）。span 取解释器同款：语句条件取 `expr_span(cond)`，
    逻辑运算/ternary 取整表达式 span（= 运算符 token）。
  - `Instr::IsBoolVal(dst, src, want)`：值是否恰为 `Bool(want)`，无类型错误——供 `&&`/`||` 短路判定，
    镜像解释器 `if let Value::Bool(false) = l`（**注意：短路判据是「恰为该布尔值」，非 truthy**）。
  - And/Or 重编译：`Expr::Binary` 对 And/Or 提前分流——先求值左操作数 → `IsBoolVal` 判短路 →
    短路才编译右操作数（真短路）→ `CondCheck(rhs, 5)` → 结果为右值；非短路路径结果为该布尔字面量。
  - runtime 新增 `rt_condcheck` / `rt_isboolval`（`rt_condcheck` 通过时返回 null 句柄，由 natgen 显式
    drop，同 `rt_itercheck` 口径）；natgen 加 `sig2_1` 签名 + 两个导入 + 两个 lowering 臂。
  - 文本 IR：`CONDCHK r{reg} {kind}` / `ISBOOLV r{dst} r{src} {true|false}`（`regress_ir` 往返 BAD=0）。
- **探针固化为工具**：`tests/tools/probe_align.py`（50 例：match 20 / cond 11 / logic 14 / destruct 5），
  默认 interp vs VM，`--nat` 追加第三引擎。与 `audit_err.py` 分工：
  前者盯正常路径形态与动态值下的条件/逻辑，后者盯错误文案与 help 行。
  （自写比对脚本注意剥 nat build 的 `[N/4]` 阶段行，否则 checker 同源拒会误判成分歧。）
- **用例**：`tests/m2/match.hn`（6 组：字面量五类型 / 臂顺序与无通配命中 / 变体无载荷·多载荷绑定·
  `_` 忽略·跨枚举同名变体 / 绑定作用域（遮蔽后外层不变、同表达式内 `+ rr` 读外层、臂后同名可重声明）/
  闭包捕获绑定与外层变量 / 嵌套·函数返回·作实参·list 字面量·循环累加），24 行输出三引擎逐字节一致；
  `tests/m2/conditions.hn`（5 组：短路副作用可见性 / 短路避免除零 / 普通布尔组合 / 优先级与结合 /
  逻辑结果直接作条件）。错误用例：`match_as_stmt`（表达式语句位置解析期 H005）、`match_no_arm`、
  `match_no_arm_enum`、`cond_if/while/dowhile/for/ternary_enum`、`cond_struct_if`、`logic_and/or_enum`、
  `logic_and_rhs`、`comp_if_enum`、`ds_notiter_run`、`enum_destruct`（G2.2 登记项修复后解除冻结，
  共 15 例新增）。
- **验证**：三引擎探针矩阵 50 例（match 20 / cond 11 / logic 14 / destruct 5，`--nat`）全部一致；
  `regress4` **10/10 全绿**（8 → 10，新增 match.hn / conditions.hn）；
  `regress_err` **全绿**（43 → 57，新增 15 例）；`regress3` 真实逻辑差异 = 0（And/Or 重编译覆盖
  全部 examples 无回退）；`regress_ir` BAD=0（两条新指令往返 OK）；`cargo test` 4/4；
  `cargo test -p honeruntime` **19/19**（新增 `condcheck_and_isboolval`：消息逐字 + 无 help 行 +
  定位 caret + 短路判定四象限 + 堆计数归零）。

## M3 — 集成与优化（任务组）

- [ ] **G3.1 import 构建期解析**：本地路径递归合并 + 远程 `~/.hone/cache/` 下载（设计 §7.3）；跨文件同名定义报错
- [ ] **G3.2 裸链接 + 交叉编译**：切 rust-lld 裸链接（设计 §5.1/5.2）；三目标 windows-gnu / linux-musl / aarch64-musl（zig 存根 → 内嵌 libdl 桩）；`--target` 选项 + 产物目标后缀
- [ ] **G3.3 体积裁剪**：`--gc-sections` + 按脚本静态收集调用的内置 → 裁剪分发表；目标 ≤3MB
- [ ] **G3.4 工具链打包**：`targets/` 运行时归档（每目标 staticlib）+ CRT + lld 随 hone 分发；`HONE_TOOLCHAIN_DIR` 覆盖；安装脚本改 zip/tar.gz 包
- [ ] **G3.5 静态表入 object**：const blob / span 表 / 函数注册表移入 script.o rodata（设计 §4.7，替换 wrapper 静态表）+ 启动 FNV 自检
- [ ] **G3.6 CI 自动化**：`ci.yml` 增 nat 三引擎回归（`regress4.py`）；release 流水线发布三目标脚本产物 + 交叉产物 CI 二次验证
- [ ] **G3.7 CPU 基线**：x86_64 默认 baseline+SSSE3（设计 §6.3）+ `--cpu` 选项

---

## 验收清单（设计 §十「完成定义」）

- [ ] `hone build --exe hello.hn` 在无 gcc 的干净机器上产出可运行 exe（M3 完成无 rustc 依赖）
- [ ] `hone build --exe app.hn --target x86_64-unknown-linux-musl` 在 Windows 上产出 linux exe，CI 二次验证（G3.2/G3.6）
- [x] `regress4.py` 正常路径三引擎（interp/VM/nat）逐字节对齐——M0 三例 + M1 三例（list/dict/别名）
      + M2 四例（struct / enum / match / conditions）共 **10 例全绿**
- [x] `regress_err.py` 错误诊断三引擎逐字符对齐——**57 例全绿**（nat：运行期 34 / checker 同源拒 22 / 显式登记跳过 1）
- [x] `tests/tools/audit_err.py` 55 例错误文案 + help 行 nat-vs-interp 一致（M1 据此修复 6 处引擎缺陷；
      G2.3 的 6 处分歧由条件/逻辑/match 探针矩阵发现，`audit_err.py` 待按新错误形态扩充用例）
- [ ] 新后端 H999 集合与 VM 完全一致（无静默漏实现）（G2.7）
- [ ] 产物 ≤5MB（M3 后 ≤3MB）（G3.3）
- [x] 文档同步：`hone.md`（§1.4 短路与条件 / §1.10 match / §4.7 后端范围）、`CHANGELOG.md`、
      `README.md`、官网 `docs.html` + `changelog.html`（`gen_changelog.py` 重生成）；
      官网待上传（沿用 M1 起的既有流程）

---

## 决策记录

### M0 基线范围裁剪（用户指令）

| 项 | 设计文档 | M0 基线实际 | 理由 |
|---|---|---|---|
| 值模型 | `Hnv` tag + RC+COW（§4.3/§4.8） | 纯 i64 深拷贝，无 tag | 用户指令：先跑通管线，砍掉 RC+COW |
| 错误渲染 | 对齐 ZError 行/列/文案（§4.5/§7.4） | `HnError(String)` 级别打印退出 | 用户指令：M0 只保证遇错打印字符串退出（T4 恢复逐字节对齐） |
| 运行时函数 | 完整 `rt_*` 族 + 分发表 | 仅 `rt_print` / `rt_arith_add` / `rt_arith_sub` | 用户指令：只写三个函数（T5 恢复完整分发） |
| 目标 | 三目标矩阵（§6.1） | 仅 `x86_64-pc-windows-gnu` 本机 | M0 先本机打通（G3.2 恢复） |
| 文件布局 | `natgen/{mod,lowering,runtime_abi,statics}.rs`（§九） | 单文件 `src/natgen.rs` | 原型收敛；T2 起按指令量增长再拆 |

### M0 剩余任务已定决策（2026-10-05）

| 项 | 决策 | 理由 |
|---|---|---|
| 值 ABI | `Hnv` 24B 按值；寄存器槽 = frame 内 24B 偏移；`rt_*` 一律走槽指针（`rt_binop(op,xs,ys,ds)` 形态） | 统一易 lower，语义核对集中在 Rust 侧；性能问题 M1 RC+COW 解决 |
| 堆值所有权 | M0 即启用引用计数（retain/release）；COW 惰性 path 深拷留 M1（G1.1） | 无 RC 则槽覆盖即泄漏，且「槽写=释放旧值」无法静态判定；retain 是 §4.8 的前半，提前不改变值语义 |
| 静态表位置 | M0 经「每脚本生成的 wrapper」承载（const 字符串 / span 表 / 函数注册表）；M3 移入 object rodata（G3.5） | Cranelift ObjectModule 取外部数据地址的 API 面窄；wrapper 是既有模式（临时运行时已如此），切换成本隔离在 T10/G3.5 |
| runtime 形态 | 单文件 `runtime/src/lib.rs`（std-only）；双轨：workspace 成员（cargo test）+ `include_str!` 嵌入 hone 二进制（natgen 时 rustc 编译 rlib） | 构建期零外部工具（G2）；干净机器无仓库也能编译；M3 换预编译 staticlib 后去掉 rustc 依赖 |
| 调用模型 | M0 统一走 `rt_call` 名分发，不做编译期直调内联 | 正确性优先；与 `Vm::do_call` 逻辑单点镜像；直调优化留 M1 |
| 错误传播 | 函数返回值 ERR 跨帧传播 + 每 fallible 指令静态最近 catch 映射（§4.5） | Cranelift 无 setjmp；与 VM `try_catch` 的「仅建立帧捕获」语义天然同构 |
| lambda 块 | lowering 为独立导出函数 `hn_f<ci>`；`HnFn{ci, caps}` 经 `rt_call_fn` 调度 | 与用户函数同构，捕获按值（整表 retain）对齐 C 运行时语义 |

### G2.3 已定决策（2026-10-07）

| 项 | 决策 | 理由 |
|---|---|---|
| match 语义基准 | 以**解释器实际行为**为准修 VM（无匹配 → H005 + help；绑定随分支出作用域），不反向改解释器 | 解释器是规范载体且文案更完整；VM 侧改动不影响既有 nat 语义（nat 镜像 VM 字节码，自动跟随） |
| 短路判据 | `&&`/`||` 的短路条件是「左操作数**恰为** `false`/`true`」，不是 truthy | 镜像解释器 `if let Value::Bool(false) = l`；这是唯一能逐字节对齐的判据（truthy 判据在 `0`/`""` 等动态值上分叉） |
| 严格 bool 校验落点 | 新增 `CondCheck` 指令在**编译期**逐站点发射（而非在 JmpIfFalse 里统一判） | 各站点文案与 span 不同（`if` vs `while` vs 运算符 token），统一判无法还原定位；编译期发射对 nat 是零成本（同一字节码） |
| 绑定变量寄存器 | 走 `decl_tmp` + 分支作用域（不进 `var_next` / `chunk.locals`） | 与推导式循环变量同口径；`decl` 会推高 `var_next` 并让 `exit()` 把 `tmp_next` 拉回低水位，可能复用表达式内活跃临时寄存器 |
| lambda 捕获顺序 | 内层优先（`scopes.iter().rev()`，与 `lookup` 同序） | 遮蔽下解释器捕获内层值；原正序首见即取会捕获被遮蔽的外层值 |
| 探针形态 | 固化为 `tests/tools/probe_align.py`（含 `--nat`），不留在临时目录 | 与 `audit_err.py` 同定位：可复用的开发期扫雷工具，须随仓库维护 |
