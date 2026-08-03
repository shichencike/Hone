// zap_lib - 用于测试 `load` 动态库加载的 Rust cdylib
// 导出 C ABI 函数（int64 参数/返回值，与 Zap 的 load 约定一致）。
// 构建：cd tests/zap_lib && cargo build --release

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
