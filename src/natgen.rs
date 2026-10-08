// Hone 原生编译后端（natgen）：VM 字节码 Module → Cranelift object 生成。
// 与 AOT（src/aot.rs 文本 IR）并列的第二个编译后端，见《Hone原生编译后端设计》：
// checker 后的 Program 经 vm::compile_module 得到字节码 Module，本模块把每个
// chunk（函数 / 顶层 main）lowering 为 Cranelift 目标代码（COFF object），
// 与 runtime（crate honeruntime，经 include! 内联进生成式 wrapper）一起由
// rustc 编译链接为最终产物。
//
// ── 值模型与 ABI（T2 定案：句柄式） ──
// 设计 §4.3 原设想「Hnv{t,a,b}=24B 内联寄存器」，但 24B repr(C) 结构体在
// Windows x64 走隐藏 sret 指针，与 cranelift 多结果函数 RAX/RDX/RCX 返回不匹配，
// 故改为句柄式：寄存器（cranelift SSA i64 值）持有 u64 句柄，指向堆上
// HnCell{rc,t,a,b,pkind,payload}。跨 object↔runtime FFI 只传 i64（干净 SysV ABI）。
// 所有权纪律（与 vm.rs 的 clone/移动严格对应，堆值重复释放/丢失 = UB）：
//   * 寄存器槽 = 权威持有者。store_slot(dst,v)：rt_drop 旧槽值后写新值；
//     运算前 rt_retain 出独立引用（镜像 VM 的 clone 消费），rt_binop/rt_unop 消费之。
//   * 函数调用：实参逐个 rt_retain 传给 callee（调用方槽保留原引用）；callee 在
//     return 前 drop 全部非返回槽（含参数），返回值句柄转移给调用方。
//   * 函数入口：非参数槽统一 rt_null() 初始化（VM regs 初始 Value::Null），
//     保证任意路径 return 前 drop 全部槽都是安全的。
//   * fallible 指令（rt_binop/rt_unop）后 rt_is_err 检查：ERR → 跳统一 err_ret
//     块（drop 全部槽后 return err），与 VM「Err 值跨帧传播」语义一致（T6 前无
//     静态 catch 映射，等价于无 handler 场景）。
//
// ── 块规划（T1 结论 + T2 句柄化） ──
// leader 基本块切分：pc 0 + 每个 Jmp/JmpIf* 的 target + 每个条件跳转的 pc+1 +
// 每个 Label 所在 pc。BFS 可达性分析后只填充可达段（不可达段保持 pristine，
// finalize 不校验）。循环回跳目标延迟 seal：全部块填充完后统一 seal_all_blocks
// （本 lowering 全用显式 SSA 值、无 Variable/undef vars，seal 顺序无约束；
// 且跳转到已 seal 块会触发 ssa.rs debug 断言，故绝不能在回跳目标处提前 seal）。
// entry 块参数数必须 == 函数签名参数数（verifier 约束），循环头等需要多值的块
// 参数放在独立块，经 jump/brif 携带。

use crate::error::{codes, ZError};
use crate::vm::{Chunk, Const, Instr, Module};
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::immediates::Offset32;
use cranelift_codegen::ir::types::I64;
use cranelift_codegen::ir::{
    AbiParam, BlockArg, FuncRef, Function, InstBuilder, MemFlagsData, Signature, StackSlot,
    StackSlotData, StackSlotKind, Value,
};
use cranelift_codegen::settings::{builder, Configurable};
use cranelift_codegen::Context;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{default_libcall_names, FuncId, Linkage, Module as ClifModule};
use cranelift_object::{ObjectBuilder, ObjectModule};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;

type ClifBlock = cranelift_codegen::ir::Block;

/// runtime 导入的 FuncId（模块级声明一次，逐 chunk 解析为 FuncRef）。
struct RtImports {
    rt_i64: FuncId,
    rt_f64: FuncId,
    rt_bool: FuncId,
    rt_char: FuncId,
    rt_null: FuncId,
    rt_str: FuncId,
    rt_retain: FuncId,
    rt_drop: FuncId,
    rt_binop: FuncId,
    rt_unop: FuncId,
    rt_print: FuncId,
    rt_truthy: FuncId,
    rt_is_err: FuncId,
    rt_call: FuncId,
    rt_callv: FuncId,
    rt_makelambda: FuncId,
    rt_throw: FuncId,
    rt_throwstr: FuncId,
    rt_field: FuncId,
    // M1：list/dict 承载（G1.2/G1.3）+ for-in 迭代（G1.4）。
    rt_newlist: FuncId,
    rt_newdict: FuncId,
    rt_index: FuncId,
    rt_indexset: FuncId,
    rt_destructget: FuncId,
    rt_len: FuncId,
    rt_keys: FuncId,
    rt_islist: FuncId,
    rt_isdict: FuncId,
    rt_isnull: FuncId,
    rt_itercheck: FuncId,
    rt_raise: FuncId,
    // G2.2：enum 指令（NewEnum/EnumElem/IsEnumVariant 的 lowering 目标）。
    rt_newenum: FuncId,
    rt_enumelem: FuncId,
    rt_isenumvariant: FuncId,
    // G2.3：条件严格 bool 校验 + &&/|| 短路判定（引擎对齐）。
    rt_condcheck: FuncId,
    rt_isboolval: FuncId,
}

/// 本 chunk 内解析好的 FuncRef（借用 bcx.func，生命周期随本 chunk）。
struct RtRefs {
    rt_i64: FuncRef,
    rt_f64: FuncRef,
    rt_bool: FuncRef,
    rt_char: FuncRef,
    rt_null: FuncRef,
    rt_str: FuncRef,
    rt_retain: FuncRef,
    rt_drop: FuncRef,
    rt_binop: FuncRef,
    rt_unop: FuncRef,
    rt_print: FuncRef,
    rt_truthy: FuncRef,
    rt_is_err: FuncRef,
    rt_call: FuncRef,
    rt_callv: FuncRef,
    rt_makelambda: FuncRef,
    rt_throw: FuncRef,
    rt_throwstr: FuncRef,
    rt_field: FuncRef,
    rt_newlist: FuncRef,
    rt_newdict: FuncRef,
    rt_index: FuncRef,
    rt_indexset: FuncRef,
    rt_destructget: FuncRef,
    rt_len: FuncRef,
    rt_keys: FuncRef,
    rt_islist: FuncRef,
    rt_isdict: FuncRef,
    rt_isnull: FuncRef,
    rt_itercheck: FuncRef,
    rt_raise: FuncRef,
    rt_newenum: FuncRef,
    rt_enumelem: FuncRef,
    rt_isenumvariant: FuncRef,
    rt_condcheck: FuncRef,
    rt_isboolval: FuncRef,
}

