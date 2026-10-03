# Hone 全项目优化方案清单

> 生成：2026-09-19 · 范围：全项目 · 维度：性能 / 严谨性 / GUI 体验 / 可维护性 · 允许激进重构
> 代码基线：commit `081b91f`（2026-08-30）＋ 3 周未提交工作区
> 本清单全部条目均经静态核实，标注 `file:line`。

---

## ⚠️ 第 7 轮修订（2026-09-19 深夜）：AOT 的判断被实测推翻

第 6 轮末尾我曾写「AOT 从未被基准验证，其存在理由没有证据支撑」，倾向移除。
**拿到可用 C 编译器后实测，这个判断是错的。** 详见独立文档 `AOT后端改造方案.md`。

| 原判断 | 实测结果 | 结论 |
|---|---|---|
| 「AOT 价值可疑，建议移除」 | `fib(25)`：**AOT 649ms / 解释器 1334ms**（2.1×）；<br>循环 2000 万次：**AOT 694ms / 解释器 36979ms**（**53×**） | ❌ **原判断错误**。AOT 是全项目性能最强的后端，接近 `rustc -O`（630ms，仅慢 10%）。移除等于主动放弃 53× 性能 |
| 「AOT 能力弱，与实际代码脱节」 | 成立，但性质不同：不是"少数边角不支持"，而是**标准库基本不可用**——核心内置白名单仅 41 个 | 部分成立。这是**待偿还的技术债**，不是移除的理由 |
| 「AOT 就是 `--dll` 那套」 | **两者零共享**：`aot.rs`(2836 行) 与 `codegen.rs`(1355 行) 各自实现 C 生成 | ❌ 原判断错误。改造边界需按两个独立后端划分 |
| 「AOT 依赖 ccache 包装导致无产物」 | 实为**自建 zig 包装器**（`Educe/.tools/bin/gcc.exe`，源码 `zig-wrap.c` 在同目录）；<br>根因是 zig 解析缓存目录失败（`AppDataDirUnavailable`） | 本机工具链问题，非 Hone 缺陷 |
| 「`cargo check` 后人肉确认即可」 | 差点因此误判——**`hone build --exe -c` 报 H999 时产物其实已生成**（`fib.exe` 59904 字节可运行） | 暴露 `run_cc_exe` 的真实缺陷（产物检查依赖相对路径），已列入方案批次 A |

**AOT 新增发现的问题清单：**（详见 `AOT后端改造方案.md` §4）
1. `run_cc_exe` 产物检查依赖相对路径 → 产物存在却报 H999（**真缺陷**）
2. 无超时、不捕获编译器 stderr → 编译器卡死会挂住 hone，失败时看不到真实报错
3. 核心内置仅 41 个 → `hone_lib` 的 `math_*`/`json`/`csv` 等**全部编不过**
4. `char` 类型不支持 → 用 char 的脚本全废（Hone 已有完整 char 体系，落差明显）
5. `bench/bench.sh` 缺 AOT 档位 → 性能优势长期不可见（正是我误判的根源）

---

## ⚠️ 第 6 轮修订（2026-09-19 晚）：实测数据推翻了多个原判断

Batch 1 已执行完毕，过程中用**实测**替换了前文的若干推测，以下更正**优先于后文所有相关表述**：

| 原判断 | 实测结果 | 结论 |
|---|---|---|
| 「234 条警告，`guimod_x11` 占 83%（194 条）」 | `cargo check --release` 实测 **35 条**；`guimod_x11.rs` **0 条** | ❌ 原判断错误。`build_errors.txt` 是 v0.7.0 的 `cargo build` 快照，与当前 `cargo check` 口径差 7 倍。**M1 的 cfg 修复收益是「少编译一个模块、缩短构建」，不是「消除警告」** |
| 「M1 是性价比最高的一项」 | 真实警告大头是 `src/aot.rs` **28 条**（25 条 `unused variable: span` + 3 条其他） | M1 降级为「代码正确性修复」；真正的清理项是 `aot.rs` 的 span |
| warn 中「6 处 unreachable pattern 可能是真 bug」 | 实测仅 **1 处**（`vm.rs:2343`），且**已确认是真缺陷**（见下） | 部分成立 |
| 「VM 侧 clone 是最大性能瓶颈」 | 未否认，但 **P1 基准不公正问题更优先**（VM 从未参与过基准） | 顺序调整 |

### 本轮已修复（Batch 1 实测成果）

