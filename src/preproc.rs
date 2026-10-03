// preproc.rs - 解析后、检查前的预处理：宏展开 + 跳转安全校验
//
// 本模块是 `goto`/`label` 与 `macro` 两个特性的**唯一权威实现点**：
// 所有后端（解释器 / 字节码 VM / AOT）看到的都是「已展开宏、已校验跳转」的 AST，
// 因此三者行为天然一致，无需各自实现一遍语义。
//
// ── 宏的安全约定（刻意保守，宁可报错也不产生意外行为）────────────────
//  1. `macro` 只能定义在**程序顶层**（函数/块/循环/类内部定义一律报错）。
//  2. **先定义后使用**：展开按源码顺序进行，未定义的调用就是普通函数调用。
//  3. **宏体在定义点即展开**：宏体只能引用「比它更早定义」的宏 ⇒ 引用关系构成
//     有向无环图，**不可能出现无限展开**（无需深度上限，也不存在自引用）。
//  4. **纯 AST 替换，不做文本替换**：没有运算符优先级陷阱、没有 token 拼接问题。
//  5. 宏名与顶层函数/结构体/类/枚举**不得重名**，宏之间也不得重名。
//  6. 实参个数必须与形参一致（编译期报错）。
//  7. 表达式宏：体内禁止 `i++/--i`（副作用）与 lambda（会改变实参求值作用域）；
//     形参不得作为被调用函数名（宏是语法替换，不支持高阶）。
//  8. 语句宏：展开为**独立作用域的代码块**，体内声明的变量不会泄漏到调用处；
//     体内禁止 `return`/`goto`/`label`/`macro`，`break`/`continue` 只能出现在
//     宏体自身的循环内（否则会跨越宏边界劫持调用处的循环）。
//
// ── goto 的安全约定 ─────────────────────────────────────────────────
//  1. 只能跳到**同一函数内**的标签（lambda / 嵌套 fn 各自独立，标签不跨函数）。
//  2. 标签名在同一函数内**必须唯一**（重复定义报错）。
//  3. 只能跳到**从跳转点向外层可见**的语句块中的标签：即标签所在语句块必须是
//     跳转点所在语句块的祖先（含自身）。因此**永远无法跳入内层语句块/循环体**。
//  4. **禁止向前跳过变量绑定**：若标签位于跳转点之后，且两者之间在同一语句块内
//     存在 `x = ...` / `int x = ...` / 解构赋值等绑定语句，则报错
//     （否则跳转后读到的是未初始化变量；两个后端也会表现不一致）。

use crate::ast::*;
use crate::error::{codes, ZError};
use crate::lexer::Span;
use std::collections::HashMap;

/// 报错所需的文件上下文。
pub struct Ctx<'a> {
    pub file: &'a str,
    pub src: &'a str,
    /// 全程序宏定义名 → 定义处 span（`preprocess` 进入时一次性填充）。
    /// 仅用于诊断「宏在定义之前被使用」，不参与展开语义。
    macro_defs: std::cell::RefCell<HashMap<String, Span>>,
}

impl<'a> Ctx<'a> {
    pub fn new(file: &'a str, src: &'a str) -> Self {
        Ctx {
            file,
            src,
            macro_defs: std::cell::RefCell::new(HashMap::new()),
        }
    }
}

impl<'a> Ctx<'a> {
    fn err(
        &self,
        code: &'static str,
        msg: impl Into<String>,
        span: &Span,
        help: Option<impl Into<String>>,
    ) -> ZError {
        ZError::new(
            code,
            msg,
            self.file,
            self.src,
            span.line,
            span.col,
            span.len.max(1),
            help,
        )
    }
}

/// 宏定义（形参 + 已展开的体）。
struct MacroInfo {
    params: Vec<String>,
    body: MacroBody,
}

/// 入口：就地完成宏展开与跳转校验。
pub fn preprocess(ctx: &Ctx, prog: &mut Program) -> Result<(), ZError> {
    expand_macros(ctx, prog)?;
    validate_goto(ctx, &prog.stmts)?;
    Ok(())
}

// ======================= 宏展开 =======================

fn expand_macros(ctx: &Ctx, prog: &mut Program) -> Result<(), ZError> {
    let reserved = top_level_names(&prog.stmts);
    // 预扫描：记录所有宏定义名（含尚未轮到展开的），仅用于「先用后定义」诊断。
    {
        let mut defs = ctx.macro_defs.borrow_mut();
        defs.clear();
        for s in &prog.stmts {
            if let Stmt::MacroDef { name, span, .. } = s {
                defs.entry(name.clone()).or_insert(*span);
            }
        }
    }
    let mut env: HashMap<String, MacroInfo> = HashMap::new();
    let stmts = std::mem::take(&mut prog.stmts);
    let mut out: Vec<Stmt> = Vec::with_capacity(stmts.len());

    for s in stmts {
        match s {
            Stmt::MacroDef {
                name,
                params,
                body,
                span,
            } => {
                if let Some(what) = reserved.get(&name) {
                    return Err(ctx.err(
                        codes::MACRO,
                        format!(
                            "macro `{}` conflicts with the existing top-level {} of the same name",
                            name, what
                        ),
                        &span,
                        Some("给宏换一个名字（宏名不能与函数/结构体/类/枚举重名）"),
                    ));
                }
                if env.contains_key(&name) {
                    return Err(ctx.err(
                        codes::MACRO,
                        format!("macro `{}` is already defined", name),
                        &span,
                        Some("宏名必须唯一，请删除重复定义或改用其他名字"),
                    ));
                }
                let pinfo: Vec<String> = params.iter().map(|p| p.name.clone()).collect();
                // 体在定义点展开：只能引用更早定义的宏 ⇒ 无环、不会无限展开
                let body = match body {
                    MacroBody::Expr(mut e) => {
                        // 表达式宏：形参只作表达式替换
                        expand_expr(ctx, &env, &mut e)?;
                        MacroBody::Expr(e)
                    }
                    MacroBody::Stmts(ss) => {
                        validate_stmt_macro_body(ctx, &ss)?;
                        MacroBody::Stmts(expand_stmts(ctx, &env, ss)?)
                    }
                };
                env.insert(name, MacroInfo { params: pinfo, body });
            }
            other => expand_one(ctx, &env, other, &mut out)?,
        }
    }
    prog.stmts = out;
    Ok(())
}

