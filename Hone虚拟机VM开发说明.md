# Hone 虚拟机（VM）开发说明文档

> 适用版本：Hone v0.7.0 及之后
> 相关源码：`src/vm.rs`（执行内核）、`src/interp.rs`（参照解释器 / 共享运行时类型）、`src/error.rs`（错误模型）、`src/main.rs`（命令行接线）
> 文档定位：面向**维护者与二次开发者**的内部实现说明，覆盖架构、指令集、寄存器模型、编译/执行流程、语义对齐策略、测试回归与扩展指南。

---

## 1. 概述

Hone 是一门轻量的、可嵌入的跨平台脚本语言。其执行内核提供**两条可互换的后端**：

| 后端 | 实现文件 | 模型 | 默认 |
| --- | --- | --- | --- |
| AST 树遍历解释器 | `src/interp.rs` | 直接对语法树递归求值 | ✅ 默认 |
| 寄存器式字节码虚拟机 | `src/vm.rs` | 编译为字节码后执行 | 需 `--vm` |

两条后端**共享同一套前端**（词法 / 语法 / 类型检查）与**同一套运行时值类型**（`interp::Value`、`ErrorObj`、`EnumVal`、`LambdaVal`、`FutureVal` 以及全部 `builtins`），因此：

* 不存在两套并行的类型系统或内置函数实现，减少维护成本；
* VM 的语义目标就是**逐字节复刻解释器的可观测行为**（stdout/stderr、错误码、错误文案、行列号、help 提示），从而做到「零回归」。

### 1.1 设计目标

1. **一次性全量覆盖**：把语言的全部特性（函数、闭包、推导式、match、struct/class/enum、try/catch、async/await、go 等）都编译为字节码，而不是只做子集。
2. **语义镜像**：任何在解释器下可运行的程序，在 VM 下应产出完全相同的输出。对无法复刻的（如依赖原生库的 `load`）给出清晰的运行时报错，**不静默失败**。
3. **可观测 / 可调试**：字节码可反汇编为人类可读的**文本 IR**。
4. **零静默降级**：默认仍走解释器；VM 为显式可选，避免未覆盖特性影响存量用户。

### 1.2 关键设计决策

| 决策点 | 选择 | 说明 |
| --- | --- | --- |
| 执行模型 | 寄存器式（类 Lua 5 / Dalvik） | 相较栈式，指令更少、寄存器复用更直接 |
| 指令载体 | Rust 枚举 `Instr` + 可反汇编文本 IR | 类型安全、便于调试 |
| 值类型 | 复用 `interp::Value` | 不引入第二套值系统 |
| 内置函数 | 复用 `builtins::call` | VM 只负责字节码，不重复实现内置库 |
| 名称解析 | **运行期**解析（镜像解释器 `env` 查找） | 编译期无法判断前向引用/递归/`load` 模块函数 |
| 并发 | `std::thread` + 克隆 VM | 与解释器一致：异步/`go` 各起独立线程 |

---

## 2. 总体架构

```
              ┌─────────────────────────────────────────────────────────┐
  源码 .hn →  │ lexer → parser → preproc → checker                      │  ← 前后端共享
              └─────────────────────────────────────────────────────────┘
                                    │  Program(AST)
                    ┌───────────────┴───────────────┐
                    ▼                               ▼
        interp::run（默认，树遍历）        vm::run（--vm，字节码 VM）
                    │                               │
                    │                    ┌──────────┴───────────┐
                    │                    │  Compiler            │
                    │                    │  AST → Vec<Chunk>    │
                    │                    │  （每函数/顶层一个）  │
                    │                    └──────────┬───────────┘
                    │                               ▼
                    │                    ┌──────────────────────┐
                    │                    │  Vm + Frame 栈        │
                    │                    │  exec() 取指/译码/执行 │
                    │                    └──────────────────────┘
                    ▼                               ▼
              ┌──────────────────────────────────────────────┐
              │ 共享：Value / ErrorObj / builtins / ZError 渲染 │
              └──────────────────────────────────────────────┘
```

> **`preproc` 阶段**（`src/preproc.rs`，挂在 `parser::Parser::parse` 出口，所有后端共享）：
> 1. **宏展开**：`macro` 定义在顶层按源码顺序注册，宏调用（写法同函数调用）替换为
>    形参→实参的 AST 替换结果；展开后 `Stmt::MacroDef` 从 AST 中移除。
>    语句宏展开为独立的 `Stmt::Block`（自带作用域）。宏体在**定义点**展开，
>    因此宏引用图必然无环，不需要展开深度上限。
> 2. **跳转校验**：为每个函数作用域建立「语句列表树」，校验标签唯一性、
>    `goto` 目标可见性（只允许跳到跳转点所在列表或其祖先列表 ⇒ 不可能跳进内层块）、
>    以及「向前跳不得跳过变量绑定」。
>
> 由于该阶段在 AST 层一次性完成，且两个后端只消费展开/校验后的 AST，
> `goto` 与 `macro` 天然不存在「解释器与 VM 语义不一致」的问题。
> 预处理失败统一报 `H105`（标签/跳转）或 `H106`（宏）。

`src/main.rs` 中的接线（节选）：

```rust
fn run_script(path: &str, src: &str, debug: bool, use_vm: bool) -> Result<(), ZError> {
    let program = parser::Parser::parse(path, src)?;
    checker::Checker::check(&program, path, src)?;
    if use_vm {
        vm::run(&program, path, src, debug)?;   // 字节码 VM
    } else {
        interp::run(&program, path, src, debug)?; // 树遍历解释器（默认）
    }
    Ok(())
}
```

`--vm` / `--disasm` 为全局开关，可出现在脚本名之前或之后。

### 2.1 模块职责

| 结构/函数 | 职责 |
| --- | --- |
| `Const` | 常量池条目（Int/Float/Bool/Str/Char/Null） |
| `Instr` | 指令集枚举（文本 IR 的语义载体） |
| `Chunk` | 一段可执行代码：指令流、常量池、标签、`nregs`、形参、捕获表、局部名表 |
| `Compiler` | 把 AST 编译为 `Vec<Chunk>` |
| `Vm` | 执行 `Chunk`，维护帧栈与 try 栈 |
| `Frame` | 单次函数/闭包调用的执行上下文（chunk、pc、寄存器文件） |
| `run` / `disassemble_program` / `disassemble` | 对外入口 |

