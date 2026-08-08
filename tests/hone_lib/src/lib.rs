// hone_lib - 用于测试 `load` 动态库加载的 Rust cdylib
// 导出 C ABI 函数（int64 参数/返回值，与 Hone 的 load 约定一致）。
// 构建：cd tests/hone_lib && cargo build --release

use std::ffi::{c_char, c_void, CStr};
use std::sync::atomic::{AtomicI64, Ordering};

// ---- 旧约定（全 int64）测试函数 ----

#[no_mangle]
pub extern "C" fn lib_add(a: i64, b: i64) -> i64 {
    a + b
}

#[no_mangle]
pub extern "C" fn lib_mul(a: i64, b: i64) -> i64 {
    a * b
}

#[no_mangle]
pub extern "C" fn lib_fact(n: i64) -> i64 {
    if n <= 1 {
        1
    } else {
        n * lib_fact(n - 1)
    }
}

#[no_mangle]
pub extern "C" fn lib_echo(x: i64) -> i64 {
    x
}

// ---- typed FFI 测试函数（配合 load 签名块）----

/// float + float → float
#[no_mangle]
pub extern "C" fn lib_add_f(a: f64, b: f64) -> f64 {
    a + b
}

/// float + int → float（混合寄存器类别）
#[no_mangle]
pub extern "C" fn lib_mix_f(f: f64, n: i64) -> f64 {
    f * (n as f64)
}

/// str → int（const char* 参数）
#[no_mangle]
pub extern "C" fn lib_strlen(s: *const c_char) -> i64 {
    if s.is_null() {
        return 0;
    }
    unsafe { CStr::from_ptr(s) }.to_bytes().len() as i64
}

/// str → str（返回静态 C 字符串）
#[no_mangle]
pub extern "C" fn lib_hello() -> *const c_char {
    b"hello from hone\0".as_ptr() as *const c_char
}

/// str + int → int（str 与 int 混合参数）
#[no_mangle]
pub extern "C" fn lib_count_char(s: *const c_char, c: i64) -> i64 {
    if s.is_null() {
        return 0;
    }
    let bytes = unsafe { CStr::from_ptr(s) }.to_bytes();
    let c = c as u8;
    bytes.iter().filter(|&&b| b == c).count() as i64
}

/// bool → bool（C ABI 用 0/1 整数表示）
#[no_mangle]
pub extern "C" fn lib_not(b: i64) -> i64 {
    if b == 0 {
        1
    } else {
        0
    }
}

/// ptr → ptr（原样返回，测试句柄传递）
#[no_mangle]
pub extern "C" fn lib_echo_ptr(p: *const c_void) -> *const c_void {
    p
}

/// 4 个 int 参数（多参数组合）
#[no_mangle]
pub extern "C" fn lib_sum4(a: i64, b: i64, c: i64, d: i64) -> i64 {
    a + b + c + d
}

/// void 返回 + 全局状态（测试副作用与 void）
static COUNTER: AtomicI64 = AtomicI64::new(0);

#[no_mangle]
pub extern "C" fn lib_bump() {
    COUNTER.fetch_add(1, Ordering::SeqCst);
}

#[no_mangle]
pub extern "C" fn lib_count() -> i64 {
    COUNTER.load(Ordering::SeqCst)
}