/// 顶层定义名 → 种类描述（用于宏重名检测）。
fn top_level_names(stmts: &[Stmt]) -> HashMap<String, &'static str> {
    let mut m = HashMap::new();
    for s in stmts {
        match s {
            Stmt::FnDef { name, .. } => {
                m.insert(name.clone(), "function");
            }
            Stmt::AsyncFnDef { name, .. } => {
                m.insert(name.clone(), "async function");
            }
            Stmt::StructDef { name, .. } => {
                m.insert(name.clone(), "struct");
            }
            Stmt::ClassDef { name, .. } => {
                m.insert(name.clone(), "class");
            }
            Stmt::EnumDef { name, .. } => {
                m.insert(name.clone(), "enum");
            }
            // 宏自身不进入保留表：宏与宏的重名由展开时的 env 单独检测
            _ => {}
        }
    }
    m
}

/// 语句宏体的静态安全校验（在展开前，对定义体本身做检查）。
fn validate_stmt_macro_body(ctx: &Ctx, stmts: &[Stmt]) -> Result<(), ZError> {
    validate_body_stmts(ctx, stmts, 0)
}

fn validate_body_stmts(ctx: &Ctx, stmts: &[Stmt], loop_depth: usize) -> Result<(), ZError> {
    for s in stmts {
        match s {
            Stmt::Return { span, .. } => {
                return Err(ctx.err(
                    codes::MACRO,
                    "`return` is not allowed inside a statement macro body",
                    span,
                    Some("宏展开在调用处，`return` 会跳出调用者的函数；请改用表达式宏或普通函数"),
                ))
            }
            Stmt::Goto { span, .. } | Stmt::Label { span, .. } => {
                return Err(ctx.err(
                    codes::MACRO,
                    "`goto` / label is not allowed inside a statement macro body",
                    span,
                    Some("宏内跳转会跨越宏边界、影响调用处的控制流；请把跳转写在调用处"),
                ))
            }
            Stmt::MacroDef { span, .. } => {
                return Err(ctx.err(
                    codes::MACRO,
                    "`macro` can only be defined at the top level of a program",
                    span,
                    Some("把宏定义移到文件顶层（不能在函数/代码块/循环内定义宏）"),
                ))
            }
            Stmt::Break { span } | Stmt::Continue { span } => {
                if loop_depth == 0 {
                    return Err(ctx.err(
                        codes::MACRO,
                        format!(
                            "`{}` inside a statement macro body needs a loop in the same body",
                            if matches!(s, Stmt::Break { .. }) { "break" } else { "continue" }
                        ),
                        span,
                        Some("宏体自身的循环里才能 break/continue；若想跳出调用处的循环，请把该语句写在调用处"),
                    ));
                }
            }
            Stmt::Block { stmts, .. } => validate_body_stmts(ctx, stmts, loop_depth)?,
            Stmt::If {
                then_branch,
                else_branch,
                ..
            } => {
                validate_body_stmts(ctx, then_branch, loop_depth)?;
                if let Some(eb) = else_branch {
                    validate_body_stmts(ctx, eb, loop_depth)?;
                }
            }
            Stmt::While { body, .. } | Stmt::ForIn { body, .. } | Stmt::DoWhile { body, .. } => {
                validate_body_stmts(ctx, body, loop_depth + 1)?
            }
            Stmt::ForC { body, .. } => validate_body_stmts(ctx, body, loop_depth + 1)?,
            Stmt::Try { body, handler, .. } => {
                validate_body_stmts(ctx, body, loop_depth)?;
                validate_body_stmts(ctx, handler, loop_depth)?;
            }
            Stmt::ClassDef { methods, .. } => validate_body_stmts(ctx, methods, loop_depth)?,
            Stmt::FnDef { body, .. } | Stmt::AsyncFnDef { body, .. } => {
                validate_body_stmts(ctx, body, loop_depth)?
            }
            _ => {}
        }
    }
    Ok(())
}

