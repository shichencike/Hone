// guimod_gtk.rs - guipro 的 Linux 原生后端（GTK3 动态加载 + X11 降级）
//
// 设计：与 sqlitemod.rs 相同的「运行时动态加载」模式——不链接任何 GTK 静态库，
// 通过 libloading 加载 libgtk-3.so 并按 C ABI transmute 函数指针。因此本模块
// 不依赖 Linux 系统头文件，可在任意平台编译（本机 Windows 亦可 cargo check）。
// 运行时要求：Linux 桌面环境已安装 GTK3（libgtk-3.so.0）；缺失时 guipro.available
// 返回 false，其余函数报 H999 并提示安装。
//
// 与 Win32 后端对齐的接口（guipro.*，Hone 层 guipro.hn 已统一封装）：
//   guipro.available()   -> bool
//   guipro.window(t,w,h) -> int
//   guipro.add(win,dict) -> int
//   guipro.poll()        -> str
//   guipro.set_text / get_text / close / msgbox
//
// 布局：GTK 用垂直 GtkBox 自动排布（Hone 层传入的 x/y 坐标被忽略，w/h 作为
// 控件最小宽度/高度提示）。控件 dict 的 type 映射到 GTK 原生控件：
//   button → GtkButton / label → GtkLabel / input → GtkEntry /
//   select → GtkComboBoxText / checkbox、radio → GtkToggleButton
//
// 事件：GTK 信号回调（extern "C"）把事件推入全局队列，guipro.poll() 先泵
// GTK 事件循环（gtk_events_pending + gtk_main_iteration_do 非阻塞）再取队列，
// 与 Win32 的 PeekMessage 泵消息模型一致；闭包分发仍在 Hone 层完成。
//
// X11 降级：GTK3 不可用且 X11 存在时，仅提供 msgbox（zenity/xmessage）与
// available=false 提示；完整 X11 自绘控件留待后续（体积与收益权衡）。

use crate::error::codes;
use crate::error::ZError;
use crate::interp::Value;
use crate::lexer::Span;
use libloading::Library;
use std::collections::HashMap;
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
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

// ---------- GTK3 C ABI 函数指针表 ----------

type GtkInitFn = unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char);
type GtkWindowNewFn = unsafe extern "C" fn(c_int) -> *mut c_void;
type GtkWindowSetTitleFn = unsafe extern "C" fn(*mut c_void, *const c_char);
type GtkWindowSetDefaultSizeFn = unsafe extern "C" fn(*mut c_void, c_int, c_int);
type GtkWidgetShowAllFn = unsafe extern "C" fn(*mut c_void);
type GtkWidgetDestroyFn = unsafe extern "C" fn(*mut c_void);
type GtkBoxNewFn = unsafe extern "C" fn(c_int, c_int) -> *mut c_void;
type GtkBoxPackStartFn = unsafe extern "C" fn(*mut c_void, *mut c_void, c_int, c_int, u32);
type GtkContainerAddFn = unsafe extern "C" fn(*mut c_void, *mut c_void);
type GtkButtonNewWithLabelFn = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type GtkLabelNewFn = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type GtkEntryNewFn = unsafe extern "C" fn() -> *mut c_void;
type GtkEntrySetTextFn = unsafe extern "C" fn(*mut c_void, *const c_char);
type GtkEntryGetTextFn = unsafe extern "C" fn(*mut c_void) -> *const c_char;
type GtkLabelSetTextFn = unsafe extern "C" fn(*mut c_void, *const c_char);
type GtkLabelGetTextFn = unsafe extern "C" fn(*mut c_void) -> *const c_char;
type GtkButtonSetLabelFn = unsafe extern "C" fn(*mut c_void, *const c_char);
type GtkButtonGetLabelFn = unsafe extern "C" fn(*mut c_void) -> *const c_char;
type GtkComboTextNewFn = unsafe extern "C" fn() -> *mut c_void;
type GtkComboTextAppendFn = unsafe extern "C" fn(*mut c_void, *const c_char);
type GtkComboTextGetActiveTextFn = unsafe extern "C" fn(*mut c_void) -> *mut c_char;
type GtkComboSetActiveFn = unsafe extern "C" fn(*mut c_void, c_int);
type GtkCheckNewWithLabelFn = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type GtkRadioNewWithLabelFn = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void;
type GtkRadioGetGroupFn = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type GtkToggleGetActiveFn = unsafe extern "C" fn(*mut c_void) -> c_int;
type GtkToggleSetActiveFn = unsafe extern "C" fn(*mut c_void, c_int);
type GSignalConnectFn = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_void, *mut c_void, *const c_void, u32) -> u64;
type GtkEventsPendingFn = unsafe extern "C" fn() -> c_int;
type GtkMainIterationDoFn = unsafe extern "C" fn(c_int) -> c_int;
type GtkMessageDialogNewFn = unsafe extern "C" fn(*mut c_void, c_int, c_int, c_int, *const c_char) -> *mut c_void;
type GtkDialogRunFn = unsafe extern "C" fn(*mut c_void) -> c_int;
type GFreeFn = unsafe extern "C" fn(*mut c_void);

