// vm.rs - Hone 寄存器式字节码虚拟机（文本 IR + 反汇编）
//
// 设计要点：
//  - 复用 `crate::interp::Value` 与全部 builtins，零重复实现。
//  - 寄存器机：每个 chunk（函数 / 顶层 main）拥有独立寄存器文件 Vec<Value>。
//  - 文本 IR：指令集以 Instr 枚举表达，经 `disassemble_program()` 序列化为人类可读文本
//    （`hone run --disasm` 即用此入口），并可经 `assemble()` 反向装配回 Module 直接执行
//    （完整往返：`hone runir program.ir`）。另有 `disassemble()` / `disassemble_module()`
//    供分块调试，见各自文档。
//  - 编译器将 AST 直接编译为字节码；VM 执行字节码。类型锁定由 checker 静态保证，
//    VM 只做运行时求值与少量类型校验（数值运算 / 比较 / 索引越界等）。
//
// 当前覆盖：字面量、变量、算术/比较/逻辑（短路）/空值合并/三元、赋值/复合赋值/自增自减、
// if/while/do-while/for-c/for-in（列表与字典，运行时分拣）/break/continue、函数（定义/调用/
// 递归/多返回值/解构）、列表字典（字面量/索引/字段/字典键）、f-string、推导式（列表与字典）、
// try/catch/throw（含 Error 字段访问 message/code/file/line/col/context）、match（字面量 + 枚举变体 + 绑定）、
// struct/class/enum 定义与使用、builtins、debug_print、breakpoint（debug 模式）、alias/use、
// lambda 闭包（按值捕获）、async/await（后台线程 + Future）、go 多线程（后台线程 fire-and-forget）、
// 运算符重载（__add/__sub/__mul/__div/__mod/__eq/__ne/__lt/__le/__gt/__ge/__neg/__not/__index/__len）。
// 暂未覆盖（后续迭代）：import/load 远程模块、
// 依赖原生 GUI/HTTP/FFI 库的示例（guipro_*/server_selftest/spider_demo/ffi_demo/ai_demo，
//   这些需要原生库绑定，已置为清晰运行时报错，不静默失败）。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::thread;

use crate::ast::*;
use crate::builtins;
use crate::error::{codes, ZError};
use crate::interp::{EnumVal, ErrorObj, FutureVal, LambdaVal, Value};
use crate::lexer::Span;

// ───────────────────────────── 常量池 ─────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Const {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    Char(char),
    Null,
}

// ───────────────────────────── 指令集（文本 IR） ─────────────────────────────

#[derive(Debug, Clone)]
pub enum Instr {
    LoadK(usize, usize),     // dst, const_idx
    LoadNull(usize),         // dst
    LoadBool(usize, bool),   // dst, val
    Move(usize, usize),      // dst, src
    Neg(usize, usize),       // dst, src
    Not(usize, usize),       // dst, src
    Add(usize, usize, usize),
    Sub(usize, usize, usize),
    Mul(usize, usize, usize),
    Div(usize, usize, usize),
    Mod(usize, usize, usize),
    Eq(usize, usize, usize),
    Ne(usize, usize, usize),
    Lt(usize, usize, usize),
    Le(usize, usize, usize),
    Gt(usize, usize, usize),
    Ge(usize, usize, usize),
    IsNull(usize, usize),        // dst, src
    IsDict(usize, usize),        // dst, src
    IterCheck(usize, bool),      // src, is_comp（迭代源必须是 list/dict，否则报错）
    Index(usize, usize, usize),    // dst, obj, key
    IndexSet(usize, usize, usize), // obj, key, val
    DestructGet(usize, usize, usize), // dst, obj, key（解构专用：列表越界/字典缺键报错）
    Field(usize, usize, String),   // dst, obj, field
    Len(usize, usize),             // dst, src
    Keys(usize, usize),            // dst, src(dict)
    NewList(usize, usize, usize),  // dst, base, n
    NewDict(usize, usize, usize),  // dst, base, n（成对 base..base+2n）
    EnumElem(usize, usize, usize), // dst, enumval, idx
    IsEnumVariant(usize, usize, String, String), // dst, src, enum_name, variant
    NewEnum(usize, String, String, usize, usize), // dst, enum_name, variant, payload_base, npayload
    Call(usize, String, usize, usize), // result, func, argbase, nargs
    MakeLambda(usize, usize, Vec<(String, usize)>), // dst, chunk_idx, captures: (变量名, 外层寄存器)
    Await(usize, usize),          // dst, future_reg
    GoCall(String, usize, usize), // callee, argbase, nargs（后台线程执行，fire-and-forget）
    Ret(usize),                   // value reg
    RetNull,
    Jmp(usize),                   // target pc（编译期占位 0，resolve 后填真实下标）
    JmpIfFalse(usize, usize),     // cond_reg, target
    JmpIfTrue(usize, usize),      // cond_reg, target
    Label(String),                // 伪指令：标签
    TryBegin(usize, usize),       // handler_pc（占位 0）, catch_reg
    TryPop,                       // 正常路径弹出 try 栈
    Throw(usize),                 // reg 持有被抛值（str→H600，error 原样）
    ThrowStr(String),             // 直接抛字面字符串 → H600
    DebugPrint(usize),            // reg
    Breakpoint,                   // 无操作数
    Nop,
}

// ───────────────────────────── Chunk ─────────────────────────────

#[derive(Debug, Clone)]
pub struct Chunk {
    pub name: String,
    pub code: Vec<Instr>,
    pub spans: Vec<Span>,
    pub consts: Vec<Const>,
    pub labels: HashMap<String, usize>,
    pub nregs: usize,
    pub params: Vec<String>,
    /// lambda chunk 专用：被捕获变量 (名字, 寄存器号)，调用时先把这些寄存器填回捕获值。
    pub captured_regs: Vec<(String, usize)>,
    /// 本 chunk 声明的局部变量名 → 寄存器号（编译期收集，运行时供 do_call 做 lambda/前向引用解析）。
    pub locals: HashMap<String, usize>,
}

// ───────────────────────────── 编译器 ─────────────────────────────

struct Compiler {
    chunks: Vec<Chunk>,
    func_map: HashMap<String, usize>,
    struct_defs: HashMap<String, Vec<String>>,
    enum_defs: HashMap<String, Vec<(String, usize)>>,
    aliases: Vec<(String, String)>,
    async_fns: HashSet<String>,
    code: Vec<Instr>,
    cur_locals: HashMap<String, usize>,
    spans: Vec<Span>,
    consts: Vec<Const>,
    labels: HashMap<String, usize>,
    pending: Vec<(usize, String)>,
    scopes: Vec<(HashMap<String, usize>, usize)>,
    var_next: usize,
    tmp_next: usize,
    max_reg: usize,
    cur_span: Span,
    loop_labels: Vec<(String, String)>, // (continue_label, break_label)
    label_seq: usize,
    syn_seq: usize,
    err: Option<ZError>,
}

impl Compiler {
    fn new() -> Self {
        Compiler {
            chunks: Vec::new(),
            func_map: HashMap::new(),
            struct_defs: HashMap::new(),
            enum_defs: HashMap::new(),
            aliases: Vec::new(),
            async_fns: HashSet::new(),
            code: Vec::new(),
            cur_locals: HashMap::new(),
            spans: Vec::new(),
            consts: Vec::new(),
            labels: HashMap::new(),
            pending: Vec::new(),
            scopes: Vec::new(),
            var_next: 0,
            tmp_next: 0,
            max_reg: 0,
            cur_span: Span { line: 0, col: 0, len: 0 },
            loop_labels: Vec::new(),
            label_seq: 0,
            syn_seq: 0,
            err: None,
        }
    }

    fn fail(&mut self, code: &'static str, msg: impl Into<String>) {
        if self.err.is_none() {
            self.err =
                Some(ZError::plain(code, msg, Some("可用默认解释器 `hone run`（不带 --vm）运行")));
        }
    }

    fn emit(&mut self, ins: Instr) {
        self.code.push(ins);
        self.spans.push(self.cur_span.clone());
    }

    fn emit_label(&mut self, name: &str) {
        self.labels.insert(name.to_string(), self.code.len());
        self.code.push(Instr::Label(name.to_string()));
        self.spans.push(self.cur_span.clone());
    }

    fn new_label(&mut self, p: &str) -> String {
        let s = format!("{}{}", p, self.label_seq);
        self.label_seq += 1;
        s
    }

    fn jmp(&mut self, lab: &str) {
        let i = self.code.len();
        self.emit(Instr::Jmp(0));
        self.pending.push((i, lab.to_string()));
    }
    fn jif(&mut self, r: usize, lab: &str) {
        let i = self.code.len();
        self.emit(Instr::JmpIfFalse(r, 0));
        self.pending.push((i, lab.to_string()));
    }
    fn jit(&mut self, r: usize, lab: &str) {
        let i = self.code.len();
        self.emit(Instr::JmpIfTrue(r, 0));
        self.pending.push((i, lab.to_string()));
    }

    fn enter(&mut self) {
        let saved = self.var_next;
        self.scopes.push((HashMap::new(), saved));
    }
    fn exit(&mut self) {
        if let Some((_, saved)) = self.scopes.pop() {
            self.var_next = saved;
            self.tmp_next = self.var_next;
        }
    }

    fn decl(&mut self, name: &str) -> usize {
        let r = self.var_next;
        self.var_next += 1;
        if r > self.max_reg {
            self.max_reg = r;
        }
        self.scopes.last_mut().unwrap().0.insert(name.to_string(), r);
        self.cur_locals.insert(name.to_string(), r);
        self.tmp_next = self.var_next;
        r
    }
    /// 在「临时区」声明一个仅用于当前表达式的作用域内名字（如推导式循环变量）：
    /// 从 tmp_next 分配并登记到当前作用域供 lookup 使用，但**不推进 var_next**，
    /// 因此不会与外围表达式的临时寄存器冲突，也不会造成寄存器区泄漏。
    fn decl_tmp(&mut self, name: &str) -> usize {
        let r = self.tmp_next;
        self.tmp_next += 1;
        if r > self.max_reg {
            self.max_reg = r;
        }
        self.scopes.last_mut().unwrap().0.insert(name.to_string(), r);
        r
    }
    fn syn(&mut self) -> String {
        let s = format!("${}", self.syn_seq);
        self.syn_seq += 1;
        s
    }
    fn lookup(&self, name: &str) -> Option<usize> {
        for s in self.scopes.iter().rev() {
            if let Some(r) = s.0.get(name) {
                return Some(*r);
            }
        }
        None
    }
    fn tmp(&mut self) -> usize {
        let r = self.tmp_next;
        self.tmp_next += 1;
        if r > self.max_reg {
            self.max_reg = r;
        }
        r
    }
    fn reset_tmp(&mut self) {
        self.tmp_next = self.var_next;
    }

    fn const_idx(&mut self, c: Const) -> usize {
        if let Some(i) = self.consts.iter().position(|x| x == &c) {
            i
        } else {
            self.consts.push(c);
            self.consts.len() - 1
        }
    }

    fn resolve(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        for (idx, lab) in pending {
            let t = *self.labels.get(&lab).expect("undefined label in resolve");
            self.code[idx] = match &self.code[idx] {
                Instr::Jmp(_) => Instr::Jmp(t),
                Instr::JmpIfFalse(r, _) => Instr::JmpIfFalse(*r, t),
                Instr::JmpIfTrue(r, _) => Instr::JmpIfTrue(*r, t),
                Instr::TryBegin(_, c) => Instr::TryBegin(t, *c),
                _ => unreachable!("pending jump on non-jump instr"),
            };
        }
    }

    fn finish_chunk(&mut self, name: &str, params: Vec<String>) -> usize {
        self.resolve();
        let nregs = self.max_reg + 1;
        let chunk = Chunk {
            name: name.to_string(),
            code: std::mem::take(&mut self.code),
            spans: std::mem::take(&mut self.spans),
            consts: std::mem::take(&mut self.consts),
            labels: std::mem::take(&mut self.labels),
            nregs,
            params,
            captured_regs: Vec::new(),
            locals: std::mem::take(&mut self.cur_locals),
        };
        let idx = self.chunks.len();
        self.chunks.push(chunk);
        self.func_map.insert(name.to_string(), idx);
        self.scopes.clear();
        self.var_next = 0;
        self.tmp_next = 0;
        self.max_reg = 0;
        self.loop_labels.clear();
        idx
    }

    // ── 顶层编译 ──
    fn compile_program(&mut self, prog: &Program) -> Result<(), ZError> {
        for s in &prog.stmts {
            match s {
                Stmt::FnDef { name, params, body, .. } => self.compile_fn(name, params, body),
                Stmt::AsyncFnDef { name, params, body, .. } => {
                    self.async_fns.insert(name.clone());
                    self.compile_fn(name, params, body)
                }
                Stmt::ClassDef { name, methods, .. } => {
                    for m in methods {
                        if let Stmt::FnDef { name: mf_name, params: mf_params, body: mf_body, .. } = m {
                            let fname = format!("{}.{}", name, mf_name);
                            self.compile_fn(&fname, mf_params, mf_body);
                        }
                    }
                }
                Stmt::StructDef { name, fields, .. } => {
                    self.struct_defs
                        .insert(name.clone(), fields.iter().map(|(n, _, _)| n.clone()).collect());
                }
                // type 实例类：注册成员方法（「类型名.方法名」限定键），供运行时解析
                Stmt::TypeDef { name, methods, .. } => {
                    for m in methods {
                        if let Stmt::FnDef { name: mf_name, params: mf_params, body: mf_body, .. } = m {
                            let fname = format!("{}.{}", name, mf_name);
                            self.compile_fn(&fname, mf_params, mf_body);
                        }
                    }
                }
                Stmt::EnumDef { name, variants, .. } => {
                    let vs: Vec<(String, usize)> = variants
                        .iter()
                        .map(|v| (v.name.clone(), v.payload.len()))
                        .collect();
                    self.enum_defs.insert(name.clone(), vs);
                }
                Stmt::Alias { original, new_name, .. } => {
                    self.aliases.push((original.clone(), new_name.clone()))
                }
                _ => {}
            }
        }
        for (orig, newn) in &self.aliases {
            if let Some(&i) = self.func_map.get(orig) {
                self.func_map.insert(newn.clone(), i);
            }
        }
        self.enter();
        for s in &prog.stmts {
            match s {
                Stmt::FnDef { .. }
                | Stmt::AsyncFnDef { .. }
                | Stmt::ClassDef { .. }
                | Stmt::TypeDef { .. }
                | Stmt::StructDef { .. }
                | Stmt::EnumDef { .. }
                | Stmt::Alias { .. }
                | Stmt::Export { .. }
                | Stmt::Import { .. }
                | Stmt::Load { .. }
                | Stmt::Use { .. } => {}
                _ => self.compile_stmt(s),
            }
            self.reset_tmp();
        }
        self.emit(Instr::RetNull);
        self.exit();
        self.finish_chunk("main", vec![]);
        if let Some(e) = self.err.take() {
            return Err(e);
        }
        Ok(())
    }

    fn compile_fn(&mut self, name: &str, params: &[Param], body: &[Stmt]) {
        self.enter();
        let mut pnames = Vec::new();
        for p in params {
            if p.cow {
                self.fail(codes::NOT_IMPLEMENTED, "VM: `cow` 形参暂不支持");
                return;
            }
            self.decl(&p.name);
            pnames.push(p.name.clone());
        }
        for s in body {
            self.compile_stmt(s);
            self.reset_tmp();
        }
        self.emit(Instr::RetNull);
        self.exit();
        self.finish_chunk(name, pnames);
    }

