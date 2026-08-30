// parser.rs - Hone 递归下降解析器
// 将 token 流解析为 AST，语法错误统一报 error[H005]。

use crate::ast::*;
use crate::error::codes;
use crate::error::ZError;
use crate::lexer::{Lexer, Span, Tok};

pub struct Parser {
    file: String,
    src: String,
    toks: Vec<(Tok, Span)>,
    pos: usize,
}

/// 关键字 token → 源文本。点号后允许关键字作为模块成员名
/// （如 glob.match、random.int、plugin.load），解析时映射回名字符串。
fn keyword_text(tok: &Tok) -> Option<&'static str> {
    Some(match tok {
        Tok::Fn => "fn",
        Tok::If => "if",
        Tok::Else => "else",
        Tok::While => "while",
        Tok::For => "for",
        Tok::In => "in",
        Tok::Return => "return",
        Tok::True => "true",
        Tok::False => "false",
        Tok::Go => "go",
        Tok::Try => "try",
        Tok::Catch => "catch",
        Tok::Throw => "throw",
        Tok::Match => "match",
        Tok::Break => "break",
        Tok::Breakpoint => "breakpoint",
        Tok::Load => "load",
        Tok::Lazy => "lazy",
        Tok::Use => "use",
        Tok::Import => "import",
        Tok::Alias => "alias",
        Tok::As => "as",
        Tok::From => "from",
        Tok::Tmp => "tmp",
        Tok::Struct => "struct",
        Tok::Class => "class",
        Tok::TInt => "int",
        Tok::TFloat => "float",
        Tok::TBool => "bool",
        Tok::TStr => "str",
        _ => return None,
    })
}

impl Parser {
    /// 词法分析 + 语法分析，返回整个程序的 AST。
    pub fn parse(file: &str, src: &str) -> Result<Program, ZError> {
        let toks = Lexer::new(file, src).tokenize()?;
        let mut p = Parser {
            file: file.to_string(),
            src: src.to_string(),
            toks,
            pos: 0,
        };
        let stmts = p.parse_program()?;
        Ok(Program { stmts })
    }

    /// REPL 用：把整段源码当作"单个表达式语句"解析（如 `1+1`、`[1,2]`），
    /// 供交互模式在普通语句解析失败时回退使用。src 需以 `;` 结尾。
    pub fn parse_expr_stmt_src(file: &str, src: &str) -> Result<Stmt, ZError> {
        let toks = Lexer::new(file, src).tokenize()?;
        let mut p = Parser {
            file: file.to_string(),
            src: src.to_string(),
            toks,
            pos: 0,
        };
        p.parse_expr_stmt()
    }

    /// 调试器用：解析单个表达式（如 `p x + 1`），供断点提示中的即时求值。
    pub(crate) fn parse_expr_src(file: &str, src: &str) -> Result<Expr, ZError> {
        let toks = Lexer::new(file, src).tokenize()?;
        if toks.is_empty() {
            return Err(ZError::plain(
                codes::SYNTAX,
                "empty expression",
                Some("type an expression, e.g. `x + 1`"),
            ));
        }
        let mut p = Parser {
            file: file.to_string(),
            src: src.to_string(),
            toks,
            pos: 0,
        };
        let e = p.parse_expr()?;
        // next() 把 pos 钳制在 len-1：完全消费时 pos == len-1，剩余未消费则 pos < len-1
        if p.pos < p.toks.len() - 1 {
            return Err(p.err_here(
                codes::SYNTAX,
                "unexpected trailing tokens after expression",
                None::<&str>,
            ));
        }
        Ok(e)
    }

    // ---------- 基础工具 ----------

    fn cur(&self) -> &(Tok, Span) {
        &self.toks[self.pos.min(self.toks.len() - 1)]
    }

    fn peek(&self) -> &Tok {
        &self.cur().0
    }

    fn peek2(&self) -> &Tok {
        let idx = (self.pos + 1).min(self.toks.len() - 1);
        &self.toks[idx].0
    }

