# Hone 原生编译后端 — 进度清单

> 配套：`Hone原生编译后端设计.md`（设计）；本文件跟踪落地进度，按可独立验收的任务拆分。
> 更新：2026-10-05
> 图例：`[x]` 完成 · `[ ]` 待办 · `[~]` 进行中 · `[!]` 已按用户指令裁剪/延后

## 总览

| 里程碑 | 状态 | 说明 |
|---|---|---|
| **M0 管线打通** | 🟡 进行中 | 整数子集已通（fib 三引擎对齐）；值模型/错误/rt_call 按 T1–T10 推进 |
| **M1 核心语义** | ⬜ 未开始 | list/dict/迭代/RC+COW；任务组 G1.1–G1.6 |
| **M2 功能对齐** | ⬜ 未开始 | struct/enum/match/重载/alias/async；任务组 G2.1–G2.7 |
| **M3 集成与优化** | ⬜ 未开始 | import 合并/交叉编译/体积裁剪/工具链打包/CI；任务组 G3.1–G3.7 |

**当前可运行产物**：`bench/fib15.hn` → `fib15.nat.o` → `bench/fib15.exe`，执行输出 `610`（interp / VM / nat 逐字节对齐）。
**复现**：`hone natgen bench/fib15.hn -o bench/fib15.exe && ./bench/fib15.exe`
**验证前置**：改代码先 `cargo build`（`target/` 旧二进制会静默沿用），回归脚本依赖 `target/debug/hone.exe`。

---

## M0 — 管线打通（任务分解）

> 已完成部分（整数子集）见「M0 已完成基线」节。以下为剩余任务的执行顺序（T2 依赖 T1，T3–T6 依赖 T2/T4 渐进，T7–T9 依赖 T6）。

### T1 工程地基：runtime 独立 crate + 链接管线升级 ⬜

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

### T2 Hnv 值模型 + 常量 + print 全类型 ⏳

- **目标**：落地设计 §4.3 值模型：寄存器槽 8B → 24B `Hnv{tag,a,b}`；str/char/float/null 常量承载；`print` 全类型输出。
- **范围**：
  - 标签：`INT / FLOAT / BOOL / CHAR / NULL / STR / ERR / FN`；标量内联（a 位），`STR / ERR / FN` 为 `*const HnBox`（a 指针）。
  - `HnBox{rc: AtomicUsize, kind, payload}`：**M0 只做 retain/release 引用计数**（COW 惰性深拷留 M1，见 G1.1）；rodata 常量串同样走 box（rt_init 时建箱，LoadK = retain）。
  - 槽写统一 `rt_slot_store(dst, val)`：释放旧堆值、存新值（所有权转移语义：load 为借用、store/call 实参/返回值转移）。
  - 字符串静态表：全部 `Const::Str` 去重后编入生成式 wrapper，object 经 import 函数取指针（M3 移入 object rodata，见 G3.5）。
  - `Hnv::display` 对齐 `interp::Value::display` 的 M0 子集（int/float/bool/char/str/null；float 用 std Display 天然一致）。
- **退出标准**：`examples/hello.hn` + float/bool/char/null 打印脚本，nat 产物与 interp / VM 逐字节一致；fib 仍对齐；debug 构建下出口断言无 box 泄漏（rc 归零）。
- **涉及文件**：`runtime/src/lib.rs`、`src/natgen.rs`、`src/main.rs`（wrapper 生成）。
- **实现进度**：`runtime` 值模型已落地并通过 6 项单测——Hnv 24B 三寄存器、HnBox 引用计数 retain/drop 平衡（出口 rc 归零、HEAP_LIVE 无泄漏）、错误码用胖指针还原（`&'static str` 无 NUL 结尾，不能用 CStr）、strtab 建箱/取值往返、算术/比较/显示逐字对齐 VM 的 M0 子集（int/float 溢出 H010、除零 H009、类型不匹配 H001、char 比较、display 全标量）。**natgen 24B 槽 lowering（槽 8B→24B、LoadK 构造 Hnv、Call→rt_binop/rt_print、cond 取 Hnv truthy）进行中**——它是 T3 地基（rt_binop 收发 Hnv）。

### T3 运算与比较（rt_binop / rt_cmp / rt_unop）⬜

