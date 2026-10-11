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
//       / T_FN / T_LIST / T_DICT
//   标量：a=值位（int / f64 位 / 码点），b=0，payload=0
//   T_STR：a=字节起始 *const u8，b=字节长度，payload=容量（用于释放）
//   T_ERR：a=0，b=0，payload=*mut HnErr
//   T_LIST / T_DICT：a/b=0，payload=*mut HnList / *mut HnDict（M1：元素/值句柄由 cell 拥有）
//
// ── ABI（object 侧 import，本文件定义；全 u64 进出，干净寄存器 ABI） ──
// __hn_main() -> u64             object 内 main chunk（object 定义，返回最终值句柄）
// hn_entry() -> i32              产物入口：ERR → rt_unwind；否则 drop 后退出码 0
// rt_i64/rt_f64/rt_bool/rt_char/rt_null -> u64   标量字面量 → 句柄（rc=1）
// rt_str(i) -> u64               字符串表第 i 项 → 句柄（retain 表项，表基引用永存）
// rt_retain(h) -> u64            加引用，返回 h
// rt_drop(h)                     减引用（归零释放）
// rt_binop(op,x,y,ci,pc) -> u64  消费 x/y，返回结果句柄；op 编码见下（T4：ci/pc = 出错指令位置）
// rt_unop(op,x,ci,pc) -> u64     消费 x（5=neg 6=not）（T4：ci/pc = 出错指令位置）
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
const T_LIST: u64 = 9;
const T_DICT: u64 = 10;
const T_ENUM: u64 = 11;
const T_PTR: u64 = 12; // G2.9：ptr 值（a=地址；镜像 interp Value::Ptr）
const T_FUTURE: u64 = 13; // G2.6：async 调用的 future（镜像 interp Value::Future）

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
    /// 标量真值（无句柄上下文时用）：与 vm.rs is_truthy 的 M0 标量子集一致。
    /// 注意：T_STR 是否为空需读句柄（a/b），故 rt_truthy 一律走 handle 版 truthy_handle。
    #[inline]
    pub fn is_truthy(&self) -> bool {
        match self.t {
            T_BOOL_TRUE => true,
            T_BOOL_FALSE => false,
            T_NULL => false,
            T_INT => self.a != 0,
            T_FLOAT => f64::from_bits(self.a) != 0.0,
            T_STR => self.b != 0,
            _ => true,
        }
    }
}

// cell 内堆负载类型（payload 字段解释）。
const P_NONE: u64 = 0; // 标量
const P_STR: u64 = 1;  // payload=容量，a=字节指针，b=长度
const P_ERR: u64 = 2;  // payload=*mut HnErr
const P_FN: u64 = 3;   // payload=*mut HnFnBox（T5：lambda 值，a=chunk 索引）
const P_LIST: u64 = 4; // payload=*mut HnList（M1：list 值，元素为句柄引用）
const P_DICT: u64 = 5; // payload=*mut HnDict（M1：dict 值，键 String + 句柄值）
const P_ENUM: u64 = 6; // payload=*mut HnEnum（G2.2：enum 值，a=枚举名 sidx，b=变体 sidx，载荷句柄）
const P_FUTURE: u64 = 7; // payload=*mut HnFuture（G2.6：async 值，Done 持结果/错误句柄引用）

/// lambda 值（T5）：chunk 索引 + 捕获句柄数组（每项 retain 引用，按值捕获语义）。
struct HnFnBox {
    ci: u32,
    caps: Vec<u64>,
}

/// list 值（M1）：元素句柄（每项 retain 引用，cell 拥有；写路径先过 cow 门）。
struct HnList {
    items: Vec<u64>,
}

/// enum 值（G2.2，镜像 interp EnumVal）：a=枚举名 strtab 索引，b=变体 strtab 索引，
/// 载荷为句柄数组（每项 retain 引用，cell 拥有）。
struct HnEnum {
    payload: Vec<u64>,
}

/// async future（G2.6，镜像 interp FutureVal）：后台线程执行函数体，await 阻塞等待。
/// Done 持**结果句柄引用**（可能是 ERR cell——镜像 VM wait 的 Err 分支，await 原样
/// 传播、由 check_err 路由 catch / 跨帧）。多次 await 各自拿到 retain 新引用
/// （镜像 VM wait 的 Value clone 值语义）。
struct HnFuture {
    state: std::sync::Mutex<FutureState>,
    cv: std::sync::Condvar,
}

enum FutureState {
    Pending,
    Done(u64),
}

/// dict 值（M1）：键 String（Hone dict 键恒为 str/int 转串，镜像 v_index 的 Dict 臂）+ 句柄值。
/// 写路径先过 cow 门；隐藏 `\u{0}__struct__` 标记键在用户可见路径过滤（G2.1 起生效，
/// M1 先按普通键处理，长度/显示/keys 已带过滤钩子）。
struct HnDict {
    pairs: Vec<(String, u64)>,
}

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
/// G2.6：rc 为**原子**操作——go/async 把值句柄跨线程传递后，两个线程可能同时
/// retain/release 同一 cell（HnCell 保持 Copy 便于快照读取，原子性经指针转型
/// AtomicU64 实现：std AtomicU64 与 u64 布局/对齐一致，字段仅经原子指令访问）。
/// 协议：减为 fetch_sub(Release)，归零方先 Acquire fence 再释放（与 Arc 同型）。
fn rc_ptr_of(h: u64) -> *mut std::sync::atomic::AtomicU64 {
    let p = h as *mut HnCell;
    unsafe { &mut (*p).rc as *mut u64 as *mut std::sync::atomic::AtomicU64 }
}

/// 原子读 rc（cow 门 / 调试用；直接值读会与并发增减竞争）。
fn rc_load(h: u64) -> u64 {
    unsafe { (*rc_ptr_of(h)).load(std::sync::atomic::Ordering::Relaxed) }
}

