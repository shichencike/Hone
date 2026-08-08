// ptrmod.rs - 指针类（ptr.* 内置函数）
// 纯 Rust 内存管理，Windows / Linux / Termux 跨平台一致，无 C 依赖。
//   ptr.alloc(size)            -> ptr    分配 size 字节内存，返回地址（失败返回 0）
//   ptr.free(p)                -> bool   释放由 ptr.alloc 分配的内存（防 double-free / 误释放）
//   ptr.is_null(p)             -> bool   判断指针是否为 0（NULL）
//   ptr.is_valid(p)            -> bool   是否为本模块分配且未释放的指针
//   ptr.size(p)                -> int    返回 ptr.alloc 分配的字节数
//   ptr.read_int(p, off)       -> int    读取 8 字节有符号整数
//   ptr.read_float(p, off)     -> float  读取 8 字节 IEEE double
//   ptr.read_byte(p, off)      -> int    读取 1 字节（0-255）
//   ptr.write_int(p, off, v)   -> void   写入 8 字节有符号整数
//   ptr.write_float(p, off, v) -> void   写入 8 字节 IEEE double
//   ptr.write_byte(p, off, v)  -> void   写入 1 字节（0-255）
//
// 安全模型（防野指针）：
//   · 分配表跟踪：ptr.alloc 分配的内存登记 (地址, 大小)；free/read/write 只允许操作
//     登记过的指针 —— 未分配、已释放（use-after-free）、重复释放（double-free）一律报错。
//   · 越界检查：读写前校验 offset + 宽度 <= 分配大小，杜绝越界访问。
//   · 空指针检查：0 视为 NULL，读写时报错。
//   · FFI 返回的外部 ptr（load 库句柄）不在分配表中，仅可传给库函数或与 0 比较，
//     本模块的 free/read/write 会拒绝操作，防止误释放外部库管理的内存。

use std::alloc::{alloc, dealloc, Layout};
use std::collections::HashMap;
use std::sync::Mutex;

use once_cell::sync::Lazy;

use crate::error::codes;
use crate::error::ZError;
use crate::interp::Value;
use crate::lexer::Span;

/// 一个已分配内存块：大小用于释放（Layout 必须一致）与越界检查。
struct PtrEntry {
    size: usize,
}

/// 分配表：地址 -> 块信息。free 时移除条目，实现 use-after-free / double-free 检测。
static ALLOCS: Lazy<Mutex<HashMap<usize, PtrEntry>>> = Lazy::new(|| Mutex::new(HashMap::new()));

fn zerr(code: &'static str, msg: impl Into<String>, span: Span, file: &str, src: &str, help: Option<impl Into<String>>) -> ZError {
    ZError::new(code, msg, file, src, span.line, span.col, span.len.max(1), help)
}

fn as_int(v: &Value, arg: usize, span: Span, file: &str, src: &str) -> Result<i64, ZError> {
    match v {
        Value::Int(i) => Ok(*i),
        other => Err(zerr(
            codes::TYPE_MISMATCH,
            format!("`ptr.*` expects an integer for argument {}, got `{}`", arg + 1, other.type_name()),
            span,
            file,
            src,
            None::<&str>,
        )),
    }
}

fn as_float(v: &Value, arg: usize, span: Span, file: &str, src: &str) -> Result<f64, ZError> {
    match v {
        Value::Float(f) => Ok(*f),
        Value::Int(i) => Ok(*i as f64),
        other => Err(zerr(
            codes::TYPE_MISMATCH,
            format!("`ptr.*` expects a float for argument {}, got `{}`", arg + 1, other.type_name()),
            span,
            file,
            src,
            None::<&str>,
        )),
    }
}

