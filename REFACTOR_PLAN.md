# 重构方案：VM / nat 引用模型 + 自动隔离（内部优化，语义不变）

> 状态：方案已定稿，未开工。进度见 `REFACTOR_PROGRESS.txt`。
> 日期：2026-10-21

## 目标

把 `List / Dict / Str / Bytes` 从深拷贝值语义改为 **内部 Arc 共享引用 + 写点自动隔离**：

- VM 寄存器赋值/传参/读取 = 句柄拷贝（指针+计数，O(1)），不再深拷贝整个容器；
- nat runtime 同口径（现状已是句柄，主要调整 retain/drop 纪律与变异门控）；
- **可观察行为逐字节不变**（值语义由「变异前自动 .copy() 隔离」保证），interp 作为语义基准同步但改动收敛在 Value 定义与 COW 门控泛化；
- 顺带**废弃 `cow` 声明**（唯一一处用户可见变更，需同步文档；见风险 R3）。

## 已确认的用户决策

1. **语义口径**：不改语言语义——别名不可见，可观察行为与现状逐字节一致；仅内部实现优化。
2. **引用粒度**：List + Dict + Str + Bytes 进入引用模型；Int/Float/Bool/Char/Byte 保持内联；TypeInst/Enum/Lambda/Future 已是 Arc 不动。
3. **copy 插入点**：变异前自动隔离（写点 `make_unique` 门控）+ 读端共享、写端拷贝。不新增用户可见 `.copy()` 内置。
4. **COW 关系**：废弃 `cow` 声明，基建内部化（`Value::Cow` 变体删除，Arc+写时复制机制泛化为所有容器的统一写屏障）。

## 核心设计

**模型**：

- `Value::List(Arc<RwLock<Vec<Value>>>)`
- `Value::Dict(Arc<RwLock<Vec<(String, Value)>>>)`
- `Value::Str(Arc<str>)`
- `Value::Bytes(Arc<[u8]>)`

Int/Float/Bool/Char/Byte 保持内联；TypeInst/Enum/Lambda/Future 已是 Arc 不动。

**语义不变式（本项目铁律的延伸）**：值语义 = 任何用户可见的变异效果与深拷贝模型逐字节一致。实现靠统一门控：

```
fn make_unique(v: &Arc<RwLock<T>>) -> &mut T
// 强计数 == 1 → 原地写（零拷贝）
// 否则 → 深拷贝内容、替换 Arc（隔离，等价自动 .copy()）
```

**变异站点审计（阶段 0 产出，写进代码注释清单）**——目前确认的写路径：

1. `a[i] = x`（VM `Instr::IndexSet` / interp `set_index_value`，含嵌套链 `m[i][j] = x`）；
2. interp 的 COW 写门控（现有 `cow_vars` 逻辑，泛化后成为所有容器变量的统一写屏障）；
3. 点号/内置变异方法（阶段 0 用 probe 脚本扫 `append`/`push` 族确认全量清单——`append` 当前是函数式返回新表，预期真正原地写很少）。

读端（索引读、for-in、keys/values、len、display、==、json）全部共享引用，无拷贝。

## 阶段划分

| 阶段 | 内容 | 验证 |
|---|---|---|
| **0. 审计** | 全量枚举三引擎变异站点 + `probe_align.py` 加 COW/别名专项形态（`b=a` 后嵌套写、循环内复用容器、lambda 捕获容器后变异） | 清单评审 |
| **1. Value 改造** | interp.rs：4 个变体换 Arc + `make_unique` 门控；`Value::clone` 退化为句柄克隆；所有 display/==/len/for-in 路径改走 Arc 读。删 `Value::Cow` 变体（内部化） | `cargo check` → 单测 |
| **2. interp 对齐** | 泛化 COW 写屏障到全部容器变量（不再需要 `cow` 声明才共享）；跑 regress3/err/4 全绿 | 三回归 + probe |
| **3. VM** | `regs[a].clone()` 自动变廉价；重点改 `IndexSet`/`Move`/调用实参路径——写点全部经 `make_unique`。**不加新指令**（ISA 不变，regress_ir 天然覆盖） | regress3/4/ir + probe |
| **4. nat** | runtime `HnCell` 增加同款 `make_unique`；`rt_binop/rt_index/rt_callv` 的 retain/drop 纪律按新模型重审（共享后大部分 rt_retain 可删）；`call_dotted` 镜像同步 | regress4 --nat + audit_err |
| **5. 废弃 cow** | parser/checker/interp 删 `cow` 声明（报错文案：语法错误，help 指引用普通赋值），fmt/LSP 同步 | audit_err + regress_err |
| **6. 收尾** | bench_ci.py 加一个 list 密集负载（体现收益）；`hone.md`/CHANGELOG/官网/README/AGENTS.md 同步 | bench 三语言对齐 + 全回归 |

## 同步点（AGENTS.md 铁律逐条过）

- **三引擎对齐**：错误文案不动（门控在写点，报错行为与现在一致）→ regress_err 应零 diff；
- **`__struct__` 过滤**：`Dict` 换 Arc 后所有过滤路径重审（display/len/keys/for-in/==/json）；
- **struct 实例/枚举**：已是 Arc，不受影响；`TypeInstVal` RwLock 语义不变；
- **线程**（`go`/async）：`Value` 需 `Send+Sync` → 用 `RwLock` 而非 `RefCell`；跨线程传参（`go` 实参）显式深拷贝，避免数据竞争，也正好保值语义；
- **natgen strtab/ISA**：无新指令，无需动；但 `rt_retain` 等签名纪律变化要在 AGENTS.md G2.4/G2.5 段落更新描述。