- **目标**：算术/比较/一元运算全部改为运行时分发，逐条镜像 `vm.rs` 的 `v_add/v_sub/v_mul/v_div/v_mod/v_eq/v_cmp/v_neg/v_not`。
- **范围**：
  - `rt_binop(op, xs, ys, ds)`：int/float 全组合（含 int×float 提升）、str+str 拼接、除零 H009、整数溢出 H010、类型不匹配 H001。
  - `rt_cmp`：`==`/`!=`（int/float/bool/char/str/null 等值类型，镜像 `v_eq` 的 M0 可等集合）；`<`/`<=`/`>`/`>=`（按 `v_cmp` 实际语义，含 str 字典序）。
  - `rt_unop`：`Neg` / `Not`（含类型错误）。
  - lowering：每个 fallible 指令后 `tag==ERR` 检查 → 无 handler 则原样 return 跨帧传播（T6 补 handler 跳转）。
- **退出标准**：`tests/smoke/arithmetic.test.hn` 数值/比较/类型谓词子集 nat 对齐；除零/溢出/类型错误文案与 VM 一致（完整渲染 T4）。
- **涉及文件**：`runtime/src/lib.rs`、`src/natgen.rs`。

### T4 错误渲染 + 静态表 + rt_unwind ⬜

- **目标**：错误输出与双引擎逐字节一致（行/列/caret/help/exit code）。
- **范围**：
  - Program 静态表（经 wrapper）：`file` / `src` 全文、每 chunk 的 span 表 `(line,col,len)×code_len`、每 chunk 内联字符串表（`ThrowStr` 等）。
  - `HnErr{code,msg,span,help}`：rt 函数取 pc 查 span；help 文本镜像 `vm_help_for`（覆盖 M0 可达码：H001/H002/H006/H007/H009/H010/H011/H600/H700）。
  - `rt_unwind(err)`：复刻 `ZError::Display`（含 Tab→4 空格 caret 对齐）→ stderr → `exit(1)`。
  - 内置参数错误（arg_err 文案）、`to_int`/`to_float` 失败（H006/H007）纳入。
- **退出标准**：`examples/err_divzero.hn` 等错误用例，nat 与 interp / VM 逐字节一致（含 `--> file:line:col`、caret 行、`help:` 行、退出码 1）。
- **涉及文件**：`runtime/src/lib.rs`、`src/natgen.rs`（span 表收集）、`src/main.rs`（wrapper 生成）。

### T5 rt_call 完整分发 + lambda ⬜

- **目标**：设计 §4.4/§4.6 —— 所有 `Call` 统一走 `rt_call` 名分发（M0 不做直调内联），支持用户函数递归、lambda、内置注册表（M0 子集）。
- **范围**：
  - `rt_call` 解析顺序镜像 `Vm::do_call`：当前帧局部变量（lambda 闭包 / 函数名字符串）→ 点号名（用户函数 / 模块内置 → H999 / 库未加载 H404）→ 用户函数 → struct 构造（M0 报 H999）→ 内置表 → `H002 undefined function`。
  - 内置表 M0 子集在 runtime 内逐条镜像 `builtins.rs`：`print / len / to_str / to_int / to_float / is_int / is_float / is_bool / is_str / type_of / abs / max / min / assert / assert_eq`（assert 失败 H700 文案 + 计数对齐）；表外但 VM 支持的内置 → 显式 H999。
  - 函数注册表（经 wrapper）：每 chunk `{nregs, locals: [(name,reg)], param_regs, cap_regs}`。
  - `MakeLambda` → `rt_makelambda`：`HnFn{ci, caps: Vec<Hnv>}`（捕获值整表 retain，按值捕获语义）；lambda 调用 → `rt_call_fn`（捕获填 cap_regs、实参填 param_regs、alloc frame 调 `__hn_f<ci>`）；lambda 块 lowering 为独立导出函数 `hn_f{ci}`。
- **退出标准**：`tests/smoke/functions.test.hn`（递归 / lambda / 向前引用）与 f-string 用例（`to_str` + 拼接）nat 对齐；lambda 捕获变量外层再修改不影响闭包（值语义）。
- **涉及文件**：`runtime/src/lib.rs`、`src/natgen.rs`、`src/main.rs`（wrapper 生成）。

### T6 最小 try-catch / throw ⬜

