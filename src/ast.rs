// ast.rs - Zap 抽象语法树定义

use crate::lexer::Span;

#[derive(Debug, Clone)]
pub struct Program {
    pub stmts: Vec<Stmt>,
}

#[derive(Debug, Clone)]
pub enum Stmt {
    /// x = expr;  若 x 未声明则隐式声明（类型由 expr 推导）
    Assign {
        name: String,
        value: Expr,
        span: Span,
    },
    /// 显式类型声明：int x = 10; / x : int = 10; / x : int;
    VarDecl {
        name: String,
        ty: TyName,
        init: Option<Expr>,
        span: Span,
    },
    /// 裸代码块 { ... }
    Block {
        stmts: Vec<Stmt>,
        span: Span,
    },
    If {
        cond: Expr,
        then_branch: Vec<Stmt>,
        else_branch: Option<Vec<Stmt>>,
        span: Span,
    },
    While {
        cond: Expr,
        body: Vec<Stmt>,
        span: Span,
    },
    Return {
        value: Option<Expr>,
        span: Span,
    },
    FnDef {
        name: String,
        params: Vec<Param>,
        ret: Option<TyName>,
        body: Vec<Stmt>,
        span: Span,
        tmp: bool, // 临时函数，编译自动忽略
    },
    /// debug_print(expr);  调试输出，非调试模式自动忽略
    DebugPrint {
        expr: Box<Expr>,
        span: Span,
    },
    ExprStmt {
        expr: Expr,
        span: Span,
    },
    Breakpoint {
        span: Span,
    },
    /// @export 函数名;  标记导出到 C ABI 动态库
    Export {
        name: String,
        span: Span,
    },
    /// import "模块名" from "URL" [as 别名];  远程模块下载并缓存
    Import {
        name: String,
        url: String,
        alias: Option<String>,
        span: Span,
    },
    /// load ["lazy"] "路径" [as 别名];  动态库加载
    Load {
        lazy: bool,
        path: String,
        alias: Option<String>,
        span: Span,
    },
    /// use 命名空间;
    Use {
        namespace: String,
        span: Span,
    },
    /// alias 原名 as 新名;
    Alias {
        original: String,
        new_name: String,
        span: Span,
    },
    /// go 函数名(参数...);
    Go {
        callee: String,
        args: Vec<Expr>,
        span: Span,
    },
    /// try { ... } catch e { ... }  捕获可恢复错误，e 为 error 类型绑定到 handler 作用域
    Try {
        body: Vec<Stmt>,
        catch_var: String,
        handler: Vec<Stmt>,
        span: Span,
    },
    /// throw 表达式;  主动抛出错误（str 或 error 值）
    Throw {
        value: Expr,
        span: Span,
    },
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub ty: Option<TyName>,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TyName {
    Int,
    Float,
    Bool,
    Str,
}

#[derive(Debug, Clone)]
pub enum Expr {
    IntLit(i64, Span),
    FloatLit(f64, Span),
    BoolLit(bool, Span),
    StrLit(String, Span),
    /// 标识符；模块函数经点号合并为完整名（如 "time.now"）
    Ident { name: String, span: Span },
    /// 字段访问：obj.field（如 e.code、e.message）
    Field { obj: Box<Expr>, field: String, span: Span },
    Unary { op: UnOp, expr: Box<Expr>, span: Span },
    Binary { op: BinOp, lhs: Box<Expr>, rhs: Box<Expr>, span: Span },
    Call { callee: String, args: Vec<Expr>, span: Span },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

impl BinOp {
    pub fn symbol(&self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Mod => "%",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::And => "&&",
            BinOp::Or => "||",
        }
    }
}

pub fn expr_span(e: &Expr) -> Span {
    match e {
        Expr::IntLit(_, s)
        | Expr::FloatLit(_, s)
        | Expr::BoolLit(_, s)
        | Expr::StrLit(_, s)
        | Expr::Ident { span: s, .. }
        | Expr::Field { span: s, .. }
        | Expr::Call { span: s, .. }
        | Expr::Unary { span: s, .. }
        | Expr::Binary { span: s, .. } => *s,
    }
}