| # | 内容 | 文件 |
|---|---|---|
| 1 | X11 后端补 `#[cfg(all(unix, not(target_os = "macos")))]` | `src/main.rs:17` |
| 2 | LSP 版本号改为 `env!("CARGO_PKG_VERSION")` 派生，根治漂移 | `src/lsp.rs:150` |
| 3 | **修掉一个真实功能缺陷**：`vm.rs:2343` 的 `TYPE_MISMATCH if m("field access")` 永远不可达（同码已在 2308 被无守卫分支吃掉），导致该 help 文案从未生效。已挪入 `if/else if` 链 | `src/vm.rs:2324-2329` |
| 4 | 消除 25 条 `unused span`（`cargo fix`）＋ `guimod.rs:1620` 无用 `mut`（手改）＋ `vm.rs` 两处未用绑定（手改） | `src/aot.rs`、`guimod.rs`、`parser.rs`、`vm.rs` |
| 5 | 修正 `hone.md` 3 处**幽灵错误码**：`H100`→`H301`、`H110`→`H404` | `hone.md:1140/1281/1286` |
| 6 | 修正 3 处「错误码 H001–H106」的范围错称 → `H001–H999` | `docs.html`、`language.html` |
| 7 | `print_help()` 补漏列的 `hone test` / `hone poop` | `src/main.rs:490-491` |
| 8 | README 删除 3 处已移除的 `hone upgrade` 声称 | `README.md:46/496/529` |
| 9 | 修正 `--keep-cache` 归属（是打包产物的**运行时**参数，非 `build --exe` 参数） | `docs.html:753` |
| 10 | **`sitemap.xml` 结构损坏修复**：第 11 行 `<url>` 未闭合导致整个 XML 无法解析，现 12 条全部合法 | `sitemap.xml` |
| 11 | AOT §4.6 补全完整不支持清单（原只列 http/crypto/sqlite/guipro+import/load/go，实缺 goto/char/async/await） | `hone.md:1245-1252`、`766-782` |
| 12 | **`upload_ftp.py` 加排除清单**：原 `os.listdir` 会把 `官网.zip`(250KB) 上传到网站根目录供人下载 | `upload_ftp.py:54` |
| 13 | **修好一个报废的官方标准库**：`hone_lib/process.hn:168` 的 `return {};` 无法解析（Hone 不支持空字典字面量）→ 整个 `process` 库在解释器下不可用。改用 `json_parse("{}")` | `hone_lib/process.hn:168` |
| 14 | 回归脚本归入 `tests/`，两个写死绝对路径的改为相对自身路径；旧 `.sh` 归入 `tests/legacy/` | `tests/` |
| 15 | `.gitignore` 补 `.workbuddy/`、`官网.zip`、`build_errors.txt`、`rt_check.ir`、缓存 | `.gitignore` |

### 本轮新发现的**语言级缺陷**（影响面大）

**① Hone 无法创建空字典 —— ✅ 已修复。**

- （原始症状）`a = {};` → `error[H005]: expected an expression, found '}'`；
  `a = dict();` → `error[H002]: undefined function 'dict'`；
  对照 `a = [];`（空列表）**正常**。即 list 有空字面量、dict 没有，**不对称**。
- 唯一变通是 `json_parse("{}")`——丑陋且慢。`hone_lib/process.hn:168` 已因此报废
  （整个 process 标准库在解释器下不可用，`test_process_lib` 直接报 H005）。
- **修复方式**：`src/parser.rs:2097` 在 `parse_primary_atom` 的 `Tok::LBrace` 分支里，
  解析字典字面量前先探一个 `}`，命中即返回 `Expr::DictLit(Vec::new(), span)`。
- **为什么安全**：语句起始的 `{` 走 `parser.rs:263` 的**另一条独立分支**（先探测
  `{a, b} = ...` 字典解构，否则按代码块解析）。两条路径互不干扰，
  `if (true) {}` 空代码块、`{a, b} = d;` 解构均未受影响（已实测验证）。
- **后端一致性**：四个后端对空 entries 天然安全，无需各自特判——
  解释器 → `Value::Dict(vec![])`；VM → `NEWDICT r? [..+0*2]`；
  AOT → `hn_dict_new()` 且无后续 `hn_dict_set`；checker → 空循环返回 `Ty::Unknown`。
- **验证**：`examples/empty_dict.hn` 含 7 组用例，解释器与 VM 输出逐字节一致；
  IR 往返一致；AOT 生成的 C 代码正确。`hone_lib/process.hn` 的绕过写法已还原为 `return {};`。

**② Hone 没有 `d["k"]` 字典索引语法 —— 未处理，待你决定。**

按键取值只能遍历（`hone_lib/gui.hn:11` 有明确注释记录这一限制）。
`{"k": 1}["k"]` 报错。这是与空字典同源的设计缺口：字典字面量有了，但读取侧仍缺最重要的一环。
补索引语法涉及 parser + checker + 三后端，属于语言级改动，不应顺手做——列在此处等你拍板。

### 关于回归基线的口径澄清

`regress3.py` 的 9 个「FAIL」经逐项复现，**全部为非逻辑差异，真实逻辑差异 = 0**，但归类需要更精确：

| 项 | 性质 | 处理 |
|---|---|---|
| `time_random`、`uuid_demo`、`alias_demo` | 读随机数/时间，同一后端两次运行都不同（`alias_demo` 实测第 3 行依次为 59/40/68） | 移入 `FLAKY` 集合 |
| `server_demo` | 随机端口 / 需启 HTTP 服务 | 移入 `FLAKY` |
| `test_hone_lib`、`test_img_lib`、`test_music_lib`、`test_sched_lib`、`test_process_lib` | **依赖 `import` 模块，VM 未覆盖**（`vm.rs:780` 明确 `VM: import 远程模块暂未支持`；`Hone虚拟机VM开发说明.md:678` 已如实记录）。属「按设计不支持」 | 移入 `SKIP` 集合 |
| `test_process_lib` | 曾因 `process.hn:168` 空字典缺陷**在解释器侧**也失败 | 已修复，现正常通过 |

**修正说明**：原脚本的 SKIP 集合漏掉了这 5 个 `test_*_lib`（文件名不匹配 SKIP 的 `guipro_`/`guide` 等规则），导致「设计性不支持」被误计为 FAIL，长期掩盖真实回归信号。已补入 SKIP 并加注释。

**追加发现（本轮）：`smoke_spider` 与 `regress_ir.py` 的整套漂移。**

- `smoke_spider` 同样 `import spider.hn`（与已在 SKIP 的 `spider_demo` 同类），但当初漏列，
  于是长期被报为 FAIL——掩盖真信号的手法与上面 5 个完全一致。
