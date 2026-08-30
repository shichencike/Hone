// main.rs - Hone 命令行入口（单文件 hone / hone.exe）
// 命令：hone <script.hn>（默认）、hone run、hone debug、--help、--version

mod ast;
mod archmod;
mod aot;
mod builtins;
mod bundle;
mod checker;
mod codegen;
mod datamod;
mod error;
mod fmt;
mod guimod;
#[cfg(not(windows))]
mod guimod_gtk;
mod guimod_x11;
mod header;
mod interp;
mod lexer;
mod lsp;
mod netmod;
mod parser;
mod pluginmod;
mod plotmod;
mod ptrmod;
mod repl;
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
            builtins::init_args(&rest[1..]);
            match opts.restart {
                Some(p) => run_with_restart(path, &p),
                None => run_file_or_pkg(path, false),
            }
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
            run_file(path, true)
        }
        "fmt" => cmd_fmt(&args[1..]),
        "doc" => cmd_doc(&args[1..]),
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
            builtins::init_args(&args[1..]);
            run_file_or_pkg(other, false)
        }
        other => Err(ZError::plain(
            codes::SYNTAX,
            format!("unknown command `{}`", other),
            Some("run `hone --help` for usage"),
        )),
    }
}

/// 执行一个 .hn 脚本：读取 → 解析 → 类型检查 → 解释执行。
fn run_file(path: &str, debug: bool) -> Result<(), ZError> {
    let src = std::fs::read_to_string(path).map_err(|e| {
        ZError::plain(
            codes::FILE_NOT_FOUND,
            format!("cannot read `{}`: {}", path, e),
            Some("check the path"),
        )
    })?;
    run_script(path, &src, debug)
}

/// 执行脚本或仅脚本包（.hzp）：先尝试解包，不是包则按普通 .hn 执行。
fn run_file_or_pkg(path: &str, debug: bool) -> Result<(), ZError> {
    let data = std::fs::read(path).map_err(|e| {
        ZError::plain(
            codes::FILE_NOT_FOUND,
            format!("cannot read `{}`: {}", path, e),
            Some("check the path"),
        )
    })?;
    if let Some((name, script)) = bundle::parse_script_pkg(&data) {
        // 包内脚本名作为展示名；load/import 相对路径仍以包文件所在目录为基准
        run_script(&name, &script, debug)
    } else {
        let src = String::from_utf8_lossy(&data).into_owned();
        run_script(path, &src, debug)
    }
}

/// 对已读取的源码执行完整流程：解析 → 类型检查 → 解释执行。
fn run_script(path: &str, src: &str, debug: bool) -> Result<(), ZError> {
    let program = parser::Parser::parse(path, src)?;
    checker::Checker::check(&program, path, src)?;
    interp::run(&program, path, src, debug)?;
    Ok(())
}

/// `--restart` 重启策略：最大重启次数、递增等待间隔（秒）、可重启错误码白名单（空 = 全部可重启）。
struct RestartPolicy {
    max: usize,
    backoff: Vec<u64>,
    codes: Vec<String>,
}

/// `hone run` 的运行选项：重启策略（可选）与是否恢复检查点。
struct RunOptions {
    restart: Option<RestartPolicy>,
    resume: bool,
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
    let mut rest = Vec::new();
    let mut parsing_opts = true;