---

## 3. 指令集与文本 IR

### 3.1 文本 IR 形态

反汇编输出以 `chunk` 为单位，先打印元信息与常量池，再逐条打印指令（行首为 pc）：

```
; chunk main  (nregs=19, params=[])
  .const 0 = 1
  .const 1 = 2
  .const 2 = 3
  .const 3 = 0

   0  LOADK   r6  1
   1  MOVE    r2 r6
   2  LOADK   r7  2
   ...
   6  NEWLIST r9 [2..+3]
   7  MOVE    r0 r9
   8  NEWLIST r1 [2..+0]
   9  ISDICT  r3 r0
  10  JMPT    r3 ->36
  11  Llist1:
  12  LOADK   r5  0
  13  Lcond3:
  14  LEN     r6 r0
  15  LT      r7 r5 r6
  16  JMPF    r7 ->34
  17  INDEX   r8 r0 r5
  18  MOVE    r4 r8
   ...
```

约定：

* `rN` 表示寄存器 N；`[b..+n]` 表示从寄存器 `b` 起、连续 `n` 个寄存器构成的区间（调用实参 / 列表字面量等）。
* `->T` 表示跳转目标为 pc = T。
* `Label` 是**伪指令**，不参与执行，只为反汇编可读；真实跳转在 `resolve()` 阶段回填为下标。

### 3.2 指令总览

下表中「操作数」省略 `r` 前缀。`d`=目标寄存器，`a/b`=源寄存器。

#### 常量与搬运

| 助记符 | 操作数 | 语义 |
| --- | --- | --- |
| `LOADK` | d, const | `regs[d] = consts[const]` |
| `LOADNULL` | d | `regs[d] = Null` |
| `LOADBOOL` | d, val | `regs[d] = Bool(val)` |
| `MOVE` | d, s | `regs[d] = regs[s]` |
| `NOP` | — | 空操作 |

#### 算术 / 一元 / 比较

| 助记符 | 操作数 | 语义 |
| --- | --- | --- |
| `ADD`/`SUB`/`MUL`/`DIV`/`MOD` | d, a, b | 数值运算；类型不匹配 / 除零 / 溢出均抛错（文案与解释器一致） |
| `NEG` | d, s | 一元负号（仅 int/float） |
| `NOT` | d, s | 逻辑非（仅 bool） |
| `EQ`/`NE`/`LT`/`LE`/`GT`/`GE` | d, a, b | 比较；仅 int/float/char，其它类型报 `cannot compare` |

#### 空值判断与容器

| 助记符 | 操作数 | 语义 |
| --- | --- | --- |
| `ISNULL` | d, s | `regs[d] = Bool(s == Null)`（可选链短路用） |
| `ISDICT` | d, s | `regs[d] = Bool(s 是 dict)`（for-in / 推导式运行时分拣用） |
| `ITERCHK` | s, is_comp | **迭代源校验**：`s` 非 list/dict 时按解释器报错（`for in` / `comprehension` 两种文案） |
| `INDEX` | d, o, k | `regs[d] = o[k]`（list 越界、str 越界、非容器分别报错） |
| `INDEXSET` | o, k, v | `o[k] = v`（list / dict 原地写入） |
| `DESTRUCTGET` | d, o, k | **解构专用**取值：list 越界 → `destructuring ...`；dict 缺键 → `dict has no key ...` |
| `FIELD` | d, o, name | 字段访问 `o.name`（dict/struct 缺字段、error 字段、非 error 类型分别报错） |
| `LEN` | d, s | 长度（list/dict/str） |
| `KEYS` | d, s | 取字典键列表 |
| `NEWLIST` | d, base, n | 用 `base..base+n` 构造列表 |
| `NEWDICT` | d, base, n | 用 `base..base+2n`（成对 k,v）构造字典 |

#### 枚举

| 助记符 | 操作数 | 语义 |
| --- | --- | --- |
| `ENUMELEM` | d, s, i | 取枚举载荷第 i 项 |
| `ISENUM` | d, s, enum, variant | 判断 `s` 是否为 `enum.variant` |
| `NEWENUM` | d, enum, variant, base, n | 构造带载荷的枚举值 |

#### 调用 / 闭包 / 并发

| 助记符 | 操作数 | 语义 |
| --- | --- | --- |
| `CALL` | res, func, base, n | 按名调用：`func(regs[base..base+n])` |
| `MAKELAMBDA` | d, chunk_idx, reads | 构造闭包，`reads` 为 `(变量名, 外层寄存器)` 捕获表 |
| `AWAIT` | d, future_reg | 等待 future 完成，结果写入 d |
| `GOCALL` | callee, base, n | 后台线程 fire-and-forget 调用（`go`） |

#### 控制流

| 助记符 | 操作数 | 语义 |
| --- | --- | --- |
| `RET` | r | 返回 `regs[r]` |
| `RETNULL` | — | 返回 `Null`（函数体末尾隐式返回） |
| `JMP` | target | 无条件跳转 |
| `JMPF` | cond, target | `cond` 为假则跳转 |
| `JMPT` | cond, target | `cond` 为真则跳转 |
| `Label` | name | 伪指令（不执行） |

#### 异常与调试

| 助记符 | 操作数 | 语义 |
| --- | --- | --- |
| `TRYBEGIN` | handler, catch_reg | 压入 try 帧（记录建立帧、handler、catch 寄存器） |
| `TRYPOP` | — | 正常路径弹出 try 帧 |
| `THROW` | r | 抛出 `regs[r]`（str→H600，error 原样） |
| `THROWSTR` | s | 直接抛字面字符串（→H600） |
| `DEBUGPRINT` | r | debug 模式打印 |
| `BREAKPOINT` | — | debug 模式断点 |

### 3.3 常量的文本表示（必须可逆）

文本 IR 的常量必须**无歧义且可单行承载**，否则往返（反汇编→装配）会失真：