- `regress_ir.py` 的 SKIP 集合是 `regress3.py` 的**残缺副本**（少了全部 import 依赖项，
  也没有 FLAKY 概念），导致 `OK=39 BAD=9` 里的 9 个 BAD 全是误报：
  `smoke_spider` + 5 个 `test_*_lib`（import 依赖，实为 SKIP）+
  `alias_demo`/`server_demo`/`time_random`/`uuid_demo`（随机时间类，实为 FLAKY）。
- **根因**：同一份分类知识在两个脚本里各写一遍，必然漂移。
- **处理**：抽出 `tests/regress_common.py` 作为唯一事实来源（`SKIP` / `FLAKY` / `should_skip()`），
  两个脚本都从它导入。同时给 `regress_ir.py` 补上 FLAKY 分支与 `TimeoutExpired` 的解码兜底
  （原先直接拼 `e.stdout`，在 `text=True` 下超时输出可能是 `None` 或 bytes 而抛异常）。

### 一类反复绊倒排查的陷阱：陈旧产物

本轮**两次**因为读了过期的本地产物而得出错误结论，值得单列：

| 产物 | 症状 | 正确做法 |
|---|---|---|
| `build_errors.txt` | v0.7.0 时期的 `cargo build` 快照，行号与当前代码完全对不上（称 `checker.rs:3889` 是 `StructDef`，实际是 `Export`），据此误判出「234 条警告，X11 占 83%」 | 真实数字必须现场 `cargo check --release` 测（实测 35 条，X11 模块 0 条） |
| `target/release/hone.exe` | `cargo check --release` 只做检查、**不产出可执行文件**；旧二进制静默沿用，导致「空字典已修复」的改动在 release 上看起来没生效 | 验证任何行为前先 `cargo build [--release]` |

已在 `README.md` 的构建章节补上显式提醒，并说明回归脚本依赖的是 `target/debug`。

### 本轮回归验收

| 回归 | 本轮结果 | 历史基线（09-13 记录） | 判定 |
|---|---|---|---|
| `tests/regress3.py` | **PASS=47 FLAKY=4 FAIL=0** | PASS=47 FAIL=9 SKIP=16 | ✅ 真实逻辑差异 = 0 |
| `tests/regress_err.py` | **PASS=37 FAIL=0** | PASS=37 FAIL=0 | ✅ 完全一致（证明 `vm_help_for` 改动无破坏） |
| `tests/regress_ir.py` | **OK=38 FLAKY=5 BAD=0** | OK=39 BAD=9 SKIP=9 | ✅ 原 9 个 BAD 全系误报，修正分类后归零 |
| `cargo check --release` | **0 warning / 0 error** | 35 warnings | ✅ 清零 |

`regress3.py` 的 9 项 FAIL 之所以能降到 0，是因为它们**全部是设计性差异或随机差异**（见上表分类），
并非本轮修复了逻辑 bug——除 `test_process_lib` 一项确实是被 `process.hn` 的空字典缺陷带崩的，本轮已修好。

### 本轮尚未触碰的部分（需你拍板后继续）

| 批次 | 内容 | 阻塞于 |
|---|---|---|
| **P0-1 提交** | 3 周成果 + 本轮修复共 20 余文件仍未提交 | 你未定提交粒度 |
| **P0-2 凭据** | 你已说「不上传」，但**未定是否加固**（轮换 / 改读环境变量） | 你的决定 |
| Batch 2 | P1 基准公正化（补 `--vm` 数据、拆 release 档位、等算法重测） | 需重跑基准，耗时较长 |
| Batch 3+ | P2 Value 瘦身 + Rc/COW、M2 抽共享层、M3 拆分大文件 | 需先有提交作为回滚点 |
| Batch 5 | GUI 交互（Tab/Enter/Esc、DPI、缩放重排） | 同上 |
| 语言级缺陷 | 空字典字面量 `{}` / `dict()` | 建议单独立项，改动 parser 或 builtins |

---

## 0. 结论先行（以下为 Batch 1 执行前的原始分析，收益评级请对照上方修订表）

### 如果只做五件事（按投入产出比排序）

| 序 | 事项 | 一句话理由 |
|---|---|---|
| 1 | **P0-1 先落盘** | 320KB 全新成果（`vm.rs`/`preproc.rs`/`language.html`）从未进版本控制，重构中丢失不可恢复 |
| 2 | **P0-2 凭据处置** | `ftp.txt` 明文 FTP 密码、`token.txt` 明文 GitHub PAT，躺在本机磁盘上 |
| 3 | **M1 X11 后端 cfg 门控** | 一行修复，消除全项目约 **83%** 的编译警告 |
| 4 | **P2-1 Value 瘦身 + Rc/COW** | 同时解决「`Value` 144 字节到处深拷贝」和「`append` 是 O(n²)」两个根因 |
| 5 | **C1–C3 幽灵版本号/错误码** | `lsp.rs` 报 0.7.0、`hone.md` 写不存在的 `H100`/`H110`，是纯事实性错误 |

### 对先前判断的两处更正（我自己核过）

- 我之前引用的「234 条编译警告」来自 `build_errors.txt`，**该文件是 v0.7.0 时期的旧快照、已过期**：其行号与当前代码对不上（它称 `checker.rs:3889` 是 `Stmt::StructDef`，实际该行是 `Stmt::Export`；它报告的 6 处 unreachable pattern 在当前代码中已消失）。真实警告数需重新 `cargo check` 测定。**但**「`guimod_x11.rs` 在 Windows 下仍被编译」这一根因经核实成立，它是警告的主体。
- 官网实际是 **12 个 HTML 页面**（非 15）；我原以为 `官网.zip` 落后，核实后**它是最新的**（内部 16 条目与当前 `官网/` 逐一相同），只是可再生冗余。