impl RtImports {
    fn declare(module: &mut ObjectModule) -> Result<Self, ZError> {
        let cc = module.isa().default_call_conv();
        let sig1 = {
            let mut s = Signature::new(cc);
            s.params.push(AbiParam::new(I64));
            s.returns.push(AbiParam::new(I64));
            s
        };
        let sig0_1 = {
            let mut s = Signature::new(cc);
            s.returns.push(AbiParam::new(I64));
            s
        };
        let sig1_0 = {
            let mut s = Signature::new(cc);
            s.params.push(AbiParam::new(I64));
            s
        };
        // G2.3：rt_isboolval(v, want) -> bool 句柄。
        let sig2_1 = {
            let mut s = Signature::new(cc);
            s.params.push(AbiParam::new(I64));
            s.params.push(AbiParam::new(I64));
            s.returns.push(AbiParam::new(I64));
            s
        };
        // T4：fallible 运算带出错指令位置 (ci, pc) —— runtime 查 span 表渲染行/列/caret。
        // rt_binop(op, x, y, ci, pc) -> r；rt_unop(op, x, ci, pc) -> r。
        let sig5_1 = {
            let mut s = Signature::new(cc);
            s.params.push(AbiParam::new(I64));
            s.params.push(AbiParam::new(I64));
            s.params.push(AbiParam::new(I64));
            s.params.push(AbiParam::new(I64));
            s.params.push(AbiParam::new(I64));
            s.returns.push(AbiParam::new(I64));
            s
        };
        let sig4_1 = {
            let mut s = Signature::new(cc);
            s.params.push(AbiParam::new(I64));
            s.params.push(AbiParam::new(I64));
            s.params.push(AbiParam::new(I64));
            s.params.push(AbiParam::new(I64));
            s.returns.push(AbiParam::new(I64));
            s
        };
        // T5：rt_call(sidx, argptr, nargs, ci, pc)；rt_makelambda(ci, capsptr, ncap)。
        let sig3_1 = {
            let mut s = Signature::new(cc);
            s.params.push(AbiParam::new(I64));
            s.params.push(AbiParam::new(I64));
            s.params.push(AbiParam::new(I64));
            s.returns.push(AbiParam::new(I64));
            s
        };
        // T5：rt_callv(v, sidx, argptr, nargs, ci, pc)——带局部变量优先的名分发。
        let sig6_1 = {
            let mut s = Signature::new(cc);
            s.params.push(AbiParam::new(I64));
            s.params.push(AbiParam::new(I64));
            s.params.push(AbiParam::new(I64));
            s.params.push(AbiParam::new(I64));
            s.params.push(AbiParam::new(I64));
            s.params.push(AbiParam::new(I64));
            s.returns.push(AbiParam::new(I64));
            s
        };
        let imp = |module: &mut ObjectModule,
                   name: &str,
                   sig: &Signature|
         -> Result<FuncId, ZError> {
            module.declare_function(name, Linkage::Import, sig).map_err(|e| {
                ZError::plain(
                    codes::INTERNAL,
                    format!("natgen: 声明导入 `{name}` 失败: {e}"),
                    None::<String>,
                )
            })
        };
        Ok(RtImports {
            rt_i64: imp(module, "rt_i64", &sig1)?,
            rt_f64: imp(module, "rt_f64", &sig1)?,
            rt_bool: imp(module, "rt_bool", &sig1)?,
            rt_char: imp(module, "rt_char", &sig1)?,
            rt_null: imp(module, "rt_null", &sig0_1)?,
            rt_str: imp(module, "rt_str", &sig1)?,
            rt_retain: imp(module, "rt_retain", &sig1)?,
            rt_drop: imp(module, "rt_drop", &sig1_0)?,
            rt_binop: imp(module, "rt_binop", &sig5_1)?,
            rt_unop: imp(module, "rt_unop", &sig4_1)?,
            rt_print: imp(module, "rt_print", &sig1_0)?,
            rt_truthy: imp(module, "rt_truthy", &sig1)?,
            rt_is_err: imp(module, "rt_is_err", &sig1)?,
            rt_call: imp(module, "rt_call", &sig5_1)?,
            rt_callv: imp(module, "rt_callv", &sig6_1)?,
            rt_makelambda: imp(module, "rt_makelambda", &sig3_1)?,
            // T6：rt_throw(v, ci, pc)；rt_throwstr(sidx, ci, pc)（ThrowStr 复用 sig1 前形参布局不同，
            // 实际签名 (sidx,ci,pc)->r 即 sig3_1）；rt_field(obj, fsidx, ci, pc) -> r（sig4_1）。
            rt_throw: imp(module, "rt_throw", &sig3_1)?,
            rt_throwstr: imp(module, "rt_throwstr", &sig3_1)?,
            rt_field: imp(module, "rt_field", &sig4_1)?,
            // M1：rt_newlist/rt_newdict(ptr, n, ci, pc)；rt_index/rt_destructget(obj, key, ci, pc)；
            // rt_indexset(obj, key, val, ci, pc)（sig5_1）；rt_len/rt_keys(v, ci, pc)；
            // rt_itercheck(src, is_comp, ci, pc)；rt_islist/rt_isdict/rt_isnull(h)。
            rt_newlist: imp(module, "rt_newlist", &sig4_1)?,
            rt_newdict: imp(module, "rt_newdict", &sig4_1)?,
            rt_index: imp(module, "rt_index", &sig4_1)?,
            rt_indexset: imp(module, "rt_indexset", &sig5_1)?,
            // M1：rt_destructget(obj, key, nvars, is_dict, ci, pc)——6 参（sig6_1，镜像解释器文案）。
            rt_destructget: imp(module, "rt_destructget", &sig6_1)?,
            rt_len: imp(module, "rt_len", &sig3_1)?,
            rt_keys: imp(module, "rt_keys", &sig3_1)?,
            rt_islist: imp(module, "rt_islist", &sig1)?,
            rt_isdict: imp(module, "rt_isdict", &sig1)?,
            rt_isnull: imp(module, "rt_isnull", &sig1)?,
            rt_itercheck: imp(module, "rt_itercheck", &sig4_1)?,
            // M1：rt_raise(code_sidx, msg_sidx, help_sidx, ci, pc) —— 5 参（sig5_1）。
            // help_sidx = -1 表示该分支无 help 行（哨兵，镜像解释器 help=None）。
            rt_raise: imp(module, "rt_raise", &sig5_1)?,
            // G2.2：rt_newenum(n_sidx, v_sidx, argptr, nargs)（sig4_1，无 ci/pc——无错误路径）；
            // rt_enumelem(src, idx, ci, pc)（sig4_1，fallible）；
            // rt_isenumvariant(src, n_sidx, v_sidx)（sig3_1，恒返回 bool）。
            rt_newenum: imp(module, "rt_newenum", &sig4_1)?,
            rt_enumelem: imp(module, "rt_enumelem", &sig4_1)?,
            rt_isenumvariant: imp(module, "rt_isenumvariant", &sig3_1)?,
            // G2.3：rt_condcheck(v, kind, ci, pc)（sig4_1）；rt_isboolval(v, want)（sig2_1）。
            rt_condcheck: imp(module, "rt_condcheck", &sig4_1)?,
            rt_isboolval: imp(module, "rt_isboolval", &sig2_1)?,
        })
    }

    fn refs(&self, module: &mut ObjectModule, func: &mut Function) -> RtRefs {
        RtRefs {
            rt_i64: module.declare_func_in_func(self.rt_i64, func),
            rt_f64: module.declare_func_in_func(self.rt_f64, func),
            rt_bool: module.declare_func_in_func(self.rt_bool, func),
            rt_char: module.declare_func_in_func(self.rt_char, func),
            rt_null: module.declare_func_in_func(self.rt_null, func),
            rt_str: module.declare_func_in_func(self.rt_str, func),
            rt_retain: module.declare_func_in_func(self.rt_retain, func),
            rt_drop: module.declare_func_in_func(self.rt_drop, func),
            rt_binop: module.declare_func_in_func(self.rt_binop, func),
            rt_unop: module.declare_func_in_func(self.rt_unop, func),
            rt_print: module.declare_func_in_func(self.rt_print, func),
            rt_truthy: module.declare_func_in_func(self.rt_truthy, func),
            rt_is_err: module.declare_func_in_func(self.rt_is_err, func),
            rt_call: module.declare_func_in_func(self.rt_call, func),
            rt_callv: module.declare_func_in_func(self.rt_callv, func),
            rt_makelambda: module.declare_func_in_func(self.rt_makelambda, func),
            rt_throw: module.declare_func_in_func(self.rt_throw, func),
            rt_throwstr: module.declare_func_in_func(self.rt_throwstr, func),
            rt_field: module.declare_func_in_func(self.rt_field, func),
            rt_newlist: module.declare_func_in_func(self.rt_newlist, func),
            rt_newdict: module.declare_func_in_func(self.rt_newdict, func),
            rt_index: module.declare_func_in_func(self.rt_index, func),
            rt_indexset: module.declare_func_in_func(self.rt_indexset, func),
            rt_destructget: module.declare_func_in_func(self.rt_destructget, func),
            rt_len: module.declare_func_in_func(self.rt_len, func),
            rt_keys: module.declare_func_in_func(self.rt_keys, func),
            rt_islist: module.declare_func_in_func(self.rt_islist, func),
            rt_isdict: module.declare_func_in_func(self.rt_isdict, func),
            rt_isnull: module.declare_func_in_func(self.rt_isnull, func),
            rt_itercheck: module.declare_func_in_func(self.rt_itercheck, func),
            rt_raise: module.declare_func_in_func(self.rt_raise, func),
            rt_newenum: module.declare_func_in_func(self.rt_newenum, func),
            rt_enumelem: module.declare_func_in_func(self.rt_enumelem, func),
            rt_isenumvariant: module.declare_func_in_func(self.rt_isenumvariant, func),
            rt_condcheck: module.declare_func_in_func(self.rt_condcheck, func),
            rt_isboolval: module.declare_func_in_func(self.rt_isboolval, func),
        }
    }
}