| 类型 | 文本形式 | 说明 |
| --- | --- | --- |
| `Int` | `42` / `-7` | 无小数点、无 `e` |
| `Float` | `1.0` / `1e100` / `inf` / `NaN` | 用 `{:?}`，保证 `Float(1.0)` 与 `Int(1)` 可区分 |
| `Bool` | `true` / `false` | — |
| `Str` | `"..."` | 转义 `\\` `\"` `\n` `\t` `\r`，**控制字符不会拆断行** |
| `Char` | `'a'` / `'\n'` / `'\''` / `'\\'` | 同上转义约定 |
| `Null` | `null` | — |

> 踩坑记录：早期 `const_text` 对 `Char` 直接 `format!("'{}'", v)`，导致换行符把 IR 行拆成两行、反斜杠/单引号无法解析，`char_demo` / `stdlib_test` 的 IR 往返直接失败。修复方式即上表的成对转义（`escape_str_body` / `escape_char_body` ↔ `unescape_str` / `parse_char`）。

### 3.4 反向装配（`assemble()` / `runir`）

`disassemble_program()`（即 `hone run --disasm` 的入口）输出的文本 IR **自包含**：文件名 + 模块头（`; structs / enums / aliases / async / funcs`）+ 每个 chunk 的 `.const` / `.spans` 与指令流 + 原始源码块。`assemble()` 可 1:1 还原为 `Module`，`runir` 直接执行——**跳过 parser 与 checker**，因此运行期错误的位置与上下文仍与源码执行一致（`.spans` 与源码块随 IR 一起序列化）。

> 另有 `disassemble(chunks)` / `disassemble_module(m)` 两个辅助接口，只导出模块头与 chunk 部分（不含真实源码块），供分块调试使用；因其产物缺少源码块、无法 1:1 还原报错上下文，**主 CLI 路径不使用它们**。

```bash
hone run --disasm f.hn > f.ir    # 反汇编
hone runir f.ir                  # 装配并执行
```

由于跳转目标是绝对 pc，装配只需重建 `labels` 表（`finalize_chunk`）供调试查看，无需二次回填。

---

## 4. 寄存器模型

### 4.1 寄存器文件

每个 `Chunk` 拥有一份**独立的寄存器文件** `Vec<Value>`，长度由编译期统计的 `nregs` 决定：

```rust
pub struct Chunk {
    pub name: String,
    pub code: Vec<Instr>,
    pub spans: Vec<Span>,          // 与 code 等长：每条指令的源码位置
    pub consts: Vec<Const>,
    pub labels: HashMap<String, usize>,
    pub nregs: usize,
    pub params: Vec<String>,
    pub captured_regs: Vec<(String, usize)>, // lambda：捕获变量名 → 寄存器号
    pub locals: HashMap<String, usize>,      // 局部名 → 寄存器号（运行期调用解析用）
}
```

帧在创建时全部初始化为 `Value::Null`：

```rust
self.frames.push(Frame { chunk: ci, pc: 0, regs: vec![Value::Null; nregs] });
```

> **注意**：`regs[i]` 存放的是 `Value` 的**拥有副本**。`Value::List`/`Value::Dict` 在 `interp.rs` 中是 `Vec`，`MOVE` 即深拷贝语义。`INDEXSET` 会取出现有容器、原地修改后再写回，以模拟「引用式」容器语义。

### 4.2 变量区与临时区

编译器在**单个 chunk 内**用两个游标管理寄存器分配：

| 游标 | 含义 |
| --- | --- |
| `var_next` | 「变量区」顶端 + 1（已声明局部变量占据 `[0, var_next)`） |
| `tmp_next` | 「当前分配水位」（变量 + 活跃临时量） |
| `max_reg` | 历史最大值，用于计算 `nregs = max_reg + 1` |

约定**变量区在低地址、临时区在高地址**，即恒有 `tmp_next >= var_next`。

关键方法：

```rust
fn tmp(&mut self) -> usize {            // 分配一个临时寄存器
    let r = self.tmp_next; self.tmp_next += 1; r
}
fn decl(&mut self, name: &str) -> usize { // 声明局部变量（推进变量区）
    let r = self.var_next; self.var_next += 1;
    self.scopes.last_mut().unwrap().0.insert(name.into(), r);
    self.cur_locals.insert(name.into(), r);
    self.tmp_next = self.var_next;       // 重置临时水位
    r
}
fn decl_tmp(&mut self, name: &str) -> usize { // 仅表达式作用域内的临时命名（推导式循环变量）
    let r = self.tmp_next; self.tmp_next += 1;
    self.scopes.last_mut().unwrap().0.insert(name.into(), r);
    r
}
fn reset_tmp(&mut self) { self.tmp_next = self.var_next; } // 语句边界回收临时寄存器
fn enter(&mut self) { /* 压入新作用域，记录 var_next */ }
fn exit(&mut self)  { /* 弹出作用域，还原 var_next / tmp_next */ }
```

生命周期：

* **语句边界**：`compile_program` / `compile_fn` 的每条语句后调用 `reset_tmp()`，回收临时寄存器。
* **作用域**：块（if/while/for/函数体/闭包体）进入 `enter()`、退出 `exit()`。
* **`decl_tmp`**：用于「必须跨内部跳转存活、但又不应污染变量区」的名字（典型：推导式的循环变量、计数器）。

### 4.3 调用约定

| 场景 | 寄存器布局 |
| --- | --- |
| 普通函数 | `regs[0..params.len()]` = 形参 |
| lambda 闭包 | `regs[0..captured_regs.len()]` = 捕获值；其后 `params.len()` 个 = 形参 |
| 调用实参 | 调用方把实参写入一段**连续**寄存器 `base..base+n`，`CALL res func base n` |

调用方编译实参的形态（`Expr::Call`）：

```rust
let base = self.tmp();
for _ in 0..n { self.tmp(); }          // 预留连续实参槽
for (i, a) in args.iter().enumerate() {
    let ra = self.compile_expr(a);
    self.emit(Instr::Move(base + i, ra));
}
let r = self.tmp();
self.emit(Instr::Call(r, callee.clone(), base, n));
```