fn cell_release(h: u64) {
    if h == 0 {
        return;
    }
    unsafe {
        let p = h as *mut HnCell;
        let rp = rc_ptr_of(h);
        let prev = (*rp).fetch_sub(1, std::sync::atomic::Ordering::Release);
        if prev == 1 {
            std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
            let c = *p;
            match c.pkind {
                P_STR => {
                    let cap = c.payload as usize;
                    if cap > 0 {
                        let layout = std::alloc::Layout::from_size_align(cap, 8).unwrap();
                        std::alloc::dealloc(c.a as *mut u8, layout);
                    }
                }
                P_ERR => drop(Box::from_raw(c.payload as *mut HnErr)),
                P_FN => {
                    // 释放捕获句柄引用（逐项 cell_release）后 drop box。
                    let b = Box::from_raw(c.payload as *mut HnFnBox);
                    for &h in &b.caps {
                        cell_release(h);
                    }
                }
                P_LIST => {
                    let b = Box::from_raw(c.payload as *mut HnList);
                    for &h in &b.items {
                        cell_release(h);
                    }
                }
                P_DICT => {
                    let b = Box::from_raw(c.payload as *mut HnDict);
                    for (_, h) in &b.pairs {
                        cell_release(*h);
                    }
                }
                P_ENUM => {
                    let b = Box::from_raw(c.payload as *mut HnEnum);
                    for h in &b.payload {
                        cell_release(*h);
                    }
                }
                P_FUTURE => {
                    // 释放已完成的 future 所持结果引用（取走后置回 Pending 防双释）。
                    let b = Box::from_raw(c.payload as *mut HnFuture);
                    let mut s = b.state.lock().unwrap();
                    if let FutureState::Done(h) = std::mem::replace(&mut *s, FutureState::Pending) {
                        cell_release(h);
                    }
                }
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
            (*rc_ptr_of(h)).fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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

/// 由元素句柄列表建一个「拥有型」list 句柄（rc=1）：逐项 retain 进 cell（元素借用）。
fn make_cell_list(items: &[u64]) -> u64 {
    let boxed = Box::new(HnList {
        items: items.iter().map(|&h| rt_retain(h)).collect(),
    });
    let c = Box::into_raw(Box::new(HnCell {
        rc: 1,
        t: T_LIST,
        a: 0,
        b: 0,
        pkind: P_LIST,
        payload: Box::into_raw(boxed) as u64,
    }));
    leak_check_inc();
    c as u64
}

/// 由键值对建一个「拥有型」dict 句柄（rc=1）：键转 String（镜像 NewDict 的 value_to_str），
/// 值逐项 retain。
fn make_cell_dict(pairs: &[(String, u64)]) -> u64 {
    let boxed = Box::new(HnDict {
        pairs: pairs
            .iter()
            .map(|(k, v)| (k.clone(), rt_retain(*v)))
            .collect(),
    });
    let c = Box::into_raw(Box::new(HnCell {
        rc: 1,
        t: T_DICT,
        a: 0,
        b: 0,
        pkind: P_DICT,
        payload: Box::into_raw(boxed) as u64,
    }));
    leak_check_inc();
    c as u64
}

/// 隐藏 struct 标记键（与 interp/vm 的 STRUCT_MARKER_KEY 一致，G2.1 起过滤生效）。
const HIDDEN_STRUCT_KEY: &str = "\u{0}__struct__";
#[inline]
fn is_hidden_key(k: &str) -> bool {
    k == HIDDEN_STRUCT_KEY
}

/// 写时复制门（设计 §4.8）：独占（rc==1）返回原句柄原地写；共享（rc>1）浅拷一份
/// 独占副本（元素/值 retain，兄弟元素不碰——惰性 path 深拷的「只拷外层」形态）。
/// **拷贝分支不修改原 cell 的 rc**：原引用仍由各持有者（槽等）负责释放——调用方
/// （rt_indexset）把返回的「新槽值句柄」store_slot 替换时，槽的 drop 会释放旧引用，
/// 其余持有者（如别名变量）rc 守恒。str 不可变，写路径不经过此门。
fn cow_handle(h: u64) -> u64 {
    let c = cell(h);
    if rc_load(h) <= 1 {
        return h;
    }
    match c.pkind {
        P_LIST => {
            let src = unsafe { &*(c.payload as *const HnList) };
            make_cell_list(&src.items)
        }
        P_DICT => {
            let src = unsafe { &*(c.payload as *const HnDict) };
            make_cell_dict(&src.pairs)
        }
        _ => h,
    }
}

/// 句柄 → 显示串（递归；错误用完整渲染，容器递归 display）。
pub fn handle_display(h: u64) -> String {
    let c = cell(h);
    match c.pkind {
        P_STR => {
            let b = unsafe { std::slice::from_raw_parts(c.a as *const u8, c.b as usize) };
            return String::from_utf8_lossy(b).into_owned();
        }
        P_ERR => return err_render(h),
        P_LIST => {
            let l = unsafe { &*(c.payload as *const HnList) };
            let inner: Vec<String> = l.items.iter().map(|&e| handle_display(e)).collect();
            return format!("[{}]", inner.join(", "));
        }
        P_DICT => {
            let d = unsafe { &*(c.payload as *const HnDict) };
            let inner: Vec<String> = d
                .pairs
                .iter()
                .filter(|(k, _)| !is_hidden_key(k))
                .map(|(k, v)| format!("{}: {}", k, handle_display(*v)))
                .collect();
            return format!("{{{}}}", inner.join(", "));
        }
        // enum 显示：Color.Red / Shape.Circle(1.5)（镜像 interp Value::display 的 Enum 臂）。
        P_ENUM => {
            let b = unsafe { &*(c.payload as *const HnEnum) };
            let name = strtab_name(c.a as u32);
            let variant = strtab_name(c.b as u32);
            if b.payload.is_empty() {
                return format!("{name}.{variant}");
            }
            let inner: Vec<String> = b.payload.iter().map(|&e| handle_display(e)).collect();
            return format!("{name}.{variant}({})", inner.join(", "));
        }
        // G2.6：future 显示（镜像 interp/vm value_to_str 的 Future 臂，恒 "<future>"）。
        P_FUTURE => return "<future>".to_string(),
        _ => {}
    }
    hnv_display(c.hnv())
}

/// 递归值相等（镜像 interp Value::eq / vm values_eq 的容器臂）：标量走 cmpv_inner，
/// str 走字节比较，list/dict 递归结构比较（dict 按键序逐对比较，与 Rust Vec== 一致——
/// Hone dict 插入序即比较序，interp/VM 同为 Vec<(String,Value)>）。
fn values_eq_h(a: u64, b: u64, ci: u32, pc: u32) -> bool {
    let ca = cell(a);
    let cb = cell(b);
    if ca.t != cb.t {
        return false;
    }
    match ca.pkind {
        P_STR => str_eq(a, b),
        P_LIST => {
            let la = unsafe { &*(ca.payload as *const HnList) };
            let lb = unsafe { &*(cb.payload as *const HnList) };
            la.items.len() == lb.items.len()
                && la
                    .items
                    .iter()
                    .zip(lb.items.iter())
                    .all(|(&x, &y)| values_eq_h(x, y, ci, pc))
        }
        P_DICT => dict_values_eq_h(dict_payload(&ca), dict_payload(&cb), ci, pc),
        // enum 相等（镜像 interp Value::eq 的 Enum 臂）：枚举名 + 变体 + 载荷逐项相等。
        P_ENUM => {
            if ca.a != cb.a || ca.b != cb.b {
                return false;
            }
            let ea = unsafe { &*(ca.payload as *const HnEnum) };
            let eb = unsafe { &*(cb.payload as *const HnEnum) };
            ea.payload.len() == eb.payload.len()
                && ea
                    .payload
                    .iter()
                    .zip(eb.payload.iter())
                    .all(|(&x, &y)| values_eq_h(x, y, ci, pc))
        }
        _ => cmpv_inner(7, ca.hnv(), cb.hnv(), ci, pc).t == T_BOOL_TRUE,
    }
}

#[inline]
fn dict_payload(c: &HnCell) -> &'static HnDict {
    unsafe { &*(c.payload as *const HnDict) }
}

/// dict 相等判定（G2.1，镜像 interp `dict_values_eq`；与 VM `v_eq` 的 Dict 臂——Rust
/// `Vec==` 深比较——对 struct 实例语义重合：标记键同名则两侧相同、不同名则两侧不同，
/// 普通 dict 无隐藏键走 else 分支）：
/// - struct 实例（带 `__struct__` 标记键）：同名且可见字段按插入序逐对相等；
/// - 普通 dict：全键值逐对相等。
fn dict_values_eq_h(da: &'static HnDict, db: &'static HnDict, ci: u32, pc: u32) -> bool {
    let name_of = |d: &'static HnDict| -> Option<&str> {
        d.pairs
            .iter()
            .find(|(k, _)| is_hidden_key(k))
            .and_then(|(_, v)| std::str::from_utf8(str_bytes(*v)).ok())
    };
    match (name_of(da), name_of(db)) {
        (Some(a), Some(b)) => {
            if a != b {
                return false;
            }
            let va: Vec<&(String, u64)> = da.pairs.iter().filter(|(k, _)| !is_hidden_key(k)).collect();
            let vb: Vec<&(String, u64)> = db.pairs.iter().filter(|(k, _)| !is_hidden_key(k)).collect();
            if va.len() != vb.len() {
                return false;
            }
            va.iter()
                .zip(vb.iter())
                .all(|((ka, va_), (kb, vb_))| ka == kb && values_eq_h(*va_, *vb_, ci, pc))
        }
        _ => da.pairs.len() == db.pairs.len()
            && da
                .pairs
                .iter()
                .zip(db.pairs.iter())
                .all(|((ka, va), (kb, vb))| ka == kb && values_eq_h(*va, *vb, ci, pc)),
    }
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

// ── 程序静态表（T4：错误渲染的行/列来源） ──
// file/src 为原始脚本路径与全文；span 表按 chunk 存 (line,col,len) 三元组
// （u32×3），与 Chunk.spans 一一对应（emit/emit_label 均 push）。
// rt_unwind 按 (ci,pc) 查 span → 在 src 里取该行文本 → 复刻 ZError::Display。
static PROG: std::sync::Mutex<(String, String, Vec<Vec<[u32; 3]>>)> =
    std::sync::Mutex::new((String::new(), String::new(), Vec::new()));

/// 程序静态表初始化：file/src 全文 + 每 chunk 的 span 三元组数组。
/// spans_ptrs[i] 指向长度为 spans_lens[i]×3 的 u32 数组（line,col,len 交错）。
#[no_mangle]
pub extern "C" fn hn_rt_prog_init(
    file: *const u8,
    file_len: usize,
    src: *const u8,
    src_len: usize,
    spans_ptrs: *const *const u32,
    spans_lens: *const usize,
    nchunks: usize,
) {
    let f = unsafe { std::slice::from_raw_parts(file, file_len) };
    let s = unsafe { std::slice::from_raw_parts(src, src_len) };
    let mut tables = Vec::with_capacity(nchunks);
    for i in 0..nchunks {
        let n = unsafe { spans_lens.add(i).read() };
        let base = unsafe { *spans_ptrs.add(i) };
        let triples = unsafe { std::slice::from_raw_parts(base, n * 3) }
            .chunks_exact(3)
            .map(|t| [t[0], t[1], t[2]])
            .collect();
        tables.push(triples);
    }
    *PROG.lock().unwrap() = (
        String::from_utf8_lossy(f).into_owned(),
        String::from_utf8_lossy(s).into_owned(),
        tables,
    );
}

// ── 错误（M0 骨架：T4 补 span 表 / help / caret 逐字节渲染） ──

// 纯运行时内部类型（不跨 FFI），msg 用 Vec 自持内存，由 Box<HnErr> 统一释放。
// code 存 &str 胖指针（ptr+len 各 u64）：&'static str 字节不一定 NUL 结尾，
// 不能按 C 串读，须用 from_utf8_unchecked 带长度还原。
// ci/pc = 出错指令所在 chunk 索引与指令序（rt_unwind 查 span 表渲染行/列/caret）。
// help：显式 help 文本（镜像 VM 侧 mk_err_with 显式 help 的调用点，如库未加载）；
// None 时渲染阶段回退 hn_help_for 推导（与 VM 的 vm_help_for 一致）。
struct HnErr {
    code_p: u64,
    code_l: u64,
    msg: Vec<u8>,
    ci: u32,
    pc: u32,
    help: Option<String>,
}

fn err_code_str(e: &HnErr) -> &'static str {
    unsafe {
        let s = std::str::from_utf8_unchecked(std::slice::from_raw_parts(
            e.code_p as *const u8,
            e.code_l as usize,
        ));
        // code 仅由运行时内部错误构造传入 &'static str 字面量，此转换安全。
        &*(s as *const str)
    }
}

/// 镜像 vm.rs vm_help_for：按错误码 + 消息子串给出 help 文本（None = 无 help 行）。
fn hn_help_for(code: &str, msg: &str) -> Option<String> {
    let m = |s: &str| msg.contains(s);
    let h = match code {
        "H009" => "check the divisor before dividing".to_string(),
        "H010" => "the result does not fit in a 64-bit signed integer".to_string(),
        "H001" => {
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
        "H006" => "`to_int` on a string requires digits only (optional leading `-`)".to_string(),
        "H007" => "`to_float` on a string requires digits only (optional decimal point)".to_string(),
        "H404" => "check the path and module cache (~/.hone/cache/)".to_string(),
        "H002" => {
            if m("undefined function") {
                "check the spelling".to_string()
            } else if m("dict has no key") {
                "check the key name, or the dict value being destructured".to_string()
            } else if m("unknown field") {
                "check the field name, or the struct definition".to_string()
            } else if m("unknown error field") {
                "error fields: code, message, file, line, col, context".to_string()
            } else {
                return None;
            }
        }
        _ => return None,
    };
    Some(h)
}

/// 完整错误渲染（T4）：逐字节镜像 error.rs ZError::Display + runner 的 eprintln 尾换行。
/// span 按 (ci,pc) 查 PROG 静态表；line==0 时走 plain 分支（无定位行）。
fn err_render(h: u64) -> String {
    unsafe {
        let c = *(h as *const HnCell);
        let e = &*(c.payload as *const HnErr);
        let code = err_code_str(e);
        let msg = String::from_utf8_lossy(&e.msg).into_owned();
        let (file, src, _tables) = PROG.lock().unwrap().clone();
        let span = _tables
            .get(e.ci as usize)
            .and_then(|t| t.get(e.pc as usize))
            .map(|t| (t[0] as usize, t[1] as usize, t[2] as usize));
        // help 优先用错误自带（镜像 VM 显式 help 的调用点），否则回退 hn_help_for 推导。
        // help = Some("") 是「显式无 help」哨兵（见 err_h_explicit_no_help）：跳过回退，
        // 镜像解释器该分支传 None 的行为。
        let help = match e.help.as_deref() {
            Some("") => None,
            Some(h) => Some(h.to_string()),
            None => hn_help_for(code, &msg),
        };
        render_error_with_help(code, &msg, &file, &src, span, help.as_deref())
    }
}

/// render_error 的可选 help 显式版：help 显式传入时不再走 hn_help_for 推导。
fn render_error_with_help(
    code: &str,
    msg: &str,
    file: &str,
    src: &str,
    span: Option<(usize, usize, usize)>,
    help: Option<&str>,
) -> String {
    let mut out = String::new();
    match span {
        None => {
            out.push_str(&format!("error[{}]: {}\n", code, msg));
            if let Some(h) = help {
                out.push_str(&format!("help: {}\n", h));
            }
        }
        Some((line, col, len)) => {
            // line 0 = 无定位（plain 分支，与 ZError::Display 一致）。
            if line == 0 {
                out.push_str(&format!("error[{}]: {}\n", code, msg));
                if let Some(h) = help {
                    out.push_str(&format!("help: {}\n", h));
                }
                return out;
            }
            let line_text = src.lines().nth(line.saturating_sub(1)).unwrap_or("").to_string();
            // Tab 展开为 4 空格，保持 caret 对齐（与 ZError::Display 一致）。
            let mut shown = String::new();
            let mut caret_col = 0usize;
            let chars: Vec<char> = line_text.chars().collect();
            for (i, ch) in chars.iter().enumerate() {
                if i < col.saturating_sub(1) {
                    if *ch == '\t' {
                        shown.push_str("    ");
                        caret_col += 4;
                    } else {
                        shown.push(*ch);
                        caret_col += 1;
                    }
                } else {
                    shown.push(*ch);
                }
            }
            let line_no = format!("{}", line);
            let pad = line_no.len();
            let caret_len = len.max(1).min(chars.len().saturating_sub(col.saturating_sub(1)).max(1));
            out.push_str(&format!("error[{}]: {}\n", code, msg));
            out.push_str(&format!("  --> {}:{}:{}\n", file, line, col));
            out.push_str(&format!("{:>pad$} | {}\n", line_no, shown, pad = pad));
            out.push_str(&format!(
                "{} | {}{}\n",
                " ".repeat(pad),
                " ".repeat(caret_col),
                "^".repeat(caret_len)
            ));
            if let Some(h) = help {
                out.push_str(&format!("help: {}\n", h));
            }
        }
    }
    out
}

/// 错误出口（T4）：完整渲染 → stderr → exit(1)。
/// 用 eprintln! 而非 eprint!：镜像 main.rs 的 `eprintln!("{}", e)`——
/// ZError::Display 内部各行已含 `\n`，eprintln! 再追加一个，最终 stderr 以
/// 空行（两个 `\n`）结尾；nat 产物必须与 interp / VM 输出逐字节一致。
#[no_mangle]
pub extern "C" fn rt_unwind(err: u64) -> ! {
    let s = err_render(err);
    eprintln!("{}", s);
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
        T_LIST => "list",
        T_DICT => "dict",
        T_ENUM => "enum",
        T_FUTURE => "future",
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

/// str+str 拼接（镜像 v_add 的 Str 臂）：合并字节建新句柄（rc=1）。
fn str_concat(xh: u64, yh: u64) -> u64 {
    let cx = cell(xh);
    let cy = cell(yh);
    let bx = unsafe { std::slice::from_raw_parts(cx.a as *const u8, cx.b as usize) };
    let by = unsafe { std::slice::from_raw_parts(cy.a as *const u8, cy.b as usize) };
    let mut out: Vec<u8> = Vec::with_capacity(bx.len() + by.len());
    out.extend_from_slice(bx);
    out.extend_from_slice(by);
    make_cell_str(out)
}

/// str 相等（镜像 v_eq 的 Str 臂）。Hone 字符串恒为合法 UTF-8（字面量 / 拼接产物），
/// 码点序列相等 ⇔ 字节序列相等，故直接比字节（免 lossy 转换，且 wrapper 侧裸 rustc
/// 下 std::str::from_utf8_lossy 路径解析不稳定，绕开）。
fn str_eq(xh: u64, yh: u64) -> bool {
    let cx = cell(xh);
    let cy = cell(yh);
    let bx = unsafe { std::slice::from_raw_parts(cx.a as *const u8, cx.b as usize) };
    let by = unsafe { std::slice::from_raw_parts(cy.a as *const u8, cy.b as usize) };
    bx == by
}

fn arith_sym(op: u64) -> &'static str {
    match op {
        0 => "+",
        1 => "-",
        2 => "*",
        3 => "/",
        _ => "%",
    }
}

// 算术（镜像 vm.rs v_add/v_sub/v_mul/v_div/v_mod：int/float 全组合 + str+str 拼接）。
// 纯 Hnv 运算，产出 Hnv（由调用方决定建句柄 / 释放输入）。ci/pc = 出错指令位置。
fn arith_inner(op: u64, x: Hnv, y: Hnv, ci: u32, pc: u32) -> Hnv {
    match (x.t, y.t) {
        // str+str：Add 在 rt_binop 句柄路径拦截（str_concat）；sub/mul/div/mod 镜像
        // v_* catchall 文案（checker 编译期已静态拒绝，防御性保留）。
        (T_STR, T_STR) => {
            return err_h(
                "H001",
                &format!("cannot apply `{}` to `str` and `str`", arith_sym(op)),
                ci,
                pc,
            )
        }
        (T_INT, T_INT) => {
            let (a, b) = (x.a as i64, y.a as i64);
            let res = match op {
                0 => a.checked_add(b),
                1 => a.checked_sub(b),
                2 => a.checked_mul(b),
                3 => {
                    if b == 0 {
                        return err_h("H009", "division by zero", ci, pc);
                    }
                    a.checked_div(b)
                }
                4 => {
                    if b == 0 {
                        return err_h("H009", "division by zero", ci, pc);
                    }
                    a.checked_rem(b)
                }
                _ => unreachable!(),
            };
            match res {
                Some(v) => Hnv::int(v),
                None => err_h("H010", "integer overflow", ci, pc),
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
                        return err_h("H009", "division by zero", ci, pc);
                    }
                    a / b
                }
                4 => {
                    if b == 0.0 {
                        return err_h("H009", "division by zero", ci, pc);
                    }
                    a % b
                }
                _ => unreachable!(),
            };
            Hnv::float(res)
        }
        // int×float 混合（镜像 v_sub/v_mul/v_div 的混合臂；v_add/v_mod 无混合臂 →
        // 落入 catchall H001。checker 静态拒绝，此为防御性对齐）：
        // sub/mul/div → float 结果；div 除零检查镜像 v_div（Float 侧判 0.0，Int 侧判 0）。
        (T_INT, T_FLOAT) | (T_FLOAT, T_INT) if op == 1 || op == 2 || op == 3 => {
            let (a, b) = if x.t == T_INT {
                (x.a as i64 as f64, f64::from_bits(y.a))
            } else {
                (f64::from_bits(x.a), y.a as i64 as f64)
            };
            if op == 3 {
                // v_div 混合臂：b 为 0（float 侧 b==0.0 / int 侧 b==0）→ H009。
                if b == 0.0 {
                    return err_h("H009", "division by zero", ci, pc);
                }
                Hnv::float(a / b)
            } else if op == 1 {
                Hnv::float(a - b)
            } else {
                Hnv::float(a * b)
            }
        }
        _ => err_h(
            "H001",
            &format!("cannot apply `{}` to `{}` and `{}`", arith_sym(op), type_name(x.t), type_name(y.t)),
            ci,
            pc,
        ),
    }
}

fn neg_h(x: Hnv, ci: u32, pc: u32) -> Hnv {
    match x.t {
        T_INT => match (x.a as i64).checked_neg() {
            Some(v) => Hnv::int(v),
            None => err_h("H010", "integer overflow", ci, pc),
        },
        T_FLOAT => Hnv::float(-f64::from_bits(x.a)),
        other => err_h(
            "H001",
            &format!("unary `-` requires a number, got `{}`", type_name(other)),
            ci,
            pc,
        ),
    }
}

fn not_h(x: Hnv, ci: u32, pc: u32) -> Hnv {
    match x.t {
        T_BOOL_TRUE => Hnv::bool_(false),
        T_BOOL_FALSE => Hnv::bool_(true),
        other => err_h(
            "H001",
            &format!("`!` requires a `bool`, got `{}`", type_name(other)),
            ci,
            pc,
        ),
    }
}

fn cmpv_inner(op: u64, x: Hnv, y: Hnv, ci: u32, pc: u32) -> Hnv {
    match op {
        7 | 8 => {
            let eq = match (x.t, y.t) {
                (T_INT, T_INT) => x.a == y.a,
                (T_FLOAT, T_FLOAT) => x.a == y.a,
                (T_CHAR, T_CHAR) => x.a == y.a,
                (T_NULL, T_NULL) => true,
                // bool 恒可等（镜像 vm.rs v_eq：Bool==Bool → Ok(x==y)，含异值 → false）。
                (T_BOOL_TRUE, T_BOOL_TRUE) | (T_BOOL_FALSE, T_BOOL_FALSE) => true,
                (T_BOOL_TRUE, T_BOOL_FALSE) | (T_BOOL_FALSE, T_BOOL_TRUE) => false,
                (T_STR, T_STR) => unreachable!(), // ==/!= 在 rt_binop 句柄路径拦截（str_eq）
                _ => {
                    return err_h(
                        "H001",
                        &format!("cannot compare `{}` with `{}`", type_name(x.t), type_name(y.t)),
                        ci,
                        pc,
                    )
                }
            };
            Hnv::bool_(if op == 7 { eq } else { !eq })
        }
        // 镜像 vm.rs v_cmp：仅 int-int / float-float / char-char；str 无 < 臂（→ H001）。
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
                        None => return err_h("H001", "cannot compare NaN values", ci, pc),
                    }
                }
                _ => {
                    return err_h(
                        "H001",
                        &format!("cannot compare `{}` with `{}`", type_name(x.t), type_name(y.t)),
                        ci,
                        pc,
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
// ci/pc = 出错指令位置（rt_unwind 查 span 表渲染）。
fn err_h(code: &'static str, msg: &str, ci: u32, pc: u32) -> Hnv {
    err_h_help(code, msg, ci, pc, None)
}

/// 带显式 help 的错误构造（镜像 VM 侧 mk_err_with 的显式 help 调用点，如库未加载 H404）。
fn err_h_help(code: &'static str, msg: &str, ci: u32, pc: u32, help: Option<String>) -> Hnv {
    let e = Box::into_raw(Box::new(HnErr {
        code_p: code.as_ptr() as u64,
        code_l: code.len() as u64,
        msg: msg.as_bytes().to_vec(),
        ci,
        pc,
        help,
    }));
    Hnv { t: T_ERR, a: e as u64, b: 0 }
}

/// 「本分支确定无 help 行」的哨兵 help。解释器某些分支显式传 None（如 builtins.rs 的
/// `abs` 溢出），渲染层不得回退 hn_help_for凭空补一行help——否则 nat 会比解释器多
/// 出一行。约定：help = Some(空串) 表示「显式无 help」，err_render 据此跳过回退。
fn err_h_explicit_no_help(code: &'static str, msg: &str, ci: u32, pc: u32) -> Hnv {
    err_h_help(code, msg, ci, pc, Some(String::new()))
}

/// (ci,pc) 定位行文本（去尾空白）——catch e 里 `e.context` 字段取值，与 VM mk_err_with /
/// 解释器 ErrorObj 的 context 填充一致（src.lines().nth(line-1).trim_end()）。
fn err_context(ci: u32, pc: u32) -> String {
    let (file, src, tables) = PROG.lock().unwrap().clone();
    let _ = file;
    let (line, _col, _len) = tables
        .get(ci as usize)
        .and_then(|t| t.get(pc as usize))
        .map(|t| (t[0] as usize, t[1] as usize, t[2] as usize))
        .unwrap_or((0, 0, 0));
    src.lines()
        .nth(line.saturating_sub(1))
        .unwrap_or("")
        .trim_end()
        .to_string()
}

/// 用 (ci,pc) 定位构造错误句柄（镜像 VM mk_err 的 context 填充）。
/// code 恒为错误码字面量（&'static str，如 "H600"），指向静态内存、永不释放；
/// msg 为运行期文案（String，自持内存经 into_bytes）。code 若用 String 局部变量，
/// 函数返回即释放、code_p 悬空——err_code_str 按 (code_p,code_l) 还原 &str 依赖静态内存。
fn err_h_ctx(code: &'static str, msg: String, ci: u32, pc: u32) -> u64 {
    let e = Box::into_raw(Box::new(HnErr {
        code_p: code.as_ptr() as u64,
        code_l: code.len() as u64,
        msg: msg.into_bytes(),
        ci,
        pc,
        help: None,
    }));
    make_cell(Hnv { t: T_ERR, a: e as u64, b: 0 })
}

/// throw 归一（镜像 VM Instr::Throw / 解释器 Stmt::Throw）：
/// str→H600（定位取 throw 指令）；error 重抛同文件保留原始定位；其余→H001。
/// 返回值句柄由 lowering 消费（移入 catch 槽 / 经 err_ret 返回）。
#[no_mangle]
pub extern "C" fn rt_throw(v: u64, ci: u64, pc: u64) -> u64 {
    let (ci, pc) = (ci as u32, pc as u32);
    let c = cell(v);
    match c.t {
        T_STR => {
            let m = String::from_utf8_lossy(str_bytes(v)).into_owned();
            cell_release(v);
            err_h_ctx("H600", m, ci, pc)
        }
        T_ERR => {
            // 重抛：同文件保留原始 (ci,pc) 定位；len 归一为 1（镜像 VM rethrow span len=1）。
            // 单模块产物 file 恒等于 PROG.file，无跨文件退化分支。code 由 (code_p,code_l)
            // 还原为 &'static str（不借用 e），msg 为自持 String——unsafe 块内取出后复用。
            let (code, msg, e_ci, e_pc) = unsafe {
                let e = &*(c.payload as *const HnErr);
                (
                    err_code_str(e),
                    String::from_utf8_lossy(&e.msg).into_owned(),
                    e.ci,
                    e.pc,
                )
            };
            cell_release(v);
            err_h_ctx(code, msg, e_ci, e_pc)
        }
        other => {
            cell_release(v);
            make_cell(err_h(
                "H001",
                &format!("`throw` accepts a `str` or `error`, got `{}`", type_name(other)),
                ci,
                pc,
            ))
        }
    }
}

/// ThrowStr 归一（镜像 VM Instr::ThrowStr）：字面字符串→H600。
/// 注：VM ThrowStr 的 ErrorObj.context 恒空串，与「按 (ci,pc) 推导出该行」可能不同；
/// 该指令仅在 match 无匹配分支时产生（M2），T6 阶段无运行期用例，context 字段差异留待 M2 对齐。
#[no_mangle]
pub extern "C" fn rt_throwstr(sidx: u64, ci: u64, pc: u64) -> u64 {
    let (ci, pc) = (ci as u32, pc as u32);
    let m = strtab_name(sidx as u32);
    err_h_ctx("H600", m, ci, pc)
}

/// M1：Raise 的「本分支无 help 行」哨兵 strtab 索引（u64 传参，取 u64::MAX 而非 -1，
/// 避免 rustc 的 useless-comparison 警告；索引本身不会取到）。
const HELP_NONE_SIDX: u64 = u64::MAX;

/// M1：Raise —— 运行期直接产生一个错误句柄（错误码 / 消息 / help 三段文本均取自 strtab）。
/// 用于镜像解释器「求值期按容器实际类型分支报错」的语义（如推导式 `for k, v in`
/// 遇到列表源时报错）。help_sidx = HELP_NONE_SIDX（u64::MAX）表示「本分支无 help 行」，
/// 此时不渲染 help（镜像解释器该分支传 None）。返回的错误句柄交由 lowering 的
/// check_err 路由（有 handler→catch 槽；无→跨帧传播）。
#[no_mangle]
pub extern "C" fn rt_raise(code_sidx: u64, msg_sidx: u64, help_sidx: u64, ci: u64, pc: u64) -> u64 {
    let (ci, pc) = (ci as u32, pc as u32);
    let code = strtab_name(code_sidx as u32);
    let msg = strtab_name(msg_sidx as u32);
    let help = if help_sidx == HELP_NONE_SIDX {
        // 哨兵：无 help 行。用 Some("") 阻止渲染层回退 hn_help_for凭空补 help。
        Some(String::new())
    } else {
        Some(strtab_name(help_sidx as u32))
    };
    // err_h_help 收&'static str 的错误码，而此处 code 来自字符串表（动态）——
    // 故先把字符串表的错误码归一为 &'static str（HnErr.code_p 契约要求静态生命周期，
    // 直接存局部 String 的指针会在函数返回后悬垂）。错误码取值集合固定且极小
    // （见 vm.rs 的 Raise 发射点：推导式双变量 H001、match 无匹配分支 H005），
    // 未命中时回退 H990 内部错误——**新增 Raise 码必须同步此表**，否则被静默归为 H990。
    let code: &'static str = match code.as_str() {
        "H001" => "H001",
        "H002" => "H002",
        "H005" => "H005",
        "H009" => "H009",
        "H010" => "H010",
        "H999" => "H999",
        _ => "H990",
    };
    let e = Box::into_raw(Box::new(HnErr {
        code_p: code.as_ptr() as u64,
        code_l: code.len() as u64,
        msg: msg.into_bytes(),
        ci,
        pc,
        help,
    }));
    make_cell(Hnv { t: T_ERR, a: e as u64, b: 0 })
}

/// 字段访问：镜像 interp `field_value` / vm `v_field` 的分支顺序——
/// dict（G1.3：按键取字段，struct 实例同走此路径）→ error（错误属性）→ 其他类型 H001。
/// 消费 obj 引用（rc-1）后返回新句柄（调用方持有所有权）。
#[no_mangle]
pub extern "C" fn rt_field(obj: u64, fsidx: u64, ci: u64, pc: u64) -> u64 {
    let (ci, pc) = (ci as u32, pc as u32);
    let field = strtab_name(fsidx as u32);
    let c = cell(obj);
    // dict / struct 实例：按键取字段（跳过隐藏 `__struct__` 标记键），缺失报 H002 并列出实有键。
    if c.pkind == P_DICT {
        // 先在 borrow 存活期内取齐「命中的值句柄」或「实有键列表」，再释放 obj——
        // 否则 cell_release 可能析构 cell，其 payload 随即失效（use-after-free）。
        let d = unsafe { &*(c.payload as *const HnDict) };
        let found = d
            .pairs
            .iter()
            .find(|(k, _)| !is_hidden_key(k) && *k == field)
            .map(|(_, v)| *v);
        let keys = if found.is_none() {
            d.pairs
                .iter()
                .filter(|(k, _)| !is_hidden_key(k))
                .map(|(k, _)| k.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            String::new()
        };
        cell_release(obj);
        return match found {
            Some(v) => rt_retain(v),
            None => make_cell(err_h_help(
                "H002",
                &format!("unknown field `{}` (dict/struct has {})", field, keys),
                ci,
                pc,
                Some("check the field name, or the struct definition".to_string()),
            )),
        };
    }
    if c.t != T_ERR {
        cell_release(obj);
        return make_cell(err_h_help(
            "H001",
            &format!("field access `.{}` requires an `error` value, got `{}`", field, type_name(c.t)),
            ci,
            pc,
            Some("only error values (catch variables) support field access".to_string()),
        ));
    }
    // code 由 (code_p,code_l) 还原为 &'static str（不借用 e），msg/位置为自持值——
    // unsafe 块内取出后复用（镜像 err_render 的 raw 解引用需 unsafe 块）。
    let (code, msg, e_ci, e_pc) = unsafe {
        let e = &*(c.payload as *const HnErr);
        (
            err_code_str(e).to_string(),
            String::from_utf8_lossy(&e.msg).into_owned(),
            e.ci,
            e.pc,
        )
    };
    let (file, _src, tables) = PROG.lock().unwrap().clone();
    let (line, col, _len) = tables
        .get(e_ci as usize)
        .and_then(|t| t.get(e_pc as usize))
        .map(|t| (t[0] as usize, t[1] as usize, t[2] as usize))
        .unwrap_or((0, 0, 0));
    let ctx = err_context(e_ci, e_pc);
    cell_release(obj);
    match field.as_str() {
        "code" => make_cell_str(code.into_bytes()),
        "message" => make_cell_str(msg.into_bytes()),
        "file" => make_cell_str(file.into_bytes()),
        "line" => make_cell(Hnv::int(line as i64)),
        "col" => make_cell(Hnv::int(col as i64)),
        "context" => make_cell_str(ctx.into_bytes()),
        other => make_cell(err_h_help(
            "H002",
            &format!("unknown error field `{}`", other),
            ci,
            pc,
            Some("error fields: code, message, file, line, col, context".to_string()),
        )),
    }
}

// ── 函数注册表与调用分发（T5：rt_call 名分发 + lambda） ──
// 注册表经 wrapper 提供（M3 移入 object rodata，G3.5）：
//   FUNCS：全局函数表 (strtab_idx, ci, nparams)，与 vm.rs func_map 对应；
//   LOCALS：每 chunk 的局部变量表 (strtab_idx, reg)，rt_call 查「当前帧局部变量」
//     （lambda 闭包 / 函数名字符串，镜像 Vm::do_call 第一步）；
//   CHUNK_NPARAMS：每 chunk 形参数（lambda 参数校验用）；
//   FN_PTRS：每 chunk 生成的统一签名函数指针（object 导出，wrapper 声明并聚合）。
// chunk 统一签名：extern "C" fn(argptr, nargs, capsptr, ncap) -> u64
//   ——用户函数：capsptr=0/ncap=0；lambda：capsptr 指向捕获句柄数组。

type FnPtr = unsafe extern "C" fn(u64, u64, u64, u64) -> u64;

static FUNCS: std::sync::Mutex<Vec<(u32, u32, u32)>> = std::sync::Mutex::new(Vec::new());
static LOCALS: std::sync::Mutex<Vec<Vec<(u32, u32)>>> = std::sync::Mutex::new(Vec::new());
static CHUNK_NPARAMS: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());
// G2.4：每 chunk 形参**名**（strtab 索引序列）——缺参报错要指名道姓（镜像解释器
// 「missing argument `c` of `f`」）。CHUNK_NPARAMS 只有个数，这里补名字。
static CHUNK_PARAM_NAMES: std::sync::Mutex<Vec<Vec<u32>>> = std::sync::Mutex::new(Vec::new());
// G2.8：每 chunk 形参默认值位（第 i 位=1 表示第 i 个形参带默认值）——参数个数校验据此
// 放宽（省略的形参由 chunk 序言求默认值填入，镜像 VM call_chunk 的 has_default）。
static CHUNK_PARAM_DEFAULTS: std::sync::Mutex<Vec<Vec<bool>>> = std::sync::Mutex::new(Vec::new());
static FN_PTRS: std::sync::Mutex<Vec<FnPtr>> = std::sync::Mutex::new(Vec::new());
// G2.1：struct 定义表 structs[i] = (strtab_idx, 字段数)，字段名按定义序存
// STRUCT_FIELDS[i]（strtab 索引）。与 vm.rs struct_defs 对应（构造时字段名序列
// 决定实例键序；只存名，readonly 标志 VM 侧同样不存——字段写入 VM 编译期 H999）。
static STRUCT_DEFS: std::sync::Mutex<Vec<(u32, u32)>> = std::sync::Mutex::new(Vec::new());
static STRUCT_FIELDS: std::sync::Mutex<Vec<Vec<u32>>> = std::sync::Mutex::new(Vec::new());
// G2.2：enum 定义表（镜像 vm.rs enum_defs）。ENUM_DEFS[i] = (枚举名 strtab 索引, 变体数)；
// ENUM_VARIANTS[i][j] = (变体名 strtab 索引, 载荷个数)。构造分发用；载荷类型不在运行期
// 校验（checker 已静态校验，与 struct 字段类型同口径）。
static ENUM_DEFS: std::sync::Mutex<Vec<(u32, u32)>> = std::sync::Mutex::new(Vec::new());
static ENUM_VARIANTS: std::sync::Mutex<Vec<Vec<(u32, u32)>>> = std::sync::Mutex::new(Vec::new());
// G2.5：别名表 ALIASES[i] = (原名 strtab 索引, 新名 strtab 索引)（镜像 vm.rs aliases，
// 方向与 VM 相反：VM 存 (original, new_name)，这里按「新名 → 原名」存便于解析）。
// 指向用户函数的别名由 VM 编译器展开进 func_map（→ wrapper 的 NAT_FUNCS），
// 这里补的是**内置 / 点号内置 / struct 构造**等非函数目标（VM 侧走 do_call 的
// resolve_alias 兜底）。别名可叠加，解析轮数上限同 VM（aliases.len()）。
static ALIASES: std::sync::Mutex<Vec<(u32, u32)>> = std::sync::Mutex::new(Vec::new());

/// 全局函数表：funcs[i] = (strtab_idx, ci, nparams)。
#[no_mangle]
pub extern "C" fn hn_rt_funcs_init(funcs: *const u32, n: usize) {
    let mut t = FUNCS.lock().unwrap();
    t.clear();
    for i in 0..n {
        let f = unsafe { *funcs.add(i * 3) };
        let c = unsafe { *funcs.add(i * 3 + 1) };
        let p = unsafe { *funcs.add(i * 3 + 2) };
        t.push((f, c, p));
    }
}

/// struct 定义表：defs[i] = (strtab_idx, 字段数) 交错；字段名扁平数组按定义序
/// （fields 指针 + 每 struct 字段数 counts 前缀和定位）。G2.1 起 call_global 据此
/// 构造带 `__struct__` 标记键的 dict 实例（与 VM do_call 的 struct 臂同序）。
#[no_mangle]
pub extern "C" fn hn_rt_structs_init(defs: *const u32, fields: *const u32, counts: *const u32, n: usize) {
    let mut t = STRUCT_DEFS.lock().unwrap();
    let mut f = STRUCT_FIELDS.lock().unwrap();
    t.clear();
    f.clear();
    let mut off = 0usize;
    for i in 0..n {
        let s = unsafe { *defs.add(i * 2) };
        let cnt = unsafe { *defs.add(i * 2 + 1) } as usize;
        t.push((s, cnt as u32));
        let mut v = Vec::with_capacity(cnt);
        for j in 0..cnt {
            v.push(unsafe { *fields.add(off + j) });
        }
        f.push(v);
        off += cnt;
    }
}

/// 每 chunk 形参数：nparams[i] = chunk i 的参数个数。
#[no_mangle]
pub extern "C" fn hn_rt_chunk_nparams(nparams: *const u32, n: usize) {
    let mut t = CHUNK_NPARAMS.lock().unwrap();
    t.clear();
    for i in 0..n {
        t.push(unsafe { *nparams.add(i) });
    }
}

/// G2.4：每 chunk 形参名（strtab 索引）：names 为按 chunk 顺序拼接的扁平数组，
/// counts[i] = chunk i 的形参个数（与 CHUNK_NPARAMS 同序）。
#[no_mangle]
pub extern "C" fn hn_rt_param_names(names: *const u32, counts: *const u32, nchunks: usize) {
    let mut t = CHUNK_PARAM_NAMES.lock().unwrap();
    t.clear();
    let mut off = 0usize;
    for i in 0..nchunks {
        let cnt = unsafe { *counts.add(i) } as usize;
        let mut v = Vec::with_capacity(cnt);
        for _ in 0..cnt {
            v.push(unsafe { *names.add(off) });
            off += 1;
        }
        t.push(v);
    }
}

/// G2.8：每 chunk 形参默认值位（wrapper 传 0/1 序列 + 每 chunk 形参个数）。
#[no_mangle]
pub extern "C" fn hn_rt_param_defaults(bits: *const u32, counts: *const u32, nchunks: usize) {
    let mut t = CHUNK_PARAM_DEFAULTS.lock().unwrap();
    t.clear();
    let mut off = 0usize;
    for i in 0..nchunks {
        let cnt = unsafe { *counts.add(i) } as usize;
        let mut v = Vec::with_capacity(cnt);
        for _ in 0..cnt {
            v.push(unsafe { *bits.add(off) } != 0);
            off += 1;
        }
        t.push(v);
    }
}

/// chunk i 的第 i 个形参是否带默认值（无该形参 → false）。
fn param_has_default(ci: u32, i: usize) -> bool {
    let t = CHUNK_PARAM_DEFAULTS.lock().unwrap();
    t.get(ci as usize)
        .and_then(|v| v.get(i))
        .copied()
        .unwrap_or(false)
}

/// chunk i 的第 j 个形参名（缺失时返回空串——调用方只在报错文案里用它）。
fn param_name(ci: u32, j: usize) -> String {
    let t = CHUNK_PARAM_NAMES.lock().unwrap();
    match t.get(ci as usize).and_then(|v| v.get(j)) {
        Some(&sidx) => strtab_name(sidx),
        None => String::new(),
    }
}

/// 参数个数校验错误（镜像解释器 call_fn / call_lambda 的两类文案 + 定位 + help）。
/// `want` 形参总数、`got` 实参数（少参时首个未填充形参即第 got 个）、`name` 函数名
/// （lambda 传 "lambda"）；`fci` 是被调 chunk（取形参名用），`ci/pc` 是调用点定位。
fn arg_count_err(name: &str, want: usize, got: usize, fci: u32, ci: u32, pc: u32) -> Hnv {
    if got > want {
        // 解释器文案：函数名带反引号（`` `f` expects at most …``），lambda 不带。
        let help = if name == "lambda" {
            "pass exactly the declared number of arguments"
        } else {
            "check the number of arguments passed"
        };
        let msg = if name == "lambda" {
            format!("lambda expects at most {} arguments, got {}", want, got)
        } else {
            format!("`{}` expects at most {} arguments, got {}", name, want, got)
        };
        err_h_help("H011", &msg, ci, pc, Some(help.to_string()))
    } else {
        let pname = param_name(fci, got);
        let msg = if name == "lambda" {
            format!("missing argument `{}`", pname)
        } else {
            format!("missing argument `{}` of `{}`", pname, name)
        };
        err_h_help(
            "H011",
            &msg,
            ci,
            pc,
            Some("pass the required argument, or give the parameter a default value".to_string()),
        )
    }
}

/// 每 chunk 局部变量表：locs[i] = (strtab_idx, reg) 交错数组；counts[i] = chunk i 的条目数。
#[no_mangle]
pub extern "C" fn hn_rt_locals_init(locs: *const u32, counts: *const u32, nchunks: usize) {
    let mut t = LOCALS.lock().unwrap();
    t.clear();
    let mut off = 0usize;
    for i in 0..nchunks {
        let cnt = unsafe { *counts.add(i) } as usize;
        let mut v = Vec::with_capacity(cnt);
        for j in 0..cnt {
            let s = unsafe { *locs.add((off + j) * 2) };
            let r = unsafe { *locs.add((off + j) * 2 + 1) };
            v.push((s, r));
        }
        t.push(v);
        off += cnt;
    }
}

/// chunk 函数指针表：ptrs[i] = chunk i 的统一签名入口（fn ptr 转 usize 传递）。
#[no_mangle]
pub extern "C" fn hn_rt_fn_ptrs_init(ptrs: *const usize, n: usize) {
    let mut t = FN_PTRS.lock().unwrap();
    t.clear();
    for i in 0..n {
        let p = unsafe { *ptrs.add(i) };
        // fn 指针与 usize 同宽：transmute 往返（wrapper 侧 fn as usize 传入）。
        let f: FnPtr = unsafe { std::mem::transmute(p) };
        t.push(f);
    }
}

/// enum 定义表：defs[i] = (枚举名 strtab 索引, 变体数) 交错；变体 (名 sidx, 载荷数)
/// 按 (variants 指针, counts 前缀和) 扁平定位。G2.2 起 call_global 的点号分发据此
/// 构造 enum 值（与 VM do_call 的 enum 臂同序）。
#[no_mangle]
pub extern "C" fn hn_rt_enums_init(defs: *const u32, variants: *const u32, counts: *const u32, n: usize) {
    let mut t = ENUM_DEFS.lock().unwrap();
    let mut v = ENUM_VARIANTS.lock().unwrap();
    t.clear();
    v.clear();
    let mut off = 0usize;
    for i in 0..n {
        let s = unsafe { *defs.add(i * 2) };
        let cnt = unsafe { *defs.add(i * 2 + 1) } as usize;
        t.push((s, cnt as u32));
        let mut vs = Vec::with_capacity(cnt);
        for j in 0..cnt {
            let vn = unsafe { *variants.add(off * 2) };
            let vp = unsafe { *variants.add(off * 2 + 1) } as u32;
            vs.push((vn, vp));
            off += 1;
        }
        v.push(vs);
    }
}

/// G2.5：别名表初始化（wrapper 传「新名, 原名」交错的 strtab 索引）。
#[no_mangle]
pub extern "C" fn hn_rt_aliases_init(pairs: *const u32, n: usize) {
    let mut t = ALIASES.lock().unwrap();
    t.clear();
    for i in 0..n {
        // 传入顺序（新名, 原名）→ 内部存（新名, 原名）
        let new_s = unsafe { *pairs.add(i * 2) };
        let orig_s = unsafe { *pairs.add(i * 2 + 1) };
        t.push((new_s, orig_s));
    }
}

/// 名字解析别名（可叠加；未定义别名返回 None）。限轮数防环，同 vm.rs resolve_alias。
fn resolve_alias_name(name: &str) -> Option<String> {
    let t = ALIASES.lock().unwrap();
    if !t.iter().any(|(n, _)| strtab_name_eq(*n, name)) {
        return None;
    }
    let mut cur = name.to_string();
    for _ in 0..=t.len() {
        match t.iter().find(|(n, _)| strtab_name_eq(*n, &cur)) {
            Some(&(_, orig)) => cur = strtab_name(orig),
            None => break,
        }
    }
    drop(t);
    Some(cur)
}

/// 字符串表第 sidx 项的名字（UTF-8 字节）。
fn strtab_name(sidx: u32) -> String {
    let t = STRTAB.lock().unwrap();
    let h = t[sidx as usize];
    drop(t);
    String::from_utf8_lossy(str_bytes(h)).into_owned()
}

/// 全局函数表按名查找 → (ci, nparams)。
fn func_lookup(name: &str) -> Option<(u32, u32)> {
    let t = FUNCS.lock().unwrap();
    for &(s, c, p) in t.iter() {
        if strtab_name_eq(s, name) {
            return Some((c, p));
        }
    }
    None
}

/// strtab 第 sidx 项与 name 字节相等（免建 String）。
fn strtab_name_eq(sidx: u32, name: &str) -> bool {
    let t = STRTAB.lock().unwrap();
    let h = t[sidx as usize];
    drop(t);
    str_bytes(h) == name.as_bytes()
}

/// 读实参数组的第 i 个句柄（借用）。
#[inline]
fn arg_at(argptr: u64, i: usize) -> u64 {
    unsafe { *(argptr as *const u64).add(i) }
}

/// 释放实参数组（rt_call 的消费路径：builtin/错误/参数校验失败）。
#[inline]
fn drop_args(argptr: u64, n: u64) {
    for i in 0..n as usize {
        cell_release(arg_at(argptr, i));
    }
}

/// 建 lambda 值句柄（rc=1）：捕获句柄逐项 retain（按值捕获，镜像 VM MakeLambda 的 clone）。
#[no_mangle]
pub extern "C" fn rt_makelambda(ci: u64, caps_ptr: u64, ncap: u64) -> u64 {
    let mut caps: Vec<u64> = Vec::with_capacity(ncap as usize);
    for i in 0..ncap as usize {
        let h = unsafe { *(caps_ptr as *const u64).add(i) };
        caps.push(rt_retain(h));
    }
    let b = Box::into_raw(Box::new(HnFnBox {
        ci: ci as u32,
        caps,
    }));
    let c = Box::into_raw(Box::new(HnCell {
        rc: 1,
        t: T_FN,
        a: ci,
        b: 0,
        pkind: P_FN,
        payload: b as u64,
    }));
    leak_check_inc();
    c as u64
}

/// 调 chunk 索引（用户函数）：实参所有权转移给 callee 帧（成功路径不 drop）。
/// 参数数不匹配 → H011（镜像 Vm::call_chunk：plain 无定位，line=0）。
fn call_chunk_idx(
    fci: u32,
    nparams: u32,
    argptr: u64,
    nargs: u64,
    name: &str,
    ci: u32,
    pc: u32,
) -> u64 {
    // G2.8：实参不足且缺失位带默认值 → 放行（chunk 序言会求默认值填入）。
    let missing_required = nargs < nparams as u64 && !param_has_default(fci, nargs as usize);
    if nargs > nparams as u64 || missing_required {
        drop_args(argptr, nargs);
        // G2.4：消息 / 定位 / help 对齐解释器（原先中文硬编码、无定位无 help，
        // 经运算符重载路径可达）。
        return make_cell(arg_count_err(name, nparams as usize, nargs as usize, fci, ci, pc));
    }
    let fptr = FN_PTRS.lock().unwrap()[fci as usize];
    unsafe { fptr(argptr, nargs, 0, 0) }
}

/// 调 lambda 值：捕获 retain 出一份独立引用给 callee 帧（callee 返回时释放，
/// lambda 值本身仍持有——镜像 Vm::call_lambda 的 clone 填帧）。实参所有权转移。
/// 参数数不匹配 → H011（带调用点 span，镜像 call_lambda）。
fn call_fn_h(h: u64, argptr: u64, nargs: u64, ci: u32, pc: u32) -> u64 {
    let c = cell(h);
    let b = unsafe { &*(c.payload as *const HnFnBox) };
    let fci = b.ci;
    let nparams = CHUNK_NPARAMS.lock().unwrap()[fci as usize];
    // G2.8：同 call_chunk_idx——带默认值的形参省略不算参数错误。
    let missing_required = nargs < nparams as u64 && !param_has_default(fci, nargs as usize);
    if nargs > nparams as u64 || missing_required {
        drop_args(argptr, nargs);
        // G2.4：消息 / 定位 / help 对齐解释器 call_lambda。
        return make_cell(arg_count_err(
            "lambda",
            nparams as usize,
            nargs as usize,
            fci,
            ci,
            pc,
        ));
    }
    // 捕获引用转移给 callee：retain 出独立引用后传数组；callee 返回时释放。
    let mut caps: Vec<u64> = Vec::with_capacity(b.caps.len());
    for &ch in &b.caps {
        caps.push(rt_retain(ch));
    }
    let fptr = FN_PTRS.lock().unwrap()[fci as usize];
    unsafe { fptr(argptr, nargs, caps.as_ptr() as u64, caps.len() as u64) }
}

/// 全局名分发（无局部变量优先路径）：点号名 / 用户函数 / 内置 / H002。
fn call_global(name: &str, argptr: u64, nargs: u64, ci: u32, pc: u32) -> u64 {
    // G2.5：别名先解析（可叠加）——镜像 vm.rs do_call 把 resolve_alias 放在分派最前。
    // 指向用户函数的别名已由编译器展开进 func_map；这里覆盖内置 / 点号内置 / struct 构造。
    let resolved = resolve_alias_name(name);
    let resolved_ref = resolved.as_deref();
    let name = resolved_ref.unwrap_or(name);
    // G2.6：async 函数调用（镜像 VM do_call 序：locals 之后、点号分发之前）——
    // 后台线程执行函数体，立即返回 future；等待由 Await 指令完成。
    if is_async_fn(name) {
        return spawn_async(name, argptr, nargs, ci, pc);
    }
    // 点号名：G2.2 枚举构造（Color.Red，镜像 VM do_call 序：enum → 函数 → 内置 → 库未加载）。
    if let Some((head, tail)) = name.split_once('.') {
        if let Some((_, name_sidx, vs)) = enum_lookup(head) {
            if vs.iter().any(|(v, _)| strtab_name_eq(*v, tail)) {
                let vsidx = vs.iter().find(|(v, _)| strtab_name_eq(*v, tail)).unwrap().0;
                // 载荷 = 全部实参（retain 进 box；实参数组引用在此释放，与 Call lowering 约定一致）。
                let mut payload = Vec::with_capacity(nargs as usize);
                for i in 0..nargs as usize {
                    payload.push(rt_retain(arg_at(argptr, i)));
                }
                drop_args(argptr, nargs);
                return make_cell_enum(name_sidx, vsidx, payload);
            }
        }
        if let Some((fci, np)) = func_lookup(name) {
            return call_chunk_idx(fci, np, argptr, nargs, name, ci, pc);
        }
        // G2.9：点号内置族（time.*/random.*/... 可移植实现 + 不可移植族 H999 登记）。
        // 返回 None = 未知点号名（load 库方法调用）→ 保留 H404（与 VM/interp 一致）。
        if let Some(r) = call_dotted(name, argptr, nargs, ci, pc) {
            return r;
        }
        let lib = &name[..name.rfind('.').unwrap_or(name.len())];
        drop_args(argptr, nargs);
        return make_cell(err_h_help(
            "H404",
            &format!("library `{}` is not loaded", lib),
            ci,
            pc,
            Some(format!(
                "add `load \"path/to/lib\" as {};` or `plugin.load(path, \"{}\")` before calling",
                lib, lib
            )),
        ));
    }
    if let Some((fci, np)) = func_lookup(name) {
        return call_chunk_idx(fci, np, argptr, nargs, name, ci, pc);
    }
    // G2.1：struct 构造（VM do_call 序：func_map 之后、内置之前）。
    if let Some((si, nfields)) = struct_lookup(name) {
        return construct_struct(si, nfields as usize, name, argptr, nargs, ci, pc);
    }
    if is_m0_builtin(name) {
        return call_builtin(name, argptr, nargs, ci, pc);
    }
    drop_args(argptr, nargs);
    make_cell(err_h("H002", &format!("undefined function `{}`", name), ci, pc))
}

// ── G2.6：async-await / go 多线程 ──
// 镜像 VM spawn_async / spawn_go / Await 执行臂（vm.rs 2673–2728 / 2288–2312）。
// 前置条件（本轮已落地）：rc 已原子化（rc_ptr_of / rc_load），跨线程共享句柄安全。

/// async 函数名注册表（strtab 索引；wrapper 经 hn_rt_asyncs_init 注入）。
static ASYNC_FNS: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

#[no_mangle]
pub extern "C" fn hn_rt_asyncs_init(names: *const u32, n: usize) {
    let mut t = ASYNC_FNS.lock().unwrap();
    t.clear();
    for i in 0..n {
        t.push(unsafe { *names.add(i) });
    }
}

fn is_async_fn(name: &str) -> bool {
    ASYNC_FNS
        .lock()
        .unwrap()
        .iter()
        .any(|&s| strtab_name_eq(s, name))
}

/// 建 future 值句柄（rc=1，初始 Pending）。
fn make_cell_future() -> u64 {
    let boxed = Box::into_raw(Box::new(HnFuture {
        state: std::sync::Mutex::new(FutureState::Pending),
        cv: std::sync::Condvar::new(),
    }));
    let c = Box::into_raw(Box::new(HnCell {
        rc: 1,
        t: T_FUTURE,
        a: 0,
        b: 0,
        pkind: P_FUTURE,
        payload: boxed as u64,
    }));
    leak_check_inc();
    c as u64
}

/// async 函数调用（镜像 VM spawn_async）：后台线程跑 chunk 函数，实参所有权转移给
/// 子线程（Call lowering 打包的 retain 产物即 callee 引用）；结果（含 ERR cell）存入
/// future，由 rt_await 取出。函数不存在 → H002（同步返回，实参在此释放——
/// VM 侧 checker 已静态拒绝，防御性对齐）。
fn spawn_async(name: &str, argptr: u64, nargs: u64, ci: u32, pc: u32) -> u64 {
    let (fci, _np) = match func_lookup(name) {
        Some(x) => x,
        None => {
            drop_args(argptr, nargs);
            return make_cell(err_h_help(
                "H002",
                &format!("undefined function `{}`", name),
                ci,
                pc,
                Some("check the spelling".to_string()),
            ));
        }
    };
    let fut = make_cell_future();
    // 实参在 spawn 前**同步**拷入堆缓冲：lowering 的 param_ss 是 per-chunk 共享槽，
    // 子线程延迟读取会被主线程下一次 Call 打包覆盖（镜像 VM spawn_go 的同步 clone）。
    // 句柄所有权随 Vec 移入子线程（callee 帧接管）。
    let args: Vec<u64> = (0..nargs as usize).map(|i| arg_at(argptr, i)).collect();
    // 裸指针不可直接跨线程（*mut HnFuture !Send）：以 u64 传递、线程内还原
    //（cell 的 payload 指针布局即 u64，还原安全）。
    let fp = cell(fut).payload;
    std::thread::spawn(move || {
        // 子线程独立执行（FN_PTRS / PROG / strtab 均为静态表，线程安全）。
        // 无 arity 预检：镜像 VM（校验发生在 call_chunk 内，错误经 future 传播到 await）。
        let argptr = args.as_ptr() as u64;
        let nargs = args.len() as u64;
        let r = unsafe { ((*FN_PTRS.lock().unwrap())[fci as usize])(argptr, nargs, 0, 0) };
        let f = fp as *mut HnFuture;
        let mut s = unsafe { (*f).state.lock().unwrap() };
        *s = FutureState::Done(r);
        unsafe { (*f).cv.notify_one() };
    });
    fut
}

/// Await（镜像 VM 执行臂）：future → 阻塞等待并返回结果句柄的 retain 新引用
/// （结果为 ERR cell 时原样返回，由 check_err 路由 catch / 跨帧传播）；
/// 非 future → H001（文案 / help 逐字对齐 VM mk_err_with）。
#[no_mangle]
pub extern "C" fn rt_await(h: u64, ci: u64, pc: u64) -> u64 {
    let (ci, pc) = (ci as u32, pc as u32);
    let c = cell(h);
    if c.t != T_FUTURE {
        return make_cell(err_h_help(
            "H001",
            &format!(
                "`await` requires an async function call result (future), got `{}`",
                type_name(c.t)
            ),
            ci,
            pc,
            Some("await an async function call: `await fetch_data()`".to_string()),
        ));
    }
    let f = unsafe { &*(c.payload as *const HnFuture) };
    let mut s = f.state.lock().unwrap();
    loop {
        if let FutureState::Done(_) = *s {
            break;
        }
        s = f.cv.wait(s).unwrap();
    }
    match *s {
        FutureState::Done(v) => rt_retain(v),
        FutureState::Pending => unreachable!("future 已完成"),
    }
}

/// GoCall（镜像 VM spawn_go）：后台线程 fire-and-forget；错误经 err_render 打印到
/// stderr（不影响主线程）。callee 不存在 → 打印渲染后的 H002（line 0 = plain 分支）。
/// 恒返回 null 句柄 0（Stmt::Go 不产生值）。
#[no_mangle]
pub extern "C" fn rt_gocall(sidx: u64, argptr: u64, nargs: u64) -> u64 {
    let name = strtab_name(sidx as u32);
    let (fci, _np) = match func_lookup(&name) {
        Some(x) => x,
        None => {
            let (file, src, _) = PROG.lock().unwrap().clone();
            eprintln!(
                "{}",
                render_error_with_help(
                    "H002",
                    &format!("undefined function `{}`", name),
                    &file,
                    &src,
                    Some((0, 0, 0)),
                    Some("check the spelling"),
                )
            );
            drop_args(argptr, nargs);
            return 0;
        }
    };
    // 实参在 spawn 前**同步**拷入堆缓冲（同 spawn_async：param_ss 共享槽会被
    // 主线程后续 Call 打包覆盖；镜像 VM spawn_go 的同步 clone）。
    let args: Vec<u64> = (0..nargs as usize).map(|i| arg_at(argptr, i)).collect();
    std::thread::spawn(move || {
        let argptr = args.as_ptr() as u64;
        let nargs = args.len() as u64;
        let r = unsafe { ((*FN_PTRS.lock().unwrap())[fci as usize])(argptr, nargs, 0, 0) };
        if cell(r).t == T_ERR {
            eprintln!("{}", err_render(r));
        }
        cell_release(r);
    });
    0
}

/// struct 定义表按名查找 → (表下标, 字段数)。
fn struct_lookup(name: &str) -> Option<(usize, u32)> {
    let t = STRUCT_DEFS.lock().unwrap();
    for (i, &(s, n)) in t.iter().enumerate() {
        if strtab_name_eq(s, name) {
            return Some((i, n));
        }
    }
    None
}

/// struct 构造（镜像 VM do_call 的 struct 臂）：
/// 实例 = dict，首键 `__struct__` → **结构体名**（隐藏标记，所有用户可见路径过滤；
/// 相等语义据此区分同名/异名 struct，与 VM/interp 的标记值一致），
/// 其余键按字段定义序、值 = 实参（缺省补 null；多出的实参在此释放——
/// checker 已静态拒绝多余实参，此分支为防御性对齐）。
fn construct_struct(si: usize, nfields: usize, name: &str, argptr: u64, nargs: u64, ci: u32, pc: u32) -> u64 {
    let fields = STRUCT_FIELDS.lock().unwrap()[si].clone();
    let name_sidx = STRUCT_DEFS.lock().unwrap()[si].0;
    let mut pairs: Vec<(String, u64)> = Vec::with_capacity(nfields + 1);
    // 标记值 = 结构体名的 strtab 句柄（rt_str = retain 表项，make_cell_dict 内再 retain 守恒）。
    pairs.push((HIDDEN_STRUCT_KEY.to_string(), rt_str(name_sidx as u64)));
    let mut used_args = 0usize;
    for (fi, fsidx) in fields.iter().enumerate() {
        if fi < nargs as usize {
            pairs.push((strtab_name(*fsidx as u32), rt_retain(arg_at(argptr, fi))));
            used_args = fi + 1;
        } else {
            let h = rt_null();
            pairs.push((strtab_name(*fsidx as u32), h));
        }
    }
    // 多余实参（nargs > 字段数）：VM 构造臂直接不读，引用须在此释放。
    for i in used_args..nargs as usize {
        cell_release(arg_at(argptr, i));
    }
    make_cell_dict(&pairs)
}

/// enum 定义表按名查找 → (表下标, 枚举名 sidx, 变体表)。
fn enum_lookup(name: &str) -> Option<(usize, u32, Vec<(u32, u32)>)> {
    let t = ENUM_DEFS.lock().unwrap();
    for (i, &(s, _)) in t.iter().enumerate() {
        if strtab_name_eq(s, name) {
            let vs = ENUM_VARIANTS.lock().unwrap()[i].clone();
            return Some((i, s, vs));
        }
    }
    None
}

/// 建 enum 值句柄（rc=1）：a=枚举名 sidx，b=变体 sidx，载荷句柄逐项 retain（消费）。
fn make_cell_enum(name_sidx: u32, variant_sidx: u32, payload: Vec<u64>) -> u64 {
    let boxed = Box::into_raw(Box::new(HnEnum { payload }));
    let c = Box::into_raw(Box::new(HnCell {
        rc: 1,
        t: T_ENUM,
        a: name_sidx as u64,
        b: variant_sidx as u64,
        pkind: P_ENUM,
        payload: boxed as u64,
    }));
    leak_check_inc();
    c as u64
}

/// 带局部变量优先的名分发（镜像 Vm::do_call）：
/// func_reg 句柄 v（借用）为 lambda → call_fn_h；为 str → 字符串内容再分发；否则全局名分发。
/// sidx = 调用点函数名在 strtab 的索引（func ∉ 当前 chunk locals 时直接走全局分发）。
#[no_mangle]
pub extern "C" fn rt_callv(v: u64, sidx: u64, argptr: u64, nargs: u64, ci: u64, pc: u64) -> u64 {
    let (ci, pc) = (ci as u32, pc as u32);
    let vt = cell(v).t;
    if vt == T_FN {
        return call_fn_h(v, argptr, nargs, ci, pc);
    }
    if vt == T_STR {
        let name = String::from_utf8_lossy(str_bytes(v)).into_owned();
        return call_global(&name, argptr, nargs, ci, pc);
    }
    let name = strtab_name(sidx as u32);
    call_global(&name, argptr, nargs, ci, pc)
}

/// 纯全局名分发（func ∉ 当前 chunk locals 的 Call 用；免去 locals 检查）。
#[no_mangle]
pub extern "C" fn rt_call(sidx: u64, argptr: u64, nargs: u64, ci: u64, pc: u64) -> u64 {
    let (ci, pc) = (ci as u32, pc as u32);
    let name = strtab_name(sidx as u32);
    call_global(&name, argptr, nargs, ci, pc)
}

/// M0 内置表（镜像 builtins.rs 的 M0 可达子集）。
fn is_m0_builtin(name: &str) -> bool {
    matches!(
        name,
        "print"
            | "len"
            | "append"
            | "keys"
            | "values"
            | "has_key"
            | "to_str"
            | "to_int"
            | "to_float"
            | "is_int"
            | "is_float"
            | "is_bool"
            | "is_str"
            | "is_list"
            | "is_dict"
            | "is_null"
            | "type_of"
            | "abs"
            | "max"
            | "min"
            | "assert"
            | "assert_eq"
            | "read_file"
    )
}

/// 内置参数错误（镜像 builtins.rs arg_err 文案）：H011 + help "check the function signature"。
fn b_arg_err(name: &str, want: u64, got: u64, ci: u32, pc: u32) -> Hnv {
    err_h_help(
        "H011",
        &format!("wrong number of arguments: `{}` expects {}, got {}", name, want, got),
        ci,
        pc,
        Some("check the function signature".to_string()),
    )
}

/// M0 标量等值（镜像 vm.rs v_eq 的 M0 可等集合；str 走句柄字节比较）。
fn values_eq_handle(a: u64, b: u64, ci: u32, pc: u32) -> Hnv {
    let ca = cell(a);
    let cb = cell(b);
    if ca.t == T_STR && cb.t == T_STR {
        return Hnv::bool_(str_eq(a, b));
    }
    cmpv_inner(7, ca.hnv(), cb.hnv(), ci, pc)
}

/// 内置函数分发：参数借用（末尾统一 drop_args），返回结果句柄。
fn call_builtin(name: &str, argptr: u64, nargs: u64, ci: u32, pc: u32) -> u64 {
    let a0 = || arg_at(argptr, 0);
    let a1 = || arg_at(argptr, 1);
    let a2 = || arg_at(argptr, 2);
    let r: Hnv = match name {
        "print" => {
            if nargs != 1 {
                b_arg_err(name, 1, nargs, ci, pc)
            } else {
                rt_print(a0());
                Hnv::null()
            }
        }
        "len" => {
            if nargs != 1 {
                b_arg_err(name, 1, nargs, ci, pc)
            } else {
                let c = cell(a0());
                match c.pkind {
                    P_STR => Hnv::int(c.b as i64),
                    P_LIST => {
                        let l = unsafe { &*(c.payload as *const HnList) };
                        Hnv::int(l.items.len() as i64)
                    }
                    P_DICT => {
                        let d = unsafe { &*(c.payload as *const HnDict) };
                        Hnv::int(
                            d.pairs
                                .iter()
                                .filter(|(k, _)| !is_hidden_key(k))
                                .count() as i64,
                        )
                    }
                    _ => {
                        // G2.4 运算符重载：定义了 __len 时，内建 len 不支持该类型 → 回退
                        // （镜像 interp / VM：内建失败且 H001 才回退；a0 为借用，先 retain）。
                        if let Some(v) = call_overload_borrowed("__len", &[a0()], ci, pc) {
                            drop_args(argptr, nargs);
                            return v;
                        }
                        err_h_help(
                            "H001",
                            &format!(
                                "`len` expects a string, list, dict, or bytes, got `{}`",
                                type_name(c.t)
                            ),
                            ci,
                            pc,
                            Some("`len` returns the byte length of a string/bytes, or the element count of a list/dict".to_string()),
                        )
                    }
                }
            }
        }
        "append" => {
            if nargs != 2 {
                b_arg_err(name, 2, nargs, ci, pc)
            } else {
                // rt_append 借用参数（内部 retain 元素与新值），完成后显式 drop 参数再返回。
                let r = rt_append(a0(), a1(), ci as u64, pc as u64);
                drop_args(argptr, nargs);
                return r;
            }
        }
        "keys" => {
            if nargs != 1 {
                b_arg_err(name, 1, nargs, ci, pc)
            } else {
                let c = cell(a0());
                if c.pkind != P_DICT {
                    err_h_help(
                        "H001",
                        &format!("`keys` expects a dict, got `{}`", type_name(c.t)),
                        ci,
                        pc,
                        Some("pass a dict as the argument".to_string()),
                    )
                } else {
                    let d = unsafe { &*(c.payload as *const HnDict) };
                    let keys: Vec<u64> = d
                        .pairs
                        .iter()
                        .filter(|(k, _)| !is_hidden_key(k))
                        .map(|(k, _)| make_cell_str(k.clone().into_bytes()))
                        .collect();
                    let cell = make_cell_list(&keys);
                    for &k in &keys {
                        cell_release(k);
                    }
                    drop_args(argptr, nargs);
                    return cell;
                }
            }
        }
        "values" => {
            if nargs != 1 {
                b_arg_err(name, 1, nargs, ci, pc)
            } else {
                let c = cell(a0());
                if c.pkind != P_DICT {
                    err_h_help(
                        "H001",
                        &format!("`values` expects a dict, got `{}`", type_name(c.t)),
                        ci,
                        pc,
                        Some("pass a dict as the argument".to_string()),
                    )
                } else {
                    let d = unsafe { &*(c.payload as *const HnDict) };
                    let vals: Vec<u64> = d
                        .pairs
                        .iter()
                        .filter(|(k, _)| !is_hidden_key(k))
                        .map(|(_, v)| *v)
                        .collect();
                    let cell = make_cell_list(&vals);
                    drop_args(argptr, nargs);
                    return cell;
                }
            }
        }
        "has_key" => {
            if nargs != 2 {
                b_arg_err(name, 2, nargs, ci, pc)
            } else {
                let c = cell(a0());
                if c.pkind != P_DICT {
                    err_h_help(
                        "H001",
                        &format!("`has_key` expects a dict, got `{}`", type_name(c.t)),
                        ci,
                        pc,
                        Some("pass a dict as the first argument and a key string as the second".to_string()),
                    )
                } else {
                    let kc = cell(a1());
                    if kc.t != T_STR {
                        err_h_help(
                            "H001",
                            &format!(
                                "`{}` expects a string for argument 2, got `{}`",
                                name,
                                type_name(kc.t)
                            ),
                            ci,
                            pc,
                            Some("pass a string value".to_string()),
                        )
                    } else {
                        let d = unsafe { &*(c.payload as *const HnDict) };
                        let k = String::from_utf8_lossy(str_bytes(a1())).into_owned();
                        Hnv::bool_(
                            d.pairs
                                .iter()
                                .filter(|(ek, _)| !is_hidden_key(ek))
                                .any(|(ek, _)| *ek == k),
                        )
                    }
                }
            }
        }
        "to_str" => {
            if nargs != 1 {
                b_arg_err(name, 1, nargs, ci, pc)
            } else {
                let s = handle_display(a0());
                drop_args(argptr, nargs);
                return make_cell_str(s.into_bytes());
            }
        }
        "read_file" => {
            if nargs != 1 {
                b_arg_err(name, 1, nargs, ci, pc)
            } else {
                let p = String::from_utf8_lossy(str_bytes(a0())).into_owned();
                drop_args(argptr, nargs);
                // return 提前退出（镜像 to_str）：两臂均返 u64 句柄，不匹配 `let r: Hnv`。
                return match std::fs::read_to_string(&p) {
                    Ok(s) => make_cell_str(s.into_bytes()),
                    Err(e) => {
                        // 细分文件错误（镜像 builtins.rs read_file）：不存在 / 权限 / 锁定 / 其他。
                        let code = match e.kind() {
                            std::io::ErrorKind::NotFound => "H401",
                            std::io::ErrorKind::PermissionDenied => "H402",
                            std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::ResourceBusy
                            | std::io::ErrorKind::Interrupted => "H403",
                            _ => "H404",
                        };
                        make_cell(err_h(
                            code,
                            &format!("cannot read file `{}`: {}", p, e),
                            ci,
                            pc,
                        ))
                    }
                };
            }
        }
        "to_int" => {
            if nargs != 1 {
                b_arg_err(name, 1, nargs, ci, pc)
            } else {
                let c = cell(a0());
                match c.t {
                    T_INT => Hnv::int(c.a as i64),
                    T_FLOAT => {
                        let f = f64::from_bits(c.a);
                        if f.is_finite() {
                            Hnv::int(f.trunc() as i64)
                        } else {
                            err_h("H001", "cannot convert NaN/infinity to `int`", ci, pc)
                        }
                    }
                    T_STR => {
                        let s = String::from_utf8_lossy(str_bytes(a0())).into_owned();
                        let t = s.trim();
                        let digits = t.strip_prefix('-').unwrap_or(t);
                        if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
                            err_h_help(
                                "H006",
                                &format!("cannot convert `{}` to `int`: not a pure digit string", s),
                                ci,
                                pc,
                                Some("`to_int` on a string requires digits only (optional leading `-`)".to_string()),
                            )
                        } else {
                            match t.parse::<i64>() {
                                Ok(v) => Hnv::int(v),
                                Err(_) => err_h(
                                    "H006",
                                    &format!("cannot convert `{}` to `int`: out of range", s),
                                    ci,
                                    pc,
                                ),
                            }
                        }
                    }
                    other => err_h_help(
                        "H001",
                        &format!("cannot convert `{}` to `int`", type_name(other)),
                        ci,
                        pc,
                        Some("`to_int` accepts `int`, `float` or a pure-digit `str`".to_string()),
                    ),
                }
            }
        }
        "to_float" => {
            if nargs != 1 {
                b_arg_err(name, 1, nargs, ci, pc)
            } else {
                let c = cell(a0());
                match c.t {
                    T_INT => Hnv::float(c.a as i64 as f64),
                    T_FLOAT => Hnv::float(f64::from_bits(c.a)),
                    T_STR => {
                        let s = String::from_utf8_lossy(str_bytes(a0())).into_owned();
                        match s.trim().parse::<f64>() {
                            Ok(f) => Hnv::float(f),
                            Err(_) => err_h_help(
                                "H007",
                                &format!("cannot convert `{}` to `float`: invalid format", s),
                                ci,
                                pc,
                                Some("`to_float` on a string requires a number format like `2.718`".to_string()),
                            ),
                        }
                    }
                    other => err_h_help(
                        "H001",
                        &format!("cannot convert `{}` to `float`", type_name(other)),
                        ci,
                        pc,
                        Some("`to_float` accepts `int`, `float` or a numeric `str`".to_string()),
                    ),
                }
            }
        }
        "is_int" => {
            if nargs != 1 {
                b_arg_err(name, 1, nargs, ci, pc)
            } else {
                Hnv::bool_(cell(a0()).t == T_INT)
            }
        }
        "is_float" => {
            if nargs != 1 {
                b_arg_err(name, 1, nargs, ci, pc)
            } else {
                Hnv::bool_(cell(a0()).t == T_FLOAT)
            }
        }
        "is_bool" => {
            if nargs != 1 {
                b_arg_err(name, 1, nargs, ci, pc)
            } else {
                let t = cell(a0()).t;
                Hnv::bool_(t == T_BOOL_TRUE || t == T_BOOL_FALSE)
            }
        }
        "is_str" => {
            if nargs != 1 {
                b_arg_err(name, 1, nargs, ci, pc)
            } else {
                Hnv::bool_(cell(a0()).t == T_STR)
            }
        }
        "is_list" => {
            if nargs != 1 {
                b_arg_err(name, 1, nargs, ci, pc)
            } else {
                Hnv::bool_(cell(a0()).t == T_LIST)
            }
        }
        "is_dict" => {
            if nargs != 1 {
                b_arg_err(name, 1, nargs, ci, pc)
            } else {
                Hnv::bool_(cell(a0()).t == T_DICT)
            }
        }
        "is_null" => {
            if nargs != 1 {
                b_arg_err(name, 1, nargs, ci, pc)
            } else {
                Hnv::bool_(cell(a0()).t == T_NULL)
            }
        }
        "type_of" => {
            if nargs != 1 {
                b_arg_err(name, 1, nargs, ci, pc)
            } else {
                let s = type_name(cell(a0()).t).to_string();
                drop_args(argptr, nargs);
                return make_cell_str(s.into_bytes());
            }
        }
        "abs" => {
            if nargs != 1 {
                b_arg_err(name, 1, nargs, ci, pc)
            } else {
                let c = cell(a0());
                match c.t {
                    T_INT => match (c.a as i64).checked_abs() {
                        Some(v) => Hnv::int(v),
                        // 注意：解释器 builtins.rs 该分支 help 传 None（无 help 行），
                        // 不得用 vm_help_for 的 H010 兜底文案——那会让 nat 多出一行 help。
                        None => err_h_explicit_no_help("H010", "`abs` overflow on i64::MIN", ci, pc),
                    },
                    T_FLOAT => Hnv::float(f64::from_bits(c.a).abs()),
                    other => err_h_help(
                        "H001",
                        &format!("`abs` expects a number, got `{}`", type_name(other)),
                        ci,
                        pc,
                        Some("pass an `int` or `float`".to_string()),
                    ),
                }
            }
        }
        "max" | "min" => {
            if nargs != 2 {
                b_arg_err(name, 2, nargs, ci, pc)
            } else {
                let (ca, cb) = (cell(a0()), cell(a1()));
                let is_max = name == "max";
                match (ca.t, cb.t) {
                    (T_INT, T_INT) => {
                        let (x, y) = (ca.a as i64, cb.a as i64);
                        Hnv::int(if is_max { x.max(y) } else { x.min(y) })
                    }
                    (T_FLOAT, T_FLOAT) => {
                        let (x, y) = (f64::from_bits(ca.a), f64::from_bits(cb.a));
                        Hnv::float(if is_max { x.max(y) } else { x.min(y) })
                    }
                    _ => err_h_help(
                        "H001",
                        &format!(
                            "`{}` requires two operands of the same type, got `{}` and `{}`",
                            name,
                            type_name(ca.t),
                            type_name(cb.t)
                        ),
                        ci,
                        pc,
                        Some("Hone has no implicit type conversion".to_string()),
                    ),
                }
            }
        }
        "assert" => {
            if nargs < 1 {
                b_arg_err(name, 1, nargs, ci, pc)
            } else {
                let c = cell(a0());
                if c.t == T_BOOL_TRUE {
                    Hnv::null()
                } else if c.t == T_BOOL_FALSE {
                    let msg = if nargs >= 2 {
                        let c2 = cell(a1());
                        if c2.t == T_STR {
                            String::from_utf8_lossy(str_bytes(a1())).into_owned()
                        } else {
                            "assertion failed".to_string()
                        }
                    } else {
                        "assertion failed".to_string()
                    };
                    err_h("H700", &msg, ci, pc)
                } else {
                    err_h(
                        "H001",
                        &format!("`assert` expects a `bool` condition, got `{}`", type_name(c.t)),
                        ci,
                        pc,
                    )
                }
            }
        }
        "assert_eq" => {
            if nargs < 2 {
                b_arg_err(name, 2, nargs, ci, pc)
            } else {
                let e = values_eq_handle(a0(), a1(), ci, pc);
                if e.t == T_ERR {
                    // 等值运算本身出错（比较类型不匹配）：释放实参后返回错误。
                    drop_args(argptr, nargs);
                    return make_cell(e);
                }
                if e.t == T_BOOL_TRUE {
                    Hnv::null()
                } else {
                    let msg = if nargs >= 3 {
                        let c3 = cell(a2());
                        if c3.t == T_STR {
                            String::from_utf8_lossy(str_bytes(a2())).into_owned()
                        } else {
                            format!("assertion failed: {} != {}", handle_display(a0()), handle_display(a1()))
                        }
                    } else {
                        format!("assertion failed: {} != {}", handle_display(a0()), handle_display(a1()))
                    };
                    err_h("H700", &msg, ci, pc)
                }
            }
        }
        _ => err_h("H999", &format!("builtin `{}` not implemented in natgen", name), ci, pc),
    };
    drop_args(argptr, nargs);
    make_cell(r)
}

// ── G2.9 点号内置族（可移植实现） ──
// 逐条镜像 src/builtins.rs / src/*mod.rs 的运行期行为（checker 已静态校验参数个数/
// 类型，这里只复刻运行期语义与错误文案）。std-only：需外部 crate 的家族在此登记 H999
// （见 call_dotted 尾部）。所有权协议与 call_builtin 一致：借用实参，末尾统一 drop_args。
//
// 静态状态（镜像 builtins.rs 的 LazyLock 静态）：
static DOTTED_CLI_ARGS: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());
static DOTTED_KV: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());
static DOTTED_TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// CLI 参数初始化（wrapper main 调用，镜像 builtins::init_args 的解析规则）：
/// `--key val` / `-k val` 取值，单独 `--key`/`-k` 记 "true"，位置参数忽略。
/// name 为空串时值 = "true"（布尔标志），否则 (key, val)。
#[no_mangle]
pub extern "C" fn hn_rt_args_init(keys: *const u8, key_lens: *const usize, vals: *const u8, val_lens: *const usize, n: usize) {
    let mut m = DOTTED_CLI_ARGS.lock().unwrap();
    m.clear();
    for i in 0..n {
        let kb = unsafe { std::slice::from_raw_parts(keys.add(i), key_lens.add(i).read()) };
        let vb = unsafe { std::slice::from_raw_parts(vals.add(i), val_lens.add(i).read()) };
        m.push((
            String::from_utf8_lossy(kb).into_owned(),
            String::from_utf8_lossy(vb).into_owned(),
        ));
    }
}

