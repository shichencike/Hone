// codegen.rs - hone build --dll 的 C 代码生成（类型化）
// 类型映射（与规范一致）：int → int64_t，float → double，bool → bool（<stdbool.h>），str → const char*
// 支持：数值/布尔/字符串运算、比较（含 strcmp）、if/while、递归与函数间调用。
// 实现要点：
//   - 轻量类型推导（参数注解或默认 int64_t、变量从初始化式推导、返回从 return 推导或注解）
//   - str 拼接使用 GNU 语句表达式（栈缓冲 256B）；str 返回值经 static 缓冲（2048B）中转
//   - 混合数值类型按语言语义报错（无隐式转换）；内置函数/模块调用报 error[H999]

use std::collections::{HashMap, HashSet};

use crate::ast::*;
use crate::error::codes;
use crate::error::ZError;
use crate::lexer::Span;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CType {
    Int,
    Double,
    Bool,
    Str,
    Void,
}

impl CType {
    fn c_name(self) -> &'static str {
        match self {
            CType::Int => "int64_t",
            CType::Double => "double",
            CType::Bool => "bool",
            CType::Str => "const char*",
            CType::Void => "void",
        }
    }

    fn from_annot(t: TyName) -> CType {
        match t {
            TyName::Int => CType::Int,
            TyName::Float => CType::Double,
            TyName::Bool => CType::Bool,
            TyName::Str => CType::Str,
        }
    }

    fn is_num(self) -> bool {
        matches!(self, CType::Int | CType::Double)
    }
}

struct CFn {
    name: String,
    params: Vec<Param>,
    ret: Option<TyName>,
    body: Vec<Stmt>,
}

/// 生成上下文：函数体内变量类型表 + str 拼接临时缓冲编号。
struct GenCtx {
    var_types: HashMap<String, CType>,
    str_temp: usize,
}

pub struct Codegen {
    file: String,
    src: String,
    fns: HashMap<String, CFn>,
}

/// 收集脚本中所有 @export 的函数名。
pub fn collect_exports(program: &Program) -> Vec<String> {
    let mut out = Vec::new();
    collect_exports_stmts(&program.stmts, &mut out);
    out
}

fn collect_exports_stmts(stmts: &[Stmt], out: &mut Vec<String>) {
    for s in stmts {
        match s {
            Stmt::Export { name, .. } => out.push(name.clone()),
            Stmt::Block { stmts, .. } => collect_exports_stmts(stmts, out),
            Stmt::If { then_branch, else_branch, .. } => {
                collect_exports_stmts(then_branch, out);
                if let Some(eb) = else_branch {
                    collect_exports_stmts(eb, out);
                }
            }
            Stmt::While { body, .. } => collect_exports_stmts(body, out),
            _ => {}
        }
    }
}

/// 生成 C 源码。exports 为要导出的函数名列表。
pub fn generate(program: &Program, exports: &[String], file: &str, src: &str) -> Result<String, ZError> {
    let mut cg = Codegen {
        file: file.to_string(),
        src: src.to_string(),
        fns: HashMap::new(),
    };
    cg.collect_fns(&program.stmts)?;

    // 收集导出函数及其调用链上的所有函数（去重、保持顺序）
    let mut reachable: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for e in exports {
        cg.collect_reachable(e, &mut reachable, &mut seen)?;
    }
    for e in exports {
        if !reachable.contains(e) {
            reachable.push(e.clone());
        }
    }

    let mut out = String::new();
    out.push_str("// 由 hone build --dll 自动生成，请勿手动编辑\n");
    out.push_str("#include <stdint.h>\n");
    out.push_str("#include <stdbool.h>\n");
    out.push_str("#include <stdio.h>\n");
    out.push_str("#include <string.h>\n\n");

    // 函数原型（处理递归与相互调用）
    for name in &reachable {
        let is_exported = exports.iter().any(|e| e == name);
        cg.gen_proto(name, is_exported, &mut out)?;
    }
    out.push('\n');

    // 函数实现
    for name in &reachable {
        let is_exported = exports.iter().any(|e| e == name);
        cg.gen_impl(name, is_exported, &mut out)?;
        out.push('\n');
    }
    Ok(out)
}

