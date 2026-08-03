// main.rs - Zap 命令行入口（单文件 zap / zap.exe）
// 命令：zap <script.zp>（默认）、zap run、zap debug、--help、--version

mod ast;
mod builtins;
mod checker;
mod codegen;
mod error;
mod fmt;
mod interp;
mod lexer;
mod lsp;
mod parser;
mod sysmod;
mod upgrade;

use std::process::ExitCode;

use error::codes;
use error::ZError;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    // 管道被提前关闭（如 `zap --help | head`）时，不打印 broken pipe 的 panic 堆栈
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
    match run_cli(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}", e);
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
            println!("zap {}", VERSION);
            Ok(())
        }
        "run" => {
            let path = args
                .get(1)
                .ok_or_else(|| {
                    ZError::plain(
                        codes::SYNTAX,
                        "missing script path: `zap run <script.zp>`",
                        Some("run `zap --help` for usage"),
                    )
                })?;
            run_file(path, false)
        }
        "debug" => {
            let path = args
                .get(1)
                .ok_or_else(|| {
                    ZError::plain(
                        codes::SYNTAX,
                        "missing script path: `zap debug <script.zp>`",
                        Some("run `zap --help` for usage"),
                    )
                })?;
            run_file(path, true)
        }
        "fmt" => cmd_fmt(&args[1..]),
        "build" => cmd_build(&args[1..]),
        "get" => cmd_get(&args[1..]),
        "upgrade" => upgrade::cmd_upgrade(&args[1..]),
        "lsp" => lsp::run_lsp(),
        other if other.ends_with(".zp") => run_file(other, false),
        other => Err(ZError::plain(
            codes::SYNTAX,
            format!("unknown command `{}`", other),
            Some("run `zap --help` for usage"),
        )),
    }
}

/// 执行一个 .zp 脚本：读取 → 解析 → 类型检查 → 解释执行。
fn run_file(path: &str, debug: bool) -> Result<(), ZError> {
    let src = std::fs::read_to_string(path).map_err(|e| {
        ZError::plain(
            codes::NOT_FOUND,
            format!("cannot read `{}`: {}", path, e),
            Some("check the path"),
        )
    })?;
    let program = parser::Parser::parse(path, &src)?;
    checker::Checker::check(&program, path, &src)?;
    interp::run(&program, path, &src, debug)?;
    Ok(())
}

fn print_help() {
    println!("Zap v{} - 轻量级、跨平台、可嵌入的脚本语言", VERSION);
    println!();
    println!("用法:");
    println!("  zap <script.zp>         执行 Zap 脚本（默认命令）");
    println!("  zap run <script.zp>     执行 Zap 脚本");
    println!("  zap debug <script.zp>   断点调试模式（breakpoint 关键字生效）");
    println!("  zap fmt [-w] <file.zp>  代码格式化（统一 Tab 缩进、运算符空格、大括号位置；-w 覆盖写）");
    println!("  zap build --dll <file.zp> 将脚本打包为 C ABI 动态库（int/float/bool/str 映射，需 C 编译器）");
    println!("  zap get <module> <url>  下载模块依赖并缓存到 ~/.zap/cache/");
    println!("  zap get <script.zp>     预下载脚本中所有 import 声明的模块");
    println!("  zap upgrade [-w] <file.zp> 按映射表自动迁移旧版本语法（-w 覆盖写）");
    println!("  zap lsp                 启动语言服务器（补全/诊断，LSP over stdio）");
    println!("  zap --help              显示帮助");
    println!("  zap --version           显示版本");
    println!();
    println!("可视化编辑器：浏览器打开 editor/index.html（拖拽代码块生成 .zp 代码）");
}

/// zap build --dll <script.zp>
fn cmd_build(args: &[String]) -> Result<(), ZError> {
    if args.first().map(|s| s.as_str()) != Some("--dll") {
        return Err(ZError::plain(
            codes::SYNTAX,
            "unknown build options: `zap build --dll <script.zp>`",
            Some("only `--dll` is supported in this version"),
        ));
    }
    let path = args
        .get(1)
        .ok_or_else(|| {
            ZError::plain(
                codes::SYNTAX,
                "missing script path: `zap build --dll <script.zp>`",
                Some("run `zap --help` for usage"),
            )
        })?;
    cmd_build_dll(path)
}

/// 将 .zp 脚本打包为 C ABI 动态库。进度条使用 \r 轻量显示。
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
        .unwrap_or_else(|| "zap_lib".to_string());
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

/// zap get <module> <url> | zap get <script.zp>
/// 远程下载模块依赖并缓存到 ~/.zap/cache/（进度条 \r 显示）。
fn cmd_get(args: &[String]) -> Result<(), ZError> {
    match args.len() {
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
            Ok(())
        }
        _ => Err(ZError::plain(
            codes::SYNTAX,
            "usage: `zap get <module> <url>` or `zap get <script.zp>`",
            Some("run `zap --help` for usage"),
        )),
    }
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

/// 下载模块并写入缓存 ~/.zap/cache/<name>.zp（已缓存则跳过）。
fn fetch_and_cache(name: &str, url: &str) -> Result<(), ZError> {
    let cache_file = interp::zap_cache_dir().join(format!("{}.zp", name));
    if cache_file.exists() {
        let size = std::fs::metadata(&cache_file).map(|m| m.len()).unwrap_or(0);
        println!("已缓存: {} ({} 字节)", name, size);
        return Ok(());
    }
    print!("\r[zap get] 下载 `{}` ...", name);
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let span = lexer::Span { line: 1, col: 1, len: 1 };
    let code = builtins::http_request(url, "GET", None, span, name, "")?;
    println!();
    if let Some(dir) = cache_file.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| ZError::plain(codes::NOT_FOUND, format!("cannot create cache dir: {}", e), None::<&str>))?;
    }
    std::fs::write(&cache_file, &code)
        .map_err(|e| ZError::plain(codes::NOT_FOUND, format!("cannot write cache file: {}", e), None::<&str>))?;
    println!("已下载并缓存: {} ({} 字节)", name, code.len());
    Ok(())
}

/// zap fmt [-w] <file.zp>...：格式化到 stdout，或 -w 覆盖写入源文件。
fn cmd_fmt(args: &[String]) -> Result<(), ZError> {
    let mut overwrite = false;
    let mut files = Vec::new();
    for a in args {
        if a == "-w" || a == "--write" {
            overwrite = true;
        } else {
            files.push(a.clone());
        }
    }
    if files.is_empty() {
        return Err(ZError::plain(
            codes::SYNTAX,
            "missing file: `zap fmt [-w] <file.zp>...`",
            Some("pass one or more .zp files, e.g. `zap fmt -w *.zp`"),
        ));
    }
    for f in files {
        let src = std::fs::read_to_string(&f).map_err(|e| {
            ZError::plain(codes::NOT_FOUND, format!("cannot read `{}`: {}", f, e), Some("check the path"))
        })?;
        let formatted = fmt::format(&src)?;
        if overwrite {
            std::fs::write(&f, formatted).map_err(|e| {
                ZError::plain(codes::NOT_FOUND, format!("cannot write `{}`: {}", f, e), Some("check the path"))
            })?;
        } else {
            print!("{}", formatted);
        }
    }
    Ok(())
}