/// 实际解析按 init_args 语义在 wrapper 侧由 hone 生成（避开在 runtime 重写 UTF-8 参数遍历）：
/// hone 的 build_native 把 std::env::args().skip(1) 解析成 (key, val) 对后传入。
fn dotted_args_get(key: &str) -> Option<String> {
    let m = DOTTED_CLI_ARGS.lock().unwrap();
    m.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

fn dotted_args_has(key: &str) -> bool {
    let m = DOTTED_CLI_ARGS.lock().unwrap();
    m.iter().any(|(k, _)| k == key)
}

// xorshift64*（镜像 builtins.rs RNG：线程本地，种子 = 纳秒 ^ 栈地址）。
thread_local! {
    static DOTTED_RNG: std::cell::RefCell<u64> = std::cell::RefCell::new(dotted_seed_rng());
}

fn dotted_seed_rng() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    let addr = (&nanos as *const u64) as usize as u64;
    (nanos ^ addr).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1
}

fn dotted_next_u64() -> u64 {
    DOTTED_RNG.with(|r| {
        let mut x = r.borrow_mut();
        *x ^= *x >> 12;
        *x ^= *x << 25;
        *x ^= *x >> 27;
        *x = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        *x
    })
}

// ── 点号内置实参读取（错误文案逐字镜像 builtins.rs as_str/as_int/as_num 与各 mod） ──

