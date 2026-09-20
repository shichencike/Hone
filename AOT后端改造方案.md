# Hone AOT 后端改造方案

> 结论先行：**选路径 1（保留并强化 AOT），放弃路径 2。**
> 依据是本机实测数据，而非推测。数据见第 1 节。

---

## 1. 决策依据：实测数据推翻了"移除"的假设

上一轮我倾向移除 AOT，理由是"能力弱、未被基准验证"。拿到可用 C 编译器后实测，
**这个判断是错的**。

### 1.1 实测结果（Windows x86_64，预热后取 5 轮最快）

| 基准 | AOT 原生 | 解释器 | 字节码 VM | AOT 相对解释器 |
|---|---|---|---|---|
| `fib(25)` 递归 | **649 ms** | 1334 ms | 1587 ms | **2.1× 快** |
| 纯循环 2000 万次 | **694 ms** | 36979 ms | 超时截断 | **53× 快** |
| 同基准对照 `rustc -O` | 630 ms | — | — | AOT 仅慢 1.1× |

`fib(25)` 原为 1.0s（首次冷启动，含杀毒扫描），预热后稳定在 649ms。
循环基线的解释器耗时 36.9s，与 `README.md` 记载的"71s → 42s"同量级，可信。

### 1.2 这个数据说明什么

1. **AOT 不是可选的锦上添花，而是 Hone 唯一的"能跑计算"后端。**
   2000 万次循环在解释器下要 37 秒、AOT 下 0.7 秒——这不是优化幅度差异，是**可用与不可用**的差异。
2. **AOT 已接近 rustc -O 的手写 Rust**（694 vs 630 ms，慢 10%）。
   考虑到 Hone 的值模型是 boxed `HnValue` + 引用计数、而对照 Rust 用裸 `i64`，
   这个差距说明 `aot.rs` 的代码生成质量相当高。
3. **README 宣传的"体积优势"反而是最不重要的一点。**
   真正价值是执行速度，而这一条此前从未被测量过。

### 1.3 附带查清：本机 AOT 报错的真实原因

排查中发现 `hone build --exe -c` 在本机常报 `H999 / CacheCheckFailed`，
**曾让我误以为 AOT 坏了**。实际根因：

- 本机无 MinGW/MSVC，`D:\shichencike\Desktop\Educe\.tools\bin\gcc.exe` 是一个
  **自建的 zig 包装器**（源码 `zig-wrap.c` 在同目录，注释写明用途）。
- zig 需要能解析缓存目录；在环境变量不全的场景下报
  `error: unable to resolve zig cache directory: AppDataDirUnavailable`。
- **同一份 `p.c` 手工调用该 gcc 能编译成功（56832 字节产物），经 hone 调用则失败**；
  且 hone 报"produced no output file"时产物**其实已生成**（已实测 `fib.exe` 59904 字节存在并可运行）。

即：这是**本机工具链包装器的环境依赖问题**，不是 Hone 的缺陷。
但它暴露了 `run_cc_exe` 的两个真实问题，见 §4。

---

## 2. 路径选择说明

用户给的两条路径，与代码库实情有偏差，先澄清：

| 用户描述 | 代码实情 |
|---|---|
| 路径 1："保留 AOT：翻译为 **Rust** 源码再调 Rust 工具链" | 现有 AOT 是翻译为 **C**（`aot.rs:1153 generate()` → C 源码 → gcc/clang）。**"翻译为 Rust"是一条尚不存在的新路线**，不是"保留现有 AOT"。 |
| 路径 2："移除 AOT：删除后端实现及其依赖" | 可行，但会把 53× 的性能优势一并删除。 |
| "本项目是静态编译的" | hone **编译器自身**静态（无 C 依赖，TLS 用纯 Rust rustls）；但 **AOT 产物不静态**——依赖系统 C 编译器，生成链接 libc 的普通 exe。AOT 恰是全项目唯一引入外部工具链的功能。 |

### 2.1 为什么选 1 而不是 2

- **选 2（移除）** = 主动放弃 53× 性能。在解释器跑一次循环要 37 秒的情况下，
  等于让 Hone 彻底退出"计算密集型"场景。
- **选 1（保留）** = 保住性能优势，但要**偿还技术债**：补全缺失语法、修 H999 误报、
  把它纳入基准与 CI。

### 2.2 但路径 1 的"翻译为 Rust"要明确否定

**不建议把翻译目标从 C 改为 Rust**，理由：

