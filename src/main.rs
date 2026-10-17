// main.rs - Hone 命令行入口（单文件 hone / hone.exe）
// 命令：hone <script.hn>（默认）、hone run、hone debug、--help、--version

mod ast;
mod archmod;
mod builtins;
mod bundle;
mod cgen_util;
mod checker;
mod codegen;
mod datamod;
mod error;
mod fmt;
mod guimod;
#[cfg(not(windows))]
mod guimod_gtk;
// X11 自绘后端仅在 Linux/BSD 上参与编译；Windows/macOS 上引用处均不可达，
// 无条件声明会让整模块变成 dead_code（约 190 条警告）并拖慢构建。
#[cfg(all(unix, not(target_os = "macos")))]
mod guimod_x11;
mod header;
mod imports;
mod interp;
mod lexer;
mod lsp;
mod natgen;
mod netmod;

// 原生后端运行时（runtime/src/lib.rs，std-only 单文件）：嵌入二进制，
// natgen 时写出临时目录参与产物链接（干净构建机无需本仓库）。
pub(crate) const NAT_RUNTIME_SRC: &str = include_str!("../runtime/src/lib.rs");
mod parser;
mod pluginmod;
mod plotmod;
mod preproc;
mod ptrmod;
mod repl;
mod vm;
mod srvmod;
mod sqlitemod;
mod statmod;
mod sysmod;
mod sysutilmod;
mod textmod;

use std::collections::HashMap;
use std::process::ExitCode;

use error::codes;
use error::ZError;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    // 管道被提前关闭（如 `hone --help | head`）时，不打印 broken pipe 的 panic 堆栈
    std::panic::set_hook(Box::new(|info| {
        let msg = info
            .payload()
            .downcast_ref::<String>()
            .map(|s| s.as_str())
            .or_else(|| info.payload().downcast_ref::<&str>().copied())
            .unwrap_or("");
        if !msg.contains("failed printing to stdout") {
            eprintln!("{}", info);
        }
    }));

    let args: Vec<String> = std::env::args().skip(1).collect();

    // 打包模式：自身携带内嵌脚本 → 走自释放启动器（--version / 释放执行 / 清理缓存）
    match bundle::detect() {
        Ok(Some(info)) => return bundle::run(&info, &args),
        Ok(None) => {}
        Err(e) => {
            eprintln!("{}", e);
            return ExitCode::FAILURE;
        }
    }

    // 大栈线程：debug 构建（未优化、栈帧巨大）在 Windows 主线程默认 1MB 栈下，
    // 递归脚本（如 bench/fib.hn 的 fib(26)，深度仅 ~20 层）即会栈溢出；
    // 把全部命令逻辑放到 64MB 栈的专用线程上执行（虚拟内存预留，不影响常驻内存）。
    match std::thread::Builder::new()
        .name("hone-main".to_string())
        .stack_size(64 * 1024 * 1024)
        .spawn(move || run_cli(&args))
    {
        Ok(handle) => match handle.join() {
            Ok(Ok(())) => ExitCode::SUCCESS,
            Ok(Err(e)) => {
                eprintln!("{}", e);
                ExitCode::FAILURE
            }
            Err(_) => ExitCode::FAILURE, // 子线程 panic：panic hook 已输出详情
        },
        Err(e) => {
            eprintln!("cannot spawn worker thread: {}", e);
            ExitCode::FAILURE
        }
    }
}

fn run_cli(args: &[String]) -> Result<(), ZError> {
    if args.is_empty() {
        print_help();
        return Ok(());
    }
    // 剥离全局开关 --vm / --disasm：可出现在脚本名前或后（两种形式都支持）。
    // 剥离后剩余参数交给各子命令处理，开关状态由 flag_vm / flag_disasm 携带。
    let mut flag_vm = false;
    let mut flag_disasm = false;
    let args: Vec<String> = args
        .iter()
        .filter(|a| {
            if *a == "--vm" {
                flag_vm = true;
                false
            } else if *a == "--disasm" {
                flag_disasm = true;
                false
            } else {
                true
            }
        })
        .cloned()
        .collect();
    match args[0].as_str() {
        "--help" | "-h" | "help" => {
            print_help();
            Ok(())
        }
        "--version" | "-V" | "version" => {
            println!("hone {}", VERSION);
            Ok(())
        }
        "run" => {
            let (opts, rest) = parse_run_args(&args[1..]);
            let path = rest
                .first()
                .ok_or_else(|| {
                    ZError::plain(
                        codes::SYNTAX,
                        "missing script path: `hone run <script.hn>`",
                        Some("run `hone --help` for usage"),
                    )
                })?;
            if opts.resume {
                load_resume_state(path)?;
            }
            // --disasm：仅编译为字节码文本 IR 并打印，不执行（对接调试/对照解释器）
            if opts.disasm || flag_disasm {
                let src = std::fs::read_to_string(path).map_err(|e| {
                    ZError::plain(
                        codes::FILE_NOT_FOUND,
                        format!("cannot read `{}`: {}", path, e),
                        Some("check the path"),
                    )
                })?;
                let program = parser::Parser::parse(path, &src)?;
                let text = vm::disassemble_program(&program, path, &src)?;
                println!("{}", text);
                return Ok(());
            }
            builtins::init_args(&rest[1..]);
            let use_vm = opts.use_vm || flag_vm;
            match opts.restart {
                Some(p) => run_with_restart(path, &p, use_vm),
                None => run_file_or_pkg(path, false, use_vm),
            }
        }
        "runir" => {
            // 直接执行文本 IR（`hone run --disasm` 的产物）：反向装配后运行，跳过解析/检查/编译。
            let path = args.get(1).ok_or_else(|| {
                ZError::plain(
                    codes::SYNTAX,
                    "missing IR path: `hone runir <program.ir>`",
                    Some("生成方式：`hone run --disasm <script.hn> > program.ir`"),
                )
            })?;
            let text = std::fs::read_to_string(path).map_err(|e| {
                ZError::plain(
                    codes::FILE_NOT_FOUND,
                    format!("cannot read `{}`: {}", path, e),
                    Some("check the path"),
                )
            })?;
            builtins::init_args(&args[2..]);
            vm::run_ir(&text, path, false)
        }
        "debug" => {
            let path = args
                .get(1)
                .ok_or_else(|| {
                    ZError::plain(
                        codes::SYNTAX,
                        "missing script path: `hone debug <script.hn>`",
                        Some("run `hone --help` for usage"),
                    )
                })?;
            builtins::init_args(&args[2..]);
            run_file(path, true, false)
        }
        "fmt" => cmd_fmt(&args[1..]),
        "doc" => cmd_doc(&args[1..]),
        "check" => cmd_check(&args[1..]),
        "natgen" => cmd_natgen(&args[1..]),
        "test" => cmd_test(&args[1..]),
        "watch" => cmd_watch(&args[1..]),
        "bind" => cmd_bind(&args[1..]),
        "build" => cmd_build(&args[1..]),
        "get" => cmd_get(&args[1..]),
        "prof" => cmd_prof(&args[1..]),
        "self-update" => cmd_self_update(&args[1..]),
        "lsp" => lsp::run_lsp(),
        "poop" => cmd_poop(&args[1..]),
        "repl" => repl::run_repl(&args[1..]),
        "explain" => {
            let code = args
                .get(1)
                .ok_or_else(|| {
                    ZError::plain(
                        codes::SYNTAX,
                        "missing error code: `hone explain <code>`",
                        Some("example: `hone explain H201`"),
                    )
                })?;
            match error::explain(code) {
                Some(text) => {
                    println!("error[{}]", code);
                    println!("{}", text);
                    Ok(())
                }
                None => Err(ZError::plain(
                    codes::NOT_FOUND,
                    format!("unknown error code `{}`", code),
                    Some("run `hone explain` with a Hxxx code listed in the docs"),
                )),
            }
        }
        other if other.ends_with(".hn") || other.ends_with(".hzp") => {
            // 直接执行脚本。--vm / --disasm 已由 run_cli 顶部剥离并存入 flag_vm / flag_disasm。
            // --disasm 时仅反汇编字节码文本 IR 后退出（不执行）。
            if flag_disasm {
                let src = std::fs::read_to_string(other).map_err(|e| {
                    ZError::plain(
                        codes::FILE_NOT_FOUND,
                        format!("cannot read `{}`: {}", other, e),
                        Some("check the path"),
                    )
                })?;
                let program = parser::Parser::parse(other, &src)?;
                // G3.1：反汇编走 VM 编译模型，同样需构建期展开顶层 import。
                let program = imports::resolve_imports(program, other, &src)?;
                let text = vm::disassemble_program(&program, other, &src)?;
                println!("{}", text);
                Ok(())
            } else {
                builtins::init_args(&args[1..]);
                run_file_or_pkg(other, false, flag_vm)
            }
        }
        other => Err(ZError::plain(
            codes::SYNTAX,
            format!("unknown command `{}`", other),
            Some("run `hone --help` for usage"),
        )),
    }
}

/// 执行一个 .hn 脚本：读取 → 解析 → 类型检查 → 解释/VM 执行。
fn run_file(path: &str, debug: bool, use_vm: bool) -> Result<(), ZError> {
    let src = std::fs::read_to_string(path).map_err(|e| {
        ZError::plain(
            codes::FILE_NOT_FOUND,
            format!("cannot read `{}`: {}", path, e),
            Some("check the path"),
        )
    })?;
    run_script(path, &src, debug, use_vm)
}

/// 执行脚本或仅脚本包（.hzp）：先尝试解包，不是包则按普通 .hn 执行。
fn run_file_or_pkg(path: &str, debug: bool, use_vm: bool) -> Result<(), ZError> {
    let data = std::fs::read(path).map_err(|e| {
        ZError::plain(
            codes::FILE_NOT_FOUND,
            format!("cannot read `{}`: {}", path, e),
            Some("check the path"),
        )
    })?;
    if let Some((name, script)) = bundle::parse_script_pkg(&data) {
        // 包内脚本名作为展示名；load/import 相对路径仍以包文件所在目录为基准
        run_script(&name, &script, debug, use_vm)
    } else {
        let src = String::from_utf8_lossy(&data).into_owned();
        run_script(path, &src, debug, use_vm)
    }
}

/// 对已读取的源码执行完整流程：解析 → 类型检查 → 执行。
/// `use_vm` 为真时走寄存器式字节码 VM（vm::run），否则走 AST 树遍历解释器（interp::run，默认）。
fn run_script(path: &str, src: &str, debug: bool, use_vm: bool) -> Result<(), ZError> {
    let program = parser::Parser::parse(path, src)?;
    if use_vm {
        // G3.1：VM 是 AOT 编译模型，无法运行期抓取模块——编译期展开顶层 import（设计 §7.3）。
        // 解释器仍走运行期 import（interp::exec_import），保持既有语义不变。
        let program = imports::resolve_imports(program, path, src)?;
        checker::Checker::check(&program, path, src)?;
        vm::run(&program, path, src, debug)?;
    } else {
        checker::Checker::check(&program, path, src)?;
        interp::run(&program, path, src, debug)?;
    }
    Ok(())
}

/// `--restart` 重启策略：最大重启次数、递增等待间隔（秒）、可重启错误码白名单（空 = 全部可重启）。
struct RestartPolicy {
    max: usize,
    backoff: Vec<u64>,
    codes: Vec<String>,
}