    // ── 语句编译 ──
    fn compile_stmt(&mut self, s: &Stmt) {
        if self.err.is_some() {
            return;
        }
        self.cur_span = s.span();
        match s {
            Stmt::Assign { name, value, .. } => {
                let rv = self.compile_expr(value);
                match self.lookup(name) {
                    Some(r) => self.emit(Instr::Move(r, rv)),
                    None => {
                        let r = self.decl(name);
                        self.emit(Instr::Move(r, rv));
                    }
                }
            }
            Stmt::IndexAssign { target, value, .. } => {
                if let Expr::Index { obj, index, .. } = target {
                    let ro = self.compile_expr(&**obj);
                    let rk = self.compile_expr(&**index);
                    let rv = self.compile_expr(value);
                    self.emit(Instr::IndexSet(ro, rk, rv));
                } else {
                    self.fail(codes::NOT_IMPLEMENTED, "VM: 仅支持单层索引赋值 a[i] = x");
                }
            }
            Stmt::DestructAssign { targets, value, .. } => {
                let rv = self.compile_expr(value);
                // 把待解构的值搬到「变量区」安全寄存器，避免循环中 decl() 重置 tmp_next
                // 时把 rv 所在的临时寄存器回收，导致后续索引源被破坏
                // （例如 `t1, t2, t3 = triple()` 第二次迭代会越界/误索引）。
                let saved = self.var_next;
                self.var_next += 1;
                if saved > self.max_reg {
                    self.max_reg = saved;
                }
                self.tmp_next = self.var_next;
                self.emit(Instr::Move(saved, rv));
                for (i, (name, key)) in targets.iter().enumerate() {
                    let ri = self.tmp();
                    let k = if let Some(kname) = key {
                        self.const_idx(Const::Str(kname.clone()))
                    } else {
                        self.const_idx(Const::Int(i as i64))
                    };
                    let rk = self.tmp();
                    self.emit(Instr::LoadK(rk, k));
                    self.emit(Instr::DestructGet(ri, saved, rk));
                    match self.lookup(name) {
                        Some(r) => self.emit(Instr::Move(r, ri)),
                        None => {
                            let r = self.decl(name);
                            self.emit(Instr::Move(r, ri));
                        }
                    }
                }
            }
            Stmt::AssignOp { name, op, value, .. } => {
                let rvar = match self.lookup(name) {
                    Some(r) => r,
                    None => {
                        self.fail(codes::UNDEFINED, format!("VM: 未定义变量 `{}`", name));
                        return;
                    }
                };
                let rv = self.compile_expr(value);
                let r = self.tmp();
                match op {
                    CompoundOp::Add => self.emit(Instr::Add(r, rvar, rv)),
                    CompoundOp::Sub => self.emit(Instr::Sub(r, rvar, rv)),
                    CompoundOp::Mul => self.emit(Instr::Mul(r, rvar, rv)),
                    CompoundOp::Div => self.emit(Instr::Div(r, rvar, rv)),
                    CompoundOp::Mod => self.emit(Instr::Mod(r, rvar, rv)),
                }
                self.emit(Instr::Move(rvar, r));
            }
            Stmt::VarDecl { name, init, cow, .. } => {
                if *cow {
                    self.fail(codes::NOT_IMPLEMENTED, "VM: `cow` 声明暂不支持");
                } else {
                    match init {
                        Some(e) => {
                            let rv = self.compile_expr(e);
                            let r = self.decl(name);
                            self.emit(Instr::Move(r, rv));
                        }
                        None => {
                            let r = self.decl(name);
                            self.emit(Instr::LoadNull(r));
                        }
                    }
                }
            }
            Stmt::Block { stmts, .. } => {
                self.enter();
                for s2 in stmts {
                    self.compile_stmt(s2);
                    self.reset_tmp();
                }
                self.exit();
            }
            Stmt::If { cond, then_branch, else_branch, .. } => {
                let rc = self.compile_expr(cond);
                let lelse = self.new_label("Lelse");
                let lend = self.new_label("Lend");
                self.jif(rc, &lelse);
                for s2 in then_branch {
                    self.compile_stmt(s2);
                    self.reset_tmp();
                }
                self.jmp(&lend);
                self.emit_label(&lelse);
                if let Some(eb) = else_branch {
                    for s2 in eb {
                        self.compile_stmt(s2);
                        self.reset_tmp();
                    }
                }
                self.emit_label(&lend);
            }
            Stmt::While { cond, body, .. } => {
                let lstart = self.new_label("Lstart");
                let lend = self.new_label("Lend");
                self.emit_label(&lstart);
                let rc = self.compile_expr(cond);
                self.jif(rc, &lend);
                self.loop_labels.push((lstart.clone(), lend.clone()));
                for s2 in body {
                    self.compile_stmt(s2);
                    self.reset_tmp();
                }
                self.loop_labels.pop();
                self.jmp(&lstart);
                self.emit_label(&lend);
            }
            Stmt::DoWhile { body, cond, .. } => {
                let lstart = self.new_label("Lstart");
                let leval = self.new_label("Leval");
                let lend = self.new_label("Lend");
                self.emit_label(&lstart);
                self.loop_labels.push((leval.clone(), lend.clone()));
                for s2 in body {
                    self.compile_stmt(s2);
                    self.reset_tmp();
                }
                self.loop_labels.pop();
                self.emit_label(&leval);
                let rc = self.compile_expr(cond);
                self.jit(rc, &lstart);
                self.emit_label(&lend);
            }
            Stmt::ForC { init, cond, step, body, .. } => {
                if let Some(i) = init {
                    self.compile_stmt(i);
                    self.reset_tmp();
                }
                let lcond = self.new_label("Lcond");
                let lstep = self.new_label("Lstep");
                let lend = self.new_label("Lend");
                self.emit_label(&lcond);
                if let Some(c) = cond {
                    let rc = self.compile_expr(c);
                    self.jif(rc, &lend);
                }
                self.loop_labels.push((lstep.clone(), lend.clone()));
                for s2 in body {
                    self.compile_stmt(s2);
                    self.reset_tmp();
                }
                self.loop_labels.pop();
                self.emit_label(&lstep);
                if let Some(s2) = step {
                    self.compile_stmt(s2);
                    self.reset_tmp();
                }
                self.jmp(&lcond);
                self.emit_label(&lend);
            }
            Stmt::ForIn { var, var2, iter, body, .. } => {
                let riter_src = self.compile_expr(iter);
                // 迭代源必须是 list/dict，否则报与解释器一致的错误（列号指向迭代源）。
                let saved_span = self.cur_span.clone();
                self.cur_span = expr_span(iter);
                self.emit(Instr::IterCheck(riter_src, false));
                self.cur_span = saved_span;
                let risdict = self.tmp();
                self.emit(Instr::IsDict(risdict, riter_src));
                let ldict = self.new_label("Ldict");
                let llist = self.new_label("Llist");
                let lend = self.new_label("Lend");
                self.jit(risdict, &ldict);
                // 列表路径
                self.emit_label(&llist);
                self.enter();
                // 将迭代对象搬入持久寄存器（在 enter 后立即声明），避免其原始 tmp
                // 寄存器在循环体 reset_tmp 后被后续临时寄存器覆盖。
                let riter = self.decl("__iter");
                self.emit(Instr::Move(riter, riter_src));
                // 先声明所有持久变量（元素/键/值/计数器），再分配临时寄存器，
                // 否则 decl() 推高 var_next 后可能与已分配的 tmp 寄存器产生冲突。
                let rvar = self.decl(var);
                // 列表路径下 var2 仅在编译期声明以满足 body 引用（运行期对列表用双变量会走 dict 分支外的报错路径）；
                // 字典迭代走下方 dict 分支，var2 正常使用。
                let _rvar2 = if let Some(v2) = var2 {
                    Some(self.decl(v2))
                } else {
                    None
                };
                let _syn = self.syn();
                let rcnt = self.decl(&_syn);
                let k0 = self.const_idx(Const::Int(0));
                self.emit(Instr::LoadK(rcnt, k0));
                let lcond = self.new_label("Lcond");
                let linc = self.new_label("Linc");
                let lend2 = self.new_label("LlistEnd");
                self.emit_label(&lcond);
                let rlen = self.tmp();
                self.emit(Instr::Len(rlen, riter));
                let rcond = self.tmp();
                self.emit(Instr::Lt(rcond, rcnt, rlen));
                self.jif(rcond, &lend2);
                let relem = self.tmp();
                self.emit(Instr::Index(relem, riter, rcnt));
                self.emit(Instr::Move(rvar, relem));
                self.loop_labels.push((linc.clone(), lend2.clone()));
                for s2 in body {
                    self.compile_stmt(s2);
                    self.reset_tmp();
                }
                self.loop_labels.pop();
                self.emit_label(&linc);
                let rone = self.tmp();
                let k = self.const_idx(Const::Int(1));
                self.emit(Instr::LoadK(rone, k));
                let rnext = self.tmp();
                self.emit(Instr::Add(rnext, rcnt, rone));
                self.emit(Instr::Move(rcnt, rnext));
                self.jmp(&lcond);
                self.emit_label(&lend2);
                self.exit();
                self.jmp(&lend); // 列表路径结束，跳过下方的字典路径
                // 字典路径
                self.emit_label(&ldict);
                self.enter();
                let riter2 = self.decl("__iter");
                self.emit(Instr::Move(riter2, riter_src));
                let rvar2 = self.decl(var);
                let rdvar = if let Some(v2) = var2 {
                    Some(self.decl(v2))
                } else {
                    None
                };
                let _syn = self.syn();
                let drcnt = self.decl(&_syn);
                let k0 = self.const_idx(Const::Int(0));
                self.emit(Instr::LoadK(drcnt, k0));
                let dcond = self.new_label("Dcond");
                let dinc = self.new_label("Dinc");
                let dend = self.new_label("DictEnd");
                self.emit_label(&dcond);
                let drkeys = self.tmp();
                self.emit(Instr::Keys(drkeys, riter2));
                let drlen = self.tmp();
                self.emit(Instr::Len(drlen, drkeys));
                let drcond = self.tmp();
                self.emit(Instr::Lt(drcond, drcnt, drlen));
                self.jif(drcond, &dend);
                let drkey = self.tmp();
                self.emit(Instr::Index(drkey, drkeys, drcnt));
                self.emit(Instr::Move(rvar2, drkey));
                if let Some(rd) = rdvar {
                    let drval = self.tmp();
                    self.emit(Instr::Index(drval, riter2, drkey));
                    self.emit(Instr::Move(rd, drval));
                }
                self.loop_labels.push((dinc.clone(), dend.clone()));
                for s2 in body {
                    self.compile_stmt(s2);
                    self.reset_tmp();
                }
                self.loop_labels.pop();
                self.emit_label(&dinc);
                let rone2 = self.tmp();
                let k2 = self.const_idx(Const::Int(1));
                self.emit(Instr::LoadK(rone2, k2));
                let rnext2 = self.tmp();
                self.emit(Instr::Add(rnext2, drcnt, rone2));
                self.emit(Instr::Move(drcnt, rnext2));
                self.jmp(&dcond);
                self.emit_label(&dend);
                self.exit();
                self.jmp(&lend);
                self.emit_label(&lend);
            }
            Stmt::Return { values, .. } => {
                if values.is_empty() {
                    self.emit(Instr::RetNull);
                } else if values.len() == 1 {
                    let rv = self.compile_expr(&values[0]);
                    self.emit(Instr::Ret(rv));
                } else {
                    let base = self.tmp();
                    let n = values.len();
                    for _ in 0..n {
                        self.tmp();
                    }
                    for (i, v) in values.iter().enumerate() {
                        let rv = self.compile_expr(v);
                        self.emit(Instr::Move(base + i, rv));
                    }
                    let r = self.tmp();
                    self.emit(Instr::NewList(r, base, n));
                    self.emit(Instr::Ret(r));
                }
            }
            Stmt::Break { .. } => {
                let bl = self.loop_labels.last().map(|(_, b)| b.clone());
                match bl {
                    Some(bl) => self.jmp(&bl),
                    None => self.fail(codes::SYNTAX, "VM: break 只能在循环内"),
                }
            }
            Stmt::Continue { .. } => {
                let cl = self.loop_labels.last().map(|(c, _)| c.clone());
                match cl {
                    Some(cl) => self.jmp(&cl),
                    None => self.fail(codes::SYNTAX, "VM: continue 只能在循环内"),
                }
            }
            // 用户标签 / 跳转：标签用 `usr_` 前缀与编译器内部标签（Lstart0/Lend1…）隔离，
            // 后者由 new_label 生成，二者不可能重名。跳转经 pending 修复表解析，
            // 前向/后向跳转都支持（与解释器的 Flow::Goto 语义一致）。
            Stmt::Label { name, .. } => {
                let l = format!("usr_{}", name);
                self.emit_label(&l);
            }
            Stmt::Goto { name, .. } => {
                let l = format!("usr_{}", name);
                self.jmp(&l);
            }
            // 宏定义：已由预处理阶段展开并从 AST 移除（仅为穷尽匹配）
            Stmt::MacroDef { .. } => {}
            // with 上下文管理器 / 字段赋值 / type 定义：VM 暂不支持，解释器兜底
            Stmt::With { .. } => self.fail(codes::NOT_IMPLEMENTED, "VM: `with` 上下文管理器暂不支持"),
            Stmt::FieldAssign { .. } => self.fail(codes::NOT_IMPLEMENTED, "VM: 字段赋值 `obj.field = x` 暂不支持"),
            Stmt::TypeDef { .. } => {
                // type 定义已由 compile 阶段注册方法（仅穷尽匹配）
            }
            Stmt::DebugPrint { expr, .. } => {
                let r = self.compile_expr(expr);
                self.emit(Instr::DebugPrint(r));
            }
            Stmt::Breakpoint { cond, .. } => {
                if let Some(c) = cond {
                    let rc = self.compile_expr(c);
                    let lskip = self.new_label("Lskip");
                    self.jif(rc, &lskip);
                    self.emit(Instr::Breakpoint);
                    self.emit_label(&lskip);
                } else {
                    self.emit(Instr::Breakpoint);
                }
            }
            Stmt::ExprStmt { expr, .. } => {
                self.compile_expr(expr);
            }
            Stmt::Throw { value, .. } => {
                let r = self.compile_expr(value);
                self.emit(Instr::Throw(r));
            }
            Stmt::Try { body, catch_var, handler, .. } => {
                let try_reg = self.decl(catch_var);
                let lhandler = self.new_label("Lhandler");
                let lafter = self.new_label("Lafter");
                let i = self.code.len();
                self.emit(Instr::TryBegin(0, try_reg));
                self.pending.push((i, lhandler.clone()));
                self.enter();
                for s2 in body {
                    self.compile_stmt(s2);
                    self.reset_tmp();
                }
                self.exit();
                self.emit(Instr::TryPop);
                self.jmp(&lafter);
                self.emit_label(&lhandler);
                self.enter();
                for s2 in handler {
                    self.compile_stmt(s2);
                    self.reset_tmp();
                }
                self.exit();
                self.emit_label(&lafter);
            }
            Stmt::Import { .. } => {
                self.fail(codes::NOT_IMPLEMENTED, "VM: import 远程模块暂未支持");
            }
            Stmt::Load { .. } => {
                self.fail(codes::NOT_IMPLEMENTED, "VM: load 动态库暂未支持");
            }
            Stmt::Use { .. } => {
                // 暂作无操作（宿主函数命名空间在本版 VM 中未接）
            }
            Stmt::FnDef { .. }
            | Stmt::AsyncFnDef { .. }
            | Stmt::ClassDef { .. }
            | Stmt::StructDef { .. }
            | Stmt::EnumDef { .. }
            | Stmt::Alias { .. }
            | Stmt::Export { .. } => {}
            Stmt::Go { callee, args, .. } => {
                let n = args.len();
                let base = self.tmp();
                for _ in 0..n {
                    self.tmp();
                }
                for (i, a) in args.iter().enumerate() {
                    let ra = self.compile_expr(a);
                    self.emit(Instr::Move(base + i, ra));
                }
                self.emit(Instr::GoCall(callee.clone(), base, n));
            }
        }
    }

    // ── 表达式编译：返回持有结果的寄存器 ──
    fn compile_expr(&mut self, e: &Expr) -> usize {
        if self.err.is_some() {
            return 0;
        }
        // 保存进入前的 span：emit 时 cur_span 应指向整个表达式起点（而非最后一个
        // 子表达式），这样运行时错误（如除零）的列号与解释器一致。
        let saved_span = self.cur_span.clone();
        self.cur_span = expr_span(e);
        let r = self.compile_expr_inner(e);
        self.cur_span = saved_span;
        r
    }