/// 展开一条语句（递归下降；语句宏调用会就地替换）。
fn expand_stmt(
    ctx: &Ctx,
    env: &HashMap<String, MacroInfo>,
    s: Stmt,
) -> Result<Stmt, ZError> {
    Ok(match s {
        Stmt::Assign { name, value, span } => {
            let mut value = value;
            expand_expr(ctx, env, &mut value)?;
            Stmt::Assign { name, value, span }
        }
        Stmt::IndexAssign { target, value, span } => {
            let (mut target, mut value) = (target, value);
            expand_expr(ctx, env, &mut target)?;
            expand_expr(ctx, env, &mut value)?;
            Stmt::IndexAssign { target, value, span }
        }
        Stmt::DestructAssign { targets, value, span } => {
            let mut value = value;
            expand_expr(ctx, env, &mut value)?;
            Stmt::DestructAssign { targets, value, span }
        }
        Stmt::AssignOp {
            name,
            op,
            value,
            span,
        } => {
            let mut value = value;
            expand_expr(ctx, env, &mut value)?;
            Stmt::AssignOp {
                name,
                op,
                value,
                span,
            }
        }
        Stmt::VarDecl {
            name,
            ty,
            init,
            span,
            readonly,
            cow,
        } => {
            let init = match init {
                Some(mut e) => {
                    expand_expr(ctx, env, &mut e)?;
                    Some(e)
                }
                None => None,
            };
            Stmt::VarDecl {
                name,
                ty,
                init,
                span,
                readonly,
                cow,
            }
        }
        Stmt::Block { stmts, span } => Stmt::Block {
            stmts: expand_stmts(ctx, env, stmts)?,
            span,
        },
        Stmt::If {
            cond,
            then_branch,
            else_branch,
            span,
        } => {
            let mut cond = cond;
            expand_expr(ctx, env, &mut cond)?;
            Stmt::If {
                cond,
                then_branch: expand_stmts(ctx, env, then_branch)?,
                else_branch: match else_branch {
                    Some(b) => Some(expand_stmts(ctx, env, b)?),
                    None => None,
                },
                span,
            }
        }
        Stmt::While { cond, body, span } => {
            let mut cond = cond;
            expand_expr(ctx, env, &mut cond)?;
            Stmt::While {
                cond,
                body: expand_stmts(ctx, env, body)?,
                span,
            }
        }
        Stmt::DoWhile { body, cond, span } => {
            let mut cond = cond;
            expand_expr(ctx, env, &mut cond)?;
            Stmt::DoWhile {
                body: expand_stmts(ctx, env, body)?,
                cond,
                span,
            }
        }
        Stmt::ForC {
            init,
            cond,
            step,
            body,
            span,
        } => {
            let init = match init {
                Some(s) => {
                    let e = expand_stmt(ctx, env, *s)?;
                    Some(Box::new(e))
                }
                None => None,
            };
            let cond = match cond {
                Some(mut c) => {
                    expand_expr(ctx, env, &mut c)?;
                    Some(c)
                }
                None => None,
            };
            let step = match step {
                Some(s) => {
                    let e = expand_stmt(ctx, env, *s)?;
                    Some(Box::new(e))
                }
                None => None,
            };
            Stmt::ForC {
                init,
                cond,
                step,
                body: expand_stmts(ctx, env, body)?,
                span,
            }
        }
        Stmt::ForIn {
            var,
            var2,
            iter,
            body,
            span,
        } => {
            let mut iter = iter;
            expand_expr(ctx, env, &mut iter)?;
            Stmt::ForIn {
                var,
                var2,
                iter,
                body: expand_stmts(ctx, env, body)?,
                span,
            }
        }
        Stmt::Return { values, span } => {
            let mut vs = values;
            for v in vs.iter_mut() {
                expand_expr(ctx, env, v)?;
            }
            Stmt::Return { values: vs, span }
        }
        Stmt::Break { span } => Stmt::Break { span },
        Stmt::Continue { span } => Stmt::Continue { span },
        Stmt::FnDef {
            name,
            type_params,
            params,
            ret,
            body,
            span,
            tmp,
        } => Stmt::FnDef {
            name,
            type_params,
            params,
            ret,
            body: expand_stmts(ctx, env, body)?,
            span,
            tmp,
        },
        Stmt::AsyncFnDef {
            name,
            type_params,
            params,
            ret,
            body,
            span,
        } => Stmt::AsyncFnDef {
            name,
            type_params,
            params,
            ret,
            body: expand_stmts(ctx, env, body)?,
            span,
        },
        Stmt::DebugPrint { expr, span } => {
            let mut e = *expr;
            expand_expr(ctx, env, &mut e)?;
            Stmt::DebugPrint {
                expr: Box::new(e),
                span,
            }
        }
        Stmt::ExprStmt { expr, span } => {
            let mut e = expr;
            expand_expr(ctx, env, &mut e)?;
            Stmt::ExprStmt { expr: e, span }
        }
        Stmt::Breakpoint { span, cond } => {
            let cond = match cond {
                Some(c) => {
                    let mut c = *c;
                    expand_expr(ctx, env, &mut c)?;
                    Some(Box::new(c))
                }
                None => None,
            };
            Stmt::Breakpoint { span, cond }
        }
        Stmt::Go { callee, args, span } => {
            let mut args = args;
            for a in args.iter_mut() {
                expand_expr(ctx, env, a)?;
            }
            Stmt::Go { callee, args, span }
        }
        Stmt::Try {
            body,
            catch_var,
            handler,
            span,
        } => Stmt::Try {
            body: expand_stmts(ctx, env, body)?,
            catch_var,
            handler: expand_stmts(ctx, env, handler)?,
            span,
        },
        Stmt::Throw { value, span } => {
            let mut v = value;
            expand_expr(ctx, env, &mut v)?;
            Stmt::Throw { value: v, span }
        }
        Stmt::ClassDef { name, methods, span } => Stmt::ClassDef {
            name,
            methods: expand_stmts(ctx, env, methods)?,
            span,
        },
        Stmt::MacroDef { span, .. } => {
            return Err(ctx.err(
                codes::MACRO,
                "`macro` can only be defined at the top level of a program",
                &span,
                Some("把宏定义移到文件顶层（这里位于函数/代码块/类/循环内部）"),
            ))
        }
        other => other,
    })
}

fn expand_stmts(
    ctx: &Ctx,
    env: &HashMap<String, MacroInfo>,
    stmts: Vec<Stmt>,
) -> Result<Vec<Stmt>, ZError> {
    let mut out = Vec::with_capacity(stmts.len());
    for s in stmts {
        expand_one(ctx, env, s, &mut out)?;
    }
    Ok(out)
}

/// 展开单条语句并压入 `out`；语句宏调用会展开为**一条独立作用域块语句**
/// （因此可以出现在任意语句列表位置，包括程序顶层）。
fn expand_one(
    ctx: &Ctx,
    env: &HashMap<String, MacroInfo>,
    s: Stmt,
    out: &mut Vec<Stmt>,
) -> Result<(), ZError> {
    // 语句宏调用：整条表达式语句替换为宏体（包一层独立作用域块，变量不外泄）
    if let Stmt::ExprStmt {
        expr: Expr::Call { callee, args, span },
        span: stmt_span,
    } = s
    {
        if let Some(m) = env.get(&callee) {
            if let MacroBody::Stmts(body) = &m.body {
                let expanded = {
                    let map = bind_args(ctx, m, &callee, &args, &span)?;
                    subst_stmts(ctx, body, &map)?
                };
                out.push(Stmt::Block {
                    stmts: expanded,
                    span: stmt_span,
                });
                return Ok(());
            }
        }
        let mut e = Expr::Call { callee, args, span };
        expand_expr(ctx, env, &mut e)?;
        out.push(Stmt::ExprStmt {
            expr: e,
            span: stmt_span,
        });
        return Ok(());
    }
    out.push(expand_stmt(ctx, env, s)?);
    Ok(())
}