/// `hone run` 的运行选项：重启策略（可选）、是否恢复检查点、是否走字节码 VM、是否仅反汇编。
struct RunOptions {
    restart: Option<RestartPolicy>,
    resume: bool,
    use_vm: bool,
    disasm: bool,
}

/// 从 `hone run` 的参数中提取运行选项。
/// 返回 (选项, 剩余参数)；剩余参数中第一个为脚本路径，其余为脚本自身的参数（原样传递）。
/// 已知选项 `--restart[=N]` / `--backoff=a,b,c` / `--restart-on=Hxxx` / `--resume` 被消费，
/// 遇到第一个非选项参数即停止解析，其后内容一律视为脚本参数。
fn parse_run_args(args: &[String]) -> (RunOptions, Vec<String>) {
    let mut max = 3usize;
    let mut backoff: Vec<u64> = vec![1, 3, 10];
    let mut codes: Vec<String> = Vec::new();
    let mut has_restart = false;
    let mut resume = false;
    let mut use_vm = false;
    let mut disasm = false;
    let mut rest = Vec::new();
    let mut parsing_opts = true;

    for a in args {
        if parsing_opts {
            match a.as_str() {
                "--restart" => {
                    has_restart = true;
                    continue;
                }
                "--vm" => {
                    use_vm = true;
                    continue;
                }
                "--disasm" => {
                    disasm = true;
                    continue;
                }
                "--resume" => {
                    resume = true;
                    continue;
                }
                s if s.starts_with("--restart=") => {
                    has_restart = true;
                    max = s["--restart=".len()..].parse().unwrap_or(3);
                    continue;
                }
                s if s.starts_with("--backoff=") => {
                    backoff = s["--backoff=".len()..]
                        .split(',')
                        .filter_map(|p| p.trim().parse::<u64>().ok())
                        .collect();
                    if backoff.is_empty() {
                        backoff = vec![1];
                    }
                    continue;
                }
                s if s.starts_with("--restart-on=") => {
                    codes = s["--restart-on=".len()..]
                        .split(',')
                        .map(|p| p.trim().to_string())
                        .filter(|c| !c.is_empty())
                        .collect();
                    continue;
                }
                _ => {}
            }
        }
        parsing_opts = false;
        rest.push(a.clone());
    }

    let restart = if has_restart {
        Some(RestartPolicy { max, backoff, codes })
    } else {
        None
    };
    (RunOptions { restart, resume, use_vm, disasm }, rest)
}

/// 按策略循环运行脚本：正常结束（Ok）立即返回；错误按白名单与次数上限重试，
/// 等待间隔取 backoff 序列（第 n 次失败后等待 backoff[n]，超出取最后一项）。
fn run_with_restart(path: &str, policy: &RestartPolicy, use_vm: bool) -> Result<(), ZError> {
    let mut count = 0usize;
    loop {
        match run_file(path, false, use_vm) {
            Ok(()) => return Ok(()),
            Err(e) => {
                let retryable = policy.codes.is_empty() || policy.codes.iter().any(|c| c == e.code);
                if !retryable || count >= policy.max {
                    // 不可重试或已达上限：以最后一次错误退出
                    return Err(e);
                }
                let delay = *policy.backoff.get(count).unwrap_or_else(|| policy.backoff.last().unwrap());
                eprintln!(
                    "[restart] {}: error[{}] — retry {}/{} after {}s",
                    path,
                    e.code,
                    count + 1,
                    policy.max,
                    delay
                );
                std::thread::sleep(std::time::Duration::from_secs(delay));
                count += 1;
            }
        }
    }
}

/// ~/.hone/state/ 状态目录（Windows 用 USERPROFILE）。
fn state_dir() -> std::path::PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(home).join(".hone").join("state")
}

/// `--resume`：恢复 db 检查点并启用自动落盘。
/// 状态文件按脚本路径哈希命名（同一脚本稳定定位）；文件内容携带脚本内容哈希，
/// 脚本变更后检查点自动失效（视为无检查点，不报错）。
fn load_resume_state(path: &str) -> Result<(), ZError> {
    use sha2::{Digest, Sha256};

    let src = std::fs::read_to_string(path).map_err(|e| {
        ZError::plain(
            codes::FILE_NOT_FOUND,
            format!("cannot read `{}`: {}", path, e),
            Some("check the path"),
        )
    })?;
    let content_hash = format!("{:x}", Sha256::digest(src.as_bytes()));
    let path_hash = format!("{:x}", Sha256::digest(path.as_bytes()));
    let dir = state_dir();
    let _ = std::fs::create_dir_all(&dir);
    let state_file = dir.join(format!("{}.json", &path_hash[..16]));

    // 读取并校验检查点；缺失 / 损坏 / 脚本已变更 → 空状态
    let kv: HashMap<String, String> = match std::fs::read_to_string(&state_file) {
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) if v.get("script").and_then(|s| s.as_str()) == Some(content_hash.as_str()) => {
                v.get("kv")
                    .and_then(|k| serde_json::from_value::<HashMap<String, String>>(k.clone()).ok())
                    .unwrap_or_default()
            }
            _ => HashMap::new(),
        },
        Err(_) => HashMap::new(),
    };

    builtins::load_state(kv);
    builtins::enable_persist(state_file, content_hash);
    Ok(())
}

fn print_help() {
    println!("Hone v{} - 轻量级、跨平台、可嵌入的脚本语言", VERSION);
    println!();
    println!("用法:");
    println!("  hone <script.hn>         执行 Hone 脚本（默认命令）");
    println!("  hone run <script.hn>     执行 Hone 脚本");
    println!("       --vm                使用寄存器式字节码虚拟机执行（默认走 AST 解释器）");
    println!("       --disasm            仅编译为字节码文本 IR 并打印（不执行）");
    println!("       --restart[=N]       失败自动重启（N 为最大次数，默认 3；仅对可恢复错误）");
    println!("       --backoff=a,b,c     重启间隔递增序列（秒，默认 1,3,10）");
    println!("       --restart-on=Hxxx   只对指定错误码重启（逗号分隔；省略则全部可重启）");
    println!("       --resume            恢复上次 db 检查点（脚本变更后自动失效）");
    println!("  hone explain <code>       查看错误码解释（如 `hone explain H201`）");
    println!("  hone bind <header.h>      从 C 头文件生成 FFI 签名块（typed load 用）");
    println!("  hone debug <script.hn>   断点调试模式（breakpoint 关键字生效）");
    println!("  hone fmt [-w|-c] <file.hn>  代码格式化（Tab 缩进/运算符空格/大括号；-w 覆盖写，-c 只检查差异）");
    println!("  hone doc <file.hn>        从 fn/class 定义与上方 `//` 注释生成 Markdown API 文档");
    println!("  hone check <file.hn|目录> [--json] 仅静态检查（解析+类型检查，不执行）；目录递归扫全部 .hn，--json 输出诊断 JSON 数组，退出码 0/1 表达成败");
    println!("  hone natgen <file.hn>  原生编译（build --exe --native 别名）：脚本 → Cranelift object → 链接原生可执行 [-o <out>] [--obj <o>]");
    println!("  hone build --dll <file.hn> 将脚本打包为 C ABI 动态库（int/float/bool/str 映射，需 C 编译器）");
    println!("  hone build --exe <file.hn> 将脚本与解释器打包为独立可执行文件（[-o <out>] [--icon <ico>]）");
    println!("  hone build --exe --native <file.hn> 原生编译为可执行文件（Cranelift→object→内嵌运行时，需 rustc；[-o <out>] [--cpu <name>] [--target <triple>]）");
    println!("  hone build --script <file.hn> 生成仅脚本压缩包 .hzp（不内嵌解释器，[-o <out>]，用 hone run 执行）");
    println!("  hone get                   读取当前目录 hone.json 清单，批量下载全部模块");
    println!("  hone get <module> <url>  下载模块依赖并缓存到 ~/.hone/cache/，并写入 hone.json 清单");
    println!("  hone get <script.hn>     预下载脚本中所有 import 声明的模块");
    println!("  hone watch <script.hn>   监控脚本文件变更自动重跑（[--interval=N] 毫秒，默认 500，Ctrl+C 退出）");
    println!("  hone prof <script.hn>    以剖析模式运行脚本，输出函数级热点报告（总耗时/调用次数/平均耗时）");
    println!("  hone test [script.hn]    运行 `*.test.hn` 测试文件与 assert 断言");
    println!("  hone poop <file.hn>      屎山检测：扫描代码异味与可疑写法");
    println!("  hone self-update [url]   从 URL 下载最新 hone 二进制并替换当前程序（需管理员/写权限）");
    println!("  hone lsp                 启动语言服务器（补全/诊断，LSP over stdio）");
    println!("  hone repl                交互式解释器（Python 式：表达式回显/多行输入/.vars）");
    println!("  hone --help              显示帮助");
    println!("  hone --version           显示版本");
    println!();
    println!("可视化编辑器：浏览器打开 editor/index.html（拖拽代码块生成 .hn 代码）");
}

/// hone bind <header.h>：解析 C 头文件，生成 typed load 签名块（打印到 stdout）。
fn cmd_bind(args: &[String]) -> Result<(), ZError> {
    let path = args
        .first()
        .ok_or_else(|| {
            ZError::plain(
                codes::SYNTAX,
                "missing header path: `hone bind <header.h>`",
                Some("example: `hone bind /usr/include/sqlite3.h`"),
            )
        })?;
    let src = std::fs::read_to_string(path).map_err(|e| {
        ZError::plain(
            codes::NOT_FOUND,
            format!("cannot read header `{}`: {}", path, e),
            Some("check the header path"),
        )
    })?;
    let sigs = header::parse(&src);
    let supported: Vec<_> = sigs.iter().filter(|s| s.unsupported.is_none()).collect();
    let unsupported: Vec<_> = sigs.iter().filter(|s| s.unsupported.is_some()).collect();
    println!("// 由 hone bind {} 生成（受限 C 原型提取，纯 Rust 实现）", path);
    println!("// 用法一：load \"你的动态库\" as lib from \"{}\";", path);
    println!("// 用法二：把下面的签名块粘贴进脚本：");
    println!("load \"你的动态库\" as lib {{");
    for sig in &supported {
        let params = sig
            .params
            .iter()
            .map(|p| format!("{}: {}", p.name, p.ty.name()))
            .collect::<Vec<_>>()
            .join(", ");
        println!("    fn {}({}) -> {};", sig.name, params, sig.ret.name());
    }
    if !unsupported.is_empty() {
        println!("    // 以下原型因受限解析器不支持而跳过（回调/变参/数组/结构体等）：");
        for sig in &unsupported {
            println!("    // fn {}() -> int;  // {}", sig.name, sig.unsupported.unwrap());
        }
    }
    println!("}}");
    println!(
        "// 统计：共 {} 个原型，{} 个可直接绑定，{} 个不支持",
        sigs.len(),
        supported.len(),
        unsupported.len()
    );
    Ok(())
}