### 4.4 寄存器分配的注意事项（踩坑记录）

> ⚠️ **这是本项目最容易引入隐蔽 bug 的地方，务必阅读。**

1. **`decl()` 会把 `tmp_next` 拉回 `var_next`**。因此若在表达式求值中途 `decl()` 一个变量，会把「水位以上」的活跃临时寄存器**就地回收**，随后 `tmp()` 可能重新分配出与它们冲突的编号。
2. **推导式曾是重灾区**：早期实现用 `enter()`/`exit()` 包裹推导式循环，而 `exit()` 会把水位重置回变量区，导致：
   * 结果列表寄存器与循环变量寄存器冲突 → 列表推导产出空列表、`append` 收到 `int`；
   * 字典路径遗漏计数器初始化与 `var2`（值）绑定。
   现行实现改为「**自包含临时作用域**」：保存 `(var_next, tmp_next)`，结果/迭代源/循环变量/循环内临时量**全部从 `tmp_next` 分配**，结束时仅保留结果寄存器并把水位设为 `saved_tmp + 1`，从而既可安全嵌套在外围调用实参中，又不泄漏变量区。见 `Compiler::compile_comp`。
3. **迭代源与结果必须跨循环体存活**：`compile_comp` 中先分配结果寄存器（处于隔离区最底层），再编译迭代源表达式；此后不再调用任何会重置水位的函数。
4. **需要真正长期存活的临时值**（如解构 `t1, t2, t3 = triple()` 的右侧结果）应显式放进变量区，避免循环中 `decl()` 重置水位时被回收。

---

## 5. 编译器（Compiler）

### 5.1 顶层编译：两趟

`compile_program` 分两趟：

1. **第一趟**：登记所有函数 / 类方法 / struct / enum / alias（插入 `func_map` / `struct_defs` / `enum_defs` / `aliases`），使**前向引用**与**递归**可行。
2. **第二趟**：逐个编译顶层语句（跳过纯声明），每条语句后 `reset_tmp()`；末尾补 `RETNULL`，`finish_chunk("main", [])`。

```rust
fn compile_program(&mut self, prog: &Program) -> Result<(), ZError> {
    // 第一趟：登记声明
    for s in &prog.stmts { /* FnDef / AsyncFnDef / ClassDef / StructDef / EnumDef / Alias */ }
    // alias → 目标函数的 func_map 指向
    // 第二趟：编译顶层可执行语句
    self.enter();
    for s in &prog.stmts { /* 跳过声明，其余 compile_stmt */ self.reset_tmp(); }
    self.emit(Instr::RetNull);
    self.exit();
    self.finish_chunk("main", vec![]);
    ...
}
```

### 5.2 `finish_chunk` 与嵌套编译

`finish_chunk` 会**取走**当前缓冲区（`std::mem::take`）并清空编译器状态：

```rust
fn finish_chunk(&mut self, name: &str, params: Vec<String>) -> usize {
    self.resolve();                       // 回填所有 pending 跳转
    let nregs = self.max_reg + 1;
    let chunk = Chunk { code: take(code), spans: take(spans), consts: take(consts),
                        labels: take(labels), nregs, params,
                        captured_regs: vec![], locals: take(cur_locals) };
    let idx = self.chunks.len();
    self.chunks.push(chunk);
    self.func_map.insert(name.into(), idx);
    self.scopes.clear(); self.var_next = 0; self.tmp_next = 0;
    self.max_reg = 0; self.loop_labels.clear();
    idx
}
```

因此**编译嵌套 lambda**时，必须对编译器缓冲做「快照 → 编译子 chunk → 还原」（`Expr::Lambda` 分支）：保存 `code/spans/consts/labels/pending/scopes/cur_locals/var_next/tmp_next/max_reg/loop_labels`，编译完 lambda 体后逐一还原。

### 5.3 标签与回填

生成跳转时先压入**占位**并把 `(指令下标, 标签名)` 记入 `pending`，全部生成后在 `resolve()` 内一次性回填（`Jmp`/`JmpIfFalse`/`JmpIfTrue`/`TryBegin`）。

### 5.4 源码位置（span）的记录

* `emit()` 会把 `cur_span` 追加到 `spans`，与 `code` 一一对应；运行期取指时同步取出 `span` 用于报错定位。
* `compile_stmt` 进入时 `self.cur_span = s.span()`。
* `compile_expr` 在**进入时保存、返回时恢复** `cur_span`，使得「二元表达式的指令」记录的是该表达式的起始 span（与解释器 `eval_expr` 的报错列号一致）。
* 这一机制是**错误行列对齐**的关键：早期 VM 报错列号比解释器偏 2（因为 `cur_span` 停在右操作数），修正后逐字节一致。

### 5.5 主要语句编译策略（速览）

| 语句 | 要点 |
| --- | --- |
| `Assign` / `CompoundAssign` / 自增自减 | 求值右侧，`Move` 到目标寄存器（已声明则复用其寄存器） |
| `DestructAssign` | 右侧结果**落到变量区安全寄存器**，逐目标用 `DESTRUCTGET` 取值（list 越界/ dict 缺键与解释器同码同文案） |
| `If` / `While` / `DoWhile` / `ForC` | 条件求值 + `JMPF`/`JMP`；`break`/`continue` 由 `loop_labels` 栈提供标签 |
| `ForIn` | 先 `ISDICT` **运行时分拣**：list 走「按下标迭代」，dict 走「按 keys 迭代」，双变量取键/值 |
| `Return` | 多值打包为 `Value::List`；单值直接 `RET` |
| `TryCatch` / `Throw` | `TRYBEGIN`/`THROW`/`THROWSTR`/`TRYPOP` + handler 标签 |
| `Go` | `GOCALL`（后台线程 fire-and-forget） |
| `Label`（用户标签） | `emit_label("usr_<名字>")`，即一条 `Label` 伪指令（运行期无动作，仅作跳转落点） |
| `Goto` | `jmp("usr_<名字>")`：经 `pending` 修复表解析为绝对 pc，**前向/后向跳转都支持**，与解释器 `Flow::Goto` 语义一致 |
| `FnDef` / `AsyncFnDef` | 编译为独立 chunk；async 名称登记进 `async_fns` |
| `ClassDef` / `StructDef` / `EnumDef` / `Alias` / `Use` | 登记声明，不产生顶层指令 |
| `MacroDef` | 预处理阶段已展开并移除（编译器侧仅为穷尽匹配的空分支） |

