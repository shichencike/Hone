// checker.rs - Hone 静态类型检查与推断
// 设计要点（与规范一致）：
//   - 类型一经推导或显式声明即锁定，禁止隐式转换（H001）
//   - 参数/变量类型可由使用上下文推导（强制绑定），推导失败报 H003
//   - 运算符歧义（如 + 可能为 int 或 str）报 H004
//   - 条件表达式必须是 bool（H008）
// 实现：两阶段。Phase A 一次性构建所有绑定（slot 分配，跨轮次稳定）；
// Phase B 对函数体与全局语句反复检查直到 slot 不再变化（不动点），
// 最后以 strict 模式再跑一遍，对仍无法确定类型的用点报错。

use std::cell::Cell;
use std::collections::{HashMap, HashSet};

use crate::ast::*;
use crate::error::codes;
use crate::error::ZError;
use crate::lexer::Span;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ty {
    Int,
    Float,
    Bool,
    Str,
    /// catch 绑定的错误对象类型（e.code / e.message 等）
    Error,
    Void,
    Unknown,
}

impl Ty {
    pub fn name(&self) -> &'static str {
        match self {
            Ty::Int => "int",
            Ty::Float => "float",
            Ty::Bool => "bool",
            Ty::Str => "str",
            Ty::Error => "error",
            Ty::Void => "void",
            Ty::Unknown => "unknown",
        }
    }

    pub fn from_annot(t: TyName) -> Ty {
        match t {
            TyName::Int => Ty::Int,
            TyName::Float => Ty::Float,
            TyName::Bool => Ty::Bool,
            TyName::Str => Ty::Str,
            // 类型变量不直接映射具体类型（build_fn_info 中由类型参数槽接管）
            TyName::Var(_) => Ty::Unknown,
        }
    }

    pub fn is_numeric(self) -> bool {
        matches!(self, Ty::Int | Ty::Float)
    }
}

/// 表达式类型检查结果。
/// slot 为 Some 时表示类型来自某个绑定槽位（参数/变量），可被使用点强制。
#[derive(Debug, Clone, Copy)]
struct TyRes {
    ty: Ty,
    slot: Option<usize>,
}

#[derive(Clone)]
struct FnInfo {
    name: String,
    params: Vec<String>,
    param_slots: Vec<usize>,
    /// 与 params 并行的默认参数表达式（None = 无默认值，必须传参）
    defaults: Vec<Option<Expr>>,
    ret_slot: usize,
    ret_annot: Option<Ty>,
    body: Vec<Stmt>,
    span: Span,
    /// 函数体内每个作用域的绑定表（Phase A 构建，Phase B 复用）
    scopes: Vec<HashMap<String, usize>>,
    /// 泛型类型参数槽（按声明顺序）。调用点为其新建槽实现实例化，
    /// 使同一泛型函数可以不同类型多次调用（编译期擦除，运行期零成本）。
    type_param_slots: Vec<usize>,
}

pub struct Checker {
    file: String,
    src: String,
    slots: Vec<Cell<Option<Ty>>>,
    globals: HashMap<String, usize>,
    global_scopes: Vec<HashMap<String, usize>>,
    fns: HashMap<String, FnInfo>,
    changed: bool,
    strict: bool,
    has_return: bool,
    /// 当前嵌套的循环深度（>0 时 break 合法），顶层与函数内检查共用
    loop_depth: usize,
    /// 程序中存在 import/load 等动态外部加载，未定义函数可能来自外部模块
    has_external: bool,
    /// load 签名块声明的 FFI 函数（键为完整调用名 "alias.fn"）
    ffi_sigs: HashMap<String, FfiSig>,
    /// from 头文件解析结果缓存（键为头文件路径，避免固定点循环重复读取）
    header_cache: HashMap<String, Vec<FfiSig>>,
    builtins: HashSet<&'static str>,
    /// 结构体定义：名称 → (字段名, 字段类型)
    structs: HashMap<String, Vec<(String, Ty)>>,
    /// 类定义：类名 → (方法名 → FnInfo)。成员函数不进入全局符号表，
    /// 只能经 `类.方法(...)` 调用（resolve_call 按限定名解析）。
    classes: HashMap<String, HashMap<String, FnInfo>>,
}

impl Checker {
    /// 对整个程序做静态类型检查，出错返回 ZError。
    pub fn check(program: &Program, file: &str, src: &str) -> Result<(), ZError> {
        let builtins = builtin_names();
        let mut ck = Checker {
            file: file.to_string(),
            src: src.to_string(),
            slots: Vec::new(),
            globals: HashMap::new(),
            global_scopes: vec![HashMap::new()],
            fns: HashMap::new(),
            changed: false,
            strict: false,
            has_return: false,
            loop_depth: 0,
            has_external: false,
            ffi_sigs: HashMap::new(),
            header_cache: HashMap::new(),
            builtins,
            structs: HashMap::new(),
            classes: HashMap::new(),
        };

        // Phase A：注册顶层函数并构建全局绑定
        ck.register_top(&program.stmts)?;

        // Phase B：不动点类型检查
        let mut guard = 0;
        loop {
            ck.changed = false;
            ck.check_all(&program.stmts)?;
            guard += 1;
            if !ck.changed || guard > 32 {
                break;
            }
        }

        // strict 模式：对残留的不可推导类型报错
        ck.strict = true;
        ck.check_all(&program.stmts)?;
        Ok(())
    }

    // ---------- Phase A：注册与绑定构建 ----------

    fn new_slot(&mut self) -> usize {
        let id = self.slots.len();
        self.slots.push(Cell::new(None));
        id
    }

    /// 顶层注册：注册所有函数定义（含嵌套扁平化），并构建全局作用域绑定。
    fn register_top(&mut self, stmts: &[Stmt]) -> Result<(), ZError> {
        for stmt in stmts {
            self.register_top_stmt(stmt)?;
        }
        Ok(())
    }

