// interp.rs - Hone 树遍历解释器
// 支持：作用域、用户函数（扁平化全局符号表）、go 多线程（std::thread）、
//       breakpoint 断点快照（hone debug 模式）、递归深度限制（H012）。
// 类型锁定由 checker 静态保证，解释器专注求值。

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::io::{self, Write};
use std::os::raw::c_char;

use crate::ast::*;
use crate::builtins;
use crate::error::codes;
use crate::error::ZError;
use crate::lexer::Span;
use crate::parser;

/// 错误对象（catch e 中的 e）。code 为 &'static str 以便原样重抛。
#[derive(Debug, Clone, PartialEq)]
pub struct ErrorObj {
    pub code: &'static str,
    pub message: String,
    pub file: String,
    pub line: usize,
    pub col: usize,
    pub context: String,
}

impl ErrorObj {
    fn from_err(e: &ZError) -> Self {
        ErrorObj {
            code: e.code,
            message: e.msg.clone(),
            file: e.file.clone(),
            line: e.line,
            col: e.col,
            context: e.line_text.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    /// 列表：[1, 2, 3]（也用于 JSON 数组）
    List(Vec<Value>),
    /// 字典：{"key": value}（保持插入顺序，也用于 JSON 对象）
    Dict(Vec<(String, Value)>),
    /// void 函数调用结果的占位值
    Null,
    /// 错误对象（catch e 中的 e）
    Error(ErrorObj),
    /// FFI 指针（typed load 的 ptr 返回值，或库函数传入的不透明句柄）
    Ptr(usize),
}

impl Value {
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Bool(_) => "bool",
            Value::Str(_) => "str",
            Value::List(_) => "list",
            Value::Dict(_) => "dict",
            Value::Null => "null",
            Value::Error(_) => "error",
            Value::Ptr(_) => "ptr",
        }
    }

    pub fn display(&self) -> String {
        match self {
            Value::Int(i) => i.to_string(),
            Value::Float(f) => f.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Str(s) => s.clone(),
            Value::List(items) => {
                let inner: Vec<String> = items.iter().map(|v| v.display()).collect();
                format!("[{}]", inner.join(", "))
            }
            Value::Dict(entries) => {
                let inner: Vec<String> = entries
                    .iter()
                    .map(|(k, v)| format!("{}: {}", k, v.display()))
                    .collect();
                format!("{{{}}}", inner.join(", "))
            }
            Value::Null => "null".to_string(),
            Value::Error(e) => format!("error[{}]: {}", e.code, e.message),
            Value::Ptr(p) => format!("0x{:x}", p),
        }
    }
}

/// 语句执行流程：Normal 继续；Return 携带返回值向上传播。
enum Flow {
    Normal,
    Return(Value),
}

#[derive(Clone)]
struct FnDef {
    params: Vec<Param>,
    body: Vec<Stmt>,
}

struct Env {
    scopes: Vec<HashMap<String, Value>>,
}

impl Env {
    fn new() -> Self {
        Env {
            scopes: vec![HashMap::new()],
        }
    }

    fn get(&self, name: &str) -> Option<&Value> {
        for s in self.scopes.iter().rev() {
            if let Some(v) = s.get(name) {
                return Some(v);
            }
        }
        None
    }

    /// 赋值：找到最近绑定则更新，否则在当前作用域声明。
    fn set_or_declare(&mut self, name: &str, v: Value) {
        for s in self.scopes.iter_mut().rev() {
            if s.contains_key(name) {
                s.insert(name.to_string(), v);
                return;
            }
        }
        self.scopes.last_mut().unwrap().insert(name.to_string(), v);
    }

    fn declare(&mut self, name: &str, v: Value) {
        self.scopes.last_mut().unwrap().insert(name.to_string(), v);
    }
}

pub struct Interp {
    pub file: String,
    pub src: String,
    fns: HashMap<String, FnDef>,
    debug: bool,
    depth: usize,
    /// 已加载的动态库（别名 → Library）
    libs: HashMap<String, libloading::Library>,
    /// 懒加载库（别名 → 路径），首次调用时加载
    lazy_libs: HashMap<String, String>,
    /// load 签名块声明的 FFI 函数（键为完整调用名 "alias.fn"）
    ffi_sigs: HashMap<String, FfiSig>,
    /// 函数别名（新名 → 原名）
    alias_map: HashMap<String, String>,
    /// 结构体定义：名称 → 字段名（构造时按顺序生成 dict 实例）
    structs: HashMap<String, Vec<String>>,
    /// 类定义：类名 → (方法名 → FnDef)。成员函数不进全局 fns 表，
    /// 只能经 `类.方法(...)` 调用（call_fn 按限定名解析）。
    classes: HashMap<String, HashMap<String, FnDef>>,
}

/// load 加载的 C ABI 库函数签名约定：全 int64 参数（不足补 0，x64 ABI 安全）。
type KaLibFn = unsafe extern "C" fn(i64, i64, i64, i64, i64, i64, i64, i64) -> i64;

/// typed FFI 调用参数：int 类（int/bool/str 指针/ptr 句柄，走整数寄存器）与 float 类（double）。
#[derive(Clone, Copy)]
enum CArg {
    I(i64),
    F(f64),
}

/// typed FFI 调用返回值。
#[derive(Clone, Copy)]
enum CRet {
    I(i64),
    F(f64),
}

#[inline]
fn carg_i(cargs: &[CArg], i: usize) -> i64 {
    match cargs[i] {
        CArg::I(v) => v,
        CArg::F(_) => unreachable!("class bit mismatch"),
    }
}

#[inline]
fn carg_f(cargs: &[CArg], i: usize) -> f64 {
    match cargs[i] {
        CArg::F(v) => v,
        CArg::I(_) => unreachable!("class bit mismatch"),
    }
}