/// hone build --dll <script.hn> / hone build --exe <script.hn>
fn cmd_build(args: &[String]) -> Result<(), ZError> {
    match args.first().map(|s| s.as_str()) {
        Some("--dll") => {
            let path = args
                .get(1)
                .ok_or_else(|| {
                    ZError::plain(
                        codes::SYNTAX,
                        "missing script path: `hone build --dll <script.hn>`",
                        Some("run `hone --help` for usage"),
                    )
                })?;
            cmd_build_dll(path)
        }
        Some("--exe") => cmd_build_exe(&args[1..]),
        Some("--script") => cmd_build_script(&args[1..]),
        _ => Err(ZError::plain(
            codes::SYNTAX,
            "unknown build options: `hone build --dll <script.hn>`, `hone build --exe <script.hn>` or `hone build --script <script.hn>`",
            Some("`--dll` compiles to a shared library; `--exe` bundles the script with the interpreter; `--script` packs the script alone"),
        )),
    }
}

/// hone build --script <script.hn> [-o <out>]
/// 仅脚本压缩包：不内嵌解释器，体积小，可配合任意 hone 运行时执行（hone run <pkg>）。
fn cmd_build_script(args: &[String]) -> Result<(), ZError> {
    let mut out: Option<String> = None;
    let mut path: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-o" => {
                i += 1;
                out = args.get(i).cloned();
            }
            s if s.starts_with("--out=") => out = Some(s["--out=".len()..].to_string()),
            s if s.starts_with('-') => {
                return Err(ZError::plain(
                    codes::SYNTAX,
                    format!("unknown build option `{}`", s),
                    Some("options: `-o <out>`"),
                ));
            }
            s => {
                if path.is_none() {
                    path = Some(s.to_string());
                } else {
                    return Err(ZError::plain(
                        codes::SYNTAX,
                        "too many arguments",
                        Some("usage: `hone build --script <script.hn> [-o <out>]`"),
                    ));
                }
            }
        }
        i += 1;
    }
    let path = path.ok_or_else(|| {
        ZError::plain(
            codes::SYNTAX,
            "missing script path: `hone build --script <script.hn>`",
            Some("run `hone --help` for usage"),
        )
    })?;
    let script = std::fs::read_to_string(&path).map_err(|e| {
        ZError::plain(
            codes::FILE_NOT_FOUND,
            format!("cannot read `{}`: {}", path, e),
            Some("check the path"),
        )
    })?;
    let name = std::path::Path::new(&path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "script.hn".to_string());
    let pkg = bundle::build_script_pkg(&script, &name);
    let out = match out {
        Some(o) => o,
        None => {
            let stem = std::path::Path::new(&path)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "app".to_string());
            format!("{}.hzp", stem)
        }
    };
    std::fs::write(&out, &pkg).map_err(|e| {
        ZError::plain(
            codes::FILE_PERMISSION,
            format!("cannot write `{}`: {}", out, e),
            Some("check the directory permissions"),
        )
    })?;
    println!(
        "生成 {} 完成（仅脚本包, {:.1} KB；运行: hone run {}）",
        out,
        pkg.len() as f64 / 1024.0,
        out
    );
    Ok(())
}

/// hone build --exe <script.hn> [-o <out>] [--icon <ico>] [--version] [--native]
/// 默认：将当前 hone 运行时与脚本打包为单个自释放可执行文件（见 bundle.rs 格式）。
/// --native：原生编译模式（natgen 管线别名入口，T7）——脚本经 Cranelift 编译为
///     object 并与内嵌运行时链接成原生可执行（不依赖 C 编译器，需本机 rustc）。
fn cmd_build_exe(args: &[String]) -> Result<(), ZError> {
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("hone build --exe (Hone v{})", VERSION);
        return Ok(());
    }
    let mut out: Option<String> = None;
    let mut icon: Option<String> = None;
    let mut path: Option<String> = None;
    let mut native_mode = false;
    let mut cpu: Option<String> = None;
    let mut target: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--native" => native_mode = true,
            "--cpu" => {
                i += 1;
                cpu = args.get(i).cloned();
            }
            "--target" => {
                i += 1;
                target = args.get(i).cloned();
            }
            "-o" => {
                i += 1;
                out = args.get(i).cloned();
            }
            "--icon" => {
                i += 1;
                icon = args.get(i).cloned();
            }
            s if s.starts_with("--out=") => out = Some(s["--out=".len()..].to_string()),
            s if s.starts_with("--icon=") => icon = Some(s["--icon=".len()..].to_string()),
            s if s.starts_with("--cpu=") => cpu = Some(s["--cpu=".len()..].to_string()),
            s if s.starts_with("--target=") => target = Some(s["--target=".len()..].to_string()),
            s if s.starts_with('-') => {
                return Err(ZError::plain(
                    codes::SYNTAX,
                    format!("unknown build option `{}`", s),
                    Some("options: `--native`, `--cpu <name>`, `--target <triple>`, `-o <out>`, `--icon <ico>`, `--version`"),
                ));
            }
            s => {
                if path.is_none() {
                    path = Some(s.to_string());
                } else {
                    return Err(ZError::plain(
                        codes::SYNTAX,
                        "too many arguments",
                        Some("usage: `hone build --exe <script.hn> [--native] [-o <out>]`"),
                    ));
                }
            }
        }
        i += 1;
    }
    let path = path.ok_or_else(|| {
        ZError::plain(
            codes::SYNTAX,
            "missing script path: `hone build --exe <script.hn>`",
            Some("run `hone --help` for usage"),
        )
    })?;
    if let Some(ic) = &icon {
        eprintln!("[build] warning: `--icon` is not supported in this build, ignoring `{}`", ic);
    }
    if native_mode {
        return build_native(&path, out, None, "build", cpu, target);
    }

    // 以当前 hone 可执行文件作为内嵌运行时
    let exe_bytes = std::fs::read(std::env::current_exe().map_err(|e| {
        ZError::plain(
            codes::NOT_FOUND,
            format!("cannot locate the hone runtime: {}", e),
            None::<&str>,
        )
    })?)
    .map_err(|e| {
        ZError::plain(
            codes::NOT_FOUND,
            format!("cannot read the hone runtime: {}", e),
            None::<&str>,
        )
    })?;
    let script = std::fs::read_to_string(&path).map_err(|e| {
        ZError::plain(
            codes::FILE_NOT_FOUND,
            format!("cannot read `{}`: {}", path, e),
            Some("check the path"),
        )
    })?;
    let name = std::path::Path::new(&path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "script.hn".to_string());

    let ver = parse_version(VERSION);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let out_bytes = bundle::build(&exe_bytes, &script, &name, ver, timestamp);

    // 默认输出名：脚本 stem + 平台可执行后缀
    let out = match out {
        Some(o) => o,
        None => {
            let stem = std::path::Path::new(&path)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "app".to_string());
            format!("{}.exe", stem)
        }
    };
    std::fs::write(&out, &out_bytes).map_err(|e| {
        ZError::plain(
            codes::FILE_PERMISSION,
            format!("cannot write `{}`: {}", out, e),
            Some("check the directory permissions"),
        )
    })?;
    // Unix 下补可执行权限
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o755));
    }
    println!(
        "生成 {} 完成（脚本: {}, Hone v{}.{}.{}, {:.1} KB）",
        out,
        name,
        ver.0,
        ver.1,
        ver.2,
        out_bytes.len() as f64 / 1024.0
    );
    Ok(())
}

/// 解析 "x.y.z" 版本号为三元组。
fn parse_version(v: &str) -> (u16, u16, u16) {
    let mut parts = v.split('.');
    let major = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let minor = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let patch = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (major, minor, patch)
}

/// 将 .hn 脚本打包为 C ABI 动态库。进度条使用 \r 轻量显示。
fn cmd_build_dll(path: &str) -> Result<(), ZError> {
    let src = std::fs::read_to_string(path).map_err(|e| {
        ZError::plain(
            codes::NOT_FOUND,
            format!("cannot read `{}`: {}", path, e),
            Some("check the path"),
        )
    })?;

    print!("[1/4] 解析与类型检查...\r");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let program = parser::Parser::parse(path, &src)?;
    checker::Checker::check(&program, path, &src)?;

    let exports = codegen::collect_exports(&program);
    if exports.is_empty() {
        return Err(ZError::plain(
            codes::NOT_IMPLEMENTED,
            "no `@export` declaration found",
            Some("add `@export 函数名;` to the script and rebuild"),
        ));
    }

    print!("[2/4] 生成 C 代码...\r");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let c_code = codegen::generate(&program, &exports, path, &src)?;

    let stem = std::path::Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "hone_lib".to_string());
    let cfile = format!("{}.c", stem);
    std::fs::write(&cfile, &c_code).map_err(|e| {
        ZError::plain(
            codes::NOT_FOUND,
            format!("cannot write `{}`: {}", cfile, e),
            Some("check the directory permissions"),
        )
    })?;

    print!("[3/4] 查找 C 编译器...\r");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let cc = match find_cc() {
        Ok(cc) => cc,
        Err(_) => {
            println!();
            return Err(ZError::plain(
                codes::NOT_IMPLEMENTED,
                "no C compiler found (gcc/clang), cannot compile the dynamic library",
                Some(format!(
                    "the generated C source is kept at `{}`; compile it manually with `gcc -shared -O2 -o <out> {}`",
                    cfile, cfile
                )),
            ));
        }
    };

    let ext = if cfg!(windows) {
        "dll"
    } else if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    let out = format!("{}.{}", stem, ext);

    print!("[4/4] 编译中（{}）...\r", cc);
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let result = run_cc(&cc, &cfile, &out);
    std::fs::remove_file(&cfile).ok();
    result?;

    println!();
    println!("生成 {} 完成（导出: {}）", out, exports.join(", "));
    Ok(())
}

/// 查找 C 编译器：CC 环境变量 > gcc > clang > cc。
fn find_cc() -> Result<String, ZError> {
    if let Ok(cc) = std::env::var("CC") {
        if !cc.trim().is_empty() {
            return Ok(cc);
        }
    }
    for name in ["gcc", "clang", "cc"] {
        let ok = std::process::Command::new(name)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return Ok(name.to_string());
        }
    }
    Err(ZError::plain(
        codes::NOT_IMPLEMENTED,
        "no C compiler found (gcc/clang), cannot build the dynamic library",
        Some("install gcc (e.g. MinGW-w64 on Windows), or set the `CC` environment variable"),
    ))
}

fn run_cc(cc: &str, cfile: &str, out: &str) -> Result<(), ZError> {
    let status = if cfg!(windows) {
        std::process::Command::new(cc)
            .args(["-shared", "-O2", "-o", out, cfile])
            .status()
    } else {
        std::process::Command::new(cc)
            .args(["-shared", "-fPIC", "-O2", "-o", out, cfile])
            .status()
    };
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(ZError::plain(
            codes::NOT_IMPLEMENTED,
            format!("C compiler exited with code {}", s.code().unwrap_or(-1)),
            Some("check the generated C code, or install a complete toolchain"),
        )),
        Err(e) => Err(ZError::plain(
            codes::NOT_IMPLEMENTED,
            format!("cannot run C compiler `{}`: {}", cc, e),
            None::<&str>,
        )),
    }
}