/// 展开表达式中的宏调用（就地；表达式宏替换为体，语句宏在表达式位置报错）。
fn expand_expr(
    ctx: &Ctx,
    env: &HashMap<String, MacroInfo>,
    e: &mut Expr,
) -> Result<(), ZError> {
    match e {
        Expr::IntLit(..)
        | Expr::FloatLit(..)
        | Expr::BoolLit(..)
        | Expr::StrLit(..)
        | Expr::CharLit(..)
        | Expr::ByteLit(..)
        | Expr::BytesLit(..)
        | Expr::Ident { .. } => {}
        Expr::ListLit(items, _) => {
            for it in items.iter_mut() {
                expand_expr(ctx, env, it)?;
            }
        }
        Expr::DictLit(entries, _) => {
            for (_, v) in entries.iter_mut() {
                expand_expr(ctx, env, v)?;
            }
        }
        Expr::ListComp {
            elem,
            iter,
            cond,
            ..
        } => {
            expand_expr(ctx, env, elem)?;
            expand_expr(ctx, env, iter)?;
            if let Some(c) = cond {
                expand_expr(ctx, env, c)?;
            }
        }
        Expr::DictComp {
            key,
            value,
            iter,
            cond,
            ..
        } => {
            expand_expr(ctx, env, key)?;
            expand_expr(ctx, env, value)?;
            expand_expr(ctx, env, iter)?;
            if let Some(c) = cond {
                expand_expr(ctx, env, c)?;
            }
        }
        Expr::FStr(segs, _) => {
            for seg in segs.iter_mut() {
                if let FStrSeg::Code(x) = seg {
                    expand_expr(ctx, env, x)?;
                }
            }
        }
        Expr::Field { obj, .. } | Expr::OptionalField { obj, .. } => {
            expand_expr(ctx, env, obj)?;
        }
        Expr::Index { obj, index, .. } => {
            expand_expr(ctx, env, obj)?;
            expand_expr(ctx, env, index)?;
        }
        Expr::Slice { obj, lo, hi, .. } => {
            expand_expr(ctx, env, obj)?;
            if let Some(x) = lo {
                expand_expr(ctx, env, x)?;
            }
            if let Some(x) = hi {
                expand_expr(ctx, env, x)?;
            }
        }
        Expr::MethodCall { obj, args, .. } => {
            expand_expr(ctx, env, obj)?;
            for a in args.iter_mut() {
                expand_expr(ctx, env, a)?;
            }
        }
        Expr::New { args, .. } => {
            for a in args.iter_mut() {
                expand_expr(ctx, env, a)?;
            }
        }
        Expr::Unary { expr, .. } => expand_expr(ctx, env, expr)?,
        Expr::Binary { lhs, rhs, .. } => {
            expand_expr(ctx, env, lhs)?;
            expand_expr(ctx, env, rhs)?;
        }
        Expr::Call { callee, args, span } => {
            // 先展开实参（实参里的宏按调用处环境展开）
            for a in args.iter_mut() {
                expand_expr(ctx, env, a)?;
            }
            if let Some(m) = env.get(callee) {
                match &m.body {
                    MacroBody::Expr(body) => {
                        let replaced = {
                            let map = bind_args(ctx, m, callee, args, span)?;
                            subst_expr(ctx, body, &map)?
                        };
                        let mut replaced = replaced;
                        // 替换结果仍可能含宏调用（由实参引入），继续展开
                        expand_expr(ctx, env, &mut replaced)?;
                        *e = replaced;
                        return Ok(());
                    }
                    MacroBody::Stmts(_) => {
                        return Err(ctx.err(
                            codes::MACRO,
                            format!("macro `{}` expands to statements and cannot be used as an expression", callee),
                            span,
                            Some("语句宏只能作为独立语句调用，如 `NAME(...);`"),
                        ))
                    }
                }
            }
            // 未匹配到已定义宏：若该名字对应程序中「更靠后」的宏定义，
            // 说明违反「宏必须先定义后使用」，给出比 H002 更明确的诊断。
            if let Some(def_span) = ctx.macro_defs.borrow().get(callee).copied() {
                return Err(ctx.err(
                    codes::MACRO,
                    format!(
                        "macro `{}` is used before its definition (defined at line {})",
                        callee, def_span.line
                    ),
                    span,
                    Some("把 `macro` 定义移到首次使用之前（宏按源码顺序展开，不支持前向引用）"),
                ));
            }
        }
        Expr::Match { value, arms, .. } => {
            expand_expr(ctx, env, value)?;
            for (_, body) in arms.iter_mut() {
                expand_expr(ctx, env, body)?;
            }
        }
        Expr::IncDec { .. } => {}
        Expr::Ternary {
            cond,
            then_expr,
            else_expr,
            ..
        } => {
            expand_expr(ctx, env, cond)?;
            expand_expr(ctx, env, then_expr)?;
            expand_expr(ctx, env, else_expr)?;
        }
        Expr::Lambda { body, .. } => {
            let b = std::mem::take(body);
            *body = expand_stmts(ctx, env, b)?;
        }
        Expr::Await { expr, .. } => expand_expr(ctx, env, expr)?,
    }
    Ok(())
}

/// 形参名 → 实参表达式 的绑定表。
type ArgMap<'a> = HashMap<&'a str, &'a Expr>;

fn bind_args<'a>(
    ctx: &Ctx,
    m: &'a MacroInfo,
    name: &str,
    args: &'a [Expr],
    span: &Span,
) -> Result<ArgMap<'a>, ZError> {
    if args.len() != m.params.len() {
        return Err(ctx.err(
            codes::MACRO,
            format!(
                "macro `{}` expects {} argument(s), got {}",
                name,
                m.params.len(),
                args.len()
            ),
            span,
            Some(format!("宏 `{}` 的形参：{}", name, if m.params.is_empty() { "（无）".to_string() } else { m.params.join(", ") })),
        ));
    }
    let mut map: ArgMap<'a> = HashMap::new();
    for (p, a) in m.params.iter().zip(args.iter()) {
        map.insert(p.as_str(), a);
    }
    Ok(map)
}