struct GtkApi {
    _lib: Library,
    init: GtkInitFn,
    window_new: GtkWindowNewFn,
    window_set_title: GtkWindowSetTitleFn,
    window_set_default_size: GtkWindowSetDefaultSizeFn,
    widget_show_all: GtkWidgetShowAllFn,
    widget_destroy: GtkWidgetDestroyFn,
    box_new: GtkBoxNewFn,
    box_pack_start: GtkBoxPackStartFn,
    container_add: GtkContainerAddFn,
    button_new_with_label: GtkButtonNewWithLabelFn,
    label_new: GtkLabelNewFn,
    entry_new: GtkEntryNewFn,
    entry_set_text: GtkEntrySetTextFn,
    entry_get_text: GtkEntryGetTextFn,
    label_set_text: GtkLabelSetTextFn,
    label_get_text: GtkLabelGetTextFn,
    button_set_label: GtkButtonSetLabelFn,
    button_get_label: GtkButtonGetLabelFn,
    combo_text_new: GtkComboTextNewFn,
    combo_text_append: GtkComboTextAppendFn,
    combo_text_get_active_text: GtkComboTextGetActiveTextFn,
    combo_set_active: GtkComboSetActiveFn,
    check_new_with_label: GtkCheckNewWithLabelFn,
    radio_new_with_label: GtkRadioNewWithLabelFn,
    radio_get_group: GtkRadioGetGroupFn,
    toggle_get_active: GtkToggleGetActiveFn,
    toggle_set_active: GtkToggleSetActiveFn,
    signal_connect: GSignalConnectFn,
    events_pending: GtkEventsPendingFn,
    main_iteration_do: GtkMainIterationDoFn,
    message_dialog_new: GtkMessageDialogNewFn,
    dialog_run: GtkDialogRunFn,
    g_free: GFreeFn,
}

