# Hone 原生编译后端设计（Cranelift）

- 状态：设计草案（待评审）
- 关联：`src/aot.rs`（现有 C 后端）、`src/vm.rs`（字节码 IR）、`src/builtins.rs`（内置函数）、`.github/workflows/release.yml`、`.cargo/config.toml`
- 定位：Go 式原生编译 —— `hone build --exe` 一条命令产出本机机器码可执行文件，**构建过程零外部工具**（无 gcc/clang/cc，目标机无需 Rust），支持**交叉编译**

---

## 一、目标

| # | 目标 | 说明 |
|---|---|---|
| G1 | 一命令一产物 | `hone build --exe script.hn` → 原生 exe，干净机器（无 Hone、无 gcc）直接运行 |
| G2 | 构建期零外部工具 | 不需要 gcc/clang/cc；不需要目标平台的 Rust 工具链（目标运行时库随工具链分发） |
| G3 | 交叉编译 | Windows 构建机上直接产出 Linux（musl）可执行文件，`--target` 选择目标 |
| G4 | 行为对齐 | 输出与错误（行/列/文案/exit code）与 interp / VM **逐字节一致**（铁律扩展为四引擎） |
| G5 | 体积 | 产物 2~5MB（首版 ≤5MB，M3 优化目标 ≤3MB）；C 后端（几十 KB）保留为极端选项 |

**非目标（本版不做）**

- glibc 目标（仅 musl，与现有 release 矩阵一致）
- Windows msvc 目标（仅 windows-gnu，与 self-contained 策略一致）
- LLVM、JIT（纯 AOT）
- VM 不支持的语言特性（`with` / type 实例 / `bytes` / 切片 / `cow`）—— 与 VM 保持同一 H999 边界

---

## 二、现状与差距

**已具备（地基）**

| 资产 | 现状 | 对本方案的价值 |
|---|---|---|
| VM 字节码 | `vm.rs`：约 48 条指令的寄存器机器，显式常量池、span 表、labels、捕获布局 | **现成的 IR**，语义已被双引擎回归验证 |
| 内置函数 | 全部 Rust 实现（`builtins.rs` + arch/datamod/sysutilmod/statmod/netmod/…） | 产物直接链接同一套运行时，无 C 后端那样的受限重实现 |
| 交叉链接先例 | `release.yml`：Windows 上交叉构建 musl 目标；`.cargo/config.toml` 已配 rust-lld 链接器 | 链接管线有实证，aarch64 的 libdl 问题有现成 workaround（zig 存根） |
| C AOT 语义参照 | `aot.rs` 的 HnValue 运行时：深拷贝值语义、闭包按值捕获、引用计数、setjmp 异常 | 新后端的**语义参考实现**，逐条可对照 |
| 回归网 | `regress3`（interp vs VM 逐字节）、`regress_err`（错误逐字符） | 新后端成为第四引擎，每条路径都有 oracle |

**缺口（本方案要建的）**

1. 字节码 → 机器码的 lowering 层（`natgen`）
2. 可链接的运行时静态库（`libhoneruntime.a`，按目标分发）
3. 工具链打包：目标运行时归档 + 链接器随 `hone` 工具分发（Go 的 GOROOT 模型）
4. 四引擎回归自动化

---

## 三、总体架构

```
 script.hn
    │  parse / check（现有，生成前必须过 checker）
    ▼
   AST ── import 构建期解析（多文件合并，见 7.3）
    │
    ├──────────────────────────────┐
    │                              │（现有）VM 编译器
    │                              ▼
    │                          Chunk 字节码（IR）
    │                              │（新）natgen lowering
    │                              ▼
    │                     Cranelift IR（每 chunk 一个函数）
    │                              │ ObjectModule::finalize
    │                              ▼
    │                     script.o（脚本代码 + const blob + span 表 + 静态表）
    │                              │
    │        rust-lld ────────────┤
    │        （随工具链分发）       │ + libhoneruntime.a（目标对应，随工具链分发）
    │                             │ + 目标 CRT 归档（self-contained）
    │                             ▼
    │                       native exe（脚本 + 运行时，独立运行）
    │
    └──→ interp（现有，语义基准）
```