/// 按参数类别（0=int 类 / 1=float 类）逐位展开二分树，叶节点用具体签名取出符号并调用。
/// $bits 为运行时类别位掩码（第 i 位 1 表示第 i 个参数是 float）；索引列表 [$i, $rest...] 由调用方按元数给出。
macro_rules! ffi_dispatch {
    // 基础：所有参数位已消费，用累积的类型列表取出符号并调用
    ([], $bits:expr, $retf:expr, $lib:expr, $name:expr, $cargs:expr, $sym_err:expr, [$($t:ty),*], [$($v:expr),*]) => {
        if $retf {
            let sym: libloading::Symbol<unsafe extern "C" fn($($t),*) -> f64> = unsafe { $lib.get($name) }.map_err($sym_err)?;
            CRet::F(unsafe { sym($($v),*) })
        } else {
            let sym: libloading::Symbol<unsafe extern "C" fn($($t),*) -> i64> = unsafe { $lib.get($name) }.map_err($sym_err)?;
            CRet::I(unsafe { sym($($v),*) })
        }
    };
    // 单元素：消费最后一个索引位后进入基础规则
    ([$i:tt], $bits:expr, $retf:expr, $lib:expr, $name:expr, $cargs:expr, $sym_err:expr, [$($t:ty),*], [$($v:expr),*]) => {
        if ($bits >> $i) & 1 == 1 {
            ffi_dispatch!([], $bits, $retf, $lib, $name, $cargs, $sym_err, [$($t,)* f64], [$($v,)* carg_f($cargs, $i)])
        } else {
            ffi_dispatch!([], $bits, $retf, $lib, $name, $cargs, $sym_err, [$($t,)* i64], [$($v,)* carg_i($cargs, $i)])
        }
    };
    // 多元素：消费头部索引位，继续递归
    ([$i:tt, $($ri:tt)*], $bits:expr, $retf:expr, $lib:expr, $name:expr, $cargs:expr, $sym_err:expr, [$($t:ty),*], [$($v:expr),*]) => {
        if ($bits >> $i) & 1 == 1 {
            ffi_dispatch!([$($ri)*], $bits, $retf, $lib, $name, $cargs, $sym_err, [$($t,)* f64], [$($v,)* carg_f($cargs, $i)])
        } else {
            ffi_dispatch!([$($ri)*], $bits, $retf, $lib, $name, $cargs, $sym_err, [$($t,)* i64], [$($v,)* carg_i($cargs, $i)])
        }
    };
}

/// 运行整个程序。debug 为 true 时 breakpoint; 生效。
pub fn run(program: &Program, file: &str, src: &str, debug: bool) -> Result<(), ZError> {
    let mut ip = Interp {
        file: file.to_string(),
        src: src.to_string(),
        fns: HashMap::new(),
        debug,
        depth: 0,
        libs: HashMap::new(),
        lazy_libs: HashMap::new(),
        ffi_sigs: HashMap::new(),
        alias_map: HashMap::new(),
        structs: HashMap::new(),
        classes: HashMap::new(),
    };
    ip.collect_fns(&program.stmts)?;
    ip.collect_structs(&program.stmts);
    ip.collect_classes(&program.stmts);
    let mut env = Env::new();
    ip.exec_stmts(&mut env, &program.stmts)?;
    Ok(())
}

impl Interp {
    /// 收集所有函数定义（含嵌套，扁平化注册；解释执行时 FnDef 语句为 no-op）。
    fn collect_fns(&mut self, stmts: &[Stmt]) -> Result<(), ZError> {
        for stmt in stmts {
            match stmt {
                Stmt::FnDef { name, params, body, tmp, .. } => {
                    if !tmp {
                        self.fns.insert(
                            name.clone(),
                            FnDef {
                                params: params.clone(),
                                body: body.clone(),
                            },
                        );
                    }
                }
                Stmt::Block { stmts, .. } => self.collect_fns(stmts)?,
                Stmt::If { then_branch, else_branch, .. } => {
                    self.collect_fns(then_branch)?;
                    if let Some(eb) = else_branch {
                        self.collect_fns(eb)?;
                    }
                }
                Stmt::While { body, .. } => self.collect_fns(body)?,
                Stmt::ForIn { body, .. } => self.collect_fns(body)?,
                _ => {}
            }
        }
        Ok(())
    }

    /// 收集所有结构体定义（含嵌套），扁平化注册；解释执行时 StructDef 语句为 no-op。
    fn collect_structs(&mut self, stmts: &[Stmt]) {
        for stmt in stmts {
            match stmt {
                Stmt::StructDef { name, fields, .. } => {
                    self.structs.insert(name.clone(), fields.iter().map(|(f, _)| f.clone()).collect());
                }
                Stmt::Block { stmts, .. } => self.collect_structs(stmts),
                Stmt::If { then_branch, else_branch, .. } => {
                    self.collect_structs(then_branch);
                    if let Some(eb) = else_branch {
                        self.collect_structs(eb);
                    }
                }
                Stmt::While { body, .. } => self.collect_structs(body),
                Stmt::ForIn { body, .. } => self.collect_structs(body),
                Stmt::Try { body, handler, .. } => {
                    self.collect_structs(body);
                    self.collect_structs(handler);
                }
                _ => {}
            }
        }
    }

    /// 收集所有类定义（含嵌套），注册到 classes 表（类名 → 方法名 → FnDef）。
    /// 成员函数不进入全局 fns 表，只能经 `类.方法(...)` 调用。
    fn collect_classes(&mut self, stmts: &[Stmt]) {
        for stmt in stmts {
            match stmt {
                Stmt::ClassDef { name, methods, .. } => {
                    let mut methods_map: HashMap<String, FnDef> = HashMap::new();
                    for m in methods {
                        if let Stmt::FnDef { name: mname, params, body, tmp, .. } = m {
                            if !tmp {
                                methods_map.insert(
                                    mname.clone(),
                                    FnDef {
                                        params: params.clone(),
                                        body: body.clone(),
                                    },
                                );
                            }
                        }
                    }
                    self.classes.insert(name.clone(), methods_map);
                }
                Stmt::Block { stmts, .. } => self.collect_classes(stmts),
                Stmt::If { then_branch, else_branch, .. } => {
                    self.collect_classes(then_branch);
                    if let Some(eb) = else_branch {
                        self.collect_classes(eb);
                    }
                }
                Stmt::While { body, .. } => self.collect_classes(body),
                Stmt::ForIn { body, .. } => self.collect_classes(body),
                Stmt::Try { body, handler, .. } => {
                    self.collect_classes(body);
                    self.collect_classes(handler);
                }
                _ => {}
            }
        }
    }

    fn runtime_err(&self, code: &'static str, msg: impl Into<String>, span: Span, help: Option<impl Into<String>>) -> ZError {
        ZError::new(code, msg, &self.file, &self.src, span.line, span.col, span.len.max(1), help)
    }

    // ---------- 语句 ----------

    fn exec_stmts(&mut self, env: &mut Env, stmts: &[Stmt]) -> Result<Flow, ZError> {
        for s in stmts {
            match self.exec_stmt(env, s)? {
                Flow::Return(v) => return Ok(Flow::Return(v)),
                Flow::Normal => {}
            }
        }
        Ok(Flow::Normal)
    }

    fn exec_block(&mut self, env: &mut Env, stmts: &[Stmt]) -> Result<Flow, ZError> {
        env.scopes.push(HashMap::new());
        let flow = self.exec_stmts(env, stmts);
        env.scopes.pop();
        flow
    }