/// hone get [<module> <url>] [<script.hn>]
/// 模块依赖管理：
///   hone get                读取当前目录 hone.json 清单，批量下载全部模块到 ~/.hone/cache/
///   hone get <module> <url> 下载单个模块，并把 (module, url) 追加写入 hone.json 清单
///   hone get <script.hn>    预下载脚本中所有 import 声明的模块
fn cmd_get(args: &[String]) -> Result<(), ZError> {
    match args.len() {
        0 => {
            // 读取 hone.json 模块清单，批量下载
            let manifest_path = "hone.json";
            let text = std::fs::read_to_string(manifest_path).map_err(|e| {
                ZError::plain(
                    codes::NOT_FOUND,
                    format!("cannot read `{}`: {}", manifest_path, e),
                    Some("create a hone.json manifest first: `hone get <module> <url>`"),
                )
            })?;
            let modules = parse_manifest_modules(&text)?;
            if modules.is_empty() {
                return Err(ZError::plain(
                    codes::SYNTAX,
                    format!("no modules found in `{}`", manifest_path),
                    Some("add entries under `\"modules\": { \"name\": \"url\" }`"),
                ));
            }
            for (name, url) in &modules {
                fetch_and_cache(name, url)?;
            }
            println!("共下载 {} 个模块", modules.len());
            Ok(())
        }
        1 => {
            // 扫描脚本中的 import 声明并预下载
            let path = &args[0];
            let src = std::fs::read_to_string(path).map_err(|e| {
                ZError::plain(codes::NOT_FOUND, format!("cannot read `{}`: {}", path, e), Some("check the path"))
            })?;
            let program = parser::Parser::parse(path, &src)?;
            let mut imports = Vec::new();
            collect_imports(&program.stmts, &mut imports);
            if imports.is_empty() {
                return Err(ZError::plain(
                    codes::SYNTAX,
                    format!("no `import` declaration found in `{}`", path),
                    Some("add `import \"mod\" from \"URL\";` to the script"),
                ));
            }
            for (name, url) in &imports {
                fetch_and_cache(name, url)?;
            }
            println!("共预下载 {} 个模块", imports.len());
            Ok(())
        }
        2 => {
            fetch_and_cache(&args[0], &args[1])?;
            // 追加写入 hone.json 清单（模块已存在则更新其 URL）
            append_manifest(&args[0], &args[1])?;
            Ok(())
        }
        _ => Err(ZError::plain(
            codes::SYNTAX,
            "usage: `hone get` | `hone get <module> <url>` | `hone get <script.hn>`",
            Some("run `hone --help` for usage"),
        )),
    }
}

/// 解析 hone.json 清单中的模块映射（{"modules": {name: url}}），返回排序后的 (name, url) 列表。
fn parse_manifest_modules(text: &str) -> Result<Vec<(String, String)>, ZError> {
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| {
        ZError::plain(codes::SYNTAX, format!("invalid hone.json: {}", e), Some("fix the JSON syntax"))
    })?;
    let mut out = Vec::new();
    if let Some(modules) = v.get("modules").and_then(|m| m.as_object()) {
        for (name, url) in modules {
            match url.as_str() {
                Some(u) => out.push((name.clone(), u.to_string())),
                None => {
                    return Err(ZError::plain(
                        codes::SYNTAX,
                        format!("module `{}` in hone.json must map to a URL string", name),
                        None::<&str>,
                    ))
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

/// 把 (name, url) 追加进当前目录 hone.json 的 modules 映射（已存在则更新其 URL）；文件不存在则新建清单。
fn append_manifest(name: &str, url: &str) -> Result<(), ZError> {
    let manifest_path = "hone.json";
    let (proj_name, version, modules) = if std::path::Path::new(manifest_path).exists() {
        let text = std::fs::read_to_string(manifest_path).map_err(|e| {
            ZError::plain(codes::NOT_FOUND, format!("cannot read `{}`: {}", manifest_path, e), None::<&str>)
        })?;
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            ZError::plain(codes::SYNTAX, format!("invalid hone.json: {}", e), Some("fix the JSON syntax"))
        })?;
        let n = v.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let ver = v.get("version").and_then(|x| x.as_str()).unwrap_or("0.1.0").to_string();
        let mut mods = parse_manifest_modules(&text)?;
        mods.retain(|(k, _)| k != name);
        mods.push((name.to_string(), url.to_string()));
        mods.sort();
        (n, ver, mods)
    } else {
        // 新建清单：name 取当前目录名，version 默认 0.1.0
        let dir = std::env::current_dir()
            .map(|d| d.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default())
            .unwrap_or_default();
        (dir, "0.1.0".to_string(), vec![(name.to_string(), url.to_string())])
    };

    let mut modules_map = serde_json::Map::new();
    for (k, u) in &modules {
        modules_map.insert(k.clone(), serde_json::Value::String(u.clone()));
    }
    let manifest = serde_json::json!({
        "name": proj_name,
        "version": version,
        "modules": serde_json::Value::Object(modules_map),
    });
    let json = serde_json::to_string_pretty(&manifest).map_err(|e| {
        ZError::plain(codes::SYNTAX, format!("cannot serialize hone.json: {}", e), None::<&str>)
    })?;
    std::fs::write(manifest_path, json + "\n").map_err(|e| {
        ZError::plain(codes::NOT_FOUND, format!("cannot write `{}`: {}", manifest_path, e), None::<&str>)
    })?;
    println!("已写入 {}: {} -> {}", manifest_path, name, url);
    Ok(())
}

fn collect_imports(stmts: &[ast::Stmt], out: &mut Vec<(String, String)>) {
    for s in stmts {
        match s {
            ast::Stmt::Import { name, url, .. } => out.push((name.clone(), url.clone())),
            ast::Stmt::Block { stmts, .. } => collect_imports(stmts, out),
            ast::Stmt::If { then_branch, else_branch, .. } => {
                collect_imports(then_branch, out);
                if let Some(eb) = else_branch {
                    collect_imports(eb, out);
                }
            }
            ast::Stmt::While { body, .. } => collect_imports(body, out),
            _ => {}
        }
    }
}

/// 下载模块并写入缓存 ~/.hone/cache/<name>.hn（已缓存则跳过）。
/// URL 非 http/https 开头时视为本地路径，直接读取源码（不联网）。
fn fetch_and_cache(name: &str, url: &str) -> Result<(), ZError> {
    let cache_file = interp::hone_cache_dir().join(format!("{}.hn", name));
    if cache_file.exists() {
        let size = std::fs::metadata(&cache_file).map(|m| m.len()).unwrap_or(0);
        println!("已缓存: {} ({} 字节)", name, size);
        return Ok(());
    }
    let code = if url.starts_with("http://") || url.starts_with("https://") {
        print!("\r[hone get] 下载 `{}` ...", name);
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let span = lexer::Span { line: 1, col: 1, len: 1 };
        let code = builtins::http_request(url, "GET", None, span, name, "")?;
        println!();
        code
    } else {
        // 本地路径：直接读取模块源码
        std::fs::read_to_string(url).map_err(|e| {
            ZError::plain(codes::NOT_FOUND, format!("cannot read local module `{}`: {}", url, e), Some("check the path"))
        })?
    };
    if let Some(dir) = cache_file.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| ZError::plain(codes::NOT_FOUND, format!("cannot create cache dir: {}", e), None::<&str>))?;
    }
    std::fs::write(&cache_file, &code)
        .map_err(|e| ZError::plain(codes::NOT_FOUND, format!("cannot write cache file: {}", e), None::<&str>))?;
    println!("已下载并缓存: {} ({} 字节)", name, code.len());
    Ok(())
}

/// 当前平台的 hone 发布资产名（self-update 默认 URL 用）。
fn update_asset_name() -> &'static str {
    if cfg!(windows) {
        "hone-windows-x86_64.exe"
    } else if cfg!(target_os = "macos") {
        "hone-macos-x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "hone-termux-aarch64"
    } else {
        "hone-linux-x86_64"
    }
}

/// 默认更新 URL 候选：镜像源优先（清华 TUNA github-release），失败回退官方 GitHub。
/// HONE_MIRROR 可覆盖镜像地址；HONE_MIRROR=off 强制只用官方源。
fn default_update_urls() -> Vec<String> {
    let asset = update_asset_name();
    let official = format!("https://github.com/shichencike/Hone/releases/latest/download/{}", asset);
    // TUNA 镜像目录结构：LatestRelease 软链指向最新版本（无 releases/latest/download 路径）
    let official_url = |base: &str| format!("{}/LatestRelease/{}", base.trim_end_matches('/'), asset);
    match std::env::var("HONE_MIRROR") {
        Ok(m) if m == "off" => vec![official],
        Ok(m) => vec![official_url(&m), official],
        Err(_) => vec![
            official_url("https://mirrors.tuna.tsinghua.edu.cn/github-release/shichencike/Hone"),
            official,
        ],
    }
}

/// hone self-update [url]：从 URL 下载最新 hone 二进制并替换当前程序。
/// 无参数时依次尝试：HONE_MIRROR 镜像源（默认清华源）→ 官方 GitHub。
fn cmd_self_update(args: &[String]) -> Result<(), ZError> {
    // 候选 URL 列表：显式参数 > HONE_UPDATE_URL > 镜像源 + 官方默认
    let candidates: Vec<String> = if let Some(u) = args.first() {
        vec![u.clone()]
    } else if let Ok(u) = std::env::var("HONE_UPDATE_URL") {
        vec![u]
    } else {
        default_update_urls()
    };

    println!("hone self-update: 当前版本 v{}", VERSION);
    let mut last_err: Option<ZError> = None;
    let mut bytes: Vec<u8> = Vec::new();
    for url in &candidates {
        print!("下载 `{}` ...", url);
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let span = lexer::Span { line: 1, col: 1, len: 1 };
        match builtins::http_get_bytes(url, span, "self-update", "") {
            Ok(b) => {
                bytes = b;
                println!(" ok");
                break;
            }
            Err(e) => {
                println!(" 失败（{}）", e.msg);
                last_err = Some(e);
            }
        }
    }
    if bytes.is_empty() {
        return Err(last_err.unwrap_or_else(|| {
            ZError::plain(
                codes::NETWORK,
                "self-update: 所有下载源均失败",
                Some("检查网络，或改用 `hone self-update <url>` 指定下载地址"),
            )
        }));
    }

    // 基本可执行文件校验：非空 + 平台魔数（PE MZ / ELF）
    let ok_magic = if cfg!(windows) {
        bytes.len() > 0x40 && bytes.starts_with(b"MZ")
    } else {
        bytes.starts_with(b"\x7fELF")
    };
    if !ok_magic {
        return Err(ZError::plain(
            codes::NETWORK,
            format!("downloaded file is not a valid hone executable ({} bytes)", bytes.len()),
            Some("check the URL points to a hone binary for this platform"),
        ));
    }

    // 替换当前可执行文件：先写临时文件再替换（Windows 下当前进程占用 exe，直接覆盖会失败）
    let exe = std::env::current_exe().map_err(|e| {
        ZError::plain(codes::SYSCALL, format!("cannot locate the current executable: {}", e), None::<&str>)
    })?;
    let tmp = exe.with_extension("new");
    std::fs::write(&tmp, &bytes).map_err(|e| {
        ZError::plain(
            codes::FILE_PERMISSION,
            format!("cannot write staging file `{}`: {}", tmp.display(), e),
            Some("check the directory permissions (try running as administrator)"),
        )
    })?;
    match std::fs::rename(&tmp, &exe) {
        Ok(()) => {
            println!("已更新到 v{}（{} 字节），重启后生效。", VERSION, bytes.len());
            Ok(())
        }
        Err(e) => {
            // Windows 下正在运行的 exe 无法被覆盖：保留新文件并提示手动替换
            eprintln!("无法直接替换当前程序（{}）。", e);
            eprintln!("新版本已保存到: {}", tmp.display());
            Err(ZError::plain(
                codes::FILE_PERMISSION,
                format!("cannot replace `{}`: the executable is in use", exe.display()),
                Some(format!("close this program and rename `{}` over `{}`", tmp.display(), exe.display())),
            ))
        }
    }
}