    fn compile_expr_inner(&mut self, e: &Expr) -> usize {
        match e {
            Expr::IntLit(v, _) => {
                let k = self.const_idx(Const::Int(*v));
                let r = self.tmp();
                self.emit(Instr::LoadK(r, k));
                r
            }
            Expr::FloatLit(v, _) => {
                let k = self.const_idx(Const::Float(*v));
                let r = self.tmp();
                self.emit(Instr::LoadK(r, k));
                r
            }
            Expr::BoolLit(v, _) => {
                let r = self.tmp();
                self.emit(Instr::LoadBool(r, *v));
                r
            }
            Expr::StrLit(v, _) => {
                let k = self.const_idx(Const::Str(v.clone()));
                let r = self.tmp();
                self.emit(Instr::LoadK(r, k));
                r
            }
            Expr::CharLit(v, _) => {
                let k = self.const_idx(Const::Char(*v));
                let r = self.tmp();
                self.emit(Instr::LoadK(r, k));
                r
            }
            // 字节类型/切片/type 实例为解释器特性，VM 暂不支持（fail 兜底）
            Expr::ByteLit(_, _) => {
                self.fail(codes::NOT_IMPLEMENTED, "VM: `byte` 字面量暂不支持");
                0
            }
            Expr::BytesLit(_, _) => {
                self.fail(codes::NOT_IMPLEMENTED, "VM: `bytes` 字面量暂不支持");
                0
            }
            Expr::Slice { .. } => {
                self.fail(codes::NOT_IMPLEMENTED, "VM: 切片 `a[i:j]` 暂不支持");
                0
            }
            Expr::MethodCall { .. } => {
                self.fail(codes::NOT_IMPLEMENTED, "VM: `type` 实例方法暂不支持");
                0
            }
            Expr::New { .. } => {
                self.fail(codes::NOT_IMPLEMENTED, "VM: `type` 实例暂不支持");
                0
            }
            Expr::Ident { name, .. } => match self.lookup(name) {
                Some(r) => r,
                None => {
                    self.fail(codes::UNDEFINED, format!("VM: 未定义变量 `{}`", name));
                    0
                }
            },
            Expr::ListLit(items, _) => {
                let base = self.tmp();
                let n = items.len();
                for _ in 0..n {
                    self.tmp();
                }
                for (i, it) in items.iter().enumerate() {
                    let ri = self.compile_expr(it);
                    self.emit(Instr::Move(base + i, ri));
                }
                let r = self.tmp();
                self.emit(Instr::NewList(r, base, n));
                r
            }
            Expr::DictLit(items, _) => {
                let base = self.tmp();
                let n = items.len();
                for _ in 0..(2 * n) {
                    self.tmp();
                }
                for (i, (k, v)) in items.iter().enumerate() {
                    let rk = self.tmp();
                    let kc = self.const_idx(Const::Str(k.clone()));
                    self.emit(Instr::LoadK(rk, kc));
                    let rv = self.compile_expr(v);
                    self.emit(Instr::Move(base + 2 * i, rk));
                    self.emit(Instr::Move(base + 2 * i + 1, rv));
                }
                let r = self.tmp();
                self.emit(Instr::NewDict(r, base, n));
                r
            }
            Expr::ListComp { elem, var, var2, iter, cond, .. } => {
                self.compile_comp(iter, var, var2, elem, None, cond)
            }
            Expr::DictComp { key, value, var, var2, iter, cond, .. } => {
                self.compile_comp(iter, var, var2, value, Some(key), cond)
            }
            Expr::FStr(segs, _) => self.compile_fstr(segs),
            Expr::Field { obj, field, .. } => {
                // 枚举类型字段访问：Color.Red 视为对 "Color.Red" 的零参调用
                if let Expr::Ident { name, .. } = &**obj {
                    if self.enum_defs.contains_key(name) {
                        let callee = format!("{}.{}", name, field);
                        let base = self.tmp();
                        let r = self.tmp();
                        self.emit(Instr::Call(r, callee, base, 0));
                        return r;
                    }
                }
                let ro = self.compile_expr(obj);
                let r = self.tmp();
                self.emit(Instr::Field(r, ro, field.clone()));
                r
            }
            Expr::OptionalField { obj, field, .. } => {
                let ro = self.compile_expr(obj);
                let r = self.tmp();
                let lnull = self.new_label("Lnull");
                let lend = self.new_label("Lend");
                let rnull = self.tmp();
                self.emit(Instr::IsNull(rnull, ro));
                self.jit(rnull, &lnull);
                self.emit(Instr::Field(r, ro, field.clone()));
                self.jmp(&lend);
                self.emit_label(&lnull);
                self.emit(Instr::LoadNull(r));
                self.emit_label(&lend);
                r
            }
            Expr::Index { obj, index, .. } => {
                let ro = self.compile_expr(obj);
                let rk = self.compile_expr(index);
                let r = self.tmp();
                self.emit(Instr::Index(r, ro, rk));
                r
            }
            Expr::Unary { op, expr, .. } => {
                let ra = self.compile_expr(expr);
                let r = self.tmp();
                match op {
                    UnOp::Neg => self.emit(Instr::Neg(r, ra)),
                    UnOp::Not => self.emit(Instr::Not(r, ra)),
                }
                r
            }
            Expr::Binary { op, lhs, rhs, .. } => {
                let ra = self.compile_expr(lhs);
                let rb = self.compile_expr(rhs);
                let r = self.tmp();
                match op {
                    BinOp::Add => self.emit(Instr::Add(r, ra, rb)),
                    BinOp::Sub => self.emit(Instr::Sub(r, ra, rb)),
                    BinOp::Mul => self.emit(Instr::Mul(r, ra, rb)),
                    BinOp::Div => self.emit(Instr::Div(r, ra, rb)),
                    BinOp::Mod => self.emit(Instr::Mod(r, ra, rb)),
                    BinOp::Eq => self.emit(Instr::Eq(r, ra, rb)),
                    BinOp::Ne => self.emit(Instr::Ne(r, ra, rb)),
                    BinOp::Lt => self.emit(Instr::Lt(r, ra, rb)),
                    BinOp::Le => self.emit(Instr::Le(r, ra, rb)),
                    BinOp::Gt => self.emit(Instr::Gt(r, ra, rb)),
                    BinOp::Ge => self.emit(Instr::Ge(r, ra, rb)),
                    BinOp::And => {
                        // a && b : 若 a 为假，结果为假；否则结果为 b
                        let lend = self.new_label("Lend");
                        let ltrue = self.new_label("LandT");
                        self.jit(ra, &ltrue); // ra 为真 -> 取 b
                        self.emit(Instr::LoadBool(r, false));
                        self.jmp(&lend);
                        self.emit_label(&ltrue);
                        self.emit(Instr::Move(r, rb));
                        self.emit_label(&lend);
                    }
                    BinOp::Or => {
                        // a || b : 若 a 为真，结果为 a；否则结果为 b
                        let lend = self.new_label("Lend");
                        let ltrue = self.new_label("LorT");
                        self.jit(ra, &ltrue); // ra 为真 -> 取 a
                        self.emit(Instr::Move(r, rb));
                        self.jmp(&lend);
                        self.emit_label(&ltrue);
                        self.emit(Instr::Move(r, ra));
                        self.emit_label(&lend);
                    }
                    BinOp::Coalesce => {
                        let lend = self.new_label("Lend");
                        let lelse = self.new_label("Lelse");
                        let rt = self.tmp();
                        self.emit(Instr::IsNull(rt, ra));
                        self.jit(rt, &lelse);
                        self.emit(Instr::Move(r, ra));
                        self.jmp(&lend);
                        self.emit_label(&lelse);
                        self.emit(Instr::Move(r, rb));
                        self.emit_label(&lend);
                    }
                }
                r
            }
            Expr::Call { callee, args, .. } => {
                let n = args.len();
                let base = self.tmp();
                for _ in 0..n {
                    self.tmp();
                }
                for (i, a) in args.iter().enumerate() {
                    let ra = self.compile_expr(a);
                    self.emit(Instr::Move(base + i, ra));
                }
                let r = self.tmp();
                // 一律按名解析：运行时 do_call 会先查当前帧局部变量（lambda 闭包 / 函数名字符串），
                // 再回退到全局函数 / 内置 / 结构体 / 枚举变体 / 方法。这天然支持递归与向前引用。
                self.emit(Instr::Call(r, callee.clone(), base, n));
                r
            }
            Expr::Match { value, arms, .. } => {
                let rv = self.compile_expr(value);
                let lend = self.new_label("Lend");
                let rresult = self.tmp();
                let mut has_wild = false;
                for (pat, body) in arms {
                    let li = self.new_label("Larm");
                    let lnext = self.new_label("Lnext");
                    match pat {
                        Pattern::Lit(pe) => {
                            let re = self.compile_expr(pe);
                            let rt = self.tmp();
                            self.emit(Instr::Eq(rt, rv, re));
                            self.jit(rt, &li);
                            self.jmp(&lnext);
                        }
                        Pattern::Variant { enum_name, variant, .. } => {
                            let rt = self.tmp();
                            self.emit(Instr::IsEnumVariant(
                                rt,
                                rv,
                                enum_name.clone(),
                                variant.clone(),
                            ));
                            self.jit(rt, &li);
                            self.jmp(&lnext);
                        }
                        Pattern::Wildcard => {
                            has_wild = true;
                            self.jmp(&li);
                        }
                    }
                    self.emit_label(&li);
                    // 变体模式：跳转目标之后、求值 body 之前完成载荷绑定
                    if let Pattern::Variant { binds, .. } = pat {
                        for (idx, b) in binds.iter().enumerate() {
                            if let Some(name) = b {
                                let rbind = self.decl(name);
                                self.emit(Instr::EnumElem(rbind, rv, idx));
                            }
                        }
                    }
                    let ra = self.compile_expr(body);
                    self.emit(Instr::Move(rresult, ra));
                    self.jmp(&lend);
                    self.emit_label(&lnext);
                }
                if !has_wild {
                    self.emit(Instr::ThrowStr(
                        "match failed: 无匹配分支 (H005)".to_string(),
                    ));
                }
                self.emit_label(&lend);
                rresult
            }
            Expr::IncDec { op, prefix, name, .. } => {
                let rvar = match self.lookup(name) {
                    Some(r) => r,
                    None => {
                        self.fail(codes::UNDEFINED, format!("VM: 未定义变量 `{}`", name));
                        return 0;
                    }
                };
                let k = self.const_idx(Const::Int(1));
                let rone = self.tmp();
                self.emit(Instr::LoadK(rone, k));
                match (op, prefix) {
                    (IncOp::Inc, false) => {
                        let rold = self.tmp();
                        self.emit(Instr::Move(rold, rvar));
                        let rnew = self.tmp();
                        self.emit(Instr::Add(rnew, rvar, rone));
                        self.emit(Instr::Move(rvar, rnew));
                        rold
                    }
                    (IncOp::Inc, true) => {
                        let rnew = self.tmp();
                        self.emit(Instr::Add(rnew, rvar, rone));
                        self.emit(Instr::Move(rvar, rnew));
                        rnew
                    }
                    (IncOp::Dec, false) => {
                        let rold = self.tmp();
                        self.emit(Instr::Move(rold, rvar));
                        let rnew = self.tmp();
                        self.emit(Instr::Sub(rnew, rvar, rone));
                        self.emit(Instr::Move(rvar, rnew));
                        rold
                    }
                    (IncOp::Dec, true) => {
                        let rnew = self.tmp();
                        self.emit(Instr::Sub(rnew, rvar, rone));
                        self.emit(Instr::Move(rvar, rnew));
                        rnew
                    }
                }
            }
            Expr::Ternary { cond, then_expr, else_expr, .. } => {
                let rc = self.compile_expr(cond);
                let r = self.tmp();
                let lelse = self.new_label("Lelse");
                let lend = self.new_label("Lend");
                self.jif(rc, &lelse);
                let ta = self.compile_expr(then_expr);
                self.emit(Instr::Move(r, ta));
                self.jmp(&lend);
                self.emit_label(&lelse);
                let eb = self.compile_expr(else_expr);
                self.emit(Instr::Move(r, eb));
                self.emit_label(&lend);
                r
            }
            Expr::Lambda { params, body, .. } => {
                // 为 lambda 生成独立 chunk；捕获当前作用域所有可见变量（按值，与解释器一致）。
                let lam_name = format!("λ{}", self.label_seq);
                self.label_seq += 1;

                // 收集当前可见的捕获变量（名字 + 外层寄存器），供 MakeLambda 运行时读取。
                let mut captures: Vec<(String, usize)> = Vec::new();
                for scope in self.scopes.iter() {
                    for (name, &r) in scope.0.iter() {
                        if !captures.iter().any(|(n, _)| n == name) {
                            captures.push((name.clone(), r));
                        }
                    }
                }

                // 快照编译器缓冲：finish_chunk 会清空 code/consts/scopes 等，需还原外层上下文。
                let saved_code = std::mem::take(&mut self.code);
                let saved_spans = std::mem::take(&mut self.spans);
                let saved_consts = std::mem::take(&mut self.consts);
                let saved_labels = std::mem::take(&mut self.labels);
                let saved_pending = std::mem::take(&mut self.pending);
                let saved_scopes = std::mem::take(&mut self.scopes);
                let saved_cur_locals = std::mem::take(&mut self.cur_locals);
                let saved_var_next = self.var_next;
                let saved_tmp_next = self.tmp_next;
                let saved_max_reg = self.max_reg;
                let saved_loop = std::mem::take(&mut self.loop_labels);

                // 在 lambda chunk 内先声明捕获变量（注册到作用域供体引用），再声明形参。
                self.enter();
                let mut cap_regs: Vec<(String, usize)> = Vec::new();
                for (name, _) in &captures {
                    let r = self.decl(name);
                    cap_regs.push((name.clone(), r));
                }
                let mut pnames = Vec::new();
                for p in params {
                    self.decl(&p.name);
                    pnames.push(p.name.clone());
                }
                for s in body {
                    self.compile_stmt(s);
                    self.reset_tmp();
                }
                self.emit(Instr::RetNull);
                self.exit();
                let idx = self.finish_chunk(&lam_name, pnames);
                self.chunks[idx].captured_regs = cap_regs;

                // 还原外层编译缓冲。
                self.code = saved_code;
                self.spans = saved_spans;
                self.consts = saved_consts;
                self.labels = saved_labels;
                self.pending = saved_pending;
                self.scopes = saved_scopes;
                self.cur_locals = saved_cur_locals;
                self.var_next = saved_var_next;
                self.tmp_next = saved_tmp_next;
                self.max_reg = saved_max_reg;
                self.loop_labels = saved_loop;

                // 在当前作用域发出 MakeLambda：运行时读取 captures 的寄存器值构建闭包。
                let r = self.tmp();
                self.emit(Instr::MakeLambda(r, idx, captures.clone()));
                r
            }
            Expr::Await { expr, .. } => {
                let r = self.compile_expr(expr);
                self.emit(Instr::Await(r, r));
                r
            }
        }
    }

    fn compile_fstr(&mut self, segs: &[FStrSeg]) -> usize {
        let r = self.tmp();
        let k = self.const_idx(Const::Str(String::new()));
        self.emit(Instr::LoadK(r, k));
        for seg in segs {
            let part = match seg {
                FStrSeg::Lit(s) => {
                    let k = self.const_idx(Const::Str(s.clone()));
                    let rp = self.tmp();
                    self.emit(Instr::LoadK(rp, k));
                    rp
                }
                FStrSeg::Code(e) => {
                    let re = self.compile_expr(e);
                    let rs = self.tmp();
                    self.emit(Instr::Call(rs, "to_str".to_string(), re, 1));
                    rs
                }
            };
            let rcat = self.tmp();
            self.emit(Instr::Add(rcat, r, part));
            self.emit(Instr::Move(r, rcat));
        }
        r
    }

    // 推导式：value_expr 为每次产出值；key_expr 仅字典推导存在（此时写入 dict）。
    fn compile_comp(
        &mut self,
        iter: &Expr,
        var: &str,
        var2: &Option<String>,
        value_expr: &Expr,
        key_expr: Option<&Expr>,
        cond: &Option<Box<Expr>>,
    ) -> usize {
        // 推导式是一个自包含的临时作用域：内部所有寄存器（结果、迭代源、循环变量、
        // 循环内临时量）都从 tmp_next 分配并整体管理，结束仅保留结果寄存器 rres，其余回收。
        // 这样推导式作为更大表达式（函数实参 / 二元操作数等）的子表达式时，既不会与外围
        // 临时寄存器冲突（此前用 enter/exit 重置水位导致 append 收到 int），也不会让变量区泄漏。
        let saved_var = self.var_next;
        let saved_tmp = self.tmp_next;
        // 结果寄存器先分配，使其处于隔离区最底层，结束时只保留它即可回收其余临时寄存器。
        let rres = self.tmp();
        let riter = self.compile_expr(iter);
        // 迭代源必须是 list/dict，否则报与解释器一致的错误（列号指向迭代源）。
        let saved_span = self.cur_span.clone();
        self.cur_span = expr_span(iter);
        self.emit(Instr::IterCheck(riter, true));
        self.cur_span = saved_span;
        let rb = self.tmp();
        match key_expr {
            None => self.emit(Instr::NewList(rres, rb, 0)),
            Some(_) => self.emit(Instr::NewDict(rres, rb, 0)),
        }
        let risdict = self.tmp();
        self.emit(Instr::IsDict(risdict, riter));
        let ldict = self.new_label("Ldict");
        let llist = self.new_label("Llist");
        let lend = self.new_label("Lend");
        self.jit(risdict, &ldict);
        // ── 列表路径 ──
        self.emit_label(&llist);
        self.scopes.push((HashMap::new(), self.var_next));
        let rvar = self.decl_tmp(var);
        let _syn = self.syn();
        let rcnt = self.decl_tmp(&_syn);
        let k0 = self.const_idx(Const::Int(0));
        self.emit(Instr::LoadK(rcnt, k0));
        let lcond = self.new_label("Lcond");
        let linc = self.new_label("Linc");
        let lend2 = self.new_label("LlistEnd");
        self.emit_label(&lcond);
        let rlen = self.tmp();
        self.emit(Instr::Len(rlen, riter));
        let rcond = self.tmp();
        self.emit(Instr::Lt(rcond, rcnt, rlen));
        self.jif(rcond, &lend2);
        let relem = self.tmp();
        self.emit(Instr::Index(relem, riter, rcnt));
        self.emit(Instr::Move(rvar, relem));
        self.emit_loop_push(rres, value_expr, key_expr, cond);
        self.emit_label(&linc);
        let rone = self.tmp();
        let k = self.const_idx(Const::Int(1));
        self.emit(Instr::LoadK(rone, k));
        let rnext = self.tmp();
        self.emit(Instr::Add(rnext, rcnt, rone));
        self.emit(Instr::Move(rcnt, rnext));
        self.jmp(&lcond);
        self.emit_label(&lend2);
        self.scopes.pop();
        // 列表路径结束后必须跳过字典路径，否则会落到 ldict 对列表调用 keys。
        self.jmp(&lend);
        // ── 字典路径 ──
        self.emit_label(&ldict);
        self.scopes.push((HashMap::new(), self.var_next));
        let rvar2 = self.decl_tmp(var);
        let rdvar = if let Some(v2) = var2 {
            Some(self.decl_tmp(v2))
        } else {
            None
        };
        let _syn = self.syn();
        let drcnt = self.decl_tmp(&_syn);
        // 计数器初始化为 0（此前漏掉，导致循环条件用未初始化值直接判定并退出）。
        let kz = self.const_idx(Const::Int(0));
        self.emit(Instr::LoadK(drcnt, kz));
        // 键列表与长度在整个循环中不变，放在循环外只算一次。
        let drkeys = self.tmp();
        self.emit(Instr::Keys(drkeys, riter));
        let drlen = self.tmp();
        self.emit(Instr::Len(drlen, drkeys));
        let dcond = self.new_label("Dcond");
        let dinc = self.new_label("Dinc");
        let dend = self.new_label("DictEnd");
        self.emit_label(&dcond);
        let drcond = self.tmp();
        self.emit(Instr::Lt(drcond, drcnt, drlen));
        self.jif(drcond, &dend);
        let drkey = self.tmp();
        self.emit(Instr::Index(drkey, drkeys, drcnt));
        self.emit(Instr::Move(rvar2, drkey));
        // 双变量形式 `for k, v in dict`：v 取该键对应的值。
        if let Some(rdv) = rdvar {
            let drval = self.tmp();
            self.emit(Instr::Index(drval, riter, drkey));
            self.emit(Instr::Move(rdv, drval));
        }
        self.emit_loop_push(rres, value_expr, key_expr, cond);
        self.emit_label(&dinc);
        let rone2 = self.tmp();
        let k2 = self.const_idx(Const::Int(1));
        self.emit(Instr::LoadK(rone2, k2));
        let rnext2 = self.tmp();
        self.emit(Instr::Add(rnext2, drcnt, rone2));
        self.emit(Instr::Move(drcnt, rnext2));
        self.jmp(&dcond);
        self.emit_label(&dend);
        self.scopes.pop();
        self.jmp(&lend);
        self.emit_label(&lend);
        // 回收隔离区：仅保留结果寄存器（其下标为 saved_tmp）。
        self.var_next = saved_var;
        self.tmp_next = saved_tmp + 1;
        rres
    }

    // 在推导式循环体内：可选条件过滤 + 产出（写入列表或字典）。
    fn emit_loop_push(
        &mut self,
        rres: usize,
        value_expr: &Expr,
        key_expr: Option<&Expr>,
        cond: &Option<Box<Expr>>,
    ) {
        let after = self.new_label("Lafter");
        if let Some(c) = cond {
            let rc = self.compile_expr(c);
            self.jif(rc, &after);
        }
        match key_expr {
            Some(ke) => {
                let rk = self.compile_expr(ke);
                let rv = self.compile_expr(value_expr);
                self.emit(Instr::IndexSet(rres, rk, rv));
            }
            None => {
                let re = self.compile_expr(value_expr);
                let base = self.tmp();
                self.tmp();
                self.emit(Instr::Move(base, rres));
                self.emit(Instr::Move(base + 1, re));
                let rnew = self.tmp();
                self.emit(Instr::Call(rnew, "append".to_string(), base, 2));
                self.emit(Instr::Move(rres, rnew));
            }
        }
        self.emit_label(&after);
    }
}