/// 表达式替换：把体内出现的形参标识符换成实参表达式。
fn subst_expr(ctx: &Ctx, e: &Expr, map: &ArgMap) -> Result<Expr, ZError> {
    Ok(match e {
        Expr::Ident { name, span } => match map.get(name.as_str()) {
            Some(arg) => (*arg).clone(),
            None => Expr::Ident {
                name: name.clone(),
                span: *span,
            },
        },
        Expr::IntLit(v, s) => Expr::IntLit(*v, *s),
        Expr::FloatLit(v, s) => Expr::FloatLit(*v, *s),
        Expr::BoolLit(v, s) => Expr::BoolLit(*v, *s),
        Expr::StrLit(v, s) => Expr::StrLit(v.clone(), *s),
        Expr::CharLit(v, s) => Expr::CharLit(*v, *s),
        Expr::ByteLit(v, s) => Expr::ByteLit(*v, *s),
        Expr::BytesLit(v, s) => Expr::BytesLit(v.clone(), *s),
        Expr::ListLit(items, s) => {
            let mut v = Vec::with_capacity(items.len());
            for it in items {
                v.push(subst_expr(ctx, it, map)?);
            }
            Expr::ListLit(v, *s)
        }
        Expr::DictLit(entries, s) => {
            let mut v = Vec::with_capacity(entries.len());
            for (k, val) in entries {
                v.push((k.clone(), subst_expr(ctx, val, map)?));
            }
            Expr::DictLit(v, *s)
        }
        Expr::ListComp {
            elem,
            var,
            var2,
            iter,
            cond,
            span,
        } => Expr::ListComp {
            elem: Box::new(subst_expr(ctx, elem, map)?),
            var: var.clone(),
            var2: var2.clone(),
            iter: Box::new(subst_expr(ctx, iter, map)?),
            cond: match cond {
                Some(c) => Some(Box::new(subst_expr(ctx, c, map)?)),
                None => None,
            },
            span: *span,
        },
        Expr::DictComp {
            key,
            value,
            var,
            var2,
            iter,
            cond,
            span,
        } => Expr::DictComp {
            key: Box::new(subst_expr(ctx, key, map)?),
            value: Box::new(subst_expr(ctx, value, map)?),
            var: var.clone(),
            var2: var2.clone(),
            iter: Box::new(subst_expr(ctx, iter, map)?),
            cond: match cond {
                Some(c) => Some(Box::new(subst_expr(ctx, c, map)?)),
                None => None,
            },
            span: *span,
        },
        Expr::FStr(segs, s) => {
            let mut v = Vec::with_capacity(segs.len());
            for seg in segs {
                v.push(match seg {
                    FStrSeg::Lit(t) => FStrSeg::Lit(t.clone()),
                    FStrSeg::Code(x) => FStrSeg::Code(subst_expr(ctx, x, map)?),
                });
            }
            Expr::FStr(v, *s)
        }
        Expr::Field { obj, field, span } => Expr::Field {
            obj: Box::new(subst_expr(ctx, obj, map)?),
            field: field.clone(),
            span: *span,
        },
        Expr::OptionalField { obj, field, span } => Expr::OptionalField {
            obj: Box::new(subst_expr(ctx, obj, map)?),
            field: field.clone(),
            span: *span,
        },
        Expr::Index { obj, index, span } => Expr::Index {
            obj: Box::new(subst_expr(ctx, obj, map)?),
            index: Box::new(subst_expr(ctx, index, map)?),
            span: *span,
        },
        Expr::Slice { obj, lo, hi, span } => Expr::Slice {
            obj: Box::new(subst_expr(ctx, obj, map)?),
            lo: match lo {
                Some(x) => Some(Box::new(subst_expr(ctx, x, map)?)),
                None => None,
            },
            hi: match hi {
                Some(x) => Some(Box::new(subst_expr(ctx, x, map)?)),
                None => None,
            },
            span: *span,
        },
        Expr::MethodCall { obj, name, args, span } => {
            let mut v = Vec::with_capacity(args.len());
            for a in args {
                v.push(subst_expr(ctx, a, map)?);
            }
            Expr::MethodCall {
                obj: Box::new(subst_expr(ctx, obj, map)?),
                name: name.clone(),
                args: v,
                span: *span,
            }
        }
        Expr::New { ty, args, span } => {
            let mut v = Vec::with_capacity(args.len());
            for a in args {
                v.push(subst_expr(ctx, a, map)?);
            }
            Expr::New {
                ty: ty.clone(),
                args: v,
                span: *span,
            }
        }
        Expr::Unary { op, expr, span } => Expr::Unary {
            op: *op,
            expr: Box::new(subst_expr(ctx, expr, map)?),
            span: *span,
        },
        Expr::Binary { op, lhs, rhs, span } => Expr::Binary {
            op: *op,
            lhs: Box::new(subst_expr(ctx, lhs, map)?),
            rhs: Box::new(subst_expr(ctx, rhs, map)?),
            span: *span,
        },
        Expr::Call { callee, args, span } => {
            if map.contains_key(callee.as_str()) {
                return Err(ctx.err(
                    codes::MACRO,
                    format!(
                        "macro parameter `{}` cannot be used as a callee (宏是语法替换，不支持把函数名当参数)",
                        callee
                    ),
                    span,
                    Some("把要调用的函数名直接写进宏体，或改用具名函数"),
                ));
            }
            let mut v = Vec::with_capacity(args.len());
            for a in args {
                v.push(subst_expr(ctx, a, map)?);
            }
            Expr::Call {
                callee: callee.clone(),
                args: v,
                span: *span,
            }
        }
        Expr::Match { value, arms, span } => {
            let mut v = Vec::with_capacity(arms.len());
            for (p, body) in arms {
                v.push((p.clone(), subst_expr(ctx, body, map)?));
            }
            Expr::Match {
                value: Box::new(subst_expr(ctx, value, map)?),
                arms: v,
                span: *span,
            }
        }
        Expr::IncDec { op, prefix, name, span } => {
            if map.contains_key(name.as_str()) {
                return Err(ctx.err(
                    codes::MACRO,
                    format!("macro parameter `{}` cannot be used with `{}` inside a macro body", name, op.symbol()),
                    span,
                    Some("宏体内不要对形参做自增/自减；把该操作写在调用处"),
                ));
            }
            Expr::IncDec {
                op: *op,
                prefix: *prefix,
                name: name.clone(),
                span: *span,
            }
        }
        Expr::Ternary {
            cond,
            then_expr,
            else_expr,
            span,
        } => Expr::Ternary {
            cond: Box::new(subst_expr(ctx, cond, map)?),
            then_expr: Box::new(subst_expr(ctx, then_expr, map)?),
            else_expr: Box::new(subst_expr(ctx, else_expr, map)?),
            span: *span,
        },
        Expr::Lambda { span, .. } => {
            // lambda 会改变实参的求值作用域与时机，宏内禁用
            return Err(ctx.err(
                codes::MACRO,
                "lambda is not allowed inside a macro body",
                span,
                Some("把 lambda 写在调用处，或改用具名函数"),
            ));
        }
        Expr::Await { expr, span } => Expr::Await {
            expr: Box::new(subst_expr(ctx, expr, map)?),
            span: *span,
        },
    })
}

/// 语句替换（语句宏体）：除表达式位置外，还支持把形参作为**赋值目标名**，
/// 此时对应实参必须是裸变量名（保证替换后仍是合法的赋值目标）。
fn subst_stmts(
    ctx: &Ctx,
    stmts: &[Stmt],
    map: &ArgMap,
) -> Result<Vec<Stmt>, ZError> {
    let mut out = Vec::with_capacity(stmts.len());
    for s in stmts {
        out.push(subst_stmt(ctx, s, map)?);
    }
    Ok(out)
}

/// 形参作为赋值/声明目标名 → 实参必须是裸变量名。
fn target_name(ctx: &Ctx, name: &str, map: &ArgMap, span: &Span) -> Result<String, ZError> {
    match map.get(name) {
        None => Ok(name.to_string()),
        Some(Expr::Ident { name: n, .. }) => Ok(n.clone()),
        Some(_) => Err(ctx.err(
            codes::MACRO,
            format!(
                "macro parameter `{}` is used as an assignment target, so the matching argument must be a plain variable name",
                name
            ),
            span,
            Some("例如 `INC(i)` 合法，`INC(a[0])` / `INC(x + 1)` 不合法"),
        )),
    }
}

