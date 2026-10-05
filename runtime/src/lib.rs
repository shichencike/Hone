// Hone 原生编译后端（natgen）的链接运行时。
// 单文件、std-only。双轨使用：
// 1. workspace 成员（cargo test -p honeruntime，开发期单测）；
// 2. 经 include_str! 嵌入 hone 二进制，hone natgen 时写出临时目录，
//    与生成式 wrapper 一起被 rustc 编译链接进产物（干净构建机自足，无需本仓库）。
// 注意：本文件会被 include! 内联进 wrapper（非 crate 根），故不可用内属属性
// （#![...]）与文件级 doc 注释（//!），只用普通注释。
//
// ── 值模型（设计 §4.3，T2 落地） ──
// Hnv = 24 字节 {t,a,b}（各 u64，x86_64 SysV 三寄存器传参/返回，T1 已验证）：
//   T_INT   a=整数位                T_FLOAT a=f64 位
//   T_BOOL_TRUE / T_BOOL_FALSE      a/b 不用
//   T_CHAR  a=Unicode 码点（u32 低 32）
//   T_NULL  a/b 不用
//   T_STR   a=HnBox 指针 | 所有权位（bit0=1 本值拥有；bit0=0 表所有常量串）
//   T_ERR   a=HnBox 指针（恒拥有）
//   T_FN    （T5 启用）
// 堆值统一 HnBox{rc, kind, payload}：payload 是堆块指针（Box<Vec<u8>> / Box<HnErr>），
// 释放一律 Box::from_raw（由 Box 掌握容量，dealloc 正确）。M0 只做 retain/release
// 引用计数（COW 惰性深拷留 M1，G1.1）。常量字符串由 wrapper 启动建箱、归表所有；
// 运行期新串（T3 起）独占 box，按引用计数释放。
//
// 所有权纪律（与 src/natgen.rs lowering 严格对应）：
//   - 寄存器槽 = 权威持有者；写寄存器：释放旧 in-flight 与旧槽值，新堆值先
//     rt_retain（槽与 in-flight 各一份）再写槽。
//   - 跨帧/跨函数传递（调用实参、返回值、Move、入口参数）前先 rt_retain 出独立
//     引用，消费方 rt_drop 平衡。
//
// ── ABI（object 侧 import，本文件定义） ──
// __hn_main() -> Hnv            object 内 main chunk（object 定义，此处声明）
// hn_entry() -> i32             产物入口：ERR → rt_unwind；否则退出码 0
// rt_print(v) -> Hnv            打印 v.display()（返回 null）
// rt_retain(v) / rt_drop(v)     引用计数原语（标量 / 表所有 no-op）
// rt_binop(op, x, y) -> Hnv     op: 0=add 1=sub 2=mul 3=div 4=mod 5=neg 6=not
//                               7=eq 8=ne 9=lt 10=le 11=gt 12=ge
//                               语义逐条镜像 vm.rs v_* ；实参所有权转移（函数内
//                               释放）；ERR 结果按返回值传回（T6 补 handler 跳转）
// rt_str_at(i) -> Hnv           字符串表第 i 项（常量串，表所有，bit0=0）

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

// payload 为堆块指针（Box<Vec<u8>> 或 Box<HnErr>），仅存 3 个 u64 → 可 Copy。
#[derive(Clone, Copy)]
#[repr(C)]
struct HnBox {
    rc: u64,
    kind: u64,
    payload: u64,
}

const K_STR: u64 = 0; // payload: *mut Vec<u8>
const K_ERR: u64 = 1; // payload: *mut HnErr

// 运行期字符串表（wrapper 启动时填充，进程生命周期不变）。
static STRTAB: std::sync::Mutex<Vec<u64>> = std::sync::Mutex::new(Vec::new());

// 堆 box 存活计数（仅「拥有型」box：运行期新串/错误；表所有不计）。
// debug 断言与单测用（T2 退出标准：出口 rc 归零、无泄漏）。
// 恒存在（dec 恒执行保持 inc/dec 对称）；产物构建下 inc 编译消除、无开销。
static HEAP_LIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(debug_assertions)]
fn leak_check_inc() {
    HEAP_LIVE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}