/// hone fmt [-w|-c] <file.hn>...：格式化到 stdout；-w 覆盖写入；-c 只检查差异（CI 用）。
fn cmd_fmt(args: &[String]) -> Result<(), ZError> {
    let mut overwrite = false;
    let mut check = false;
    let mut files = Vec::new();
    for a in args {
        match a.as_str() {
            "-w" | "--write" => overwrite = true,
            "-c" | "--check" => check = true,
            _ => files.push(a.clone()),
        }
    }
    if files.is_empty() {
        return Err(ZError::plain(
            codes::SYNTAX,
            "missing file: `hone fmt [-w|-c] <file.hn>...`",
            Some("pass one or more .hn files, e.g. `hone fmt -w *.hn`"),
        ));
    }
    if overwrite && check {
        return Err(ZError::plain(
            codes::SYNTAX,
            "`-w` and `-c` are mutually exclusive",
            Some("use `-w` to rewrite files, or `-c` to only report differences"),
        ));
    }
    let mut dirty = 0usize;
    let mut total = 0usize;
    for f in files {
        let src = std::fs::read_to_string(&f).map_err(|e| {
            ZError::plain(codes::NOT_FOUND, format!("cannot read `{}`: {}", f, e), Some("check the path"))
        })?;
        let formatted = fmt::format(&src)?;
        total += 1;
        if check {
            if formatted == src {
                println!("OK         {}", f);
            } else {
                println!("NEEDS FMT  {}", f);
                dirty += 1;
            }
        } else if overwrite {
            std::fs::write(&f, formatted).map_err(|e| {
                ZError::plain(codes::NOT_FOUND, format!("cannot write `{}`: {}", f, e), Some("check the path"))
            })?;
        } else {
            print!("{}", formatted);
        }
    }
    if check {
        println!();
        if dirty > 0 {
            println!("{} of {} file(s) need formatting", dirty, total);
            return Err(ZError::plain(
                codes::SYNTAX,
                format!("{} file(s) not formatted (run `hone fmt -w` to fix)", dirty),
                Some("run `hone fmt -w <file>...` to apply formatting"),
            ));
        }
        println!("all {} file(s) formatted", total);
    }
    Ok(())
}

/// hone doc <file.hn>：扫描函数/类定义及其上方 `//` 注释，生成 Markdown API 文档。
fn cmd_doc(args: &[String]) -> Result<(), ZError> {
    let path = args.get(0).ok_or_else(|| {
        ZError::plain(
            codes::SYNTAX,
            "missing file: `hone doc <file.hn>`",
            Some("pass a .hn script, e.g. `hone doc mylib.hn`"),
        )
    })?;
    let src = std::fs::read_to_string(path).map_err(|e| {
        ZError::plain(codes::NOT_FOUND, format!("cannot read `{}`: {}", path, e), Some("check the path"))
    })?;
    // 语法校验：doc 工具要求输入是合法脚本
    parser::Parser::parse(path, &src)?;

    let lines: Vec<&str> = src.lines().collect();
    let mut fns: Vec<(String, String, Vec<String>)> = Vec::new(); // (名, 签名, 文档行)
    let mut classes: Vec<(String, Vec<String>)> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim_start();
        // 收集紧邻上方的连续 // 注释（空行或其它代码会中断注释块）
        let mut doc: Vec<String> = Vec::new();
        let mut j = i;
        while j > 0 {
            let prev = lines[j - 1].trim_start();
            if let Some(c) = prev.strip_prefix("//") {
                doc.insert(0, c.trim().to_string());
                j -= 1;
            } else {
                break;
            }
        }
        let is_fn = trimmed.starts_with("fn ") || trimmed.starts_with("tmp fn ");
        let is_class = trimmed.starts_with("class ");
        if is_fn || is_class {
            let (name, sig) = collect_def_sig(&lines, i);
            if is_class {
                classes.push((name, doc));
            } else {
                fns.push((name, sig, doc));
            }
        }
        i += 1;
    }

    println!("# {} — API 文档", path);
    println!();
    println!("> 由 `hone doc` 生成：{} 个函数，{} 个类。", fns.len(), classes.len());
    if !fns.is_empty() {
        println!();
        println!("## 函数");
        for (name, sig, doc) in &fns {
            println!();
            println!("### `fn {}`", name);
            println!("```hn");
            println!("{}", sig);
            println!("```");
            if doc.is_empty() {
                println!("_（无注释）_");
            } else {
                for d in doc {
                    println!("{}", d);
                }
            }
        }
    }
    if !classes.is_empty() {
        println!();
        println!("## 类");
        for (name, doc) in &classes {
            println!();
            println!("### `class {}`", name);
            if doc.is_empty() {
                println!("_（无注释）_");
            } else {
                for d in doc {
                    println!("{}", d);
                }
            }
        }
    }
    println!();
    Ok(())
}

/// M0 原生管线：脚本 → VM 字节码（整数子集）→ Cranelift object（x86_64 COFF，含
/// __hn_main 入口）→ 与 Rust native 运行时链接成原生 exe。
/// 用法: hone natgen <file.hn> [-o <out>] [--obj <obj>]
fn cmd_natgen(args: &[String]) -> Result<(), ZError> {
    let mut out: Option<String> = None;
    let mut obj: Option<String> = None;
    let mut cpu: Option<String> = None;
    let mut target: Option<String> = None;
    let mut path: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--cpu" => {
                cpu = Some(args.get(i + 1).cloned().ok_or_else(|| {
                    ZError::plain(
                        codes::SYNTAX,
                        "missing value for --cpu: `hone natgen <file.hn> --cpu <name>`",
                        None::<String>,
                    )
                })?);
                i += 2;
            }
            s if s.starts_with("--cpu=") => {
                cpu = Some(s["--cpu=".len()..].to_string());
                i += 1;
            }
            "--target" => {
                target = Some(args.get(i + 1).cloned().ok_or_else(|| {
                    ZError::plain(
                        codes::SYNTAX,
                        "missing value for --target: `hone natgen <file.hn> --target <triple>`",
                        None::<String>,
                    )
                })?);
                i += 2;
            }
            s if s.starts_with("--target=") => {
                target = Some(s["--target=".len()..].to_string());
                i += 1;
            }
            "-o" => {
                out = Some(args.get(i + 1).cloned().ok_or_else(|| {
                    ZError::plain(
                        codes::SYNTAX,
                        "missing value for -o: `hone natgen <file.hn> -o <out>`",
                        None::<String>,
                    )
                })?);
                i += 2;
            }
            "--obj" => {
                obj = Some(args.get(i + 1).cloned().ok_or_else(|| {
                    ZError::plain(
                        codes::SYNTAX,
                        "missing value for --obj: `hone natgen <file.hn> --obj <o>`",
                        None::<String>,
                    )
                })?);
                i += 2;
            }
            a if a.starts_with('-') => {
                return Err(ZError::plain(
                    codes::SYNTAX,
                    format!("unknown natgen option `{a}`"),
                    Some("usage: hone natgen <file.hn> [-o <out>] [--obj <o>] [--cpu <name>] [--target <triple>]"),
                ));
            }
            a => {
                path = Some(a.to_string());
                i += 1;
            }
        }
    }
    let path = path.ok_or_else(|| {
        ZError::plain(
            codes::SYNTAX,
            "missing script path: `hone natgen <file.hn>`",
            Some("usage: hone natgen <file.hn> [-o <out>] [--obj <o>]"),
        )
    })?;
    // T7：管线与 `hone build --exe --native` 共用（build_native）；natgen 保留为别名。
    build_native(&path, out, obj, "natgen", cpu, target)
}