impl Codegen {
    fn collect_fns(&mut self, stmts: &[Stmt]) -> Result<(), ZError> {
        for stmt in stmts {
            match stmt {
                Stmt::FnDef { name, params, ret, body, tmp, .. } => {
                    if !tmp {
                        self.fns.insert(
                            name.clone(),
                            CFn {
                                name: name.clone(),
                                params: params.clone(),
                                ret: *ret,
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
                _ => {}
            }
        }
        Ok(())
    }

    /// 递归收集 name 及其调用的用户函数。
    fn collect_reachable(&self, name: &str, reachable: &mut Vec<String>, seen: &mut HashSet<String>) -> Result<(), ZError> {
        if seen.contains(name) {
            return Ok(());
        }
        let f = match self.fns.get(name) {
            Some(f) => f,
            None => {
                // checker 已保证 @export 的函数存在，此处兜底
                return Err(self.zerr(
                    codes::UNDEFINED,
                    format!("cannot export undefined function `{}`", name),
                    Span { line: 1, col: 1, len: 1 },
                    Some("define the function before exporting it"),
                ));
            }
        };
        seen.insert(name.to_string());
        for s in &f.body {
            self.collect_calls_stmt(s, reachable, seen)?;
        }
        reachable.push(name.to_string());
        Ok(())
    }

    fn collect_calls_stmt(&self, stmt: &Stmt, reachable: &mut Vec<String>, seen: &mut HashSet<String>) -> Result<(), ZError> {
        match stmt {
            Stmt::Assign { value, .. } => self.collect_calls_expr(value, reachable, seen),
            Stmt::VarDecl { init, .. } => match init {
                Some(e) => self.collect_calls_expr(e, reachable, seen),
                None => Ok(()),
            },
            Stmt::If { cond, then_branch, else_branch, .. } => {
                self.collect_calls_expr(cond, reachable, seen)?;
                for s in then_branch {
                    self.collect_calls_stmt(s, reachable, seen)?;
                }
                if let Some(eb) = else_branch {
                    for s in eb {
                        self.collect_calls_stmt(s, reachable, seen)?;
                    }
                }
                Ok(())
            }
            Stmt::While { cond, body, .. } => {
                self.collect_calls_expr(cond, reachable, seen)?;
                for s in body {
                    self.collect_calls_stmt(s, reachable, seen)?;
                }
                Ok(())
            }
            Stmt::Return { value, .. } => match value {
                Some(e) => self.collect_calls_expr(e, reachable, seen),
                None => Ok(()),
            },
            Stmt::ExprStmt { expr, .. } => self.collect_calls_expr(expr, reachable, seen),
            _ => Ok(()),
        }
    }

    fn collect_calls_expr(&self, e: &Expr, reachable: &mut Vec<String>, seen: &mut HashSet<String>) -> Result<(), ZError> {
        match e {
            Expr::Call { callee, args, .. } => {
                if self.fns.contains_key(callee) {
                    self.collect_reachable(callee, reachable, seen)?;
                }
                for a in args {
                    self.collect_calls_expr(a, reachable, seen)?;
                }
                Ok(())
            }
            Expr::Unary { expr, .. } => self.collect_calls_expr(expr, reachable, seen),
            Expr::Binary { lhs, rhs, .. } => {
                self.collect_calls_expr(lhs, reachable, seen)?;
                self.collect_calls_expr(rhs, reachable, seen)
            }
            _ => Ok(()),
        }
    }

    // ============ 轻量类型推导 ============

    /// 推导函数签名：(参数类型, 返回类型, 变量类型表)。
    /// 返回类型：注解优先；否则从 return 表达式推导；无 return → void。
    fn infer_signature(&self, f: &CFn, stack: &mut Vec<String>) -> Result<(Vec<CType>, CType, HashMap<String, CType>), ZError> {
        let mut param_types = Vec::new();
        let mut vt: HashMap<String, CType> = HashMap::new();
        for p in &f.params {
            let t = p.ty.map(CType::from_annot).unwrap_or(CType::Int);
            param_types.push(t);
            vt.insert(p.name.clone(), t);
        }
        self.infer_body_types(&f.body, &mut vt, stack)?;

        let ret = if let Some(a) = f.ret {
            CType::from_annot(a)
        } else {
            if stack.contains(&f.name) {
                return Err(self.zerr(
                    codes::CANNOT_INFER,
                    format!("cannot infer the return type of `{}` (recursive); add a `-> type` annotation", f.name),
                    Span { line: 1, col: 1, len: 1 },
                    Some("annotate the function, e.g. `fn f() -> int { ... }`"),
                ));
            }
            stack.push(f.name.clone());
            let r = self.return_type_stmt(&f.body, &vt, stack)?;
            stack.pop();
            r.unwrap_or(CType::Void)
        };
        Ok((param_types, ret, vt))
    }

    /// 顺序扫描语句，从初始化式推导变量类型（变量先声明后使用）。
    fn infer_body_types(&self, stmts: &[Stmt], vt: &mut HashMap<String, CType>, stack: &mut Vec<String>) -> Result<(), ZError> {
        for s in stmts {
            match s {
                Stmt::Assign { name, value, .. } => {
                    if !vt.contains_key(name) {
                        let t = self.infer_expr_type(value, vt, stack)?;
                        vt.insert(name.clone(), t);
                    }
                }
                Stmt::VarDecl { name, ty, init, .. } => {
                    let annot = CType::from_annot(*ty);
                    if let Some(e) = init {
                        let t = self.infer_expr_type(e, vt, stack)?;
                        if t != annot {
                            return Err(self.zerr(
                                codes::TYPE_MISMATCH,
                                format!(
                                    "type mismatch: variable `{}` is declared `{}` but initialized with `{}`",
                                    name,
                                    annot.c_name(),
                                    t.c_name()
                                ),
                                expr_span(e),
                                Some("Hone has no implicit type conversion"),
                            ));
                        }
                    }
                    vt.insert(name.clone(), annot);
                }
                Stmt::Block { stmts, .. } => self.infer_body_types(stmts, vt, stack)?,
                Stmt::If { cond: _, then_branch, else_branch, .. } => {
                    self.infer_body_types(then_branch, vt, stack)?;
                    if let Some(eb) = else_branch {
                        self.infer_body_types(eb, vt, stack)?;
                    }
                }
                Stmt::While { body, .. } => self.infer_body_types(body, vt, stack)?,
                _ => {}
            }
        }
        Ok(())
    }

    /// 推导语句序列的返回类型（第一个 return；多个不一致报错）。
    fn return_type_stmt(&self, stmts: &[Stmt], vt: &HashMap<String, CType>, stack: &mut Vec<String>) -> Result<Option<CType>, ZError> {
        let mut ret: Option<CType> = None;
        for s in stmts {
            let t = match s {
                Stmt::Return { value: Some(e), .. } => Some(self.infer_expr_type(e, vt, stack)?),
                Stmt::Return { value: None, .. } => Some(CType::Void),
                Stmt::Block { stmts, .. } => self.return_type_stmt(stmts, vt, stack)?,
                Stmt::If { then_branch, else_branch, .. } => {
                    let a = self.return_type_stmt(then_branch, vt, stack)?;
                    let b = match else_branch {
                        Some(eb) => self.return_type_stmt(eb, vt, stack)?,
                        None => None,
                    };
                    match (a, b) {
                        (Some(x), Some(y)) if x != y => {
                            return Err(self.zerr(
                                codes::TYPE_MISMATCH,
                                format!("inconsistent return types: `{}` vs `{}`", x.c_name(), y.c_name()),
                                stmt_span(s),
                                Some("make all branches return the same type"),
                            ));
                        }
                        (Some(x), _) | (_, Some(x)) => Some(x),
                        _ => None,
                    }
                }
                Stmt::While { body, .. } => self.return_type_stmt(body, vt, stack)?,
                _ => None,
            };
            if let Some(t) = t {
                if let Some(prev) = ret {
                    if prev != t {
                        return Err(self.zerr(
                            codes::TYPE_MISMATCH,
                            format!("inconsistent return types: `{}` vs `{}`", prev.c_name(), t.c_name()),
                            stmt_span(s),
                            Some("make all return statements return the same type"),
                        ));
                    }
                } else {
                    ret = Some(t);
                }
            }
        }
        Ok(ret)
    }

    /// 推导表达式类型（不做代码生成）。
    fn infer_expr_type(&self, e: &Expr, vt: &HashMap<String, CType>, stack: &mut Vec<String>) -> Result<CType, ZError> {
        match e {
            Expr::IntLit(..) => Ok(CType::Int),
            Expr::FloatLit(..) => Ok(CType::Double),
            Expr::BoolLit(..) => Ok(CType::Bool),
            Expr::StrLit(..) => Ok(CType::Str),
            Expr::Ident { name, span } => vt.get(name).copied().ok_or_else(|| {
                self.zerr(
                    codes::UNDEFINED,
                    format!("undefined variable `{}`", name),
                    *span,
                    Some("declare the variable before reading it"),
                )
            }),
            Expr::Field { span, .. } => Err(self.zerr(
                codes::NOT_IMPLEMENTED,
                "field access is not supported in DLL builds",
                *span,
                Some("field access (`e.code` etc.) works in interpreted mode only"),
            )),
            Expr::ListLit(..) | Expr::DictLit(..) | Expr::FStr(..) => Err(self.zerr(
                codes::NOT_IMPLEMENTED,
                "list/dict literals and f-strings are not supported in DLL builds",
                expr_span(e),
                Some("these features work in interpreted mode only"),
            )),
            Expr::Match { span, .. } => Err(self.zerr(
                codes::NOT_IMPLEMENTED,
                "match expressions are not supported in DLL builds",
                *span,
                Some("match works in interpreted mode only"),
            )),
            Expr::Unary { op, expr, span } => {
                let t = self.infer_expr_type(expr, vt, stack)?;
                match op {
                    UnOp::Neg if t.is_num() => Ok(t),
                    UnOp::Neg => Err(self.zerr(
                        codes::TYPE_MISMATCH,
                        format!("unary `-` requires a number, got `{}`", t.c_name()),
                        *span,
                        Some("negation works on `int` / `float`"),
                    )),
                    UnOp::Not if t == CType::Bool => Ok(CType::Bool),
                    UnOp::Not => Err(self.zerr(
                        codes::TYPE_MISMATCH,
                        format!("`!` requires a `bool`, got `{}`", t.c_name()),
                        *span,
                        Some("logical NOT works on `bool` values"),
                    )),
                }
            }
            Expr::Binary { op, lhs, rhs, span } => {
                let l = self.infer_expr_type(lhs, vt, stack)?;
                let r = self.infer_expr_type(rhs, vt, stack)?;
                self.infer_binary(*op, l, r, *span)
            }
            Expr::Call { callee, args: _, span } => {
                if let Some(f) = self.fns.get(callee) {
                    let (_, ret, _) = self.infer_signature(f, stack)?;
                    Ok(ret)
                } else {
                    Err(self.zerr(
                        codes::NOT_IMPLEMENTED,
                        format!("cannot translate call to `{}` (builtins are not supported in --dll yet)", callee),
                        *span,
                        Some("use only user-defined functions in exported code"),
                    ))
                }
            }
        }
    }

    fn infer_binary(&self, op: BinOp, l: CType, r: CType, span: Span) -> Result<CType, ZError> {
        let err_mixed = || {
            self.zerr(
                codes::TYPE_MISMATCH,
                format!("cannot apply `{}` to `{}` and `{}` (no implicit conversion)", op.symbol(), l.c_name(), r.c_name()),
                span,
                Some("Hone has no implicit type conversion"),
            )
        };
        match op {
            BinOp::Add => match (l, r) {
                (CType::Int, CType::Int) => Ok(CType::Int),
                (CType::Double, CType::Double) => Ok(CType::Double),
                (CType::Str, CType::Str) => Ok(CType::Str),
                (a, b) if a.is_num() && b.is_num() => Err(err_mixed()),
                _ => Err(self.zerr(
                    codes::TYPE_MISMATCH,
                    format!("cannot apply `+` to `{}` and `{}`", l.c_name(), r.c_name()),
                    span,
                    Some("`+` works on numbers and concatenates strings"),
                )),
            },
            BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
                if l == r && l.is_num() {
                    Ok(l)
                } else if l.is_num() && r.is_num() {
                    Err(err_mixed())
                } else {
                    Err(self.zerr(
                        codes::TYPE_MISMATCH,
                        format!("cannot apply `{}` to `{}` and `{}`", op.symbol(), l.c_name(), r.c_name()),
                        span,
                        Some("arithmetic works on `int` / `float`"),
                    ))
                }
            }
            BinOp::Eq | BinOp::Ne => {
                if l == r {
                    Ok(CType::Bool)
                } else {
                    Err(err_mixed())
                }
            }
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                if l == r && l.is_num() {
                    Ok(CType::Bool)
                } else if l.is_num() && r.is_num() {
                    Err(err_mixed())
                } else {
                    Err(self.zerr(
                        codes::TYPE_MISMATCH,
                        format!("`{}` requires numeric operands, got `{}` and `{}`", op.symbol(), l.c_name(), r.c_name()),
                        span,
                        Some("comparison operators work on `int` / `float`"),
                    ))
                }
            }
            BinOp::And | BinOp::Or => {
                if l == CType::Bool && r == CType::Bool {
                    Ok(CType::Bool)
                } else {
                    Err(self.zerr(
                        codes::TYPE_MISMATCH,
                        format!("`{}` requires `bool` operands, got `{}` and `{}`", op.symbol(), l.c_name(), r.c_name()),
                        span,
                        Some("logical operators work on `bool` values"),
                    ))
                }
            }
        }
    }

    fn zerr(&self, code: &'static str, msg: impl Into<String>, span: Span, help: Option<impl Into<String>>) -> ZError {
        ZError::new(code, msg, &self.file, &self.src, span.line, span.col, span.len.max(1), help)
    }

    // ============ C 代码生成 ============

    fn gen_proto(&self, name: &str, exported: bool, out: &mut String) -> Result<(), ZError> {
        let f = &self.fns[name];
        let (param_types, ret_type, _) = self.infer_signature(f, &mut Vec::new())?;
        let params: Vec<String> = f
            .params
            .iter()
            .zip(&param_types)
            .map(|(p, t)| format!("{} {}", t.c_name(), p.name))
            .collect();
        // 辅助函数用 static 原型（与 static 定义匹配）；导出函数用外部声明
        let prefix = if exported { "" } else { "static " };
        out.push_str(&format!("{}{} {}({});\n", prefix, ret_type.c_name(), f.name, params.join(", ")));
        Ok(())
    }

    fn gen_impl(&self, name: &str, exported: bool, out: &mut String) -> Result<(), ZError> {
        let f = &self.fns[name];
        let (param_types, ret_type, vt) = self.infer_signature(f, &mut Vec::new())?;
        let params: Vec<String> = f
            .params
            .iter()
            .zip(&param_types)
            .map(|(p, t)| format!("{} {}", t.c_name(), p.name))
            .collect();
        let prefix = if exported {
            if cfg!(windows) {
                "__declspec(dllexport) "
            } else {
                "__attribute__((visibility(\"default\"))) "
            }
        } else {
            "static "
        };
        out.push_str(&format!("{}{} {}({}) {{\n", prefix, ret_type.c_name(), f.name, params.join(", ")));

        let mut ctx = GenCtx {
            var_types: vt,
            str_temp: 0,
        };
        // str 返回值经 static 缓冲中转（DLL 重复调用会覆盖，文档注明）
        if ret_type == CType::Str {
            out.push_str("    static char ka_ret_buf[2048];\n");
        }
        for s in &f.body {
            self.gen_stmt(s, &mut ctx, ret_type, out)?;
        }
        // 无 return 语句时补默认返回（避免编译器警告）
        if !has_return(&f.body) && ret_type != CType::Void {
            match ret_type {
                CType::Str => out.push_str("    return NULL;\n"),
                CType::Bool => out.push_str("    return false;\n"),
                _ => out.push_str("    return 0;\n"),
            }
        }
        out.push_str("}\n");
        Ok(())
    }

    // ---------- 语句生成 ----------

    fn gen_stmt(&self, stmt: &Stmt, ctx: &mut GenCtx, ret_type: CType, out: &mut String) -> Result<(), ZError> {
        match stmt {
            Stmt::Assign { name, value, span } => {
                let (t, code) = self.gen_expr(value, ctx)?;
                match ctx.var_types.get(name) {
                    Some(prev) if *prev != t => {
                        return Err(self.zerr(
                            codes::TYPE_MISMATCH,
                            format!("type mismatch: variable `{}` is locked to `{}`, got `{}`", name, prev.c_name(), t.c_name()),
                            *span,
                            Some("Hone types are locked after inference; no implicit conversion is allowed"),
                        ));
                    }
                    Some(_) => out.push_str(&format!("    {} = {};\n", name, code)),
                    None => {
                        ctx.var_types.insert(name.clone(), t);
                        out.push_str(&format!("    {} {} = {};\n", t.c_name(), name, code));
                    }
                }
                Ok(())
            }
            Stmt::VarDecl { name, ty, init, span } => {
                let annot = CType::from_annot(*ty);
                if let Some(e) = init {
                    let (t, code) = self.gen_expr(e, ctx)?;
                    if t != annot {
                        return Err(self.zerr(
                            codes::TYPE_MISMATCH,
                            format!(
                                "type mismatch: variable `{}` is declared `{}` but initialized with `{}`",
                                name,
                                annot.c_name(),
                                t.c_name()
                            ),
                            *span,
                            Some("Hone has no implicit type conversion"),
                        ));
                    }
                    if ctx.var_types.contains_key(name) {
                        out.push_str(&format!("    {} = {};\n", name, code));
                    } else {
                        ctx.var_types.insert(name.clone(), annot);
                        out.push_str(&format!("    {} {} = {};\n", annot.c_name(), name, code));
                    }
                } else {
                    ctx.var_types.insert(name.clone(), annot);
                    let d = match annot {
                        CType::Int => "0",
                        CType::Double => "0.0",
                        CType::Bool => "false",
                        CType::Str => "NULL",
                        CType::Void => unreachable!(),
                    };
                    out.push_str(&format!("    {} {} = {};\n", annot.c_name(), name, d));
                }
                Ok(())
            }
            Stmt::Block { stmts, .. } => {
                out.push_str("    {\n");
                for s in stmts {
                    self.gen_stmt(s, ctx, ret_type, out)?;
                }
                out.push_str("    }\n");
                Ok(())
            }
            Stmt::If { cond, then_branch, else_branch, span } => {
                let (t, c) = self.gen_expr(cond, ctx)?;
                if t != CType::Bool {
                    return Err(self.zerr(
                        codes::COND_NOT_BOOL,
                        format!("condition must be `bool`, got `{}`", t.c_name()),
                        *span,
                        Some("use a comparison like `x == 1`, or a boolean variable"),
                    ));
                }
                out.push_str(&format!("    if ({}) {{\n", c));
                for s in then_branch {
                    self.gen_stmt(s, ctx, ret_type, out)?;
                }
                out.push_str("    }");
                if let Some(eb) = else_branch {
                    out.push_str(" else {\n");
                    for s in eb {
                        self.gen_stmt(s, ctx, ret_type, out)?;
                    }
                    out.push_str("    }");
                }
                out.push('\n');
                Ok(())
            }
            Stmt::While { cond, body, span } => {
                let (t, c) = self.gen_expr(cond, ctx)?;
                if t != CType::Bool {
                    return Err(self.zerr(
                        codes::COND_NOT_BOOL,
                        format!("condition must be `bool`, got `{}`", t.c_name()),
                        *span,
                        Some("use a comparison like `i < 10`, or a boolean variable"),
                    ));
                }
                out.push_str(&format!("    while ({}) {{\n", c));
                for s in body {
                    self.gen_stmt(s, ctx, ret_type, out)?;
                }
                out.push_str("    }\n");
                Ok(())
            }
            Stmt::Return { value, span } => {
                match value {
                    Some(e) => {
                        let (t, code) = self.gen_expr(e, ctx)?;
                        if t != ret_type {
                            return Err(self.zerr(
                                codes::TYPE_MISMATCH,
                                format!(
                                    "return type mismatch: function returns `{}`, got `{}`",
                                    ret_type.c_name(),
                                    t.c_name()
                                ),
                                *span,
                                Some("make the return value match the function's return type"),
                            ));
                        }
                        if ret_type == CType::Str {
                            out.push_str(&format!(
                                "    {{ snprintf(ka_ret_buf, sizeof ka_ret_buf, \"%s\", {}); return ka_ret_buf; }}\n",
                                code
                            ));
                        } else {
                            out.push_str(&format!("    return {};\n", code));
                        }
                    }
                    None => {
                        if ret_type != CType::Void {
                            return Err(self.zerr(
                                codes::TYPE_MISMATCH,
                                format!("bare `return;` in a function returning `{}`", ret_type.c_name()),
                                *span,
                                Some("return a value of the declared type"),
                            ));
                        }
                        out.push_str("    return;\n");
                    }
                }
                Ok(())
            }
            Stmt::ExprStmt { expr, span } => {
                if let Expr::Call { callee, args, .. } = expr {
                    if !self.fns.contains_key(callee) {
                        return Err(self.zerr(
                            codes::NOT_IMPLEMENTED,
                            format!("cannot translate call to `{}` (builtins are not supported in --dll yet)", callee),
                            *span,
                            Some("use only user-defined functions in exported code"),
                        ));
                    }
                    let mut out_args = Vec::new();
                    for a in args {
                        let (_, s) = self.gen_expr(a, ctx)?;
                        out_args.push(s);
                    }
                    out.push_str(&format!("    {}({});\n", callee, out_args.join(", ")));
                    Ok(())
                } else {
                    Err(self.zerr(
                        codes::NOT_IMPLEMENTED,
                        "cannot translate this expression statement to C",
                        expr_span(expr),
                        Some("only function calls are supported as expression statements"),
                    ))
                }
            }
            Stmt::FnDef { .. } => Ok(()), // 已收集
            Stmt::DebugPrint { .. } | Stmt::Breakpoint { .. } => Ok(()), // 调试/临时语句不生成 C 代码
            other => Err(self.zerr(
                codes::NOT_IMPLEMENTED,
                format!("cannot translate `{:?}` to C in v0.1.0", other),
                stmt_span(other),
                Some("exported functions must be pure computations"),
            )),
        }
    }

    // ---------- 表达式生成 ----------

    /// 生成表达式 C 代码并返回其类型。
    fn gen_expr(&self, e: &Expr, ctx: &mut GenCtx) -> Result<(CType, String), ZError> {
        match e {
            Expr::IntLit(v, _) => Ok((CType::Int, v.to_string())),
            Expr::FloatLit(v, _) => {
                let mut s = v.to_string();
                if !s.contains('.') && !s.contains('e') && !s.contains('E') {
                    s.push_str(".0");
                }
                Ok((CType::Double, s))
            }
            Expr::BoolLit(b, _) => Ok((CType::Bool, if *b { "true".to_string() } else { "false".to_string() })),
            Expr::StrLit(s, _) => Ok((CType::Str, c_str_lit(s))),
            Expr::Ident { name, span } => match ctx.var_types.get(name) {
                Some(t) => Ok((*t, name.clone())),
                None => Err(self.zerr(
                    codes::UNDEFINED,
                    format!("undefined variable `{}`", name),
                    *span,
                    Some("declare the variable before reading it"),
                )),
            },
            Expr::Field { span, .. } => Err(self.zerr(
                codes::NOT_IMPLEMENTED,
                "field access is not supported in DLL builds",
                *span,
                Some("field access (`e.code` etc.) works in interpreted mode only"),
            )),
            Expr::ListLit(..) | Expr::DictLit(..) | Expr::FStr(..) => Err(self.zerr(
                codes::NOT_IMPLEMENTED,
                "list/dict literals and f-strings are not supported in DLL builds",
                expr_span(e),
                Some("these features work in interpreted mode only"),
            )),
            Expr::Match { span, .. } => Err(self.zerr(
                codes::NOT_IMPLEMENTED,
                "match expressions are not supported in DLL builds",
                *span,
                Some("match works in interpreted mode only"),
            )),
            Expr::Unary { op, expr, span } => {
                let (t, s) = self.gen_expr(expr, ctx)?;
                match op {
                    UnOp::Neg if t.is_num() => Ok((t, format!("(-{})", s))),
                    UnOp::Neg => Err(self.zerr(
                        codes::TYPE_MISMATCH,
                        format!("unary `-` requires a number, got `{}`", t.c_name()),
                        *span,
                        Some("negation works on `int` / `float`"),
                    )),
                    UnOp::Not if t == CType::Bool => Ok((CType::Bool, format!("(!{})", s))),
                    UnOp::Not => Err(self.zerr(
                        codes::TYPE_MISMATCH,
                        format!("`!` requires a `bool`, got `{}`", t.c_name()),
                        *span,
                        Some("logical NOT works on `bool` values"),
                    )),
                }
            }
            Expr::Binary { op, lhs, rhs, span } => {
                let (l, ls) = self.gen_expr(lhs, ctx)?;
                let (r, rs) = self.gen_expr(rhs, ctx)?;
                match op {
                    BinOp::Add => match (l, r) {
                        // str 拼接：GNU 语句表达式，栈缓冲 256B（gcc/clang 均支持）
                        (CType::Str, CType::Str) => {
                            let n = ctx.str_temp;
                            ctx.str_temp += 1;
                            Ok((
                                CType::Str,
                                format!(
                                    "({{ char ka_s{}[256]; snprintf(ka_s{}, sizeof ka_s{}, \"%s%s\", {}, {}); (const char*)ka_s{}; }})",
                                    n, n, n, ls, rs, n
                                ),
                            ))
                        }
                        (CType::Int, CType::Int) | (CType::Double, CType::Double) => {
                            Ok((l, format!("({} + {})", ls, rs)))
                        }
                        _ => Err(self.zerr(
                            codes::TYPE_MISMATCH,
                            format!("cannot apply `+` to `{}` and `{}`", l.c_name(), r.c_name()),
                            *span,
                            Some("`+` works on numbers and concatenates strings"),
                        )),
                    },
                    BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
                        let sym = op.symbol();
                        Ok((l, format!("({} {} {})", ls, sym, rs)))
                    }
                    BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                        let c = if l == CType::Str {
                            let cop = match op {
                                BinOp::Eq => "==",
                                BinOp::Ne => "!=",
                                BinOp::Lt => "<",
                                BinOp::Le => "<=",
                                BinOp::Gt => ">",
                                BinOp::Ge => ">=",
                                _ => unreachable!(),
                            };
                            format!("(strcmp({}, {}) {} 0)", ls, rs, cop)
                        } else {
                            format!("({} {} {})", ls, op.symbol(), rs)
                        };
                        Ok((CType::Bool, c))
                    }
                    BinOp::And | BinOp::Or => {
                        let sym = if *op == BinOp::And { "&&" } else { "||" };
                        Ok((CType::Bool, format!("({} {} {})", ls, sym, rs)))
                    }
                }
            }
            Expr::Call { callee, args, span } => {
                let f = match self.fns.get(callee) {
                    Some(f) => f,
                    None => {
                        return Err(self.zerr(
                            codes::NOT_IMPLEMENTED,
                            format!("cannot translate call to `{}` (builtins are not supported in --dll yet)", callee),
                            *span,
                            Some("use only user-defined functions in exported code"),
                        ));
                    }
                };
                let (_, ret, _) = self.infer_signature(f, &mut Vec::new())?;
                let mut out_args = Vec::new();
                for a in args {
                    let (_, s) = self.gen_expr(a, ctx)?;
                    out_args.push(s);
                }
                Ok((ret, format!("{}({})", callee, out_args.join(", "))))
            }
        }
    }
}