/// builtins.rs 风格 str 参数（"`{name}` expects a string for argument {n}, got `{ty}`" + help）。
fn d_str(h: u64, name: &str, arg: usize, ci: u32, pc: u32) -> Result<String, Hnv> {
    let c = cell(h);
    if c.t == T_STR {
        Ok(String::from_utf8_lossy(str_bytes(h)).into_owned())
    } else {
        Err(err_h_help(
            "H001",
            &format!("`{}` expects a string for argument {}, got `{}`", name, arg + 1, type_name(c.t)),
            ci,
            pc,
            Some("pass a string value".to_string()),
        ))
    }
}

/// builtins.rs 风格 int 参数（"`{name}` expects an integer for argument {n}, got `{ty}`" + help）。
fn d_int(h: u64, name: &str, arg: usize, ci: u32, pc: u32) -> Result<i64, Hnv> {
    let c = cell(h);
    if c.t == T_INT {
        Ok(c.a as i64)
    } else {
        Err(err_h_help(
            "H001",
            &format!("`{}` expects an integer for argument {}, got `{}`", name, arg + 1, type_name(c.t)),
            ci,
            pc,
            Some("pass an `int` value".to_string()),
        ))
    }
}

/// builtins.rs 风格 number 参数（int/float 提升，"`{name}` expects a number for argument {n}" + help）。
fn d_num(h: u64, name: &str, arg: usize, ci: u32, pc: u32) -> Result<f64, Hnv> {
    let c = cell(h);
    match c.t {
        T_INT => Ok(c.a as i64 as f64),
        T_FLOAT => Ok(f64::from_bits(c.a)),
        _ => Err(err_h_help(
            "H001",
            &format!("`{}` expects a number for argument {}, got `{}`", name, arg + 1, type_name(c.t)),
            ci,
            pc,
            Some("pass an `int` or `float` value".to_string()),
        )),
    }
}

/// 借用 list 元素句柄（非 list 返回 None，由调用方按各 mod 文案报错）。
fn d_list(h: u64) -> Option<Vec<u64>> {
    let c = cell(h);
    if c.pkind == P_LIST {
        let l = unsafe { &*(c.payload as *const HnList) };
        Some(l.items.clone())
    } else {
        None
    }
}

/// statmod / plotmod 风格 what 前缀报错（"`{what}` expects a number, got `{ty}`"，无 help）。
fn stat_as_num(h: u64, what: &str, ci: u32, pc: u32) -> Result<f64, Hnv> {
    let c = cell(h);
    match c.t {
        T_INT => Ok(c.a as i64 as f64),
        T_FLOAT => Ok(f64::from_bits(c.a)),
        _ => Err(err_h(
            "H001",
            &format!("`{}` expects a number, got `{}`", what, type_name(c.t)),
            ci,
            pc,
        )),
    }
}

/// statmod num_list（元素错误带 "`{what} element {n}`" 前缀）。
fn stat_num_list(h: u64, what: &str, ci: u32, pc: u32) -> Result<Vec<f64>, Hnv> {
    let items = match d_list(h) {
        Some(it) => it,
        None => {
            let ty = type_name(cell(h).t);
            return Err(err_h("H001", &format!("`{}` expects a list of numbers, got `{}`", what, ty), ci, pc));
        }
    };
    let mut out = Vec::with_capacity(items.len());
    for (i, &x) in items.iter().enumerate() {
        out.push(stat_as_num(x, &format!("{} element {}", what, i + 1), ci, pc)?);
    }
    Ok(out)
}

/// statmod 矩阵提取（矩形校验文案逐字镜像）。
fn stat_matrix(h: u64, what: &str, ci: u32, pc: u32) -> Result<Vec<Vec<f64>>, Hnv> {
    let rows = match d_list(h) {
        Some(r) => r,
        None => {
            let ty = type_name(cell(h).t);
            return Err(err_h("H001", &format!("`{}` expects a list of lists (matrix), got `{}`", what, ty), ci, pc));
        }
    };
    let mut out: Vec<Vec<f64>> = Vec::with_capacity(rows.len());
    for (ri, &row) in rows.iter().enumerate() {
        out.push(stat_num_list(row, &format!("{} row {}", what, ri + 1), ci, pc)?);
    }
    if let Some(first) = out.first() {
        let cols = first.len();
        for (ri, row) in out.iter().enumerate() {
            if row.len() != cols {
                return Err(err_h_help(
                    "H001",
                    &format!(
                        "`{}` requires a rectangular matrix: row 1 has {} columns but row {} has {}",
                        what,
                        cols,
                        ri + 1,
                        row.len()
                    ),
                    ci,
                    pc,
                    Some("pad or trim rows to the same length".to_string()),
                ));
            }
        }
    }
    Ok(out)
}

/// statmod to_value（整数 f64 → int 句柄，否则 float 句柄）。
fn stat_to_value(x: f64) -> Hnv {
    if x.fract() == 0.0 && x.abs() < 9.007_199_254_740_992e15 {
        Hnv::int(x as i64)
    } else {
        Hnv::float(x)
    }
}

/// 矩阵 → list 句柄（逐行 list，元素 stat_to_value）。
fn stat_matrix_to_cell(m: &[Vec<f64>]) -> u64 {
    let rows: Vec<u64> = m
        .iter()
        .map(|row| {
            let items: Vec<u64> = row.iter().map(|&x| make_cell(stat_to_value(x))).collect();
            let c = make_cell_list(&items);
            for &it in &items {
                cell_release(it);
            }
            c
        })
        .collect();
    let c = make_cell_list(&rows);
    for &r in &rows {
        cell_release(r);
    }
    c
}

// ── time 辅助（逐字镜像 builtins.rs format_timestamp / civil_from_days / parse_timestamp） ──

fn dotted_civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

