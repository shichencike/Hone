// sysmod.rs - sys 模块（Windows API 优先，其他平台尽力模拟或报错）
// 规范 3.2：Windows 优先，跨平台部分用 std 模拟。本实现：
//   sys.msgbox(title, message, style)  系统消息弹窗（style: "info"/"warn"/"error"）
//   sys.beep(freq, duration)           系统提示音（Hz, ms）
//   sys.clipboard_set(text)            复制文本到剪贴板
//   sys.get_screen_size()              返回 "宽x高"（Zap 无元组类型，以字符串表达）
//   sys.reg_read(key)                  读取注册表值（Windows）
//   sys.reg_write(key, value)          写入注册表值（Windows）
// 非 Windows 平台：msgbox/beep 降级为终端输出，其余报 error[Z999]。

use crate::error::codes;
use crate::error::ZError;
use crate::interp::Value;
use crate::lexer::Span;

fn zerr(code: &'static str, msg: impl Into<String>, span: Span, file: &str, src: &str, help: Option<impl Into<String>>) -> ZError {
    ZError::new(code, msg, file, src, span.line, span.col, span.len.max(1), help)
}

fn as_str<'a>(v: &'a Value, arg: usize, span: Span, file: &str, src: &str) -> Result<&'a str, ZError> {
    match v {
        Value::Str(s) => Ok(s),
        other => Err(zerr(
            codes::TYPE_MISMATCH,
            format!("`sys` expects a string for argument {}, got `{}`", arg + 1, other.type_name()),
            span,
            file,
            src,
            Some("pass a string value"),
        )),
    }
}

fn as_int(v: &Value, arg: usize, span: Span, file: &str, src: &str) -> Result<i64, ZError> {
    match v {
        Value::Int(i) => Ok(*i),
        other => Err(zerr(
            codes::TYPE_MISMATCH,
            format!("`sys` expects an integer for argument {}, got `{}`", arg + 1, other.type_name()),
            span,
            file,
            src,
            Some("pass an `int` value"),
        )),
    }
}

/// sys 模块调用入口（参数已由 checker 校验数量）。
pub fn call(name: &str, args: &[Value], span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    match name {
        "sys.msgbox" => {
            let title = as_str(&args[0], 0, span, file, src)?;
            let message = as_str(&args[1], 1, span, file, src)?;
            let style = as_str(&args[2], 2, span, file, src)?;
            platform::msgbox(title, message, style, span, file, src)
        }
        "sys.beep" => {
            let freq = as_int(&args[0], 0, span, file, src)?;
            let dur = as_int(&args[1], 1, span, file, src)?;
            platform::beep(freq, dur, span, file, src)
        }
        "sys.clipboard_set" => {
            let text = as_str(&args[0], 0, span, file, src)?;
            platform::clipboard_set(text, span, file, src)
        }
        "sys.get_screen_size" => platform::get_screen_size(span, file, src),
        "sys.reg_read" => {
            let key = as_str(&args[0], 0, span, file, src)?;
            platform::reg_read(key, span, file, src)
        }
        "sys.reg_write" => {
            let key = as_str(&args[0], 0, span, file, src)?;
            let value = as_str(&args[1], 1, span, file, src)?;
            platform::reg_write(key, value, span, file, src)
        }
        other => Err(zerr(
            codes::UNDEFINED,
            format!("undefined function `{}`", other),
            span,
            file,
            src,
            Some("check the spelling"),
        )),
    }
}

// ============ Windows 实现（winapi） ============