/// 定位 rustc 自带的 rust-lld（G3.2 交叉目标裸链接用；位于 `<sysroot>/lib/rustlib/<host>/bin/`）。
fn rust_lld_path() -> Option<std::path::PathBuf> {
    let out = std::process::Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sysroot = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let host = target_lexicon::Triple::host().to_string();
    let exe = if cfg!(windows) { "rust-lld.exe" } else { "rust-lld" };
    let p = std::path::Path::new(&sysroot)
        .join("lib")
        .join("rustlib")
        .join(&host)
        .join("bin")
        .join(exe);
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

// ── G3.4 工具链打包（设计 §5.1：script.o + libhoneruntime.a + CRT → exe，目标机无 rustc）──
// 目录布局（targets/ 随 hone 分发；安装包解压即用）：
//   targets/<triple>/libhoneruntime.a   该目标的运行时 staticlib（rustc 预编译，见
//                                       tests/tools/build_toolchain.py）
//   targets/lld/rust-lld.exe            链接器（宿主工具，跨目标通用）
//   targets/crt/<triple>/               该目标的 CRT + 导入库（rustc sysroot
//                                       self-contained 目录拷贝：crt2.o / lib*.a）
// HONE_TOOLCHAIN_DIR 环境变量可覆盖 targets/ 位置（开发/嵌入式场景）。

/// 定位工具链目录：HONE_TOOLCHAIN_DIR → `<exe 目录>/targets/` → `./targets/`。
fn toolchain_dir() -> Option<std::path::PathBuf> {
    if let Ok(d) = std::env::var("HONE_TOOLCHAIN_DIR") {
        let p = std::path::PathBuf::from(d);
        if p.is_dir() {
            return Some(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let p = parent.join("targets");
            if p.is_dir() {
                return Some(p);
            }
        }
    }
    let p = std::path::PathBuf::from("targets");
    if p.is_dir() {
        Some(p)
    } else {
        None
    }
}

/// rustc 是否可用（决定走 rustc 链接还是打包工具链链接）。
fn rustc_available() -> bool {
    std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// G3.4：无 rustc 的打包链接（仅宿主 windows-gnu COFF；交叉目标的打包链接待 G3.6）。
/// 链接线与手动验证一致：crt2.o + libmingw32/libgcc/libgcc_eh/libmoldname/
/// libmingwex/libmsvcrt/libntdll/libpthread/libadvapi32/libws2_32/libkernel32/
/// libuser32/libbcrypt/libuserenv（std 与 runtime 的全部符号依赖）。
fn link_native_packaged(
    triple_s: &str,
    script_obj: &str,
    out: &str,
) -> Result<(), ZError> {
    let internal = |msg: String, hint: &str| {
        ZError::plain(codes::INTERNAL, msg, Some(hint.to_string()))
    };
    let tc = toolchain_dir().ok_or_else(|| {
        internal(
            "未找到打包工具链（targets/）且本机无 rustc".into(),
            "安装完整 hone 发行包（含 targets/），或设置 HONE_TOOLCHAIN_DIR 指向工具链目录，或安装 rustc",
        )
    })?;
    let lld = tc.join("lld").join(if cfg!(windows) { "rust-lld.exe" } else { "rust-lld" });
    let crt = tc.join("crt").join(triple_s);
    let rtlib = tc.join(triple_s).join("libhoneruntime.a");
    for (p, what) in [
        (&lld, "链接器 rust-lld"),
        (&crt, "CRT 目录"),
        (&rtlib, "运行时归档 libhoneruntime.a"),
    ] {
        if !p.exists() {
            return Err(internal(
                format!("工具链不完整：缺少 {what}: {}", p.display()),
                "重新安装 hone 发行包或运行 tests/tools/build_toolchain.py 重建",
            ));
        }
    }
    // 按 target 环境选链接 flavor：windows-gnu → COFF（lld-link）；musl/gnu ELF → GNU flavor。
    let triple: target_lexicon::Triple = triple_s
        .parse()
        .map_err(|e| internal(format!("目标三元组解析失败 `{triple_s}`: {e}"), "检查 --target 值"))?;
    let is_coff = matches!(triple.operating_system, target_lexicon::OperatingSystem::Windows);
    let mut args: Vec<std::ffi::OsString>;
    if is_coff {
        // COFF 链接（windows-gnu）：lld-link flavor。CRT/导入库为显式输入文件
        // （lld-link 的 -defaultlib 只认 .lib 名，不认 GNU 归档）。
        let libs = [
            "crt2.o", "libmingw32.a", "libgcc.a", "libgcc_eh.a", "libmoldname.a",
            "libmingwex.a", "libmsvcrt.a", "libntdll.a", "libpthread.a",
            "libadvapi32.a", "libws2_32.a", "libkernel32.a", "libuser32.a",
            "libbcrypt.a", "libuserenv.a",
        ];
        args = vec![
            "-flavor".into(), "link".into(),
            format!("/OUT:{}", out).into(),
            "/SUBSYSTEM:CONSOLE".into(),
            script_obj.into(),
            rtlib.into_os_string(),
        ];
        for l in libs {
            args.push(crt.join(l).into_os_string());
        }
    } else {
        // GNU/ELF 链接（musl / gnu）：crt1/crti/crtbegin → 产物段 → libunwind → libc/libm
        // → crtend/crtn。-no-pie：Cranelift 产非 PIC 对象（与 G3.2 rustc 路径口径一致，
        // 产物 ET_EXEC 静态）。CRT 归档为 targets/crt/<triple>/（self-contained 拷贝）。
        let libs_start = ["crt1.o", "crti.o", "crtbegin.o"];
        let libs_end = ["crtend.o", "crtn.o"];
        args = vec![
            "-flavor".into(), "gnu".into(),
            "-o".into(), out.into(),
            "-no-pie".into(), "--gc-sections".into(), "--strip-all".into(),
        ];
        for l in libs_start {
            args.push(crt.join(l).into_os_string());
        }
        args.push(script_obj.into());
        args.push(rtlib.into_os_string());
        args.push(crt.join("libunwind.a").into_os_string());
        args.push(format!("-L{}", crt.display()).into());
        args.push("-lc".into());
        args.push("-lm".into());
        for l in libs_end {
            args.push(crt.join(l).into_os_string());
        }
    }
    let p = std::process::Command::new(&lld)
        .args(&args)
        .output()
        .map_err(|e| internal(format!("链接器启动失败: {e}"), "检查 targets/lld/ 下的 rust-lld"))?;
    if !p.status.success() {
        let mut err = String::from_utf8_lossy(&p.stderr).to_string();
        if err.is_empty() {
            // 无 stderr（如 DLL 缺失等加载期失败）：至少给出退出码。
            err = format!("链接器退出码 {}", p.status.code().unwrap_or(-1));
        }
        return Err(internal(
            format!("链接失败:\n{err}"),
            "检查 script.o 与 libhoneruntime.a 的目标三元组一致性",
        ));
    }
    Ok(())
}

/// 共享原生编译管线（T7）：`hone natgen` 与 `hone build --exe --native` 共用入口。
/// 阶段：解析+类型检查 → VM 字节码 → Cranelift object（x86_64 COFF，含 `__hn_main`
/// 与各用户函数）→ 生成式 wrapper + rustc 链接原生 exe。
/// `label`：阶段输出前缀（"natgen" / "build"）；out/obj 为 None 时取默认名。
fn build_native(
    path: &str,
    out: Option<String>,
    obj: Option<String>,
    label: &str,
    cpu: Option<String>,
    target: Option<String>,
) -> Result<(), ZError> {
    // G3.2：目标三元组——默认宿主目标；`--target` 指定交叉目标（交叉产物需目标 std/运行时，见 G3.4）。
    let host = target_lexicon::Triple::host();
    let triple = match &target {
        Some(t) => t.parse::<target_lexicon::Triple>().map_err(|e| {
            ZError::plain(
                codes::SYNTAX,
                format!("无效的 --target `{t}`: {e}"),
                Some("例如 x86_64-pc-windows-gnu / x86_64-unknown-linux-musl"),
            )
        })?,
        None => host.clone(),
    };
    let triple_s = triple.to_string();
    let cross = triple_s != host.to_string();
    let src = std::fs::read_to_string(path).map_err(|e| {
        ZError::plain(
            codes::FILE_NOT_FOUND,
            format!("cannot read `{}`: {}", path, e),
            Some("check the path"),
        )
    })?;
    eprintln!("[1/4] {} 解析 + 类型检查: {}", label, path);
    let program = parser::Parser::parse(path, &src)?;
    // G3.1：原生编译后端在构建期把顶层 import 递归展开、合并为单编译单元（设计 §7.3）。
    let program = imports::resolve_imports(program, path, &src)?;
    let _checked = checker::Checker::check(&program, path, &src)?;
    let vm_mod = vm::compile_module(&program)?;
    // 字符串静态表（T5 起）去重全集，编入生成式 wrapper（object 经 rt_str 按索引取句柄；
    // M3 移入 object rodata，见 G3.5）。收集源：
    //   1. 全部 chunk 的 Const::Str（字符串字面量）；
    //   2. Call 指令函数名（rt_call 名分发用）；
    //   3. 每 chunk 的 locals 名 + func_map 函数名（注册表 / 局部变量优先分发用）。
    let mut strtab: Vec<String> = Vec::new();
    let mut strtab_idx: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut push_str = |s: &str| -> usize {
        if let Some(&i) = strtab_idx.get(s) {
            i
        } else {
            let i = strtab.len();
            strtab.push(s.to_string());
            strtab_idx.insert(s.to_string(), i);
            i
        }
    };
    for c in &vm_mod.chunks {
        for k in &c.consts {
            if let vm::Const::Str(s) = k {
                push_str(s);
            }
        }
        for ins in &c.code {
            if let vm::Instr::Call(_, func, _, _) = ins {
                push_str(func);
            }
            // T6：Field 字段名 / ThrowStr 文案进 strtab（rt_field / rt_throwstr 名分发用）。
            if let vm::Instr::Field(_, _, field) = ins {
                push_str(field);
            }
            // G2.6：GoCall callee 名进 strtab（rt_gocall 名分发用；go 目标可能从不作为
            // Call 名出现，漏收集会静默解析成 strtab[0]）。
            if let vm::Instr::GoCall(func, _, _) = ins {
                push_str(func);
            }
            if let vm::Instr::ThrowStr(s) = ins {
                push_str(s);
            }
            // M1：Raise 的错误码 / 消息 / help 文本进 strtab（rt_raise 名分发用）。
            if let vm::Instr::Raise(code, msg, help) = ins {
                push_str(code);
                push_str(msg);
                if let Some(h) = help {
                    push_str(h);
                }
            }
        }
        for name in c.locals.keys() {
            push_str(name);
        }
    }
    for name in vm_mod.func_map.keys() {
        push_str(name);
    }
    // G2.1：struct 定义表进 strtab（构造名分发 + 字段名序列 + `__struct__` 标记键）。
    // 标记键是隐藏键，脚本里不可能写出来，须在此显式收集（wrapper 侧 rt.rs 的
    // HIDDEN_STRUCT_KEY 常量与之一致，runtime 按内容匹配取 strtab 索引）。
    push_str("\u{0}__struct__");
    for (name, fields) in vm_mod.struct_defs.iter() {
        push_str(name);
        for f in fields {
            push_str(f);
        }
    }
    // G2.2：enum 定义表进 strtab（构造/匹配名分发 + 枚举名/变体名序列）。
    for (name, variants) in vm_mod.enum_defs.iter() {
        push_str(name);
        for (v, _) in variants {
            push_str(v);
        }
    }
    // G2.5：alias 的**原名**与新名都进 strtab。原名可能只出现在 alias 声明里
    // （`alias len as mylen` 的 `len` 从不作为 Call 名出现），不显式收集会拿到
    // 错误索引（unwrap_or(0)）→ runtime 把别名解析成无关的名字（症状：H002 说另一个函数名）。
    for (orig, new_name) in &vm_mod.aliases {
        push_str(orig);
        push_str(new_name);
    }
    drop(push_str);
    // 程序静态表（T4/G3.5）：span 表与 file/src 由 natgen emit_static_tables 直接
    // 编入 script.o rodata（此前经生成式 wrapper 传入，G3.5 起固定 wrapper）。
    let base = std::path::Path::new(&path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string());
    let obj = obj.unwrap_or_else(|| format!("{}.nat.o", base));
    let out = out.unwrap_or_else(|| {
        if cross {
            // 交叉产物默认名带目标后缀（设计 §7.1）。
            let ext = if matches!(triple.operating_system, target_lexicon::OperatingSystem::Windows) {
                ".exe"
            } else {
                ""
            };
            format!("{base}-{triple_s}{ext}")
        } else if cfg!(windows) {
            format!("{}.exe", base)
        } else {
            base
        }
    });
    eprintln!("[2/4] {} 生成 object: {}（Cranelift，目标 {}）", label, obj, triple_s);
    natgen::gen_object(&vm_mod, &strtab, &path, &src, std::path::Path::new(&obj), &triple_s, cpu.as_deref().unwrap_or("ssse3"))?;
    if cross {
        eprintln!("[3/4] {} 链接原生 exe: {}（rustc + 内嵌运行时，交叉目标 {}）", label, out, triple_s);
    } else {
        eprintln!("[3/4] {} 链接原生 exe: {}（rustc + 内嵌运行时）", label, out);
    }
    // 链接管线（干净构建机自足，无需本仓库）：
    //   临时目录/
    //     rt.rs    内嵌运行时（NAT_RUNTIME_SRC，含 rt_* 与 hn_entry 入口）
    //     main.rs  生成式 wrapper：include! rt.rs + 字符串表 + main 调 hn_entry
    //   rustc main.rs -C link-arg=obj.o → exe
    // object 提供 __hn_main（脚本 main chunk）与各用户函数；rustc 负责 CRT。
    let tmpdir = std::env::temp_dir().join(format!("hone_natgen_{}", std::process::id()));
    std::fs::create_dir_all(&tmpdir)
        .map_err(|e| ZError::plain(codes::INTERNAL, format!("创建临时构建目录: {e}"), None::<String>))?;
    let rt_rs = tmpdir.join("rt.rs");
    std::fs::write(&rt_rs, NAT_RUNTIME_SRC)
        .map_err(|e| ZError::plain(codes::INTERNAL, format!("写运行时源: {e}"), None::<String>))?;
    let main_rs = tmpdir.join("main.rs");
    // G3.5：wrapper 瘦身为脚本无关的固定代码——全部程序静态表已移入 script.o
    // rodata（natgen emit_static_tables 的 HN_* 符号），运行时 hn_entry 首行经
    // hn_boot（--cfg hone_boot）自举注册；CLI 参数解析同步移入 runtime。
    let mut wrapper = String::new();
    wrapper.push_str("// 固定 wrapper（G3.5）：include 运行时 + 入口；程序静态表在 script.o rodata。\n");
    wrapper.push_str("include!(\"rt.rs\");\n");
    wrapper.push_str("fn main() {\n");
    wrapper.push_str("    std::process::exit(hn_entry() as i32);\n");
    wrapper.push_str("}\n");
    std::fs::write(&main_rs, wrapper)
        .map_err(|e| ZError::plain(codes::INTERNAL, format!("写 wrapper: {e}"), None::<String>))?;
    let out_s = out.to_string();
    let obj_s = obj.to_string();
    // G3.3 体积裁剪：opt-level=z（体积优先）+ 单 codegen unit + 链接期 --gc-sections
    // （去未引用段）+ --strip-all（去符号表）。可用 HONE_NAT_FLAGS 覆盖（调试/实验用）。
    // 参数形态按**目标 + 链接器驱动**分派，而不是按「是否交叉」——宿主 Linux（glibc）
    // 与交叉 musl 同属 ELF，同为「Cranelift 非 PIC 对象 + 平台默认 PIE」这一组合。
    let is_elf = matches!(triple.operating_system, target_lexicon::OperatingSystem::Linux);
    let is_msvc = matches!(triple.environment, target_lexicon::Environment::Msvc);
    let default_flags = {
        let core = "-C opt-level=z -C codegen-units=1";
        let link = if cross {
            // 交叉：rustc 直接调 rust-lld（裸参数，lld 不认 gcc 驱动的 `-Wl,` 前缀）。
            // **必须**加 `-C relocation-model=static`：rustc 对 musl 目标默认按 static-PIE 选启动
            // CRT（`rcrt1.o` + `crtbeginS.o` + `-pie --no-dynamic-linker`），而 `-C link-arg=-no-pie`
            // 只改链接器末端的开关、改不了「选哪个 crt1」。两者混用会产出「rcrt1.o 自重定位启动 +
            // ET_EXEC（无 PT_DYNAMIC，`_DYNAMIC=0`）」的产物——`_start` 一扫 dynamic 段就解引用空指针
            // → SIGSEGV（CI 的 Linux nat 回归 16 例全 -11 且零输出即此）。static 后 rustc 选
            // `crt1.o`/`crtbegin.o`，入口与打包工具链路径（G3.4）逐字节一致。
            "-C relocation-model=static -C link-arg=--gc-sections -C link-arg=--strip-all -C link-arg=-no-pie"
        } else if is_msvc {
            // msvc 宿主：链接驱动是 link.exe，GNU/ld 风格参数（`-Wl,--gc-sections`）不适用。
            // 用 link.exe 自己的等价开关去未引用段 / 折叠相同函数。
            "-C link-arg=/OPT:REF -C link-arg=/OPT:ICF"
        } else if is_elf {
            // 宿主 ELF（Linux gnu / musl）：cc 驱动 + GNU ld，参数经 `-Wl,` 传；且 **必须** 非 PIE
            // ——Cranelift 产非 PIC 对象（含 R_X86_64_64/32S 绝对重定位），而 Linux 目标默认按
            // PIE 链接，会报 `relocation R_X86_64_32S ... can not be used when making a PIE
            // object; recompile with -fPIC`。`relocation-model=static` 让 rustc 自身不再追加
            // `-pie`，`link-arg=-no-pie` 再明确交给 cc（双保险，与 `-pie` 出现顺序无关）。
            "-C relocation-model=static -C link-arg=-no-pie -C link-arg=-Wl,--gc-sections -C link-arg=-Wl,--strip-all"
        } else {
            // windows-gnu 宿主：经 MinGW gcc 驱动链接，参数须 `-Wl,`。
            "-C link-arg=-Wl,--gc-sections -C link-arg=-Wl,--strip-all"
        };
        format!("{core} {link}")
    };

    let flags = std::env::var("HONE_NAT_FLAGS").unwrap_or(default_flags);
    let mut rustc_args: Vec<String> = vec!["--edition".into(), "2021".into(), "--cfg".into(), "hone_boot".into()];
    rustc_args.extend(flags.split_whitespace().map(str::to_string));
    // 交叉目标：显式传 --target（宿主目标不传，保持既有行为逐字节不变），并用 rustc
    // 自带 CRT（link-self-contained）+ rust-lld 裸链接（设计 §5.1/5.2）。
    if cross {
        rustc_args.push("--target".into());
        rustc_args.push(triple_s.clone());
        rustc_args.push("-C".into());
        rustc_args.push("link-self-contained=yes".into());
        if let Some(lld) = rust_lld_path() {
            rustc_args.push("-C".into());
            rustc_args.push(format!("linker={}", lld.display()));
        }
    }
    rustc_args.push("-o".into());
    rustc_args.push(out_s.clone());
    rustc_args.push(main_rs.to_str().unwrap().to_string());
    rustc_args.push("-C".into());
    rustc_args.push(format!("link-arg={}", obj_s));
    // G3.4：本机无 rustc 时回退打包工具链链接（script.o + libhoneruntime.a + CRT，
    // 设计 §5.1）。rustc 可用则维持既有 rustc 路径（逐字节回归基准不变）；
    // 打包路径当前仅覆盖宿主 windows-gnu COFF（交叉目标待 G3.6 CI 二次验证）。
    if !rustc_available() {
        // G3.4：rustc 缺失 → 打包工具链链接（COFF/ELF 双分支，见 link_native_packaged）。
        let packaged = link_native_packaged(&triple_s, &obj_s, &out_s);
        let _ = std::fs::remove_dir_all(&tmpdir);
        packaged?;
        if !std::path::Path::new(&out).is_file() {
            return Err(ZError::plain(
                codes::INTERNAL,
                "链接器报告成功但产物不存在",
                None::<String>,
            ));
        }
        eprintln!("[4/4] {} 完成: {}（工具链链接，无需 rustc）", label, out);
        return Ok(());
    }
    let p = std::process::Command::new("rustc")
        .args(&rustc_args)
        .output()
        .map_err(|e| {
            ZError::plain(
                codes::INTERNAL,
                format!("rustc 启动失败: {e}"),
                Some("需要本机 rustc 工具链"),
            )
        })?;
    let _ = std::fs::remove_dir_all(&tmpdir);
    if !p.status.success() {
        let err = String::from_utf8_lossy(&p.stderr);
        let hint = if cross {
            // 交叉失败的常见三类原因分开说：缺目标 std（rustup 组件未装）/ 缺 rust-lld /
            // glibc 目标不支持 `link-self-contained`（Linux 交叉只支持 musl，见设计 §一 非目标）。
            format!(
                "交叉目标 `{triple_s}` 需要该目标的 Rust std 与自带 CRT（`rustup target add {triple_s}`）\
                 ，以及 sysroot 内的 rust-lld；Linux 侧交叉请用 musl 目标（glibc 目标不在 CRT 分发矩阵内），\
                 或改用随 hone 分发的打包工具链（G3.4）"
            )
        } else {
            "检查 rustc 工具链与 object 目标三元组一致性".to_string()
        };
        return Err(ZError::plain(codes::INTERNAL, format!("链接失败:\n{err}"), Some(hint)));
    }
    if !std::path::Path::new(&out).is_file() {
        return Err(ZError::plain(
            codes::INTERNAL,
            "rustc 报告成功但产物不存在",
            None::<String>,
        ));
    }
    eprintln!("[4/4] {} 完成: {}", label, out);
    Ok(())
}

/// 末尾输出汇总；有失败即退出码非 0。
/// --json：stdout 输出诊断 JSON 数组（每失败文件一项，全通过为 []），文本信息走 stderr，
/// 便于 CI / 编辑器集成（`hone check src/ --json | jq`）。
fn cmd_check(args: &[String]) -> Result<(), ZError> {
    let json = args.iter().any(|a| a == "--json");
    let mut roots: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    let mut dir_roots = 0usize;
    for a in args {
        if a == "--json" {
            continue;
        }
        let meta = std::fs::metadata(a).map_err(|e| {
            ZError::plain(
                codes::NOT_FOUND,
                format!("cannot access `{}`: {}", a, e),
                Some("check the path"),
            )
        })?;
        roots.push(a.clone());
        if meta.is_dir() {
            dir_roots += 1;
            collect_check_files(a, &mut files)?;
        } else {
            files.push(a.clone());
        }
    }
    if roots.is_empty() {
        return Err(ZError::plain(
            codes::SYNTAX,
            "missing file or directory: `hone check <file.hn|directory> [--json]`",
            Some("example: `hone check src/` or `hone check mylib.hn --json`"),
        ));
    }
    files.sort();
    files.dedup();
    if files.is_empty() {
        println!("no .hn files found under `{}`", roots.join(", "));
        return Ok(());
    }

    let mut diags: Vec<serde_json::Value> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    for f in &files {
        let err = match std::fs::read_to_string(f) {
            Ok(src) => match run_check_one(f, &src) {
                Ok(()) => None,
                Err(e) => {
                    diags.push(check_diag_json(f, &e));
                    Some(e)
                }
            },
            Err(io) => {
                let e = ZError::plain(
                    codes::FILE_NOT_FOUND,
                    format!("cannot read `{}`: {}", f, io),
                    Some("check the path"),
                );
                diags.push(check_diag_json(f, &e));
                Some(e)
            }
        };
        match err {
            Some(e) => {
                if !json {
                    eprintln!("{}", e);
                }
                failed.push(f.clone());
            }
            None => {
                if !json {
                    println!("check OK: {}", f);
                }
            }
        }
    }

    if json {
        println!(
            "{}",
            serde_json::to_string(&diags).unwrap_or_else(|_| "[]".to_string())
        );
    } else if dir_roots > 0 || files.len() > 1 {
        println!();
        println!(
            "check: {} files, {} passed, {} failed",
            files.len(),
            files.len() - failed.len(),
            failed.len()
        );
    }
    if !failed.is_empty() {
        return Err(ZError::plain(
            codes::CHECK_FAILED,
            format!("{} of {} file(s) failed check", failed.len(), files.len()),
            Some("fix the diagnostics above; run `hone explain <code>` for an error code"),
        ));
    }
    Ok(())
}

/// 仅解析 + 类型检查（不执行），供 hone check 使用。
fn run_check_one(path: &str, src: &str) -> Result<(), ZError> {
    let program = parser::Parser::parse(path, src)?;
    checker::Checker::check(&program, path, src)?;
    Ok(())
}

/// ZError → JSON 诊断对象（`hone check --json` 用）。
fn check_diag_json(file: &str, e: &ZError) -> serde_json::Value {
    let shown = if e.file.is_empty() { file } else { e.file.as_str() };
    serde_json::json!({
        "file": shown,
        "code": e.code,
        "message": e.msg,
        "line": e.line,
        "col": e.col,
        "len": e.len,
        "line_text": e.line_text,
        "help": e.help,
    })
}

/// 递归收集目录下所有 .hn 文件（跳过隐藏目录与构建/缓存目录，按名排序保证确定性）。
fn collect_check_files(dir: &str, out: &mut Vec<String>) -> Result<(), ZError> {
    let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| {
            ZError::plain(
                codes::NOT_FOUND,
                format!("cannot read directory `{}`: {}", dir, e),
                Some("check the path"),
            )
        })?
        .map(|e| {
            e.map(|e| e.path())
                .map_err(|e| ZError::plain(codes::SYSCALL, format!("cannot read dir entry: {}", e), None::<&str>))
        })
        .collect::<Result<_, _>>()?;
    entries.sort();
    for path in entries {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if path.is_dir() {
            let skip = name.starts_with('.')
                || matches!(name.as_str(), "target" | "node_modules" | "dist" | "out");
            if !skip {
                collect_check_files(&path.to_string_lossy(), out)?;
            }
        } else if name.ends_with(".hn") {
            out.push(path.to_string_lossy().into_owned());
        }
    }
    Ok(())
}

/// 收集函数/类定义签名：从定义行起拼接到包含 `{` 的行（支持多行签名）。
fn collect_def_sig(lines: &[&str], start: usize) -> (String, String) {
    let mut sig = String::new();
    let mut name = String::new();
    for l in lines.iter().skip(start) {
        let t = l.trim();
        if name.is_empty() {
            let body = t
                .strip_prefix("tmp fn ")
                .or_else(|| t.strip_prefix("fn "))
                .or_else(|| t.strip_prefix("class "))
                .unwrap_or(t)
                .trim_start();
            name = body.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
        }
        if !sig.is_empty() {
            sig.push(' ');
        }
        sig.push_str(t);
        if t.contains('{') {
            break;
        }
    }
    // 去掉尾部 `{` 与多余空格
    let sig = sig.trim().trim_end_matches('{').trim_end().to_string();
    (name, sig)
}

/// hone test [目录]：递归扫描 `*.test.hn` 测试文件，逐个运行并汇总结果。
/// 测试文件用 assert(条件[, 消息]) 断言；任何文件解析/检查/运行失败均记为失败。
fn cmd_test(args: &[String]) -> Result<(), ZError> {
    let root = args.first().cloned().unwrap_or_else(|| ".".to_string());
    let mut files = Vec::new();
    collect_test_files(&root, &mut files)?;
    if files.is_empty() {
        println!("no *.test.hn files found under `{}`", root);
        return Ok(());
    }
    // 断言统计清零（assert / assert_eq 进程级累计，按文件差值显示）
    builtins::reset_assertions();
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut total_asserts = 0usize;
    for f in &files {
        let (ok0, fail0) = builtins::assertion_stats();
        match run_file(f, false, false) {
            Ok(()) => {
                let (ok, fail) = builtins::assertion_stats();
                println!("PASS  {} (断言 {}/{})", f, ok - ok0, ok - ok0 + fail - fail0);
                total_asserts += ok - ok0;
                passed += 1;
            }
            Err(e) => {
                let (ok, fail) = builtins::assertion_stats();
                println!("FAIL  {} (断言 {}/{}) -> {}", f, ok - ok0, ok - ok0 + fail - fail0, e);
                total_asserts += ok - ok0;
                failed += 1;
            }
        }
    }
    println!();
    println!("{} passed, {} failed ({} total, {} 断言通过)", passed, failed, passed + failed, total_asserts);
    if failed > 0 {
        return Err(ZError::plain(
            codes::ASSERT,
            format!("{} of {} test file(s) failed", failed, files.len()),
            None::<&str>,
        ));
    }
    Ok(())
}

/// hone watch <script.hn> [--interval=N]：监控脚本文件变更，自动重跑（跨平台轮询，零依赖）。
/// 轮询文件 mtime + 大小（std 实现，Windows/Linux/Termux 一致），变化即重新执行。
fn cmd_watch(args: &[String]) -> Result<(), ZError> {
    let path = args.first().ok_or_else(|| {
        ZError::plain(
            codes::SYNTAX,
            "missing script path: `hone watch <script.hn>`",
            Some("run `hone --help` for usage"),
        )
    })?;
    let mut interval_ms: u64 = 500;
    for a in &args[1..] {
        if let Some(v) = a.strip_prefix("--interval=") {
            interval_ms = v.parse().unwrap_or(500);
        }
    }
    interval_ms = interval_ms.max(50); // 下限 50ms，避免忙等
    let now_str = || {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        format!("{:02}:{:02}:{:02}", (t / 3600) % 24, (t / 60) % 60, t % 60)
    };
    let run_once = |path: &str| {
        println!("[{}] 运行 {}", now_str(), path);
        match run_file_or_pkg(path, false, false) {
            Ok(()) => println!("[{}] 运行成功", now_str()),
            Err(e) => println!("[{}] {}", now_str(), e),
        }
    };
    let fingerprint = |path: &str| -> Result<(u128, u64), ZError> {
        let md = std::fs::metadata(path).map_err(|e| {
            ZError::plain(
                codes::FILE_NOT_FOUND,
                format!("cannot stat `{}`: {}", path, e),
                Some("check the path"),
            )
        })?;
        let mtime = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Ok((mtime, md.len()))
    };
    run_once(path);
    let mut last = fingerprint(path)?;
    println!("watch: 监听 `{}` 变更（--interval={}ms，Ctrl+C 退出）", path, interval_ms);
    loop {
        std::thread::sleep(std::time::Duration::from_millis(interval_ms));
        match fingerprint(path) {
            Ok(fp) if fp != last => {
                last = fp;
                run_once(path);
            }
            // 文件被删除/不可读：提示后继续监听（恢复后重跑）
            Err(e) => eprintln!("[{}] {}", now_str(), e),
            _ => {}
        }
    }
}

/// 递归收集目录下所有以 `.test.hn` 结尾的文件。
fn collect_test_files(dir: &str, out: &mut Vec<String>) -> Result<(), ZError> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        ZError::plain(codes::NOT_FOUND, format!("cannot read directory `{}`: {}", dir, e), Some("check the path"))
    })?;
    for entry in entries {
        let path = entry.map_err(|e| ZError::plain(codes::SYSCALL, format!("cannot read dir entry: {}", e), None::<&str>))?.path();
        if path.is_dir() {
            collect_test_files(&path.to_string_lossy(), out)?;
        } else if path.to_string_lossy().ends_with(".test.hn") {
            out.push(path.to_string_lossy().into_owned());
        }
    }
    Ok(())
}

