// guimod.rs - guipro 原生图形界面模块（跨平台抽象 + 原生后端）
// 设计：Rust 层提供原生窗口/控件原语与事件队列；Hone 层 hone_lib/guipro.hn
// 提供 guipro_ 前缀统一 API、主循环与闭包分发（builtins::call 无 Interp 上下文，
// 闭包调用必须在 Hone 脚本侧完成，Rust 只推送事件，Hone 层 guipro.poll() 取走）。
//
// 内置函数（guipro.*，checker 签名表 + builtins 分发）：
//   guipro.available()                 -> bool  当前平台是否有原生后端
//   guipro.window(title, w, h)         -> int   创建窗口，返回窗口 id
//   guipro.add(win, widget_dict)       -> int   在窗口添加控件，返回控件 id
//   guipro.poll()                      -> str   泵消息 + 取事件 JSON 数组
//   guipro.set_text(win, id, text)     -> void  更新控件文本
//   guipro.get_text(win, id)           -> str   读取控件文本
//   guipro.close(win)                  -> void  销毁窗口
//   guipro.msgbox(title, msg)          -> void  原生消息框（对话框）
//
// 控件 dict 约定（add 的第二个参数）：
//   {"type": "button"|"label"|"input"|"select"|"checkbox"|"radio",
//    "x": int, "y": int, "w": int, "h": int,
//    "text": str,                      // button/label/checkbox/radio 文本
//    "options": ["a", "b"],            // select 选项
//    "placeholder": str}               // input 占位提示（仅 label 之上绘制，MVP 忽略）
//  布局由 Hone 层计算绝对坐标（guipro.hn 提供 VBox/HBox 布局函数）。
//
// 事件 JSON（poll 返回，风格与 server.poll 一致）：
//   [{"win":1,"id":2,"type":"click","value":""},        // button
//    {"win":1,"id":3,"type":"change","value":"文本"},    // input/select 变更
//    {"win":1,"id":4,"type":"change","value":"1"},       // checkbox/radio（1/0）
//    {"win":1,"id":0,"type":"close","value":""},         // 窗口关闭
//    {"win":1,"id":0,"type":"resize","value":"800x600"}] // 窗口缩放
//
// 后端：Windows = Win32 标准控件（user32，零新增依赖）；
//       Linux = GTK3 动态加载（libloading），缺失时降级 X11 自绘（后续任务）。

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
            format!("`guipro` expects a string for argument {}, got `{}`", arg + 1, other.type_name()),
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
            format!("`guipro` expects an integer for argument {}, got `{}`", arg + 1, other.type_name()),
            span,
            file,
            src,
            Some("pass an `int` value"),
        )),
    }
}

// ---------- dict 辅助（Hone dict 为 Vec<(String, Value)> 保持插入顺序） ----------

fn dict_get<'a>(d: &'a Value, key: &str) -> Option<&'a Value> {
    if let Value::Dict(entries) = d {
        entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    } else {
        None
    }
}

fn dict_str(d: &Value, key: &str, span: Span, file: &str, src: &str) -> Result<String, ZError> {
    match dict_get(d, key) {
        Some(Value::Str(s)) => Ok(s.clone()),
        Some(other) => Err(zerr(
            codes::TYPE_MISMATCH,
            format!("widget field `{}` must be a string, got `{}`", key, other.type_name()),
            span,
            file,
            src,
            Some("check the widget dict"),
        )),
        None => Ok(String::new()),
    }
}

fn dict_int(d: &Value, key: &str, def: i64, span: Span, file: &str, src: &str) -> Result<i64, ZError> {
    match dict_get(d, key) {
        Some(Value::Int(i)) => Ok(*i),
        Some(other) => Err(zerr(
            codes::TYPE_MISMATCH,
            format!("widget field `{}` must be an integer, got `{}`", key, other.type_name()),
            span,
            file,
            src,
            Some("check the widget dict"),
        )),
        None => Ok(def),
    }
}

// ---------- 入口 ----------

pub fn call(name: &str, args: &[Value], span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    platform::call(name, args, span, file, src)
}

// ============ 非 Windows 实现（GTK3 动态加载 + X11 降级，见 guimod_gtk.rs） ============

#[cfg(not(windows))]
mod platform {
    use super::*;

    pub fn call(name: &str, args: &[Value], span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        crate::guimod_gtk::call(name, args, span, file, src)
    }
}

// ============ Windows 实现（Win32 标准控件，user32/gdi32，零新增依赖） ============

