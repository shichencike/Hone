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
use crate::interp::{EnumVal, ErrorObj, FutureVal, LambdaVal, TypeInstVal, Value, STRUCT_MARKER_KEY};
use crate::lexer::Span;

// ───────────────────────────── 常量池 ─────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Const {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    Char(char),
    Byte(u8),
    Bytes(Vec<u8>),
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
    IsBytes(usize, usize),       // dst, src（for-in 双变量：运行期区分 list / bytes 报错文案）
    IterCheck(usize, bool),      // src, is_comp（迭代源必须是 list/dict/bytes，否则报错）
    Index(usize, usize, usize),    // dst, obj, key
    IndexSet(usize, usize, usize), // obj, key, val
    /// 切片 a[i:j]（T1）：dst, src, lo 寄存器（None = 缺省 0）, hi 寄存器（None = 缺省 len）。
    /// 语义镜像解释器 eval_slice：负索引从末尾归一、越界钳制、lo>hi 产空副本。
    Slice(usize, usize, Option<usize>, Option<usize>),
    DestructGet(usize, usize, usize, usize, bool), // dst, obj, key, nvars, is_dict（解构专用：列表越界/字典缺键报错；nvars/is_dict 用于镜像解释器文案）
    /// 字段赋值（T3）：obj, field, val, leaf。leaf=true 为叶字段（readonly 拦截 +
    /// 未知字段/隐藏键报错，镜像解释器 update_chain 叶分支）；leaf=false 为中间层
    /// 写回（隐藏键报 "cannot write to the hidden struct marker field"，其余静默更新）。
    /// TypeInst 写路径由 T6 落地后接手。
    FieldSet(usize, String, usize, bool),
    /// with 进入（T5）：dst, src。运行时镜像解释器 call_enter——TypeInst 调
    /// `类型.__enter__(self)`（func_map「类型名.方法名」派发）；Int 内置句柄
    /// （sqlite/sse/guipro 资源表）原样返回句柄；Ptr 原样；其余报 H002
    /// （文案 / help 与解释器 err_no_context 逐字节一致）。
    WithEnter(usize, usize),
    /// with 退出（T5）：src, propagate。镜像解释器 call_exit——TypeInst 调
    /// `类型.__exit__(self)`；Int 资源派发 with_close（返回值忽略）；Ptr 原样通过。
    /// propagate=true（块正常结束）：__exit__ 自身报错则以该错误为准（不能吞错）；
    /// propagate=false（异常路径重抛前）：__exit__ 报错被丢弃，块的原始错误优先
    /// （镜像解释器 (Err(e), Err(_)) => Err(e) 的优先序）。
    WithExit(usize, bool),
    /// type 实例构造（T6）：dst, 类型名, argbase, nargs。运行时镜像解释器 Expr::New——
    /// 沿继承链收集字段（父链在前，镜像遍历序）按序绑定实参、缺省零值；
    /// 实参过多报「`new {ty}` takes N field(s), got M」（H011）；构造后沿链找
    /// init（先本类后父类）以 self 首参调用，返回非 null 报「must not return a value」。
    TypeNew(usize, String, usize, usize),
    /// type 实例方法调用（T6）：dst, obj, 方法名, argbase, nargs。运行时沿 extends
    /// 链查「类型名.方法名」（子类覆盖父类），self 作首参；缺方法 / 非 TypeInst
    /// 报错文案与解释器逐字节一致。
    CallMethod(usize, usize, String, usize, usize),
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
    /// 条件严格 bool 校验（G2.3 引擎对齐）：非 bool → H001，消息按站点镜像解释器。
    /// 解释器在 if / while / do-while / for / ternary 五处条件站点**运行期**校验 bool，
    /// VM 此前统一走 truthy 判定，enum / struct 等动态类型值不报错也不拦（分叉）。
    /// kind：0=if 1=while 2=do-while 3=for 4=ternary 5=逻辑运算 6=推导式 if 过滤。
    CondCheck(usize, u8),
    /// 值是否恰为 Bool(want)（无类型错误，恒产出 bool）——&&/|| 的短路判定，
    /// 镜像解释器 `if let Value::Bool(false) = l` / `if let Value::Bool(true) = l`。
    IsBoolVal(usize, usize, bool), // dst, src, want
    Label(String),                // 伪指令：标签
    TryBegin(usize, usize),       // handler_pc（占位 0）, catch_reg
    TryPop,                       // 正常路径弹出 try 栈
    Throw(usize),                 // reg 持有被抛值（str→H600，error 原样）
    ThrowStr(String),             // 直接抛字面字符串 → H600
    /// 运行期直接产生一个错误并走错误路由（抛Error 值，由 try_catch / exec 传播）。
    /// 用途：解释器在求值期按容器实际类型分支报错（如「推导式 `for k, v in` 要求 dict，
    /// 得到 list」），VM 需在对应分支发射本指令才能逐字节对齐——此前 VM 缺该路径，
    /// 双变量推导式在列表源上静默退化为单变量，与解释器分叉（G1.6 审计发现）。
    /// 字段：错误码、消息、help（None = 该分支无 help 行）。
    Raise(&'static str, String, Option<String>),
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
    /// 形参默认值位（与 `params` 同序）：`true` = 该形参带默认表达式。
    /// G2.8：调用侧据此放宽参数个数校验（省略的形参由 chunk 序言求默认值填入）。
    pub param_defaults: Vec<bool>,
    /// 实参数寄存器号：chunk 序言读它判断哪些形参被省略；无默认形参时为 `usize::MAX`。
    pub argc_reg: usize,
    /// lambda chunk 专用：被捕获变量 (名字, 寄存器号)，调用时先把这些寄存器填回捕获值。
    pub captured_regs: Vec<(String, usize)>,
    /// 本 chunk 声明的局部变量名 → 寄存器号（编译期收集，运行时供 do_call 做 lambda/前向引用解析）。
    pub locals: HashMap<String, usize>,
}

// ───────────────────────────── 编译器 ─────────────────────────────

struct Compiler {
    chunks: Vec<Chunk>,
    func_map: HashMap<String, usize>,
    struct_defs: HashMap<String, Vec<(String, bool)>>,
    enum_defs: HashMap<String, Vec<(String, usize)>>,
    /// type 定义（T6）：类型名 → (父类型名(空串=根), (字段名, 类型, 是否只读) 列表)
    type_defs: HashMap<String, (String, Vec<(String, crate::ast::TyName, bool)>)>,
    aliases: Vec<(String, String)>,
    async_fns: HashSet<String>,
    code: Vec<Instr>,
    cur_locals: HashMap<String, usize>,
    spans: Vec<Span>,
    consts: Vec<Const>,
    labels: HashMap<String, usize>,
    /// (跳转指令 pc, 目标标签, 跳转点 try 深度)；resolve 时回填目标并按深度差补弹 TryPop。
    pending: Vec<(usize, String, usize)>,
    /// 线性流中活跃的 TryBegin 数（Try 体编译期间 +1，TryPop 后 -1）。
    try_depth: usize,
    /// 各标签所在位置的 try 深度（补弹数 = 跳转点深度 − 目标深度）。
    label_try_depth: HashMap<String, usize>,
    scopes: Vec<(HashMap<String, usize>, usize)>,
    var_next: usize,
    tmp_next: usize,
    max_reg: usize,
    cur_span: Span,
    loop_labels: Vec<(String, String, usize)>, // (continue_label, break_label, 入栈时 with_stack 深度)
    /// T5：活跃 with 语句的资源寄存器栈（内层在后）。break/continue/return 跳出
    /// with 块时，须先按内层优先发射 WithExit（镜像解释器 Flow 穿透时 __exit__ 必调）。
    with_stack: Vec<usize>,
    label_seq: usize,
    syn_seq: usize,
    /// G2.8：当前 chunk 的形参默认值位 / 实参数寄存器（finish_chunk 取走并复位）。
    cur_param_defaults: Vec<bool>,
    cur_argc_reg: usize,
    /// G2.8：正在编译某个默认表达式（值 = 该形参序号）。此模式下变量解析失败不报编译期
    /// 「未定义变量」，而发射运行期 Raise(H002)——镜像解释器：默认值在**仅含形参**的调用
    /// 环境里求值（`fn f(a, b: int = c, c: int = 3)` 解释器运行期报 H002 undefined
    /// variable `c`，而 VM 原先静默取到后置形参的 null）。
    in_default_expr: Option<usize>,
    /// G2.8：当前 chunk 形参名 → 序号（默认表达式里序号 ≥ 当前默认形参的形参视为不可见）。
    cur_param_index: HashMap<String, usize>,
    err: Option<ZError>,
}