// ───────────────────────────── VM 执行 ─────────────────────────────

struct Frame {
    chunk: usize,
    pc: usize,
    regs: Vec<Value>,
}

struct TryFrame {
    handler: usize,
    catch_reg: usize,
    /// 建立该 try 的帧下标。跨帧 throw 时只有建立帧才能处理，
    /// 否则必须向上层帧传播（让 call_chunk 弹出本帧后由建立帧的 exec 循环捕获）。
    frame: usize,
}

pub struct Vm {
    chunks: Vec<Chunk>,
    func_map: HashMap<String, usize>,
    struct_defs: HashMap<String, Vec<String>>,
    enum_defs: HashMap<String, Vec<(String, usize)>>,
    aliases: Vec<(String, String)>,
    async_fns: HashSet<String>,
    file: String,
    src: String,
    debug: bool,
    frames: Vec<Frame>,
    try_stack: Vec<TryFrame>,
}

impl Vm {
    fn new(
        chunks: Vec<Chunk>,
        func_map: HashMap<String, usize>,
        struct_defs: HashMap<String, Vec<String>>,
        enum_defs: HashMap<String, Vec<(String, usize)>>,
        aliases: Vec<(String, String)>,
        async_fns: HashSet<String>,
        file: &str,
        src: &str,
        debug: bool,
    ) -> Self {
        Vm {
            chunks,
            func_map,
            struct_defs,
            enum_defs,
            aliases,
            async_fns,
            file: file.to_string(),
            src: src.to_string(),
            debug,
            frames: Vec::new(),
            try_stack: Vec::new(),
        }
    }

    fn mk_err(&self, code: &'static str, msg: String, span: &Span) -> Value {
        let help = vm_help_for(code, &msg);
        self.mk_err_with(code, msg, span, help)
    }

    /// 同 `mk_err`，但显式指定 help。
    /// 用于「同一消息文本在不同语义下 help 不同」的场景——例如 `cannot compare`
    /// 在相等判断（`values_eq`）与大小比较（`values_cmp`）下解释器给出不同 help。
    fn mk_err_with(
        &self,
        code: &'static str,
        msg: String,
        span: &Span,
        help: Option<String>,
    ) -> Value {
        // context 填充该行源码文本，与解释器 ErrorObj::from_err 一致（catch e 里 e.context）。
        let line_text = self
            .src
            .lines()
            .nth(span.line.saturating_sub(1))
            .unwrap_or("")
            .trim_end()
            .to_string();
        Value::Error(ErrorObj {
            code,
            message: msg,
            file: self.file.clone(),
            line: span.line,
            col: span.col,
            len: span.len.max(1),
            context: line_text,
            help,
        })
    }

    fn exec_program(&mut self, main_idx: usize) -> Result<(), ZError> {
        let nregs = self.chunks[main_idx].nregs;
        self.frames.push(Frame {
            chunk: main_idx,
            pc: 0,
            regs: vec![Value::Null; nregs],
        });
        let res = self.exec();
        self.frames.pop();
        match res {
            Ok(_) => Ok(()),
            Err(thrown) => Err(self.value_to_zerror(&thrown)),
        }
    }

    fn value_to_zerror(&self, v: &Value) -> ZError {
        if let Value::Error(e) = v {
            ZError::new(
                e.code,
                e.message.clone(),
                &e.file,
                &self.src,
                e.line,
                e.col,
                e.len.max(1),
                e.help.clone(),
            )
        } else {
            ZError::plain(codes::THROW, format!("{}", value_to_str(v)), None::<String>)
        }
    }

    /// 尝试用栈顶 try 处理一个被抛出的异常。
    /// 仅当该 try 由「当前顶层帧」建立时才在本帧内捕获（设置 catch 寄存器并跳转到 handler）；
    /// 若 try 由更低层帧建立（异常来自被调用函数），则必须向上传播——
    /// 返回 false 让调用方 exec 循环把本帧弹出后由建立帧捕获，从而避免跳转到错误的 chunk。
    fn try_catch(&mut self, thrown: &Value) -> bool {
        let top = self.frames.len() - 1;
        if let Some(tf) = self.try_stack.last() {
            if tf.frame == top {
                let tf = self.try_stack.pop().unwrap();
                self.frames[top].regs[tf.catch_reg] = thrown.clone();
                self.frames[top].pc = tf.handler;
                return true;
            }
        }
        false
    }

    fn exec(&mut self) -> Result<Value, Value> {
        loop {
            let fi = self.frames.len() - 1;
            let pc = self.frames[fi].pc;
            let code_len = self.chunks[self.frames[fi].chunk].code.len();
            if pc >= code_len {
                return Ok(Value::Null);
            }
            let instr = self.chunks[self.frames[fi].chunk].code[pc].clone();
            let span = self.chunks[self.frames[fi].chunk].spans[pc].clone();
            match instr {
                Instr::Ret(r) => return Ok(self.frames[fi].regs[r].clone()),
                Instr::RetNull => return Ok(Value::Null),
                Instr::Jmp(t) => {
                    self.frames[fi].pc = t;
                    continue;
                }
                Instr::JmpIfFalse(r, t) => {
                    let v = self.frames[fi].regs[r].clone();
                    if !is_truthy(&v) {
                        self.frames[fi].pc = t;
                    } else {
                        self.frames[fi].pc = pc + 1;
                    }
                    continue;
                }
                Instr::JmpIfTrue(r, t) => {
                    let v = self.frames[fi].regs[r].clone();
                    if is_truthy(&v) {
                        self.frames[fi].pc = t;
                    } else {
                        self.frames[fi].pc = pc + 1;
                    }
                    continue;
                }
                other => match self.exec_instr(other, &span) {
                    Ok(()) => self.frames[fi].pc = pc + 1,
                    Err(thrown) => {
                        if self.try_catch(&thrown) {
                            continue;
                        } else {
                            return Err(thrown);
                        }
                    }
                },
            }
        }
    }

    fn exec_instr(&mut self, ins: Instr, span: &Span) -> Result<(), Value> {
        let fi = self.frames.len() - 1;
        match ins {
            Instr::LoadK(r, k) => {
                let c = self.chunks[self.frames[fi].chunk].consts[k].clone();
                self.frames[fi].regs[r] = const_to_value(c);
            }
            Instr::LoadNull(r) => self.frames[fi].regs[r] = Value::Null,
            Instr::LoadBool(r, v) => self.frames[fi].regs[r] = Value::Bool(v),
            Instr::Move(d, s) => {
                self.frames[fi].regs[d] = self.frames[fi].regs[s].clone();
            }
            Instr::Neg(d, s) => {
                let v = self.frames[fi].regs[s].clone();
                let result = match v_neg(v.clone(), span, self) {
                    Ok(r) => Ok(r),
                    Err(e) if err_code(&e) == Some(codes::TYPE_MISMATCH) => {
                        match self.overload("__neg", vec![v]) {
                            Some(r) => r,
                            None => Err(e),
                        }
                    }
                    Err(e) => Err(e),
                };
                self.frames[fi].regs[d] = result?;
            }
            Instr::Not(d, s) => {
                let v = self.frames[fi].regs[s].clone();
                let result = match v_not(v.clone(), span, self) {
                    Ok(r) => Ok(r),
                    Err(e) if err_code(&e) == Some(codes::TYPE_MISMATCH) => {
                        match self.overload("__not", vec![v]) {
                            Some(r) => r,
                            None => Err(e),
                        }
                    }
                    Err(e) => Err(e),
                };
                self.frames[fi].regs[d] = result?;
            }
            Instr::Add(d, a, b) => {
                let x = self.frames[fi].regs[a].clone();
                let y = self.frames[fi].regs[b].clone();
                let tn_a = x.type_name();
                let tn_b = y.type_name();
                let native = is_num_or_str_pair(&x, &y);
                let result = match v_add(x.clone(), y.clone(), span, self, "+", tn_a, tn_b) {
                    Ok(v) => Ok(v),
                    Err(e) if !native && err_code(&e) == Some(codes::TYPE_MISMATCH) => {
                        match self.overload("__add", vec![x, y]) {
                            Some(r) => r,
                            None => Err(e),
                        }
                    }
                    Err(e) => Err(e),
                };
                self.frames[fi].regs[d] = result?;
            }
            Instr::Sub(d, a, b) => {
                let x = self.frames[fi].regs[a].clone();
                let y = self.frames[fi].regs[b].clone();
                let tn_a = x.type_name();
                let tn_b = y.type_name();
                let native = is_num_or_str_pair(&x, &y);
                let result = match v_sub(x.clone(), y.clone(), span, self, "-", tn_a, tn_b) {
                    Ok(v) => Ok(v),
                    Err(e) if !native && err_code(&e) == Some(codes::TYPE_MISMATCH) => {
                        match self.overload("__sub", vec![x, y]) {
                            Some(r) => r,
                            None => Err(e),
                        }
                    }
                    Err(e) => Err(e),
                };
                self.frames[fi].regs[d] = result?;
            }
            Instr::Mul(d, a, b) => {
                let x = self.frames[fi].regs[a].clone();
                let y = self.frames[fi].regs[b].clone();
                let tn_a = x.type_name();
                let tn_b = y.type_name();
                let native = is_num_or_str_pair(&x, &y);
                let result = match v_mul(x.clone(), y.clone(), span, self, "*", tn_a, tn_b) {
                    Ok(v) => Ok(v),
                    Err(e) if !native && err_code(&e) == Some(codes::TYPE_MISMATCH) => {
                        match self.overload("__mul", vec![x, y]) {
                            Some(r) => r,
                            None => Err(e),
                        }
                    }
                    Err(e) => Err(e),
                };
                self.frames[fi].regs[d] = result?;
            }
            Instr::Div(d, a, b) => {
                let x = self.frames[fi].regs[a].clone();
                let y = self.frames[fi].regs[b].clone();
                let tn_a = x.type_name();
                let tn_b = y.type_name();
                let native = is_num_or_str_pair(&x, &y);
                let result = match v_div(x.clone(), y.clone(), span, self, "/", tn_a, tn_b) {
                    Ok(v) => Ok(v),
                    Err(e) if !native && err_code(&e) == Some(codes::TYPE_MISMATCH) => {
                        match self.overload("__div", vec![x, y]) {
                            Some(r) => r,
                            None => Err(e),
                        }
                    }
                    Err(e) => Err(e),
                };
                self.frames[fi].regs[d] = result?;
            }
            Instr::Mod(d, a, b) => {
                let x = self.frames[fi].regs[a].clone();
                let y = self.frames[fi].regs[b].clone();
                let tn_a = x.type_name();
                let tn_b = y.type_name();
                let native = is_num_or_str_pair(&x, &y);
                let result = match v_mod(x.clone(), y.clone(), span, self, "%", tn_a, tn_b) {
                    Ok(v) => Ok(v),
                    Err(e) if !native && err_code(&e) == Some(codes::TYPE_MISMATCH) => {
                        match self.overload("__mod", vec![x, y]) {
                            Some(r) => r,
                            None => Err(e),
                        }
                    }
                    Err(e) => Err(e),
                };
                self.frames[fi].regs[d] = result?;
            }
            Instr::Eq(d, a, b) => {
                let x = self.frames[fi].regs[a].clone();
                let y = self.frames[fi].regs[b].clone();
                let result = match v_eq(&x, &y, span, self) {
                    Ok(e) => Ok(Value::Bool(e)),
                    Err(e) => match self.overload("__eq", vec![x, y]) {
                        Some(r) => r,
                        None => Err(e),
                    },
                };
                self.frames[fi].regs[d] = result?;
            }
            Instr::Ne(d, a, b) => {
                let x = self.frames[fi].regs[a].clone();
                let y = self.frames[fi].regs[b].clone();
                let result = match v_eq(&x, &y, span, self) {
                    Ok(e) => Ok(Value::Bool(!e)),
                    Err(e) => match self.overload("__ne", vec![x, y]) {
                        Some(r) => r,
                        None => Err(e),
                    },
                };
                self.frames[fi].regs[d] = result?;
            }
            Instr::Lt(d, a, b) => {
                let x = self.frames[fi].regs[a].clone();
                let y = self.frames[fi].regs[b].clone();
                let result = match v_cmp(x.clone(), y.clone(), span, self, |o| o < 0) {
                    Ok(v) => Ok(v),
                    Err(e) => {
                        if is_num_pair(&x, &y) {
                            Err(e)
                        } else {
                            match self.overload("__lt", vec![x, y]) {
                                Some(r) => r,
                                None => Err(e),
                            }
                        }
                    }
                };
                self.frames[fi].regs[d] = result?;
            }
            Instr::Le(d, a, b) => {
                let x = self.frames[fi].regs[a].clone();
                let y = self.frames[fi].regs[b].clone();
                let result = match v_cmp(x.clone(), y.clone(), span, self, |o| o <= 0) {
                    Ok(v) => Ok(v),
                    Err(e) => {
                        if is_num_pair(&x, &y) {
                            Err(e)
                        } else {
                            match self.overload("__le", vec![x, y]) {
                                Some(r) => r,
                                None => Err(e),
                            }
                        }
                    }
                };
                self.frames[fi].regs[d] = result?;
            }
            Instr::Gt(d, a, b) => {
                let x = self.frames[fi].regs[a].clone();
                let y = self.frames[fi].regs[b].clone();
                let result = match v_cmp(x.clone(), y.clone(), span, self, |o| o > 0) {
                    Ok(v) => Ok(v),
                    Err(e) => {
                        if is_num_pair(&x, &y) {
                            Err(e)
                        } else {
                            match self.overload("__gt", vec![x, y]) {
                                Some(r) => r,
                                None => Err(e),
                            }
                        }
                    }
                };
                self.frames[fi].regs[d] = result?;
            }
            Instr::Ge(d, a, b) => {
                let x = self.frames[fi].regs[a].clone();
                let y = self.frames[fi].regs[b].clone();
                let result = match v_cmp(x.clone(), y.clone(), span, self, |o| o >= 0) {
                    Ok(v) => Ok(v),
                    Err(e) => {
                        if is_num_pair(&x, &y) {
                            Err(e)
                        } else {
                            match self.overload("__ge", vec![x, y]) {
                                Some(r) => r,
                                None => Err(e),
                            }
                        }
                    }
                };
                self.frames[fi].regs[d] = result?;
            }
            Instr::IsNull(d, s) => {
                self.frames[fi].regs[d] =
                    Value::Bool(matches!(self.frames[fi].regs[s], Value::Null));
            }
            Instr::IsDict(d, s) => {
                self.frames[fi].regs[d] =
                    Value::Bool(matches!(self.frames[fi].regs[s], Value::Dict(_)));
            }
            Instr::IterCheck(s, is_comp) => {
                if !matches!(
                    self.frames[fi].regs[s],
                    Value::List(_) | Value::Dict(_)
                ) {
                    let what = if is_comp { "comprehension" } else { "`for in`" };
                    let tn = self.frames[fi].regs[s].type_name();
                    return Err(self.mk_err(
                        codes::TYPE_MISMATCH,
                        format!("{} requires a list or dict, got `{}`", what, tn),
                        span,
                    ));
                }
            }
            Instr::Index(d, o, k) => {
                let obj = self.frames[fi].regs[o].clone();
                let key = self.frames[fi].regs[k].clone();
                // 定义了 `__index` 时：先试内建索引，类型不支持则回退重载（镜像解释器）。
                let result = if self.func_map.contains_key("__index") {
                    match v_index(&obj, &key, span, self) {
                        Ok(v) => Ok(v),
                        Err(e) if err_code(&e) == Some(codes::TYPE_MISMATCH) => {
                            match self.overload("__index", vec![obj, key]) {
                                Some(r) => r,
                                None => Err(e),
                            }
                        }
                        Err(e) => Err(e),
                    }
                } else {
                    v_index(&obj, &key, span, self)
                };
                self.frames[fi].regs[d] = result?;
            }
            Instr::IndexSet(o, k, v) => {
                let key = self.frames[fi].regs[k].clone();
                let val = self.frames[fi].regs[v].clone();
                let mut dst = self.frames[fi].regs[o].clone();
                v_index_set(&mut dst, &key, val, span, self)?;
                self.frames[fi].regs[o] = dst;
            }
            Instr::DestructGet(d, o, k) => {
                let obj = self.frames[fi].regs[o].clone();
                let key = self.frames[fi].regs[k].clone();
                self.frames[fi].regs[d] = v_destruct_get(&obj, &key, span, self)?;
            }
            Instr::Field(d, o, field) => {
                let obj = self.frames[fi].regs[o].clone();
                self.frames[fi].regs[d] = v_field(&obj, &field, span, self)?;
            }
            Instr::Len(d, s) => {
                let v = self.frames[fi].regs[s].clone();
                self.frames[fi].regs[d] = v_len(&v, span, self)?;
            }
            Instr::Keys(d, s) => {
                let v = self.frames[fi].regs[s].clone();
                self.frames[fi].regs[d] = v_keys(&v, span, self)?;
            }
            Instr::NewList(d, base, n) => {
                let mut v = Vec::with_capacity(n);
                for i in 0..n {
                    v.push(self.frames[fi].regs[base + i].clone());
                }
                self.frames[fi].regs[d] = Value::List(v);
            }
            Instr::NewDict(d, base, n) => {
                let mut v = Vec::with_capacity(n);
                for i in 0..n {
                    let key = match &self.frames[fi].regs[base + 2 * i] {
                        Value::Str(s) => s.clone(),
                        other => value_to_str(other),
                    };
                    let val = self.frames[fi].regs[base + 2 * i + 1].clone();
                    v.push((key, val));
                }
                self.frames[fi].regs[d] = Value::Dict(v);
            }
            Instr::EnumElem(d, s, idx) => {
                let v = self.frames[fi].regs[s].clone();
                match v {
                    Value::Enum(e) => {
                        if idx < e.payload.len() {
                            self.frames[fi].regs[d] = e.payload[idx].clone();
                        } else {
                            return Err(self.mk_err(codes::TYPE_MISMATCH, "枚举载荷索引越界".into(), span));
                        }
                    }
                    _ => {
                        return Err(self.mk_err(codes::TYPE_MISMATCH, "EnumElem 作用于非枚举值".into(), span))
                    }
                }
            }
            Instr::IsEnumVariant(d, s, ename, variant) => {
                let v = self.frames[fi].regs[s].clone();
                let ok = matches!(&v, Value::Enum(e) if e.ty == ename && e.variant == variant);
                self.frames[fi].regs[d] = Value::Bool(ok);
            }
            Instr::NewEnum(d, ename, variant, base, n) => {
                let mut payload = Vec::with_capacity(n);
                for i in 0..n {
                    payload.push(self.frames[fi].regs[base + i].clone());
                }
                self.frames[fi].regs[d] =
                    Value::Enum(Arc::new(EnumVal { ty: ename, variant, payload }));
            }
            Instr::Call(res, func, base, n) => {
                let args: Vec<Value> =
                    (base..base + n).map(|i| self.frames[fi].regs[i].clone()).collect();
                match self.do_call(&func, args, span) {
                    Ok(v) => self.frames[fi].regs[res] = v,
                    Err(t) => return Err(t),
                }
            }
            Instr::MakeLambda(d, ci, reads) => {
                let mut captured: HashMap<String, Value> = HashMap::new();
                for (name, r) in &reads {
                    captured.insert(name.clone(), self.frames[fi].regs[*r].clone());
                }
                self.frames[fi].regs[d] = Value::Lambda(Arc::new(LambdaVal {
                    params: Vec::new(),
                    body: Vec::new(),
                    captured,
                    vm_chunk: Some(ci),
                }));
            }
            Instr::Await(d, fr) => {
                let fv = self.frames[fi].regs[fr].clone();
                match fv {
                    Value::Future(f) => match f.wait() {
                        Ok(v) => self.frames[fi].regs[d] = v,
                        Err(e) => return Err(Value::Error(ErrorObj::from_err(&e))),
                    },
                    other => {
                        return Err(self.mk_err_with(
                            codes::TYPE_MISMATCH,
                            format!(
                                "`await` requires an async function call result (future), got `{}`",
                                other.type_name()
                            ),
                            span,
                            Some("await an async function call: `await fetch_data()`".to_string()),
                        ))
                    }
                }
            }
            Instr::GoCall(callee, base, n) => {
                let args: Vec<Value> =
                    (base..base + n).map(|i| self.frames[fi].regs[i].clone()).collect();
                self.spawn_go(&callee, args);
            }
            Instr::TryBegin(handler, catch_reg) => {
                let frame = self.frames.len() - 1;
                self.try_stack.push(TryFrame { handler, catch_reg, frame });
            }
            Instr::TryPop => {
                self.try_stack.pop();
            }
            Instr::Throw(reg) => {
                let v = self.frames[fi].regs[reg].clone();
                return Err(normalize_throw(v));
            }
            Instr::ThrowStr(s) => {
                return Err(Value::Error(ErrorObj {
                    code: codes::THROW,
                    message: s,
                    file: self.file.clone(),
                    line: span.line,
                    col: span.col,
                    len: span.len.max(1),
                    context: String::new(),
                    help: None,
                }));
            }
            Instr::DebugPrint(reg) => {
                if self.debug {
                    let v = self.frames[fi].regs[reg].clone();
                    eprintln!("[vm debug] {}", value_to_str(&v));
                }
            }
            Instr::Breakpoint => {
                if self.debug {
                    let f = &self.frames[fi];
                    eprintln!("[vm breakpoint] @ {}:{}", span.line, span.col);
                    for (i, rv) in f.regs.iter().enumerate() {
                        eprintln!("  r{} = {}", i, value_to_str(rv));
                    }
                    let mut line = String::new();
                    let _ = std::io::stdin().read_line(&mut line);
                }
            }
            Instr::Label(_) | Instr::Nop => {}
            Instr::Ret(_)
            | Instr::RetNull
            | Instr::Jmp(_)
            | Instr::JmpIfFalse(_, _)
            | Instr::JmpIfTrue(_, _) => {
                unreachable!("控制流指令由 exec 主循环处理，不应进入 exec_instr")
            }
        }
        Ok(())
    }