    fn exec_stmt(&mut self, env: &mut Env, stmt: &Stmt) -> Result<Flow, ZError> {
        match stmt {
            Stmt::VarDecl { name, ty, init, .. } => {
                let v = match init {
                    Some(e) => self.eval_expr(env, e)?,
                    None => default_value(ty.clone()),
                };
                env.set_or_declare(name, v);
                Ok(Flow::Normal)
            }
            Stmt::Assign { name, value, .. } => {
                let v = self.eval_expr(env, value)?;
                env.set_or_declare(name, v);
                Ok(Flow::Normal)
            }
            Stmt::Block { stmts, .. } => self.exec_block(env, stmts),
            Stmt::If { cond, then_branch, else_branch, .. } => {
                let c = self.eval_expr(env, cond)?;
                if let Value::Bool(b) = c {
                    if b {
                        self.exec_block(env, then_branch)
                    } else if let Some(eb) = else_branch {
                        self.exec_block(env, eb)
                    } else {
                        Ok(Flow::Normal)
                    }
                } else {
                    // checker 已保证条件为 bool，此处兜底
                    Err(self.runtime_err(
                        codes::TYPE_MISMATCH,
                        format!("if condition must be `bool`, got `{}`", c.type_name()),
                        expr_span(cond),
                        None::<&str>,
                    ))
                }
            }
            Stmt::While { cond, body, .. } => {
                loop {
                    let c = self.eval_expr(env, cond)?;
                    if let Value::Bool(b) = c {
                        if !b {
                            break;
                        }
                    } else {
                        return Err(self.runtime_err(
                            codes::TYPE_MISMATCH,
                            format!("while condition must be `bool`, got `{}`", c.type_name()),
                            expr_span(cond),
                            None::<&str>,
                        ));
                    }
                    if let Flow::Return(v) = self.exec_block(env, body)? {
                        return Ok(Flow::Return(v));
                    }
                }
                Ok(Flow::Normal)
            }
            Stmt::ForIn { var, var2, iter, body, span } => {
                let it = self.eval_expr(env, iter)?;
                let is_dict = matches!(it, Value::Dict(_));
                match it {
                    // 列表：单变量绑定元素
                    Value::List(items) => {
                        if var2.is_some() {
                            return Err(self.runtime_err(
                                codes::TYPE_MISMATCH,
                                "`for k, v in` requires a dict, got a list",
                                *span,
                                Some("iterate lists with a single variable: `for x in list`"),
                            ));
                        }
                        for item in items {
                            env.scopes.push(HashMap::new());
                            env.declare(var, item);
                            let flow = self.exec_stmts(env, body)?;
                            env.scopes.pop();
                            if let Flow::Return(v) = flow {
                                return Ok(Flow::Return(v));
                            }
                        }
                        Ok(Flow::Normal)
                    }
                    // 字典：var=键，var2=值（可选）
                    Value::Dict(entries) => {
                        for (k, v) in entries {
                            env.scopes.push(HashMap::new());
                            env.declare(var, Value::Str(k));
                            if let Some(v2) = var2 {
                                env.declare(v2, v);
                            }
                            let flow = self.exec_stmts(env, body)?;
                            env.scopes.pop();
                            if let Flow::Return(v) = flow {
                                return Ok(Flow::Return(v));
                            }
                        }
                        Ok(Flow::Normal)
                    }
                    other => Err(self.runtime_err(
                        codes::TYPE_MISMATCH,
                        format!(
                            "`for in` requires a list or dict, got `{}`{}",
                            other.type_name(),
                            if is_dict { "" } else { "" }
                        ),
                        expr_span(iter),
                        Some("iterate a list with `for x in list` or a dict with `for k, v in dict`"),
                    )),
                }
            }
            Stmt::Return { value, .. } => {
                let v = match value {
                    Some(e) => self.eval_expr(env, e)?,
                    None => Value::Null,
                };
                Ok(Flow::Return(v))
            }
            Stmt::FnDef { .. } => Ok(Flow::Normal), // 已扁平化注册
            Stmt::ExprStmt { expr, .. } => {
                self.eval_expr(env, expr)?;
                Ok(Flow::Normal)
            }
            Stmt::Breakpoint { span } => {
                if self.debug {
                    self.do_breakpoint(env, *span);
                }
                Ok(Flow::Normal)
            }
            Stmt::Export { .. } => Ok(Flow::Normal), // 仅 hone build --dll 使用
            Stmt::Import { name, url, alias, span } => self.exec_import(name, url, alias.as_deref(), *span),
            Stmt::Load { lazy, path, alias, from, sigs, span } => {
                self.exec_load(*lazy, path, alias.as_deref(), from.as_deref(), sigs, *span)
            }
            Stmt::Use { namespace, .. } => {
                // 命名空间导入：内置函数已全局可用，namespace 仅作声明记录
                let _ = namespace;
                Ok(Flow::Normal)
            }
            Stmt::Alias { original, new_name, .. } => {
                self.alias_map.insert(new_name.clone(), original.clone());
                Ok(Flow::Normal)
            }
            Stmt::StructDef { .. } => Ok(Flow::Normal), // 已扁平化注册
            Stmt::ClassDef { .. } => Ok(Flow::Normal), // 已注册到 classes 表（成员函数不进全局 fns）
            Stmt::Go { callee, args, span } => self.exec_go(env, callee, args, *span),
            Stmt::DebugPrint { expr, span: _ } => {
                if self.debug {
                    let v = self.eval_expr(env, expr)?;
                    println!("[debug] {}", v.display());
                }
                Ok(Flow::Normal)
            }
            Stmt::Try { body, catch_var, handler, .. } => {
                match self.exec_block(env, body) {
                    Ok(flow) => Ok(flow),
                    Err(e) => {
                        // 捕获可恢复错误：绑定错误对象后执行 handler
                        env.scopes.push(HashMap::new());
                        env.declare(catch_var, Value::Error(ErrorObj::from_err(&e)));
                        let flow = self.exec_stmts(env, handler);
                        env.scopes.pop();
                        flow
                    }
                }
            }
            Stmt::Throw { value, span } => {
                let v = self.eval_expr(env, value)?;
                match v {
                    // 抛字符串：构造一个 H600 用户错误
                    Value::Str(s) => Err(self.runtime_err(codes::THROW, s, *span, None::<&str>)),
                    // 重抛 error 值：同文件保留原始定位，跨文件退化并附原始位置
                    Value::Error(e) => {
                        if e.file == self.file {
                            Err(ZError::new(
                                e.code,
                                e.message,
                                &self.file,
                                &self.src,
                                e.line,
                                e.col,
                                1,
                                None::<&str>,
                            ))
                        } else {
                            Err(ZError::plain(
                                e.code,
                                format!("{} (at {}:{}:{})", e.message, e.file, e.line, e.col),
                                None::<&str>,
                            ))
                        }
                    }
                    other => Err(self.runtime_err(
                        codes::TYPE_MISMATCH,
                        format!("`throw` accepts a `str` or `error`, got `{}`", other.type_name()),
                        *span,
                        None::<&str>,
                    )),
                }
            }
        }
    }