- **目标**：设计 §4.5 无 setjmp 方案 —— lowering 期静态 catch 映射 + ERR 值跨帧传播。
- **范围**：
  - 每 chunk 预计算：每个 pc 的「最近活跃 handler」`TryBegin(handler, catch_reg)`（TryPop 出栈；嵌套取最内层）。
  - fallible 指令后：`tag==ERR` → 有 handler：err 值移入 catch_reg 槽、跳 handler 块；无 handler：return 传播（T3 已有）。
  - `Throw` / `ThrowStr` lowering：镜像 `normalize_throw`（str→H600、error 原样、其余→H600+value_to_str）；`TryBegin`/`TryPop` 为 lowering 期元指令（生成代码无运行时痕迹）。
  - 跨帧语义对齐 `Vm::try_catch`：catch 只发生在建立 try 的帧（天然由「函数返回 ERR、调用方检查」实现）。
- **退出标准**：`examples/try_catch.hn` 及「错误被 catch 后继续执行」用例 nat 与 interp / VM 逐字节一致（H600 文案、catch 变量值、后续输出）。
- **涉及文件**：`src/natgen.rs`（catch 映射 + Throw lowering）、`runtime/src/lib.rs`（normalize 镜像）。

### T7 命令集成：`hone build --exe --native` ⬜

- **目标**：设计 §7.1 命令统一 —— natgen 管线并入 `hone build --exe`。
- **范围**：`hone build --exe --native <file.hn> [-o] [--obj]` 与 `hone natgen` 共用同一管线函数（重构 `cmd_natgen` 为共享入口）；help 文案同步（含 `[1/4]` 阶段输出风格）；`hone natgen` 保留为别名。`-c`（C 后端）行为不变，无 C 编译器自动切换留 M3。
- **退出标准**：两命令产物等价；`hone --help` 文案准确；fib/hello 经新命令复跑对齐。
- **涉及文件**：`src/main.rs`。

### T8 四引擎回归 `regress4.py` ⬜

- **目标**：设计 §十 / 进度「回归接入」—— 新后端成为第四引擎。
- **范围**：`tests/regress4.py`：interp（`hone run`）/ VM（`--vm`）/ AOT-C（`build --exe -c`，无 gcc 时跳过并标注）/ nat（`build --exe --native`）四路 stdout+stderr 逐字节比对；用例集 = `examples/fib.hn`、`examples/hello.hn` + 错误用例（`err_divzero` 等 M0 可对齐项）；SKIP/FLAKY 复用 `regress_common.py`；nat 产物写临时目录、跑完即删。
- **退出标准**：`python tests/regress4.py` 全绿（M0 用例集）；失败时打印首个分叉行。
- **涉及文件**：`tests/regress4.py`、`tests/regress_common.py`（如需扩 FLAKY 表）。

### T9 冒烟用例扩展（M0 对齐子集）⬜

- **目标**：M0 退出标准「四引擎对齐」从 fib/hello 扩到冒烟子集。
- **范围**：筛选 `tests/smoke/` 中 M0 指令集可覆盖的用例入 regress4：`arithmetic`（全量）、`functions`、`strings`、`errors`（可对齐子集）、`controls`（不含 for-in/list 的部分）；不可对齐项在本文档登记原因（留 M1/M2）。
- **退出标准**：子集四引擎全绿；登记完整。
- **涉及文件**：`tests/regress4.py`、本文档。

### T10 文档同步 ⬜

- **目标**：项目约定「新功能需同步四处文档」。
- **范围**：`hone.md`（原生编译章节：值模型/命令/限制）、`CHANGELOG.md`、`README.md`（`build --exe --native`）、官网 changelog（如存在对应页）；本进度文档状态收尾。
- **退出标准**：文档与实现一致；M0 退出标准（设计 §八）逐条核销。
- **涉及文件**：`hone.md`、`CHANGELOG.md`、`README.md`、`官网/`、本文件。

---

## M0 已完成基线（整数子集）

- [x] `src/natgen.rs` lowering：leader 基本块切分（0 + 分支目标 + 条件 fall-through）
- [x] `block_succs()` 统一 terminator/后继判定，BFS 可达性与 emit 共用
- [x] 寄存器文件落地（M0 基线为每寄存器 8B i64 槽；T1 起改为 frame 内槽数组，T2 起 24B Hnv）
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

