// guimod_x11.rs - guipro 的 Linux X11 自绘后端（libloading 动态加载 libX11）
//
// 设计：与 guimod_gtk.rs 相同的「运行时动态加载」模式——不链接任何 X11 静态库，
// 通过 libloading 加载 libX11.so.6 并按 C ABI transmute 函数指针。因此本模块
// 不依赖 Linux 系统头文件，可在任意平台编译（本机 Windows 亦可 cargo check）。
// 运行时要求：Linux 有 X11 显示（DISPLAY 环境变量）且已安装 libX11.so.6。
//
// 自绘模型：主窗口单窗口自绘——控件是窗口内的矩形区域（x/y/w/h），整窗重绘；
// 事件（Expose/ButtonPress/KeyPress/ConfigureNotify/ClientMessage）由 poll()
// 泵出后按坐标命中控件。select 下拉与菜单用 override-redirect 弹窗。
//
// 与 Win32/GTK 后端对齐的接口（guipro.*，Hone 层 guipro.hn 已统一封装）：
//   guipro.available()   -> bool
//   guipro.window(t,w,h) -> int
//   guipro.add(win,dict) -> int
//   guipro.poll()        -> str
//   guipro.set_text / get_text / set_value / get_value / close / msgbox
//   guipro.table_* / tree_* / canvas_* / menu / tray_*（XEmbed 系统托盘）
//
// 文本渲染：优先 XCreateFontSet + XmbDrawString（UTF-8，需 setlocale(LC_CTYPE,"")），
// 失败则退回 XLoadQueryFont("fixed") + XDrawString（仅 Latin-1，度量按固定字体近似）。

use crate::error::codes;
use crate::error::ZError;
use crate::interp::Value;
use crate::lexer::Span;
use libloading::Library;
use std::collections::HashMap;
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_long, c_short, c_uchar, c_uint, c_ulong, c_ushort, c_void};
use std::ptr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;

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

// ---------- dict 辅助（与 guimod.rs 相同约定） ----------

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

// ---------- X11 基础类型（不透明句柄用 usize 存，规避裸指针 Send 限制） ----------

type XDisplay = c_void;   // Display*
type XGC = c_void;        // GC
type XWindow = c_ulong;   // Window (XID)
type XAtom = c_ulong;
type XColormap = c_ulong;
type XKeySym = c_ulong;

/// XColor（XAllocColor 用）
#[repr(C)]
struct XColor {
    pixel: c_ulong,
    red: c_ushort,
    green: c_ushort,
    blue: c_ushort,
    flags: c_char,
    pad: c_char,
}

/// XRectangle（XmbTextExtents / XFontSetExtents 用）
#[repr(C)]
struct XRectangle {
    x: c_short,
    y: c_short,
    width: c_ushort,
    height: c_ushort,
}

#[repr(C)]
struct XFontSetExtents {
    max_ink_extent: XRectangle,
    max_logical_extent: XRectangle,
}

/// XSetWindowAttributes（XChangeWindowAttributes 用，仅需 override_redirect）
#[repr(C)]
struct XSetWindowAttributes {
    background_pixmap: c_ulong,
    background_pixel: c_ulong,
    border_pixmap: c_ulong,
    border_pixel: c_ulong,
    bit_gravity: c_int,
    win_gravity: c_int,
    backing_store: c_int,
    backing_planes: c_ulong,
    backing_pixel: c_ulong,
    save_under: c_int,
    event_mask: c_long,
    do_not_propagate_mask: c_long,
    override_redirect: c_int,
    colormap: c_ulong,
    cursor: c_ulong,
}

/// XFontStruct 最小布局（仅取到 ascent/descent，供固定字体回退路径度量）
#[repr(C)]
struct XFontStruct {
    ext_data: *mut c_void,
    fid: c_ulong,
    direction: c_uint,
    min_char_or_byte2: c_uint,
    max_char_or_byte2: c_uint,
    min_byte1: c_uint,
    max_byte1: c_uint,
    all_chars_exist: c_int,
    default_char: c_int,
    n_properties: c_int,
    properties: *mut c_void,
    min_bounds: XCharStruct,
    max_bounds: XCharStruct,
    per_char: *mut c_void,
    ascent: c_int,
    descent: c_int,
}

#[repr(C)]
struct XCharStruct {
    lbearing: c_short,
    rbearing: c_short,
    width: c_short,
    ascent: c_short,
    descent: c_short,
    attributes: c_ushort,
}

// ---------- XEvent 结构（仅定义用到的成员，布局与 Xlib.h 一致） ----------