1. **收益不确定**：现有 C 产物已达 rustc -O 的 90%，换目标语言的空间很小。
2. **成本极高**：需重写 1747 行翻译层 + 1089 行运行时（C → Rust），
   且 Rust 编译明显慢于 C（本机 cargo build release 需 7 分钟，gcc 编译 4.5 万行 C 只要 2 秒）。
   **AOT 的核心价值是"快速产出快程序"，用 7 分钟的 Rust 编译换 10% 性能是亏本。**
3. **引入重量级依赖**：产物将依赖 Rust 工具链（数百 MB），而 C 工具链是系统常备。
4. **`--dll` 已经证明 C 路线可行**：`codegen.rs` 走的就是 C + typed FFI，两者可共享基础设施。

**因此：选路径 1 的"保留 AOT"部分，不采纳其"改为 Rust 目标"部分。**
保留 C 后端，把力气花在补全与修缺陷上。

---

## 3. 现状盘点

### 3.1 代码结构

| 项 | 数值 |
|---|---|
| `src/aot.rs` 总行数 | 2836 |
| ├─ 内嵌 C 运行时 `RUNTIME_C` | 1089 行（85 个 `hn_*` 函数） |
| └─ Rust 翻译层 | 1747 行 |
| 调用点 | **仅 1 处**：`main.rs:811` `aot::generate(&program, path, &src)` |
| 共享工具函数 | `find_cc()` `main.rs:956`、`run_cc_exe()` `main.rs:1008`、`run_cc()` —— 与 `--dll` 共用 |
| Cargo 依赖 | **无 AOT 专属依赖，无 feature 门控** |
| 回归脚本引用 | **零**（`regress3/err/ir.py` 均不涉及） |

### 3.2 与 `--dll` 的关系（关键，影响改造边界）

`hone build` 有三条**互相独立**的路径：

| 命令 | 实现 | 产物 |
|---|---|---|
| `--exe`（无 `-c`） | `bundle.rs` | 自释放 exe（内嵌解释器） |
| `--exe -c` | **`aot.rs`** | AOT 原生 exe（转 C） |
| `--dll` | **`codegen.rs`**（1355 行） | C ABI 动态库（转 C，typed FFI） |

- `aot.rs` 与 `codegen.rs` **零共享**（各有一份 C 生成逻辑）。
- 但 `find_cc` / `run_cc` / `run_cc_exe` 是 `main.rs` 里的公共函数，**两者都用**。
- **结论：动 `aot.rs` 不会牵连 `--dll`；但公共编译器查找/调用函数必须保留。**

### 3.3 AOT 的能力边界（现状）

不支持的语法，均在编译期报 `H999`：

| 类别 | 报错位置（`aot.rs`） |
|---|---|
| `goto` / 标签 | :1661 |
| `char` 类型与字面量 | :1675、:1698、:2220 |
| `import` 模块 | :1959 |
| `load` 动态库 / FFI | :1967 |
| `go` 并发 | :1975 |
| `async` / `await` | :1983、:2532 |
| 非核心内置函数 | :2748（`builtin_err` 延迟报错） |

**核心内置白名单只有 41 个**（`is_aot_builtin`，`aot.rs:2154`）：
`print len to_str to_int to_float type_of is_* append contains index_of
keys values has_key abs max min str_contains str_replace str_trim
read_file write_file file_exists read_bytes write_bytes
input read_int read_float assert clone copy time.now time.sleep random.int random.float`

**这意味着 `hone_lib/` 里绝大多数函数（`math_*`/`collections`/`json`/`csv`/`sqlite`…）都编不过。**
这是 AOT 当前最实际的短板——不是"少数边角语法不支持"，而是**标准库基本不可用**。

---

## 4. 改造方案

按优先级分为四批。每批都可独立完成、独立验证。

### 批次 A：修正确性缺陷（最高优先级，工作量 S）

**A1. 修复 `run_cc_exe` 的产物检查误报**

现状（`main.rs:1008-1037`）：C 编译器返回 0 但 `out` 不存在时报 H999。
实测发现**产物确实存在却仍报错**（`fib.exe` 59904 字节，可运行）。

根因：`out` 是相对路径（如 `fib.exe`），检查用 `std::path::Path::new(out).exists()`，
依赖调用时的 cwd；若编译器把产物写到别处（或 cwd 被改变），检查即失败。

修法：
```rust
// 1. 检查前把 out 规范化，并用绝对路径比对
let out_abs = std::fs::canonicalize(&out).unwrap_or_else(|_| PathBuf::from(&out));
// 2. 找不到时，在同目录下按文件名回退查找一次
// 3. 报错信息里附上"已知产物路径"便于用户自查
```