    // ---------- 断点 ----------

    fn do_breakpoint(&self, env: &Env, span: Span) {
        println!("[Hone Debug] 断点触发 -> {}:{}", self.file, span.line);
        println!("--- 变量快照 ---");
        let mut seen: HashSet<String> = HashSet::new();
        for scope in env.scopes.iter().rev() {
            for (k, v) in scope {
                if seen.insert(k.clone()) {
                    println!("{} : {} = {}", k, v.type_name(), v.display());
                }
            }
        }
        print!("按 Enter 继续 (Ctrl+C 退出)...");
        let _ = io::stdout().flush();
        let mut line = String::new();
        let _ = io::stdin().read_line(&mut line);
    }

    // ---------- go 多线程 ----------

    fn exec_go(
        &mut self,
        env: &mut Env,
        callee: &str,
        args: &[Expr],
        span: Span,
    ) -> Result<Flow, ZError> {
        // 参数在主线程求值后按值克隆传入子线程
        let mut arg_vals = Vec::new();
        for a in args {
            arg_vals.push(self.eval_expr(env, a)?);
        }
        let fns = self.fns.clone();
        let alias_map = self.alias_map.clone();
        let lazy_libs = self.lazy_libs.clone();
        let structs = self.structs.clone();
        let classes = self.classes.clone();
        let file = self.file.clone();
        let src = self.src.clone();
        let callee = callee.to_string();
        let span = span;
        // 子线程崩溃仅打印错误，不影响主线程
        std::thread::spawn(move || {
            let mut t = Interp {
                file,
                src,
                fns,
                debug: false,
                depth: 0,
                // 已加载的库（Library 不可克隆）不跨线程；懒加载路径与别名可克隆
                libs: HashMap::new(),
                lazy_libs,
                ffi_sigs: HashMap::new(),
                alias_map,
                structs,
                classes,
            };
            if let Err(err) = t.call_fn(&callee, arg_vals, span) {
                eprintln!("{}", err);
            }
        });
        Ok(Flow::Normal)
    }

    // ---------- import 远程模块 ----------

    fn exec_import(&mut self, name: &str, url: &str, alias: Option<&str>, span: Span) -> Result<Flow, ZError> {
        let code = self.fetch_module(name, url, span)?;
        let file = format!("{}.hn", name);
        let program = parser::Parser::parse(&file, &code).map_err(|e| {
            self.runtime_err(
                codes::SYNTAX,
                format!("cannot parse imported module `{}`: {}", name, e.msg),
                span,
                Some("check the module source"),
            )
        })?;
        // 收集模块函数（以别名前缀注册，或保持原名）
        let prefix = alias.unwrap_or(name);
        for stmt in &program.stmts {
            self.collect_fns_with_prefix(stmt, name, prefix)?;
        }
        // 执行模块顶层语句（独立作用域）
        let mut menv = Env::new();
        self.exec_stmts(&mut menv, &program.stmts)?;
        Ok(Flow::Normal)
    }

