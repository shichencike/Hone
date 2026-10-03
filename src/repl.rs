// repl.rs - Hone 交互式解释器（类似 Python REPL）
// hone repl：逐行读取 → 自动续行（未闭合大括号/括号）→ 解析执行。
// 跨输入共享变量与函数定义；表达式语句自动回显结果（Python 式行为）。

use std::io::{self, BufRead, Write};

use crate::ast::Program;
use crate::error::ZError;
use crate::interp::{Env, Interp, Value};
use crate::lexer::{Lexer, Tok};
use crate::parser::Parser;

const PROMPT: &str = ">>> ";
const CONT: &str = "... ";

/// hone repl 入口：进入交互模式，直到 exit()/quit()/.exit 或 EOF（Ctrl+D/Ctrl+Z）。
pub fn run_repl(args: &[String]) -> Result<(), ZError> {
    let _ = args; // 暂无子参数
    println!(
        "hone {} REPL — exit()/quit()/.exit 退出, .help 帮助, .vars 查看变量",
        env!("CARGO_PKG_VERSION")
    );

    let stdin = io::stdin();
    let mut ip = Interp::new("<repl>", "", false);
    let mut env = Env::new();
    let mut buffer = String::new();

    loop {
        // 有新输入时显示 >>>，多行续行显示 ...
        print!("{}", if buffer.is_empty() { PROMPT } else { CONT });
        io::stdout().flush().ok();

        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => {
                // EOF：Ctrl+D（Unix）/ Ctrl+Z+Enter（Windows），结束交互
                println!();
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
        let line = line.trim_end_matches(['\n', '\r']);

        // 点命令与函数式退出只在单行输入（非续行）时生效
        if buffer.is_empty() {
            match line {
                ".help" => {
                    print_help();
                    continue;
                }
                ".exit" | ".quit" => break,
                ".vars" => {
                    print_vars(&env, &ip);
                    continue;
                }
                "exit()" | "quit()" => break,
                _ => {}
            }
            if line.starts_with('.') && line.len() > 1 {
                println!("未知命令 `{}`（.help 查看帮助）", line);
                continue;
            }
        }

        // 空行：单行时跳过；续行中保留换行以维持语义（如多行字符串）
        if line.is_empty() && buffer.is_empty() {
            continue;
        }
        if !buffer.is_empty() {
            buffer.push('\n');
        }
        buffer.push_str(line);

        // 尚未闭合（大括号/括号不平衡或词法不完整）→ 继续读下一行
        if is_incomplete(&buffer) {
            continue;
        }

        let chunk = std::mem::take(&mut buffer);
        // Hone 语句以 `;` 结尾；块/函数定义以 `}` 结尾，不追加
        let mut src = chunk.clone();
        if !src.trim_end().ends_with(['}', ';']) {
            src.push(';');
        }
        // 解析：REPL 不做静态类型检查（跨行变量/函数无法增量检查），错误在运行期报出
        let program = match Parser::parse("<repl>", &src) {
            Ok(p) => p,
            Err(e) => {
                // 回退：整段作为单个表达式语句（如 `1+1`、`[1, 2]`、`"hi"`）
                match Parser::parse_expr_stmt_src("<repl>", &(chunk + ";")) {
                    Ok(stmt) => Program { stmts: vec![stmt] },
                    Err(_) => {
                        println!("{}", e);
                        continue;
                    }
                }
            }
        };

        // 注册函数/结构体/类（FnDef/StructDef/ClassDef 执行时为 no-op）
        if let Err(e) = ip.collect_fns(&program.stmts) {
            println!("{}", e);
            continue;
        }
        ip.collect_structs(&program.stmts);
        ip.collect_classes(&program.stmts);

        // 让运行期错误回显当前输入的源码行（错误 excerpt 定位用）
        ip.src = src.clone();
        ip.reset_last_expr();
        match ip.exec_stmts(&mut env, &program.stmts) {
            Ok(_) => {
                // Python 式回显：最近一条表达式语句的值（void 调用的 null 不回显）
                if let Some(v) = ip.last_expr() {
                    if !matches!(v, Value::Null) {
                        println!("{}", v.display());
                    }
                }
            }
            Err(e) => println!("{}", e),
        }
    }
    Ok(())
}

/// 判断累积输入是否尚未闭合（需要继续读下一行）：
/// 词法失败（未闭合字符串/块注释）或括号/大括号不平衡 → 续行。
fn is_incomplete(src: &str) -> bool {
    let toks = match Lexer::new("<repl>", src).tokenize() {
        Ok(t) => t,
        Err(_) => return true,
    };
    let mut depth: i32 = 0;
    for (tok, _) in &toks {
        match tok {
            Tok::LBrace => depth += 1,
            Tok::RBrace => depth -= 1,
            Tok::LParen => depth += 1,
            Tok::RParen => depth -= 1,
            Tok::LBracket => depth += 1,
            Tok::RBracket => depth -= 1,
            _ => {}
        }
    }
    depth > 0
}

fn print_vars(env: &Env, ip: &Interp) {
    let vars = env.vars();
    let fns = ip.fn_names();
    if vars.is_empty() && fns.is_empty() {
        println!("（暂无已定义变量）");
        return;
    }
    for (name, val) in vars {
        println!("{} = {}", name, val);
    }
    for name in fns {
        println!("fn {}", name);
    }
}

fn print_help() {
    println!("Hone REPL 帮助：");
    println!("  <表达式>           输入表达式自动回显结果（如 1+1 → 2）");
    println!("  exit() / quit()    退出交互模式");
    println!("  .exit / .quit      退出交互模式");
    println!("  .vars              列出当前已定义变量");
    println!("  .help              显示本帮助");
    println!("  Ctrl+D / Ctrl+Z    结束输入并退出");
    println!("  多行输入：函数/循环/if 等未闭合大括号时自动续行（... 提示）");
}