    fn resolve_alias(&self, name: &str) -> String {
        let mut cur = name.to_string();
        for _ in 0..=self.aliases.len() {
            if let Some(orig) = self
                .aliases
                .iter()
                .find(|(_, n)| n == &cur)
                .map(|(o, _)| o.clone())
            {
                cur = orig;
            } else {
                break;
            }
        }
        cur
    }

    /// 运算符重载回退：若顶层定义了 `name`（如 `__add` / `__lt` / `__index`），调用之并返回
    /// `Some(result)`；未定义则返回 `None`（由调用方继续抛出原错误）。
    /// 镜像解释器 `Interp::call_overload`：仅在内建运算因操作数类型不匹配而失败时尝试。
    fn overload(&mut self, name: &str, args: Vec<Value>) -> Option<Result<Value, Value>> {
        if let Some(&ci) = self.func_map.get(name) {
            Some(self.call_chunk(ci, args))
        } else {
            None
        }
    }

    fn do_call(&mut self, func: &str, args: Vec<Value>, span: &Span) -> Result<Value, Value> {
        let func = self.resolve_alias(func);
        // 先查当前帧局部变量：若持有 lambda 闭包或函数名字符串，按其调用（与解释器 env.get 一致，
        // 从而支持 lambda 变量调用、递归、向前引用——这些都无法在编译期判定）。
        let fi = self.frames.len() - 1;
        if let Some(&reg) = self.chunks[self.frames[fi].chunk].locals.get(&func) {
            let v = self.frames[fi].regs[reg].clone();
            match v {
                Value::Lambda(l) => return self.call_lambda(&l, args, span),
                Value::Str(s) => return self.do_call(&s, args, span),
                _ => {}
            }
        }
        // 异步函数调用：后台线程执行，立即返回 future（等待由 await 完成）。
        if self.async_fns.contains(&func) {
            return self.spawn_async(&func, args);
        }
        if let Some((head, tail)) = func.split_once('.') {
            if let Some(variants) = self.enum_defs.get(head) {
                if variants.iter().any(|(v, _)| v == tail) {
                    return Ok(Value::Enum(Arc::new(EnumVal {
                        ty: head.to_string(),
                        variant: tail.to_string(),
                        payload: args,
                    })));
                }
            }
            if let Some(&ci) = self.func_map.get(&func) {
                return self.call_chunk(ci, args);
            }
            if builtins::is_builtin(&func) {
                return self.call_builtin(&func, args, span);
            }
            // 点号调用且非枚举/函数/内置：与解释器一致，视为库方法调用 → 库未加载错误。
            let dot = func.rfind('.').unwrap_or(func.len());
            let lib_name = func[..dot].to_string();
            return Err(self.mk_err_with(
                codes::NOT_FOUND,
                format!("library `{}` is not loaded", lib_name),
                span,
                Some(format!(
                    "add `load \"path/to/lib\" as {};` or `plugin.load(path, \"{}\")` before calling",
                    lib_name, lib_name
                )),
            ));
        }
        if let Some(&ci) = self.func_map.get(&func) {
            return self.call_chunk(ci, args);
        }
        if let Some(fields) = self.struct_defs.get(&func) {
            let mut d = Vec::new();
            d.push(("\u{0}__struct__".to_string(), Value::Str(func.clone())));
            for (i, f) in fields.iter().enumerate() {
                let v = args.get(i).cloned().unwrap_or(Value::Null);
                d.push((f.clone(), v));
            }
            return Ok(Value::Dict(d));
        }
        // len 重载：内建 `len` 失败（类型不支持）时回退 `__len`（与解释器 call_fn 一致）。
        if func == "len" && self.func_map.contains_key("__len") {
            return match self.call_builtin(&func, args.clone(), span) {
                Ok(v) => Ok(v),
                Err(e) if err_code(&e) == Some(codes::TYPE_MISMATCH) => {
                    match self.overload("__len", args) {
                        Some(r) => r,
                        None => Err(e),
                    }
                }
                Err(e) => Err(e),
            };
        }
        if builtins::is_builtin(&func) {
            return self.call_builtin(&func, args, span);
        }
        Err(self.mk_err(codes::UNDEFINED, format!("undefined function `{}`", func), span))
    }

    fn call_chunk(&mut self, ci: usize, args: Vec<Value>) -> Result<Value, Value> {
        let expected = self.chunks[ci].params.len();
        if args.len() != expected {
            return Err(Value::Error(ErrorObj {
                code: codes::ARG_COUNT,
                message: format!(
                    "函数 `{}` 期望 {} 个参数，收到 {}",
                    self.chunks[ci].name, expected, args.len()
                ),
                file: self.file.clone(),
                line: 0,
                col: 0,
                len: 0,
                context: String::new(),
                help: None,
            }));
        }
        let nregs = self.chunks[ci].nregs;
        let mut regs = vec![Value::Null; nregs];
        for (i, a) in args.iter().enumerate() {
            if i < nregs {
                regs[i] = a.clone();
            }
        }
        self.frames.push(Frame { chunk: ci, pc: 0, regs });
        let res = self.exec();
        self.frames.pop();
        res
    }

    fn call_builtin(&mut self, func: &str, args: Vec<Value>, span: &Span) -> Result<Value, Value> {
        match builtins::call(func, args, span.clone(), &self.file, &self.src) {
            Ok(v) => Ok(v),
            Err(ze) => Err(Value::Error(ErrorObj::from_err(&ze))),
        }
    }

    /// 调用 lambda 闭包：先将其捕获环境填入前 captured_regs.len() 个寄存器，
    /// 再将实参绑定到后续寄存器，执行闭包自身的 chunk。
    fn call_lambda(&mut self, lam: &LambdaVal, args: Vec<Value>, span: &Span) -> Result<Value, Value> {
        let ci = match lam.vm_chunk {
            Some(i) => i,
            None => {
                return Err(self.mk_err(
                    codes::NOT_IMPLEMENTED,
                    "该闭包未编译为 VM 字节码（请使用 --vm 运行）".into(),
                    span,
                ))
            }
        };
        let chunk = &self.chunks[ci];
        let ncap = chunk.captured_regs.len();
        let expected = chunk.params.len();
        if args.len() != expected {
            return Err(Value::Error(ErrorObj {
                code: codes::ARG_COUNT,
                message: format!("lambda 期望 {} 个参数，收到 {}", expected, args.len()),
                file: self.file.clone(),
                line: span.line,
                col: span.col,
                len: span.len.max(1),
                context: String::new(),
                help: None,
            }));
        }
        let nregs = chunk.nregs;
        let mut regs = vec![Value::Null; nregs];
        for (name, r) in &chunk.captured_regs {
            if let Some(v) = lam.captured.get(name) {
                regs[*r] = v.clone();
            }
        }
        for (i, a) in args.iter().enumerate() {
            let r = ncap + i;
            if r < nregs {
                regs[r] = a.clone();
            }
        }
        self.frames.push(Frame { chunk: ci, pc: 0, regs });
        let res = self.exec();
        self.frames.pop();
        res
    }

    /// async 函数调用：在后台线程用克隆的字节码运行闭包 chunk，立即返回 future。
    fn spawn_async(&self, callee: &str, args: Vec<Value>) -> Result<Value, Value> {
        let ci = match self.func_map.get(callee) {
            Some(&i) => i,
            None => {
                return Err(self.mk_err(
                    codes::UNDEFINED,
                    format!("undefined function `{}`", callee),
                    &Span { line: 0, col: 0, len: 0 },
                ))
            }
        };
        let chunks = self.chunks.clone();
        let func_map = self.func_map.clone();
        let struct_defs = self.struct_defs.clone();
        let enum_defs = self.enum_defs.clone();
        let aliases = self.aliases.clone();
        let async_fns = self.async_fns.clone();
        let file = self.file.clone();
        let src = self.src.clone();
        let future = FutureVal::new();
        let fut = future.clone();
        thread::spawn(move || {
            let mut vm = Vm::new(
                chunks, func_map, struct_defs, enum_defs, aliases, async_fns, &file, &src, false,
            );
            let result = vm.call_chunk(ci, args);
            // 用带 src 的实例方法转换，保留源码上下文（否则 await 抛出的错误会丢 line_text）。
            let result = result.map_err(|v| vm.value_to_zerror(&v));
            fut.complete(result);
        });
        Ok(Value::Future(future))
    }

    /// go 多线程：后台线程运行函数体（fire-and-forget），错误仅打印不影响主线程。
    fn spawn_go(&self, callee: &str, args: Vec<Value>) {
        let ci = match self.func_map.get(callee) {
            Some(&i) => i,
            None => {
                // 与解释器一致：go 目标不存在时按渲染后的错误打印（不影响主线程）。
                eprintln!(
                    "{}",
                    ZError::new(
                        codes::UNDEFINED,
                        format!("undefined function `{}`", callee),
                        &self.file,
                        &self.src,
                        0,
                        0,
                        0,
                        Some("check the spelling".to_string()),
                    )
                );
                return;
            }
        };
        let chunks = self.chunks.clone();
        let func_map = self.func_map.clone();
        let struct_defs = self.struct_defs.clone();
        let enum_defs = self.enum_defs.clone();
        let aliases = self.aliases.clone();
        let async_fns = self.async_fns.clone();
        let file = self.file.clone();
        let src = self.src.clone();
        thread::spawn(move || {
            let mut vm = Vm::new(
                chunks, func_map, struct_defs, enum_defs, aliases, async_fns, &file, &src, false,
            );
            if let Err(e) = vm.call_chunk(ci, args) {
                let ze = vm.value_to_zerror(&e);
                eprintln!("{}", ze);
            }
        });
    }
}

// ───────────────────────────── 值运算辅助 ─────────────────────────────

fn const_to_value(c: Const) -> Value {
    match c {
        Const::Int(v) => Value::Int(v),
        Const::Float(v) => Value::Float(v),
        Const::Bool(v) => Value::Bool(v),
        Const::Str(v) => Value::Str(v),
        Const::Char(v) => Value::Char(v),
        Const::Null => Value::Null,
    }
}

fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Null => false,
        Value::Int(x) => *x != 0,
        Value::Float(x) => *x != 0.0,
        Value::Str(s) => !s.is_empty(),
        _ => true,
    }
}

/// 根据错误码与消息正文推导 help 文本，与解释器 runtime_err 里各调用点的 help 对齐，
/// 使 VM 错误渲染与解释器逐行一致（零回归目标）。
fn vm_help_for(code: &'static str, msg: &str) -> Option<String> {
    let m = |s: &str| msg.contains(s);
    let h = match code {
        codes::DIV_ZERO => "check the divisor before dividing".to_string(),
        codes::INTEGER_OVERFLOW => "the result does not fit in a 64-bit signed integer".to_string(),
        codes::TYPE_MISMATCH => {
            if m("cannot apply `") {
                "Hone has no implicit type conversion".to_string()
            } else if m("cannot compare `") {
                "comparison operators work on `int` / `float` / `char`".to_string()
            } else if m("`for in` requires") {
                "iterate a list with `for x in list` or a dict with `for k, v in dict`".to_string()
            } else if m("comprehension requires") {
                "comprehend over a list with `for x in list` or a dict with `for k, v in dict`"
                    .to_string()
            } else if m("list index must be an int") {
                "use an integer expression as the index, e.g. `a[0]`, `a[i]`".to_string()
            } else if m("string length") {
                "check the index against `len(str)`".to_string()
            } else if m("out of bounds") {
                "check the index against `len(list)`".to_string()
            } else if m("destructuring requires") {
                "destructure a list with `a, b = [..]` or a dict with `{a, b} = dict`".to_string()
            } else if m("field access") {
                "only error values (catch variables) support field access".to_string()
            } else {
                return None;
            }
        }
        codes::STR_TO_INT => "`to_int` on a string requires digits only (optional leading `-`)".to_string(),
        codes::STR_TO_FLOAT => "`to_float` on a string requires digits only (optional decimal point)".to_string(),
        codes::NOT_FOUND => "check the path and module cache (~/.hone/cache/)".to_string(),
        codes::UNDEFINED if m("undefined function") => "check the spelling".to_string(),
        codes::UNDEFINED if m("dict has no key") => {
            "check the key name, or the dict value being destructured".to_string()
        }
        codes::UNDEFINED if m("unknown field") => {
            "check the field name, or the struct definition".to_string()
        }
        codes::UNDEFINED if m("unknown error field") => {
            "error fields: code, message, file, line, col, context".to_string()
        }
        _ => return None,
    };
    Some(h)
}

fn normalize_throw(v: Value) -> Value {
    match v {
        Value::Error(_) => v,
        Value::Str(s) => Value::Error(ErrorObj {
            code: codes::THROW,
            message: s,
            file: String::new(),
            line: 0,
            col: 0,
            len: 0,
            context: String::new(),
            help: None,
        }),
        other => Value::Error(ErrorObj {
            code: codes::THROW,
            message: value_to_str(&other),
            file: String::new(),
            line: 0,
            col: 0,
            len: 0,
            context: String::new(),
            help: None,
        }),
    }
}