fn subst_stmt(ctx: &Ctx, s: &Stmt, map: &ArgMap) -> Result<Stmt, ZError> {
    Ok(match s {
        Stmt::Assign { name, value, span } => Stmt::Assign {
            name: target_name(ctx, name, map, span)?,
            value: subst_expr(ctx, value, map)?,
            span: *span,
        },
        Stmt::AssignOp {
            name,
            op,
            value,
            span,
        } => Stmt::AssignOp {
            name: target_name(ctx, name, map, span)?,
            op: *op,
            value: subst_expr(ctx, value, map)?,
            span: *span,
        },
        Stmt::VarDecl {
            name,
            ty,
            init,
            span,
            readonly,
            cow,
        } => {
            if let TyName::Var(t) = ty {
                if map.contains_key(t.as_str()) {
                    return Err(ctx.err(
                        codes::MACRO,
                        format!("macro parameter `{}` cannot be used as a type name", t),
                        span,
                        Some("宏参数只能用于表达式与赋值目标名"),
                    ));
                }
            }
            Stmt::VarDecl {
                name: target_name(ctx, name, map, span)?,
                ty: ty.clone(),
                init: match init {
                    Some(e) => Some(subst_expr(ctx, e, map)?),
                    None => None,
                },
                span: *span,
                readonly: *readonly,
                cow: *cow,
            }
        }
        Stmt::IndexAssign { target, value, span } => Stmt::IndexAssign {
            target: subst_expr(ctx, target, map)?,
            value: subst_expr(ctx, value, map)?,
            span: *span,
        },
        Stmt::DestructAssign { targets, value, span } => {
            for (n, _) in targets {
                if map.contains_key(n.as_str()) {
                    return Err(ctx.err(
                        codes::MACRO,
                        format!("macro parameter `{}` cannot be a destructuring target", n),
                        span,
                        Some("宏参数只能用于表达式与简单赋值目标名"),
                    ));
                }
            }
            Stmt::DestructAssign {
                targets: targets.clone(),
                value: subst_expr(ctx, value, map)?,
                span: *span,
            }
        }
        Stmt::Block { stmts, span } => Stmt::Block {
            stmts: subst_stmts(ctx, stmts, map)?,
            span: *span,
        },
        Stmt::If {
            cond,
            then_branch,
            else_branch,
            span,
        } => Stmt::If {
            cond: subst_expr(ctx, cond, map)?,
            then_branch: subst_stmts(ctx, then_branch, map)?,
            else_branch: match else_branch {
                Some(b) => Some(subst_stmts(ctx, b, map)?),
                None => None,
            },
            span: *span,
        },
        Stmt::While { cond, body, span } => Stmt::While {
            cond: subst_expr(ctx, cond, map)?,
            body: subst_stmts(ctx, body, map)?,
            span: *span,
        },
        Stmt::DoWhile { body, cond, span } => Stmt::DoWhile {
            body: subst_stmts(ctx, body, map)?,
            cond: subst_expr(ctx, cond, map)?,
            span: *span,
        },
        Stmt::ForC {
            init,
            cond,
            step,
            body,
            span,
        } => Stmt::ForC {
            init: match init {
                Some(s0) => Some(Box::new(subst_stmt(ctx, s0, map)?)),
                None => None,
            },
            cond: match cond {
                Some(c) => Some(subst_expr(ctx, c, map)?),
                None => None,
            },
            step: match step {
                Some(s0) => Some(Box::new(subst_stmt(ctx, s0, map)?)),
                None => None,
            },
            body: subst_stmts(ctx, body, map)?,
            span: *span,
        },
        Stmt::ForIn {
            var,
            var2,
            iter,
            body,
            span,
        } => {
            if map.contains_key(var.as_str())
                || var2.as_ref().map(|v| map.contains_key(v.as_str())).unwrap_or(false)
            {
                return Err(ctx.err(
                    codes::MACRO,
                    format!("macro parameter `{}` cannot be used as a loop variable", var),
                    span,
                    Some("宏参数不能作为 for 循环变量名"),
                ));
            }
            Stmt::ForIn {
                var: var.clone(),
                var2: var2.clone(),
                iter: subst_expr(ctx, iter, map)?,
                body: subst_stmts(ctx, body, map)?,
                span: *span,
            }
        }
        Stmt::Return { values, span } => {
            let mut v = Vec::with_capacity(values.len());
            for e in values {
                v.push(subst_expr(ctx, e, map)?);
            }
            Stmt::Return { values: v, span: *span }
        }
        Stmt::Break { span } => Stmt::Break { span: *span },
        Stmt::Continue { span } => Stmt::Continue { span: *span },
        Stmt::DebugPrint { expr, span } => Stmt::DebugPrint {
            expr: Box::new(subst_expr(ctx, expr, map)?),
            span: *span,
        },
        Stmt::ExprStmt { expr, span } => Stmt::ExprStmt {
            expr: subst_expr(ctx, expr, map)?,
            span: *span,
        },
        Stmt::Breakpoint { span, cond } => Stmt::Breakpoint {
            span: *span,
            cond: match cond {
                Some(c) => Some(Box::new(subst_expr(ctx, c, map)?)),
                None => None,
            },
        },
        Stmt::Go { callee, args, span } => {
            if map.contains_key(callee.as_str()) {
                return Err(ctx.err(
                    codes::MACRO,
                    format!("macro parameter `{}` cannot be used as a goroutine callee", callee),
                    span,
                    Some("宏参数只能用于表达式与简单赋值目标名"),
                ));
            }
            let mut v = Vec::with_capacity(args.len());
            for a in args {
                v.push(subst_expr(ctx, a, map)?);
            }
            Stmt::Go {
                callee: callee.clone(),
                args: v,
                span: *span,
            }
        }
        Stmt::Try {
            body,
            catch_var,
            handler,
            span,
        } => {
            if map.contains_key(catch_var.as_str()) {
                return Err(ctx.err(
                    codes::MACRO,
                    format!("macro parameter `{}` cannot be used as a catch variable", catch_var),
                    span,
                    Some("宏参数不能作为 catch 绑定名"),
                ));
            }
            Stmt::Try {
                body: subst_stmts(ctx, body, map)?,
                catch_var: catch_var.clone(),
                handler: subst_stmts(ctx, handler, map)?,
                span: *span,
            }
        }
        Stmt::Throw { value, span } => Stmt::Throw {
            value: subst_expr(ctx, value, map)?,
            span: *span,
        },
        Stmt::FnDef {
            name,
            type_params,
            params,
            ret,
            body,
            span,
            tmp,
        } => Stmt::FnDef {
            name: name.clone(),
            type_params: type_params.clone(),
            params: params.clone(),
            ret: ret.clone(),
            body: subst_stmts(ctx, body, map)?,
            span: *span,
            tmp: *tmp,
        },
        Stmt::AsyncFnDef {
            name,
            type_params,
            params,
            ret,
            body,
            span,
        } => Stmt::AsyncFnDef {
            name: name.clone(),
            type_params: type_params.clone(),
            params: params.clone(),
            ret: ret.clone(),
            body: subst_stmts(ctx, body, map)?,
            span: *span,
        },
        Stmt::ClassDef { name, methods, span } => Stmt::ClassDef {
            name: name.clone(),
            methods: subst_stmts(ctx, methods, map)?,
            span: *span,
        },
        // 以下语句不含表达式，原样克隆
        other => other.clone(),
    })
}

