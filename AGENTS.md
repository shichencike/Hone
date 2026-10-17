# AGENTS.md — Hone 项目代理指南

Hone：Rust 实现的脚本语言（单一可执行 `hone`）。主 crate 在 `src/`；`hone_lib/` 是 Hone 标准库（.hn 脚本，非 Rust）。

## 构建与测试

- `cargo check` 只查类型、不产二进制；**改代码后验证行为必须先 `cargo build`**，`target/` 旧二进制会静默沿用。
- 回归脚本依赖 `target/debug/hone.exe`，先 build 再跑：
  - `python tests/regress3.py` — 解释器 vs VM 输出逐字节对齐
  - `python tests/regress_err.py` — 三引擎（interp/VM/nat）错误诊断逐字符对齐（行/列/文案/help）
  - `python tests/regress4.py` — 三引擎正常路径逐字节对齐（需本机 rustc）；nat 目标：Linux 侧首选 musl 交叉目标（需 `rustup target add x86_64-unknown-linux-musl`，未装则用宿主目标），首选目标编译失败时退回宿主目标重试并打印 `FALLBACK`，不静默。
  - `python tests/regress_ir.py` — 文本 IR 往返
  - `python tests/tools/audit_err.py` — 开发期扫雷：55 例错误文案 + help 行 nat-vs-interp 批量比对（慢，不入常规回归）
  - `python tests/tools/probe_align.py` — 开发期扫雷：152 例 match / 条件 / 逻辑 / 解构 / 重载 / alias+goto / 默认参数 / 点号内置 / async+go 形态的 interp-vs-VM 逐字节比对；`--nat` 追加第三引擎（每例单独 build，慢）。动态类型值（enum/struct）下的条件与逻辑分叉、以及重载回退边界靠它扫出。
- CI（.github/workflows/ci.yml）：ubuntu/windows 两个 test job 做 `cargo build --locked` + `cargo test --locked` + hello/fib smoke + `tests/regress4.py`（Linux job 先 `rustup target add x86_64-unknown-linux-musl`，供 nat 回归的 musl 目标）；无 rustfmt/clippy 配置，格式化用语言自带 `hone fmt`（作用于 .hn，非 Rust）。

## 架构

- 单 crate 流水线：lexer → parser → ast → checker → 四后端：`interp.rs`（AST 解释器）、`vm.rs`（字节码 VM）、`aot.rs`+`codegen.rs`（C 生成）、`natgen.rs`+`runtime/`（Cranelift 原生编译）。
- `Value` 定义在 `interp.rs`，VM 与全部 builtins 复用之——改 Value/字典行为要同时顾及三引擎。
- 铁律：**interp 与 VM 输出（含报错）必须逐字节一致**；错误码 Hxxx 集中在 `error.rs`，`H999`=功能未实现（VM/AOT/DLL 不支持 with、type 实例、byte/bytes、字段赋值等解释器特性时统一报 H999）。
- 原生后端与 VM **共享同一套 `vm::Instr`**：加指令时 `vm.rs` 的执行臂 / `instr_text` / `parse_instr`（文本 IR 往返）与 `natgen.rs` 的 lowering 臂、`main.rs` 的 strtab 收集必须同步；漏一处会被 `regress_ir.py` / `regress4.py` 抓住。
  - 指令带**字符串操作数**（Call 名 / Field 名 / Raise 三段文本 / ThrowStr 文案 / enum 构造名等）才需 `main.rs` strtab 收集；纯数值操作数指令（如 `CondCheck` / `IsBoolVal`）不需要。
  - `Instr::Raise` 的**错误码**另有两处白名单，漏改会让 nat 静默渲染成 H990 或 IR 往返失败：`runtime/src/lib.rs` 的 `rt_raise` 码表、`vm.rs` `parse_instr` 的 `RAISE` 码表。
  - runtime 侧「解释器显式无 help」的分支必须用 `err_h_explicit_no_help`（哨兵 `Some("")`），否则渲染层 `hn_help_for` 会凭空补一行 help。
  - **新增指令必须同步 `natgen.rs` 的 `NOT_IMPLEMENTED` 决策表**（`mod instr_coverage` 单测强制）：要么写 lowering 臂，要么带理由登记；未实现的指令报 **H999**（功能未实现，与 VM/AOT/DLL 同口径），不是 H990（internal）。当前登记项：GoCall / Await（G2.6）、Breakpoint / DebugPrint（无调试器）。