要点：
- **lowering 的输入是 VM 字节码，不是 AST**（决策 4.2）
- 脚本代码由 Cranelift 生成；运行时是 rustc 编译的 Rust 静态库 —— 两者由 rust-lld 链接
- 交叉编译 = Cranelift 换目标 ISA（`isa::lookup(triple)`）+ 换对应目标的运行时归档/CRT，管线不变

---

## 四、关键设计决策

### 4.1 为什么选 Cranelift

| 路线 | 构建期外部依赖 | 结论 |
|---|---|---|
| **A. Cranelift（object 模式）** | 无（纯 Rust，无 LLVM） | ✅ 采用 |
| B. AST 直接 → Cranelift | 无 | 优化上限更高，但代码量 2~3 倍、语义基准模糊，留给未来 |
| C. 保留 C 生成 + 内嵌 tcc | 无 | ❌ tcc 在 Windows 上只有 32 位/实验性 64 位，本项目以 Windows 为主 |
| D. LLVM（inkwell） | 无 | ❌ 二进制 +50MB、构建链重，违背"轻量单文件"定位（Go 自己也是避开 LLVM） |

- 依赖：`cranelift-codegen` + `cranelift-module` + `target-lexicon`，**版本三元组锁定**（wasmtime 生态一致版本）
- `ObjectModule` 产出 ELF/COFF 目标文件，直接交 rust-lld 链接（miri 的 Cranelift AOT 后端已验证此路线可行）
- 所有 Cranelift API 调用封装在 `natgen/` 内，API 漂移的升级面收敛到少数文件

### 4.2 输入 IR 复用 VM 字节码，不做 AST 直接代码生成

- 字节码是**现成且被验证的 IR**：四引擎对齐天然以 VM 为语义基准，lowering 只是机械的逐指令映射（约 48 条）
- 未来新增 VM 指令时，lowering 有显式 default 分支兜底（H999 + 单元测试断言 `Instr` 全覆盖），不会静默漏指令
- AST 直接代码生成保留为远期优化方向（内联、逃逸分析等），届时可替换 lowering 前端而不影响链接管线

### 4.3 值模型与 ABI

机器 ABI 值类型（与 C 后端 `HnValue`、interp `Value` 语义对齐）：

```rust
/// 24 字节，按值传递：3 个 64 位寄存器，SysV / Win64 x86_64 均放得下
pub struct Hnv { pub tag: u8, pub a: u64, pub b: u64 }
```

| 类型 | tag | 承载 |
|---|---|---|
| int | INT | `a = i64` |
| float | FLOAT | `a = f64.to_bits()` |
| bool | BOOL | `a = 0/1` |
| char | CHAR | `a = u32` |
| null | NULL | — |
| str / list / dict / fn / enum / err | 各自 tag | `a = 堆对象指针`（b 预留 len/扩展） |

- **堆对象**（str/list/dict）放入**引用计数 box**（`HnBox`，见 4.8 RC+COW 值模型）；enum/err/char 走内联 tag。值语义与 C 运行时一致（变量读取 = 拷贝、闭包按值捕获），但"拷贝"由 RC+COW 实现为 O(1) retain + 写时惰性深拷（见 4.8）
- **值语义逐条对齐 C 运行时**：变量读取 = 深拷贝（`rt_copy`）；list 索引赋值 = clone 写回；函数按值传参；闭包按值捕获
- **寄存器帧 → 内存槽**：每个 chunk 函数的帧含 `regs` 字节数组（`nregs * 24`），VM 寄存器 = 内存偏移。M0 不做寄存器分配（Cranelift 自动分配 IR 值）；M1 起对 int 运算加快速路径（tag 检查 + 内联算术 + 出错分支）

### 4.4 调用模型

