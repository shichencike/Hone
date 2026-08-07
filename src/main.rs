// main.rs - Zap 命令行入口（单文件 zap / zap.exe）
// 命令：zap <script.zp>（默认）、zap run、zap debug、--help、--version

mod ast;
mod builtins;
mod bundle;
mod checker;
mod codegen;
mod error;
mod fmt;
mod interp;
mod lexer;
mod lsp;
mod parser;
mod srvmod;
mod sysmod;
mod upgrade;

use std::collections::HashMap;
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

    // 打包模式：自身携带内嵌脚本 → 走自释放启动器（--version / 释放执行 / 清理缓存）
    match bundle::detect() {
        Ok(Some(info)) => return bundle::run(&info, &args),
        Ok(None) => {}
        Err(e) => {
            eprintln!("{}", e);
            return ExitCode::FAILURE;
        }
    }

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
            let (opts, rest) = parse_run_args(&args[1..]);
            let path = rest
                .first()
                .ok_or_else(|| {
                    ZError::plain(
                        codes::SYNTAX,
                        "missing script path: `zap run <script.zp>`",
                        Some("run `zap --help` for usage"),
                    )
                })?;
            if opts.resume {
                load_resume_state(path)?;
            }
            builtins::init_args(&rest[1..]);
            match opts.restart {
                Some(p) => run_with_restart(path, &p),
                None => run_file(path, false),
            }
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
            builtins::init_args(&args[2..]);
            run_file(path, true)
        }
        "fmt" => cmd_fmt(&args[1..]),
        "build" => cmd_build(&args[1..]),
        "get" => cmd_get(&args[1..]),
        "upgrade" => upgrade::cmd_upgrade(&args[1..]),
        "lsp" => lsp::run_lsp(),
        "poop" => cmd_poop(&args[1..]),
        "explain" => {
            let code = args
                .get(1)
                .ok_or_else(|| {
                    ZError::plain(
                        codes::SYNTAX,
                        "missing error code: `zap explain <code>`",
                        Some("example: `zap explain Z201`"),
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
                    Some("run `zap explain` with a Zxxx code listed in the docs"),
                )),
            }
        }
        other if other.ends_with(".zp") => {
            builtins::init_args(&args[1..]);
            run_file(other, false)
        }
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
            codes::FILE_NOT_FOUND,
            format!("cannot read `{}`: {}", path, e),
            Some("check the path"),
        )
    })?;
    let program = parser::Parser::parse(path, &src)?;
    checker::Checker::check(&program, path, &src)?;
    interp::run(&program, path, &src, debug)?;
    Ok(())
}

/// `--restart` 重启策略：最大重启次数、递增等待间隔（秒）、可重启错误码白名单（空 = 全部可重启）。
struct RestartPolicy {
    max: usize,
    backoff: Vec<u64>,
    codes: Vec<String>,
}

/// `zap run` 的运行选项：重启策略（可选）与是否恢复检查点。
struct RunOptions {
    restart: Option<RestartPolicy>,
    resume: bool,
}

/// 从 `zap run` 的参数中提取运行选项。
/// 返回 (选项, 剩余参数)；剩余参数中第一个为脚本路径，其余为脚本自身的参数（原样传递）。
/// 已知选项 `--restart[=N]` / `--backoff=a,b,c` / `--restart-on=Zxxx` / `--resume` 被消费，
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