- 运算符重载（G2.4）：`__op` 回退链**全部在 runtime**（`rt_binop` / `rt_unop` / `rt_index` /
  `call_builtin` 的 `len` 臂），字节码与 natgen 无需改动。触发条件表以 `vm.rs` exec 臂为准
  （算术：非数字/str 组合 + H001；比较：非数字对、错误码不限；一元：H001）。
- 别名（G2.5）：三处同步——runtime `ALIASES` 表 + `call_global` 入口解析、natgen Call lowering
  编译期解析（决定 rt_callv / rt_call）、wrapper `NAT_ALIASES` **且原名与新名都要进 strtab**
  （原名可能从不作为 Call 名出现，漏收集会静默解析成 strtab[0]，报错指向无关函数名）。
  语义基准：VM `do_call`（先 resolve_alias → 再查局部变量 → 函数 → 内置）。
- 参数个数校验三引擎统一到解释器文案（`expects at most N arguments, got M` /
  ``missing argument `c` of `f` `` + 定位 + help）——**不要**改回中文硬编码消息。
- 点号内置族（G2.9）：三套实现——interp/VM 共用 `builtins.rs` + 各 `*mod.rs`（基于
  `Value`），**runtime 另有一份 std-only 镜像**（`runtime/src/lib.rs` 的 `call_dotted`，
  基于句柄）。改任一家的行为/文案必须同步另一家。外部 crate 依赖的家族（regex/sqlite/
  zlib/archive/http/ws/smtp/crypto/plugin/guipro/sys-winapi）在 runtime 登记 H999；
  未登记点号名保留 H404（load 库语义）。runtime 值模型含 **T_PTR** 标签（ptr 值）。
- import（G3.1）：解释器**运行期**处理（`interp::exec_import`，`hone run` 语义基准）；VM（`--vm`）
  与原生后端（`--native`）在**编译期**经 `src/imports.rs::resolve_imports` 递归展开顶层 import、
  合并为单编译单元（本地路径相对 CWD / 远程缓存 `~/.hone/cache/`，模块前缀改名 + 顶层语句入 Block）。
  改 import 语义需同时顾及运行期与构建期两处；`hone check`/`fmt`/LSP 不经过构建期展开。
- natgen 入口块不变量：函数**入口块独立于 pc=0 段块**（入口只做形参装载 / 寄存器槽初始化，末尾无条件
  跳入 pc=0 段块）。勿把两者合块——函数首语句为循环（循环头在 pc 0）时回边即「跳转到 entry 块」，
  Cranelift 校验失败（`invalid reference to entry block`），且序言会被循环重复执行。
- 原生后端产物体积（G3.3）：`build_native` 链接期固定 `-C opt-level=z -C codegen-units=1`
  `-C link-arg=-Wl,--gc-sections -C link-arg=-Wl,--strip-all`（产物 ~1MB，设计目标 ≤3MB）；
  `HONE_NAT_FLAGS` 可临时覆盖。windows-gnu 的链接驱动是 `x86_64-w64-mingw32-gcc`，链接器参数
  必须经 `-Wl,` 传递（直接 `--gc-sections` 会被 gcc 拒绝）。
- natgen ISA 旗标（G3.7）：x86_64 默认 baseline+SSSE3，`--cpu` 预设 baseline/ssse3/sse4.2/avx2。
  ISA 专属旗标（`has_ssse3` 等）**必须经 `isa::lookup(triple)` 的 `IsaBuilder`（`settings::Configurable::set`）
  设置**，不能在全局 `settings::builder()` 上设（会报 `No existing setting named`）；设完再 `finish(flags)`。