    fn register_top_stmt(&mut self, stmt: &Stmt) -> Result<(), ZError> {
        match stmt {
            Stmt::FnDef { name, type_params, params, ret, body, span, tmp } => {
                if !tmp {
                    self.register_fn(name, type_params, params, ret.clone(), body, *span)?;
                }
            }
            Stmt::Block { stmts, .. } => {
                self.global_scopes.push(HashMap::new());
                self.register_top(stmts)?;
                self.global_scopes.pop();
            }
            Stmt::If { then_branch, else_branch, .. } => {
                for branch in [then_branch, else_branch.as_deref().unwrap_or(&[])] {
                    self.global_scopes.push(HashMap::new());
                    self.register_top(branch)?;
                    self.global_scopes.pop();
                }
            }
            Stmt::While { body, .. } => {
                self.global_scopes.push(HashMap::new());
                self.register_top(body)?;
                self.global_scopes.pop();
            }
            Stmt::DoWhile { body, .. } => {
                self.global_scopes.push(HashMap::new());
                self.register_top(body)?;
                self.global_scopes.pop();
            }
            Stmt::ForC { init, step, body, span, .. } => {
                self.global_scopes.push(HashMap::new());
                if let Some(i) = init {
                    self.register_top(std::slice::from_ref(i.as_ref()))?;
                }
                if let Some(s) = step {
                    self.register_top(std::slice::from_ref(s.as_ref()))?;
                }
                self.register_top(body)?;
                self.global_scopes.pop();
                let _ = span;
            }
            Stmt::ForIn { var, var2, body, span, .. } => {
                self.global_scopes.push(HashMap::new());
                self.bind_or_unify(var, None, *span)?;
                if let Some(v2) = var2 {
                    self.bind_or_unify(v2, None, *span)?;
                }
                self.register_top(body)?;
                self.global_scopes.pop();
            }
            Stmt::Try { body, catch_var, handler, .. } => {
                self.global_scopes.push(HashMap::new());
                self.register_top(body)?;
                self.global_scopes.pop();
                // handler 作用域：catch 变量固定为 error 类型
                self.global_scopes.push(HashMap::new());
                self.bind_or_unify(catch_var, Some(Ty::Error), stmt_span(stmt))?;
                self.register_top(handler)?;
                self.global_scopes.pop();
            }
            Stmt::VarDecl { name, .. } => {
                self.bind_or_unify(name, None, stmt_span(stmt))?;
            }
            Stmt::StructDef { name, fields, span } => {
                // 注册结构体定义（供构造调用与字段访问的类型检查）
                if self.structs.contains_key(name) {
                    return Err(self.zerr(
                        codes::SYNTAX,
                        format!("struct `{}` is already defined", name),
                        *span,
                        Some("struct names must be unique"),
                    ));
                }
                let mapped = fields.iter().map(|(f, t)| (f.clone(), Ty::from_annot(t.clone()))).collect();
                self.structs.insert(name.clone(), mapped);
            }
            Stmt::ClassDef { name, methods, span } => {
                // 注册类定义：类名唯一；成员函数以「类名.方法名」限定键存入 classes 表，
                // 不进入全局 fns 表（成员函数不可被裸名调用）
                if self.classes.contains_key(name) || self.structs.contains_key(name) {
                    return Err(self.zerr(
                        codes::SYNTAX,
                        format!("class `{}` is already defined", name),
                        *span,
                        Some("class names must be unique and not clash with struct names"),
                    ));
                }
                let mut methods_map: HashMap<String, FnInfo> = HashMap::new();
                for m in methods {
                    if let Stmt::FnDef { name: mname, type_params, params, ret, body, span: mspan, tmp } = m {
                        if !tmp {
                            let fname = format!("{}.{}", name, mname);
                            let info = self.build_fn_info(&fname, type_params, params, ret.clone(), body, *mspan)?;
                            methods_map.insert(mname.clone(), info);
                        }
                    }
                }
                self.classes.insert(name.clone(), methods_map);
            }
            Stmt::Assign { name, .. } => {
                if self.globals.get(name).is_none() {
                    let slot = self.new_slot();
                    self.globals.insert(name.clone(), slot);
                }
            }
            Stmt::AssignOp { name, .. } => {
                if self.globals.get(name).is_none() {
                    let slot = self.new_slot();
                    self.globals.insert(name.clone(), slot);
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// 函数体内注册：构建该函数的作用域绑定表。
    fn register_fn_body(
        &mut self,
        body: &[Stmt],
        scopes: &mut Vec<HashMap<String, usize>>,
        scope_stack: &mut Vec<usize>,
    ) -> Result<(), ZError> {
        for stmt in body {
            match stmt {
                Stmt::FnDef { name, type_params, params, ret, body, span, tmp: _ } => {
                    // 嵌套函数定义：扁平化注册到全局符号表
                    self.register_fn(name, type_params, params, ret.clone(), body, *span)?;
                }
                Stmt::Block { stmts, .. } => {
                    let idx = scopes.len();
                    scopes.push(HashMap::new());
                    scope_stack.push(idx);
                    self.register_fn_body(stmts, scopes, scope_stack)?;
                    scope_stack.pop();
                }
                Stmt::If { then_branch, else_branch, .. } => {
                    for branch in [then_branch, else_branch.as_deref().unwrap_or(&[])] {
                        let idx = scopes.len();
                        scopes.push(HashMap::new());
                        scope_stack.push(idx);
                        self.register_fn_body(branch, scopes, scope_stack)?;
                        scope_stack.pop();
                    }
                }
                Stmt::While { body, .. } => {
                    let idx = scopes.len();
                    scopes.push(HashMap::new());
                    scope_stack.push(idx);
                    self.register_fn_body(body, scopes, scope_stack)?;
                    scope_stack.pop();
                }
                Stmt::DoWhile { body, .. } => {
                    let idx = scopes.len();
                    scopes.push(HashMap::new());
                    scope_stack.push(idx);
                    self.register_fn_body(body, scopes, scope_stack)?;
                    scope_stack.pop();
                }
                Stmt::ForC { init, step, body, .. } => {
                    let idx = scopes.len();
                    scopes.push(HashMap::new());
                    scope_stack.push(idx);
                    if let Some(i) = init {
                        self.register_fn_body(std::slice::from_ref(i.as_ref()), scopes, scope_stack)?;
                    }
                    if let Some(s) = step {
                        self.register_fn_body(std::slice::from_ref(s.as_ref()), scopes, scope_stack)?;
                    }
                    self.register_fn_body(body, scopes, scope_stack)?;
                    scope_stack.pop();
                }
                Stmt::ForIn { var, var2, body, span, .. } => {
                    let idx = scopes.len();
                    scopes.push(HashMap::new());
                    scope_stack.push(idx);
                    self.bind_in_stack(var, None, *span, scopes, scope_stack)?;
                    if let Some(v2) = var2 {
                        self.bind_in_stack(v2, None, *span, scopes, scope_stack)?;
                    }
                    self.register_fn_body(body, scopes, scope_stack)?;
                    scope_stack.pop();
                }
                Stmt::Try { body, catch_var, handler, .. } => {
                    let idx = scopes.len();
                    scopes.push(HashMap::new());
                    scope_stack.push(idx);
                    self.register_fn_body(body, scopes, scope_stack)?;
                    scope_stack.pop();
                    // handler 作用域：catch 变量固定为 error 类型
                    let idx = scopes.len();
                    scopes.push(HashMap::new());
                    scope_stack.push(idx);
                    self.bind_in_stack(catch_var, Some(Ty::Error), stmt_span(stmt), scopes, scope_stack)?;
                    self.register_fn_body(handler, scopes, scope_stack)?;
                    scope_stack.pop();
                }
                Stmt::VarDecl { name, span, .. } => {
                    self.bind_in_stack(name, None, *span, scopes, scope_stack)?;
                }
                Stmt::Assign { name, .. } => {
                    if lookup_in_stack(name, scopes, scope_stack).is_none() {
                        let top = *scope_stack.last().unwrap();
                        let slot = self.new_slot();
                        scopes[top].insert(name.clone(), slot);
                    }
                }
                Stmt::AssignOp { name, .. } => {
                    if lookup_in_stack(name, scopes, scope_stack).is_none() {
                        let top = *scope_stack.last().unwrap();
                        let slot = self.new_slot();
                        scopes[top].insert(name.clone(), slot);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// 全局作用域绑定：存在则按类型统一，不存在则创建。
    fn bind_or_unify(&mut self, name: &str, ty: Option<Ty>, span: Span) -> Result<(), ZError> {
        if let Some(slot) = self.globals.get(name) {
            match (self.slots[*slot].get(), ty) {
                (Some(c), Some(t)) if c != t => {
                    return Err(self.zerr(
                        codes::TYPE_MISMATCH,
                        format!(
                            "type mismatch: variable `{}` is locked to `{}`, cannot assign `{}`",
                            name,
                            c.name(),
                            t.name()
                        ),
                        span,
                        Some(format!(
                            "change the value to `{}`, or declare `{}` with a different type",
                            c.name(),
                            name
                        )),
                    ));
                }
                (None, Some(t)) => {
                    self.slots[*slot].set(Some(t));
                    self.changed = true;
                }
                _ => {}
            }
            Ok(())
        } else {
            let slot = self.new_slot();
            if let Some(t) = ty {
                self.slots[slot].set(Some(t));
                self.changed = true;
            }
            self.globals.insert(name.to_string(), slot);
            Ok(())
        }
    }

    /// 函数作用域绑定：存在则按类型统一，不存在则创建。
    fn bind_in_stack(
        &mut self,
        name: &str,
        ty: Option<Ty>,
        span: Span,
        scopes: &mut Vec<HashMap<String, usize>>,
        scope_stack: &mut Vec<usize>,
    ) -> Result<(), ZError> {
        if let Some(slot) = lookup_in_stack(name, scopes, scope_stack) {
            match (self.slots[slot].get(), ty) {
                (Some(c), Some(t)) if c != t => {
                    return Err(self.zerr(
                        codes::TYPE_MISMATCH,
                        format!(
                            "type mismatch: variable `{}` is locked to `{}`, cannot assign `{}`",
                            name,
                            c.name(),
                            t.name()
                        ),
                        span,
                        Some(format!(
                            "change the value to `{}`, or declare `{}` with a different type",
                            c.name(),
                            name
                        )),
                    ));
                }
                (None, Some(t)) => {
                    self.slots[slot].set(Some(t));
                    self.changed = true;
                }
                _ => {}
            }
            Ok(())
        } else {
            let top = *scope_stack.last().unwrap();
            let slot = self.new_slot();
            if let Some(t) = ty {
                self.slots[slot].set(Some(t));
                self.changed = true;
            }
            scopes[top].insert(name.to_string(), slot);
            Ok(())
        }
    }

    fn register_fn(
        &mut self,
        name: &str,
        type_params: &[String],
        params: &[Param],
        ret: Option<TyName>,
        body: &[Stmt],
        span: Span,
    ) -> Result<(), ZError> {
        if self.fns.contains_key(name) || self.builtins.contains(name) {
            return Err(self.zerr(
                codes::UNDEFINED,
                format!("`{}` is already defined", name),
                span,
                Some("function names must be unique; builtin names cannot be redefined"),
            ));
        }
        let info = self.build_fn_info(name, type_params, params, ret, body, span)?;
        self.fns.insert(name.to_string(), info);
        Ok(())
    }

    /// 构建 FnInfo（参数槽 / 返回槽 / 函数体作用域注册）。类方法复用此逻辑，
    /// 但插入 classes 表而非全局 fns 表（成员函数不进全局符号表）。
    /// 泛型：每个类型参数（fn name[T, U]）建一个专属槽，注解 `x: T` / `-> T`
    /// 直接复用该槽（槽别名），调用点 unify 参数时自然推导出具体类型。
    fn build_fn_info(
        &mut self,
        name: &str,
        type_params: &[String],
        params: &[Param],
        ret: Option<TyName>,
        body: &[Stmt],
        span: Span,
    ) -> Result<FnInfo, ZError> {
        let mut seen = HashSet::new();
        let mut defaults = Vec::with_capacity(params.len());
        let mut seen_default = false;
        for p in params {
            if !seen.insert(&p.name) {
                return Err(self.zerr(
                    codes::UNDEFINED,
                    format!("duplicate parameter name `{}` in function `{}`", p.name, name),
                    p.span,
                    Some("rename one of the parameters"),
                ));
            }
            // 默认参数必须连续出现在参数列表尾部（无默认值的参数不能跟在有默认值的后面）
            if p.default.is_some() {
                seen_default = true;
            } else if seen_default {
                return Err(self.zerr(
                    codes::SYNTAX,
                    format!("parameter `{}` of `{}` has no default but follows a defaulted parameter", p.name, name),
                    p.span,
                    Some("move all parameters without defaults before the first defaulted parameter"),
                ));
            }
            defaults.push(p.default.clone());
        }

        // 类型参数 → 槽映射（泛型：T 首次出现时建槽，后续注解复用）
        let mut type_slots: HashMap<String, usize> = HashMap::new();
        for tp in type_params {
            if type_slots.contains_key(tp) {
                return Err(self.zerr(
                    codes::SYNTAX,
                    format!("duplicate type parameter `{}` in `{}`", tp, name),
                    span,
                    Some("type parameters must be unique"),
                ));
            }
            type_slots.insert(tp.clone(), self.new_slot());
        }

        // 构建函数作用域（scope 0 = 函数体顶层）
        let mut scopes: Vec<HashMap<String, usize>> = vec![HashMap::new()];
        let mut param_slots = Vec::new();
        let mut scope_stack: Vec<usize> = vec![0];
        for p in params {
            let slot = match &p.ty {
                // 注解为类型变量：直接用类型参数槽（编译期擦除，调用点推导）
                Some(TyName::Var(tv)) => match type_slots.get(tv) {
                    Some(s) => *s,
                    None => {
                        return Err(self.zerr(
                            codes::UNDEFINED,
                            format!("undeclared type parameter `{}` in function `{}`", tv, name),
                            p.span,
                            Some(format!("add `[{}]` to the function signature", tv)),
                        ))
                    }
                },
                // 常规注解：新槽并锁定类型
                Some(t) => {
                    let s = self.new_slot();
                    self.slots[s].set(Some(Ty::from_annot(t.clone())));
                    self.changed = true;
                    s
                }
                None => self.new_slot(),
            };
            scopes[0].insert(p.name.clone(), slot);
            param_slots.push(slot);
        }
        self.register_fn_body(body, &mut scopes, &mut scope_stack)?;

        let ret_slot = match &ret {
            Some(TyName::Var(tv)) => match type_slots.get(tv) {
                Some(s) => *s,
                None => {
                    return Err(self.zerr(
                        codes::UNDEFINED,
                        format!("undeclared type parameter `{}` in return type of `{}`", tv, name),
                        span,
                        Some(format!("add `[{}]` to the function signature", tv)),
                    ))
                }
            },
            Some(t) => {
                let s = self.new_slot();
                self.slots[s].set(Some(Ty::from_annot(t.clone())));
                self.changed = true;
                s
            }
            None => self.new_slot(),
        };

        Ok(FnInfo {
            name: name.to_string(),
            params: params.iter().map(|p| p.name.clone()).collect(),
            param_slots,
            defaults,
            ret_slot,
            ret_annot: ret.map(|t| Ty::from_annot(t.clone())),
            body: body.to_vec(),
            span,
            scopes,
            type_param_slots: type_params.iter().map(|tp| type_slots[tp]).collect(),
        })
    }

    // ---------- Phase B：类型检查 ----------

    fn check_all(&mut self, top_stmts: &[Stmt]) -> Result<(), ZError> {
        // 先检查所有用户函数
        let names: Vec<String> = self.fns.keys().cloned().collect();
        for name in names {
            self.check_fn(&name)?;
        }
        // 再检查类成员函数（以「类名.方法名」限定键展示）
        let class_methods: Vec<(String, String)> = self
            .classes
            .iter()
            .flat_map(|(cls, methods)| methods.keys().map(move |m| (cls.clone(), m.clone())))
            .collect();
        for (cls, m) in class_methods {
            let fname = format!("{}.{}", cls, m);
            self.check_fn(&fname)?;
        }
        // 再检查全局语句
        self.check_stmts(top_stmts)?;
        Ok(())
    }

    fn check_fn(&mut self, name: &str) -> Result<(), ZError> {
        // 类方法（类名.方法名）从 classes 表取，普通函数从 fns 表取
        let info = if let Some((cls, m)) = name.split_once('.') {
            if let Some(methods) = self.classes.get(cls) {
                match methods.get(m) {
                    Some(i) => i.clone(),
                    None => return Ok(()), // 不是类方法限定名，按普通函数走下方
                }
            } else {
                return Ok(());
            }
        } else {
            match self.fns.get(name) {
                Some(i) => i.clone(),
                None => return Ok(()),
            }
        };
        self.check_method(&info)
    }

    /// 检查单个函数/方法体：返回语句与声明类型一致性。
    fn check_method(&mut self, f: &FnInfo) -> Result<(), ZError> {
        let (body, param_slots, ret_slot, mut scopes, ret_annot) = (
            f.body.clone(),
            f.param_slots.clone(),
            f.ret_slot,
            f.scopes.clone(),
            f.ret_annot,
        );
        self.has_return = false;
        let mut scope_stack: Vec<usize> = vec![0];
        self.check_stmts_with_scopes(&body, &mut scopes, &mut scope_stack, &param_slots, ret_slot)?;

        // 默认参数表达式：在参数作用域中检查（可引用前面的参数），并与参数槽统一类型。
        // 类型参数槽跳过统一（避免默认值锁死泛型参数）。
        for (i, d) in f.defaults.iter().enumerate() {
            if let Some(de) = d {
                let res = self.check_expr_in_fn(de, &mut scopes, &mut scope_stack, &param_slots, ret_slot)?;
                if !f.type_param_slots.contains(&param_slots[i]) {
                    self.unify_slot(
                        param_slots[i],
                        res,
                        f.span,
                        format!("default value of parameter `{}` of `{}`", f.params[i], f.name),
                    )?;
                }
            }
        }

        if !self.has_return {
            let cur = self.slots[ret_slot].get();
            match (cur, ret_annot) {
                (None, None) => {
                    // 无 return 语句且无注解 → 默认 void
                    self.slots[ret_slot].set(Some(Ty::Void));
                    self.changed = true;
                }
                (Some(_), Some(_)) => {
                    // 声明了返回类型却没有任何 return 语句
                    return Err(self.zerr(
                        codes::TYPE_MISMATCH,
                        format!(
                            "function `{}` declares `-> {}` but never returns a value",
                            f.name,
                            ret_annot.unwrap().name()
                        ),
                        f.span,
                        Some("add a `return` statement, or remove the `->` annotation"),
                    ));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn check_stmts(&mut self, stmts: &[Stmt]) -> Result<(), ZError> {
        for stmt in stmts {
            self.check_stmt(stmt)?;
        }
        Ok(())
    }

    fn check_stmts_with_scopes(
        &mut self,
        stmts: &[Stmt],
        scopes: &mut Vec<HashMap<String, usize>>,
        scope_stack: &mut Vec<usize>,
        param_slots: &[usize],
        ret_slot: usize,
    ) -> Result<(), ZError> {
        for stmt in stmts {
            self.check_stmt_in_fn(stmt, scopes, scope_stack, param_slots, ret_slot)?;
        }
        Ok(())
    }

    // ---------- 语句检查（全局作用域） ----------

    fn check_stmt(&mut self, stmt: &Stmt) -> Result<(), ZError> {
        match stmt {
            Stmt::VarDecl { name, ty, init, span } => {
                let annot = Ty::from_annot(ty.clone());
                if let Some(e) = init {
                    let res = self.check_expr(e)?;
                    self.unify_with(annot, res, *span, format!("variable `{}`", name))?;
                }
                self.bind_or_unify(name, Some(annot), *span)?;
                Ok(())
            }
            Stmt::Assign { name, value, span } => {
                let res = self.check_expr(value)?;
                match self.globals.get(name) {
                    Some(slot) => {
                        self.unify_slot(*slot, res, *span, format!("variable `{}`", name))?;
                    }
                    None => {
                        let slot = self.new_slot();
                        if res.ty != Ty::Unknown {
                            self.slots[slot].set(Some(res.ty));
                            self.changed = true;
                        }
                        self.globals.insert(name.clone(), slot);
                    }
                }
                Ok(())
            }
            Stmt::AssignOp { name, op, value, span } => {
                let res = self.check_expr(value)?;
                match self.globals.get(name) {
                    Some(slot) => {
                        let cur = self.slots[*slot].get().unwrap_or(Ty::Unknown);
                        self.check_compound_assign(*slot, cur, *op, res, *span, name)
                    }
                    None => Err(self.zerr(
                        codes::UNDEFINED,
                        format!("undefined variable `{}` (declare it before `{}` assignment)", name, op.symbol()),
                        *span,
                        Some("use `x = value` to declare and initialize"),
                    )),
                }
            }
            Stmt::Block { stmts, .. } => {
                self.global_scopes.push(HashMap::new());
                self.check_stmts(stmts)?;
                self.global_scopes.pop();
                Ok(())
            }
            Stmt::If { cond, then_branch, else_branch, span } => {
                let res = self.check_expr(cond)?;
                self.require_bool(res, span, "`if` condition")?;
                self.global_scopes.push(HashMap::new());
                self.check_stmts(then_branch)?;
                self.global_scopes.pop();
                if let Some(eb) = else_branch {
                    self.global_scopes.push(HashMap::new());
                    self.check_stmts(eb)?;
                    self.global_scopes.pop();
                }
                Ok(())
            }
            Stmt::While { cond, body, span } => {
                let res = self.check_expr(cond)?;
                self.require_bool(res, span, "`while` condition")?;
                self.global_scopes.push(HashMap::new());
                self.loop_depth += 1;
                self.check_stmts(body)?;
                self.loop_depth -= 1;
                self.global_scopes.pop();
                Ok(())
            }
            Stmt::DoWhile { body, cond, span } => {
                let res = self.check_expr(cond)?;
                self.require_bool(res, span, "`do-while` condition")?;
                self.global_scopes.push(HashMap::new());
                self.loop_depth += 1;
                self.check_stmts(body)?;
                self.loop_depth -= 1;
                self.global_scopes.pop();
                Ok(())
            }
            Stmt::ForC { init, cond, step, body, span } => {
                self.global_scopes.push(HashMap::new());
                if let Some(i) = init {
                    self.check_stmt(i)?;
                }
                if let Some(c) = cond {
                    let res = self.check_expr(c)?;
                    self.require_bool(res, span, "`for` condition")?;
                }
                if let Some(s) = step {
                    self.check_stmt(s)?;
                }
                self.loop_depth += 1;
                self.check_stmts(body)?;
                self.loop_depth -= 1;
                self.global_scopes.pop();
                Ok(())
            }
            Stmt::ForIn { var, var2, iter, body, span } => {
                let res = self.check_expr(iter)?;
                // 迭代对象可为 list / dict（动态），此处仅确保表达式合法
                let _ = res;
                self.global_scopes.push(HashMap::new());
                self.bind_or_unify(var, None, *span)?;
                if let Some(v2) = var2 {
                    self.bind_or_unify(v2, None, *span)?;
                }
                self.loop_depth += 1;
                self.check_stmts(body)?;
                self.loop_depth -= 1;
                self.global_scopes.pop();
                Ok(())
            }
            Stmt::Return { span, .. } => Err(self.zerr(
                codes::SYNTAX,
                "`return` is only allowed inside a function",
                *span,
                Some("move the `return` into a `fn` body"),
            )),
            Stmt::FnDef { .. } => Ok(()), // 已注册
            Stmt::StructDef { .. } => Ok(()), // 已注册
            Stmt::ClassDef { .. } => Ok(()), // 已注册（成员函数经 classes 表单独校验）
            Stmt::ExprStmt { expr, .. } => {
                self.check_expr(expr)?;
                Ok(())
            }
            Stmt::Break { span } => {
                if self.loop_depth == 0 {
                    return Err(self.zerr(
                        codes::SYNTAX,
                        "`break` is only allowed inside a loop",
                        *span,
                        Some("move the `break` into a `while`, `do-while` or `for` body"),
                    ));
                }
                Ok(())
            }
            Stmt::Continue { span } => {
                if self.loop_depth == 0 {
                    return Err(self.zerr(
                        codes::SYNTAX,
                        "`continue` is only allowed inside a loop",
                        *span,
                        Some("move the `continue` into a `while`, `do-while` or `for` body"),
                    ));
                }
                Ok(())
            }
            Stmt::Breakpoint { .. } => Ok(()),
            Stmt::Export { name, span } => {
                if !self.fns.contains_key(name) {
                    return Err(self.zerr(
                        codes::UNDEFINED,
                        format!("`@export` refers to undefined function `{}`", name),
                        *span,
                        Some("define the function with `fn` before exporting it"),
                    ));
                }
                Ok(())
            }
            Stmt::Import { span, .. } => {
                // 模块函数在运行时下载后可用，此处标记外部动态加载
                self.has_external = true;
                let _ = span;
                Ok(())
            }
            Stmt::Load { alias, from, sigs, .. } => {
                // load 库函数在运行时才可用，标记外部动态加载
                self.has_external = true;
                self.register_ffi_sigs(alias.as_deref(), from.as_deref(), sigs)
            }
            Stmt::Alias { .. } => {
                // alias 新名在运行时才可用，标记外部动态加载
                self.has_external = true;
                Ok(())
            }
            Stmt::Use { .. } => Ok(()),
            Stmt::Go { callee, args, span } => {
                let mut arg_tys = Vec::new();
                for a in args {
                    arg_tys.push(self.check_expr(a)?);
                }
                self.resolve_call(callee, &arg_tys, *span)?;
                Ok(())
            }
            Stmt::DebugPrint { expr, .. } => {
                self.check_expr(expr)?;
                Ok(())
            }
            Stmt::Try { body, catch_var, handler, span } => {
                self.global_scopes.push(HashMap::new());
                self.check_stmts(body)?;
                self.global_scopes.pop();
                self.global_scopes.push(HashMap::new());
                self.bind_or_unify(catch_var, Some(Ty::Error), *span)?;
                self.check_stmts(handler)?;
                self.global_scopes.pop();
                Ok(())
            }
            Stmt::Throw { value, span } => {
                let res = self.check_expr(value)?;
                self.check_throw_value(res, *span)
            }
        }
    }

    // ---------- 语句检查（函数体内） ----------

    fn check_stmt_in_fn(
        &mut self,
        stmt: &Stmt,
        scopes: &mut Vec<HashMap<String, usize>>,
        scope_stack: &mut Vec<usize>,
        param_slots: &[usize],
        ret_slot: usize,
    ) -> Result<(), ZError> {
        match stmt {
            Stmt::VarDecl { name, ty, init, span } => {
                let annot = Ty::from_annot(ty.clone());
                if let Some(e) = init {
                    let res = self.check_expr_in_fn(e, scopes, scope_stack, param_slots, ret_slot)?;
                    self.unify_with(annot, res, *span, format!("variable `{}`", name))?;
                }
                self.bind_in_stack(name, Some(annot), *span, scopes, scope_stack)?;
                Ok(())
            }
            Stmt::Assign { name, value, span } => {
                let res = self.check_expr_in_fn(value, scopes, scope_stack, param_slots, ret_slot)?;
                match lookup_in_stack(name, scopes, scope_stack) {
                    Some(slot) => {
                        self.unify_slot(slot, res, *span, format!("variable `{}`", name))?;
                    }
                    None => {
                        let top = *scope_stack.last().unwrap();
                        let slot = self.new_slot();
                        if res.ty != Ty::Unknown {
                            self.slots[slot].set(Some(res.ty));
                            self.changed = true;
                        }
                        scopes[top].insert(name.clone(), slot);
                    }
                }
                Ok(())
            }
            Stmt::AssignOp { name, op, value, span } => {
                let res = self.check_expr_in_fn(value, scopes, scope_stack, param_slots, ret_slot)?;
                match lookup_in_stack(name, scopes, scope_stack) {
                    Some(slot) => {
                        let cur = self.slots[slot].get().unwrap_or(Ty::Unknown);
                        self.check_compound_assign(slot, cur, *op, res, *span, name)
                    }
                    None => Err(self.zerr(
                        codes::UNDEFINED,
                        format!("undefined variable `{}` (declare it before `{}` assignment)", name, op.symbol()),
                        *span,
                        Some("use `x = value` to declare and initialize"),
                    )),
                }
            }
            Stmt::Block { stmts, .. } => {
                let idx = scopes.len();
                scopes.push(HashMap::new());
                scope_stack.push(idx);
                self.check_stmts_with_scopes(stmts, scopes, scope_stack, param_slots, ret_slot)?;
                scope_stack.pop();
                scopes.pop();
                Ok(())
            }
            Stmt::If { cond, then_branch, else_branch, span } => {
                let res = self.check_expr_in_fn(cond, scopes, scope_stack, param_slots, ret_slot)?;
                self.require_bool(res, span, "`if` condition")?;
                let idx = scopes.len();
                scopes.push(HashMap::new());
                scope_stack.push(idx);
                self.check_stmts_with_scopes(then_branch, scopes, scope_stack, param_slots, ret_slot)?;
                scope_stack.pop();
                scopes.pop();
                if let Some(eb) = else_branch {
                    let idx = scopes.len();
                    scopes.push(HashMap::new());
                    scope_stack.push(idx);
                    self.check_stmts_with_scopes(eb, scopes, scope_stack, param_slots, ret_slot)?;
                    scope_stack.pop();
                    scopes.pop();
                }
                Ok(())
            }
            Stmt::While { cond, body, span } => {
                let res = self.check_expr_in_fn(cond, scopes, scope_stack, param_slots, ret_slot)?;
                self.require_bool(res, span, "`while` condition")?;
                let idx = scopes.len();
                scopes.push(HashMap::new());
                scope_stack.push(idx);
                self.loop_depth += 1;
                self.check_stmts_with_scopes(body, scopes, scope_stack, param_slots, ret_slot)?;
                self.loop_depth -= 1;
                scope_stack.pop();
                scopes.pop();
                Ok(())
            }
            Stmt::DoWhile { body, cond, span } => {
                let res = self.check_expr_in_fn(cond, scopes, scope_stack, param_slots, ret_slot)?;
                self.require_bool(res, span, "`do-while` condition")?;
                let idx = scopes.len();
                scopes.push(HashMap::new());
                scope_stack.push(idx);
                self.loop_depth += 1;
                self.check_stmts_with_scopes(body, scopes, scope_stack, param_slots, ret_slot)?;
                self.loop_depth -= 1;
                scope_stack.pop();
                scopes.pop();
                Ok(())
            }
            Stmt::ForC { init, cond, step, body, span } => {
                let idx = scopes.len();
                scopes.push(HashMap::new());
                scope_stack.push(idx);
                if let Some(i) = init {
                    self.check_stmt_in_fn(i, scopes, scope_stack, param_slots, ret_slot)?;
                }
                if let Some(c) = cond {
                    let res = self.check_expr_in_fn(c, scopes, scope_stack, param_slots, ret_slot)?;
                    self.require_bool(res, span, "`for` condition")?;
                }
                if let Some(s) = step {
                    self.check_stmt_in_fn(s, scopes, scope_stack, param_slots, ret_slot)?;
                }
                self.loop_depth += 1;
                self.check_stmts_with_scopes(body, scopes, scope_stack, param_slots, ret_slot)?;
                self.loop_depth -= 1;
                scope_stack.pop();
                scopes.pop();
                Ok(())
            }
            Stmt::ForIn { var, var2, iter, body, span } => {
                let res = self.check_expr_in_fn(iter, scopes, scope_stack, param_slots, ret_slot)?;
                let _ = res;
                let idx = scopes.len();
                scopes.push(HashMap::new());
                scope_stack.push(idx);
                self.bind_in_stack(var, None, *span, scopes, scope_stack)?;
                if let Some(v2) = var2 {
                    self.bind_in_stack(v2, None, *span, scopes, scope_stack)?;
                }
                self.loop_depth += 1;
                self.check_stmts_with_scopes(body, scopes, scope_stack, param_slots, ret_slot)?;
                self.loop_depth -= 1;
                scope_stack.pop();
                scopes.pop();
                Ok(())
            }
            Stmt::Return { value, span } => {
                self.has_return = true;
                match value {
                    Some(e) => {
                        let res = self.check_expr_in_fn(e, scopes, scope_stack, param_slots, ret_slot)?;
                        self.unify_slot(ret_slot, res, *span, "the function's return type".to_string())?;
                    }
                    None => {
                        self.unify_slot_ty(ret_slot, Ty::Void, *span, "the function's return type".to_string())?;
                    }
                }
                Ok(())
            }
            Stmt::FnDef { .. } => Ok(()),
            Stmt::StructDef { .. } => Ok(()),
            Stmt::ClassDef { .. } => Ok(()), // 类定义仅注册，方法体经 check_all 校验 // 顶层已注册，函数体内不新增
            Stmt::ExprStmt { expr, .. } => {
                self.check_expr_in_fn(expr, scopes, scope_stack, param_slots, ret_slot)?;
                Ok(())
            }
            Stmt::Break { span } => {
                if self.loop_depth == 0 {
                    return Err(self.zerr(
                        codes::SYNTAX,
                        "`break` is only allowed inside a loop",
                        *span,
                        Some("move the `break` into a `while`, `do-while` or `for` body"),
                    ));
                }
                Ok(())
            }
            Stmt::Continue { span } => {
                if self.loop_depth == 0 {
                    return Err(self.zerr(
                        codes::SYNTAX,
                        "`continue` is only allowed inside a loop",
                        *span,
                        Some("move the `continue` into a `while`, `do-while` or `for` body"),
                    ));
                }
                Ok(())
            }
            Stmt::Breakpoint { .. } => Ok(()),
            Stmt::Export { .. } => Ok(()), // @export 只在顶层检查
            Stmt::Import { span, .. } => {
                self.has_external = true;
                let _ = span;
                Ok(())
            }
            Stmt::Load { alias, from, sigs, .. } => {
                self.has_external = true;
                self.register_ffi_sigs(alias.as_deref(), from.as_deref(), sigs)
            }
            Stmt::Alias { .. } => {
                self.has_external = true;
                Ok(())
            }
            Stmt::Use { .. } => Ok(()),
            Stmt::Go { callee, args, span } => {
                let mut arg_tys = Vec::new();
                for a in args {
                    arg_tys.push(self.check_expr_in_fn(a, scopes, scope_stack, param_slots, ret_slot)?);
                }
                self.resolve_call(callee, &arg_tys, *span)?;
                Ok(())
            }
            Stmt::DebugPrint { expr, .. } => {
                self.check_expr_in_fn(expr, scopes, scope_stack, param_slots, ret_slot)?;
                Ok(())
            }
            Stmt::Try { body, catch_var, handler, span } => {
                let idx = scopes.len();
                scopes.push(HashMap::new());
                scope_stack.push(idx);
                self.check_stmts_with_scopes(body, scopes, scope_stack, param_slots, ret_slot)?;
                scope_stack.pop();
                scopes.pop();
                // handler 作用域：catch 变量固定为 error 类型
                let idx = scopes.len();
                scopes.push(HashMap::new());
                scope_stack.push(idx);
                self.bind_in_stack(catch_var, Some(Ty::Error), *span, scopes, scope_stack)?;
                self.check_stmts_with_scopes(handler, scopes, scope_stack, param_slots, ret_slot)?;
                scope_stack.pop();
                scopes.pop();
                Ok(())
            }
            Stmt::Throw { value, span } => {
                let res = self.check_expr_in_fn(value, scopes, scope_stack, param_slots, ret_slot)?;
                self.check_throw_value(res, *span)
            }
        }
    }

    // ---------- 表达式检查（全局作用域） ----------

    fn check_expr(&mut self, e: &Expr) -> Result<TyRes, ZError> {
        match e {
            Expr::Ident { name, span } => match self.globals.get(name) {
                Some(slot) => Ok(self.res_from_slot(*slot)),
                None => Err(self.zerr(
                    codes::UNDEFINED,
                    format!("undefined variable `{}`", name),
                    *span,
                    Some("declare the variable before reading it"),
                )),
            },
            Expr::Call { callee, args, span } => {
                let mut arg_tys = Vec::new();
                for a in args {
                    arg_tys.push(self.check_expr(a)?);
                }
                // 变量名调用：callee 为已声明的变量（lambda 闭包）→ 动态调用；
                // 类型未定（lambda 为 Unknown）放行，已定且非函数则报错
                if let Some(slot) = self.globals.get(callee) {
                    let ty = self.slots[*slot].get().unwrap_or(Ty::Unknown);
                    if ty == Ty::Unknown {
                        return Ok(TyRes { ty: Ty::Unknown, slot: Some(*slot) });
                    }
                    return Err(self.zerr(
                        codes::TYPE_MISMATCH,
                        format!("`{}` is `{}`, not a function", callee, ty.name()),
                        *span,
                        Some("assign a lambda to the variable, or call a defined function"),
                    ));
                }
                self.resolve_call(callee, &arg_tys, *span)
            }
            Expr::Match { value, arms, .. } => {
                // 模式匹配：值可为任意类型，各分支类型可不同 → 动态类型
                self.check_expr(value)?;
                for (pat, body) in arms {
                    if let Some(p) = pat {
                        self.check_expr(p)?;
                    }
                    self.check_expr(body)?;
                }
                Ok(TyRes { ty: Ty::Unknown, slot: None })
            }
            Expr::IncDec { name, span, op, .. } => {
                match self.globals.get(name) {
                    Some(slot) => {
                        let cur = self.slots[*slot].get().unwrap_or(Ty::Unknown);
                        if cur.is_numeric() || cur == Ty::Unknown {
                            Ok(TyRes { ty: cur, slot: Some(*slot) })
                        } else {
                            Err(self.zerr(
                                codes::TYPE_MISMATCH,
                                format!("`{}` requires a numeric variable, `{}` is `{}`", op.symbol(), name, cur.name()),
                                *span,
                                Some("increment/decrement works on `int` / `float`; convert the value with `to_int` / `to_float` if needed"),
                            ))
                        }
                    }
                    None => Err(self.zerr(
                        codes::UNDEFINED,
                        format!("undefined variable `{}`", name),
                        *span,
                        Some("declare the variable before incrementing or decrementing it"),
                    )),
                }
            }
            Expr::Ternary { cond, then_expr, else_expr, span } => {
                let c = self.check_expr(cond)?;
                self.require_bool(c, span, "ternary condition")?;
                let t = self.check_expr(then_expr)?;
                let e = self.check_expr(else_expr)?;
                // 两分支类型相同则返回该类型，否则动态类型（与 match 一致）
                if t.ty != Ty::Unknown && t.ty == e.ty {
                    Ok(TyRes { ty: t.ty, slot: None })
                } else {
                    Ok(TyRes { ty: Ty::Unknown, slot: None })
                }
            }
            Expr::Lambda { params, body, .. } => {
                // 顶层 lambda：独立作用域，全局变量可见（种子为 globals 拷贝）
                let mut scopes = vec![self.globals.clone()];
                let mut scope_stack = vec![0usize];
                self.check_lambda_body(params, body, &mut scopes, &mut scope_stack)?;
                Ok(TyRes { ty: Ty::Unknown, slot: None })
            }
            Expr::Binary { op, lhs, rhs, span } => {
                let l = self.check_expr(lhs)?;
                let r = self.check_expr(rhs)?;
                self.check_binary(*op, l, r, *span)
            }
            Expr::Field { obj, field, span } => {
                let res = self.check_expr(obj)?;
                self.check_field(res, field, *span)
            }
            Expr::Unary { op, expr, span } => {
                let res = self.check_expr(expr)?;
                self.check_unary(*op, res, *span)
            }
            Expr::IntLit(..) => Ok(TyRes { ty: Ty::Int, slot: None }),
            Expr::FloatLit(..) => Ok(TyRes { ty: Ty::Float, slot: None }),
            Expr::BoolLit(..) => Ok(TyRes { ty: Ty::Bool, slot: None }),
            Expr::StrLit(..) => Ok(TyRes { ty: Ty::Str, slot: None }),
            Expr::ListLit(items, _) => {
                for it in items {
                    self.check_expr(it)?;
                }
                Ok(TyRes { ty: Ty::Unknown, slot: None })
            }
            Expr::DictLit(entries, _) => {
                for (_, v) in entries {
                    self.check_expr(v)?;
                }
                Ok(TyRes { ty: Ty::Unknown, slot: None })
            }
            Expr::FStr(segs, _) => {
                for seg in segs {
                    if let FStrSeg::Code(e) = seg {
                        self.check_expr(e)?;
                    }
                }
                Ok(TyRes { ty: Ty::Str, slot: None })
            }
        }
    }

    // ---------- 表达式检查（函数体内） ----------

    fn check_expr_in_fn(
        &mut self,
        e: &Expr,
        scopes: &mut Vec<HashMap<String, usize>>,
        scope_stack: &mut Vec<usize>,
        param_slots: &[usize],
        ret_slot: usize,
    ) -> Result<TyRes, ZError> {
        match e {
            Expr::Ident { name, span } => match lookup_in_stack(name, scopes, scope_stack) {
                Some(slot) => Ok(self.res_from_slot(slot)),
                None => Err(self.zerr(
                    codes::UNDEFINED,
                    format!("undefined variable `{}`", name),
                    *span,
                    Some("declare the variable before reading it"),
                )),
            },
            Expr::Call { callee, args, span } => {
                let mut arg_tys = Vec::new();
                for a in args {
                    arg_tys.push(self.check_expr_in_fn(a, scopes, scope_stack, param_slots, ret_slot)?);
                }
                // 变量名调用：callee 为栈中已声明的变量（lambda 闭包）→ 动态调用
                if let Some(slot) = lookup_in_stack(callee, scopes, scope_stack) {
                    let ty = self.slots[slot].get().unwrap_or(Ty::Unknown);
                    if ty == Ty::Unknown {
                        return Ok(TyRes { ty: Ty::Unknown, slot: Some(slot) });
                    }
                    return Err(self.zerr(
                        codes::TYPE_MISMATCH,
                        format!("`{}` is `{}`, not a function", callee, ty.name()),
                        *span,
                        Some("assign a lambda to the variable, or call a defined function"),
                    ));
                }
                self.resolve_call(callee, &arg_tys, *span)
            }
            Expr::Match { value, arms, .. } => {
                self.check_expr_in_fn(value, scopes, scope_stack, param_slots, ret_slot)?;
                for (pat, body) in arms {
                    if let Some(p) = pat {
                        self.check_expr_in_fn(p, scopes, scope_stack, param_slots, ret_slot)?;
                    }
                    self.check_expr_in_fn(body, scopes, scope_stack, param_slots, ret_slot)?;
                }
                Ok(TyRes { ty: Ty::Unknown, slot: None })
            }
            Expr::IncDec { name, span, op, .. } => {
                match lookup_in_stack(name, scopes, scope_stack) {
                    Some(slot) => {
                        let cur = self.slots[slot].get().unwrap_or(Ty::Unknown);
                        if cur.is_numeric() || cur == Ty::Unknown {
                            Ok(TyRes { ty: cur, slot: Some(slot) })
                        } else {
                            Err(self.zerr(
                                codes::TYPE_MISMATCH,
                                format!("`{}` requires a numeric variable, `{}` is `{}`", op.symbol(), name, cur.name()),
                                *span,
                                Some("increment/decrement works on `int` / `float`; convert the value with `to_int` / `to_float` if needed"),
                            ))
                        }
                    }
                    None => Err(self.zerr(
                        codes::UNDEFINED,
                        format!("undefined variable `{}`", name),
                        *span,
                        Some("declare the variable before incrementing or decrementing it"),
                    )),
                }
            }
            Expr::Ternary { cond, then_expr, else_expr, span } => {
                let c = self.check_expr_in_fn(cond, scopes, scope_stack, param_slots, ret_slot)?;
                self.require_bool(c, span, "ternary condition")?;
                let t = self.check_expr_in_fn(then_expr, scopes, scope_stack, param_slots, ret_slot)?;
                let e = self.check_expr_in_fn(else_expr, scopes, scope_stack, param_slots, ret_slot)?;
                if t.ty != Ty::Unknown && t.ty == e.ty {
                    Ok(TyRes { ty: t.ty, slot: None })
                } else {
                    Ok(TyRes { ty: Ty::Unknown, slot: None })
                }
            }
            Expr::Lambda { params, body, .. } => {
                // 函数内 lambda：推入新作用域（可捕获外围局部变量），循环深度归零
                self.check_lambda_body(params, body, scopes, scope_stack)?;
                Ok(TyRes { ty: Ty::Unknown, slot: None })
            }
            Expr::Binary { op, lhs, rhs, span } => {
                let l = self.check_expr_in_fn(lhs, scopes, scope_stack, param_slots, ret_slot)?;
                let r = self.check_expr_in_fn(rhs, scopes, scope_stack, param_slots, ret_slot)?;
                self.check_binary(*op, l, r, *span)
            }
            Expr::Field { obj, field, span } => {
                let res = self.check_expr_in_fn(obj, scopes, scope_stack, param_slots, ret_slot)?;
                self.check_field(res, field, *span)
            }
            Expr::Unary { op, expr, span } => {
                let res = self.check_expr_in_fn(expr, scopes, scope_stack, param_slots, ret_slot)?;
                self.check_unary(*op, res, *span)
            }
            Expr::IntLit(..) => Ok(TyRes { ty: Ty::Int, slot: None }),
            Expr::FloatLit(..) => Ok(TyRes { ty: Ty::Float, slot: None }),
            Expr::BoolLit(..) => Ok(TyRes { ty: Ty::Bool, slot: None }),
            Expr::StrLit(..) => Ok(TyRes { ty: Ty::Str, slot: None }),
            Expr::ListLit(items, _) => {
                for it in items {
                    self.check_expr_in_fn(it, scopes, scope_stack, param_slots, ret_slot)?;
                }
                Ok(TyRes { ty: Ty::Unknown, slot: None })
            }
            Expr::DictLit(entries, _) => {
                for (_, v) in entries {
                    self.check_expr_in_fn(v, scopes, scope_stack, param_slots, ret_slot)?;
                }
                Ok(TyRes { ty: Ty::Unknown, slot: None })
            }
            Expr::FStr(segs, _) => {
                for seg in segs {
                    if let FStrSeg::Code(e) = seg {
                        self.check_expr_in_fn(e, scopes, scope_stack, param_slots, ret_slot)?;
                    }
                }
                Ok(TyRes { ty: Ty::Str, slot: None })
            }
        }
    }

    // ---------- try/throw 辅助 ----------

    /// throw 的表达式必须是 str（转为用户错误）或 error（原样重抛）。
    fn check_throw_value(&self, res: TyRes, span: Span) -> Result<(), ZError> {
        match res.ty {
            Ty::Str | Ty::Error | Ty::Unknown => Ok(()),
            other => Err(self.zerr(
                codes::TYPE_MISMATCH,
                format!("`throw` accepts a `str` or `error`, got `{}`", other.name()),
                span,
                Some("throw a message string, or re-throw an `error` value"),
            )),
        }
    }

    /// error 类型字段访问检查：e.code / e.message / e.file / e.context → str；e.line / e.col → int。
    /// struct 实例 / dict 为动态类型（Unknown），字段访问放行，类型由运行期决定。
    fn check_field(&self, res: TyRes, field: &str, span: Span) -> Result<TyRes, ZError> {
        if res.ty == Ty::Unknown {
            return Ok(TyRes { ty: Ty::Unknown, slot: None });
        }
        if res.ty != Ty::Error {
            return Err(self.zerr(
                codes::TYPE_MISMATCH,
                format!(
                    "field access `.{}` requires an `error`, `struct`, or `dict` value, got `{}`",
                    field,
                    res.ty.name()
                ),
                span,
                Some("struct instances and dicts support field access; errors expose code/message/..."),
            ));
        }
        match field {
            "code" | "message" | "file" | "context" => Ok(TyRes { ty: Ty::Str, slot: None }),
            "line" | "col" => Ok(TyRes { ty: Ty::Int, slot: None }),
            other => Err(self.zerr(
                codes::UNDEFINED,
                format!("unknown error field `{}`", other),
                span,
                Some("error fields: code, message, file, line, col, context"),
            )),
        }
    }

    // ---------- 运算符与条件 ----------

    fn check_binary(&mut self, op: BinOp, l: TyRes, r: TyRes, span: Span) -> Result<TyRes, ZError> {
        match op {
            BinOp::And | BinOp::Or => {
                self.require_operand_bool(l, op, span)?;
                self.require_operand_bool(r, op, span)?;
                Ok(TyRes { ty: Ty::Bool, slot: None })
            }
            BinOp::Eq | BinOp::Ne => {
                self.unify_pair(l, r, span, op.symbol())?;
                Ok(TyRes { ty: Ty::Bool, slot: None })
            }
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                self.unify_numeric(l, r, span, op.symbol())?;
                Ok(TyRes { ty: Ty::Bool, slot: None })
            }
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
                let ty = self.unify_arith(l, r, span, op)?;
                Ok(TyRes { ty, slot: None })
            }
            BinOp::Coalesce => {
                // a ?? b：a 为 null 时取 b。Hone 无 null 字面量，null 仅来自 void 调用，
                // 静态类型无法确定，放行并返回动态类型（运行期校验）
                Ok(TyRes { ty: Ty::Unknown, slot: None })
            }
        }
    }

    /// 复合赋值类型检查：`x op= y` 等价于 `x = x op y`，要求 x 已声明且与 y 类型兼容。
    /// str 仅支持 +=（字符串拼接）；其余运算符要求数字。
    fn check_compound_assign(
        &mut self,
        slot: usize,
        cur: Ty,
        op: CompoundOp,
        rhs: TyRes,
        span: Span,
        name: &str,
    ) -> Result<(), ZError> {
        if cur == Ty::Str {
            if op != CompoundOp::Add {
                return Err(self.zerr(
                    codes::TYPE_MISMATCH,
                    format!("`{}` cannot be applied to `str` variable `{}` (only `+=` supports str concatenation)", op.symbol(), name),
                    span,
                    None::<&str>,
                ));
            }
            return self.unify_with(Ty::Str, rhs, span, format!("variable `{}`", name));
        }
        if cur == Ty::Unknown {
            // 类型未定：按普通赋值绑定右侧类型（运行期校验由解释器保证）
            return self.unify_slot(slot, rhs, span, format!("variable `{}`", name));
        }
        if !cur.is_numeric() {
            return Err(self.zerr(
                codes::TYPE_MISMATCH,
                format!("`{}` requires a numeric variable, `{}` is `{}`", op.symbol(), name, cur.name()),
                span,
                Some(format!(
                    "`{}` only works on `int` / `float` (and `+=` also concatenates `str`); convert the value with `to_int` / `to_float` if needed",
                    op.symbol()
                )),
            ));
        }
        self.unify_numeric(TyRes { ty: cur, slot: Some(slot) }, rhs, span, op.symbol())?;
        Ok(())
    }

    /// 检查 lambda 函数体：独立返回槽、独立作用域（可访问外围变量与顶层全局变量）、
    /// 循环深度归零（break/continue 不能跨 lambda 逃逸）。
    fn check_lambda_body(
        &mut self,
        params: &[Param],
        body: &[Stmt],
        scopes: &mut Vec<HashMap<String, usize>>,
        scope_stack: &mut Vec<usize>,
    ) -> Result<(), ZError> {
        // 默认参数必须连续位于参数列表尾部
        let mut seen_default = false;
        for p in params {
            if p.default.is_some() {
                seen_default = true;
            } else if seen_default {
                return Err(self.zerr(
                    codes::SYNTAX,
                    format!("parameter `{}` has no default but follows a defaulted parameter", p.name),
                    p.span,
                    Some("move all parameters without defaults before the first defaulted parameter"),
                ));
            }
        }
        let ret_slot = self.new_slot();
        let idx = scopes.len();
        scopes.push(HashMap::new());
        scope_stack.push(idx);
        for p in params {
            self.bind_in_stack(&p.name, p.ty.clone().map(Ty::from_annot), p.span, scopes, scope_stack)?;
        }
        // 默认参数表达式：可引用其前面的参数（lambda 作用域中检查）。
        // 默认值类型确定 → 直接写入槽位（含 strict 阶段：默认值等价于显式类型声明，
        // 否则 strict 阶段的全新参数槽无法获得类型，导致歧义误报）。
        for p in params {
            if let Some(de) = &p.default {
                let res = self.check_expr_in_fn(de, scopes, scope_stack, &[], ret_slot)?;
                if res.ty != Ty::Unknown {
                    if let Some(slot) = lookup_in_stack(&p.name, scopes, scope_stack) {
                        match self.slots[slot].get() {
                            None => {
                                self.slots[slot].set(Some(res.ty));
                                self.changed = true;
                            }
                            Some(c) if c != res.ty => {
                                return Err(self.zerr(
                                    codes::TYPE_MISMATCH,
                                    format!(
                                        "type mismatch: default value of parameter `{}` is `{}`, but it is locked to `{}`",
                                        p.name,
                                        res.ty.name(),
                                        c.name()
                                    ),
                                    p.span,
                                    Some("make the default value match the parameter type"),
                                ));
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        let saved_loop = self.loop_depth;
        let saved_return = self.has_return;
        self.loop_depth = 0;
        let r = self.check_stmts_with_scopes(body, scopes, scope_stack, &[], ret_slot);
        self.loop_depth = saved_loop;
        self.has_return = saved_return;
        scope_stack.pop();
        scopes.pop();
        r
    }

    fn require_operand_bool(&mut self, res: TyRes, op: BinOp, span: Span) -> Result<(), ZError> {
        match res.ty {
            Ty::Bool => Ok(()),
            Ty::Unknown => {
                if let Some(slot) = res.slot {
                    if !self.strict {
                        self.unify_slot_ty(slot, Ty::Bool, span, format!("`{}` operand", op.symbol()))?;
                    }
                    return Ok(());
                }
                if self.strict {
                    return Err(self.zerr(
                        codes::TYPE_MISMATCH,
                        format!("`{}` requires `bool` operands, but the type cannot be determined", op.symbol()),
                        span,
                        Some("add explicit type annotations"),
                    ));
                }
                Ok(())
            }
            other => Err(self.zerr(
                codes::TYPE_MISMATCH,
                format!("`{}` requires `bool` operands, got `{}`", op.symbol(), other.name()),
                span,
                Some("use `==`/`!=`/`<`/`>` to build a boolean expression"),
            )),
        }
    }

    /// 校验条件表达式为 bool。`what` 描述条件上下文（如 "`if` condition"），用于报错定位。
    fn require_bool(&mut self, res: TyRes, span: &Span, what: &str) -> Result<(), ZError> {
        match res.ty {
            Ty::Bool => Ok(()),
            Ty::Unknown => {
                if let Some(slot) = res.slot {
                    if !self.strict {
                        self.unify_slot_ty(slot, Ty::Bool, *span, "condition".to_string())?;
                    }
                    return Ok(());
                }
                Err(self.zerr(
                    codes::COND_NOT_BOOL,
                    format!("{} must be `bool`, but its type cannot be determined", what),
                    *span,
                    Some("the condition must explicitly evaluate to a `bool`"),
                ))
            }
            other => Err(self.zerr(
                codes::COND_NOT_BOOL,
                format!("{} must be `bool`, got `{}`", what, other.name()),
                *span,
                Some("use a comparison like `x == 1`, or a boolean variable"),
            )),
        }
    }

    /// 相等性：要求两侧类型一致（未知槽位可被强制）。
    fn unify_pair(&mut self, l: TyRes, r: TyRes, span: Span, sym: &str) -> Result<(), ZError> {
        match (l.ty, r.ty) {
            (Ty::Unknown, Ty::Unknown) => {
                if let (Some(a), Some(b)) = (l.slot, r.slot) {
                    if a == b {
                        return Ok(());
                    }
                }
                // 两侧类型均未定：不做强制（strict 时运行期校验，此处放行）
                Ok(())
            }
            (Ty::Unknown, t) => {
                if let Some(slot) = l.slot {
                    if !self.strict {
                        self.unify_slot_ty(slot, t, span, format!("`{}` operand", sym))?;
                    }
                    return Ok(());
                }
                if self.strict {
                    return Err(self.zerr(
                        codes::TYPE_MISMATCH,
                        format!("cannot compare with `{}` using `{}`", t.name(), sym),
                        span,
                        Some("add explicit type annotations"),
                    ));
                }
                Ok(())
            }
            (t, Ty::Unknown) => {
                if let Some(slot) = r.slot {
                    if !self.strict {
                        self.unify_slot_ty(slot, t, span, format!("`{}` operand", sym))?;
                    }
                    return Ok(());
                }
                if self.strict {
                    return Err(self.zerr(
                        codes::TYPE_MISMATCH,
                        format!("cannot compare `{}` with `{}`", t.name(), sym),
                        span,
                        Some("add explicit type annotations"),
                    ));
                }
                Ok(())
            }
            (a, b) if a == b => Ok(()),
            (a, b) => Err(self.zerr(
                codes::TYPE_MISMATCH,
                format!("cannot compare `{}` with `{}` using `{}`", a.name(), b.name(), sym),
                span,
                Some("Hone has no implicit type conversion; make both sides the same type"),
            )),
        }
    }

    /// 比较运算：要求两侧为数值且类型一致。
    fn unify_numeric(&mut self, l: TyRes, r: TyRes, span: Span, sym: &str) -> Result<(), ZError> {
        match (l.ty, r.ty) {
            (Ty::Unknown, Ty::Unknown) => {
                if self.strict {
                    return Err(self.zerr(
                        codes::TYPE_MISMATCH,
                        format!("`{}` requires numeric operands, but the types cannot be determined", sym),
                        span,
                        Some("add explicit type annotations"),
                    ));
                }
                Ok(())
            }
            (Ty::Unknown, t) if t.is_numeric() => {
                if let Some(slot) = l.slot {
                    if !self.strict {
                        self.unify_slot_ty(slot, t, span, format!("`{}` operand", sym))?;
                    }
                    return Ok(());
                }
                if self.strict {
                    return Err(self.zerr(
                        codes::TYPE_MISMATCH,
                        format!("cannot determine the type of the `{}` operand", sym),
                        span,
                        Some("add explicit type annotations"),
                    ));
                }
                Ok(())
            }
            (t, Ty::Unknown) if t.is_numeric() => {
                if let Some(slot) = r.slot {
                    if !self.strict {
                        self.unify_slot_ty(slot, t, span, format!("`{}` operand", sym))?;
                    }
                    return Ok(());
                }
                if self.strict {
                    return Err(self.zerr(
                        codes::TYPE_MISMATCH,
                        format!("cannot determine the type of the `{}` operand", sym),
                        span,
                        Some("add explicit type annotations"),
                    ));
                }
                Ok(())
            }
            (a, b) if a.is_numeric() && b.is_numeric() && a != b => Err(self.zerr(
                codes::TYPE_MISMATCH,
                format!("cannot compare `{}` with `{}` using `{}` (no implicit conversion)", a.name(), b.name(), sym),
                span,
                Some("convert one side with `to_int` / `to_float` first"),
            )),
            (a, _) if !a.is_numeric() => Err(self.zerr(
                codes::TYPE_MISMATCH,
                format!("`{}` requires numeric operands, got `{}`", sym, a.name()),
                span,
                Some("comparison operators work on `int` / `float`"),
            )),
            _ => Ok(()),
        }
    }

    /// 算术运算：同类型数值；+ 额外允许 str 拼接。
    fn unify_arith(&mut self, l: TyRes, r: TyRes, span: Span, op: BinOp) -> Result<Ty, ZError> {
        let sym = op.symbol();
        match (l.ty, r.ty) {
            (Ty::Unknown, Ty::Unknown) => {
                if self.strict {
                    if op == BinOp::Add {
                        return Err(self.zerr(
                            codes::AMBIGUOUS_OP,
                            "ambiguous `+`: could be `int`/`float` addition or `str` concatenation",
                            span,
                            Some("add explicit type annotations to the operands"),
                        ));
                    }
                    return Err(self.zerr(
                        codes::CANNOT_INFER,
                        format!("cannot infer the operand types of `{}`", sym),
                        span,
                        Some("add explicit type annotations"),
                    ));
                }
                Ok(Ty::Unknown)
            }
            (Ty::Unknown, t) => {
                if t.is_numeric() {
                    if let Some(slot) = l.slot {
                        if !self.strict {
                            self.unify_slot_ty(slot, t, span, format!("`{}` operand", sym))?;
                        }
                    }
                    return Ok(t);
                }
                if op == BinOp::Add && t == Ty::Str {
                    if let Some(slot) = l.slot {
                        if !self.strict {
                            self.unify_slot_ty(slot, Ty::Str, span, "`+` operand".to_string())?;
                        }
                    }
                    return Ok(Ty::Str);
                }
                Err(self.zerr(
                    codes::TYPE_MISMATCH,
                    format!("cannot apply `{}` to `{}`", sym, t.name()),
                    span,
                    Some(format!(
                        "`{}` works on numbers{}",
                        sym,
                        if op == BinOp::Add { " and `+` also concatenates strings" } else { "" }
                    )),
                ))
            }
            (t, Ty::Unknown) => {
                if t.is_numeric() {
                    if let Some(slot) = r.slot {
                        if !self.strict {
                            self.unify_slot_ty(slot, t, span, format!("`{}` operand", sym))?;
                        }
                    }
                    return Ok(t);
                }
                if op == BinOp::Add && t == Ty::Str {
                    if let Some(slot) = r.slot {
                        if !self.strict {
                            self.unify_slot_ty(slot, Ty::Str, span, "`+` operand".to_string())?;
                        }
                    }
                    return Ok(Ty::Str);
                }
                Err(self.zerr(
                    codes::TYPE_MISMATCH,
                    format!("cannot apply `{}` to `{}`", sym, t.name()),
                    span,
                    Some("check the operand type"),
                ))
            }
            (a, b) if a.is_numeric() && b.is_numeric() && a != b => Err(self.zerr(
                codes::TYPE_MISMATCH,
                format!("cannot apply `{}` to `{}` and `{}` (no implicit conversion)", sym, a.name(), b.name()),
                span,
                Some("convert one side with `to_int` / `to_float` first"),
            )),
            (a, b) if a.is_numeric() && b.is_numeric() => Ok(a),
            (Ty::Str, Ty::Str) => {
                if op == BinOp::Add {
                    Ok(Ty::Str)
                } else {
                    Err(self.zerr(
                        codes::TYPE_MISMATCH,
                        format!("cannot apply `{}` to `str`", sym),
                        span,
                        Some("`+` concatenates strings; other arithmetic is numeric-only"),
                    ))
                }
            }
            (a, _) => Err(self.zerr(
                codes::TYPE_MISMATCH,
                format!("cannot apply `{}` to `{}`", sym, a.name()),
                span,
                Some("check the operand type"),
            )),
        }
    }

    fn check_unary(&mut self, op: UnOp, res: TyRes, span: Span) -> Result<TyRes, ZError> {
        match op {
            UnOp::Neg => match res.ty {
                Ty::Unknown => {
                    if self.strict {
                        return Err(self.zerr(
                            codes::CANNOT_INFER,
                            "cannot infer the type of the operand of unary `-`",
                            span,
                            Some("add an explicit type annotation"),
                        ));
                    }
                    Ok(res)
                }
                t if t.is_numeric() => Ok(TyRes { ty: t, slot: None }),
                other => Err(self.zerr(
                    codes::TYPE_MISMATCH,
                    format!("unary `-` requires a number, got `{}`", other.name()),
                    span,
                    Some("negation works on `int` / `float`"),
                )),
            },
            UnOp::Not => match res.ty {
                Ty::Bool => Ok(TyRes { ty: Ty::Bool, slot: None }),
                Ty::Unknown => {
                    if let Some(slot) = res.slot {
                        if !self.strict {
                            self.unify_slot_ty(slot, Ty::Bool, span, "`!` operand".to_string())?;
                        }
                        return Ok(TyRes { ty: Ty::Bool, slot: None });
                    }
                    if self.strict {
                        return Err(self.zerr(
                            codes::TYPE_MISMATCH,
                            "`!` requires a `bool` operand, but the type cannot be determined",
                            span,
                            Some("add explicit type annotations"),
                        ));
                    }
                    Ok(TyRes { ty: Ty::Unknown, slot: None })
                }
                other => Err(self.zerr(
                    codes::TYPE_MISMATCH,
                    format!("`!` requires a `bool` operand, got `{}`", other.name()),
                    span,
                    Some("logical NOT works on `bool` values"),
                )),
            },
        }
    }

    // ---------- 函数调用解析 ----------

    /// 注册 load 的 FFI 函数签名（键为完整调用名 "alias.fn"）。
    /// from 头文件解析出的签名先注册，签名块中的同名声明覆盖之。
    fn register_ffi_sigs(&mut self, alias: Option<&str>, from: Option<&str>, sigs: &[FfiSig]) -> Result<(), ZError> {
        if let Some(hpath) = from {
            let header_sigs = if let Some(cached) = self.header_cache.get(hpath) {
                cached.clone()
            } else {
                let src = std::fs::read_to_string(hpath).map_err(|e| {
                    self.zerr(
                        codes::NOT_FOUND,
                        format!("cannot read header `{}`: {}", hpath, e),
                        Span { line: 1, col: 1, len: 1 },
                        Some("check the header path, or remove the `from` clause"),
                    )
                })?;
                let sigs = crate::header::parse(&src);
                self.header_cache.insert(hpath.to_string(), sigs.clone());
                sigs
            };
            for sig in &header_sigs {
                let key = match alias {
                    Some(a) => format!("{}.{}", a, sig.name),
                    None => sig.name.clone(),
                };
                self.ffi_sigs.insert(key, sig.clone());
            }
        }
        for sig in sigs {
            let key = match alias {
                Some(a) => format!("{}.{}", a, sig.name),
                None => sig.name.clone(),
            };
            self.ffi_sigs.insert(key, sig.clone());
        }
        Ok(())
    }

    /// 校验对已声明签名的 FFI 函数的调用：参数个数与类型，返回声明的类型。
    fn check_ffi_call(&mut self, sig: &FfiSig, arg_tys: &[TyRes], span: Span) -> Result<TyRes, ZError> {
        // 头文件解析失败的原型（回调/变参/数组等）：调用时直接报错
        if let Some(reason) = sig.unsupported {
            return Err(self.zerr(
                codes::NOT_IMPLEMENTED,
                format!("`{}` cannot be called: {}", sig.name, reason),
                span,
                Some("declare a manual signature for this function, or use `ptr` for the unsupported parts"),
            ));
        }
        let shown = || {
            format!(
                "`fn {}({}) -> {}`",
                sig.name,
                sig.params
                    .iter()
                    .map(|p| format!("{}: {}", p.name, p.ty.name()))
                    .collect::<Vec<_>>()
                    .join(", "),
                sig.ret.name()
            )
        };
        if sig.params.len() != arg_tys.len() {
            return Err(self.zerr(
                codes::ARG_COUNT,
                format!(
                    "wrong number of arguments: `{}` expects {}, got {}",
                    sig.name,
                    sig.params.len(),
                    arg_tys.len()
                ),
                span,
                Some(format!("declared signature: {}", shown())),
            ));
        }
        for (p, aty) in sig.params.iter().zip(arg_tys) {
            let expected = match p.ty {
                FfiTy::Int => Some(Ty::Int),
                FfiTy::Float => Some(Ty::Float),
                FfiTy::Bool => Some(Ty::Bool),
                FfiTy::Str => Some(Ty::Str),
                // 不透明指针：静态阶段不限制（值为运行时地址）
                FfiTy::Ptr => None,
                // 参数不允许 void（parser 已拦截）
                FfiTy::Void => None,
            };
            if let Some(exp) = expected {
                if aty.ty != Ty::Unknown && aty.ty != exp {
                    return Err(self.zerr(
                        codes::TYPE_MISMATCH,
                        format!(
                            "`{}` parameter `{}` expects `{}`, got `{}`",
                            sig.name,
                            p.name,
                            p.ty.name(),
                            aty.ty.name()
                        ),
                        span,
                        Some(format!("declared signature: {}", shown())),
                    ));
                }
            }
        }
        let ty = match sig.ret {
            FfiTy::Int => Ty::Int,
            FfiTy::Float => Ty::Float,
            FfiTy::Bool => Ty::Bool,
            FfiTy::Str => Ty::Str,
            FfiTy::Ptr => Ty::Unknown,
            FfiTy::Void => Ty::Void,
        };
        Ok(TyRes { ty, slot: None })
    }

    /// 解析调用目标：用户函数 / 类方法 / 内置函数 / FFI 签名 / 未定义（H002）。
    fn resolve_call(&mut self, callee: &str, arg_tys: &[TyRes], span: Span) -> Result<TyRes, ZError> {
        // 类方法（类名.方法名）：成员函数不在全局符号表，只经类名限定调用。
        // 类名已注册但方法不存在 → 明确报错；类名未注册 → 落入下方常规解析
        // （内置点号函数如 time.now、load 库、import 模块等）。
        if let Some((cls, method)) = callee.split_once('.') {
            if let Some(info) = self.classes.get(cls).and_then(|m| m.get(method).cloned()) {
                return self.check_user_call(&info, arg_tys, span);
            }
            if self.classes.contains_key(cls) {
                return Err(self.zerr(
                    codes::UNDEFINED,
                    format!("class `{}` has no method `{}`", cls, method),
                    span,
                    Some("check the method name"),
                ));
            }
        }
        if let Some(f) = self.fns.get(callee).cloned() {
            self.check_user_call(&f, arg_tys, span)
        } else if let Some(fields) = self.structs.get(callee).cloned() {
            // 结构体构造：Point(1, 2) —— 校验字段个数与类型
            if fields.len() != arg_tys.len() {
                return Err(self.zerr(
                    codes::ARG_COUNT,
                    format!(
                        "wrong number of arguments: struct `{}` expects {} fields, got {}",
                        callee,
                        fields.len(),
                        arg_tys.len()
                    ),
                    span,
                    Some(format!(
                        "construct with `{}({})`",
                        callee,
                        fields.iter().map(|(f, _)| f.as_str()).collect::<Vec<_>>().join(", ")
                    )),
                ));
            }
            for ((fname, fty), aty) in fields.iter().zip(arg_tys) {
                self.unify_with(*fty, *aty, span, format!("field `{}` of struct `{}`", fname, callee))?;
            }
            // struct 实例为动态类型（字段访问在运行时校验，字段类型可查定义）
            Ok(TyRes { ty: Ty::Unknown, slot: None })
        } else if self.builtins.contains(callee) {
            self.builtin_result(callee, arg_tys, span)
        } else if let Some(sig) = self.ffi_sigs.get(callee).cloned() {
            // load 签名块声明的 FFI 函数：按签名静态校验参数，返回声明类型
            self.check_ffi_call(&sig, arg_tys, span)
        } else if self.has_external || callee.contains('.') {
            // 动态外部加载（未声明签名的 load 库函数 / import 模块函数 / alias 别名）：
            // 类型在运行时才能确定，静态阶段放行
            Ok(TyRes { ty: Ty::Unknown, slot: None })
        } else {
            Err(self.zerr(
                codes::UNDEFINED,
                format!("undefined function `{}`", callee),
                span,
                Some("define it with `fn`, or check the spelling"),
            ))
        }
    }

    fn check_user_call(&mut self, f: &FnInfo, arg_tys: &[TyRes], span: Span) -> Result<TyRes, ZError> {
        // 默认参数：可省略尾部实参（最少 = 无默认值参数个数，最多 = 总参数个数）
        let min = f.defaults.iter().filter(|d| d.is_none()).count();
        let max = f.param_slots.len();
        if arg_tys.len() < min || arg_tys.len() > max {
            // 签名展示：可选参数标注 `= ...`
            let shown: Vec<String> = f
                .params
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    if f.defaults[i].is_some() {
                        format!("{} = ...", p)
                    } else {
                        p.clone()
                    }
                })
                .collect();
            let expected = if min == max {
                min.to_string()
            } else {
                format!("{} to {}", min, max)
            };
            return Err(self.zerr(
                codes::ARG_COUNT,
                format!(
                    "wrong number of arguments: `{}` expects {}, got {}",
                    f.name,
                    expected,
                    arg_tys.len()
                ),
                span,
                Some(format!(
                    "call `{}({})` ({} {} optional)",
                    f.name,
                    shown.join(", "),
                    max - min,
                    if max - min == 1 { "argument is" } else { "arguments are" }
                )),
            ));
        }
        // 泛型调用点实例化：为每个类型参数新建槽（旧类型参数槽 → 新槽映射）。
        // 使同一泛型函数可以不同类型多次调用，且互不锁定（编译期擦除，运行期零成本）。
        let mut fresh: HashMap<usize, usize> = HashMap::new();
        for old in &f.type_param_slots {
            fresh.insert(*old, self.new_slot());
        }
        let map_slot = |slot: usize| fresh.get(&slot).copied().unwrap_or(slot);
        for i in 0..arg_tys.len() {
            self.unify_slot(
                map_slot(f.param_slots[i]),
                arg_tys[i],
                span,
                format!("parameter `{}` of `{}`", f.params[i], f.name),
            )?;
        }
        Ok(self.res_from_slot(map_slot(f.ret_slot)))
    }

    fn res_from_slot(&self, slot: usize) -> TyRes {
        let ty = self.slots[slot].get().unwrap_or(Ty::Unknown);
        TyRes {
            ty,
            slot: if ty == Ty::Unknown { Some(slot) } else { None },
        }
    }

    // ---------- 类型统一 ----------

    /// 将表达式结果与期望类型统一。
    fn unify_with(&mut self, expected: Ty, res: TyRes, span: Span, what: String) -> Result<(), ZError> {
        match res.ty {
            Ty::Unknown => {
                if let Some(slot) = res.slot {
                    if !self.strict {
                        self.unify_slot_ty(slot, expected, span, what)?;
                    }
                    return Ok(());
                }
                // 结果类型无法确定（如函数返回值未解析）：不强制
                Ok(())
            }
            t if t == expected => Ok(()),
            t => Err(self.zerr(
                codes::TYPE_MISMATCH,
                format!("type mismatch: expected `{}`, got `{}` for {}", expected.name(), t.name(), what),
                span,
                Some("Hone has no implicit type conversion"),
            )),
        }
    }

    /// 将表达式结果与槽位统一；结果未知但带槽位时按目标槽位反向强制。
    fn unify_slot(&mut self, slot: usize, res: TyRes, span: Span, what: String) -> Result<(), ZError> {
        match res.ty {
            Ty::Unknown => {
                if let Some(s) = res.slot {
                    if s == slot {
                        return Ok(());
                    }
                    // 目标槽位已知、来源槽位未知 → 用目标类型强制来源
                    let cur = self.slots[slot].get();
                    if let Some(t) = cur {
                        if !self.strict {
                            self.unify_slot_ty(s, t, span, what)?;
                        }
                    }
                }
                Ok(())
            }
            t => self.unify_slot_ty(slot, t, span, what),
        }
    }

    fn unify_slot_ty(&mut self, slot: usize, t: Ty, span: Span, what: String) -> Result<(), ZError> {
        let cur = self.slots[slot].get();
        match cur {
            None => {
                if !self.strict {
                    self.slots[slot].set(Some(t));
                    self.changed = true;
                }
                Ok(())
            }
            Some(c) if c == t => Ok(()),
            Some(c) => Err(self.zerr(
                codes::TYPE_MISMATCH,
                format!("type mismatch: {} is locked to `{}`, got `{}`", what, c.name(), t.name()),
                span,
                Some("Hone types are locked after inference; no implicit conversion is allowed"),
            )),
        }
    }

    // ---------- 内置函数签名 ----------

    fn arg_count(&self, name: &str, n: usize, want: usize, span: Span) -> Result<(), ZError> {
        if n == want {
            Ok(())
        } else {
            Err(self.zerr(
                codes::ARG_COUNT,
                format!("wrong number of arguments: `{}` expects {}, got {}", name, want, n),
                span,
                Some("check the function signature"),
            ))
        }
    }

    fn expect_str(&mut self, name: &str, args: &[TyRes], i: usize, span: Span, what: &str) -> Result<(), ZError> {
        match args[i].ty {
            Ty::Str => Ok(()),
            Ty::Unknown => {
                if let Some(slot) = args[i].slot {
                    if !self.strict {
                        self.unify_slot_ty(slot, Ty::Str, span, what.to_string())?;
                    }
                    return Ok(());
                }
                if self.strict {
                    return Err(self.zerr(
                        codes::CANNOT_INFER,
                        format!("cannot determine the type of {}, expected `str`", what),
                        span,
                        Some("add an explicit type annotation"),
                    ));
                }
                Ok(())
            }
            other => Err(self.zerr(
                codes::TYPE_MISMATCH,
                format!("`{}` expects `str` for {}, got `{}`", name, what, other.name()),
                span,
                Some("pass a string value"),
            )),
        }
    }

    fn expect_int(&mut self, name: &str, args: &[TyRes], i: usize, span: Span, what: &str) -> Result<(), ZError> {
        match args[i].ty {
            Ty::Int => Ok(()),
            Ty::Unknown => {
                if let Some(slot) = args[i].slot {
                    if !self.strict {
                        self.unify_slot_ty(slot, Ty::Int, span, what.to_string())?;
                    }
                    return Ok(());
                }
                if self.strict {
                    return Err(self.zerr(
                        codes::CANNOT_INFER,
                        format!("cannot determine the type of {}, expected `int`", what),
                        span,
                        Some("add an explicit type annotation"),
                    ));
                }
                Ok(())
            }
            other => Err(self.zerr(
                codes::TYPE_MISMATCH,
                format!("`{}` expects `int` for {}, got `{}`", name, what, other.name()),
                span,
                Some("pass an integer value"),
            )),
        }
    }

    /// 接受 int 或 float（数值）。用于允许 `int` 字面量通过 `float` 参数的场景。
    fn expect_numeric(&mut self, name: &str, args: &[TyRes], i: usize, span: Span, what: &str) -> Result<(), ZError> {
        match args[i].ty {
            Ty::Int | Ty::Float => Ok(()),
            Ty::Unknown => {
                if let Some(slot) = args[i].slot {
                    if !self.strict {
                        self.unify_slot_ty(slot, Ty::Float, span, what.to_string())?;
                    }
                    return Ok(());
                }
                Ok(())
            }
            other => Err(self.zerr(
                codes::TYPE_MISMATCH,
                format!("`{}` expects a number for {}, got `{}`", name, what, other.name()),
                span,
                Some("pass an integer or float value"),
            )),
        }
    }

    /// 接受任意类型（含 Unknown/动态类型）。用于不透明指针等静态阶段无法确定的参数。
    fn expect_any(&mut self, _name: &str, _args: &[TyRes], _i: usize, _span: Span, _what: &str) -> Result<(), ZError> {
        Ok(())
    }

    /// 内置函数签名检查，返回调用结果的类型。
    /// json_parse 返回动态类型（Unknown，无槽位）。
    fn builtin_result(&mut self, name: &str, args: &[TyRes], span: Span) -> Result<TyRes, ZError> {
        let n = args.len();
        match name {
            "print" => {
                self.arg_count(name, n, 1, span)?;
                Ok(TyRes { ty: Ty::Void, slot: None })
            }
            "len" => {
                self.arg_count(name, n, 1, span)?;
                Ok(TyRes { ty: Ty::Int, slot: None })
            }
            "append" => {
                self.arg_count(name, n, 2, span)?;
                // 列表是动态类型，返回类型未知
                Ok(TyRes { ty: Ty::Unknown, slot: None })
            }
            "clone" | "copy" => {
                self.arg_count(name, n, 1, span)?;
                // 深度拷贝：返回类型与原值一致（集合为动态类型）
                Ok(args[0])
            }
            "contains" => {
                self.arg_count(name, n, 2, span)?;
                Ok(TyRes { ty: Ty::Bool, slot: None })
            }
            "index_of" => {
                self.arg_count(name, n, 2, span)?;
                Ok(TyRes { ty: Ty::Int, slot: None })
            }
            "keys" | "values" => {
                self.arg_count(name, n, 1, span)?;
                Ok(TyRes { ty: Ty::Unknown, slot: None })
            }
            "has_key" => {
                self.arg_count(name, n, 2, span)?;
                self.expect_str(name, args, 1, span, "the key")?;
                Ok(TyRes { ty: Ty::Bool, slot: None })
            }
            "is_int" | "is_float" | "is_str" | "is_bool" | "is_list" | "is_dict" | "is_null" => {
                self.arg_count(name, n, 1, span)?;
                Ok(TyRes { ty: Ty::Bool, slot: None })
            }
            "type_of" | "to_str" | "json_stringify" => {
                self.arg_count(name, n, 1, span)?;
                Ok(TyRes { ty: Ty::Str, slot: None })
            }
            "assert" => {
                if !(1..=2).contains(&n) {
                    return Err(self.zerr(
                        codes::ARG_COUNT,
                        format!("wrong number of arguments: `assert` expects 1-2 (condition[, message]), got {}", n),
                        span,
                        Some("form: assert(condition) or assert(condition, \"message\")"),
                    ));
                }
                self.require_bool(args[0], &span, "`assert` condition")?;
                if n == 2 {
                    self.expect_str(name, args, 1, span, "the assertion message")?;
                }
                Ok(TyRes { ty: Ty::Void, slot: None })
            }
            "to_int" => {
                self.arg_count(name, n, 1, span)?;
                Ok(TyRes { ty: Ty::Int, slot: None })
            }
            "to_float" => {
                self.arg_count(name, n, 1, span)?;
                Ok(TyRes { ty: Ty::Float, slot: None })
            }
            // input / read_int / read_float：0-1 个参数（可选提示文本），从标准输入读取
            "input" | "read_int" | "read_float" => {
                if n > 1 {
                    return Err(self.zerr(
                        codes::ARG_COUNT,
                        format!("wrong number of arguments: `{}` expects 0-1 (optional prompt), got {}", name, n),
                        span,
                        Some("form: `input()` or `input(\"prompt\")`; prompt must be a string"),
                    ));
                }
                if n == 1 {
                    self.expect_str(name, args, 0, span, "the optional prompt")?;
                }
                let ty = match name {
                    "input" => Ty::Str,
                    "read_int" => Ty::Int,
                    _ => Ty::Float,
                };
                Ok(TyRes { ty, slot: None })
            }
            "read_file" | "http_get" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_str(name, args, 0, span, "the URL/path")?;
                Ok(TyRes { ty: Ty::Str, slot: None })
            }
            "write_file" => {
                self.arg_count(name, n, 2, span)?;
                self.expect_str(name, args, 0, span, "the first argument")?;
                self.expect_str(name, args, 1, span, "the second argument")?;
                Ok(TyRes { ty: Ty::Void, slot: None })
            }
            "http_post" => {
                self.arg_count(name, n, 2, span)?;
                self.expect_str(name, args, 0, span, "the URL")?;
                self.expect_str(name, args, 1, span, "the request body")?;
                Ok(TyRes { ty: Ty::Str, slot: None })
            }
            "http.request" => {
                self.arg_count(name, n, 2, span)?;
                self.expect_str(name, args, 0, span, "the URL")?;
                self.expect_any(name, args, 1, span, "an options dict {method, headers, body, timeout}")?;
                Ok(TyRes { ty: Ty::Str, slot: None })
            }
            "smtp.send" => {
                self.arg_count(name, n, 3, span)?;
                self.expect_str(name, args, 0, span, "the SMTP host")?;
                self.expect_int(name, args, 1, span, "the port")?;
                self.expect_any(name, args, 2, span, "an options dict {from, to, subject, body, ...}")?;
                Ok(TyRes { ty: Ty::Bool, slot: None })
            }
            "ws.request" => {
                if n < 2 || n > 3 {
                    return Err(self.zerr(
                        codes::ARG_COUNT,
                        format!("wrong number of arguments: `ws.request` expects 2-3, got {}", n),
                        span,
                        Some("form: ws.request(url, message[, timeout])"),
                    ));
                }
                self.expect_str(name, args, 0, span, "the WebSocket URL")?;
                self.expect_str(name, args, 1, span, "the message")?;
                if n == 3 {
                    self.expect_any(name, args, 2, span, "the timeout (seconds)")?;
                }
                Ok(TyRes { ty: Ty::Str, slot: None })
            }
            "sys.run" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_str(name, args, 0, span, "the command")?;
                Ok(TyRes { ty: Ty::Str, slot: None })
            }
            "server.listen" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_int(name, args, 0, span, "the port number (0 = auto-assign)")?;
                Ok(TyRes { ty: Ty::Int, slot: None })
            }
            "server.poll" => {
                self.arg_count(name, n, 0, span)?;
                Ok(TyRes { ty: Ty::Str, slot: None })
            }
            "server.respond" => {
                if !(2..=3).contains(&n) {
                    return Err(self.zerr(
                        codes::ARG_COUNT,
                        format!("wrong number of arguments: `server.respond` expects 2-3 (id, body[, status]), got {}", n),
                        span,
                        Some("form: server.respond(id, body[, status])"),
                    ));
                }
                self.expect_int(name, args, 0, span, "the request id from `server.poll`")?;
                self.expect_str(name, args, 1, span, "the response body")?;
                if n == 3 {
                    self.expect_int(name, args, 2, span, "the HTTP status code (e.g. 404, 500)")?;
                }
                Ok(TyRes { ty: Ty::Bool, slot: None })
            }
            // ---------- ptr 指针类 ----------
            "ptr.alloc" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_int(name, args, 0, span, "the allocation size in bytes")?;
                // ptr 为动态类型（值为运行时地址）
                Ok(TyRes { ty: Ty::Unknown, slot: None })
            }
            "ptr.free" | "ptr.is_null" | "ptr.is_valid" | "ptr.size" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_any(name, args, 0, span, "a `ptr` value (or `0` for NULL)")?;
                let ty = if name == "ptr.size" { Ty::Int } else { Ty::Bool };
                Ok(TyRes { ty, slot: None })
            }
            "ptr.read_int" | "ptr.read_byte" => {
                self.arg_count(name, n, 2, span)?;
                self.expect_any(name, args, 0, span, "a `ptr` value")?;
                self.expect_int(name, args, 1, span, "the byte offset")?;
                Ok(TyRes { ty: Ty::Int, slot: None })
            }
            "ptr.read_float" => {
                self.arg_count(name, n, 2, span)?;
                self.expect_any(name, args, 0, span, "a `ptr` value")?;
                self.expect_int(name, args, 1, span, "the byte offset")?;
                Ok(TyRes { ty: Ty::Float, slot: None })
            }
            "ptr.write_int" | "ptr.write_byte" => {
                self.arg_count(name, n, 3, span)?;
                self.expect_any(name, args, 0, span, "a `ptr` value")?;
                self.expect_int(name, args, 1, span, "the byte offset")?;
                self.expect_int(name, args, 2, span, "the value to write")?;
                Ok(TyRes { ty: Ty::Void, slot: None })
            }
            "ptr.write_float" => {
                self.arg_count(name, n, 3, span)?;
                self.expect_any(name, args, 0, span, "a `ptr` value")?;
                self.expect_int(name, args, 1, span, "the byte offset")?;
                self.expect_numeric(name, args, 2, span, "the value to write")?;
                Ok(TyRes { ty: Ty::Void, slot: None })
            }
            "file_exists" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_str(name, args, 0, span, "the path")?;
                Ok(TyRes { ty: Ty::Bool, slot: None })
            }
            "str_contains" => {
                self.arg_count(name, n, 2, span)?;
                self.expect_str(name, args, 0, span, "the string")?;
                self.expect_str(name, args, 1, span, "the substring")?;
                Ok(TyRes { ty: Ty::Bool, slot: None })
            }
            "str_replace" => {
                self.arg_count(name, n, 3, span)?;
                for i in 0..3 {
                    self.expect_str(name, args, i, span, "a string argument")?;
                }
                Ok(TyRes { ty: Ty::Str, slot: None })
            }
            "str_trim" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_str(name, args, 0, span, "the string")?;
                Ok(TyRes { ty: Ty::Str, slot: None })
            }
            "abs" => {
                self.arg_count(name, n, 1, span)?;
                match args[0].ty {
                    t if t.is_numeric() => Ok(TyRes { ty: t, slot: None }),
                    Ty::Unknown => Ok(TyRes { ty: Ty::Unknown, slot: None }),
                    other => Err(self.zerr(
                        codes::TYPE_MISMATCH,
                        format!("`abs` expects a number, got `{}`", other.name()),
                        span,
                        Some("pass an `int` or `float`"),
                    )),
                }
            }
            "max" | "min" => {
                self.arg_count(name, n, 2, span)?;
                let a = args[0];
                let b = args[1];
                match (a.ty, b.ty) {
                    (x, y) if x.is_numeric() && y.is_numeric() && x == y => {
                        Ok(TyRes { ty: x, slot: None })
                    }
                    (x, y) if x.is_numeric() && y.is_numeric() && x != y => Err(self.zerr(
                        codes::TYPE_MISMATCH,
                        format!(
                            "`{}` requires two operands of the same type, got `{}` and `{}`",
                            name,
                            x.name(),
                            y.name()
                        ),
                        span,
                        Some("Hone has no implicit type conversion"),
                    )),
                    (Ty::Unknown, y) if y.is_numeric() => {
                        if let Some(s) = a.slot {
                            if !self.strict {
                                self.unify_slot_ty(s, y, span, "the first argument".to_string())?;
                            }
                        }
                        Ok(TyRes { ty: y, slot: None })
                    }
                    (x, Ty::Unknown) if x.is_numeric() => {
                        if let Some(s) = b.slot {
                            if !self.strict {
                                self.unify_slot_ty(s, x, span, "the second argument".to_string())?;
                            }
                        }
                        Ok(TyRes { ty: x, slot: None })
                    }
                    (Ty::Unknown, Ty::Unknown) => Ok(TyRes { ty: Ty::Unknown, slot: None }),
                    (x, _) => Err(self.zerr(
                        codes::TYPE_MISMATCH,
                        format!("`{}` expects numbers, got `{}`", name, x.name()),
                        span,
                        Some("pass two numbers of the same type"),
                    )),
                }
            }
            "time.now" => {
                self.arg_count(name, n, 0, span)?;
                Ok(TyRes { ty: Ty::Int, slot: None })
            }
            "time.sleep" => {
                self.arg_count(name, n, 1, span)?;
                match args[0].ty {
                    t if t.is_numeric() => Ok(TyRes { ty: Ty::Void, slot: None }),
                    Ty::Unknown => Ok(TyRes { ty: Ty::Void, slot: None }),
                    other => Err(self.zerr(
                        codes::TYPE_MISMATCH,
                        format!("`time.sleep` expects a number, got `{}`", other.name()),
                        span,
                        Some("pass an `int` or `float`"),
                    )),
                }
            }
            "time.format" => {
                self.arg_count(name, n, 2, span)?;
                self.expect_int(name, args, 0, span, "the timestamp")?;
                self.expect_str(name, args, 1, span, "the format")?;
                Ok(TyRes { ty: Ty::Str, slot: None })
            }
            "time.parse" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_str(name, args, 0, span, "the timestamp string")?;
                Ok(TyRes { ty: Ty::Int, slot: None })
            }
            "time.add" => {
                self.arg_count(name, n, 2, span)?;
                self.expect_int(name, args, 0, span, "the timestamp")?;
                self.expect_int(name, args, 1, span, "the seconds to add")?;
                Ok(TyRes { ty: Ty::Int, slot: None })
            }
            "time.diff" => {
                self.arg_count(name, n, 2, span)?;
                self.expect_int(name, args, 0, span, "the first timestamp")?;
                self.expect_int(name, args, 1, span, "the second timestamp")?;
                Ok(TyRes { ty: Ty::Int, slot: None })
            }
            "time.weekday" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_int(name, args, 0, span, "the timestamp")?;
                Ok(TyRes { ty: Ty::Int, slot: None })
            }
            "random.int" => {
                self.arg_count(name, n, 2, span)?;
                self.expect_int(name, args, 0, span, "the minimum")?;
                self.expect_int(name, args, 1, span, "the maximum")?;
                Ok(TyRes { ty: Ty::Int, slot: None })
            }
            "random.float" => {
                self.arg_count(name, n, 0, span)?;
                Ok(TyRes { ty: Ty::Float, slot: None })
            }
            "json_parse" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_str(name, args, 0, span, "the JSON string")?;
                // 动态类型：返回值类型由 JSON 内容决定
                Ok(TyRes { ty: Ty::Unknown, slot: None })
            }
            "sys.get_env" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_str(name, args, 0, span, "the environment variable name")?;
                Ok(TyRes { ty: Ty::Str, slot: None })
            }
            "sys.msgbox" => {
                self.arg_count(name, n, 3, span)?;
                for i in 0..3 {
                    self.expect_str(name, args, i, span, "a string argument")?;
                }
                Ok(TyRes { ty: Ty::Void, slot: None })
            }
            "sys.beep" => {
                self.arg_count(name, n, 2, span)?;
                self.expect_int(name, args, 0, span, "the frequency")?;
                self.expect_int(name, args, 1, span, "the duration")?;
                Ok(TyRes { ty: Ty::Void, slot: None })
            }
            "sys.clipboard_set" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_str(name, args, 0, span, "the text")?;
                Ok(TyRes { ty: Ty::Void, slot: None })
            }
            "sys.get_screen_size" => {
                self.arg_count(name, n, 0, span)?;
                Ok(TyRes { ty: Ty::Str, slot: None })
            }
            "sys.reg_read" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_str(name, args, 0, span, "the registry key")?;
                Ok(TyRes { ty: Ty::Str, slot: None })
            }
            "sys.reg_write" => {
                self.arg_count(name, n, 2, span)?;
                self.expect_str(name, args, 0, span, "the registry key")?;
                self.expect_str(name, args, 1, span, "the value")?;
                Ok(TyRes { ty: Ty::Void, slot: None })
            }
            // log
            "log.info" | "log.warn" | "log.error" | "log.debug" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_str(name, args, 0, span, "the message")?;
                Ok(TyRes { ty: Ty::Void, slot: None })
            }
            // path
            "path.join" => {
                if n == 0 {
                    return Err(self.zerr(codes::ARG_COUNT, "`path.join` expects at least 1 argument", span, None::<&str>));
                }
                for i in 0..n {
                    self.expect_str(name, args, i, span, "a path component")?;
                }
                Ok(TyRes { ty: Ty::Str, slot: None })
            }
            "path.dirname" | "path.basename" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_str(name, args, 0, span, "the path")?;
                Ok(TyRes { ty: Ty::Str, slot: None })
            }
            // args
            "args.has" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_str(name, args, 0, span, "the key")?;
                Ok(TyRes { ty: Ty::Bool, slot: None })
            }
            "args.get" => {
                if !(1..=3).contains(&n) {
                    return Err(self.zerr(
                        codes::ARG_COUNT,
                        format!("wrong number of arguments: `args.get` expects 1-3 (key[, type[, default]]), got {}", n),
                        span,
                        Some("form: args.get(key) / args.get(key, type) / args.get(key, type, default)"),
                    ));
                }
                self.expect_str(name, args, 0, span, "the key")?;
                if n >= 2 {
                    self.expect_str(name, args, 1, span, "the target type (`int`/`float`/`bool`/`str`)")?;
                }
                // 带类型转换时返回类型由运行期决定（字符串→数字/布尔），静态阶段按动态类型放行
                let ty = if n == 1 { Ty::Str } else { Ty::Unknown };
                Ok(TyRes { ty, slot: None })
            }
            // env
            "env.get" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_str(name, args, 0, span, "the environment variable name")?;
                Ok(TyRes { ty: Ty::Str, slot: None })
            }
            "env.set" => {
                self.arg_count(name, n, 2, span)?;
                self.expect_str(name, args, 0, span, "the key")?;
                self.expect_str(name, args, 1, span, "the value")?;
                Ok(TyRes { ty: Ty::Void, slot: None })
            }
            // db
            "db.set" => {
                self.arg_count(name, n, 2, span)?;
                self.expect_str(name, args, 0, span, "the key")?;
                self.expect_str(name, args, 1, span, "the value")?;
                Ok(TyRes { ty: Ty::Void, slot: None })
            }
            "db.get" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_str(name, args, 0, span, "the key")?;
                Ok(TyRes { ty: Ty::Str, slot: None })
            }
            // regex
            "regex.match" => {
                self.arg_count(name, n, 2, span)?;
                self.expect_str(name, args, 0, span, "the pattern")?;
                self.expect_str(name, args, 1, span, "the text")?;
                Ok(TyRes { ty: Ty::Bool, slot: None })
            }
            "regex.replace" => {
                self.arg_count(name, n, 3, span)?;
                for i in 0..3 {
                    self.expect_str(name, args, i, span, "a string argument")?;
                }
                Ok(TyRes { ty: Ty::Str, slot: None })
            }
            // crypto
            "crypto.md5" | "crypto.sha1" | "crypto.sha256" | "crypto.base64_encode" | "crypto.base64_decode" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_str(name, args, 0, span, "the input text")?;
                Ok(TyRes { ty: Ty::Str, slot: None })
            }
            "crypto.hmac_sha256" => {
                self.arg_count(name, n, 2, span)?;
                self.expect_str(name, args, 0, span, "the HMAC key")?;
                self.expect_str(name, args, 1, span, "the message")?;
                Ok(TyRes { ty: Ty::Str, slot: None })
            }
            // ---------- archive 压缩与归档 ----------
            "archive.zip_list" | "archive.tgz_list" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_str(name, args, 0, span, "the archive path")?;
                Ok(TyRes { ty: Ty::Unknown, slot: None }) // 条目列表（动态类型）
            }
            "archive.zip_read" | "archive.tgz_read" => {
                self.arg_count(name, n, 2, span)?;
                self.expect_str(name, args, 0, span, "the archive path")?;
                self.expect_str(name, args, 1, span, "the entry name")?;
                Ok(TyRes { ty: Ty::Str, slot: None })
            }
            "archive.zip_extract" | "archive.tgz_extract" => {
                self.arg_count(name, n, 2, span)?;
                self.expect_str(name, args, 0, span, "the archive path")?;
                self.expect_str(name, args, 1, span, "the destination directory")?;
                Ok(TyRes { ty: Ty::Int, slot: None })
            }
            "archive.zip_create" | "archive.tgz_create" => {
                self.arg_count(name, n, 2, span)?;
                self.expect_str(name, args, 0, span, "the archive path")?;
                self.expect_any(name, args, 1, span, "a dict of {entry: content}")?;
                Ok(TyRes { ty: Ty::Bool, slot: None })
            }
            // ---------- zlib / gzip 压缩 ----------
            "zlib.compress" | "zlib.decompress" | "zlib.gzip" | "zlib.gunzip" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_str(name, args, 0, span, "the input string")?;
                Ok(TyRes { ty: Ty::Str, slot: None })
            }
            // ---------- csv 数据处理 ----------
            "csv.parse" | "csv.parse_dict" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_str(name, args, 0, span, "the CSV text")?;
                Ok(TyRes { ty: Ty::Unknown, slot: None }) // 行/字典列表（动态类型）
            }
            "csv.stringify" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_any(name, args, 0, span, "a list of rows")?;
                Ok(TyRes { ty: Ty::Str, slot: None })
            }
            // ---------- glob / temp 系统工具 ----------
            "glob.match" => {
                self.arg_count(name, n, 2, span)?;
                self.expect_str(name, args, 0, span, "the glob pattern")?;
                self.expect_str(name, args, 1, span, "the path")?;
                Ok(TyRes { ty: Ty::Bool, slot: None })
            }
            "glob.list" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_str(name, args, 0, span, "the glob pattern")?;
                Ok(TyRes { ty: Ty::Unknown, slot: None }) // 路径列表（动态类型）
            }
            "temp.dir" | "temp.file" => {
                if n > 1 {
                    self.arg_count(name, n, 1, span)?;
                }
                if n == 1 {
                    self.expect_str(name, args, 0, span, "the prefix (or null)")?;
                }
                Ok(TyRes { ty: Ty::Str, slot: None })
            }
            "temp.remove" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_str(name, args, 0, span, "the path")?;
                Ok(TyRes { ty: Ty::Bool, slot: None })
            }
            // ---------- stat / matrix 科学计算 ----------
            "stat.sum" | "stat.mean" | "stat.median" | "stat.variance" | "stat.stddev"
            | "stat.min" | "stat.max" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_any(name, args, 0, span, "a list of numbers")?;
                let ty = if name == "stat.sum" { Ty::Unknown } else { Ty::Float };
                Ok(TyRes { ty, slot: None })
            }
            "matrix.identity" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_any(name, args, 0, span, "the matrix size")?;
                Ok(TyRes { ty: Ty::Unknown, slot: None }) // 矩阵（动态类型）
            }
            "matrix.transpose" | "matrix.scale" => {
                if name == "matrix.transpose" {
                    self.arg_count(name, n, 1, span)?;
                } else {
                    self.arg_count(name, n, 2, span)?;
                }
                self.expect_any(name, args, 0, span, "a matrix (list of lists)")?;
                Ok(TyRes { ty: Ty::Unknown, slot: None })
            }
            "matrix.add" | "matrix.mul" => {
                self.arg_count(name, n, 2, span)?;
                self.expect_any(name, args, 0, span, "a matrix (list of lists)")?;
                self.expect_any(name, args, 1, span, "a matrix (list of lists)")?;
                Ok(TyRes { ty: Ty::Unknown, slot: None })
            }
            // ---------- diff / regex 文本处理 ----------
            "diff.lines" | "diff.unified" => {
                self.arg_count(name, n, 2, span)?;
                self.expect_str(name, args, 0, span, "the original text")?;
                self.expect_str(name, args, 1, span, "the modified text")?;
                let ty = if name == "diff.lines" { Ty::Unknown } else { Ty::Str };
                Ok(TyRes { ty, slot: None })
            }
            "regex.find" | "regex.groups" | "regex.split" => {
                self.arg_count(name, n, 2, span)?;
                self.expect_str(name, args, 0, span, "the pattern")?;
                self.expect_str(name, args, 1, span, "the text")?;
                Ok(TyRes { ty: Ty::Unknown, slot: None }) // 结果列表（动态类型）
            }
            // ---------- plot / yaml ----------
            "plot.bar" => {
                if n < 1 || n > 2 {
                    return Err(self.zerr(
                        codes::ARG_COUNT,
                        format!("wrong number of arguments: `plot.bar` expects 1-2, got {}", n),
                        span,
                        Some("form: plot.bar(values[, labels])"),
                    ));
                }
                self.expect_any(name, args, 0, span, "a list of numbers")?;
                if n == 2 {
                    self.expect_any(name, args, 1, span, "a list of labels")?;
                }
                Ok(TyRes { ty: Ty::Str, slot: None })
            }
            "plot.line" => {
                self.arg_count(name, n, 2, span)?;
                self.expect_any(name, args, 0, span, "a list of x values")?;
                self.expect_any(name, args, 1, span, "a list of y values")?;
                Ok(TyRes { ty: Ty::Str, slot: None })
            }
            "yaml.parse" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_str(name, args, 0, span, "the YAML text")?;
                Ok(TyRes { ty: Ty::Unknown, slot: None }) // 动态类型
            }
            "yaml.stringify" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_any(name, args, 0, span, "a value")?;
                Ok(TyRes { ty: Ty::Str, slot: None })
            }
            // ---------- sqlite ----------
            "sqlite.open" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_str(name, args, 0, span, "the database path")?;
                Ok(TyRes { ty: Ty::Int, slot: None }) // 句柄 id
            }
            "sqlite.close" | "sqlite.last_insert_id" | "sqlite.changes" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_int(name, args, 0, span, "the database handle")?;
                let ty = if name == "sqlite.close" { Ty::Bool } else { Ty::Int };
                Ok(TyRes { ty, slot: None })
            }
            "sqlite.exec" => {
                self.arg_count(name, n, 2, span)?;
                self.expect_int(name, args, 0, span, "the database handle")?;
                self.expect_str(name, args, 1, span, "the SQL statement")?;
                Ok(TyRes { ty: Ty::Bool, slot: None })
            }
            "sqlite.query" => {
                self.arg_count(name, n, 2, span)?;
                self.expect_int(name, args, 0, span, "the database handle")?;
                self.expect_str(name, args, 1, span, "the SQL query")?;
                Ok(TyRes { ty: Ty::Unknown, slot: None }) // dict 列表（动态类型）
            }
            "sqlite.query_one" => {
                self.arg_count(name, n, 2, span)?;
                self.expect_int(name, args, 0, span, "the database handle")?;
                self.expect_str(name, args, 1, span, "the SQL query")?;
                Ok(TyRes { ty: Ty::Unknown, slot: None }) // dict 或 null（动态类型）
            }
            "sqlite.escape" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_str(name, args, 0, span, "the string to escape")?;
                Ok(TyRes { ty: Ty::Str, slot: None })
            }
            // ---------- plugin 插件系统 ----------
            "plugin.load" => {
                self.arg_count(name, n, 2, span)?;
                self.expect_str(name, args, 0, span, "the plugin library path")?;
                self.expect_str(name, args, 1, span, "the plugin alias")?;
                Ok(TyRes { ty: Ty::Bool, slot: None })
            }
            "plugin.has" | "plugin.unload" => {
                self.arg_count(name, n, 1, span)?;
                self.expect_str(name, args, 0, span, "the plugin alias")?;
                Ok(TyRes { ty: Ty::Bool, slot: None })
            }
            "plugin.list" => {
                self.arg_count(name, n, 0, span)?;
                Ok(TyRes { ty: Ty::Unknown, slot: None }) // 插件信息列表（动态类型）
            }
            // uuid
            "uuid.new" => {
                self.arg_count(name, n, 0, span)?;
                Ok(TyRes { ty: Ty::Str, slot: None })
            }
            other => Err(self.zerr(
                codes::UNDEFINED,
                format!("undefined function `{}`", other),
                span,
                Some("check the spelling"),
            )),
        }
    }

    fn zerr(&self, code: &'static str, msg: impl Into<String>, span: Span, help: Option<impl Into<String>>) -> ZError {
        ZError::new(code, msg, &self.file, &self.src, span.line, span.col, span.len.max(1), help)
    }
}