#[cfg(windows)]
mod platform {
    use super::*;
    use std::collections::HashMap;
    use std::ptr;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::Mutex;
    use winapi::ctypes::c_void;
    use winapi::shared::minwindef::{DWORD, HINSTANCE, HIWORD, LOWORD, LPARAM, LRESULT, UINT, WPARAM, WORD};
    use winapi::shared::windef::{HBRUSH, HFONT, HWND};
    use winapi::um::errhandlingapi::GetLastError;
    use winapi::um::libloaderapi::GetModuleHandleW;
    use winapi::um::wingdi::{GetStockObject, DEFAULT_GUI_FONT};
    use winapi::um::winuser::*;

    // ---------- 全局状态 ----------

    static NEXT_WIN_ID: AtomicI64 = AtomicI64::new(1);
    static NEXT_CTL_ID: AtomicI64 = AtomicI64::new(1);
    static EVENTS: Mutex<Vec<String>> = Mutex::new(Vec::new());
    static WINDOWS: std::sync::LazyLock<Mutex<HashMap<i64, WinState>>> =
        std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
    static CLASS_READY: std::sync::Once = std::sync::Once::new();
    /// HINSTANCE 句柄（存 usize 规避裸指针的 Send/Sync 限制；仅主线程使用）。
    static HINST: Mutex<usize> = Mutex::new(0);

    struct WinState {
        hwnd: HWND,
        /// 控件 id → 类型（"button"/"label"/"input"/"select"/"checkbox"/"radio"）
        ctl_kind: HashMap<i64, String>,
        /// 控件 id → 原生句柄
        ctl_hwnd: HashMap<i64, HWND>,
    }

    /// HWND 是裸指针；窗口注册表仅经 Mutex 在 GUI 线程访问，标记 Send 可行。
    unsafe impl Send for WinState {}