/// 每个 VM 寄存器一个 8B 句柄槽（cranelift 栈槽，ExplicitSlot）。
/// 槽是**内存**而非 SSA 值：任意块（含多前驱的 err_ret / if-else 合并点 / 循环
/// 回边）在自己的块内 stack_load/stack_store 即可，无「值定义块必须支配使用块」
/// 的约束——SSA 槽在 err_ret（N 个前驱）上必然违反支配，故帧必须走内存。
/// 栈槽 store 是带副作用指令（can_store），DCE 不会删除，所有权转移安全。
/// bcx 以 `&'x mut` 借用持有；方法内部对非 bcx 字段先拷副本再调 &mut self 方法，
/// 避免「参数 immutable + 调用 mutable」的同表达式借用冲突（FuncRef/Value/Block 均 Copy）。
struct ChunkLower<'x, 'f> {
    bcx: &'x mut FunctionBuilder<'f>,
    /// 寄存器槽（每 VM 寄存器一个 StackSlot）。
    slots: Vec<StackSlot>,
    /// leader pc → cranelift 块。
    blocks: HashMap<usize, ClifBlock>,
    /// 可达 leader（BFS）。
    reachable: HashSet<usize>,
    /// 统一错误返回块（惰性创建）：块参数 = err 句柄。
    err_ret: Option<ClifBlock>,
    err_ret_param: Option<Value>,
    /// 当前正在填充的块。
    cur_block: ClifBlock,
    /// 当前块是否已以 terminator 结束。
    cur_terminated: bool,
    /// runtime 函数引用（本 chunk 内解析）。
    rt: RtRefs,
    /// chunk 常量 → 字符串表索引（预计算的 const_idx → strtab idx）。
    const_str_idx: Vec<Option<usize>>,
    /// 本 chunk 在 Module 中的索引（T4：rt_binop/rt_unop 出错时查 span 表）。
    ci: u32,
    /// Call 指令函数名 → strtab 索引（T5：rt_call/rt_callv 名分发）。
    func_str_idx: HashMap<String, usize>,
    /// 实参打包区（T5：Call 前把实参句柄连续 store，stack_addr 传 runtime）。
    param_ss: StackSlot,
    /// 捕获打包区（T5：MakeLambda 前把捕获句柄连续 store）。
    cap_ss: StackSlot,
    /// T6：每个 pc 的最近活跃 handler（TryBegin 压栈 / TryPop 出栈，嵌套取最内层）。
    catch_map: Vec<Option<(usize, usize)>>,
    /// T6：catch 块懒建（key = (handler_pc, catch_reg)）；块参数 = err 句柄。
    catch_blocks: Vec<((usize, usize), ClifBlock, Value)>,
    /// T6：字段名 → strtab 索引（Field lowering 经 rt_field 名分发）。
    field_str_idx: HashMap<String, usize>,
    /// G2.5 别名表（新名 → 原名）：Call lowering 编译期解析别名，决定走 rt_callv 还是 rt_call
    /// （镜像 Vm::do_call 把 resolve_alias 放在局部变量查找之前）。
    alias_map: HashMap<String, String>,
}

impl<'x, 'f> ChunkLower<'x, 'f> {
    /// 别名解析（可叠加，限轮数防环，同 vm.rs resolve_alias / interp resolve_alias_name）。
    fn resolve_alias(&self, name: &str) -> Option<String> {
        if !self.alias_map.contains_key(name) {
            return None;
        }
        let mut cur = name.to_string();
        for _ in 0..=self.alias_map.len() {
            match self.alias_map.get(&cur) {
                Some(next) => cur = next.clone(),
                None => break,
            }
        }
        Some(cur)
    }
    fn call1(&mut self, fref: FuncRef, args: &[Value]) -> Value {
        let inst = self.bcx.ins().call(fref, args);
        self.bcx.func.dfg.first_result(inst)
    }

    /// 读槽内当前句柄（当前块内 stack_load）。
    fn load_slot(&mut self, i: usize) -> Value {
        let ss = self.slots[i];
        self.bcx.ins().stack_load(I64, I64, ss, 0)
    }

    /// 释放槽内旧值并写入新值（槽 = 权威持有者；v 的所有权转移给槽）。
    fn store_slot(&mut self, dst: usize, v: Value) {
        let ss = self.slots[dst];
        let old = self.bcx.ins().stack_load(I64, I64, ss, 0);
        self.bcx.ins().call(self.rt.rt_drop, &[old]);
        self.bcx.ins().stack_store(I64, v, ss, 0);
    }

    /// 保留 in-flight 值（调用方将消费的实参 / 运算输入）。
    fn retain(&mut self, v: Value) -> Value {
        let f = self.rt.rt_retain;
        self.call1(f, &[v])
    }

    /// 枚举名 / 变体名 → strtab 索引（G2.2，口径同 Field 字段名：main.rs 已把
    /// enum_defs 全集收进 strtab，field_str_idx = strtab 全集反查表）。
    fn enum_str_idx(&mut self, s: &str, kind: &str) -> Result<usize, ZError> {
        self.field_str_idx
            .get(s)
            .copied()
            .ok_or_else(|| {
                ZError::plain(
                    codes::INTERNAL,
                    format!("natgen: enum {kind} `{s}` 缺少字符串表索引"),
                    None::<String>,
                )
            })
    }

    /// fallible 指令后统一 ERR 检查：ERR → err_ret 块；否则切到新 cont 块继续。
    /// 必须切 cont 块（不能 brif 自指留在当前块）：cranelift 前端禁止对已被分支
    /// 回指的块继续发射指令（"cannot add an instruction to a block already filled"）。
    /// cont 块带块参数接收结果句柄，与内存槽正交：err_ret 用 stack_load 读槽（内存），
    /// 无跨前驱 SSA 依赖，多前驱不触发支配问题。
    fn check_err(&mut self, r: Value, pc: usize) {
        let f = self.rt.rt_is_err;
        let err = self.call1(f, &[r]);
        let ok = self.bcx.ins().icmp_imm_s(IntCC::Equal, err, 0);
        let (on_err, err_args): (ClifBlock, Vec<Value>) =
            match self.catch_map.get(pc).copied().flatten() {
                // T6：有活跃 handler → 跳该 handler 的 catch 块（存 catch_reg 后跳 handler）。
                Some((handler_pc, catch_reg)) => {
                    let key = (handler_pc, catch_reg);
                    let entry = self
                        .catch_blocks
                        .iter()
                        .find(|(k, _, _)| *k == key)
                        .cloned();
                    match entry {
                        Some((_, b, p)) => (b, vec![r.into()]),
                        None => {
                            let b = self.bcx.create_block();
                            let p = self.bcx.func.dfg.append_block_param(b, I64);
                            self.catch_blocks.push((key, b, p));
                            (b, vec![r.into()])
                        }
                    }
                }
                // 无 handler → 原有 err_ret（drop 全部槽后 return err，跨帧传播）。
                None => {
                    if self.err_ret.is_none() {
                        let b = self.bcx.create_block();
                        let p = self.bcx.func.dfg.append_block_param(b, I64);
                        self.err_ret = Some(b);
                        self.err_ret_param = Some(p);
                    }
                    (self.err_ret.unwrap(), vec![r.into()])
                }
            };
        // cont 块不设块参数：结果句柄 r 定义在前驱块，cont 单前驱即被支配，
        // 调用方后续 store_slot(*d, r) 直接复用 r（所有权转移入槽）。
        let cont = self.bcx.create_block();
        let args: Vec<BlockArg> = err_args.into_iter().map(BlockArg::from).collect();
        self.bcx.ins().brif(ok, cont, &[], on_err, &args);
        // 当前块被 brif 终止，切换到 cont 继续。
        self.bcx.switch_to_block(cont);
        self.cur_block = cont;
        self.cur_terminated = false;
    }

    /// 通用返回序列：drop 全部非返回槽，return 返回槽句柄（转移所有权）或新建 Null。
    fn emit_ret(&mut self, ret_slot: Option<usize>) {
        let drop_f = self.rt.rt_drop;
        for i in 0..self.slots.len() {
            if ret_slot == Some(i) {
                continue;
            }
            let v = self.load_slot(i);
            self.bcx.ins().call(drop_f, &[v]);
        }
        match ret_slot {
            Some(r) => {
                let v = self.load_slot(r);
                self.bcx.ins().return_(&[v]);
            }
            None => {
                let f = self.rt.rt_null;
                let n = self.call1(f, &[]);
                self.bcx.ins().return_(&[n]);
            }
        }
        self.cur_terminated = true;
    }

