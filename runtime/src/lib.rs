// Hone 原生编译后端（natgen）的链接运行时。
// 单文件、std-only。双轨使用：
// 1. workspace 成员（cargo test -p honeruntime，开发期单测）；
// 2. 经 include_str! 内联进 hone 二进制，hone natgen 时写出临时目录，
//    与生成式 wrapper 一起被 rustc 编译链接进产物（干净构建机自足，无需本仓库）。
// 注意：本文件会被 include! 内联进 wrapper（非 crate 根），故不可用内属属性
// （#![...]）与文件级 doc 注释（//!），只用普通注释。
//
// ── 值模型与 ABI 边界（T2 定案） ──
// 设计文档 §4.3 原设想「Hnv{t,a,b}=24B 内联寄存器」，但经 ABI 实测（rustc
// windows-gnu 反汇编确认）：24 字节 > 16 字节的 repr(C) 结构体在 Windows x64 下
// 走「隐藏 sret 指针」（返回值写进调用方提供的缓冲区、参数经指针传入），而
// cranelift 多结果函数用 RAX/RDX/RCX 返回——二者在 object↔rustc 边界不匹配。
// 故 T2 起改为「句柄式」：寄存器（cranelift SSA i64 值）持有 u64 句柄，指向堆上
// 的 HnCell{rc,t,a,b,payload}；跨 FFI 只传 i64（T1 已验证的干净 SysV 寄存器 ABI）。
// 所有权：槽位/调用方持有句柄 = 持有 HnCell 的一个引用（rc≥1）；rt_retain 加引用
// 返回同一句柄，rt_drop 减引用、归零时释放 cell 及其 payload。所有「可能消费」的
// 运算（rt_binop/rt_unop）前 lowering 先 rt_retain 出独立引用，镜像 VM 的 clone 语义。
//
// ── Hnv（cell 内的值本体） ──
// 标签：T_INT / T_FLOAT / T_BOOL_TRUE / T_BOOL_FALSE / T_CHAR / T_NULL / T_STR / T_ERR
//   标量：a=值位（int / f64 位 / 码点），b=0，payload=0
//   T_STR：a=字节起始 *const u8，b=字节长度，payload=容量（用于释放）
//   T_ERR：a=0，b=0，payload=*mut HnErr
//
// ── ABI（object 侧 import，本文件定义；全 u64 进出，干净寄存器 ABI） ──
// __hn_main() -> u64             object 内 main chunk（object 定义，返回最终值句柄）
// hn_entry() -> i32              产物入口：ERR → rt_unwind；否则 drop 后退出码 0
// rt_i64/rt_f64/rt_bool/rt_char/rt_null -> u64   标量字面量 → 句柄（rc=1）
// rt_str(i) -> u64               字符串表第 i 项 → 句柄（retain 表项，表基引用永存）
// rt_retain(h) -> u64            加引用，返回 h
// rt_drop(h)                     减引用（归零释放）
// rt_binop(op,x,y) -> u64        消费 x/y，返回结果句柄；op 编码见下
// rt_unop(op,x) -> u64           消费 x（5=neg 6=not）
// rt_print(h)                    打印 display（借用，不 drop）
// rt_truthy(h) -> u64            真值 0/1（借用）；rt_is_err(h) -> u64 同理
// rt_binop op: 0=add 1=sub 2=mul 3=div 4=mod 7=eq 8=ne 9=lt 10=le 11=gt 12=ge
//   语义逐条镜像 vm.rs v_* ；T3 补 str 拼接 / 全类型

const T_INT: u64 = 0;
const T_FLOAT: u64 = 1;
const T_BOOL_TRUE: u64 = 2;
const T_BOOL_FALSE: u64 = 3;
const T_CHAR: u64 = 4;
const T_NULL: u64 = 5;
const T_STR: u64 = 6;
const T_ERR: u64 = 7;
const T_FN: u64 = 8;