- 每个 chunk（用户函数 / lambda / 顶层）lowering 为**一个 Cranelift 函数**，形如 `fn __hn_f<N>(frame: *mut Frame) -> Hnv`（返回值为顶层 return 值；实际结果写 dst 槽）
- 启动时向运行时**函数注册表**登记：`name → (fnptr, nparams, captured 布局)`
- `Call` 指令 lowering 分两档：
  - **M0**：统一 `rt_call(env, frame, name, args, span)` —— 运行时完整复刻 `do_call` 逻辑（局部变量查 lambda/字符串 → 函数注册表 → 内置 → 运算符重载 → 报错），正确性优先
  - **M1**：编译期可判定"用户函数且未被局部变量遮蔽"时 → **直接函数指针调用**（递归即普通调用，无特判）；其余走 `rt_call`
- `MakeLambda`：`rt_alloc_fn(env, entry_ptr, cap_values) → HnFn { entry, cap: Hnv[] }`；lambda 调用 = `rt_call_fn(fn_value, args, span)`

### 4.5 异常（无 setjmp）

- 运行时**线程局部错误槽**：`(code, msg, file, line, col, context, 抛出值)`
- 每个 fallible `rt_*` 辅助函数：失败时写错误槽，返回 `Hnv{tag: ERR}`
- lowering 期为**每条指令静态计算"当前最近的 catch handler"**（VM try 栈的静态等价物：词法上最近的外层 try）
- 生成代码模式：fallible 指令后 `if tag == ERR → 跳转对应 handler block`（catch 寄存器 = 错误值）；try 之外 → `rt_unwind`（按 interp 格式打印 + `exit(1)`）
- `Throw`：写错误槽后跳最近 handler —— 与 VM "抛出弹到最近 try" 的语义严格一致
- 诊断复用 `ZError` 的渲染路径（文案/行/列/format 函数与双引擎同一套），保证 G4

### 4.6 内置函数与分发

- 字节码 `Call` 携带名字字符串；`rt_call` 统一分发：内置（`builtins.rs` 现有名字表）→ 用户函数 → 重载（镜像 `overload()`）
- 构建期**无需**枚举脚本用到哪些内置（M0/M1 走运行时动态分发，正确性优先）
- M3 体积优化：从字节码静态收集调用的内置名 → 生成的对象文件只含"用到的 (名字 → fnptr) 分发表"，未用到的 `mod*` 由 `--gc-sections` 裁掉

### 4.7 常量与静态表（嵌入 script.o）

| 表 | 内容 | 用途 |
|---|---|---|
| const blob | 各 chunk 的 `Const`（int/float/str/char/null）序列化 | `LoadK` |
| span 表 | 指令级 `Span` 数组 | 错误行/列（G4 对齐的硬依赖） |
| 函数注册表 | 名字/arity/captured 布局 | 启动登记 |
| struct/enum 定义表、alias 表、async_fns 集 | 编译期收集 | 类型相关运行时行为 |

由 natgen 生成，作为对象文件的 `.rodata` 段 + 启动自检（FNV-1a，复用 `bundle.rs` 机制）。

### 4.8 RC + COW 优化值模型（堆对象共享与写时复制）

**动机**：C 后端的 `hn_copy` 每次读变量都整树深拷贝（`a[i]=x` 先克隆整个 list 再改一个槽位），O(n) 拷贝是脚本热点开销。新后端在**值语义不变**（读变量拿独立副本、写不污染其他变量）的前提下，把拷贝降为 O(1) retain + 写时惰性深拷。

**对象布局**（运行时侧，与解释器 `Value` 解耦）：

```rust
/// 引用计数 + 写时复制 box：堆对象（str/list/dict）的共享容器
#[repr(C)]
pub struct HnBox {
    pub rc: AtomicUsize,   // 引用计数（写锁计数合并其中：0=只读共享）
    pub kind: u8,          // STR | LIST | DICT
    pub flags: u8,         // 高位：脏位/保留
    // kind 相关载荷：HnStr / HnList / HnDict（运行时内部结构）
}
```

**不变量**：`rc > 1` 时对象必须处于**不可变共享态**（所有持有者只读）；`rc == 1` 时可独占原地修改。任何写操作前先过 `cow()` 门：