    /// lower 一条非控制流指令。pc = 指令在 chunk.code 中的下标（T4：错误定位）。
    fn lower_simple(
        &mut self,
        ins: &Instr,
        chunk: &Chunk,
        pc: usize,
    ) -> Result<(), ZError> {
        match ins {
            Instr::LoadK(dst, k) => {
                let v = match &chunk.consts[*k] {
                    Const::Int(x) => {
                        let f = self.rt.rt_i64;
                        let imm = self.bcx.ins().iconst(I64, *x);
                        self.call1(f, &[imm])
                    }
                    Const::Float(x) => {
                        let f = self.rt.rt_f64;
                        let imm = self.bcx.ins().iconst(I64, x.to_bits() as i64);
                        self.call1(f, &[imm])
                    }
                    Const::Bool(b) => {
                        let f = self.rt.rt_bool;
                        let imm = self.bcx.ins().iconst(I64, i64::from(*b));
                        self.call1(f, &[imm])
                    }
                    Const::Char(c) => {
                        let f = self.rt.rt_char;
                        let imm = self.bcx.ins().iconst(I64, *c as u32 as i64);
                        self.call1(f, &[imm])
                    }
                    Const::Null => {
                        let f = self.rt.rt_null;
                        self.call1(f, &[])
                    }
                    Const::Str(_) => {
                        let idx = self.const_str_idx[*k].ok_or_else(|| {
                            ZError::plain(
                                codes::INTERNAL,
                                "natgen: Const::Str 缺少字符串表索引（wrapper 未收集？）",
                                None::<String>,
                            )
                        })?;
                        let f = self.rt.rt_str;
                        let imm = self.bcx.ins().iconst(I64, idx as i64);
                        self.call1(f, &[imm])
                    }
                };
                self.store_slot(*dst, v);
            }
            Instr::LoadNull(dst) => {
                let f = self.rt.rt_null;
                let v = self.call1(f, &[]);
                self.store_slot(*dst, v);
            }
            Instr::LoadBool(dst, b) => {
                let f = self.rt.rt_bool;
                let imm = self.bcx.ins().iconst(I64, i64::from(*b));
                let v = self.call1(f, &[imm]);
                self.store_slot(*dst, v);
            }
            Instr::Move(dst, src) => {
                if dst != src {
                    let v = self.load_slot(*src);
                    let r = self.retain(v);
                    self.store_slot(*dst, r);
                }
            }
            Instr::Neg(dst, src) | Instr::Not(dst, src) => {
                let op = if matches!(ins, Instr::Neg(_, _)) { 5 } else { 6 };
                let v = self.load_slot(*src);
                let x = self.retain(v);
                let f = self.rt.rt_unop;
                let imm = self.bcx.ins().iconst(I64, op);
                let cii = self.bcx.ins().iconst(I64, i64::from(self.ci));
                let pci = self.bcx.ins().iconst(I64, pc as i64);
                let r = self.call1(f, &[imm, x, cii, pci]);
                self.check_err(r, pc);
                self.store_slot(*dst, r);
            }
            Instr::Add(d, a, b)
            | Instr::Sub(d, a, b)
            | Instr::Mul(d, a, b)
            | Instr::Div(d, a, b)
            | Instr::Mod(d, a, b) => {
                let op = match ins {
                    Instr::Add(_, _, _) => 0,
                    Instr::Sub(_, _, _) => 1,
                    Instr::Mul(_, _, _) => 2,
                    Instr::Div(_, _, _) => 3,
                    _ => 4,
                };
                let va = self.load_slot(*a);
                let vb = self.load_slot(*b);
                let x = self.retain(va);
                let y = self.retain(vb);
                let f = self.rt.rt_binop;
                let imm = self.bcx.ins().iconst(I64, op);
                let cii = self.bcx.ins().iconst(I64, i64::from(self.ci));
                let pci = self.bcx.ins().iconst(I64, pc as i64);
                let r = self.call1(f, &[imm, x, y, cii, pci]);
                self.check_err(r, pc);
                self.store_slot(*d, r);
            }
            Instr::Eq(d, a, b)
            | Instr::Ne(d, a, b)
            | Instr::Lt(d, a, b)
            | Instr::Le(d, a, b)
            | Instr::Gt(d, a, b)
            | Instr::Ge(d, a, b) => {
                let op = match ins {
                    Instr::Eq(_, _, _) => 7,
                    Instr::Ne(_, _, _) => 8,
                    Instr::Lt(_, _, _) => 9,
                    Instr::Le(_, _, _) => 10,
                    Instr::Gt(_, _, _) => 11,
                    _ => 12,
                };
                let va = self.load_slot(*a);
                let vb = self.load_slot(*b);
                let x = self.retain(va);
                let y = self.retain(vb);
                let f = self.rt.rt_binop;
                let imm = self.bcx.ins().iconst(I64, op);
                let cii = self.bcx.ins().iconst(I64, i64::from(self.ci));
                let pci = self.bcx.ins().iconst(I64, pc as i64);
                let r = self.call1(f, &[imm, x, y, cii, pci]);
                self.check_err(r, pc);
                self.store_slot(*d, r);
            }
            Instr::Call(res, func, base, n) => {
                // T5：统一走 runtime 名分发（镜像 Vm::do_call）。实参 retain 后打包进
                // param_ss 连续区（stack_addr 取基址传 runtime），所有权由 rt_call 消费
                // （callee 帧槽接管 / builtin 借用后释放）。
                for i in 0..*n {
                    let v = self.load_slot(*base + i);
                    let r = self.retain(v);
                    self.bcx
                        .ins()
                        .stack_store(I64, r, self.param_ss, 8 * i as i32);
                }
                let argptr = self.bcx.ins().stack_addr(I64, self.param_ss, Offset32::new(0));
                let nimm = self.bcx.ins().iconst(I64, *n as i64);
                let cii = self.bcx.ins().iconst(I64, i64::from(self.ci));
                let pci = self.bcx.ins().iconst(I64, pc as i64);
                // G2.5：先解析别名（镜像 do_call：resolve_alias 在局部变量查找之前）。
                // 别名指向**局部变量**（lambda 闭包 / 函数名字符串）时必须走 rt_callv，
                // 否则 runtime 的全局分发查不到该名字（报 H002 undefined function）。
                let resolved = self.resolve_alias(func);
                let target = resolved.as_deref().unwrap_or(func);
                let sidx = self
                    .func_str_idx
                    .get(target)
                    .copied()
                    .or_else(|| self.func_str_idx.get(func).copied())
                    .ok_or_else(|| {
                        ZError::plain(
                            codes::INTERNAL,
                            format!("natgen: Call 函数名 `{func}` 缺少字符串表索引"),
                            None::<String>,
                        )
                    })?;
                let s_imm = self.bcx.ins().iconst(I64, sidx as i64);
                // target 名 ∈ 当前 chunk 局部变量 → rt_callv（运行期优先按变量值分发：
                // lambda 闭包 / 函数名字符串，镜像 do_call 第一步）；否则 rt_call 纯全局分发。
                let r = if let Some(&freg) = chunk.locals.get(target) {
                    let v = self.load_slot(freg);
                    let rf = self.rt.rt_callv;
                    self.call1(rf, &[v, s_imm, argptr, nimm, cii, pci])
                } else {
                    let rf = self.rt.rt_call;
                    self.call1(rf, &[s_imm, argptr, nimm, cii, pci])
                };
                self.check_err(r, pc);
                self.store_slot(*res, r);
            }
            Instr::MakeLambda(d, ci_idx, caps) => {
                // T5：捕获句柄 retain 后打包进 cap_ss 连续区 → rt_makelambda（按值捕获）。
                for (j, (_, reg)) in caps.iter().enumerate() {
                    let v = self.load_slot(*reg);
                    let r = self.retain(v);
                    self.bcx
                        .ins()
                        .stack_store(I64, r, self.cap_ss, 8 * j as i32);
                }
                let capsptr = self.bcx.ins().stack_addr(I64, self.cap_ss, Offset32::new(0));
                let cimm = self.bcx.ins().iconst(I64, *ci_idx as i64);
                let ncimm = self.bcx.ins().iconst(I64, caps.len() as i64);
                let rf = self.rt.rt_makelambda;
                let r = self.call1(rf, &[cimm, capsptr, ncimm]);
                self.store_slot(*d, r);
            }
            // ── M1：list/dict 承载（G1.2/G1.3）+ for-in 迭代（G1.4） ──
            // 语义逐条镜像 vm.rs 执行臂；所有权协议见 runtime 文件头注释：
            // 读操作（rt_index/rt_destructget/rt_len/rt_keys）借用、返回新句柄；
            // 写操作（rt_indexset）返回「新槽值句柄」，调用方 store_slot 无条件替换。
            Instr::IsNull(d, s) => {
                let v = self.load_slot(*s);
                let r0 = self.call1(self.rt.rt_isnull, &[v]);
                let r = self.call1(self.rt.rt_bool, &[r0]);
                self.store_slot(*d, r);
            }
            Instr::IsDict(d, s) => {
                let v = self.load_slot(*s);
                let r0 = self.call1(self.rt.rt_isdict, &[v]);
                let r = self.call1(self.rt.rt_bool, &[r0]);
                self.store_slot(*d, r);
            }
            // G2.3：条件严格 bool 校验（镜像 VM CondCheck 执行臂；消息按站点）。
            Instr::CondCheck(r, kind) => {
                let v = self.load_slot(*r);
                let k = self.bcx.ins().iconst(I64, i64::from(*kind));
                let cii = self.bcx.ins().iconst(I64, i64::from(self.ci));
                let pci = self.bcx.ins().iconst(I64, pc as i64);
                let e = self.call1(self.rt.rt_condcheck, &[v, k, cii, pci]);
                self.check_err(e, pc);
                // 通过则 e 为 null 句柄（无 dest），显式释放（同 IterCheck 口径）。
                self.bcx.ins().call(self.rt.rt_drop, &[e]);
            }
            // G2.3：&&/|| 短路判定（值恰为 Bool(want)，无错误路径）。
            Instr::IsBoolVal(d, s, want) => {
                let v = self.load_slot(*s);
                let w = self.bcx.ins().iconst(I64, i64::from(*want));
                let r = self.call1(self.rt.rt_isboolval, &[v, w]);
                self.store_slot(*d, r);
            }
            Instr::IterCheck(s, is_comp) => {
                let v = self.load_slot(*s);
                let ic = self.bcx.ins().iconst(I64, i64::from(*is_comp));
                let cii = self.bcx.ins().iconst(I64, i64::from(self.ci));
                let pci = self.bcx.ins().iconst(I64, pc as i64);
                let r = self.call1(self.rt.rt_itercheck, &[v, ic, cii, pci]);
                self.check_err(r, pc);
                // 通过则 r 为 null 句柄（无 dest），显式释放。
                self.bcx.ins().call(self.rt.rt_drop, &[r]);
            }
            Instr::Index(d, o, k) => {
                let vo = self.load_slot(*o);
                let vk = self.load_slot(*k);
                let cii = self.bcx.ins().iconst(I64, i64::from(self.ci));
                let pci = self.bcx.ins().iconst(I64, pc as i64);
                let r = self.call1(self.rt.rt_index, &[vo, vk, cii, pci]);
                self.check_err(r, pc);
                self.store_slot(*d, r);
            }
            Instr::IndexSet(o, k, v) => {
                let vo = self.load_slot(*o);
                let vk = self.load_slot(*k);
                let vv = self.load_slot(*v);
                let cii = self.bcx.ins().iconst(I64, i64::from(self.ci));
                let pci = self.bcx.ins().iconst(I64, pc as i64);
                let r = self.call1(self.rt.rt_indexset, &[vo, vk, vv, cii, pci]);
                self.check_err(r, pc);
                // 无条件替换协议：正常路径把槽 o 换为 cow 后的「新槽值句柄」。
                self.store_slot(*o, r);
            }
            Instr::DestructGet(d, o, k, nvars, is_dict) => {
                let vo = self.load_slot(*o);
                let vk = self.load_slot(*k);
                let nimm = self.bcx.ins().iconst(I64, *nvars as i64);
                let dimm = self.bcx.ins().iconst(I64, i64::from(*is_dict));
                let cii = self.bcx.ins().iconst(I64, i64::from(self.ci));
                let pci = self.bcx.ins().iconst(I64, pc as i64);
                let r = self.call1(self.rt.rt_destructget, &[vo, vk, nimm, dimm, cii, pci]);
                self.check_err(r, pc);
                self.store_slot(*d, r);
            }
            Instr::Len(d, s) => {
                let v = self.load_slot(*s);
                let cii = self.bcx.ins().iconst(I64, i64::from(self.ci));
                let pci = self.bcx.ins().iconst(I64, pc as i64);
                let r = self.call1(self.rt.rt_len, &[v, cii, pci]);
                self.check_err(r, pc);
                self.store_slot(*d, r);
            }
            Instr::Keys(d, s) => {
                let v = self.load_slot(*s);
                let cii = self.bcx.ins().iconst(I64, i64::from(self.ci));
                let pci = self.bcx.ins().iconst(I64, pc as i64);
                let r = self.call1(self.rt.rt_keys, &[v, cii, pci]);
                self.check_err(r, pc);
                self.store_slot(*d, r);
            }
            Instr::NewList(d, base, n) => {
                // 元素句柄（借用）打包进 param_ss 连续区；rt_newlist 内部 retain 建 cell。
                for i in 0..*n {
                    let v = self.load_slot(*base + i);
                    self.bcx
                        .ins()
                        .stack_store(I64, v, self.param_ss, 8 * i as i32);
                }
                let argptr = self.bcx.ins().stack_addr(I64, self.param_ss, Offset32::new(0));
                let nimm = self.bcx.ins().iconst(I64, *n as i64);
                let cii = self.bcx.ins().iconst(I64, i64::from(self.ci));
                let pci = self.bcx.ins().iconst(I64, pc as i64);
                let r = self.call1(self.rt.rt_newlist, &[argptr, nimm, cii, pci]);
                self.store_slot(*d, r);
            }
            Instr::NewDict(d, base, n) => {
                // 成对 (key, val) 句柄（借用）打包进 param_ss 连续区；rt_newdict 内部转键 + retain。
                for i in 0..*n * 2 {
                    let v = self.load_slot(*base + i);
                    self.bcx
                        .ins()
                        .stack_store(I64, v, self.param_ss, 8 * i as i32);
                }
                let argptr = self.bcx.ins().stack_addr(I64, self.param_ss, Offset32::new(0));
                let nimm = self.bcx.ins().iconst(I64, *n as i64);
                let cii = self.bcx.ins().iconst(I64, i64::from(self.ci));
                let pci = self.bcx.ins().iconst(I64, pc as i64);
                let r = self.call1(self.rt.rt_newdict, &[argptr, nimm, cii, pci]);
                self.store_slot(*d, r);
            }
            // T6：throw —— rt_throw 归一（str→H600 / error 重抛 / 其余→H001），
            // 归一结果经 check_err 路由（有 handler→catch 槽；无→跨帧传播）。
            Instr::Throw(reg) => {
                // 被抛值：retain 出独立引用交给归一器（镜像 VM 的 clone：槽保留原引用、
                // 帧末统一释放；错误句柄引用经 catch 槽 / err_ret 返回转移）。
                let v = self.load_slot(*reg);
                let x = self.retain(v);
                let cii = self.bcx.ins().iconst(I64, i64::from(self.ci));
                let pci = self.bcx.ins().iconst(I64, pc as i64);
                let rf = self.rt.rt_throw;
                let r = self.call1(rf, &[x, cii, pci]);
                self.check_err(r, pc);
            }
            // T6：字面字符串 throw（`throw "msg"` 形态）——rt_throwstr → H600。
            Instr::ThrowStr(s) => {
                let sidx = self
                    .field_str_idx
                    .get(s)
                    .copied()
                    .ok_or_else(|| {
                        ZError::plain(
                            codes::INTERNAL,
                            format!("natgen: ThrowStr 文案 `{s}` 缺少字符串表索引"),
                            None::<String>,
                        )
                    })?;
                let s_imm = self.bcx.ins().iconst(I64, sidx as i64);
                let cii = self.bcx.ins().iconst(I64, i64::from(self.ci));
                let pci = self.bcx.ins().iconst(I64, pc as i64);
                let rf = self.rt.rt_throwstr;
                let r = self.call1(rf, &[s_imm, cii, pci]);
                self.check_err(r, pc);
            }
            // ── M1：Raise(code, msg, help) —— 运行期直接产生错误（VM 与解释器同源报错的 lowering）。
            // 三段文本（错误码 / 消息 / help）取 strtab 索引传runtime；help_sidx = -1
            // 表示「本分支无 help 行」，runtime 据此不渲染 help（镜像解释器 help=None）。
            Instr::Raise(code, msg, help) => {
                let mut lookup = |s: &str| -> Result<i64, ZError> {
                    self.field_str_idx
                        .get(s)
                        .map(|&i| i as i64)
                        .ok_or_else(|| {
                            ZError::plain(
                                codes::INTERNAL,
                                format!("natgen: Raise 文本 `{s}` 缺少字符串表索引"),
                                None::<String>,
                            )
                        })
                };
                let c_imm = self.bcx.ins().iconst(I64, lookup(code)?);
                let m_imm = self.bcx.ins().iconst(I64, lookup(msg)?);
                let h_imm = match help {
                    Some(h) => self.bcx.ins().iconst(I64, lookup(h)?),
                    // 哨兵 = u64::MAX（runtime 侧 HELP_NONE_SIDX）：本分支无 help 行。
                    None => self.bcx.ins().iconst(I64, -1i64 as u64 as i64),
                };
                let cii = self.bcx.ins().iconst(I64, i64::from(self.ci));
                let pci = self.bcx.ins().iconst(I64, pc as i64);
                let r = self.call1(self.rt.rt_raise, &[c_imm, m_imm, h_imm, cii, pci]);
                // Raise 产出的必然是错误句柄：直接进错误路由（有 handler→catch 槽，
                // 无→ 跨帧传播），随后无正常路径。
                self.check_err(r, pc);
            }
            // T6：错误对象字段访问（e.code/.message/.file/.line/.col/.context）——
            // rt_field 消费 obj 引用、返回新句柄；出错（unknown field / 非 error 值）经 check_err 路由。
            Instr::Field(d, o, field) => {
                let fsidx = self
                    .field_str_idx
                    .get(field)
                    .copied()
                    .ok_or_else(|| {
                        ZError::plain(
                            codes::INTERNAL,
                            format!("natgen: Field 字段名 `{field}` 缺少字符串表索引"),
                            None::<String>,
                        )
                    })?;
                let v = self.load_slot(*o);
                let x = self.retain(v);
                let f_imm = self.bcx.ins().iconst(I64, fsidx as i64);
                let cii = self.bcx.ins().iconst(I64, i64::from(self.ci));
                let pci = self.bcx.ins().iconst(I64, pc as i64);
                let rf = self.rt.rt_field;
                let r = self.call1(rf, &[x, f_imm, cii, pci]);
                self.check_err(r, pc);
                self.store_slot(*d, r);
            }
            // G2.2：enum 三指令（语义镜像 vm.rs 执行臂 2058–2085）。
            // 枚举名 / 变体名 → strtab 索引复用 field_str_idx（strtab 全集反查；
            // main.rs 已把 enum_defs 的名字收进 strtab，同 Field 字段名口径）。
            Instr::NewEnum(d, ename, variant, pbase, np) => {
                // 载荷区打包（同 Call 约定：retain 入 param_ss，runtime 消费并释放）。
                for i in 0..*np {
                    let v = self.load_slot(*pbase + i);
                    let r = self.retain(v);
                    self.bcx
                        .ins()
                        .stack_store(I64, r, self.param_ss, 8 * i as i32);
                }
                let argptr = self.bcx.ins().stack_addr(I64, self.param_ss, Offset32::new(0));
                let n_imm = self.bcx.ins().iconst(I64, *np as i64);
                let ni = self.enum_str_idx(ename, "枚举名")? as i64;
                let vi = self.enum_str_idx(variant, "变体名")? as i64;
                let n_sidx = self.bcx.ins().iconst(I64, ni);
                let v_sidx = self.bcx.ins().iconst(I64, vi);
                let r = self.call1(self.rt.rt_newenum, &[n_sidx, v_sidx, argptr, n_imm]);
                self.store_slot(*d, r);
            }
            Instr::EnumElem(d, src, idx) => {
                // 借用 src（槽保留），返回载荷新句柄；越界/非枚举 → 错误路由。
                let v = self.load_slot(*src);
                let x = self.retain(v);
                let i_imm = self.bcx.ins().iconst(I64, *idx as i64);
                let cii = self.bcx.ins().iconst(I64, i64::from(self.ci));
                let pci = self.bcx.ins().iconst(I64, pc as i64);
                let r = self.call1(self.rt.rt_enumelem, &[x, i_imm, cii, pci]);
                self.check_err(r, pc);
                self.store_slot(*d, r);
            }
            Instr::IsEnumVariant(d, src, ename, variant) => {
                // 借用 src，恒返回 bool（无错误路径）。
                let v = self.load_slot(*src);
                let x = self.retain(v);
                let ni = self.enum_str_idx(ename, "枚举名")? as i64;
                let vi = self.enum_str_idx(variant, "变体名")? as i64;
                let n_sidx = self.bcx.ins().iconst(I64, ni);
                let v_sidx = self.bcx.ins().iconst(I64, vi);
                let r = self.call1(self.rt.rt_isenumvariant, &[x, n_sidx, v_sidx]);
                self.store_slot(*d, r);
            }
            // T6：TryBegin / TryPop 为 lowering 期元指令——catch 路由全在 check_err 的
            // catch_map 查询处，运行时不留痕迹（无 setjmp），故二者 lower 为空操作。
            Instr::TryBegin(_, _) | Instr::TryPop => {}
            other => {
                // G2.7：能走到这里的只有「已登记未实现」的指令（见下方 mod instr_coverage 的
                // NOT_IMPLEMENTED 表，单测保证无静默漏实现）。错误码用 H999（功能未实现）
                // 而非 H990（internal）——与 VM / AOT / DLL 的能力边界口径一致：
                // 「这个引擎不支持该特性」不是「编译器内部错误」。
                return Err(ZError::plain(
                    codes::NOT_IMPLEMENTED,
                    format!(
                        "原生编译后端暂不支持 `{}`（见 M2 G2.6 async-await）",
                        instr_name(&other)
                    ),
                    Some("用解释器运行同一脚本：`hone run <file.hn>`（输出与退出码与本后端一致）"),
                ));
            }
        }
        Ok(())
    }
}