fn dotted_days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn dotted_format_timestamp(secs: i64, fmt: &str) -> String {
    let days = secs.div_euclid(86400);
    let sod = secs.rem_euclid(86400);
    let (y, mo, d) = dotted_civil_from_days(days);
    let h = sod / 3600;
    let mi = (sod % 3600) / 60;
    let s = sod % 60;

    let mut out = String::new();
    let chars: Vec<char> = fmt.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        if i + 4 <= n && chars[i..i + 4] == ['Y', 'Y', 'Y', 'Y'] {
            out.push_str(&format!("{:04}", y));
            i += 4;
            continue;
        }
        if i + 2 <= n {
            let seg: String = chars[i..i + 2].iter().collect();
            match seg.as_str() {
                "MM" => {
                    out.push_str(&format!("{:02}", mo));
                    i += 2;
                    continue;
                }
                "DD" => {
                    out.push_str(&format!("{:02}", d));
                    i += 2;
                    continue;
                }
                "HH" => {
                    out.push_str(&format!("{:02}", h));
                    i += 2;
                    continue;
                }
                "mm" => {
                    out.push_str(&format!("{:02}", mi));
                    i += 2;
                    continue;
                }
                "SS" => {
                    out.push_str(&format!("{:02}", s));
                    i += 2;
                    continue;
                }
                "WW" => {
                    let days = secs.div_euclid(86400);
                    out.push_str(&(((days + 3).rem_euclid(7)) + 1).to_string());
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn dotted_parse_timestamp(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let n = b.len();
    let digit = |c: u8| c.is_ascii_digit();
    if n < 10
        || !digit(b[0]) || !digit(b[1]) || !digit(b[2]) || !digit(b[3])
        || b[4] != b'-'
        || !digit(b[5]) || !digit(b[6])
        || b[7] != b'-'
        || !digit(b[8]) || !digit(b[9])
    {
        return None;
    }
    let y = (b[0] - b'0') as i64 * 1000 + (b[1] - b'0') as i64 * 100 + (b[2] - b'0') as i64 * 10 + (b[3] - b'0') as i64;
    let mo = (b[5] - b'0') as i64 * 10 + (b[6] - b'0') as i64;
    let d = (b[8] - b'0') as i64 * 10 + (b[9] - b'0') as i64;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    if n == 10 {
        return Some(dotted_days_from_civil(y, mo as u32, d as u32) * 86400);
    }
    let sep = b[10];
    if sep != b'T' && sep != b' ' {
        return None;
    }
    let two = |i: usize| -> Option<i64> {
        if i + 1 < n && digit(b[i]) && digit(b[i + 1]) {
            Some((b[i] - b'0') as i64 * 10 + (b[i + 1] - b'0') as i64)
        } else {
            None
        }
    };
    let h = two(11)?;
    if n < 14 || b[13] != b':' {
        return None;
    }
    let mi = two(14)?;
    let mut i = 16;
    let mut sec = 0i64;
    if i < n && b[i] == b':' {
        sec = two(i + 1)?;
        i += 3;
    }
    if h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    if i < n && b[i] == b'.' {
        while i < n && digit(b[i]) {
            i += 1;
        }
    }
    let mut offset = 0i64;
    if i < n {
        match b[i] {
            b'Z' | b'z' => i += 1,
            b'+' | b'-' => {
                let sign = if b[i] == b'-' { -1 } else { 1 };
                let oh = two(i + 1)?;
                let mut om = 0i64;
                let mut j = i + 3;
                if j < n && b[j] == b':' {
                    om = two(j + 1)?;
                    j += 3;
                }
                if oh > 23 || om > 59 {
                    return None;
                }
                offset = sign * (oh * 3600 + om * 60);
                i = j;
            }
            _ => return None,
        }
    }
    if i != n {
        return None;
    }
    Some(dotted_days_from_civil(y, mo as u32, d as u32) * 86400 + h * 3600 + mi * 60 + sec - offset)
}

/// 点号内置分发（G2.9）。返回 Some(cell)：已处理（含 H999 登记，实参已释放）；
/// None：未知点号名 → 调用方保留 H404（load 库方法调用，实参由调用方释放）。
fn call_dotted(name: &str, argptr: u64, nargs: u64, ci: u32, pc: u32) -> Option<u64> {
    let a0 = || arg_at(argptr, 0);
    let a1 = || arg_at(argptr, 1);
    let a2 = || arg_at(argptr, 2);
    // 可移植族逐臂实现；尾部统一 drop_args + make_cell。
    let r: Option<Hnv> = match name {
        // ---------- time ----------
        "time.now" => {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            Some(Hnv::int(secs))
        }
        "time.sleep" => {
            let secs = match d_num(a0(), name, 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            match std::time::Duration::try_from_secs_f64(secs) {
                Ok(d) => {
                    std::thread::sleep(d);
                    Some(Hnv::null())
                }
                Err(_) => {
                    drop_args(argptr, nargs);
                    return Some(make_cell(err_h("H001", "`time.sleep` duration must be a non-negative number", ci, pc)));
                }
            }
        }
        "time.format" => {
            let ts = match d_int(a0(), name, 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let fmt = match d_str(a1(), name, 1, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let out = dotted_format_timestamp(ts, &fmt);
            drop_args(argptr, nargs);
            return Some(make_cell_str(out.into_bytes()));
        }
        "time.parse" => {
            let s = match d_str(a0(), name, 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            match dotted_parse_timestamp(&s) {
                Some(secs) => Some(Hnv::int(secs)),
                None => {
                    drop_args(argptr, nargs);
                    return Some(make_cell(err_h_help(
                        "H001",
                        &format!("cannot parse `{}` as a timestamp", s),
                        ci,
                        pc,
                        Some("supported formats: `YYYY-MM-DDTHH:MM:SSZ`, `YYYY-MM-DD HH:MM:SS`, optional `+08:00` offset".to_string()),
                    )));
                }
            }
        }
        "time.add" => {
            let ts = match d_int(a0(), name, 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let secs = match d_int(a1(), name, 1, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            match ts.checked_add(secs) {
                Some(v) => Some(Hnv::int(v)),
                None => {
                    drop_args(argptr, nargs);
                    return Some(make_cell(err_h("H010", "time.add: timestamp overflow", ci, pc)));
                }
            }
        }
        "time.diff" => {
            let a = match d_int(a0(), name, 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let b = match d_int(a1(), name, 1, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            match a.checked_sub(b) {
                Some(v) => Some(Hnv::int(v)),
                None => {
                    drop_args(argptr, nargs);
                    return Some(make_cell(err_h("H010", "time.diff: timestamp overflow", ci, pc)));
                }
            }
        }
        "time.weekday" => {
            let ts = match d_int(a0(), name, 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let days = ts.div_euclid(86400);
            Some(Hnv::int(((days + 3).rem_euclid(7)) + 1))
        }
        // ---------- random ----------
        "random.int" => {
            let min = match d_int(a0(), name, 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let max = match d_int(a1(), name, 1, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            if min > max {
                drop_args(argptr, nargs);
                return Some(make_cell(err_h_help(
                    "H001",
                    &format!("`random.int` range is invalid: min ({}) > max ({})", min, max),
                    ci,
                    pc,
                    Some("swap the two arguments".to_string()),
                )));
            }
            // 闭区间 [min, max]（镜像 builtins.rs random_int）
            let span = (max as u64).wrapping_sub(min as u64).wrapping_add(1);
            let v = if span == 0 {
                dotted_next_u64() as i64
            } else {
                (min as u64).wrapping_add(dotted_next_u64() % span) as i64
            };
            Some(Hnv::int(v))
        }
        "random.float" => Some(Hnv::float((dotted_next_u64() >> 11) as f64 / (1u64 << 53) as f64)),
        // ---------- uuid ----------
        "uuid.new" => {
            let hi = dotted_next_u64();
            let lo = dotted_next_u64();
            let mut b = [0u8; 16];
            b[..8].copy_from_slice(&hi.to_be_bytes());
            b[8..].copy_from_slice(&lo.to_be_bytes());
            b[6] = (b[6] & 0x0f) | 0x40;
            b[8] = (b[8] & 0x3f) | 0x80;
            let s = format!(
                "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
            );
            drop_args(argptr, nargs);
            return Some(make_cell_str(s.into_bytes()));
        }
        // ---------- log（stderr，镜像 builtins.rs 颜色码） ----------
        "log.info" | "log.warn" | "log.error" | "log.debug" => {
            let msg = match d_str(a0(), name, 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let tag = match name {
                "log.info" => "\x1b[34m[INFO]\x1b[0m",
                "log.warn" => "\x1b[33m[WARN]\x1b[0m",
                "log.error" => "\x1b[31m[ERROR]\x1b[0m",
                _ => "\x1b[32m[DEBUG]\x1b[0m",
            };
            eprintln!("{} {}", tag, msg);
            Some(Hnv::null())
        }
        // ---------- path ----------
        "path.join" => {
            let mut parts: Vec<String> = Vec::new();
            for i in 0..nargs as usize {
                match d_str(arg_at(argptr, i), name, i, ci, pc) {
                    Ok(s) => parts.push(s),
                    Err(e) => return Some(make_cell(e)),
                }
            }
            let p: std::path::PathBuf = parts.iter().collect();
            drop_args(argptr, nargs);
            return Some(make_cell_str(p.to_string_lossy().to_string().into_bytes()));
        }
        "path.dirname" => {
            let p = match d_str(a0(), name, 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let parent = std::path::Path::new(&p)
                .parent()
                .filter(|d| !d.as_os_str().is_empty())
                .map(|d| d.to_string_lossy().to_string())
                .unwrap_or_else(|| ".".to_string());
            drop_args(argptr, nargs);
            return Some(make_cell_str(parent.into_bytes()));
        }
        "path.basename" => {
            let p = match d_str(a0(), name, 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let base = std::path::Path::new(&p)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            drop_args(argptr, nargs);
            return Some(make_cell_str(base.into_bytes()));
        }
        // ---------- args ----------
        "args.get" => {
            let key = match d_str(a0(), name, 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            match dotted_args_get(&key) {
                Some(v) => {
                    if nargs >= 2 {
                        let ty = match d_str(a1(), name, 1, ci, pc) {
                            Ok(t) => t,
                            Err(e) => return Some(make_cell(e)),
                        };
                        let t = v.trim();
                        match ty.as_str() {
                            "int" => match t.parse::<i64>() {
                                Ok(i) => Some(Hnv::int(i)),
                                Err(_) => {
                                    drop_args(argptr, nargs);
                                    return Some(make_cell(err_h_help(
                                        "H006",
                                        &format!("`args.get(\"{}\", int)` cannot parse `{}` as an integer", key, v),
                                        ci,
                                        pc,
                                        Some("pass a valid integer on the command line".to_string()),
                                    )));
                                }
                            },
                            "float" => match t.parse::<f64>() {
                                Ok(f) => Some(Hnv::float(f)),
                                Err(_) => {
                                    drop_args(argptr, nargs);
                                    return Some(make_cell(err_h_help(
                                        "H007",
                                        &format!("`args.get(\"{}\", float)` cannot parse `{}` as a float", key, v),
                                        ci,
                                        pc,
                                        Some("pass a valid number on the command line".to_string()),
                                    )));
                                }
                            },
                            "bool" => match t {
                                "true" | "1" => Some(Hnv::bool_(true)),
                                "false" | "0" => Some(Hnv::bool_(false)),
                                _ => {
                                    drop_args(argptr, nargs);
                                    return Some(make_cell(err_h_help(
                                        "H001",
                                        &format!("`args.get(\"{}\", bool)` cannot parse `{}` as a boolean", key, v),
                                        ci,
                                        pc,
                                        Some("use `true`/`false` or `1`/`0`".to_string()),
                                    )));
                                }
                            },
                            "str" => {
                                drop_args(argptr, nargs);
                                return Some(make_cell_str(v.into_bytes()));
                            }
                            _ => {
                                drop_args(argptr, nargs);
                                return Some(make_cell(err_h_help(
                                    "H001",
                                    &format!("unknown type `{}` for `args.get`", ty),
                                    ci,
                                    pc,
                                    Some("expected one of `int`, `float`, `bool`, `str`".to_string()),
                                )));
                            }
                        }
                    } else {
                        drop_args(argptr, nargs);
                        return Some(make_cell_str(v.into_bytes()));
                    }
                }
                None => {
                    // 键不存在：有默认值参数（第 3 个）则返回默认值，否则 null。
                    if nargs >= 3 {
                        let h = rt_retain(a2());
                        drop_args(argptr, nargs);
                        return Some(h);
                    }
                    Some(Hnv::null())
                }
            }
        }
        "args.has" => {
            let key = match d_str(a0(), name, 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            Some(Hnv::bool_(dotted_args_has(&key)))
        }
        // ---------- env ----------
        "env.get" | "sys.get_env" => {
            let key = match d_str(a0(), name, 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let v = std::env::var(&key).unwrap_or_default();
            drop_args(argptr, nargs);
            return Some(make_cell_str(v.into_bytes()));
        }
        "env.set" => {
            let key = match d_str(a0(), name, 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let val = match d_str(a1(), name, 1, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            std::env::set_var(&key, &val);
            Some(Hnv::null())
        }
        // ---------- db（进程内 KV，镜像 builtins.rs KV_STORE；--resume 落盘为 hone CLI 特性，nat 产物无此路径） ----------
        "db.set" => {
            let key = match d_str(a0(), name, 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let val = match d_str(a1(), name, 1, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let mut store = DOTTED_KV.lock().unwrap();
            match store.iter_mut().find(|(k, _)| *k == key) {
                Some(entry) => entry.1 = val,
                None => store.push((key, val)),
            }
            Some(Hnv::null())
        }
        "db.get" => {
            let key = match d_str(a0(), name, 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let store = DOTTED_KV.lock().unwrap();
            match store.iter().find(|(k, _)| *k == key) {
                Some((_, v)) => {
                    let out = v.clone();
                    drop(store);
                    drop_args(argptr, nargs);
                    return Some(make_cell_str(out.into_bytes()));
                }
                None => Some(Hnv::null()),
            }
        }
        // ---------- sys.run（镜像 run_shell：cmd /C 或 sh -c，stdout+stderr 合并） ----------
        "sys.run" => {
            let cmd = match d_str(a0(), name, 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let output = if cfg!(windows) {
                std::process::Command::new("cmd").args(["/C", &cmd]).output()
            } else {
                std::process::Command::new("sh").args(["-c", &cmd]).output()
            };
            match output {
                Ok(o) => {
                    let mut out = String::from_utf8_lossy(&o.stdout).into_owned();
                    out.push_str(&String::from_utf8_lossy(&o.stderr));
                    drop_args(argptr, nargs);
                    return Some(make_cell_str(out.into_bytes()));
                }
                Err(e) => {
                    drop_args(argptr, nargs);
                    return Some(make_cell(err_h_help(
                        "H200",
                        &format!("cannot run command `{}`: {}", cmd, e),
                        ci,
                        pc,
                        Some("check the command".to_string()),
                    )));
                }
            }
        }
        // ---------- stat ----------
        "stat.sum" => {
            let nums = match stat_num_list(a0(), "stat.sum", ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            Some(stat_to_value(nums.iter().sum()))
        }
        "stat.mean" => {
            let nums = match stat_num_list(a0(), "stat.mean", ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            if nums.is_empty() {
                drop_args(argptr, nargs);
                return Some(make_cell(err_h_help("H001", "stat.mean: empty list", ci, pc, Some("pass at least one number".to_string()))));
            }
            Some(Hnv::float(nums.iter().sum::<f64>() / nums.len() as f64))
        }
        "stat.median" => {
            let mut nums = match stat_num_list(a0(), "stat.median", ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            if nums.is_empty() {
                drop_args(argptr, nargs);
                return Some(make_cell(err_h_help("H001", "stat.median: empty list", ci, pc, Some("pass at least one number".to_string()))));
            }
            nums.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let mid = nums.len() / 2;
            let med = if nums.len() % 2 == 1 { nums[mid] } else { (nums[mid - 1] + nums[mid]) / 2.0 };
            Some(Hnv::float(med))
        }
        "stat.variance" | "stat.stddev" => {
            let nums = match stat_num_list(a0(), name, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            if nums.len() < 2 {
                drop_args(argptr, nargs);
                return Some(make_cell(err_h_help(
                    "H001",
                    &format!("{}: need at least 2 numbers", name),
                    ci,
                    pc,
                    Some("pass at least two numbers".to_string()),
                )));
            }
            let mean = nums.iter().sum::<f64>() / nums.len() as f64;
            let var: f64 = nums.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>() / nums.len() as f64;
            Some(Hnv::float(if name == "stat.variance" { var } else { var.sqrt() }))
        }
        "stat.min" | "stat.max" => {
            let nums = match stat_num_list(a0(), name, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            if nums.is_empty() {
                drop_args(argptr, nargs);
                return Some(make_cell(err_h_help("H001", &format!("{}: empty list", name), ci, pc, Some("pass at least one number".to_string()))));
            }
            let cmp = |a: &f64, b: &f64| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal);
            let v = if name == "stat.min" {
                nums.iter().copied().min_by(cmp).unwrap()
            } else {
                nums.iter().copied().max_by(cmp).unwrap()
            };
            Some(stat_to_value(v))
        }
        // ---------- matrix ----------
        "matrix.identity" => {
            let n = match cell(a0()).t {
                T_INT if (cell(a0()).a as i64) >= 0 => cell(a0()).a as usize,
                _ => {
                    let ty = type_name(cell(a0()).t);
                    drop_args(argptr, nargs);
                    return Some(make_cell(err_h(
                        "H001",
                        &format!("matrix.identity expects a non-negative int, got `{}`", ty),
                        ci,
                        pc,
                    )));
                }
            };
            let mut m = vec![vec![0.0f64; n]; n];
            for i in 0..n {
                m[i][i] = 1.0;
            }
            drop_args(argptr, nargs);
            return Some(stat_matrix_to_cell(&m));
        }
        "matrix.transpose" => {
            let m = match stat_matrix(a0(), "matrix.transpose", ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            if m.is_empty() {
                drop_args(argptr, nargs);
                return Some(make_cell_list(&[]));
            }
            let (rows, cols) = (m.len(), m[0].len());
            let mut t = vec![vec![0.0f64; rows]; cols];
            for i in 0..rows {
                for j in 0..cols {
                    t[j][i] = m[i][j];
                }
            }
            drop_args(argptr, nargs);
            return Some(stat_matrix_to_cell(&t));
        }
        "matrix.add" => {
            let ma = match stat_matrix(a0(), "matrix.add", ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let mb = match stat_matrix(a1(), "matrix.add", ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            if ma.len() != mb.len()
                || (ma.first().map(|r| r.len()).unwrap_or(0) != mb.first().map(|r| r.len()).unwrap_or(0))
            {
                drop_args(argptr, nargs);
                return Some(make_cell(err_h_help(
                    "H001",
                    "matrix.add: shapes must match",
                    ci,
                    pc,
                    Some("both matrices must have the same rows and columns".to_string()),
                )));
            }
            let (rows, cols) = (ma.len(), ma.first().map(|r| r.len()).unwrap_or(0));
            let mut c = vec![vec![0.0f64; cols]; rows];
            for i in 0..rows {
                for j in 0..cols {
                    c[i][j] = ma[i][j] + mb[i][j];
                }
            }
            drop_args(argptr, nargs);
            return Some(stat_matrix_to_cell(&c));
        }
        "matrix.mul" => {
            let ma = match stat_matrix(a0(), "matrix.mul", ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let mb = match stat_matrix(a1(), "matrix.mul", ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            if ma.is_empty() || mb.is_empty() {
                drop_args(argptr, nargs);
                return Some(make_cell(err_h("H001", "matrix.mul: empty matrix", ci, pc)));
            }
            let (ar, ac) = (ma.len(), ma[0].len());
            let (br, bc) = (mb.len(), mb[0].len());
            if ac != br {
                drop_args(argptr, nargs);
                return Some(make_cell(err_h_help(
                    "H001",
                    &format!("matrix.mul: shapes mismatch ({}x{} @ {}x{})", ar, ac, br, bc),
                    ci,
                    pc,
                    Some("matrix A columns must equal matrix B rows".to_string()),
                )));
            }
            let mut c = vec![vec![0.0f64; bc]; ar];
            for i in 0..ar {
                for j in 0..bc {
                    let mut s = 0.0;
                    for k in 0..ac {
                        s += ma[i][k] * mb[k][j];
                    }
                    c[i][j] = s;
                }
            }
            drop_args(argptr, nargs);
            return Some(stat_matrix_to_cell(&c));
        }
        "matrix.scale" => {
            let m = match stat_matrix(a0(), "matrix.scale", ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let k = match stat_as_num(a1(), "matrix.scale scalar", ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let (rows, cols) = (m.len(), m.first().map(|r| r.len()).unwrap_or(0));
            let mut c = vec![vec![0.0f64; cols]; rows];
            for i in 0..rows {
                for j in 0..cols {
                    c[i][j] = m[i][j] * k;
                }
            }
            drop_args(argptr, nargs);
            return Some(stat_matrix_to_cell(&c));
        }
        _ => return dotted_dispatch2(name, argptr, nargs, ci, pc),
    };
    drop_args(argptr, nargs);
    r.map(make_cell)
}

// ── csv（逐字镜像 datamod.rs parse_csv / escape_field / field_str） ──

fn dotted_parse_csv(text: &str) -> Vec<Vec<String>> {
    let b: Vec<char> = text.chars().collect();
    let n = b.len();
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut i = 0usize;
    let mut in_quotes = false;
    while i < n {
        let c = b[i];
        if in_quotes {
            if c == '"' {
                if i + 1 < n && b[i + 1] == '"' {
                    field.push('"');
                    i += 2;
                    continue;
                }
                in_quotes = false;
                i += 1;
                continue;
            }
            field.push(c);
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_quotes = true;
                i += 1;
            }
            ',' => {
                row.push(std::mem::take(&mut field));
                i += 1;
            }
            '\r' => {
                if i + 1 < n && b[i + 1] == '\n' {
                    i += 1;
                }
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
                i += 1;
            }
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
                i += 1;
            }
            _ => {
                field.push(c);
                i += 1;
            }
        }
    }
    if !field.is_empty() || !row.is_empty() || in_quotes {
        row.push(field);
        rows.push(row);
    }
    rows
}

fn dotted_escape_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// csv.stringify 单元格字符串化（镜像 datamod.rs field_str：标量直转，容器走 display）。
fn dotted_field_str(h: u64) -> String {
    let c = cell(h);
    match c.t {
        T_STR => String::from_utf8_lossy(str_bytes(h)).into_owned(),
        _ => handle_display(h),
    }
}

// ── diff（逐字镜像 textmod.rs split_lines / lcs_diff） ──

fn dotted_split_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();
    if lines.last().map(|s| s.is_empty()).unwrap_or(false) {
        lines.pop();
    }
    lines
}

fn dotted_lcs_diff(a: &[String], b: &[String]) -> Vec<(char, String)> {
    let n = a.len();
    let m = b.len();
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut out = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push((' ', a[i].clone()));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            out.push(('-', a[i].clone()));
            i += 1;
        } else {
            out.push(('+', b[j].clone()));
            j += 1;
        }
    }
    while i < n {
        out.push(('-', a[i].clone()));
        i += 1;
    }
    while j < m {
        out.push(('+', b[j].clone()));
        j += 1;
    }
    out
}

// ── glob（手写匹配器替换 regex：* 单层 / ** 跨层 / ? 单字符 / [a-z] 字符类） ──
// 语义镜像 sysutilmod.rs glob_to_regex 的翻译结果（全路径匹配，'/' 不被 * 跨越）。

/// 匹配一个字符类 [spec]（spec 含 [、可能 ^ 前缀、字符与 a-z 区间）。
fn dotted_glob_class(spec: &[char], c: char) -> bool {
    // spec = 类内字符序列（已去掉外层方括号与 '^' 前缀）
    let negated = spec.first() == Some(&'^');
    let body = if negated { &spec[1..] } else { spec };
    let mut hit = false;
    let mut i = 0;
    while i < body.len() {
        if i + 2 < body.len() && body[i + 1] == '-' {
            if c >= body[i] && c <= body[i + 2] {
                hit = true;
            }
            i += 3;
        } else {
            if c == body[i] {
                hit = true;
            }
            i += 1;
        }
    }
    hit != negated
}

/// 递归 glob 匹配（p/s 为字符切片）：* 单层（不跨 '/'）、** 跨目录（跳过后续 '/'）、
/// ? 单字符（不含 '/'）、[a-z]/[^abc] 字符类（未闭合 '[' 按字面）。递归深度 = 星号数×匹配长度，
/// glob 模式短，无爆栈风险。
fn dotted_glob_match(p: &[char], s: &[char]) -> bool {
    if p.is_empty() {
        return s.is_empty();
    }
    match p[0] {
        '*' => {
            if p.len() > 1 && p[1] == '*' {
                // ** ：跳过多余 '/' 后匹配任意字符序列（含空）
                let mut np = 2;
                while np < p.len() && p[np] == '/' {
                    np += 1;
                }
                let rest = &p[np..];
                for k in 0..=s.len() {
                    if dotted_glob_match(rest, &s[k..]) {
                        return true;
                    }
                }
                false
            } else {
                // 单层 * ：匹配本层任意字符序列（含空，不跨 '/'）
                let rest = &p[1..];
                for k in 0..=s.len() {
                    if k > 0 && s[k - 1] == '/' {
                        break;
                    }
                    if dotted_glob_match(rest, &s[k..]) {
                        return true;
                    }
                }
                false
            }
        }
        '?' => s.first() != Some(&'/') && dotted_glob_match(&p[1..], &s[1..]),
        '[' => {
            if s.is_empty() {
                return false;
            }
            // 找闭合 ']'（首 ']' 视为字面量，镜像 glob_to_regex）
            let mut j = 1;
            if j < p.len() && (p[j] == '^' || p[j] == '!') {
                j += 1;
            }
            if j < p.len() && p[j] == ']' {
                j += 1;
            }
            while j < p.len() && p[j] != ']' {
                j += 1;
            }
            if j < p.len() {
                dotted_glob_class(&p[1..j], s[0]) && dotted_glob_match(&p[j + 1..], &s[1..])
            } else {
                // 未闭合：按字面 '[' 匹配
                s[0] == '[' && dotted_glob_match(&p[1..], &s[1..])
            }
        }
        lc => s.first() == Some(&lc) && dotted_glob_match(&p[1..], &s[1..]),
    }
}

fn dotted_glob_base(pattern: &str) -> String {
    let pat = pattern.replace('\\', "/");
    let cut = pat
        .char_indices()
        .find(|(_, c)| matches!(c, '*' | '?' | '['))
        .map(|(i, _)| i)
        .unwrap_or(pat.len());
    let prefix = &pat[..cut];
    match prefix.rfind('/') {
        Some(i) => {
            let base = &prefix[..i];
            if base.is_empty() {
                ".".to_string()
            } else {
                base.to_string()
            }
        }
        None => ".".to_string(),
    }
}

/// 递归收集 base 下所有文件的相对路径（/ 分隔，文件名排序——镜像 collect_files）。
fn dotted_collect_files(base: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) -> std::io::Result<()> {
    let rd = std::fs::read_dir(dir)?;
    let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            dotted_collect_files(base, &path, out)?;
        } else if path.is_file() {
            if let Ok(rel) = path.strip_prefix(base) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    Ok(())
}

// ── ptr（逐字镜像 ptrmod.rs 分配表模型） ──

static DOTTED_ALLOCS: std::sync::Mutex<Vec<(usize, usize)>> = std::sync::Mutex::new(Vec::new());

fn dotted_alloc_bytes(size: usize) -> usize {
    if size == 0 {
        return 0;
    }
    let layout = match std::alloc::Layout::from_size_align(size, 8) {
        Ok(l) => l,
        Err(_) => return 0,
    };
    let p = unsafe { std::alloc::alloc(layout) };
    if p.is_null() {
        return 0;
    }
    DOTTED_ALLOCS.lock().unwrap().push((p as usize, size));
    p as usize
}

fn dotted_free_bytes(addr: usize) -> bool {
    let mut all = DOTTED_ALLOCS.lock().unwrap();
    if let Some(pos) = all.iter().position(|(a, _)| *a == addr) {
        let size = all.remove(pos).1;
        if let Ok(layout) = std::alloc::Layout::from_size_align(size, 8) {
            unsafe { std::alloc::dealloc(addr as *mut u8, layout) };
        }
        true
    } else {
        false
    }
}

/// ptr 句柄读取（Value::Ptr / 非负 int，镜像 ptrmod.rs as_addr）。
fn dotted_addr(h: u64, arg: usize, ci: u32, pc: u32) -> Result<usize, Hnv> {
    let c = cell(h);
    match c.t {
        T_PTR => Ok(c.a as usize),
        T_INT if (c.a as i64) >= 0 => Ok(c.a as usize),
        _ => Err(err_h_help(
            "H001",
            &format!("`ptr.*` expects a `ptr` for argument {}, got `{}`", arg + 1, type_name(c.t)),
            ci,
            pc,
            Some("pass a `ptr` value (from ptr.alloc or an FFI call) or `0` for NULL".to_string()),
        )),
    }
}

/// 校验可访问区间 [addr, addr+width)（镜像 ptrmod.rs check_range 文案）。
fn dotted_check_range(addr: usize, width: usize, offset: i64, what: &str, ci: u32, pc: u32) -> Result<usize, Hnv> {
    if addr == 0 {
        return Err(err_h_help(
            "H304",
            &format!("`{}`: cannot access a null pointer (0)", what),
            ci,
            pc,
            Some("check the pointer with `ptr.is_null` before dereferencing".to_string()),
        ));
    }
    if offset < 0 {
        return Err(err_h_help(
            "H304",
            &format!("`{}`: negative offset `{}`", what, offset),
            ci,
            pc,
            Some("offsets must be non-negative".to_string()),
        ));
    }
    let all = DOTTED_ALLOCS.lock().unwrap();
    let entry = all.iter().find(|(a, _)| *a == addr);
    let size = match entry {
        Some((_, s)) => *s,
        None => {
            return Err(err_h_help(
                "H304",
                &format!(
                    "`{}`: pointer 0x{:x} is not allocated by `ptr.alloc` (already freed, never allocated, or an external FFI handle)",
                    what, addr
                ),
                ci,
                pc,
                Some("only pointers returned by `ptr.alloc` can be read/written/freed".to_string()),
            ));
        }
    };
    let end = match (offset as usize).checked_add(width) {
        Some(e) => e,
        None => return Err(err_h("H304", &format!("`{}`: offset overflow", what), ci, pc)),
    };
    if end > size {
        return Err(err_h_help(
            "H305",
            &format!("`{}`: out of bounds (block size {}, offset {} + {} bytes)", what, size, offset, width),
            ci,
            pc,
            Some("check the offset against `ptr.size(p)`".to_string()),
        ));
    }
    Ok(addr + offset as usize)
}

// ── server（逐字镜像 srvmod.rs：队列 + 响应通道） ──

struct DottedReq {
    id: u64,
    method: String,
    path: String,
    body: String,
}
struct DottedResp {
    status: u16,
    body: String,
}
static DOTTED_QUEUE: std::sync::Mutex<Vec<DottedReq>> = std::sync::Mutex::new(Vec::new());
static DOTTED_RESPONDERS: std::sync::Mutex<Vec<(u64, std::sync::mpsc::SyncSender<DottedResp>)>> =
    std::sync::Mutex::new(Vec::new());
static DOTTED_NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn dotted_json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn dotted_content_type(path: &str) -> String {
    let p = path.split('?').next().unwrap_or("");
    let ext = p.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let base = match ext.as_str() {
        "html" | "htm" => "text/html",
        "js" | "mjs" => "text/javascript",
        "css" => "text/css",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "txt" => "text/plain",
        _ => "text/html",
    };
    if base.starts_with("text/") || base.contains("json") || base.contains("svg") {
        format!("{}; charset=utf-8", base)
    } else {
        base.to_string()
    }
}

fn dotted_write_resp(stream: &mut std::net::TcpStream, status: u16, ctype: &str, body: &str) -> std::io::Result<()> {
    use std::io::{Read as _, Write as _};
    let reason = match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Status",
    };
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
        status,
        reason,
        ctype,
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

fn dotted_read_request(stream: &mut std::net::TcpStream) -> Option<(String, String, String)> {
    use std::io::Read as _;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        match stream.read(&mut chunk) {
            Ok(0) => return None,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
                if buf.len() > 64 * 1024 {
                    return None;
                }
            }
            Err(_) => return None,
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let req_line: Vec<&str> = lines.next()?.split_whitespace().collect();
    if req_line.len() < 2 {
        return None;
    }
    let method = req_line[0].to_string();
    let path = req_line[1].to_string();
    let mut clen = 0usize;
    for line in lines {
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            clen = v.trim().parse().ok()?;
        }
    }
    let mut body = buf[header_end..].to_vec();
    while body.len() < clen {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    body.truncate(clen);
    let body_str = String::from_utf8_lossy(&body).to_string();
    Some((method, path, body_str))
}

fn dotted_handle_conn(mut stream: std::net::TcpStream) {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
    let (method, path, body) = match dotted_read_request(&mut stream) {
        Some(r) => r,
        None => {
            let _ = dotted_write_resp(&mut stream, 400, "text/plain; charset=utf-8", "bad request");
            return;
        }
    };
    let id = DOTTED_NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (tx, rx) = std::sync::mpsc::sync_channel::<DottedResp>(1);
    DOTTED_RESPONDERS.lock().unwrap().push((id, tx));
    {
        let mut q = DOTTED_QUEUE.lock().unwrap();
        if q.len() >= 1024 {
            q.remove(0);
        }
        q.push(DottedReq { id, method, path: path.clone(), body });
    }
    let wait = match rx.recv_timeout(std::time::Duration::from_secs(120)) {
        Ok(r) => (r.status, r.body),
        Err(_) => {
            DOTTED_RESPONDERS.lock().unwrap().retain(|(i, _)| *i != id);
            let _ = dotted_write_resp(&mut stream, 504, "text/plain; charset=utf-8", "no response from script");
            return;
        }
    };
    let ctype = dotted_content_type(&path);
    if dotted_write_resp(&mut stream, wait.0, &ctype, &wait.1).is_err() {
        DOTTED_RESPONDERS.lock().unwrap().retain(|(i, _)| *i != id);
    }
}

// ── yaml（逐字镜像 plotmod.rs YAML 子集解析/序列化） ──

fn dotted_strip_comment(line: &str) -> String {
    let mut in_single = false;
    let mut in_double = false;
    for (i, c) in line.char_indices() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '#' if !in_single && !in_double => return line[..i].to_string(),
            _ => {}
        }
    }
    line.to_string()
}

/// YAML 标量解析（镜像 plotmod.rs parse_scalar：数值 / 布尔 / null / 引号字符串 / 裸串）。
/// 返回句柄（str 直接建 cell）。
fn dotted_yaml_scalar(s: &str) -> u64 {
    let t = s.trim();
    if t.is_empty() {
        return make_cell(Hnv::null());
    }
    if (t.starts_with('"') && t.ends_with('"') && t.len() >= 2)
        || (t.starts_with('\'') && t.ends_with('\'') && t.len() >= 2)
    {
        return make_cell_str(t[1..t.len() - 1].as_bytes().to_vec());
    }
    match t {
        "true" | "True" | "TRUE" => return make_cell(Hnv::bool_(true)),
        "false" | "False" | "FALSE" => return make_cell(Hnv::bool_(false)),
        "null" | "Null" | "NULL" | "~" => return make_cell(Hnv::null()),
        _ => {}
    }
    if let Ok(i) = t.parse::<i64>() {
        return make_cell(Hnv::int(i));
    }
    if let Ok(f) = t.parse::<f64>() {
        return make_cell(Hnv::float(f));
    }
    make_cell_str(t.as_bytes().to_vec())
}

/// 点号内置分发第二段（G2.9）：csv / diff / temp / ptr / glob / server / plot / yaml
/// + 需外部 crate 的 H999 登记族。
fn dotted_dispatch2(name: &str, argptr: u64, nargs: u64, ci: u32, pc: u32) -> Option<u64> {
    let a0 = || arg_at(argptr, 0);
    let a1 = || arg_at(argptr, 1);
    let a2 = || arg_at(argptr, 2);
    let r: Option<Hnv> = match name {
        // ---------- csv ----------
        "csv.parse" => {
            let text = match d_str(a0(), name, 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let rows = dotted_parse_csv(&text);
            let row_cells: Vec<u64> = rows
                .iter()
                .map(|r| {
                    let items: Vec<u64> = r.iter().map(|f| make_cell_str(f.as_bytes().to_vec())).collect();
                    let c = make_cell_list(&items);
                    for &it in &items {
                        cell_release(it);
                    }
                    c
                })
                .collect();
            let c = make_cell_list(&row_cells);
            for &rc in &row_cells {
                cell_release(rc);
            }
            drop_args(argptr, nargs);
            return Some(c);
        }
        "csv.parse_dict" => {
            let text = match d_str(a0(), name, 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let rows = dotted_parse_csv(&text);
            let mut it = rows.into_iter();
            let headers = match it.next() {
                Some(h) => h,
                None => {
                    drop_args(argptr, nargs);
                    return Some(make_cell_list(&[]));
                }
            };
            let mut out_rows: Vec<u64> = Vec::new();
            for row in it {
                let pairs: Vec<(String, u64)> = headers
                    .iter()
                    .enumerate()
                    .map(|(idx, h)| (h.clone(), make_cell_str(row.get(idx).cloned().unwrap_or_default().into_bytes())))
                    .collect();
                let dc = make_cell_dict(&pairs);
                for (_, v) in &pairs {
                    cell_release(*v);
                }
                out_rows.push(dc);
            }
            let c = make_cell_list(&out_rows);
            for &rc in &out_rows {
                cell_release(rc);
            }
            drop_args(argptr, nargs);
            return Some(c);
        }
        "csv.stringify" => {
            let rows_h = a0();
            let row_items = match d_list(rows_h) {
                Some(it) => it,
                None => {
                    let ty = type_name(cell(rows_h).t);
                    drop_args(argptr, nargs);
                    return Some(make_cell(err_h_help(
                        "H001",
                        &format!("`csv.stringify` expects a list of rows, got `{}`", ty),
                        ci,
                        pc,
                        Some("pass [[\"a\", 1], [\"b\", 2]] or a list of dicts".to_string()),
                    )));
                }
            };
            let mut out = String::new();
            for (ri, &row) in row_items.iter().enumerate() {
                let rc = cell(row);
                match rc.pkind {
                    P_LIST => {
                        let l = unsafe { &*(rc.payload as *const HnList) };
                        let cells: Vec<String> = l.items.iter().map(|&h| dotted_escape_field(&dotted_field_str(h))).collect();
                        out.push_str(&cells.join(","));
                        out.push('\n');
                    }
                    P_DICT => {
                        let d = unsafe { &*(rc.payload as *const HnDict) };
                        let cells: Vec<String> = d
                            .pairs
                            .iter()
                            .filter(|(k, _)| !is_hidden_key(k))
                            .map(|(_, v)| dotted_escape_field(&dotted_field_str(*v)))
                            .collect();
                        out.push_str(&cells.join(","));
                        out.push('\n');
                    }
                    _ => {
                        let ty = type_name(rc.t);
                        drop_args(argptr, nargs);
                        return Some(make_cell(err_h(
                            "H001",
                            &format!("`csv.stringify` row {} must be a list or dict, got `{}`", ri + 1, ty),
                            ci,
                            pc,
                        )));
                    }
                }
            }
            drop_args(argptr, nargs);
            return Some(make_cell_str(out.into_bytes()));
        }
        // ---------- diff ----------
        "diff.lines" => {
            let sa = match d_str(a0(), name, 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let sb = match d_str(a1(), name, 1, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let ops = dotted_lcs_diff(&dotted_split_lines(&sa), &dotted_split_lines(&sb));
            let items: Vec<u64> = ops
                .into_iter()
                .map(|(op, line)| {
                    let pairs = vec![
                        ("op".to_string(), make_cell_str(op.to_string().into_bytes())),
                        ("line".to_string(), make_cell_str(line.into_bytes())),
                    ];
                    let dc = make_cell_dict(&pairs);
                    for (_, v) in &pairs {
                        cell_release(*v);
                    }
                    dc
                })
                .collect();
            let c = make_cell_list(&items);
            for &it in &items {
                cell_release(it);
            }
            drop_args(argptr, nargs);
            return Some(c);
        }
        "diff.unified" => {
            let sa = match d_str(a0(), name, 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let sb = match d_str(a1(), name, 1, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let ops = dotted_lcs_diff(&dotted_split_lines(&sa), &dotted_split_lines(&sb));
            let mut out = String::new();
            let (mut adds, mut dels) = (0i64, 0i64);
            for (op, line) in &ops {
                match op {
                    '+' => {
                        adds += 1;
                        out.push_str(&format!("+{}\n", line));
                    }
                    '-' => {
                        dels += 1;
                        out.push_str(&format!("-{}\n", line));
                    }
                    _ => out.push_str(&format!(" {}\n", line)),
                }
            }
            let header = format!("@@ -1,{} +1,{} @@\n", dels.max(1), adds.max(1));
            let mut result = String::new();
            result.push_str(&header);
            result.push_str(&out);
            drop_args(argptr, nargs);
            return Some(make_cell_str(result.into_bytes()));
        }
        // ---------- temp（镜像 sysutilmod.rs：pid-纳秒-序号） ----------
        "temp.dir" | "temp.file" => {
            // 可选 prefix（null/缺省 → "hone-"）；checker 保证类型
            let prefix = if nargs >= 1 && cell(a0()).t == T_STR {
                match d_str(a0(), name, 0, ci, pc) {
                    Ok(v) => v,
                    Err(e) => return Some(make_cell(e)),
                }
            } else {
                "hone-".to_string()
            };
            let seq = DOTTED_TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let fname = format!(
                "{}{}-{}-{}",
                prefix,
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0),
                seq
            );
            let path = std::env::temp_dir().join(fname);
            let res = if name == "temp.dir" {
                std::fs::create_dir_all(&path).map(|_| path.clone())
            } else {
                std::fs::File::create(&path).map(|_| path.clone())
            };
            match res {
                Ok(p) => {
                    let out = p.to_string_lossy().to_string();
                    drop_args(argptr, nargs);
                    return Some(make_cell_str(out.into_bytes()));
                }
                Err(e) => {
                    let kind = if name == "temp.dir" { "dir" } else { "file" };
                    drop_args(argptr, nargs);
                    return Some(make_cell(err_h(
                        "H300",
                        &format!("cannot create temp {} `{}`: {}", kind, path.display(), e),
                        ci,
                        pc,
                    )));
                }
            }
        }
        "temp.remove" => {
            let path = match d_str(a0(), name, 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let p = std::path::Path::new(&path);
            let ok = if p.is_dir() {
                std::fs::remove_dir_all(p).is_ok()
            } else {
                std::fs::remove_file(p).is_ok()
            };
            Some(Hnv::bool_(ok))
        }
        // ---------- glob ----------
        "glob.match" => {
            let pattern = match d_str(a0(), name, 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let path = match d_str(a1(), name, 1, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let pchars: Vec<char> = pattern.chars().collect();
            let schars: Vec<char> = path.replace('\\', "/").chars().collect();
            Some(Hnv::bool_(dotted_glob_match(&pchars, &schars)))
        }
        "glob.list" => {
            let pattern = match d_str(a0(), name, 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let base = dotted_glob_base(&pattern);
            let mut rels: Vec<String> = Vec::new();
            if let Err(e) = dotted_collect_files(std::path::Path::new(&base), std::path::Path::new(&base), &mut rels) {
                drop_args(argptr, nargs);
                return Some(make_cell(err_h(
                    "H300",
                    &format!("cannot scan `{}`: {}", base, e),
                    ci,
                    pc,
                )));
            }
            let pchars: Vec<char> = pattern.chars().collect();
            let mut matched: Vec<String> = rels.into_iter().filter(|rel| {
                let schars: Vec<char> = rel.chars().collect();
                dotted_glob_match(&pchars, &schars)
            }).collect();
            matched.sort();
            let items: Vec<u64> = matched.iter().map(|s| make_cell_str(s.as_bytes().to_vec())).collect();
            let c = make_cell_list(&items);
            for &it in &items {
                cell_release(it);
            }
            drop_args(argptr, nargs);
            return Some(c);
        }
        // ---------- ptr ----------
        "ptr.alloc" => {
            let size = match d_int(a0(), name, 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let addr = dotted_alloc_bytes(size.max(0) as usize);
            Some(Hnv { t: T_PTR, a: addr as u64, b: 0 })
        }
        "ptr.free" => {
            let addr = match dotted_addr(a0(), 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            if dotted_free_bytes(addr) {
                Some(Hnv::bool_(true))
            } else {
                drop_args(argptr, nargs);
                let msg = if addr == 0 {
                    "`ptr.free`: freeing a null pointer (no-op)".to_string()
                } else {
                    format!("`ptr.free`: pointer 0x{:x} was not allocated by `ptr.alloc` (double free, or an external FFI handle)", addr)
                };
                return Some(make_cell(err_h_help(
                    "H304",
                    &msg,
                    ci,
                    pc,
                    Some("call `ptr.free` once per `ptr.alloc`; external FFI handles are owned by the library".to_string()),
                )));
            }
        }
        "ptr.is_null" => {
            let addr = match dotted_addr(a0(), 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            Some(Hnv::bool_(addr == 0))
        }
        "ptr.is_valid" => {
            let addr = match dotted_addr(a0(), 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let valid = addr != 0 && DOTTED_ALLOCS.lock().unwrap().iter().any(|(a, _)| *a == addr);
            Some(Hnv::bool_(valid))
        }
        "ptr.size" => {
            let addr = match dotted_addr(a0(), 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            match DOTTED_ALLOCS.lock().unwrap().iter().find(|(a, _)| *a == addr) {
                Some((_, s)) => Some(Hnv::int(*s as i64)),
                None => {
                    drop_args(argptr, nargs);
                    return Some(make_cell(err_h_help(
                        "H304",
                        &format!("`ptr.size`: pointer 0x{:x} is not allocated by `ptr.alloc`", addr),
                        ci,
                        pc,
                        Some("only pointers returned by `ptr.alloc` have a tracked size".to_string()),
                    )));
                }
            }
        }
        "ptr.read_int" | "ptr.read_float" | "ptr.read_byte" => {
            let addr = match dotted_addr(a0(), 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let off = match d_int(a1(), name, 1, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let (width, what) = if name == "ptr.read_byte" { (1usize, "ptr.read_byte") } else { (8usize, name) };
            let p = match dotted_check_range(addr, width, off, what, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            if name == "ptr.read_int" {
                Some(Hnv::int(unsafe { std::ptr::read_unaligned(p as *const i64) }))
            } else if name == "ptr.read_float" {
                Some(Hnv::float(unsafe { std::ptr::read_unaligned(p as *const f64) }))
            } else {
                Some(Hnv::int(unsafe { *(p as *const u8) } as i64))
            }
        }
        "ptr.write_int" | "ptr.write_float" | "ptr.write_byte" => {
            let addr = match dotted_addr(a0(), 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let off = match d_int(a1(), name, 1, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let c2 = cell(a2());
            let (width, what) = if name == "ptr.write_byte" { (1usize, "ptr.write_byte") } else { (8usize, name) };
            if name == "ptr.write_int" || name == "ptr.write_byte" {
                if c2.t != T_INT {
                    drop_args(argptr, nargs);
                    return Some(make_cell(err_h_help(
                        "H001",
                        &format!("`ptr.*` expects an integer for argument 3, got `{}`", type_name(c2.t)),
                        ci,
                        pc,
                        Some("pass an `int` value".to_string()),
                    )));
                }
            } else if c2.t != T_FLOAT && c2.t != T_INT {
                drop_args(argptr, nargs);
                return Some(make_cell(err_h_help(
                    "H001",
                    &format!("`ptr.*` expects a float for argument 3, got `{}`", type_name(c2.t)),
                    ci,
                    pc,
                    Some("pass a `float` value".to_string()),
                )));
            }
            let val = c2.a;
            if name == "ptr.write_byte" && (val as i64) < 0 || name == "ptr.write_byte" && (val as i64) > 255 {
                drop_args(argptr, nargs);
                return Some(make_cell(err_h_help(
                    "H304",
                    &format!("`ptr.write_byte`: value {} out of range 0..=255", val as i64),
                    ci,
                    pc,
                    Some("bytes are unsigned 8-bit values".to_string()),
                )));
            }
            let p = match dotted_check_range(addr, width, off, what, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            match name {
                "ptr.write_int" => unsafe { std::ptr::write_unaligned(p as *mut i64, val as i64) },
                "ptr.write_float" => unsafe {
                    let f = if c2.t == T_FLOAT { f64::from_bits(val) } else { c2.a as i64 as f64 };
                    std::ptr::write_unaligned(p as *mut f64, f)
                },
                _ => unsafe { *(p as *mut u8) = val as u8 },
            }
            Some(Hnv::null())
        }
        // ---------- server（镜像 srvmod.rs call） ----------
        "server.listen" => {
            let port = match cell(a0()).t {
                T_INT => cell(a0()).a as i64,
                other => {
                    let ty = type_name(other);
                    drop_args(argptr, nargs);
                    return Some(make_cell(err_h_help(
                        "H001",
                        &format!("`server.listen` expects an integer port, got `{}`", ty),
                        ci,
                        pc,
                        Some("pass `0` to auto-assign a free port".to_string()),
                    )));
                }
            };
            if !(0..=65535).contains(&port) {
                drop_args(argptr, nargs);
                return Some(make_cell(err_h_help(
                    "H300",
                    &format!("invalid port `{}`", port),
                    ci,
                    pc,
                    Some("port must be in 0..=65535".to_string()),
                )));
            }
            let addr = format!("127.0.0.1:{}", port);
            match std::net::TcpListener::bind(&addr) {
                Ok(listener) => {
                    let actual = match listener.local_addr() {
                        Ok(a) => a.port(),
                        Err(e) => {
                            drop_args(argptr, nargs);
                            return Some(make_cell(err_h("H300", &format!("cannot resolve bound address: {}", e), ci, pc)));
                        }
                    };
                    std::thread::spawn(move || {
                        for stream in listener.incoming() {
                            if let Ok(s) = stream {
                                std::thread::spawn(move || dotted_handle_conn(s));
                            }
                        }
                    });
                    Some(Hnv::int(actual as i64))
                }
                Err(e) => {
                    drop_args(argptr, nargs);
                    return Some(make_cell(err_h_help(
                        "H300",
                        &format!("cannot bind {}: {}", addr, e),
                        ci,
                        pc,
                        Some("check the port is not already in use".to_string()),
                    )));
                }
            }
        }
        "server.poll" => {
            let mut q = DOTTED_QUEUE.lock().unwrap();
            if q.is_empty() {
                drop_args(argptr, nargs);
                return Some(make_cell_str(b"[]".to_vec()));
            }
            let mut out = String::from("[");
            let mut first = true;
            while let Some(r) = q.pop() {
                if !first {
                    out.push(',');
                }
                first = false;
                out.push_str(&format!(
                    "{{\"id\":{},\"method\":{},\"path\":{},\"body\":{}}}",
                    r.id,
                    dotted_json_escape(&r.method),
                    dotted_json_escape(&r.path),
                    dotted_json_escape(&r.body)
                ));
            }
            out.push(']');
            drop(q);
            drop_args(argptr, nargs);
            return Some(make_cell_str(out.into_bytes()));
        }
        "server.respond" => {
            let id = match cell(a0()).t {
                T_INT => cell(a0()).a as i64,
                other => {
                    let ty = type_name(other);
                    drop_args(argptr, nargs);
                    return Some(make_cell(err_h_help(
                        "H001",
                        &format!("`server.respond` expects an integer request id, got `{}`", ty),
                        ci,
                        pc,
                        Some("pass the `id` from `server.poll`".to_string()),
                    )));
                }
            };
            let body = match d_str(a1(), name, 1, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let status = if nargs >= 3 {
                let c3 = cell(a2());
                if c3.t != T_INT {
                    drop_args(argptr, nargs);
                    return Some(make_cell(err_h_help(
                        "H001",
                        &format!("`server.respond` expects an integer status code, got `{}`", type_name(c3.t)),
                        ci,
                        pc,
                        Some("pass the status code as the 3rd argument, e.g. `server.respond(id, body, 404)`".to_string()),
                    )));
                }
                let s = c3.a as i64;
                if !(100..=599).contains(&s) {
                    drop_args(argptr, nargs);
                    return Some(make_cell(err_h_help(
                        "H001",
                        &format!("`server.respond` expects an HTTP status code in 100..=599, got `{}`", s),
                        ci,
                        pc,
                        Some("common codes: 200, 404, 500".to_string()),
                    )));
                }
                s as u16
            } else {
                200u16
            };
            let mut resp = DOTTED_RESPONDERS.lock().unwrap();
            if let Some(pos) = resp.iter().position(|(i, _)| *i == id as u64) {
                let (i, tx) = resp.remove(pos);
                drop(i);
                let _ = tx.send(DottedResp { status, body });
                Some(Hnv::bool_(true))
            } else {
                Some(Hnv::bool_(false))
            }
        }
        // ---------- plot / yaml ----------
        "plot.bar" | "plot.line" => {
            let is_bar = name == "plot.bar";
            let vals = match stat_num_list(a0(), if is_bar { "plot.bar" } else { "plot.line xs" }, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            let labels: Vec<String> = if is_bar {
                if nargs > 1 {
                    let items = match d_list(a1()) {
                        Some(it) => it,
                        None => {
                            let ty = type_name(cell(a1()).t);
                            drop_args(argptr, nargs);
                            return Some(make_cell(err_h(
                                "H001",
                                &format!("`plot.bar labels` expects a list of strings, got `{}`", ty),
                                ci,
                                pc,
                            )));
                        }
                    };
                    let mut ls = Vec::with_capacity(items.len());
                    for (i, &x) in items.iter().enumerate() {
                        match d_str(x, name, i, ci, pc) {
                            Ok(s) => ls.push(s),
                            Err(e) => return Some(make_cell(e)),
                        }
                    }
                    ls
                } else {
                    (0..vals.len()).map(|i| i.to_string()).collect()
                }
            } else {
                Vec::new()
            };
            let ys = if is_bar { Vec::new() } else {
                match stat_num_list(a1(), "plot.line ys", ci, pc) {
                    Ok(v) => v,
                    Err(e) => return Some(make_cell(e)),
                }
            };
            let out = if is_bar {
                dotted_plot_bar(&vals, &labels)
            } else {
                dotted_plot_line(&vals, &ys)
            };
            match out {
                Ok(s) => {
                    drop_args(argptr, nargs);
                    return Some(make_cell_str(s.into_bytes()));
                }
                Err(msg) => {
                    drop_args(argptr, nargs);
                    return Some(make_cell(err_h_help(msg.0, &msg.1, ci, pc, msg.2.map(|s| s.to_string()))));
                }
            }
        }
        "yaml.parse" => {
            let text = match d_str(a0(), name, 0, ci, pc) {
                Ok(v) => v,
                Err(e) => return Some(make_cell(e)),
            };
            match dotted_parse_yaml(&text) {
                Ok(c) => {
                    drop_args(argptr, nargs);
                    return Some(c);
                }
                Err(msg) => {
                    drop_args(argptr, nargs);
                    return Some(make_cell(err_h_help(
                        "H005",
                        &format!("yaml: expected `key: value` at line `{}`", msg),
                        ci,
                        pc,
                        Some("map entries must be `key: value`".to_string()),
                    )));
                }
            }
        }
        "yaml.stringify" => {
            let out = match dotted_stringify_yaml(a0(), 0) {
                Ok(s) => s,
                Err(()) => {
                    drop_args(argptr, nargs);
                    return Some(make_cell(err_h("H001", &format!("`yaml.stringify` cannot serialize `{}`", type_name(cell(a0()).t)), ci, pc)));
                }
            };
            drop_args(argptr, nargs);
            return Some(make_cell_str(out.into_bytes()));
        }
        // ---------- H999 登记：需外部 crate / 平台 API 的家族（std-only runtime 不可移植） ----------
        "regex.match" | "regex.replace" | "regex.find" | "regex.groups" | "regex.split"
        | "sqlite.open" | "sqlite.close" | "sqlite.exec" | "sqlite.query" | "sqlite.query_one"
        | "sqlite.escape" | "sqlite.last_insert_id" | "sqlite.changes"
        | "zlib.compress" | "zlib.decompress" | "zlib.gzip" | "zlib.gunzip"
        | "archive.zip_list" | "archive.zip_read" | "archive.zip_extract" | "archive.zip_create"
        | "archive.tgz_list" | "archive.tgz_read" | "archive.tgz_extract" | "archive.tgz_create"
        | "http.request" | "http.sse_open" | "http.sse_next" | "http.sse_close"
        | "smtp.send" | "ws.request"
        | "crypto.md5" | "crypto.sha1" | "crypto.sha256" | "crypto.hmac_sha256"
        | "crypto.base64_encode" | "crypto.base64_decode"
        | "plugin.load" | "plugin.has" | "plugin.list" | "plugin.unload"
        | "guipro.available" | "guipro.window" | "guipro.add" | "guipro.poll"
        | "guipro.set_text" | "guipro.get_text" | "guipro.set_value" | "guipro.get_value"
        | "guipro.close" | "guipro.msgbox"
        | "guipro.table_add_row" | "guipro.table_clear" | "guipro.table_count"
        | "guipro.table_get" | "guipro.table_get_row" | "guipro.table_set"
        | "guipro.tree_add" | "guipro.tree_clear" | "guipro.tree_get"
        | "guipro.canvas_clear" | "guipro.canvas_line" | "guipro.canvas_rect"
        | "guipro.canvas_ellipse" | "guipro.canvas_text" | "guipro.canvas_repaint"
        | "guipro.tray_add" | "guipro.tray_tip" | "guipro.tray_remove" | "guipro.menu"
        | "guipro.pet_window" | "guipro.pet_frame" | "guipro.pet_text" | "guipro.pet_move"
        | "guipro.pet_pos" | "guipro.pet_cursor" | "guipro.pet_menu" | "guipro.pet_close"
        | "sys.msgbox" | "sys.beep" | "sys.clipboard_set" | "sys.get_screen_size" | "sys.reg_read" | "sys.reg_write" => {
            drop_args(argptr, nargs);
            return Some(make_cell(err_h_help(
                "H999",
                &format!("原生编译后端暂不支持 `{}`（见 M2 G2.9 点号内置族：依赖外部 crate，无法进入 std-only runtime）", name),
                ci,
                pc,
                Some("用解释器运行同一脚本：`hone run <file.hn>`（输出与退出码与本后端一致）".to_string()),
            )));
        }
        _ => return None,
    };
    drop_args(argptr, nargs);
    r.map(make_cell)
}

/// plot.bar SVG（逐字镜像 plotmod.rs：60px 柱宽、#4c6ef5、基线）。
fn dotted_plot_bar(values: &[f64], labels: &[String]) -> Result<String, (&'static str, String, Option<&'static str>)> {
    if values.is_empty() {
        return Err(("H001", "plot.bar: empty values".to_string(), Some("pass at least one value")));
    }
    let n = values.len();
    let max_v = values.iter().cloned().fold(0.0f64, |a, b| a.max(b)).max(1.0);
    let w = (n as f64) * 60.0 + 80.0;
    let h = 320.0f64;
    let pad = 40.0f64;
    let chart_h = h - pad - 30.0;
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let fmt_num = |x: f64| -> String {
        if x.fract() == 0.0 && x.abs() < 9.007_199_254_740_992e15 {
            format!("{}", x as i64)
        } else {
            format!("{:.2}", x)
        }
    };
    let mut out = String::new();
    out.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\">\n",
        w, h, w, h
    ));
    out.push_str(&format!("<rect x=\"0\" y=\"0\" width=\"{}\" height=\"{}\" fill=\"#f8f9fa\"/>\n", w, h));
    let bar_w = 60.0f64;
    for (i, v) in values.iter().enumerate() {
        let bh = (v / max_v * chart_h).abs();
        let x = pad + i as f64 * 60.0;
        let y = pad + chart_h - bh;
        out.push_str(&format!(
            "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" fill=\"#4c6ef5\" rx=\"2\"/>\n",
            x, y, bar_w - 8.0, bh
        ));
        out.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"12\" fill=\"#333\" text-anchor=\"middle\">{}</text>\n",
            x + (bar_w - 8.0) / 2.0,
            y - 5.0,
            fmt_num(*v)
        ));
        if i < labels.len() {
            out.push_str(&format!(
                "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"11\" fill=\"#555\" text-anchor=\"middle\">{}</text>\n",
                x + (bar_w - 8.0) / 2.0,
                h - 10.0,
                esc(&labels[i])
            ));
        }
    }
    out.push_str(&format!(
        "<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" stroke=\"#999\" stroke-width=\"1\"/>\n",
        pad, pad + chart_h, pad + n as f64 * 60.0, pad + chart_h
    ));
    out.push_str("</svg>\n");
    Ok(out)
}

/// plot.line SVG（逐字镜像 plotmod.rs：640x320、4 条网格线、polyline + 数据点）。
fn dotted_plot_line(xs: &[f64], ys: &[f64]) -> Result<String, (&'static str, String, Option<&'static str>)> {
    if xs.len() != ys.len() || xs.is_empty() {
        return Err((
            "H001",
            "plot.line: xs and ys must be non-empty lists of equal length".to_string(),
            Some("pass two lists of the same length"),
        ));
    }
    let n = xs.len();
    let w = 640.0f64;
    let h = 320.0f64;
    let pad = 40.0f64;
    let chart_w = w - pad * 2.0;
    let chart_h = h - pad * 2.0;
    let min_x = xs.iter().cloned().fold(f64::INFINITY, f64::min);
    let max_x = xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let min_y = ys.iter().cloned().fold(f64::INFINITY, f64::min);
    let max_y = ys.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let sx = if max_x > min_x { (max_x - min_x) / chart_w } else { 1.0 };
    let syy = if max_y > min_y { (max_y - min_y) / chart_h } else { 1.0 };
    let syoff = min_y;
    let px = |x: f64| pad + (x - min_x) / sx;
    let py = |y: f64| pad + chart_h - (y - syoff) / syy;
    let fmt_num = |x: f64| -> String {
        if x.fract() == 0.0 && x.abs() < 9.007_199_254_740_992e15 {
            format!("{}", x as i64)
        } else {
            format!("{:.2}", x)
        }
    };
    let mut out = String::new();
    out.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\">\n",
        w, h, w, h
    ));
    out.push_str(&format!("<rect x=\"0\" y=\"0\" width=\"{}\" height=\"{}\" fill=\"#f8f9fa\"/>\n", w, h));
    for i in 0..=4 {
        let gy = pad + chart_h * i as f64 / 4.0;
        let val = max_y - (max_y - min_y) * i as f64 / 4.0;
        out.push_str(&format!(
            "<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" stroke=\"#ddd\" stroke-width=\"1\"/>\n",
            pad, gy, w - pad, gy
        ));
        out.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"11\" fill=\"#666\" text-anchor=\"end\">{}</text>\n",
            pad - 6.0,
            gy + 4.0,
            fmt_num(val)
        ));
    }
    let mut pts = String::new();
    for i in 0..n {
        pts.push_str(&format!("{:.1},{:.1} ", px(xs[i]), py(ys[i])));
    }
    out.push_str(&format!(
        "<polyline points=\"{}\" fill=\"none\" stroke=\"#4c6ef5\" stroke-width=\"2\"/>\n",
        pts.trim()
    ));
    for i in 0..n {
        out.push_str(&format!(
            "<circle cx=\"{:.1}\" cy=\"{:.1}\" r=\"3\" fill=\"#4c6ef5\"/>\n",
            px(xs[i]),
            py(ys[i])
        ));
    }
    out.push_str("</svg>\n");
    Ok(out)
}

/// YAML 行预处理（去注释/空行，记缩进）。
fn dotted_yaml_lines(text: &str) -> Vec<(usize, String)> {
    let mut lines: Vec<(usize, String)> = Vec::new();
    for raw in text.lines() {
        let stripped = dotted_strip_comment(raw);
        if stripped.trim().is_empty() {
            continue;
        }
        let indent = stripped.chars().take_while(|c| *c == ' ').count();
        lines.push((indent, stripped.trim_end().to_string()));
    }
    lines
}

/// 拆分 `key: value`（引号感知，镜像 split_key_value）。
fn dotted_split_key_value(line: &str) -> Option<(String, String)> {
    let mut in_single = false;
    let mut in_double = false;
    let chars: Vec<char> = line.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            ':' if !in_single && !in_double => {
                let key = line[..i].trim().to_string();
                if key.is_empty() {
                    return None;
                }
                let val = line[i + 1..].trim_start().to_string();
                return Some((key, val));
            }
            _ => {}
        }
    }
    None
}

/// 解析一个块（map 或 list），返回句柄；Err(line) = 缺 `key: value` 的行内容。
fn dotted_yaml_block(lines: &[(usize, String)], idx: &mut usize, indent: usize) -> Result<u64, String> {
    if *idx >= lines.len() {
        return Ok(make_cell(Hnv::null()));
    }
    let (first_indent, first) = &lines[*idx];
    if *first_indent < indent {
        return Ok(make_cell(Hnv::null()));
    }
    if first.trim_start().starts_with("-") && first.trim_start().len() > 1 {
        let mut items: Vec<u64> = Vec::new();
        while *idx < lines.len() {
            let (ind, line) = &lines[*idx];
            if *ind < indent {
                break;
            }
            let content = line.trim_start();
            if !content.starts_with('-') {
                break;
            }
            let rest = content[1..].trim_start().to_string();
            *idx += 1;
            if rest.is_empty() {
                let item = dotted_yaml_block(lines, idx, indent + 1)?;
                items.push(item);
            } else if let Some((k, v)) = dotted_split_key_value(&rest) {
                let mut pairs: Vec<(String, u64)> = Vec::new();
                if v.trim().is_empty() {
                    let child = dotted_yaml_block(lines, idx, *ind + 1)?;
                    pairs.push((k, child));
                } else {
                    pairs.push((k, dotted_yaml_scalar(&v)));
                }
                let dc = make_cell_dict(&pairs);
                for (_, v) in &pairs {
                    cell_release(*v);
                }
                items.push(dc);
            } else {
                items.push(dotted_yaml_scalar(&rest));
            }
        }
        let c = make_cell_list(&items);
        for &it in &items {
            cell_release(it);
        }
        return Ok(c);
    }
    let mut pairs: Vec<(String, u64)> = Vec::new();
    while *idx < lines.len() {
        let (ind, line) = &lines[*idx];
        if *ind < indent {
            break;
        }
        let content = line.trim_start();
        if content.is_empty() || content.starts_with('-') {
            break;
        }
        let (k, v) = match dotted_split_key_value(content) {
            Some(pair) => pair,
            None => return Err(content.clone().to_string()),
        };
        *idx += 1;
        if v.trim().is_empty() {
            let child = dotted_yaml_block(lines, idx, *ind + 1)?;
            pairs.push((k, child));
        } else {
            pairs.push((k, dotted_yaml_scalar(&v)));
        }
    }
    let dc = make_cell_dict(&pairs);
    for (_, v) in &pairs {
        cell_release(*v);
    }
    Ok(dc)
}

fn dotted_parse_yaml(text: &str) -> Result<u64, String> {
    let lines = dotted_yaml_lines(text);
    if lines.is_empty() {
        return Ok(make_cell(Hnv::null()));
    }
    let mut idx = 0usize;
    let base = lines[0].0;
    dotted_yaml_block(&lines, &mut idx, base)
}

/// yaml_quote（镜像 plotmod.rs：含 :/#/前导-/首尾空白/空 → 双引号包裹并转义 "）。
fn dotted_yaml_quote(s: &str) -> String {
    if s.contains(':') || s.contains('#') || s.starts_with('-') || s.trim() != s || s.is_empty() {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

/// 标量字符串化（str/int/float/bool/null；容器 None）。
fn dotted_yaml_scalar_str(h: u64) -> Option<String> {
    let c = cell(h);
    match c.t {
        T_STR => Some(String::from_utf8_lossy(str_bytes(h)).into_owned()),
        T_INT => Some((c.a as i64).to_string()),
        T_FLOAT => Some(f64::from_bits(c.a).to_string()),
        T_BOOL_TRUE => Some("true".to_string()),
        T_BOOL_FALSE => Some("false".to_string()),
        T_NULL => Some("null".to_string()),
        _ => None,
    }
}

/// yaml.stringify（镜像 plotmod.rs stringify_yaml；Err(()) = 不可序列化类型）。
fn dotted_stringify_yaml(h: u64, depth: usize) -> Result<String, ()> {
    let pad = "  ".repeat(depth);
    let c = cell(h);
    match c.pkind {
        P_DICT => {
            let d = unsafe { &*(c.payload as *const HnDict) };
            let visible: Vec<&(String, u64)> = d.pairs.iter().filter(|(k, _)| !is_hidden_key(k)).collect();
            if visible.is_empty() {
                return Ok(format!("{}null", pad));
            }
            let mut out = String::new();
            for (k, val) in visible {
                match dotted_yaml_scalar_str(*val) {
                    Some(s) => out.push_str(&format!("{}{}: {}\n", pad, dotted_yaml_quote(k), s)),
                    None => {
                        out.push_str(&format!("{}{}:\n", pad, dotted_yaml_quote(k)));
                        out.push_str(&dotted_stringify_yaml(*val, depth + 1)?);
                        if !out.ends_with('\n') {
                            out.push('\n');
                        }
                    }
                }
            }
            Ok(out)
        }
        P_LIST => {
            let l = unsafe { &*(c.payload as *const HnList) };
            if l.items.is_empty() {
                return Ok(format!("{}[]", pad));
            }
            let mut out = String::new();
            for &item in &l.items {
                match dotted_yaml_scalar_str(item) {
                    Some(s) => out.push_str(&format!("{}- {}\n", pad, s)),
                    None => {
                        let sub = dotted_stringify_yaml(item, depth + 1)?;
                        let sub_lines: Vec<&str> = sub.lines().collect();
                        out.push_str(&format!("{}- {}", pad, sub_lines.first().unwrap_or(&"").trim()));
                        for l2 in &sub_lines[1..] {
                            out.push('\n');
                            out.push_str(&format!("{}  {}", pad, l2.trim_start()));
                        }
                        out.push('\n');
                    }
                }
            }
            Ok(out)
        }
        _ => {
            let s = dotted_yaml_scalar_str(h).ok_or(())?;
            Ok(format!("{}{}", pad, s))
        }
    }
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
/// ci/pc = 当前指令位置（错误时渲染行/列）。
#[no_mangle]
pub extern "C" fn rt_binop(op: u64, x: u64, y: u64, ci: u64, pc: u64) -> u64 {
    let (ci, pc) = (ci as u32, pc as u32);
    // G2.4：重载回退要读操作数类型——cell 释放后不可读，先取 tag 快照。
    let (tx, ty) = (cell(x).t, cell(y).t);
    // 先取 tag：str 路径需句柄（字节在堆上），不能走 Hnv 快照。
    let cx = cell(x);
    let cy = cell(y);
    let r = match op {
        // str+str 拼接（v_add 的 Str 臂）：合并字节建新句柄，输入照常释放。
        0 if cx.t == T_STR && cy.t == T_STR => str_concat(x, y),
        // str ==/!=（v_eq 的 Str 臂）：借用字节比较，结果 bool 建新句柄。
        7 | 8 if cx.t == T_STR && cy.t == T_STR => {
            let eq = str_eq(x, y);
            make_cell(Hnv::bool_(if op == 7 { eq } else { !eq }))
        }
        // list/dict/enum ==/!=（v_eq 容器臂）：递归结构比较，结果 bool 建新句柄。
        7 | 8 if (cx.t == T_LIST || cx.t == T_DICT || cx.t == T_ENUM) && cx.t == cy.t => {
            let eq = values_eq_h(x, y, ci, pc);
            make_cell(Hnv::bool_(if op == 7 { eq } else { !eq }))
        }
        _ => {
            let h = match op {
                0..=4 => arith_inner(op, cx.hnv(), cy.hnv(), ci, pc),
                7..=12 => cmpv_inner(op, cx.hnv(), cy.hnv(), ci, pc),
                _ => err_h("H001", "bad rt_binop op", ci, pc),
            };
            make_cell(h)
        }
    };
    // G2.4 运算符重载：内建失败且命中回退条件 → 实参引用转交 __op（不再释放 x/y）。
    if let Some(name) = binop_overload(op, tx, ty, handle_err_code(r)) {
        if let Some(v) = call_overload(name, &[x, y], ci, pc) {
            cell_release(r); // 丢弃内建产生的错误句柄
            return v;
        }
    }
    cell_release(x);
    cell_release(y);
    r
}

/// 一元运算分发（5=neg 6=not）。消费 x（drop），返回结果句柄。
/// ci/pc = 当前指令位置（错误时渲染行/列）。
#[no_mangle]
pub extern "C" fn rt_unop(op: u64, x: u64, ci: u64, pc: u64) -> u64 {
    let (ci, pc) = (ci as u32, pc as u32);
    let cx = cell(x);
    let h = match op {
        5 => neg_h(cx.hnv(), ci, pc),
        6 => not_h(cx.hnv(), ci, pc),
        _ => err_h("H001", "bad rt_unop op", ci, pc),
    };
    let r = make_cell(h);
    // G2.4 运算符重载：非 bool 的 -x / !x → __neg / __not（仅 H001 回退，镜像 VM exec 臂）。
    if handle_err_code(r) == Some("H001") {
        let name = match op {
            5 => "__neg",
            6 => "__not",
            _ => "",
        };
        if !name.is_empty() {
            if let Some(v) = call_overload(name, &[x], ci, pc) {
                cell_release(r);
                return v;
            }
        }
    }
    cell_release(x);
    r
}

/// 结果句柄的错误码（非错误句柄 → None）。
fn handle_err_code(h: u64) -> Option<&'static str> {
    let c = cell(h);
    if c.t != T_ERR {
        return None;
    }
    let e = unsafe { &*(c.payload as *const HnErr) };
    Some(err_code_str(e))
}

#[inline]
fn t_is_num(t: u64) -> bool {
    t == T_INT || t == T_FLOAT
}

/// 二元运算的重载回退判定（op 0..=4 / 7..=12），返回 `__op` 名；None = 不回退。
/// 条件逐条镜像 vm.rs exec 臂（interp eval_binary 同）：算术需「非 num/str 组合 + H001」
/// （除零 / 溢出等 native 错误不回退）；eq/ne 任何内建错误都回退；大小比较需「非 num 对」
/// （任何错误码都回退）。
fn binop_overload(op: u64, tx: u64, ty: u64, code: Option<&str>) -> Option<&'static str> {
    match op {
        0..=4 => {
            let native = (t_is_num(tx) && t_is_num(ty)) || (tx == T_STR && ty == T_STR);
            if !native && code == Some("H001") {
                Some(["__add", "__sub", "__mul", "__div", "__mod"][op as usize])
            } else {
                None
            }
        }
        7 => code.map(|_| "__eq"),
        8 => code.map(|_| "__ne"),
        9..=12 => {
            let native = (tx == T_INT && ty == T_INT) || (tx == T_FLOAT && ty == T_FLOAT);
            if native {
                None
            } else {
                Some(["__lt", "__le", "__gt", "__ge"][(op - 9) as usize])
            }
        }
        _ => None,
    }
}

/// 运算符重载调用（G2.4）：未定义 `name` → None（调用方保留原错误）。
/// **所有权**：`args` 里的引用全部转交被调帧（镜像 VM `overload(name, vec![x, y])` 的 clone
/// 语义——此处的入参本身已是 rt_binop/rt_unop 持有的独占引用）。
fn call_overload(name: &str, args: &[u64], ci: u32, pc: u32) -> Option<u64> {
    if func_lookup(name).is_none() {
        return None;
    }
    Some(call_global(name, args.as_ptr() as u64, args.len() as u64, ci, pc))
}

/// 借用型调用点的重载包装：先 retain 再转交（被调帧接管引用，调用方槽仍持原引用）。
fn call_overload_borrowed(name: &str, args: &[u64], ci: u32, pc: u32) -> Option<u64> {
    if func_lookup(name).is_none() {
        return None;
    }
    let owned: Vec<u64> = args.iter().map(|&a| rt_retain(a)).collect();
    Some(call_global(name, owned.as_ptr() as u64, owned.len() as u64, ci, pc))
}

/// 打印 display（借用 h，不 drop）。
#[no_mangle]
pub extern "C" fn rt_print(h: u64) {
    println!("{}", handle_display(h));
}

#[no_mangle]
pub extern "C" fn rt_truthy(h: u64) -> u64 {
    // 句柄版真值：完整镜像 vm.rs is_truthy（含 T_STR 空串为假，需句柄读长度）。
    let c = cell(h);
    match c.t {
        T_BOOL_TRUE => 1,
        T_BOOL_FALSE => 0,
        T_NULL => 0,
        T_INT => u64::from(c.a != 0),
        T_FLOAT => u64::from(f64::from_bits(c.a) != 0.0),
        T_STR => u64::from(c.b != 0),
        _ => 1,
    }
}

#[no_mangle]
pub extern "C" fn rt_is_err(h: u64) -> u64 {
    u64::from(cell(h).t == T_ERR)
}

// ── M1：list/dict 承载（G1.2/G1.3）与 RC+COW 写门（G1.1） ──
// 语义逐条镜像 vm.rs：v_index / v_index_set / v_destruct_get / v_len / v_keys /
// IterCheck 执行臂；所有权协议：
//   * rt_newlist/rt_newdict：从连续参数区读句柄（借用），retain 进新 cell，返回新句柄
//   * rt_index/rt_destructget/rt_len/rt_keys：借用，返回新句柄（元素 retain / null / err）
//   * rt_indexset：借用 obj/key/val，cow 后原地写，返回「新槽值句柄」（无条件替换协议：
//     调用方 store_slot(slot, r)——原地写时内部已 retain，替换后 rc 守恒）
//   * rt_append：借用，返回新 list 句柄（值语义，不修改原值）

/// dict 键 String 化（镜像 v_index 的 Dict 臂）：Str 原样、Int to_string、其他 H001。
fn dict_key(h: u64, ci: u32, pc: u32) -> Result<String, Hnv> {
    let c = cell(h);
    match c.t {
        T_STR => Ok(String::from_utf8_lossy(str_bytes(h)).into_owned()),
        T_INT => Ok((c.a as i64).to_string()),
        other => Err(err_h(
            "H001",
            &format!("字典键类型不支持: {}", type_name(other)),
            ci,
            pc,
        )),
    }
}

/// 建 list（NewList lowering）：ptr 指向 n 个连续句柄（借用），retain 建 cell。
#[no_mangle]
pub extern "C" fn rt_newlist(ptr: u64, n: u64, _ci: u64, _pc: u64) -> u64 {
    let items: Vec<u64> = (0..n as usize).map(|i| arg_at(ptr, i)).collect();
    make_cell_list(&items)
}

/// 建 dict（NewDict lowering）：ptr 指向 n 对连续 (key, val) 句柄；键转 String（镜像
/// NewDict 执行臂：Str 原样、其他 value_to_str——与 v_index 的「字典键类型不支持」不同，
/// 字面量构造允许任意键转串）。
#[no_mangle]
pub extern "C" fn rt_newdict(ptr: u64, n: u64, _ci: u64, _pc: u64) -> u64 {
    let mut pairs: Vec<(String, u64)> = Vec::with_capacity(n as usize);
    for i in 0..n as usize {
        let kh = arg_at(ptr, 2 * i);
        let vh = arg_at(ptr, 2 * i + 1);
        let k = {
            let c = cell(kh);
            if c.t == T_STR {
                String::from_utf8_lossy(str_bytes(kh)).into_owned()
            } else {
                handle_display(kh)
            }
        };
        pairs.push((k, vh));
    }
    make_cell_dict(&pairs)
}

/// 索引读取（Index lowering，镜像 v_index）：list 按 int（越界 H001）、str 按码点索引
/// （返回单字符 str）、dict 按键（缺键返回 null）；其他类型 H001。
/// 借用 obj/key，返回新句柄（元素 retain / null / err）。定义了 `__index` 时内建
/// H001 回退重载（G2.4）。
#[no_mangle]
pub extern "C" fn rt_index(obj: u64, key: u64, ci: u64, pc: u64) -> u64 {
    let (ci, pc) = (ci as u32, pc as u32);
    let r = index_inner(obj, key, ci, pc);
    // G2.4 运算符重载：定义了 __index 时，内建索引类型不支持（H001）→ 回退重载
    // （镜像 vm.rs Index 执行臂：先内建、H001 才回退；obj/key 为借用，先 retain 再转交）。
    if handle_err_code(r) == Some("H001") {
        if let Some(v) = call_overload_borrowed("__index", &[obj, key], ci, pc) {
            cell_release(r);
            return v;
        }
    }
    r
}

/// 内建索引（list / str / dict；借用 obj/key，返回新句柄）。
fn index_inner(obj: u64, key: u64, ci: u32, pc: u32) -> u64 {
    let c = cell(obj);
    match c.pkind {
        P_LIST => {
            let kc = cell(key);
            if kc.t != T_INT {
                return make_cell(err_h(
                    "H001",
                    &format!("list index must be an int, got `{}`", handle_display(key)),
                    ci,
                    pc,
                ));
            }
            let idx = kc.a as i64;
            let l = unsafe { &*(c.payload as *const HnList) };
            if idx < 0 || idx as usize >= l.items.len() {
                return make_cell(err_h(
                    "H001",
                    &format!("index {} out of bounds (list length {})", idx, l.items.len()),
                    ci,
                    pc,
                ));
            }
            rt_retain(l.items[idx as usize])
        }
        P_STR => {
            let kc = cell(key);
            if kc.t != T_INT {
                return make_cell(err_h(
                    "H001",
                    &format!("list index must be an int, got `{}`", handle_display(key)),
                    ci,
                    pc,
                ));
            }
            let idx = kc.a as i64;
            let s = String::from_utf8_lossy(str_bytes(obj)).into_owned();
            let chars: Vec<char> = s.chars().collect();
            if idx < 0 || idx as usize >= chars.len() {
                return make_cell(err_h(
                    "H001",
                    &format!("index {} out of bounds (string length {})", idx, chars.len()),
                    ci,
                    pc,
                ));
            }
            make_cell_str(chars[idx as usize].to_string().into_bytes())
        }
        P_DICT => match dict_key(key, ci, pc) {
            Ok(k) => {
                let d = unsafe { &*(c.payload as *const HnDict) };
                for (kk, vv) in &d.pairs {
                    if *kk == k {
                        return rt_retain(*vv);
                    }
                }
                make_cell(Hnv::null())
            }
            Err(e) => make_cell(e),
        },
        _ => make_cell(err_h(
            "H001",
            &format!("cannot index a value of type `{}`", handle_display(obj)),
            ci,
            pc,
        )),
    }
}

/// 索引赋值（IndexSet lowering，镜像 v_index_set）：cow 门 + list 槽替换 / dict 键写或
/// push。借用 obj/key/val，返回「新槽值句柄」（无条件替换协议，见文件头注释）。
#[no_mangle]
pub extern "C" fn rt_indexset(obj: u64, key: u64, val: u64, ci: u64, pc: u64) -> u64 {
    let (ci, pc) = (ci as u32, pc as u32);
    let c = cell(obj);
    match c.pkind {
        P_LIST => {
            let kc = cell(key);
            if kc.t != T_INT {
                return make_cell(err_h(
                    "H001",
                    &format!("list index must be an int, got `{}`", handle_display(key)),
                    ci,
                    pc,
                ));
            }
            let idx = kc.a as i64;
            let len = {
                let l = unsafe { &*(c.payload as *const HnList) };
                l.items.len()
            };
            if idx < 0 || idx as usize >= len {
                return make_cell(err_h(
                    "H001",
                    &format!("index {} out of bounds (list length {})", idx, len),
                    ci,
                    pc,
                ));
            }
            let mut owned = cow_handle(obj);
            if owned == obj {
                owned = rt_retain(owned); // 原地写：补引用，供调用方 store_slot 替换守恒
            }
            unsafe {
                let l = &mut *((owned as *mut HnCell).read().payload as *mut HnList);
                cell_release(l.items[idx as usize]);
                l.items[idx as usize] = rt_retain(val);
            }
            owned
        }
        P_DICT => match dict_key(key, ci, pc) {
            Ok(k) => {
                let mut owned = cow_handle(obj);
                if owned == obj {
                    owned = rt_retain(owned);
                }
                unsafe {
                    let d = &mut *((owned as *mut HnCell).read().payload as *mut HnDict);
                    match d.pairs.iter_mut().find(|(kk, _)| *kk == k) {
                        Some((_, vv)) => {
                            cell_release(*vv);
                            *vv = rt_retain(val);
                        }
                        None => d.pairs.push((k, rt_retain(val))),
                    }
                }
                owned
            }
            Err(e) => make_cell(e),
        },
        _ => make_cell(err_h(
            "H001",
            &format!("cannot index a value of type `{}`", handle_display(obj)),
            ci,
            pc,
        )),
    }
}

/// 解构取值（DestructGet lowering，镜像 interp DestructAssign 语义：形态区分 + 变量数文案）。
/// 借用 obj/key，返回新句柄（元素 retain / err）；nvars = 目标变量数，is_dict = `{a, b} = dict` 形态。
#[no_mangle]
pub extern "C" fn rt_destructget(obj: u64, key: u64, nvars: u64, is_dict: u64, ci: u64, pc: u64) -> u64 {
    let (ci, pc) = (ci as u32, pc as u32);
    let is_dict = is_dict != 0;
    let nvars = nvars as usize;
    let c = cell(obj);
    match c.pkind {
        P_LIST => {
            if is_dict {
                return make_cell(err_h_help(
                    "H001",
                    "dict destructuring `{...} =` requires a dict value, got a list",
                    ci,
                    pc,
                    Some("use `a, b = list` for list destructuring".to_string()),
                ));
            }
            let kc = cell(key);
            if kc.t != T_INT {
                return make_cell(err_h_help(
                    "H001",
                    &format!("list index must be an int, got `{}`", handle_display(key)),
                    ci,
                    pc,
                    Some("use an integer expression as the index, e.g. `a[0]`, `a[i]`".to_string()),
                ));
            }
            let idx = kc.a as i64;
            let l = unsafe { &*(c.payload as *const HnList) };
            if idx < 0 || idx as usize >= l.items.len() {
                return make_cell(err_h_help(
                    "H001",
                    &format!(
                        "destructuring a list of {} element(s) into {} variable(s)",
                        l.items.len(),
                        nvars
                    ),
                    ci,
                    pc,
                    Some("the list must have at least as many elements as the variables".to_string()),
                ));
            }
            rt_retain(l.items[idx as usize])
        }
        P_DICT => {
            if !is_dict {
                return make_cell(err_h_help(
                    "H001",
                    "list destructuring `a, b =` requires a list value, got a dict",
                    ci,
                    pc,
                    Some("use `{a, b} = dict` for dict destructuring".to_string()),
                ));
            }
            match dict_key(key, ci, pc) {
                Ok(k) => {
                    let d = unsafe { &*(c.payload as *const HnDict) };
                    for (kk, vv) in &d.pairs {
                        if *kk == k {
                            return rt_retain(*vv);
                        }
                    }
                    make_cell(err_h_help(
                        "H002",
                        &format!("dict has no key `{}` for destructuring", k),
                        ci,
                        pc,
                        Some("check the key name, or the dict value being destructured".to_string()),
                    ))
                }
                Err(e) => make_cell(e),
            }
        }
        _ => make_cell(err_h_help(
            "H001",
            &format!(
                "destructuring requires a list or dict value, got `{}`",
                // 用类型名而非值显示——镜像解释器 field_value/destructuring 分支的
                // other.type_name()（值显示会写成 `5`，与解释器的 `int` 不一致）。
                type_name(c.t)
            ),
            ci,
            pc,
            Some("destructure a list with `a, b = [..]` or a dict with `{a, b} = dict`".to_string()),
        )),
    }
}

// ── G2.2：enum 指令（NewEnum / EnumElem / IsEnumVariant 的 lowering 目标）──
// 语义逐条镜像 vm.rs 执行臂（2058–2085 行）；所有权协议同 M1 读指令：
// src 句柄借用（槽内保留），返回新句柄由调用方 store_slot。

/// NewEnum：由载荷句柄（param_ss 连续区，借用）建 enum 值——逐项 retain 入 box，
/// 实参数组引用（lowering 的 retain 产物，与 Call 同约定）在此统一释放。
/// 无错误路径（镜像 VM：直接 clone 载荷），ci/pc 省略。
#[no_mangle]
pub extern "C" fn rt_newenum(n_sidx: u64, v_sidx: u64, argptr: u64, nargs: u64) -> u64 {
    let mut payload = Vec::with_capacity(nargs as usize);
    for i in 0..nargs as usize {
        payload.push(rt_retain(arg_at(argptr, i)));
    }
    drop_args(argptr, nargs);
    make_cell_enum(n_sidx as u32, v_sidx as u32, payload)
}

/// EnumElem：借 enum 值，返回第 idx 项载荷的新句柄。
/// 非枚举值 / 越界 → H001（镜像 VM mk_err 文案；checker 已静态校验，防御性保留）。
#[no_mangle]
pub extern "C" fn rt_enumelem(src: u64, idx: u64, ci: u64, pc: u64) -> u64 {
    let (ci, pc) = (ci as u32, pc as u32);
    let c = cell(src);
    if c.t != T_ENUM {
        return make_cell(err_h("H001", "EnumElem 作用于非枚举值", ci, pc));
    }
    let b = unsafe { &*(c.payload as *const HnEnum) };
    match b.payload.get(idx as usize) {
        Some(h) => rt_retain(*h),
        None => make_cell(err_h("H001", "枚举载荷索引越界", ci, pc)),
    }
}

/// IsEnumVariant：借值，按 strtab 索引比较枚举名 + 变体（索引相等 ⇔ 名字相等，
/// 主程序侧收集时去重）。无错误路径（镜像 VM：恒返回 bool）。
#[no_mangle]
pub extern "C" fn rt_isenumvariant(src: u64, n_sidx: u64, v_sidx: u64) -> u64 {
    let c = cell(src);
    make_cell(Hnv::bool_(c.t == T_ENUM && c.a == n_sidx && c.b == v_sidx))
}

/// 长度（Len 指令 lowering，镜像 v_len）：list 元素数 / dict 非隐藏键数 / str 字节长。
#[no_mangle]
pub extern "C" fn rt_len(v: u64, ci: u64, pc: u64) -> u64 {
    let (ci, pc) = (ci as u32, pc as u32);
    let c = cell(v);
    match c.pkind {
        P_STR => make_cell(Hnv::int(c.b as i64)),
        P_LIST => {
            let l = unsafe { &*(c.payload as *const HnList) };
            make_cell(Hnv::int(l.items.len() as i64))
        }
        P_DICT => {
            let d = unsafe { &*(c.payload as *const HnDict) };
            let n = d.pairs.iter().filter(|(k, _)| !is_hidden_key(k)).count();
            make_cell(Hnv::int(n as i64))
        }
        _ => make_cell(err_h_help(
            "H001",
            &format!(
                "`len` expects a string, list, dict, or bytes, got `{}`",
                type_name(c.t)
            ),
            ci,
            pc,
            Some("`len` returns the byte length of a string/bytes, or the element count of a list/dict".to_string()),
        )),
    }
}

/// 键列表（Keys lowering，镜像 v_keys）：dict → list[Str]（隐藏键过滤）；其他 H001。
#[no_mangle]
pub extern "C" fn rt_keys(v: u64, ci: u64, pc: u64) -> u64 {
    let (ci, pc) = (ci as u32, pc as u32);
    let c = cell(v);
    if c.pkind != P_DICT {
        return make_cell(err_h(
            "H001",
            &format!("`keys` expects a dict, got `{}`", handle_display(v)),
            ci,
            pc,
        ));
    }
    let d = unsafe { &*(c.payload as *const HnDict) };
    let keys: Vec<u64> = d
        .pairs
        .iter()
        .filter(|(k, _)| !is_hidden_key(k))
        .map(|(k, _)| make_cell_str(k.clone().into_bytes()))
        .collect();
    // make_cell_list 内部 retain 每个元素；keys 的原始引用须显式释放（否则 rc+1 泄漏）。
    let cell = make_cell_list(&keys);
    for &k in &keys {
        cell_release(k);
    }
    cell
}

#[no_mangle]
pub extern "C" fn rt_islist(h: u64) -> u64 {
    u64::from(cell(h).t == T_LIST)
}

#[no_mangle]
pub extern "C" fn rt_isdict(h: u64) -> u64 {
    u64::from(cell(h).t == T_DICT)
}

#[no_mangle]
pub extern "C" fn rt_isnull(h: u64) -> u64 {
    u64::from(cell(h).t == T_NULL)
}

/// 迭代源检查（IterCheck lowering，镜像 VM 执行臂）：非 list/dict → H001。
/// is_comp=1 为推导式（"comprehension requires..."），0 为 `for in`。
#[no_mangle]
pub extern "C" fn rt_itercheck(src: u64, is_comp: u64, ci: u64, pc: u64) -> u64 {
    let (ci, pc) = (ci as u32, pc as u32);
    let c = cell(src);
    if c.t != T_LIST && c.t != T_DICT {
        let what = if is_comp != 0 { "comprehension" } else { "`for in`" };
        return make_cell(err_h(
            "H001",
            &format!("{} requires a list or dict, got `{}`", what, type_name(c.t)),
            ci,
            pc,
        ));
    }
    make_cell(Hnv::null())
}

/// 条件严格 bool 校验（G2.3 引擎对齐，镜像 VM `Instr::CondCheck` 执行臂）：
/// 非 bool → H001，消息按站点；通过 → null 句柄（调用方 drop）。
/// 解释器七处条件站点均传 help=None，故用 explicit_no_help 阻渲染层回退补行。
#[no_mangle]
pub extern "C" fn rt_condcheck(v: u64, kind: u64, ci: u64, pc: u64) -> u64 {
    let (ci, pc) = (ci as u32, pc as u32);
    let c = cell(v);
    if c.t == T_BOOL_TRUE || c.t == T_BOOL_FALSE {
        return make_cell(Hnv::null());
    }
    let tn = type_name(c.t);
    let msg = match kind {
        0 => format!("if condition must be `bool`, got `{tn}`"),
        1 => format!("while condition must be `bool`, got `{tn}`"),
        2 => format!("do-while condition must be `bool`, got `{tn}`"),
        3 => format!("for condition must be `bool`, got `{tn}`"),
        4 => format!("ternary condition must be `bool`, got `{tn}`"),
        5 => format!("logical operators require `bool` operands, got `{tn}`"),
        _ => format!("comprehension `if` condition must be `bool`, got `{tn}`"),
    };
    make_cell(err_h_explicit_no_help("H001", &msg, ci, pc))
}

/// 值是否恰为 Bool(want)（无类型错误，恒产出 bool 句柄）——&&/|| 短路判定，
/// 镜像 VM `Instr::IsBoolVal` 执行臂 / 解释器 `if let Value::Bool(false) = l`。
#[no_mangle]
pub extern "C" fn rt_isboolval(v: u64, want: u64) -> u64 {
    let c = cell(v);
    let is = (c.t == T_BOOL_TRUE && want != 0) || (c.t == T_BOOL_FALSE && want == 0);
    rt_bool(u64::from(is))
}

/// append 内置（镜像 builtins.rs append）：值语义返回新 list（元素 retain + 新值 retain），
/// 不修改原值；非 list → H001 + help。
#[no_mangle]
pub extern "C" fn rt_append(list: u64, val: u64, ci: u64, pc: u64) -> u64 {
    let (ci, pc) = (ci as u32, pc as u32);
    let c = cell(list);
    if c.pkind != P_LIST {
        return make_cell(err_h_help(
            "H001",
            &format!("`append` expects a list, got `{}`", type_name(c.t)),
            ci,
            pc,
            Some("use `l = append(l, x)` to add `x` to the tail of list `l`".to_string()),
        ));
    }
    let l = unsafe { &*(c.payload as *const HnList) };
    let mut items: Vec<u64> = l.items.clone(); // 借用句柄值拷贝（make_cell_list 内部 retain）
    items.push(val);
    make_cell_list(&items)
}

/// 产物入口：ERR → rt_unwind；否则 drop 后退出码 0（与 hone run 一致）。
/// main chunk 为统一签名（argptr, nargs, capsptr, ncap），顶层无参无捕获 → 全 0。
#[no_mangle]
pub extern "C" fn hn_entry() -> i32 {
    let r = unsafe { __hn_main(0, 0, 0, 0) };
    if cell(r).t == T_ERR {
        rt_unwind(r);
    }
    cell_release(r);
    // debug 构建出口泄漏断言（T2 退出标准）：运行期新建 cell 的 rc 应全部归零
    // （strtab 基引用不计入 HEAP_LIVE，常量视为 rodata 常驻）。
    #[cfg(debug_assertions)]
    {
        let n = HEAP_LIVE.load(std::sync::atomic::Ordering::Relaxed);
        debug_assert!(n == 0, "natgen leak: {} 个运行期 cell 在出口仍存活", n);
    }
    0
}

// 测试构建下 object 不存在：不声明 extern，改由下方测试桩定义同名符号。
#[cfg(not(test))]
extern "C" {
    fn __hn_main(argptr: u64, nargs: u64, capsptr: u64, ncap: u64) -> u64;
}

#[cfg(test)]
#[no_mangle]
pub extern "C" fn __hn_main(_argptr: u64, _nargs: u64, _capsptr: u64, _ncap: u64) -> u64 {
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
    fn truthy_full_semantics() {
        let _g = locked();
        let base = heap_live();
        // 对齐 vm.rs is_truthy：int 0 / float 0.0 / 空串 / false / null 为假，余真。
        let cases: Vec<(u64, u64)> = vec![
            (rt_i64(0), 0),
            (rt_i64(5), 1),
            (rt_i64((-1i64) as u64), 1),
            (rt_f64(0.0f64.to_bits()), 0),
            (rt_f64(0.5f64.to_bits()), 1),
            (rt_bool(0), 0),
            (rt_bool(1), 1),
            (rt_null(), 0),
            (rt_char('a' as u32 as u64), 1),
        ];
        for (h, want) in cases {
            assert_eq!(rt_truthy(h), want);
            rt_drop(h);
        }
        // 空串 / 非空串（走 strtab 建箱）
        hn_rt_strtab_init(
            ["".as_ptr(), "x".as_ptr()].as_ptr(),
            [0usize, 1usize].as_ptr(),
            2,
        );
        let empty = rt_str(0);
        let nonempty = rt_str(1);
        assert_eq!(rt_truthy(empty), 0);
        assert_eq!(rt_truthy(nonempty), 1);
        rt_drop(empty);
        rt_drop(nonempty);
        assert_eq!(heap_live(), base);
    }

    #[test]
    fn entry_returns_zero() {
        // 持有 TEST_LOCK：hn_entry 内的 debug 出口泄漏断言要求 HEAP_LIVE==0，
        // 必须与其他建/销 cell 的测试串行，避免并行污染断言。
        let _g = locked();
        assert_eq!(hn_entry(), 0);
    }

    #[test]
    fn binop_int_add_overflow() {
        let _g = locked();
        let base = heap_live();
        let a = rt_i64(i64::MAX as u64);
        let b = rt_i64(1);
        let r = rt_binop(0, a, b, 0, 0);
        assert_eq!(cell(r).t, T_ERR);
        // plain 渲染（PROG 未初始化 → 无定位）：error 行 + help 行，均以 \n 结尾。
        assert_eq!(
            err_render(r),
            "error[H010]: integer overflow\nhelp: the result does not fit in a 64-bit signed integer\n"
        );
        cell_release(r);
        assert_eq!(heap_live(), base);
    }

    #[test]
    fn binop_div_zero() {
        let _g = locked();
        let base = heap_live();
        let a = rt_i64(1);
        let b = rt_i64(0);
        let r = rt_binop(3, a, b, 0, 0);
        assert_eq!(
            err_render(r),
            "error[H009]: division by zero\nhelp: check the divisor before dividing\n"
        );
        cell_release(r);
        assert_eq!(heap_live(), base);
    }

    #[test]
    fn binop_type_mismatch() {
        let _g = locked();
        let base = heap_live();
        let a = rt_bool(1);
        let b = rt_i64(1);
        let r = rt_binop(0, a, b, 0, 0);
        assert_eq!(
            err_render(r),
            "error[H001]: cannot apply `+` to `bool` and `int`\nhelp: Hone has no implicit type conversion\n"
        );
        cell_release(r);
        assert_eq!(heap_live(), base);
    }

    #[test]
    fn binop_basic() {
        let _g = locked();
        let base = heap_live();
        let r = rt_binop(0, rt_i64(2), rt_i64(3), 0, 0);
        assert_eq!(cell(r).t, T_INT);
        assert_eq!(cell(r).a as i64, 5);
        cell_release(r);
        let r = rt_binop(2, rt_i64(4), rt_i64(5), 0, 0);
        assert_eq!(cell(r).a as i64, 20);
        cell_release(r);
        let r = rt_unop(5, rt_i64(7), 0, 0);
        assert_eq!(cell(r).a as i64, -7);
        cell_release(r);
        let r = rt_unop(6, rt_bool(1), 0, 0);
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
    fn str_concat_and_eq() {
        // T3：str+str 拼接（op 0）与 str ==/!=（op 7/8）句柄路径。
        // STRTAB 是进程全局：测试自行初始化，不依赖其他测试的表内容。
        let _g = locked();
        hn_rt_strtab_init(
            ["hello".as_ptr(), "世界".as_ptr()].as_ptr(),
            [5usize, 6usize].as_ptr(),
            2,
        );
        let base = heap_live();
        let r = rt_binop(0, rt_str(0), rt_str(1), 0, 0);
        assert_eq!(handle_display(r), "hello世界");
        cell_release(r);
        // 等值 → true；异值 → false；!= 取反（结果句柄逐个释放）。
        let r = rt_binop(7, rt_str(0), rt_str(0), 0, 0);
        assert_eq!(cell(r).t, T_BOOL_TRUE);
        cell_release(r);
        let r = rt_binop(7, rt_str(0), rt_str(1), 0, 0);
        assert_eq!(cell(r).t, T_BOOL_FALSE);
        cell_release(r);
        let r = rt_binop(8, rt_str(0), rt_str(1), 0, 0);
        assert_eq!(cell(r).t, T_BOOL_TRUE);
        cell_release(r);
        // 空串拼接："" + "x" = "x"（重新初始化表）
        hn_rt_strtab_init(
            ["".as_ptr(), "x".as_ptr()].as_ptr(),
            [0usize, 1usize].as_ptr(),
            2,
        );
        let r = rt_binop(0, rt_str(0), rt_str(1), 0, 0);
        assert_eq!(handle_display(r), "x");
        cell_release(r);
        // str < str → H001（v_cmp 无 str 臂）
        let r = rt_binop(9, rt_str(0), rt_str(1), 0, 0);
        assert_eq!(cell(r).t, T_ERR);
        cell_release(r);
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

    #[test]
    fn error_render_with_span_caret() {
        // T4：hn_rt_prog_init 装载 file/src/span 表后，按 (ci,pc) 查 span 渲染，
        // 与 error.rs ZError::Display 的定位分支逐字节一致（--> 行/行号行/caret 行/help 行）。
        let _g = locked();
        let src = "x = 1 / 0;\n";
        let sp0: [u32; 3] = [1, 7, 1]; // line=1, col=7（`/` 位置）, len=1
        let ptrs: [*const u32; 1] = [sp0.as_ptr()];
        let lens: [usize; 1] = [3];
        hn_rt_prog_init(
            b"probe.hn".as_ptr(),
            b"probe.hn".len(),
            src.as_ptr(),
            src.len(),
            ptrs.as_ptr(),
            lens.as_ptr(),
            1,
        );
        let base = heap_live();
        // ci=0, pc=0 → span [1,7,1]；除零错误渲染带定位。
        let r = rt_binop(3, rt_i64(1), rt_i64(0), 0, 0);
        assert_eq!(
            err_render(r),
            "error[H009]: division by zero\n  --> probe.hn:1:7\n1 | x = 1 / 0;\n  |       ^\nhelp: check the divisor before dividing\n"
        );
        cell_release(r);
        assert_eq!(heap_live(), base);
    }

    #[test]
    fn error_render_tab_caret_align() {
        // T4：源码行含 Tab（缩进）时 caret 按 Tab→4 空格展开对齐（镜像 ZError::Display）。
        let _g = locked();
        let src = "fn f() {\n\tx = 1 / 0;\n}\n";
        let sp0: [u32; 3] = [2, 8, 1]; // 第 2 行，`/` 在第 8 列
        let ptrs: [*const u32; 1] = [sp0.as_ptr()];
        let lens: [usize; 1] = [3];
        hn_rt_prog_init(
            b"tab.hn".as_ptr(),
            b"tab.hn".len(),
            src.as_ptr(),
            src.len(),
            ptrs.as_ptr(),
            lens.as_ptr(),
            1,
        );
        let base = heap_live();
        let r = rt_binop(3, rt_i64(1), rt_i64(0), 0, 0);
        let s = err_render(r);
        // Tab 展开：`\tx = 1 / 0;` → 显示 `    x = 1 / 0;`，caret 前导 10 空格（4 + 6）。
        assert_eq!(
            s,
            "error[H009]: division by zero\n  --> tab.hn:2:8\n2 |     x = 1 / 0;\n  |           ^\nhelp: check the divisor before dividing\n"
        );
        cell_release(r);
        assert_eq!(heap_live(), base);
    }

    // ── M1：list/dict 承载 + RC+COW 写门（G1.1/G1.2/G1.3） ──
    // 所有权契约：rt_newlist/rt_newdict/rt_index/rt_indexset/rt_len/rt_keys/rt_append/
    // rt_destructget 均「借用」输入（调用方保留引用、负责释放）；rt_binop/rt_unop
    // 「消费」输入（调用方不得再释放）。测试严格按此管理句柄。

    #[test]
    fn list_basic_and_cow_alias() {
        // G1.2 + G1.1：字面量建 list、索引读写、b=a 别名值语义（写 a 不污染 b）。
        let _g = locked();
        let base = heap_live();
        let (h1, h2, h3) = (rt_i64(1), rt_i64(2), rt_i64(3));
        let items = [h1, h2, h3];
        let a = rt_newlist(items.as_ptr() as u64, 3, 0, 0);
        assert_eq!(handle_display(a), "[1, 2, 3]");
        assert_eq!(cell(a).t, T_LIST);
        // b = a（Move retain）：rc=2。
        let b = rt_retain(a);
        assert_eq!(rc_load(a), 2);
        // a[0] = 100：cow 拷贝（rc=2>1），槽替换后 a 独占、b 原值不变。
        let (h0, h100) = (rt_i64(0), rt_i64(100));
        let new_a = rt_indexset(a, h0, h100, 0, 0);
        assert_ne!(new_a, a, "共享写必须 COW 拷贝出新句柄");
        assert_eq!(handle_display(new_a), "[100, 2, 3]");
        assert_eq!(handle_display(b), "[1, 2, 3]");
        // cow 拷贝不修改原 cell rc：a 槽 + b 槽 = 2（拷贝时仅值 retain 进新 cell）。
        assert_eq!(rc_load(a), 2, "原句柄仍由 a/b 两槽持有");
        cell_release(new_a);
        cell_release(b);
        cell_release(a);
        cell_release(h1);
        cell_release(h2);
        cell_release(h3);
        cell_release(h0);
        cell_release(h100);
        assert_eq!(heap_live(), base);
    }

    #[test]
    fn list_cow_inplace_when_exclusive() {
        // G1.1：独占（rc==1）写路径原地改，不拷贝（cow 门放行）。
        let _g = locked();
        let base = heap_live();
        let (h1, h2) = (rt_i64(1), rt_i64(2));
        let a = rt_newlist([h1, h2].as_ptr() as u64, 2, 0, 0);
        assert_eq!(rc_load(a), 1);
        let (h0, h9) = (rt_i64(0), rt_i64(9));
        let r = rt_indexset(a, h0, h9, 0, 0);
        assert_eq!(r, a, "独占写返回原句柄");
        assert_eq!(handle_display(a), "[9, 2]");
        // 原地写路径 rt_indexset 内部 retain 补引用（无条件替换协议），须按「槽替换」释放两次。
        cell_release(r);
        cell_release(a);
        cell_release(h1);
        cell_release(h2);
        cell_release(h0);
        cell_release(h9);
        assert_eq!(heap_live(), base);
    }

    #[test]
    fn list_index_errors() {
        // G1.2：越界 / 非 int 键 / 类型错误文案对齐。
        let _g = locked();
        let base = heap_live();
        let (h1, h2) = (rt_i64(1), rt_i64(2));
        let a = rt_newlist([h1, h2].as_ptr() as u64, 2, 0, 0);
        let h5 = rt_i64(5);
        let e1 = rt_index(a, h5, 0, 0);
        assert_eq!(cell(e1).t, T_ERR);
        assert!(err_render(e1).contains("index 5 out of bounds (list length 2)"));
        cell_release(e1);
        cell_release(h5);
        // 非 int 键（脚本不可达，checker 静态拒绝；防御性对齐）。
        let ks = make_cell_str(b"k".to_vec());
        let e2 = rt_index(a, ks, 0, 0);
        assert!(err_render(e2).contains("list index must be an int, got `k`"));
        cell_release(e2);
        cell_release(ks);
        // 非容器索引。
        let (h1b, h0b) = (rt_i64(1), rt_i64(0));
        let e3 = rt_index(h1b, h0b, 0, 0);
        assert!(err_render(e3).contains("cannot index a value of type `1`"));
        cell_release(e3);
        cell_release(h1b);
        cell_release(h0b);
        cell_release(a);
        cell_release(h1);
        cell_release(h2);
        assert_eq!(heap_live(), base);
    }

    #[test]
    fn dict_basic_and_keys() {
        // G1.3：NewDict 字面量、len、keys、==（按序结构比较）、display。
        let _g = locked();
        let base = heap_live();
        let (kx, ky) = (make_cell_str(b"x".to_vec()), make_cell_str(b"y".to_vec()));
        let (v10, v20) = (rt_i64(10), rt_i64(20));
        let d = rt_newdict([kx, v10, ky, v20].as_ptr() as u64, 2, 0, 0);
        assert_eq!(handle_display(d), "{x: 10, y: 20}");
        assert_eq!(cell(d).t, T_DICT);
        let len = rt_len(d, 0, 0);
        assert_eq!(cell(len).t, T_INT);
        cell_release(len);
        // keys → ["x", "y"]
        let ks = rt_keys(d, 0, 0);
        assert_eq!(handle_display(ks), "[x, y]");
        cell_release(ks);
        // dict 相等（插入序一致 → 相等）；rt_binop 消费 d/d2。
        let (v10b, v20b) = (rt_i64(10), rt_i64(20));
        let d2 = rt_newdict([kx, v10b, ky, v20b].as_ptr() as u64, 2, 0, 0);
        let eq = rt_binop(7, d, d2, 0, 0);
        assert_eq!(handle_display(eq), "true");
        cell_release(eq);
        // 键句柄 dict 不 retain（键 String 化），值句柄 dict retain——全部显式释放。
        cell_release(kx);
        cell_release(ky);
        cell_release(v10);
        cell_release(v20);
        cell_release(v10b);
        cell_release(v20b);
        assert_eq!(heap_live(), base);
    }

    #[test]
    fn dict_cow_write() {
        // G1.3 + G1.1：dict 写（键写/新增）先过 cow 门；共享写不污染别名。
        let _g = locked();
        let base = heap_live();
        let (kx, ky, kz) = (
            make_cell_str(b"x".to_vec()),
            make_cell_str(b"y".to_vec()),
            make_cell_str(b"z".to_vec()),
        );
        let (v1, v2, v3, v9) = (rt_i64(1), rt_i64(2), rt_i64(3), rt_i64(9));
        let d = rt_newdict([kx, v1, ky, v2].as_ptr() as u64, 2, 0, 0);
        let alias = rt_retain(d);
        let new_d = rt_indexset(d, kz, v3, 0, 0);
        assert_ne!(new_d, d, "共享写必须 COW 拷贝");
        assert_eq!(handle_display(new_d), "{x: 1, y: 2, z: 3}");
        assert_eq!(handle_display(alias), "{x: 1, y: 2}");
        // 独占写原地（r2==new_d，内部 retain 补引用——按槽替换释放两次：r2 + new_d）。
        let r2 = rt_indexset(new_d, kz, v9, 0, 0);
        assert_eq!(r2, new_d);
        assert_eq!(handle_display(new_d), "{x: 1, y: 2, z: 9}");
        cell_release(r2);
        cell_release(new_d);
        cell_release(alias);
        cell_release(d);
        cell_release(kx);
        cell_release(ky);
        cell_release(kz);
        cell_release(v1);
        cell_release(v2);
        cell_release(v3);
        cell_release(v9);
        assert_eq!(heap_live(), base);
    }

    #[test]
    fn append_returns_new_list() {
        // G1.2：append 值语义（返回新 list，原值不变）。
        let _g = locked();
        let base = heap_live();
        let (h1, h2, h3) = (rt_i64(1), rt_i64(2), rt_i64(3));
        let a = rt_newlist([h1, h2].as_ptr() as u64, 2, 0, 0);
        let r = rt_append(a, h3, 0, 0);
        assert_eq!(handle_display(r), "[1, 2, 3]");
        assert_eq!(handle_display(a), "[1, 2]");
        // 非 list → H001 + help。
        let (n1, n2) = (rt_i64(1), rt_i64(2));
        let e = rt_append(n1, n2, 0, 0);
        assert!(err_render(e).contains("`append` expects a list, got `int`"));
        cell_release(e);
        cell_release(n1);
        cell_release(n2);
        cell_release(r);
        cell_release(a);
        cell_release(h1);
        cell_release(h2);
        cell_release(h3);
        assert_eq!(heap_live(), base);
    }

    #[test]
    fn destructget_semantics() {
        // G1.2：解构取值（形态区分 + 变量数 + 缺键文案）。
        let _g = locked();
        let base = heap_live();
        let (h7, h8) = (rt_i64(7), rt_i64(8));
        let l = rt_newlist([h7, h8].as_ptr() as u64, 2, 0, 0);
        let h1 = rt_i64(1);
        let v = rt_destructget(l, h1, 2, 0, 0, 0);
        assert_eq!(handle_display(v), "8");
        cell_release(v);
        cell_release(h1);
        // 越界：`x, y = [1]` → 报变量数。
        let o1 = rt_i64(1);
        let l1 = rt_newlist([o1].as_ptr() as u64, 1, 0, 0);
        let h1b = rt_i64(1);
        let e = rt_destructget(l1, h1b, 2, 0, 0, 0);
        assert!(err_render(e).contains("destructuring a list of 1 element(s) into 2 variable(s)"));
        cell_release(e);
        cell_release(h1b);
        cell_release(l1);
        cell_release(o1);
        // dict 形态收到 list。
        let h0 = rt_i64(0);
        let e2 = rt_destructget(l, h0, 2, 1, 0, 0);
        assert!(err_render(e2).contains("dict destructuring `{...} =` requires a dict value, got a list"));
        cell_release(e2);
        cell_release(h0);
        cell_release(l);
        cell_release(h7);
        cell_release(h8);
        assert_eq!(heap_live(), base);
    }

    // ── G2.3 引擎对齐：条件严格 bool 校验 + &&/|| 短路判定 ──

    #[test]
    fn condcheck_and_isboolval() {
        let _g = locked();
        let src = "if (c) { }\n";
        let sp0: [u32; 3] = [1, 5, 1]; // 第 1 行，第 5 列（`c`）
        let ptrs: [*const u32; 1] = [sp0.as_ptr()];
        let lens: [usize; 1] = [3];
        hn_rt_prog_init(
            b"c.hn".as_ptr(),
            4,
            src.as_ptr(),
            src.len(),
            ptrs.as_ptr(),
            lens.as_ptr(),
            1,
        );
        let base = heap_live();
        // bool 值通过 → null 句柄（无错误）。
        let tb = rt_bool(1);
        let ok = rt_condcheck(tb, 0, 0, 0);
        assert_eq!(cell(ok).t, T_NULL);
        cell_release(ok);
        // 非 bool（int）→ H001，消息逐字镜像解释器 if 站点，且**无 help 行**
        // （err_h_explicit_no_help 哨兵阻渲染层回退补行）。
        let h1 = rt_i64(1);
        let e = rt_condcheck(h1, 0, 0, 0);
        assert_eq!(
            err_render(e),
            "error[H001]: if condition must be `bool`, got `int`\n  --> c.hn:1:5\n1 | if (c) { }\n  |     ^\n"
        );
        cell_release(e);
        // kind=6：推导式 if 过滤站点文案。
        let e6 = rt_condcheck(h1, 6, 0, 0);
        assert!(err_render(e6).contains("comprehension `if` condition must be `bool`, got `int`"));
        cell_release(e6);
        // IsBoolVal：bool 精确匹配 want；非 bool 一律 false（无类型错误）。
        let t = rt_isboolval(tb, 1);
        assert_eq!(handle_display(t), "true");
        cell_release(t);
        let f = rt_isboolval(tb, 0);
        assert_eq!(handle_display(f), "false");
        cell_release(f);
        let g = rt_isboolval(h1, 1);
        assert_eq!(handle_display(g), "false");
        cell_release(g);
        let h0 = rt_i64(0);
        let g2 = rt_isboolval(h0, 0);
        assert_eq!(handle_display(g2), "false");
        cell_release(g2);
        cell_release(tb);
        cell_release(h1);
        cell_release(h0);
        assert_eq!(heap_live(), base);
    }
}