**A2. 把 `CacheCheckFailed` / `AppDataDirUnavailable` 纳入错误提示**

现在统一提示"ccache/缓存包装导致无产物"，让人以为是缓存问题。
应在 help 中补一条：**若使用 zig/自定义包装器，需确保 `APPDATA`/`USERPROFILE` 等环境变量可见**。

**A3. `run_cc_exe` 增加超时与输出捕获**

现状直接 `.status()`，既不捕获 stderr（用户看不到编译器真实报错），也无超时（编译器卡死会挂住 hone）。
改为 `.output()` + 超时，失败时把编译器 stderr 附在错误里。

### 批次 B：补全标准库（收益最高，工作量 L）

这是 AOT 能否真正可用的分水岭。两个可选做法：

**B1（推荐）· 扩展 `RUNTIME_C` 里的内置白名单**

在 1089 行 C 运行时中补充高频 `hone_lib` 函数，优先：

| 模块 | 函数 | 说明 |
|---|---|---|
| `math` | `abs` `max` `min` `sqrt` `pow` `floor` `ceil` `round` | 纯数值，C 只需 `<math.h>`，**成本极低** |
| `str` | `str_split` `str_join` `str_upper` `str_lower` `str_len` `str_sub` `str_find` | 已有 `str_contains/replace/trim` 打底 |
| `collections` | `list_reverse` `list_slice` `list_sort` `dict_keys` `dict_values` | 已有 `append/keys/values` 打底 |
| `json` | `json_parse` `json_str` | 需内嵌极简解析器，成本较高，可放最后 |

**B2 · 在 AOT 里直接内联展开模块函数**

把 `import "math" from "..."` 的函数体在编译期内联进 C 代码，
绕过"模块调用不支持"的限制。**更彻底但工作量更大**，建议在 B1 之后再评估。

**B3 · 明确失败提示**

对确实不支持的，在错误里点名**替代方案**：
> `math_sqrt` 在 AOT 模式暂不支持；可用解释器运行，或改用内建 `sqrt()`（若 B1 已补）

### 批次 C：补全语言特性（工作量 M–L，按需）

按"实际使用频率 × 实现成本"排序：

| 特性 | 成本 | 建议 |
|---|---|---|
| `char` 类型 | 中 | Hone 已有完整 char 体系，AOT 缺它导致用 char 的脚本全废。**建议补**（映射到 C 的 `int32_t` 码点即可） |
| `goto` / 标签 | 低 | C 原生有 goto，映射直接。但 Hone 的 goto 有跨块安全校验，需在 AOT 侧复用同一套校验。**建议补** |
| `import` 模块 | 中 | 与 B2 是同一件事，合并处理 |
| `go` / `async` / `await` | 高 | AOT 是单线程模型，补齐需引入线程运行时。**建议长期维持"不支持+清晰报错"** |
| `load` / FFI | 高 | 与"静态编译"目标冲突。**建议维持不支持** |

### 批次 D：纳入基准与 CI（工作量 S，防止再次误判）

**D1. `bench/bench.sh` 增加 AOT 档位**

现状只有 `hone 解释器 vs python vs rust`，AOT 从未被测量——这正是我上一轮误判的根源。
新增：
```bash
# 编译 AOT 版本（若 CC 可用）
"$HONE" build --exe -c "$b.hn" -o "$OUT/$b.aot" 2>/dev/null \
  && AOT=$("$OUT/$b.aot") || echo "AOT 不可用，跳过"
```
并在输出表中加一列 `hone-aot`。

**D2. CI 增加 AOT 冒烟测试**

不是全量回归（AOT 覆盖率不足会大量误报），而是：
挑 5~10 个**纯计算**示例（如 `fib.hn`/`loop.hn`），验证
「AOT 产物能生成 + 输出与解释器逐字节一致」。

**D3. 文档同步**

- `hone.md` §4.6 的 AOT 不支持清单需补全（现缺 `goto`/`char`，我上一轮已补，但需复核）
- `README.md` 的 AOT 宣传应改为**性能导向**（"比解释器快 50×"），而非体积导向
- 明确标注"需系统 C 编译器"这一前置条件

---

## 5. 文件清单

### 新增（3 个）

| 文件 | 用途 |
|---|---|
| `tests/aot_smoke.py` | AOT 冒烟测试：挑纯计算示例验证产物与输出（批次 D2） |
| `bench/aot/`（可选） | AOT 专用基准脚本，若与 `bench.sh` 合并则不需要 |
| `docs/aot-architecture.md`（可选） | 若 AOT 要长期维护，值得单独记录运行时设计 |