    fn collect_fns_with_prefix(&mut self, stmt: &Stmt, mod_name: &str, prefix: &str) -> Result<(), ZError> {
        match stmt {
            Stmt::FnDef { name, params, body, tmp, .. } => {
                if !tmp {
                    // 若提供了别名，将函数名中的模块名前缀替换为别名前缀
                    let new_name = if prefix != mod_name {
                        let old_prefix = format!("{}_", mod_name);
                        if name.starts_with(&old_prefix) {
                            name.replacen(&old_prefix, &format!("{}_", prefix), 1)
                        } else {
                            name.clone()
                        }
                    } else {
                        name.clone()
                    };
                    self.fns.insert(
                        new_name,
                        FnDef {
                            params: params.clone(),
                            body: body.clone(),
                        },
                    );
                }
            }
            Stmt::Block { stmts, .. } => {
                for s in stmts {
                    self.collect_fns_with_prefix(s, mod_name, prefix)?;
                }
            }
            Stmt::If { then_branch, else_branch, .. } => {
                for s in then_branch {
                    self.collect_fns_with_prefix(s, mod_name, prefix)?;
                }
                if let Some(eb) = else_branch {
                    for s in eb {
                        self.collect_fns_with_prefix(s, mod_name, prefix)?;
                    }
                }
            }
            Stmt::While { body, .. } => {
                for s in body {
                    self.collect_fns_with_prefix(s, mod_name, prefix)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// 获取模块源码：本地路径（非 http/https 开头）直接读取；否则缓存 ~/.hone/cache/<name>.hn 优先，下载写入缓存。
    fn fetch_module(&self, name: &str, url: &str, span: Span) -> Result<String, ZError> {
        // 本地路径模块：直接读文件，不写缓存（相对路径基于当前工作目录）
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return std::fs::read_to_string(url).map_err(|e| {
                self.runtime_err(
                    codes::NOT_FOUND,
                    format!("cannot read local module `{}` at `{}`: {}", name, url, e),
                    span,
                    Some("check the module path; local paths are relative to the working directory"),
                )
            });
        }
        let cache_file = hone_cache_dir().join(format!("{}.hn", name));
        if cache_file.exists() {
            return std::fs::read_to_string(&cache_file).map_err(|e| {
                self.runtime_err(
                    codes::NOT_FOUND,
                    format!("cannot read cached module `{}`: {}", name, e),
                    span,
                    None::<&str>,
                )
            });
        }
        // 下载（进度条 \r 轻量显示）
        print!("\r[import] 下载模块 `{}` ...", name);
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let code = crate::builtins::http_request(url, "GET", None, span, &self.file, &self.src)?;
        println!();
        if let Some(dir) = cache_file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::write(&cache_file, &code).map_err(|e| {
            self.runtime_err(
                codes::NOT_FOUND,
                format!("cannot cache module `{}`: {}", name, e),
                span,
                None::<&str>,
            )
        })?;
        Ok(code)
    }

    // ---------- load 动态库 ----------

    fn exec_load(&mut self, lazy: bool, path: &str, alias: Option<&str>, from: Option<&str>, sigs: &[FfiSig], span: Span) -> Result<Flow, ZError> {
        let lib_name = match alias {
            Some(a) => a.to_string(),
            None => std::path::Path::new(path)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "lib".to_string()),
        };
        // 注册签名：from 头文件解析的签名先注册，签名块中的同名声明覆盖之
        if let Some(hpath) = from {
            let src = std::fs::read_to_string(hpath).map_err(|e| {
                self.runtime_err(
                    codes::NOT_FOUND,
                    format!("cannot read header `{}`: {}", hpath, e),
                    span,
                    Some("check the header path, or remove the `from` clause"),
                )
            })?;
            let header_sigs = crate::header::parse(&src, span);
            for sig in &header_sigs {
                self.ffi_sigs.insert(format!("{}.{}", lib_name, sig.name), sig.clone());
            }
        }
        for sig in sigs {
            self.ffi_sigs.insert(format!("{}.{}", lib_name, sig.name), sig.clone());
        }
        if lazy {
            self.lazy_libs.insert(lib_name, path.to_string());
            return Ok(Flow::Normal);
        }
        self.load_library(&lib_name, path, span)?;
        // 同步到插件注册表（plugin.list / plugin.has 可见）
        crate::pluginmod::register(&lib_name, path);
        Ok(Flow::Normal)
    }

    fn load_library(&mut self, name: &str, path: &str, span: Span) -> Result<(), ZError> {
        let lib = unsafe { libloading::Library::new(path) }.map_err(|e| {
            self.runtime_err(
                codes::DLL_LOAD,
                format!("cannot load dynamic library `{}`: {}", path, e),
                span,
                Some("check the library path and architecture"),
            )
        })?;
        self.libs.insert(name.to_string(), lib);
        Ok(())
    }

    /// 调用动态库函数（C ABI 约定：全 int64 参数与返回值，最多 8 个参数）。
    fn call_lib_fn(&mut self, callee: &str, args: Vec<Value>, span: Span) -> Result<Value, ZError> {
        let dot = callee.rfind('.').unwrap();
        let (lib_name, func_name) = (&callee[..dot], &callee[dot + 1..]);
        // 懒加载库：首次调用时加载
        if !self.libs.contains_key(lib_name) {
            if let Some(path) = self.lazy_libs.get(lib_name).cloned() {
                self.load_library(lib_name, &path, span)?;
            } else if let Some(path) = crate::pluginmod::lookup(lib_name) {
                // 运行期 plugin.load 注册的插件：调用时加载
                self.load_library(lib_name, &path, span)?;
            } else {
                return Err(self.runtime_err(
                    codes::NOT_FOUND,
                    format!("library `{}` is not loaded", lib_name),
                    span,
                    Some(format!("add `load \"path/to/lib\" as {};` or `plugin.load(path, \"{}\")` before calling", lib_name, lib_name)),
                ));
            }
        }
        if let Some(sig) = self.ffi_sigs.get(callee).cloned() {
            // typed FFI：按签名块声明的类型转换参数与返回值
            return self.call_ffi_typed(&sig, lib_name, func_name, args, span);
        }
        if args.len() > 8 {
            return Err(self.runtime_err(
                codes::DLL_ARG,
                format!("`{}` takes at most 8 arguments", callee),
                span,
                Some("the C ABI convention supports up to 8 int64 parameters"),
            ));
        }
        let mut cargs = [0i64; 8];
        for (i, a) in args.iter().enumerate() {
            match a {
                Value::Int(v) => cargs[i] = *v,
                other => {
                    return Err(self.runtime_err(
                        codes::TYPE_MISMATCH,
                        format!("`{}` expects `int` arguments, got `{}`", callee, other.type_name()),
                        span,
                        Some("the C ABI convention maps `int` to int64"),
                    ));
                }
            }
        }
        let sym: libloading::Symbol<KaLibFn> = {
            let lib = self.libs.get(lib_name).unwrap();
            unsafe { lib.get(func_name.as_bytes()) }
        }
        .map_err(|e| {
            self.runtime_err(
                codes::NOT_FOUND,
                format!("symbol `{}` not found in library `{}`: {}", func_name, lib_name, e),
                span,
                Some("check the exported symbol name (e.g. `#[no_mangle] pub extern \"C\" fn`)"),
            )
        })?;
        let ret = unsafe { sym(cargs[0], cargs[1], cargs[2], cargs[3], cargs[4], cargs[5], cargs[6], cargs[7]) };
        Ok(Value::Int(ret))
    }

    /// 调用签名块/头文件声明的 FFI 函数：按签名将 Hone 参数转换为 C ABI 值，调用后转换返回值。
    fn call_ffi_typed(&mut self, sig: &FfiSig, lib_name: &str, func_name: &str, args: Vec<Value>, span: Span) -> Result<Value, ZError> {
        // 头文件解析失败的原型（回调/变参/数组等）：调用时直接报错
        if let Some(reason) = sig.unsupported {
            return Err(self.runtime_err(
                codes::NOT_IMPLEMENTED,
                format!("`{}` cannot be called: {}", func_name, reason),
                span,
                Some("declare a manual signature for this function, or use `ptr` for the unsupported parts"),
            ));
        }
        if sig.params.len() != args.len() {
            return Err(self.runtime_err(
                codes::DLL_ARG,
                format!("`{}` expects {} arguments, got {}", func_name, sig.params.len(), args.len()),
                span,
                Some(format!(
                    "declared signature: `fn {}({}) -> {}`",
                    sig.name,
                    sig.params.iter().map(|p| p.ty.name()).collect::<Vec<_>>().join(", "),
                    sig.ret.name()
                )),
            ));
        }
        // 参数转换：str 参数需 CString 保持存活直到调用结束
        let mut cargs: Vec<CArg> = Vec::with_capacity(args.len());
        let mut cstrings: Vec<CString> = Vec::new();
        for (p, a) in sig.params.iter().zip(args.iter()) {
            match p.ty {
                FfiTy::Int => match a {
                    Value::Int(v) => cargs.push(CArg::I(*v)),
                    other => {
                        return Err(self.runtime_err(
                            codes::TYPE_MISMATCH,
                            format!("`{}` parameter `{}` expects `int`, got `{}`", func_name, p.name, other.type_name()),
                            span,
                            Some("the declared FFI signature maps `int` to int64"),
                        ))
                    }
                },
                FfiTy::Float => match a {
                    Value::Float(v) => cargs.push(CArg::F(*v)),
                    other => {
                        return Err(self.runtime_err(
                            codes::TYPE_MISMATCH,
                            format!("`{}` parameter `{}` expects `float`, got `{}`", func_name, p.name, other.type_name()),
                            span,
                            Some("the declared FFI signature maps `float` to double"),
                        ))
                    }
                },
                FfiTy::Bool => match a {
                    Value::Bool(b) => cargs.push(CArg::I(if *b { 1 } else { 0 })),
                    other => {
                        return Err(self.runtime_err(
                            codes::TYPE_MISMATCH,
                            format!("`{}` parameter `{}` expects `bool`, got `{}`", func_name, p.name, other.type_name()),
                            span,
                            Some("the declared FFI signature maps `bool` to a C boolean"),
                        ))
                    }
                },
                FfiTy::Str => match a {
                    Value::Str(s) => {
                        let cs = CString::new(s.as_bytes()).map_err(|_| {
                            self.runtime_err(
                                codes::TYPE_MISMATCH,
                                format!("`{}` parameter `{}` contains a NUL byte", func_name, p.name),
                                span,
                                Some("C strings cannot contain embedded NUL characters"),
                            )
                        })?;
                        let ptr = cs.as_ptr() as i64;
                        cstrings.push(cs);
                        cargs.push(CArg::I(ptr));
                    }
                    other => {
                        return Err(self.runtime_err(
                            codes::TYPE_MISMATCH,
                            format!("`{}` parameter `{}` expects `str`, got `{}`", func_name, p.name, other.type_name()),
                            span,
                            Some("the declared FFI signature maps `str` to `const char*`"),
                        ))
                    }
                },
                FfiTy::Ptr => match a {
                    Value::Ptr(p) => cargs.push(CArg::I(*p as i64)),
                    Value::Int(0) => cargs.push(CArg::I(0)), // 0 作为 NULL
                    other => {
                        return Err(self.runtime_err(
                            codes::TYPE_MISMATCH,
                            format!("`{}` parameter `{}` expects `ptr`, got `{}`", func_name, p.name, other.type_name()),
                            span,
                            Some("pass a `ptr` value (e.g. from another FFI call) or `0` for NULL"),
                        ))
                    }
                },
                FfiTy::Void => unreachable!("void is not a parameter type"),
            }
        }
        // 参数类别位：第 i 位 1 表示第 i 个参数为 float（double，走 XMM 寄存器）
        let bits: u32 = sig
            .params
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, p)| if p.ty == FfiTy::Float { acc | (1 << i) } else { acc });
        let retf = sig.ret == FfiTy::Float;
        let name = func_name.as_bytes();
        let lib = self.libs.get(lib_name).unwrap();
        let sym_err = |e: libloading::Error| {
            self.runtime_err(
                codes::NOT_FOUND,
                format!("symbol `{}` not found in library `{}`: {}", func_name, lib_name, e),
                span,
                Some("check the exported symbol name (e.g. `#[no_mangle] pub extern \"C\" fn`)"),
            )
        };
        let cret = match args.len() {
            0 => ffi_dispatch!([], bits, retf, lib, name, &cargs, &sym_err, [], []),
            1 => ffi_dispatch!([0], bits, retf, lib, name, &cargs, &sym_err, [], []),
            2 => ffi_dispatch!([0, 1], bits, retf, lib, name, &cargs, &sym_err, [], []),
            3 => ffi_dispatch!([0, 1, 2], bits, retf, lib, name, &cargs, &sym_err, [], []),
            4 => ffi_dispatch!([0, 1, 2, 3], bits, retf, lib, name, &cargs, &sym_err, [], []),
            5 => ffi_dispatch!([0, 1, 2, 3, 4], bits, retf, lib, name, &cargs, &sym_err, [], []),
            6 => ffi_dispatch!([0, 1, 2, 3, 4, 5], bits, retf, lib, name, &cargs, &sym_err, [], []),
            7 => ffi_dispatch!([0, 1, 2, 3, 4, 5, 6], bits, retf, lib, name, &cargs, &sym_err, [], []),
            8 => ffi_dispatch!([0, 1, 2, 3, 4, 5, 6, 7], bits, retf, lib, name, &cargs, &sym_err, [], []),
            _ => {
                return Err(self.runtime_err(
                    codes::DLL_ARG,
                    format!("`{}` takes at most 8 parameters", func_name),
                    span,
                    Some("the C ABI convention supports up to 8 scalar parameters"),
                ))
            }
        };
        // cstrings 在此作用域内保持存活，调用完成后再释放
        Ok(match sig.ret {
            FfiTy::Int => Value::Int(match cret {
                CRet::I(v) => v,
                CRet::F(_) => unreachable!("return class mismatch"),
            }),
            FfiTy::Float => Value::Float(match cret {
                CRet::F(v) => v,
                CRet::I(_) => unreachable!("return class mismatch"),
            }),
            FfiTy::Bool => Value::Bool(match cret {
                CRet::I(v) => v != 0,
                CRet::F(_) => unreachable!("return class mismatch"),
            }),
            FfiTy::Str => {
                let p = match cret {
                    CRet::I(v) => v,
                    CRet::F(_) => unreachable!("return class mismatch"),
                };
                if p == 0 {
                    return Err(self.runtime_err(
                        codes::TYPE_MISMATCH,
                        format!("`{}` returned NULL where `str` was expected", func_name),
                        span,
                        Some("the C function returned a null `const char*`"),
                    ));
                }
                let s = unsafe { CStr::from_ptr(p as *const c_char) };
                Value::Str(s.to_string_lossy().into_owned())
            }
            FfiTy::Ptr => Value::Ptr(match cret {
                CRet::I(v) => v as usize,
                CRet::F(_) => unreachable!("return class mismatch"),
            }),
            FfiTy::Void => Value::Null,
        })
    }