fn value_to_str(v: &Value) -> String {
    // COW 透明：cow 容器按内层值显示
    match v.as_plain() {
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Str(s) => s.clone(),
        Value::Char(c) => c.to_string(),
        // 字节值 / 字节序列：以十六进制表示（与解释器 display 风格一致）
        Value::Byte(b) => format!("0x{:02x}", b),
        Value::Bytes(items) => {
            let s: String = items.iter().map(|b| format!("{:02x}", b)).collect();
            format!("b\"{}\"", s)
        }
        // type 实例：Type(field: value, ...)
        Value::TypeInst(inst) => {
            let parts: Vec<String> = inst
                .fields
                .read()
                .unwrap()
                .iter()
                .map(|(k, v)| format!("{}: {}", k, value_to_str(v)))
                .collect();
            format!("{}({})", inst.ty, parts.join(", "))
        }
        Value::Null => "null".to_string(),
        Value::List(l) => {
            let parts: Vec<String> = l.iter().map(value_to_str).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Dict(d) => {
            // 隐藏 `__struct__` 标记键不显示（struct 实例内部携带）
            let parts: Vec<String> = d
                .iter()
                .filter(|(k, _)| !Value::is_hidden_struct_key(k))
                .map(|(k, v)| format!("{}: {}", k, value_to_str(v)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        Value::Error(e) => format!("error[{}]: {}", e.code, e.message),
        Value::Ptr(p) => format!("ptr({})", p),
        Value::Lambda(_) => "<lambda>".to_string(),
        Value::Enum(e) => {
            if e.payload.is_empty() {
                format!("{}.{}", e.ty, e.variant)
            } else {
                let parts: Vec<String> = e.payload.iter().map(value_to_str).collect();
                format!("{}.{}({})", e.ty, e.variant, parts.join(", "))
            }
        }
        Value::Future(_) => "<future>".to_string(),
        Value::Cow(_) => unreachable!("as_plain strips COW"),
    }
}

fn v_neg(v: Value, span: &Span, vm: &Vm) -> Result<Value, Value> {
    match v {
        Value::Int(x) => x
            .checked_neg()
            .map(Value::Int)
            .ok_or_else(|| vm.mk_err(codes::INTEGER_OVERFLOW, "integer overflow".into(), span)),
        Value::Float(x) => Ok(Value::Float(-x)),
        other => Err(vm.mk_err(
            codes::TYPE_MISMATCH,
            format!("unary `-` requires a number, got `{}`", other.type_name()),
            span,
        )),
    }
}

fn v_not(v: Value, span: &Span, vm: &Vm) -> Result<Value, Value> {
    match v {
        Value::Bool(b) => Ok(Value::Bool(!b)),
        other => Err(vm.mk_err(
            codes::TYPE_MISMATCH,
            format!("`!` requires a `bool`, got `{}`", other.type_name()),
            span,
        )),
    }
}

fn v_add(a: Value, b: Value, span: &Span, vm: &Vm, sym: &str, tn_a: &str, tn_b: &str) -> Result<Value, Value> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x
            .checked_add(y)
            .map(Value::Int)
            .ok_or_else(|| vm.mk_err(codes::INTEGER_OVERFLOW, "integer overflow".into(), span)),
        (Value::Float(x), Value::Float(y)) => Ok(Value::Float(x + y)),
        (Value::Str(x), Value::Str(y)) => Ok(Value::Str(x + &y)),
        _ => Err(vm.mk_err(
            codes::TYPE_MISMATCH,
            format!("cannot apply `{}` to `{}` and `{}`", sym, tn_a, tn_b),
            span,
        )),
    }
}

fn v_sub(a: Value, b: Value, span: &Span, vm: &Vm, sym: &str, tn_a: &str, tn_b: &str) -> Result<Value, Value> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x
            .checked_sub(y)
            .map(Value::Int)
            .ok_or_else(|| vm.mk_err(codes::INTEGER_OVERFLOW, "integer overflow".into(), span)),
        (Value::Float(x), Value::Float(y)) => Ok(Value::Float(x - y)),
        (Value::Int(x), Value::Float(y)) => Ok(Value::Float(x as f64 - y)),
        (Value::Float(x), Value::Int(y)) => Ok(Value::Float(x - y as f64)),
        _ => Err(vm.mk_err(
            codes::TYPE_MISMATCH,
            format!("cannot apply `{}` to `{}` and `{}`", sym, tn_a, tn_b),
            span,
        )),
    }
}

fn v_mul(a: Value, b: Value, span: &Span, vm: &Vm, sym: &str, tn_a: &str, tn_b: &str) -> Result<Value, Value> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x
            .checked_mul(y)
            .map(Value::Int)
            .ok_or_else(|| vm.mk_err(codes::INTEGER_OVERFLOW, "integer overflow".into(), span)),
        (Value::Float(x), Value::Float(y)) => Ok(Value::Float(x * y)),
        (Value::Int(x), Value::Float(y)) => Ok(Value::Float(x as f64 * y)),
        (Value::Float(x), Value::Int(y)) => Ok(Value::Float(x * y as f64)),
        _ => Err(vm.mk_err(
            codes::TYPE_MISMATCH,
            format!("cannot apply `{}` to `{}` and `{}`", sym, tn_a, tn_b),
            span,
        )),
    }
}

fn v_div(a: Value, b: Value, span: &Span, vm: &Vm, sym: &str, tn_a: &str, tn_b: &str) -> Result<Value, Value> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => {
            if y == 0 {
                return Err(vm.mk_err(codes::DIV_ZERO, "division by zero".into(), span));
            }
            x.checked_div(y)
                .map(Value::Int)
                .ok_or_else(|| vm.mk_err(codes::INTEGER_OVERFLOW, "integer overflow".into(), span))
        }
        (Value::Float(x), Value::Float(y)) => {
            if y == 0.0 {
                return Err(vm.mk_err(codes::DIV_ZERO, "division by zero".into(), span));
            }
            Ok(Value::Float(x / y))
        }
        (Value::Int(x), Value::Float(y)) => {
            if y == 0.0 {
                return Err(vm.mk_err(codes::DIV_ZERO, "division by zero".into(), span));
            }
            Ok(Value::Float(x as f64 / y))
        }
        (Value::Float(x), Value::Int(y)) => {
            if y == 0 {
                return Err(vm.mk_err(codes::DIV_ZERO, "division by zero".into(), span));
            }
            Ok(Value::Float(x / y as f64))
        }
        _ => Err(vm.mk_err(
            codes::TYPE_MISMATCH,
            format!("cannot apply `{}` to `{}` and `{}`", sym, tn_a, tn_b),
            span,
        )),
    }
}

fn v_mod(a: Value, b: Value, span: &Span, vm: &Vm, sym: &str, tn_a: &str, tn_b: &str) -> Result<Value, Value> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => {
            if y == 0 {
                return Err(vm.mk_err(codes::DIV_ZERO, "division by zero".into(), span));
            }
            x.checked_rem(y)
                .map(Value::Int)
                .ok_or_else(|| vm.mk_err(codes::INTEGER_OVERFLOW, "integer overflow".into(), span))
        }
        (Value::Float(x), Value::Float(y)) => {
            if y == 0.0 {
                return Err(vm.mk_err(codes::DIV_ZERO, "division by zero".into(), span));
            }
            Ok(Value::Float(x % y))
        }
        _ => Err(vm.mk_err(
            codes::TYPE_MISMATCH,
            format!("cannot apply `{}` to `{}` and `{}`", sym, tn_a, tn_b),
            span,
        )),
    }
}

/// 提取错误码（用于判断是否可回退运算符重载）。
fn err_code(v: &Value) -> Option<&'static str> {
    match v {
        Value::Error(e) => Some(e.code),
        _ => None,
    }
}

/// 内建算术语义覆盖的操作数组合（数字-数字 或 str-str）。
/// 与解释器 `eval_binary` 的 `native` 判定一致：此类组合不回退 `__add` 等。
fn is_num_or_str_pair(a: &Value, b: &Value) -> bool {
    matches!(
        (a, b),
        (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_))
            | (Value::Str(_), Value::Str(_))
    )
}

/// 内建「大小比较」语义覆盖的操作数组合（int-int / float-float）。
/// 与解释器 `values_cmp` 分支的 `native` 判定一致：此类组合不回退 `__lt` 等。
fn is_num_pair(a: &Value, b: &Value) -> bool {
    matches!(
        (a, b),
        (Value::Int(_), Value::Int(_)) | (Value::Float(_), Value::Float(_))
    )
}

/// 相等判定：镜像解释器 `values_eq`。类型不匹配返回错误，由调用方决定是否回退 `__eq`/`__ne`。
fn v_eq(a: &Value, b: &Value, span: &Span, vm: &Vm) -> Result<bool, Value> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Ok(x == y),
        (Value::Float(x), Value::Float(y)) => Ok(x == y),
        (Value::Bool(x), Value::Bool(y)) => Ok(x == y),
        (Value::Str(x), Value::Str(y)) => Ok(x == y),
        (Value::Char(x), Value::Char(y)) => Ok(x == y),
        (Value::List(x), Value::List(y)) => Ok(x == y),
        (Value::Dict(x), Value::Dict(y)) => Ok(x == y),
        (Value::Ptr(x), Value::Ptr(y)) => Ok(x == y),
        // ptr 与整数比较：`p == 0` 判断 NULL，`p == n` 比较句柄数值
        (Value::Ptr(x), Value::Int(y)) => Ok(*x as i64 == *y),
        (Value::Int(x), Value::Ptr(y)) => Ok(*x == *y as i64),
        (Value::Null, Value::Null) => Ok(true),
        // 枚举值：类型 + 变体 + 载荷全部相等才相等
        (Value::Enum(x), Value::Enum(y)) => {
            Ok(x.ty == y.ty && x.variant == y.variant && x.payload == y.payload)
        }
        _ => Err(vm.mk_err_with(
            codes::TYPE_MISMATCH,
            format!("cannot compare `{}` with `{}`", a.type_name(), b.type_name()),
            span,
            Some("Hone has no implicit type conversion".to_string()),
        )),
    }
}

fn v_cmp(
    a: Value,
    b: Value,
    span: &Span,
    vm: &Vm,
    pred: impl Fn(i32) -> bool,
) -> Result<Value, Value> {
    let tn_a = a.type_name();
    let tn_b = b.type_name();
    let ord = match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(&y),
        (Value::Float(x), Value::Float(y)) => match x.partial_cmp(&y) {
            Some(c) => c,
            None => {
                return Err(vm.mk_err(
                    codes::TYPE_MISMATCH,
                    "cannot compare NaN values".into(),
                    span,
                ))
            }
        },
        // 字符按 Unicode 码点比较
        (Value::Char(x), Value::Char(y)) => x.cmp(&y),
        _ => {
            return Err(vm.mk_err(
                codes::TYPE_MISMATCH,
                format!("cannot compare `{}` with `{}`", tn_a, tn_b),
                span,
            ))
        }
    };
    Ok(Value::Bool(pred(match ord {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    })))
}

fn v_index(obj: &Value, key: &Value, span: &Span, vm: &Vm) -> Result<Value, Value> {
    match obj {
        Value::List(l) => {
            if let Value::Int(i) = key {
                let idx = *i;
                if idx < 0 || idx as usize >= l.len() {
                    return Err(vm.mk_err(
                        codes::TYPE_MISMATCH,
                        format!("index {} out of bounds (list length {})", idx, l.len()),
                        span,
                    ));
                }
                Ok(l[idx as usize].clone())
            } else {
                Err(vm.mk_err(
                    codes::TYPE_MISMATCH,
                    format!("list index must be an int, got `{}`", value_to_str(key)),
                    span,
                ))
            }
        }
        Value::Str(s) => {
            if let Value::Int(i) = key {
                let idx = *i;
                let chars: Vec<char> = s.chars().collect();
                if idx < 0 || idx as usize >= chars.len() {
                    return Err(vm.mk_err(
                        codes::TYPE_MISMATCH,
                        format!("index {} out of bounds (string length {})", idx, chars.len()),
                        span,
                    ));
                }
                Ok(Value::Str(chars[idx as usize].to_string()))
            } else {
                Err(vm.mk_err(
                    codes::TYPE_MISMATCH,
                    format!("list index must be an int, got `{}`", value_to_str(key)),
                    span,
                ))
            }
        }
        Value::Dict(d) => {
            let k = match key {
                Value::Str(s) => s.clone(),
                Value::Int(i) => i.to_string(),
                other => {
                    return Err(vm.mk_err(
                        codes::TYPE_MISMATCH,
                        format!("字典键类型不支持: {}", value_to_str(other)),
                        span,
                    ))
                }
            };
            for (kk, vv) in d {
                if *kk == k {
                    return Ok(vv.clone());
                }
            }
            Ok(Value::Null)
        }
        other => Err(vm.mk_err(
            codes::TYPE_MISMATCH,
            format!("cannot index a value of type `{}`", value_to_str(other)),
            span,
        )),
    }
}

fn v_index_set(dst: &mut Value, key: &Value, val: Value, span: &Span, vm: &Vm) -> Result<(), Value> {
    match dst {
        Value::List(l) => {
            if let Value::Int(i) = key {
                let idx = *i;
                if idx < 0 || idx as usize >= l.len() {
                    return Err(vm.mk_err(
                        codes::TYPE_MISMATCH,
                        format!("index {} out of bounds (list length {})", idx, l.len()),
                        span,
                    ));
                }
                l[idx as usize] = val;
                Ok(())
            } else {
                Err(vm.mk_err(
                    codes::TYPE_MISMATCH,
                    format!("list index must be an int, got `{}`", value_to_str(key)),
                    span,
                ))
            }
        }
        Value::Dict(d) => {
            let k = match key {
                Value::Str(s) => s.clone(),
                Value::Int(i) => i.to_string(),
                other => {
                    return Err(vm.mk_err(
                        codes::TYPE_MISMATCH,
                        format!("字典键类型不支持: {}", value_to_str(other)),
                        span,
                    ))
                }
            };
            for item in d.iter_mut() {
                if item.0 == k {
                    item.1 = val;
                    return Ok(());
                }
            }
            d.push((k, val));
            Ok(())
        }
        other => Err(vm.mk_err(
            codes::TYPE_MISMATCH,
            format!("cannot index a value of type `{}`", value_to_str(other)),
            span,
        )),
    }
}

/// 解构取值：列表按位取（越界报错），字典按键取（缺键报错），其他类型报错。
/// 与解释器 exec_stmt 的 DestructAssign 语义一致。
fn v_destruct_get(obj: &Value, key: &Value, span: &Span, vm: &Vm) -> Result<Value, Value> {
    match obj {
        Value::List(l) => {
            if let Value::Int(i) = key {
                let idx = *i;
                if idx < 0 || (idx as usize) >= l.len() {
                    return Err(vm.mk_err(
                        codes::TYPE_MISMATCH,
                        format!(
                            "destructuring a list of {} element(s) into more variables",
                            l.len()
                        ),
                        span,
                    ));
                }
                Ok(l[idx as usize].clone())
            } else {
                Err(vm.mk_err(
                    codes::TYPE_MISMATCH,
                    format!("list index must be an int, got `{}`", value_to_str(key)),
                    span,
                ))
            }
        }
        Value::Dict(d) => {
            let k = match key {
                Value::Str(s) => s.clone(),
                Value::Int(i) => i.to_string(),
                other => {
                    return Err(vm.mk_err(
                        codes::TYPE_MISMATCH,
                        format!("字典键类型不支持: {}", value_to_str(other)),
                        span,
                    ))
                }
            };
            for (kk, vv) in d {
                if *kk == k {
                    return Ok(vv.clone());
                }
            }
            Err(vm.mk_err(
                codes::UNDEFINED,
                format!("dict has no key `{}` for destructuring", k),
                span,
            ))
        }
        other => Err(vm.mk_err(
            codes::TYPE_MISMATCH,
            format!(
                "destructuring requires a list or dict value, got `{}`",
                value_to_str(other)
            ),
            span,
        )),
    }
}

fn v_field(obj: &Value, field: &str, span: &Span, vm: &Vm) -> Result<Value, Value> {
    match obj {
        Value::Dict(d) => match d.iter().find(|(k, _)| !Value::is_hidden_struct_key(k) && k == field) {
            Some((_, v)) => Ok(v.clone()),
            None => Err(vm.mk_err(
                codes::UNDEFINED,
                format!(
                    "unknown field `{}` (dict/struct has {})",
                    field,
                    d.iter().filter(|(k, _)| !Value::is_hidden_struct_key(k)).map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(", ")
                ),
                span,
            )),
        },
        Value::Error(e) => match field {
            "message" => Ok(Value::Str(e.message.clone())),
            "code" => Ok(Value::Str(e.code.to_string())),
            "file" => Ok(Value::Str(e.file.clone())),
            "line" => Ok(Value::Int(e.line as i64)),
            "col" => Ok(Value::Int(e.col as i64)),
            "context" => Ok(Value::Str(e.context.clone())),
            other => Err(vm.mk_err(
                codes::UNDEFINED,
                format!("unknown error field `{}`", other),
                span,
            )),
        },
        other => Err(vm.mk_err(
            codes::TYPE_MISMATCH,
            format!(
                "field access `.{}` requires an `error` value, got `{}`",
                field,
                other.type_name()
            ),
            span,
        )),
    }
}

fn v_len(v: &Value, span: &Span, vm: &Vm) -> Result<Value, Value> {
    match v {
        Value::List(l) => Ok(Value::Int(l.len() as i64)),
        // 隐藏 `__struct__` 标记键不计入长度
        Value::Dict(d) => Ok(Value::Int(
            d.iter().filter(|(k, _)| !Value::is_hidden_struct_key(k)).count() as i64,
        )),
        Value::Str(s) => Ok(Value::Int(s.len() as i64)),
        other => Err(vm.mk_err(
            codes::TYPE_MISMATCH,
            format!("`len` expects a string, list, or dict, got `{}`", value_to_str(other)),
            span,
        )),
    }
}

fn v_keys(v: &Value, span: &Span, vm: &Vm) -> Result<Value, Value> {
    match v {
        Value::Dict(d) => {
            // 隐藏 `__struct__` 标记键不暴露（for-in / keys 共用此路径）
            let keys: Vec<Value> = d
                .iter()
                .filter(|(k, _)| !Value::is_hidden_struct_key(k))
                .map(|(k, _)| Value::Str(k.clone()))
                .collect();
            Ok(Value::List(keys))
        }
        _ => Err(vm.mk_err(
            codes::TYPE_MISMATCH,
            format!("`keys` expects a dict, got `{}`", value_to_str(v)),
            span,
        )),
    }
}

// ───────────────────────────── 模块与对外入口 ─────────────────────────────

