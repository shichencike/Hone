// interp.rs - Hone 树遍历解释器
// 支持：作用域、用户函数（扁平化全局符号表）、go 多线程（std::thread）、
//       breakpoint 断点快照（hone debug 模式）、递归深度限制（H012）。
// 类型锁定由 checker 静态保证，解释器专注求值。

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::io::{self, Write};
use std::os::raw::c_char;
use std::sync::{Arc, Condvar, Mutex};

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

/// 匿名函数值（lambda）：参数 + 函数体 + 创建时按值捕获的环境快照（闭包）。
/// 捕获 env 按作用域外→内合并（内层同名覆盖外层），与 Env::get 语义一致。
#[derive(Debug, Clone)]
pub struct LambdaVal {
    pub params: Vec<Param>,
    pub body: Vec<Stmt>,
    pub captured: HashMap<String, Value>,
}

#[derive(Debug, Clone)]
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
    /// 匿名函数值（lambda / 闭包）
    Lambda(Arc<LambdaVal>),
    /// 枚举值（enum 类型实例：Color.Red / Shape.Circle(1.5)）。ty 为枚举名，payload 为变体载荷。
    Enum(Arc<EnumVal>),
    /// async 函数调用的 future：await 等待结果
    Future(Arc<FutureVal>),
}

/// 枚举值内容：枚举名 + 变体名 + 可选载荷（简单变体载荷为空）。
#[derive(Debug, Clone)]
pub struct EnumVal {
    pub ty: String,
    pub variant: String,
    pub payload: Vec<Value>,
}

/// async 函数调用的 future：后台线程执行，`await` 阻塞等待结果。
#[derive(Debug)]
pub struct FutureVal {
    state: Mutex<FutureState>,
    cv: Condvar,
}

#[derive(Debug)]
enum FutureState {
    Pending,
    Done(Result<Value, ZError>),
}

impl FutureVal {
    fn new() -> Arc<Self> {
        Arc::new(FutureVal {
            state: Mutex::new(FutureState::Pending),
            cv: Condvar::new(),
        })
    }

    /// 后台线程完成后写入结果并唤醒等待者。
    fn complete(&self, result: Result<Value, ZError>) {
        let mut s = self.state.lock().unwrap();
        *s = FutureState::Done(result);
        self.cv.notify_one();
    }

    /// 阻塞等待结果（错误原样传播，ZError 带子线程执行位置）。
    fn wait(&self) -> Result<Value, ZError> {
        let mut s = self.state.lock().unwrap();
        loop {
            match &*s {
                FutureState::Done(r) => return r.clone(),
                FutureState::Pending => s = self.cv.wait(s).unwrap(),
            }
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::List(a), Value::List(b)) => a == b,
            (Value::Dict(a), Value::Dict(b)) => a == b,
            (Value::Null, Value::Null) => true,
            (Value::Error(a), Value::Error(b)) => a == b,
            (Value::Ptr(a), Value::Ptr(b)) => a == b,
            // lambda 无结构性相等：同一创建点的引用视为相等
            (Value::Lambda(a), Value::Lambda(b)) => Arc::ptr_eq(a, b),
            // 枚举值：类型 + 变体 + 载荷全部相等才相等
            (Value::Enum(a), Value::Enum(b)) => a.ty == b.ty && a.variant == b.variant && a.payload == b.payload,
            // future 无结构相等：同一创建点的引用视为相等
            (Value::Future(a), Value::Future(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
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
            Value::Lambda(_) => "fn",
            Value::Enum(_) => "enum",
            Value::Future(_) => "future",
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
            Value::Lambda(f) => format!("fn({})", f.params.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")),
            // 枚举显示：Color.Red / Shape.Circle(1.5, 2.5)
            Value::Enum(e) => {
                if e.payload.is_empty() {
                    format!("{}.{}", e.ty, e.variant)
                } else {
                    let inner: Vec<String> = e.payload.iter().map(|v| v.display()).collect();
                    format!("{}.{}({})", e.ty, e.variant, inner.join(", "))
                }
            }
            Value::Future(_) => "future".to_string(),
        }
    }
}

/// 语句执行流程：Normal 继续；Return 携带返回值向上传播；Break 跳出最近循环；
/// Continue 跳过本次循环剩余语句，进入下一次迭代。
pub enum Flow {
    Normal,
    Return(Value),
    Break,
    Continue,
}

#[derive(Clone)]
struct FnDef {
    params: Vec<Param>,
    body: Vec<Stmt>,
}

pub struct Env {
    scopes: Vec<HashMap<String, Value>>,
}

impl Env {
    pub fn new() -> Self {
        Env {
            scopes: vec![HashMap::new()],
        }
    }

    /// REPL 用：列出当前作用域已定义变量（名 → 显示值），按名排序。
    pub fn vars(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = self
            .scopes
            .last()
            .map(|m| m.iter().map(|(k, val)| (k.clone(), val.display())).collect())
            .unwrap_or_default();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    fn get(&self, name: &str) -> Option<&Value> {
        for s in self.scopes.iter().rev() {
            if let Some(v) = s.get(name) {
                return Some(v);
            }
        }
        None
    }

    /// 赋值：找到最近绑定则原地更新（避免 String 分配与二次哈希），否则在当前作用域声明。
    fn set_or_declare(&mut self, name: &str, v: Value) {
        for s in self.scopes.iter_mut().rev() {
            if let Some(slot) = s.get_mut(name) {
                *slot = v;
                return;
            }
        }
        self.scopes.last_mut().unwrap().insert(name.to_string(), v);
    }

    fn declare(&mut self, name: &str, v: Value) {
        self.scopes.last_mut().unwrap().insert(name.to_string(), v);
    }
}

/// profiler 单函数统计：调用次数、累计耗时（纳秒）、自耗时（不含子调用，纳秒）。
#[derive(Clone, Copy, Default)]
struct ProfEntry {
    calls: u64,
    total_ns: u128,
    self_ns: u128,
}

/// profiler 调用栈帧：入口时间 + 子调用累计耗时，用于计算自耗时（独占时间）。
struct ProfFrame {
    name: String,
    start: std::time::Instant,
    children_ns: u128,
}

/// hone prof 收集到的函数级剖析数据（可按总耗时降序排序，含调用图）。
pub struct ProfData {
    entries: HashMap<String, ProfEntry>,
    /// 调用图边：调用方 → 被调方 → 次数
    edges: HashMap<(String, String), u64>,
}

impl ProfData {
    fn from_map(entries: HashMap<String, ProfEntry>, edges: HashMap<(String, String), u64>) -> Self {
        ProfData { entries, edges }
    }

    /// 是否有任何用户函数被调用。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 按总耗时降序返回 (函数名, 调用次数, 总耗时纳秒, 平均耗时纳秒, 自耗时纳秒)。
    pub fn sorted(&self) -> Vec<(String, u64, u128, u128, u128)> {
        let mut v: Vec<(String, u64, u128, u128, u128)> = self
            .entries
            .iter()
            .map(|(k, e)| {
                let avg = if e.calls > 0 { e.total_ns / e.calls as u128 } else { 0 };
                (k.clone(), e.calls, e.total_ns, avg, e.self_ns)
            })
            .collect();
        v.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| b.1.cmp(&a.1)));
        v
    }

    /// 所有函数的累计总耗时（嵌套调用重复计入，用于占比分母）。
    pub fn total_ns(&self) -> u128 {
        self.entries.values().map(|e| e.total_ns).sum()
    }

    /// 所有函数的累计调用次数。
    pub fn total_calls(&self) -> u64 {
        self.entries.values().map(|e| e.calls).sum()
    }

    /// 调用图边（调用方 → 被调方 → 次数），按次数降序。
    pub fn edges(&self) -> Vec<(String, String, u64)> {
        let mut v: Vec<(String, String, u64)> = self
            .edges
            .iter()
            .map(|((a, b), c)| (a.clone(), b.clone(), *c))
            .collect();
        v.sort_by(|x, y| y.2.cmp(&x.2));
        v
    }
}

pub struct Interp {
    pub file: String,
    pub src: String,
    /// 函数定义用 Arc 共享：每次调用只克隆引用计数，避免深拷贝整个函数体 AST
    fns: HashMap<String, Arc<FnDef>>,
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
    /// 枚举定义：名称 → 变体名列表（变体访问/构造/匹配时校验存在性）
    enums: HashMap<String, Vec<String>>,
    /// 异步函数名集合（async fn 定义）：调用时后台线程执行并返回 future
    async_fns: HashSet<String>,
    /// 类定义：类名 → (方法名 → FnDef)。成员函数不进全局 fns 表，
    /// 只能经 `类.方法(...)` 调用（call_fn 按限定名解析）。
    classes: HashMap<String, HashMap<String, Arc<FnDef>>>,
    /// profiler 统计表（None = 未启用剖析，减少热路径开销）
    prof: Option<HashMap<String, ProfEntry>>,
    /// REPL 最近一次表达式语句的值（Python 式回显用）
    last_expr: Option<Value>,
    /// 调试模式：监视变量名（每次断点自动打印当前值）
    watch: Vec<String>,
    /// profiler 调用栈（prof 模式）：入口时间 + 子调用累计，用于自耗时
    prof_stack: Vec<ProfFrame>,
    /// profiler 调用图边（调用方 → 被调方 → 次数）
    prof_edges: HashMap<(String, String), u64>,
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
    run_impl(program, file, src, debug, false).map(|_| ())
}

/// 以 profiler 模式运行脚本：返回函数级剖析数据（总耗时 / 调用次数 / 平均耗时）。
pub fn run_prof(program: &Program, file: &str, src: &str) -> Result<ProfData, ZError> {
    run_impl(program, file, src, false, true)?
        .ok_or_else(|| ZError::plain(codes::SYSCALL, "profiler data unavailable", None::<&str>))
}

fn run_impl(
    program: &Program,
    file: &str,
    src: &str,
    debug: bool,
    prof: bool,
) -> Result<Option<ProfData>, ZError> {
    let mut ip = Interp::new(file, src, debug);
    if prof {
        ip.prof = Some(HashMap::new());
    }
    ip.collect_fns(&program.stmts)?;
    ip.collect_structs(&program.stmts);
    ip.collect_enums(&program.stmts);
    ip.collect_classes(&program.stmts);
    let mut env = Env::new();
    let exec = ip.exec_stmts(&mut env, &program.stmts);
    // 无论执行结果如何都取出剖析数据（DEBUG_QUIT 也需返回）
    let data = ip
        .prof
        .take()
        .map(|p| ProfData::from_map(p, std::mem::take(&mut ip.prof_edges)));
    match exec {
        Ok(_) => {}
        // 调试器用户主动退出：正常结束，不打印错误
        Err(e) if e.code == DEBUG_QUIT => return Ok(data),
        Err(e) => return Err(e),
    }
    Ok(data)
}

/// 调试器用户主动退出时使用的特殊错误码（run_impl 捕获后正常结束，不打印错误）。
const DEBUG_QUIT: &'static str = "H909";

impl Interp {
    /// 新建解释器（REPL 复用：跨输入保持函数表与已加载库等状态）。
    pub fn new(file: &str, src: &str, debug: bool) -> Self {
        Interp {
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
            enums: HashMap::new(),
            async_fns: HashSet::new(),
            classes: HashMap::new(),
            prof: None,
            last_expr: None,
            watch: Vec::new(),
            prof_stack: Vec::new(),
            prof_edges: HashMap::new(),
        }
    }