#[cfg(windows)]
mod platform {
    use super::*;
    use std::ptr;
    use winapi::shared::minwindef::{BOOL, DWORD, HKEY, UINT};
    use winapi::um::errhandlingapi::GetLastError;
    use winapi::um::winbase::{GlobalAlloc, GlobalFree, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use winapi::um::winnt::{KEY_READ, KEY_SET_VALUE, REG_EXPAND_SZ, REG_OPTION_NON_VOLATILE, REG_SZ};
    use winapi::um::winreg::{
        RegCloseKey, RegCreateKeyExW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
        HKEY_CLASSES_ROOT, HKEY_CURRENT_CONFIG, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, HKEY_USERS,
    };
    use winapi::um::winuser::{
        CloseClipboard, EmptyClipboard, GetSystemMetrics, MessageBoxW, OpenClipboard,
        SetClipboardData, CF_UNICODETEXT, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONWARNING,
        MB_OK, SM_CXSCREEN, SM_CYSCREEN,
    };

    // kernel32!Beep（winapi 0.3 未绑定该函数，直接声明）
    extern "system" {
        fn Beep(dwFreq: DWORD, dwDuration: DWORD) -> BOOL;
    }

    fn to_utf16(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn win_err(what: &str, span: Span, file: &str, src: &str) -> ZError {
        let code = unsafe { GetLastError() };
        // Windows 错误码 5 = ERROR_ACCESS_DENIED，归类为权限不足
        let (err_code, hint): (&'static str, &'static str) = if code == 5 {
            (codes::PERMISSION, "access denied — check permissions or run with administrator rights")
        } else {
            (codes::SYSCALL, "check the arguments or system state")
        };
        zerr(
            err_code,
            format!("{} failed (Windows error {})", what, code),
            span,
            file,
            src,
            Some(hint),
        )
    }

    pub fn msgbox(title: &str, message: &str, style: &str, _span: Span, _file: &str, _src: &str) -> Result<Value, ZError> {
        let flags: UINT = match style {
            "warn" => MB_OK | MB_ICONWARNING,
            "error" => MB_OK | MB_ICONERROR,
            _ => MB_OK | MB_ICONINFORMATION,
        };
        let t = to_utf16(title);
        let m = to_utf16(message);
        unsafe {
            MessageBoxW(ptr::null_mut(), m.as_ptr(), t.as_ptr(), flags);
        }
        Ok(Value::Null)
    }

    pub fn beep(freq: i64, duration: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        if freq < 0 || duration < 0 {
            return Err(zerr(
                codes::TYPE_MISMATCH,
                "`sys.beep` requires non-negative frequency and duration",
                span,
                file,
                src,
                None::<&str>,
            ));
        }
        let ok = unsafe { Beep(freq as DWORD, duration as DWORD) };
        if ok == 0 {
            return Err(win_err("Beep", span, file, src));
        }
        Ok(Value::Null)
    }

    pub fn clipboard_set(text: &str, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        let data = to_utf16(text);
        let bytes = data.len() * 2;
        unsafe {
            if OpenClipboard(ptr::null_mut()) == 0 {
                return Err(win_err("OpenClipboard", span, file, src));
            }
            if EmptyClipboard() == 0 {
                CloseClipboard();
                return Err(win_err("EmptyClipboard", span, file, src));
            }
            let hmem = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if hmem.is_null() {
                CloseClipboard();
                return Err(win_err("GlobalAlloc", span, file, src));
            }
            let dst = GlobalLock(hmem) as *mut u8;
            if dst.is_null() {
                GlobalFree(hmem);
                CloseClipboard();
                return Err(win_err("GlobalLock", span, file, src));
            }
            ptr::copy_nonoverlapping(data.as_ptr() as *const u8, dst, bytes);
            GlobalUnlock(hmem);
            if SetClipboardData(CF_UNICODETEXT, hmem as *mut winapi::ctypes::c_void).is_null() {
                GlobalFree(hmem);
                CloseClipboard();
                return Err(win_err("SetClipboardData", span, file, src));
            }
            CloseClipboard();
        }
        Ok(Value::Null)
    }

    pub fn get_screen_size(span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        let w = unsafe { GetSystemMetrics(SM_CXSCREEN) };
        let h = unsafe { GetSystemMetrics(SM_CYSCREEN) };
        if w <= 0 || h <= 0 {
            return Err(win_err("GetSystemMetrics", span, file, src));
        }
        Ok(Value::Str(format!("{}x{}", w, h)))
    }

    /// 解析 "HKLM\Software\Zap\ValueName" → (根键, 路径, 值名)。
    fn parse_reg_key(key: &str, span: Span, file: &str, src: &str) -> Result<(HKEY, String, String), ZError> {
        let mut parts = key.split('\\');
        let root = match parts.next().unwrap_or("").to_uppercase().as_str() {
            "HKCR" => HKEY_CLASSES_ROOT,
            "HKCU" => HKEY_CURRENT_USER,
            "HKLM" => HKEY_LOCAL_MACHINE,
            "HKU" => HKEY_USERS,
            "HKCC" => HKEY_CURRENT_CONFIG,
            other => {
                return Err(zerr(
                    codes::TYPE_MISMATCH,
                    format!(
                        "invalid registry key `{}`: unknown root `{}` (use HKCR/HKCU/HKLM/HKU/HKCC)",
                        key, other
                    ),
                    span,
                    file,
                    src,
                    Some("format: `HKCU\\Software\\App\\ValueName`"),
                ));
            }
        };
        let rest: Vec<&str> = parts.collect();
        if rest.is_empty() {
            return Err(zerr(
                codes::TYPE_MISMATCH,
                format!("invalid registry key `{}`: missing value name", key),
                span,
                file,
                src,
                Some("format: `HKCU\\Software\\App\\ValueName`"),
            ));
        }
        let value_name = rest.last().unwrap().to_string();
        let path = rest[..rest.len() - 1].join("\\");
        Ok((root, path, value_name))
    }

    pub fn reg_read(key: &str, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        let (root, path, value_name) = parse_reg_key(key, span, file, src)?;
        let mut hkey: HKEY = ptr::null_mut();
        let status = unsafe {
            RegOpenKeyExW(root, to_utf16(&path).as_ptr(), 0, KEY_READ, &mut hkey)
        };
        if status != 0 {
            return Err(zerr(
                codes::SYSCALL,
                format!("cannot open registry key `{}` (error {})", key, status),
                span,
                file,
                src,
                Some("the key may not exist, or access is denied"),
            ));
        }
        let mut size: DWORD = 0;
        let st1 = unsafe {
            RegQueryValueExW(
                hkey,
                to_utf16(&value_name).as_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut size,
            )
        };
        if st1 != 0 {
            unsafe { RegCloseKey(hkey) };
            return Err(zerr(
                codes::SYSCALL,
                format!("cannot read registry value `{}` (error {})", value_name, st1),
                span,
                file,
                src,
                Some("the value may not exist"),
            ));
        }
        let mut buf = vec![0u8; size as usize];
        let mut ty: DWORD = 0;
        let st2 = unsafe {
            RegQueryValueExW(
                hkey,
                to_utf16(&value_name).as_ptr(),
                ptr::null_mut(),
                &mut ty,
                buf.as_mut_ptr() as *mut u8,
                &mut size,
            )
        };
        unsafe { RegCloseKey(hkey) };
        if st2 != 0 {
            return Err(zerr(
                codes::SYSCALL,
                format!("cannot read registry value `{}` (error {})", value_name, st2),
                span,
                file,
                src,
                None::<&str>,
            ));
        }
        if ty != REG_SZ && ty != REG_EXPAND_SZ {
            return Err(zerr(
                codes::TYPE_MISMATCH,
                format!("registry value `{}` is not a string (type {})", value_name, ty),
                span,
                file,
                src,
                Some("only string registry values are supported"),
            ));
        }
        let u16s: Vec<u16> = buf
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0)
            .collect();
        match String::from_utf16(&u16s) {
            Ok(s) => Ok(Value::Str(s)),
            Err(_) => Err(zerr(
                codes::SYSCALL,
                format!("registry value `{}` is not valid UTF-16", value_name),
                span,
                file,
                src,
                None::<&str>,
            )),
        }
    }

    pub fn reg_write(key: &str, value: &str, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        let (root, path, value_name) = parse_reg_key(key, span, file, src)?;
        let mut hkey: HKEY = ptr::null_mut();
        let status = unsafe {
            RegCreateKeyExW(
                root,
                to_utf16(&path).as_ptr(),
                0,
                ptr::null_mut(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                ptr::null_mut(),
                &mut hkey,
                ptr::null_mut(),
            )
        };
        if status != 0 {
            return Err(zerr(
                codes::SYSCALL,
                format!("cannot create/open registry key `{}` (error {})", path, status),
                span,
                file,
                src,
                Some("access may be denied"),
            ));
        }
        let data = to_utf16(value);
        let st = unsafe {
            RegSetValueExW(
                hkey,
                to_utf16(&value_name).as_ptr(),
                0,
                REG_SZ,
                data.as_ptr() as *const u8,
                (data.len() * 2) as DWORD,
            )
        };
        unsafe { RegCloseKey(hkey) };
        if st != 0 {
            return Err(zerr(
                codes::SYSCALL,
                format!("cannot write registry value `{}` (error {})", value_name, st),
                span,
                file,
                src,
                None::<&str>,
            ));
        }
        Ok(Value::Null)
    }
}

// ============ 非 Windows 降级实现 ============

#[cfg(not(windows))]
mod platform {
    use super::*;
    use std::io::Write;

    pub fn msgbox(_title: &str, message: &str, _style: &str, _span: Span, _file: &str, _src: &str) -> Result<Value, ZError> {
        println!("[msgbox] {}", message);
        Ok(Value::Null)
    }

    pub fn beep(_freq: i64, _duration: i64, _span: Span, _file: &str, _src: &str) -> Result<Value, ZError> {
        let _ = std::io::stdout().write_all(b"\x07");
        let _ = std::io::stdout().flush();
        Ok(Value::Null)
    }

    pub fn clipboard_set(_text: &str, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        Err(zerr(
            codes::NOT_IMPLEMENTED,
            "`sys.clipboard_set` is only available on Windows",
            span,
            file,
            src,
            None::<&str>,
        ))
    }

    pub fn get_screen_size(span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        Err(zerr(
            codes::NOT_IMPLEMENTED,
            "`sys.get_screen_size` is only available on Windows",
            span,
            file,
            src,
            None::<&str>,
        ))
    }

    pub fn reg_read(_key: &str, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        Err(zerr(
            codes::NOT_IMPLEMENTED,
            "`sys.reg_read` is only available on Windows",
            span,
            file,
            src,
            None::<&str>,
        ))
    }

    pub fn reg_write(_key: &str, _value: &str, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        Err(zerr(
            codes::NOT_IMPLEMENTED,
            "`sys.reg_write` is only available on Windows",
            span,
            file,
            src,
            None::<&str>,
        ))
    }
}