#[cfg(not(debug_assertions))]
fn leak_check_inc() {}

/// 释放一个 HnBox 引用；rc→0 时按 kind 归还堆块与 box 本体。
#[inline]
fn box_release(h: u64) {
    let p = h as *mut HnBox;
    unsafe {
        let box_ = *p;
        (*p).rc -= 1;
        if box_.rc == 1 {
            match box_.kind {
                K_STR => drop(Box::from_raw(box_.payload as *mut Vec<u8>)),
                K_ERR => drop(Box::from_raw(box_.payload as *mut HnErr)),
                _ => {}
            }
            drop(Box::from_raw(p));
            HEAP_LIVE.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// 释放 v 若持有独立引用（STR 带 bit0 / ERR 恒拥有）。
#[no_mangle]
pub extern "C" fn rt_drop(v: Hnv) {
    match v.t {
        T_STR if v.a & 1 != 0 => box_release(v.a & !1u64),
        T_ERR => box_release(v.a),
        _ => {}
    }
}

/// 增加独立引用（STR 表所有 / 标量 no-op）。
#[no_mangle]
pub extern "C" fn rt_retain(v: Hnv) {
    match v.t {
        T_STR if v.a & 1 != 0 => unsafe {
            let p = (v.a & !1u64) as *mut HnBox;
            (*p).rc += 1;
        },
        T_ERR => unsafe {
            let p = v.a as *mut HnBox;
            (*p).rc += 1;
        },
        _ => {}
    }
}

/// 串字节（M0：返回借用；数据由 box 的 rc 保护，调用方须保证 v 的 box 存活）。
fn str_bytes<'a>(v: Hnv) -> &'a [u8] {
    unsafe {
        let bx = *((v.a & !1u64) as *const HnBox);
        let vec_ref = &*(bx.payload as *const Vec<u8>);
        std::slice::from_raw_parts(vec_ref.as_ptr(), vec_ref.len())
    }
}

/// 由字节建一个「拥有型」串 box（rc=1，bit0=1）。T3 起拼接用。
fn str_new(bytes: Vec<u8>) -> Hnv {
    let vec = Box::into_raw(Box::new(bytes));
    let n = unsafe { (*vec).len() };
    let h = Box::into_raw(Box::new(HnBox { rc: 1, kind: K_STR, payload: vec as u64 }));
    leak_check_inc();
    Hnv { t: T_STR, a: (h as u64) | 1, b: n as u64 }
}

// ── 字符串表（wrapper 初始化） ──

/// 表初始化：strings[i] 指向长度为 lens[i] 的字节串；建表所有 box（rc=1），进程存活即有效。
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
        let vec = Box::into_raw(Box::new(bytes.to_vec()));
        let h = Box::into_raw(Box::new(HnBox { rc: 1, kind: K_STR, payload: vec as u64 }));
        t.push(h as u64);
    }
}

/// 常量字符串按索引取值（表所有，bit0=0）。
#[no_mangle]
pub extern "C" fn rt_str_at(i: usize) -> Hnv {
    let t = STRTAB.lock().unwrap();
    let h = t[i];
    let bx = unsafe { *(h as *const HnBox) };
    let n = unsafe { (&*(bx.payload as *const Vec<u8>)).len() };
    Hnv { t: T_STR, a: h, b: n as u64 }
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

/// 建错误 box（rc=1，调用方获得所有权）。
fn err_new(code: &'static str, msg: String) -> Hnv {
    let e = Box::into_raw(Box::new(HnErr {
        code_p: code.as_ptr() as u64,
        code_l: code.len() as u64,
        msg: msg.into_bytes(),
    }));
    let h = Box::into_raw(Box::new(HnBox { rc: 1, kind: K_ERR, payload: e as u64 }));
    leak_check_inc();
    Hnv { t: T_ERR, a: h as u64, b: 0 }
}

/// M0：`error[Hxxx]: msg`（T4 对齐 ZError 行/列/caret/help）。
pub fn err_render(v: Hnv) -> String {
    unsafe {
        let box_ = *(v.a as *const HnBox);
        let e: &HnErr = &*(box_.payload as *const HnErr);
        format!("error[{}]: {}", err_code_str(e), String::from_utf8_lossy(&e.msg))
    }
}

/// M0 错误出口：stderr 打印后退出（T4 → 完整渲染 + T6 跨帧协议）。
#[no_mangle]
pub extern "C" fn rt_unwind(err: Hnv) -> ! {
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

/// 与 interp::Value::display 的 M0 子集逐字一致（float 用 std Display 天然一致）。
pub fn hnv_display(v: Hnv) -> String {
    match v.t {
        T_INT => (v.a as i64).to_string(),
        T_FLOAT => f64::from_bits(v.a).to_string(),
        T_BOOL_TRUE => "true".to_string(),
        T_BOOL_FALSE => "false".to_string(),
        T_CHAR => char::from_u32(v.a as u32).unwrap_or('\u{FFFD}').to_string(),
        T_NULL => "null".to_string(),
        T_STR => String::from_utf8_lossy(str_bytes(v)).into_owned(),
        T_ERR => err_render(v),
        T_FN => "fn".to_string(),
        _ => "null".to_string(),
    }
}

// 算术（镜像 vm.rs v_add/v_sub/v_mul/v_div/v_mod；T3 补 str 拼接与全类型）。
// 实参 x/y 所有权已转移给本函数：所有出口前释放。
fn arith(op: u64, x: Hnv, y: Hnv) -> Hnv {
    let r = arith_inner(op, x, y);
    rt_drop(x);
    rt_drop(y);
    r
}

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
                        return err_new("H009", "division by zero".to_string());
                    }
                    a.checked_div(b)
                }
                4 => {
                    if b == 0 {
                        return err_new("H009", "division by zero".to_string());
                    }
                    a.checked_rem(b)
                }
                _ => unreachable!(),
            };
            match res {
                Some(v) => Hnv::int(v),
                None => err_new("H010", "integer overflow".to_string()),
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
                        return err_new("H009", "division by zero".to_string());
                    }
                    a / b
                }
                4 => {
                    if b == 0.0 {
                        return err_new("H009", "division by zero".to_string());
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
                return err_new("H009", "division by zero".to_string());
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
            err_new(
                "H001",
                format!("cannot apply `{}` to `{}` and `{}`", sym, type_name(x.t), type_name(y.t)),
            )
        }
    }
}