// ======================= goto 安全校验 =======================

/// 一个语句列表在函数内的编号信息。
struct ListInfo {
    /// 父列表 id（None = 函数根列表）
    parent: Option<usize>,
    /// 本列表在父列表中的「入口语句下标」（即父列表中包含本列表的那条语句）
    owner_index: usize,
    /// 每条语句是否向本列表的**作用域**引入绑定
    binds: Vec<bool>,
}

struct GotoScan {
    lists: HashMap<usize, ListInfo>,
    /// 标签名 → (列表 id, 语句下标, span)
    labels: HashMap<String, (usize, usize, Span)>,
    /// 待校验的跳转
    gotos: Vec<(String, usize, usize, Span)>,
    next_id: usize,
}

impl GotoScan {
    fn new() -> Self {
        GotoScan {
            lists: HashMap::new(),
            labels: HashMap::new(),
            gotos: Vec::new(),
            next_id: 0,
        }
    }
}

/// 语句是否向所在列表的作用域引入绑定（保守判定）。
fn stmt_binds(s: &Stmt) -> bool {
    matches!(
        s,
        Stmt::Assign { .. }
            | Stmt::VarDecl { .. }
            | Stmt::DestructAssign { .. }
            | Stmt::AssignOp { .. }
            | Stmt::FnDef { .. }
            | Stmt::AsyncFnDef { .. }
    )
}

/// 递归收集一个语句列表及其子列表（**不进入嵌套函数/lambda**）。
fn scan_stmts(
    ctx: &Ctx,
    sc: &mut GotoScan,
    stmts: &[Stmt],
    parent: Option<usize>,
    owner_index: usize,
) -> Result<usize, ZError> {
    let id = sc.next_id;
    sc.next_id += 1;
    sc.lists.insert(
        id,
        ListInfo {
            parent,
            owner_index,
            binds: stmts.iter().map(stmt_binds).collect(),
        },
    );

    for (i, s) in stmts.iter().enumerate() {
        match s {
            Stmt::Label { name, span } => {
                if let Some((_, _, prev)) = sc.labels.get(name) {
                    return Err(ctx.err(
                        codes::LABEL,
                        format!("label `{}` is already defined in this function", name),
                        span,
                        Some(format!(
                            "标签名在同一函数内必须唯一；前一次定义在第 {} 行，请改名或删除重复标签",
                            prev.line
                        )),
                    ));
                }
                sc.labels.insert(name.clone(), (id, i, *span));
            }
            Stmt::Goto { name, span } => sc.gotos.push((name.clone(), id, i, *span)),
            // 子语句列表（作用于同一函数内的可见性）
            Stmt::Block { stmts, .. } => {
                scan_stmts(ctx, sc, stmts, Some(id), i)?;
            }
            Stmt::If {
                then_branch,
                else_branch,
                ..
            } => {
                scan_stmts(ctx, sc, then_branch, Some(id), i)?;
                if let Some(eb) = else_branch {
                    scan_stmts(ctx, sc, eb, Some(id), i)?;
                }
            }
            Stmt::While { body, .. } | Stmt::ForIn { body, .. } | Stmt::DoWhile { body, .. } => {
                scan_stmts(ctx, sc, body, Some(id), i)?;
            }
            Stmt::ForC { init, step, body, .. } => {
                if let Some(s0) = init {
                    scan_stmts(ctx, sc, std::slice::from_ref(&**s0), Some(id), i)?;
                }
                if let Some(s0) = step {
                    scan_stmts(ctx, sc, std::slice::from_ref(&**s0), Some(id), i)?;
                }
                scan_stmts(ctx, sc, body, Some(id), i)?;
            }
            Stmt::Try { body, handler, .. } => {
                scan_stmts(ctx, sc, body, Some(id), i)?;
                scan_stmts(ctx, sc, handler, Some(id), i)?;
            }
            // 嵌套函数 / async fn / class 方法 / lambda：独立函数作用域，标签不跨函数
            _ => {}
        }
    }
    Ok(id)
}

fn validate_goto(ctx: &Ctx, top_stmts: &[Stmt]) -> Result<(), ZError> {
    // 每个函数作用域单独做一次扫描与校验
    check_scope(ctx, top_stmts)?;
    Ok(())
}

/// 对一个函数作用域的语句列表做标签/跳转校验，并递归进入嵌套函数（各自独立校验）。
fn check_scope(ctx: &Ctx, stmts: &[Stmt]) -> Result<(), ZError> {
    let mut sc = GotoScan::new();
    scan_stmts(ctx, &mut sc, stmts, None, 0)?;

    for (name, list_id, gidx, span) in &sc.gotos {
        let Some((label_list, lidx, _)) = sc.labels.get(name).copied() else {
            return Err(ctx.err(
                codes::LABEL,
                format!("undefined label `{}`", name),
                span,
                Some(format!(
                    "`goto` 只能跳到同一函数内的标签；本函数内已定义的标签：{}",
                    label_names(&sc)
                )),
            ));
        };
        // 向上回溯：标签所在列表必须是跳转点所在列表的祖先（含自身）——
        // 这保证永远不会「跳入内层语句块/循环体」。
        let mut cur = *list_id;
        let mut src_idx = *gidx;
        loop {
            if cur == label_list {
                break;
            }
            let info = &sc.lists[&cur];
            match info.parent {
                Some(p) => {
                    src_idx = info.owner_index;
                    cur = p;
                }
                None => {
                    return Err(ctx.err(
                        codes::LABEL,
                        format!(
                            "label `{}` is not visible from this `goto` (不能在语句块之间跳入)",
                            name
                        ),
                        span,
                        Some("标签必须定义在跳转点所在的语句块或其外层语句块中；不能从外层跳进 if/循环等内层块"),
                    ))
                }
            }
        }
        // 向前跳：不得跳过同一语句块内的变量绑定，否则跳转后变量未初始化
        if lidx > src_idx {
            let binds = &sc.lists[&label_list].binds;
            for j in (src_idx + 1)..lidx {
                if binds[j] {
                    return Err(ctx.err(
                        codes::LABEL,
                        format!(
                            "`goto {}` jumps forward over a variable declaration/assignment",
                            name
                        ),
                        span,
                        Some("把标签移到该声明之前，或先声明变量再跳转（避免跳转后读到未初始化的变量）"),
                    ));
                }
            }
        }
    }

    // 递归处理嵌套函数作用域（各自独立）与表达式中的 lambda
    for s in stmts {
        match s {
            Stmt::FnDef { body, .. } | Stmt::AsyncFnDef { body, .. } => check_scope(ctx, body)?,
            Stmt::ClassDef { methods, .. } => {
                for m in methods {
                    if let Stmt::FnDef { body, .. } = m {
                        check_scope(ctx, body)?;
                    }
                }
            }
            _ => {}
        }
        let mut lambda_bodies: Vec<&Vec<Stmt>> = Vec::new();
        collect_lambdas_stmt(s, &mut lambda_bodies);
        for b in lambda_bodies {
            check_scope(ctx, b)?;
        }
    }
    Ok(())
}