- [ ] **G1.1 RC+COW 值模型**（设计 §4.8）：`cow()` 门 + 惰性 path 深拷（list 写槽只拷外层 + 槽位、dict 同理）；debug 断言（rc 下溢 / 泄漏 / cow 后独占）
- [ ] **G1.2 list 承载**：`HnList` + `NewList` / `Index` / `IndexSet` / `append` / `len` / 解构 `DestructGet`
- [ ] **G1.3 dict 承载**：`HnDict` + 键值读写 / `keys` / `len`（`__struct__` 隐藏键过滤约定在 G2.1 生效）
- [ ] **G1.4 for-in 迭代**：`IterCheck` + list/dict 迭代协议（与 VM 迭代序逐位一致）
- [ ] **G1.5 别名用例集**：`tests/smoke/rc_alias.test.hn`（`b=a; a[i]=x; print(b)` 类，retain/cow/release 三路径）
- [ ] **G1.6 全量回归**：`regress3` 全部用例四引擎对齐 + `regress_err` 错误逐字符对齐（nat 接入）

## M2 — 功能对齐（任务组）

- [ ] **G2.1 struct**：构造 / `Field` / readonly 拦截 / `__struct__` 标记键全路径过滤（display/len/keys/for-in/解构/==/json）
- [ ] **G2.2 enum**：`NewEnum` / `EnumElem` / `IsEnumVariant` + 点号变体构造（`Color.Red` 零参调用形态）
- [ ] **G2.3 match**：模式匹配 lowering（载荷绑定 / 通配）
- [ ] **G2.4 运算符重载**：`__add` / `__eq` / `__neg` / `__len` 等回退链（镜像 `Vm::overload` 的触发条件：仅非 native 组合 + TYPE_MISMATCH）
- [ ] **G2.5 alias / goto**：alias 解析表接入 rt_call；goto 已是 Label+Jmp（基线覆盖，补用例）
- [ ] **G2.6 async-await**：`GoCall` / `Await` 线程 lowering（future 值 + wait，与 VM 线程语义对齐；跨线程值传递按 M2 边界）
- [ ] **G2.7 H999 边界对齐**：lowering 显式全覆盖单测（`Instr` 全变体断言）+ 新后端 H999 集合 = VM H999 集合 + `.t_neg` / `examples` 全过

## M3 — 集成与优化（任务组）

- [ ] **G3.1 import 构建期解析**：本地路径递归合并 + 远程 `~/.hone/cache/` 下载（设计 §7.3）；跨文件同名定义报错
- [ ] **G3.2 裸链接 + 交叉编译**：切 rust-lld 裸链接（设计 §5.1/5.2）；三目标 windows-gnu / linux-musl / aarch64-musl（zig 存根 → 内嵌 libdl 桩）；`--target` 选项 + 产物目标后缀
- [ ] **G3.3 体积裁剪**：`--gc-sections` + 按脚本静态收集调用的内置 → 裁剪分发表；目标 ≤3MB
- [ ] **G3.4 工具链打包**：`targets/` 运行时归档（每目标 staticlib）+ CRT + lld 随 hone 分发；`HONE_TOOLCHAIN_DIR` 覆盖；安装脚本改 zip/tar.gz 包
- [ ] **G3.5 静态表入 object**：const blob / span 表 / 函数注册表移入 script.o rodata（设计 §4.7，替换 wrapper 静态表）+ 启动 FNV 自检
- [ ] **G3.6 CI 自动化**：`ci.yml` 增 nat 四引擎回归（`regress4.py`）；release 流水线发布三目标脚本产物 + 交叉产物 CI 二次验证
- [ ] **G3.7 CPU 基线**：x86_64 默认 baseline+SSSE3（设计 §6.3）+ `--cpu` 选项

---

## 验收清单（设计 §十「完成定义」）

- [ ] `hone build --exe hello.hn` 在无 gcc 的干净机器上产出可运行 exe（M3 完成无 rustc 依赖）
- [ ] `hone build --exe app.hn --target x86_64-unknown-linux-musl` 在 Windows 上产出 linux exe，CI 二次验证（G3.2/G3.6）
- [ ] `regress4.py` 全用例四引擎逐字节对齐（含错误诊断）（T8 → G1.6 逐步扩容）
- [ ] 新后端 H999 集合与 VM 完全一致（无静默漏实现）（G2.7）
- [ ] 产物 ≤5MB（M3 后 ≤3MB）（G3.3）
- [ ] 文档同步：`hone.md` / `CHANGELOG.md` / `README.md` / 官网 changelog（T10 起每里程碑核销）

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