```rust
/// 写时复制门：独占则放行原地写；共享则整树深拷一份独占副本，共享方 rc-1
pub unsafe fn cow(this: *mut HnBox) -> *mut HnBox
```

- `cow()` 深拷是**惰性**的：只在首次写路径触发，只拷贝"被写的那条路径上的父节点"（list 写槽位只拷外层 list + 槽位值 retain，兄弟元素不碰；dict 同理）
- str 不可变（Hone 无字符级 str 原地写），`rc>1` 的 str 永不触发深拷，retain 即"拷贝"
- 线程：`GoCall` 跨线程传递的堆对象按 Hone 现有线程语义（消息传递深拷）处理，共享 box 跨线程前先 `cow()` 独占（M2 处理，M0/M1 不碰并发）

**与 4.3 值语义的映射**（逐条对齐，不改语义只改代价）：

| C 运行时行为 | 新后端行为 | 代价 |
|---|---|---|
| 变量读取 = `hn_copy`（整树深拷） | 读取 = `retain(box)`，写者触发 `cow()` | O(1) / 写 O(path) |
| `a[i] = x` = 克隆整个 list 再改槽位 | `cow(a)` → 原地改槽位（槽位旧值 release、新值 retain） | O(path) 首次，O(1) 后续 |
| 函数按值传参 = 每参深拷 | 每参 retain（独占语义由 cow 保证） | O(1)/参 |
| 闭包按值捕获 = 深拷捕获表 | 捕获表整表 retain | O(ncap) retain |
| `==` / `len` / 迭代 / display | 不变（读操作，不触发 cow） | 不变 |

**与解释器 `cow` 声明式容器的关系**：解释器的 `cow` 是**显式关键字**（用户选择）；本节的 RC+COW 是**运行时默认值模型**（用户无感，所有 list/dict/str 共享）。两者不冲突：`cow` 声明在 VM/新后端仍按 H999 边界处理（非目标清单不变），运行时默认 COW 不影响该边界。

**正确性护栏**（对齐铁律）：
1. `regress3`/`regress_err` 逐字节比对天然覆盖：若 COW 深拷时机/深度与解释器 `Value::clone` 行为有任何语义差（别名泄漏、共享残留），双引擎输出即分叉 → 回归直接抓住
2. 运行时内置**debug 断言**（仅 debug profile 编译进运行时）：`release` 后检查 `rc==0` 且对象仍存活（泄漏）/ `rc<0`（double-free）；`cow()` 后断言独占
3. M1 增加专门别名用例集（`tests/smoke/rc_alias.test.hn`）：`b = a; a[i]=x; print(b)` 类，覆盖 retain/cow/release 三条路径

**体积与性能预算**：RC+COW 代码约 300 行运行时（原子计数 + 分 kind 深拷），对产物体积影响 <0.1MB；`opt-level=z` 下原子操作在单线程场景为普通 mov（x86_64），共享路径才走 `lock inc`。

---

## 五、链接管线（无 C 编译器）

### 5.1 目标：`script.o + libhoneruntime.a + CRT → exe`

- `script.o`：Cranelift ObjectModule 产出（ELF/COFF 目标文件，含代码 + 静态表）
- `libhoneruntime.a`：运行时 Rust crate 以静态库形式编译（rustc `--crate-type=staticlib` 或 cargo `crate-type = ["staticlib", "rlib"]`），**构建机按支持的目标矩阵各编译一份，随 `hone` 工具分发**（Go 的 GOROOT 模型：工具链自带目标运行时，目标机不需要 Rust）
- CRT：musl 目标用 self-contained CRT 归档（`musl-gcc` 的 libcrt1.o 等，随工具链打包）；windows-gnu 目标用 MinGW crt2.o（同法打包）
- 链接器：**rust-lld**（随 Rust 工具链自带的 lld），构建机 Windows 上对 musl/elf 目标已有实证（`.cargo/config.toml`、release.yml）

### 5.2 链接命令（示意）

```
# Linux musl 目标（在 Windows 构建机上）
rust-lld -flavor gnu -m elf_x86_64 \
  script.o libhoneruntime.a libmuslcrt.a \
  --gc-sections -o app

# Windows gnu 目标（在 Windows 构建机上）
rust-lld -flavor link script.o libhoneruntime.a libcrt2.a -subsystem console -o app.exe
```