- 交叉编译（G3.2）：natgen 目标三元组参数化（默认宿主），`--target` 选目标。交叉链接走 rustc 驱动 +
  `-C link-self-contained=yes`（用 sysroot 自带 musl CRT）+ `-C linker=<sysroot>/lib/rustlib/<host>/bin/rust-lld`
  （rustc 自动带 `-flavor gnu`）+ 裸 `--gc-sections`/`--strip-all`（lld 不认 `-Wl,`）+ `-no-pie`
  （Cranelift 产非 PIC 对象，musl 默认 PIE 会重定位失败）。aarch64 需 cranelift-codegen 开 `arm64` 特性。
  交叉产物在 Windows 上不可运行，靠 CI 验证（G3.6）。
  - 链接参数**按目标 + 链接驱动分派**，不是按「是否交叉」（`main.rs::build_native`）：交叉 → rust-lld 裸参数；
    宿主 Linux（ELF，gnu/musl 同理）→ cc 驱动 `-Wl,` 参数 **且必须非 PIE**
    （`-C relocation-model=static -C link-arg=-no-pie`；缺了会让非 PIC 对象在默认 PIE 链接下报
    `relocation R_X86_64_32S ... recompile with -fPIC`，CI 的 Linux nat 回归曾因此全灭）；宿主 msvc →
    链接器是 link.exe，GNU 风格参数不适用（用 `/OPT:REF /OPT:ICF`）。
  - ELF 交叉**必须**显式 `-C relocation-model=static`，不能只给 `-C link-arg=-no-pie`：rustc 对
    musl 目标默认按 static-PIE 选启动 CRT（`rcrt1.o` + `crtbeginS.o` + `-pie --no-dynamic-linker`），
    而 `link-arg=-no-pie` 只改链接器末端开关、改不了「选哪个 crt1」——混用会产出「`rcrt1.o` 启动 +
    ET_EXEC（无 PT_DYNAMIC，`_DYNAMIC=0`）」，产物**启动即 SIGSEGV、零输出**（CI 的 Linux nat 回归
    16 例全 -11 即此）。static 后的产物入口字节应与打包工具链路径（G3.4）一致，可作为回归判据。
  - Linux 只支持 musl 交叉目标：glibc 目标（含 `x86_64-unknown-linux-gnu`）不在 CRT 分发矩阵内，
    rustc 直接报 `option -C link-self-contained is not supported on this target`；宿主 glibc 路径不传
    `link-self-contained`，故不受此限。
- 静态表入 object（G3.5）：程序静态表（strtab blob / file / src / span 表 / 全部注册表 /
  函数指针表）由 natgen `emit_static_tables` 编为 script.o 导出数据符号（`HN_*`，24 个，
  `HN_LENS` 为字节长度目录），runtime `hn_boot`（`#[cfg(hone_boot)]`，链接产物路径才编译，
  cargo test 不定义该 cfg）在 `hn_entry` 首行自举注册 + FNV 自检；wrapper 已固定为 5 行
  （无脚本信息）。改 blob 布局必须两端同步：`natgen.rs HN_BLOBS` 与 `runtime hn_boot`。
