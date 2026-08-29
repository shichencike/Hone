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

    /// GTK3 优先，X11 自绘兜底。
    pub fn call(name: &str, args: &[Value], span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        if name == "guipro.available" {
            return Ok(Value::Bool(crate::guimod_gtk::available() || crate::guimod_x11::available()));
        }
        if crate::guimod_gtk::available() {
            crate::guimod_gtk::call(name, args, span, file, src)
        } else {
            crate::guimod_x11::call(name, args, span, file, src)
        }
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
    use winapi::ctypes::{c_int, c_void};
    use winapi::shared::minwindef::{DWORD, HINSTANCE, HIWORD, LOWORD, LPARAM, LRESULT, UINT, WPARAM, WORD};
    use winapi::shared::ntdef::LPWSTR;
    use winapi::shared::windef::{HBRUSH, HDC, HGDIOBJ, HFONT, HMENU, HWND, POINT, RECT};
    use winapi::um::errhandlingapi::GetLastError;
    use winapi::um::commctrl::{
        InitCommonControlsEx, INITCOMMONCONTROLSEX, ICC_BAR_CLASSES, ICC_LISTVIEW_CLASSES,
        ICC_TREEVIEW_CLASSES, ICC_PROGRESS_CLASS, ICC_TAB_CLASSES, TRACKBAR_CLASS,
        TBM_SETRANGE, TBM_SETPOS, TBM_GETPOS, TBS_HORZ, TBS_AUTOTICKS,
        TB_THUMBTRACK, TB_ENDTRACK,
        WC_LISTVIEW, WC_TREEVIEW,
        LVM_INSERTCOLUMNW, LVM_INSERTITEMW, LVM_SETITEMTEXTW, LVM_GETITEMCOUNT,
        LVM_DELETEALLITEMS, LVM_GETNEXTITEM, LVM_GETITEMW, LVM_SETEXTENDEDLISTVIEWSTYLE,
        LVIF_TEXT, LVCF_TEXT, LVCF_WIDTH, LVCF_SUBITEM, LVNI_SELECTED, LVIS_SELECTED,
        LVS_REPORT, LVS_SINGLESEL, LVS_SHOWSELALWAYS, LVS_EX_FULLROWSELECT, LVS_EX_GRIDLINES,
        LVN_ITEMCHANGED, NM_DBLCLK, LVITEMW, LVCOLUMNW, NMLISTVIEW,
        TVM_INSERTITEMW, TVM_DELETEITEM, TVM_GETNEXTITEM,
        TVGN_CARET, TVIF_TEXT, TVIF_PARAM,
        TVN_SELCHANGEDW, TVS_HASLINES, TVS_LINESATROOT, TVS_HASBUTTONS, TVS_SHOWSELALWAYS,
        TVINSERTSTRUCTW, TVI_ROOT, TVI_SORT, NMTREEVIEWW, HTREEITEM,
    };
    use winapi::um::libloaderapi::GetModuleHandleW;
    use winapi::um::shellapi::{Shell_NotifyIconW, NOTIFYICONDATAW, NIM_ADD, NIM_MODIFY, NIM_DELETE, NIF_MESSAGE, NIF_ICON, NIF_TIP};
    use winapi::um::wingdi::{
        GetStockObject, DEFAULT_GUI_FONT, CreatePen, CreateSolidBrush, SelectObject, DeleteObject,
        MoveToEx, LineTo, Rectangle, Ellipse, SetTextColor, SetBkMode, TextOutW,
        PS_SOLID, NULL_BRUSH, WHITE_BRUSH, TRANSPARENT,
        StretchDIBits, SetStretchBltMode, CreateFontW,
        BITMAPINFO, BITMAPINFOHEADER, BI_RGB, COLORONCOLOR, DIB_RGB_COLORS, SRCCOPY,
        DEFAULT_CHARSET, FW_NORMAL, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS,
        CLEARTYPE_QUALITY, DEFAULT_PITCH, FF_DONTCARE,
    };
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
        /// 控件 id → 类型（"button"/"label"/"input"/"select"/"checkbox"/"radio"/"slider"/"table"/"tree"/"canvas"）
        ctl_kind: HashMap<i64, String>,
        /// 控件 id → 原生句柄
        ctl_hwnd: HashMap<i64, HWND>,
        /// 控件 id → 附加数据（canvas 图形 / tree 节点表）
        ctl_data: HashMap<i64, CtlData>,
        /// 菜单 id → 菜单路径（"文件/打开"，用于菜单点击事件）
        menu_path: HashMap<i64, String>,
        /// 下一个菜单项 id（从 0x1000 起，避开控件 id 常用区）
        next_menu_id: i64,
        /// 当前窗口菜单句柄（替换/销毁时回收）
        menu_hmenu: Option<HMENU>,
        /// 托盘图标是否已添加（每窗口一个）
        tray_added: bool,
        /// 桌宠窗口状态（guipro.pet_window 创建的窗口才有）
        pet: Option<PetState>,
    }

    /// 桌宠窗口附加状态（pet_* 内置函数使用）。
    struct PetState {
        /// 帧源尺寸（Hone 侧传入，通常 48×48）
        fw: i32,
        fh: i32,
        /// 帧 RGB 缓冲（fw*fh*3；已按 flip 处理）
        frame: Vec<u8>,
        /// 气泡文本（"" 表示不显示）
        text: String,
        /// 拖拽状态
        dragging: bool,
        drag_moved: bool,
        drag_ox: i32,
        drag_oy: i32,
        drag_start_x: i32,
        drag_start_y: i32,
    }

    impl Clone for PetState {
        fn clone(&self) -> Self {
            PetState {
                fw: self.fw,
                fh: self.fh,
                frame: self.frame.clone(),
                text: self.text.clone(),
                dragging: false,
                drag_moved: false,
                drag_ox: 0,
                drag_oy: 0,
                drag_start_x: 0,
                drag_start_y: 0,
            }
        }
    }

    /// 控件附加数据（进阶控件专用）
    enum CtlData {
        /// canvas 图形指令列表
        Canvas(Vec<Shape>),
        /// tree 节点表
        Tree(TreeData),
    }

    /// canvas 绘图指令（kind: "line"/"rect"/"ellipse"/"text"）
    #[derive(Clone)]
    struct Shape {
        kind: &'static str,
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        text: String,
        /// 0xRRGGBB
        color: u32,
        /// 是否填充（rect/ellipse 有效）
        fill: bool,
    }

    /// tree 节点表：节点 id → HTREEITEM 句柄
    struct TreeData {
        nodes: HashMap<i64, HTREEITEM>,
        next_node: i64,
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

    /// 仅按控件句柄定位（WM_HSCROLL 等只带 lParam 句柄的通知）。
    fn find_ctl_by_hwnd_only(hctl: HWND) -> Option<(i64, String)> {
        let wins = WINDOWS.lock().unwrap();
        for (win_id, s) in wins.iter() {
            for (id, h) in s.ctl_hwnd.iter() {
                if *h == hctl {
                    if let Some(kind) = s.ctl_kind.get(id) {
                        return Some((*win_id, kind.clone()));
                    }
                }
            }
        }
        None
    }

    /// 由控件句柄反查控件 id（配合 find_ctl_by_hwnd_only 使用）。
    fn ctl_id_of(hctl: HWND) -> i64 {
        let wins = WINDOWS.lock().unwrap();
        for s in wins.values() {
            for (id, h) in s.ctl_hwnd.iter() {
                if *h == hctl {
                    return *id;
                }
            }
        }
        0
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
                // 菜单命令：lParam == 0（控件通知 lParam 为控件句柄）
                let ctl_id = LOWORD(wparam as DWORD) as i64;
                let code = HIWORD(wparam as DWORD) as WORD;
                let hctl = lparam as HWND;
                if lparam == 0 {
                    if let Some((win_id, path)) = find_menu_item(ctl_id) {
                        push_event(win_id, 0, "menu", &path);
                    }
                    return 0;
                }
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
            WM_NOTIFY => {
                // 表/树等公共控件通知：lParam 指向 NMHDR
                let nm = lparam as *const NMHDR;
                if nm.is_null() {
                    return DefWindowProcW(hwnd, msg, wparam, lparam);
                }
                unsafe {
                    let hctl = (*nm).hwndFrom;
                    let ncode = (*nm).code;
                    if ncode == LVN_ITEMCHANGED {
                        // 表：选中行变化（仅当选中有变化时推送）
                        let nmlv = lparam as *const NMLISTVIEW;
                        let new_sel = (*nmlv).uNewState & LVIS_SELECTED;
                        let old_sel = (*nmlv).uOldState & LVIS_SELECTED;
                        if new_sel != old_sel {
                            if let Some((win_id, _)) = find_ctl_by_hwnd_only(hctl) {
                                let ctl_id = ctl_id_of(hctl);
                                let sel = SendMessageW(hctl, LVM_GETNEXTITEM, !0 as WPARAM, LVNI_SELECTED) as i64;
                                push_event(win_id, ctl_id, "change", &sel.to_string());
                            }
                        }
                    } else if ncode == NM_DBLCLK {
                        // 表：双击行
                        if let Some((win_id, _)) = find_ctl_by_hwnd_only(hctl) {
                            let ctl_id = ctl_id_of(hctl);
                            let sel = SendMessageW(hctl, LVM_GETNEXTITEM, !0 as WPARAM, LVNI_SELECTED) as i64;
                            push_event(win_id, ctl_id, "click", &sel.to_string());
                        }
                    } else if ncode == TVN_SELCHANGEDW {
                        // 树：选中节点变化（itemNew.lParam 即节点 id）
                        let nmtv = lparam as *const NMTREEVIEWW;
                        if let Some((win_id, _)) = find_ctl_by_hwnd_only(hctl) {
                            let ctl_id = ctl_id_of(hctl);
                            let node_id = (*nmtv).itemNew.lParam;
                            push_event(win_id, ctl_id, "change", &node_id.to_string());
                        }
                    }
                }
                0
            }
            0x8001 => {
                // 托盘图标回调（WM_APP+1）：wparam = uID，lparam 低 16 位 = 鼠标消息
                let mouse = LOWORD(lparam as DWORD) as u32;
                if let Some(win_id) = find_win_by_hwnd(hwnd) {
                    match mouse {
                        WM_LBUTTONDOWN => push_event(win_id, 0, "tray", "left"),
                        WM_RBUTTONUP => push_event(win_id, 0, "tray", "right"),
                        WM_LBUTTONDBLCLK => push_event(win_id, 0, "tray", "double"),
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
            WM_HSCROLL | WM_VSCROLL => {
                // slider 拖动：lParam 为滑块句柄，取当前位置推送 change 事件
                let hctl = lparam as HWND;
                let code = LOWORD(wparam as DWORD) as WORD;
                if (code as usize) == TB_THUMBTRACK || (code as usize) == TB_ENDTRACK {
                    if let Some((win_id, kind)) = find_ctl_by_hwnd_only(hctl) {
                        if kind == "slider" {
                            let pos = unsafe { SendMessageW(hctl, TBM_GETPOS, 0, 0) };
                            push_event(win_id, ctl_id_of(hctl), "change", &pos.to_string());
                        }
                    }
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
            // ---------- 桌宠窗口（pet_*） ----------
            WM_PAINT => {
                if let Some(win_id) = find_win_by_hwnd(hwnd) {
                    let pet = { WINDOWS.lock().unwrap().get(&win_id).and_then(|w| w.pet.clone()) };
                    if let Some(p) = pet {
                        paint_pet(hwnd, &p);
                        return 0;
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_ERASEBKGND => {
                if let Some(win_id) = find_win_by_hwnd(hwnd) {
                    let is_pet = WINDOWS.lock().unwrap().get(&win_id).map_or(false, |w| w.pet.is_some());
                    if is_pet {
                        // 背景由 WM_PAINT 全量绘制，跳过擦除避免闪烁
                        return 1;
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_LBUTTONDOWN => {
                if let Some(win_id) = find_win_by_hwnd(hwnd) {
                    let is_pet = WINDOWS.lock().unwrap().get(&win_id).map_or(false, |w| w.pet.is_some());
                    if is_pet {
                        let mut cur: POINT = std::mem::zeroed();
                        let mut rc: RECT = std::mem::zeroed();
                        unsafe {
                            GetCursorPos(&mut cur);
                            GetWindowRect(hwnd, &mut rc);
                            SetCapture(hwnd);
                        }
                        let mut wins = WINDOWS.lock().unwrap();
                        if let Some(w) = wins.get_mut(&win_id) {
                            if let Some(p) = w.pet.as_mut() {
                                p.dragging = true;
                                p.drag_moved = false;
                                p.drag_ox = cur.x - rc.left;
                                p.drag_oy = cur.y - rc.top;
                                p.drag_start_x = cur.x;
                                p.drag_start_y = cur.y;
                            }
                        }
                        return 0;
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_MOUSEMOVE => {
                let win_id = find_win_by_hwnd(hwnd);
                let dragging = win_id.map_or(false, |id| {
                    WINDOWS.lock().unwrap().get(&id).map_or(false, |w| w.pet.as_ref().map_or(false, |p| p.dragging))
                });
                if dragging {
                    let win_id = win_id.unwrap();
                    let mut cur: POINT = std::mem::zeroed();
                    unsafe { GetCursorPos(&mut cur); }
                    let (ox, oy, sx, sy) = {
                        let wins = WINDOWS.lock().unwrap();
                        let p = wins.get(&win_id).and_then(|w| w.pet.as_ref()).unwrap();
                        (p.drag_ox, p.drag_oy, p.drag_start_x, p.drag_start_y)
                    };
                    let nx = cur.x - ox;
                    let ny = cur.y - oy;
                    if nx >= 0 && ny >= 0 {
                        unsafe {
                            SetWindowPos(hwnd, ptr::null_mut(), nx, ny, 0, 0,
                                         SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
                        }
                    }
                    // 相对拖拽起点位移超过 3px 才算拖拽（否则视为点击）
                    let mut wins = WINDOWS.lock().unwrap();
                    if let Some(w) = wins.get_mut(&win_id) {
                        if let Some(p) = w.pet.as_mut() {
                            if !p.drag_moved {
                                let dx = cur.x - sx;
                                let dy = cur.y - sy;
                                if dx * dx + dy * dy > 9 {
                                    p.drag_moved = true;
                                }
                            }
                        }
                    }
                    return 0;
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_LBUTTONUP => {
                if let Some(win_id) = find_win_by_hwnd(hwnd) {
                    let mut wins = WINDOWS.lock().unwrap();
                    if let Some(w) = wins.get_mut(&win_id) {
                        if let Some(p) = w.pet.as_mut() {
                            if p.dragging {
                                p.dragging = false;
                                let moved = p.drag_moved;
                                let mut rc: RECT = std::mem::zeroed();
                                unsafe {
                                    ReleaseCapture();
                                    GetWindowRect(hwnd, &mut rc);
                                }
                                if moved {
                                    push_event(win_id, 0, "drag", &format!("{},{}", rc.left, rc.top));
                                } else {
                                    push_event(win_id, 0, "click", "");
                                }
                                return 0;
                            }
                        }
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_LBUTTONDBLCLK => {
                if let Some(win_id) = find_win_by_hwnd(hwnd) {
                    let is_pet = WINDOWS.lock().unwrap().get(&win_id).map_or(false, |w| w.pet.is_some());
                    if is_pet {
                        push_event(win_id, 0, "dblclick", "");
                        return 0;
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_RBUTTONUP => {
                if let Some(win_id) = find_win_by_hwnd(hwnd) {
                    let is_pet = WINDOWS.lock().unwrap().get(&win_id).map_or(false, |w| w.pet.is_some());
                    if is_pet {
                        push_event(win_id, 0, "rclick", "");
                        return 0;
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_DESTROY => {
                // 从注册表移除（close 已推送事件，脚本侧据此收尾）
                if let Some(win_id) = find_win_by_hwnd(hwnd) {
                    let mut wins = WINDOWS.lock().unwrap();
                    if let Some(s) = wins.get(&win_id) {
                        if let Some(m) = s.menu_hmenu {
                            DestroyMenu(m);
                        }
                        // 进程退出前自动回收托盘图标
                        if s.tray_added {
                            let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
                            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as DWORD;
                            nid.hWnd = hwnd;
                            nid.uID = 1;
                            unsafe {
                                Shell_NotifyIconW(NIM_DELETE, &mut nid);
                            }
                        }
                    }
                    wins.remove(&win_id);
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
                // 注册公共控件类（slider/table/tree 等 comctl32 控件需要）
                let mut icc = INITCOMMONCONTROLSEX {
                    dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as DWORD,
                    dwICC: ICC_BAR_CLASSES | ICC_LISTVIEW_CLASSES | ICC_TREEVIEW_CLASSES
                        | ICC_PROGRESS_CLASS | ICC_TAB_CLASSES,
                };
                InitCommonControlsEx(&mut icc);
                let hinst = GetModuleHandleW(ptr::null());
                *HINST.lock().unwrap() = hinst as usize;
                let class_name = to_utf16("HoneGuiproWnd");
                let wc = WNDCLASSEXW {
                    cbSize: std::mem::size_of::<WNDCLASSEXW>() as UINT,
                    // CS_DBLCLKS：桌宠窗口需要 WM_LBUTTONDBLCLK（双击事件）
                    style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
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
                // canvas 子窗口类（白底，自绘图形）
                let canvas_class = to_utf16("HoneCanvasWnd");
                let wc2 = WNDCLASSEXW {
                    cbSize: std::mem::size_of::<WNDCLASSEXW>() as UINT,
                    style: CS_HREDRAW | CS_VREDRAW,
                    lpfnWndProc: Some(canvas_wnd_proc),
                    cbClsExtra: 0,
                    cbWndExtra: 0,
                    hInstance: hinst,
                    hIcon: ptr::null_mut(),
                    hCursor: LoadCursorW(ptr::null_mut(), IDC_ARROW),
                    hbrBackground: GetStockObject(WHITE_BRUSH as i32) as *mut c_void as HBRUSH,
                    lpszMenuName: ptr::null(),
                    lpszClassName: canvas_class.as_ptr(),
                    hIconSm: ptr::null_mut(),
                };
                // 类已注册（同进程重复）时返回 0，可忽略
                RegisterClassExW(&wc2);
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
            "slider" => (TRACKBAR_CLASS, TBS_HORZ | TBS_AUTOTICKS | WS_TABSTOP),
            "table" => (WC_LISTVIEW, LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS | WS_TABSTOP),
            "tree" => (WC_TREEVIEW, TVS_HASLINES | TVS_LINESATROOT | TVS_HASBUTTONS | TVS_SHOWSELALWAYS | WS_TABSTOP),
            "canvas" => ("HoneCanvasWnd", WS_BORDER),
            other => {
                return Err(zerr(
                    codes::TYPE_MISMATCH,
                    format!("unknown widget type `{}` (button/label/input/select/checkbox/radio/slider/table/tree/canvas)", other),
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
        // slider 初始化：范围（min/max，默认 0-100）与取值（value，默认 min）
        if kind == "slider" {
            let min = dict_int(d, "min", 0, span, file, src)? as i32;
            let max = dict_int(d, "max", 100, span, file, src)? as i32;
            let val = dict_int(d, "value", min as i64, span, file, src)? as i32;
            unsafe {
                // TBM_SETRANGE：lParam 低 16 位 = 最小值，高 16 位 = 最大值
                let range = ((max as u32) << 16) | (min as u32 & 0xFFFF);
                SendMessageW(hctl, TBM_SETRANGE, 1, range as LPARAM);
                SendMessageW(hctl, TBM_SETPOS, 1, val as LPARAM);
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
            "guipro.set_value" => {
                if args.len() != 3 {
                    return Err(arg_count(name, 3, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                let ctl = as_int(&args[1], 1, span, file, src)?;
                let val = as_int(&args[2], 2, span, file, src)?;
                set_value(win, ctl, val, span, file, src)
            }
            "guipro.get_value" => {
                if args.len() != 2 {
                    return Err(arg_count(name, 2, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                let ctl = as_int(&args[1], 1, span, file, src)?;
                get_value(win, ctl, span, file, src)
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
            // ---------- 表（ListView） ----------
            "guipro.table_add_row" => {
                if args.len() != 3 {
                    return Err(arg_count(name, 3, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                let ctl = as_int(&args[1], 1, span, file, src)?;
                table_add_row(win, ctl, &args[2], span, file, src)
            }
            "guipro.table_clear" => {
                if args.len() != 2 {
                    return Err(arg_count(name, 2, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                let ctl = as_int(&args[1], 1, span, file, src)?;
                table_clear(win, ctl, span, file, src)
            }
            "guipro.table_count" => {
                if args.len() != 2 {
                    return Err(arg_count(name, 2, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                let ctl = as_int(&args[1], 1, span, file, src)?;
                table_count(win, ctl, span, file, src)
            }
            "guipro.table_get" => {
                if args.len() != 2 {
                    return Err(arg_count(name, 2, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                let ctl = as_int(&args[1], 1, span, file, src)?;
                table_get(win, ctl, span, file, src)
            }
            "guipro.table_get_row" => {
                if args.len() != 3 {
                    return Err(arg_count(name, 3, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                let ctl = as_int(&args[1], 1, span, file, src)?;
                let row = as_int(&args[2], 2, span, file, src)?;
                table_get_row(win, ctl, row, span, file, src)
            }
            "guipro.table_set" => {
                if args.len() != 5 {
                    return Err(arg_count(name, 5, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                let ctl = as_int(&args[1], 1, span, file, src)?;
                let row = as_int(&args[2], 2, span, file, src)?;
                let col = as_int(&args[3], 3, span, file, src)?;
                let text = as_str(&args[4], 4, span, file, src)?;
                table_set(win, ctl, row, col, text, span, file, src)
            }
            // ---------- 树（TreeView） ----------
            "guipro.tree_add" => {
                if args.len() != 4 {
                    return Err(arg_count(name, 4, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                let ctl = as_int(&args[1], 1, span, file, src)?;
                let parent = as_int(&args[2], 2, span, file, src)?;
                let label = as_str(&args[3], 3, span, file, src)?;
                tree_add(win, ctl, parent, label, span, file, src)
            }
            "guipro.tree_clear" => {
                if args.len() != 2 {
                    return Err(arg_count(name, 2, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                let ctl = as_int(&args[1], 1, span, file, src)?;
                tree_clear(win, ctl, span, file, src)
            }
            "guipro.tree_get" => {
                if args.len() != 2 {
                    return Err(arg_count(name, 2, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                let ctl = as_int(&args[1], 1, span, file, src)?;
                tree_get(win, ctl, span, file, src)
            }
            // ---------- 画布（Canvas） ----------
            "guipro.canvas_clear" => {
                if args.len() != 2 {
                    return Err(arg_count(name, 2, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                let ctl = as_int(&args[1], 1, span, file, src)?;
                canvas_clear(win, ctl, span, file, src)
            }
            "guipro.canvas_line" => {
                if args.len() != 7 {
                    return Err(arg_count(name, 7, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                let ctl = as_int(&args[1], 1, span, file, src)?;
                let x1 = as_int(&args[2], 2, span, file, src)? as i32;
                let y1 = as_int(&args[3], 3, span, file, src)? as i32;
                let x2 = as_int(&args[4], 4, span, file, src)? as i32;
                let y2 = as_int(&args[5], 5, span, file, src)? as i32;
                let color = as_int(&args[6], 6, span, file, src)? as u32;
                canvas_push_shape(
                    win, ctl,
                    Shape { kind: "line", x1, y1, x2, y2, text: String::new(), color, fill: false },
                    span, file, src,
                )
            }
            "guipro.canvas_rect" => {
                if args.len() != 8 {
                    return Err(arg_count(name, 8, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                let ctl = as_int(&args[1], 1, span, file, src)?;
                let x = as_int(&args[2], 2, span, file, src)? as i32;
                let y = as_int(&args[3], 3, span, file, src)? as i32;
                let w = as_int(&args[4], 4, span, file, src)? as i32;
                let h = as_int(&args[5], 5, span, file, src)? as i32;
                let color = as_int(&args[6], 6, span, file, src)? as u32;
                let fill = as_int(&args[7], 7, span, file, src)? != 0;
                canvas_push_shape(
                    win, ctl,
                    Shape { kind: "rect", x1: x, y1: y, x2: x + w, y2: y + h, text: String::new(), color, fill },
                    span, file, src,
                )
            }
            "guipro.canvas_ellipse" => {
                if args.len() != 8 {
                    return Err(arg_count(name, 8, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                let ctl = as_int(&args[1], 1, span, file, src)?;
                let x = as_int(&args[2], 2, span, file, src)? as i32;
                let y = as_int(&args[3], 3, span, file, src)? as i32;
                let w = as_int(&args[4], 4, span, file, src)? as i32;
                let h = as_int(&args[5], 5, span, file, src)? as i32;
                let color = as_int(&args[6], 6, span, file, src)? as u32;
                let fill = as_int(&args[7], 7, span, file, src)? != 0;
                canvas_push_shape(
                    win, ctl,
                    Shape { kind: "ellipse", x1: x, y1: y, x2: x + w, y2: y + h, text: String::new(), color, fill },
                    span, file, src,
                )
            }
            "guipro.canvas_text" => {
                if args.len() != 6 {
                    return Err(arg_count(name, 6, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                let ctl = as_int(&args[1], 1, span, file, src)?;
                let x = as_int(&args[2], 2, span, file, src)? as i32;
                let y = as_int(&args[3], 3, span, file, src)? as i32;
                let text = as_str(&args[4], 4, span, file, src)?;
                let color = as_int(&args[5], 5, span, file, src)? as u32;
                canvas_push_shape(
                    win, ctl,
                    Shape { kind: "text", x1: x, y1: y, x2: 0, y2: 0, text: text.to_string(), color, fill: false },
                    span, file, src,
                )
            }
            "guipro.canvas_repaint" => {
                if args.len() != 2 {
                    return Err(arg_count(name, 2, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                let ctl = as_int(&args[1], 1, span, file, src)?;
                canvas_repaint(win, ctl, span, file, src)
            }
            // ---------- 托盘图标 ----------
            "guipro.tray_add" => {
                if args.len() != 2 {
                    return Err(arg_count(name, 2, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                let tip = as_str(&args[1], 1, span, file, src)?;
                tray_add(win, tip, span, file, src)
            }
            "guipro.tray_tip" => {
                if args.len() != 2 {
                    return Err(arg_count(name, 2, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                let tip = as_str(&args[1], 1, span, file, src)?;
                tray_tip(win, tip, span, file, src)
            }
            "guipro.tray_remove" => {
                if args.len() != 1 {
                    return Err(arg_count(name, 1, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                tray_remove(win, span, file, src)
            }
            // ---------- 菜单栏 ----------
            "guipro.menu" => {
                if args.len() != 2 {
                    return Err(arg_count(name, 2, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                menu(win, &args[1], span, file, src)
            }
            // ---------- 桌宠窗口（guipro.pet_*） ----------
            "guipro.pet_window" => {
                if args.len() != 3 {
                    return Err(arg_count(name, 3, args.len(), span, file, src));
                }
                let title = as_str(&args[0], 0, span, file, src)?;
                let w = as_int(&args[1], 1, span, file, src)?;
                let h = as_int(&args[2], 2, span, file, src)?;
                pet_window(title, w, h, span, file, src)
            }
            "guipro.pet_frame" => {
                if args.len() != 5 {
                    return Err(arg_count(name, 5, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                let w = as_int(&args[1], 1, span, file, src)?;
                let h = as_int(&args[2], 2, span, file, src)?;
                let rgb = as_str(&args[3], 3, span, file, src)?;
                let flip = as_int(&args[4], 4, span, file, src)?;
                pet_frame(win, w, h, rgb, flip, span, file, src)
            }
            "guipro.pet_text" => {
                if args.len() != 2 {
                    return Err(arg_count(name, 2, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                let text = as_str(&args[1], 1, span, file, src)?;
                pet_text(win, text, span, file, src)
            }
            "guipro.pet_move" => {
                if args.len() != 3 {
                    return Err(arg_count(name, 3, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                let x = as_int(&args[1], 1, span, file, src)?;
                let y = as_int(&args[2], 2, span, file, src)?;
                pet_move(win, x, y, span, file, src)
            }
            "guipro.pet_pos" => {
                if args.len() != 1 {
                    return Err(arg_count(name, 1, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                pet_pos(win, span, file, src)
            }
            "guipro.pet_cursor" => {
                if args.len() != 1 {
                    return Err(arg_count(name, 1, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                pet_cursor(win, span, file, src)
            }
            "guipro.pet_menu" => {
                if args.len() != 2 {
                    return Err(arg_count(name, 2, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                pet_menu(win, &args[1], span, file, src)
            }
            "guipro.pet_close" => {
                if args.len() != 1 {
                    return Err(arg_count(name, 1, args.len(), span, file, src));
                }
                let win = as_int(&args[0], 0, span, file, src)?;
                pet_close(win, span, file, src)
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
                ctl_data: HashMap::new(),
                menu_path: HashMap::new(),
                next_menu_id: 0x1000,
                menu_hmenu: None,
                tray_added: false,
                pet: None,
            },
        );
        unsafe {
            ShowWindow(hwnd, SW_SHOW);
            UpdateWindow(hwnd);
        }
        Ok(Value::Int(win_id))
    }

    /// 创建桌宠窗（guipro.pet_window）：无边框 + 置顶 + 任务栏隐藏 + 品红键透明 + 不抢焦点。
    /// 帧/气泡/移动/菜单/关闭分别用 pet_frame/pet_text/pet_move/pet_menu/pet_close 操作。
    fn pet_window(title: &str, w: i64, h: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        if w <= 0 || h <= 0 {
            return Err(zerr(
                codes::TYPE_MISMATCH,
                "`guipro.pet_window` requires positive width and height",
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
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
                to_utf16("HoneGuiproWnd").as_ptr(),
                to_utf16(title).as_ptr(),
                WS_POPUP | WS_VISIBLE,
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
        // 品红 (255,0,255) 颜色键透明（与 pet 帧背景一致）
        unsafe {
            SetLayeredWindowAttributes(hwnd, 0x00FF00FF, 0, LWA_COLORKEY);
        }
        WINDOWS.lock().unwrap().insert(
            win_id,
            WinState {
                hwnd,
                ctl_kind: HashMap::new(),
                ctl_hwnd: HashMap::new(),
                ctl_data: HashMap::new(),
                menu_path: HashMap::new(),
                next_menu_id: 0x1000,
                menu_hmenu: None,
                tray_added: false,
                pet: Some(PetState {
                    fw: 0,
                    fh: 0,
                    frame: Vec::new(),
                    text: String::new(),
                    dragging: false,
                    drag_moved: false,
                    drag_ox: 0,
                    drag_oy: 0,
                    drag_start_x: 0,
                    drag_start_y: 0,
                }),
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
                "widget dict requires a `type` field (button/label/input/select/checkbox/radio/slider/table/tree/canvas)",
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
        // 进阶控件初始化（表/树/画布）
        match kind.as_str() {
            "table" => init_table(hctl, d, span, file, src)?,
            "tree" => {
                let mut td = TreeData { nodes: HashMap::new(), next_node: 1 };
                if let Some(Value::List(items)) = dict_get(d, "items") {
                    for it in items {
                        insert_tree_node(hctl, TVI_ROOT, it, &mut td, span, file, src)?;
                    }
                }
                win.ctl_data.insert(ctl_id, CtlData::Tree(td));
            }
            "canvas" => {
                win.ctl_data.insert(ctl_id, CtlData::Canvas(Vec::new()));
            }
            _ => {}
        }
        win.ctl_kind.insert(ctl_id, kind);
        win.ctl_hwnd.insert(ctl_id, hctl);
        Ok(Value::Int(ctl_id))
    }

    // ---------- 进阶控件辅助（table/tree/canvas） ----------

    /// 初始化表：扩展样式 + 列头 + 初始行。
    fn init_table(hctl: HWND, d: &Value, span: Span, file: &str, src: &str) -> Result<(), ZError> {
        unsafe {
            SendMessageW(
                hctl,
                LVM_SETEXTENDEDLISTVIEWSTYLE,
                (LVS_EX_FULLROWSELECT | LVS_EX_GRIDLINES) as WPARAM,
                (LVS_EX_FULLROWSELECT | LVS_EX_GRIDLINES) as LPARAM,
            );
        }
        if let Some(Value::List(cols)) = dict_get(d, "columns") {
            let mut ci = 0;
            for c in cols {
                if let Value::Str(s) = c {
                    let ws = to_utf16(s);
                    let mut col = LVCOLUMNW {
                        mask: LVCF_TEXT | LVCF_WIDTH | LVCF_SUBITEM,
                        fmt: 0,
                        cx: 120,
                        pszText: ws.as_ptr() as LPWSTR,
                        cchTextMax: 0,
                        iSubItem: ci,
                        iImage: 0,
                        iOrder: 0,
                        cxMin: 0,
                        cxDefault: 0,
                        cxIdeal: 0,
                    };
                    unsafe {
                        SendMessageW(hctl, LVM_INSERTCOLUMNW, ci as WPARAM, &mut col as *mut LVCOLUMNW as LPARAM);
                    }
                    ci += 1;
                }
            }
        }
        if let Some(Value::List(rows)) = dict_get(d, "rows") {
            for r in rows {
                if let Value::List(cells) = r {
                    insert_table_row(hctl, cells);
                }
            }
        }
        let _ = (span, file, src);
        Ok(())
    }

    /// 追加一行（cells 为字符串列表）。
    fn insert_table_row(hctl: HWND, cells: &[Value]) {
        unsafe {
            let count = SendMessageW(hctl, LVM_GETITEMCOUNT, 0, 0) as i32;
            let mut col = 0;
            for c in cells {
                if let Value::Str(s) = c {
                    let ws = to_utf16(s);
                    let mut item = LVITEMW {
                        mask: LVIF_TEXT,
                        iItem: count,
                        iSubItem: col,
                        state: 0,
                        stateMask: 0,
                        pszText: ws.as_ptr() as LPWSTR,
                        cchTextMax: 0,
                        iImage: 0,
                        lParam: 0,
                        iIndent: 0,
                        iGroupId: 0,
                        cColumns: 0,
                        puColumns: ptr::null_mut(),
                        piColFmt: ptr::null_mut(),
                        iGroup: 0,
                    };
                    if col == 0 {
                        SendMessageW(hctl, LVM_INSERTITEMW, 0, &mut item as *mut LVITEMW as LPARAM);
                    } else {
                        SendMessageW(hctl, LVM_SETITEMTEXTW, 0, &mut item as *mut LVITEMW as LPARAM);
                    }
                }
                col += 1;
            }
        }
    }

    /// 递归插入树节点（parent 为 HTREEITEM 句柄；TVI_ROOT 表示根）。
    fn insert_tree_node(
        hctl: HWND,
        parent: HTREEITEM,
        d: &Value,
        td: &mut TreeData,
        span: Span,
        file: &str,
        src: &str,
    ) -> Result<i64, ZError> {
        let text = dict_str(d, "text", span, file, src)?;
        let node_id = td.next_node;
        td.next_node += 1;
        let ws = to_utf16(&text);
        let mut tv = TVINSERTSTRUCTW {
            hParent: parent,
            hInsertAfter: TVI_SORT,
            u: unsafe { std::mem::zeroed() },
        };
        unsafe {
            let item = tv.u.item_mut();
            item.mask = TVIF_TEXT | TVIF_PARAM;
            item.pszText = ws.as_ptr() as LPWSTR;
            item.lParam = node_id as LPARAM;
        }
        let hitem = unsafe { SendMessageW(hctl, TVM_INSERTITEMW, 0, &mut tv as *mut TVINSERTSTRUCTW as LPARAM) } as HTREEITEM;
        td.nodes.insert(node_id, hitem);
        if let Some(Value::List(children)) = dict_get(d, "items") {
            for ch in children {
                insert_tree_node(hctl, hitem, ch, td, span, file, src)?;
            }
        }
        Ok(node_id)
    }

    /// 菜单 id → (窗口 id, 菜单路径)。
    fn find_menu_item(id: i64) -> Option<(i64, String)> {
        let wins = WINDOWS.lock().unwrap();
        for (win_id, s) in wins.iter() {
            if let Some(path) = s.menu_path.get(&id) {
                return Some((*win_id, path.clone()));
            }
        }
        None
    }

    /// 由画布句柄取图形列表。
    fn get_canvas_shapes(hwnd: HWND) -> Option<Vec<Shape>> {
        let wins = WINDOWS.lock().unwrap();
        for (_, s) in wins.iter() {
            for (id, data) in s.ctl_data.iter() {
                if s.ctl_hwnd.get(id) == Some(&hwnd) {
                    if let CtlData::Canvas(shapes) = data {
                        return Some(shapes.clone());
                    }
                }
            }
        }
        None
    }

    /// 绘制画布图形列表。
    fn draw_shapes(hdc: HDC, shapes: &[Shape]) {
        unsafe {
            for sh in shapes {
                let pen = CreatePen(PS_SOLID as i32, 1, sh.color);
                let old_pen = SelectObject(hdc, pen as HGDIOBJ);
                let brush: HGDIOBJ = if sh.fill {
                    CreateSolidBrush(sh.color) as HGDIOBJ
                } else {
                    GetStockObject(NULL_BRUSH as i32)
                };
                let old_brush = SelectObject(hdc, brush);
                match sh.kind {
                    "line" => {
                        MoveToEx(hdc, sh.x1, sh.y1, ptr::null_mut());
                        LineTo(hdc, sh.x2, sh.y2);
                    }
                    "rect" => {
                        Rectangle(hdc, sh.x1, sh.y1, sh.x2, sh.y2);
                    }
                    "ellipse" => {
                        Ellipse(hdc, sh.x1, sh.y1, sh.x2, sh.y2);
                    }
                    "text" => {
                        SetTextColor(hdc, sh.color);
                        SetBkMode(hdc, TRANSPARENT as i32);
                        let ws = to_utf16(&sh.text);
                        TextOutW(hdc, sh.x1, sh.y1, ws.as_ptr(), sh.text.len() as i32);
                    }
                    _ => {}
                }
                SelectObject(hdc, old_pen);
                SelectObject(hdc, old_brush);
                DeleteObject(pen as HGDIOBJ);
                if sh.fill {
                    DeleteObject(brush);
                }
            }
        }
    }

    // ---------- 桌宠窗口绘制与帧数据处理 ----------

    /// 解析 "r g b r g b ..." 十进制 RGB 文本为字节缓冲（需恰好 need 个字节）。
    fn parse_rgb(s: &str, need: usize) -> Result<Vec<u8>, String> {
        let mut out: Vec<u8> = Vec::with_capacity(need);
        let mut cur: u32 = 0;
        let mut has = false;
        for b in s.bytes() {
            if b == b' ' || b == b'\n' || b == b'\t' || b == b'\r' {
                if has {
                    if cur > 255 {
                        return Err("frame RGB value out of range (0-255)".into());
                    }
                    out.push(cur as u8);
                    cur = 0;
                    has = false;
                    if out.len() == need {
                        break;
                    }
                }
            } else if b.is_ascii_digit() {
                cur = cur * 10 + (b - b'0') as u32;
                has = true;
            } else {
                return Err("invalid byte in frame RGB data (expect decimal digits)".into());
            }
        }
        if has {
            if cur > 255 {
                return Err("frame RGB value out of range (0-255)".into());
            }
            out.push(cur as u8);
        }
        if out.len() != need {
            return Err(format!("frame RGB data needs {} bytes, got {}", need, out.len()));
        }
        Ok(out)
    }

    /// 水平翻转 RGB 缓冲（行内左右镜像）。
    fn flip_rgb(mut buf: Vec<u8>, fw: i32, fh: i32) -> Vec<u8> {
        let w = fw.max(0) as usize;
        for y in 0..fh.max(0) as usize {
            let row = y * w;
            for x in 0..w / 2 {
                let a = (row + x) * 3;
                let b = (row + w - 1 - x) * 3;
                for k in 0..3 {
                    buf.swap(a + k, b + k);
                }
            }
        }
        buf
    }

    /// 桌宠窗 WM_PAINT：品红底 + 帧最近邻放大 + 顶部气泡文本。
    fn paint_pet(hwnd: HWND, pet: &PetState) {
        unsafe {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut rc);
            let cw = rc.right - rc.left;
            let ch = rc.bottom - rc.top;
            let bubble_h = 30;
            // 品红底（透明键）
            let bg = CreateSolidBrush(0x00FF00FF);
            FillRect(hdc, &rc, bg);
            DeleteObject(bg as HGDIOBJ);
            // 帧（最近邻放大到窗口下部区域）
            if !pet.frame.is_empty() && pet.fw > 0 && pet.fh > 0 && ch > bubble_h {
                let mut bmi: BITMAPINFO = std::mem::zeroed();
                bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
                bmi.bmiHeader.biWidth = pet.fw;
                bmi.bmiHeader.biHeight = -pet.fh; // 自顶向下
                bmi.bmiHeader.biPlanes = 1;
                bmi.bmiHeader.biBitCount = 24;
                bmi.bmiHeader.biCompression = BI_RGB;
                SetStretchBltMode(hdc, COLORONCOLOR);
                StretchDIBits(
                    hdc,
                    0, bubble_h, cw, ch - bubble_h,
                    0, 0, pet.fw, pet.fh,
                    pet.frame.as_ptr() as *const c_void,
                    &bmi,
                    DIB_RGB_COLORS,
                    SRCCOPY,
                );
            }
            // 气泡：白底圆角感矩形 + 单行文本（居中、超出省略）
            let text = pet.text.as_str();
            if !text.is_empty() {
                let brush = CreateSolidBrush(0x00FFFFFF);
                let mut br: RECT = RECT { left: 2, top: 2, right: cw - 2, bottom: bubble_h - 2 };
                if br.right > br.left && br.bottom > br.top {
                    FillRect(hdc, &br, brush);
                }
                DeleteObject(brush as HGDIOBJ);
                let font = CreateFontW(
                    -14, 0, 0, 0, FW_NORMAL, 0, 0, 0, DEFAULT_CHARSET,
                    OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY,
                    DEFAULT_PITCH | FF_DONTCARE, to_utf16("Microsoft YaHei").as_ptr(),
                );
                let old = SelectObject(hdc, font as HGDIOBJ);
                SetTextColor(hdc, 0x00000000);
                SetBkMode(hdc, TRANSPARENT as i32);
                let ws = to_utf16(text);
                let mut tr: RECT = RECT { left: 4, top: 2, right: cw - 4, bottom: bubble_h - 2 };
                DrawTextW(
                    hdc, ws.as_ptr(), text.len() as i32, &mut tr,
                    winapi::um::winuser::DT_SINGLELINE | winapi::um::winuser::DT_VCENTER
                        | winapi::um::winuser::DT_CENTER | winapi::um::winuser::DT_END_ELLIPSIS,
                );
                SelectObject(hdc, old);
                DeleteObject(font as HGDIOBJ);
            }
            EndPaint(hwnd, &ps);
        }
    }

    /// canvas 子窗口过程：WM_PAINT 自绘，WM_LBUTTONDOWN 推送点击事件。
    unsafe extern "system" fn canvas_wnd_proc(hwnd: HWND, msg: UINT, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        match msg {
            WM_PAINT => {
                let mut ps: PAINTSTRUCT = std::mem::zeroed();
                let hdc = BeginPaint(hwnd, &mut ps);
                if let Some(shapes) = get_canvas_shapes(hwnd) {
                    draw_shapes(hdc, &shapes);
                }
                EndPaint(hwnd, &ps);
                0
            }
            WM_LBUTTONDOWN => {
                let x = LOWORD(lparam as DWORD) as i32;
                let y = HIWORD(lparam as DWORD) as i32;
                if let Some((win_id, _)) = find_ctl_by_hwnd_only(hwnd) {
                    let ctl_id = ctl_id_of(hwnd);
                    // 推送 "[x,y]"（Hone 侧用 json_parse 解析）
                    push_event(win_id, ctl_id, "click", &format!("[{},{}]", x, y));
                }
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
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

    // ---------- 桌宠窗口（guipro.pet_*） ----------

    /// 取桌宠窗口句柄并校验其确为宠物窗。
    fn pet_hwnd(win_id: i64, span: Span, file: &str, src: &str) -> Result<HWND, ZError> {
        let wins = WINDOWS.lock().unwrap();
        let win = wins.get(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
        if win.pet.is_none() {
            return Err(zerr(
                codes::TYPE_MISMATCH,
                format!("window {} is not a pet window (create it with `guipro.pet_window`)", win_id),
                span,
                file,
                src,
                Some("check the window id"),
            ));
        }
        Ok(win.hwnd)
    }

    /// 推帧：RGB 十进制文本 → 缓冲并重绘；flip 非 0 时水平翻转。
    fn pet_frame(win_id: i64, w: i64, h: i64, rgb: &str, flip: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        if w <= 0 || h <= 0 {
            return Err(zerr(
                codes::TYPE_MISMATCH,
                "`guipro.pet_frame` requires positive frame width and height",
                span,
                file,
                src,
                Some("pass w > 0 and h > 0"),
            ));
        }
        let need = (w * h * 3) as usize;
        let buf = parse_rgb(rgb, need).map_err(|e| {
            zerr(
                codes::TYPE_MISMATCH,
                format!("`guipro.pet_frame` frame data error: {}", e),
                span,
                file,
                src,
                Some("frame data must be \"r g b r g b ...\" decimal text of w*h*3 values"),
            )
        })?;
        let buf = if flip != 0 { flip_rgb(buf, w as i32, h as i32) } else { buf };
        let hwnd = pet_hwnd(win_id, span, file, src)?;
        let mut wins = WINDOWS.lock().unwrap();
        if let Some(ws) = wins.get_mut(&win_id) {
            if let Some(p) = ws.pet.as_mut() {
                p.fw = w as i32;
                p.fh = h as i32;
                p.frame = buf;
            }
        }
        unsafe {
            InvalidateRect(hwnd, ptr::null_mut(), 1);
        }
        Ok(Value::Null)
    }

    /// 设置气泡文本（"" 清除）。
    fn pet_text(win_id: i64, text: &str, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        let hwnd = pet_hwnd(win_id, span, file, src)?;
        let mut wins = WINDOWS.lock().unwrap();
        if let Some(w) = wins.get_mut(&win_id) {
            if let Some(p) = w.pet.as_mut() {
                p.text = text.to_string();
            }
        }
        unsafe {
            InvalidateRect(hwnd, ptr::null_mut(), 1);
        }
        Ok(Value::Null)
    }

    /// 移动窗口到屏幕坐标 (x, y)。
    fn pet_move(win_id: i64, x: i64, y: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        let hwnd = pet_hwnd(win_id, span, file, src)?;
        unsafe {
            SetWindowPos(hwnd, ptr::null_mut(), x as i32, y as i32, 0, 0,
                         SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
        Ok(Value::Null)
    }

    /// 当前窗口位置 "x,y"。
    fn pet_pos(win_id: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        let hwnd = pet_hwnd(win_id, span, file, src)?;
        let mut rc: RECT = unsafe { std::mem::zeroed() };
        unsafe {
            GetWindowRect(hwnd, &mut rc);
        }
        Ok(Value::Str(format!("{},{}", rc.left, rc.top)))
    }

    /// 光标屏幕坐标 "x,y"（跟随鼠标模式用）。
    fn pet_cursor(_win_id: i64, _span: Span, _file: &str, _src: &str) -> Result<Value, ZError> {
        let mut p: POINT = unsafe { std::mem::zeroed() };
        unsafe {
            GetCursorPos(&mut p);
        }
        Ok(Value::Str(format!("{},{}", p.x, p.y)))
    }

    /// 弹出右键菜单（items 为字符串列表），返回选中项文本；未选择返回 ""。
    fn pet_menu(win_id: i64, items: &Value, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        let hwnd = pet_hwnd(win_id, span, file, src)?;
        let labels: Vec<String> = match items {
            Value::List(list) => {
                let mut out = Vec::with_capacity(list.len());
                for v in list {
                    match v {
                        Value::Str(s) => out.push(s.clone()),
                        other => {
                            return Err(zerr(
                                codes::TYPE_MISMATCH,
                                format!("`guipro.pet_menu` expects a list of strings, got item `{}`", other.type_name()),
                                span,
                                file,
                                src,
                                Some("pass a list of menu labels"),
                            ));
                        }
                    }
                }
                out
            }
            other => {
                return Err(zerr(
                    codes::TYPE_MISMATCH,
                    format!("`guipro.pet_menu` expects a list of strings, got `{}`", other.type_name()),
                    span,
                    file,
                    src,
                    Some("pass a list of menu labels"),
                ));
            }
        };
        if labels.is_empty() {
            return Ok(Value::Str(String::new()));
        }
        let hmenu = unsafe { CreatePopupMenu() };
        if hmenu.is_null() {
            return Err(win_err("CreatePopupMenu", span, file, src));
        }
        for (i, s) in labels.iter().enumerate() {
            unsafe {
                AppendMenuW(hmenu, MF_STRING, (0x1000 + i) as usize, to_utf16(s).as_ptr());
            }
        }
        let mut cur: POINT = unsafe { std::mem::zeroed() };
        unsafe {
            GetCursorPos(&mut cur);
        }
        let sel = unsafe {
            TrackPopupMenu(hmenu, TPM_RIGHTBUTTON | TPM_RETURNCMD, cur.x, cur.y, 0, hwnd, ptr::null_mut())
        };
        unsafe {
            DestroyMenu(hmenu);
        }
        if sel == 0 {
            return Ok(Value::Str(String::new()));
        }
        let idx = (sel - 0x1000) as usize;
        if idx < labels.len() {
            Ok(Value::Str(labels[idx].clone()))
        } else {
            Ok(Value::Str(String::new()))
        }
    }

    /// 销毁桌宠窗并推送 close 事件。
    fn pet_close(win_id: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        let hwnd = pet_hwnd(win_id, span, file, src)?;
        unsafe {
            DestroyWindow(hwnd);
        }
        push_event(win_id, 0, "close", "");
        Ok(Value::Null)
    }

    /// 设置控件数值（slider 用 TBM_SETPOS；其他控件不支持时报错）。
    fn set_value(win_id: i64, ctl_id: i64, val: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        let wins = WINDOWS.lock().unwrap();
        let win = wins.get(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
        let hctl = win.ctl_hwnd.get(&ctl_id).copied().ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
        let kind = win.ctl_kind.get(&ctl_id).map(|s| s.as_str()).unwrap_or("");
        match kind {
            "slider" => {
                unsafe {
                    SendMessageW(hctl, TBM_SETPOS, 1, val as LPARAM);
                }
                Ok(Value::Null)
            }
            _ => Err(zerr(
                codes::TYPE_MISMATCH,
                format!("widget type `{}` does not support set_value (slider only)", kind),
                span,
                file,
                src,
                Some("set_value works on slider widgets"),
            )),
        }
    }

    /// 读取控件数值（slider 用 TBM_GETPOS）。
    fn get_value(win_id: i64, ctl_id: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        let wins = WINDOWS.lock().unwrap();
        let win = wins.get(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
        let hctl = win.ctl_hwnd.get(&ctl_id).copied().ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
        let kind = win.ctl_kind.get(&ctl_id).map(|s| s.as_str()).unwrap_or("");
        match kind {
            "slider" => {
                let pos = unsafe { SendMessageW(hctl, TBM_GETPOS, 0, 0) };
                Ok(Value::Int(pos as i64))
            }
            _ => Err(zerr(
                codes::TYPE_MISMATCH,
                format!("widget type `{}` does not support get_value (slider only)", kind),
                span,
                file,
                src,
                Some("get_value works on slider widgets"),
            )),
        }
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

    // ---------- 表（ListView）实现 ----------

    fn table_add_row(win_id: i64, ctl_id: i64, row: &Value, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        let wins = WINDOWS.lock().unwrap();
        let win = wins.get(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
        let hctl = win.ctl_hwnd.get(&ctl_id).copied().ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
        match row {
            Value::List(cells) => insert_table_row(hctl, cells),
            _ => {
                return Err(zerr(
                    codes::TYPE_MISMATCH,
                    "table row must be a list of strings",
                    span,
                    file,
                    src,
                    Some("pass e.g. [\"a\", \"b\"]"),
                ));
            }
        }
        Ok(Value::Null)
    }

    fn table_clear(win_id: i64, ctl_id: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        let wins = WINDOWS.lock().unwrap();
        let win = wins.get(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
        let hctl = win.ctl_hwnd.get(&ctl_id).copied().ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
        unsafe {
            SendMessageW(hctl, LVM_DELETEALLITEMS, 0, 0);
        }
        Ok(Value::Null)
    }

    fn table_count(win_id: i64, ctl_id: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        let wins = WINDOWS.lock().unwrap();
        let win = wins.get(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
        let hctl = win.ctl_hwnd.get(&ctl_id).copied().ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
        let count = unsafe { SendMessageW(hctl, LVM_GETITEMCOUNT, 0, 0) } as i64;
        Ok(Value::Int(count))
    }

    /// 选中行索引（无选中返回 -1）。
    fn table_get(win_id: i64, ctl_id: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        let wins = WINDOWS.lock().unwrap();
        let win = wins.get(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
        let hctl = win.ctl_hwnd.get(&ctl_id).copied().ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
        let sel = unsafe { SendMessageW(hctl, LVM_GETNEXTITEM, !0 as WPARAM, LVNI_SELECTED) } as i64;
        Ok(Value::Int(sel))
    }

    /// 读取某行全部单元格（返回字符串列表）。
    fn table_get_row(win_id: i64, ctl_id: i64, row: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        let wins = WINDOWS.lock().unwrap();
        let win = wins.get(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
        let hctl = win.ctl_hwnd.get(&ctl_id).copied().ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
        let count = unsafe { SendMessageW(hctl, LVM_GETITEMCOUNT, 0, 0) } as i64;
        if row < 0 || row >= count {
            return Err(zerr(
                codes::NOT_FOUND,
                format!("table row {} out of range (0..{})", row, count),
                span,
                file,
                src,
                Some("check the row index"),
            ));
        }
        let mut cells = Vec::new();
        for col in 0..256i64 {
            let mut buf = [0u16; 512];
            let mut item = LVITEMW {
                mask: LVIF_TEXT,
                iItem: row as c_int,
                iSubItem: col as c_int,
                state: 0,
                stateMask: 0,
                pszText: buf.as_mut_ptr(),
                cchTextMax: buf.len() as c_int,
                iImage: 0,
                lParam: 0,
                iIndent: 0,
                iGroupId: 0,
                cColumns: 0,
                puColumns: ptr::null_mut(),
                piColFmt: ptr::null_mut(),
                iGroup: 0,
            };
            let r = unsafe { SendMessageW(hctl, LVM_GETITEMW, 0, &mut item as *mut LVITEMW as LPARAM) };
            if r == 0 {
                break;
            }
            let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            cells.push(Value::Str(String::from_utf16_lossy(&buf[..len])));
        }
        Ok(Value::List(cells))
    }

    fn table_set(win_id: i64, ctl_id: i64, row: i64, col: i64, text: &str, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        let wins = WINDOWS.lock().unwrap();
        let win = wins.get(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
        let hctl = win.ctl_hwnd.get(&ctl_id).copied().ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
        let ws = to_utf16(text);
        let mut item = LVITEMW {
            mask: LVIF_TEXT,
            iItem: row as c_int,
            iSubItem: col as c_int,
            state: 0,
            stateMask: 0,
            pszText: ws.as_ptr() as LPWSTR,
            cchTextMax: 0,
            iImage: 0,
            lParam: 0,
            iIndent: 0,
            iGroupId: 0,
            cColumns: 0,
            puColumns: ptr::null_mut(),
            piColFmt: ptr::null_mut(),
            iGroup: 0,
        };
        unsafe {
            SendMessageW(hctl, LVM_SETITEMTEXTW, 0, &mut item as *mut LVITEMW as LPARAM);
        }
        Ok(Value::Null)
    }

    // ---------- 树（TreeView）实现 ----------

    /// 添加节点（parent_id = 0 表示根），返回新节点 id。
    fn tree_add(win_id: i64, ctl_id: i64, parent_id: i64, label: &str, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        let mut wins = WINDOWS.lock().unwrap();
        let win = wins.get_mut(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
        let hctl = win.ctl_hwnd.get(&ctl_id).copied().ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
        let td = match win.ctl_data.get_mut(&ctl_id) {
            Some(CtlData::Tree(td)) => td,
            _ => {
                return Err(zerr(
                    codes::TYPE_MISMATCH,
                    format!("widget {} is not a tree", ctl_id),
                    span,
                    file,
                    src,
                    Some("create it with guipro_tree"),
                ));
            }
        };
        let parent = if parent_id == 0 {
            TVI_ROOT
        } else {
            match td.nodes.get(&parent_id) {
                Some(h) => *h,
                None => {
                    return Err(zerr(
                        codes::NOT_FOUND,
                        format!("tree parent node {} does not exist", parent_id),
                        span,
                        file,
                        src,
                        Some("check the parent node id"),
                    ));
                }
            }
        };
        let node_id = td.next_node;
        td.next_node += 1;
        let ws = to_utf16(label);
        let mut tv = TVINSERTSTRUCTW {
            hParent: parent,
            hInsertAfter: TVI_SORT,
            u: unsafe { std::mem::zeroed() },
        };
        unsafe {
            let item = tv.u.item_mut();
            item.mask = TVIF_TEXT | TVIF_PARAM;
            item.pszText = ws.as_ptr() as LPWSTR;
            item.lParam = node_id as LPARAM;
        }
        let hitem = unsafe { SendMessageW(hctl, TVM_INSERTITEMW, 0, &mut tv as *mut TVINSERTSTRUCTW as LPARAM) } as HTREEITEM;
        td.nodes.insert(node_id, hitem);
        Ok(Value::Int(node_id))
    }

    fn tree_clear(win_id: i64, ctl_id: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        let mut wins = WINDOWS.lock().unwrap();
        let win = wins.get_mut(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
        let hctl = win.ctl_hwnd.get(&ctl_id).copied().ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
        unsafe {
            SendMessageW(hctl, TVM_DELETEITEM, 0, TVI_ROOT as LPARAM);
        }
        if let Some(CtlData::Tree(td)) = win.ctl_data.get_mut(&ctl_id) {
            td.nodes.clear();
            td.next_node = 1;
        }
        Ok(Value::Null)
    }

    /// 选中节点 id（无选中返回 -1）。
    fn tree_get(win_id: i64, ctl_id: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        let wins = WINDOWS.lock().unwrap();
        let win = wins.get(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
        let hctl = win.ctl_hwnd.get(&ctl_id).copied().ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
        let hsel = unsafe { SendMessageW(hctl, TVM_GETNEXTITEM, TVGN_CARET, 0) } as HTREEITEM;
        if hsel.is_null() {
            return Ok(Value::Int(-1));
        }
        if let Some(CtlData::Tree(td)) = win.ctl_data.get(&ctl_id) {
            for (id, h) in td.nodes.iter() {
                if *h == hsel {
                    return Ok(Value::Int(*id));
                }
            }
        }
        Ok(Value::Int(-1))
    }

    // ---------- 画布（Canvas）实现 ----------

    /// 向画布追加图形并重绘。
    fn canvas_push_shape(win_id: i64, ctl_id: i64, shape: Shape, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        let mut wins = WINDOWS.lock().unwrap();
        let win = wins.get_mut(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
        let hctl = win.ctl_hwnd.get(&ctl_id).copied().ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
        match win.ctl_data.get_mut(&ctl_id) {
            Some(CtlData::Canvas(shapes)) => shapes.push(shape),
            _ => {
                return Err(zerr(
                    codes::TYPE_MISMATCH,
                    format!("widget {} is not a canvas", ctl_id),
                    span,
                    file,
                    src,
                    Some("create it with guipro_canvas"),
                ));
            }
        }
        unsafe {
            InvalidateRect(hctl, ptr::null(), 1);
        }
        Ok(Value::Null)
    }

    fn canvas_clear(win_id: i64, ctl_id: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        let mut wins = WINDOWS.lock().unwrap();
        let win = wins.get_mut(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
        let hctl = win.ctl_hwnd.get(&ctl_id).copied().ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
        if let Some(CtlData::Canvas(shapes)) = win.ctl_data.get_mut(&ctl_id) {
            shapes.clear();
        }
        unsafe {
            InvalidateRect(hctl, ptr::null(), 1);
        }
        Ok(Value::Null)
    }

    fn canvas_repaint(win_id: i64, ctl_id: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        let wins = WINDOWS.lock().unwrap();
        let win = wins.get(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
        let hctl = win.ctl_hwnd.get(&ctl_id).copied().ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
        unsafe {
            InvalidateRect(hctl, ptr::null(), 1);
        }
        Ok(Value::Null)
    }

    // ---------- 托盘图标实现 ----------

    /// 底层 Shell_NotifyIconW 调用（action: NIM_ADD / NIM_MODIFY / NIM_DELETE）。
    fn tray_notify(win: &WinState, tip: &str, action: DWORD) {
        let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as DWORD;
        nid.hWnd = win.hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = 0x8001;
        nid.hIcon = unsafe { LoadIconW(ptr::null_mut(), IDI_APPLICATION) };
        let tip16: Vec<u16> = tip.encode_utf16().chain(std::iter::once(0)).collect();
        for (i, c) in tip16.iter().enumerate() {
            if i >= 127 {
                break;
            }
            nid.szTip[i] = *c;
        }
        unsafe {
            Shell_NotifyIconW(action, &mut nid);
        }
    }

    fn tray_add(win_id: i64, tip: &str, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        let mut wins = WINDOWS.lock().unwrap();
        let win = wins.get_mut(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
        tray_notify(win, tip, NIM_ADD);
        win.tray_added = true;
        Ok(Value::Null)
    }

    fn tray_tip(win_id: i64, tip: &str, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        let wins = WINDOWS.lock().unwrap();
        let win = wins.get(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
        tray_notify(win, tip, NIM_MODIFY);
        Ok(Value::Null)
    }

    fn tray_remove(win_id: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        let mut wins = WINDOWS.lock().unwrap();
        let win = wins.get_mut(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
        tray_notify(win, "", NIM_DELETE);
        win.tray_added = false;
        Ok(Value::Null)
    }

    // ---------- 菜单栏实现 ----------

    /// 递归构建菜单（items 元素：{"text": "..", "items": [子项]}；text 为 "-" 表示分隔线）。
    /// 叶子项自动分配 id 并记录路径（parent_path + "/" + text）到 win.menu_path。
    fn build_menu(win: &mut WinState, items: &Value, parent_path: &str, span: Span, file: &str, src: &str) -> Result<HMENU, ZError> {
        let list = match items {
            Value::List(l) => l,
            _ => {
                return Err(zerr(
                    codes::TYPE_MISMATCH,
                    "menu items must be a list of dicts",
                    span,
                    file,
                    src,
                    Some("pass e.g. [{\"text\": \"文件\", \"items\": [...]}]"),
                ));
            }
        };
        let menu = unsafe { CreateMenu() };
        for item in list {
            let text = dict_str(item, "text", span, file, src)?;
            if text == "-" {
                unsafe {
                    AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
                }
                continue;
            }
            // 有子项 → 弹出子菜单；否则 → 叶子菜单项
            if dict_get(item, "items").is_some() {
                let sub = build_menu(win, item, "", span, file, src)?;
                let ws = to_utf16(&text);
                unsafe {
                    AppendMenuW(menu, MF_POPUP, sub as usize, ws.as_ptr());
                }
            } else {
                let id = win.next_menu_id;
                win.next_menu_id += 1;
                let path = if parent_path.is_empty() {
                    text.clone()
                } else {
                    format!("{}/{}", parent_path, text)
                };
                win.menu_path.insert(id, path);
                let ws = to_utf16(&text);
                unsafe {
                    AppendMenuW(menu, MF_STRING, id as usize, ws.as_ptr());
                }
            }
        }
        Ok(menu)
    }

    /// 构建菜单栏并挂到窗口（替换旧菜单）。
    fn menu(win_id: i64, items: &Value, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
        let mut wins = WINDOWS.lock().unwrap();
        let win = wins.get_mut(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
        let m = build_menu(win, items, "", span, file, src)?;
        unsafe {
            SetMenu(win.hwnd, m);
            DrawMenuBar(win.hwnd);
        }
        // 回收旧菜单句柄
        if let Some(old) = win.menu_hmenu {
            unsafe {
                DestroyMenu(old);
            }
        }
        win.menu_hmenu = Some(m);
        Ok(Value::Null)
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