### 5.3 aarch64-musl 的 libdl 问题

现有 release 流水线已踩过：rust-lld 在 aarch64-musl 上 `-ldl` 解析失败，用 zig 的 musl libc 存根链接。两个选项：
- **M0/M1**：直接沿用 zig 存根（与现状一致，不阻塞）
- **M3**：在运行时归档中内嵌一份最小的 libdl 存根（`dlopen` 等在目标平台无意义的桩），彻底摆脱 zig 依赖

### 5.4 M0 链接集成（原型期）与 5.1~5.3 的分阶段落地

M0 不直接做 rust-lld 裸链接（CRT 归档收集、musl 存根等留 M3），采用**临时 cargo 工程**验证全管线：

```
tmpbuild/
  Cargo.toml            [dependencies] honeruntime = { path = ... }（rlib）
  src/main.rs           fn main() -> i32 { extern "C" { fn __hn_main() -> i32 }
                        #[link(kind = "object", name = "script")]
                        __hn_main() }
  link.txt              script.o 的绝对路径（通过 build.rs 注入 -C link-arg=obj）
```

- 构建机 host 链接器（windows-gnu 的 link.exe）负责 CRT —— M0 只验证"Cranelift object + Rust 运行时 + 系统 CRT"可链接可运行，**目标本机**
- 产物 = 临时工程 release exe 拷贝到 `-o`（strip 后）；M0 产物体积会偏大（未裁 CRT/静态链接器未压），体积指标从 M3 起算
- M3 切换到 5.1 的裸 rust-lld + 随工具链分发的目标 CRT 归档（去掉临时工程、解锁交叉编译）；脚本侧符号约定（`__hn_main`、`hn_rt_*`）两阶段完全一致，切换无感

### 5.5 产物体积策略

| 手段 | 预期效果 |
|---|---|
| 运行时 `opt-level=z` + `strip` + `panic=abort`（与主 crate 同 profile） | 基础 2~3MB |
| `--gc-sections` + 按脚本用到的内置裁剪分发表（4.6） | 去未用模块 |
| 静态表按脚本实际 chunk 只生成用到的表 | 去未用元数据 |
| 首版不做：per-target 运行时多版本、LTO 跨 crate | 留观察 |

首版目标 ≤5MB；M3 后目标 ≤3MB（对标 Go hello 的 ~2MB 量级）。

---

## 六、交叉编译

### 6.1 目标矩阵（首版）

| 目标 triple | 构建机 | 链接器 | CRT/运行时来源 |
|---|---|---|---|
| `x86_64-pc-windows-gnu` | Windows | rust-lld (link flavor) | MinGW crt2.a（随工具链） |
| `x86_64-unknown-linux-musl` | Windows | rust-lld (gnu flavor) | musl CRT 归档（随工具链） |
| `aarch64-unknown-linux-musl` | Windows | rust-lld (gnu flavor) + zig 存根 | musl CRT 归档（随工具链） |

- 与现有 release.yml 的产物矩阵完全一致 → 用户侧无新概念
- 未来扩展（macos、aarch64-windows）：加一行 Cranelift ISA + 对应 CRT 归档即可，管线零改动

 ### 6.2 工具链打包布局（随 `hone` 分发）

```
hone/
  hone.exe                       解释器 + 编译器宿主
  targets/
    x86_64-pc-windows-gnu/
      libhoneruntime.a
      crt2.a
    x86_64-unknown-linux-musl/
      libhoneruntime.a
      libmuslcrt.a 
    aarch64-unknown-linux-musl/
      libhon                                                                                     er   untime.a
      libmuslcrt.a
      libdl-stub.a
  lld/
    rust-lld.exe                 或指向系统 Rust 工具链的路径（可配置）
```