    fn next(&mut self) -> (Tok, Span) {
        let t = self.toks[self.pos.min(self.toks.len() - 1)].clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn at(&self, t: &Tok) -> bool {
        self.peek() == t
    }

    fn err_here(&self, code: &'static str, msg: impl Into<String>, help: Option<impl Into<String>>) -> ZError {
        let (_, span) = self.cur();
        ZError::new(code, msg, &self.file, &self.src, span.line, span.col, span.len.max(1), help)
    }

    fn err_at(&self, span: &Span, code: &'static str, msg: impl Into<String>, help: Option<impl Into<String>>) -> ZError {
        ZError::new(code, msg, &self.file, &self.src, span.line, span.col, span.len.max(1), help)
    }

    fn expect(&mut self, t: &Tok, what: &str) -> Result<(Tok, Span), ZError> {
        if self.at(t) {
            Ok(self.next())
        } else {
            Err(self.err_here(
                codes::SYNTAX,
                format!("expected {}, found {}", what, self.peek().describe()),
                Some(format!("insert `{}` here", t.describe())),
            ))
        }
    }

    fn expect_semi(&mut self) -> Result<(), ZError> {
        if self.at(&Tok::Semi) {
            self.next();
            Ok(())
        } else {
            Err(self.err_here(
                codes::MISSING_SEMI,
                format!("expected `;`, found {}", self.peek().describe()),
                Some("insert `;` at the end of the statement"),
            ))
        }
    }

    // ---------- 程序 ----------

    fn parse_program(&mut self) -> Result<Vec<Stmt>, ZError> {
        let mut stmts = Vec::new();
        while !self.at(&Tok::Eof) {
            if self.at(&Tok::Semi) {
                self.next();
                continue;
            }
            stmts.push(self.parse_stmt()?);
        }
        Ok(stmts)
    }

    // ---------- 语句 ----------

    fn parse_stmt(&mut self) -> Result<Stmt, ZError> {
        match self.peek() {
            Tok::TInt | Tok::TFloat | Tok::TBool | Tok::TStr => self.parse_decl_c(),
            Tok::Ident(_) => {
                if self.peek2() == &Tok::LParen && self.peek() == &Tok::Ident("debug_print".to_string()) {
                    self.parse_debug_print()
                } else if self.peek2() == &Tok::Colon {
                    self.parse_decl_ts()
                } else if self.peek2() == &Tok::Assign {
                    self.parse_assign()
                } else if self.peek2() == &Tok::Comma {
                    // a, b = expr;  列表解构赋值
                    self.parse_destruct_assign()
                } else if matches!(
                    self.peek2(),
                    Tok::PlusEq | Tok::MinusEq | Tok::StarEq | Tok::SlashEq | Tok::PercentEq
                ) {
                    self.parse_assign_op()
                } else if self.peek2() == &Tok::LBracket {
                    // a[i] = x;  列表索引赋值（读 a[i] 走表达式后缀解析）
                    self.parse_index_stmt()
                } else {
                    self.parse_expr_stmt()
                }
            }
            Tok::Fn => self.parse_fn_def(),
            Tok::If => self.parse_if(),
            Tok::While => self.parse_while(),
            Tok::Do => self.parse_do_while(),
            Tok::For => {
                // for (init; cond; step) 为 C 风格循环；for x in ... 为遍历循环
                if self.peek2() == &Tok::LParen {
                    self.parse_for_c()
                } else {
                    self.parse_for_in()
                }
            }
            Tok::Return => self.parse_return(),
            Tok::Go => self.parse_go(),
            Tok::Try => self.parse_try(),
            Tok::Throw => self.parse_throw(),
            // 语句级前缀自增/自减：++i;  --i;
            Tok::PlusPlus | Tok::MinusMinus => self.parse_expr_stmt(),
            Tok::Continue => {
                let (_, span) = self.next();
                self.expect_semi()?;
                Ok(Stmt::Continue { span })
            }
            Tok::Break => {
                let (_, span) = self.next();
                self.expect_semi()?;
                Ok(Stmt::Break { span })
            }
            Tok::Breakpoint => {
                let (_, span) = self.next();
                // 条件断点：breakpoint if (expr);
                let cond = if self.peek() == &Tok::If {
                    self.next();
                    self.expect(&Tok::LParen, "`(`")?;
                    let e = self.parse_expr()?;
                    self.expect(&Tok::RParen, "`)`")?;
                    Some(Box::new(e))
                } else {
                    None
                };
                self.expect_semi()?;
                Ok(Stmt::Breakpoint { span, cond })
            }
            Tok::LBrace => {
                // 语句起始 `{`：先探测 `{a, b} = ...` 字典解构模式，否则按代码块解析
                if self.scan_dict_destruct() {
                    self.parse_dict_destruct()
                } else {
                    self.parse_block_stmt()
                }
            }
            Tok::Semi => {
                self.next();
                self.parse_stmt()
            }
            Tok::Load => self.parse_load(),
            Tok::Use => self.parse_use(),
            Tok::Import => self.parse_import(),
            Tok::Alias => self.parse_alias(),
            Tok::At => self.parse_export(),
            Tok::Tmp => self.parse_tmp_fn(),
            Tok::Struct => self.parse_struct_def(),
            Tok::Class => self.parse_class_def(),
            Tok::Enum => self.parse_enum_def(),
            Tok::Async => self.parse_async_fn(),
            // 语句级 await：await expr;（如 try 块内的 await 调用）
            Tok::Await => self.parse_expr_stmt(),
            other => Err(self.err_here(
                codes::SYNTAX,
                format!("expected a statement, found {}", other.describe()),
                Some("statements start with an identifier, `fn`, `if`, `while`, `do`, `for`, `return`, `go`, `break`, `continue`, `breakpoint`, `try` or `{`"),
            )),
        }
    }

    /// C 风格声明：int x = 10;
    fn parse_decl_c(&mut self) -> Result<Stmt, ZError> {
        let (ty_tok, span) = self.next();
        let ty = match ty_tok {
            Tok::TInt => TyName::Int,
            Tok::TFloat => TyName::Float,
            Tok::TBool => TyName::Bool,
            Tok::TStr => TyName::Str,
            _ => unreachable!(),
        };
        let (name_tok, name_span) = self.next();
        let name = match name_tok {
            Tok::Ident(s) => s,
            other => {
                return Err(self.err_at(
                    &name_span,
                    codes::SYNTAX,
                    format!("expected a variable name after type, found {}", other.describe()),
                    Some("declaration form: `int x = 10;`"),
                ))
            }
        };
        let init = if self.at(&Tok::Assign) {
            self.next();
            Some(self.parse_expr()?)
        } else {
            None
        };
        self.expect_semi()?;
        Ok(Stmt::VarDecl {
            name,
            ty,
            init,
            span,
        })
    }

    /// Rust/TS 风格声明：x : int = 10;
    fn parse_decl_ts(&mut self) -> Result<Stmt, ZError> {
        let (name_tok, span) = self.next();
        let name = match name_tok {
            Tok::Ident(s) => s,
            other => {
                return Err(self.err_here(
                    codes::SYNTAX,
                    format!("expected a variable name, found {}", other.describe()),
                    None::<&str>,
                ))
            }
        };
        self.expect(&Tok::Colon, "`:`")?;
        let ty = self.parse_type()?;
        let init = if self.at(&Tok::Assign) {
            self.next();
            Some(self.parse_expr()?)
        } else {
            None
        };
        self.expect_semi()?;
        Ok(Stmt::VarDecl {
            name,
            ty,
            init,
            span,
        })
    }

    /// x = expr;
    fn parse_assign(&mut self) -> Result<Stmt, ZError> {
        let (name_tok, span) = self.next();
        let name = match name_tok {
            Tok::Ident(s) => s,
            _ => unreachable!(),
        };
        self.next(); // =
        let value = self.parse_expr()?;
        self.expect_semi()?;
        Ok(Stmt::Assign { name, value, span })
    }

    /// a[i] = x;  列表索引赋值；a[i];  索引表达式语句。
    /// 变量须先声明为列表，支持链式索引目标（m[i][j] = x），下标越界/非列表在运行时报错。
    fn parse_index_stmt(&mut self) -> Result<Stmt, ZError> {
        let (name_tok, span) = self.next();
        let name = match name_tok {
            Tok::Ident(s) => s,
            _ => unreachable!(),
        };
        // 链式索引目标：a[i][j]... 逐层包装为 Expr::Index
        let mut target = Expr::Ident { name, span };
        loop {
            if self.at(&Tok::LBracket) {
                self.next();
                let idx = self.parse_expr()?;
                self.expect(&Tok::RBracket, "`]`")?;
                target = Expr::Index {
                    obj: Box::new(target),
                    index: Box::new(idx),
                    span,
                };
            } else {
                break;
            }
        }
        if self.at(&Tok::Assign) {
            self.next();
            let value = self.parse_expr()?;
            self.expect_semi()?;
            Ok(Stmt::IndexAssign { target, value, span })
        } else {
            self.expect_semi()?;
            Ok(Stmt::ExprStmt { expr: target, span })
        }
    }

    /// a, b = expr;  列表解构赋值：右侧为列表（或多返回值），按位置依次绑定变量。
    fn parse_destruct_assign(&mut self) -> Result<Stmt, ZError> {
        let (name_tok, span) = self.next();
        let name = match name_tok {
            Tok::Ident(s) => s,
            _ => unreachable!(),
        };
        let mut targets = Vec::new();
        let mut cur = name;
        loop {
            targets.push((cur, None)); // 列表解构：无字典键，按位置取
            if self.at(&Tok::Comma) {
                self.next();
                let (t2, tspan) = self.next();
                cur = match t2 {
                    Tok::Ident(s) => s,
                    other => {
                        return Err(self.err_at(
                            &tspan,
                            codes::SYNTAX,
                            format!("expected a variable name in destructuring, found {}", other.describe()),
                            Some("form: `a, b = [1, 2]`"),
                        ))
                    }
                };
            } else {
                break;
            }
        }
        self.expect(&Tok::Assign, "`=`")?;
        let value = self.parse_expr()?;
        self.expect_semi()?;
        Ok(Stmt::DestructAssign { targets, value, span })
    }

    /// 探测语句起始的 `{` 是否为字典解构模式：`{ident [, ident]*} =` 或 `{ident: ident [, ...]} =`。
    /// 只有 `}` 后紧跟 `=` 才判定为解构，否则回退为代码块解析。
    fn scan_dict_destruct(&self) -> bool {
        let mut i = self.pos + 1; // 跳过 {
        let mut depth = 1usize;
        while i < self.toks.len() {
            match &self.toks[i].0 {
                Tok::Ident(_) | Tok::Comma | Tok::Colon => i += 1,
                Tok::LBrace => {
                    depth += 1;
                    i += 1;
                }
                Tok::RBrace => {
                    depth -= 1;
                    i += 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => return false,
            }
        }
        if depth != 0 {
            return false;
        }
        matches!(self.toks.get(i).map(|(t, _)| t), Some(Tok::Assign))
    }

    /// {a, b} = expr;  字典解构赋值：右侧为字典，按键取出。
    /// {a, b} 形式绑定同名变量；{a: x, b: y} 形式可改名（x = dict["a"]）。
    fn parse_dict_destruct(&mut self) -> Result<Stmt, ZError> {
        let (_, span) = self.next(); // {
        let mut targets = Vec::new();
        loop {
            let (kt, kspan) = self.next();
            let key = match kt {
                Tok::Ident(k) => k,
                other => {
                    return Err(self.err_at(
                        &kspan,
                        codes::SYNTAX,
                        format!("expected a dict key name in `{{...}} = ...`, found {}", other.describe()),
                        Some("form: `{a, b} = dict` or `{a: x, b: y} = dict`"),
                    ))
                }
            };
            // 可选改名：{a: x} → x = dict["a"]
            let var = if self.at(&Tok::Colon) {
                self.next();
                let (vt, vspan) = self.next();
                match vt {
                    Tok::Ident(v) => v,
                    other => {
                        return Err(self.err_at(
                            &vspan,
                            codes::SYNTAX,
                            format!("expected a variable name after `:`, found {}", other.describe()),
                            Some("form: `{a: x}` binds `x` to the dict key `a`"),
                        ))
                    }
                }
            } else {
                key.clone()
            };
            targets.push((var, Some(key)));
            if self.at(&Tok::Comma) {
                self.next();
                if self.at(&Tok::RBrace) {
                    break;
                }
                continue;
            }
            break;
        }
        self.expect(&Tok::RBrace, "`}`")?;
        self.expect(&Tok::Assign, "`=`")?;
        let value = self.parse_expr()?;
        self.expect_semi()?;
        Ok(Stmt::DestructAssign { targets, value, span })
    }

    /// 复合赋值：x += expr;  x -= expr;  x *= expr;  x /= expr;  x %= expr;
    fn parse_assign_op(&mut self) -> Result<Stmt, ZError> {
        let (name_tok, span) = self.next();
        let name = match name_tok {
            Tok::Ident(s) => s,
            _ => unreachable!(),
        };
        let (op_tok, _) = self.next();
        let op = match op_tok {
            Tok::PlusEq => CompoundOp::Add,
            Tok::MinusEq => CompoundOp::Sub,
            Tok::StarEq => CompoundOp::Mul,
            Tok::SlashEq => CompoundOp::Div,
            Tok::PercentEq => CompoundOp::Mod,
            _ => unreachable!(),
        };
        let value = self.parse_expr()?;
        self.expect_semi()?;
        Ok(Stmt::AssignOp { name, op, value, span })
    }

    fn parse_expr_stmt(&mut self) -> Result<Stmt, ZError> {
        let expr = self.parse_expr()?;
        let span = expr_span(&expr);
        self.expect_semi()?;
        Ok(Stmt::ExprStmt { expr, span })
    }

    fn parse_fn_def(&mut self) -> Result<Stmt, ZError> {
        self.next(); // fn
        self.parse_fn_body(false)
    }

    fn parse_tmp_fn(&mut self) -> Result<Stmt, ZError> {
        self.next(); // tmp
        self.expect(&Tok::Fn, "`fn`")?;
        self.parse_fn_body(true)
    }

    /// 匿名函数（lambda）：fn(参数1, 参数2) { ... }
    /// 无函数名、无泛型、无 -> 返回注解；返回类型动态。
    /// 调用时 `fn` token 已被 parse_primary 消费（span 由调用方传入）。
    fn parse_lambda(&mut self, span: Span) -> Result<Expr, ZError> {
        self.expect(&Tok::LParen, "`(`")?;
        let mut params = Vec::new();
        while !self.at(&Tok::RParen) {
            let p = self.parse_param()?;
            params.push(p);
            if self.at(&Tok::Comma) {
                self.next();
            } else {
                break;
            }
        }
        self.expect(&Tok::RParen, "`)`")?;
        let body = self.parse_block()?;
        Ok(Expr::Lambda { params, body, span })
    }

    fn parse_fn_body(&mut self, tmp: bool) -> Result<Stmt, ZError> {
        let (name_tok, span) = self.next();
        let name = match name_tok {
            Tok::Ident(s) => s,
            Tok::LParen => {
                return Err(self.err_here(
                    codes::SYNTAX,
                    "expected a function name after `fn`, found `(`",
                    Some("a lambda must be assigned or passed: `f = fn(x) { ... }`, or define a named function `fn name(...) { ... }`"),
                ))
            }
            other => {
                return Err(self.err_here(
                    codes::SYNTAX,
                    format!("expected a function name after `fn`, found {}", other.describe()),
                    Some("function form: `fn name(param1, param2) { ... }`"),
                ))
            }
        };
        // 泛型类型参数：fn name[T, U](...)（编译期擦除，运行期零成本）
        let mut type_params = Vec::new();
        if self.at(&Tok::LBracket) {
            self.next(); // [
            while !self.at(&Tok::RBracket) {
                let (tp_tok, tp_span) = self.next();
                match tp_tok {
                    Tok::Ident(s) => type_params.push(s),
                    other => {
                        return Err(self.err_at(
                            &tp_span,
                            codes::SYNTAX,
                            format!("expected a type parameter name, found {}", other.describe()),
                            Some("type parameters look like `fn name[T, U](...)`"),
                        ))
                    }
                }
                if self.at(&Tok::Comma) {
                    self.next();
                } else {
                    break;
                }
            }
            self.expect(&Tok::RBracket, "`]`")?;
        }
        self.expect(&Tok::LParen, "`(`")?;
        let mut params = Vec::new();
        while !self.at(&Tok::RParen) {
            params.push(self.parse_param()?);
            if self.at(&Tok::Comma) {
                self.next();
            } else {
                break;
            }
        }
        self.expect(&Tok::RParen, "`)`")?;
        let ret = if self.at(&Tok::Arrow) {
            self.next();
            Some(self.parse_type()?)
        } else {
            None
        };
        self.expect(&Tok::LBrace, "`{`")?;
        let body = self.parse_block_body()?;
        Ok(Stmt::FnDef {
            name,
            type_params,
            params,
            ret,
            body,
            span,
            tmp,
        })
    }

    /// debug_print(expr);
    fn parse_debug_print(&mut self) -> Result<Stmt, ZError> {
        self.next(); // debug_print
        let (_, span) = self.next(); // (
        let expr = self.parse_expr()?;
        self.expect(&Tok::RParen, "`)`")?;
        self.expect_semi()?;
        Ok(Stmt::DebugPrint {
            expr: Box::new(expr),
            span,
        })
    }

    /// 参数：name | name : type | type name
    fn parse_param(&mut self) -> Result<Param, ZError> {
        let (tok, span) = self.next();
        match tok {
            Tok::Ident(s) => {
                let ty = if self.at(&Tok::Colon) {
                    self.next();
                    Some(self.parse_type()?)
                } else {
                    None
                };
                // 默认参数值：a = 表达式
                let default = if self.at(&Tok::Assign) {
                    self.next();
                    Some(self.parse_expr()?)
                } else {
                    None
                };
                Ok(Param { name: s, ty, span, default })
            }
            Tok::TInt | Tok::TFloat | Tok::TBool | Tok::TStr => {
                let ty = match tok {
                    Tok::TInt => TyName::Int,
                    Tok::TFloat => TyName::Float,
                    Tok::TBool => TyName::Bool,
                    Tok::TStr => TyName::Str,
                    _ => unreachable!(),
                };
                let (name_tok, name_span) = self.next();
                let name = match name_tok {
                    Tok::Ident(s) => s,
                    other => {
                        return Err(self.err_at(
                            &name_span,
                            codes::SYNTAX,
                            format!("expected a parameter name, found {}", other.describe()),
                            None::<&str>,
                        ))
                    }
                };
                // 默认参数值：int a = 表达式
                let default = if self.at(&Tok::Assign) {
                    self.next();
                    Some(self.parse_expr()?)
                } else {
                    None
                };
                Ok(Param {
                    name,
                    ty: Some(ty),
                    span,
                    default,
                })
            }
            other => Err(self.err_here(
                codes::SYNTAX,
                format!("expected a parameter, found {}", other.describe()),
                Some("parameter form: `a`, `a : int`, `int a`, or with a default `a = 10`"),
            )),
        }
    }

    fn parse_type(&mut self) -> Result<TyName, ZError> {
        let (tok, _) = self.next();
        match tok {
            Tok::TInt => Ok(TyName::Int),
            Tok::TFloat => Ok(TyName::Float),
            Tok::TBool => Ok(TyName::Bool),
            Tok::TStr => Ok(TyName::Str),
            // 泛型类型变量（fn name[T] 的 T）：注解写 `x: T`。是否已声明由 checker 校验。
            Tok::Ident(s) => Ok(TyName::Var(s)),
            other => Err(self.err_here(
                codes::SYNTAX,
                format!("expected a type name (`int`/`float`/`bool`/`str` or a type parameter), found {}", other.describe()),
                None::<&str>,
            )),
        }
    }

    fn parse_if(&mut self) -> Result<Stmt, ZError> {
        let (_, span) = self.next(); // if
        self.expect(&Tok::LParen, "`(`")?;
        let cond = self.parse_expr()?;
        self.expect(&Tok::RParen, "`)`")?;
        let then_branch = self.parse_block()?;
        let else_branch = if self.at(&Tok::Else) {
            self.next();
            if self.at(&Tok::If) {
                Some(vec![self.parse_if()?])
            } else {
                Some(self.parse_block()?)
            }
        } else {
            None
        };
        Ok(Stmt::If {
            cond,
            then_branch,
            else_branch,
            span,
        })
    }

    fn parse_while(&mut self) -> Result<Stmt, ZError> {
        let (_, span) = self.next(); // while
        self.expect(&Tok::LParen, "`(`")?;
        let cond = self.parse_expr()?;
        self.expect(&Tok::RParen, "`)`")?;
        let body = self.parse_block()?;
        Ok(Stmt::While { cond, body, span })
    }

    /// do { ... } while (cond);
    fn parse_do_while(&mut self) -> Result<Stmt, ZError> {
        let (_, span) = self.next(); // do
        let body = self.parse_block()?;
        self.expect(&Tok::While, "`while`")?;
        self.expect(&Tok::LParen, "`(`")?;
        let cond = self.parse_expr()?;
        self.expect(&Tok::RParen, "`)`")?;
        self.expect_semi()?;
        Ok(Stmt::DoWhile { body, cond, span })
    }

    /// C 风格三段式循环：for (init; cond; step) { ... }，各段均可省略
    fn parse_for_c(&mut self) -> Result<Stmt, ZError> {
        let (_, span) = self.next(); // for
        self.expect(&Tok::LParen, "`(`")?;
        // init 段：赋值 / 复合赋值 / 自增自减 / 表达式，或空
        let init = if self.at(&Tok::Semi) {
            self.next();
            None
        } else {
            let s = self.parse_for_part_stmt()?;
            self.expect(&Tok::Semi, "`;`")?;
            Some(Box::new(s))
        };
        // cond 段：表达式，或空
        let cond = if self.at(&Tok::Semi) {
            self.next();
            None
        } else {
            let e = self.parse_expr()?;
            self.expect(&Tok::Semi, "`;`")?;
            Some(e)
        };
        // step 段：赋值 / 复合赋值 / 自增自减 / 表达式，或空
        let step = if self.at(&Tok::RParen) {
            None
        } else {
            Some(Box::new(self.parse_for_part_stmt()?))
        };
        self.expect(&Tok::RParen, "`)`")?;
        let body = self.parse_block()?;
        Ok(Stmt::ForC { init, cond, step, body, span })
    }

    /// 解析 C 风格 for 的 init/step 段：识别 `x = e`、`x op= e`、`x++` / `++x`，
    /// 其余按表达式处理（如 `x |> f`、函数调用等）。
    fn parse_for_part_stmt(&mut self) -> Result<Stmt, ZError> {
        let (tok, span) = self.cur().clone();
        if let Tok::Ident(name) = tok {
            match self.peek2() {
                Tok::Assign => {
                    self.next(); // 变量名
                    self.next(); // =
                    let value = self.parse_expr()?;
                    return Ok(Stmt::Assign { name, value, span });
                }
                Tok::PlusEq | Tok::MinusEq | Tok::StarEq | Tok::SlashEq | Tok::PercentEq => {
                    self.next(); // 变量名
                    let (op_tok, _) = self.next();
                    let op = match op_tok {
                        Tok::PlusEq => CompoundOp::Add,
                        Tok::MinusEq => CompoundOp::Sub,
                        Tok::StarEq => CompoundOp::Mul,
                        Tok::SlashEq => CompoundOp::Div,
                        Tok::PercentEq => CompoundOp::Mod,
                        _ => unreachable!(),
                    };
                    let value = self.parse_expr()?;
                    return Ok(Stmt::AssignOp { name, op, value, span });
                }
                _ => {}
            }
        }
        let e = self.parse_expr()?;
        let espan = expr_span(&e);
        Ok(Stmt::ExprStmt { expr: e, span: espan })
    }

    /// for x in expr { ... } / for k, v in dict { ... }
    fn parse_for_in(&mut self) -> Result<Stmt, ZError> {
        let (_, span) = self.next(); // for
        let (v1_tok, _) = self.next();
        let var = match v1_tok {
            Tok::Ident(v) => v,
            other => {
                return Err(self.err_here(
                    codes::SYNTAX,
                    format!("expected a loop variable, found {}", other.describe()),
                    Some("form: `for x in list { ... }`"),
                ))
            }
        };
        // 可选第二变量：for k, v in dict { ... }
        let mut var2 = None;
        if self.at(&Tok::Comma) {
            self.next();
            let (v2_tok, _) = self.next();
            match v2_tok {
                Tok::Ident(v) => var2 = Some(v),
                other => {
                    return Err(self.err_here(
                        codes::SYNTAX,
                        format!("expected a second loop variable, found {}", other.describe()),
                        Some("form: `for k, v in dict { ... }`"),
                    ))
                }
            }
        }
        self.expect(&Tok::In, "`in`")?;
        let iter = self.parse_expr()?;
        let body = self.parse_block()?;
        Ok(Stmt::ForIn { var, var2, iter, body, span })
    }

    /// 推导式子句：for var [, var2] in iter [if cond]（列表/字典推导式共用）。
    fn parse_comp_clause(&mut self) -> Result<(String, Option<String>, Expr, Option<Expr>), ZError> {
        self.expect(&Tok::For, "`for`")?;
        let (v1t, _) = self.next();
        let var = match v1t {
            Tok::Ident(v) => v,
            other => {
                return Err(self.err_here(
                    codes::SYNTAX,
                    format!("expected a loop variable in comprehension, found {}", other.describe()),
                    Some("form: `[x * 2 for x in nums if x > 0]`"),
                ))
            }
        };
        // 可选第二变量：{k: v for k, v in dict}
        let mut var2 = None;
        if self.at(&Tok::Comma) {
            self.next();
            let (v2t, _) = self.next();
            var2 = match v2t {
                Tok::Ident(v) => Some(v),
                other => {
                    return Err(self.err_here(
                        codes::SYNTAX,
                        format!("expected a second loop variable in comprehension, found {}", other.describe()),
                        Some("form: `{k: v for k, v in dict}`"),
                    ))
                }
            };
        }
        self.expect(&Tok::In, "`in`")?;
        let iter = self.parse_expr()?;
        // 可选过滤：if cond
        let cond = if self.at(&Tok::If) {
            self.next();
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok((var, var2, iter, cond))
    }

    /// return; / return expr; / return a, b, ...;
    /// 多返回值以逗号分隔，运行时打包为列表，由解构赋值 `a, b = f()` 接收。
    fn parse_return(&mut self) -> Result<Stmt, ZError> {
        let (_, span) = self.next(); // return
        let mut values = Vec::new();
        if !self.at(&Tok::Semi) {
            values.push(self.parse_expr()?);
            while self.at(&Tok::Comma) {
                self.next();
                values.push(self.parse_expr()?);
            }
        }
        self.expect_semi()?;
        Ok(Stmt::Return { values, span })
    }

    /// try { body } catch e { handler }
    fn parse_try(&mut self) -> Result<Stmt, ZError> {
        let (_, span) = self.next(); // try
        let body = self.parse_block()?;
        if !self.at(&Tok::Catch) {
            return Err(self.err_here(
                codes::SYNTAX,
                "expected `catch` after the `try` block",
                Some("form: `try { ... } catch e { ... }`"),
            ));
        }
        self.next(); // catch
        let (var_tok, var_span) = self.next();
        let catch_var = match var_tok {
            Tok::Ident(s) => s,
            other => {
                return Err(self.err_at(
                    &var_span,
                    codes::SYNTAX,
                    format!("expected an error variable name after `catch`, found {}", other.describe()),
                    Some("form: `catch e` where `e` is a new variable of type `error`"),
                ))
            }
        };
        let handler = self.parse_block()?;
        Ok(Stmt::Try {
            body,
            catch_var,
            handler,
            span,
        })
    }

    /// throw 表达式;  主动抛出 str 或 error 值
    fn parse_throw(&mut self) -> Result<Stmt, ZError> {
        let (_, span) = self.next(); // throw
        let value = self.parse_expr()?;
        self.expect_semi()?;
        Ok(Stmt::Throw { value, span })
    }

    /// @export 函数名;
    fn parse_export(&mut self) -> Result<Stmt, ZError> {
        let (_, span) = self.next(); // @
        let (tok, _) = self.next();
        match tok {
            Tok::Ident(s) if s == "export" => {}
            other => {
                return Err(self.err_here(
                    codes::SYNTAX,
                    format!("expected `export` after `@`, found {}", other.describe()),
                    Some("`@export` form: `@export function_name;`"),
                ))
            }
        }
        let (name_tok, _) = self.next();
        let name = match name_tok {
            Tok::Ident(s) => s,
            other => {
                return Err(self.err_here(
                    codes::SYNTAX,
                    format!("expected a function name after `@export`, found {}", other.describe()),
                    Some("`@export` form: `@export function_name;`"),
                ))
            }
        };
        self.expect_semi()?;
        Ok(Stmt::Export { name, span })
    }

    /// load ["lazy"] "路径" [as 别名] [ { fn 签名...; } ];
    fn parse_load(&mut self) -> Result<Stmt, ZError> {
        let (_, span) = self.next(); // load
        let lazy = if self.at(&Tok::Lazy) {
            self.next();
            true
        } else {
            false
        };
        let (ptok, _) = self.next();
        let path = match ptok {
            Tok::StrLit(s) => s,
            other => {
                return Err(self.err_here(
                    codes::SYNTAX,
                    format!("expected a library path string after `load`, found {}", other.describe()),
                    Some("`load` form: `load \"path/to/lib\" [as lib];`"),
                ))
            }
        };
        let alias = if self.at(&Tok::As) {
            self.next();
            let (atok, _) = self.next();
            match atok {
                Tok::Ident(s) => Some(s),
                other => {
                    return Err(self.err_here(
                        codes::SYNTAX,
                        format!("expected an alias name after `as`, found {}", other.describe()),
                        Some("`load \"path\" as lib;`"),
                    ))
                }
            }
        } else {
            None
        };
        // 可选 from 子句：load "lib" as m from "header.h";
        let from = if self.at(&Tok::From) {
            self.next();
            let (ftok, _) = self.next();
            match ftok {
                Tok::StrLit(s) => Some(s),
                other => {
                    return Err(self.err_here(
                        codes::SYNTAX,
                        format!("expected a header path string after `from`, found {}", other.describe()),
                        Some("`load \"path/to/lib\" as lib from \"path/to/header.h\";`"),
                    ))
                }
            }
        } else {
            None
        };
        // 可选签名块：load "lib" as lib { fn name(params) -> ret; ... }
        let has_sigs = self.at(&Tok::LBrace);
        let sigs = if has_sigs {
            if alias.is_none() {
                return Err(self.err_here(
                    codes::SYNTAX,
                    "a signature block requires an alias: `load \"path\" as lib { fn ...; }`",
                    Some("the alias is used as the call prefix, e.g. `lib.name(...)`"),
                ));
            }
            self.parse_ffi_sigs()?
        } else {
            Vec::new()
        };
        // 签名块以 `}` 结束，无需分号；无签名块时保持旧语法 `load "path" as lib;`
        if !has_sigs {
            self.expect_semi()?;
        }
        Ok(Stmt::Load { lazy, path, alias, from, sigs, span })
    }

    /// 解析签名块 { fn name(p: ty, ...) -> ret; ... }
    fn parse_ffi_sigs(&mut self) -> Result<Vec<FfiSig>, ZError> {
        self.expect(&Tok::LBrace, "`{`")?;
        let mut sigs = Vec::new();
        while !self.at(&Tok::RBrace) {
            self.expect(&Tok::Fn, "`fn`")?;
            let (name_tok, _fspan) = self.next();
            let name = match name_tok {
                Tok::Ident(s) => s,
                other => {
                    return Err(self.err_here(
                        codes::SYNTAX,
                        format!("expected a function name after `fn` in the signature block, found {}", other.describe()),
                        Some("form: `fn name(p: ty, ...) -> ret;`"),
                    ))
                }
            };
            self.expect(&Tok::LParen, "`(`")?;
            let mut params = Vec::new();
            if !self.at(&Tok::RParen) {
                loop {
                    let (ptok, _pspan) = self.next();
                    let pname = match ptok {
                        Tok::Ident(s) => s,
                        other => {
                            return Err(self.err_here(
                                codes::SYNTAX,
                                format!("expected a parameter name, found {}", other.describe()),
                                Some("form: `name: ty`"),
                            ))
                        }
                    };
                    self.expect(&Tok::Colon, "`:`")?;
                    let (ttok, _) = self.next();
                    let ty = self.ffi_ty_from_tok(&ttok)?;
                    if params.len() >= 8 {
                        return Err(self.err_here(
                            codes::SYNTAX,
                            format!("`{}` has more than 8 parameters", name),
                            Some("the C ABI convention supports up to 8 scalar parameters"),
                        ));
                    }
                    params.push(FfiParam { name: pname, ty });
                    if self.at(&Tok::Comma) {
                        self.next();
                        if self.at(&Tok::RParen) {
                            break;
                        }
                        continue;
                    }
                    break;
                }
            }
            self.expect(&Tok::RParen, "`)`")?;
            self.expect(&Tok::Arrow, "`->`")?;
            let (rtok, _) = self.next();
            let ret = self.ffi_ty_from_tok(&rtok)?;
            self.expect_semi()?;
            sigs.push(FfiSig { name, params, ret, unsupported: None });
        }
        self.expect(&Tok::RBrace, "`}`")?;
        Ok(sigs)
    }

    /// 将 token 解析为 FFI 类型。
    fn ffi_ty_from_tok(&self, tok: &Tok) -> Result<FfiTy, ZError> {
        match tok {
            Tok::TInt => Ok(FfiTy::Int),
            Tok::TFloat => Ok(FfiTy::Float),
            Tok::TBool => Ok(FfiTy::Bool),
            Tok::TStr => Ok(FfiTy::Str),
            Tok::Ident(s) if s == "ptr" => Ok(FfiTy::Ptr),
            Tok::Ident(s) if s == "void" => Ok(FfiTy::Void),
            Tok::Fn => Err(self.err_here(
                codes::SYNTAX,
                "callback parameter types (`fn(...)`) are not supported yet",
                Some("declare the callback as `ptr` and pass a function pointer obtained from the library"),
            )),
            other => Err(self.err_here(
                codes::SYNTAX,
                format!("expected a type name, found {}", other.describe()),
                Some("supported FFI types: `int`/`float`/`bool`/`str`/`ptr`, return types also allow `void`"),
            )),
        }
    }

    /// use 命名空间;
    fn parse_use(&mut self) -> Result<Stmt, ZError> {
        let (_, span) = self.next(); // use
        let (tok, _) = self.next();
        let namespace = match tok {
            Tok::Ident(s) => s,
            other => {
                return Err(self.err_here(
                    codes::SYNTAX,
                    format!("expected a namespace name after `use`, found {}", other.describe()),
                    Some("`use` form: `use namespace;`"),
                ))
            }
        };
        self.expect_semi()?;
        Ok(Stmt::Use { namespace, span })
    }

    /// import "模块名" from "URL";
    fn parse_import(&mut self) -> Result<Stmt, ZError> {
        let (_, span) = self.next(); // import
        let (tok, _) = self.next();
        let name = match tok {
            Tok::StrLit(s) => s,
            other => {
                return Err(self.err_here(
                    codes::SYNTAX,
                    format!("expected a module name string after `import`, found {}", other.describe()),
                    Some("`import` form: `import \"mod\" from \"URL\";`"),
                ))
            }
        };
        self.expect(&Tok::From, "`from`")?;
        let (tok, _) = self.next();
        let url = match tok {
            Tok::StrLit(s) => s,
            other => {
                return Err(self.err_here(
                    codes::SYNTAX,
                    format!("expected a URL string after `from`, found {}", other.describe()),
                    Some("`import` form: `import \"mod\" from \"URL\";`"),
                ))
            }
        };
        let alias = if self.at(&Tok::As) {
            self.next();
            let (tok, _) = self.next();
            match tok {
                Tok::Ident(s) => Some(s),
                other => {
                    return Err(self.err_here(
                        codes::SYNTAX,
                        format!("expected an alias name after `as`, found {}", other.describe()),
                        Some("`import` form: `import \"mod\" from \"URL\" as alias;`"),
                    ))
                }
            }
        } else {
            None
        };
        self.expect_semi()?;
        Ok(Stmt::Import { name, url, alias, span })
    }

    /// alias 原名 as 新名;   原名支持点号路径（模块/类/内置点号函数），如 alias time.now as tnow;
    fn parse_alias(&mut self) -> Result<Stmt, ZError> {
        let (_, span) = self.next(); // alias
        let (tok, _) = self.next();
        let first = match tok {
            Tok::Ident(s) => s,
            other => {
                return Err(self.err_here(
                    codes::SYNTAX,
                    format!("expected a function name after `alias`, found {}", other.describe()),
                    Some("`alias` form: `alias original_name as new_name;`"),
                ))
            }
        };
        // 支持点号原名（模块/类/内置点号函数）：alias time.now as tnow;
        let original = self.join_dotted(first, span)?;
        self.expect(&Tok::As, "`as`")?;
        let (tok, _) = self.next();
        let new_name = match tok {
            Tok::Ident(s) => s,
            other => {
                return Err(self.err_here(
                    codes::SYNTAX,
                    format!("expected a new name after `as`, found {}", other.describe()),
                    Some("`alias` form: `alias original_name as new_name;`"),
                ))
            }
        };
        self.expect_semi()?;
        Ok(Stmt::Alias { original, new_name, span })
    }

    /// struct 名称 { 字段: 类型, ... };  定义结构体（数据形态声明）。
    fn parse_struct_def(&mut self) -> Result<Stmt, ZError> {
        let (_, span) = self.next(); // struct
        let (tok, _) = self.next();
        let name = match tok {
            Tok::Ident(s) => s,
            other => {
                return Err(self.err_here(
                    codes::SYNTAX,
                    format!("expected a struct name after `struct`, found {}", other.describe()),
                    Some("`struct` form: `struct Name { field: type, ... };`"),
                ))
            }
        };
        self.expect(&Tok::LBrace, "`{`")?;
        let mut fields = Vec::new();
        while !self.at(&Tok::RBrace) {
            let (ftok, fspan) = self.next();
            let fname = match ftok {
                Tok::Ident(s) => s,
                other => {
                    return Err(self.err_at(
                        &fspan,
                        codes::SYNTAX,
                        format!("expected a field name, found {}", other.describe()),
                        Some("fields look like `name: type`"),
                    ))
                }
            };
            self.expect(&Tok::Colon, "`:`")?;
            let ty = self.parse_type()?;
            fields.push((fname, ty));
            if self.at(&Tok::Comma) {
                self.next();
            } else {
                break;
            }
        }
        self.expect(&Tok::RBrace, "`}`")?;
        self.expect_semi()?;
        Ok(Stmt::StructDef { name, fields, span })
    }

    /// class 名称 { fn 方法(...) {...} ... }  类定义。
    /// 成员函数不进入全局符号表，只能经 `类.方法(...)` 调用。
    fn parse_class_def(&mut self) -> Result<Stmt, ZError> {
        let (_, span) = self.next(); // class
        let (tok, _) = self.next();
        let name = match tok {
            Tok::Ident(s) => s,
            other => {
                return Err(self.err_here(
                    codes::SYNTAX,
                    format!("expected a class name after `class`, found {}", other.describe()),
                    Some("`class` form: `class Name { fn method(...) { ... } ... }`"),
                ))
            }
        };
        self.expect(&Tok::LBrace, "`{`")?;
        let mut methods = Vec::new();
        while !self.at(&Tok::RBrace) {
            let (mtok, mspan) = self.next();
            match mtok {
                Tok::Fn => methods.push(self.parse_fn_body(false)?),
                Tok::Tmp => {
                    self.expect(&Tok::Fn, "`fn`")?;
                    methods.push(self.parse_fn_body(true)?);
                }
                other => {
                    return Err(self.err_at(
                        &mspan,
                        codes::SYNTAX,
                        format!("expected a method definition (`fn`), found {}", other.describe()),
                        Some("class members are functions: `fn method(params) { ... }`"),
                    ))
                }
            }
        }
        self.expect(&Tok::RBrace, "`}`")?;
        // 类定义结尾分号可选（与 struct 不同，class 是块结构）
        if self.at(&Tok::Semi) {
            self.next();
        }
        Ok(Stmt::ClassDef { name, methods, span })
    }

    /// enum 名称 { A, B(int), C(float, float) };  枚举定义。
    /// 变体逗号分隔；带载荷变体为 变体名(类型, ...)；结尾分号可选。
    fn parse_enum_def(&mut self) -> Result<Stmt, ZError> {
        let (_, span) = self.next(); // enum
        let (tok, _) = self.next();
        let name = match tok {
            Tok::Ident(s) => s,
            other => {
                return Err(self.err_here(
                    codes::SYNTAX,
                    format!("expected an enum name after `enum`, found {}", other.describe()),
                    Some("`enum` form: `enum Name { A, B(int), ... };`"),
                ))
            }
        };
        self.expect(&Tok::LBrace, "`{`")?;
        let mut variants = Vec::new();
        while !self.at(&Tok::RBrace) {
            let (vtok, vspan) = self.next();
            let vname = match vtok {
                Tok::Ident(s) => s,
                other => {
                    return Err(self.err_at(
                        &vspan,
                        codes::SYNTAX,
                        format!("expected a variant name, found {}", other.describe()),
                        Some("variants look like `Name` or `Name(type, ...)`"),
                    ))
                }
            };
            let mut payload = Vec::new();
            if self.at(&Tok::LParen) {
                self.next();
                while !self.at(&Tok::RParen) {
                    payload.push(self.parse_type()?);
                    if self.at(&Tok::Comma) {
                        self.next();
                    } else {
                        break;
                    }
                }
                self.expect(&Tok::RParen, "`)`")?;
            }
            variants.push(EnumVariant { name: vname, payload, span: vspan });
            if self.at(&Tok::Comma) {
                self.next();
            } else {
                break;
            }
        }
        self.expect(&Tok::RBrace, "`}`")?;
        // 枚举定义结尾分号可选（与 class 一致）
        if self.at(&Tok::Semi) {
            self.next();
        }
        Ok(Stmt::EnumDef { name, variants, span })
    }

    /// async fn 名称(参数) { ... }  异步函数定义。
    /// 已消费 `async` 关键字；复用 fn 解析并把 FnDef 转为 AsyncFnDef（后台线程执行 + await 等待）。
    fn parse_async_fn(&mut self) -> Result<Stmt, ZError> {
        let (_, span) = self.next(); // async
        self.expect(&Tok::Fn, "`fn`")?;
        let stmt = self.parse_fn_body(false)?;
        match stmt {
            Stmt::FnDef { name, type_params, params, ret, body, span, tmp: _ } => {
                Ok(Stmt::AsyncFnDef { name, type_params, params, ret, body, span })
            }
            _ => unreachable!("parse_fn_body always returns FnDef"),
        }
    }

    /// match 表达式 { 模式 => 分支体, ..., _ => 默认值 }  模式匹配（模式为字面量或 `_`）。
    /// 已消费 `match` 关键字；返回 Match 表达式，其值为匹配分支体的值。
    fn parse_match(&mut self, span: Span) -> Result<Expr, ZError> {
        let value = self.parse_expr()?;
        self.expect(&Tok::LBrace, "`{`")?;
        let mut arms = Vec::new();
        let mut saw_wildcard = false;
        while !self.at(&Tok::RBrace) {
            let (ptok, pspan) = self.next();
            let pat = match ptok {
                Tok::IntLit(v) => Pattern::Lit(Expr::IntLit(v, pspan)),
                Tok::FloatLit(v) => Pattern::Lit(Expr::FloatLit(v, pspan)),
                Tok::True => Pattern::Lit(Expr::BoolLit(true, pspan)),
                Tok::False => Pattern::Lit(Expr::BoolLit(false, pspan)),
                Tok::StrLit(s) => Pattern::Lit(Expr::StrLit(s, pspan)),
                // `_` 通配符：匹配任意值
                Tok::Ident(s) if s == "_" => {
                    if saw_wildcard {
                        return Err(self.err_at(
                            &pspan,
                            codes::SYNTAX,
                            "duplicate `_` wildcard in match",
                            Some("`_` may only appear once, as the last arm"),
                        ));
                    }
                    saw_wildcard = true;
                    Pattern::Wildcard
                }
                // 枚举变体模式：Color.Red（无载荷）或 Shape.Circle(r)（带载荷绑定）
                Tok::Ident(first) => {
                    let parts = self.join_dotted_parts(first, pspan)?;
                    if parts.len() != 2 {
                        return Err(self.err_at(
                            &pspan,
                            codes::SYNTAX,
                            format!("unsupported match pattern `{}`", parts.join(".")),
                            Some("patterns: literals (`1`, `\"a\"`, `true`), enum variants (`Color.Red`, `Shape.Circle(r)`), or `_` wildcard"),
                        ));
                    }
                    let (enum_name, variant) = (parts[0].clone(), parts[1].clone());
                    let mut binds = Vec::new();
                    if self.at(&Tok::LParen) {
                        self.next();
                        while !self.at(&Tok::RParen) {
                            let (btok, bspan) = self.next();
                            let b = match btok {
                                Tok::Ident(s) if s == "_" => None,
                                Tok::Ident(s) => Some(s),
                                other => {
                                    return Err(self.err_at(
                                        &bspan,
                                        codes::SYNTAX,
                                        format!("expected a binding name or `_` in variant pattern, found {}", other.describe()),
                                        Some("form: `Enum.Variant(name, _)`"),
                                    ))
                                }
                            };
                            binds.push(b);
                            if self.at(&Tok::Comma) {
                                self.next();
                            } else {
                                break;
                            }
                        }
                        self.expect(&Tok::RParen, "`)`")?;
                    }
                    Pattern::Variant { enum_name, variant, binds, span: pspan }
                }
                other => {
                    return Err(self.err_at(
                        &pspan,
                        codes::SYNTAX,
                        format!("unsupported match pattern, found {}", other.describe()),
                        Some("patterns: literals (`1`, `\"a\"`, `true`), enum variants (`Color.Red`, `Shape.Circle(r)`), or `_` wildcard"),
                    ))
                }
            };
            self.expect(&Tok::FatArrow, "`=>`")?;
            let body = self.parse_expr()?;
            arms.push((pat, body));
            if self.at(&Tok::Comma) {
                self.next();
            } else {
                break;
            }
        }
        self.expect(&Tok::RBrace, "`}`")?;
        Ok(Expr::Match { value: Box::new(value), arms, span })
    }

    fn parse_go(&mut self) -> Result<Stmt, ZError> {
        let (_, span) = self.next(); // go
        let (tok, _) = self.next();
        let first = match tok {
            Tok::Ident(s) => s,
            other => {
                return Err(self.err_here(
                    codes::SYNTAX,
                    format!("expected a function name after `go`, found {}", other.describe()),
                    Some("`go` form: `go function_name(args);`"),
                ))
            }
        };
        let callee = self.join_dotted(first, span)?;
        self.expect(&Tok::LParen, "`(`")?;
        let args = self.parse_args()?;
        self.expect(&Tok::RParen, "`)`")?;
        self.expect_semi()?;
        Ok(Stmt::Go { callee, args, span })
    }

    fn parse_block_stmt(&mut self) -> Result<Stmt, ZError> {
        let (_, span) = self.next(); // {
        let stmts = self.parse_block_body()?;
        Ok(Stmt::Block { stmts, span })
    }

    /// 解析 { ... }，消费两端大括号，返回内部语句。
    fn parse_block(&mut self) -> Result<Vec<Stmt>, ZError> {
        self.expect(&Tok::LBrace, "`{`")?;
        self.parse_block_body()
    }

    fn parse_block_body(&mut self) -> Result<Vec<Stmt>, ZError> {
        let mut stmts = Vec::new();
        while !self.at(&Tok::RBrace) {
            if self.at(&Tok::Eof) {
                return Err(self.err_here(
                    codes::SYNTAX,
                    "unexpected end of file, expected `}`",
                    Some("close the block with `}`"),
                ));
            }
            if self.at(&Tok::Semi) {
                self.next();
                continue;
            }
            stmts.push(self.parse_stmt()?);
        }
        self.next(); // }
        Ok(stmts)
    }

    // ---------- 表达式 ----------

    fn parse_expr(&mut self) -> Result<Expr, ZError> {
        let mut lhs = self.parse_ternary()?;
        // 管道操作符：x |> f  →  f(x)；x |> f(a, b)  →  f(x, a, b)
        // 左侧作为第一个参数插入右侧调用，可链式：a |> f |> g  →  g(f(a))
        while self.at(&Tok::Pipe) {
            let (_, span) = self.next();
            let (tok, fspan) = self.next();
            let first = match tok {
                Tok::Ident(s) => s,
                other => {
                    return Err(self.err_at(
                        &fspan,
                        codes::SYNTAX,
                        format!("expected a function name after `|>`, found {}", other.describe()),
                        Some("pipe form: `x |> f` or `x |> f(a, b)`"),
                    ))
                }
            };
            let callee = self.join_dotted(first, fspan)?;
            let mut args = vec![lhs];
            if self.at(&Tok::LParen) {
                self.next();
                args.extend(self.parse_args()?);
                self.expect(&Tok::RParen, "`)`")?;
            }
            lhs = Expr::Call { callee, args, span };
        }
        Ok(lhs)
    }

    /// 三元表达式：cond ? then_expr : else_expr（右结合；then 分支解析完整表达式）
    fn parse_ternary(&mut self) -> Result<Expr, ZError> {
        let cond = self.parse_coalesce()?;
        if self.at(&Tok::Question) {
            let (_, span) = self.next();
            let then_expr = self.parse_expr()?;
            if !self.at(&Tok::Colon) {
                let (tok, cspan) = self.cur().clone();
                return Err(self.err_at(
                    &cspan,
                    codes::SYNTAX,
                    format!("expected `:` in ternary expression, found {}", tok.describe()),
                    Some("ternary form: `cond ? then_expr : else_expr` (e.g. `a >= 0 ? \"pos\" : \"neg\"`)"),
                ));
            }
            self.next();
            let else_expr = self.parse_expr()?;
            Ok(Expr::Ternary {
                cond: Box::new(cond),
                then_expr: Box::new(then_expr),
                else_expr: Box::new(else_expr),
                span,
            })
        } else {
            Ok(cond)
        }
    }

    /// 空值合并：a ?? b（左结合；a 为 null 时取 b）。绑定强于 || 弱于 ?:
    fn parse_coalesce(&mut self) -> Result<Expr, ZError> {
        let mut lhs = self.parse_or()?;
        while self.at(&Tok::QuestionQuestion) {
            let (_, span) = self.next();
            let rhs = self.parse_or()?;
            lhs = Expr::Binary {
                op: BinOp::Coalesce,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_or(&mut self) -> Result<Expr, ZError> {
        let mut lhs = self.parse_and()?;
        while self.at(&Tok::OrOr) {
            let (_, span) = self.next();
            let rhs = self.parse_and()?;
            lhs = Expr::Binary {
                op: BinOp::Or,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expr, ZError> {
        let mut lhs = self.parse_equality()?;
        while self.at(&Tok::AndAnd) {
            let (_, span) = self.next();
            let rhs = self.parse_equality()?;
            lhs = Expr::Binary {
                op: BinOp::And,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_equality(&mut self) -> Result<Expr, ZError> {
        let mut lhs = self.parse_comparison()?;
        loop {
            let op = if self.at(&Tok::EqEq) {
                BinOp::Eq
            } else if self.at(&Tok::NotEq) {
                BinOp::Ne
            } else {
                break;
            };
            let (_, span) = self.next();
            let rhs = self.parse_comparison()?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_comparison(&mut self) -> Result<Expr, ZError> {
        let mut lhs = self.parse_additive()?;
        loop {
            let op = if self.at(&Tok::Lt) {
                BinOp::Lt
            } else if self.at(&Tok::Le) {
                BinOp::Le
            } else if self.at(&Tok::Gt) {
                BinOp::Gt
            } else if self.at(&Tok::Ge) {
                BinOp::Ge
            } else {
                break;
            };
            let (_, span) = self.next();
            let rhs = self.parse_additive()?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_additive(&mut self) -> Result<Expr, ZError> {
        let mut lhs = self.parse_multiplicative()?;
        loop {
            let op = if self.at(&Tok::Plus) {
                BinOp::Add
            } else if self.at(&Tok::Minus) {
                BinOp::Sub
            } else {
                break;
            };
            let (_, span) = self.next();
            let rhs = self.parse_multiplicative()?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr, ZError> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = if self.at(&Tok::Star) {
                BinOp::Mul
            } else if self.at(&Tok::Slash) {
                BinOp::Div
            } else if self.at(&Tok::Percent) {
                BinOp::Mod
            } else {
                break;
            };
            let (_, span) = self.next();
            let rhs = self.parse_unary()?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr, ZError> {
        if self.at(&Tok::Minus) {
            let (_, span) = self.next();
            let expr = self.parse_unary()?;
            Ok(Expr::Unary {
                op: UnOp::Neg,
                expr: Box::new(expr),
                span,
            })
        } else if self.at(&Tok::Bang) {
            let (_, span) = self.next();
            let expr = self.parse_unary()?;
            Ok(Expr::Unary {
                op: UnOp::Not,
                expr: Box::new(expr),
                span,
            })
        } else if self.at(&Tok::Await) {
            // await 表达式：await expr  等待 async 函数调用的 future 完成并返回结果
            let (_, span) = self.next();
            let expr = self.parse_unary()?;
            Ok(Expr::Await {
                expr: Box::new(expr),
                span,
            })
        } else if self.at(&Tok::PlusPlus) || self.at(&Tok::MinusMinus) {
            // 前缀自增/自减：++i / --i
            let (tok, span) = self.next();
            let (name_tok, _) = self.next();
            let name = match name_tok {
                Tok::Ident(s) => s,
                other => {
                    return Err(self.err_here(
                        codes::SYNTAX,
                        format!("expected a variable name after `{}`, found {}", tok.describe(), other.describe()),
                        Some("form: `++i` or `--i`"),
                    ))
                }
            };
            Ok(Expr::IncDec {
                op: if matches!(tok, Tok::PlusPlus) { IncOp::Inc } else { IncOp::Dec },
                prefix: true,
                name,
                span,
            })
        } else {
            self.parse_primary()
        }
    }

    fn parse_primary(&mut self) -> Result<Expr, ZError> {
        // 先解析原子表达式（字面量/标识符/调用/括号等），再处理后缀：可选链 ?. 与索引 [i]
        let mut expr = self.parse_primary_atom()?;
        while self.at(&Tok::QuestionDot) || self.at(&Tok::LBracket) {
            // 索引访问：a[i]（可链式 a[i][j]）
            if self.at(&Tok::LBracket) {
                let (_, ispan) = self.next();
                let idx = self.parse_expr()?;
                self.expect(&Tok::RBracket, "`]`")?;
                expr = Expr::Index {
                    obj: Box::new(expr),
                    index: Box::new(idx),
                    span: ispan,
                };
                continue;
            }
            let (_, span) = self.next();
            let (ftok, fspan) = self.next();
            let field = match ftok {
                Tok::Ident(s) => s,
                other => {
                    return Err(self.err_at(
                        &fspan,
                        codes::SYNTAX,
                        format!("expected an identifier after `?.`, found {}", other.describe()),
                        Some("optional chaining form: `a?.b`"),
                    ))
                }
            };
            expr = Expr::OptionalField {
                obj: Box::new(expr),
                field,
                span,
            };
            // 可选链后可继续普通字段访问：a?.b.c（与 JS 语义一致，? 只短路其后一个字段）
            while self.at(&Tok::Dot) {
                let (_, dspan) = self.next();
                let (dtok, dfspan) = self.next();
                let f2 = match dtok {
                    Tok::Ident(s) => s,
                    other => {
                        return Err(self.err_at(
                            &dfspan,
                            codes::SYNTAX,
                            format!("expected an identifier after `.`, found {}", other.describe()),
                            Some("field access form: `a?.b.c`"),
                        ))
                    }
                };
                expr = Expr::Field {
                    obj: Box::new(expr),
                    field: f2,
                    span: dspan,
                };
            }
        }
        Ok(expr)
    }

    fn parse_primary_atom(&mut self) -> Result<Expr, ZError> {
        let (tok, span) = self.next();
        match tok {
            Tok::IntLit(v) => Ok(Expr::IntLit(v, span)),
            Tok::FloatLit(v) => Ok(Expr::FloatLit(v, span)),
            Tok::True => Ok(Expr::BoolLit(true, span)),
            Tok::False => Ok(Expr::BoolLit(false, span)),
            Tok::StrLit(s) => Ok(Expr::StrLit(s, span)),
            // 三引号原始字符串：内容不做转义处理，与普通字符串同值
            Tok::MultiStr(s) => Ok(Expr::StrLit(s, span)),
            // 匿名函数（lambda）：fn(参数) { ... }
            Tok::Fn => self.parse_lambda(span),
            // 类型关键字在表达式位置等价于类型名字符串（供 args.get 等指定期望类型）
            Tok::TInt => Ok(Expr::StrLit("int".to_string(), span)),
            Tok::TFloat => Ok(Expr::StrLit("float".to_string(), span)),
            Tok::TBool => Ok(Expr::StrLit("bool".to_string(), span)),
            Tok::TStr => Ok(Expr::StrLit("str".to_string(), span)),
            Tok::FStr(parts) => {
                // 插值字符串：文字段保留（折叠转义大括号 {{ → {，}} → }），代码段子解析为表达式
                let mut segs = Vec::new();
                for part in parts {
                    match part {
                        crate::lexer::FStrPart::Lit(s) => {
                            let folded = s.replace("{{", "{").replace("}}", "}");
                            segs.push(FStrSeg::Lit(folded));
                        }
                        crate::lexer::FStrPart::Code(code) => {
                            let e = self.parse_fstr_code(&code, span)?;
                            segs.push(FStrSeg::Code(e));
                        }
                    }
                }
                Ok(Expr::FStr(segs, span))
            }
            Tok::LBracket => {
                // 列表字面量 [a, b, c] 或列表推导式 [elem for x in iter [if cond]]
                if self.at(&Tok::RBracket) {
                    self.next();
                    return Ok(Expr::ListLit(Vec::new(), span));
                }
                let first = self.parse_expr()?;
                if self.at(&Tok::For) {
                    let (var, var2, iter, cond) = self.parse_comp_clause()?;
                    self.expect(&Tok::RBracket, "`]`")?;
                    return Ok(Expr::ListComp {
                        elem: Box::new(first),
                        var,
                        var2,
                        iter: Box::new(iter),
                        cond: cond.map(Box::new),
                        span,
                    });
                }
                let mut items = vec![first];
                while self.at(&Tok::Comma) {
                    self.next();
                    items.push(self.parse_expr()?);
                }
                self.expect(&Tok::RBracket, "`]`")?;
                Ok(Expr::ListLit(items, span))
            }
            Tok::LBrace => {
                // 字典字面量 {"key": value, ...} 或字典推导式 {key: value for k, v in iter [if cond]}
                let key_expr = self.parse_expr()?;
                self.expect(&Tok::Colon, "`:`")?;
                let v = self.parse_expr()?;
                if self.at(&Tok::For) {
                    let (var, var2, iter, cond) = self.parse_comp_clause()?;
                    self.expect(&Tok::RBrace, "`}`")?;
                    return Ok(Expr::DictComp {
                        key: Box::new(key_expr),
                        value: Box::new(v),
                        var,
                        var2,
                        iter: Box::new(iter),
                        cond: cond.map(Box::new),
                        span,
                    });
                }
                // 普通字典字面量：键必须为字符串字面量
                let key_span = expr_span(&key_expr);
                let key = match key_expr {
                    Expr::StrLit(k, _) => k,
                    _ => {
                        return Err(self.err_at(
                            &key_span,
                            codes::SYNTAX,
                            "dict keys must be string literals (use a dict comprehension for dynamic keys: `{k: v for ...}`)",
                            Some("form: {\"key\": value, ...}"),
                        ))
                    }
                };
                let mut entries = vec![(key, v)];
                while self.at(&Tok::Comma) {
                    self.next();
                    if self.at(&Tok::RBrace) {
                        break; // 尾逗号
                    }
                    let (key_tok, kspan) = self.next();
                    let k2 = match key_tok {
                        Tok::StrLit(k) => k,
                        other => {
                            return Err(self.err_at(
                                &kspan,
                                codes::SYNTAX,
                                format!("dict keys must be string literals, found {}", other.describe()),
                                Some("form: {\"key\": value, ...}"),
                            ))
                        }
                    };
                    self.expect(&Tok::Colon, "`:`")?;
                    let v2 = self.parse_expr()?;
                    entries.push((k2, v2));
                }
                self.expect(&Tok::RBrace, "`}`")?;
                Ok(Expr::DictLit(entries, span))
            }
            Tok::LParen => {
                let inner = self.parse_expr()?;
                self.expect(&Tok::RParen, "`)`")?;
                Ok(inner)
            }
            Tok::Match => self.parse_match(span),
            Tok::Ident(first) => {
                let parts = self.join_dotted_parts(first, span)?;
                let expr = if self.at(&Tok::LParen) {
                    self.next();
                    let args = self.parse_args()?;
                    self.expect(&Tok::RParen, "`)`")?;
                    Expr::Call {
                        callee: parts.join("."),
                        args,
                        span,
                    }
                } else if parts.len() > 1 {
                    // 字段访问链：e.code / a.b.c → Field(Field(a,b),c)
                    let mut expr = Expr::Ident {
                        name: parts[0].clone(),
                        span,
                    };
                    for f in &parts[1..] {
                        expr = Expr::Field {
                            obj: Box::new(expr),
                            field: f.clone(),
                            span,
                        };
                    }
                    expr
                } else {
                    Expr::Ident {
                        name: parts[0].clone(),
                        span,
                    }
                };
                // 后缀自增/自减：i++ / i--（仅作用于裸变量名）
                if parts.len() == 1 && (self.at(&Tok::PlusPlus) || self.at(&Tok::MinusMinus)) {
                    let (tok, pspan) = self.next();
                    return Ok(Expr::IncDec {
                        op: if matches!(tok, Tok::PlusPlus) { IncOp::Inc } else { IncOp::Dec },
                        prefix: false,
                        name: parts[0].clone(),
                        span: pspan,
                    });
                }
                Ok(expr)
            }
            other => Err(self.err_here(
                codes::SYNTAX,
                format!("expected an expression, found {}", other.describe()),
                Some("an expression is a literal, a variable name, or a function call"),
            )),
        }
    }

    /// 将 "a.b.c" 合并为单个限定名（调用场景，如 go time.now()）。
    fn join_dotted(&mut self, first: String, span: Span) -> Result<String, ZError> {
        Ok(self.join_dotted_parts(first, span)?.join("."))
    }

    /// 收集 "a.b.c" 的点号链各部分。类型关键字（int/float/bool/str）在
    /// 点号后视为模块成员名（如 random.int、random.float）。
    fn join_dotted_parts(&mut self, first: String, _span: Span) -> Result<Vec<String>, ZError> {
        let mut parts = vec![first];
        while self.at(&Tok::Dot) {
            self.next();
            let (tok, span) = self.next();
            match tok {
                Tok::Ident(part) => parts.push(part),
                // 点号后的关键字一律视为模块成员名（如 glob.match、random.int、plugin.load）
                other => match keyword_text(&other) {
                    Some(kw) => parts.push(kw.to_string()),
                    None => {
                        return Err(self.err_at(
                            &span,
                            codes::SYNTAX,
                            format!("expected an identifier after `.`, found {}", other.describe()),
                            None::<&str>,
                        ))
                    }
                },
            }
        }
        Ok(parts)
    }

    /// 解析逗号分隔的参数列表（不含括号）。
    fn parse_args(&mut self) -> Result<Vec<Expr>, ZError> {
        let mut args = Vec::new();
        while !self.at(&Tok::RParen) {
            args.push(self.parse_expr()?);
            if self.at(&Tok::Comma) {
                self.next();
            } else {
                break;
            }
        }
        Ok(args)
    }

    /// 将 f-string 代码段 `{code}` 子解析为单个表达式。
    /// 错误定位统一指向外层 f-string 的 span。
    fn parse_fstr_code(&mut self, code: &str, outer: Span) -> Result<Expr, ZError> {
        let toks = match Lexer::new(&self.file, code).tokenize() {
            Ok(t) => t,
            Err(e) => {
                return Err(self.err_at(
                    &outer,
                    codes::SYNTAX,
                    format!("invalid expression in f-string: {}", e.msg),
                    Some("check the code between `{` and `}`"),
                ))
            }
        };
        let mut p = Parser {
            file: self.file.clone(),
            src: code.to_string(),
            toks,
            pos: 0,
        };
        let e = match p.parse_expr() {
            Ok(e) => e,
            Err(inner) => {
                return Err(self.err_at(
                    &outer,
                    codes::SYNTAX,
                    format!("invalid expression in f-string: {}", inner.msg),
                    Some("check the code between `{` and `}`"),
                ))
            }
        };
        if !p.at(&Tok::Eof) {
            return Err(self.err_at(
                &outer,
                codes::SYNTAX,
                "f-string code segment must be a single expression",
                Some("wrap compound code in parentheses, e.g. `{(a + b) * 2}`"),
            ));
        }
        Ok(e)
    }
}