    for a in args {
        if parsing_opts {
            match a.as_str() {
                "--restart" => {
                    has_restart = true;
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
    (RunOptions { restart, resume }, rest)
}

/// 按策略循环运行脚本：正常结束（Ok）立即返回；错误按白名单与次数上限重试，
/// 等待间隔取 backoff 序列（第 n 次失败后等待 backoff[n]，超出取最后一项）。
fn run_with_restart(path: &str, policy: &RestartPolicy) -> Result<(), ZError> {
    let mut count = 0usize;
    loop {
        match run_file(path, false) {
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
    println!("       --restart[=N]       失败自动重启（N 为最大次数，默认 3；仅对可恢复错误）");
    println!("       --backoff=a,b,c     重启间隔递增序列（秒，默认 1,3,10）");
    println!("       --restart-on=Hxxx   只对指定错误码重启（逗号分隔；省略则全部可重启）");
    println!("       --resume            恢复上次 db 检查点（脚本变更后自动失效）");
    println!("  hone explain <code>       查看错误码解释（如 `hone explain H201`）");
    println!("  hone bind <header.h>      从 C 头文件生成 FFI 签名块（typed load 用）");
    println!("  hone debug <script.hn>   断点调试模式（breakpoint 关键字生效）");
    println!("  hone fmt [-w|-c] <file.hn>  代码格式化（Tab 缩进/运算符空格/大括号；-w 覆盖写，-c 只检查差异）");
    println!("  hone doc <file.hn>        从 fn/class 定义与上方 `//` 注释生成 Markdown API 文档");
    println!("  hone build --dll <file.hn> 将脚本打包为 C ABI 动态库（int/float/bool/str 映射，需 C 编译器）");
    println!("  hone build --exe <file.hn> 将脚本与解释器打包为独立可执行文件（[-o <out>] [--icon <ico>]）");
    println!("  hone build --exe -c <file.hn> AOT 编译为原生可执行文件（脚本→C 中间文件→gcc/clang；默认删 .c，--keep-c 保留）");
    println!("  hone build --script <file.hn> 生成仅脚本压缩包 .hzp（不内嵌解释器，[-o <out>]，用 hone run 执行）");
    println!("  hone get                   读取当前目录 hone.json 清单，批量下载全部模块");
    println!("  hone get <module> <url>  下载模块依赖并缓存到 ~/.hone/cache/，并写入 hone.json 清单");
    println!("  hone get <script.hn>     预下载脚本中所有 import 声明的模块");
    println!("  hone watch <script.hn>   监控脚本文件变更自动重跑（[--interval=N] 毫秒，默认 500，Ctrl+C 退出）");
    println!("  hone prof <script.hn>    以剖析模式运行脚本，输出函数级热点报告（总耗时/调用次数/平均耗时）");
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

/// hone build --exe <script.hn> [-o <out>] [--icon <ico>] [--version]
/// [-c] [--keep-c]
/// 默认：将当前 hone 运行时与脚本打包为单个自释放可执行文件（见 bundle.rs 格式）。
/// -c：AOT 模式——脚本编译为 C 中间文件，再用 C 编译器生成原生可执行文件；
///     生成前先做语法/类型检查；默认删除中间 .c，--keep-c 保留。
fn cmd_build_exe(args: &[String]) -> Result<(), ZError> {
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("hone build --exe (Hone v{})", VERSION);
        return Ok(());
    }
    let mut out: Option<String> = None;
    let mut icon: Option<String> = None;
    let mut path: Option<String> = None;
    let mut aot_mode = false;
    let mut keep_c = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-c" => aot_mode = true,
            "--keep-c" => keep_c = true,
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
            s if s.starts_with('-') => {
                return Err(ZError::plain(
                    codes::SYNTAX,
                    format!("unknown build option `{}`", s),
                    Some("options: `-c`, `--keep-c`, `-o <out>`, `--icon <ico>`, `--version`"),
                ));
            }
            s => {
                if path.is_none() {
                    path = Some(s.to_string());
                } else {
                    return Err(ZError::plain(
                        codes::SYNTAX,
                        "too many arguments",
                        Some("usage: `hone build --exe <script.hn> [-c] [-o <out>]`"),
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
    if aot_mode {
        return cmd_build_exe_aot(&path, out, keep_c);
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

/// hone build --exe -c <script.hn> [-o <out>] [--keep-c]
/// AOT 编译：脚本 → C 中间文件 → C 编译器 → 原生可执行文件。
/// 生成前先做语法与类型检查（用户要求），重内置在生成期报错。
fn cmd_build_exe_aot(path: &str, out: Option<String>, keep_c: bool) -> Result<(), ZError> {
    let src = std::fs::read_to_string(path).map_err(|e| {
        ZError::plain(
            codes::FILE_NOT_FOUND,
            format!("cannot read `{}`: {}", path, e),
            Some("check the path"),
        )
    })?;

    // [1/4] 解析与语法/类型检查（生成前必须检查脚本语法）
    print!("[1/4] 解析与类型检查...\r");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let program = parser::Parser::parse(path, &src)?;
    checker::Checker::check(&program, path, &src)?;

    // [2/4] 生成 C 中间文件
    print!("[2/4] 生成 C 中间文件...\r");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let c_code = aot::generate(&program, path, &src)?;

    let stem = std::path::Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "app".to_string());
    let cfile = format!("{}.c", stem);
    std::fs::write(&cfile, &c_code).map_err(|e| {
        ZError::plain(
            codes::NOT_FOUND,
            format!("cannot write `{}`: {}", cfile, e),
            Some("check the directory permissions"),
        )
    })?;

    // [3/4] 查找 C 编译器
    print!("[3/4] 查找 C 编译器...\r");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let cc = match find_cc() {
        Ok(cc) => cc,
        Err(_) => {
            println!();
            if keep_c {
                eprintln!("[build] 中间文件保留在 `{}`", cfile);
            }
            return Err(ZError::plain(
                codes::NOT_IMPLEMENTED,
                "no C compiler found (gcc/clang), cannot compile the AOT executable",
                Some(format!(
                    "install gcc (e.g. MinGW-w64 on Windows), or set the `CC` environment variable; the generated C source is kept at `{}`",
                    cfile
                )),
            ));
        }
    };

    // [4/4] 编译为原生可执行文件（-Os + 函数节 + 链接期死代码消除 → 小体积）
    print!("[4/4] 编译中（{}）...\r", cc);
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let out = match out {
        Some(o) => o,
        None => format!("{}.exe", stem),
    };
    let result = run_cc_exe(&cc, &cfile, &out);
    if !keep_c {
        std::fs::remove_file(&cfile).ok();
    }
    result?;

    let size = std::fs::metadata(&out)
        .map(|m| m.len() as f64 / 1024.0)
        .unwrap_or(0.0);
    println!();
    println!("生成 {} 完成（AOT 原生编译, {:.1} KB）", out, size);
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

/// AOT 可执行文件编译：-Os + 函数/数据节 + 链接期死代码消除 → 几十 KB 级体积。
/// 编译后校验输出文件确实生成（个别工具链报错时退出码仍为 0，如 Educe 的 clang 包装）。
fn run_cc_exe(cc: &str, cfile: &str, out: &str) -> Result<(), ZError> {
    let status = std::process::Command::new(cc)
        .args([
            "-Os",
            "-ffunction-sections",
            "-fdata-sections",
            "-o",
            out,
            cfile,
            "-Wl,--gc-sections",
        ])
        .status();
    match status {
        Ok(s) if s.success() => {
            if std::path::Path::new(out).exists() {
                Ok(())
            } else {
                Err(ZError::plain(
                    codes::NOT_IMPLEMENTED,
                    "C compiler reported success but produced no output file",
                    Some("check the C toolchain (e.g. ccache/缓存包装导致无产物), or set the `CC` environment variable"),
                ))
            }
        }
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
        match run_file(f, false) {
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
        match run_file_or_pkg(path, false) {
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