### 修改（6 个）

| 文件 | 改动 | 批次 |
|---|---|---|
| `src/main.rs` | `run_cc_exe` 产物检查修正 + 超时 + stderr 捕获；help 文案 | A |
| `src/aot.rs` | `RUNTIME_C` 扩展内置函数；补 `char`/`goto` 分支；错误提示点名替代方案 | B、C |
| `README.md` | AOT 宣传改为性能导向；补"需 C 编译器"前置 | D |
| `hone.md` | §4.6 不支持清单复核补全 | D |
| `bench/bench.sh` | 增加 AOT 档位 | D |
| `CHANGELOG.md` | 记录上述改动 | 全部 |

### 删除（0 个）

**本次改造不删除任何文件。** 这是选择路径 1 的直接结果。

### 明确**不**改动

| 文件 | 原因 |
|---|---|
| `src/codegen.rs` | `--dll` 专用，与 AOT 零共享，不受影响 |
| `src/bundle.rs` | `--exe` 自释放打包，与 AOT 无关 |
| `tests/regress3.py` / `regress_err.py` / `regress_ir.py` | 对 AOT 零引用，基线不受影响 |
| `Cargo.toml` | AOT 无专属依赖，无需增删 |

---

## 6. 影响评估

### 6.1 对编译流程

**无改变。** `aot.rs` 仍是 `main.rs` 的普通模块，无 feature 门控、无构建脚本。
`cargo build` / `cargo build --release` 行为完全不变。

### 6.2 对外 API / CLI

**CLI 参数不变**（`hone build --exe -c` / `--keep-c` / `-o`），但**行为改善**：

| 场景 | 改造前 | 改造后 |
|---|---|---|
| C 编译器成功但产物路径检查失败 | 报 H999 误报（产物其实存在） | 正确定位产物，成功退出 |
| C 编译器报错 | 只显示"produced no output file" | 附带编译器真实 stderr |
| C 编译器卡死 | hone 无限挂起 | 超时退出 |
| 用了 `math_sqrt` 等标准库 | H999「暂不支持」 | （批次 B 后）可用；仍不支持时提示替代方案 |

**无破坏性变更**，所有改动都是"原本失败/误报 → 现在成功或提示更清晰"。

### 6.3 对已有测试

- **三个回归脚本完全不受影响**（对 AOT 零引用）。
- 新增 `tests/aot_smoke.py`——需注意它**依赖系统 C 编译器**，
  在无编译器的环境上必须优雅跳过（`SKIP` 而非 `FAIL`），否则会像 `regress_ir.py` 那样把
  "环境缺失"误报成"功能缺陷"。
- 基线样例数从 72 增至 72（不新增示例），或视情况加入 `examples/aot_*.hn`。

### 6.4 改造后仍可正常构建运行

验证清单：
```bash
cargo build                    # 无 AOT 专属改动，编译通过
cargo check --release          # 期望 0 warning
tests/regress3.py              # 期望 PASS=47 FLAKY=5 FAIL=0（与当前一致）
tests/regress_err.py           # 期望 PASS=37 FAIL=0
tests/regress_ir.py            # 期望 OK=38 FLAKY=5 BAD=0
tests/aot_smoke.py             # 新增：有 CC 则验证，无则 SKIP
```

---

## 7. 执行建议

**建议顺序**：

1. **批次 A（修缺陷）** —— 工作量最小，直接消除"看起来坏了"的误判，且不依赖 C 编译器就能验证。
2. **批次 D1（基准加 AOT 档位）** —— 让性能优势可见，防止未来又被误判为"无价值"。
3. **批次 B1（补 math/str 高频函数）** —— 收益最大，让 AOT 对真实脚本可用。
4. 批次 C 按需推进。

**需要你拍板的两点**：

- **批次 B 的范围**：只补 `math`/`str`/`collections`（快，但 `json` 等仍不可用），
  还是做到"覆盖 `hone_lib` 主要模块"（慢，但 AOT 才真正完整）？
- **`--dll` 与 AOT 是否要合并生成逻辑**：两者都在做"Hone AST → C"，
  合并可消除重复（一次实现两处受益），但重构风险中高。要做吗？

---

## 8. 已执行：`--dll` 与 AOT 的共用部分抽取（2026-09-19）

### 8.1 调研结论：主体不可合并，机械部分可共用