- 体积预算：每  目标运行时归档约 1~2MB（musl 下 rustc staticlib 经 strip 后），三目标合计 ~5MB，工具链总增量可控
- 安装脚本（install.ps1 / install.sh）从"下载单个 exe"扩展为"下载 zip/tar.gz 工具链包"；`hone self-update` 同步升级整个包
- `HONE_TOOLCHAIN_DIR` 环境变量覆盖工具链目录（开发/嵌入式场景）

### 6.3 交叉编译的正确性约束

- 交叉产物**无法在构建机上运行**，回归策略：
  - 构建机本地目标（win-x86_64-gnu / linux-musl 在 Linux CI）跑完整四引擎回归
  - 交叉产物（windows 上编的 linux）走 CI 的 Linux job 二次验证（GitHub Actions ubuntu runner）
  - 这正与现有 release.yml 的"Windows 编 musl 产物"的验证方式一致
- **目标 CPU 默认**：x86_64 默认 `x86-64-asm` baseline + SSSE3（与当前 release 目标一致，避免 AVX 类指令在旧 CPU 上非法）；aarch64 默认 v8 基础集。`--cpu` 选项留 M3

### 6.4 与现有 release 流水线的关系

- 现有 release.yml 交叉构建的是 **`hone` 工具本身**（解释器二进制）—— 继续保留，因为解释器仍是默认执行方式
- 新增的交叉能力是 **`hone build --exe --target`** 编译用户脚本 —— 运行时归档随工具链走，release 流水线只需多发布 `targets/` 目录
- 两条线互不干扰：解释器产物走现有矩阵，脚本产物走新管线

---

## 七、构建接口

### 7.1 命令

```
hone build --exe <script.hn> [-o <out>] [--target <triple>] [--keep-obj] [--gc-sections]
```

- 默认 `--target` = 构建机自身（本机编译）
- `--target` 指定 → 交叉编译，产物名默认带目标后缀（`app-x86_64-unknown-linux-musl`）
- 现有 `-c` 标志（C 后端）语义保留为兼容别名：无 C 编译器时自动落到新 Cranelift 路径（并提示已切换）；`--c` 显式强制旧 C 路径（需 gcc）
- `--keep-obj` 保留中间目标文件（调试用）

### 7.2 构建阶段输出（四阶段，与现有 `[1/4]` 风格一致）

```
[1/4] 解析与类型检查...
[2/4] 生成字节码 IR...
[3/4] 降低为机器码 (x86_64-unknown-linux-musl)...
[4/4] 链接 (rust-lld)...
生成 app-x86_64-unknown-linux-musl 完成（原生编译, 2.8 MB）
```

### 7.3 import 构建期解析（多文件脚本）

现有 VM 对 `import` 报 H999（不支持远程模块）。原生编译需要：
- 构建期把 `import "x" from "./local/x.hn"` 的**本地路径** import 递归解析、合并进同一编译单元（常量池/函数表统一编号）
- 远程 `import ... from "http://..."`：构建期下载到 `~/.hone/cache/`（复用 `fetch_module` 缓存逻辑）后按本地处理；构建失败时提示先 `hone get`
- 这是**原生编译独有**的能力（解释器/VM 仍按原 import 语义运行），不构成对现有行为的破坏
- struct/enum/type 定义跨文件合并：同名定义在合并期报错（沿用现有 checker 的全局命名空间规则）

### 7.4 与 `hone check` / `hone fmt` / LSP 的关系

- 原生编译入口**强制过 checker**（与现有 `build --exe -c` 一致，生成前必须类型检查）
- `hone check` / `fmt` / LSP 不涉及新后端（纯文本/类型层），零改动
- LSP 的 `build --exe` 命令提示文案可补充 `--target` 说明（M3 一起做）

---

## 八、里程碑