> **用户标签与内部标签的隔离**：编译器内部标签由 `new_label("Lstart"/"Lend"/"Lelse"/"Lskip"...)` 生成，
> 用户标签统一加 `usr_` 前缀，二者不可能重名。
> **为什么不需要额外的作用域回收**：VM 的寄存器编号是编译期静态分配的，跳转只改 pc，
> 不改变寄存器归属；配合「向前跳不得跳过变量绑定」的编译期校验，跳转后不会读到未初始化寄存器
> ——这也正是解释器（`Flow::Goto` 逐层向外查找标签）与 VM 能逐字节一致的原因。

### 5.6 主要表达式编译策略（速览）

| 表达式 | 要点 |
| --- | --- |
| 字面量 / 变量 / f-string | `LOADK` / 直接返回变量寄存器 / `compile_fstr` |
| 二元（算术/比较/逻辑/空值合并） | 短路运算用 `JMPF`/`JMPT`；生成 `ADD`/`LT`/... 并记录表达式起始 span |
| 一元（`-` / `!`） | `NEG` / `NOT`（类型不符报错，与解释器一致） |
| 三元 / 可选链 `?.` | `?.` 生成 `ISNULL` + 短路分支：obj 为 null 时结果为 null，否则走 `FIELD` |
| `Call` | 预留连续实参槽 → 逐个 `Move` → `CALL`（按名，运行期解析） |
| `List` / `Dict` 字面量 | `NEWLIST` / `NEWDICT` |
| `Index` / `Field` | `INDEX` / `FIELD`（含枚举变体特殊处理） |
| 推导式（ListComp / DictComp） | `compile_comp`：自包含临时作用域（见 §4.4） |
| `Match` | 逐 arm 生成「模式匹配 + 绑定 + 跳转」，支持字面量/枚举变体/绑定/通配 |
| `Lambda` | 快照编译器缓冲 → 编译独立 chunk（捕获表写入 `captured_regs`）→ 还原 → `MAKELAMBDA` |
| `Await` | 求值 future 寄存器 → `AWAIT` |

---

## 6. 运行时（VM Execution）

### 6.1 帧与 Try 帧

```rust
struct Frame { chunk: usize, pc: usize, regs: Vec<Value> }

struct TryFrame {
    handler: usize,   // handler 所在 pc
    catch_reg: usize, // 捕获到的 error 写入的寄存器
    frame: usize,     // 建立该 try 的帧下标（跨帧传播的关键）
}

pub struct Vm {
    chunks, func_map, struct_defs, enum_defs, aliases, async_fns,
    file, src, debug,
    frames: Vec<Frame>,    // 帧栈
    try_stack: Vec<TryFrame>,
}
```

### 6.2 主循环 `exec()`

`exec()` 是一个「取指 → 取 span → 译码 → 执行」的循环，返回 `Result<Value, Value>`：

* `Ok(v)`：本帧因 `RET`/`RETNULL`（或代码越界）返回；
* `Err(v)`：抛出异常值 `v`（`Value::Error` 或原始值）。

关键行为：

* 每条指令从**当前帧**的 chunk 取其 `spans[pc]`，用于运行期报错定位；
* 跳转类指令直接改写 `frames[fi].pc` 并 `continue`；
* 其余指令通过 `exec_instr` 执行，正常后 `pc += 1`；
* `exec_instr` 返回 `Err(thrown)` 时进入异常处理：**只有由当前顶层帧建立的 try 才能在本帧捕获**（见 §6.4）。

### 6.3 调用分派 `do_call`

`do_call(func, args, span)` 严格镜像解释器的运行期查找顺序：

```
1) resolve_alias(func)                       // alias 名归一化
2) 查当前帧的 Chunk.locals：若该名字已绑定
     - Value::Lambda → call_lambda          // lambda 变量调用
     - Value::Str    → 递归 do_call（字符串函数名，支持前向引用/递归/load）
     - 其它           → 继续
3) async_fns 命中 → spawn_async             // 异步函数 → 后台线程 + future
4) "Enum.Variant" 命中 enum_defs → 构造 Value::Enum
5) 形如 "a.b"：
     - func_map 命中 → call_chunk           // 类方法等
     - 内置函数     → call_builtin
     - 否则报「未定义函数」
6) func_map 命中 → call_chunk
7) struct_defs 命中 → 构造 struct 实例（dict，含 \0__struct__ 标记）
8) 内置函数 → call_builtin
9) 否则报「未定义函数」
```

> **为什么必须运行期解析？** `fib` 递归自调用（函数体编译先于 `func_map` 注册）、`load`/`import` 的模块函数、lambda 变量调用都**无法在编译期判定**。早期曾尝试编译期路由（区分 `Call` 与 `CallVal`），造成 26 处回归；现行方案是统一 `CALL` + 运行期查 `Chunk.locals`。

### 6.4 跨帧异常传播（关键机制）

问题场景：`main` 里 `try { f(10, 0); } catch e { ... }`，而 `f` 内部抛错。

* `call_chunk` 推入 `f` 的帧并调用 `exec()`；若 `f` 抛错，`f` 自己的 `exec()` 循环最先看到 `Err(thrown)`。
* 此时栈顶 `TryFrame` 是 **`main` 建立**的（`tf.frame == 0`），而当前顶层帧是 `f`（下标 1）。
* 若在 `f` 的帧内直接按 `handler` 跳转，会**跳到 `main` 的 chunk 偏移**，产生垃圾结果。

修复：`TryFrame` 记录建立帧；`try_catch` 仅当 `tf.frame == 当前顶层帧` 时才在本帧捕获，否则返回 `false` 让错误**向上传播**（`call_chunk` 弹出 `f` 帧后，由 `main` 的 `exec()` 捕获并跳到正确 handler）：