/// ~/.zap/state/ 状态目录（Windows 用 USERPROFILE）。
fn state_dir() -> std::path::PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(home).join(".zap").join("state")
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
    println!("Zap v{} - 轻量级、跨平台、可嵌入的脚本语言", VERSION);
    println!();
    println!("用法:");
    println!("  zap <script.zp>         执行 Zap 脚本（默认命令）");
    println!("  zap run <script.zp>     执行 Zap 脚本");
    println!("       --restart[=N]       失败自动重启（N 为最大次数，默认 3；仅对可恢复错误）");
    println!("       --backoff=a,b,c     重启间隔递增序列（秒，默认 1,3,10）");
    println!("       --restart-on=Zxxx   只对指定错误码重启（逗号分隔；省略则全部可重启）");
    println!("       --resume            恢复上次 db 检查点（脚本变更后自动失效）");
    println!("  zap explain <code>       查看错误码解释（如 `zap explain Z201`）");
    println!("  zap debug <script.zp>   断点调试模式（breakpoint 关键字生效）");
    println!("  zap fmt [-w] <file.zp>  代码格式化（统一 Tab 缩进、运算符空格、大括号位置；-w 覆盖写）");
    println!("  zap build --dll <file.zp> 将脚本打包为 C ABI 动态库（int/float/bool/str 映射，需 C 编译器）");
    println!("  zap build --exe <file.zp> 将脚本与解释器打包为独立可执行文件（[-o <out>] [--icon <ico>]）");
    println!("  zap get <module> <url>  下载模块依赖并缓存到 ~/.zap/cache/");
    println!("  zap get <script.zp>     预下载脚本中所有 import 声明的模块");
    println!("  zap upgrade [-w] <file.zp> 按映射表自动迁移旧版本语法（-w 覆盖写）");
    println!("  zap lsp                 启动语言服务器（补全/诊断，LSP over stdio）");
    println!("  zap --help              显示帮助");
    println!("  zap --version           显示版本");
    println!();
    println!("可视化编辑器：浏览器打开 editor/index.html（拖拽代码块生成 .zp 代码）");
}

/// zap build --dll <script.zp> / zap build --exe <script.zp>
fn cmd_build(args: &[String]) -> Result<(), ZError> {
    match args.first().map(|s| s.as_str()) {
        Some("--dll") => {
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
        Some("--exe") => cmd_build_exe(&args[1..]),
        _ => Err(ZError::plain(
            codes::SYNTAX,
            "unknown build options: `zap build --dll <script.zp>` or `zap build --exe <script.zp>`",
            Some("`--dll` compiles to a shared library; `--exe` bundles the script with the interpreter"),
        )),
    }
}

/// zap build --exe <script.zp> [-o <out>] [--icon <ico>] [--version]
/// 将当前 zap 运行时与脚本打包为单个自释放可执行文件（见 bundle.rs 格式）。
fn cmd_build_exe(args: &[String]) -> Result<(), ZError> {
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("zap build --exe (Zap v{})", VERSION);
        return Ok(());
    }
    let mut out: Option<String> = None;
    let mut icon: Option<String> = None;
    let mut path: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
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
                    Some("options: `-o <out>`, `--icon <ico>`, `--version`"),
                ));
            }
            s => {
                if path.is_none() {
                    path = Some(s.to_string());
                } else {
                    return Err(ZError::plain(
                        codes::SYNTAX,
                        "too many arguments",
                        Some("usage: `zap build --exe <script.zp> [-o <out>]`"),
                    ));
                }
            }
        }
        i += 1;
    }
    let path = path.ok_or_else(|| {
        ZError::plain(
            codes::SYNTAX,
            "missing script path: `zap build --exe <script.zp>`",
            Some("run `zap --help` for usage"),
        )
    })?;
    if let Some(ic) = &icon {
        eprintln!("[build] warning: `--icon` is not supported in this build, ignoring `{}`", ic);
    }

    // 以当前 zap 可执行文件作为内嵌运行时
    let exe_bytes = std::fs::read(std::env::current_exe().map_err(|e| {
        ZError::plain(
            codes::NOT_FOUND,
            format!("cannot locate the zap runtime: {}", e),
            None::<&str>,
        )
    })?)
    .map_err(|e| {
        ZError::plain(
            codes::NOT_FOUND,
            format!("cannot read the zap runtime: {}", e),
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
        .unwrap_or_else(|| "script.zp".to_string());

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
        "生成 {} 完成（脚本: {}, Zap v{}.{}.{}, {:.1} KB）",
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

/// zap poop <file.zp>：屎山检测
fn cmd_poop(args: &[String]) -> Result<(), ZError> {
    let path = args.get(0).ok_or_else(|| {
        ZError::plain(
            codes::SYNTAX,
            "missing file: `zap poop <file.zp>`",
            Some("pass a .zp file to analyze, e.g. `zap poop mycode.zp`"),
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