fn stmt_span(s: &Stmt) -> Span {
    match s {
        Stmt::Assign { span, .. }
        | Stmt::AssignOp { span, .. }
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
        | Stmt::DebugPrint { span, .. } => *span,
    }
}

fn lookup_in_stack(
    name: &str,
    scopes: &[HashMap<String, usize>],
    scope_stack: &[usize],
) -> Option<usize> {
    for idx in scope_stack.iter().rev() {
        if let Some(slot) = scopes[*idx].get(name) {
            return Some(*slot);
        }
    }
    None
}

pub(crate) fn builtin_names() -> HashSet<&'static str> {
    [
        "print",
        "len",
        "append",
        "clone",
        "copy",
        "contains",
        "index_of",
        "keys",
        "values",
        "has_key",
        "is_int",
        "is_float",
        "is_str",
        "is_bool",
        "is_list",
        "is_dict",
        "is_null",
        "type_of",
        "assert",
        "to_str",
        "to_int",
        "to_float",
        "input",
        "read_int",
        "read_float",
        "read_file",
        "write_file",
        "file_exists",
        "abs",
        "max",
        "min",
        "str_contains",
        "str_replace",
        "str_trim",
        "time.now",
        "time.sleep",
        "time.format",
        "time.parse",
        "time.add",
        "time.diff",
        "time.weekday",
        "random.int",
        "random.float",
        "http_get",
        "http_post",
        "http.request",
        "smtp.send",
        "ws.request",
        "json_parse",
        "json_stringify",
        "sys.run",
        "sys.get_env",
        "sys.msgbox",
        "sys.beep",
        "sys.clipboard_set",
        "sys.get_screen_size",
        "sys.reg_read",
        "sys.reg_write",
        "server.listen",
        "server.poll",
        "server.respond",
        "ptr.alloc",
        "ptr.free",
        "ptr.is_null",
        "ptr.is_valid",
        "ptr.size",
        "ptr.read_int",
        "ptr.read_float",
        "ptr.read_byte",
        "ptr.write_int",
        "ptr.write_float",
        "ptr.write_byte",
        "log.info",
        "log.warn",
        "log.error",
        "log.debug",
        "path.join",
        "path.dirname",
        "path.basename",
        "args.get",
        "args.has",
        "env.get",
        "env.set",
        "db.set",
        "db.get",
        "regex.match",
        "regex.replace",
        "crypto.md5",
        "crypto.sha1",
        "crypto.sha256",
        "crypto.hmac_sha256",
        "crypto.base64_encode",
        "crypto.base64_decode",
        "archive.zip_list",
        "archive.zip_read",
        "archive.zip_extract",
        "archive.zip_create",
        "archive.tgz_list",
        "archive.tgz_read",
        "archive.tgz_extract",
        "archive.tgz_create",
        "zlib.compress",
        "zlib.decompress",
        "zlib.gzip",
        "zlib.gunzip",
        "csv.parse",
        "csv.parse_dict",
        "csv.stringify",
        "glob.match",
        "glob.list",
        "temp.dir",
        "temp.file",
        "temp.remove",
        "stat.sum",
        "stat.mean",
        "stat.median",
        "stat.variance",
        "stat.stddev",
        "stat.min",
        "stat.max",
        "matrix.identity",
        "matrix.transpose",
        "matrix.add",
        "matrix.mul",
        "matrix.scale",
        "diff.lines",
        "diff.unified",
        "regex.find",
        "regex.groups",
        "regex.split",
        "plot.bar",
        "plot.line",
        "yaml.parse",
        "yaml.stringify",
        "sqlite.open",
        "sqlite.close",
        "sqlite.exec",
        "sqlite.query",
        "sqlite.query_one",
        "sqlite.escape",
        "sqlite.last_insert_id",
        "sqlite.changes",
        "plugin.load",
        "plugin.has",
        "plugin.list",
        "plugin.unload",
        "uuid.new",
    ]
    .into_iter()
    .collect()
}