fn neg(x: Hnv) -> Hnv {
    let r = match x.t {
        T_INT => match (x.a as i64).checked_neg() {
            Some(v) => Hnv::int(v),
            None => err_new("H010", "integer overflow".to_string()),
        },
        T_FLOAT => Hnv::float(-f64::from_bits(x.a)),
        other => err_new(
            "H001",
            format!("unary `-` requires a number, got `{}`", type_name(other)),
        ),
    };
    rt_drop(x);
    r
}

fn not_(x: Hnv) -> Hnv {
    let r = match x.t {
        T_BOOL_TRUE => Hnv::bool_(false),
        T_BOOL_FALSE => Hnv::bool_(true),
        other => err_new("H001", format!("`!` requires a `bool`, got `{}`", type_name(other))),
    };
    rt_drop(x);
    r
}

// 比较（镜像 v_eq/v_cmp；实参所有权转移，出口前释放）。
fn cmpv(op: u64, x: Hnv, y: Hnv) -> Hnv {
    let r = cmpv_inner(op, x, y);
    rt_drop(x);
    rt_drop(y);
    r
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
                (T_STR, T_STR) => str_bytes(x) == str_bytes(y),
                _ => {
                    return err_new(
                        "H001",
                        format!(
                            "cannot compare `{}` with `{}`",
                            type_name(x.t),
                            type_name(y.t)
                        ),
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
                        None => {
                            return err_new("H001", "cannot compare NaN values".to_string())
                        }
                    }
                }
                _ => {
                    return err_new(
                        "H001",
                        format!(
                            "cannot compare `{}` with `{}`",
                            type_name(x.t),
                            type_name(y.t)
                        ),
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

// ── 入口与内置 ──

/// 产物入口：ERR → rt_unwind（T4 起完整渲染）；否则退出码 0（与 hone run 一致）。
#[no_mangle]
#[allow(unused_unsafe)] // 测试构建下 __hn_main 为安全桩函数
pub extern "C" fn hn_entry() -> i32 {
    let r = unsafe { __hn_main() };
    if r.t == T_ERR {
        rt_unwind(r);
    }
    rt_drop(r);
    0
}

/// 内置 print：`println!("{}", v.display())`，返回 null（镜像 builtins.rs print）。
#[no_mangle]
pub extern "C" fn rt_print(v: Hnv) -> Hnv {
    println!("{}", hnv_display(v));
    rt_drop(v);
    Hnv::null()
}

/// 运算/比较分发（op 编码见文件头 ABI 注释）。
#[no_mangle]
pub extern "C" fn rt_binop(op: u64, x: Hnv, y: Hnv) -> Hnv {
    match op {
        0..=4 => arith(op, x, y),
        5 => neg(x),
        6 => not_(x),
        7..=12 => cmpv(op, x, y),
        _ => err_new("H001", "bad rt_binop op".to_string()),
    }
}

// 测试构建下 object 不存在：不声明 extern，改由下方测试桩定义同名符号。
#[cfg(not(test))]
extern "C" {
    fn __hn_main() -> Hnv;
}

// 开发期单测桩：产物构建下 __hn_main 由 object 提供。
#[cfg(test)]
#[no_mangle]
pub extern "C" fn __hn_main() -> Hnv {
    Hnv::null()
}

#[cfg(test)]
pub fn heap_live() -> usize {
    HEAP_LIVE.load(std::sync::atomic::Ordering::Relaxed)
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
        let r = rt_binop(0, Hnv::int(i64::MAX), Hnv::int(1));
        assert_eq!(r.t, T_ERR);
        assert_eq!(err_render(r), "error[H010]: integer overflow");
        rt_drop(r);
    }

    #[test]
    fn binop_div_zero() {
        let r = rt_binop(3, Hnv::int(1), Hnv::int(0));
        assert_eq!(r.t, T_ERR);
        assert_eq!(err_render(r), "error[H009]: division by zero");
        rt_drop(r);
    }

    #[test]
    fn binop_type_mismatch() {
        let r = rt_binop(0, Hnv::bool_(true), Hnv::int(1));
        assert_eq!(err_render(r), "error[H001]: cannot apply `+` to `bool` and `int`");
        rt_drop(r);
    }

    #[test]
    fn display_m0_subset() {
        assert_eq!(hnv_display(Hnv::int(-5)), "-5");
        assert_eq!(hnv_display(Hnv::float(2.5)), "2.5");
        assert_eq!(hnv_display(Hnv::bool_(true)), "true");
        assert_eq!(hnv_display(Hnv::char_('中')), "中");
        assert_eq!(hnv_display(Hnv::null()), "null");
    }

    #[test]
    fn strtab_roundtrip_and_no_leak() {
        let strs: Vec<String> = vec!["hello".to_string(), "世界".to_string()];
        let ptrs: Vec<*const u8> = strs.iter().map(|s| s.as_ptr()).collect();
        let lens: Vec<usize> = strs.iter().map(|s| s.len()).collect();
        hn_rt_strtab_init(ptrs.as_ptr(), lens.as_ptr(), ptrs.len());
        let a = rt_str_at(0);
        let b = rt_str_at(1);
        assert_eq!(hnv_display(a), "hello");
        assert_eq!(hnv_display(b), "世界");
        assert_eq!(a.a & 1, 0); // 表所有：不拥有
        // 表所有串 drop 是 no-op，可安全多次释放。
        rt_drop(a);
        rt_drop(a);
        // 拥有型串：retain/drop 平衡后无泄漏。
        let s = str_new(b"leak-check".to_vec());
        assert_eq!(heap_live(), 1);
        rt_retain(s);
        rt_drop(s);
        rt_drop(s);
        assert_eq!(heap_live(), 0);
    }
}