| 阶段 | 范围 | 退出标准 | 规模 |
|---|---|---|---|
| **M0 管线打通** | hello / fib / int/str/bool/char / 控制流 / 用户函数 / 基本错误；x86_64 本机目标 | `hone build --exe examples/fib.hn` 出 exe，跑通且输出与 interp 一致；四引擎对 fib/hello 逐字节对齐 | 1~2 周 |
| **M1 核心语义** | list/dict/索引/字段/闭包/lambda/try-catch；RC+COW 值模型落地（4.8，retain/cow/release 三条路径）；`rt_call` 完整分发 | `regress3` 全部用例四引擎对齐；错误诊断 `regress_err` 对齐；`rc_alias.test.hn` 别名用例集全过 | 2~4 周 |
| **M2 功能对齐** | struct/enum/match/运算符重载/alias/goto/async-await；H999 边界与 VM 一致 | 现有 .t_neg / examples 全过；新后端 H999 集合 = VM H999 集合 | 4~8 周 |
| **M3 集成与优化** | import 多文件合并、交叉编译三目标、体积裁剪、工具链打包、四引擎回归自动化入 CI | release 流水线产出三目标脚本产物；产物 ≤3MB；交叉产物 CI 二次验证通过 | 2~4 周 |

**风险与缓解（贯穿）**

| 风险 | 等级 | 缓解 |
|---|---|---|
| Cranelift 无 setjmp，异常 lowering 复杂 | 高 | 4.5 静态 catch 映射 + 每指令 ERR 检查；M0 先用最小 try 验证 |
| 值语义对齐（深拷贝/闭包捕获）逐条核对 | 中 | C 运行时 + interp 双参照；每条 lowering 配 unit test |
| rust-lld 对 COFF/musl 边角 | 中 | 现有 release 已验证 ELF；COFF 在 M0 用 hello 验证 |
| 体积超标 | 中 | 4.6/5.4 分层裁剪；首版 5MB 封顶 |
| 功能长尾（async/enum/match） | 中 | M2 集中消化；H999 边界显式声明，不静默 |
| Cranelift API 版本漂移 | 低 | 版本锁定 + API 封装在 natgen/ 内 |

---

## 九、文件布局

```
src/
  natgen/              （新）字节码 → Cranelift lowering
    mod.rs             入口：compile(chunk) → object bytes
    lowering.rs        逐指令 lowering（48 条，显式全覆盖 + 兜底）
    runtime_abi.rs     Hnv / Frame / rt_* 辅助函数的符号约定
    statics.rs         常量 blob / span 表 / 函数注册表生成
  runtime/             （新）可链接运行时 crate（crate-type: staticlib,rlib）
    lib.rs             rt_call / rt_copy / rt_alloc_* / 错误槽 / 分发表
    （复用 builtins.rs 的内置实现，经 crate 边界暴露）
Cargo.toml             + cranelift-codegen / cranelift-module / target-lexicon
                       + [lib] 段（runtime 作为 workspace 内 crate 或 path dep）
runtime/               （新）可链接运行时 crate（crate-type: staticlib,rlib；按目标矩阵各编译一份）
tests/
  regress4.py          （新）四引擎回归（interp / VM / AOT-C / AOT-nat 逐字节）
  smoke/               现有冒烟用例，扩展 --target 维度
  smoke/rc_alias.test.hn （新，M1）RC+COW 别名/retain/cow/release 用例集
.github/workflows/
  ci.yml               增：nat 后端四引擎回归
  release.yml          增：targets/ 运行时归档随工具链发布
```

- `Value` 定义仍在 `interp.rs`，`runtime/` 通过 crate 边界复用（不改 Value 本体，符合"Value 在 interp、VM 与 builtins 复用之"的既有约定）
- `builtins.rs` 不拆分，`runtime/` 以 `#[path]` 或 pub re-export 方式复用（避免大规模重构）

---

## 十、验收清单（完成定义）

- [ ] `hone build --exe hello.hn` 在无 gcc 的干净机器上产出可运行 exe
- [ ] `hone build --exe app.hn --target x86_64-unknown-linux-musl` 在 Windows 上产出 linux exe，CI 二次验证通过
- [ ] `regress4.py` 全用例四引擎逐字节对齐（含错误诊断）
- [ ] 新后端 H999 集合与 VM 完全一致（无静默漏实现）
- [ ] 产物 ≤5MB（M3 后 ≤3MB）
- [ ] 文档同步：`hone.md` / `CHANGELOG.md` / `README.md` / 官网 changelog
