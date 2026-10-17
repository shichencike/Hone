// imports.rs - 构建期 import 解析（VM 与原生编译后端共用）
//
// 解释器在**运行期**处理 import（interp::exec_import）：抓取模块、按前缀注册函数、
// 执行模块顶层。VM 与原生编译后端是 AOT 编译模型，无法在运行期下载/注册，故在
// **编译期**把顶层 `import` 递归展开、合并为单一编译单元（设计 §7.3）。语义逐条对齐解释器：
//   · 模块函数以「模块名_」前缀注册；`import ... as 别名` 时整体替换为「别名_」前缀
//   · 模块顶层（非函数）语句在 import 位点执行一次，包进 Block 隔离模块作用域，不泄漏到主脚本
//   · 本地路径相对当前工作目录读取；远程 http(s) 先查 ~/.hone/cache/<name>.hn，缺失则下载入缓存
//   · 同名定义不在本层拦截，交给 checker 的全局命名空间规则报「already defined」
//   · 循环 import 报错；同一模块重复 import 只合并一次
//
// 这是 VM / 原生编译独有能力（解释器仍走运行期 import）。

use crate::ast::{Program, Stmt};
use crate::error::{codes, ZError};
use crate::lexer::Span;
use crate::parser::Parser;
use std::collections::HashSet;

/// 把程序里全部**顶层** import 声明递归展开、合并为单编译单元。
/// `file` / `src` 为主脚本路径与源码，仅用于抓取失败时的错误定位渲染。
pub fn resolve_imports(program: Program, file: &str, src: &str) -> Result<Program, ZError> {
    let mut r = Resolver {
        main_file: file,
        main_src: src,
        merged: HashSet::new(),
        stack: Vec::new(),
    };
    let stmts = r.resolve_stmts(program.stmts)?;
    Ok(Program { stmts })
}

struct Resolver<'a> {
    main_file: &'a str,
    main_src: &'a str,
    /// 已合并模块的规范 key（去重：同一模块只合并一次）。
    merged: HashSet<String>,
    /// 当前递归栈（环检测）。
    stack: Vec<String>,
}

impl Resolver<'_> {
    fn resolve_stmts(&mut self, stmts: Vec<Stmt>) -> Result<Vec<Stmt>, ZError> {
        let mut out = Vec::with_capacity(stmts.len());
        for s in stmts {
            match s {
                Stmt::Import { name, url, alias, span } => {
                    let mut expanded = self.expand(&name, &url, alias.as_deref(), span)?;
                    out.append(&mut expanded);
                }
                other => out.push(other),
            }
        }
        Ok(out)
    }

    /// 展开单个 import：抓取 → 解析 → 递归展开其内部 import → 前缀改名 → 就地拼接。
    fn expand(
        &mut self,
        name: &str,
        url: &str,
        alias: Option<&str>,
        span: Span,
    ) -> Result<Vec<Stmt>, ZError> {
        let key = module_key(url);
        if self.stack.iter().any(|k| k == &key) {
            return Err(self.err(
                codes::SYNTAX,
                format!("circular import of module `{name}`"),
                span,
                Some("break the import cycle between modules"),
            ));
        }
        if self.merged.contains(&key) {
            return Ok(Vec::new());
        }
        let code = self.fetch(name, url, span)?;
        let mod_file = format!("{name}.hn");
        let mod_prog = Parser::parse(&mod_file, &code).map_err(|e| {
            self.err(
                codes::SYNTAX,
                format!("cannot parse imported module `{name}`: {}", e.msg),
                span,
                Some("check the module source"),
            )
        })?;
        // 先递归展开模块自身的 import（模块内本地路径同样相对 CWD / 走同一缓存）。
        self.stack.push(key.clone());
        let mod_stmts = self.resolve_stmts(mod_prog.stmts)?;
        self.stack.pop();
        self.merged.insert(key);

        let prefix = alias.unwrap_or(name);
        let mut fns = Vec::new();
        let mut rest = Vec::new();
        for s in mod_stmts {
            match s {
                Stmt::FnDef { name: fname, type_params, params, ret, body, span: fspan, tmp } => {
                    // 别名：模块名前缀整体替换为别名前缀（镜像 interp::collect_fns_with_prefix）。
                    let new_name = if prefix != name {
                        let old_prefix = format!("{name}_");
                        if fname.starts_with(&old_prefix) {
                            fname.replacen(&old_prefix, &format!("{prefix}_"), 1)
                        } else {
                            fname
                        }
                    } else {
                        fname
                    };
                    fns.push(Stmt::FnDef {
                        name: new_name,
                        type_params,
                        params,
                        ret,
                        body,
                        span: fspan,
                        tmp,
                    });
                }
                other => rest.push(other),
            }
        }
        let mut out = fns;
        // 模块顶层（非函数）语句：包进 Block 隔离作用域，并在 import 位点按序执行一次。
        if !rest.is_empty() {
            out.push(Stmt::Block { stmts: rest, span });
        }
        Ok(out)
    }

    /// 获取模块源码（逐条镜像 interp::fetch_module 的路径/缓存/文案/定位）。
    fn fetch(&self, name: &str, url: &str, span: Span) -> Result<String, ZError> {
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return std::fs::read_to_string(url).map_err(|e| {
                self.err(
                    codes::NOT_FOUND,
                    format!("cannot read local module `{name}` at `{url}`: {e}"),
                    span,
                    Some("check the module path; local paths are relative to the working directory"),
                )
            });
        }
        let cache_file = crate::interp::hone_cache_dir().join(format!("{name}.hn"));
        if cache_file.exists() {
            return std::fs::read_to_string(&cache_file).map_err(|e| {
                self.err(
                    codes::NOT_FOUND,
                    format!("cannot read cached module `{name}`: {e}"),
                    span,
                    None::<&str>,
                )
            });
        }
        let code = crate::builtins::http_request(url, "GET", None, span, self.main_file, self.main_src)?;
        if let Some(dir) = cache_file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::write(&cache_file, &code).map_err(|e| {
            self.err(
                codes::NOT_FOUND,
                format!("cannot cache module `{name}`: {e}"),
                span,
                None::<&str>,
            )
        })?;
        Ok(code)
    }

    /// 带定位的错误构造（file/src 取主脚本，span 取 import 语句）。
    fn err(
        &self,
        code: &'static str,
        msg: impl Into<String>,
        span: Span,
        help: Option<impl Into<String>>,
    ) -> ZError {
        ZError::new(code, msg, self.main_file, self.main_src, span.line, span.col, span.len.max(1), help)
    }
}

/// 模块的规范 key（去重 / 环检测）：本地路径取 canonicalize，远程取 URL。
fn module_key(url: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") {
        format!("remote:{url}")
    } else {
        std::fs::canonicalize(url)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| format!("local:{url}"))
    }
}