```rust
fn try_catch(&mut self, thrown: &Value) -> bool {
    let top = self.frames.len() - 1;
    if let Some(tf) = self.try_stack.last() {
        if tf.frame == top {
            let tf = self.try_stack.pop().unwrap();
            self.frames[top].regs[tf.catch_reg] = thrown.clone();
            self.frames[top].pc = tf.handler;
            return true;
        }
    }
    false
}
```

### 6.5 错误模型与对齐

VM 内的错误一律构造成 `Value::Error(ErrorObj { ... })`，最终由 `value_to_zerror` 转成 `ZError` 渲染：

```
mk_err(code, msg, span)
   → ErrorObj { code, message, file, line, col, len, context(=该行源码), help }
      → value_to_zerror(&Value::Error) → ZError::new(code, msg, file, src, line, col, len, help)
         → error.rs 的 Display 渲染：error[Hxxx] / --> file:line:col / 源码行 / ^^^^ / help:
```

对齐要点（与解释器逐字节一致）：

| 维度 | 做法 |
| --- | --- |
| 错误码 | 复用 `crate::error::codes` 常量（H001 类型不匹配、H002 未定义、H009 除零…） |
| 文案 | VM 的运算/索引/字段/内置错误文案与解释器**逐字相同**（统一英文，如 `cannot apply \`+\` to \`str\` and \`list\``） |
| 行列 / len | 由指令 span 提供；`compile_expr` 保存/恢复 `cur_span` 使列号指向表达式起点 |
| 源码行上下文 | `mk_err` 把 `span.line` 对应源码行填入 `ErrorObj.context`（供 `catch e` 的 `e.context`） |
| help 提示 | `vm_help_for(code, msg)` 依据错误码 + 文案片段**集中推导**（避免 48 处 `mk_err` 逐点传 help）：除零、溢出、类型不匹配、越界、解构、str→int、未知字段、error 字段等 |

### 6.6 帧回收

`call_chunk` / `call_lambda` 均在推入帧、执行 `exec()` 后 `pop()`，保证异常路径下帧栈也能正确回退。

---

## 7. 闭包与并发

### 7.1 Lambda 闭包

* 编译 `Expr::Lambda` 时收集「当前所有可见作用域」的 `(名字, 寄存器)` 作为**捕获表**（按值捕获快照语义）。
* 生成的 `MAKELAMBDA r, chunk_idx, reads` 在运行期构造 `Value::Lambda(Arc<LambdaVal>)`，其 `captured` 为「名字 → 当前寄存器值」的快照，`vm_chunk` 记录目标 chunk 下标（解释器构造的 lambda 该字段为 `None`）。
* `call_lambda`：把捕获值填回 `regs[0..ncap]`，形参填 `regs[ncap..ncap+params.len()]`，再执行子 chunk。

### 7.2 async / await

* `async fn` 的名称登记进 `async_fns`；调用时 `do_call` 走 `spawn_async`。
* `spawn_async`：**克隆** `chunks/func_map/struct_defs/enum_defs/aliases/async_fns`，`thread::spawn` 起一个新 `Vm`，执行目标 chunk，把结果经 `FutureVal::complete` 写入；立即返回 `Value::Future`。
* `await` 编译为 `AWAIT`；运行期对 `Value::Future` 调用 `f.wait()`（错误转 `ErrorObj::from_err`）。

### 7.3 go 多线程

* `go` 编译为 `GOCALL`；运行期 `spawn_go`：与 `spawn_async` 相同的克隆 + 独立线程，但 **fire-and-forget**，错误仅 `eprintln!`，不影响主线程。

> 线程内的错误转换使用 `Vm::value_to_zerror`（线程持有克隆的 `src`），因此 `await` 抛出的错误同样保留源码上下文行；`go` 的错误渲染后 `eprintln!`，不影响主线程退出码。

---

## 8. 与解释器的语义对齐策略

「零回归」的含义是：对同一脚本，`hone run foo.hn` 与 `hone run --vm foo.hn` 的 **stdout + stderr 完全一致**。为此需要在以下方面逐一镜像解释器：

1. **值语义**：直接复用 `interp::Value`，避免类型/格式化差异。
2. **内置库**：直接调用 `builtins::call`，错误经 `ErrorObj::from_err` 保留 `code/msg/line/col/len/help`。
3. **求值顺序**：短路求值（`&&`/`||`/`??`/三元）用跳转实现；可选链 `?.` 仅短路其后一个字段（与 JS 一致）。
4. **错误边界**：比较仅支持 int/float/char；`!` 仅支持 bool；索引/字段/解构的越界与缺键行为与解释器一致（含错误码与 help）。
5. **错误定位**：span 记录 + `cur_span` 保存/恢复，使行列与解释器一致。
6. **调用解析**：运行期按名解析，支持递归 / 前向引用 / lambda 变量 / 字符串函数名。
7. **并发语义**：async/go 与解释器一致地起独立线程；`Future` 语义一致。
8. **错误渲染**：见 §8.1，错误对象的六个维度必须逐一还原。

### 8.1 错误的六维对齐

最终落地的错误文本由 `ZError` 渲染，要逐字节一致必须同时对齐六个维度：

| 维度 | 来源 | 对齐方式 |
| --- | --- | --- |
| 错误码 | `ErrorObj.code` | 与解释器同 code（如 `H009` 除零、`H010` 溢出、`H404` 库未加载） |
| 消息正文 | `ErrorObj.message` | 英文、与解释器同一措辞（如 `cannot apply \`+\` to \`int\` and \`float\``） |
| 位置 `line/col` | 指令 span | 指令 emit 时记录 `cur_span`；`compile_expr` 进入时保存、返回时恢复，使二元运算的 span 指向**表达式起点**（与解释器一致） |
| caret 长度 | `ErrorObj.len` | 取 `span.len.max(1)`（不再恒为 1） |
| 源码上下文行 | `ErrorObj.context` | `mk_err` 用 `self.src` 的对应行填充，`catch e` 里 `e.context` 因此有值 |
| `help:` 文案 | `ErrorObj.help` | 默认由 `vm_help_for(code, msg)` 按 code + 消息特征推导；同消息不同语义（如 `cannot compare` 在相等/大小比较下）用 `mk_err_with(..., Some(help))` 显式覆盖 |