对两个后端逐项比对后确认，**代码生成主体无法合并**：

| | `codegen.rs`（`--dll`） | `aot.rs`（`--exe -c`） |
|---|---|---|
| 值表示 | `int64_t` / `double` / `bool` / `const char*` | 统一 `struct HnValue`（union + 引用计数） |
| 装箱 | 无 | 全部装箱 |
| 内嵌运行时 | **0 行** | **1089 行**（85 个 `hn_*`） |
| 存在理由 | 对接 **C ABI** | 独立运行、支持动态类型 |
| 不支持分支数 | 25 处 | 11 处 |

`--dll` 的对外契约就是 C ABI（导出 `int64_t f(const char*)`），
强制合并意味着让 `--dll` 也装箱 —— **这会破坏所有既有 `--dll` 调用方**。
故采取折中：**只抽取与值模型无关的机械工具**。

### 8.2 抽取内容

新建 `src/cgen_util.rs`（71 行，含 4 个单元测试），提供 `c_str_lit()`。

**这不仅是消重，更修了一个真实缺陷。** 两个原本地实现不一致：

| 控制字符（如 `0x01`） | `aot.rs` 原实现 | `codegen.rs` 原实现 |
|---|---|---|
| 生成形式 | `\001`（三位八进制） | `\x01`（十六进制） |

而 **C 的 `\x` 转义是贪婪的**——实测（gcc 编译验证）：

```
"\x01a"  → strlen=1, 首字节=26   ← 被解析成单字节 \x1a，字符串被截短、内容错误
"\001a"  → strlen=2, 首字节=1    ← 正确
```

即 `codegen.rs` 生成的 DLL 中，**任何含控制字符且后随十六进制数字（0-9a-fA-F）的字符串都是错的**。
这类错误不会引发编译失败，只会静默产出错值。

统一采用三位八进制（固定补足 3 位，最多吃 3 位数字，消除歧义）。

### 8.3 文件影响

| 文件 | 改动 |
|---|---|
| `src/cgen_util.rs` | **新增**（71 行 + 4 单测） |
| `src/main.rs` | `:9` 加 `mod cgen_util;` |
| `src/aot.rs` | 删本地 `c_str_lit`（原 :2134-2153），改 `use crate::cgen_util::c_str_lit;` |
| `src/codegen.rs` | 删本地 `c_str_lit`（原 :1266-1281），改 `use crate::cgen_util::c_str_lit;` |

**净减少约 20 行重复代码**，并新增单元测试覆盖。

### 8.4 验证结果

| 项目 | 结果 |
|---|---|
| `cargo build` | 0 warning / 0 error |
| `cargo test`（新增 4 个单测） | 4 passed, 0 failed |
| 关键回归测试 | `c_str_lit("\u{1}a") == "\"\\001a\""` ✅（若退化为 `\x01a` 即失败） |
| `tests/regress3.py` | PASS=47 FLAKY=4 FAIL=0 |
| `tests/regress_err.py` | PASS=37 FAIL=0 |

### 8.5 未纳入共用（有意保留）

| 候选项 | 不共用的原因 |
|---|---|
| 运算符映射（`overload_fn_name` / `native_bin_cond` / `builtin_bin_fn`） | 仅 `aot.rs` 有；`codegen.rs` 是内联 `match`，语义不同（装箱 vs 原生），强行抽取反而增加耦合 |
| C 运行时 | `codegen.rs` 根本不生成运行时（纯原生类型），无共享基础 |
| `gen_stmt` / `gen_expr` 主体 | 值模型不同，见 §8.1 |
| `gen_proto` | 两者签名规则不同（`--dll` 需 `__attribute__((visibility))` 导出标记，AOT 全部 static） |

---

## 附：本方案的实测数据来源

所有数字均为 2026-09-19 在本机实测，方法为：
- 编译：`cargo build --release`（7 分 04 秒，0 warning）
- 基准脚本：`fib(25)` 递归、2000 万次整数累加循环
- 计时：`date +%s%N` 前后取差，**预热一次后取 5 轮最快值**（消除杀毒/首次加载干扰）
- 对照：`rustc -O` 编译等价 Rust 代码
- 编译器：`D:\shichencike\Desktop\Educe\.tools\bin\gcc.exe`（zig 包装器，clang 21.1.0）

原始数据：
```
fib(25)      AOT 649ms / 解释器 1334ms / VM 1587ms
loop 20M     AOT 694ms / 解释器 36979ms / VM 超时
loop 20M     rustc -O 630ms
```