fn label_names(sc: &GotoScan) -> String {
    if sc.labels.is_empty() {
        return "（无）".to_string();
    }
    let mut v: Vec<&str> = sc.labels.keys().map(|s| s.as_str()).collect();
    v.sort_unstable();
    v.join(", ")
}

fn collect_lambdas_expr<'a>(e: &'a Expr, out: &mut Vec<&'a Vec<Stmt>>) {
    match e {
        Expr::Lambda { body, .. } => {
            out.push(body);
        }
        Expr::ListLit(items, _) => items.iter().for_each(|x| collect_lambdas_expr(x, out)),
        Expr::DictLit(entries, _) => entries.iter().for_each(|(_, x)| collect_lambdas_expr(x, out)),
        Expr::ListComp { elem, iter, cond, .. } => {
            collect_lambdas_expr(elem, out);
            collect_lambdas_expr(iter, out);
            if let Some(c) = cond {
                collect_lambdas_expr(c, out);
            }
        }
        Expr::DictComp { key, value, iter, cond, .. } => {
            collect_lambdas_expr(key, out);
            collect_lambdas_expr(value, out);
            collect_lambdas_expr(iter, out);
            if let Some(c) = cond {
                collect_lambdas_expr(c, out);
            }
        }
        Expr::FStr(segs, _) => {
            for seg in segs {
                if let FStrSeg::Code(x) = seg {
                    collect_lambdas_expr(x, out);
                }
            }
        }
        Expr::Field { obj, .. } | Expr::OptionalField { obj, .. } => collect_lambdas_expr(obj, out),
        Expr::Index { obj, index, .. } => {
            collect_lambdas_expr(obj, out);
            collect_lambdas_expr(index, out);
        }
        Expr::Unary { expr, .. } | Expr::Await { expr, .. } => collect_lambdas_expr(expr, out),
        Expr::Binary { lhs, rhs, .. } => {
            collect_lambdas_expr(lhs, out);
            collect_lambdas_expr(rhs, out);
        }
        Expr::Call { args, .. } => args.iter().for_each(|x| collect_lambdas_expr(x, out)),
        Expr::Match { value, arms, .. } => {
            collect_lambdas_expr(value, out);
            arms.iter().for_each(|(_, b)| collect_lambdas_expr(b, out));
        }
        Expr::Ternary { cond, then_expr, else_expr, .. } => {
            collect_lambdas_expr(cond, out);
            collect_lambdas_expr(then_expr, out);
            collect_lambdas_expr(else_expr, out);
        }
        _ => {}
    }
}

fn collect_lambdas_stmt<'a>(s: &'a Stmt, out: &mut Vec<&'a Vec<Stmt>>) {
    match s {
        Stmt::Assign { value, .. } => collect_lambdas_expr(value, out),
        Stmt::IndexAssign { target, value, .. } => {
            collect_lambdas_expr(target, out);
            collect_lambdas_expr(value, out);
        }
        Stmt::DestructAssign { value, .. } => collect_lambdas_expr(value, out),
        Stmt::AssignOp { value, .. } => collect_lambdas_expr(value, out),
        Stmt::VarDecl { init, .. } => {
            if let Some(e) = init {
                collect_lambdas_expr(e, out);
            }
        }
        Stmt::Block { stmts, .. } => stmts.iter().for_each(|x| collect_lambdas_stmt(x, out)),
        Stmt::If { cond, then_branch, else_branch, .. } => {
            collect_lambdas_expr(cond, out);
            then_branch.iter().for_each(|x| collect_lambdas_stmt(x, out));
            if let Some(eb) = else_branch {
                eb.iter().for_each(|x| collect_lambdas_stmt(x, out));
            }
        }
        Stmt::While { cond, body, .. } => {
            collect_lambdas_expr(cond, out);
            body.iter().for_each(|x| collect_lambdas_stmt(x, out));
        }
        Stmt::DoWhile { body, cond, .. } => {
            body.iter().for_each(|x| collect_lambdas_stmt(x, out));
            collect_lambdas_expr(cond, out);
        }
        Stmt::ForC { init, cond, step, body, .. } => {
            if let Some(s0) = init {
                collect_lambdas_stmt(s0, out);
            }
            if let Some(c) = cond {
                collect_lambdas_expr(c, out);
            }
            if let Some(s0) = step {
                collect_lambdas_stmt(s0, out);
            }
            body.iter().for_each(|x| collect_lambdas_stmt(x, out));
        }
        Stmt::ForIn { iter, body, .. } => {
            collect_lambdas_expr(iter, out);
            body.iter().for_each(|x| collect_lambdas_stmt(x, out));
        }
        Stmt::Return { values, .. } => values.iter().for_each(|x| collect_lambdas_expr(x, out)),
        Stmt::DebugPrint { expr, .. } => collect_lambdas_expr(expr, out),
        Stmt::ExprStmt { expr, .. } => collect_lambdas_expr(expr, out),
        Stmt::Breakpoint { cond, .. } => {
            if let Some(c) = cond {
                collect_lambdas_expr(c, out);
            }
        }
        Stmt::Go { args, .. } => args.iter().for_each(|x| collect_lambdas_expr(x, out)),
        Stmt::Try { body, handler, .. } => {
            body.iter().for_each(|x| collect_lambdas_stmt(x, out));
            handler.iter().for_each(|x| collect_lambdas_stmt(x, out));
        }
        Stmt::Throw { value, .. } => collect_lambdas_expr(value, out),
        Stmt::FnDef { body, .. } | Stmt::AsyncFnDef { body, .. } => {
            // 独立函数作用域：由 check_scope 递归处理，此处不收集
            let _ = body;
        }
        _ => {}
    }
}