---

## 1. 前置阻塞项 P0 —— 建议在动任何代码之前完成

| ID | 问题 | 证据 | 建议 | 风险 |
|---|---|---|---|---|
| **P0-1** | 3 周成果未提交 | 最后提交 `081b91f`（08-30）；`git status` 显示 `src/vm.rs` 152KB、`src/preproc.rs` 54KB、`官网/language.html` 120KB、`Hone虚拟机VM开发说明.md` 43KB、3 个 regress 脚本、`examples/goto_demo.hn`/`macro_demo.hn` 全部 untracked；另有 12 个核心源文件 modified | 分两次提交：① **现状快照**（原样落盘，一个字节都不改）② 之后的优化各自独立提交 | 不做则无回滚点。重构将大面积改 `checker/interp/aot`，届时无法用 `git diff` 验证「行为不变」 |
| **P0-2** | 明文凭据 | `ftp.txt:1-8`：主机 `ftpupload.net:21`、账号 `if0_42562465`、密码明文；`token.txt:1-3`：`github_pat_…` 明文 | `.gitignore:8-9` 已忽略，未泄露到仓库——但**磁盘上仍是明文**。建议改用环境变量或系统凭据管理器，并轮换这两个密钥 | 高危。我未打开修改这两个文件，需你决定处置方式 |
| **P0-3** | 无权威基线 | `build_errors.txt` 过期；`rt_check.ir` 是 `examples/vars.hn` 的旧 disasm | 重新生成三份基线：`cargo check` 警告全量、`regress3.py` / `regress_err.py` / `regress_ir.py` 结果 | 无基线则「优化后是否变好」无法判断 |

---

## 2. 维度一：可维护性 M

### M1 · X11 后端未做 cfg 门控 〔工作量 S · 收益极高 · 风险极低〕

- **问题**：`src/main.rs:17` 是裸 `mod guimod_x11;`，**无任何 `#[cfg]`**。对照 `src/main.rs:15-16` 的 `#[cfg(not(windows))] mod guimod_gtk;` 是正确写法。`src/guimod.rs:119-134` 只在 `#[cfg(not(windows))] mod platform` 内引用 x11，Windows 分支（`guimod.rs:138`）从不引用它。
- **后果**：103KB 的 X11 后端在 Windows 上被完整编译，全部 item 不可达 → 约 **194 条 dead_code 警告**（占全项目警告约 83%），并拖慢构建。
- **改法**：`#[cfg(all(unix, not(target_os = "macos")))] mod guimod_x11;`，与引用处 cfg 保持一致。
- **注意**：`guimod_gtk.rs` 已有 cfg，无此问题。

### M2 · 抽共享层，消灭 2~4 份重复语义 〔工作量 L · 收益高 · 风险中〕

| 重复项 | 份数与位置 | 备注 |
|---|---|---|
| `zerr(...)` 错误构造 | **18 份**：模块级 15 份（`textmod.rs:16`、`sysutilmod.rs:22`、`guimod_x11.rs:35`、`guimod_gtk.rs:40`、`sqlitemod.rs:29`、`guimod.rs:39`、`pluginmod.rs:24`、`ptrmod.rs:42`、`plotmod.rs:16`、`sysmod.rs:16`、`datamod.rs:14`、`statmod.rs:23`、`archmod.rs:23`、`netmod.rs:22`、`srvmod.rs:44`）＋ `aot.rs:1244` ＋ 方法版 `checker.rs:3860`、`codegen.rs:657` | |
| `as_str` / `as_int` | 12 份（上述 mod 的 `as_str:20`/`as_int:2x` 系列 ＋ `builtins.rs:127/146/165` 带 `name` 变体） | 签名逐字相同 |
| `dict_get/dict_str/dict_int/arg_count` | 3 份：`guimod.rs:43/57/73/81/96/1161`、`guimod_gtk.rs:44/58/74/82/97/427`、`guimod_x11.rs:39/53/69/77/92/2673` | 六个函数签名完全一致 |
| `stmt_span` | 2 份：`checker.rs:3865` 与 `codegen.rs:1319` 完全重复 | 应上移到 `ast.rs` 的 `impl Stmt` |
| 运算符分派 | 各写一份：`checker.rs:2025`、`interp.rs:2622`、`vm.rs:1593/2437-2547`、`aot.rs:1423/2344`、`codegen.rs:590/1183` | `BinOp::Add` 在 src 出现 24 次 |

- **改法**：新增 `src/value.rs`（`Value` + `type_name` + `display`，**以解释器版为权威**）与 `src/errhelp.rs`（`zerr`/`as_str`/`as_int`/`dict_*`/`arg_count`）。`src/preproc.rs:3-5` 已经示范了「唯一权威实现点」的写法，照搬即可。
- **⚠️ 顺手修一个真实缺陷**：`interp.rs:175` 的 `display()` 与 `vm.rs:2377` 的 `value_to_str()` 几乎逐行相同，**但已漂移**——`Ptr` 在解释器输出 `0x{:x}`、在 VM 输出 `ptr({})`；`Lambda` 在解释器输出 `fn(params)`、在 VM 输出 `<lambda>`。即同一脚本 `hone run` 与 `hone run --vm` 打印不同。这是**真实的双后端不一致 bug**，不只是代码风格问题。
- **风险**：改动面覆盖几乎所有 mod。必须先有 P0-3 基线。

### M3 · 拆分超大文件 〔工作量 XL · 收益中 · 风险中〕