/// 已编译模块：VM 执行所需的全部数据。
/// 由编译器产出（`compile_module`），或由文本 IR 经 `assemble()` 反向装配得到。
#[derive(Clone)]
pub struct Module {
    pub chunks: Vec<Chunk>,
    pub func_map: HashMap<String, usize>,
    pub struct_defs: HashMap<String, Vec<String>>,
    pub enum_defs: HashMap<String, Vec<(String, usize)>>,
    pub aliases: Vec<(String, String)>,
    pub async_fns: HashSet<String>,
}

impl Compiler {
    /// 取出编译结果（消费编译器）。
    fn into_module(self) -> Module {
        Module {
            chunks: self.chunks,
            func_map: self.func_map,
            struct_defs: self.struct_defs,
            enum_defs: self.enum_defs,
            aliases: self.aliases,
            async_fns: self.async_fns,
        }
    }
}

/// 编译源码为模块（解析与类型检查由调用方完成）。
pub fn compile_module(program: &Program) -> Result<Module, ZError> {
    let mut c = Compiler::new();
    c.compile_program(program)?;
    Ok(c.into_module())
}

/// 用寄存器式字节码 VM 执行程序（解析与类型检查由调用方完成）。
pub fn run(program: &Program, file: &str, src: &str, debug: bool) -> Result<(), ZError> {
    let m = compile_module(program)?;
    run_module(&m, file, src, debug)
}

/// 用寄存器式字节码 VM 执行已装配模块。
pub fn run_module(m: &Module, file: &str, src: &str, debug: bool) -> Result<(), ZError> {
    let main_idx = *m.func_map.get("main").ok_or_else(|| {
        ZError::plain(
            codes::UNDEFINED,
            "模块缺少入口函数 `main`",
            Some("文本 IR 的 `funcs:` 行必须声明 `main`"),
        )
    })?;
    let mut vm = Vm::new(
        m.chunks.clone(),
        m.func_map.clone(),
        m.struct_defs.clone(),
        m.enum_defs.clone(),
        m.aliases.clone(),
        m.async_fns.clone(),
        file,
        src,
        debug,
    );
    vm.exec_program(main_idx)
}

/// 文本 IR 装配结果：模块 + 自包含的原始文件名与源码（用于错误渲染）。
pub struct IrProgram {
    pub module: Module,
    pub file: String,
    pub src: String,
}

/// 执行文本 IR：反向装配后运行（等价于 `hone run --vm`，但跳过解析/检查/编译阶段）。
/// IR 自包含原始文件名与源码，因此报错位置与上下文与源码运行一致。
pub fn run_ir(text: &str, fallback_file: &str, debug: bool) -> Result<(), ZError> {
    let ir = assemble(text)?;
    let file = if ir.file.is_empty() {
        fallback_file.to_string()
    } else {
        ir.file
    };
    run_module(&ir.module, &file, &ir.src, debug)
}

/// 编译为字节码并序列化为完整文本 IR（自包含：模块头 + 各 chunk + 原始文件名与源码）。
/// 该输出可被 `assemble()` 1:1 还原，`run_ir()` 执行时连报错位置与上下文都一致。
pub fn disassemble_program(program: &Program, file: &str, src: &str) -> Result<String, ZError> {
    let m = compile_module(program)?;
    let mut out = format!("; === Hone IR v1 ===\n; file: {}\n", file);
    out.push_str(&module_head(&m));
    out.push_str(&module_chunks(&m));
    out.push_str(SRC_BEGIN);
    out.push('\n');
    out.push_str(src);
    if !src.is_empty() && !src.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(SRC_END);
    out.push('\n');
    Ok(out)
}

/// 反汇编「仅指令序列」（不含模块头、不含源码块）。
///
/// 说明：`hone run --disasm` 走的是 [`disassemble_program`]（自包含、可经 `assemble()`
/// 往返）。本函数与 [`disassemble_module`] 保留为「只导出 chunk 部分」的辅助接口，
/// 供 IR 调试与文档中「分块查看字节码」的场景使用，当前主 CLI 路径未调用。
#[allow(dead_code)]
pub fn disassemble(chunks: &[Chunk]) -> String {
    let mut out = String::new();
    for chunk in chunks {
        out.push_str(&chunk_text(chunk));
    }
    out
}

/// 反汇编完整模块（模块头 + 各 chunk，源码块为空）。
///
/// 说明：与 [`disassemble_program`] 的区别是不写入真实源码，故产物不能 1:1 还原报错
/// 上下文；主 CLI 路径使用 `disassemble_program`。保留供内部调试与文档引用。
#[allow(dead_code)]
pub fn disassemble_module(m: &Module) -> String {
    let mut out = String::from("; === Hone IR v1 ===\n; file: \n");
    out.push_str(&module_head(m));
    out.push_str(&module_chunks(m));
    out.push_str(SRC_BEGIN);
    out.push('\n');
    out.push_str(SRC_END);
    out.push('\n');
    out
}

/// 源码块起始/结束标记。
const SRC_BEGIN: &str = "; @src-begin";
const SRC_END: &str = "; @src-end";

/// 模块头：structs / enums / aliases / async / funcs。
fn module_head(m: &Module) -> String {
    let mut out = String::new();
    let mut structs: Vec<String> = m
        .struct_defs
        .iter()
        .map(|(k, fs)| format!("{}({})", k, fs.join(",")))
        .collect();
    structs.sort();
    out.push_str(&format!("; structs: {}\n", structs.join(" ")));
    let mut enums: Vec<String> = m
        .enum_defs
        .iter()
        .map(|(k, vs)| {
            let s: Vec<String> = vs.iter().map(|(v, n)| format!("{}:{}", v, n)).collect();
            format!("{}({})", k, s.join(","))
        })
        .collect();
    enums.sort();
    out.push_str(&format!("; enums: {}\n", enums.join(" ")));
    let mut aliases: Vec<String> = m
        .aliases
        .iter()
        .map(|(a, b)| format!("{}->{}", a, b))
        .collect();
    aliases.sort();
    out.push_str(&format!("; aliases: {}\n", aliases.join(" ")));
    let mut asyncs: Vec<String> = m.async_fns.iter().cloned().collect();
    asyncs.sort();
    out.push_str(&format!("; async: {}\n", asyncs.join(" ")));
    let mut funcs: Vec<(usize, String)> =
        m.func_map.iter().map(|(k, v)| (*v, k.clone())).collect();
    funcs.sort();
    let fstr: Vec<String> = funcs.iter().map(|(i, n)| format!("{}:{}", n, i)).collect();
    out.push_str(&format!("; funcs: {}\n\n", fstr.join(" ")));
    out
}

/// 各 chunk 的文本。
fn module_chunks(m: &Module) -> String {
    let mut out = String::new();
    for chunk in &m.chunks {
        out.push_str(&chunk_text(chunk));
    }
    out
}

/// 渲染单个 chunk（头部 + 常量表 + 指令序列）。
fn chunk_text(chunk: &Chunk) -> String {
    let fmt_pairs = |v: &[(String, usize)]| -> String {
        let s: Vec<String> = v.iter().map(|(n, r)| format!("{}@r{}", n, r)).collect();
        s.join(", ")
    };
    let mut locals: Vec<(String, usize)> =
        chunk.locals.iter().map(|(k, v)| (k.clone(), *v)).collect();
    locals.sort_by_key(|(_, r)| *r);
    let mut out = String::new();
    out.push_str(&format!(
        "; chunk {}  (nregs={}, params=[{}], captures=[{}], locals=[{}])\n",
        chunk.name,
        chunk.nregs,
        chunk.params.join(", "),
        fmt_pairs(&chunk.captured_regs),
        fmt_pairs(&locals)
    ));
    for (i, k) in chunk.consts.iter().enumerate() {
        out.push_str(&format!("  .const {} = {}\n", i, const_text(k)));
    }
    // 每条指令的源码位置（行:列:长度），按 pc 顺序。用于 IR 执行时还原报错行号。
    let spans: Vec<String> = chunk
        .spans
        .iter()
        .map(|s| format!("{}:{}:{}", s.line, s.col, s.len))
        .collect();
    out.push_str(&format!("  .spans {}\n", spans.join(" ")));
    out.push('\n');
    for (i, ins) in chunk.code.iter().enumerate() {
        let text = instr_text(ins, &chunk.consts);
        out.push_str(&format!("{:4}  {}\n", i, text));
    }
    out.push('\n');
    out
}

/// 转义字符串内容，使其可单行承载于文本 IR（与 `unescape_str` 互逆）。
fn escape_str_body(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
}

/// 转义单个字符（与 `parse_char` 互逆）。
fn escape_char_body(c: char) -> String {
    match c {
        '\\' => "\\\\".to_string(),
        '\'' => "\\'".to_string(),
        '\n' => "\\n".to_string(),
        '\t' => "\\t".to_string(),
        '\r' => "\\r".to_string(),
        other => other.to_string(),
    }
}

fn const_text(c: &Const) -> String {
    match c {
        Const::Int(v) => v.to_string(),
        // 用 Debug 格式（`{:?}`）保证有限浮点始终带小数点（1.0 而非 1），
        // 否则文本 IR 无法区分 Float(1.0) 与 Int(1)，反向装配会失真。
        Const::Float(v) => format!("{:?}", v),
        Const::Bool(v) => v.to_string(),
        Const::Str(v) => format!("\"{}\"", escape_str_body(v)),
        Const::Char(v) => format!("'{}'", escape_char_body(*v)),
        Const::Null => "null".to_string(),
    }
}

fn instr_text(ins: &Instr, consts: &[Const]) -> String {
    match ins {
        Instr::LoadK(r, k) => format!("LOADK   r{}  {}", r, const_text(&consts[*k])),
        Instr::LoadNull(r) => format!("LOADNULL r{}", r),
        Instr::LoadBool(r, v) => format!("LOADBOOL r{}  {}", r, v),
        Instr::Move(d, s) => format!("MOVE    r{} r{}", d, s),
        Instr::Neg(d, s) => format!("NEG     r{} r{}", d, s),
        Instr::Not(d, s) => format!("NOT     r{} r{}", d, s),
        Instr::Add(d, a, b) => format!("ADD     r{} r{} r{}", d, a, b),
        Instr::Sub(d, a, b) => format!("SUB     r{} r{} r{}", d, a, b),
        Instr::Mul(d, a, b) => format!("MUL     r{} r{} r{}", d, a, b),
        Instr::Div(d, a, b) => format!("DIV     r{} r{} r{}", d, a, b),
        Instr::Mod(d, a, b) => format!("MOD     r{} r{} r{}", d, a, b),
        Instr::Eq(d, a, b) => format!("EQ      r{} r{} r{}", d, a, b),
        Instr::Ne(d, a, b) => format!("NE      r{} r{} r{}", d, a, b),
        Instr::Lt(d, a, b) => format!("LT      r{} r{} r{}", d, a, b),
        Instr::Le(d, a, b) => format!("LE      r{} r{} r{}", d, a, b),
        Instr::Gt(d, a, b) => format!("GT      r{} r{} r{}", d, a, b),
        Instr::Ge(d, a, b) => format!("GE      r{} r{} r{}", d, a, b),
        Instr::IsNull(d, s) => format!("ISNULL  r{} r{}", d, s),
        Instr::IsDict(d, s) => format!("ISDICT  r{} r{}", d, s),
        Instr::IterCheck(s, is_comp) => {
            format!("ITERCHK r{} {}", s, if *is_comp { "comp" } else { "forin" })
        }
        Instr::Index(d, o, k) => format!("INDEX   r{} r{} r{}", d, o, k),
        Instr::IndexSet(o, k, v) => format!("INDEXSET r{} r{} r{}", o, k, v),
        Instr::DestructGet(d, o, k) => format!("DESTRUCTGET r{} r{} r{}", d, o, k),
        Instr::Field(d, o, f) => format!("FIELD   r{} r{} {}", d, o, f),
        Instr::Len(d, s) => format!("LEN     r{} r{}", d, s),
        Instr::Keys(d, s) => format!("KEYS    r{} r{}", d, s),
        Instr::NewList(d, b, n) => format!("NEWLIST r{} [{}..+{}]", d, b, n),
        Instr::NewDict(d, b, n) => format!("NEWDICT r{} [{}..+{}*2]", d, b, n),
        Instr::EnumElem(d, s, i) => format!("ENUMELEM r{} r{} {}", d, s, i),
        Instr::IsEnumVariant(d, s, e, v) => format!("ISENUM  r{} r{} {}.{}", d, s, e, v),
        Instr::NewEnum(d, e, v, b, n) => format!("NEWENUM r{} {}.{} [{}..+{}]", d, e, v, b, n),
        Instr::Call(r, f, b, n) => format!("CALL    r{} {} [{}..+{}]", r, f, b, n),
        Instr::MakeLambda(r, ci, reads) => {
            let caps: Vec<String> = reads.iter().map(|(n, rr)| format!("{}@r{}", n, rr)).collect();
            format!("MAKELAMBDA r{} #{} [{}]", r, ci, caps.join(", "))
        }
        Instr::Await(d, fr) => format!("AWAIT   r{} r{}", d, fr),
        Instr::GoCall(f, b, n) => format!("GOCALL  {} [{}..+{}]", f, b, n),
        Instr::Ret(r) => format!("RET     r{}", r),
        Instr::RetNull => "RETNULL".to_string(),
        Instr::Jmp(t) => format!("JMP     ->{}", t),
        Instr::JmpIfFalse(r, t) => format!("JMPF    r{} ->{}", r, t),
        Instr::JmpIfTrue(r, t) => format!("JMPT    r{} ->{}", r, t),
        Instr::Label(l) => format!("{}:", l),
        Instr::TryBegin(h, c) => format!("TRYBEGIN ->{} catch r{}", h, c),
        Instr::TryPop => "TRYPOP".to_string(),
        Instr::Throw(r) => format!("THROW   r{}", r),
        Instr::ThrowStr(s) => format!("THROWSTR \"{}\"", s),
        Instr::DebugPrint(r) => format!("DEBUGPRINT r{}", r),
        Instr::Breakpoint => "BREAKPOINT".to_string(),
        Instr::Nop => "NOP".to_string(),
    }
}

// ───────────────────────────── 文本 IR 反向装配 ─────────────────────────────

/// 文本 IR 反向装配：把 `disassemble_module` 的产物还原为 `Module`。
///
/// 覆盖完整 Hone IR v1 格式：模块头（structs / enums / aliases / async / funcs）、
/// chunk 头（nregs / params / captures / locals）、常量表与指令序列。
///
/// 注意：文本 IR 不携带源码位置，装配后各 chunk 的 `spans` 均为默认值，
/// 因此从 IR 运行时若发生错误会丢失行号/列号（执行语义与结果不受影响）。
pub fn assemble(text: &str) -> Result<IrProgram, ZError> {
    fn syn(msg: impl std::fmt::Display) -> ZError {
        ZError::plain(
            codes::SYNTAX,
            format!("文本 IR 解析失败：{}", msg),
            Some("请使用 `hone run --disasm <脚本.hn>` 生成文本 IR"),
        )
    }

    let mut chunks: Vec<Chunk> = Vec::new();
    let mut func_map: HashMap<String, usize> = HashMap::new();
    let mut struct_defs: HashMap<String, Vec<String>> = HashMap::new();
    let mut enum_defs: HashMap<String, Vec<(String, usize)>> = HashMap::new();
    let mut aliases: Vec<(String, String)> = Vec::new();
    let mut async_fns: HashSet<String> = HashSet::new();
    let mut cur: Option<Chunk> = None;
    let mut file = String::new();
    let mut in_src = false;
    let mut src_lines: Vec<String> = Vec::new();

    for (lineno, raw) in text.lines().enumerate() {
        // 源码块：逐行原样收集，不做任何解析
        if in_src {
            if raw.trim_end() == SRC_END {
                in_src = false;
            } else {
                src_lines.push(raw.to_string());
            }
            continue;
        }
        let line = raw.trim_end();
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if t == SRC_BEGIN {
            in_src = true;
            continue;
        }
        // chunk 头（必须先于通用注释判断，二者都以 `;` 开头）
        if let Some(rest) = t.strip_prefix("; chunk ") {
            if let Some(mut c) = cur.take() {
                finalize_chunk(&mut c);
                chunks.push(c);
            }
            cur = Some(parse_chunk_header(rest).map_err(syn)?);
            continue;
        }
        // 模块头 / 普通注释
        if let Some(rest) = t.strip_prefix(';') {
            let rest = rest.trim();
            if let Some(v) = rest.strip_prefix("structs:") {
                for tok in v.split_whitespace() {
                    let (name, inner) = split_call(tok).map_err(syn)?;
                    let fields: Vec<String> = if inner.trim().is_empty() {
                        Vec::new()
                    } else {
                        split_top(inner, ',')
                            .into_iter()
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect()
                    };
                    struct_defs.insert(name, fields);
                }
            } else if let Some(v) = rest.strip_prefix("enums:") {
                for tok in v.split_whitespace() {
                    let (name, inner) = split_call(tok).map_err(syn)?;
                    let mut vs = Vec::new();
                    if !inner.trim().is_empty() {
                        for item in split_top(inner, ',') {
                            let item = item.trim();
                            if item.is_empty() {
                                continue;
                            }
                            let (vn, ar) = item
                                .split_once(':')
                                .ok_or_else(|| syn(format!("枚举变体缺少元数 `{}`", item)))?;
                            vs.push((
                                vn.to_string(),
                                ar.trim()
                                    .parse::<usize>()
                                    .map_err(|_| syn("枚举变体元数不是整数"))?,
                            ));
                        }
                    }
                    enum_defs.insert(name, vs);
                }
            } else if let Some(v) = rest.strip_prefix("aliases:") {
                for tok in v.split_whitespace() {
                    if let Some((a, b)) = tok.split_once("->") {
                        aliases.push((a.to_string(), b.to_string()));
                    }
                }
            } else if let Some(v) = rest.strip_prefix("async:") {
                for tok in v.split_whitespace() {
                    async_fns.insert(tok.to_string());
                }
            } else if let Some(v) = rest.strip_prefix("funcs:") {
                for tok in v.split_whitespace() {
                    let (name, idx) = tok
                        .rsplit_once(':')
                        .ok_or_else(|| syn(format!("funcs 项缺少 `:`：`{}`", tok)))?;
                    let idx: usize = idx.trim().parse().map_err(|_| syn("funcs 下标不是整数"))?;
                    func_map.insert(name.to_string(), idx);
                }
            } else if let Some(v) = rest.strip_prefix("file:") {
                file = v.trim().to_string();
            }
            continue;
        }
        // 常量
        if let Some(rest) = t.strip_prefix(".const ") {
            let (idx_s, val) = rest.split_once('=').ok_or_else(|| syn(".const 缺少 `=`"))?;
            let idx: usize = idx_s.trim().parse().map_err(|_| syn(".const 下标不是整数"))?;
            let c = parse_const(val.trim()).map_err(syn)?;
            let Some(ch) = cur.as_mut() else {
                return Err(syn(".const 出现在 chunk 之外"));
            };
            while ch.consts.len() <= idx {
                ch.consts.push(Const::Null);
            }
            ch.consts[idx] = c;
            continue;
        }
        // 源码位置表（行:列:长度，按 pc 顺序）
        if let Some(rest) = t.strip_prefix(".spans") {
            let Some(ch) = cur.as_mut() else {
                return Err(syn(".spans 出现在 chunk 之外"));
            };
            let mut v = Vec::new();
            for item in rest.split_whitespace() {
                let parts: Vec<&str> = item.split(':').collect();
                if parts.len() != 3 {
                    return Err(syn(format!("span 项非法 `{}`（应为 行:列:长度）", item)));
                }
                v.push(Span {
                    line: parts[0].parse().map_err(|_| syn("span 行号非法"))?,
                    col: parts[1].parse().map_err(|_| syn("span 列号非法"))?,
                    len: parts[2].parse().map_err(|_| syn("span 长度非法"))?,
                });
            }
            ch.spans = v;
            continue;
        }
        // 指令行
        let Some(ch) = cur.as_mut() else {
            return Err(syn(format!(
                "第 {} 行出现在任何 chunk 之前：`{}`",
                lineno + 1,
                t
            )));
        };
        let body = strip_pc(line);
        let ins =
            parse_instr(body, &ch.consts).map_err(|m| syn(format!("第 {} 行：{}", lineno + 1, m)))?;
        ch.code.push(ins);
    }
    if let Some(mut c) = cur.take() {
        finalize_chunk(&mut c);
        chunks.push(c);
    }
    if chunks.is_empty() {
        return Err(syn("未发现任何 chunk"));
    }
    Ok(IrProgram {
        module: Module {
            chunks,
            func_map,
            struct_defs,
            enum_defs,
            aliases,
            async_fns,
        },
        file,
        src: src_lines.join("\n"),
    })
}