    /// REPL 用：最近一次表达式语句的值（Python 式回显）。
    pub fn last_expr(&self) -> Option<&Value> {
        self.last_expr.as_ref()
    }

    /// REPL 用：当前已注册的用户函数名（.vars 展示用）。
    pub fn fn_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.fns.keys().cloned().collect();
        v.sort();
        v
    }

    /// REPL 用：开始执行新一轮输入前清空回显值。
    pub fn reset_last_expr(&mut self) {
        self.last_expr = None;
    }

    /// 收集所有函数定义（含嵌套，扁平化注册；解释执行时 FnDef 语句为 no-op）。
    pub fn collect_fns(&mut self, stmts: &[Stmt]) -> Result<(), ZError> {
        for stmt in stmts {
            match stmt {
                Stmt::FnDef { name, params, body, tmp, .. } => {
                    if !tmp {
                        self.fns.insert(
                            name.clone(),
                            Arc::new(FnDef {
                                params: params.clone(),
                                body: body.clone(),
                            }),
                        );
                    }
                }
                Stmt::AsyncFnDef { name, params, body, .. } => {
                    // 异步函数：注册到 fns（函数体可调用），并登记到 async_fns（调用时后台执行）
                    self.fns.insert(
                        name.clone(),
                        Arc::new(FnDef {
                            params: params.clone(),
                            body: body.clone(),
                        }),
                    );
                    self.async_fns.insert(name.clone());
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
    pub fn collect_structs(&mut self, stmts: &[Stmt]) {
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

    /// 收集所有枚举定义（含嵌套），扁平化注册；解释执行时 EnumDef 语句为 no-op。
    pub fn collect_enums(&mut self, stmts: &[Stmt]) {
        for stmt in stmts {
            match stmt {
                Stmt::EnumDef { name, variants, .. } => {
                    let names: Vec<String> = variants.iter().map(|v| v.name.clone()).collect();
                    self.enums.insert(name.clone(), names);
                }
                Stmt::Block { stmts, .. } => self.collect_enums(stmts),
                Stmt::If { then_branch, else_branch, .. } => {
                    self.collect_enums(then_branch);
                    if let Some(eb) = else_branch {
                        self.collect_enums(eb);
                    }
                }
                Stmt::While { body, .. } => self.collect_enums(body),
                Stmt::ForIn { body, .. } => self.collect_enums(body),
                Stmt::Try { body, handler, .. } => {
                    self.collect_enums(body);
                    self.collect_enums(handler);
                }
                _ => {}
            }
        }
    }

    /// 收集所有类定义（含嵌套），注册到 classes 表（类名 → 方法名 → FnDef）。
    /// 成员函数不进入全局 fns 表，只能经 `类.方法(...)` 调用。
    pub fn collect_classes(&mut self, stmts: &[Stmt]) {
        for stmt in stmts {
            match stmt {
                Stmt::ClassDef { name, methods, .. } => {
                    let mut methods_map: HashMap<String, Arc<FnDef>> = HashMap::new();
                    for m in methods {
                        if let Stmt::FnDef { name: mname, params, body, tmp, .. } = m {
                            if !tmp {
                                methods_map.insert(
                                    mname.clone(),
                                    Arc::new(FnDef {
                                        params: params.clone(),
                                        body: body.clone(),
                                    }),
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

    pub fn exec_stmts(&mut self, env: &mut Env, stmts: &[Stmt]) -> Result<Flow, ZError> {
        for s in stmts {
            match self.exec_stmt(env, s)? {
                Flow::Return(v) => return Ok(Flow::Return(v)),
                Flow::Break => return Ok(Flow::Break),
                Flow::Continue => return Ok(Flow::Continue),
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
            Stmt::IndexAssign { target, value, span } => {
                // 索引赋值：沿索引链更新容器（列表为值类型，克隆后写回基变量）
                let v = self.eval_expr(env, value)?;
                self.set_index_value(env, target, v, *span)?;
                Ok(Flow::Normal)
            }
            Stmt::DestructAssign { targets, value, span } => {
                let v = self.eval_expr(env, value)?;
                let is_dict = targets.iter().all(|(_, k)| k.is_some());
                match v {
                    // 列表解构：按位置依次绑定
                    Value::List(items) => {
                        if is_dict {
                            return Err(self.runtime_err(
                                codes::TYPE_MISMATCH,
                                "dict destructuring `{...} =` requires a dict value, got a list",
                                *span,
                                Some("use `a, b = list` for list destructuring"),
                            ));
                        }
                        if items.len() < targets.len() {
                            return Err(self.runtime_err(
                                codes::TYPE_MISMATCH,
                                format!(
                                    "destructuring a list of {} element(s) into {} variable(s)",
                                    items.len(),
                                    targets.len()
                                ),
                                *span,
                                Some("the list must have at least as many elements as the variables"),
                            ));
                        }
                        for (i, (name, _)) in targets.iter().enumerate() {
                            env.set_or_declare(name, items[i].clone());
                        }
                        Ok(Flow::Normal)
                    }
                    // 字典解构：按键取出
                    Value::Dict(entries) => {
                        if !is_dict {
                            return Err(self.runtime_err(
                                codes::TYPE_MISMATCH,
                                "list destructuring `a, b =` requires a list value, got a dict",
                                *span,
                                Some("use `{a, b} = dict` for dict destructuring"),
                            ));
                        }
                        for (name, key) in targets {
                            let key = key.as_deref().expect("dict destructure target carries a key");
                            match entries.iter().find(|(k, _)| k == key) {
                                Some((_, val)) => env.set_or_declare(name, val.clone()),
                                None => {
                                    return Err(self.runtime_err(
                                        codes::UNDEFINED,
                                        format!("dict has no key `{}` for destructuring", key),
                                        *span,
                                        Some("check the key name, or the dict value being destructured"),
                                    ))
                                }
                            }
                        }
                        Ok(Flow::Normal)
                    }
                    other => Err(self.runtime_err(
                        codes::TYPE_MISMATCH,
                        format!(
                            "destructuring requires a list or dict value, got `{}`",
                            other.type_name()
                        ),
                        *span,
                        Some("destructure a list with `a, b = [..]` or a dict with `{a, b} = dict`"),
                    )),
                }
            }
            Stmt::AssignOp { name, op, value, span } => {
                let cur = env.get(name).cloned();
                match cur {
                    Some(cur) => {
                        let rhs = self.eval_expr(env, value)?;
                        let v = self.compound_assign(*op, cur, rhs, *span)?;
                        env.set_or_declare(name, v);
                        Ok(Flow::Normal)
                    }
                    None => Err(self.runtime_err(
                        codes::UNDEFINED,
                        format!("undefined variable `{}` (declare it before `{}` assignment)", name, op.symbol()),
                        *span,
                        Some("use `x = value` to declare and initialize"),
                    )),
                }
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
                // 复用同一个作用域 HashMap：热循环每轮迭代避免一次堆分配
                let mut body_scope = HashMap::new();
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
                    env.scopes.push(body_scope);
                    let flow = self.exec_stmts(env, body);
                    body_scope = env.scopes.pop().expect("while body scope");
                    // 复用作用域必须清空：否则上一轮迭代体内声明的变量会泄漏到下一轮
                    body_scope.clear();
                    match flow? {
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        Flow::Break => break,
                        Flow::Continue => {} // 跳过剩余语句，进入下一次迭代
                        Flow::Normal => {}
                    }
                }
                Ok(Flow::Normal)
            }
            Stmt::DoWhile { body, cond, .. } => {
                // do-while：先执行一次循环体，再判断条件
                let mut body_scope = HashMap::new();
                loop {
                    env.scopes.push(body_scope);
                    let flow = self.exec_stmts(env, body);
                    body_scope = env.scopes.pop().expect("do-while body scope");
                    body_scope.clear();
                    match flow? {
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        Flow::Break => break,
                        Flow::Continue => {} // 跳过剩余语句，直接判断条件
                        Flow::Normal => {}
                    }
                    let c = self.eval_expr(env, cond)?;
                    if let Value::Bool(b) = c {
                        if !b {
                            break;
                        }
                    } else {
                        return Err(self.runtime_err(
                            codes::TYPE_MISMATCH,
                            format!("do-while condition must be `bool`, got `{}`", c.type_name()),
                            expr_span(cond),
                            None::<&str>,
                        ));
                    }
                }
                Ok(Flow::Normal)
            }
            Stmt::ForC { init, cond, step, body, .. } => {
                // init 在循环外作用域执行一次；cond/step 与 while 同语义；
                // 循环体复用单个作用域 HashMap
                if let Some(i) = init {
                    match self.exec_stmt(env, i)? {
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        Flow::Break | Flow::Continue | Flow::Normal => {}
                    }
                }
                let mut body_scope = HashMap::new();
                loop {
                    if let Some(c) = cond {
                        let cval = self.eval_expr(env, c)?;
                        if let Value::Bool(b) = cval {
                            if !b {
                                break;
                            }
                        } else {
                            return Err(self.runtime_err(
                                codes::TYPE_MISMATCH,
                                format!("for condition must be `bool`, got `{}`", cval.type_name()),
                                expr_span(c),
                                None::<&str>,
                            ));
                        }
                    }
                    env.scopes.push(body_scope);
                    let flow = self.exec_stmts(env, body);
                    body_scope = env.scopes.pop().expect("for body scope");
                    body_scope.clear();
                    match flow? {
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        Flow::Break => break,
                        Flow::Continue => {} // 跳过剩余语句，执行 step 后进入下一轮
                        Flow::Normal => {}
                    }
                    if let Some(s) = step {
                        match self.exec_stmt(env, s)? {
                            Flow::Return(v) => return Ok(Flow::Return(v)),
                            Flow::Break => break,
                            Flow::Continue | Flow::Normal => {}
                        }
                    }
                }
                Ok(Flow::Normal)
            }
            Stmt::ForIn { var, var2, iter, body, span } => {
                let it = self.eval_expr(env, iter)?;
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
                        // 复用同一个作用域 HashMap：每轮迭代避免一次堆分配
                        let mut body_scope = HashMap::new();
                        for item in items {
                            env.scopes.push(body_scope);
                            env.declare(var, item);
                            let flow = self.exec_stmts(env, body);
                            body_scope = env.scopes.pop().expect("for-in scope");
                            // 复用作用域必须清空：否则上一轮迭代体内声明的变量会泄漏到下一轮
                            body_scope.clear();
                            match flow? {
                                Flow::Return(v) => return Ok(Flow::Return(v)),
                                Flow::Break => break,
                                Flow::Continue => {} // 跳过剩余语句，进入下一次迭代
                                Flow::Normal => {}
                            }
                        }
                        Ok(Flow::Normal)
                    }
                    // 字典：var=键，var2=值（可选）
                    Value::Dict(entries) => {
                        let mut body_scope = HashMap::new();
                        for (k, v) in entries {
                            env.scopes.push(body_scope);
                            env.declare(var, Value::Str(k));
                            if let Some(v2) = var2 {
                                env.declare(v2, v);
                            }
                            let flow = self.exec_stmts(env, body);
                            body_scope = env.scopes.pop().expect("for-in scope");
                            // 复用作用域必须清空：否则上一轮迭代体内声明的变量会泄漏到下一轮
                            body_scope.clear();
                            match flow? {
                                Flow::Return(v) => return Ok(Flow::Return(v)),
                                Flow::Break => break,
                                Flow::Continue => {} // 跳过剩余语句，进入下一次迭代
                                Flow::Normal => {}
                            }
                        }
                        Ok(Flow::Normal)
                    }
                    other => Err(self.runtime_err(
                        codes::TYPE_MISMATCH,
                        format!("`for in` requires a list or dict, got `{}`", other.type_name()),
                        expr_span(iter),
                        Some("iterate a list with `for x in list` or a dict with `for k, v in dict`"),
                    )),
                }
            }
            Stmt::Return { values, .. } => {
                // 多返回值：return a, b, ...; 打包为列表，由解构赋值 `a, b = f()` 接收
                if values.len() > 1 {
                    let mut packed = Vec::new();
                    for e in values {
                        packed.push(self.eval_expr(env, e)?);
                    }
                    Ok(Flow::Return(Value::List(packed)))
                } else {
                    let v = match values.first() {
                        Some(e) => self.eval_expr(env, e)?,
                        None => Value::Null,
                    };
                    Ok(Flow::Return(v))
                }
            }
            Stmt::FnDef { .. } => Ok(Flow::Normal), // 已扁平化注册
            Stmt::ExprStmt { expr, .. } => {
                let v = self.eval_expr(env, expr)?;
                // REPL 回显用：记录最近一次表达式语句的值
                self.last_expr = Some(v);
                Ok(Flow::Normal)
            }
            Stmt::Break { .. } => Ok(Flow::Break),
            Stmt::Continue { .. } => Ok(Flow::Continue),
            Stmt::Breakpoint { span, cond } => {
                if self.debug {
                    // 条件断点：仅当条件为 true 时暂停
                    let hit = match cond {
                        Some(c) => matches!(self.eval_expr(env, c)?, Value::Bool(true)),
                        None => true,
                    };
                    if hit {
                        self.do_breakpoint(env, *span)?;
                    }
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
            Stmt::EnumDef { .. } => Ok(Flow::Normal), // 已扁平化注册
            Stmt::AsyncFnDef { .. } => Ok(Flow::Normal), // 已扁平化注册
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

    /// 调试断点：打印位置、监视变量与变量快照，进入交互提示。
    /// 命令：Enter/c 继续、q 退出调试、l 重列变量、p <expr> 即时求值、
    ///       w <name> 监视变量、u <name> 取消监视、h 帮助。
    /// 用户选择退出时返回 Err(DEBUG_QUIT)，由 run_impl 捕获后正常结束。
    fn do_breakpoint(&mut self, env: &mut Env, span: Span) -> Result<(), ZError> {
        let mut show_snapshot = true;
        loop {
            println!("[Hone Debug] 断点触发 -> {}:{}", self.file, span.line);
            if !self.watch.is_empty() {
                println!("--- 监视变量 ---");
                for name in &self.watch {
                    match env.get(name) {
                        Some(v) => println!("{} : {} = {}", name, v.type_name(), v.display()),
                        None => println!("{} : (未定义)", name),
                    }
                }
            }
            if show_snapshot {
                println!("--- 变量快照 ---");
                let mut seen: HashSet<String> = HashSet::new();
                for scope in env.scopes.iter().rev() {
                    for (k, v) in scope {
                        if seen.insert(k.clone()) {
                            println!("{} : {} = {}", k, v.type_name(), v.display());
                        }
                    }
                }
                show_snapshot = false;
            }
            print!("[dbg] c=继续 q=退出 l=列表 p=<expr>求值 w=<name>监视 u=<name>取消 h=帮助> ");
            let _ = io::stdout().flush();
            let mut line = String::new();
            if io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
                return Ok(()); // EOF 视为继续
            }
            let cmd = line.trim();
            if cmd.is_empty() || cmd == "c" || cmd == "continue" {
                return Ok(());
            }
            match cmd {
                "q" | "quit" | "exit" => {
                    return Err(ZError::plain(DEBUG_QUIT, "debugger quit by user", None::<&str>))
                }
                "l" | "list" => {
                    show_snapshot = true;
                    continue;
                }
                "h" | "help" => {
                    println!("  Enter/c 继续执行；q 退出调试；l 重列变量快照；p <expr> 求值表达式；w <name> 监视变量；u <name> 取消监视");
                    continue;
                }
                _ => {}
            }
            if let Some(rest) = cmd.strip_prefix("p ") {
                match crate::parser::Parser::parse_expr_src("<dbg>", rest) {
                    Ok(expr) => match self.eval_expr(env, &expr) {
                        Ok(v) => println!("{} = {}", rest, v.display()),
                        Err(e) => println!("求值失败: {}: {}", e.code, e.msg),
                    },
                    Err(e) => println!("解析失败: {}: {}", e.code, e.msg),
                }
                continue;
            }
            if let Some(name) = cmd.strip_prefix("w ") {
                let name = name.trim().to_string();
                if name.is_empty() {
                    println!("用法: w <变量名>");
                } else if !self.watch.contains(&name) {
                    self.watch.push(name.clone());
                    println!("已加入监视: {}", name);
                } else {
                    println!("已在监视列表中: {}", name);
                }
                continue;
            }
            if let Some(name) = cmd.strip_prefix("u ") {
                let name = name.trim().to_string();
                if let Some(idx) = self.watch.iter().position(|x| *x == name) {
                    self.watch.remove(idx);
                    println!("已取消监视: {}", name);
                } else {
                    println!("未在监视列表: {}", name);
                }
                continue;
            }
            println!("未知命令 `{}`（h 查看帮助）", cmd);
        }
    }

    // ---------- go 多线程 ----------

    /// async 函数调用：后台线程执行（状态克隆与 go 一致），立即返回 future。
    /// 错误原样存入 future，由 `await` 阻塞等待并传播。
    fn exec_async_fn(&mut self, callee: &str, args: Vec<Value>, span: Span) -> Result<Value, ZError> {
        let fns = self.fns.clone();
        let alias_map = self.alias_map.clone();
        let lazy_libs = self.lazy_libs.clone();
        let structs = self.structs.clone();
        let enums = self.enums.clone();
        let async_fns = self.async_fns.clone();
        let classes = self.classes.clone();
        let file = self.file.clone();
        let src = self.src.clone();
        let callee = callee.to_string();
        let span = span;
        let future = FutureVal::new();
        let fut = future.clone();
        std::thread::spawn(move || {
            let mut t = Interp {
                file,
                src,
                fns,
                debug: false,
                depth: 0,
                libs: HashMap::new(),
                lazy_libs,
                ffi_sigs: HashMap::new(),
                alias_map,
                structs,
                enums,
                async_fns,
                classes,
                prof: None,
                last_expr: None,
                watch: Vec::new(),
                prof_stack: Vec::new(),
                prof_edges: HashMap::new(),
            };
            // 直接执行函数体（exec_user_fn）而非 call_fn：
            // call_fn 会再次命中 async 分支造成无限递归启动线程。
            // 函数体内调用其他 async 函数时仍走 call_fn 的 async 分支（嵌套线程，语义正确）。
            let result = if let Some(f) = t.fns.get(&callee).cloned() {
                t.exec_user_fn(&callee, &f, args, span)
            } else {
                t.call_fn(&callee, args, span)
            };
            fut.complete(result);
        });
        Ok(Value::Future(future))
    }

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
        let enums = self.enums.clone();
        let async_fns = self.async_fns.clone();
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
                enums,
                async_fns,
                classes,
                prof: None,
                last_expr: None,
                watch: Vec::new(),
                prof_stack: Vec::new(),
                prof_edges: HashMap::new(),
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
                        Arc::new(FnDef {
                            params: params.clone(),
                            body: body.clone(),
                        }),
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
            let header_sigs = crate::header::parse(&src);
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

    /// 运算符重载回退：调用顶层特殊函数 `__op(...)`（若已定义）。
    /// 仅在内建运算不支持的操作数组合时由运算符求值路径调用。
    fn call_overload(&mut self, name: &str, args: Vec<Value>, span: Span) -> Option<Result<Value, ZError>> {
        if self.fns.contains_key(name) {
            Some(self.call_fn(name, args, span))
        } else {
            None
        }
    }

    fn call_fn(&mut self, callee: &str, args: Vec<Value>, span: Span) -> Result<Value, ZError> {
        if self.async_fns.contains(callee) {
            // 异步函数调用：后台线程执行，返回 future（await 等待结果）
            self.exec_async_fn(callee, args, span)
        } else if let Some(f) = self.fns.get(callee).cloned() {
            self.exec_user_fn(callee, &f, args, span)
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
        } else if let Some(v) = self.try_enum_construct(callee, &args, span)? {
            // 枚举变体构造：Shape.Circle(1.5) → 枚举值
            Ok(v)
        } else if let Some(result) = self.call_class_method(callee, &args, span) {
            // 类方法（类名.方法名）：成员函数不进全局 fns 表，经此解析执行
            result
        } else if builtins::is_builtin(callee) {
            // len 重载：内建失败（值类型不支持）时回退 __len
            if callee == "len" && self.fns.contains_key("__len") {
                match builtins::call(callee, args.clone(), span, &self.file, &self.src) {
                    Ok(v) => return Ok(v),
                    Err(e) if e.code == codes::TYPE_MISMATCH => {
                        return match self.call_overload("__len", args, span) {
                            Some(r) => r,
                            None => Err(e),
                        };
                    }
                    Err(e) => return Err(e),
                }
            }
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

    /// 枚举变体构造：callee 形如 `Enum.Variant` 且枚举已注册 → 返回 Some(枚举值)；
    /// 非枚举限定名返回 None（由调用方继续解析为类方法/内置/模块函数）。
    fn try_enum_construct(&self, callee: &str, args: &[Value], span: Span) -> Result<Option<Value>, ZError> {
        let Some((enum_name, variant)) = callee.split_once('.') else {
            return Ok(None);
        };
        let Some(vs) = self.enums.get(enum_name) else {
            return Ok(None);
        };
        if !vs.iter().any(|v| v == variant) {
            return Err(self.runtime_err(
                codes::UNDEFINED,
                format!("enum `{}` has no variant `{}`", enum_name, variant),
                span,
                Some(format!("variants: {}", vs.join(", "))),
            ));
        }
        Ok(Some(Value::Enum(Arc::new(EnumVal {
            ty: enum_name.to_string(),
            variant: variant.to_string(),
            payload: args.to_vec(),
        }))))
    }

    /// 执行用户函数/类方法体：绑定参数到新环境，执行函数体，处理 return。
    fn exec_user_fn(&mut self, name: &str, f: &FnDef, args: Vec<Value>, span: Span) -> Result<Value, ZError> {
        if self.depth >= 5000 {
            return Err(self.runtime_err(
                codes::RECURSION_DEPTH,
                "recursion depth exceeded (limit 5000)",
                span,
                Some("check for infinite recursion, or rewrite iteratively"),
            ));
        }
        if args.len() > f.params.len() {
            return Err(self.runtime_err(
                codes::ARG_COUNT,
                format!("`{}` expects at most {} arguments, got {}", name, f.params.len(), args.len()),
                span,
                Some("check the number of arguments passed"),
            ));
        }
        let mut call_env = Env {
            scopes: vec![HashMap::with_capacity(f.params.len())],
        };
        for (i, p) in f.params.iter().enumerate() {
            let v = if i < args.len() {
                args[i].clone()
            } else if let Some(d) = &p.default {
                // 默认表达式在调用环境求值（可引用其前面的参数）
                self.eval_expr(&mut call_env, d)?
            } else {
                return Err(self.runtime_err(
                    codes::ARG_COUNT,
                    format!("missing argument `{}` of `{}`", p.name, name),
                    span,
                    Some("pass the required argument, or give the parameter a default value"),
                ));
            };
            call_env.declare(&p.name, v);
        }
        self.depth += 1;
        // profiler：记录调用图边并入栈；无论执行结果如何都必须出栈结算
        let prof_enabled = self.prof.is_some();
        if prof_enabled {
            if let Some(caller) = self.prof_stack.last() {
                *self
                    .prof_edges
                    .entry((caller.name.clone(), name.to_string()))
                    .or_insert(0) += 1;
            }
            self.prof_stack.push(ProfFrame {
                name: name.to_string(),
                start: std::time::Instant::now(),
                children_ns: 0,
            });
        }
        let flow = self.exec_stmts(&mut call_env, &f.body);
        self.depth -= 1;
        if prof_enabled {
            if let Some(frame) = self.prof_stack.pop() {
                let elapsed = frame.start.elapsed().as_nanos();
                if let Some(prof) = self.prof.as_mut() {
                    let entry = prof.entry(frame.name.clone()).or_default();
                    entry.calls += 1;
                    entry.total_ns += elapsed;
                    entry.self_ns += elapsed.saturating_sub(frame.children_ns);
                }
                // 把本次调用的墙钟耗时累计到父帧的子调用耗时（父帧此时在栈顶）
                if let Some(top) = self.prof_stack.last_mut() {
                    top.children_ns += elapsed;
                }
            }
        }
        match flow? {
            Flow::Return(v) => Ok(v),
            Flow::Normal => Ok(Value::Null),
            // checker 已保证 break/continue 只在循环体内，循环会捕获它们，
            // 因此正常情况下不会逃逸到函数体；兜底防内部不一致
            Flow::Break | Flow::Continue => Err(self.runtime_err(
                codes::SYNTAX,
                "loop control escaped a function body (internal error)",
                span,
                None::<&str>,
            )),
        }
    }

    /// 执行 lambda 值：环境 = 捕获快照（底层）+ 参数（上层，同名覆盖捕获值）。
    /// return 从 lambda 返回；break/continue 在 lambda 体内非法（checker 已保证），兜底报错。
    fn exec_lambda(&mut self, f: &LambdaVal, args: Vec<Value>, span: Span) -> Result<Value, ZError> {
        if self.depth >= 5000 {
            return Err(self.runtime_err(
                codes::RECURSION_DEPTH,
                "recursion depth exceeded (limit 5000)",
                span,
                Some("check for infinite recursion, or rewrite iteratively"),
            ));
        }
        if args.len() > f.params.len() {
            return Err(self.runtime_err(
                codes::ARG_COUNT,
                format!("lambda expects at most {} arguments, got {}", f.params.len(), args.len()),
                span,
                Some("pass exactly the declared number of arguments"),
            ));
        }
        let mut call_env = Env {
            scopes: vec![f.captured.clone(), HashMap::with_capacity(f.params.len())],
        };
        for (i, p) in f.params.iter().enumerate() {
            let v = if i < args.len() {
                args[i].clone()
            } else if let Some(d) = &p.default {
                // 默认表达式在调用环境求值（可引用前面的参数与捕获变量）
                self.eval_expr(&mut call_env, d)?
            } else {
                return Err(self.runtime_err(
                    codes::ARG_COUNT,
                    format!("missing argument `{}`", p.name),
                    span,
                    Some("pass the required argument, or give the parameter a default value"),
                ));
            };
            call_env.declare(&p.name, v);
        }
        self.depth += 1;
        let t0 = if self.prof.is_some() { Some(std::time::Instant::now()) } else { None };
        let flow = self.exec_stmts(&mut call_env, &f.body);
        self.depth -= 1;
        if let (Some(t0), Some(prof)) = (t0, self.prof.as_mut()) {
            // lambda 无名字，统一归入 "(lambda)" 条目
            let entry = prof.entry("(lambda)".to_string()).or_default();
            entry.calls += 1;
            entry.total_ns += t0.elapsed().as_nanos();
        }
        match flow? {
            Flow::Return(v) => Ok(v),
            Flow::Normal => Ok(Value::Null),
            Flow::Break | Flow::Continue => Err(self.runtime_err(
                codes::SYNTAX,
                "loop control escaped a lambda body (internal error)",
                span,
                None::<&str>,
            )),
        }
    }

    /// 自增/自减的值运算：int 增减 1，float 增减 1.0；其他类型报错。
    fn incdec_value(&self, op: IncOp, v: Value, span: Span) -> Result<Value, ZError> {
        let delta: i64 = if op == IncOp::Inc { 1 } else { -1 };
        match v {
            Value::Int(x) => match x.checked_add(delta) {
                Some(n) => Ok(Value::Int(n)),
                None => Err(self.runtime_err(
                    codes::INTEGER_OVERFLOW,
                    "integer overflow",
                    span,
                    Some("the result does not fit in a 64-bit signed integer"),
                )),
            },
            Value::Float(f) => Ok(Value::Float(if op == IncOp::Inc { f + 1.0 } else { f - 1.0 })),
            other => Err(self.runtime_err(
                codes::TYPE_MISMATCH,
                format!("`{}` requires a numeric variable, got `{}`", op.symbol(), other.type_name()),
                span,
                Some("increment/decrement works on `int` / `float` variables only"),
            )),
        }
    }

    /// 复合赋值运行语义：`x op= y` 等价于 `x = x op y`（str 仅支持 += 拼接）。
    fn compound_assign(&self, op: CompoundOp, cur: Value, rhs: Value, span: Span) -> Result<Value, ZError> {
        let binop = match op {
            CompoundOp::Add => BinOp::Add,
            CompoundOp::Sub => BinOp::Sub,
            CompoundOp::Mul => BinOp::Mul,
            CompoundOp::Div => BinOp::Div,
            CompoundOp::Mod => BinOp::Mod,
        };
        self.arith(binop, cur, rhs, span)
    }

    /// 尝试按「类名.方法名」解析类方法调用。
    /// 类已注册 → Some(执行结果)；类未注册或非类调用 → None（落入常规解析）。
    fn call_class_method(&mut self, callee: &str, args: &[Value], span: Span) -> Option<Result<Value, ZError>> {
        let (cls, method) = callee.split_once('.')?;
        let methods = self.classes.get(cls)?;
        match methods.get(method).cloned() {
            Some(f) => Some(self.exec_user_fn(callee, &f, args.to_vec(), span)),
            None => Some(Err(self.runtime_err(
                codes::UNDEFINED,
                format!("class `{}` has no method `{}`", cls, method),
                span,
                Some("check the method name"),
            ))),
        }
    }

    // ---------- 表达式 ----------

    /// 索引赋值：把 target（a[i] 或 m[i][j]...）更新为 value。
    /// 列表为值类型：每层克隆后写回基变量，保持拷贝语义（b = a 后改 a 不影响 b）。
    fn set_index_value(&mut self, env: &mut Env, target: &Expr, value: Value, span: Span) -> Result<(), ZError> {
        let Expr::Index { obj, index, span: ispan } = target else {
            return Err(self.runtime_err(
                codes::TYPE_MISMATCH,
                "invalid index assignment target",
                span,
                Some("index assignment target must be `a[i]` or `m[i][j]`"),
            ));
        };
        let span = *ispan; // 解引用：Span 为 Copy，后续统一按值传递
        let i = match self.eval_expr(env, index)? {
            Value::Int(x) => x,
            other => {
                return Err(self.runtime_err(
                    codes::TYPE_MISMATCH,
                    format!("list index must be an int, got `{}`", other.type_name()),
                    span,
                    Some("use an integer expression as the index, e.g. `a[0] = x`"),
                ));
            }
        };
        let cur = self.eval_expr(env, obj)?; // 当前层容器（列表）
        let items = match cur {
            Value::List(items) => items,
            other => {
                return Err(self.runtime_err(
                    codes::TYPE_MISMATCH,
                    format!("cannot index a value of type `{}`", other.type_name()),
                    span,
                    Some("index assignment is supported on lists, e.g. `a[0] = x`"),
                ));
            }
        };
        if i < 0 || (i as usize) >= items.len() {
            return Err(self.runtime_err(
                codes::TYPE_MISMATCH,
                format!("index {} out of bounds (list length {})", i, items.len()),
                span,
                Some("check the index against `len(list)`"),
            ));
        }
        let mut new_items = items.clone();
        new_items[i as usize] = value;
        match obj.as_ref() {
            // 基变量：写回环境
            Expr::Ident { name, .. } => {
                env.set_or_declare(name, Value::List(new_items));
                Ok(())
            }
            // 内层仍是索引：把更新后的容器作为新值递归写回
            inner @ Expr::Index { .. } => self.set_index_value(env, inner, Value::List(new_items), span),
            _ => Err(self.runtime_err(
                codes::TYPE_MISMATCH,
                "invalid index assignment target",
                span,
                Some("index assignment target must be `a[i]` or `m[i][j]`"),
            )),
        }
    }

    /// 索引取值：列表按下标取元素；字符串按下标取单个字符（越界/非容器报错）。
    fn index_value(&self, v: Value, idx: Value, span: Span) -> Result<Value, ZError> {
        let i = match idx {
            Value::Int(x) => x,
            other => {
                return Err(self.runtime_err(
                    codes::TYPE_MISMATCH,
                    format!("list index must be an int, got `{}`", other.type_name()),
                    span,
                    Some("use an integer expression as the index, e.g. `a[0]`, `a[i]`"),
                ));
            }
        };
        match v {
            Value::List(items) => {
                if i < 0 || (i as usize) >= items.len() {
                    return Err(self.runtime_err(
                        codes::TYPE_MISMATCH,
                        format!("index {} out of bounds (list length {})", i, items.len()),
                        span,
                        Some("check the index against `len(list)`"),
                    ));
                }
                Ok(items[i as usize].clone())
            }
            Value::Str(s) => {
                let chars: Vec<char> = s.chars().collect();
                if i < 0 || (i as usize) >= chars.len() {
                    return Err(self.runtime_err(
                        codes::TYPE_MISMATCH,
                        format!("index {} out of bounds (string length {})", i, chars.len()),
                        span,
                        Some("check the index against `len(str)`"),
                    ));
                }
                Ok(Value::Str(chars[i as usize].to_string()))
            }
            other => Err(self.runtime_err(
                codes::TYPE_MISMATCH,
                format!("cannot index a value of type `{}`", other.type_name()),
                span,
                Some("indexing is supported on lists and strings, e.g. `a[0]`, `s[1]`"),
            )),
        }
    }

    fn eval_expr(&mut self, env: &mut Env, e: &Expr) -> Result<Value, ZError> {
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
            Expr::ListComp { elem, var, var2, iter, cond, span } => {
                let it = self.eval_expr(env, iter)?;
                let mut out = Vec::new();
                match it {
                    // 列表推导式：单变量绑定元素
                    Value::List(items) => {
                        if var2.is_some() {
                            return Err(self.runtime_err(
                                codes::TYPE_MISMATCH,
                                "comprehension `for k, v in` requires a dict, got a list",
                                *span,
                                Some("comprehend over lists with a single variable: `for x in list`"),
                            ));
                        }
                        for item in items {
                            env.scopes.push(HashMap::new());
                            env.declare(var, item);
                            let pass = self.comp_cond(env, cond.as_deref())?;
                            if pass {
                                out.push(self.eval_expr(env, elem)?);
                            }
                            env.scopes.pop();
                        }
                    }
                    // 字典推导式来源：var=键，var2=值（可选）
                    Value::Dict(entries) => {
                        for (k, v) in entries {
                            env.scopes.push(HashMap::new());
                            env.declare(var, Value::Str(k));
                            if let Some(v2) = var2 {
                                env.declare(v2, v);
                            }
                            let pass = self.comp_cond(env, cond.as_deref())?;
                            if pass {
                                out.push(self.eval_expr(env, elem)?);
                            }
                            env.scopes.pop();
                        }
                    }
                    other => {
                        return Err(self.runtime_err(
                            codes::TYPE_MISMATCH,
                            format!("comprehension requires a list or dict, got `{}`", other.type_name()),
                            expr_span(iter),
                            Some("comprehend over a list with `for x in list` or a dict with `for k, v in dict`"),
                        ))
                    }
                }
                Ok(Value::List(out))
            }
            Expr::DictComp { key, value, var, var2, iter, cond, span } => {
                let it = self.eval_expr(env, iter)?;
                let mut out = Vec::new();
                match it {
                    Value::List(items) => {
                        if var2.is_some() {
                            return Err(self.runtime_err(
                                codes::TYPE_MISMATCH,
                                "comprehension `for k, v in` requires a dict, got a list",
                                *span,
                                Some("comprehend over lists with a single variable: `for x in list`"),
                            ));
                        }
                        for item in items {
                            env.scopes.push(HashMap::new());
                            env.declare(var, item);
                            let pass = self.comp_cond(env, cond.as_deref())?;
                            if pass {
                                out.push(self.comp_pair(env, key, value, *span)?);
                            }
                            env.scopes.pop();
                        }
                    }
                    Value::Dict(entries) => {
                        for (k, v) in entries {
                            env.scopes.push(HashMap::new());
                            env.declare(var, Value::Str(k));
                            if let Some(v2) = var2 {
                                env.declare(v2, v);
                            }
                            let pass = self.comp_cond(env, cond.as_deref())?;
                            if pass {
                                out.push(self.comp_pair(env, key, value, *span)?);
                            }
                            env.scopes.pop();
                        }
                    }
                    other => {
                        return Err(self.runtime_err(
                            codes::TYPE_MISMATCH,
                            format!("comprehension requires a list or dict, got `{}`", other.type_name()),
                            expr_span(iter),
                            Some("comprehend over a list with `for x in list` or a dict with `for k, v in dict`"),
                        ))
                    }
                }
                Ok(Value::Dict(out))
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
                // 枚举变体访问：Color.Red（obj 为已注册枚举名）→ 校验变体并构造枚举值
                if let Expr::Ident { name, .. } = obj.as_ref() {
                    if let Some(vs) = self.enums.get(name) {
                        if !vs.iter().any(|v| v == field) {
                            return Err(self.runtime_err(
                                codes::UNDEFINED,
                                format!("enum `{}` has no variant `{}`", name, field),
                                *span,
                                Some(format!("variants: {}", vs.join(", "))),
                            ));
                        }
                        return Ok(Value::Enum(Arc::new(EnumVal {
                            ty: name.clone(),
                            variant: field.clone(),
                            payload: Vec::new(),
                        })));
                    }
                }
                let v = self.eval_expr(env, obj)?;
                self.field_value(v, field, *span)
            }
            Expr::OptionalField { obj, field, span } => {
                let v = self.eval_expr(env, obj)?;
                // 可选链：obj 为 null 时短路返回 null，否则同普通字段访问
                if let Value::Null = v {
                    return Ok(Value::Null);
                }
                self.field_value(v, field, *span)
            }
            Expr::Index { obj, index, span } => {
                let v = self.eval_expr(env, obj)?;
                let idx = self.eval_expr(env, index)?;
                // 定义了 __index 时先试内建索引，类型不支持则回退重载
                if self.fns.contains_key("__index") {
                    match self.index_value(v.clone(), idx.clone(), *span) {
                        Ok(r) => Ok(r),
                        Err(e) if e.code == codes::TYPE_MISMATCH => {
                            match self.call_overload("__index", vec![v, idx], *span) {
                                Some(res) => res,
                                None => Err(e),
                            }
                        }
                        Err(e) => Err(e),
                    }
                } else {
                    self.index_value(v, idx, *span)
                }
            }
            Expr::Unary { op, expr, span } => {
                let v = self.eval_expr(env, expr)?;
                // 先取类型名（match 会移动 v，错误分支仍需展示类型）
                let tn = v.type_name();
                match op {
                    UnOp::Neg => match v {
                        Value::Int(x) => Ok(Value::Int(-x)),
                        Value::Float(x) => Ok(Value::Float(-x)),
                        // 类型不支持：若定义了 __neg 则回退重载
                        other => match self.call_overload("__neg", vec![other], *span) {
                            Some(res) => res,
                            None => Err(self.runtime_err(
                                codes::TYPE_MISMATCH,
                                format!("unary `-` requires a number, got `{}`", tn),
                                *span,
                                None::<&str>,
                            )),
                        },
                    },
                    UnOp::Not => match v {
                        Value::Bool(b) => Ok(Value::Bool(!b)),
                        // 类型不支持：若定义了 __not 则回退重载
                        other => match self.call_overload("__not", vec![other], *span) {
                            Some(res) => res,
                            None => Err(self.runtime_err(
                                codes::TYPE_MISMATCH,
                                format!("`!` requires a `bool`, got `{}`", tn),
                                *span,
                                None::<&str>,
                            )),
                        },
                    },
                }
            }
            Expr::Binary { op, lhs, rhs, span } => self.eval_binary(env, *op, lhs, rhs, *span),
            Expr::Match { value, arms, span } => {
                let v = self.eval_expr(env, value)?;
                for (pat, body) in arms {
                    match pat {
                        Pattern::Wildcard => {
                            // `_` 通配符
                            return self.eval_expr(env, body);
                        }
                        Pattern::Lit(p) => {
                            let pv = self.eval_expr(env, p)?;
                            if self.values_eq(&v, &pv, *span)? {
                                return self.eval_expr(env, body);
                            }
                        }
                        Pattern::Variant { enum_name, variant, binds, .. } => {
                            if let Value::Enum(ev) = &v {
                                if &ev.ty == enum_name && &ev.variant == variant {
                                    // 运行时兜底校验载荷个数（checker 已静态校验）
                                    if binds.len() != ev.payload.len() {
                                        return Err(self.runtime_err(
                                            codes::ARG_COUNT,
                                            format!(
                                                "variant `{}` of enum `{}` expects {} payload value(s), pattern binds {}",
                                                variant,
                                                enum_name,
                                                ev.payload.len(),
                                                binds.len()
                                            ),
                                            *span,
                                            Some("check the binding count in the pattern"),
                                        ));
                                    }
                                    // 有绑定：新作用域声明绑定变量，求值分支体后弹出
                                    if binds.iter().any(|b| b.is_some()) {
                                        env.scopes.push(HashMap::new());
                                        for (b, pv) in binds.iter().zip(ev.payload.iter()) {
                                            if let Some(name) = b {
                                                env.declare(name, pv.clone());
                                            }
                                        }
                                        let r = self.eval_expr(env, body)?;
                                        env.scopes.pop();
                                        return Ok(r);
                                    }
                                    return self.eval_expr(env, body);
                                }
                            }
                        }
                    }
                }
                Err(self.runtime_err(
                    codes::SYNTAX,
                    "no match arm matched the value",
                    *span,
                    Some("add a `_` wildcard arm as the fallback"),
                ))
            }
            Expr::IncDec { op, prefix, name, span } => {
                let cur = env.get(name).cloned();
                match cur {
                    Some(cur) => {
                        let new = self.incdec_value(*op, cur.clone(), *span)?;
                        env.set_or_declare(name, new.clone());
                        // 前缀返回新值（++i），后缀返回旧值（i++）
                        Ok(if *prefix { new } else { cur })
                    }
                    None => Err(self.runtime_err(
                        codes::UNDEFINED,
                        format!("undefined variable `{}`", name),
                        *span,
                        Some("declare the variable before incrementing or decrementing it"),
                    )),
                }
            }
            Expr::Ternary { cond, then_expr, else_expr, span } => {
                let c = self.eval_expr(env, cond)?;
                if let Value::Bool(b) = c {
                    if b {
                        self.eval_expr(env, then_expr)
                    } else {
                        self.eval_expr(env, else_expr)
                    }
                } else {
                    Err(self.runtime_err(
                        codes::TYPE_MISMATCH,
                        format!("ternary condition must be `bool`, got `{}`", c.type_name()),
                        *span,
                        None::<&str>,
                    ))
                }
            }
            Expr::Lambda { params, body, .. } => {
                // 创建 lambda：按值捕获当前作用域的全部可见变量（外→内合并，内层覆盖外层）
                let mut captured = HashMap::new();
                for scope in &env.scopes {
                    captured.extend(scope.iter().map(|(k, v)| (k.clone(), v.clone())));
                }
                Ok(Value::Lambda(Arc::new(LambdaVal {
                    params: params.clone(),
                    body: body.clone(),
                    captured,
                })))
            }
            Expr::Await { expr, span } => {
                // await：阻塞等待 async 函数调用的 future 完成，返回其结果
                let v = self.eval_expr(env, expr)?;
                match v {
                    Value::Future(f) => f.wait(),
                    other => Err(self.runtime_err(
                        codes::TYPE_MISMATCH,
                        format!("`await` requires an async function call result (future), got `{}`", other.type_name()),
                        *span,
                        Some("await an async function call: `await fetch_data()`"),
                    )),
                }
            }
            Expr::Call { callee, args, span } => {
                let mut arg_vals = Vec::new();
                for a in args {
                    arg_vals.push(self.eval_expr(env, a)?);
                }
                // 变量名调用优先解析为 lambda 值（闭包），否则走全局函数/内置/库解析
                if let Some(Value::Lambda(f)) = env.get(callee).cloned() {
                    return self.exec_lambda(&f, arg_vals, *span);
                }
                self.call_fn(callee, arg_vals, *span)
            }
        }
    }

    /// 推导式过滤条件求值：无 cond 视为通过；cond 必须求值为 bool。
    fn comp_cond(&mut self, env: &mut Env, cond: Option<&Expr>) -> Result<bool, ZError> {
        match cond {
            None => Ok(true),
            Some(c) => match self.eval_expr(env, c)? {
                Value::Bool(b) => Ok(b),
                other => Err(self.runtime_err(
                    codes::TYPE_MISMATCH,
                    format!("comprehension `if` condition must be `bool`, got `{}`", other.type_name()),
                    expr_span(c),
                    None::<&str>,
                )),
            },
        }
    }

    /// 字典推导式单对 (键, 值)：键必须求值为 str（与字典字面量键为字符串的约束一致）。
    fn comp_pair(&mut self, env: &mut Env, key: &Expr, value: &Expr, span: Span) -> Result<(String, Value), ZError> {
        let k = self.eval_expr(env, key)?;
        let v = self.eval_expr(env, value)?;
        let ks = match k {
            Value::Str(s) => s,
            other => {
                return Err(self.runtime_err(
                    codes::TYPE_MISMATCH,
                    format!("dict comprehension keys must be `str`, got `{}`", other.type_name()),
                    expr_span(key),
                    Some("convert the key with `to_str(...)`"),
                ))
            }
        };
        let _ = span;
        Ok((ks, v))
    }

    /// 字段访问核心：dict/struct 按键取字段，error 取错误属性；其他类型报错。
    /// Field 与 OptionalField（?.）共用；可选链在调用方先做 null 短路。
    fn field_value(&self, v: Value, field: &str, span: Span) -> Result<Value, ZError> {
        match v {
            Value::Dict(entries) => {
                // struct 实例 / dict 字段访问：按键查找
                match entries.iter().find(|(k, _)| k == field) {
                    Some((_, val)) => Ok(val.clone()),
                    None => Err(self.runtime_err(
                        codes::UNDEFINED,
                        format!(
                            "unknown field `{}` (dict/struct has {})",
                            field,
                            entries.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(", ")
                        ),
                        span,
                        Some("check the field name, or the struct definition"),
                    )),
                }
            }
            Value::Error(e) => match field {
                "code" => Ok(Value::Str(e.code.to_string())),
                "message" => Ok(Value::Str(e.message.clone())),
                "file" => Ok(Value::Str(e.file.clone())),
                "context" => Ok(Value::Str(e.context.clone())),
                "line" => Ok(Value::Int(e.line as i64)),
                "col" => Ok(Value::Int(e.col as i64)),
                other => Err(self.runtime_err(
                    codes::UNDEFINED,
                    format!("unknown error field `{}`", other),
                    span,
                    Some("error fields: code, message, file, line, col, context"),
                )),
            },
            other => Err(self.runtime_err(
                codes::TYPE_MISMATCH,
                format!("field access `.{}` requires an `error` value, got `{}`", field, other.type_name()),
                span,
                Some("only error values (catch variables) support field access"),
            )),
        }
    }

    fn eval_binary(&mut self, env: &mut Env, op: BinOp, lhs: &Expr, rhs: &Expr, span: Span) -> Result<Value, ZError> {
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
                match self.values_eq(&l, &r, span) {
                    Ok(eq) => Ok(Value::Bool(if op == BinOp::Eq { eq } else { !eq })),
                    // 类型不匹配：若定义了 __eq / __ne 则回退重载
                    Err(e) => {
                        let ov = if op == BinOp::Eq { "__eq" } else { "__ne" };
                        match self.call_overload(ov, vec![l, r], span) {
                            Some(res) => res,
                            None => Err(e),
                        }
                    }
                }
            }
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                let l = self.eval_expr(env, lhs)?;
                let r = self.eval_expr(env, rhs)?;
                match self.values_cmp(&l, &r, span) {
                    Ok(c) => Ok(Value::Bool(match op {
                        BinOp::Lt => c == std::cmp::Ordering::Less,
                        BinOp::Le => c != std::cmp::Ordering::Greater,
                        BinOp::Gt => c == std::cmp::Ordering::Greater,
                        BinOp::Ge => c != std::cmp::Ordering::Less,
                        _ => unreachable!(),
                    })),
                    Err(e) => {
                        // float 比较是内建语义（含 NaN 错误），不回退；其余类型不匹配回退重载
                        let native = matches!((&l, &r), (Value::Float(_), Value::Float(_)) | (Value::Int(_), Value::Int(_)));
                        if native {
                            Err(e)
                        } else {
                            let ov = match op {
                                BinOp::Lt => "__lt",
                                BinOp::Le => "__le",
                                BinOp::Gt => "__gt",
                                BinOp::Ge => "__ge",
                                _ => unreachable!(),
                            };
                            match self.call_overload(ov, vec![l, r], span) {
                                Some(res) => res,
                                None => Err(e),
                            }
                        }
                    }
                }
            }
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
                let l = self.eval_expr(env, lhs)?;
                let r = self.eval_expr(env, rhs)?;
                // 内建组合（数字 / str+str）直接内建运算（零克隆热路径）；
                // 非内建组合：先试内建（除零/溢出错误不回退），类型不匹配时回退 __op 重载
                let native = matches!(
                    (&l, &r),
                    (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) | (Value::Str(_), Value::Str(_))
                );
                if native {
                    self.arith(op, l, r, span)
                } else {
                    match self.arith(op, l.clone(), r.clone(), span) {
                        Ok(v) => Ok(v),
                        Err(e) if e.code == codes::TYPE_MISMATCH => {
                            let ov = match op {
                                BinOp::Add => "__add",
                                BinOp::Sub => "__sub",
                                BinOp::Mul => "__mul",
                                BinOp::Div => "__div",
                                BinOp::Mod => "__mod",
                                _ => unreachable!(),
                            };
                            match self.call_overload(ov, vec![l, r], span) {
                                Some(res) => res,
                                None => Err(e),
                            }
                        }
                        Err(e) => Err(e),
                    }
                }
            }
            BinOp::Coalesce => {
                // a ?? b：a 为 null 时取 b，否则取 a（短路：null 时才对 b 求值）
                let l = self.eval_expr(env, lhs)?;
                if matches!(l, Value::Null) {
                    self.eval_expr(env, rhs)
                } else {
                    Ok(l)
                }
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
            // 枚举值：类型 + 变体 + 载荷全部相等才相等
            (Value::Enum(x), Value::Enum(y)) => Ok(x.ty == y.ty && x.variant == y.variant && x.payload == y.payload),
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
        // 字符串拼接快速路径：预留容量一次分配，避免 format! 的额外开销
        // （只借用检查，非 Str 情况落回下方原有算术逻辑）
        if op == BinOp::Add {
            if let (Value::Str(x), Value::Str(y)) = (&a, &b) {
                let mut out = String::with_capacity(x.len() + y.len());
                out.push_str(x);
                out.push_str(y);
                return Ok(Value::Str(out));
            }
        }
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