| 文件 | 体积 / `fn` 数 | 建议拆分边界 |
|---|---|---|
| `checker.rs` | 174.9KB / 54 | `Ty` 定义 20-117 → `ty.rs`；`builtin_result` 3015-3858 → `builtins_ty.rs`；主检查 248-3010 → `stmt/` `expr/` |
| `vm.rs` | 148.6KB / 95 | 类型 35-115 → `instr.rs`；`impl Compiler` 141-1387 → `compile.rs`；`impl Vm` 1416-2278 → `exec.rs`；`v_*` 2411-2888 → `ops.rs`；IR 文本 2950-3860 → `textir.rs` |
| `interp.rs` | 120.9KB / 62 | `Value` 156-209 → `value.rs`；运算符 2622-2800 → `ops.rs` |
| `guimod_x11.rs` | 100.7KB / 85 | 按「API 加载 / 绘制 / 事件 / 控件命令」四分 |
| `builtins.rs` | 122.8KB / 40 | 按命名空间切目录 |
| `main.rs` | 66.7KB / 38 | 命令分派 → `cli/` |

- **可行性**：单 bin crate、无 `lib.rs`；`impl` 块可跨文件分块，但需把 `Checker`/`Vm`/`Compiler` 的私有字段放宽为 `pub(crate)`。无公共 API 破坏，成本可控。
- **建议**：不要一次性全拆。**先拆 `vm.rs` 与 `interp.rs`**（性能优化必然要动它们，先拆后优化更省事），其余按需。

### M4 · 收编散落脚本与产物 〔工作量 S · 收益中 · 风险低〕