/// 装配收尾：补齐标签表与 span 表。
fn finalize_chunk(ch: &mut Chunk) {
    for (i, ins) in ch.code.iter().enumerate() {
        if let Instr::Label(l) = ins {
            ch.labels.insert(l.clone(), i);
        }
    }
    // span 表按 pc 对齐；缺失则补默认值（0:0:0），多余则截断。
    ch.spans.resize(ch.code.len(), Span { line: 0, col: 0, len: 0 });
}

/// 拆分 `Name(inner)` → (Name, inner)；无括号时返回 (原串, "")。
fn split_call(s: &str) -> Result<(String, &str), String> {
    match s.find('(') {
        Some(p) => {
            let name = &s[..p];
            let rest = &s[p + 1..];
            let inner = rest
                .strip_suffix(')')
                .ok_or_else(|| format!("`{}` 缺少右括号", s))?;
            Ok((name.to_string(), inner))
        }
        None => Ok((s.to_string(), "")),
    }
}

/// 按 `sep` 拆分，但忽略 `[..]` 内的分隔符。
fn split_top(s: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for ch in s.chars() {
        match ch {
            '[' => {
                depth += 1;
                cur.push(ch);
            }
            ']' => {
                depth -= 1;
                cur.push(ch);
            }
            c if c == sep && depth == 0 => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// 解析 chunk 头：`NAME  (nregs=N, params=[..], captures=[..], locals=[..])`。
fn parse_chunk_header(rest: &str) -> Result<Chunk, String> {
    let rest = rest.trim();
    let name_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let name = rest[..name_end].trim().to_string();
    let meta = rest[name_end..].trim();
    let inner = meta
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .ok_or_else(|| format!("chunk `{}` 元信息缺少括号", name))?;
    let mut nregs = 0usize;
    let mut params: Vec<String> = Vec::new();
    let mut captured: Vec<(String, usize)> = Vec::new();
    let mut locals: Vec<(String, usize)> = Vec::new();
    for field in split_top(inner, ',') {
        let f = field.trim();
        if let Some(v) = f.strip_prefix("nregs=") {
            nregs = v.trim().parse().map_err(|_| "nregs 不是整数".to_string())?;
        } else if let Some(v) = f.strip_prefix("params=[") {
            let v = v.strip_suffix(']').ok_or("params 缺少 ]")?;
            params = split_top(v, ',')
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        } else if let Some(v) = f.strip_prefix("captures=[") {
            let v = v.strip_suffix(']').ok_or("captures 缺少 ]")?;
            captured = parse_pairs(v)?;
        } else if let Some(v) = f.strip_prefix("locals=[") {
            let v = v.strip_suffix(']').ok_or("locals 缺少 ]")?;
            locals = parse_pairs(v)?;
        }
    }
    Ok(Chunk {
        name,
        code: Vec::new(),
        spans: Vec::new(),
        consts: Vec::new(),
        labels: HashMap::new(),
        nregs,
        params,
        captured_regs: captured,
        locals: locals.into_iter().collect(),
    })
}

/// 解析 `name@rN, ...` 形式的寄存器绑定列表。
fn parse_pairs(v: &str) -> Result<Vec<(String, usize)>, String> {
    let mut out = Vec::new();
    for item in split_top(v, ',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (n, r) = item
            .rsplit_once('@')
            .ok_or_else(|| format!("`{}` 缺少 `@`", item))?;
        let r = r
            .strip_prefix('r')
            .ok_or_else(|| format!("`{}` 寄存器应以 `r` 开头", item))?;
        out.push((
            n.to_string(),
            r.parse().map_err(|_| format!("`{}` 寄存器号非法", item))?,
        ));
    }
    Ok(out)
}

/// 解析常量文本（与 `const_text` 互逆）。
fn parse_const(v: &str) -> Result<Const, String> {
    match v {
        "null" => return Ok(Const::Null),
        "true" => return Ok(Const::Bool(true)),
        "false" => return Ok(Const::Bool(false)),
        _ => {}
    }
    if v.starts_with('"') {
        return Ok(Const::Str(unescape_str(v)?));
    }
    if v.starts_with('\'') {
        return Ok(Const::Char(parse_char(v)?));
    }
    if v.contains('.') || v.contains('e') || v.contains('E') || v == "inf" || v == "-inf" || v == "NaN" {
        return v
            .parse::<f64>()
            .map(Const::Float)
            .map_err(|_| format!("浮点常量无法解析 `{}`", v));
    }
    v.parse::<i64>()
        .map(Const::Int)
        .map_err(|_| format!("整数常量无法解析 `{}`", v))
}

/// 反转义字符串常量（与 `const_text` 的转义互逆）。
fn unescape_str(v: &str) -> Result<String, String> {
    let inner = v
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .ok_or("字符串常量缺少引号")?;
    let mut out = String::new();
    let mut it = inner.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(o) => {
                out.push('\\');
                out.push(o);
            }
            None => return Err("字符串常量以反斜杠结尾".into()),
        }
    }
    Ok(out)
}

/// 解析字符常量 `'c'`（含 `'\''` / `'\\'` / `'\n'` / `'\t'` / `'\r'`）。
fn parse_char(v: &str) -> Result<char, String> {
    let inner = v
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .ok_or("字符常量缺少引号")?;
    let cs: Vec<char> = inner.chars().collect();
    match cs.as_slice() {
        [c] => Ok(*c),
        ['\\', e] => match e {
            '\\' => Ok('\\'),
            '\'' => Ok('\''),
            '"' => Ok('"'),
            'n' => Ok('\n'),
            't' => Ok('\t'),
            'r' => Ok('\r'),
            o => Err(format!("字符常量转义非法 `\\{}`", o)),
        },
        _ => Err(format!("字符常量非法 `{}`", v)),
    }
}

/// 去掉行首的 pc 编号（`{:4}` 前缀）。
fn strip_pc(line: &str) -> &str {
    let s = line.trim_start();
    let mut end = 0usize;
    for (i, c) in s.char_indices() {
        if c.is_ascii_digit() {
            end = i + 1;
        } else {
            break;
        }
    }
    s[end..].trim_start()
}

/// 解析一条指令文本（与 `instr_text` 互逆）。
fn parse_instr(body: &str, consts: &[Const]) -> Result<Instr, String> {
    let body = body.trim();
    // 标签行：`name:`
    if let Some(lbl) = body.strip_suffix(':') {
        if !lbl.is_empty() && !lbl.contains(char::is_whitespace) {
            return Ok(Instr::Label(lbl.to_string()));
        }
    }
    let (mnem, rest) = match body.split_once(char::is_whitespace) {
        Some((m, r)) => (m, r.trim()),
        None => (body, ""),
    };
    let toks: Vec<&str> = rest.split_whitespace().collect();
    let reg = |t: &str| -> Result<usize, String> {
        let t = t.trim();
        t.strip_prefix('r')
            .ok_or_else(|| format!("期望寄存器，得到 `{}`", t))?
            .parse()
            .map_err(|_| format!("寄存器号非法 `{}`", t))
    };
    let need = |n: usize| -> Result<(), String> {
        if toks.len() < n {
            Err(format!("`{}` 操作数不足", mnem))
        } else {
            Ok(())
        }
    };
    let ins = match mnem {
        "LOADK" => {
            need(2)?;
            let d = reg(toks[0])?;
            let ktext = rest[toks[0].len()..].trim();
            let idx = consts
                .iter()
                .position(|c| const_text(c) == ktext)
                .ok_or_else(|| format!("常量表中找不到 `{}`", ktext))?;
            Instr::LoadK(d, idx)
        }
        "LOADNULL" => {
            need(1)?;
            Instr::LoadNull(reg(toks[0])?)
        }
        "LOADBOOL" => {
            need(2)?;
            Instr::LoadBool(reg(toks[0])?, toks[1] == "true")
        }
        "MOVE" => {
            need(2)?;
            Instr::Move(reg(toks[0])?, reg(toks[1])?)
        }
        "NEG" => {
            need(2)?;
            Instr::Neg(reg(toks[0])?, reg(toks[1])?)
        }
        "NOT" => {
            need(2)?;
            Instr::Not(reg(toks[0])?, reg(toks[1])?)
        }
        "ADD" => {
            need(3)?;
            Instr::Add(reg(toks[0])?, reg(toks[1])?, reg(toks[2])?)
        }
        "SUB" => {
            need(3)?;
            Instr::Sub(reg(toks[0])?, reg(toks[1])?, reg(toks[2])?)
        }
        "MUL" => {
            need(3)?;
            Instr::Mul(reg(toks[0])?, reg(toks[1])?, reg(toks[2])?)
        }
        "DIV" => {
            need(3)?;
            Instr::Div(reg(toks[0])?, reg(toks[1])?, reg(toks[2])?)
        }
        "MOD" => {
            need(3)?;
            Instr::Mod(reg(toks[0])?, reg(toks[1])?, reg(toks[2])?)
        }
        "EQ" => {
            need(3)?;
            Instr::Eq(reg(toks[0])?, reg(toks[1])?, reg(toks[2])?)
        }
        "NE" => {
            need(3)?;
            Instr::Ne(reg(toks[0])?, reg(toks[1])?, reg(toks[2])?)
        }
        "LT" => {
            need(3)?;
            Instr::Lt(reg(toks[0])?, reg(toks[1])?, reg(toks[2])?)
        }
        "LE" => {
            need(3)?;
            Instr::Le(reg(toks[0])?, reg(toks[1])?, reg(toks[2])?)
        }
        "GT" => {
            need(3)?;
            Instr::Gt(reg(toks[0])?, reg(toks[1])?, reg(toks[2])?)
        }
        "GE" => {
            need(3)?;
            Instr::Ge(reg(toks[0])?, reg(toks[1])?, reg(toks[2])?)
        }
        "ISNULL" => {
            need(2)?;
            Instr::IsNull(reg(toks[0])?, reg(toks[1])?)
        }
        "ISDICT" => {
            need(2)?;
            Instr::IsDict(reg(toks[0])?, reg(toks[1])?)
        }
        "ITERCHK" => {
            need(2)?;
            Instr::IterCheck(reg(toks[0])?, toks[1] == "comp")
        }
        "INDEX" => {
            need(3)?;
            Instr::Index(reg(toks[0])?, reg(toks[1])?, reg(toks[2])?)
        }
        "INDEXSET" => {
            need(3)?;
            Instr::IndexSet(reg(toks[0])?, reg(toks[1])?, reg(toks[2])?)
        }
        "DESTRUCTGET" => {
            need(3)?;
            Instr::DestructGet(reg(toks[0])?, reg(toks[1])?, reg(toks[2])?)
        }
        "FIELD" => {
            need(3)?;
            Instr::Field(reg(toks[0])?, reg(toks[1])?, toks[2].to_string())
        }
        "LEN" => {
            need(2)?;
            Instr::Len(reg(toks[0])?, reg(toks[1])?)
        }
        "KEYS" => {
            need(2)?;
            Instr::Keys(reg(toks[0])?, reg(toks[1])?)
        }
        "NEWLIST" => {
            need(2)?;
            let (b, n) = parse_range(toks[1])?;
            Instr::NewList(reg(toks[0])?, b, n)
        }
        "NEWDICT" => {
            need(2)?;
            let (b, n) = parse_range(toks[1])?;
            Instr::NewDict(reg(toks[0])?, b, n)
        }
        "ENUMELEM" => {
            need(3)?;
            Instr::EnumElem(
                reg(toks[0])?,
                reg(toks[1])?,
                toks[2].parse().map_err(|_| "ENUMELEM 下标非法".to_string())?,
            )
        }
        "ISENUM" => {
            need(3)?;
            let (e, v) = toks[2]
                .split_once('.')
                .ok_or_else(|| format!("ISENUM 变体名非法 `{}`", toks[2]))?;
            Instr::IsEnumVariant(reg(toks[0])?, reg(toks[1])?, e.to_string(), v.to_string())
        }
        "NEWENUM" => {
            need(3)?;
            let (e, v) = toks[1]
                .split_once('.')
                .ok_or_else(|| format!("NEWENUM 变体名非法 `{}`", toks[1]))?;
            let (b, n) = parse_range(toks[2])?;
            Instr::NewEnum(reg(toks[0])?, e.to_string(), v.to_string(), b, n)
        }
        "CALL" => {
            need(3)?;
            let (b, n) = parse_range(toks[2])?;
            Instr::Call(reg(toks[0])?, toks[1].to_string(), b, n)
        }
        "MAKELAMBDA" => {
            need(3)?;
            let d = reg(toks[0])?;
            let ci: usize = toks[1]
                .strip_prefix('#')
                .ok_or_else(|| format!("MAKELAMBDA chunk 下标非法 `{}`", toks[1]))?
                .parse()
                .map_err(|_| "MAKELAMBDA chunk 下标非法".to_string())?;
            let lb = rest.find('[').ok_or("MAKELAMBDA 缺少捕获列表")?;
            let rb = rest.rfind(']').ok_or("MAKELAMBDA 缺少 ]")?;
            let caps = parse_pairs(&rest[lb + 1..rb])?;
            Instr::MakeLambda(d, ci, caps)
        }
        "AWAIT" => {
            need(2)?;
            Instr::Await(reg(toks[0])?, reg(toks[1])?)
        }
        "GOCALL" => {
            need(2)?;
            let (b, n) = parse_range(toks[1])?;
            Instr::GoCall(toks[0].to_string(), b, n)
        }
        "RET" => {
            need(1)?;
            Instr::Ret(reg(toks[0])?)
        }
        "RETNULL" => Instr::RetNull,
        "JMP" => Instr::Jmp(parse_target(toks.first().copied().unwrap_or(""))?),
        "JMPF" => {
            need(2)?;
            Instr::JmpIfFalse(reg(toks[0])?, parse_target(toks[1])?)
        }
        "JMPT" => {
            need(2)?;
            Instr::JmpIfTrue(reg(toks[0])?, parse_target(toks[1])?)
        }
        "TRYBEGIN" => {
            need(3)?;
            Instr::TryBegin(parse_target(toks[0])?, reg(toks[2])?)
        }
        "TRYPOP" => Instr::TryPop,
        "THROW" => {
            need(1)?;
            Instr::Throw(reg(toks[0])?)
        }
        "THROWSTR" => Instr::ThrowStr(unescape_str(rest)?),
        "DEBUGPRINT" => {
            need(1)?;
            Instr::DebugPrint(reg(toks[0])?)
        }
        "BREAKPOINT" => Instr::Breakpoint,
        "NOP" => Instr::Nop,
        other => return Err(format!("未知指令 `{}`", other)),
    };
    Ok(ins)
}

/// 解析 `[base..+n]` / `[base..+n*2]`。
fn parse_range(t: &str) -> Result<(usize, usize), String> {
    let inner = t
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| format!("非法区间 `{}`", t))?;
    let inner = inner.trim_end_matches("*2");
    let (b, n) = inner
        .split_once("..+")
        .ok_or_else(|| format!("非法区间 `{}`", t))?;
    Ok((
        b.trim().parse().map_err(|_| format!("区间起点非法 `{}`", t))?,
        n.trim().parse().map_err(|_| format!("区间长度非法 `{}`", t))?,
    ))
}

/// 解析 `->N` 跳转目标。
fn parse_target(t: &str) -> Result<usize, String> {
    t.trim()
        .strip_prefix("->")
        .ok_or_else(|| format!("跳转目标非法 `{}`", t))?
        .parse()
        .map_err(|_| format!("跳转目标非法 `{}`", t))
}