    // ---------- 函数调用 ----------

    fn call_fn(&mut self, callee: &str, args: Vec<Value>, span: Span) -> Result<Value, ZError> {
        if let Some(f) = self.fns.get(callee).cloned() {
            self.exec_user_fn(&f, args, span)
        } else if let Some(fields) = self.structs.get(callee).cloned() {
            // 结构体构造：按字段顺序生成 dict 实例
            if fields.len() != args.len() {
                return Err(self.runtime_err(
                    codes::ARG_COUNT,
                    format!("struct `{}` expects {} fields, got {}", callee, fields.len(), args.len()),
                    span,
                    Some(format!(
                        "construct with `{}({})`",
                        callee,
                        fields.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                    )),
                ));
            }
            Ok(Value::Dict(fields.into_iter().zip(args).collect()))
        } else if let Some(result) = self.call_class_method(callee, &args, span) {
            // 类方法（类名.方法名）：成员函数不进全局 fns 表，经此解析执行
            result
        } else if builtins::is_builtin(callee) {
            // 内置函数优先（含 time.now / random.int / sys.* 等点号内置）
            builtins::call(callee, args, span, &self.file, &self.src)
        } else if let Some(orig) = self.alias_map.get(callee).cloned() {
            self.call_fn(&orig, args, span)
        } else if callee.contains('.') {
            self.call_lib_fn(callee, args, span)
        } else {
            builtins::call(callee, args, span, &self.file, &self.src)
        }
    }