要点：

- `vm_help_for` 是**集中式**映射，避免在 40+ 个 `mk_err` 调用点逐一手传 help；仅当同一 code 下消息文本无法区分语义时才用 `mk_err_with`。
- 新增错误场景请到 `regress_err.py` 补用例（§11.2），保证「报错精准」可持续验证。
- 与解释器的差异主要集中在**编译期/检查期诊断**：`hone run` 会先跑 parser + checker，而 VM/IR 模式只执行字节码。故 `--disasm` 不跑 checker；这也意味着 IR 模式无法复现检查期诊断（§11.3 的 `SKIP-CHECK`）。

### 8.2 已知的「非逻辑」差异（可接受）

回归中剩余的失败均为**不可复现的随机/时序差异**或**按设计不支持**，不属于语义回归：

| 示例 | 原因 |
| --- | --- |
| `alias_demo` / `time_random` / `uuid_demo` | 依赖随机数 / 当前时间，两次运行本就不会相同 |
| `server_demo` | 端口动态分配 |
| `threads` | 多线程完成顺序不确定 |
| `test_hone_lib` / `test_img_lib` / `test_music_lib` / `test_sched_lib` / `test_process_lib` / `load_*` / `import_demo` | 依赖 `load`/`import` **原生库**，VM 未覆盖（见 §10） |

---

## 9. 使用方式

```bash
# 默认：AST 解释器
hone run examples/fib.hn

# 字节码 VM
hone run --vm examples/fib.hn

# 仅反汇编（不执行）：打印文本 IR
hone run --disasm examples/fib.hn

# 从文本 IR 反向装配并执行（等价于 --vm，但跳过编译阶段）
hone run --disasm examples/fib.hn > fib.ir && hone runir fib.ir

# 开关可在脚本名前后：等价
hone --vm run examples/fib.hn
```

帮助文本（节选）：

```
--vm       使用寄存器式字节码虚拟机执行（默认走 AST 解释器）
--disasm   仅编译为字节码文本 IR 并打印（不执行）
hone runir <file.ir>   装配文本 IR 并执行
```

---

## 10. 覆盖范围

### 10.1 已覆盖

字面量、变量、算术/比较/逻辑（短路）/空值合并/三元、赋值/复合赋值/自增自减、`if`/`while`/`do-while`/`for-c`/`for-in`（列表与字典，**运行时分拣**）/`break`/`continue`、**标签与 `goto`（含跳出嵌套循环、回跳成环）**、**宏 `macro`（表达式宏 / 语句宏，预处理阶段展开）**、函数（定义/调用/递归/**多返回值**/**解构**）、列表字典（字面量/索引/字段/字典键）、f-string、**推导式（列表与字典，含双变量、过滤、空结果）**、`try`/`catch`/`throw`（含 Error 字段访问 `message/code/file/line/col/context`）、`match`（字面量 + 枚举变体 + 绑定）、`struct`/`class`/`enum` 定义与使用、`builtins`、`debug_print`、`breakpoint`（debug 模式）、`alias`/`use`、**lambda 闭包（按值捕获）**、**async/await（后台线程 + Future）**、**go 多线程**、可选链 `?.`、**运算符重载 `__op`（15 个钩子）**、**文本 IR 反汇编 `--disasm` / 反向装配 `runir`**。

### 10.2 暂未覆盖（后续迭代）

| 特性 | 现状 |
| --- | --- |
| `import` / `load`（远程/原生模块） | 未覆盖；运行期给出与解释器一致的「库未加载」报错 |
| 依赖原生 GUI/HTTP/FFI 的示例（`guipro_*`、`server_selftest`、`spider_demo`、`ffi_demo`、`ai_demo`） | 需要原生库绑定，未覆盖 |
| `goto` 的 AOT 原生编译 | AOT 后端不支持（报 H999 并提示用解释器或 `--vm`）；**解释器与字节码 VM 均完整支持** |

> 已从「暂未覆盖」转正的项：**运算符重载 `__op`**（§7.x，15 个钩子全覆盖）、**文本 IR 反向装配 `assemble()`**（§3.4）、**`goto` 与 `macro`**（§5.5，预处理阶段统一实现）。

---

## 11. 测试与回归

共三套脚本，均以「逐字节比较 stdout + stderr」为准，任何一处文本差异都算失败。

### 11.1 语义回归 `regress3.py`

遍历 `examples/*.hn`，同一脚本分别以**解释器**与 `--vm` 运行并逐字节比对，带超时保护。

```bash
python3 regress3.py
# 末尾输出：PASS=n FAIL=m ；FAILED: <列表>
```

`SKIP` 集合（按设计无法 1:1 复现）：`load/import` 原生库、原生 GUI/HTTP/FFI 示例、需交互/定时器的 `pet_demo`、访问外网的 `https_demo` 等。

**容错重试**：首次不一致时会重跑至多 2 次，任一次两侧一致即判 PASS 并记入 `FLAKY` 行。
用于 `threads` / `async_demo` 这类**线程完成顺序**导致的偶发不一致（非逻辑差异）；
真实逻辑差异会稳定失败，不会被重试掩盖。

**当前基线**：**PASS=47 / FAIL=9 / SKIP=16（共 72 个示例）**，9 个 FAIL 全部属于 §8.1 的「非逻辑差异」（随机数 / 时间 / 随机端口 / `load` 原生库 / 随机 UUID），即**真实逻辑差异为 0**。
稳定 FAIL 集合：`alias_demo server_demo test_hone_lib test_img_lib test_music_lib test_process_lib test_sched_lib time_random uuid_demo`。

### 11.2 错误精准回归 `regress_err.py`

内嵌 37 个「必定报错」的片段（未定义变量/函数、除零、溢出、类型不匹配、越界、缺字段、非可迭代、解构失败、`await` 非 future、库未加载……），逐个比对解释器与 VM 的错误输出。