    fn to_utf16(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn win_err(what: &str, span: Span, file: &str, src: &str) -> ZError {
        let code = unsafe { GetLastError() };
        zerr(
            codes::SYSCALL,
            format!("{} failed (Windows error {})", what, code),
            span,
            file,
            src,
            Some("check the arguments or system state"),
        )
    }

    /// 控件 id 必须是 16 位（WM_COMMAND 的 wParam 低 16 位传控件 id），跳过 0。
    fn next_ctl_id() -> i64 {
        loop {
            let id = NEXT_CTL_ID.fetch_add(1, Ordering::Relaxed) & 0xFFFF;
            if id != 0 {
                return id;
            }
        }
    }

    /// 事件入队（JSON 对象字符串，poll 时拼接为数组返回）。
    fn push_event(win: i64, id: i64, ty: &str, value: &str) {
        let ev = serde_json::json!({"win": win, "id": id, "type": ty, "value": value}).to_string();
        EVENTS.lock().unwrap().push(ev);
    }

    fn find_win_by_hwnd(hwnd: HWND) -> Option<i64> {
        WINDOWS.lock().unwrap().iter().find(|(_, s)| s.hwnd == hwnd).map(|(id, _)| *id)
    }

    /// 按 (控件 id, 控件句柄) 定位窗口 id 与控件类型。
    fn find_ctl(ctl_id: i64, hctl: HWND) -> Option<(i64, String)> {
        let wins = WINDOWS.lock().unwrap();
        for (win_id, s) in wins.iter() {
            if let Some(kind) = s.ctl_kind.get(&ctl_id) {
                if s.ctl_hwnd.get(&ctl_id) == Some(&hctl) {
                    return Some((*win_id, kind.clone()));
                }
            }
        }
        None
    }

    fn get_ctl_text(hctl: HWND) -> String {
        unsafe {
            let len = GetWindowTextLengthW(hctl) as usize;
            let mut buf = vec![0u16; len + 1];
            GetWindowTextW(hctl, buf.as_mut_ptr(), (len + 1) as i32);
            String::from_utf16_lossy(&buf[..len])
        }
    }

    // ---------- 窗口过程 ----------

    unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: UINT, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        match msg {
            WM_COMMAND => {
                // 子控件通知：wParam 低 16 位 = 控件 id，高 16 位 = 通知码；lParam = 控件句柄
                let ctl_id = LOWORD(wparam as DWORD) as i64;
                let code = HIWORD(wparam as DWORD) as WORD;
                let hctl = lparam as HWND;
                if let Some((win_id, kind)) = find_ctl(ctl_id, hctl) {
                    match code {
                        BN_CLICKED => {
                            if kind == "button" {
                                push_event(win_id, ctl_id, "click", "");
                            } else if kind == "checkbox" || kind == "radio" {
                                let checked = SendMessageW(hctl, BM_GETCHECK, 0, 0) == BST_CHECKED as isize;
                                push_event(win_id, ctl_id, "change", if checked { "1" } else { "0" });
                            }
                        }
                        EN_CHANGE => {
                            // input 文本变更
                            push_event(win_id, ctl_id, "change", &get_ctl_text(hctl));
                        }
                        CBN_SELCHANGE => {
                            // select 选项变更：取当前选中项文本
                            let idx = SendMessageW(hctl, CB_GETCURSEL, 0, 0) as i32;
                            let text = if idx >= 0 {
                                unsafe {
                                    let len = SendMessageW(hctl, CB_GETLBTEXTLEN, idx as WPARAM, 0) as usize;
                                    let mut buf = vec![0u16; len + 1];
                                    SendMessageW(hctl, CB_GETLBTEXT, idx as WPARAM, buf.as_mut_ptr() as LPARAM);
                                    String::from_utf16_lossy(&buf[..len])
                                }
                            } else {
                                String::new()
                            };
                            push_event(win_id, ctl_id, "change", &text);
                        }
                        _ => {}
                    }
                }
                0
            }
            WM_SIZE => {
                if let Some(win_id) = find_win_by_hwnd(hwnd) {
                    let w = LOWORD(lparam as DWORD) as i32;
                    let h = HIWORD(lparam as DWORD) as i32;
                    push_event(win_id, 0, "resize", &format!("{}x{}", w, h));
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_CLOSE => {
                if let Some(win_id) = find_win_by_hwnd(hwnd) {
                    push_event(win_id, 0, "close", "");
                }
                DestroyWindow(hwnd);
                0
            }
            WM_DESTROY => {
                // 从注册表移除（close 已推送事件，脚本侧据此收尾）
                if let Some(win_id) = find_win_by_hwnd(hwnd) {
                    WINDOWS.lock().unwrap().remove(&win_id);
                }
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    /// 注册窗口类（进程内一次）。
    fn ensure_class(span: Span, file: &str, src: &str) -> Result<HINSTANCE, ZError> {
        let mut reg_err: Option<DWORD> = None;
        CLASS_READY.call_once(|| {
            unsafe {
                let hinst = GetModuleHandleW(ptr::null());
                *HINST.lock().unwrap() = hinst as usize;
                let class_name = to_utf16("HoneGuiproWnd");
                let wc = WNDCLASSEXW {
                    cbSize: std::mem::size_of::<WNDCLASSEXW>() as UINT,
                    style: CS_HREDRAW | CS_VREDRAW,
                    lpfnWndProc: Some(wnd_proc),
                    cbClsExtra: 0,
                    cbWndExtra: 0,
                    hInstance: hinst,
                    hIcon: ptr::null_mut(),
                    hCursor: LoadCursorW(ptr::null_mut(), IDC_ARROW),
                    hbrBackground: (COLOR_BTNFACE + 1) as *mut c_void as HBRUSH,
                    lpszMenuName: ptr::null(),
                    lpszClassName: class_name.as_ptr(),
                    hIconSm: ptr::null_mut(),
                };
                if RegisterClassExW(&wc) == 0 {
                    reg_err = Some(GetLastError());
                }
            }
        });
        if let Some(code) = reg_err {
            return Err(zerr(
                codes::SYSCALL,
                format!("RegisterClassExW failed (Windows error {})", code),
                span,
                file,
                src,
                Some("guipro requires a GUI session"),
            ));
        }
        Ok(*HINST.lock().unwrap() as HINSTANCE)
    }

    /// 创建子控件（标准控件类）。
    fn create_ctl(
        win: &WinState,
        ctl_id: i64,
        kind: &str,
        d: &Value,
        hinst: HINSTANCE,
        span: Span,
        file: &str,
        src: &str,
    ) -> Result<HWND, ZError> {
        let x = dict_int(d, "x", 0, span, file, src)? as i32;
        let y = dict_int(d, "y", 0, span, file, src)? as i32;
        let w = dict_int(d, "w", 80, span, file, src)? as i32;
        let h = dict_int(d, "h", 24, span, file, src)? as i32;
        let text = dict_str(d, "text", span, file, src)?;
        let (cls, style) = match kind {
            "button" => ("BUTTON", BS_PUSHBUTTON | WS_TABSTOP),
            "label" => ("STATIC", 0),
            "input" => ("EDIT", ES_AUTOHSCROLL | WS_TABSTOP),
            "select" => ("COMBOBOX", CBS_DROPDOWNLIST | WS_TABSTOP),
            "checkbox" => ("BUTTON", BS_AUTOCHECKBOX | WS_TABSTOP),
            "radio" => ("BUTTON", BS_AUTORADIOBUTTON | WS_TABSTOP),
            other => {
                return Err(zerr(
                    codes::TYPE_MISMATCH,
                    format!("unknown widget type `{}` (button/label/input/select/checkbox/radio)", other),
                    span,
                    file,
                    src,
                    Some("check the widget `type` field"),
                ));
            }
        };
        let hctl = unsafe {
            CreateWindowExW(
                0,
                to_utf16(cls).as_ptr(),
                to_utf16(&text).as_ptr(),
                WS_CHILD | WS_VISIBLE | style,
                x, y, w, h,
                win.hwnd,
                ctl_id as *mut c_void as winapi::shared::windef::HMENU,
                hinst,
                ptr::null_mut(),
            )
        };
        if hctl.is_null() {
            return Err(win_err("CreateWindowExW", span, file, src));
        }
        // 统一 GUI 字体（避免中文乱码）
        unsafe {
            let font = GetStockObject(DEFAULT_GUI_FONT as i32) as HFONT;
            SendMessageW(hctl, WM_SETFONT, font as WPARAM, 1);
        }
        // select 填充选项
        if kind == "select" {
            if let Some(Value::List(opts)) = dict_get(d, "options") {
                for o in opts {
                    if let Value::Str(s) = o {
                        let ws = to_utf16(s);
                        unsafe {
                            SendMessageW(hctl, CB_ADDSTRING as UINT, 0, ws.as_ptr() as LPARAM);
                        }
                    }
                }
                // 默认选中第一项
                unsafe {
                    SendMessageW(hctl, CB_SETCURSEL, 0, 0);
                }
            }
        }
        Ok(hctl)
    }

    // ---------- 内置函数实现 ----------

    pub fn call(name: &str, args: &[Value], span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        match name {
            "guipro.available" => Ok(Value::Bool(true)),
            "guipro.window" => {
                if args.len() != 3 {
                    return Err(arg_count(name, 3, args.len(), span, file, src));
                }
                let title = as_str(&args[0], 0, span, file, src)?;
                let w = as_int(&args[1], 1, span, file, src)?;
                let h = as_int(&args[2], 2, span, file, src)?;
                window(title, w, h, span, file, src)
            }
            "guipro.add" => {
                if args.len() != 2 {
                    return Err(arg_count(name, 2, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                add(win, &args[1], span, file, src)
            }
            "guipro.poll" => {
                if !args.is_empty() {
                    return Err(arg_count(name, 0, args.len(), span, file, src));
                }
                poll(span, file, src)
            }
            "guipro.set_text" => {
                if args.len() != 3 {
                    return Err(arg_count(name, 3, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                let ctl = as_int(&args[1], 1, span, file, src)?;
                let text = as_str(&args[2], 2, span, file, src)?;
                set_text(win, ctl, text, span, file, src)
            }
            "guipro.get_text" => {
                if args.len() != 2 {
                    return Err(arg_count(name, 2, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                let ctl = as_int(&args[1], 1, span, file, src)?;
                get_text(win, ctl, span, file, src)
            }
            "guipro.close" => {
                if args.len() != 1 {
                    return Err(arg_count(name, 1, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                close(win, span, file, src)
            }
            "guipro.msgbox" => {
                if args.len() != 2 {
                    return Err(arg_count(name, 2, args.len(), span, file, src));
                }
                let title = as_str(&args[0], 0, span, file, src)?;
                let msg = as_str(&args[1], 1, span, file, src)?;
                msgbox(title, msg, span, file, src)
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

    fn arg_count(name: &str, want: usize, got: usize, span: Span, file: &str, src: &str) -> ZError {
        zerr(
            codes::ARG_COUNT,
            format!("wrong number of arguments: `{}` expects {}, got {}", name, want, got),
            span,
            file,
            src,
            Some("check the call"),
        )
    }

    /// 创建窗口，返回窗口 id。
    fn window(title: &str, w: i64, h: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        if w <= 0 || h <= 0 {
            return Err(zerr(
                codes::TYPE_MISMATCH,
                "`guipro.window` requires positive width and height",
                span,
                file,
                src,
                Some("pass w > 0 and h > 0"),
            ));
        }
        let hinst = ensure_class(span, file, src)?;
        let win_id = NEXT_WIN_ID.fetch_add(1, Ordering::Relaxed);
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                to_utf16("HoneGuiproWnd").as_ptr(),
                to_utf16(title).as_ptr(),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT, CW_USEDEFAULT,
                w as i32, h as i32,
                ptr::null_mut(),
                ptr::null_mut(),
                hinst,
                ptr::null_mut(),
            )
        };
        if hwnd.is_null() {
            return Err(win_err("CreateWindowExW", span, file, src));
        }
        WINDOWS.lock().unwrap().insert(
            win_id,
            WinState {
                hwnd,
                ctl_kind: HashMap::new(),
                ctl_hwnd: HashMap::new(),
            },
        );
        unsafe {
            ShowWindow(hwnd, SW_SHOW);
            UpdateWindow(hwnd);
        }
        Ok(Value::Int(win_id))
    }

    /// 添加控件，返回控件 id。
    fn add(win_id: i64, d: &Value, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        let kind = dict_str(d, "type", span, file, src)?;
        if kind.is_empty() {
            return Err(zerr(
                codes::TYPE_MISMATCH,
                "widget dict requires a `type` field (button/label/input/select/checkbox/radio)",
                span,
                file,
                src,
                Some("check the widget dict"),
            ));
        }
        let mut wins = WINDOWS.lock().unwrap();
        let win = wins.get_mut(&win_id).ok_or_else(|| {
            zerr(
                codes::NOT_FOUND,
                format!("window {} does not exist", win_id),
                span,
                file,
                src,
                Some("create it with `guipro.window` first"),
            )
        })?;
        let ctl_id = next_ctl_id();
        let hinst = *HINST.lock().unwrap() as HINSTANCE;
        let hctl = create_ctl(win, ctl_id, &kind, d, hinst, span, file, src)?;
        win.ctl_kind.insert(ctl_id, kind);
        win.ctl_hwnd.insert(ctl_id, hctl);
        Ok(Value::Int(ctl_id))
    }

    /// 泵消息（非阻塞）+ 取事件 JSON 数组。
    fn poll(_span: Span, _file: &str, _src: &str) -> Result<Value, ZError> {
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                if msg.message == WM_QUIT {
                    continue;
                }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        let evs = EVENTS.lock().unwrap().clone();
        EVENTS.lock().unwrap().clear();
        Ok(Value::Str(format!("[{}]", evs.join(","))))
    }

    fn set_text(win_id: i64, ctl_id: i64, text: &str, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        let wins = WINDOWS.lock().unwrap();
        let win = wins.get(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
        let hctl = win.ctl_hwnd.get(&ctl_id).copied().ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
        unsafe {
            SetWindowTextW(hctl, to_utf16(text).as_ptr());
        }
        Ok(Value::Null)
    }

    fn get_text(win_id: i64, ctl_id: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        let wins = WINDOWS.lock().unwrap();
        let win = wins.get(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
        let hctl = win.ctl_hwnd.get(&ctl_id).copied().ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
        Ok(Value::Str(get_ctl_text(hctl)))
    }

    fn close(win_id: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        let hwnd = {
            let wins = WINDOWS.lock().unwrap();
            wins.get(&win_id).map(|w| w.hwnd).ok_or_else(|| win_missing(win_id, span, file, src))?
        };
        unsafe {
            DestroyWindow(hwnd);
        }
        // 主动 close 也推送 close 事件：WM_DESTROY 不产生 WM_CLOSE，
        // 不推送则主循环（guipro_run 的 poll）无法感知窗口已关，会继续跑。
        push_event(win_id, 0, "close", "");
        Ok(Value::Null)
    }

    fn msgbox(title: &str, msg: &str, _span: Span, _file: &str, _src: &str) -> Result<Value, ZError> {
        unsafe {
            MessageBoxW(
                ptr::null_mut(),
                to_utf16(msg).as_ptr(),
                to_utf16(title).as_ptr(),
                MB_OK | MB_ICONINFORMATION,
            );
        }
        Ok(Value::Null)
    }

    fn win_missing(win_id: i64, span: Span, file: &str, src: &str) -> ZError {
        zerr(
            codes::NOT_FOUND,
            format!("window {} does not exist", win_id),
            span,
            file,
            src,
            Some("create it with `guipro.window` first"),
        )
    }

    fn ctl_missing(win_id: i64, ctl_id: i64, span: Span, file: &str, src: &str) -> ZError {
        zerr(
            codes::NOT_FOUND,
            format!("widget {} does not exist in window {}", ctl_id, win_id),
            span,
            file,
            src,
            Some("add it with `guipro.add` first"),
        )
    }
}