/// 加载 libgtk-3（候选名依次尝试）。
fn load_api() -> Result<GtkApi, String> {
    let candidates: &[&str] = &["libgtk-3.so.0", "libgtk-3.so"];
    let mut last_err = String::from("no GTK3 library found");
    for name in candidates {
        let lib = unsafe { Library::new(name) };
        let lib = match lib {
            Ok(l) => l,
            Err(e) => {
                last_err = format!("{}: {}", name, e);
                continue;
            }
        };
        let get = |sym: &[u8]| unsafe { lib.get::<*mut c_void>(sym).map(|s| *s).map_err(|e| format!("symbol {:?}: {}", String::from_utf8_lossy(sym), e)) };
        let api = GtkApi {
            init: unsafe { std::mem::transmute(get(b"gtk_init\0")?) },
            window_new: unsafe { std::mem::transmute(get(b"gtk_window_new\0")?) },
            window_set_title: unsafe { std::mem::transmute(get(b"gtk_window_set_title\0")?) },
            window_set_default_size: unsafe { std::mem::transmute(get(b"gtk_window_set_default_size\0")?) },
            widget_show_all: unsafe { std::mem::transmute(get(b"gtk_widget_show_all\0")?) },
            widget_destroy: unsafe { std::mem::transmute(get(b"gtk_widget_destroy\0")?) },
            box_new: unsafe { std::mem::transmute(get(b"gtk_box_new\0")?) },
            box_pack_start: unsafe { std::mem::transmute(get(b"gtk_box_pack_start\0")?) },
            container_add: unsafe { std::mem::transmute(get(b"gtk_container_add\0")?) },
            button_new_with_label: unsafe { std::mem::transmute(get(b"gtk_button_new_with_label\0")?) },
            label_new: unsafe { std::mem::transmute(get(b"gtk_label_new\0")?) },
            entry_new: unsafe { std::mem::transmute(get(b"gtk_entry_new\0")?) },
            entry_set_text: unsafe { std::mem::transmute(get(b"gtk_entry_set_text\0")?) },
            entry_get_text: unsafe { std::mem::transmute(get(b"gtk_entry_get_text\0")?) },
            label_set_text: unsafe { std::mem::transmute(get(b"gtk_label_set_text\0")?) },
            label_get_text: unsafe { std::mem::transmute(get(b"gtk_label_get_text\0")?) },
            button_set_label: unsafe { std::mem::transmute(get(b"gtk_button_set_label\0")?) },
            button_get_label: unsafe { std::mem::transmute(get(b"gtk_button_get_label\0")?) },
            combo_text_new: unsafe { std::mem::transmute(get(b"gtk_combo_box_text_new\0")?) },
            combo_text_append: unsafe { std::mem::transmute(get(b"gtk_combo_box_text_append_text\0")?) },
            combo_text_get_active_text: unsafe { std::mem::transmute(get(b"gtk_combo_box_text_get_active_text\0")?) },
            combo_set_active: unsafe { std::mem::transmute(get(b"gtk_combo_box_set_active\0")?) },
            check_new_with_label: unsafe { std::mem::transmute(get(b"gtk_check_button_new_with_label\0")?) },
            radio_new_with_label: unsafe { std::mem::transmute(get(b"gtk_radio_button_new_with_label\0")?) },
            radio_get_group: unsafe { std::mem::transmute(get(b"gtk_radio_button_get_group\0")?) },
            toggle_get_active: unsafe { std::mem::transmute(get(b"gtk_toggle_button_get_active\0")?) },
            toggle_set_active: unsafe { std::mem::transmute(get(b"gtk_toggle_button_set_active\0")?) },
            signal_connect: unsafe { std::mem::transmute(get(b"g_signal_connect_data\0")?) },
            events_pending: unsafe { std::mem::transmute(get(b"gtk_events_pending\0")?) },
            main_iteration_do: unsafe { std::mem::transmute(get(b"gtk_main_iteration_do\0")?) },
            message_dialog_new: unsafe { std::mem::transmute(get(b"gtk_message_dialog_new\0")?) },
            dialog_run: unsafe { std::mem::transmute(get(b"gtk_dialog_run\0")?) },
            g_free: unsafe { std::mem::transmute(get(b"g_free\0")?) },
            _lib: lib,
        };
        return Ok(api);
    }
    Err(last_err)
}

static API: std::sync::LazyLock<Result<GtkApi, String>> = std::sync::LazyLock::new(load_api);

// ---------- 全局状态（与 guimod.rs 同构） ----------

static NEXT_WIN_ID: AtomicI64 = AtomicI64::new(1);
static NEXT_CTL_ID: AtomicI64 = AtomicI64::new(1);
static EVENTS: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// 窗口 id → 窗口状态（GtkWidget* 存 usize 规避裸指针 Send 限制）
static WINDOWS: std::sync::LazyLock<Mutex<HashMap<i64, WinState>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

struct WinState {
    /// GtkWindow*
    widget: usize,
    /// 垂直排布的 GtkBox*（所有控件 pack 到此容器）
    box_widget: usize,
    /// 控件 id → (类型, GtkWidget*)
    ctls: HashMap<i64, (String, usize)>,
}

unsafe impl Send for WinState {}

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

