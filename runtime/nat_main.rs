// G3.4 工具链打包入口：runtime + 固定 wrapper main 一起编译为 staticlib
// （`rustc --crate-type=staticlib --cfg hone_boot nat_main.rs`），随 hone 分发
// （设计 §5.1：`script.o + libhoneruntime.a + CRT → exe`，目标机无 rustc）。
// 静态表自举（hn_boot）在 hn_entry 首行执行，与 rustc-wrapper 路径完全同源。
// 运行时（同 wrapper 路径的 include! 合并，单文件无外部依赖）。
include!("src/lib.rs");

// CRT 入口：MinGW crt2.o 调 `main`（C 约定，argc/argv 进来、返回 int）。
// hn_entry 内部：hn_boot 自举静态表 → __hn_main → ERR 渲染 exit(1) / 正常 0。
#[no_mangle]
pub extern "C" fn main(_argc: i32, _argv: *const *const u8) -> i32 {
    hn_entry()
}