#[derive(Clone, Copy)]
#[repr(C)]
pub struct Hnv {
    pub t: u64,
    pub a: u64,
    pub b: u64,
}

impl Hnv {
    #[inline]
    pub fn int(v: i64) -> Hnv {
        Hnv { t: T_INT, a: v as u64, b: 0 }
    }
    #[inline]
    pub fn float(f: f64) -> Hnv {
        Hnv { t: T_FLOAT, a: f.to_bits(), b: 0 }
    }
    #[inline]
    pub fn bool_(b: bool) -> Hnv {
        let t = if b { T_BOOL_TRUE } else { T_BOOL_FALSE };
        Hnv { t, a: 0, b: 0 }
    }
    #[inline]
    pub fn char_(c: char) -> Hnv {
        Hnv { t: T_CHAR, a: c as u32 as u64, b: 0 }
    }
    #[inline]
    pub fn null() -> Hnv {
        Hnv { t: T_NULL, a: 0, b: 0 }
    }
    /// 真值：与 vm.rs is_truthy 的 M0 子集一致（null/false 为假，余真）。
    #[inline]
    pub fn is_truthy(&self) -> bool {
        !matches!(self.t, T_BOOL_FALSE | T_NULL)
    }
}

// cell 内堆负载类型（payload 字段解释）。
const P_NONE: u64 = 0; // 标量
const P_STR: u64 = 1;  // payload=容量，a=字节指针，b=长度
const P_ERR: u64 = 2;  // payload=*mut HnErr

#[derive(Clone, Copy)]
#[repr(C)]
struct HnCell {
    rc: u64,
    t: u64,
    a: u64,
    b: u64,
    pkind: u64, // P_NONE / P_STR / P_ERR
    payload: u64,
}

impl HnCell {
    fn hnv(&self) -> Hnv {
        Hnv { t: self.t, a: self.a, b: self.b }
    }
}

// 运行期句柄存活计数（仅「运行期新建」cell：str_new/err_new；strtable 不计）。
// debug 断言与单测用（T2 退出标准：出口句柄全 drop、rc 归零、无泄漏）。
static HEAP_LIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(debug_assertions)]
fn leak_check_inc() {
    HEAP_LIVE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}
#[cfg(not(debug_assertions))]
fn leak_check_inc() {}