/// T6：预计算每个 pc 的「最近活跃 handler」。
/// 线性扫描已 resolve 的 chunk.code：TryBegin 压栈 (handler_pc, catch_reg)，
/// TryPop 出栈；pc 处取栈顶（嵌套取最内层）。与 VM 帧 try_stack 语义一致：
/// catch 只发生在建立 try 的帧（跨帧 ERR 由「函数返回 ERR、调用方 check_err」传播）。
fn compute_catch_map(code: &[Instr]) -> Vec<Option<(usize, usize)>> {
    let mut map: Vec<Option<(usize, usize)>> = Vec::with_capacity(code.len());
    let mut stack: Vec<(usize, usize)> = Vec::new();
    for ins in code.iter() {
        if let Instr::TryBegin(h, c) = ins {
            stack.push((*h, *c));
        }
        if let Instr::TryPop = ins {
            stack.pop();
        }
        map.push(stack.last().copied());
    }
    map
}

/// 指令变体名（用于能力边界的报错文案；G2.7 要求报错指名指令，而不是 `{other:?}` 整串）。
fn instr_name(ins: &Instr) -> &'static str {
    match ins {
        Instr::GoCall(_, _, _) => "go",
        Instr::Await(_, _) => "await",
        Instr::Breakpoint => "breakpoint",
        Instr::DebugPrint(_) => "debug_print",
        Instr::Nop => "nop",
        other => {
            // 兜底仅供「新增指令忘了加名字」时也能编过：单测 instr_coverage 会同时失败，
            // 提醒在 NOT_IMPLEMENTED 与此处补登记。
            let s = format!("{other:?}");
            Box::leak(s.into_boxed_str())
        }
    }
}