#[derive(Clone, Copy)]
#[repr(C)]
struct XAnyEvent {
    type_: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut XDisplay,
    window: XWindow,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct XButtonEvent {
    type_: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut XDisplay,
    window: XWindow,
    root: XWindow,
    subwindow: XWindow,
    time: c_ulong,
    x: c_int,
    y: c_int,
    x_root: c_int,
    y_root: c_int,
    state: c_uint,
    button: c_uint,
    same_screen: c_int,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct XKeyEvent {
    type_: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut XDisplay,
    window: XWindow,
    root: XWindow,
    subwindow: XWindow,
    time: c_ulong,
    x: c_int,
    y: c_int,
    x_root: c_int,
    y_root: c_int,
    state: c_uint,
    keycode: c_uint,
    same_screen: c_int,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct XExposeEvent {
    type_: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut XDisplay,
    window: XWindow,
    x: c_int,
    y: c_int,
    width: c_int,
    height: c_int,
    count: c_int,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct XConfigureEvent {
    type_: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut XDisplay,
    event: XWindow,
    window: XWindow,
    x: c_int,
    y: c_int,
    width: c_int,
    height: c_int,
    border_width: c_int,
    above: XWindow,
    override_redirect: c_int,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct XClientMessageEvent {
    type_: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut XDisplay,
    window: XWindow,
    message_type: XAtom,
    format: c_int,
    data: [c_long; 5],
}

#[derive(Clone, Copy)]
#[repr(C)]
struct XFocusChangeEvent {
    type_: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut XDisplay,
    window: XWindow,
    mode: c_int,
    detail: c_int,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct XDestroyWindowEvent {
    type_: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut XDisplay,
    event: XWindow,
    window: XWindow,
}

#[repr(C)]
union XEvent {
    type_: c_int,
    xany: XAnyEvent,
    xbutton: XButtonEvent,
    xkey: XKeyEvent,
    xexpose: XExposeEvent,
    xconfigure: XConfigureEvent,
    xclient: XClientMessageEvent,
    xfocus: XFocusChangeEvent,
    xdestroy: XDestroyWindowEvent,
}

// ---------- X11 C ABI 函数指针表 ----------

type XOpenDisplayFn = unsafe extern "C" fn(*const c_char) -> *mut XDisplay;
type XCloseDisplayFn = unsafe extern "C" fn(*mut XDisplay) -> c_int;
type XDefaultScreenFn = unsafe extern "C" fn(*mut XDisplay) -> c_int;
type XDefaultRootWindowFn = unsafe extern "C" fn(*mut XDisplay) -> XWindow;
type XBlackPixelFn = unsafe extern "C" fn(*mut XDisplay, c_int) -> c_ulong;
type XWhitePixelFn = unsafe extern "C" fn(*mut XDisplay, c_int) -> c_ulong;
type XDefaultColormapFn = unsafe extern "C" fn(*mut XDisplay, c_int) -> XColormap;
type XCreateSimpleWindowFn = unsafe extern "C" fn(*mut XDisplay, XWindow, c_int, c_int, c_uint, c_uint, c_uint, c_ulong, c_ulong) -> XWindow;
type XDestroyWindowFn = unsafe extern "C" fn(*mut XDisplay, XWindow) -> c_int;
type XChangeWindowAttributesFn = unsafe extern "C" fn(*mut XDisplay, XWindow, c_ulong, *mut XSetWindowAttributes) -> c_int;
type XMapWindowFn = unsafe extern "C" fn(*mut XDisplay, XWindow) -> c_int;
type XMapRaisedFn = unsafe extern "C" fn(*mut XDisplay, XWindow) -> c_int;
type XStoreNameFn = unsafe extern "C" fn(*mut XDisplay, XWindow, *const c_char) -> c_int;
type XSelectInputFn = unsafe extern "C" fn(*mut XDisplay, XWindow, c_long) -> c_int;
type XMoveResizeWindowFn = unsafe extern "C" fn(*mut XDisplay, XWindow, c_int, c_int, c_uint, c_uint) -> c_int;
type XClearWindowFn = unsafe extern "C" fn(*mut XDisplay, XWindow) -> c_int;
type XFlushFn = unsafe extern "C" fn(*mut XDisplay) -> c_int;
type XSyncFn = unsafe extern "C" fn(*mut XDisplay, c_int) -> c_int;
type XCreateGCFn = unsafe extern "C" fn(*mut XDisplay, XWindow, c_ulong, *mut c_void) -> *mut XGC;
type XFreeGCFn = unsafe extern "C" fn(*mut XDisplay, *mut XGC) -> c_int;
type XSetForegroundFn = unsafe extern "C" fn(*mut XDisplay, *mut XGC, c_ulong) -> c_int;
type XSetBackgroundFn = unsafe extern "C" fn(*mut XDisplay, *mut XGC, c_ulong) -> c_int;
type XSetFontFn = unsafe extern "C" fn(*mut XDisplay, *mut XGC, c_ulong) -> c_int;
type XDrawStringFn = unsafe extern "C" fn(*mut XDisplay, XWindow, *mut XGC, c_int, c_int, *const c_char, c_int) -> c_int;
type XDrawLineFn = unsafe extern "C" fn(*mut XDisplay, XWindow, *mut XGC, c_int, c_int, c_int, c_int) -> c_int;
type XDrawRectangleFn = unsafe extern "C" fn(*mut XDisplay, XWindow, *mut XGC, c_int, c_int, c_uint, c_uint) -> c_int;
type XFillRectangleFn = unsafe extern "C" fn(*mut XDisplay, XWindow, *mut XGC, c_int, c_int, c_uint, c_uint) -> c_int;
type XDrawArcFn = unsafe extern "C" fn(*mut XDisplay, XWindow, *mut XGC, c_int, c_int, c_uint, c_uint, c_int, c_int) -> c_int;
type XFillArcFn = unsafe extern "C" fn(*mut XDisplay, XWindow, *mut XGC, c_int, c_int, c_uint, c_uint, c_int, c_int) -> c_int;
type XLoadQueryFontFn = unsafe extern "C" fn(*mut XDisplay, *const c_char) -> *mut XFontStruct;
type XFreeFontFn = unsafe extern "C" fn(*mut XDisplay, *mut XFontStruct) -> c_int;
type XInternAtomFn = unsafe extern "C" fn(*mut XDisplay, *const c_char, c_int) -> XAtom;
type XGetSelectionOwnerFn = unsafe extern "C" fn(*mut XDisplay, XAtom) -> XWindow;
type XSendEventFn = unsafe extern "C" fn(*mut XDisplay, XWindow, c_int, c_long, *mut XEvent) -> c_int;
type XChangePropertyFn = unsafe extern "C" fn(*mut XDisplay, XWindow, XAtom, XAtom, c_int, c_int, *const c_uchar, c_int) -> c_int;
type XSetWMProtocolsFn = unsafe extern "C" fn(*mut XDisplay, XWindow, *const XAtom, c_int) -> c_int;
type XSetInputFocusFn = unsafe extern "C" fn(*mut XDisplay, XWindow, c_int, c_ulong) -> c_int;
type XLookupStringFn = unsafe extern "C" fn(*const XKeyEvent, *mut c_char, c_int, *mut XKeySym, *mut c_void) -> c_int;
type XPendingFn = unsafe extern "C" fn(*mut XDisplay) -> c_int;
type XNextEventFn = unsafe extern "C" fn(*mut XDisplay, *mut XEvent) -> c_int;
type XTranslateCoordinatesFn = unsafe extern "C" fn(*mut XDisplay, XWindow, XWindow, c_int, c_int, *mut c_int, *mut c_int, *mut XWindow) -> c_int;
type XAllocColorFn = unsafe extern "C" fn(*mut XDisplay, XColormap, *mut XColor) -> c_int;
type XCreateFontSetFn = unsafe extern "C" fn(*mut XDisplay, *const c_char, *mut *mut c_char, *mut c_int, *mut *mut c_char) -> *mut c_void;
type XFreeFontSetFn = unsafe extern "C" fn(*mut XDisplay, *mut c_void) -> c_int;
type XmbDrawStringFn = unsafe extern "C" fn(*mut XDisplay, XWindow, *mut c_void, *mut XGC, c_int, c_int, *const c_char, c_int) -> c_int;
type XmbTextExtentsFn = unsafe extern "C" fn(*mut c_void, *const c_char, c_int, *mut XRectangle, *mut XRectangle) -> c_int;
type XFontSetExtentsFn = unsafe extern "C" fn(*mut c_void) -> *const XFontSetExtents;

struct X11Api {
    _lib: Library,
    open_display: XOpenDisplayFn,
    close_display: XCloseDisplayFn,
    default_screen: XDefaultScreenFn,
    default_root_window: XDefaultRootWindowFn,
    black_pixel: XBlackPixelFn,
    white_pixel: XWhitePixelFn,
    default_colormap: XDefaultColormapFn,
    create_simple_window: XCreateSimpleWindowFn,
    destroy_window: XDestroyWindowFn,
    change_window_attributes: XChangeWindowAttributesFn,
    map_window: XMapWindowFn,
    map_raised: XMapRaisedFn,
    store_name: XStoreNameFn,
    select_input: XSelectInputFn,
    move_resize_window: XMoveResizeWindowFn,
    clear_window: XClearWindowFn,
    flush: XFlushFn,
    sync: XSyncFn,
    create_gc: XCreateGCFn,
    free_gc: XFreeGCFn,
    set_foreground: XSetForegroundFn,
    set_background: XSetBackgroundFn,
    set_font: XSetFontFn,
    draw_string: XDrawStringFn,
    draw_line: XDrawLineFn,
    draw_rectangle: XDrawRectangleFn,
    fill_rectangle: XFillRectangleFn,
    draw_arc: XDrawArcFn,
    fill_arc: XFillArcFn,
    load_query_font: XLoadQueryFontFn,
    free_font: XFreeFontFn,
    intern_atom: XInternAtomFn,
    get_selection_owner: XGetSelectionOwnerFn,
    send_event: XSendEventFn,
    change_property: XChangePropertyFn,
    set_wm_protocols: XSetWMProtocolsFn,
    set_input_focus: XSetInputFocusFn,
    lookup_string: XLookupStringFn,
    pending: XPendingFn,
    next_event: XNextEventFn,
    translate_coordinates: XTranslateCoordinatesFn,
    alloc_color: XAllocColorFn,
    create_font_set: XCreateFontSetFn,
    free_font_set: XFreeFontSetFn,
    mb_draw_string: XmbDrawStringFn,
    mb_text_extents: XmbTextExtentsFn,
    font_set_extents: XFontSetExtentsFn,
}

/// 加载 libX11（候选名依次尝试）。
fn load_api() -> Result<X11Api, String> {
    let candidates: &[&str] = &["libX11.so.6", "libX11.so"];
    let mut last_err = String::from("no libX11 found");
    for name in candidates {
        let lib = match unsafe { Library::new(name) } {
            Ok(l) => l,
            Err(e) => {
                last_err = format!("{}: {}", name, e);
                continue;
            }
        };
        let get = |sym: &[u8]| unsafe { lib.get::<*mut c_void>(sym).map(|s| *s).map_err(|e| format!("symbol {:?}: {}", String::from_utf8_lossy(sym), e)) };
        let api = X11Api {
            open_display: unsafe { std::mem::transmute(get(b"XOpenDisplay\0")?) },
            close_display: unsafe { std::mem::transmute(get(b"XCloseDisplay\0")?) },
            default_screen: unsafe { std::mem::transmute(get(b"XDefaultScreen\0")?) },
            default_root_window: unsafe { std::mem::transmute(get(b"XDefaultRootWindow\0")?) },
            black_pixel: unsafe { std::mem::transmute(get(b"XBlackPixel\0")?) },
            white_pixel: unsafe { std::mem::transmute(get(b"XWhitePixel\0")?) },
            default_colormap: unsafe { std::mem::transmute(get(b"XDefaultColormap\0")?) },
            create_simple_window: unsafe { std::mem::transmute(get(b"XCreateSimpleWindow\0")?) },
            destroy_window: unsafe { std::mem::transmute(get(b"XDestroyWindow\0")?) },
            change_window_attributes: unsafe { std::mem::transmute(get(b"XChangeWindowAttributes\0")?) },
            map_window: unsafe { std::mem::transmute(get(b"XMapWindow\0")?) },
            map_raised: unsafe { std::mem::transmute(get(b"XMapRaised\0")?) },
            store_name: unsafe { std::mem::transmute(get(b"XStoreName\0")?) },
            select_input: unsafe { std::mem::transmute(get(b"XSelectInput\0")?) },
            move_resize_window: unsafe { std::mem::transmute(get(b"XMoveResizeWindow\0")?) },
            clear_window: unsafe { std::mem::transmute(get(b"XClearWindow\0")?) },
            flush: unsafe { std::mem::transmute(get(b"XFlush\0")?) },
            sync: unsafe { std::mem::transmute(get(b"XSync\0")?) },
            create_gc: unsafe { std::mem::transmute(get(b"XCreateGC\0")?) },
            free_gc: unsafe { std::mem::transmute(get(b"XFreeGC\0")?) },
            set_foreground: unsafe { std::mem::transmute(get(b"XSetForeground\0")?) },
            set_background: unsafe { std::mem::transmute(get(b"XSetBackground\0")?) },
            set_font: unsafe { std::mem::transmute(get(b"XSetFont\0")?) },
            draw_string: unsafe { std::mem::transmute(get(b"XDrawString\0")?) },
            draw_line: unsafe { std::mem::transmute(get(b"XDrawLine\0")?) },
            draw_rectangle: unsafe { std::mem::transmute(get(b"XDrawRectangle\0")?) },
            fill_rectangle: unsafe { std::mem::transmute(get(b"XFillRectangle\0")?) },
            draw_arc: unsafe { std::mem::transmute(get(b"XDrawArc\0")?) },
            fill_arc: unsafe { std::mem::transmute(get(b"XFillArc\0")?) },
            load_query_font: unsafe { std::mem::transmute(get(b"XLoadQueryFont\0")?) },
            free_font: unsafe { std::mem::transmute(get(b"XFreeFont\0")?) },
            intern_atom: unsafe { std::mem::transmute(get(b"XInternAtom\0")?) },
            get_selection_owner: unsafe { std::mem::transmute(get(b"XGetSelectionOwner\0")?) },
            send_event: unsafe { std::mem::transmute(get(b"XSendEvent\0")?) },
            change_property: unsafe { std::mem::transmute(get(b"XChangeProperty\0")?) },
            set_wm_protocols: unsafe { std::mem::transmute(get(b"XSetWMProtocols\0")?) },
            set_input_focus: unsafe { std::mem::transmute(get(b"XSetInputFocus\0")?) },
            lookup_string: unsafe { std::mem::transmute(get(b"XLookupString\0")?) },
            pending: unsafe { std::mem::transmute(get(b"XPending\0")?) },
            next_event: unsafe { std::mem::transmute(get(b"XNextEvent\0")?) },
            translate_coordinates: unsafe { std::mem::transmute(get(b"XTranslateCoordinates\0")?) },
            alloc_color: unsafe { std::mem::transmute(get(b"XAllocColor\0")?) },
            create_font_set: unsafe { std::mem::transmute(get(b"XCreateFontSet\0")?) },
            free_font_set: unsafe { std::mem::transmute(get(b"XFreeFontSet\0")?) },
            mb_draw_string: unsafe { std::mem::transmute(get(b"XmbDrawString\0")?) },
            mb_text_extents: unsafe { std::mem::transmute(get(b"XmbTextExtents\0")?) },
            font_set_extents: unsafe { std::mem::transmute(get(b"XFontSetExtents\0")?) },
            _lib: lib,
        };
        return Ok(api);
    }
    Err(last_err)
}

/// 全局 API（懒加载；加载失败则 X11 后端不可用）。
static API: std::sync::LazyLock<Result<X11Api, String>> = std::sync::LazyLock::new(load_api);

/// 全局 Display（懒打开；失败则 X11 后端不可用）。
static DISPLAY: std::sync::LazyLock<Result<usize, String>> = std::sync::LazyLock::new(|| {
    let api = match API.as_ref() {
        Ok(a) => a,
        Err(e) => return Err(e.clone()),
    };
    let d = unsafe { (api.open_display)(ptr::null()) };
    if d.is_null() {
        return Err(String::from("XOpenDisplay failed (no X11 display; check DISPLAY)"));
    }
    Ok(d as usize)
});

// ---------- 全局状态（与 guimod.rs 同构） ----------

static NEXT_WIN_ID: AtomicI64 = AtomicI64::new(1);
static NEXT_CTL_ID: AtomicI64 = AtomicI64::new(1);
static EVENTS: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// 窗口 id → 窗口状态
static WINDOWS: std::sync::LazyLock<Mutex<HashMap<i64, WinState>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// 事件类型常量（X11 头文件值）
const KEY_PRESS: c_int = 2;
const BUTTON_PRESS: c_int = 4;
const FOCUS_IN: c_int = 9;
const FOCUS_OUT: c_int = 10;
const EXPOSE: c_int = 12;
const DESTROY_NOTIFY: c_int = 17;
const CONFIGURE_NOTIFY: c_int = 22;
const CLIENT_MESSAGE: c_int = 33;

/// 按键 Keysym 常量
const XK_BACKSPACE: c_ulong = 0xff08;
const XK_RETURN: c_ulong = 0xff0d;
const XK_DELETE: c_ulong = 0xffff;

/// 鼠标滚轮（ButtonPress.button）
const WHEEL_UP: c_uint = 4;
const WHEEL_DOWN: c_uint = 5;

/// 标准颜色（RGB 0-65535）
const COL_BLACK: (u16, u16, u16) = (0, 0, 0);
const COL_WHITE: (u16, u16, u16) = (65535, 65535, 65535);
const COL_GRAY: (u16, u16, u16) = (50000, 50000, 50000);
const COL_DGRAY: (u16, u16, u16) = (33000, 33000, 33000);
const COL_HILITE: (u16, u16, u16) = (40000, 50000, 65535);

fn next_ctl_id() -> i64 {
    loop {
        let id = NEXT_CTL_ID.fetch_add(1, Ordering::Relaxed) & 0xFFFF;
        if id != 0 {
            return id;
        }
    }
}

fn push_event(win: i64, id: i64, ty: &str, value: &str) {
    let ev = serde_json::json!({"win": win, "id": id, "type": ty, "value": value}).to_string();
    EVENTS.lock().unwrap().push(ev);
}

fn x11_unavailable(span: Span, file: &str, src: &str) -> ZError {
    let detail = DISPLAY.as_ref().err().cloned().unwrap_or_else(|| String::from("unknown error"));
    zerr(
        codes::NOT_IMPLEMENTED,
        format!("guipro X11 backend unavailable: {}", detail),
        span,
        file,
        src,
        Some("ensure a graphical X11 session (DISPLAY set) and libX11.so.6 installed"),
    )
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

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap_or_else(|_| CString::new("").unwrap())
}

// ---------- 后端初始化辅助 ----------

/// 初始化 X11（打开 display、创建 GC、加载字体）。返回 (display, gc, fontset, font)。
struct Backend {
    display: *mut XDisplay,
    gc: *mut XGC,
    screen: c_int,
    root: XWindow,
    colormap: XColormap,
    black: c_ulong,
    white: c_ulong,
    /// XFontSet（0 = 未加载，退回固定字体）
    fontset: *mut c_void,
    /// XFontStruct*（固定字体回退）
    font: *mut XFontStruct,
    font_height: c_int,
}

fn backend_init() -> Result<Backend, String> {
    let api = API.as_ref().map_err(|e| e.clone())?;
    let d = DISPLAY.as_ref().map_err(|e| e.clone())?;
    let display = *d as *mut XDisplay;
    // 设置 locale（LC_CTYPE=0），使 XmbDrawString 支持 UTF-8
    unsafe {
        extern "C" {
            fn setlocale(category: c_int, locale: *const c_char) -> *mut c_char;
        }
        setlocale(0, b"\0".as_ptr() as *const c_char);
    }
    let screen = unsafe { (api.default_screen)(display) };
    let root = unsafe { (api.default_root_window)(display) };
    let colormap = unsafe { (api.default_colormap)(display, screen) };
    let black = unsafe { (api.black_pixel)(display, screen) };
    let white = unsafe { (api.white_pixel)(display, screen) };
    let gc = unsafe { (api.create_gc)(display, root, 0, ptr::null_mut()) };
    if gc.is_null() {
        return Err(String::from("XCreateGC failed"));
    }
    // 字体：优先 fontset，失败退回 "fixed"
    let mut fontset: *mut c_void = ptr::null_mut();
    let mut font: *mut XFontStruct = ptr::null_mut();
    let mut font_height: c_int = 16;
    unsafe {
        let fs = (api.create_font_set)(display, b"-*-*-*-*-*-*-*-*-*-*-*-*-*-*\0".as_ptr() as *const c_char, ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
        if !fs.is_null() {
            fontset = fs;
            let ext = (api.font_set_extents)(fs);
            if !ext.is_null() {
                font_height = (*ext).max_logical_extent.height as c_int;
                if font_height <= 0 {
                    font_height = 16;
                }
            }
        } else {
            let f = (api.load_query_font)(display, b"fixed\0".as_ptr() as *const c_char);
            if !f.is_null() {
                font = f;
                font_height = (*f).ascent + (*f).descent;
                (api.set_font)(display, gc, (*f).fid);
            }
        }
    }
    Ok(Backend {
        display,
        gc,
        screen,
        root,
        colormap,
        black,
        white,
        fontset,
        font,
        font_height,
    })
}

unsafe impl Send for Backend {}
unsafe impl Sync for Backend {}

/// 全局后端（懒初始化一次）。
static BACKEND: std::sync::LazyLock<Result<Backend, String>> = std::sync::LazyLock::new(backend_init);

// ---------- 控件/窗口状态 ----------

/// 树节点
struct TreeNode {
    id: i64,
    label: String,
    depth: i32,
    expanded: bool,
    has_children: bool,
}

/// canvas 图形指令
struct Shape {
    kind: String, // "line"/"rect"/"ellipse"/"text"
    x1: c_int,
    y1: c_int,
    x2: c_int,
    y2: c_int,
    text: String,
    color: (u16, u16, u16),
    fill: bool,
}

/// 控件状态（单窗口自绘，控件是窗口内矩形区域）
struct CtlState {
    kind: String,
    x: c_int,
    y: c_int,
    w: c_int,
    h: c_int,
    text: String,
    checked: bool,
    // slider
    min: i64,
    max: i64,
    val: i64,
    // select
    options: Vec<String>,
    sel: i64,
    // table
    columns: Vec<String>,
    rows: Vec<Vec<String>>,
    sel_row: i64,
    scroll: i64,
    // tree
    nodes: HashMap<i64, TreeNode>,
    roots: Vec<i64>,
    children: HashMap<i64, Vec<i64>>,
    next_node: i64,
    sel_node: i64,
    tree_scroll: i64,
    // canvas
    shapes: Vec<Shape>,
}

/// select 下拉弹窗
struct SelectPopup {
    win: XWindow,
    ctl_id: i64,
    items: Vec<String>,
    item_h: c_int,
    sel: i64,
}

/// 菜单弹窗
struct MenuPopup {
    win: XWindow,
    items: Vec<(String, i64)>, // (text, item_id)，item_id=-1 分隔线
    item_h: c_int,
}

/// 托盘图标状态（XEmbed 系统托盘）
struct TrayState {
    win: XWindow,
    tip: String,
    /// 上次左键点击时间（双击检测）
    last_click: c_ulong,
}

struct WinState {
    display: usize, // Display*
    window: XWindow,
    gc: usize, // GC*
    screen: c_int,
    root: XWindow,
    colormap: XColormap,
    black: c_ulong,
    white: c_ulong,
    fontset: *mut c_void,
    font: *mut XFontStruct,
    font_height: c_int,
    w: c_int,
    h: c_int,
    ctls: HashMap<i64, CtlState>,
    /// 聚焦的 input 控件 id（-1 无）
    focused: i64,
    // 菜单栏
    menu_top: Vec<(String, i64)>, // 顶层项 (text, submenu_id)
    menu_subs: HashMap<i64, Vec<(String, i64)>>, // submenu_id → (text, item_id)，item_id=-1 分隔线
    menu_paths: HashMap<i64, String>, // item_id → 路径（"文件/打开"）
    next_menu_id: i64,
    // 弹窗
    select_popup: Option<SelectPopup>,
    menu_popup: Option<MenuPopup>,
    /// 托盘图标（XEmbed）
    tray: Option<TrayState>,
}

unsafe impl Send for WinState {}

// ---------- 绘制辅助 ----------

fn alloc_color(win: &WinState, api: &X11Api, rgb: (u16, u16, u16)) -> c_ulong {
    let mut c = XColor {
        pixel: 0,
        red: rgb.0,
        green: rgb.1,
        blue: rgb.2,
        flags: 7, // DoRed | DoGreen | DoBlue
        pad: 0,
    };
    unsafe {
        (api.alloc_color)(win.display as *mut XDisplay, win.colormap, &mut c);
    }
    c.pixel
}

fn draw_text(win: &WinState, api: &X11Api, x: c_int, y: c_int, s: &str, fg: c_ulong) {
    if s.is_empty() {
        return;
    }
    let d = win.display as *mut XDisplay;
    let cs = cstr(s);
    unsafe {
        (api.set_foreground)(d, win.gc as *mut XGC, fg);
        if !win.fontset.is_null() {
            (api.mb_draw_string)(d, win.window, win.fontset, win.gc as *mut XGC, x, y, cs.as_ptr(), s.len() as c_int);
        } else {
            (api.draw_string)(d, win.window, win.gc as *mut XGC, x, y, cs.as_ptr(), s.len() as c_int);
        }
    }
}

fn text_width(win: &WinState, api: &X11Api, s: &str) -> c_int {
    if s.is_empty() {
        return 0;
    }
    if !win.fontset.is_null() {
        let cs = cstr(s);
        let mut ink = XRectangle { x: 0, y: 0, width: 0, height: 0 };
        let mut logical = XRectangle { x: 0, y: 0, width: 0, height: 0 };
        unsafe {
            (api.mb_text_extents)(win.fontset, cs.as_ptr(), s.len() as c_int, &mut ink, &mut logical);
        }
        logical.width as c_int
    } else {
        s.len() as c_int * 8
    }
}

/// 文本垂直居中基线（控件内）
fn text_baseline(win: &WinState, ctl: &CtlState) -> c_int {
    ctl.y + (ctl.h + win.font_height) / 2
}

// ---------- 基础控件绘制 ----------

fn draw_label(win: &WinState, api: &X11Api, ctl: &CtlState) {
    draw_text(win, api, ctl.x + 2, text_baseline(win, ctl), &ctl.text, win.black);
}

fn draw_button(win: &WinState, api: &X11Api, ctl: &CtlState) {
    let d = win.display as *mut XDisplay;
    unsafe {
        (api.set_foreground)(d, win.gc as *mut XGC, alloc_color(win, api, COL_GRAY));
        (api.fill_rectangle)(d, win.window, win.gc as *mut XGC, ctl.x, ctl.y, ctl.w as c_uint, ctl.h as c_uint);
        (api.set_foreground)(d, win.gc as *mut XGC, win.black);
        (api.draw_rectangle)(d, win.window, win.gc as *mut XGC, ctl.x, ctl.y, ctl.w as c_uint, ctl.h as c_uint);
    }
    let tw = text_width(win, api, &ctl.text);
    let tx = ctl.x + (ctl.w - tw) / 2;
    draw_text(win, api, tx, text_baseline(win, ctl), &ctl.text, win.black);
}

fn draw_input(win: &WinState, api: &X11Api, id: i64, ctl: &CtlState) {
    let d = win.display as *mut XDisplay;
    unsafe {
        (api.set_foreground)(d, win.gc as *mut XGC, win.white);
        (api.fill_rectangle)(d, win.window, win.gc as *mut XGC, ctl.x, ctl.y, ctl.w as c_uint, ctl.h as c_uint);
        (api.set_foreground)(d, win.gc as *mut XGC, win.black);
        (api.draw_rectangle)(d, win.window, win.gc as *mut XGC, ctl.x, ctl.y, ctl.w as c_uint, ctl.h as c_uint);
    }
    let tx = ctl.x + 4;
    draw_text(win, api, tx, text_baseline(win, ctl), &ctl.text, win.black);
    // 光标（聚焦时画在文本末尾）
    if win.focused == id {
        let caret_x = tx + text_width(win, api, &ctl.text) + 1;
        unsafe {
            (api.set_foreground)(d, win.gc as *mut XGC, win.black);
            (api.draw_line)(d, win.window, win.gc as *mut XGC, caret_x, ctl.y + 2, caret_x, ctl.y + ctl.h - 2);
        }
    }
}

fn draw_checkbox(win: &WinState, api: &X11Api, ctl: &CtlState, radio: bool) {
    let d = win.display as *mut XDisplay;
    let bs = ctl.h.min(16);
    unsafe {
        (api.set_foreground)(d, win.gc as *mut XGC, win.white);
        if radio {
            (api.fill_arc)(d, win.window, win.gc as *mut XGC, ctl.x, ctl.y, bs as c_uint, bs as c_uint, 0, 360 * 64);
        } else {
            (api.fill_rectangle)(d, win.window, win.gc as *mut XGC, ctl.x, ctl.y, bs as c_uint, bs as c_uint);
        }
        (api.set_foreground)(d, win.gc as *mut XGC, win.black);
        if radio {
            (api.draw_arc)(d, win.window, win.gc as *mut XGC, ctl.x, ctl.y, bs as c_uint, bs as c_uint, 0, 360 * 64);
        } else {
            (api.draw_rectangle)(d, win.window, win.gc as *mut XGC, ctl.x, ctl.y, bs as c_uint, bs as c_uint);
        }
        if ctl.checked {
            let inner = (bs - 8).max(1);
            (api.set_foreground)(d, win.gc as *mut XGC, win.black);
            if radio {
                (api.fill_arc)(d, win.window, win.gc as *mut XGC, ctl.x + 4, ctl.y + 4, inner as c_uint, inner as c_uint, 0, 360 * 64);
            } else {
                (api.fill_rectangle)(d, win.window, win.gc as *mut XGC, ctl.x + 3, ctl.y + 3, (bs - 6).max(1) as c_uint, (bs - 6).max(1) as c_uint);
            }
        }
    }
    draw_text(win, api, ctl.x + bs + 4, text_baseline(win, ctl), &ctl.text, win.black);
}

fn draw_slider(win: &WinState, api: &X11Api, ctl: &CtlState) {
    let d = win.display as *mut XDisplay;
    let track_y = ctl.y + ctl.h / 2;
    unsafe {
        (api.set_foreground)(d, win.gc as *mut XGC, win.black);
        (api.draw_line)(d, win.window, win.gc as *mut XGC, ctl.x, track_y, ctl.x + ctl.w, track_y);
    }
    let range = (ctl.max - ctl.min).max(1);
    let frac = ((ctl.val - ctl.min) as f64 / range as f64).clamp(0.0, 1.0);
    let thumb_w = 8;
    let thumb_x = ctl.x + (frac * ctl.w as f64) as c_int;
    unsafe {
        (api.set_foreground)(d, win.gc as *mut XGC, alloc_color(win, api, COL_DGRAY));
        (api.fill_rectangle)(d, win.window, win.gc as *mut XGC, thumb_x - thumb_w / 2, ctl.y, thumb_w as c_uint, ctl.h as c_uint);
        (api.set_foreground)(d, win.gc as *mut XGC, win.black);
        (api.draw_rectangle)(d, win.window, win.gc as *mut XGC, thumb_x - thumb_w / 2, ctl.y, thumb_w as c_uint, ctl.h as c_uint);
    }
}

fn draw_select(win: &WinState, api: &X11Api, ctl: &CtlState) {
    let d = win.display as *mut XDisplay;
    unsafe {
        (api.set_foreground)(d, win.gc as *mut XGC, win.white);
        (api.fill_rectangle)(d, win.window, win.gc as *mut XGC, ctl.x, ctl.y, ctl.w as c_uint, ctl.h as c_uint);
        (api.set_foreground)(d, win.gc as *mut XGC, win.black);
        (api.draw_rectangle)(d, win.window, win.gc as *mut XGC, ctl.x, ctl.y, ctl.w as c_uint, ctl.h as c_uint);
    }
    let cur = if ctl.sel >= 0 && (ctl.sel as usize) < ctl.options.len() {
        ctl.options[ctl.sel as usize].clone()
    } else {
        String::new()
    };
    draw_text(win, api, ctl.x + 4, text_baseline(win, ctl), &cur, win.black);
    // 右侧下拉箭头
    unsafe {
        (api.set_foreground)(d, win.gc as *mut XGC, win.black);
        let ax = ctl.x + ctl.w - 14;
        let ay = ctl.y + ctl.h / 2;
        (api.draw_line)(d, win.window, win.gc as *mut XGC, ax, ay - 2, ax + 8, ay - 2);
        (api.draw_line)(d, win.window, win.gc as *mut XGC, ax, ay - 2, ax + 4, ay + 2);
        (api.draw_line)(d, win.window, win.gc as *mut XGC, ax + 8, ay - 2, ax + 4, ay + 2);
    }
}

/// 整窗重绘（菜单栏 + 全部控件 + 弹窗）
fn redraw(win: &WinState) {
    let api = match API.as_ref() {
        Ok(a) => a,
        Err(_) => return,
    };
    let d = win.display as *mut XDisplay;
    unsafe {
        (api.clear_window)(d, win.window);
    }
    for (id, ctl) in win.ctls.iter() {
        draw_ctl(win, api, *id, ctl);
    }
    if !win.menu_top.is_empty() {
        draw_menu_bar(win, api);
    }
    // 弹窗内容由弹窗自身窗口绘制（Expose 时），此处无需处理
    unsafe {
        (api.flush)(d);
    }
}

fn draw_ctl(win: &WinState, api: &X11Api, id: i64, ctl: &CtlState) {
    match ctl.kind.as_str() {
        "label" => draw_label(win, api, ctl),
        "button" => draw_button(win, api, ctl),
        "input" => draw_input(win, api, id, ctl),
        "checkbox" => draw_checkbox(win, api, ctl, false),
        "radio" => draw_checkbox(win, api, ctl, true),
        "slider" => draw_slider(win, api, ctl),
        "select" => draw_select(win, api, ctl),
        "canvas" => draw_canvas(win, api, ctl),
        "table" => draw_table(win, api, ctl),
        "tree" => draw_tree(win, api, ctl),
        _ => {}
    }
}

// ---------- 窗口 ----------

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
    let be = BACKEND.as_ref().map_err(|e| {
        zerr(codes::SYSCALL, e.clone(), span, file, src, Some("check X11 display availability"))
    })?;
    let api = API.as_ref().map_err(|e| zerr(codes::SYSCALL, e.clone(), span, file, src, None::<&str>))?;
    let win_id = NEXT_WIN_ID.fetch_add(1, Ordering::Relaxed);
    let win = unsafe {
        (api.create_simple_window)(be.display, be.root, 0, 0, w as c_uint, h as c_uint, 0, be.black, be.white)
    };
    if win == 0 {
        return Err(zerr(codes::SYSCALL, "XCreateSimpleWindow failed", span, file, src, None::<&str>));
    }
    unsafe {
        (api.store_name)(be.display, win, cstr(title).as_ptr());
        // ExposureMask|ButtonPressMask|KeyPressMask|FocusChangeMask|StructureNotifyMask
        (api.select_input)(be.display, win, 0x8000 | 0x4 | 0x1 | 0x200000 | 0x80000);
        let wm_delete = (api.intern_atom)(be.display, b"WM_DELETE_WINDOW\0".as_ptr() as *const c_char, 0);
        (api.set_wm_protocols)(be.display, win, &wm_delete, 1);
        (api.map_window)(be.display, win);
        (api.flush)(be.display);
    }
    WINDOWS.lock().unwrap().insert(
        win_id,
        WinState {
            display: be.display as usize,
            window: win,
            gc: be.gc as usize,
            screen: be.screen,
            root: be.root,
            colormap: be.colormap,
            black: be.black,
            white: be.white,
            fontset: be.fontset,
            font: be.font,
            font_height: be.font_height,
            w: w as c_int,
            h: h as c_int,
            ctls: HashMap::new(),
            focused: -1,
            menu_top: Vec::new(),
            menu_subs: HashMap::new(),
            menu_paths: HashMap::new(),
            next_menu_id: 0x1000,
            select_popup: None,
            menu_popup: None,
            tray: None,
        },
    );
    Ok(Value::Int(win_id))
}

// ---------- 添加控件 ----------

/// 递归插入树节点（parent=0 表示根），返回节点 id。
fn insert_tree_node(ctl: &mut CtlState, parent: i64, d: &Value, span: Span, file: &str, src: &str) -> Result<i64, ZError> {
    let text = dict_str(d, "text", span, file, src)?;
    let id = ctl.next_node;
    ctl.next_node += 1;
    let has_children = dict_get(d, "items").is_some();
    let depth = if parent == 0 {
        0
    } else {
        ctl.nodes.get(&parent).map(|n| n.depth + 1).unwrap_or(0)
    };
    ctl.nodes.insert(
        id,
        TreeNode { id, label: text.clone(), depth, expanded: true, has_children },
    );
    if parent == 0 {
        ctl.roots.push(id);
    } else {
        ctl.children.entry(parent).or_insert_with(Vec::new).push(id);
    }
    if let Some(Value::List(children)) = dict_get(d, "items") {
        for ch in children {
            insert_tree_node(ctl, id, ch, span, file, src)?;
        }
    }
    Ok(id)
}

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
    let win = wins.get_mut(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
    let ctl_id = next_ctl_id();
    let mut ctl = CtlState {
        kind: kind.clone(),
        x: dict_int(d, "x", 0, span, file, src)? as c_int,
        y: dict_int(d, "y", 0, span, file, src)? as c_int,
        w: dict_int(d, "w", 80, span, file, src)? as c_int,
        h: dict_int(d, "h", 24, span, file, src)? as c_int,
        text: dict_str(d, "text", span, file, src)?,
        checked: false,
        min: 0,
        max: 100,
        val: 0,
        options: Vec::new(),
        sel: 0,
        columns: Vec::new(),
        rows: Vec::new(),
        sel_row: -1,
        scroll: 0,
        nodes: HashMap::new(),
        roots: Vec::new(),
        children: HashMap::new(),
        next_node: 1,
        sel_node: -1,
        tree_scroll: 0,
        shapes: Vec::new(),
    };
    match kind.as_str() {
        "slider" => {
            ctl.min = dict_int(d, "min", 0, span, file, src)?;
            ctl.max = dict_int(d, "max", 100, span, file, src)?;
            ctl.val = dict_int(d, "value", ctl.min, span, file, src)?;
        }
        "select" => {
            if let Some(Value::List(opts)) = dict_get(d, "options") {
                for o in opts {
                    if let Value::Str(s) = o {
                        ctl.options.push(s.clone());
                    }
                }
            }
            ctl.sel = 0;
        }
        "table" => {
            if let Some(Value::List(cols)) = dict_get(d, "columns") {
                for c in cols {
                    if let Value::Str(s) = c {
                        ctl.columns.push(s.clone());
                    }
                }
            }
            if let Some(Value::List(rows)) = dict_get(d, "rows") {
                for r in rows {
                    if let Value::List(cells) = r {
                        let mut row = Vec::new();
                        for c in cells {
                            if let Value::Str(s) = c {
                                row.push(s.clone());
                            }
                        }
                        ctl.rows.push(row);
                    }
                }
            }
        }
        "tree" => {
            if let Some(Value::List(items)) = dict_get(d, "items") {
                for it in items {
                    insert_tree_node(&mut ctl, 0, it, span, file, src)?;
                }
            }
        }
        _ => {}
    }
    win.ctls.insert(ctl_id, ctl);
    redraw(win);
    Ok(Value::Int(ctl_id))
}

// ---------- canvas 绘制 ----------

fn draw_canvas(win: &WinState, api: &X11Api, ctl: &CtlState) {
    let d = win.display as *mut XDisplay;
    unsafe {
        (api.set_foreground)(d, win.gc as *mut XGC, win.white);
        (api.fill_rectangle)(d, win.window, win.gc as *mut XGC, ctl.x, ctl.y, ctl.w as c_uint, ctl.h as c_uint);
        (api.set_foreground)(d, win.gc as *mut XGC, win.black);
        (api.draw_rectangle)(d, win.window, win.gc as *mut XGC, ctl.x, ctl.y, ctl.w as c_uint, ctl.h as c_uint);
    }
    for sh in &ctl.shapes {
        let color = alloc_color(win, api, sh.color);
        let x1 = ctl.x + sh.x1;
        let y1 = ctl.y + sh.y1;
        let x2 = ctl.x + sh.x2;
        let y2 = ctl.y + sh.y2;
        unsafe {
            (api.set_foreground)(d, win.gc as *mut XGC, color);
            match sh.kind.as_str() {
                "line" => {
                    (api.draw_line)(d, win.window, win.gc as *mut XGC, x1, y1, x2, y2);
                }
                "rect" => {
                    if sh.fill {
                        (api.fill_rectangle)(d, win.window, win.gc as *mut XGC, x1, y1, (x2 - x1) as c_uint, (y2 - y1) as c_uint);
                    }
                    (api.draw_rectangle)(d, win.window, win.gc as *mut XGC, x1, y1, (x2 - x1) as c_uint, (y2 - y1) as c_uint);
                }
                "ellipse" => {
                    if sh.fill {
                        (api.fill_arc)(d, win.window, win.gc as *mut XGC, x1, y1, (x2 - x1) as c_uint, (y2 - y1) as c_uint, 0, 360 * 64);
                    }
                    (api.draw_arc)(d, win.window, win.gc as *mut XGC, x1, y1, (x2 - x1) as c_uint, (y2 - y1) as c_uint, 0, 360 * 64);
                }
                "text" => {
                    draw_text(win, api, x1, y1 + win.font_height, &sh.text, color);
                }
                _ => {}
            }
        }
    }
}

// ---------- table 绘制 ----------

fn draw_table(win: &WinState, api: &X11Api, ctl: &CtlState) {
    let d = win.display as *mut XDisplay;
    let header_h = win.font_height + 6;
    let row_h = win.font_height + 4;
    let col_count = ctl.columns.len().max(1);
    let col_w = (ctl.w / col_count as c_int).max(40);
    unsafe {
        (api.set_foreground)(d, win.gc as *mut XGC, win.white);
        (api.fill_rectangle)(d, win.window, win.gc as *mut XGC, ctl.x, ctl.y, ctl.w as c_uint, ctl.h as c_uint);
        (api.set_foreground)(d, win.gc as *mut XGC, win.black);
        (api.draw_rectangle)(d, win.window, win.gc as *mut XGC, ctl.x, ctl.y, ctl.w as c_uint, ctl.h as c_uint);
    }
    // 列头
    let mut cx = ctl.x + 2;
    for col in &ctl.columns {
        draw_text(win, api, cx, ctl.y + header_h - 2, col, win.black);
        unsafe {
            (api.set_foreground)(d, win.gc as *mut XGC, win.black);
            (api.draw_line)(d, win.window, win.gc as *mut XGC, cx - 1, ctl.y + 1, cx - 1, ctl.y + ctl.h - 1);
        }
        cx += col_w;
    }
    // 表头底线
    unsafe {
        (api.set_foreground)(d, win.gc as *mut XGC, win.black);
        (api.draw_line)(d, win.window, win.gc as *mut XGC, ctl.x, ctl.y + header_h, ctl.x + ctl.w, ctl.y + header_h);
    }
    // 数据行
    let visible = ((ctl.h - header_h) / row_h).max(0) as i64;
    for r in ctl.scroll..(ctl.scroll + visible) {
        if r >= ctl.rows.len() as i64 {
            break;
        }
        let ry = ctl.y + header_h + ((r - ctl.scroll) as c_int) * row_h;
        if r == ctl.sel_row {
            unsafe {
                (api.set_foreground)(d, win.gc as *mut XGC, alloc_color(win, api, COL_HILITE));
                (api.fill_rectangle)(d, win.window, win.gc as *mut XGC, ctl.x + 1, ry, (ctl.w - 1) as c_uint, row_h as c_uint);
            }
        }
        let row = &ctl.rows[r as usize];
        let mut rx = ctl.x + 2;
        for cell in row {
            draw_text(win, api, rx, ry + row_h - 2, cell, win.black);
            rx += col_w;
        }
    }
}

// ---------- tree 绘制 ----------

fn collect_visible(ctl: &CtlState, id: i64, out: &mut Vec<i64>) {
    out.push(id);
    if let Some(n) = ctl.nodes.get(&id) {
        if n.expanded {
            if let Some(children) = ctl.children.get(&id) {
                for c in children {
                    collect_visible(ctl, *c, out);
                }
            }
        }
    }
}

fn draw_tree(win: &WinState, api: &X11Api, ctl: &CtlState) {
    let d = win.display as *mut XDisplay;
    let row_h = win.font_height + 4;
    unsafe {
        (api.set_foreground)(d, win.gc as *mut XGC, win.white);
        (api.fill_rectangle)(d, win.window, win.gc as *mut XGC, ctl.x, ctl.y, ctl.w as c_uint, ctl.h as c_uint);
        (api.set_foreground)(d, win.gc as *mut XGC, win.black);
        (api.draw_rectangle)(d, win.window, win.gc as *mut XGC, ctl.x, ctl.y, ctl.w as c_uint, ctl.h as c_uint);
    }
    let mut visible: Vec<i64> = Vec::new();
    for root in &ctl.roots {
        collect_visible(ctl, *root, &mut visible);
    }
    let visible_count = visible.len() as i64;
    let visible_h = ((ctl.h - 2) / row_h).max(0) as i64;
    let mut r = ctl.tree_scroll;
    while r < visible_count && (r - ctl.tree_scroll) < visible_h {
        let id = visible[r as usize];
        let node = ctl.nodes.get(&id).unwrap();
        let ry = ctl.y + 2 + ((r - ctl.tree_scroll) as c_int) * row_h;
        if id == ctl.sel_node {
            unsafe {
                (api.set_foreground)(d, win.gc as *mut XGC, alloc_color(win, api, COL_HILITE));
                (api.fill_rectangle)(d, win.window, win.gc as *mut XGC, ctl.x + 1, ry, (ctl.w - 1) as c_uint, row_h as c_uint);
            }
        }
        let indent = node.depth * 16;
        if node.has_children {
            let ax = ctl.x + indent + 4;
            let ay = ry + row_h / 2;
            unsafe {
                (api.set_foreground)(d, win.gc as *mut XGC, win.black);
                if node.expanded {
                    (api.draw_line)(d, win.window, win.gc as *mut XGC, ax, ay - 2, ax + 8, ay - 2);
                    (api.draw_line)(d, win.window, win.gc as *mut XGC, ax, ay - 2, ax + 4, ay + 2);
                    (api.draw_line)(d, win.window, win.gc as *mut XGC, ax + 8, ay - 2, ax + 4, ay + 2);
                } else {
                    (api.draw_line)(d, win.window, win.gc as *mut XGC, ax, ay - 3, ax, ay + 3);
                    (api.draw_line)(d, win.window, win.gc as *mut XGC, ax, ay - 3, ax + 4, ay);
                    (api.draw_line)(d, win.window, win.gc as *mut XGC, ax + 4, ay, ax, ay + 3);
                }
            }
        }
        draw_text(win, api, ctl.x + indent + 16, ry + row_h - 2, &node.label, win.black);
        r += 1;
    }
}

// ---------- 菜单栏绘制与构建 ----------

fn draw_menu_bar(win: &WinState, api: &X11Api) {
    let d = win.display as *mut XDisplay;
    let menu_h = win.font_height + 8;
    unsafe {
        (api.set_foreground)(d, win.gc as *mut XGC, alloc_color(win, api, COL_GRAY));
        (api.fill_rectangle)(d, win.window, win.gc as *mut XGC, 0, 0, win.w as c_uint, menu_h as c_uint);
        (api.set_foreground)(d, win.gc as *mut XGC, win.black);
        (api.draw_line)(d, win.window, win.gc as *mut XGC, 0, menu_h, win.w, menu_h);
    }
    let mut x = 8;
    for (text, _) in &win.menu_top {
        draw_text(win, api, x, (menu_h + win.font_height) / 2, text, win.black);
        x += text_width(win, api, text) + 20;
    }
}

/// 构建菜单栏状态（顶层项 + 子菜单 + 路径表）。
fn build_menu(win: &mut WinState, items: &Value, span: Span, file: &str, src: &str) -> Result<(), ZError> {
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
    for item in list {
        let text = dict_str(item, "text", span, file, src)?;
        if dict_get(item, "items").is_some() {
            let sub_id = win.next_menu_id;
            win.next_menu_id += 1;
            win.menu_top.push((text.clone(), sub_id));
            let mut sub: Vec<(String, i64)> = Vec::new();
            if let Some(Value::List(child_items)) = dict_get(item, "items") {
                for ci in child_items {
                    let ctext = dict_str(ci, "text", span, file, src)?;
                    if ctext == "-" {
                        sub.push(("-".to_string(), -1));
                    } else {
                        let item_id = win.next_menu_id;
                        win.next_menu_id += 1;
                        win.menu_paths.insert(item_id, format!("{}/{}", text, ctext));
                        sub.push((ctext, item_id));
                    }
                }
            }
            win.menu_subs.insert(sub_id, sub);
        } else {
            let item_id = win.next_menu_id;
            win.next_menu_id += 1;
            win.menu_paths.insert(item_id, text.clone());
            win.menu_top.push((text, item_id));
        }
    }
    Ok(())
}

// ---------- 弹窗绘制（select 下拉 / 菜单） ----------

/// 向任意目标窗口绘制文本（弹窗用）。
fn draw_text_on(win: &WinState, api: &X11Api, target: XWindow, x: c_int, y: c_int, s: &str, fg: c_ulong) {
    if s.is_empty() {
        return;
    }
    let d = win.display as *mut XDisplay;
    let cs = cstr(s);
    unsafe {
        (api.set_foreground)(d, win.gc as *mut XGC, fg);
        if !win.fontset.is_null() {
            (api.mb_draw_string)(d, target, win.fontset, win.gc as *mut XGC, x, y, cs.as_ptr(), s.len() as c_int);
        } else {
            (api.draw_string)(d, target, win.gc as *mut XGC, x, y, cs.as_ptr(), s.len() as c_int);
        }
    }
}

fn draw_select_popup(win: &WinState, api: &X11Api, popup: &SelectPopup) {
    let d = win.display as *mut XDisplay;
    let pw = popup.win;
    let w = popup.items.iter().map(|s| text_width(win, api, s)).max().unwrap_or(80) + 16;
    let h = (popup.items.len() as c_int) * popup.item_h;
    unsafe {
        (api.set_foreground)(d, win.gc as *mut XGC, win.white);
        (api.fill_rectangle)(d, pw, win.gc as *mut XGC, 0, 0, w as c_uint, h as c_uint);
        (api.set_foreground)(d, win.gc as *mut XGC, win.black);
        (api.draw_rectangle)(d, pw, win.gc as *mut XGC, 0, 0, w as c_uint, h as c_uint);
    }
    for (i, item) in popup.items.iter().enumerate() {
        let ry = (i as c_int) * popup.item_h;
        if i as i64 == popup.sel {
            unsafe {
                (api.set_foreground)(d, win.gc as *mut XGC, alloc_color(win, api, COL_HILITE));
                (api.fill_rectangle)(d, pw, win.gc as *mut XGC, 1, ry, (w - 2) as c_uint, popup.item_h as c_uint);
            }
        }
        draw_text_on(win, api, pw, 6, ry + (popup.item_h + win.font_height) / 2, item, win.black);
    }
}

fn draw_menu_popup(win: &WinState, api: &X11Api, popup: &MenuPopup) {
    let d = win.display as *mut XDisplay;
    let pw = popup.win;
    let w = popup.items.iter().map(|(s, _)| text_width(win, api, s)).max().unwrap_or(80) + 24;
    let h = (popup.items.len() as c_int) * popup.item_h;
    unsafe {
        (api.set_foreground)(d, win.gc as *mut XGC, win.white);
        (api.fill_rectangle)(d, pw, win.gc as *mut XGC, 0, 0, w as c_uint, h as c_uint);
        (api.set_foreground)(d, win.gc as *mut XGC, win.black);
        (api.draw_rectangle)(d, pw, win.gc as *mut XGC, 0, 0, w as c_uint, h as c_uint);
    }
    for (i, (text, item_id)) in popup.items.iter().enumerate() {
        let ry = (i as c_int) * popup.item_h;
        if *item_id == -1 {
            // 分隔线
            unsafe {
                (api.set_foreground)(d, win.gc as *mut XGC, win.black);
                (api.draw_line)(d, pw, win.gc as *mut XGC, 4, ry + popup.item_h / 2, w - 4, ry + popup.item_h / 2);
            }
        } else {
            draw_text_on(win, api, pw, 8, ry + (popup.item_h + win.font_height) / 2, text, win.black);
        }
    }
}

// ---------- 事件处理 ----------

fn win_root_pos(api: &X11Api, d: *mut XDisplay, ws: &WinState) -> (c_int, c_int) {
    let mut rx = 0;
    let mut ry = 0;
    let mut child: XWindow = 0;
    unsafe {
        (api.translate_coordinates)(d, ws.window, ws.root, 0, 0, &mut rx, &mut ry, &mut child);
    }
    (rx, ry)
}

fn close_select_popup(api: &X11Api, d: *mut XDisplay, ws: &mut WinState) {
    if let Some(p) = ws.select_popup.take() {
        unsafe {
            (api.destroy_window)(d, p.win);
            (api.flush)(d);
        }
    }
}

fn close_menu_popup(api: &X11Api, d: *mut XDisplay, ws: &mut WinState) {
    if let Some(p) = ws.menu_popup.take() {
        unsafe {
            (api.destroy_window)(d, p.win);
            (api.flush)(d);
        }
    }
}

fn open_select_popup(api: &X11Api, d: *mut XDisplay, ws: &mut WinState, ctl_id: i64) {
    close_select_popup(api, d, ws);
    close_menu_popup(api, d, ws);
    let (ctl_x, ctl_y, ctl_h, options, cur_sel) = {
        let ctl = match ws.ctls.get(&ctl_id) {
            Some(c) => c,
            None => return,
        };
        (ctl.x, ctl.y, ctl.h, ctl.options.clone(), ctl.sel)
    };
    if options.is_empty() {
        return;
    }
    let item_h = ws.font_height + 6;
    let w = options.iter().map(|s| text_width(ws, api, s)).max().unwrap_or(80) + 16;
    let h = (options.len() as c_int) * item_h;
    let (rx, ry) = win_root_pos(api, d, ws);
    let pw = unsafe {
        (api.create_simple_window)(d, ws.root, rx + ctl_x, ry + ctl_y + ctl_h, w as c_uint, h as c_uint, 1, ws.black, ws.white)
    };
    unsafe {
        // override_redirect=1（CWOverrideRedirect=0x200）：弹窗不被 WM 管理/重定位
        let mut attrs: XSetWindowAttributes = std::mem::zeroed();
        attrs.override_redirect = 1;
        (api.change_window_attributes)(d, pw, 0x200, &mut attrs);
        (api.select_input)(d, pw, 0x8000 | 0x4); // ExposureMask | ButtonPressMask
        (api.map_raised)(d, pw);
        (api.flush)(d);
    }
    ws.select_popup = Some(SelectPopup { win: pw, ctl_id, items: options, item_h, sel: cur_sel });
}

fn open_menu_popup(api: &X11Api, d: *mut XDisplay, ws: &mut WinState, sub_id: i64, x: c_int) {
    close_select_popup(api, d, ws);
    close_menu_popup(api, d, ws);
    let items = ws.menu_subs.get(&sub_id).cloned().unwrap_or_default();
    if items.is_empty() {
        return;
    }
    let item_h = ws.font_height + 6;
    let w = items.iter().map(|(s, _)| text_width(ws, api, s)).max().unwrap_or(80) + 24;
    let h = (items.len() as c_int) * item_h;
    let (rx, ry) = win_root_pos(api, d, ws);
    let pw = unsafe {
        (api.create_simple_window)(d, ws.root, rx + x, ry + ws.font_height + 8, w as c_uint, h as c_uint, 1, ws.black, ws.white)
    };
    unsafe {
        let mut attrs: XSetWindowAttributes = std::mem::zeroed();
        attrs.override_redirect = 1;
        (api.change_window_attributes)(d, pw, 0x200, &mut attrs);
        (api.select_input)(d, pw, 0x8000 | 0x4);
        (api.map_raised)(d, pw);
        (api.flush)(d);
    }
    ws.menu_popup = Some(MenuPopup { win: pw, items, item_h });
}

fn hit_test(ws: &WinState, x: c_int, y: c_int) -> Option<i64> {
    for (id, ctl) in ws.ctls.iter() {
        if x >= ctl.x && x < ctl.x + ctl.w && y >= ctl.y && y < ctl.y + ctl.h {
            return Some(*id);
        }
    }
    None
}

fn handle_tree_click(api: &X11Api, d: *mut XDisplay, ws: &mut WinState, win_id: i64, ctl_id: i64, x: c_int, y: c_int) {
    let row_h = ws.font_height + 4;
    let (ctl_y, visible, node_id, has_children, depth) = {
        let ctl = match ws.ctls.get(&ctl_id) {
            Some(c) => c,
            None => return,
        };
        let mut visible: Vec<i64> = Vec::new();
        for root in &ctl.roots {
            collect_visible(ctl, *root, &mut visible);
        }
        let idx = ctl.tree_scroll + ((y - ctl.y - 2) / row_h) as i64;
        if idx < 0 || idx >= visible.len() as i64 {
            return;
        }
        let nid = visible[idx as usize];
        let node = match ctl.nodes.get(&nid) {
            Some(n) => n,
            None => return,
        };
        (ctl.y, visible, nid, node.has_children, node.depth)
    };
    let indent = depth * 16;
    // 点击箭头区域 → 展开/折叠
    if has_children && x >= ctl_y + indent + 2 && x <= ctl_y + indent + 14 {
        if let Some(ctl) = ws.ctls.get_mut(&ctl_id) {
            if let Some(n) = ctl.nodes.get_mut(&node_id) {
                n.expanded = !n.expanded;
            }
        }
        redraw(ws);
        return;
    }
    // 点击标签 → 选中
    if let Some(ctl) = ws.ctls.get_mut(&ctl_id) {
        ctl.sel_node = node_id;
    }
    push_event(win_id, ctl_id, "change", &node_id.to_string());
    redraw(ws);
}

fn handle_ctl_click(api: &X11Api, d: *mut XDisplay, ws: &mut WinState, win_id: i64, ctl_id: i64, x: c_int, y: c_int) {
    let kind = ws.ctls.get(&ctl_id).map(|c| c.kind.clone()).unwrap_or_default();
    match kind.as_str() {
        "button" => {
            push_event(win_id, ctl_id, "click", "");
        }
        "checkbox" | "radio" => {
            if let Some(ctl) = ws.ctls.get_mut(&ctl_id) {
                ctl.checked = !ctl.checked;
                let v = if ctl.checked { "1" } else { "0" };
                push_event(win_id, ctl_id, "change", v);
            }
        }
        "input" => {
            ws.focused = ctl_id;
            unsafe {
                (api.set_input_focus)(d, ws.window, 2, 0); // RevertToParent, CurrentTime
            }
        }
        "slider" => {
            if let Some(ctl) = ws.ctls.get_mut(&ctl_id) {
                let frac = ((x - ctl.x) as f64 / ctl.w.max(1) as f64).clamp(0.0, 1.0);
                let val = ctl.min + ((frac * (ctl.max - ctl.min) as f64).round() as i64);
                ctl.val = val;
                push_event(win_id, ctl_id, "change", &val.to_string());
            }
        }
        "select" => {
            open_select_popup(api, d, ws, ctl_id);
        }
        "table" => {
            let header_h = ws.font_height + 6;
            let row_h = ws.font_height + 4;
            let row = ws.ctls.get(&ctl_id).map(|c| {
                if y < c.y + header_h {
                    -1
                } else {
                    c.scroll + ((y - c.y - header_h) / row_h) as i64
                }
            }).unwrap_or(-1);
            if let Some(ctl) = ws.ctls.get_mut(&ctl_id) {
                if row >= 0 && row < ctl.rows.len() as i64 {
                    ctl.sel_row = row;
                    push_event(win_id, ctl_id, "change", &row.to_string());
                }
            }
        }
        "tree" => {
            handle_tree_click(api, d, ws, win_id, ctl_id, x, y);
        }
        "canvas" => {
            if let Some(ctl) = ws.ctls.get(&ctl_id) {
                let cx = x - ctl.x;
                let cy = y - ctl.y;
                push_event(win_id, ctl_id, "click", &format!("[{},{}]", cx, cy));
            }
        }
        _ => {}
    }
}

fn handle_wheel(ws: &mut WinState, x: c_int, y: c_int, button: c_uint) {
    let ctl_id = match hit_test(ws, x, y) {
        Some(id) => id,
        None => return,
    };
    let kind = ws.ctls.get(&ctl_id).map(|c| c.kind.clone()).unwrap_or_default();
    if kind == "table" {
        if let Some(ctl) = ws.ctls.get_mut(&ctl_id) {
            let max_scroll = (ctl.rows.len() as i64 - 1).max(0);
            if button == WHEEL_UP {
                ctl.scroll = (ctl.scroll - 1).max(0);
            } else if button == WHEEL_DOWN {
                ctl.scroll = (ctl.scroll + 1).min(max_scroll);
            }
        }
    } else if kind == "tree" {
        if let Some(ctl) = ws.ctls.get_mut(&ctl_id) {
            if button == WHEEL_UP {
                ctl.tree_scroll = (ctl.tree_scroll - 1).max(0);
            } else if button == WHEEL_DOWN {
                ctl.tree_scroll += 1;
            }
        }
    }
}

fn handle_menu_bar_click(api: &X11Api, d: *mut XDisplay, ws: &mut WinState, win_id: i64, x: c_int) {
    let mut cx = 8;
    for (text, id) in ws.menu_top.clone() {
        let tw = text_width(ws, api, &text);
        if x >= cx && x < cx + tw + 20 {
            if ws.menu_subs.contains_key(&id) {
                open_menu_popup(api, d, ws, id, cx);
            } else if let Some(path) = ws.menu_paths.get(&id) {
                push_event(win_id, 0, "menu", path);
            }
            return;
        }
        cx += tw + 20;
    }
}

/// 托盘点击 → 推送 tray 事件（left/middle/right，左键双击 → double）。
fn handle_tray_button(ws: &mut WinState, win_id: i64, button: c_uint, time: c_ulong) {
    let value = match button {
        1 => "left",
        2 => "middle",
        3 => "right",
        _ => return, // 滚轮等忽略
    };
    if button == 1 {
        // 双击检测：400ms 内再次左键 → double
        if let Some(t) = ws.tray.as_mut() {
            if time > 0 && t.last_click > 0 && time - t.last_click < 400 {
                t.last_click = 0;
                push_event(win_id, 0, "tray", "double");
                return;
            }
            t.last_click = time;
        }
    }
    push_event(win_id, 0, "tray", value);
}

fn handle_button(api: &X11Api, d: *mut XDisplay, w: XWindow, x: c_int, y: c_int, button: c_uint, time: c_ulong) {
    let mut wins = WINDOWS.lock().unwrap();
    for (win_id, ws) in wins.iter_mut() {
        // 托盘窗口点击
        if ws.tray.as_ref().map(|t| t.win) == Some(w) {
            handle_tray_button(ws, *win_id, button, time);
            return;
        }
        // select 弹窗
        let popup_win = ws.select_popup.as_ref().map(|p| p.win);
        if popup_win == Some(w) {
            let (ctl_id, item_h, items) = {
                let p = ws.select_popup.as_ref().unwrap();
                (p.ctl_id, p.item_h, p.items.clone())
            };
            let idx = (y / item_h) as i64;
            let mut selected: Option<String> = None;
            if idx >= 0 && (idx as usize) < items.len() {
                if let Some(ctl) = ws.ctls.get_mut(&ctl_id) {
                    ctl.sel = idx;
                    selected = Some(items[idx as usize].clone());
                }
            }
            close_select_popup(api, d, ws);
            if let Some(t) = selected {
                push_event(*win_id, ctl_id, "change", &t);
            }
            redraw(ws);
            return;
        }
        // 菜单弹窗
        let mpopup_win = ws.menu_popup.as_ref().map(|p| p.win);
        if mpopup_win == Some(w) {
            let (item_h, items) = {
                let p = ws.menu_popup.as_ref().unwrap();
                (p.item_h, p.items.clone())
            };
            let idx = (y / item_h) as usize;
            let mut fired: Option<String> = None;
            if idx < items.len() {
                let (_, item_id) = items[idx];
                if item_id != -1 {
                    if let Some(path) = ws.menu_paths.get(&item_id) {
                        fired = Some(path.clone());
                    }
                }
            }
            close_menu_popup(api, d, ws);
            if let Some(p) = fired {
                push_event(*win_id, 0, "menu", &p);
            }
            redraw(ws);
            return;
        }
        // 主窗口
        if ws.window == w {
            close_select_popup(api, d, ws);
            close_menu_popup(api, d, ws);
            let menu_h = if ws.menu_top.is_empty() { 0 } else { ws.font_height + 8 };
            if y < menu_h && !ws.menu_top.is_empty() {
                handle_menu_bar_click(api, d, ws, *win_id, x);
                redraw(ws);
                return;
            }
            if button == WHEEL_UP || button == WHEEL_DOWN {
                handle_wheel(ws, x, y, button);
                redraw(ws);
                return;
            }
            if let Some(ctl_id) = hit_test(ws, x, y) {
                handle_ctl_click(api, d, ws, *win_id, ctl_id, x, y);
                redraw(ws);
            }
            return;
        }
    }
}

fn handle_key(api: &X11Api, d: *mut XDisplay, xkey: &XKeyEvent) {
    let mut wins = WINDOWS.lock().unwrap();
    for (win_id, ws) in wins.iter_mut() {
        if ws.window == xkey.window {
            let focused = ws.focused;
            if focused < 0 {
                return;
            }
            let mut buf = [0u8; 64];
            let mut keysym: XKeySym = 0;
            let n = unsafe {
                (api.lookup_string)(xkey as *const XKeyEvent, buf.as_mut_ptr() as *mut c_char, buf.len() as c_int, &mut keysym, ptr::null_mut())
            };
            if n > 0 {
                let s = String::from_utf8_lossy(&buf[..n as usize]).into_owned();
                if let Some(ctl) = ws.ctls.get_mut(&focused) {
                    ctl.text.push_str(&s);
                    let text = ctl.text.clone();
                    push_event(*win_id, focused, "change", &text);
                }
            } else if keysym == XK_BACKSPACE || keysym == XK_DELETE {
                if let Some(ctl) = ws.ctls.get_mut(&focused) {
                    ctl.text.pop();
                    let text = ctl.text.clone();
                    push_event(*win_id, focused, "change", &text);
                }
            }
            redraw(ws);
            return;
        }
    }
}

fn handle_configure(api: &X11Api, d: *mut XDisplay, w: XWindow, width: c_int, height: c_int) {
    let mut wins = WINDOWS.lock().unwrap();
    for (win_id, ws) in wins.iter_mut() {
        if ws.window == w {
            ws.w = width;
            ws.h = height;
            push_event(*win_id, 0, "resize", &format!("{}x{}", width, height));
            redraw(ws);
            return;
        }
    }
}

fn handle_client(api: &X11Api, d: *mut XDisplay, w: XWindow, message_type: XAtom, data: [c_long; 5]) {
    // 托盘点击消息（托盘管理器转发）：_NET_SYSTEM_TRAY_OPCODE + SYSTEM_TRAY_MESSAGE(1)
    let opcode = unsafe { (api.intern_atom)(d, b"_NET_SYSTEM_TRAY_OPCODE\0".as_ptr() as *const c_char, 0) };
    if message_type == opcode && data[0] == 1 {
        let mut wins = WINDOWS.lock().unwrap();
        for (win_id, ws) in wins.iter_mut() {
            if ws.tray.as_ref().map(|t| t.win) == Some(w) {
                // data.l[1]=time, data.l[4]=button
                handle_tray_button(ws, *win_id, data[4] as c_uint, data[1] as c_ulong);
                return;
            }
        }
        return;
    }
    let wm_delete = unsafe { (api.intern_atom)(d, b"WM_DELETE_WINDOW\0".as_ptr() as *const c_char, 0) };
    if message_type != wm_delete {
        return;
    }
    let mut wins = WINDOWS.lock().unwrap();
    for (win_id, ws) in wins.iter_mut() {
        if ws.window == w {
            push_event(*win_id, 0, "close", "");
            close_select_popup(api, d, ws);
            close_menu_popup(api, d, ws);
            unsafe {
                (api.destroy_window)(d, w);
                (api.flush)(d);
            }
            return;
        }
    }
}

fn handle_destroy(w: XWindow) {
    let mut wins = WINDOWS.lock().unwrap();
    let mut removed = None;
    for (win_id, ws) in wins.iter() {
        if ws.window == w {
            removed = Some(*win_id);
            break;
        }
    }
    if let Some(id) = removed {
        wins.remove(&id);
        return;
    }
    // 托盘窗口被销毁 → 仅清除托盘状态（不删主窗口）
    for (_, ws) in wins.iter_mut() {
        if ws.tray.as_ref().map(|t| t.win) == Some(w) {
            ws.tray = None;
            return;
        }
    }
}

fn handle_expose(api: &X11Api, d: *mut XDisplay, w: XWindow) {
    let wins = WINDOWS.lock().unwrap();
    for (_, ws) in wins.iter() {
        if ws.window == w {
            redraw(ws);
            return;
        }
        if let Some(t) = &ws.tray {
            if t.win == w {
                draw_tray_icon(ws, api, t);
                unsafe { (api.flush)(d); }
                return;
            }
        }
        if let Some(p) = &ws.select_popup {
            if p.win == w {
                draw_select_popup(ws, api, p);
                unsafe { (api.flush)(d); }
                return;
            }
        }
        if let Some(p) = &ws.menu_popup {
            if p.win == w {
                draw_menu_popup(ws, api, p);
                unsafe { (api.flush)(d); }
                return;
            }
        }
    }
}

fn process_event(api: &X11Api, d: *mut XDisplay, ev: &XEvent) {
    let etype = unsafe { ev.type_ };
    match etype {
        EXPOSE => {
            let e = unsafe { ev.xexpose };
            handle_expose(api, d, e.window);
        }
        BUTTON_PRESS => {
            let e = unsafe { ev.xbutton };
            handle_button(api, d, e.window, e.x, e.y, e.button, e.time);
        }
        KEY_PRESS => {
            let e = unsafe { ev.xkey };
            handle_key(api, d, &e);
        }
        CONFIGURE_NOTIFY => {
            let e = unsafe { ev.xconfigure };
            handle_configure(api, d, e.window, e.width, e.height);
        }
        CLIENT_MESSAGE => {
            let e = unsafe { ev.xclient };
            handle_client(api, d, e.window, e.message_type, e.data);
        }
        DESTROY_NOTIFY => {
            let e = unsafe { ev.xdestroy };
            handle_destroy(e.window);
        }
        FOCUS_IN | FOCUS_OUT => {}
        _ => {}
    }
}

/// 泵 X 事件（非阻塞）+ 取事件 JSON 数组。
fn poll(_span: Span, _file: &str, _src: &str) -> Result<Value, ZError> {
    let api = match API.as_ref() {
        Ok(a) => a,
        Err(_) => return Ok(Value::Str("[]".to_string())),
    };
    let d = match DISPLAY.as_ref() {
        Ok(dd) => *dd as *mut XDisplay,
        Err(_) => return Ok(Value::Str("[]".to_string())),
    };
    unsafe {
        while (api.pending)(d) > 0 {
            let mut ev: XEvent = std::mem::zeroed();
            (api.next_event)(d, &mut ev);
            process_event(api, d, &ev);
        }
    }
    let evs = EVENTS.lock().unwrap().clone();
    EVENTS.lock().unwrap().clear();
    Ok(Value::Str(format!("[{}]", evs.join(","))))
}

// ---------- 读写 ----------

fn set_text(win_id: i64, ctl_id: i64, text: &str, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    let mut wins = WINDOWS.lock().unwrap();
    let win = wins.get_mut(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
    let ctl = win.ctls.get_mut(&ctl_id).ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
    ctl.text = text.to_string();
    redraw(win);
    Ok(Value::Null)
}

fn get_text(win_id: i64, ctl_id: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    let wins = WINDOWS.lock().unwrap();
    let win = wins.get(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
    let ctl = win.ctls.get(&ctl_id).ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
    Ok(Value::Str(ctl.text.clone()))
}

fn set_value(win_id: i64, ctl_id: i64, val: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    let mut wins = WINDOWS.lock().unwrap();
    let win = wins.get_mut(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
    let ctl = win.ctls.get_mut(&ctl_id).ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
    if ctl.kind != "slider" {
        return Err(zerr(
            codes::TYPE_MISMATCH,
            format!("widget type `{}` does not support set_value (slider only)", ctl.kind),
            span,
            file,
            src,
            Some("set_value works on slider widgets"),
        ));
    }
    ctl.val = val;
    redraw(win);
    Ok(Value::Null)
}

fn get_value(win_id: i64, ctl_id: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    let wins = WINDOWS.lock().unwrap();
    let win = wins.get(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
    let ctl = win.ctls.get(&ctl_id).ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
    if ctl.kind != "slider" {
        return Err(zerr(
            codes::TYPE_MISMATCH,
            format!("widget type `{}` does not support get_value (slider only)", ctl.kind),
            span,
            file,
            src,
            Some("get_value works on slider widgets"),
        ));
    }
    Ok(Value::Int(ctl.val))
}

fn close(win_id: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    let api = match API.as_ref() {
        Ok(a) => a,
        Err(_) => return Err(x11_unavailable(span, file, src)),
    };
    let d = match DISPLAY.as_ref() {
        Ok(dd) => *dd as *mut XDisplay,
        Err(_) => return Err(x11_unavailable(span, file, src)),
    };
    let mut wins = WINDOWS.lock().unwrap();
    let win = wins.get_mut(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
    let w = win.window;
    close_select_popup(api, d, win);
    close_menu_popup(api, d, win);
    tray_remove_inner(api, d, win);
    unsafe {
        (api.destroy_window)(d, w);
        (api.flush)(d);
    }
    push_event(win_id, 0, "close", "");
    wins.remove(&win_id);
    Ok(Value::Null)
}

/// X11 弹窗：优先 zenity，其次 xmessage（纯 std::process，零依赖）。
fn msgbox(title: &str, msg: &str, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    let zenity = std::process::Command::new("zenity")
        .args(["--info", "--title", title, "--text", msg])
        .status();
    if zenity.is_ok() {
        return Ok(Value::Null);
    }
    let xmsg = std::process::Command::new("xmessage")
        .args(["-center", &format!("{}\n\n{}", title, msg)])
        .status();
    if xmsg.is_ok() {
        return Ok(Value::Null);
    }
    Err(zerr(
        codes::NOT_IMPLEMENTED,
        "guipro msgbox on X11 requires zenity or xmessage",
        span,
        file,
        src,
        Some("install zenity or xmessage"),
    ))
}

// ---------- 进阶控件读写 ----------

fn color_rgb(c: i64) -> (u16, u16, u16) {
    let r = ((c >> 16) & 0xFF) as u16;
    let g = ((c >> 8) & 0xFF) as u16;
    let b = (c & 0xFF) as u16;
    (r * 257, g * 257, b * 257)
}

fn table_add_row(win_id: i64, ctl_id: i64, row: &Value, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    let mut wins = WINDOWS.lock().unwrap();
    let win = wins.get_mut(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
    let ctl = win.ctls.get_mut(&ctl_id).ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
    if ctl.kind != "table" {
        return Err(zerr(codes::TYPE_MISMATCH, format!("widget {} is not a table", ctl_id), span, file, src, Some("create it with guipro_table")));
    }
    match row {
        Value::List(cells) => {
            let mut r = Vec::new();
            for c in cells {
                if let Value::Str(s) = c {
                    r.push(s.clone());
                }
            }
            ctl.rows.push(r);
        }
        _ => {
            return Err(zerr(codes::TYPE_MISMATCH, "table row must be a list of strings", span, file, src, Some("pass e.g. [\"a\", \"b\"]")));
        }
    }
    redraw(win);
    Ok(Value::Null)
}

fn table_clear(win_id: i64, ctl_id: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    let mut wins = WINDOWS.lock().unwrap();
    let win = wins.get_mut(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
    let ctl = win.ctls.get_mut(&ctl_id).ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
    if ctl.kind != "table" {
        return Err(zerr(codes::TYPE_MISMATCH, format!("widget {} is not a table", ctl_id), span, file, src, Some("create it with guipro_table")));
    }
    ctl.rows.clear();
    ctl.sel_row = -1;
    ctl.scroll = 0;
    redraw(win);
    Ok(Value::Null)
}

fn table_count(win_id: i64, ctl_id: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    let wins = WINDOWS.lock().unwrap();
    let win = wins.get(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
    let ctl = win.ctls.get(&ctl_id).ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
    if ctl.kind != "table" {
        return Err(zerr(codes::TYPE_MISMATCH, format!("widget {} is not a table", ctl_id), span, file, src, Some("create it with guipro_table")));
    }
    Ok(Value::Int(ctl.rows.len() as i64))
}

fn table_get(win_id: i64, ctl_id: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    let wins = WINDOWS.lock().unwrap();
    let win = wins.get(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
    let ctl = win.ctls.get(&ctl_id).ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
    if ctl.kind != "table" {
        return Err(zerr(codes::TYPE_MISMATCH, format!("widget {} is not a table", ctl_id), span, file, src, Some("create it with guipro_table")));
    }
    Ok(Value::Int(ctl.sel_row))
}

fn table_get_row(win_id: i64, ctl_id: i64, row: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    let wins = WINDOWS.lock().unwrap();
    let win = wins.get(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
    let ctl = win.ctls.get(&ctl_id).ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
    if ctl.kind != "table" {
        return Err(zerr(codes::TYPE_MISMATCH, format!("widget {} is not a table", ctl_id), span, file, src, Some("create it with guipro_table")));
    }
    if row < 0 || row >= ctl.rows.len() as i64 {
        return Err(zerr(codes::NOT_FOUND, format!("table row {} out of range (0..{})", row, ctl.rows.len()), span, file, src, Some("check the row index")));
    }
    let cells: Vec<Value> = ctl.rows[row as usize].iter().map(|s| Value::Str(s.clone())).collect();
    Ok(Value::List(cells))
}

fn table_set(win_id: i64, ctl_id: i64, row: i64, col: i64, text: &str, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    let mut wins = WINDOWS.lock().unwrap();
    let win = wins.get_mut(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
    let ctl = win.ctls.get_mut(&ctl_id).ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
    if ctl.kind != "table" {
        return Err(zerr(codes::TYPE_MISMATCH, format!("widget {} is not a table", ctl_id), span, file, src, Some("create it with guipro_table")));
    }
    if row >= 0 && row < ctl.rows.len() as i64 && col >= 0 {
        let r = &mut ctl.rows[row as usize];
        if (col as usize) < r.len() {
            r[col as usize] = text.to_string();
        }
    }
    redraw(win);
    Ok(Value::Null)
}

fn tree_add(win_id: i64, ctl_id: i64, parent_id: i64, label: &str, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    let mut wins = WINDOWS.lock().unwrap();
    let win = wins.get_mut(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
    let ctl = win.ctls.get_mut(&ctl_id).ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
    if ctl.kind != "tree" {
        return Err(zerr(codes::TYPE_MISMATCH, format!("widget {} is not a tree", ctl_id), span, file, src, Some("create it with guipro_tree")));
    }
    if parent_id != 0 && !ctl.nodes.contains_key(&parent_id) {
        return Err(zerr(codes::NOT_FOUND, format!("tree parent node {} does not exist", parent_id), span, file, src, Some("check the parent node id")));
    }
    let id = ctl.next_node;
    ctl.next_node += 1;
    let depth = if parent_id == 0 {
        0
    } else {
        ctl.nodes.get(&parent_id).map(|n| n.depth + 1).unwrap_or(0)
    };
    ctl.nodes.insert(id, TreeNode { id, label: label.to_string(), depth, expanded: true, has_children: false });
    if parent_id == 0 {
        ctl.roots.push(id);
    } else {
        ctl.children.entry(parent_id).or_insert_with(Vec::new).push(id);
    }
    redraw(win);
    Ok(Value::Int(id))
}

fn tree_clear(win_id: i64, ctl_id: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    let mut wins = WINDOWS.lock().unwrap();
    let win = wins.get_mut(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
    let ctl = win.ctls.get_mut(&ctl_id).ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
    if ctl.kind != "tree" {
        return Err(zerr(codes::TYPE_MISMATCH, format!("widget {} is not a tree", ctl_id), span, file, src, Some("create it with guipro_tree")));
    }
    ctl.nodes.clear();
    ctl.roots.clear();
    ctl.children.clear();
    ctl.next_node = 1;
    ctl.sel_node = -1;
    ctl.tree_scroll = 0;
    redraw(win);
    Ok(Value::Null)
}

fn tree_get(win_id: i64, ctl_id: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    let wins = WINDOWS.lock().unwrap();
    let win = wins.get(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
    let ctl = win.ctls.get(&ctl_id).ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
    if ctl.kind != "tree" {
        return Err(zerr(codes::TYPE_MISMATCH, format!("widget {} is not a tree", ctl_id), span, file, src, Some("create it with guipro_tree")));
    }
    Ok(Value::Int(ctl.sel_node))
}

fn canvas_push_shape(win_id: i64, ctl_id: i64, shape: Shape, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    let mut wins = WINDOWS.lock().unwrap();
    let win = wins.get_mut(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
    let ctl = win.ctls.get_mut(&ctl_id).ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
    if ctl.kind != "canvas" {
        return Err(zerr(codes::TYPE_MISMATCH, format!("widget {} is not a canvas", ctl_id), span, file, src, Some("create it with guipro_canvas")));
    }
    ctl.shapes.push(shape);
    redraw(win);
    Ok(Value::Null)
}

fn canvas_clear(win_id: i64, ctl_id: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    let mut wins = WINDOWS.lock().unwrap();
    let win = wins.get_mut(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
    let ctl = win.ctls.get_mut(&ctl_id).ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
    if ctl.kind != "canvas" {
        return Err(zerr(codes::TYPE_MISMATCH, format!("widget {} is not a canvas", ctl_id), span, file, src, Some("create it with guipro_canvas")));
    }
    ctl.shapes.clear();
    redraw(win);
    Ok(Value::Null)
}

fn canvas_repaint(win_id: i64, ctl_id: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    let mut wins = WINDOWS.lock().unwrap();
    let win = wins.get_mut(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
    let _ = win.ctls.get(&ctl_id).ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
    redraw(win);
    Ok(Value::Null)
}

fn menu(win_id: i64, items: &Value, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    let mut wins = WINDOWS.lock().unwrap();
    let win = wins.get_mut(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
    build_menu(win, items, span, file, src)?;
    redraw(win);
    Ok(Value::Null)
}

// ---------- 托盘图标（XEmbed 系统托盘协议） ----------

/// 绘制托盘图标（简单圆形）。
fn draw_tray_icon(ws: &WinState, api: &X11Api, tray: &TrayState) {
    let d = ws.display as *mut XDisplay;
    unsafe {
        (api.set_foreground)(d, ws.gc as *mut XGC, alloc_color(ws, api, COL_HILITE));
        (api.fill_arc)(d, tray.win, ws.gc as *mut XGC, 2, 2, 20, 20, 0, 360 * 64);
        (api.set_foreground)(d, ws.gc as *mut XGC, ws.black);
        (api.draw_arc)(d, tray.win, ws.gc as *mut XGC, 2, 2, 20, 20, 0, 360 * 64);
    }
}

fn tray_remove_inner(api: &X11Api, d: *mut XDisplay, ws: &mut WinState) {
    if let Some(t) = ws.tray.take() {
        unsafe {
            (api.destroy_window)(d, t.win);
            (api.flush)(d);
        }
    }
}

/// 添加托盘图标（XEmbed：找托盘管理器 selection owner，发 dock 请求，嵌入）。
fn tray_add(win_id: i64, tip: &str, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    let api = match API.as_ref() {
        Ok(a) => a,
        Err(_) => return Err(x11_unavailable(span, file, src)),
    };
    let d = match DISPLAY.as_ref() {
        Ok(dd) => *dd as *mut XDisplay,
        Err(_) => return Err(x11_unavailable(span, file, src)),
    };
    let mut wins = WINDOWS.lock().unwrap();
    let win = wins.get_mut(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
    if win.tray.is_some() {
        tray_remove_inner(api, d, win);
    }
    // 托盘管理器：_NET_SYSTEM_TRAY_S<screen> selection owner
    let sel_name = format!("_NET_SYSTEM_TRAY_S{}", win.screen);
    let sel_atom = unsafe { (api.intern_atom)(d, cstr(&sel_name).as_ptr(), 0) };
    let manager = unsafe { (api.get_selection_owner)(d, sel_atom) };
    if manager == 0 {
        return Err(zerr(
            codes::NOT_IMPLEMENTED,
            "no X11 system tray available (no _NET_SYSTEM_TRAY selection owner)",
            span,
            file,
            src,
            Some("start a panel/tray (GNOME Shell, KDE Plasma, xfce4-panel, stalonetray, ...)"),
        ));
    }
    // 创建托盘图标窗口（24x24，override_redirect 不被 WM 管理）
    let tray_win = unsafe {
        (api.create_simple_window)(d, win.root, 0, 0, 24, 24, 0, win.black, win.white)
    };
    if tray_win == 0 {
        return Err(zerr(codes::SYSCALL, "XCreateSimpleWindow failed for tray icon", span, file, src, None::<&str>));
    }
    unsafe {
        let mut attrs: XSetWindowAttributes = std::mem::zeroed();
        attrs.override_redirect = 1;
        (api.change_window_attributes)(d, tray_win, 0x200, &mut attrs);
        // _NET_WM_WINDOW_TYPE = DOCK
        let wm_type = (api.intern_atom)(d, b"_NET_WM_WINDOW_TYPE\0".as_ptr() as *const c_char, 0);
        let dock = (api.intern_atom)(d, b"_NET_WM_WINDOW_TYPE_DOCK\0".as_ptr() as *const c_char, 0);
        let atom_list: [XAtom; 1] = [dock];
        (api.change_property)(d, tray_win, wm_type, 4, 32, 0, atom_list.as_ptr() as *const c_uchar, 1);
        // ExposureMask | ButtonPressMask | StructureNotifyMask
        (api.select_input)(d, tray_win, 0x8000 | 0x4 | 0x80000);
        // 发送 SYSTEM_TRAY_REQUEST_DOCK(0) 给托盘管理器
        let opcode = (api.intern_atom)(d, b"_NET_SYSTEM_TRAY_OPCODE\0".as_ptr() as *const c_char, 0);
        let mut ev: XEvent = std::mem::zeroed();
        ev.xclient = XClientMessageEvent {
            type_: CLIENT_MESSAGE,
            serial: 0,
            send_event: 1,
            display: d,
            window: manager,
            message_type: opcode,
            format: 32,
            data: [0, tray_win as c_long, 0, 0, 0],
        };
        (api.send_event)(d, manager, 0, 0, &mut ev);
        (api.map_window)(d, tray_win);
        (api.flush)(d);
    }
    win.tray = Some(TrayState { win: tray_win, tip: tip.to_string(), last_click: 0 });
    Ok(Value::Null)
}

fn tray_tip(win_id: i64, tip: &str, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    let mut wins = WINDOWS.lock().unwrap();
    let win = wins.get_mut(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
    if let Some(t) = win.tray.as_mut() {
        t.tip = tip.to_string();
    }
    Ok(Value::Null)
}

fn tray_remove(win_id: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    let api = match API.as_ref() {
        Ok(a) => a,
        Err(_) => return Err(x11_unavailable(span, file, src)),
    };
    let d = match DISPLAY.as_ref() {
        Ok(dd) => *dd as *mut XDisplay,
        Err(_) => return Err(x11_unavailable(span, file, src)),
    };
    let mut wins = WINDOWS.lock().unwrap();
    let win = wins.get_mut(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
    tray_remove_inner(api, d, win);
    Ok(Value::Null)
}

// ---------- 内置函数分发 ----------

pub fn call(name: &str, args: &[Value], span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    match name {
        "guipro.available" => Ok(Value::Bool(DISPLAY.as_ref().is_ok())),
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
        // ---------- 表 ----------
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
        // ---------- 树 ----------
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
        // ---------- 画布 ----------
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
            let x1 = as_int(&args[2], 2, span, file, src)? as c_int;
            let y1 = as_int(&args[3], 3, span, file, src)? as c_int;
            let x2 = as_int(&args[4], 4, span, file, src)? as c_int;
            let y2 = as_int(&args[5], 5, span, file, src)? as c_int;
            let color = color_rgb(as_int(&args[6], 6, span, file, src)?);
            canvas_push_shape(win, ctl, Shape { kind: "line".to_string(), x1, y1, x2, y2, text: String::new(), color, fill: false }, span, file, src)
        }
        "guipro.canvas_rect" => {
            if args.len() != 8 {
                return Err(arg_count(name, 8, args.len(), span, file, src));
            }
            let win = as_int(&args[0], 0, span, file, src)?;
            let ctl = as_int(&args[1], 1, span, file, src)?;
            let x = as_int(&args[2], 2, span, file, src)? as c_int;
            let y = as_int(&args[3], 3, span, file, src)? as c_int;
            let w = as_int(&args[4], 4, span, file, src)? as c_int;
            let h = as_int(&args[5], 5, span, file, src)? as c_int;
            let color = color_rgb(as_int(&args[6], 6, span, file, src)?);
            let fill = as_int(&args[7], 7, span, file, src)? != 0;
            canvas_push_shape(win, ctl, Shape { kind: "rect".to_string(), x1: x, y1: y, x2: x + w, y2: y + h, text: String::new(), color, fill }, span, file, src)
        }
        "guipro.canvas_ellipse" => {
            if args.len() != 8 {
                return Err(arg_count(name, 8, args.len(), span, file, src));
            }
            let win = as_int(&args[0], 0, span, file, src)?;
            let ctl = as_int(&args[1], 1, span, file, src)?;
            let x = as_int(&args[2], 2, span, file, src)? as c_int;
            let y = as_int(&args[3], 3, span, file, src)? as c_int;
            let w = as_int(&args[4], 4, span, file, src)? as c_int;
            let h = as_int(&args[5], 5, span, file, src)? as c_int;
            let color = color_rgb(as_int(&args[6], 6, span, file, src)?);
            let fill = as_int(&args[7], 7, span, file, src)? != 0;
            canvas_push_shape(win, ctl, Shape { kind: "ellipse".to_string(), x1: x, y1: y, x2: x + w, y2: y + h, text: String::new(), color, fill }, span, file, src)
        }
        "guipro.canvas_text" => {
            if args.len() != 6 {
                return Err(arg_count(name, 6, args.len(), span, file, src));
            }
            let win = as_int(&args[0], 0, span, file, src)?;
            let ctl = as_int(&args[1], 1, span, file, src)?;
            let x = as_int(&args[2], 2, span, file, src)? as c_int;
            let y = as_int(&args[3], 3, span, file, src)? as c_int;
            let text = as_str(&args[4], 4, span, file, src)?;
            let color = color_rgb(as_int(&args[5], 5, span, file, src)?);
            canvas_push_shape(win, ctl, Shape { kind: "text".to_string(), x1: x, y1: y, x2: 0, y2: 0, text: text.to_string(), color, fill: false }, span, file, src)
        }
        "guipro.canvas_repaint" => {
            if args.len() != 2 {
                return Err(arg_count(name, 2, args.len(), span, file, src));
            }
            let win = as_int(&args[0], 0, span, file, src)?;
            let ctl = as_int(&args[1], 1, span, file, src)?;
            canvas_repaint(win, ctl, span, file, src)
        }
        // ---------- 菜单 ----------
        "guipro.menu" => {
            if args.len() != 2 {
                return Err(arg_count(name, 2, args.len(), span, file, src));
            }
            let win = as_int(&args[0], 0, span, file, src)?;
            menu(win, &args[1], span, file, src)
        }
        // ---------- 托盘（XEmbed 系统托盘） ----------
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

/// X11 后端是否可用（供 guimod.rs 的 GTK 优先、X11 兜底分发使用）。
pub fn available() -> bool {
    DISPLAY.as_ref().is_ok()
}



