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
    /// a[i] = x;  列表索引赋值：变量须先声明为列表，下标越界/非列表在运行时报错。
    /// target 为索引链表达式（如 a[i]、m[i][j]），基变量为链底 Ident。
    IndexAssign {
        target: Expr,
        value: Expr,
        span: Span,
    },
    /// 解构赋值：a, b = [1, 2]（列表，按位置）或 {a, b} = dict / {a: x, b: y} = dict（字典，按键）。
    /// 每个目标 = (变量名, 字典键)；列表解构键为 None，字典解构键为 Some(键名)。
    DestructAssign {
        targets: Vec<(String, Option<String>)>,
        value: Expr,
        span: Span,
    },
    /// 复合赋值：x += expr;  x -= expr;  x *= expr;  x /= expr;  x %= expr;
    /// 要求 x 已声明且类型匹配；str 仅支持 +=（字符串拼接）
    AssignOp {
        name: String,
        op: CompoundOp,
        value: Expr,
        span: Span,
    },
    /// 字段赋值：p.f = x;  target 为字段链表达式（如 p.f、p.a.b），基变量为链底 Ident。
    /// struct 实例（dict 表示）按字段名写回；type 实例按实例字段表写回；readonly 字段报错。
    FieldAssign {
        target: Expr,
        value: Expr,
        span: Span,
    },
    /// 显式类型声明：int x = 10; / x : int = 10; / x : int;
    /// readonly int x = 5; 只读变量：声明后不可重新赋值（含复合赋值/自增自减）。
    VarDecl {
        name: String,
        ty: TyName,
        init: Option<Expr>,
        span: Span,
        readonly: bool,
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
    /// do { ... } while (cond);  先执行循环体，再判断条件
    DoWhile {
        body: Vec<Stmt>,
        cond: Expr,
        span: Span,
    },
    /// for (init; cond; step) { ... }  C 风格三段式循环，各段均可省略
    ForC {
        init: Option<Box<Stmt>>,
        cond: Option<Expr>,
        step: Option<Box<Stmt>>,
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
    /// return; / return expr; / return a, b, ...;
    /// 多返回值（return a, b, ...）打包为列表，由解构赋值 `a, b = f()` 接收
    Return {
        values: Vec<Expr>,
        span: Span,
    },
    /// break;  跳出当前 while / for 循环（checker 校验只能在循环体内）
    Break {
        span: Span,
    },
    /// continue;  跳过本次循环剩余代码，进入下一次迭代（checker 校验只能在循环体内）
    Continue {
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
    /// breakpoint;  或  breakpoint if (expr);  断点（hone debug 模式下生效）
    Breakpoint {
        span: Span,
        /// 条件断点：仅当条件为 true 时暂停（None = 无条件）
        cond: Option<Box<Expr>>,
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
    /// struct 名称 { [readonly] 字段: 类型, ... };  结构体定义（数据形态声明，构造 = 名称(字段...)）
    /// 字段 readonly：构造后不可再写（p.f = x 报错），两种写法等价：`readonly f: int` / `f: readonly int`
    StructDef {
        name: String,
        /// (字段名, 类型, 是否只读)
        fields: Vec<(String, TyName, bool)>,
        span: Span,
    },
    /// class 名称 { fn 方法(...) {...} ... }  类定义。
    /// 成员函数不进入全局符号表，只能经 类.方法(...) 调用（methods 中每个元素为 FnDef）。
    ClassDef {
        name: String,
        methods: Vec<Stmt>,
        span: Span,
    },
    /// enum 名称 { A, B(int), C(float, float) };  枚举定义。
    /// 简单变体无载荷；带载荷变体为 变体名(类型, ...)，构造 = 枚举名.变体名(实参...)，
    /// 匹配经 match 变体模式（Pattern::Variant）。
    EnumDef {
        name: String,
        variants: Vec<EnumVariant>,
        span: Span,
    },
    /// async fn 名称(参数) { ... }  异步函数定义。
    /// 与 fn 同构，但调用时在后台线程执行并立即返回 future，经 `await` 等待结果。
    AsyncFnDef {
        name: String,
        /// 泛型类型参数（async fn name[T](...)）
        type_params: Vec<String>,
        params: Vec<Param>,
        ret: Option<TyName>,
        body: Vec<Stmt>,
        span: Span,
    },
    /// with 上下文管理器：with expr [as r] { ... }
    /// 进入时调 target 的 `__enter__()`（其返回值绑定 r，r 仅块内可见），
    /// 退出时（含报错）必调 `__exit__()` 清理；__exit__ 不能吞掉块内错误。
    /// 协议按鸭子类型查找：type 实例走 类型.__enter__(实例)；内置 Ptr 句柄（如 sqlite）运行时特判。
    With {
        target: Box<Expr>,
        /// `as r` 绑定的变量名（None = 只走协议不绑定）
        var: Option<String>,
        body: Vec<Stmt>,
        span: Span,
    },
    /// type 实例类定义：type 名称 [extends 父类] { [readonly] 字段: 类型; fn 方法(self, ...) { ... } }
    /// 与 struct 的区别：带行为（方法）；与 class 的区别：class 是静态命名空间（无实例），
    /// type 有实例对象（new 构造）。方法首参约定为 self（显式首参，调用时自动填充实例）；
    /// 特殊方法 init(参数...)：构造时自动调用，不允许 return 值。
    /// 继承：单继承（extends），字段/方法沿继承链合并，子类方法覆盖父类。
    TypeDef {
        name: String,
        /// 父类型名（单继承；None = 根类型）
        base: Option<String>,
        /// (字段名, 类型, 是否只读)；继承合并时同名子类覆盖父类
        fields: Vec<(String, TyName, bool)>,
        /// 方法（每个元素为 FnDef，首参名为 self）
        methods: Vec<Stmt>,
        span: Span,
    },
    /// 标签定义：`label NAME;` 或 `NAME:`（作为位置标记，供同函数内的 `goto` 跳转）。
    /// 标签不产生任何运行期动作，只标记语句序列中的一个位置。
    Label {
        name: String,
        span: Span,
    },
    /// 无条件跳转：`goto NAME;`（只能跳到同一函数内、且从跳转点向外层可见的标签）。
    Goto {
        name: String,
        span: Span,
    },
    /// 宏定义：`macro NAME(参数...) => 表达式;`（表达式宏）
    ///        `macro NAME(参数...) { 语句... }`（语句宏，展开为独立作用域块）
    /// 宏在解析后被预处理阶段完全展开（AST 级替换），运行期不存在宏。
    MacroDef {
        name: String,
        /// 形参（仅用名字；ty/default 恒为空，宏参数是纯语法替换）
        params: Vec<Param>,
        body: MacroBody,
        span: Span,
    },
}

/// 宏体：表达式宏（可嵌入表达式位置）或语句宏（展开为独立作用域的语句序列）。
#[derive(Debug, Clone)]
pub enum MacroBody {
    /// `macro NAME(a) => a * 2;`
    Expr(Expr),
    /// `macro NAME(a) { print(a); }`
    Stmts(Vec<Stmt>),
}

/// enum 变体：名称 + 可选载荷字段类型（空 = 简单变体）。
#[derive(Debug, Clone)]
pub struct EnumVariant {
    pub name: String,
    pub payload: Vec<TyName>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub ty: Option<TyName>,
    pub span: Span,
    /// 默认参数值：fn f(a = 10, b = "x")  调用时可省略尾部实参。
    /// 默认表达式在调用时求值，可引用其前面的参数（如 b = a * 2）。
    pub default: Option<Expr>,
    /// 只读参数：`fn f(x: readonly int)` 体内不可重新赋值
    pub readonly: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TyName {
    Int,
    Float,
    Bool,
    Str,
    Char,
    /// 单个字节值（0-255），字面量 `0b01000001`；与 int 严格隔离
    Byte,
    /// 字节序列，字面量 `b"..."`
    Bytes,
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
}

/// load 签名块中的函数签名：fn name(p: ty, ...) -> ret;
#[derive(Debug, Clone)]
pub struct FfiSig {
    pub name: String,
    pub params: Vec<FfiParam>,
    pub ret: FfiTy,
    /// 头文件解析失败的原型（如回调/变参/数组），调用时直接报错而非 ABI 崩溃
    pub unsupported: Option<&'static str>,
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
    /// 字符字面量 'a'（词法层已校验恰好一个 Unicode 字符）
    CharLit(char, Span),
    /// 字节字面量 0b01000001（词法层已校验最多 8 位，值域 0-255）
    ByteLit(u8, Span),
    /// 字节序列字面量 b"..."
    BytesLit(Vec<u8>, Span),
    /// 标识符；模块函数经点号合并为完整名（如 "time.now"）
    Ident { name: String, span: Span },
    /// 列表字面量 [a, b, c]
    ListLit(Vec<Expr>, Span),
    /// 字典字面量 {"key": value, ...}（键为字符串）
    DictLit(Vec<(String, Expr)>, Span),
    /// 列表推导式 [elem for x in iter [if cond]]：对 iter 逐元素求 elem，可选 if 过滤。
    /// var2 为字典遍历时的值变量（for k, v in dict）。
    ListComp {
        elem: Box<Expr>,
        var: String,
        var2: Option<String>,
        iter: Box<Expr>,
        cond: Option<Box<Expr>>,
        span: Span,
    },
    /// 字典推导式 {key: value for k, v in iter [if cond]}：键/值表达式按迭代元素求值，键必须为 str。
    DictComp {
        key: Box<Expr>,
        value: Box<Expr>,
        var: String,
        var2: Option<String>,
        iter: Box<Expr>,
        cond: Option<Box<Expr>>,
        span: Span,
    },
    /// 插值字符串 f"..."：文字段与代码段交替（代码段已解析为表达式）
    FStr(Vec<FStrSeg>, Span),
    /// 字段访问：obj.field（如 e.code、e.message）
    Field { obj: Box<Expr>, field: String, span: Span },
    /// 可选链字段访问：obj?.field（obj 为 null 时短路返回 null，否则同 Field）
    OptionalField { obj: Box<Expr>, field: String, span: Span },
    /// 索引访问：a[i]（列表按下标取元素；下标越界/非列表在运行时报错）
    Index { obj: Box<Expr>, index: Box<Expr>, span: Span },
    /// 切片访问：a[i:j]（半开区间 [i, j)；端点可省略 a[:j] / a[i:] / a[:]；越界自动截断不报错）
    /// list 返回 list 副本；bytes 返回 bytes 副本
    Slice {
        obj: Box<Expr>,
        lo: Option<Box<Expr>>,
        hi: Option<Box<Expr>>,
        span: Span,
    },
    Unary { op: UnOp, expr: Box<Expr>, span: Span },
    Binary { op: BinOp, lhs: Box<Expr>, rhs: Box<Expr>, span: Span },
    Call { callee: String, args: Vec<Expr>, span: Span },
    /// match 表达式 { 模式 => 表达式, ..., _ => 默认值 }  模式匹配，返回匹配分支的值。
    /// 模式为 Pattern：字面量 / 枚举变体（可绑定载荷）/ `_` 通配符。
    Match {
        value: Box<Expr>,
        arms: Vec<(Pattern, Expr)>,
        span: Span,
    },
    /// 实例方法调用：obj.method(arg, ...)（receiver 为任意表达式，如 a[i].m()、new T(1).m()）
    /// 求值：receiver 须为 type 实例；方法按 类型.method 沿继承链查找，实例自动作为首参（self）传入
    MethodCall {
        obj: Box<Expr>,
        name: String,
        args: Vec<Expr>,
        span: Span,
    },
    /// 实例构造：new Type(arg, ...)（Type 须为已定义的 type；自动调用 init 方法）
    New {
        ty: String,
        args: Vec<Expr>,
        span: Span,
    },
    /// 自增/自减表达式：i++ / i-- / ++i / --i（仅作用于已声明的变量名）
    IncDec {
        op: IncOp,
        prefix: bool,
        name: String,
        span: Span,
    },
    /// 三元表达式：cond ? then_expr : else_expr
    Ternary {
        cond: Box<Expr>,
        then_expr: Box<Expr>,
        else_expr: Box<Expr>,
        span: Span,
    },
    /// 匿名函数（lambda）：fn(param1, param2) { ... }
    /// 一等值：可赋值给变量、作为参数传递、作为返回值；
    /// 创建时按值捕获当前作用域的变量（闭包），调用经变量名进行。
    Lambda {
        params: Vec<Param>,
        body: Vec<Stmt>,
        span: Span,
    },
    /// await 表达式：await expr  阻塞等待 expr（async 函数调用的 future）完成并返回其结果。
    Await {
        expr: Box<Expr>,
        span: Span,
    },
}

/// match 模式：字面量 / 枚举变体 / `_` 通配符。
#[derive(Debug, Clone)]
pub enum Pattern {
    /// 字面量模式（int / float / bool / str 字面量）
    Lit(Expr),
    /// 枚举变体模式：`Color.Red`（无载荷）或 `Shape.Circle(r)`（带载荷，binds 绑定载荷到变量）。
    /// binds 与变体载荷一一对应：Some(name) 绑定到分支体变量，None 表示 `_` 忽略。
    Variant {
        enum_name: String,
        variant: String,
        binds: Vec<Option<String>>,
        span: Span,
    },
    /// `_` 通配符（匹配任意值）
    Wildcard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncOp {
    Inc,
    Dec,
}

impl IncOp {
    pub fn symbol(&self) -> &'static str {
        match self {
            IncOp::Inc => "++",
            IncOp::Dec => "--",
        }
    }
}

/// 复合赋值运算符：+= -= *= /= %=
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompoundOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

impl CompoundOp {
    pub fn symbol(&self) -> &'static str {
        match self {
            CompoundOp::Add => "+=",
            CompoundOp::Sub => "-=",
            CompoundOp::Mul => "*=",
            CompoundOp::Div => "/=",
            CompoundOp::Mod => "%=",
        }
    }
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
    /// a ?? b：a 为 null 时取 b，否则取 a（空值合并）
    Coalesce,
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
            BinOp::Coalesce => "??",
        }
    }
}

pub fn expr_span(e: &Expr) -> Span {
    match e {
        Expr::IntLit(_, s)
        | Expr::FloatLit(_, s)
        | Expr::BoolLit(_, s)
        | Expr::StrLit(_, s)
        | Expr::CharLit(_, s)
        | Expr::ByteLit(_, s)
        | Expr::BytesLit(_, s)
        | Expr::ListLit(_, s)
        | Expr::DictLit(_, s)
        | Expr::ListComp { span: s, .. }
        | Expr::DictComp { span: s, .. }
        | Expr::FStr(_, s)
        | Expr::Ident { span: s, .. }
        | Expr::Field { span: s, .. }
        | Expr::OptionalField { span: s, .. }
        | Expr::Index { span: s, .. }
        | Expr::Slice { span: s, .. }
        | Expr::Call { span: s, .. }
        | Expr::Unary { span: s, .. }
        | Expr::Binary { span: s, .. }
        | Expr::Match { span: s, .. }
        | Expr::MethodCall { span: s, .. }
        | Expr::New { span: s, .. }
        | Expr::IncDec { span: s, .. }
        | Expr::Ternary { span: s, .. }
        | Expr::Lambda { span: s, .. }
        | Expr::Await { span: s, .. } => *s,
    }
}

impl Stmt {
    /// 返回语句自身携带的源码位置（各变体均含 `span` 字段）。
    pub fn span(&self) -> Span {
        match self {
            Stmt::Assign { span, .. }
            | Stmt::IndexAssign { span, .. }
            | Stmt::DestructAssign { span, .. }
            | Stmt::AssignOp { span, .. }
            | Stmt::FieldAssign { span, .. }
            | Stmt::VarDecl { span, .. }
            | Stmt::Block { span, .. }
            | Stmt::If { span, .. }
            | Stmt::While { span, .. }
            | Stmt::DoWhile { span, .. }
            | Stmt::ForC { span, .. }
            | Stmt::ForIn { span, .. }
            | Stmt::Return { span, .. }
            | Stmt::Break { span, .. }
            | Stmt::Continue { span, .. }
            | Stmt::FnDef { span, .. }
            | Stmt::DebugPrint { span, .. }
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
            | Stmt::ClassDef { span, .. }
            | Stmt::EnumDef { span, .. }
            | Stmt::TypeDef { span, .. }
            | Stmt::With { span, .. }
            | Stmt::AsyncFnDef { span, .. }
            | Stmt::Label { span, .. }
            | Stmt::Goto { span, .. }
            | Stmt::MacroDef { span, .. } => *span,
        }
    }
}