    /// 执行用户函数/类方法体：绑定参数到新环境，执行函数体，处理 return。
    fn exec_user_fn(&mut self, f: &FnDef, args: Vec<Value>, span: Span) -> Result<Value, ZError> {
        if self.depth >= 5000 {
            return Err(self.runtime_err(
                codes::RECURSION_DEPTH,
                "recursion depth exceeded (limit 5000)",
                span,
                Some("check for infinite recursion, or rewrite iteratively"),
            ));
        }
        let mut call_env = Env::new();
        for (p, v) in f.params.iter().zip(args) {
            call_env.declare(&p.name, v);
        }
        self.depth += 1;
        let flow = self.exec_stmts(&mut call_env, &f.body);
        self.depth -= 1;
        match flow? {
            Flow::Return(v) => Ok(v),
            Flow::Normal => Ok(Value::Null),
        }
    }

    /// 尝试按「类名.方法名」解析类方法调用。
    /// 类已注册 → Some(执行结果)；类未注册或非类调用 → None（落入常规解析）。
    fn call_class_method(&mut self, callee: &str, args: &[Value], span: Span) -> Option<Result<Value, ZError>> {
        let (cls, method) = callee.split_once('.')?;
        let methods = self.classes.get(cls)?;
        match methods.get(method).cloned() {
            Some(f) => Some(self.exec_user_fn(&f, args.to_vec(), span)),
            None => Some(Err(self.runtime_err(
                codes::UNDEFINED,
                format!("class `{}` has no method `{}`", cls, method),
                span,
                Some("check the method name"),
            ))),
        }
    }

    // ---------- 表达式 ----------

    fn eval_expr(&mut self, env: &Env, e: &Expr) -> Result<Value, ZError> {
        match e {
            Expr::IntLit(v, _) => Ok(Value::Int(*v)),
            Expr::FloatLit(v, _) => Ok(Value::Float(*v)),
            Expr::BoolLit(v, _) => Ok(Value::Bool(*v)),
            Expr::StrLit(v, _) => Ok(Value::Str(v.clone())),
            Expr::ListLit(items, _) => {
                let mut vals = Vec::new();
                for it in items {
                    vals.push(self.eval_expr(env, it)?);
                }
                Ok(Value::List(vals))
            }
            Expr::DictLit(entries, _) => {
                let mut vals = Vec::new();
                for (k, v) in entries {
                    vals.push((k.clone(), self.eval_expr(env, v)?));
                }
                Ok(Value::Dict(vals))
            }
            Expr::FStr(segs, _) => {
                let mut out = String::new();
                for seg in segs {
                    match seg {
                        FStrSeg::Lit(s) => out.push_str(s),
                        FStrSeg::Code(e) => {
                            let v = self.eval_expr(env, e)?;
                            out.push_str(&v.display());
                        }
                    }
                }
                Ok(Value::Str(out))
            }
            Expr::Ident { name, span } => match env.get(name) {
                Some(v) => Ok(v.clone()),
                None => Err(self.runtime_err(
                    codes::UNDEFINED,
                    format!("undefined variable `{}`", name),
                    *span,
                    Some("declare the variable before reading it"),
                )),
            },
            Expr::Field { obj, field, span } => {
                let v = self.eval_expr(env, obj)?;
                match v {
                    Value::Dict(entries) => {
                        // struct 实例 / dict 字段访问：按键查找
                        match entries.iter().find(|(k, _)| k == field) {
                            Some((_, val)) => Ok(val.clone()),
                            None => Err(self.runtime_err(
                                codes::UNDEFINED,
                                format!("unknown field `{}` (dict/struct has {})", field,
                                    entries.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(", ")),
                                *span,
                                Some("check the field name, or the struct definition"),
                            )),
                        }
                    }
                    Value::Error(e) => match field.as_str() {
                        "code" => Ok(Value::Str(e.code.to_string())),
                        "message" => Ok(Value::Str(e.message.clone())),
                        "file" => Ok(Value::Str(e.file.clone())),
                        "context" => Ok(Value::Str(e.context.clone())),
                        "line" => Ok(Value::Int(e.line as i64)),
                        "col" => Ok(Value::Int(e.col as i64)),
                        other => Err(self.runtime_err(
                            codes::UNDEFINED,
                            format!("unknown error field `{}`", other),
                            *span,
                            Some("error fields: code, message, file, line, col, context"),
                        )),
                    },
                    other => Err(self.runtime_err(
                        codes::TYPE_MISMATCH,
                        format!(
                            "field access `.{}` requires an `error` value, got `{}`",
                            field,
                            other.type_name()
                        ),
                        *span,
                        Some("only error values (catch variables) support field access"),
                    )),
                }
            }
            Expr::Unary { op, expr, span } => {
                let v = self.eval_expr(env, expr)?;
                match op {
                    UnOp::Neg => match v {
                        Value::Int(x) => Ok(Value::Int(-x)),
                        Value::Float(x) => Ok(Value::Float(-x)),
                        other => Err(self.runtime_err(
                            codes::TYPE_MISMATCH,
                            format!("unary `-` requires a number, got `{}`", other.type_name()),
                            *span,
                            None::<&str>,
                        )),
                    },
                    UnOp::Not => match v {
                        Value::Bool(b) => Ok(Value::Bool(!b)),
                        other => Err(self.runtime_err(
                            codes::TYPE_MISMATCH,
                            format!("`!` requires a `bool`, got `{}`", other.type_name()),
                            *span,
                            None::<&str>,
                        )),
                    },
                }
            }
            Expr::Binary { op, lhs, rhs, span } => self.eval_binary(env, *op, lhs, rhs, *span),
            Expr::Match { value, arms, span } => {
                let v = self.eval_expr(env, value)?;
                for (pat, body) in arms {
                    let matched = match pat {
                        None => true, // `_` 通配符
                        Some(p) => {
                            let pv = self.eval_expr(env, p)?;
                            self.values_eq(&v, &pv, *span)?
                        }
                    };
                    if matched {
                        return self.eval_expr(env, body);
                    }
                }
                Err(self.runtime_err(
                    codes::SYNTAX,
                    "no match arm matched the value",
                    *span,
                    Some("add a `_` wildcard arm as the fallback"),
                ))
            }
            Expr::Call { callee, args, span } => {
                let mut arg_vals = Vec::new();
                for a in args {
                    arg_vals.push(self.eval_expr(env, a)?);
                }
                self.call_fn(callee, arg_vals, *span)
            }
        }
    }