/// Hone 字符串 → C 字符串字面量（转义）。
fn c_str_lit(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\t' => o.push_str("\\t"),
            '\r' => o.push_str("\\r"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\x{:02x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// 判断语句序列中是否存在 return 语句（递归扫描分支与嵌套块）。
fn has_return(stmts: &[Stmt]) -> bool {
    for s in stmts {
        match s {
            Stmt::Return { .. } => return true,
            Stmt::Block { stmts, .. } => {
                if has_return(stmts) {
                    return true;
                }
            }
            Stmt::If { then_branch, else_branch, .. } => {
                if has_return(then_branch) || else_branch.as_deref().map_or(false, has_return) {
                    return true;
                }
            }
            Stmt::While { body, .. } => {
                if has_return(body) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

fn stmt_span(s: &Stmt) -> Span {
    match s {
        Stmt::Assign { span, .. }
        | Stmt::VarDecl { span, .. }
        | Stmt::Block { span, .. }
        | Stmt::If { span, .. }
        | Stmt::While { span, .. }
        | Stmt::ForIn { span, .. }
        | Stmt::Return { span, .. }
        | Stmt::FnDef { span, .. }
        | Stmt::ExprStmt { span, .. }
        | Stmt::Breakpoint { span, .. }
        | Stmt::Export { span, .. }
        | Stmt::Import { span, .. }
        | Stmt::Load { span, .. }
        | Stmt::Use { span, .. }
        | Stmt::Alias { span, .. }
        | Stmt::Go { span, .. }
        | Stmt::Try { span, .. }
        | Stmt::Throw { span, .. }
        | Stmt::StructDef { span, .. }
        | Stmt::DebugPrint { span, .. } => *span,
    }
}