## 风险

- **R1 性能回退**：`RwLock` 原子操作对小 dict 可能比深拷贝慢（写多读少场景）。对策：阶段 6 bench 实测，必要时热点路径用 `unsafe` 去锁（有 Arc 独占保证）。
- **R2 别名泄漏**：任何漏改的变异站点会让共享缓冲被跨变量看见——唯一的正确性炸点，靠阶段 0 清单 + probe 专项形态兜底。
- **R3 cow 废弃是破坏性变更**：与「语义不变」有张力。changelog 标 BREAKING，`hone.md` 移除该节。
- **R4 构建约束**：dev profile 无调试符号的铁律不变（Cargo.toml `debug = 0`），全程 `cargo build --release` 验证。

## 验证矩阵

`cargo build --release` → `python tests/regress3.py` + `python tests/regress_err.py` + `python tests/regress4.py`（含 nat musl）+ `python tests/regress_ir.py` + `python tests/tools/probe_align.py`（新增别名专项）+ `python tests/bench_ci.py` 对照。

## 待拍板

- 阶段 5「废弃 cow」与本次一起做，还是拆独立 PR？（当前方案：一起做，changelog 标 BREAKING）
- 阶段 6 是否新增 list 密集 bench 负载？（会改 `bench/lua/`、`bench_ci.py`，触发三语言同步规则）

---

## 阶段 0 产出：变异站点审计清单（2026-10-21）

### 关键发现（改变方案工作量评估）

1. **nat runtime 已实现目标模型**：`runtime/src/lib.rs` 的 `cow_handle(h)`（rc==1 原地写 / rc>1 浅拷隔离）+ `rt_indexset` 无条件替换协议 + rc_alias 单测（rt_indexset 共享写 COW 拷贝、独占写原地 + retain 补引用守恒），阶段 4 工作量大幅缩小——主要是把 `Hnv` 的 Str/Bytes 也纳入句柄化（当前 runtime 仅 list/dict 有 T_LIST/T_DICT 句柄；str 不可变未过 COW 门）。
2. **容器没有点号方法**：`.m()` 语法是 type 实例方法专属（VM 报 H999「type 实例方法暂不支持」）；G2.9 点号内置是模块前缀全局函数（`time.parse` 等），非接收者方法。变异面比预想小得多。
3. **`copy`/`clone` 内置已存在**（`builtins.rs` 注册表），用户可见 .copy() 无需新增。
4. **`append` 是函数式**：返回新列表（`l = append(l, x)`），不原地写——三引擎一致。
5. **dict 索引写脚本层不可达**：checker 静态拒（读被拒在先，`set_index_value` 仅支持 List；VM `v_index_set` 虽有 Dict 臂但不可达）。dict 的 COW 写路径天然只有 runtime 内部使用。
6. **已有专项用例集**：`tests/smoke/rc_alias.test.hn`（G1.5，11 组 35 断言三引擎对齐）；本次新增 `tests/tools/probe_align.py` 的 `value` 组（14 例），全量 166 例零分歧。

### 变异站点全量清单（引用模型下需挂 `make_unique` 门控的位置）

| # | 引擎 | 站点 | 现状 | 阶段 |
|---|---|---|---|---|
| 1 | interp | `set_index_value`（`a[i] = x`，含嵌套链递归写回，3348 行） | 每层 clone 后写回基变量（值语义靠 clone） | 2 |
| 2 | interp | `assign_value` + `cow_vars` 写屏障（455 行） | 仅 cow 声明变量走 COW 包装/解包；泛化为全部容器 | 2 |
| 3 | interp | `set_field_value`（`Stmt::FieldAssign`，type 实例字段写） | `TypeInstVal.fields` 已是 RwLock 引用语义 | 不动 |
| 4 | interp | `isolate_cow` / `wrap_cow` / `unwrap_cow`（COW 三件套） | 阶段 1 随 `Value::Cow` 变体删除而内部化 | 1 |
| 5 | VM | `v_index_set`（3264 行，`IndexSet` 执行臂） | 直接 `l[idx] = val`（寄存器值已 clone，值语义靠编译臂 clone 链） | 3 |
| 6 | VM | 编译臂 `IndexSet` 链展开（613 行）+ 调用实参/`Move` 的 clone | 换 Arc 后自动变廉价，写点语义同 #5 | 3 |
| 7 | natgen | `Instr::IndexSet` lowering（741 行）→ `rt_indexset` | 已走 cow 门 + 无条件替换协议 | 不动 |
| 8 | runtime | `rt_indexset` / `cow_handle`（359/5357 行） | 已实现；Str/Bytes 句柄化后补门 | 4 |
| 9 | interp/VM | `go` 跨线程传参（interp 1956 行「按值克隆传入子线程」） | 显式深拷贝 | 保留深拷贝 |

### 读路径（共享引用，无门控，需重审 Arc 化）

display / len / keys / values / for-in / 推导式 / 解构 / `==` / json / 切片（interp 专属）——全部只读，Arc 化后走 `Arc` 读；`__struct__` 隐藏键过滤路径逐个过。

### 探针基线（阶段 0 验证）

`python tests/tools/probe_align.py` 全量 166 例（含新增 value 组 14 例）interp-vs-VM 零分歧；重构各阶段完成后本命令必须保持零分歧。