    fn eval_binary(&mut self, env: &Env, op: BinOp, lhs: &Expr, rhs: &Expr, span: Span) -> Result<Value, ZError> {
        match op {
            BinOp::And => {
                let l = self.eval_expr(env, lhs)?;
                if let Value::Bool(false) = l {
                    return Ok(Value::Bool(false));
                }
                let r = self.eval_expr(env, rhs)?;
                self.require_bool_val(r, span)
            }
            BinOp::Or => {
                let l = self.eval_expr(env, lhs)?;
                if let Value::Bool(true) = l {
                    return Ok(Value::Bool(true));
                }
                let r = self.eval_expr(env, rhs)?;
                self.require_bool_val(r, span)
            }
            BinOp::Eq | BinOp::Ne => {
                let l = self.eval_expr(env, lhs)?;
                let r = self.eval_expr(env, rhs)?;
                let eq = self.values_eq(&l, &r, span)?;
                Ok(Value::Bool(if op == BinOp::Eq { eq } else { !eq }))
            }
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                let l = self.eval_expr(env, lhs)?;
                let r = self.eval_expr(env, rhs)?;
                let c = self.values_cmp(&l, &r, span)?;
                Ok(Value::Bool(match op {
                    BinOp::Lt => c == std::cmp::Ordering::Less,
                    BinOp::Le => c != std::cmp::Ordering::Greater,
                    BinOp::Gt => c == std::cmp::Ordering::Greater,
                    BinOp::Ge => c != std::cmp::Ordering::Less,
                    _ => unreachable!(),
                }))
            }
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
                let l = self.eval_expr(env, lhs)?;
                let r = self.eval_expr(env, rhs)?;
                self.arith(op, l, r, span)
            }
        }
    }

    fn require_bool_val(&self, v: Value, span: Span) -> Result<Value, ZError> {
        match v {
            Value::Bool(_) => Ok(v),
            other => Err(self.runtime_err(
                codes::TYPE_MISMATCH,
                format!("logical operators require `bool` operands, got `{}`", other.type_name()),
                span,
                None::<&str>,
            )),
        }
    }

    fn values_eq(&self, a: &Value, b: &Value, span: Span) -> Result<bool, ZError> {
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => Ok(x == y),
            (Value::Float(x), Value::Float(y)) => Ok(x == y),
            (Value::Bool(x), Value::Bool(y)) => Ok(x == y),
            (Value::Str(x), Value::Str(y)) => Ok(x == y),
            (Value::List(x), Value::List(y)) => Ok(x == y),
            (Value::Dict(x), Value::Dict(y)) => Ok(x == y),
            (Value::Ptr(x), Value::Ptr(y)) => Ok(x == y),
            // ptr 与整数比较：`p == 0` 判断 NULL，`p == n` 比较句柄数值
            (Value::Ptr(x), Value::Int(y)) => Ok(*x as i64 == *y),
            (Value::Int(x), Value::Ptr(y)) => Ok(*x == *y as i64),
            (Value::Null, Value::Null) => Ok(true),
            _ => Err(self.runtime_err(
                codes::TYPE_MISMATCH,
                format!("cannot compare `{}` with `{}`", a.type_name(), b.type_name()),
                span,
                Some("Hone has no implicit type conversion"),
            )),
        }
    }

    fn values_cmp(&self, a: &Value, b: &Value, span: Span) -> Result<std::cmp::Ordering, ZError> {
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => Ok(x.cmp(y)),
            (Value::Float(x), Value::Float(y)) => x.partial_cmp(y).ok_or_else(|| {
                self.runtime_err(
                    codes::TYPE_MISMATCH,
                    "cannot compare NaN values",
                    span,
                    None::<&str>,
                )
            }),
            _ => Err(self.runtime_err(
                codes::TYPE_MISMATCH,
                format!("cannot compare `{}` with `{}`", a.type_name(), b.type_name()),
                span,
                Some("comparison operators work on `int` / `float`"),
            )),
        }
    }

    fn arith(&self, op: BinOp, a: Value, b: Value, span: Span) -> Result<Value, ZError> {
        let div_zero = |self_: &Self| {
            self_.runtime_err(
                codes::DIV_ZERO,
                "division by zero",
                span,
                Some("check the divisor before dividing"),
            )
        };
        match (&a, &b) {
            (Value::Int(x), Value::Int(y)) => {
                let r = match op {
                    BinOp::Add => x.checked_add(*y),
                    BinOp::Sub => x.checked_sub(*y),
                    BinOp::Mul => x.checked_mul(*y),
                    BinOp::Div => {
                        if *y == 0 {
                            return Err(div_zero(self));
                        }
                        x.checked_div(*y)
                    }
                    BinOp::Mod => {
                        if *y == 0 {
                            return Err(div_zero(self));
                        }
                        x.checked_rem(*y)
                    }
                    _ => unreachable!(),
                };
                match r {
                    Some(v) => Ok(Value::Int(v)),
                    None => Err(self.runtime_err(
                        codes::INTEGER_OVERFLOW,
                        "integer overflow",
                        span,
                        Some("the result does not fit in a 64-bit signed integer"),
                    )),
                }
            }
            (Value::Float(x), Value::Float(y)) => {
                let r = match op {
                    BinOp::Add => x + y,
                    BinOp::Sub => x - y,
                    BinOp::Mul => x * y,
                    BinOp::Div => {
                        if *y == 0.0 {
                            return Err(div_zero(self));
                        }
                        x / y
                    }
                    BinOp::Mod => {
                        if *y == 0.0 {
                            return Err(div_zero(self));
                        }
                        x % y
                    }
                    _ => unreachable!(),
                };
                Ok(Value::Float(r))
            }
            (Value::Str(x), Value::Str(y)) if op == BinOp::Add => Ok(Value::Str(format!("{}{}", x, y))),
            _ => Err(self.runtime_err(
                codes::TYPE_MISMATCH,
                format!(
                    "cannot apply `{}` to `{}` and `{}`",
                    op.symbol(),
                    a.type_name(),
                    b.type_name()
                ),
                span,
                Some("Hone has no implicit type conversion"),
            )),
        }
    }
}

fn default_value(ty: TyName) -> Value {
    match ty {
        TyName::Int => Value::Int(0),
        TyName::Float => Value::Float(0.0),
        TyName::Bool => Value::Bool(false),
        TyName::Str => Value::Str(String::new()),
        // 泛型类型参数无固定默认值（编译期擦除，运行时不可达）
        TyName::Var(_) => Value::Null,
    }
}

/// ~/.hone/cache/ 模块缓存目录（Windows 用 USERPROFILE）。
pub(crate) fn hone_cache_dir() -> std::path::PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(home).join(".hone").join("cache")
}