/// 把单个 chunk lower 为 Cranelift 函数。
/// 符号名：main chunk → `__hn_main`（runtime hn_entry 硬编码引用），其余 → `hn_f{ci}`。
fn lower_chunk(
    bcx: &mut FunctionBuilder,
    module: &mut ObjectModule,
    chunk: &Chunk,
    rt_imports: &RtImports,
    const_str_idx: &[Option<usize>],
    func_str_idx: &HashMap<String, usize>,
    field_str_idx: &HashMap<String, usize>,
    alias_map: &HashMap<String, String>,
    catch_map: Vec<Option<(usize, usize)>>,
    ci: usize,
) -> Result<(), ZError> {
    let nregs = chunk.nregs;
    let nparams = chunk.params.len();
    let ncap = chunk.captured_regs.len();
    let code = &chunk.code;

    // 1. leader 切分。
    let mut leaders: BTreeSet<usize> = BTreeSet::new();
    leaders.insert(0);
    for (i, ins) in code.iter().enumerate() {
        match ins {
            Instr::Jmp(t) => {
                leaders.insert(*t);
            }
            Instr::JmpIfFalse(_, t) | Instr::JmpIfTrue(_, t) => {
                leaders.insert(*t);
                if i + 1 < code.len() {
                    leaders.insert(i + 1);
                }
            }
            Instr::Label(_) => {
                leaders.insert(i);
            }
            _ => {}
        }
    }

    // 2. BFS 可达性（段图：terminator 后继 + fall-through 后继）。
    let mut reachable: HashSet<usize> = HashSet::new();
    let mut work = vec![0usize];
    // T6：handler_pc 经 catch 块可达，而 BFS 只跟显式跳转边——把每个 handler 作为
    // BFS 起点补入，否则 handler 段不被 lower、catch 块跳向 pristine 块致验证失败。
    for e in &catch_map {
        if let Some((h, _)) = e {
            work.push(*h);
        }
    }
    let next_leader = |pc: usize| leaders.range(pc + 1..).next().copied();
    while let Some(pc) = work.pop() {
        if !reachable.insert(pc) {
            continue;
        }
        let end = next_leader(pc).unwrap_or(code.len());
        let mut has_terminator = false;
        for i in pc..end {
            match &code[i] {
                Instr::Jmp(t) => {
                    work.push(*t);
                    has_terminator = true;
                }
                Instr::JmpIfFalse(_, t) | Instr::JmpIfTrue(_, t) => {
                    work.push(*t);
                    if i + 1 < code.len() {
                        work.push(i + 1);
                    }
                    has_terminator = true;
                }
                Instr::Ret(_) | Instr::RetNull => {
                    has_terminator = true;
                }
                _ => {}
            }
        }
        if !has_terminator {
            if let Some(np) = next_leader(pc) {
                work.push(np);
            }
        }
    }

    // 3. 块创建 + 入口。
    let mut blocks: HashMap<usize, ClifBlock> = HashMap::new();
    for &pc in &leaders {
        blocks.insert(pc, bcx.create_block());
    }
    let entry = blocks[&0];
    bcx.switch_to_block(entry);
    // 栈槽：每 VM 寄存器一个 8B ExplicitSlot（入口初始化，递归经独立栈帧）+ 实参
    // 打包区（Call/NewList/NewDict 前连续 store，T5/M1）＋ 捕获打包区（MakeLambda，T5）。
    let max_pack = code
        .iter()
        .filter_map(|i| match i {
            Instr::Call(_, _, _, n) => Some(*n),
            Instr::NewList(_, _, n) => Some(*n),
            Instr::NewDict(_, _, n) => Some(*n * 2),
            Instr::NewEnum(_, _, _, _, n) => Some(*n),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let max_caps = code
        .iter()
        .filter_map(|i| match i {
            Instr::MakeLambda(_, _, caps) => Some(caps.len()),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let mut slots: Vec<StackSlot> = Vec::with_capacity(nregs);
    for _ in 0..nregs {
        slots.push(bcx.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            8,
            3,
        )));
    }
    let param_ss = bcx.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        (8 * max_pack.max(1)) as u32,
        3,
    ));
    let cap_ss = bcx.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        (8 * max_caps.max(1)) as u32,
        3,
    ));
    let rt = rt_imports.refs(module, &mut bcx.func);
    // 入口（T5 统一签名 argptr/nargs/capsptr/ncap）：无块参数对应形参——实参与捕获
    // 从 argptr/capsptr 内存读入（调用方打包的连续句柄数组），镜像 Vm 帧填充：
    // 参数按 params 名 → locals 寄存器号（与 VM call_chunk/call_lambda 一致），
    // 捕获按 captured_regs 元组寄存器号；其余寄存器 rt_null 初始化。
    let argptr = bcx.func.dfg.append_block_param(entry, I64);
    let _nargs = bcx.func.dfg.append_block_param(entry, I64);
    let capsptr = bcx.func.dfg.append_block_param(entry, I64);
    let _ncap = bcx.func.dfg.append_block_param(entry, I64);
    // load 的 flags 参数为 MemFlagsData（trusted = 无 trap：实参/捕获区由调用方
    // 打包、恒可达；builder 会负责注册进 dfg.mem_flags 集合）。
    let mflags = MemFlagsData::trusted();
    for i in 0..nregs {
        let v: Value = if let Some(j) = (0..nparams).find(|&j| {
            chunk.locals.get(&chunk.params[j]).copied() == Some(i)
        }) {
            bcx.ins().load(I64, mflags, argptr, Offset32::new(8 * j as i32))
        } else if let Some(j) = (0..ncap).find(|&j| chunk.captured_regs[j].1 == i) {
            bcx.ins().load(I64, mflags, capsptr, Offset32::new(8 * j as i32))
        } else {
            let inst = bcx.ins().call(rt.rt_null, &[]);
            bcx.func.dfg.first_result(inst)
        };
        bcx.ins().stack_store(I64, v, slots[i], 0);
    }

    // 4. 逐段 lower（只填充可达段；不可达段保持 pristine）。
    let mut lc = ChunkLower {
        bcx: &mut *bcx,
        slots,
        blocks,
        reachable,
        err_ret: None,
        err_ret_param: None,
        cur_block: entry,
        cur_terminated: false,
        rt,
        const_str_idx: const_str_idx.to_vec(),
        ci: ci as u32,
        func_str_idx: func_str_idx.clone(),
        param_ss,
        cap_ss,
        catch_map,
        catch_blocks: Vec::new(),
        field_str_idx: field_str_idx.clone(),
        alias_map: alias_map.clone(),
    };

    let leaders_list: Vec<usize> = leaders.iter().copied().collect();
    for (si, &pc) in leaders_list.iter().enumerate() {
        if !lc.reachable.contains(&pc) {
            continue;
        }
        let end = leaders_list.get(si + 1).copied().unwrap_or(code.len());
        // 进入新段：若当前块已被终止或不是本段块，切块。
        let blk = lc.blocks[&pc];
        if lc.cur_terminated || lc.cur_block != blk {
            lc.bcx.switch_to_block(blk);
            lc.cur_block = blk;
            lc.cur_terminated = false;
        }
        let mut has_term = false;
        for i in pc..end {
            let ins = &code[i];
            match ins {
                Instr::Jmp(t) => {
                    let dest = lc.blocks[t];
                    lc.bcx.ins().jump(dest, &[]);
                    lc.cur_terminated = true;
                    has_term = true;
                    break;
                }
                Instr::JmpIfFalse(c, t) => {
                    // 语义：条件为假 → 跳 t；真 → 落入 pc+1（fall-through）。
                    let tf = lc.rt.rt_truthy;
                    let v = lc.load_slot(*c);
                    let tv = lc.call1(tf, &[v]);
                    let is_false = lc.bcx.ins().icmp_imm_s(IntCC::Equal, tv, 0);
                    let dest = lc.blocks[t];
                    if i + 1 < code.len() {
                        let next = lc.blocks[&(i + 1)];
                        lc.bcx.ins().brif(is_false, dest, &[], next, &[]);
                    } else {
                        // 条件跳转在段尾且无 fall-through：false → 掉出函数末尾返回 Null。
                        let cont = lc.bcx.create_block();
                        lc.bcx.ins().brif(is_false, cont, &[], dest, &[]);
                        lc.bcx.switch_to_block(cont);
                        lc.cur_block = cont;
                        lc.cur_terminated = false;
                    }
                    lc.cur_terminated = true;
                    has_term = true;
                    break;
                }
                Instr::JmpIfTrue(c, t) => {
                    let tf = lc.rt.rt_truthy;
                    let v = lc.load_slot(*c);
                    let tv = lc.call1(tf, &[v]);
                    let is_true = lc.bcx.ins().icmp_imm_s(IntCC::NotEqual, tv, 0);
                    let dest = lc.blocks[t];
                    if i + 1 < code.len() {
                        let next = lc.blocks[&(i + 1)];
                        lc.bcx.ins().brif(is_true, dest, &[], next, &[]);
                    } else {
                        let cont = lc.bcx.create_block();
                        lc.bcx.ins().brif(is_true, dest, &[], cont, &[]);
                        lc.bcx.switch_to_block(cont);
                        lc.cur_block = cont;
                        lc.cur_terminated = false;
                    }
                    lc.cur_terminated = true;
                    has_term = true;
                    break;
                }
                Instr::Ret(r) => {
                    lc.emit_ret(Some(*r));
                    has_term = true;
                    break;
                }
                Instr::RetNull => {
                    lc.emit_ret(None);
                    has_term = true;
                    break;
                }
                Instr::Label(_) => {}
                other => lc.lower_simple(other, chunk, i)?,
            }
        }
        if !has_term {
            // 段尾无 terminator：fall-through 到下一可达段，或函数末尾返回 Null。
            match leaders_list.get(si + 1) {
                Some(&next_pc) => {
                    if lc.reachable.contains(&next_pc) {
                        let dest = lc.blocks[&next_pc];
                        lc.bcx.ins().jump(dest, &[]);
                        lc.cur_terminated = true;
                    } else {
                        lc.emit_ret(None);
                    }
                }
                None => lc.emit_ret(None),
            }
        }
    }

    // 5a. T6 catch 块（懒建）：err 句柄（块参数）→ catch_reg 槽 → 跳 handler 块。
    //     槽是内存：多前驱 catch 块各自 block-param 取值后 stack_store，无支配问题。
    //     handler 块本身是既有 leader 块（TryBegin 的 handler_pc），此处不创建。
    // 先克隆（块/参数不可变），循环内对 lc 可变借用调 store_slot 无冲突。
    let catch_blocks = lc.catch_blocks.clone();
    for (key, blk, param) in &catch_blocks {
        lc.bcx.switch_to_block(*blk);
        lc.cur_block = *blk;
        // store_slot：先 drop 槽内旧值再存（镜像 VM regs[catch_reg] = thrown.clone()）。
        lc.store_slot(key.1, *param);
        let handler = lc.blocks[&key.0];
        lc.bcx.ins().jump(handler, &[]);
        lc.cur_terminated = true;
    }

    // 5. 统一错误返回块（所有 fallible 指令的 ERR 终点）：drop 全部槽后 return err。
    //    槽是内存：本块（多前驱）在自己块内 stack_load 再 drop，无支配问题。
    if let (Some(blk), Some(param)) = (lc.err_ret, lc.err_ret_param) {
        lc.bcx.switch_to_block(blk);
        lc.cur_block = blk;
        let drop_f = lc.rt.rt_drop;
        for i in 0..lc.slots.len() {
            let v = lc.load_slot(i);
            lc.bcx.ins().call(drop_f, &[v]);
        }
        lc.bcx.ins().return_(&[param]);
    }

    // 6. 统一 seal（循环回跳目标延迟 seal 至此）。
    lc.bcx.seal_all_blocks();
    Ok(())
}