/// 提取指针地址：ptr 值或整数（0 作 NULL）。
fn as_addr(v: &Value, arg: usize, span: Span, file: &str, src: &str) -> Result<usize, ZError> {
    match v {
        Value::Ptr(p) => Ok(*p),
        Value::Int(i) if *i >= 0 => Ok(*i as usize),
        other => Err(zerr(
            codes::TYPE_MISMATCH,
            format!("`ptr.*` expects a `ptr` for argument {}, got `{}`", arg + 1, other.type_name()),
            span,
            file,
            src,
            Some("pass a `ptr` value (from ptr.alloc or an FFI call) or `0` for NULL"),
        )),
    }
}

/// 分配 size 字节（对齐 8），失败返回 0。
fn alloc_bytes(size: usize) -> usize {
    if size == 0 {
        return 0;
    }
    let layout = match Layout::from_size_align(size, 8) {
        Ok(l) => l,
        Err(_) => return 0,
    };
    let p = unsafe { alloc(layout) };
    if p.is_null() {
        return 0;
    }
    ALLOCS.lock().unwrap().insert(p as usize, PtrEntry { size });
    p as usize
}

/// 释放登记过的块；未登记（未分配/已释放/外部句柄）返回 false。
fn free_bytes(addr: usize) -> bool {
    let mut all = ALLOCS.lock().unwrap();
    match all.remove(&addr) {
        Some(entry) => {
            let layout = match Layout::from_size_align(entry.size, 8) {
                Ok(l) => l,
                Err(_) => return false,
            };
            unsafe { dealloc(addr as *mut u8, layout) };
            true
        }
        None => false,
    }
}

/// 校验可访问区间 [addr, addr+width)，返回数据起始地址。
/// 空指针 / 未分配 / 已释放 / 越界一律报错。
fn check_range(addr: usize, width: usize, offset: i64, what: &str, span: Span, file: &str, src: &str) -> Result<usize, ZError> {
    if addr == 0 {
        return Err(zerr(
            codes::PTR_INVALID,
            format!("`{}`: cannot access a null pointer (0)", what),
            span,
            file,
            src,
            Some("check the pointer with `ptr.is_null` before dereferencing"),
        ));
    }
    if offset < 0 {
        return Err(zerr(
            codes::PTR_INVALID,
            format!("`{}`: negative offset `{}`", what, offset),
            span,
            file,
            src,
            Some("offsets must be non-negative"),
        ));
    }
    let all = ALLOCS.lock().unwrap();
    let entry = all.get(&addr).ok_or_else(|| {
        zerr(
            codes::PTR_INVALID,
            format!(
                "`{}`: pointer 0x{:x} is not allocated by `ptr.alloc` (already freed, never allocated, or an external FFI handle)",
                what, addr
            ),
            span,
            file,
            src,
            Some("only pointers returned by `ptr.alloc` can be read/written/freed"),
        )
    })?;
    let end = (offset as usize).checked_add(width).ok_or_else(|| {
        zerr(codes::PTR_INVALID, format!("`{}`: offset overflow", what), span, file, src, None::<&str>)
    })?;
    if end > entry.size {
        return Err(zerr(
            codes::PTR_OOB,
            format!("`{}`: out of bounds (block size {}, offset {} + {} bytes)", what, entry.size, offset, width),
            span,
            file,
            src,
            Some("check the offset against `ptr.size(p)`"),
        ));
    }
    Ok(addr + offset as usize)
}