/// 释放一个句柄（减引用；归零时释放 payload 与 cell 本体）。
fn cell_release(h: u64) {
    if h == 0 {
        return;
    }
    let p = h as *mut HnCell;
    unsafe {
        let c = *p;
        (*p).rc -= 1;
        if c.rc == 1 {
            match c.pkind {
                P_STR => {
                    let cap = c.payload as usize;
                    if cap > 0 {
                        let layout = std::alloc::Layout::from_size_align(cap, 8).unwrap();
                        std::alloc::dealloc(c.a as *mut u8, layout);
                    }
                }
                P_ERR => drop(Box::from_raw(c.payload as *mut HnErr)),
                _ => {}
            }
            drop(Box::from_raw(p));
            HEAP_LIVE.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

#[no_mangle]
pub extern "C" fn rt_drop(h: u64) {
    cell_release(h);
}

#[no_mangle]
pub extern "C" fn rt_retain(h: u64) -> u64 {
    if h != 0 {
        unsafe {
            let p = h as *mut HnCell;
            (*p).rc += 1;
        }
    }
    h
}

/// 由 Hnv + 堆负载建一个新句柄（rc=1，调用方获得所有权）。
fn make_cell(h: Hnv) -> u64 {
    let (pkind, payload) = if h.t == T_ERR {
        (P_ERR, h.a) // ERR：a 携带 HnErr 指针，移入 payload
    } else if h.t == T_STR {
        (P_STR, 0) // M0 数值路径不产 STR；T3 拼接走 make_cell_str
    } else {
        (P_NONE, 0)
    };
    let a = if h.t == T_ERR { 0 } else { h.a };
    let c = Box::into_raw(Box::new(HnCell {
        rc: 1,
        t: h.t,
        a,
        b: h.b,
        pkind,
        payload,
    }));
    leak_check_inc();
    c as u64
}

/// 由字节建一个「拥有型」串句柄（rc=1）：a=字节指针，b=长度，payload=容量。
fn make_cell_str(bytes: Vec<u8>) -> u64 {
    let (ptr, len, cap) = bytes.into_raw_parts();
    let c = Box::into_raw(Box::new(HnCell {
        rc: 1,
        t: T_STR,
        a: ptr as u64,
        b: len as u64,
        pkind: P_STR,
        payload: cap as u64,
    }));
    leak_check_inc();
    c as u64
}

#[inline]
fn cell(h: u64) -> HnCell {
    unsafe { *(h as *const HnCell) }
}

#[inline]
fn str_bytes<'a>(h: u64) -> &'a [u8] {
    unsafe {
        let c = *(h as *const HnCell);
        std::slice::from_raw_parts(c.a as *const u8, c.b as usize)
    }
}

// ── 字符串表（wrapper/object 初始化） ──

static STRTAB: std::sync::Mutex<Vec<u64>> = std::sync::Mutex::new(Vec::new());

/// 表初始化：strings[i] 指向长度为 lens[i] 的字节串；建表所有 cell（rc=1 基引用，
/// 进程存活即有效；不计入 HEAP_LIVE，常量视为 rodata 常驻）。
#[no_mangle]
pub extern "C" fn hn_rt_strtab_init(
    strings: *const *const u8,
    lens: *const usize,
    n: usize,
) {
    let mut t = STRTAB.lock().unwrap();
    t.clear();
    for i in 0..n {
        let bytes = unsafe { std::slice::from_raw_parts(strings.add(i).read(), lens.add(i).read()) };
        let (ptr, len, cap) = bytes.to_vec().into_raw_parts();
        let c = Box::into_raw(Box::new(HnCell {
            rc: 1,
            t: T_STR,
            a: ptr as u64,
            b: len as u64,
            pkind: P_STR,
            payload: cap as u64,
        }));
        t.push(c as u64);
    }
}

/// 常量字符串按索引取值：retain 表项后返回句柄（调用方负责 drop）。
#[no_mangle]
pub extern "C" fn rt_str(i: u64) -> u64 {
    let t = STRTAB.lock().unwrap();
    let h = t[i as usize];
    drop(t);
    rt_retain(h)
}

// ── 错误（M0 骨架：T4 补 span 表 / help / caret 逐字节渲染） ──

// 纯运行时内部类型（不跨 FFI），msg 用 Vec 自持内存，由 Box<HnErr> 统一释放。
// code 存 &str 胖指针（ptr+len 各 u64）：&'static str 字节不一定 NUL 结尾，
// 不能按 C 串读，须用 from_utf8_unchecked 带长度还原。
struct HnErr {
    code_p: u64,
    code_l: u64,
    msg: Vec<u8>,
}

fn err_code_str(e: &HnErr) -> &'static str {
    unsafe {
        let s = std::str::from_utf8_unchecked(std::slice::from_raw_parts(
            e.code_p as *const u8,
            e.code_l as usize,
        ));
        // code 仅由运行时内部 err_new 传入 &'static str 字面量，此转换安全。
        &*(s as *const str)
    }
}

/// 建错误句柄（rc=1，调用方获得所有权）。
fn err_new(code: &'static str, msg: String) -> u64 {
    let e = Box::into_raw(Box::new(HnErr {
        code_p: code.as_ptr() as u64,
        code_l: code.len() as u64,
        msg: msg.into_bytes(),
    }));
    let c = Box::into_raw(Box::new(HnCell {
        rc: 1,
        t: T_ERR,
        a: 0,
        b: 0,
        pkind: P_ERR,
        payload: e as u64,
    }));
    leak_check_inc();
    c as u64
}

fn err_render(h: u64) -> String {
    unsafe {
        let c = *(h as *const HnCell);
        let e = &*(c.payload as *const HnErr);
        format!(
            "error[{}]: {}",
            err_code_str(e),
            String::from_utf8_lossy(&e.msg)
        )
    }
}

/// M0 错误出口：stderr 打印后退出（T4 → 完整渲染 + T6 跨帧协议）。
#[no_mangle]
pub extern "C" fn rt_unwind(err: u64) -> ! {
    eprintln!("{}", err_render(err));
    std::process::exit(1);
}

// ── 值语义（镜像 interp::Value::display / vm.rs v_*） ──

fn type_name(t: u64) -> &'static str {
    match t {
        T_INT => "int",
        T_FLOAT => "float",
        T_BOOL_TRUE | T_BOOL_FALSE => "bool",
        T_CHAR => "char",
        T_NULL => "null",
        T_STR => "str",
        T_ERR => "error",
        T_FN => "fn",
        _ => "?",
    }
}

/// 标量 display，与 interp::Value::display 的 M0 子集逐字一致（float 用 std Display 天然一致）。
/// 仅处理标量：T_STR / T_ERR 由 handle_display 走句柄路径（需堆字节）。
pub fn hnv_display(v: Hnv) -> String {
    match v.t {
        T_INT => (v.a as i64).to_string(),
        T_FLOAT => f64::from_bits(v.a).to_string(),
        T_BOOL_TRUE => "true".to_string(),
        T_BOOL_FALSE => "false".to_string(),
        T_CHAR => char::from_u32(v.a as u32).unwrap_or('\u{FFFD}').to_string(),
        T_NULL => "null".to_string(),
        T_FN => "fn".to_string(),
        _ => "null".to_string(),
    }
}

/// 句柄版 display（T_STR 需句柄取字节）。
pub fn handle_display(h: u64) -> String {
    let c = cell(h);
    if c.t == T_STR {
        let b = unsafe { std::slice::from_raw_parts(c.a as *const u8, c.b as usize) };
        return String::from_utf8_lossy(b).into_owned();
    }
    if c.t == T_ERR {
        return err_render(h);
    }
    hnv_display(c.hnv())
}

// 算术（镜像 vm.rs v_add/v_sub/v_mul/v_div/v_mod；T3 补 str 拼接与全类型）。
// 纯 Hnv 运算，产出 Hnv（由调用方决定建句柄 / 释放输入）。
fn arith_inner(op: u64, x: Hnv, y: Hnv) -> Hnv {
    match (x.t, y.t) {
        (T_INT, T_INT) => {
            let (a, b) = (x.a as i64, y.a as i64);
            let res = match op {
                0 => a.checked_add(b),
                1 => a.checked_sub(b),
                2 => a.checked_mul(b),
                3 => {
                    if b == 0 {
                        return err_h("H009", "division by zero");
                    }
                    a.checked_div(b)
                }
                4 => {
                    if b == 0 {
                        return err_h("H009", "division by zero");
                    }
                    a.checked_rem(b)
                }
                _ => unreachable!(),
            };
            match res {
                Some(v) => Hnv::int(v),
                None => err_h("H010", "integer overflow"),
            }
        }
        (T_FLOAT, T_FLOAT) => {
            let (a, b) = (f64::from_bits(x.a), f64::from_bits(y.a));
            let res = match op {
                0 => a + b,
                1 => a - b,
                2 => a * b,
                3 => {
                    if b == 0.0 {
                        return err_h("H009", "division by zero");
                    }
                    a / b
                }
                4 => {
                    if b == 0.0 {
                        return err_h("H009", "division by zero");
                    }
                    a % b
                }
                _ => unreachable!(),
            };
            Hnv::float(res)
        }
        // int×float 混合：除/取余镜像 v_div/v_mod 的组合；加/减/乘 M0 未覆盖
        // （T3 与 VM 对齐：v_add 无混合臂 → 应为 H001，故 M0 统一报 H001）。
        (T_INT, T_FLOAT) | (T_FLOAT, T_INT) if op == 3 || op == 4 => {
            let (a, b) = if x.t == T_INT {
                (x.a as i64 as f64, f64::from_bits(y.a))
            } else {
                (f64::from_bits(x.a), y.a as i64 as f64)
            };
            if b == 0.0 {
                return err_h("H009", "division by zero");
            }
            let res = if op == 3 { a / b } else { a % b };
            Hnv::float(res)
        }
        _ => {
            let sym = match op {
                0 => "+",
                1 => "-",
                2 => "*",
                3 => "/",
                _ => "%",
            };
            err_h(
                "H001",
                &format!("cannot apply `{}` to `{}` and `{}`", sym, type_name(x.t), type_name(y.t)),
            )
        }
    }
}

fn neg_h(x: Hnv) -> Hnv {
    match x.t {
        T_INT => match (x.a as i64).checked_neg() {
            Some(v) => Hnv::int(v),
            None => err_h("H010", "integer overflow"),
        },
        T_FLOAT => Hnv::float(-f64::from_bits(x.a)),
        other => err_h("H001", &format!("unary `-` requires a number, got `{}`", type_name(other))),
    }
}

fn not_h(x: Hnv) -> Hnv {
    match x.t {
        T_BOOL_TRUE => Hnv::bool_(false),
        T_BOOL_FALSE => Hnv::bool_(true),
        other => err_h("H001", &format!("`!` requires a `bool`, got `{}`", type_name(other))),
    }
}

fn cmpv_inner(op: u64, x: Hnv, y: Hnv) -> Hnv {
    match op {
        7 | 8 => {
            let eq = match (x.t, y.t) {
                (T_INT, T_INT) => x.a == y.a,
                (T_FLOAT, T_FLOAT) => x.a == y.a,
                (T_BOOL_TRUE, T_BOOL_TRUE) | (T_BOOL_FALSE, T_BOOL_FALSE) => true,
                (T_CHAR, T_CHAR) => x.a == y.a,
                (T_NULL, T_NULL) => true,
                (T_STR, T_STR) => unreachable!(), // 比较需句柄，句柄路径单独处理
                _ => {
                    return err_h(
                        "H001",
                        &format!("cannot compare `{}` with `{}`", type_name(x.t), type_name(y.t)),
                    )
                }
            };
            Hnv::bool_(if op == 7 { eq } else { !eq })
        }
        9 | 10 | 11 | 12 => {
            let c = match (x.t, y.t) {
                (T_INT, T_INT) => (x.a as i64).cmp(&(y.a as i64)),
                (T_CHAR, T_CHAR) => {
                    let cx = char::from_u32(x.a as u32).unwrap_or('\u{FFFD}');
                    let cy = char::from_u32(y.a as u32).unwrap_or('\u{FFFD}');
                    cx.cmp(&cy)
                }
                (T_FLOAT, T_FLOAT) => {
                    let (a, b) = (f64::from_bits(x.a), f64::from_bits(y.a));
                    match a.partial_cmp(&b) {
                        Some(c) => c,
                        None => return err_h("H001", "cannot compare NaN values"),
                    }
                }
                _ => {
                    return err_h(
                        "H001",
                        &format!("cannot compare `{}` with `{}`", type_name(x.t), type_name(y.t)),
                    )
                }
            };
            let v = match op {
                9 => c == std::cmp::Ordering::Less,
                10 => c != std::cmp::Ordering::Greater,
                11 => c == std::cmp::Ordering::Greater,
                _ => c != std::cmp::Ordering::Less,
            };
            Hnv::bool_(v)
        }
        _ => unreachable!(),
    }
}

// 内部错误构造（返回 Hnv，a 携带 HnErr 指针，由 make_cell 移入 payload）。
fn err_h(code: &'static str, msg: &str) -> Hnv {
    let e = Box::into_raw(Box::new(HnErr {
        code_p: code.as_ptr() as u64,
        code_l: code.len() as u64,
        msg: msg.as_bytes().to_vec(),
    }));
    Hnv { t: T_ERR, a: e as u64, b: 0 }
}

// ── FFI 入口（全 u64 进出） ──

#[no_mangle]
pub extern "C" fn rt_i64(v: u64) -> u64 {
    make_cell(Hnv::int(v as i64))
}

#[no_mangle]
pub extern "C" fn rt_f64(v: u64) -> u64 {
    make_cell(Hnv::float(f64::from_bits(v)))
}

#[no_mangle]
pub extern "C" fn rt_bool(v: u64) -> u64 {
    make_cell(Hnv::bool_(v != 0))
}

#[no_mangle]
pub extern "C" fn rt_char(v: u64) -> u64 {
    make_cell(Hnv::char_(char::from_u32(v as u32).unwrap_or('\u{FFFD}')))
}

#[no_mangle]
pub extern "C" fn rt_null() -> u64 {
    make_cell(Hnv::null())
}

/// 运算/比较分发（op 编码见文件头）。消费 x/y（drop），返回结果句柄。
#[no_mangle]
pub extern "C" fn rt_binop(op: u64, x: u64, y: u64) -> u64 {
    // 先取出 Hnv 再释放输入（镜像 VM clone 后 v_* 消费克隆）
    let cx = cell(x);
    let cy = cell(y);
    let r = match op {
        0..=4 => arith_inner(op, cx.hnv(), cy.hnv()),
        7..=12 => cmpv_inner(op, cx.hnv(), cy.hnv()),
        _ => err_h("H001", "bad rt_binop op"),
    };
    cell_release(x);
    cell_release(y);
    make_cell(r)
}

/// 一元运算分发（5=neg 6=not）。消费 x（drop），返回结果句柄。
#[no_mangle]
pub extern "C" fn rt_unop(op: u64, x: u64) -> u64 {
    let cx = cell(x);
    let r = match op {
        5 => neg_h(cx.hnv()),
        6 => not_h(cx.hnv()),
        _ => err_h("H001", "bad rt_unop op"),
    };
    cell_release(x);
    make_cell(r)
}

/// 打印 display（借用 h，不 drop）。
#[no_mangle]
pub extern "C" fn rt_print(h: u64) {
    println!("{}", handle_display(h));
}

#[no_mangle]
pub extern "C" fn rt_truthy(h: u64) -> u64 {
    let c = cell(h);
    u64::from(c.hnv().is_truthy())
}

#[no_mangle]
pub extern "C" fn rt_is_err(h: u64) -> u64 {
    u64::from(cell(h).t == T_ERR)
}

/// 产物入口：ERR → rt_unwind；否则 drop 后退出码 0（与 hone run 一致）。
#[no_mangle]
pub extern "C" fn hn_entry() -> i32 {
    let r = unsafe { __hn_main() };
    if cell(r).t == T_ERR {
        rt_unwind(r);
    }
    cell_release(r);
    0
}

// 测试构建下 object 不存在：不声明 extern，改由下方测试桩定义同名符号。
#[cfg(not(test))]
extern "C" {
    fn __hn_main() -> u64;
}

#[cfg(test)]
#[no_mangle]
pub extern "C" fn __hn_main() -> u64 {
    rt_null()
}

#[cfg(test)]
pub fn heap_live() -> usize {
    HEAP_LIVE.load(std::sync::atomic::Ordering::Relaxed)
}

// HEAP_LIVE 为进程全局计数器；测试并行会互相污染基线，故全部涉及泄漏断言的
// 测试串行化（每个测试净增 0 + 串行 → 基线差值恒成立）。
#[cfg(test)]
static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
fn locked() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_returns_zero() {
        assert_eq!(hn_entry(), 0);
    }

    #[test]
    fn binop_int_add_overflow() {
        let _g = locked();
        let base = heap_live();
        let a = rt_i64(i64::MAX as u64);
        let b = rt_i64(1);
        let r = rt_binop(0, a, b);
        assert_eq!(cell(r).t, T_ERR);
        assert_eq!(err_render(r), "error[H010]: integer overflow");
        cell_release(r);
        assert_eq!(heap_live(), base);
    }

    #[test]
    fn binop_div_zero() {
        let _g = locked();
        let base = heap_live();
        let a = rt_i64(1);
        let b = rt_i64(0);
        let r = rt_binop(3, a, b);
        assert_eq!(err_render(r), "error[H009]: division by zero");
        cell_release(r);
        assert_eq!(heap_live(), base);
    }

    #[test]
    fn binop_type_mismatch() {
        let _g = locked();
        let base = heap_live();
        let a = rt_bool(1);
        let b = rt_i64(1);
        let r = rt_binop(0, a, b);
        assert_eq!(err_render(r), "error[H001]: cannot apply `+` to `bool` and `int`");
        cell_release(r);
        assert_eq!(heap_live(), base);
    }

    #[test]
    fn binop_basic() {
        let _g = locked();
        let base = heap_live();
        let r = rt_binop(0, rt_i64(2), rt_i64(3));
        assert_eq!(cell(r).t, T_INT);
        assert_eq!(cell(r).a as i64, 5);
        cell_release(r);
        let r = rt_binop(2, rt_i64(4), rt_i64(5));
        assert_eq!(cell(r).a as i64, 20);
        cell_release(r);
        let r = rt_unop(5, rt_i64(7));
        assert_eq!(cell(r).a as i64, -7);
        cell_release(r);
        let r = rt_unop(6, rt_bool(1));
        assert_eq!(cell(r).t, T_BOOL_FALSE);
        cell_release(r);
        assert_eq!(heap_live(), base);
    }

    #[test]
    fn display_m0_subset() {
        let _g = locked();
        let base = heap_live();
        let h = rt_i64((-5i64) as u64);
        assert_eq!(handle_display(h), "-5");
        rt_drop(h);
        let h = rt_f64(2.5f64.to_bits());
        assert_eq!(handle_display(h), "2.5");
        rt_drop(h);
        let h = rt_bool(1);
        assert_eq!(handle_display(h), "true");
        rt_drop(h);
        let h = rt_char('中' as u32 as u64);
        assert_eq!(handle_display(h), "中");
        rt_drop(h);
        let h = rt_null();
        assert_eq!(handle_display(h), "null");
        rt_drop(h);
        assert_eq!(heap_live(), base);
    }

    #[test]
    fn strtab_roundtrip_and_no_leak() {
        let _g = locked();
        let base = heap_live();
        let strs: Vec<String> = vec!["hello".to_string(), "世界".to_string()];
        let ptrs: Vec<*const u8> = strs.iter().map(|s| s.as_ptr()).collect();
        let lens: Vec<usize> = strs.iter().map(|s| s.len()).collect();
        hn_rt_strtab_init(ptrs.as_ptr(), lens.as_ptr(), ptrs.len());
        let a = rt_str(0);
        let b = rt_str(1);
        assert_eq!(handle_display(a), "hello");
        assert_eq!(handle_display(b), "世界");
        assert_eq!(rt_truthy(a), 1);
        assert_eq!(rt_is_err(a), 0);
        rt_drop(a);
        rt_drop(b);
        // 表基引用常驻不计入 HEAP_LIVE；运行期 cell 净增归零
        assert_eq!(heap_live(), base);
    }
}