/// GTK 信号回调包装：user_data 传控件 id（usize），经全局表还原窗口/类型。
unsafe extern "C" fn cb_simple(widget: *mut c_void, user_data: *mut c_void) {
    let ctl_id = user_data as usize as i64;
    let wins = WINDOWS.lock().unwrap();
    for (win_id, s) in wins.iter() {
        if let Some((kind, w)) = s.ctls.get(&ctl_id) {
            if *w == widget as usize {
                match kind.as_str() {
                    "button" => push_event(*win_id, ctl_id, "click", ""),
                    "checkbox" | "radio" => {
                        if let Ok(api) = API.as_ref() {
                            let active = (api.toggle_get_active)(widget);
                            push_event(*win_id, ctl_id, "change", if active != 0 { "1" } else { "0" });
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

/// entry/combo 的 change 回调：读取当前文本后推送。
unsafe extern "C" fn cb_change(widget: *mut c_void, user_data: *mut c_void) {
    let ctl_id = user_data as usize as i64;
    let wins = WINDOWS.lock().unwrap();
    for (win_id, s) in wins.iter() {
        if let Some((kind, w)) = s.ctls.get(&ctl_id) {
            if *w == widget as usize {
                let mut text = String::new();
                if let Ok(api) = API.as_ref() {
                    if kind == "input" {
                        let p = (api.entry_get_text)(widget);
                        if !p.is_null() {
                            text = std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned();
                        }
                    } else if kind == "select" {
                        let p = (api.combo_text_get_active_text)(widget);
                        if !p.is_null() {
                            text = std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned();
                            (api.g_free)(p as *mut c_void);
                        }
                    }
                }
                push_event(*win_id, ctl_id, "change", &text);
            }
        }
    }
}

/// 窗口 destroy 信号：推送 close 并从注册表移除。
unsafe extern "C" fn cb_window_destroy(widget: *mut c_void, _user_data: *mut c_void) {
    let mut wins = WINDOWS.lock().unwrap();
    let mut removed = None;
    for (win_id, s) in wins.iter() {
        if s.widget == widget as usize {
            push_event(*win_id, 0, "close", "");
            removed = Some(*win_id);
            break;
        }
    }
    if let Some(id) = removed {
        wins.remove(&id);
    }
}

// ---------- 内置函数实现 ----------

pub fn call(name: &str, args: &[Value], span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    match name {
        "guipro.available" => Ok(Value::Bool(API.as_ref().is_ok())),
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

fn gtk_unavailable(span: Span, file: &str, src: &str) -> ZError {
    let detail = API.as_ref().err().cloned().unwrap_or_else(|| String::from("unknown error"));
    zerr(
        codes::NOT_IMPLEMENTED,
        format!("guipro requires GTK3 on Linux, but loading failed: {}", detail),
        span,
        file,
        src,
        Some("install GTK3 (e.g. `sudo apt install libgtk-3-0` or `sudo dnf install gtk3`)"),
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
    let api = API.as_ref().map_err(|_| gtk_unavailable(span, file, src))?;
    let title_c = CString::new(title).map_err(|_| {
        zerr(codes::TYPE_MISMATCH, "window title contains NUL", span, file, src, None::<&str>)
    })?;
    let win_id = NEXT_WIN_ID.fetch_add(1, Ordering::Relaxed);
    unsafe {
        let win_widget = (api.window_new)(0); // GTK_WINDOW_TOPLEVEL
        if win_widget.is_null() {
            return Err(zerr(codes::SYSCALL, "gtk_window_new failed", span, file, src, None::<&str>));
        }
        (api.window_set_title)(win_widget, title_c.as_ptr());
        (api.window_set_default_size)(win_widget, w as c_int, h as c_int);
        // 垂直排布容器：所有控件 pack 到 box
        let box_widget = (api.box_new)(1, 6); // GTK_ORIENTATION_VERTICAL
        (api.container_add)(win_widget, box_widget);
        WINDOWS.lock().unwrap().insert(
            win_id,
            WinState {
                widget: win_widget as usize,
                box_widget: box_widget as usize,
                ctls: HashMap::new(),
            },
        );
        // 关闭事件（点 X 触发 destroy）
        let sig = CString::new("destroy").unwrap();
        (api.signal_connect)(win_widget, sig.as_ptr(), cb_window_destroy as *const c_void, ptr::null_mut(), ptr::null(), 0);
        (api.widget_show_all)(win_widget);
    }
    Ok(Value::Int(win_id))
}

/// 添加控件，返回控件 id。GTK 布局：pack 进窗口的垂直 box（忽略 x/y）。
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
    let api = API.as_ref().map_err(|_| gtk_unavailable(span, file, src))?;
    let text = dict_str(d, "text", span, file, src)?;
    let mut wins = WINDOWS.lock().unwrap();
    let win = wins.get_mut(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
    let ctl_id = next_ctl_id();
    let widget = unsafe {
        let w = match kind.as_str() {
            "button" => (api.button_new_with_label)(cstr(&text).as_ptr()),
            "label" => (api.label_new)(cstr(&text).as_ptr()),
            "input" => (api.entry_new)(),
            "select" => (api.combo_text_new)(),
            "checkbox" => (api.check_new_with_label)(cstr(&text).as_ptr()),
            "radio" => {
                // 同窗口内多个 radio 自动成组（以第一个 radio 的 group 为参照）
                let mut group: *mut c_void = ptr::null_mut();
                for (k, (k2, w2)) in win.ctls.iter() {
                    if k2 == "radio" {
                        group = (api.radio_get_group)(*w2 as *mut c_void);
                        let _ = k;
                        break;
                    }
                }
                let r = (api.radio_new_with_label)(group, cstr(&text).as_ptr());
                r
            }
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
        if w.is_null() {
            return Err(zerr(codes::SYSCALL, format!("gtk widget create failed for `{}`", kind), span, file, src, None::<&str>));
        }
        // select 填充选项
        if kind == "select" {
            if let Some(Value::List(opts)) = dict_get(d, "options") {
                for o in opts {
                    if let Value::Str(s) = o {
                        (api.combo_text_append)(w, cstr(s).as_ptr());
                    }
                }
                (api.combo_set_active)(w, 0);
            }
        }
        // 信号连接：button/checkbox/radio → clicked；input/select → changed
        let sig_name = match kind.as_str() {
            "input" | "select" => "changed",
            _ => "clicked",
        };
        let sig = CString::new(sig_name).unwrap();
        let cb: *const c_void = match kind.as_str() {
            "input" | "select" => cb_change as *const c_void,
            _ => cb_simple as *const c_void,
        };
        (api.signal_connect)(w, sig.as_ptr(), cb, ctl_id as *mut c_void, ptr::null(), 0);
        // pack 进容器
        (api.box_pack_start)(win.box_widget as *mut c_void, w, 0, 1, 2);
        w
    };
    win.ctls.insert(ctl_id, (kind, widget as usize));
    Ok(Value::Int(ctl_id))
}

/// 泵 GTK 事件循环（非阻塞）+ 取事件 JSON 数组。
fn poll(_span: Span, _file: &str, _src: &str) -> Result<Value, ZError> {
    if let Ok(api) = API.as_ref() {
        unsafe {
            let mut guard = 0;
            while (api.events_pending)() != 0 && guard < 1000 {
                (api.main_iteration_do)(0); // FALSE = 非阻塞
                guard += 1;
            }
        }
    }
    let evs = EVENTS.lock().unwrap().clone();
    EVENTS.lock().unwrap().clear();
    Ok(Value::Str(format!("[{}]", evs.join(","))))
}

fn set_text(win_id: i64, ctl_id: i64, text: &str, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    let api = API.as_ref().map_err(|_| gtk_unavailable(span, file, src))?;
    let wins = WINDOWS.lock().unwrap();
    let win = wins.get(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
    let (kind, w) = win.ctls.get(&ctl_id).ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
    unsafe {
        match kind.as_str() {
            "input" => (api.entry_set_text)(*w as *mut c_void, cstr(text).as_ptr()),
            "label" => (api.label_set_text)(*w as *mut c_void, cstr(text).as_ptr()),
            "button" => (api.button_set_label)(*w as *mut c_void, cstr(text).as_ptr()),
            _ => {
                return Err(zerr(
                    codes::TYPE_MISMATCH,
                    format!("widget type `{}` does not support set_text", kind),
                    span,
                    file,
                    src,
                    Some("set_text works on input/label/button"),
                ));
            }
        }
    }
    Ok(Value::Null)
}

fn get_text(win_id: i64, ctl_id: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    let api = API.as_ref().map_err(|_| gtk_unavailable(span, file, src))?;
    let wins = WINDOWS.lock().unwrap();
    let win = wins.get(&win_id).ok_or_else(|| win_missing(win_id, span, file, src))?;
    let (kind, w) = win.ctls.get(&ctl_id).ok_or_else(|| ctl_missing(win_id, ctl_id, span, file, src))?;
    let text = unsafe {
        match kind.as_str() {
            "input" => ptr_to_string((api.entry_get_text)(*w as *mut c_void)),
            "label" => ptr_to_string((api.label_get_text)(*w as *mut c_void)),
            "button" => ptr_to_string((api.button_get_label)(*w as *mut c_void)),
            "select" => {
                let p = (api.combo_text_get_active_text)(*w as *mut c_void);
                let s = ptr_to_string(p);
                if !p.is_null() {
                    (api.g_free)(p as *mut c_void);
                }
                s
            }
            "checkbox" | "radio" => {
                let active = (api.toggle_get_active)(*w as *mut c_void);
                if active != 0 { "1".to_string() } else { "0".to_string() }
            }
            other => {
                return Err(zerr(
                    codes::TYPE_MISMATCH,
                    format!("widget type `{}` does not support get_text", other),
                    span,
                    file,
                    src,
                    Some("get_text works on input/label/button/select/checkbox/radio"),
                ));
            }
        }
    };
    Ok(Value::Str(text))
}

fn close(win_id: i64, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    let api = API.as_ref().map_err(|_| gtk_unavailable(span, file, src))?;
    let widget = {
        let wins = WINDOWS.lock().unwrap();
        wins.get(&win_id).map(|w| w.widget).ok_or_else(|| win_missing(win_id, span, file, src))?
    };
    unsafe {
        (api.widget_destroy)(widget as *mut c_void);
    }
    // destroy 信号已推送 close 事件；此处不再重复推送
    Ok(Value::Null)
}

fn msgbox(title: &str, msg: &str, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    // X11 降级：GTK3 不可用时用 zenity / xmessage 命令行弹窗（纯 std::process，零依赖）
    if API.as_ref().is_err() {
        return msgbox_x11(title, msg, span, file, src);
    }
    let api = API.as_ref().map_err(|_| gtk_unavailable(span, file, src))?;
    let title_c = CString::new(title).unwrap_or_else(|_| CString::new("").unwrap());
    // gtk_message_dialog_new 的 format 是 printf 风格且为可变参数；Rust 侧按固定
    // 5 参数签名调用，无法传 %s 的实参，故把 msg 中的 % 全部转义为 %% 后直接作
    // format 传入，输出即原文（GTK 内部 g_strdup_vprintf 处理 %% → %）。
    let escaped = msg.replace('%', "%%");
    let fmt = CString::new(escaped).unwrap_or_else(|_| CString::new("").unwrap());
    unsafe {
        let dlg = (api.message_dialog_new)(
            ptr::null_mut(),
            0, // GTK_DIALOG_MODAL
            0, // GTK_MESSAGE_INFO
            1, // GTK_BUTTONS_OK
            fmt.as_ptr(),
        );
        if dlg.is_null() {
            return Err(zerr(codes::SYSCALL, "gtk_message_dialog_new failed", span, file, src, None::<&str>));
        }
        // 对话框也是 GtkWindow，设置标题
        (api.window_set_title)(dlg, title_c.as_ptr());
        (api.dialog_run)(dlg);
        (api.widget_destroy)(dlg);
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

/// X11 降级弹窗：优先 zenity，其次 xmessage（纯 std::process，不引入依赖）。
fn msgbox_x11(title: &str, msg: &str, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    // zenity 需要 GTK 运行时但通常随桌面安装；xmessage 是 X11 自带（xorg 基础包）。
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
        "guipro on Linux requires GTK3 or a message-box tool (zenity/xmessage) for dialogs",
        span,
        file,
        src,
        Some("install GTK3 (`sudo apt install libgtk-3-0` / `sudo dnf install gtk3`), or zenity/xmessage"),
    ))
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

fn ptr_to_string(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        unsafe { std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned() }
    }
}