/// ptr 模块调用入口。
pub fn call(name: &str, args: &[Value], span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    match name {
        "ptr.alloc" => {
            let size = as_int(&args[0], 0, span, file, src)?;
            Ok(Value::Ptr(alloc_bytes(size.max(0) as usize)))
        }
        "ptr.free" => {
            let addr = as_addr(&args[0], 0, span, file, src)?;
            if free_bytes(addr) {
                Ok(Value::Bool(true))
            } else {
                Err(zerr(
                    codes::PTR_INVALID,
                    if addr == 0 {
                        "`ptr.free`: freeing a null pointer (no-op)".to_string()
                    } else {
                        format!("`ptr.free`: pointer 0x{:x} was not allocated by `ptr.alloc` (double free, or an external FFI handle)", addr)
                    },
                    span,
                    file,
                    src,
                    Some("call `ptr.free` once per `ptr.alloc`; external FFI handles are owned by the library"),
                ))
            }
        }
        "ptr.is_null" => {
            let addr = as_addr(&args[0], 0, span, file, src)?;
            Ok(Value::Bool(addr == 0))
        }
        "ptr.is_valid" => {
            let addr = as_addr(&args[0], 0, span, file, src)?;
            let valid = addr != 0 && ALLOCS.lock().unwrap().contains_key(&addr);
            Ok(Value::Bool(valid))
        }
        "ptr.size" => {
            let addr = as_addr(&args[0], 0, span, file, src)?;
            let all = ALLOCS.lock().unwrap();
            match all.get(&addr) {
                Some(entry) => Ok(Value::Int(entry.size as i64)),
                None => Err(zerr(
                    codes::PTR_INVALID,
                    format!("`ptr.size`: pointer 0x{:x} is not allocated by `ptr.alloc`", addr),
                    span,
                    file,
                    src,
                    Some("only pointers returned by `ptr.alloc` have a tracked size"),
                )),
            }
        }
        "ptr.read_int" => {
            let addr = as_addr(&args[0], 0, span, file, src)?;
            let off = as_int(&args[1], 1, span, file, src)?;
            let p = check_range(addr, 8, off, "ptr.read_int", span, file, src)? as *const i64;
            Ok(Value::Int(unsafe { std::ptr::read_unaligned(p) }))
        }
        "ptr.read_float" => {
            let addr = as_addr(&args[0], 0, span, file, src)?;
            let off = as_int(&args[1], 1, span, file, src)?;
            let p = check_range(addr, 8, off, "ptr.read_float", span, file, src)? as *const f64;
            Ok(Value::Float(unsafe { std::ptr::read_unaligned(p) }))
        }
        "ptr.read_byte" => {
            let addr = as_addr(&args[0], 0, span, file, src)?;
            let off = as_int(&args[1], 1, span, file, src)?;
            let p = check_range(addr, 1, off, "ptr.read_byte", span, file, src)? as *const u8;
            Ok(Value::Int(unsafe { *p } as i64))
        }
        "ptr.write_int" => {
            let addr = as_addr(&args[0], 0, span, file, src)?;
            let off = as_int(&args[1], 1, span, file, src)?;
            let v = as_int(&args[2], 2, span, file, src)?;
            let p = check_range(addr, 8, off, "ptr.write_int", span, file, src)? as *mut i64;
            unsafe { std::ptr::write_unaligned(p, v) };
            Ok(Value::Null)
        }
        "ptr.write_float" => {
            let addr = as_addr(&args[0], 0, span, file, src)?;
            let off = as_int(&args[1], 1, span, file, src)?;
            let v = as_float(&args[2], 2, span, file, src)?;
            let p = check_range(addr, 8, off, "ptr.write_float", span, file, src)? as *mut f64;
            unsafe { std::ptr::write_unaligned(p, v) };
            Ok(Value::Null)
        }
        "ptr.write_byte" => {
            let addr = as_addr(&args[0], 0, span, file, src)?;
            let off = as_int(&args[1], 1, span, file, src)?;
            let v = as_int(&args[2], 2, span, file, src)?;
            if !(0..=255).contains(&v) {
                return Err(zerr(
                    codes::PTR_INVALID,
                    format!("`ptr.write_byte`: value {} out of range 0..=255", v),
                    span,
                    file,
                    src,
                    Some("bytes are unsigned 8-bit values"),
                ));
            }
            let p = check_range(addr, 1, off, "ptr.write_byte", span, file, src)? as *mut u8;
            unsafe { *p = v as u8 };
            Ok(Value::Null)
        }
        _ => Err(zerr(
            codes::NOT_IMPLEMENTED,
            format!("unknown ptr function `{}`", name),
            span,
            file,
            src,
            None::<&str>,
        )),
    }
}
