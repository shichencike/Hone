// ast.rs - Hone 抽象语法树定义

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
    /// for x in expr { ... } / for k, v in dict { ... }
    ForIn {
        /// 循环变量（列表元素 / 字典键）
        var: String,
        /// 可选第二个变量（字典遍历时的值）
        var2: Option<String>,
        iter: Expr,
        body: Vec<Stmt>,
        span: Span,
    },
    Return {
        value: Option<Expr>,
        span: Span,
    },
    FnDef {
        name: String,
        /// 泛型类型参数（fn name[T, U](...)），编译期擦除，运行期零成本
        type_params: Vec<String>,
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
    /// load ["lazy"] "路径" [as 别名] [from "头文件.h"] [ { fn 签名...; } ];  动态库加载
    /// 签名块显式声明 C ABI 参数/返回类型（typed FFI），调用按签名转换，可被静态检查；
    /// from 子句从 C 头文件自动提取原型生成签名（与签名块二选一，签名块优先）
    Load {
        lazy: bool,
        path: String,
        alias: Option<String>,
        /// 可选的 C 头文件路径：解析其中的函数原型作为 FFI 签名
        from: Option<String>,
        sigs: Vec<FfiSig>,
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
    /// struct 名称 { 字段: 类型, ... };  结构体定义（数据形态声明，构造 = 名称(字段...)）
    StructDef {
        name: String,
        fields: Vec<(String, TyName)>,
        span: Span,
    },
    /// class 名称 { fn 方法(...) {...} ... }  类定义。
    /// 成员函数不进入全局符号表，只能经 类.方法(...) 调用（methods 中每个元素为 FnDef）。
    ClassDef {
        name: String,
        methods: Vec<Stmt>,
        span: Span,
    },
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub ty: Option<TyName>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TyName {
    Int,
    Float,
    Bool,
    Str,
    /// 泛型类型参数引用（fn name[T] 中的 T，注解写 `x: T`）
    Var(String),
}

/// load 签名块中的 C ABI 类型（typed FFI）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FfiTy {
    /// int64_t
    Int,
    /// double
    Float,
    /// _Bool / bool（按整数寄存器传递，返回时非零即 true）
    Bool,
    /// const char*（UTF-8 / C 字符串）
    Str,
    /// void*（不透明指针，Hone 侧为 ptr 值）
    Ptr,
    /// void（仅作返回类型）
    Void,
}

impl FfiTy {
    pub fn name(&self) -> &'static str {
        match self {
            FfiTy::Int => "int",
            FfiTy::Float => "float",
            FfiTy::Bool => "bool",
            FfiTy::Str => "str",
            FfiTy::Ptr => "ptr",
            FfiTy::Void => "void",
        }
    }
}

/// load 签名块中的参数声明：name: ty
#[derive(Debug, Clone)]
pub struct FfiParam {
    pub name: String,
    pub ty: FfiTy,
    pub span: Span,
}

/// load 签名块中的函数签名：fn name(p: ty, ...) -> ret;
#[derive(Debug, Clone)]
pub struct FfiSig {
    pub name: String,
    pub params: Vec<FfiParam>,
    pub ret: FfiTy,
    /// 头文件解析失败的原型（如回调/变参/数组），调用时直接报错而非 ABI 崩溃
    pub unsupported: Option<&'static str>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum FStrSeg {
    Lit(String),
    Code(Expr),
}

#[derive(Debug, Clone)]
pub enum Expr {
    IntLit(i64, Span),
    FloatLit(f64, Span),
    BoolLit(bool, Span),
    StrLit(String, Span),
    /// 标识符；模块函数经点号合并为完整名（如 "time.now"）
    Ident { name: String, span: Span },
    /// 列表字面量 [a, b, c]
    ListLit(Vec<Expr>, Span),
    /// 字典字面量 {"key": value, ...}（键为字符串）
    DictLit(Vec<(String, Expr)>, Span),
    /// 插值字符串 f"..."：文字段与代码段交替（代码段已解析为表达式）
    FStr(Vec<FStrSeg>, Span),
    /// 字段访问：obj.field（如 e.code、e.message）
    Field { obj: Box<Expr>, field: String, span: Span },
    Unary { op: UnOp, expr: Box<Expr>, span: Span },
    Binary { op: BinOp, lhs: Box<Expr>, rhs: Box<Expr>, span: Span },
    Call { callee: String, args: Vec<Expr>, span: Span },
    /// match 表达式 { 模式 => 表达式, ..., _ => 默认值 }  模式匹配，返回匹配分支的值
    Match {
        value: Box<Expr>,
        /// 模式 + 分支体；模式为 None 表示 `_` 通配符
        arms: Vec<(Option<Expr>, Expr)>,
        span: Span,
    },
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
        | Expr::ListLit(_, s)
        | Expr::DictLit(_, s)
        | Expr::FStr(_, s)
        | Expr::Ident { span: s, .. }
        | Expr::Field { span: s, .. }
        | Expr::Call { span: s, .. }
        | Expr::Unary { span: s, .. }
        | Expr::Binary { span: s, .. }
        | Expr::Match { span: s, .. } => *s,
    }
}