/// hone poop <file.hn>：屎山检测
fn cmd_poop(args: &[String]) -> Result<(), ZError> {
    let path = args.get(0).ok_or_else(|| {
        ZError::plain(
            codes::SYNTAX,
            "missing file: `hone poop <file.hn>`",
            Some("pass a .hn file to analyze, e.g. `hone poop mycode.hn`"),
        )
    })?;
    let code = std::fs::read_to_string(path).map_err(|e| {
        ZError::plain(codes::NOT_FOUND, format!("cannot read `{}`: {}", path, e), Some("check the path"))
    })?;
    let (max_depth, complexity) = analyze_poop(&code);
    println!("💩 屎山检测报告 💩");
    println!("  if 嵌套深度: {}", max_depth);
    println!("  圈复杂度:   {}", complexity);
    if max_depth >= 5 || complexity >= 15 {
        println!("  评级: 💩💩💩 危机！这是屎山！");
        if max_depth >= 5 {
            println!("  建议: 减少 if 嵌套，使用 return early 或模式匹配");
        } else {
            println!("  建议: 拆分函数，降低单函数复杂度");
        }
    } else if max_depth >= 3 || complexity >= 8 {
        println!("  评级: 💩💩 注意，代码需要重构");
    } else {
        println!("  评级: ✅ 代码质量良好，继续保持！");
    }
    Ok(())
}