```bash
python3 regress_err.py
# 末尾输出：PASS=n FAIL=m (共 37 个错误用例)
```

**当前基线**：**PASS=37 / FAIL=0** —— 错误码、消息正文、`--> 文件:行:列`、caret 长度、源码上下文行、`help:` 文案全部一致。

### 11.3 文本 IR 往返回归 `regress_ir.py`

对每个示例：`run --disasm` 生成文本 IR → `runir` 反向装配执行 → 与源执行逐字节比对。

```bash
python3 regress_ir.py
# 三类结果：OK（等价）/ SKIP-COMPILE|SKIP-CHECK（结构性，非缺陷）/ DIFF（真实差异）
```

**当前基线**：**OK=39 / BAD=9 / SKIP(结构)=9**。BAD 仅剩随机数 / 端口 / 线程顺序 / 原生库 `load` 一类非逻辑差异；`SKIP-CHECK` 指脚本预期输出为**检查期诊断**（IR 模式有意跳过类型检查阶段），属结构性。

### 11.4 手工对照

```bash
# 单文件对照
diff <(hone run examples/try_catch.hn 2>&1) <(hone run --vm examples/try_catch.hn 2>&1)

# 查看反汇编
hone run --disasm examples/fib.hn

# 文本 IR 往返（反汇编 → 装配 → 执行）
hone run --disasm examples/fib.hn > fib.ir && hone runir fib.ir
```

---

## 12. 开发指南

### 12.1 新增一条指令

1. 在 `enum Instr` 中增加变体（注明操作数语义）。
2. 在 `exec()` / `exec_instr` 中实现执行语义。
3. 在 `instr_text()` 中增加反汇编格式（否则 `--disasm` 会漏打印）。
4. **在 `parse_instr()` 中增加装配分支**（否则 `assemble()` / `runir` 无法读回该指令，往返会失败）。
5. 在编译器中生成该指令（注意寄存器分配与 span 记录）。
6. 若涉及错误，使用 `mk_err(code, msg, span)` 并视需要在 `vm_help_for` 补 help；若同一消息在不同语义下 help 不同，改用 `mk_err_with(..., Some(help))`。
7. 新增错误场景后，到 `regress_err.py` 补一条用例。

### 12.2 新增一个语法特性

1. 确认 AST（`ast.rs`）与类型检查（`checker.rs`）已支持。
2. 在 `compile_stmt` / `compile_expr` 增加编译分支；注意：
   * 作用域用 `enter()`/`exit()`；
   * 需要长期存活的临时值放到变量区（`decl`）；
   * 表达式子作用域（如推导式）使用 `decl_tmp` + 保存/还原水位；
   * 跳转标签经 `pending` 回填。
3. 若与解释器有可观测行为差异，**先跑回归对照**，对齐文案 / 错误码 / 行列。

### 12.3 调试技巧

* `hone run --disasm foo.hn`：查看每个 chunk 的字节码与常量池，确认寄存器分配与跳转。
* 对照解释器：`diff <(hone run foo.hn) <(hone run --vm foo.hn)`。
* 最小复现：把出问题的语法抽成几行的小脚本，逐步缩小范围。
* 关注点：寄存器冲突（读 `--disasm` 的 `rN`）、span 定位（错误行列）、跨帧异常与 try 栈。

---

## 13. 已知限制与路线图

* **限制**
  * `load`/`import` 未覆盖（有与解释器一致的「库未加载」报错）。
  * 依赖原生 GUI/HTTP/FFI 的示例未覆盖（需原生库绑定）。
  * 错误文本只映射解释器中会出现的常见 help；新增错误场景需到 `regress_err.py` 补用例。
  * `go` 的错误仅打印，不影响主程序退出码（与解释器一致）。
* **路线图**
  1. `load`/`import` 模块化支持（与解释器的模块加载机制合流）。
  2. 常量池 / 寄存器复用等优化；可选的分支预测式跳转缓存。
  3. 为 `--disasm` 增加可选的「编译期诊断」输出，使 IR 模式也能保留检查期提示。

> 已完成（原路线图项）：运算符重载 `__op`（§7.x，15 个钩子全覆盖）、文本 IR 反向装配 `assemble()` / `runir`（§3.4）、错误六维对齐（§8.1）。

---

## 附录 A：核心结构速查

```rust
pub enum Const { Int(i64), Float(f64), Bool(bool), Str(String), Char(char), Null }

pub struct Chunk {
    pub name: String,
    pub code: Vec<Instr>,
    pub spans: Vec<Span>,
    pub consts: Vec<Const>,
    pub labels: HashMap<String, usize>,
    pub nregs: usize,
    pub params: Vec<String>,
    pub captured_regs: Vec<(String, usize)>,
    pub locals: HashMap<String, usize>,
}

struct Frame { chunk: usize, pc: usize, regs: Vec<Value> }

struct TryFrame { handler: usize, catch_reg: usize, frame: usize }

// 对外入口
pub fn run(program: &Program, file: &str, src: &str, debug: bool) -> Result<(), ZError>;
pub fn disassemble_program(program: &Program, file: &str, src: &str) -> Result<String, ZError>;
pub fn disassemble(chunks: &[Chunk]) -> String;          // 辅助：仅导出 chunk 部分
pub fn disassemble_module(m: &Module) -> String;         // 辅助：模块头 + chunk（无源码块）
```

## 附录 B：常见错误码（节选 `src/error.rs::codes`）

| 码 | 典型含义 |
| --- | --- |
| H001 | 类型不匹配（`cannot apply` / `cannot compare` / 索引 / 字段 / 解构类型错误） |
| H002 | 未定义（变量 / 字段 / 字典缺键 / 函数） |
| H005 | 语法错误 |
| H009 | 除以零 |
| H011 | 参数个数错误 |
| H600 | `throw` 抛出非 error 值（字符串/其它） |

> 完整的错误码与文案以 `src/error.rs` 与解释器 `interp.rs` 的 `runtime_err` 调用为准；VM 侧以「与解释器逐字节一致」为准则。

---

*文档随代码演进更新；修改执行内核后请同步更新第 3、10、13 节，并复跑 `regress3.py` 记录新基线。*