- 无 rustc 链接（G3.4）：本机无 rustc 时 `build_native` 自动走打包工具链
  （`targets/<triple>/libhoneruntime.a` + `targets/lld/rust-lld.exe` + `targets/crt/<triple>/`，
  `HONE_TOOLCHAIN_DIR` 覆盖；打包脚本 `tests/tools/build_toolchain.py`，CRT 取
  **各目标** rustlib 的 self-contained 目录）。链接 flavor 按目标环境分派：Windows→COFF
  （lld-link + 显式归档路径，`-defaultlib` 不认 `.a`；CRT 清单含 libntdll.a / libuserenv.a），
  其余→GNU/ELF（`crt1/crti/crtbegin … libunwind -lc -lm crtend/crtn` + `-no-pie
  --gc-sections --strip-all`）。lld 的依赖 DLL（libgcc_s_seh-1 / libwinpthread-1）须随包分发；
  rustc 可用时维持 rustc 链接路径（逐字节回归基准）。runtime 有改动后需重跑打包脚本。
  - 链接路径由 **`HONE_NAT_LINK=rustc|packaged`** 强制（默认 auto：rustc 可用即走 rustc，缺失才
    回退打包）。**Linux 侧 `packaged` 目前不可用**：随包的 `targets/lld/rust-lld` 动态依赖 sysroot
    的 `libLLVM.so`（未随包分发，见 G3.8），进程一起就报 `error while loading shared libraries`；
    `link_native_packaged` 会针对该形态给出指名 help 并建议走 rustc 路径，且会先把
    `targets/lld/`（含 `lib/`）加进 `LD_LIBRARY_PATH`（随包依赖一旦补上即可生效）。Windows 侧靠
    同目录 DLL 已自足。`build_toolchain.py::package_lld` 会把这类依赖的真实体积打印出来。
  - `libhoneruntime.a` 必须由打包脚本以 **`--cfg hone_boot`** 编（`build_toolchain.py` 已如此）：
    缺该 cfg 则 `hn_boot` 静态表自举不参与编译，产物运行期注册表为空。
- CI/发布（G3.6）：`ci.yml` test job 含 `regress4.py`（二进制定位支持 `HONE_BIN` 覆盖；Linux job 跑
  nat 回归前 `rustup target add x86_64-unknown-linux-musl`——**漏装时 rustc 报 E0463
  `can't find crate for std`，16 例全 BUILD-FAIL**）；
  `release.yml` 构建三目标工具链并产出 full 包（linux tar.gz / windows zip，hone + targets/
  同目录），`verify-nat` job 在 ubuntu runner 上：① 用**发布产物本身**编 x86_64-musl 产物并
  **真跑**断言输出（故先 `rustup target add x86_64-unknown-linux-musl`）；② aarch64 ELF 头校验
  （同法先装该目标 std）；③ full 包内容清单检查（运行时归档 / CRT / lld 是否齐全）。
  **不要**给该 job 加 `HONE_NAT_LINK=packaged`：Linux 侧打包链接缺 libLLVM（G3.8），只会以
  加载期失败告终。publish 等待验证通过。安装脚本（install.sh / install.ps1）优先下载 full 包，
  解压到 hone 同目录即启用原生编译工具链。
- 条件与逻辑语义（改 `vm.rs` 编译臂时注意，解释器为准）：`if`/`while`/`do-while`/`for`/三元/推导式 `if` 过滤六处条件站点**运行期严格 bool**（`Instr::CondCheck`），`&&`/`||` 短路求值且右侧操作数严格 bool（`Instr::IsBoolVal` 判「恰为该布尔值」，不是 truthy）。

## 项目特有约定

- struct 实例内部携带 `__struct__` 隐藏键（`Value::Str` 标记）：新增/改动任何用户可见 dict 路径（display/len/keys/values/has_key/for-in/推导式/解构/==/json）必须过滤该键，否则泄漏。
- 无 null 字面量（null 仅 void 占位，用 `""` 哨兵）；变量块级作用域。
- `match` 绑定变量**仅在分支体内可见**（编译期用 `decl_tmp` + 分支作用域，不推进 `var_next`）；lambda 捕获按**内层优先**（与 `lookup` 同序）。改这两处前先跑 `tests/tools/probe_align.py`。
- `d["k"]` 形式的 dict 索引**读**在 checker 层被拒（索引须 int）；dict 取值只能走 for-in / `keys` / `values` / `has_key` / 解构。别按JS/Python 直觉写 dict 读写。
- 新功能需同步：`hone.md`（语言规格）、`CHANGELOG.md`、`官网/` 站点页、README。

## 维护规则

当项目结构、构建/测试命令、架构边界、开发约定，或本文件记录的其他事实发生变化时，必须在同一次改动中同步更新本文件。