/// hone prof <script.hn>：以剖析模式运行脚本，输出函数级热点报告。
fn cmd_prof(args: &[String]) -> Result<(), ZError> {
    let path = args.get(0).ok_or_else(|| {
        ZError::plain(
            codes::SYNTAX,
            "missing file: `hone prof <script.hn>`",
            Some("pass a .hn script to profile, e.g. `hone prof fib.hn`"),
        )
    })?;
    let src = std::fs::read_to_string(path).map_err(|e| {
        ZError::plain(codes::NOT_FOUND, format!("cannot read `{}`: {}", path, e), Some("check the path"))
    })?;
    let program = parser::Parser::parse(path, &src)?;
    checker::Checker::check(&program, path, &src)?;
    let data = interp::run_prof(&program, path, &src)?;
    print_prof_report(&data);
    Ok(())
}

/// 打印函数级剖析报告：占比/调用次数/总耗时/自耗时/平均耗时 + 总览 + 调用图。
fn print_prof_report(data: &interp::ProfData) {
    if data.is_empty() {
        println!("(未调用任何用户函数)");
        return;
    }
    let rows = data.sorted();
    let total_ns = data.total_ns() as f64;
    let total_calls = data.total_calls();
    println!("函数级热点报告（按总耗时降序）:");
    println!(
        "  {:<24} {:>8} {:>7} {:>12} {:>12} {:>12}",
        "函数", "调用次数", "占比", "总耗时", "自耗时", "平均耗时"
    );
    for (name, calls, total, avg, self_ns) in &rows {
        let pct = if total_ns > 0.0 { *total as f64 / total_ns * 100.0 } else { 0.0 };
        println!(
            "  {:<24} {:>8} {:>6.1}% {:>10.3}ms {:>10.3}ms {:>10.3}ms",
            name,
            calls,
            pct,
            *total as f64 / 1_000_000.0,
            *self_ns as f64 / 1_000_000.0,
            *avg as f64 / 1_000_000.0
        );
    }
    println!(
        "  总耗时 {:.3}ms，总调用 {} 次（占比按总耗时计，自耗时不含子调用）",
        total_ns / 1_000_000.0,
        total_calls
    );
    // 调用图
    let edges = data.edges();
    if !edges.is_empty() {
        println!("调用图（调用方 → 被调方 → 次数，按次数降序）:");
        for (a, b, n) in &edges {
            println!("  {} → {} × {}", a, b, n);
        }
    }
}

/// 分析源码中的 if 嵌套深度和圈复杂度
fn analyze_poop(code: &str) -> (usize, usize) {
    let mut max_depth = 0usize;
    let mut cur_depth = 0usize;
    let mut complexity = 1usize;
    let mut in_string = false;
    let mut prev_c = ' ';
    let chars: Vec<char> = code.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];

        if c == '"' && prev_c != '\\' {
            in_string = !in_string;
            prev_c = c;
            i += 1;
            continue;
        }
        if in_string {
            prev_c = c;
            i += 1;
            continue;
        }

        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
            while i < chars.len() && chars[i] != '\n' { i += 1; }
            continue;
        }
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') { i += 1; }
            i += 2;
            continue;
        }

        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') { i += 1; }
            let word: String = chars[start..i].iter().collect();
            match word.as_str() {
                "if" | "else if" | "for" | "while" | "case" | "catch" | "&&" | "||" => complexity += 1,
                _ => {}
            }
            if word == "if" {
                cur_depth += 1;
                if cur_depth > max_depth { max_depth = cur_depth; }
            }
            continue;
        }

        if c == '}' {
            cur_depth = cur_depth.saturating_sub(1);
        }

        prev_c = c;
        i += 1;
    }

    (max_depth, complexity)
}