impl Compiler {
    fn new() -> Self {
        Compiler {
            chunks: Vec::new(),
            func_map: HashMap::new(),
            struct_defs: HashMap::new(),
            enum_defs: HashMap::new(),
            type_defs: HashMap::new(),
            aliases: Vec::new(),
            async_fns: HashSet::new(),
            code: Vec::new(),
            cur_locals: HashMap::new(),
            spans: Vec::new(),
            consts: Vec::new(),
            labels: HashMap::new(),
            pending: Vec::new(),
            try_depth: 0,
            label_try_depth: HashMap::new(),
            scopes: Vec::new(),
            var_next: 0,
            tmp_next: 0,
            max_reg: 0,
            cur_span: Span { line: 0, col: 0, len: 0 },
            loop_labels: Vec::new(),
            with_stack: Vec::new(),
            label_seq: 0,
            syn_seq: 0,
            cur_param_defaults: Vec::new(),
            cur_argc_reg: usize::MAX,
            in_default_expr: None,
            cur_param_index: HashMap::new(),
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
        self.label_try_depth.insert(name.to_string(), self.try_depth);
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
        self.pending.push((i, lab.to_string(), self.try_depth));
    }
    fn jif(&mut self, r: usize, lab: &str) {
        let i = self.code.len();
        self.emit(Instr::JmpIfFalse(r, 0));
        self.pending.push((i, lab.to_string(), self.try_depth));
    }
    fn jit(&mut self, r: usize, lab: &str) {
        let i = self.code.len();
        self.emit(Instr::JmpIfTrue(r, 0));
        self.pending.push((i, lab.to_string(), self.try_depth));
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
    /// G2.8：当前是否在编译「序号为 n 的形参的默认表达式」，且 `name` 是序号 ≥ n 的形参
    /// （后置形参在解释器的调用环境里尚未绑定 → 运行期 H002）。
    fn hidden_by_default(&self, name: &str) -> bool {
        match self.in_default_expr {
            Some(limit) => self.cur_param_index.get(name).map_or(false, |&i| i >= limit),
            None => false,
        }
    }
    /// 分配一个**常驻**寄存器（推进 `var_next`，不会被 `reset_tmp` 回收）：G2.8 序言的
    /// 实参数槽用它——序言在函数体之前执行，槽必须活到整个 chunk 结束。
    fn alloc_pinned(&mut self) -> usize {
        let r = self.var_next;
        self.var_next += 1;
        if r > self.max_reg {
            self.max_reg = r;
        }
        self.tmp_next = self.var_next;
        r
    }
    fn reset_tmp(&mut self) {
        self.tmp_next = self.var_next;
    }

    /// G2.3 引擎对齐：条件站点发射严格 bool 校验指令（span 显式给定——
    /// 与解释器各站点的错误定位一致：语句条件取 expr_span(cond)，
    /// 逻辑运算 / ternary 取整表达式 span（= 运算符 token））。
    fn cond_check(&mut self, r: usize, kind: u8, span: Span) {
        let saved = self.cur_span.clone();
        self.cur_span = span;
        self.emit(Instr::CondCheck(r, kind));
        self.cur_span = saved;
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
        // pending 按发射顺序（pc 升序）。向前跳出（break/continue/goto 越过线性流
        // 中的 TryPop）需按 try 深度差补弹，否则 stale 条目残留、误捕跳出后逃逸的
        // 异常（死循环 / 跨帧误捕）。插入会前移其后所有指令/标签，分两遍：先按原
        // 坐标算出补弹数，再统一回填（目标与标签同步前移）。
        // 无条件跳转：TryPop 插在跳转之前（必经路径）。条件跳转：仅分支 taken 路径
        // 需要弹（fallthrough 仍在 try 内，不能弹）——走尾部蹦床
        //（Ltmp: TryPop×n; Jmp 目标）。
        let n_pops: Vec<usize> = pending
            .iter()
            .map(|(_, lab, depth)| {
                let target_depth = *self.label_try_depth.get(lab).unwrap_or(&0);
                depth.saturating_sub(target_depth)
            })
            .collect();
        // 原坐标位置 p 的最终前移量 = 所有「插入点位于 p 之前」的补弹之和，
        // 即满足 idx_j < p 的全部 n_j。
        let shift_of = |p: usize| -> usize {
            pending
                .iter()
                .zip(n_pops.iter())
                .filter(|(e, _)| e.0 < p)
                .map(|(_, n)| *n)
                .sum()
        };
        let mut tramps: Vec<(usize, usize)> = Vec::new(); // (补弹数, 目标 pc)
        let mut tramp_tail = self.code.len();
        for ((idx, lab, _depth), n) in pending.iter().zip(n_pops.iter()) {
            let pos = idx + shift_of(*idx);
            let t = *self.labels.get(lab).expect("undefined label in resolve") + shift_of(*self.labels.get(lab).unwrap());
            match &self.code[pos] {
                Instr::TryBegin(_, c) => self.code[pos] = Instr::TryBegin(t, *c),
                Instr::Jmp(_) => {
                    self.code[pos] = Instr::Jmp(t);
                    for _ in 0..*n {
                        self.code.insert(pos, Instr::TryPop);
                        self.spans.insert(pos, self.cur_span.clone());
                    }
                }
                Instr::JmpIfFalse(r, _) if *n > 0 => {
                    self.code[pos] = Instr::JmpIfFalse(*r, tramp_tail);
                    tramps.push((*n, t));
                    tramp_tail += n + 2;
                }
                Instr::JmpIfFalse(r, _) => self.code[pos] = Instr::JmpIfFalse(*r, t),
                Instr::JmpIfTrue(r, _) if *n > 0 => {
                    self.code[pos] = Instr::JmpIfTrue(*r, tramp_tail);
                    tramps.push((*n, t));
                    tramp_tail += n + 2;
                }
                Instr::JmpIfTrue(r, _) => self.code[pos] = Instr::JmpIfTrue(*r, t),
                _ => unreachable!("pending jump on non-jump instr"),
            }
        }
        for (k, (n, t)) in tramps.iter().enumerate() {
            self.code.push(Instr::Label(format!("$tramp${}", k)));
            self.spans.push(self.cur_span.clone());
            for _ in 0..*n {
                self.code.push(Instr::TryPop);
                self.spans.push(self.cur_span.clone());
            }
            self.code.push(Instr::Jmp(*t));
            self.spans.push(self.cur_span.clone());
        }
        for lp in self.labels.values_mut() {
            *lp += shift_of(*lp);
        }
    }

    fn finish_chunk(&mut self, name: &str, params: Vec<String>) -> usize {
        self.cur_param_index.clear();
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
            param_defaults: std::mem::take(&mut self.cur_param_defaults),
            argc_reg: std::mem::replace(&mut self.cur_argc_reg, usize::MAX),
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
        self.try_depth = 0;
        self.label_try_depth.clear();
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
                        .insert(name.clone(), fields.iter().map(|(n, _, ro)| (n.clone(), *ro)).collect());
                }
                // type 实例类：注册成员方法（「类型名.方法名」限定键），供运行时解析
                Stmt::TypeDef { name, base, fields, methods, .. } => {
                    self.type_defs.insert(
                        name.clone(),
                        (base.clone().unwrap_or_default(), fields.clone()),
                    );
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

    /// G2.8：默认形参序言——被调用时按「实参数 ≤ 形参序号」逐个求默认值填入形参寄存器。
///
/// 放在 chunk 体最前面（而非调用方求值）的原因：解释器在**仅含形参**的调用环境里求默认值
/// （`fn f(a, b: a * 2)` 可行，引用全局 / 后置形参则被 checker 静态拒），调用方求值会
/// 额外看见调用点的局部变量，与解释器分叉。序言在 callee 帧内执行，作用域天然正确。
///
/// 实参数经 `argc_reg` 传入（VM 的 call_chunk / call_lambda 与 natgen 生成的 chunk 序言
/// 都写这个寄存器）。返回该寄存器号；无默认形参时返回 `usize::MAX`。
fn emit_default_prologue(&mut self, params: &[Param], preg: &[usize]) -> usize {
    if !params.iter().any(|p| p.default.is_some()) {
        return usize::MAX;
    }
    // 序言专用寄存器：必须是「常驻」槽（reset_tmp 会回收 tmp_next 之后的槽）。
    let argc = self.alloc_pinned();
    self.cur_argc_reg = argc;
    self.cur_param_defaults = params.iter().map(|p| p.default.is_some()).collect();
    for (i, p) in params.iter().enumerate() {
        let Some(def) = &p.default else { continue };
        let lskip = self.new_label("Lnodflt");
        let lnext = self.new_label("Ldfltnext");
        let ki = self.tmp();
        let kidx = self.const_idx(Const::Int(i as i64));
        self.emit(Instr::LoadK(ki, kidx));
        let c = self.tmp();
        // 实参数 ≤ i ⇔ 第 i 个形参未被提供 ⇔ 需要求默认值
        self.emit(Instr::Le(c, argc, ki));
        self.jif(c, &lskip);
        let saved = self.cur_span.clone();
        let saved_mode = self.in_default_expr.replace(i);
        self.cur_span = p.span;
        let rv = self.compile_expr(def);
        self.in_default_expr = saved_mode;
        self.cur_span = saved;
        self.emit(Instr::Move(preg[i], rv));
        self.jmp(&lnext);
        self.emit_label(&lskip);
        self.emit_label(&lnext);
        self.reset_tmp();
    }
    argc
}

fn compile_fn(&mut self, name: &str, params: &[Param], body: &[Stmt]) {
        self.enter();
        let mut pnames = Vec::new();
        let mut preg = Vec::new();
        for p in params {
            preg.push(self.decl(&p.name));
            pnames.push(p.name.clone());
        }
        // G2.8：登记形参序号（默认表达式里后置形参不可见）+ 默认形参序言
        // （必须在函数体之前，此时作用域里只有形参）
        self.cur_param_index = pnames
            .iter()
            .enumerate()
            .map(|(i, n)| (n.clone(), i))
            .collect();
        self.emit_default_prologue(params, &preg);
        for s in body {
            self.compile_stmt(s);
            self.reset_tmp();
        }
        self.emit(Instr::RetNull);
        self.in_default_expr = None;
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
                // 嵌套索引赋值展开为「从深到浅」的 IndexSet 链（镜像 interp set_index_value
                // 递归写回）：m[i][j] = x → t = Index(m, i); IndexSet(t, j, x); IndexSet(m, i, t)。
                // 单层 a[i] = x 时链长为 1，与旧行为等价。
                let rv = self.compile_expr(value);
                let mut chain: Vec<(Expr, Expr)> = Vec::new();
                let mut cur: &Expr = target;
                loop {
                    match cur {
                        Expr::Index { obj, index, .. } => {
                            chain.push(((**obj).clone(), (**index).clone()));
                            cur = obj;
                        }
                        _ => break,
                    }
                }
                if chain.is_empty() {
                    self.fail(codes::NOT_IMPLEMENTED, "VM: 仅支持索引赋值 a[i] = x");
                }
                let mut val_reg = rv;
                for (obj, idx) in &chain {
                    let ro = self.compile_expr(obj);
                    let rk = self.compile_expr(idx);
                    // IndexSet 语义：regs[o] = 修改后值；最深一层 obj 为标识符时直接
                    // 写回其槽（m[i][j]=x 的 m），中间层 obj 为 Index 读取的临时寄存器，
                    // 其结果作为上一层的新容器值（val_reg 链式传递）。
                    self.emit(Instr::IndexSet(ro, rk, val_reg));
                    val_reg = ro;
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
                    // nvars/is_dict 镜像解释器 DestructAssign 的报错文案（列表越界报变量数、
                    // 形态不符区分 list/dict 解构语法）。
                    let is_dict = targets.iter().all(|(_, k)| k.is_some());
                    self.emit(Instr::DestructGet(ri, saved, rk, targets.len(), is_dict));
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
            Stmt::VarDecl { name, init, .. } => {
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
                // G2.3：条件严格 bool 校验（解释器 if 站点的运行期 H001 对齐）。
                self.cond_check(rc, 0, expr_span(cond));
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
                self.cond_check(rc, 1, expr_span(cond));
                self.jif(rc, &lend);
                self.loop_labels.push((lstart.clone(), lend.clone(), self.with_stack.len()));
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
                self.loop_labels.push((leval.clone(), lend.clone(), self.with_stack.len()));
                for s2 in body {
                    self.compile_stmt(s2);
                    self.reset_tmp();
                }
                self.loop_labels.pop();
                self.emit_label(&leval);
                let rc = self.compile_expr(cond);
                self.cond_check(rc, 2, expr_span(cond));
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
                    self.cond_check(rc, 3, expr_span(c));
                    self.jif(rc, &lend);
                }
                self.loop_labels.push((lstep.clone(), lend.clone(), self.with_stack.len()));
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
            Stmt::ForIn { var, var2, iter, body, span } => {
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
                // 双变量 for-in：list / bytes 均被解释器运行期拒绝（"requires a dict, got ..."），
                // 用 IsBytes 运行期区分文案（span 指向 for 语句，镜像解释器）；dict 走下方 dict 分支。
                let lbytes_err = if var2.is_some() {
                    let risbytes = self.tmp();
                    self.emit(Instr::IsBytes(risbytes, riter_src));
                    let l = self.new_label("LbytesErr");
                    self.jit(risbytes, &l);
                    self.cur_span = span.clone();
                    self.emit(Instr::Raise(
                        codes::TYPE_MISMATCH,
                        "`for k, v in` requires a dict, got a list".to_string(),
                        Some("iterate lists with a single variable: `for x in list`".to_string()),
                    ));
                    Some(l)
                } else {
                    None
                };
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
                self.loop_labels.push((linc.clone(), lend2.clone(), self.with_stack.len()));
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
                // 双变量 + bytes：报错分支（标签置于列表路径末尾；循环体编译后 cur_span
                // 已被最后一条语句改写，此处显式指回 for 语句）。
                if let Some(l) = lbytes_err {
                    self.emit_label(&l);
                    self.cur_span = span.clone();
                    self.emit(Instr::Raise(
                        codes::TYPE_MISMATCH,
                        "`for k, v in` requires a dict, got bytes".to_string(),
                        Some("iterate bytes with a single variable: `for b in bs`".to_string()),
                    ));
                    self.jmp(&lend);
                }
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
                self.loop_labels.push((dinc.clone(), dend.clone(), self.with_stack.len()));
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
                // return 穿过 with 块：__exit__ 仍必调（内层优先，镜像解释器 Flow::Return 穿透）
                let exits: Vec<usize> = self.with_stack.iter().rev().copied().collect();
                for rt in exits {
                    self.emit(Instr::WithExit(rt, false));
                }
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
                let top = self.loop_labels.last().cloned();
                match top {
                    Some((_, bl, wdepth)) => {
                        // 跳出目标循环仍在其内的 with 不调 __exit__；更外层的必调（镜像解释器）
                        let exits: Vec<usize> = self.with_stack[wdepth..].to_vec();
                        for &rt in exits.iter().rev() {
                            self.emit(Instr::WithExit(rt, false));
                        }
                        self.jmp(&bl);
                    }
                    None => self.fail(codes::SYNTAX, "VM: break 只能在循环内"),
                }
            }
            Stmt::Continue { .. } => {
                let top = self.loop_labels.last().cloned();
                match top {
                    Some((cl, _, wdepth)) => {
                        let exits: Vec<usize> = self.with_stack[wdepth..].to_vec();
                        for &rt in exits.iter().rev() {
                            self.emit(Instr::WithExit(rt, false));
                        }
                        self.jmp(&cl);
                    }
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
            Stmt::With { target, var, body, .. } => {
                // with 展开（镜像解释器 exec_with）：
                //   rt = eval(target); r = __enter__(rt); [as 绑定]; body;
                //   __exit__(rt); 跳过 handler; handler: __exit__(rt, 吞错); Throw(rc)
                // 错误优先序：块报错优先于 __exit__ 报错（handler 路径 propagate=false）；
                // break/continue/return 穿出时由对应编译点先发射 WithExit。
                let rt = self.compile_expr(target);
                // 资源寄存器挪到「变量区」安全寄存器：body 内 tmp_next 会随语句回收复用，
                // tmp 寄存器会被后续表达式覆盖（例如 if 条件求值），break/return 穿出时
                // WithExit 读到的是被覆盖后的值。镜像 DestructAssign 的处理。
                let saved_vn = self.var_next;
                self.var_next += 1;
                let rt = if rt == saved_vn {
                    rt
                } else {
                    self.emit(Instr::Move(saved_vn, rt));
                    saved_vn
                };
                if saved_vn > self.max_reg {
                    self.max_reg = saved_vn;
                }
                self.tmp_next = self.var_next;
                let renter = self.tmp();
                self.emit(Instr::WithEnter(renter, rt));
                self.with_stack.push(rt);
                if let Some(v) = var {
                    let _ = self.decl(v);
                }
                let lhandler = self.new_label("Lwhandler");
                let lafter = self.new_label("Lwafter");
                let rcatch = self.tmp();
                let i = self.code.len();
                self.emit(Instr::TryBegin(0, rcatch));
                self.pending.push((i, lhandler.clone(), self.try_depth));
                self.try_depth += 1;
                self.enter();
                for s2 in body {
                    self.compile_stmt(s2);
                    self.reset_tmp();
                }
                self.exit();
                self.emit(Instr::TryPop);
                self.try_depth -= 1;
                self.emit(Instr::WithExit(rt, true));
                self.jmp(&lafter);
                self.emit_label(&lhandler);
                // 异常路径：先 __exit__（吞错，块的原始错误优先）再重抛
                self.emit(Instr::WithExit(rt, false));
                self.emit(Instr::Throw(rcatch));
                self.emit_label(&lafter);
                self.with_stack.pop();
            }
            Stmt::FieldAssign { target, value, .. } => {
                // 字段赋值展开为「从深到浅」的 FieldSet 链（镜像 interp set_field_value /
                // update_chain 递归写回）：p.a.b = x → t1 = Field(p, a); t2 = Field(t1, b);
                // FieldSet(t2, b, x, leaf); FieldSet(t1, a, t2, mid); FieldSet(p, a, t1, mid)。
                // 叶层带 readonly / 未知字段 / 隐藏键拦截；中间层写回静默更新（解释器同款）。
                let rv = self.compile_expr(value);
                let mut chain: Vec<String> = Vec::new();
                let mut cur: &Expr = target;
                loop {
                    match cur {
                        Expr::Field { obj, field, .. } => {
                            chain.push(field.clone());
                            cur = obj;
                        }
                        Expr::Ident { name, .. } => {
                            chain.push(name.clone());
                            break;
                        }
                        _ => {
                            self.fail(codes::NOT_IMPLEMENTED, "VM: 仅支持字段赋值 obj.field = x");
                            return;
                        }
                    }
                }
                chain.reverse();
                let base_name = chain[0].clone();
                let base_reg = match self.lookup(&base_name) {
                    Some(r) => r,
                    None => {
                        // 镜像解释器：运行期 H002（`base` is not defined），定位随语句
                        self.emit(Instr::Raise(
                            codes::UNDEFINED,
                            format!("`{}` is not defined", base_name),
                            None,
                        ));
                        let rnull = self.tmp();
                        self.emit(Instr::LoadNull(rnull));
                        return;
                    }
                };
                let mut ro = base_reg;
                let mut regs = vec![base_reg];
                let levels = &chain[1..];
                for f in &levels[..levels.len().saturating_sub(1)] {
                    let rsub = self.tmp();
                    self.emit(Instr::Field(rsub, ro, f.clone()));
                    regs.push(rsub);
                    ro = rsub;
                }
                // 从叶到基写回：val 链式传递「已更新的子对象」（镜像 update_chain 递归）
                let mut val = rv;
                for i in (0..levels.len()).rev() {
                    self.emit(Instr::FieldSet(regs[i], levels[i].clone(), val, i + 1 == levels.len()));
                    val = regs[i];
                }
            }
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
                // TryBegin 的 pending 条目记录「handler 标签深度」——它指向的
                // handler 位于 Try 块外部，与 try 本身建立时同深。
                self.pending.push((i, lhandler.clone(), self.try_depth));
                self.try_depth += 1;
                self.enter();
                for s2 in body {
                    self.compile_stmt(s2);
                    self.reset_tmp();
                }
                self.exit();
                self.emit(Instr::TryPop);
                self.try_depth -= 1;
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
            | Stmt::Alias { .. } => {}
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
            // T2：byte/bytes 字面量 → 常量池装载（display / 运算执行臂早已对齐解释器）
            Expr::ByteLit(v, _) => {
                let k = self.const_idx(Const::Byte(*v));
                let r = self.tmp();
                self.emit(Instr::LoadK(r, k));
                r
            }
            Expr::BytesLit(v, _) => {
                let k = self.const_idx(Const::Bytes(v.clone()));
                let r = self.tmp();
                self.emit(Instr::LoadK(r, k));
                r
            }
            // T1：切片 a[i:j] —— 求值顺序 obj → lo → hi（镜像解释器 eval_slice）
            Expr::Slice { obj, lo, hi, .. } => {
                let ro = self.compile_expr(obj);
                let rlo = lo.as_ref().map(|e| self.compile_expr(e));
                let rhi = hi.as_ref().map(|e| self.compile_expr(e));
                let r = self.tmp();
                self.emit(Instr::Slice(r, ro, rlo, rhi));
                r
            }
            // T6：new Type(...) → TypeNew（实参求值顺序与 interp 一致：绑定序 = 字段序）
            Expr::New { ty, args, .. } => {
                let n = args.len();
                let base = self.tmp();
                for _ in 0..n {
                    self.tmp();
                }
                for (i, a) in args.iter().enumerate() {
                    let ri = self.compile_expr(a);
                    self.emit(Instr::Move(base + i, ri));
                }
                let r = self.tmp();
                self.emit(Instr::TypeNew(r, ty.clone(), base, n));
                r
            }
            // T6：obj.m(...) → CallMethod（运行期沿 extends 链解析「类型.方法」）
            Expr::MethodCall { obj, name, args, .. } => {
                let ro = self.compile_expr(obj);
                let n = args.len();
                let base = self.tmp();
                for _ in 0..n {
                    self.tmp();
                }
                for (i, a) in args.iter().enumerate() {
                    let ri = self.compile_expr(a);
                    self.emit(Instr::Move(base + i, ri));
                }
                let r = self.tmp();
                self.emit(Instr::CallMethod(r, ro, name.clone(), base, n));
                r
            }
            Expr::Ident { name, .. } if self.hidden_by_default(name) => {
                // G2.8：默认表达式只能看见序号更小的形参（后置形参在解释器里未绑定）
                let r = self.tmp();
                self.emit(Instr::Raise(
                    codes::UNDEFINED,
                    format!("undefined variable `{name}`"),
                    Some("declare the variable before reading it".to_string()),
                ));
                self.emit(Instr::LoadNull(r));
                r
            }
            Expr::Ident { name, .. } => match self.lookup(name) {
                Some(r) => r,
                None if self.in_default_expr.is_some() => {
                    // G2.8：默认表达式求值失败 → 运行期 H002（文案 / help / 定位对齐解释器）。
                    let r = self.tmp();
                    self.emit(Instr::Raise(
                        codes::UNDEFINED,
                        format!("undefined variable `{name}`"),
                        Some("declare the variable before reading it".to_string()),
                    ));
                    self.emit(Instr::LoadNull(r));
                    r
                }
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
            Expr::ListComp { elem, var, var2, iter, cond, span } => {
                self.compile_comp(iter, var, var2, elem, None, cond, span)
            }
            Expr::DictComp { key, value, var, var2, iter, cond, span } => {
                self.compile_comp(iter, var, var2, value, Some(key), cond, span)
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
                // G2.3 引擎对齐：&&/|| 改**短路求值**。此前 rhs 在分支前无条件求值——
                // 与解释器分叉（`false && f()` 仍调用 f，副作用可见），且非 bool 操作数
                // 静默按 truthy 处理。新语义逐条镜像解释器：
                //   And = a 恰为 Bool(false) → false；否则 b 必须为 bool → 结果为 b；
                //   Or  = a 恰为 Bool(true)  → true; 否则 b 必须为 bool → 结果为 b。
                // 非 bool 的 b → H001「logical operators require `bool` operands」（span = 运算符）。
                if matches!(op, BinOp::And | BinOp::Or) {
                    let is_and = matches!(op, BinOp::And);
                    let ra = self.compile_expr(lhs);
                    let r = self.tmp();
                    let rt = self.tmp();
                    self.emit(Instr::IsBoolVal(rt, ra, !is_and)); // And 判 false / Or 判 true
                    let lshort = self.new_label("Lshort");
                    let lend = self.new_label("Lend");
                    self.jit(rt, &lshort);
                    let rb = self.compile_expr(rhs); // 延迟到分支后编译：短路时不求值
                    self.cond_check(rb, 5, expr_span(e));
                    self.emit(Instr::Move(r, rb));
                    self.jmp(&lend);
                    self.emit_label(&lshort);
                    self.emit(Instr::LoadBool(r, !is_and));
                    self.emit_label(&lend);
                    return r;
                }
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
                    // And / Or 已在函数开头短路分支处理（早返回）。
                    BinOp::And | BinOp::Or => unreachable!("And/Or 已由短路分支处理"),
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
                    // 绑定变量与分支体按独立作用域编译（G2.3 对齐解释器的 push_scope/pop_scope）。
                    // 用「临时区声明 + 手工压/弹作用域」（与推导式循环变量 decl_tmp 同口径）：
                    // 绑定不推进 var_next、不写 chunk.locals，弹出后对外层完全无痕——
                    // 此前 decl 直接插进当前作用域且从不弹出，绑定寄存器会顶掉外层同名变量
                    // （`rr` 绑定后外层 `rr` 在 VM 侧被改写；interp 分支结束弹作用域、外层不变）。
                    self.scopes.push((HashMap::new(), self.var_next));
                    // 变体模式：跳转目标之后、求值 body 之前完成载荷绑定
                    if let Pattern::Variant { binds, .. } = pat {
                        for (idx, b) in binds.iter().enumerate() {
                            if let Some(name) = b {
                                let rbind = self.decl_tmp(name);
                                self.emit(Instr::EnumElem(rbind, rv, idx));
                            }
                        }
                    }
                    let ra = self.compile_expr(body);
                    self.emit(Instr::Move(rresult, ra));
                    self.scopes.pop();
                    self.jmp(&lend);
                    self.emit_label(&lnext);
                }
                if !has_wild {
                    // 无匹配分支：报与解释器逐字节一致的 H005（此前抛字面串 → H600 分叉，
                    // G2.3 审计探针实测）。span 取 match 表达式自身（cur_span 未被子表达式改写）。
                    self.emit(Instr::Raise(
                        codes::SYNTAX,
                        "no match arm matched the value".to_string(),
                        Some("add a `_` wildcard arm as the fallback".to_string()),
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
                // G2.3：ternary 条件严格 bool（span = 整表达式 = `?` token，与解释器一致）。
                self.cond_check(rc, 4, expr_span(e));
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
                // 作用域按「内层优先」遍历（rev = 与 lookup 同序，内层同名遮蔽外层）：
                // 正序首见即取会捕获**最外层**同名变量——match 绑定遮蔽外层名时，
                // 闭包捕获到被遮蔽的外层值（interp 取内层绑定值），审计探针实测分叉。
                let mut captures: Vec<(String, usize)> = Vec::new();
                for scope in self.scopes.iter().rev() {
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
                let saved_try_depth = self.try_depth;
                let saved_label_try_depth = std::mem::take(&mut self.label_try_depth);
                let saved_scopes = std::mem::take(&mut self.scopes);
                let saved_cur_locals = std::mem::take(&mut self.cur_locals);
                let saved_var_next = self.var_next;
                let saved_tmp_next = self.tmp_next;
                let saved_max_reg = self.max_reg;
                let saved_loop = std::mem::take(&mut self.loop_labels);
                let saved_with = std::mem::take(&mut self.with_stack);

                // 在 lambda chunk 内先声明捕获变量（注册到作用域供体引用），再声明形参。
                self.enter();
                let mut cap_regs: Vec<(String, usize)> = Vec::new();
                for (name, _) in &captures {
                    let r = self.decl(name);
                    cap_regs.push((name.clone(), r));
                }
                let mut pnames = Vec::new();
                let mut preg = Vec::new();
                for p in params {
                    preg.push(self.decl(&p.name));
                    pnames.push(p.name.clone());
                }
                // G2.8：默认形参序言（在 lambda chunk 内，作用域 = 捕获变量 + 形参，
                // 与解释器 exec_lambda 的调用环境一致）+ 形参序号登记
                self.cur_param_index = pnames
                    .iter()
                    .enumerate()
                    .map(|(i, n)| (n.clone(), i))
                    .collect();
                self.emit_default_prologue(params, &preg);
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
                self.try_depth = saved_try_depth;
                self.label_try_depth = saved_label_try_depth;
                self.scopes = saved_scopes;
                self.cur_locals = saved_cur_locals;
                self.var_next = saved_var_next;
                self.tmp_next = saved_tmp_next;
                self.max_reg = saved_max_reg;
                self.loop_labels = saved_loop;
                self.with_stack = saved_with;

                // 在当前作用域发出 MakeLambda：运行时读取 captures 的寄存器值构建闭包。
                let r = self.tmp();
                self.emit(Instr::MakeLambda(r, idx, captures.clone()));
                r
            }
            Expr::Await { expr, .. } => {
                let src = self.compile_expr(expr);
                let dst = self.tmp();
                self.emit(Instr::Await(dst, src));
                dst
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
        comp_span: &Span,
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
        // 解释器在「列表源 + 双变量」时报错而非退化为单变量（G1.6 审计发现的 interp/VM
        // 分叉）：VM 两条路径都在编译期展开，故此处也发射 Raise，且需把 var2 声明进
        // 作用域，否则产出表达式引用 var2 会在编译期就报「未定义变量」。
        if var2.is_some() {
            let saved_span = self.cur_span.clone();
            self.cur_span = comp_span.clone();
            self.emit(Instr::Raise(
                codes::TYPE_MISMATCH,
                "comprehension `for k, v in` requires a dict, got a list".to_string(),
                Some("comprehend over lists with a single variable: `for x in list`".to_string()),
            ));
            self.cur_span = saved_span;
        }
        self.scopes.push((HashMap::new(), self.var_next));
        let rvar = self.decl_tmp(var);
        if let Some(v2) = var2 {
            // 双变量在列表路径下恒为 null（上面已Raise 报错，此路径不可达）；
            // 仅为满足作用域查找而声明。
            self.decl_tmp(v2);
        }
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
            // G2.3：推导式 if 过滤条件严格 bool（解释器 comprehension 站点的 H001 对齐）。
            self.cond_check(rc, 6, expr_span(c));
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
    /// 本帧内建立的活跃 try 条目（内层在末尾）。随帧弹出而销毁，
    /// 因此跨帧 throw 天然向调用方传播——不会残留 callee 的 stale 条目
    /// 遮蔽 caller 的有效 try（stale 条目会误捕 break/continue 越过
    /// TryPop 后逃逸出的异常，导致死循环/跨帧误捕）。
    try_stack: Vec<TryFrame>,
}

#[derive(Clone)]
struct TryFrame {
    handler: usize,
    catch_reg: usize,
}

pub struct Vm {
    chunks: Vec<Chunk>,
    func_map: HashMap<String, usize>,
    struct_defs: HashMap<String, Vec<(String, bool)>>,
    enum_defs: HashMap<String, Vec<(String, usize)>>,
    /// type 定义（T6）：类型名 → (父类型名(空串=根), (字段名, 类型, 是否只读) 列表)
    type_defs: HashMap<String, (String, Vec<(String, crate::ast::TyName, bool)>)>,
    aliases: Vec<(String, String)>,
    async_fns: HashSet<String>,
    file: String,
    src: String,
    debug: bool,
    frames: Vec<Frame>,
}

impl Vm {
    fn new(
        chunks: Vec<Chunk>,
        func_map: HashMap<String, usize>,
        struct_defs: HashMap<String, Vec<(String, bool)>>,
        enum_defs: HashMap<String, Vec<(String, usize)>>,
        type_defs: HashMap<String, (String, Vec<(String, crate::ast::TyName, bool)>)>,
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
            type_defs,
            async_fns,
            file: file.to_string(),
            src: src.to_string(),
            debug,
            frames: Vec::new(),
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
            try_stack: Vec::new(),
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

    /// 尝试用本帧栈顶 try 处理一个被抛出的异常（设置 catch 寄存器并跳转到 handler）。
    /// try 条目随帧弹出而销毁：被调用函数内的 try 不可能捕获本帧的异常，
    /// 本帧无活跃 try 时返回 false，由 exec 循环把本帧弹出后由调用方的 try 捕获。
    fn try_catch(&mut self, thrown: &Value) -> bool {
        let top = self.frames.len() - 1;
        if let Some(tf) = self.frames[top].try_stack.last().cloned() {
            self.frames[top].try_stack.pop();
            self.frames[top].regs[tf.catch_reg] = thrown.clone();
            self.frames[top].pc = tf.handler;
            true
        } else {
            false
        }
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
                        match self.overload("__neg", vec![v], span) {
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
                        match self.overload("__not", vec![v], span) {
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
                        match self.overload("__add", vec![x, y], span) {
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
                        match self.overload("__sub", vec![x, y], span) {
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
                        match self.overload("__mul", vec![x, y], span) {
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
                        match self.overload("__div", vec![x, y], span) {
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
                        match self.overload("__mod", vec![x, y], span) {
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
                    Err(e) => match self.overload("__eq", vec![x, y], span) {
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
                    Err(e) => match self.overload("__ne", vec![x, y], span) {
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
                            match self.overload("__lt", vec![x, y], span) {
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
                            match self.overload("__le", vec![x, y], span) {
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
                            match self.overload("__gt", vec![x, y], span) {
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
                            match self.overload("__ge", vec![x, y], span) {
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
            Instr::IsBytes(d, s) => {
                self.frames[fi].regs[d] =
                    Value::Bool(matches!(self.frames[fi].regs[s], Value::Bytes(_)));
            }
            // G2.3 引擎对齐：条件严格 bool 校验（消息逐字镜像解释器对应分支，无 help）。
            Instr::CondCheck(r, kind) => {
                let v = &self.frames[fi].regs[r];
                if !matches!(v, Value::Bool(_)) {
                    let msg = cond_check_msg(kind, v.type_name());
                    return Err(self.mk_err(codes::TYPE_MISMATCH, msg, span));
                }
            }
            // G2.3 引擎对齐：&&/|| 短路判定（值恰为 Bool(want)，无类型错误）。
            Instr::IsBoolVal(d, s, want) => {
                let v = &self.frames[fi].regs[s];
                self.frames[fi].regs[d] = Value::Bool(matches!(v, Value::Bool(b) if *b == want));
            }
            Instr::IterCheck(s, is_comp) => {
                // 站点分派（镜像解释器）：推导式 ListComp/DictComp 无 bytes 分支（拒收）；
                // `for in` 有 bytes 分支（单变量迭代字节）。
                let ok = if is_comp {
                    matches!(
                        self.frames[fi].regs[s],
                        Value::List(_) | Value::Dict(_)
                    )
                } else {
                    matches!(
                        self.frames[fi].regs[s],
                        Value::List(_) | Value::Dict(_) | Value::Bytes(_)
                    )
                };
                if !ok {
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
                            match self.overload("__index", vec![obj, key], span) {
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
            Instr::Slice(d, o, lo, hi) => {
                let obj = self.frames[fi].regs[o].clone();
                let lov = lo.map(|r| self.frames[fi].regs[r].clone());
                let hiv = hi.map(|r| self.frames[fi].regs[r].clone());
                self.frames[fi].regs[d] = v_slice(&obj, lov.as_ref(), hiv.as_ref(), span, self)?;
            }
            Instr::IndexSet(o, k, v) => {
                let key = self.frames[fi].regs[k].clone();
                let val = self.frames[fi].regs[v].clone();
                let mut dst = self.frames[fi].regs[o].clone();
                v_index_set(&mut dst, &key, val, span, self)?;
                self.frames[fi].regs[o] = dst;
            }
            Instr::DestructGet(d, o, k, nvars, is_dict) => {
                let obj = self.frames[fi].regs[o].clone();
                let key = self.frames[fi].regs[k].clone();
                self.frames[fi].regs[d] = v_destruct_get(&obj, &key, span, self, nvars, is_dict)?;
            }
            Instr::Field(d, o, field) => {
                let obj = self.frames[fi].regs[o].clone();
                self.frames[fi].regs[d] = v_field(&obj, &field, span, self)?;
            }
            Instr::FieldSet(o, field, v, leaf) => {
                let mut dst = self.frames[fi].regs[o].clone();
                let val = self.frames[fi].regs[v].clone();
                v_field_set(&mut dst, &field, val, leaf, span, self)?;
                self.frames[fi].regs[o] = dst;
            }
            Instr::WithEnter(d, s) => {
                let t = self.frames[fi].regs[s].clone();
                self.frames[fi].regs[d] = v_with_enter(&t, span, self)?;
            }
            Instr::WithExit(s, propagate) => {
                let t = self.frames[fi].regs[s].clone();
                v_with_exit(&t, propagate, span, self)?;
            }
            Instr::TypeNew(d, ty, base, n) => {
                let args: Vec<Value> =
                    (base..base + n).map(|i| self.frames[fi].regs[i].clone()).collect();
                self.frames[fi].regs[d] = v_type_new(&ty, args, span, self)?;
            }
            Instr::CallMethod(d, o, name, base, n) => {
                let obj = self.frames[fi].regs[o].clone();
                let args: Vec<Value> =
                    (base..base + n).map(|i| self.frames[fi].regs[i].clone()).collect();
                self.frames[fi].regs[d] = v_call_method(&obj, &name, args, span, self)?;
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
                self.frames[fi].regs[d] = Value::mk_list(v);
            }
            Instr::NewDict(d, base, n) => {
                let mut v = Vec::with_capacity(n);
                for i in 0..n {
                    let key = match &self.frames[fi].regs[base + 2 * i] {
                        Value::Str(s) => s.to_string(),
                        other => value_to_str(other),
                    };
                    let val = self.frames[fi].regs[base + 2 * i + 1].clone();
                    v.push((key, val));
                }
                self.frames[fi].regs[d] = Value::mk_dict(v);
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
                self.frames[fi].try_stack.push(TryFrame { handler, catch_reg });
            }
            Instr::TryPop => {
                self.frames[fi].try_stack.pop();
            }
            Instr::Throw(reg) => {
                let v = self.frames[fi].regs[reg].clone();
                // 与解释器 Stmt::Throw 逐字一致：str → H600（定位取 throw 语句 span）；
                // error 重抛同文件保留原始定位、跨文件退化并附原始位置；其余类型 H001。
                match v {
                    Value::Str(s) => {
                        return Err(self.mk_err(codes::THROW, s.to_string(), span));
                    }
                    Value::Error(e) => {
                        if e.file == self.file {
                            return Err(self.mk_err_with(
                                e.code,
                                e.message.clone(),
                                &Span {
                                    line: e.line,
                                    col: e.col,
                                    len: e.len,
                                },
                                e.help.clone(),
                            ));
                        } else {
                            return Err(Value::Error(ErrorObj {
                                code: e.code,
                                message: format!("{} (at {}:{}:{})", e.message, e.file, e.line, e.col),
                                file: String::new(),
                                line: 0,
                                col: 0,
                                len: 0,
                                context: String::new(),
                                help: None,
                            }));
                        }
                    }
                    other => {
                        return Err(self.mk_err(
                            codes::TYPE_MISMATCH,
                            format!("`throw` accepts a `str` or `error`, got `{}`", other.type_name()),
                            span,
                        ));
                    }
                }
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
            Instr::Raise(code, msg, help) => {
                // 直接抛出错误值：与解释器求值期报错同构，由exec 的 Err 路由
                // 交给 try_catch捕获或向上传播。
                return Err(self.mk_err_with(code, msg.clone(), span, help.clone()));
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
    fn overload(
        &mut self,
        name: &str,
        args: Vec<Value>,
        span: &Span,
    ) -> Option<Result<Value, Value>> {
        if let Some(&ci) = self.func_map.get(name) {
            Some(self.call_chunk(ci, args, span))
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
        // 限定名 `变量.方法(...)` 且变量求值为 type 实例 → 实例方法调用
        // （镜像解释器 Expr::Call 分支：lambda 变量 → 实例方法 → call_fn 的派发序）
        if let Some((base_name, method)) = func.split_once('.') {
            if let Some(&reg) = self.chunks[self.frames[fi].chunk].locals.get(base_name) {
                let v = self.frames[fi].regs[reg].clone();
                if matches!(v, Value::TypeInst(_)) {
                    return v_call_method(&v, method, args, span, self);
                }
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
                return self.call_chunk(ci, args, span);
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
            return self.call_chunk(ci, args, span);
        }
        if let Some(fields) = self.struct_defs.get(&func) {
            let mut d = Vec::new();
            d.push(("\u{0}__struct__".to_string(), Value::mk_str(func.clone())));
            for (i, (f, _ro)) in fields.iter().enumerate() {
                let v = args.get(i).cloned().unwrap_or(Value::Null);
                d.push((f.clone(), v));
            }
            return Ok(Value::mk_dict(d));
        }
        // len 重载：内建 `len` 失败（类型不支持）时回退 `__len`（与解释器 call_fn 一致）。
        if func == "len" && self.func_map.contains_key("__len") {
            return match self.call_builtin(&func, args.clone(), span) {
                Ok(v) => Ok(v),
                Err(e) if err_code(&e) == Some(codes::TYPE_MISMATCH) => {
                    match self.overload("__len", args, span) {
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

    fn call_chunk(&mut self, ci: usize, args: Vec<Value>, span: &Span) -> Result<Value, Value> {
        let expected = self.chunks[ci].params.len();
        // G2.8：默认形参放宽个数校验——实参过多报错；实参不足时，缺失位置若带默认值则
        // 交由 chunk 序言填值（否则报「missing argument」，文案对齐解释器）。
        let has_default = |i: usize| {
            self.chunks[ci]
                .param_defaults
                .get(i)
                .copied()
                .unwrap_or(false)
        };
        let missing_required = args.len() < expected && !has_default(args.len());
        if args.len() > expected || missing_required {
            let name = self.chunks[ci].name.clone();
            let (msg, help) = if args.len() > expected {
                (
                    format!(
                        "`{}` expects at most {} arguments, got {}",
                        name,
                        expected,
                        args.len()
                    ),
                    "check the number of arguments passed",
                )
            } else {
                (
                    format!(
                        "missing argument `{}` of `{}`",
                        self.chunks[ci].params[args.len()], name
                    ),
                    "pass the required argument, or give the parameter a default value",
                )
            };
            return Err(self.mk_err_with(
                codes::ARG_COUNT,
                msg,
                span,
                Some(help.to_string()),
            ));
        }
        let nregs = self.chunks[ci].nregs;
        let mut regs = vec![Value::Null; nregs];
        // 实参数写入序言槽（G2.8）：序言按它决定哪些形参需要求默认值。
        let argc_reg = self.chunks[ci].argc_reg;
        if argc_reg != usize::MAX && argc_reg < nregs {
            regs[argc_reg] = Value::Int(args.len() as i64);
        }
        for (i, a) in args.iter().enumerate() {
            if i < nregs {
                regs[i] = a.clone();
            }
        }
        self.frames.push(Frame { chunk: ci, pc: 0, regs, try_stack: Vec::new() });
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
        let expected = chunk.params.len();
        // G2.8：默认形参放宽个数校验（与 call_chunk 同口径）。
        let missing_required = args.len() < expected
            && !chunk
                .param_defaults
                .get(args.len())
                .copied()
                .unwrap_or(false);
        if args.len() > expected || missing_required {
            // G2.4：消息 / help 对齐解释器 call_lambda（原先中文硬编码、无 help）。
            let (msg, help) = if args.len() > expected {
                (
                    format!("lambda expects at most {} arguments, got {}", expected, args.len()),
                    "pass exactly the declared number of arguments",
                )
            } else {
                (
                    format!("missing argument `{}`", chunk.params[args.len()]),
                    "pass the required argument, or give the parameter a default value",
                )
            };
            return Err(self.mk_err_with(
                codes::ARG_COUNT,
                msg,
                span,
                Some(help.to_string()),
            ));
        }
        let nregs = chunk.nregs;
        let mut regs = vec![Value::Null; nregs];
        // 实参数写入序言槽（G2.8）。
        if chunk.argc_reg != usize::MAX && chunk.argc_reg < nregs {
            regs[chunk.argc_reg] = Value::Int(args.len() as i64);
        }
        for (name, r) in &chunk.captured_regs {
            if let Some(v) = lam.captured.get(name) {
                regs[*r] = v.clone();
            }
        }
        // 实参绑定到 lambda chunk 实际声明的参数寄存器：lambda chunk 编译期继承外层
        // var_next，捕获/参数寄存器不从 0 起排（如捕获 base@r1、参数 x@r2），不能用
        // ncap+i 偏移假设；用 locals 表（参数名 → 寄存器号）精确定位（与捕获填装一致）。
        for (i, a) in args.iter().enumerate() {
            if i < expected {
                if let Some(&r) = chunk.locals.get(&chunk.params[i]) {
                    if r < nregs {
                        regs[r] = a.clone();
                    }
                }
            }
        }
        self.frames.push(Frame { chunk: ci, pc: 0, regs, try_stack: Vec::new() });
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
        let type_defs = self.type_defs.clone();
        let aliases = self.aliases.clone();
        let async_fns = self.async_fns.clone();
        let file = self.file.clone();
        let src = self.src.clone();
        let future = FutureVal::new();
        let fut = future.clone();
        thread::spawn(move || {
            let mut vm = Vm::new(
                chunks, func_map, struct_defs, enum_defs, type_defs, aliases, async_fns, &file, &src, false,
            );
            let no_span = Span { line: 0, col: 0, len: 0 };
            let result = vm.call_chunk(ci, args, &no_span);
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
        let type_defs = self.type_defs.clone();
        let aliases = self.aliases.clone();
        let async_fns = self.async_fns.clone();
        let file = self.file.clone();
        let src = self.src.clone();
        thread::spawn(move || {
            let mut vm = Vm::new(
                chunks, func_map, struct_defs, enum_defs, type_defs, aliases, async_fns, &file, &src, false,
            );
            let no_span = Span { line: 0, col: 0, len: 0 };
            if let Err(e) = vm.call_chunk(ci, args, &no_span) {
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
        Const::Str(v) => Value::mk_str(v),
        Const::Char(v) => Value::Char(v),
        Const::Byte(v) => Value::Byte(v),
        Const::Bytes(v) => Value::mk_bytes(v),
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

/// CondCheck 各站点的错误消息（逐字镜像解释器 if/while/do-while/for/ternary/
/// 逻辑运算/推导式 if 七处的运行期校验文案，均无 help 行）。
fn cond_check_msg(kind: u8, tn: &str) -> String {
    match kind {
        0 => format!("if condition must be `bool`, got `{tn}`"),
        1 => format!("while condition must be `bool`, got `{tn}`"),
        2 => format!("do-while condition must be `bool`, got `{tn}`"),
        3 => format!("for condition must be `bool`, got `{tn}`"),
        4 => format!("ternary condition must be `bool`, got `{tn}`"),
        5 => format!("logical operators require `bool` operands, got `{tn}`"),
        _ => format!("comprehension `if` condition must be `bool`, got `{tn}`"),
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
                "comparison operators work on `int` / `float` / `char` / `byte`".to_string()
            } else if m("bytes length") {
                "check the index against `len(bytes)`".to_string()
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
            } else if m("`len` expects") {
                "`len` returns the byte length of a string/bytes, or the element count of a list/dict"
                    .to_string()
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

fn value_to_str(v: &Value) -> String {
    match v.as_plain() {
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Str(s) => s.to_string(),
        Value::Char(c) => c.to_string(),
        // 字节值：8 位二进制（镜像解释器 display）
        Value::Byte(b) => format!("0b{:08b}", b),
        // 字节序列：可打印 ASCII 原样（反斜杠/引号转义），其余 \xNN（镜像解释器 display）
        Value::Bytes(items) => {
            let inner: Vec<String> = items
                .iter()
                .map(|b| {
                    if b.is_ascii_graphic() || *b == b' ' {
                        match *b {
                            b'\\' => "\\\\".to_string(),
                            b'"' => "\\\"".to_string(),
                            other => char::from(other).to_string(),
                        }
                    } else {
                        format!("\\x{:02x}", b)
                    }
                })
                .collect();
            format!("b\"{}\"", inner.join(""))
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
            let l = l.read().unwrap();
            let parts: Vec<String> = l.iter().map(value_to_str).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Dict(d) => {
            // 隐藏 `__struct__` 标记键不显示（struct 实例内部携带）
            let d = d.read().unwrap();
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
        (Value::Str(x), Value::Str(y)) => Ok(Value::mk_str(format!("{}{}", x, y))),
        // 字节序列拼接（+，镜像解释器 arith）
        (Value::Bytes(x), Value::Bytes(y)) => {
            let mut r = x.as_ref().clone();
            r.extend_from_slice(&y);
            Ok(Value::mk_bytes(r))
        }
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
        (Value::Byte(x), Value::Byte(y)) => Ok(x == y),
        (Value::Bytes(x), Value::Bytes(y)) => Ok(x == y),
        (Value::List(x), Value::List(y)) => Ok(*x.read().unwrap() == *y.read().unwrap()),
        (Value::Dict(x), Value::Dict(y)) => Ok(*x.read().unwrap() == *y.read().unwrap()),
        (Value::Ptr(x), Value::Ptr(y)) => Ok(x == y),
        // 实例类实例：按引用相等（同一 new 创建点视为相等，不做结构相等）
        (Value::TypeInst(x), Value::TypeInst(y)) => Ok(std::sync::Arc::ptr_eq(x, y)),
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
        // 字节按数值比较（0-255，镜像解释器 values_cmp；bytes 不支持排序比较）
        (Value::Byte(x), Value::Byte(y)) => x.cmp(&y),
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
                let l = l.read().unwrap();
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
                Ok(Value::mk_str(chars[idx as usize].to_string()))
            } else {
                Err(vm.mk_err(
                    codes::TYPE_MISMATCH,
                    format!("list index must be an int, got `{}`", value_to_str(key)),
                    span,
                ))
            }
        }
        // 字节序列按下标取单个字节（byte 值，与 int 严格隔离；镜像解释器 index_value）
        Value::Bytes(b) => {
            if let Value::Int(i) = key {
                let idx = *i;
                if idx < 0 || idx as usize >= b.len() {
                    return Err(vm.mk_err(
                        codes::TYPE_MISMATCH,
                        format!("index {} out of bounds (bytes length {})", idx, b.len()),
                        span,
                    ));
                }
                Ok(Value::Byte(b[idx as usize]))
            } else {
                Err(vm.mk_err(
                    codes::TYPE_MISMATCH,
                    format!("list index must be an int, got `{}`", key.type_name()),
                    span,
                ))
            }
        }
        Value::Dict(d) => {
            let k = match key {
                Value::Str(s) => s.to_string(),
                Value::Int(i) => i.to_string(),
                other => {
                    return Err(vm.mk_err(
                        codes::TYPE_MISMATCH,
                        format!("字典键类型不支持: {}", value_to_str(other)),
                        span,
                    ))
                }
            };
            let d = d.read().unwrap();
            for (kk, vv) in d.iter() {
                if kk == &k {
                    return Ok(vv.clone());
                }
            }
            Ok(Value::Null)
        }
        other => Err(vm.mk_err_with(
            codes::TYPE_MISMATCH,
            format!("cannot index a value of type `{}`", other.type_name()),
            span,
            Some("indexing is supported on lists and strings, e.g. `a[0]`, `s[1]`".to_string()),
        )),
    }
}

/// 切片求值（T1）：a[i:j]，语义逐字镜像解释器 eval_slice——
/// list/bytes 产副本；负索引从末尾归一、越界钳制、lo>hi 产空副本；
/// 非容器 / 非 int 界的报错文案与 help 与解释器逐字节一致。
fn v_slice(
    obj: &Value,
    lo: Option<&Value>,
    hi: Option<&Value>,
    span: &Span,
    vm: &Vm,
) -> Result<Value, Value> {
    let len: i64 = match obj {
        Value::List(items) => items.read().unwrap().len() as i64,
        Value::Bytes(b) => b.len() as i64,
        other => {
            return Err(vm.mk_err_with(
                codes::TYPE_MISMATCH,
                format!("cannot slice a value of type `{}`", other.type_name()),
                span,
                Some("slicing is supported on lists and byte sequences".to_string()),
            ))
        }
    };
    let lo_v = match lo {
        Some(Value::Int(x)) => *x,
        Some(other) => {
            return Err(vm.mk_err(
                codes::TYPE_MISMATCH,
                format!("slice index must be an int, got `{}`", other.type_name()),
                span,
            ))
        }
        None => 0,
    };
    let hi_v = match hi {
        Some(Value::Int(x)) => *x,
        Some(other) => {
            return Err(vm.mk_err(
                codes::TYPE_MISMATCH,
                format!("slice index must be an int, got `{}`", other.type_name()),
                span,
            ))
        }
        None => len,
    };
    // 规范化：负索引从末尾算；越界截断（镜像解释器）
    let lo_n = if lo_v < 0 { (len + lo_v).max(0) } else { lo_v.min(len) };
    let hi_n = if hi_v < 0 { (len + hi_v).max(0) } else { hi_v.min(len) };
    let (lo_n, hi_n) = (lo_n as usize, hi_n as usize);
    if lo_n > hi_n {
        return Ok(match obj {
            Value::List(_) => Value::mk_list(Vec::new()),
            Value::Bytes(_) => Value::mk_bytes(Vec::new()),
            _ => unreachable!(),
        });
    }
    Ok(match obj {
        Value::List(items) => {
            let items = items.read().unwrap();
            Value::mk_list(items[lo_n..hi_n].to_vec())
        }
        Value::Bytes(b) => Value::mk_bytes(b[lo_n..hi_n].to_vec()),
        _ => unreachable!(),
    })
}

/// 字段赋值（T3）：镜像解释器 update_chain 的 dict 分支——
/// leaf=true：readonly 拦截（经 __struct__ 标记查 struct 表）→ 隐藏键报
/// 「no field ...」→ 未知字段报同文案；非 dict 报 cannot assign to field。
/// leaf=false（中间层写回）：隐藏键报 "cannot write to the hidden struct marker
/// field"，存在则静默更新（缺失不可能：Field 读已先报）。TypeInst 写路径由 T6 接手。
/// 值语义：写屏障 ensure_unique（独占原地写，共享先隔离），可观察行为与
/// 解释器「快照 + 整体写回」逐字节一致。
fn v_field_set(
    dst: &mut Value,
    field: &str,
    val: Value,
    leaf: bool,
    span: &Span,
    vm: &Vm,
) -> Result<(), Value> {
    dst.ensure_unique();
    match dst {
        Value::Dict(entries) => {
            let mut entries = entries.write().unwrap();
            if leaf {
                // readonly 字段拦截（运行时兜底，镜像 is_struct_readonly_field）
                let sname = match entries.iter().find(|(k, _)| k == STRUCT_MARKER_KEY) {
                    Some((_, Value::Str(s))) => Some(s.to_string()),
                    _ => None,
                };
                if let Some(name) = sname {
                    if let Some(fields) = vm.struct_defs.get(&name) {
                        if let Some((_, ro)) = fields.iter().find(|(k, _)| k == field) {
                            if *ro {
                                return Err(vm.mk_err_with(
                                    codes::TYPE_MISMATCH,
                                    format!("field `{}` of struct `{}` is readonly", field, name),
                                    span,
                                    Some(
                                        "remove `readonly` from the field definition to allow assignment"
                                            .to_string(),
                                    ),
                                ));
                            }
                        }
                    }
                }
                if Value::is_hidden_struct_key(field) {
                    let visible: Vec<&str> = entries
                        .iter()
                        .filter(|(k, _)| !Value::is_hidden_struct_key(k))
                        .map(|(k, _)| k.as_str())
                        .collect();
                    return Err(vm.mk_err_with(
                        codes::UNDEFINED,
                        format!("no field `{}` (dict/struct has {})", field, visible.join(", ")),
                        span,
                        None,
                    ));
                }
                if let Some(slot) = entries.iter_mut().find(|(k, _)| k == field) {
                    slot.1 = val;
                } else {
                    let visible: Vec<&str> = entries
                        .iter()
                        .filter(|(k, _)| !Value::is_hidden_struct_key(k))
                        .map(|(k, _)| k.as_str())
                        .collect();
                    return Err(vm.mk_err_with(
                        codes::UNDEFINED,
                        format!("no field `{}` (dict/struct has {})", field, visible.join(", ")),
                        span,
                        None,
                    ));
                }
            } else if Value::is_hidden_struct_key(field) {
                return Err(vm.mk_err_with(
                    codes::UNDEFINED,
                    "cannot write to the hidden struct marker field".to_string(),
                    span,
                    None,
                ));
            } else if let Some(slot) = entries.iter_mut().find(|(k, _)| k == field) {
                slot.1 = val;
            }
            Ok(())
        }
        Value::TypeInst(inst) => {
            // 引用语义：RwLock 字段表共享，直写对调用方立即可见（镜像 interp update_chain）
            let mut fields = inst.fields.write().unwrap();
            if leaf {
                // readonly 拦截：沿继承链查字段定义（镜像 check_field_readonly）
                let mut cur = Some(inst.ty.clone());
                let mut seen: HashSet<String> = HashSet::new();
                while let Some(tname) = cur.take() {
                    if !seen.insert(tname.clone()) {
                        break;
                    }
                    match vm.type_defs.get(&tname) {
                        Some((base, defs)) => {
                            if let Some((_, _, ro)) = defs.iter().find(|(k, _, _)| k == field) {
                                if *ro {
                                    return Err(vm.mk_err_with(
                                        codes::TYPE_MISMATCH,
                                        format!("field `{}` of type `{}` is readonly", field, tname),
                                        span,
                                        Some("remove `readonly` from the field definition to allow assignment".to_string()),
                                    ));
                                }
                                break;
                            }
                            cur = if base.is_empty() { None } else { Some(base.clone()) };
                        }
                        None => break,
                    }
                }
                if Value::is_hidden_struct_key(field) || !fields.iter().any(|(k, _)| k == field) {
                    return Err(vm.mk_err_with(
                        codes::UNDEFINED,
                        format!("type `{}` has no field `{}`", inst.ty, field),
                        span,
                        None,
                    ));
                }
                if let Some(slot) = fields.iter_mut().find(|(k, _)| k == field) {
                    slot.1 = val;
                }
            } else {
                if Value::is_hidden_struct_key(field) {
                    return Err(vm.mk_err_with(
                        codes::UNDEFINED,
                        format!("type `{}` has no field `{}`", inst.ty, field),
                        span,
                        None,
                    ));
                }
                // 中间层写回：存在则更新（缺失不可能：Field 读已先报），静默
                if let Some(slot) = fields.iter_mut().find(|(k, _)| k == field) {
                    slot.1 = val;
                }
            }
            Ok(())
        }
        other => {
            if leaf {
                Err(vm.mk_err_with(
                    codes::TYPE_MISMATCH,
                    format!("cannot assign to field `.{}` of type `{}`", field, other.type_name()),
                    span,
                    Some("field assignment targets a `struct`/`type` instance field".to_string()),
                ))
            } else {
                Err(vm.mk_err_with(
                    codes::TYPE_MISMATCH,
                    format!("cannot traverse field `.{}` of type `{}`", field, other.type_name()),
                    span,
                    None,
                ))
            }
        }
    }
}

/// with 进入（T5）：镜像解释器 call_enter——TypeInst 调 `类型.__enter__(self)`
/// （func_map「类型名.方法名」派发，缺方法报与解释器 invoke_type_method 同文案）；
/// Int 内置句柄（sqlite/sse/guipro 资源）原样返回；Ptr 原样；其余报 H002。
fn v_with_enter(t: &Value, span: &Span, vm: &mut Vm) -> Result<Value, Value> {
    match t {
        Value::TypeInst(inst) => match vm_find_method(vm, &inst.ty, "__enter__") {
            Some(func) => vm.do_call(&func, vec![t.clone()], span),
            None => Err(vm.mk_err_with(
                codes::UNDEFINED,
                format!("type `{}` has no method `__enter__`", inst.ty),
                span,
                Some("check the method name, or the `extends` chain".to_string()),
            )),
        },
        Value::Int(_) | Value::Ptr(_) => Ok(t.clone()),
        other => Err(vm.mk_err_with(
            codes::UNDEFINED,
            format!(
                "`with` target `{}` does not support `__enter__`",
                other.type_name()
            ),
            span,
            Some(
                "wrap the resource in a `type` defining `fn __enter__(self)` and `fn __exit__(self)`"
                    .to_string(),
            ),
        )),
    }
}

/// with 退出（T5）：镜像解释器 call_exit——TypeInst 调 `类型.__exit__(self)`；
/// Int 资源派发 builtins::with_close（返回值忽略，非资源静默通过）；Ptr 原样通过。
/// propagate=true：__exit__ 报错不能吞（块成功时以 __exit__ 错误为准）；
/// propagate=false：__exit__ 报错丢弃（块报错时原始错误优先）。
fn v_with_exit(t: &Value, propagate: bool, span: &Span, vm: &mut Vm) -> Result<(), Value> {
    let result: Result<(), Value> = match t {
        Value::TypeInst(inst) => match vm_find_method(vm, &inst.ty, "__exit__") {
            Some(func) => vm.do_call(&func, vec![t.clone()], span).map(|_| ()),
            None => Err(vm.mk_err_with(
                codes::UNDEFINED,
                format!("type `{}` has no method `__exit__`", inst.ty),
                span,
                Some("check the method name, or the `extends` chain".to_string()),
            )),
        },
        Value::Int(id) => {
            // 内置句柄：登记于 with 资源表则派发 close；否则非资源，静默通过
            let _ = builtins::with_close(*id);
            Ok(())
        }
        Value::Ptr(_) => Ok(()),
        other => Err(vm.mk_err_with(
            codes::UNDEFINED,
            format!(
                "`with` target `{}` does not support `__exit__`",
                other.type_name()
            ),
            span,
            Some(
                "wrap the resource in a `type` defining `fn __enter__(self)` and `fn __exit__(self)`"
                    .to_string(),
            ),
        )),
    };
    match (result, propagate) {
        (Err(_), false) => Ok(()), // 异常路径：__exit__ 报错被丢弃，块的原始错误优先
        (r, _) => r,
    }
}

/// T6：沿 extends 链查找方法（「类型名.方法名」限定键，子类覆盖父类，继承环保护）。
/// 返回完整调用名。镜像解释器 invoke_type_method 的查找序。
fn vm_find_method(vm: &Vm, ty: &str, method: &str) -> Option<String> {
    let mut cur = Some(ty.to_string());
    let mut seen: HashSet<String> = HashSet::new();
    while let Some(tname) = cur.take() {
        if !seen.insert(tname.clone()) {
            break;
        }
        if vm.func_map.contains_key(&format!("{}.{}", tname, method)) {
            return Some(format!("{}.{}", tname, method));
        }
        if let Some((base, _)) = vm.type_defs.get(&tname) {
            cur = if base.is_empty() { None } else { Some(base.clone()) };
        } else {
            break;
        }
    }
    None
}

/// T6：new Type(...) 构造。镜像解释器 Expr::New——沿继承链收集字段
/// （interp 遍历序：base 字段 → 逐级父字段 → 自身字段）按序绑定实参、
/// 缺省按字段类型取零值；实参过多报 H011；构造后沿链找 init（先本类后父类）
/// 以 self 首参调用，返回非 null 报 H005。
fn v_type_new(ty: &str, args: Vec<Value>, span: &Span, vm: &mut Vm) -> Result<Value, Value> {
    let Some((base, own_fields)) = vm.type_defs.get(ty).cloned() else {
        return Err(vm.mk_err_with(
            codes::UNDEFINED,
            format!("`new` requires a `type` definition, `{}` is not a type", ty),
            span,
            Some("define it with `type Name { ... }` before constructing an instance".to_string()),
        ));
    };
    // 收集继承链字段（镜像 interp 遍历序：先 base，再沿 base 链向上，最后自身）
    let mut all_fields: Vec<(String, TyName, bool)> = Vec::new();
    let mut cur = if base.is_empty() { None } else { Some(base.clone()) };
    let mut seen: HashSet<String> = HashSet::new();
    while let Some(tname) = cur.take() {
        if !seen.insert(tname.clone()) {
            break;
        }
        if let Some((nb, nf)) = vm.type_defs.get(&tname) {
            all_fields.extend(nf.iter().cloned());
            cur = if nb.is_empty() { None } else { Some(nb.clone()) };
        } else {
            break;
        }
    }
    all_fields.extend(own_fields.iter().cloned());
    if args.len() > all_fields.len() {
        return Err(vm.mk_err_with(
            codes::ARG_COUNT,
            format!("`new {}` takes {} field(s), got {}", ty, all_fields.len(), args.len()),
            span,
            Some("construct with `new Type(field1, field2, ...)` matching the field order".to_string()),
        ));
    }
    // 按字段顺序绑定实参，缺省为零值（镜像 default_value）
    let mut field_vals: Vec<(String, Value)> = Vec::new();
    for (i, (fname, fty, _ro)) in all_fields.iter().enumerate() {
        let v = if i < args.len() {
            args[i].clone()
        } else {
            match fty {
                TyName::Int => Value::Int(0),
                TyName::Float => Value::Float(0.0),
                TyName::Bool => Value::Bool(false),
                TyName::Str => Value::mk_str(String::new()),
                TyName::Char => Value::Char('\0'),
                TyName::Byte => Value::Byte(0),
                TyName::Bytes => Value::mk_bytes(Vec::new()),
                TyName::Var(_) | TyName::Inferred => Value::Null,
            }
        };
        field_vals.push((fname.clone(), v));
    }
    let inst = Value::TypeInst(std::sync::Arc::new(TypeInstVal {
        ty: ty.to_string(),
        fields: std::sync::RwLock::new(field_vals),
    }));
    // 找 init：优先本类（方法覆盖语义），再沿链向上（镜像 interp 查找序）
    if let Some(init_name) = vm_find_method(vm, ty, "init") {
        let mut full_args = vec![inst.clone()];
        full_args.extend(args);
        let r = vm.do_call(&init_name, full_args, span)?;
        if !matches!(r, Value::Null) {
            return Err(vm.mk_err_with(
                codes::SYNTAX,
                format!("`init` of `{}` must not return a value", ty),
                span,
                Some("remove the `return` from `init`; assign to `self.field` instead".to_string()),
            ));
        }
    }
    Ok(inst)
}

/// T6：type 实例方法调用。沿 extends 链查方法（子类覆盖父类），self 作首参；
/// 缺方法 / 非 TypeInst 的报错文案与解释器逐字节一致。
fn v_call_method(obj: &Value, name: &str, args: Vec<Value>, span: &Span, vm: &mut Vm) -> Result<Value, Value> {
    match obj {
        Value::TypeInst(inst) => match vm_find_method(vm, &inst.ty, name) {
            Some(fname) => {
                let mut full_args = vec![obj.clone()];
                full_args.extend(args);
                vm.do_call(&fname, full_args, span)
            }
            None => Err(vm.mk_err_with(
                codes::UNDEFINED,
                format!("type `{}` has no method `{}`", inst.ty, name),
                span,
                Some("check the method name, or the `extends` chain".to_string()),
            )),
        },
        other => Err(vm.mk_err_with(
            codes::TYPE_MISMATCH,
            format!(
                "method call `.{}` requires a `type` instance, got `{}`",
                name,
                other.type_name()
            ),
            span,
            Some("methods can only be called on instances created with `new Type(...)`".to_string()),
        )),
    }
}

fn v_index_set(dst: &mut Value, key: &Value, val: Value, span: &Span, vm: &Vm) -> Result<(), Value> {
    // 值语义闸口：共享容器先深拷贝隔离写者（镜像解释器 set_index_value 的快照+写回；
    // 独占时零拷贝原地写）。str 不可变，不经此门。
    dst.ensure_unique();
    match dst {
        Value::List(l) => {
            if let Value::Int(i) = key {
                let idx = *i;
                let mut l = l.write().unwrap();
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
                Value::Str(s) => s.to_string(),
                Value::Int(i) => i.to_string(),
                other => {
                    return Err(vm.mk_err(
                        codes::TYPE_MISMATCH,
                        format!("字典键类型不支持: {}", value_to_str(other)),
                        span,
                    ))
                }
            };
            let mut d = d.write().unwrap();
            for item in d.iter_mut() {
                if item.0 == k {
                    item.1 = val;
                    return Ok(());
                }
            }
            d.push((k, val));
            Ok(())
        }
        other => Err(vm.mk_err_with(
            codes::TYPE_MISMATCH,
            format!("cannot index a value of type `{}`", other.type_name()),
            span,
            Some("index assignment is supported on lists, e.g. `a[0] = x`".to_string()),
        )),
    }
}

/// 解构取值：列表按位取（越界报错），字典按键取（缺键报错），其他类型报错。
/// 与解释器 exec_stmt 的 DestructAssign 语义一致（含形态区分与变量数报错文案）。
/// nvars = 解构目标变量数；is_dict = 目标是否全为带键形态（`{a, b} = dict`）。
fn v_destruct_get(
    obj: &Value,
    key: &Value,
    span: &Span,
    vm: &Vm,
    nvars: usize,
    is_dict: bool,
) -> Result<Value, Value> {
    match obj {
        Value::List(l) => {
            // 形态不符：`{...} =` 收到列表（镜像解释器 1031-1036）
            if is_dict {
                return Err(vm.mk_err_with(
                    codes::TYPE_MISMATCH,
                    "dict destructuring `{...} =` requires a dict value, got a list".to_string(),
                    span,
                    Some("use `a, b = list` for list destructuring".to_string()),
                ));
            }
            if let Value::Int(i) = key {
                let idx = *i;
                let l = l.read().unwrap();
                if idx < 0 || (idx as usize) >= l.len() {
                    return Err(vm.mk_err_with(
                        codes::TYPE_MISMATCH,
                        format!(
                            "destructuring a list of {} element(s) into {} variable(s)",
                            l.len(),
                            nvars
                        ),
                        span,
                        Some("the list must have at least as many elements as the variables".to_string()),
                    ));
                }
                Ok(l[idx as usize].clone())
            } else {
                Err(vm.mk_err_with(
                    codes::TYPE_MISMATCH,
                    format!("list index must be an int, got `{}`", value_to_str(key)),
                    span,
                    Some("use an integer expression as the index, e.g. `a[0]`, `a[i]`".to_string()),
                ))
            }
        }
        Value::Dict(d) => {
            // 形态不符：`a, b =` 收到字典（镜像解释器 1058-1063）
            if !is_dict {
                return Err(vm.mk_err_with(
                    codes::TYPE_MISMATCH,
                    "list destructuring `a, b =` requires a list value, got a dict".to_string(),
                    span,
                    Some("use `{a, b} = dict` for dict destructuring".to_string()),
                ));
            }
            let k = match key {
                Value::Str(s) => s.to_string(),
                Value::Int(i) => i.to_string(),
                other => {
                    return Err(vm.mk_err(
                        codes::TYPE_MISMATCH,
                        format!("字典键类型不支持: {}", value_to_str(other)),
                        span,
                    ))
                }
            };
            let d = d.read().unwrap();
            for (kk, vv) in d.iter() {
                if kk == &k {
                    return Ok(vv.clone());
                }
            }
            Err(vm.mk_err_with(
                codes::UNDEFINED,
                format!("dict has no key `{}` for destructuring", k),
                span,
                Some("check the key name, or the dict value being destructured".to_string()),
            ))
        }
        other => Err(vm.mk_err_with(
            codes::TYPE_MISMATCH,
            format!(
                "destructuring requires a list or dict value, got `{}`",
                other.type_name()
            ),
            span,
            Some("destructure a list with `a, b = [..]` or a dict with `{a, b} = dict`".to_string()),
        )),
    }
}

fn v_field(obj: &Value, field: &str, span: &Span, vm: &Vm) -> Result<Value, Value> {
    match obj {
        Value::TypeInst(inst) => {
            // 实例字段访问：按实例字段表取值（镜像 interp field_value TypeInst 分支）
            match inst.fields.read().unwrap().iter().find(|(k, _)| k == field) {
                Some((_, val)) => Ok(val.clone()),
                None => Err(vm.mk_err_with(
                    codes::UNDEFINED,
                    format!("type `{}` has no field `{}`", inst.ty, field),
                    span,
                    Some("check the field name, or the `extends` chain".to_string()),
                )),
            }
        }
        Value::Dict(d) => {
            let d = d.read().unwrap();
            match d.iter().find(|(k, _)| !Value::is_hidden_struct_key(k) && k == field) {
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
            }
        }
        Value::Error(e) => match field {
            "message" => Ok(Value::mk_str(e.message.clone())),
            "code" => Ok(Value::mk_str(e.code.to_string())),
            "file" => Ok(Value::mk_str(e.file.clone())),
            "line" => Ok(Value::Int(e.line as i64)),
            "col" => Ok(Value::Int(e.col as i64)),
            "context" => Ok(Value::mk_str(e.context.clone())),
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
        Value::List(l) => Ok(Value::Int(l.read().unwrap().len() as i64)),
        // 隐藏 `__struct__` 标记键不计入长度
        Value::Dict(d) => Ok(Value::Int(
            d.read().unwrap().iter().filter(|(k, _)| !Value::is_hidden_struct_key(k)).count() as i64,
        )),
        Value::Str(s) => Ok(Value::Int(s.len() as i64)),
        // 字节序列长度 = 字节数（与 str 同为字节口径，镜像解释器 len 内置）
        Value::Bytes(b) => Ok(Value::Int(b.len() as i64)),
        other => Err(vm.mk_err(
            codes::TYPE_MISMATCH,
            format!("`len` expects a string, list, dict, or bytes, got `{}`", other.type_name()),
            span,
        )),
    }
}

fn v_keys(v: &Value, span: &Span, vm: &Vm) -> Result<Value, Value> {
    match v {
        Value::Dict(d) => {
            // 隐藏 `__struct__` 标记键不暴露（for-in / keys 共用此路径）
            let keys: Vec<Value> = d
                .read()
                .unwrap()
                .iter()
                .filter(|(k, _)| !Value::is_hidden_struct_key(k))
                .map(|(k, _)| Value::mk_str(k.clone()))
                .collect();
            Ok(Value::mk_list(keys))
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
    pub struct_defs: HashMap<String, Vec<(String, bool)>>,
    pub enum_defs: HashMap<String, Vec<(String, usize)>>,
    pub type_defs: HashMap<String, (String, Vec<(String, crate::ast::TyName, bool)>)>,
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
            type_defs: self.type_defs,
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
        m.type_defs.clone(),
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

/// TyName 的文本形态（与 parse_ty_name 互逆；IR 头 types: 序列化用）。
fn ty_name_text(t: &TyName) -> String {
    match t {
        TyName::Int => "int".to_string(),
        TyName::Float => "float".to_string(),
        TyName::Bool => "bool".to_string(),
        TyName::Str => "str".to_string(),
        TyName::Char => "char".to_string(),
        TyName::Byte => "byte".to_string(),
        TyName::Bytes => "bytes".to_string(),
        TyName::Var(s) => s.clone(),
        TyName::Inferred => String::new(),
    }
}

/// 解析 ty_name_text 的文本形态。
fn parse_ty_name(s: &str) -> TyName {
    match s {
        "int" => TyName::Int,
        "float" => TyName::Float,
        "bool" => TyName::Bool,
        "str" => TyName::Str,
        "char" => TyName::Char,
        "byte" => TyName::Byte,
        "bytes" => TyName::Bytes,
        "" => TyName::Inferred,
        other => TyName::Var(other.to_string()),
    }
}

/// 模块头：structs / enums / types / aliases / async / funcs。
fn module_head(m: &Module) -> String {
    let mut out = String::new();
    let mut structs: Vec<String> = m
        .struct_defs
        .iter()
        .map(|(k, fs)| {
            format!(
                "{}({})",
                k,
                fs.iter()
                    .map(|(f, ro)| if *ro { format!("{}!", f) } else { f.clone() })
                    .collect::<Vec<_>>()
                    .join(",")
            )
        })
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
    // types: Name[Base](字段:类型, 只读字段!类型, ...)——Base 省略 = 根类型
    let mut types: Vec<String> = m
        .type_defs
        .iter()
        .map(|(k, (base, fs))| {
            let fs_s: Vec<String> = fs
                .iter()
                .map(|(f, t, ro)| {
                    let tt = ty_name_text(t);
                    if *ro {
                        format!("{}!{}", f, tt)
                    } else {
                        format!("{}:{}", f, tt)
                    }
                })
                .collect();
            let head = if base.is_empty() { k.clone() } else { format!("{}[{}]", k, base) };
            format!("{}({})", head, fs_s.join(","))
        })
        .collect();
    types.sort();
    out.push_str(&format!("; types: {}\n", types.join(" ")));
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
    let defaults_text = chunk
        .param_defaults
        .iter()
        .map(|b| if *b { "1" } else { "0" })
        .collect::<Vec<_>>()
        .join(", ");
    let argc_text = if chunk.argc_reg == usize::MAX {
        "-1".to_string()
    } else {
        chunk.argc_reg.to_string()
    };
    out.push_str(&format!(
        "; chunk {}  (nregs={}, params=[{}], captures=[{}], locals=[{}], defaults=[{}], argc={})\n",
        chunk.name,
        chunk.nregs,
        chunk.params.join(", "),
        fmt_pairs(&chunk.captured_regs),
        fmt_pairs(&locals),
        defaults_text,
        argc_text
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
        Const::Byte(v) => format!("0b{:08b}", v),
        // 每个字节 \xNN 转义（含可打印字符），保证含 " / \ / 非法 UTF-8 字节往返无损
        Const::Bytes(v) => {
            let inner: String = v.iter().map(|b| format!("\\x{:02x}", b)).collect();
            format!("b\"{}\"", inner)
        }
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
        Instr::IsBytes(d, s) => format!("ISBYTES r{} r{}", d, s),
        Instr::CondCheck(r, kind) => format!("CONDCHK r{} {}", r, kind),
        Instr::IsBoolVal(d, s, want) => {
            format!("ISBOOLV r{} r{} {}", d, s, if *want { "true" } else { "false" })
        }
        Instr::IterCheck(s, is_comp) => {
            format!("ITERCHK r{} {}", s, if *is_comp { "comp" } else { "forin" })
        }
        Instr::Index(d, o, k) => format!("INDEX   r{} r{} r{}", d, o, k),
        Instr::Slice(d, o, lo, hi) => format!(
            "SLICE   r{} r{} {} {}",
            d,
            o,
            lo.map(|r| format!("r{}", r)).unwrap_or_else(|| "-".to_string()),
            hi.map(|r| format!("r{}", r)).unwrap_or_else(|| "-".to_string())
        ),
        Instr::IndexSet(o, k, v) => format!("INDEXSET r{} r{} r{}", o, k, v),
        Instr::DestructGet(d, o, k, nvars, is_dict) => format!(
            "DESTRUCTGET r{} r{} r{} {} {}",
            d,
            o,
            k,
            nvars,
            if *is_dict { "dict" } else { "list" }
        ),
        Instr::Field(d, o, f) => format!("FIELD   r{} r{} {}", d, o, f),
        Instr::FieldSet(o, f, v, leaf) => {
            format!("FIELDSET r{} r{} {} {}", o, v, f, if *leaf { "leaf" } else { "mid" })
        }
        Instr::WithEnter(d, s) => format!("WITHENTER r{} r{}", d, s),
        Instr::WithExit(s, propagate) => {
            format!("WITHEXIT r{} {}", s, if *propagate { "prop" } else { "quiet" })
        }
        Instr::TypeNew(d, ty, base, n) => format!("TYPENEW  r{} {} [r{}..+{}]", d, ty, base, n),
        Instr::CallMethod(d, o, m, base, n) => {
            format!("CALLMETH r{} r{} .{} [r{}..+{}]", d, o, m, base, n)
        }
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
        Instr::Raise(code, msg, help) => format!(
            "RAISE {} {:?}{}",
            code,
            msg,
            match help {
                Some(h) => format!(" help={:?}", h),
                None => String::new(),
            }
        ),
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
    let mut struct_defs: HashMap<String, Vec<(String, bool)>> = HashMap::new();
    let mut enum_defs: HashMap<String, Vec<(String, usize)>> = HashMap::new();
    let mut type_defs: HashMap<String, (String, Vec<(String, crate::ast::TyName, bool)>)> = HashMap::new();
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
                    let fields: Vec<(String, bool)> = if inner.trim().is_empty() {
                        Vec::new()
                    } else {
                        split_top(inner, ',')
                            .into_iter()
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .map(|s| match s.strip_suffix('!') {
                                Some(f) => (f.to_string(), true),
                                None => (s, false),
                            })
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
            } else if let Some(v) = rest.strip_prefix("types:") {
                for tok in v.split_whitespace() {
                    let (head, inner) = split_call(tok).map_err(syn)?;
                    // head: Name 或 Name[Base]（Base = 父类型名，空 = 根类型）
                    let (name, base) = match head.split_once('[') {
                        Some((n, b)) => {
                            let b = b.strip_suffix(']').ok_or_else(|| syn("type 头缺少 `]`"))?;
                            (n.to_string(), b.to_string())
                        }
                        None => (head, String::new()),
                    };
                    let mut fields = Vec::new();
                    if !inner.trim().is_empty() {
                        for item in split_top(inner, ',') {
                            let item = item.trim();
                            if item.is_empty() {
                                continue;
                            }
                            let (fname, ro) = match item.strip_suffix('!') {
                                Some(f) => {
                                    let (f, t) = f
                                        .split_once(':')
                                        .ok_or_else(|| syn(format!("type 字段缺少类型 `{}`", f)))?;
                                    (f.to_string(), true)
                                }
                                None => {
                                    let (f, t) = item
                                        .split_once(':')
                                        .ok_or_else(|| syn(format!("type 字段缺少类型 `{}`", item)))?;
                                    (f.to_string(), false)
                                }
                            };
                            // ro 分支的 t 未取：重新拆一次拿类型文本
                            let ty_text = if ro {
                                item.strip_suffix('!').unwrap().split_once(':').unwrap().1
                            } else {
                                item.split_once(':').unwrap().1
                            };
                            fields.push((fname, parse_ty_name(ty_text.trim()), ro));
                        }
                    }
                    type_defs.insert(name, (base, fields));
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
            type_defs,
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
    let mut param_defaults: Vec<bool> = Vec::new();
    let mut argc_reg = usize::MAX;
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
        } else if let Some(v) = f.strip_prefix("defaults=[") {
            let v = v.strip_suffix(']').ok_or("defaults 缺少 ]")?;
            param_defaults = v
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| s == "1")
                .collect();
        } else if let Some(v) = f.strip_prefix("argc=") {
            // -1 = 无默认形参（无序言）
            argc_reg = match v.trim() {
                "-1" => usize::MAX,
                t => t.parse().map_err(|_| "argc 不是整数".to_string())?,
            };
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
        param_defaults,
        argc_reg,
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
    // 0b01000001：byte 常量（与 Str/Char 前缀不冲突，先于整数解析）
    if let Some(bits) = v.strip_prefix("0b") {
        return u8::from_str_radix(bits, 2)
            .map(Const::Byte)
            .map_err(|_| format!("byte 常量无法解析 `{}`", v));
    }
    // b"\x41\x42"：bytes 常量（const_text 侧恒为 \xNN 转义）
    if v.starts_with("b\"") {
        let inner = v.strip_prefix("b\"").and_then(|s| s.strip_suffix('"')).ok_or("bytes 常量缺少引号")?;
        let mut out = Vec::new();
        let mut it = inner.chars();
        while let Some(c) = it.next() {
            if c != '\\' {
                return Err(format!("bytes 常量含非转义字符 `{}`", c));
            }
            let x = it.next().ok_or("bytes 常量转义不完整")?;
            if x != 'x' {
                return Err(format!("bytes 常量转义非法 `\\{}`", x));
            }
            let hi = it.next().ok_or("bytes 常量转义不完整")?;
            let lo = it.next().ok_or("bytes 常量转义不完整")?;
            let byte = u8::from_str_radix(&format!("{}{}", hi, lo), 16)
                .map_err(|_| format!("bytes 常量 \\x 转义非法 `{}{}`", hi, lo))?;
            out.push(byte);
        }
        return Ok(Const::Bytes(out));
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
        "ISBYTES" => {
            need(2)?;
            Instr::IsBytes(reg(toks[0])?, reg(toks[1])?)
        }
        "CONDCHK" => {
            need(2)?;
            Instr::CondCheck(
                reg(toks[0])?,
                toks[1]
                    .parse()
                    .map_err(|_| format!("CONDCHK kind 非法: {}", toks[1]))?,
            )
        }
        "ISBOOLV" => {
            need(3)?;
            match toks[2] {
                "true" => Instr::IsBoolVal(reg(toks[0])?, reg(toks[1])?, true),
                "false" => Instr::IsBoolVal(reg(toks[0])?, reg(toks[1])?, false),
                other => return Err(format!("ISBOOLV want 非法: {}", other)),
            }
        }
        "ITERCHK" => {
            need(2)?;
            Instr::IterCheck(reg(toks[0])?, toks[1] == "comp")
        }
        "INDEX" => {
            need(3)?;
            Instr::Index(reg(toks[0])?, reg(toks[1])?, reg(toks[2])?)
        }
        "SLICE" => {
            need(4)?;
            let reg_or_none = |t: &str| -> Result<Option<usize>, String> {
                if t == "-" {
                    Ok(None)
                } else {
                    reg(t).map(Some)
                }
            };
            Instr::Slice(reg(toks[0])?, reg(toks[1])?, reg_or_none(toks[2])?, reg_or_none(toks[3])?)
        }
        "INDEXSET" => {
            need(3)?;
            Instr::IndexSet(reg(toks[0])?, reg(toks[1])?, reg(toks[2])?)
        }
        "DESTRUCTGET" => {
            need(5)?;
            Instr::DestructGet(
                reg(toks[0])?,
                reg(toks[1])?,
                reg(toks[2])?,
                toks[3]
                    .parse()
                    .map_err(|_| format!("DESTRUCTGET nvars 非法: {}", toks[3]))?,
                toks[4] == "dict",
            )
        }
        "FIELD" => {
            need(3)?;
            Instr::Field(reg(toks[0])?, reg(toks[1])?, toks[2].to_string())
        }
        "FIELDSET" => {
            need(4)?;
            let leaf = match toks[3] {
                "leaf" => true,
                "mid" => false,
                other => return Err(format!("FIELDSET leaf 标志非法: {}", other)),
            };
            Instr::FieldSet(reg(toks[0])?, toks[2].to_string(), reg(toks[1])?, leaf)
        }
        "WITHENTER" => {
            need(2)?;
            Instr::WithEnter(reg(toks[0])?, reg(toks[1])?)
        }
        "TYPENEW" => {
            need(4)?;
            let (base, n) = parse_range(toks[2])?;
            Instr::TypeNew(reg(toks[0])?, toks[1].to_string(), base, n)
        }
        "CALLMETH" => {
            need(5)?;
            let name = toks[3]
                .strip_prefix('.')
                .ok_or_else(|| format!("CALLMETH 方法名缺少 `.` 前缀: {}", toks[3]))?
                .to_string();
            let (base, n) = parse_range(toks[4])?;
            Instr::CallMethod(reg(toks[0])?, reg(toks[1])?, name, base, n)
        }
        "WITHEXIT" => {
            need(2)?;
            let propagate = match toks[1] {
                "prop" => true,
                "quiet" => false,
                other => return Err(format!("WITHEXIT 传播标志非法: {}", other)),
            };
            Instr::WithExit(reg(toks[0])?, propagate)
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
        "RAISE" => {
            // 文本形态：RAISE <code> <msg...> [help=<help...>]
            need(2)?;
            let (code, tail) = (toks[0], &toks[1..]);
            let (msg, help) = match tail.iter().position(|t| t.starts_with("help=")) {
                Some(i) => (
                    tail[..i].join(" "),
                    Some(unescape_str(&tail[i..].join(" ")[5..])?),
                ),
                None => (tail.join(" "), None),
            };
            let code: &'static str = match code {
                "H001" => codes::TYPE_MISMATCH,
                "H002" => codes::UNDEFINED,
                "H005" => codes::SYNTAX,
                "H009" => codes::DIV_ZERO,
                "H999" => codes::NOT_IMPLEMENTED,
                other => return Err(format!("RAISE 错误码非法: {}", other)),
            };
            Instr::Raise(code, unescape_str(&msg)?, help)
        }
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