`build_errors.txt`（过期日志）、`rt_check.ir`（过期 disasm）、`regress.sh` / `regress2.sh`（已被 3 个 `.py` 取代，且写死 `D:\` 路径）、`Hone exe打包方案.md` / `Hone错误处理与自动恢复机制增强方案.md`（08-08 设计稿）→ 分别删除或移入 `tests/`、`scripts/`、`docs/`。`官网/官网.zip` 是可再生的 250KB 冗余，建议不入库。

---

## 3. 维度二：性能 P

### P1 · 基准本身的公正性 〔工作量 S · 收益高（可信度）· 风险低〕

这是**必须先修的一环**：不修，后续所有性能优化都无法证伪。

| 问题 | 证据 | 说明 |
|---|---|---|
| 基准从未跑 VM | `bench/bench.sh:44` 直接 `$HONE $hname.hn`，走默认 AST 解释器（`src/main.rs:292-296`） | 花了 3 周做的字节码 VM **完全没有参与性能宣称**，`perf.html` 反映的是最慢后端 |
| 构建档位不对等 | `Cargo.toml:16` `opt-level = "z"`（**体积优先**，另有 `lto=true`、`panic="abort"`、`codegen-units=1`）；`perf.html:51` 写 Rust 用 `-O` / opt-level 3 | 是「刻意压体积的 Hone」对比「速度优化的 Rust」。`perf.html:49` 已如实披露 Hone 的档位，但没点明这是性能上的自我惩罚 |
| 算法不对等 | `perf.html:79` list_append 169x、`:82` sort 162x；`:60` 自述 Hone 侧是 O(n²) 负载 | Hone 用冒泡/整表拷贝，Python/Rust 用内置排序。`perf.html:60` 有披露，但汇总结论没区分「语言慢」和「算法不同」 |
| 遗漏场景 | `bench/alias_test.hn` 未列入 `bench/bench.sh:16` 的 `BENCHES` | 别名/共享语义是 Hone 的特色，反而没测 |

- **改法**：① 补 `--vm` 一组数据；② 把 release profile 分档（`release` 用 `opt-level=3`，另设 `release-size` 保留 `z`），重跑；③ 给 list_append/sort 换等算法版本（list 预分配、sort 用同复杂度算法）另出一行；④ 页面上把「语言速度」与「算法代价」分开陈述。

### P2 · 值语义表示与克隆开销 〔工作量 XL · 收益最高 · 风险高〕

- **根因**：`src/interp.rs:59-82` 无任何 `Rc`/`Arc`：`Str(String)`、`List(Vec<Value>)`、`Dict(Vec<(String,Value)>)` 全深拷贝；`Error(ErrorObj)` 内联（`interp.rs:21-30`，约 136B）→ **`Value` 尺寸约 144 字节**，每次 `clone` 复制整块。
- **热点**：
  - `src/vm.rs:1617-1618` 每条算术指令 clone 两个操作数，`:1622` 再 clone 一次（字符串拼接即双重 O(n) 拷贝）
  - `src/vm.rs:1535-1536` 每条指令 clone 一次 `Instr` + 一次 `Span`
  - `src/interp.rs:2256` 每次读变量都 clone
  - `src/interp.rs:2738/2795` 字符串加法重建整串
- **交叉影响**（最值得优先修的点）：`src/builtins.rs:421-424` 的 `append` 是值语义整表拷贝 → O(n²)；VM 侧因 `v_add(x.clone(), y.clone())` 无法复用字符串缓冲，在 str 场景同样退化为 O(n²)。**这正好解释了 `perf.html` 上最难看的那两项（169x / 162x）。**
- **改法**：`Str(Rc<str>)`、`List/Dict` 改 `Rc` + 写时复制（COW，保持现有值语义不变）、`Error(Arc<ErrorObj>)`，`Value` 可压到约 24B。预估 1.5–3x。
- **风险**：COW 语义散落在 `builtins.rs` / `vm.rs` / `interp.rs` 三处，改动面大，必须三后端同步并靠 `regress3.py` 守行为。

### P3 · VM 执行循环 〔工作量 L · 收益中高 · 风险中〕

- `src/vm.rs:1527-1573` 是 `loop` + 大 `match`（非函数指针表）；每轮 `code[pc].clone()`(1535)、`spans[pc].clone()`(1536)；带 `String` 的 Call/Field/Label 指令每条都堆分配。
- 寄存器为 `Vec<Value>`(1388-1392)，每次调用 `vec![Value::Null; nregs]`(2135) 重新分配。
- 变量名**已在编译期落地**（这点做得好），但函数调用仍是运行期链：`resolve_alias` 分配 `String`(2013)，随后 locals/async_fns/func_map 多次 `String` 哈希（2045-2087）。
- **改法**：取指改借用（或 `Rc<[Instr]>` + 索引操作数）；寄存器改可复用 arena；`Call` 在编译期绑定 chunk 直调。预估 2–4x。
- **澄清**：`vm.rs:202` 的 `jit` 字段只是「跳转若真」的编译辅助，**项目并没有 JIT 引擎**——若官网有暗示 JIT 的措辞需一并修正。

### P4 · 编译期开销 〔工作量 M · 收益中 · 风险中〕

- `src/checker.rs:142-158`：Phase A 全树注册，Phase B **不动点整树 `check_all` 最多 32 轮**（147-154），再 strict 整树一次（158）。`preproc` 另有 `top_level_names`(91) 与 `validate_goto`(84) 两次独立遍历。
- **改法**：不动点改 worklist，只重查受影响函数。预估省 20–40% 编译时间。风险：收敛判定变复杂。
- 另核实：`--vm` / `--disasm` / `watch` **无重复编译**（`main.rs:289-297`），这块干净。

### P5 · 标准库 O(n²) 热点 〔工作量 M · 收益高（特定场景）· 风险低〕

| 位置 | 问题 |
|---|---|
| `hone_lib/img.hn:31-33` | `img_range` 用 `append` 构表 O(n²)，却在 `:263/269/275` 的 h 层推导式里反复调用 → **O(h·w²)**。这是图像路径上最严重的 |
| `hone_lib/pet.hn:39-41`、`:316`、`:584` | 同上模式 |
| `hone_lib/str.hn:6-14`（`str_repeat`）、`:22-34`（`str_join`） | 循环 `r = r + s` O(n²) |
| `hone_lib/gui.hn:78-106` | 顺序约 30 次字符串拼接，`:88-89` 在循环内再拼 HTML → O(n²) |
| `hone_lib/guipro.hn:302-324`、`:366-383` | 循环 `append` |
| `hone_lib/collections.hn:42-48` | `coll_unique` 用线性 `contains`（`builtins.rs:446`）+ 整表 `append` → O(n²) |
| `hone_lib/music.hn:144/186/327`、`spider.hn:127-147`、`:184` | 循环 `append` |

- **改法**：列表加 builder / 原地 push，或让 `append` 走 COW（与 P2 是同一个改动）；`img_range` 结果缓存。图像/桌宠场景可达数量级提升。

### P6 · 执行后端收敛 〔工作量 XL · 战略项〕

仓库只有「双后端语义一致」的说明（`Hone虚拟机VM开发说明.md:15-21`、`:518`），**没有任何后端速度对比数据**。判断：长期应把执行统一收敛到 VM（单后端更易优化），但**必须等 P2 / P3 修完**，否则 VM 因更重的 clone 未必比解释器快，收敛反而亏。此项建议排在最后。

---

## 4. 维度三：严谨性与一致性 C

### C1 · 版本号漏改 〔工作量 S · 风险低〕

版本矩阵已逐项核对，**除一处外全部为 0.7.11，一致**：`Cargo.toml:3` ✅、`Cargo.lock:478-479` ✅、`main.rs:43`+`:124`（`env!("CARGO_PKG_VERSION")`，自动派生不会漂）✅、`README.md:6` ✅、`hone.md:1` ✅、`CHANGELOG.md:3` ✅、官网 12 个页面顶栏/页脚 ✅、`download.html:104`/`install.html:66` 的 `HONE_VERSION` ✅。
（`CHANGELOG.md:41-184`、`changelog.html:81-270` 的 v0.7.0–v0.7.10 是历史条目，**应保留不动**。）

- **唯一缺陷**：`src/lsp.rs:150` → `"serverInfo": { "name": "hone-lsp", "version": "0.7.0" }`，硬编码，漏改。改为从 `env!("CARGO_PKG_VERSION")` 派生即可根治。

### C2 · 幽灵错误码 〔工作量 S · 风险低〕

权威全集（`src/error.rs:115-158`）：`H001–H012`、`H101–H106`、`H200–H204`、`H300–H306`、`H401–H404`、`H600`、`H700`、`H999`。

| 位置 | 文档写的 | 事实 |
|---|---|---|
| `hone.md:1281` | `· H100：动态库加载失败` | ❌ **`H100` 不存在**。`error.rs:132` 只是注释「H100 区段」，真实码是 `H101–H106`。动态库加载失败应为 `H301` |
| `hone.md:1140` | `找不到路径或符号时抛出 error[H404] / error[H100]` | ❌ 幽灵码同上 |
| `hone.md:1286` | `· H110：懒加载依赖函数未找到` | ❌ 全库无 `H110` |
| `language.html:7`、`:67`、`docs.html:48` | 「错误码 H001–H106」 | ❌ 以偏概全，实际覆盖到 `H999` |

（`hone.md:1267` 自标「部分示例」，故未列全可接受。）

### C3 · README 与帮助的陈旧项 〔工作量 S · 风险低〕

权威 = `main.rs:461-494` 的 `print_help()`。

| 问题 | 证据 |
|---|---|
| README 声称 `hone upgrade` 已实现 | `README.md:46`、`:497-498`、`:530` 标「已实现 ✅」，但 `CHANGELOG.md:356`、`changelog.html:509` 明确**已移除**，且 `main.rs:118-205` 无该分支 → 应删 |
| 帮助漏列 `hone test` | 程序支持（`main.rs:196`），`docs.html:756`/`hone.md:636`/`index.html:114` 都有，`print_help()` 无 |
| 帮助漏列 `hone poop` | 程序支持（`main.rs:204`），`README.md:51`/`hone.md:650`/`docs.html:765` 都有，`print_help()` 无 |
| `--keep-cache` 归属错误 | `docs.html:753` 把它写在 `hone build --exe` 行；实际它是打包后 exe 的**运行时**参数（`src/bundle.rs:206`）；`hone build --exe` 只认 `-c/--keep-c/-o/--icon/--version`（`main.rs:667-685`） |
| README 其他缺项 | 缺 `hone doc`/`bind`/`build --script`/`self-update`；`README.md:38` 写 `fmt [-w]`，实际 `main.rs:476` 为 `[-w\|-c]` |
| （可忽略） | `hone runir`（`main.rs:162`）为内部命令，全部文档未列，无需补 |

### C4 · AOT 能力声称过宽 〔工作量 S · 风险低〕

- `hone.md:1245` 称 AOT 支持「**全语言特性**」；`hone.md:1247`（§4.6）只列了 http/crypto/sqlite/guipro + import/load/go 不支持。
- 但 `src/aot.rs` 实际返回 `H999` 的位置另有：`goto`（`:1666`）、`char` 字面量/声明（`:2215`、`:1698`）、`async fn`（`:1983`）、`await`（`:2529`）、未知函数（`:2745`）。
- **改法**：§4.6 补 **goto / char / async / await**，并把「全语言特性」改为可核查的具体清单；`hone.md:768-779` 的 char 节也应标注 AOT 限制。

### C5 · `sitemap.xml` 结构损坏 〔工作量 S · 风险低〕

- 第 9-20 行：`docs` 的 `<url>` **未闭合**，内嵌了 language 的 `<url>`，并存在孤立 `</url>`。搜索引擎会解析失败。
- `lastmod` 多为 8 月，滞后于页面的 v0.7.11 改版。

### C6 · 网站周边文件 〔工作量 S · 风险低〕

`官网/官网.zip` 已核实与当前 `官网/` **一致**（非过期），但是可再生冗余，建议 `.gitignore` 或打包脚本化；`robots.txt`、`favicon.png` 未见异常。

---

## 5. 维度四：GUI 外观与交互 G

### G1 · 两套 GUI 的分工（先定战略）〔决策项〕

| 控件 | `gui.hn`（浏览器） | guipro / Win32 | guipro / GTK | guipro / X11 |
|---|---|---|---|---|
| button / label / input / select | 有 | 有 | 有 | 有 |
| checkbox / radio | ❌ | 有 | 有 | 有 |
| slider | ❌ | 有 | ❌ | 有 |
| table / tree / canvas | ❌ | 有 | ❌ | 有 |
| menu / tray / msgbox | ❌ | menu/tray/msgbox | ❌ | menu/tray |
| pet 桌宠 | ❌ | 有 | ❌ | ❌ |

证据：`guimod.rs:736-745`；`guimod_gtk.rs:402-415`（进阶控件直接报 `NOT_IMPLEMENTED`）。

- **建议明确分工**：`gui.hn` 定位「零依赖、跨平台的富文本与表单」（HTML 模板硬编码在 `gui.hn:43-72`，仅 5 种控件，事件统一 `on_event(id,value)` → JSON，`gui.hn:131-154`）；`guipro` 定位「原生桌面」（事件为轮询 JSON + 函数注册表，`guipro.hn:332-421`）。**不要把两套接口硬统一**——坐标与事件模型不兼容，强行合并是负收益。文档层面统一控件命名即可。

### G2 · Windows 后端交互缺陷 〔工作量 M · 收益高 · 风险低〕

| 问题 | 证据 | 影响 |
|---|---|---|
| **Tab 焦点实际不生效** | `guimod.rs:736-745` 设了 `WS_TABSTOP`，但消息泵只做 `TranslateMessage`/`DispatchMessage`，**全仓无 `IsDialogMessage`** | 纯键盘操作不可能 |
| 无 Enter 提交 / 无 Esc 关闭 | 同上 | |
| 窗口缩放不重排 | `guimod.rs:465-471` 的 `WM_SIZE` 只推事件、不调 `MoveWindow` | 绝对像素布局，拉伸后控件错位 |
| 双击 / 右键支持面窄 | 双击仅桌宠与表格（`:606`、`:433`），右键菜单仅桌宠（`:616`） | |

### G3 · 外观现代化 〔工作量 L · 收益中高 · 风险中〕

- **无 DPI 感知**：全仓 grep 无 `SetProcessDpiAwareness` / `WM_DPICHANGED` → 高分屏模糊。这是最容易感知的体验缺陷。
- 字体固定 `DEFAULT_GUI_FONT`（`guimod.rs:775`），背景 `COLOR_BTNFACE`（`:678`）。
- 无 `WM_CTLCOLOR` / `WM_DRAWITEM` → 无悬停态、焦点态、禁用态定制；无深色模式；无高对比度适配。
- 自绘部分常量硬编码：品红 `0x00FF00FF`（`:1593`）、白 `0xFFFFFF`（`:1619`）、气泡高 `30`（`:1591`）、字体 `-14px Microsoft YaHei`（`:1625-1628`）；`draw_shapes` 只用 `Rectangle`/`Ellipse`（`:1503-1508`）→ **全直角、无圆角/阴影**。
- X11 自绘更素：灰底直角按钮（`guimod_x11.rs:852-863`），硬编码色 `:541-545`，字体写死 `fixed`（`:640-656`）。
- `guipro.hn` **完全没有样式 API**（仅 canvas 有颜色，`:149-161`）。

### G4 · 跨平台一致性 〔工作量 XL · 收益高 · 风险高〕

同一份 guipro 程序三平台表现明显不同：

1. GTK 仅 7 个命令，table/tree/canvas/tray/menu/slider 全部 `NOT_IMPLEMENTED`（`guimod_gtk.rs:402-415`）。
2. **布局模型冲突**：GTK 把控件 pack 进垂直 box 并**忽略 x/y**（`guimod_gtk.rs:493`），而 Win32/X11 用绝对坐标（`guimod.rs:730-733`）→ 同一程序界面结构直接变形。
3. 外观来源不同：GTK 用系统主题、Win32 用系统控件、X11 自绘灰白直角，三者观感割裂。

### G5 · X11 输入能力 〔工作量 L · 风险高〕

键盘仅处理可打印字符与 Backspace/Delete（`guimod_x11.rs:1822-1835`）——**无方向键、无 Home/End、无选区、无剪贴板、无输入法**；焦点靠点击（`:1620`）、无 Tab；滚轮仅 table/tree（`:1667-1688`）且**不画滚动条**；缩放不重排（`:1842-1852`）。

### G6 · 示例与编辑器 〔工作量 M · 收益中 · 风险低〕

- **`examples/gui_demo.hn` 作为官方示例不合格**：仅 7 个控件（`:28-36`），事件是 `if (id == "...")` 字符串长链（`:8-25`），返回值手拼 JSON；无布局分组、无样式、无错误处理、无多窗口，完全未展示 slider/table/tree/canvas/menu/tray。
  - `:38` 的 `db.set("count","0")` **脆弱**：`db` 是进程级全局 `KV_STORE`（`builtins.rs:1586-1614`），`db.get` 未命中返回 `Null`（`:1613`）→ `to_int(Null)` 有风险；键名 `count` 易与其他库冲突；且 `--resume` 下每次点击都写盘（`:1594`）。建议改用闭包/返回值保存状态。
- **`editor/index.html`（57KB）**：是独立单文件 HTML + 原生 JS（`:1`、`:231`），**不与 hone 二进制通信**——「▶ hone run」只是复制命令行文本（`:1238-1242`），有误导性。完成度中等（三栏、拖拽、撤销、深浅主题 CSS 变量 `:9-28`、窄屏 `@media` 抽屉 `:177-185`、localStorage `:912-914`）；但**无任何 `aria`/`role`/`tabindex`**（grep 零命中），可访问性差。

---

## 6. 建议执行批次

| 批次 | 内容 | 为什么这个顺序 |
|---|---|---|
| **Batch 0** | P0-1 落盘 ＋ P0-2 凭据 ＋ P0-3 基线 | 无回滚点、无基线，后面全是赌博 |
| **Batch 1**（低风险高收益，可当天见效） | M1（x11 cfg）· C1 · C2 · C3 · C4 · C5 · M4 | 全是事实性修正与一行修复，不改任何行为，改完立刻可验证 |
| **Batch 2** | P1（基准公正化）＋ 重跑 `perf.html` | 必须先让性能可证伪，才能判断 Batch 3 是否真的有用 |
| **Batch 3** | M2（抽共享层，**含修 interp/vm 的 `display` 漂移 bug**） | 为 Batch 4 铺路；顺手修掉一个真实双后端不一致缺陷 |
| **Batch 4** | P2（Value 瘦身 + Rc/COW）＋ M3 拆分 `vm.rs`/`interp.rs` ＋ P3 ＋ P5 | 收益最大的核心改动，靠 `regress3.py` 守行为不变 |
| **Batch 5** | G2（Tab/Enter/Esc + DPI + 缩放重排）· G3 · G6 | 交互缺陷性价比高，先修能立刻感知的 |
| **Batch 6** | G4 · G5 · P4 · P6 | 跨平台一致性与后端收敛属战略级，改动面最大，放最后 |

---

## 7. 需要你拍板的开放问题

1. **P0-2 凭据**：`ftp.txt` / `token.txt` 要我怎么处理？（删除 / 改成读环境变量 / 你手动迁移后我来改脚本）
2. **P0-1 提交粒度**：现状快照用一个 commit 全量落盘，还是拆成「VM 相关」「goto/宏相关」「官网相关」三个？
3. **P1 release 档位**：把 `opt-level = "z"` 改成 `3` 会增大二进制。是要「性能优先」还是「保留体积档、另设 `release-size`」？
4. **M3 拆分范围**：全拆（6 个大文件）还是只拆 `vm.rs` + `interp.rs`？
5. **G4 跨平台策略**：GTK 后端是**补齐**到与 Win32 同级，还是**明确定位为「基础控件够用」并在文档写清能力矩阵**？（后者成本低得多）
6. **G6 编辑器**：「▶ hone run」按钮是真接上后端（需要 LSP/HTTP 通道），还是改成诚实的「复制命令」文案？
7. **不要动的部分**：`CHANGELOG.md` / `changelog.html` 里的历史版本条目（v0.7.0–v0.7.10）我建议一律不动——确认吗？