/// 生成 COFF object（含全部用户函数与 __hn_main 入口）。
/// strtab 为 wrapper 侧承载的字符串静态表（全部 Const::Str 去重，M3 移入 object rodata）。
pub fn gen_object(m: &Module, strtab: &[String], out: &Path) -> Result<(), ZError> {
    let internal = |msg: String| ZError::plain(codes::INTERNAL, msg, None::<String>);

    let main_ci = m
        .func_map
        .get("main")
        .copied()
        .ok_or_else(|| ZError::plain(codes::INTERNAL, "natgen: 模块缺少 main chunk", None::<String>))?;

    let triple = target_lexicon::Triple::from_str("x86_64-pc-windows-gnu")
        .map_err(|e| internal(format!("natgen: 目标三元组解析失败: {e}")))?;
    let mut flag_builder = builder();
    flag_builder
        .set("opt_level", "speed")
        .map_err(|e| internal(format!("natgen: 设置 opt_level 失败: {e}")))?;
    let flags = cranelift_codegen::settings::Flags::new(flag_builder);
    let isa = cranelift_codegen::isa::lookup(triple)
        .map_err(|e| internal(format!("natgen: ISA 查找失败: {e}")))?
        .finish(flags)
        .map_err(|e| internal(format!("natgen: ISA 初始化失败: {e}")))?;

    let mut module = ObjectModule::new(
        ObjectBuilder::new(isa, "script", default_libcall_names())
            .map_err(|e| internal(format!("natgen: ObjectModule 创建失败: {e}")))?,
    );

    let rt_imports = RtImports::declare(&mut module)?;

    // 声明全部用户 chunk 为导出函数（T5 统一签名：argptr/nargs/capsptr/ncap → 返回值句柄）。
    let cc = module.isa().default_call_conv();
    let mut fn_ids: Vec<FuncId> = Vec::with_capacity(m.chunks.len());
    for ci in 0..m.chunks.len() {
        let mut sig = Signature::new(cc);
        for _ in 0..4 {
            sig.params.push(AbiParam::new(I64));
        }
        sig.returns.push(AbiParam::new(I64));
        let name = if ci == main_ci {
            "__hn_main".to_string()
        } else {
            format!("hn_f{ci}")
        };
        let fid = module
            .declare_function(&name, Linkage::Export, &sig)
            .map_err(|e| internal(format!("natgen: 声明函数 `{name}` 失败: {e}")))?;
        fn_ids.push(fid);
    }

    // 每 chunk 的常量串 → strtab 索引映射（const_idx → strtab idx；非串为 None）。
    let strtab_pos: HashMap<&str, usize> =
        strtab.iter().enumerate().map(|(i, s)| (s.as_str(), i)).collect();
    let const_str_idx: Vec<Vec<Option<usize>>> = m
        .chunks
        .iter()
        .map(|c| {
            c.consts
                .iter()
                .map(|k| match k {
                    Const::Str(s) => strtab_pos.get(s.as_str()).copied(),
                    _ => None,
                })
                .collect()
        })
        .collect();
    // T5：Call 指令函数名 → strtab 索引（main.rs 收集 Call 名进 strtab，这里反查）。
    let func_str_idx: HashMap<String, usize> = strtab
        .iter()
        .enumerate()
        .map(|(i, s)| (s.clone(), i))
        .collect();
    // G2.5：别名表反向成「新名 → 原名」（Module.aliases 存的是 (original, new_name)），
    // 供 Call lowering 编译期解析（决定 rt_callv / rt_call 与错误文案里的名字）。
    let alias_map: HashMap<String, String> = m
        .aliases
        .iter()
        .map(|(orig, new_name)| (new_name.clone(), orig.clone()))
        .collect();

    // 逐 chunk define。
    for (ci, chunk) in m.chunks.iter().enumerate() {
        let fid = fn_ids[ci];
        let mut sig = Signature::new(cc);
        for _ in 0..4 {
            sig.params.push(AbiParam::new(I64));
        }
        sig.returns.push(AbiParam::new(I64));
        let mut ctx = Context::new();
        ctx.func = Function::with_name_signature(
            cranelift_codegen::ir::UserFuncName::user(0, fid.as_u32()),
            sig,
        );
        let mut fbcx = FunctionBuilderContext::new();
        let mut bcx = FunctionBuilder::new(&mut ctx.func, &mut fbcx);
        lower_chunk(
            &mut bcx,
            &mut module,
            chunk,
            &rt_imports,
            &const_str_idx[ci],
            &func_str_idx,
            // field_str_idx 复用 func_str_idx（strtab 全集反查；Field 字段名 / ThrowStr 文案
            // 已由 main.rs 收进 strtab）。
            &func_str_idx,
            &alias_map,
            compute_catch_map(&chunk.code),
            ci,
        )?;
        bcx.finalize(module.isa().frontend_config());
        module
            .define_function(fid, &mut ctx)
            .map_err(|e| internal(format!("natgen: 定义函数 `hn_f{ci}` 失败: {e:?}")))?;
    }

    let prod = module.finish();
    let bytes = prod
        .emit()
        .map_err(|e| internal(format!("natgen: object 生成失败: {e}")))?;
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| internal(format!("natgen: 创建输出目录失败: {e}")))?;
        }
    }
    std::fs::write(out, &bytes).map_err(|e| internal(format!("natgen: 写入 object 失败: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod instr_coverage {
    //! G2.7：指令覆盖铁律——**每个 `vm::Instr` 变体都必须在 natgen 里显式出现**
    //! （有 lowering 臂、或在 `NOT_IMPLEMENTED` 里带理由登记），不允许静默落到兜底 H990。
    //!
    //! 为什么用「读源码」的方式测：lowering 的分派是 `match ins { Instr::X => … }`，
    //! Rust 没有反射可查「哪些变体有臂」。直接对 `src/natgen.rs` 与 `src/vm.rs` 做文本提取，
    //! 能机械地回答「新增指令是否忘了决策」——这正是 G2.7 要消灭的那类静默缺口。

    /// 已登记为「原生后端不支持」的指令变体 → 未实现原因（会出现在编译期 H990 里）。
    /// 新增条目必须写清原因与所属任务号，便于回头消账。
    const NOT_IMPLEMENTED: &[(&str, &str)] = &[
        ("GoCall", "G2.6 async-await：需要线程 spawn + future 值（runtime 的 rc 非原子，跨线程共享值不安全）"),
        ("Await", "G2.6 async-await：需要阻塞等待 future（依赖 GoCall 的线程基础设施）"),
        ("Breakpoint", "解释器调试指令（`hone debug` 断点），原生后端无调试器"),
        ("DebugPrint", "解释器调试指令（debug_print），原生后端无调试器"),
        ("Nop", "占位指令（无副作用），lowering 无需处理即可；列此仅为覆盖声明完整"),
    ];

    fn read(rel: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("读 {rel} 失败: {e}"))
    }

    /// 提取 `pub enum Instr { ... }` 里的变体名（形如 `Name(` 或 `Name,`）。
    fn instr_variants(vm_src: &str) -> Vec<String> {
        let start = vm_src
            .find("pub enum Instr {")
            .expect("vm.rs 里找不到 `pub enum Instr`");
        let body = &vm_src[start..];
        let end = body.find("\n}").expect("Instr 枚举未闭合");
        body[..end]
            .lines()
            .filter_map(|l| {
                let t = l.trim();
                // 形如 `Move(usize, usize),` / `LoadBool(usize, bool),` / `RetNull,`
                let name: String = t.chars().take_while(|c| c.is_alphanumeric()).collect();
                if name.is_empty() || !name.chars().next().unwrap().is_uppercase() {
                    return None;
                }
                let rest = t[name.len()..].trim_start();
                // 变体行必须以 `(` 或 `,` 开头；文档注释行以 `///` 开头（已被过滤）
                if rest.starts_with('(') || rest.starts_with(',') {
                    Some(name)
                } else {
                    None
                }
            })
            .collect()
    }

    /// 提取 natgen 源码里出现的所有 `Instr::Xxx` 变体引用。
    fn natgen_variants(nat_src: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut i = 0usize;
        while i < nat_src.len() {
            let p = match nat_src[i..].find("Instr::") {
                Some(p) => p,
                None => break,
            };
            let at = i + p + "Instr::".len();
            let name: String = nat_src[at..]
                .chars()
                .take_while(|c| c.is_alphanumeric())
                .collect();
            if !name.is_empty() {
                let consumed = name.len();
                if !out.contains(&name) {
                    out.push(name);
                }
                i = at + consumed;
            } else {
                i = at;
            }
        }
        out
    }

    #[test]
    fn every_instr_has_explicit_decision() {
        let vm_src = read("src/vm.rs");
        let nat_src = read("src/natgen.rs");
        let variants = instr_variants(&vm_src);
        assert!(
            variants.len() > 40,
            "Instr 变体提取疑似失效，只拿到 {} 个",
            variants.len()
        );
        // 「已决策」= 在 natgen 里有显式引用（lowering 臂 / 块级处理 / catch_map 元指令）
        //        ∪ 在 NOT_IMPLEMENTED 里带理由登记。
        let mut decided = natgen_variants(&nat_src);
        for (name, _) in NOT_IMPLEMENTED {
            decided.push(name.to_string());
        }
        let missing: Vec<&String> = variants.iter().filter(|v| !decided.contains(v)).collect();
        assert!(
            missing.is_empty(),
            "以下 Instr 变体在 natgen.rs 里既无 lowering 臂、也不在 NOT_IMPLEMENTED 登记中：\n  {:?}\n\
             （要么实现 lowering，要么在 NOT_IMPLEMENTED 里带理由登记，否则会静默落到 H990）",
            missing
        );
    }

    #[test]
    fn not_implemented_entries_are_real_variants() {
        let vm_src = read("src/vm.rs");
        let variants = instr_variants(&vm_src);
        for (name, reason) in NOT_IMPLEMENTED {
            assert!(
                variants.iter().any(|v| v == name),
                "NOT_IMPLEMENTED 里的 `{}` 已不是 Instr 变体（拼写变了？）",
                name
            );
            assert!(!reason.trim().is_empty(), "`{}` 的未实现原因为空", name);
        }
    }
}
