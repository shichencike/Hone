// sqlitemod.rs - SQLite 轻量封装（sqlite.* 内置函数）
// 策略：运行时通过 libloading 加载系统 libsqlite3（Windows: sqlite3.dll，
// Linux/macOS/Termux: libsqlite3.so / .dylib），保持零 C 构建依赖、纯 Rust 编译，
// 与项目「核心依赖仅 std + libloading」的定位一致。库缺失时给出明确报错。
//
//   sqlite.open(path)            -> int   打开数据库，返回句柄（失败报 H301）
//   sqlite.close(handle)         -> bool  关闭数据库
//   sqlite.exec(handle, sql)     -> bool  执行无返回行的 SQL（建表/插入/更新/删除）
//   sqlite.query(handle, sql)    -> list  查询，返回 dict 列表（列名 -> 值）
//   sqlite.query_one(handle, sql)-> dict|null 查询单行
//   sqlite.escape(str)           -> str   转义字符串字面量中的单引号
//   sqlite.last_insert_id(h)     -> int   最近一次 INSERT 的行 id
//   sqlite.changes(h)            -> int   最近一次语句影响的行数
//
// 类型映射：INTEGER -> int，REAL -> float，TEXT -> str，NULL -> null，BLOB -> str（按 UTF-8 读出）。
// 说明：值绑定（参数化查询）暂未实现，动态值请用 sqlite.escape 拼接（见示例）。

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::Mutex;

use once_cell::sync::Lazy;

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
            format!("`sqlite.*` expects a string for argument {}, got `{}`", arg + 1, other.type_name()),
            span,
            file,
            src,
            None::<&str>,
        )),
    }
}

fn as_int(v: &Value, arg: usize, span: Span, file: &str, src: &str) -> Result<i64, ZError> {
    match v {
        Value::Int(i) => Ok(*i),
        other => Err(zerr(
            codes::TYPE_MISMATCH,
            format!("`sqlite.*` expects an int for argument {}, got `{}`", arg + 1, other.type_name()),
            span,
            file,
            src,
            None::<&str>,
        )),
    }
}

// ---------- FFI 类型别名 ----------

type OpenFn = unsafe extern "C" fn(*const c_char, *mut *mut c_void, c_int, *const c_char) -> c_int;
type CloseFn = unsafe extern "C" fn(*mut c_void) -> c_int;
type ExecFn = unsafe extern "C" fn(
    *mut c_void,
    *const c_char,
    Option<unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int>,
    *mut c_void,
    *mut *mut c_char,
) -> c_int;
type PrepareFn = unsafe extern "C" fn(*mut c_void, *const c_char, c_int, *mut *mut c_void, *mut *const c_char) -> c_int;
type StepFn = unsafe extern "C" fn(*mut c_void) -> c_int;
type ColumnCountFn = unsafe extern "C" fn(*mut c_void) -> c_int;
type ColumnNameFn = unsafe extern "C" fn(*mut c_void, c_int) -> *const c_char;
type ColumnTypeFn = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type ColumnInt64Fn = unsafe extern "C" fn(*mut c_void, c_int) -> i64;
type ColumnDoubleFn = unsafe extern "C" fn(*mut c_void, c_int) -> f64;
type ColumnTextFn = unsafe extern "C" fn(*mut c_void, c_int) -> *const c_char;
type FinalizeFn = unsafe extern "C" fn(*mut c_void) -> c_int;
type ErrMsgFn = unsafe extern "C" fn(*mut c_void) -> *const c_char;
type LastInsertIdFn = unsafe extern "C" fn(*mut c_void) -> i64;
type ChangesFn = unsafe extern "C" fn(*mut c_void) -> c_int;

/// SQLite 动态库句柄 + 函数指针（Library 保持存活，函数指针无借用）。
struct SqliteApi {
    _lib: libloading::Library,
    open: OpenFn,
    close: CloseFn,
    exec: ExecFn,
    prepare: PrepareFn,
    step: StepFn,
    column_count: ColumnCountFn,
    column_name: ColumnNameFn,
    column_type: ColumnTypeFn,
    column_int64: ColumnInt64Fn,
    column_double: ColumnDoubleFn,
    column_text: ColumnTextFn,
    finalize: FinalizeFn,
    errmsg: ErrMsgFn,
    last_insert_id: LastInsertIdFn,
    changes: ChangesFn,
}

/// 加载 sqlite3 动态库（平台名列表依次尝试）。
fn load_api() -> Result<SqliteApi, String> {
    let candidates: &[&str] = if cfg!(windows) {
        &["sqlite3.dll"]
    } else if cfg!(target_os = "macos") {
        &["libsqlite3.dylib", "libsqlite3.so"]
    } else {
        &["libsqlite3.so", "libsqlite3.so.0"]
    };
    let mut last_err = String::from("no sqlite3 library found");
    for name in candidates {
        let lib = unsafe { libloading::Library::new(name) };
        let lib = match lib {
            Ok(l) => l,
            Err(e) => {
                last_err = format!("{}: {}", name, e);
                continue;
            }
        };
        let get = |sym: &[u8]| unsafe { lib.get::<*mut c_void>(sym).map(|s| *s).map_err(|e| format!("symbol {:?}: {}", String::from_utf8_lossy(sym), e)) };
        // 用 usize 占位，随后按各自签名转换
        let open = get(b"sqlite3_open_v2\0")?;
        let close = get(b"sqlite3_close\0")?;
        let exec = get(b"sqlite3_exec\0")?;
        let prepare = get(b"sqlite3_prepare_v2\0")?;
        let step = get(b"sqlite3_step\0")?;
        let column_count = get(b"sqlite3_column_count\0")?;
        let column_name = get(b"sqlite3_column_name\0")?;
        let column_type = get(b"sqlite3_column_type\0")?;
        let column_int64 = get(b"sqlite3_column_int64\0")?;
        let column_double = get(b"sqlite3_column_double\0")?;
        let column_text = get(b"sqlite3_column_text\0")?;
        let finalize = get(b"sqlite3_finalize\0")?;
        let errmsg = get(b"sqlite3_errmsg\0")?;
        let last_insert_id = get(b"sqlite3_last_insert_rowid\0")?;
        let changes = get(b"sqlite3_changes\0")?;
        return Ok(SqliteApi {
            _lib: lib,
            open: unsafe { std::mem::transmute(open) },
            close: unsafe { std::mem::transmute(close) },
            exec: unsafe { std::mem::transmute(exec) },
            prepare: unsafe { std::mem::transmute(prepare) },
            step: unsafe { std::mem::transmute(step) },
            column_count: unsafe { std::mem::transmute(column_count) },
            column_name: unsafe { std::mem::transmute(column_name) },
            column_type: unsafe { std::mem::transmute(column_type) },
            column_int64: unsafe { std::mem::transmute(column_int64) },
            column_double: unsafe { std::mem::transmute(column_double) },
            column_text: unsafe { std::mem::transmute(column_text) },
            finalize: unsafe { std::mem::transmute(finalize) },
            errmsg: unsafe { std::mem::transmute(errmsg) },
            last_insert_id: unsafe { std::mem::transmute(last_insert_id) },
            changes: unsafe { std::mem::transmute(changes) },
        });
    }
    Err(last_err)
}

static API: Lazy<Result<SqliteApi, String>> = Lazy::new(load_api);

/// 打开的数据库句柄表：句柄 id -> sqlite3*（以 usize 存指针地址，满足 Send）
static HANDLES: Lazy<Mutex<HashMap<i64, usize>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static NEXT_ID: Lazy<Mutex<i64>> = Lazy::new(|| Mutex::new(1));

fn db_err(api: &SqliteApi, db: *mut c_void, ctx: &str, span: Span, file: &str, src: &str) -> ZError {
    let msg = unsafe {
        let p = (api.errmsg)(db);
        if p.is_null() {
            String::new()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    };
    zerr(
        codes::SYSCALL,
        format!("sqlite {}: {}", ctx, if msg.is_empty() { "unknown error" } else { &msg }),
        span,
        file,
        src,
        Some("check the SQL statement and the database file"),
    )
}

/// 从 stmt 取当前行的列值（按 sqlite3_column_type 映射）。
fn column_value(api: &SqliteApi, stmt: *mut c_void, i: c_int) -> Value {
    unsafe {
        match (api.column_type)(stmt, i) {
            1 => Value::Int((api.column_int64)(stmt, i) as i64), // SQLITE_INTEGER
            2 => Value::Float((api.column_double)(stmt, i)),      // SQLITE_FLOAT
            5 => Value::Null,                                     // SQLITE_NULL
            _ => {
                // TEXT / BLOB：按 UTF-8 读出
                let p = (api.column_text)(stmt, i);
                if p.is_null() {
                    Value::Null
                } else {
                    Value::Str(CStr::from_ptr(p).to_string_lossy().into_owned())
                }
            }
        }
    }
}

/// 取句柄对应的 sqlite3*，句柄无效报错。
fn get_db(handle: i64, span: Span, file: &str, src: &str) -> Result<*mut c_void, ZError> {
    HANDLES
        .lock()
        .unwrap()
        .get(&handle)
        .copied()
        .map(|addr| addr as *mut c_void)
        .ok_or_else(|| zerr(
            codes::SYSCALL,
            format!("invalid sqlite handle `{}` (closed or never opened)", handle),
            span,
            file,
            src,
            Some("store the handle returned by `sqlite.open` and do not close twice"),
        ))
}

/// sqlite 模块调用入口。
pub fn call(name: &str, args: &[Value], span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    let api = API.as_ref().map_err(|e| {
        zerr(
            codes::DLL_LOAD,
            format!("cannot load libsqlite3: {}", e),
            span,
            file,
            src,
            Some("install sqlite3 (e.g. `apt install libsqlite3-dev`, or put sqlite3.dll next to hone.exe)"),
        )
    })?;

    match name {
        "sqlite.open" => {
            let path = as_str(&args[0], 0, span, file, src)?;
            let cpath = CString::new(path).map_err(|_| zerr(codes::TYPE_MISMATCH, "sqlite.open: path contains NUL", span, file, src, None::<&str>))?;
            let mut db: *mut c_void = std::ptr::null_mut();
            // SQLITE_OPEN_READWRITE(0x2) | SQLITE_OPEN_CREATE(0x4) | SQLITE_OPEN_FULLMUTEX(0x10000)
            let rc = unsafe { (api.open)(cpath.as_ptr(), &mut db, 0x2 | 0x4 | 0x10000, std::ptr::null()) };
            if rc != 0 {
                let msg = unsafe {
                    let p = (api.errmsg)(db);
                    if p.is_null() { "unknown error".to_string() } else { CStr::from_ptr(p).to_string_lossy().into_owned() }
                };
                if !db.is_null() {
                    unsafe { (api.close)(db) };
                }
                return Err(zerr(codes::SYSCALL, format!("sqlite.open failed: {}", msg), span, file, src, Some("check the path is writable")));
            }
            let mut next = NEXT_ID.lock().unwrap();
            let id = *next;
            *next += 1;
            HANDLES.lock().unwrap().insert(id, db as usize);
            Ok(Value::Int(id))
        }
        "sqlite.close" => {
            let handle = as_int(&args[0], 0, span, file, src)?;
            let db = get_db(handle, span, file, src)?;
            HANDLES.lock().unwrap().remove(&handle);
            let rc = unsafe { (api.close)(db) };
            Ok(Value::Bool(rc == 0))
        }
        "sqlite.exec" => {
            let handle = as_int(&args[0], 0, span, file, src)?;
            let sql = as_str(&args[1], 1, span, file, src)?;
            let db = get_db(handle, span, file, src)?;
            let csql = CString::new(sql).map_err(|_| zerr(codes::TYPE_MISMATCH, "sqlite.exec: SQL contains NUL", span, file, src, None::<&str>))?;
            let mut errmsg: *mut c_char = std::ptr::null_mut();
            let rc = unsafe { (api.exec)(db, csql.as_ptr(), None, std::ptr::null_mut(), &mut errmsg) };
            if rc != 0 {
                // sqlite3 用 sqlite3_malloc 分配 errmsg，进程内泄漏可接受（错误路径罕见）
                let msg = if errmsg.is_null() {
                    String::new()
                } else {
                    unsafe { CStr::from_ptr(errmsg).to_string_lossy().into_owned() }
                };
                return Err(zerr(codes::SYSCALL, format!("sqlite.exec failed: {}", msg), span, file, src, Some("check the SQL syntax")));
            }
            Ok(Value::Bool(true))
        }
        "sqlite.query" | "sqlite.query_one" => {
            let handle = as_int(&args[0], 0, span, file, src)?;
            let sql = as_str(&args[1], 1, span, file, src)?;
            let db = get_db(handle, span, file, src)?;
            let csql = CString::new(sql).map_err(|_| zerr(codes::TYPE_MISMATCH, "sqlite.query: SQL contains NUL", span, file, src, None::<&str>))?;
            let mut stmt: *mut c_void = std::ptr::null_mut();
            let rc = unsafe { (api.prepare)(db, csql.as_ptr(), -1, &mut stmt, std::ptr::null_mut()) };
            if rc != 0 {
                return Err(db_err(api, db, "prepare failed", span, file, src));
            }
            let mut rows: Vec<Value> = Vec::new();
            loop {
                let sr = unsafe { (api.step)(stmt) };
                if sr == 100 {
                    // SQLITE_ROW
                    let ncols = unsafe { (api.column_count)(stmt) };
                    let mut entries: Vec<(String, Value)> = Vec::with_capacity(ncols as usize);
                    for i in 0..ncols {
                        let name = unsafe {
                            let p = (api.column_name)(stmt, i);
                            if p.is_null() { format!("col{}", i) } else { CStr::from_ptr(p).to_string_lossy().into_owned() }
                        };
                        let val = column_value(api, stmt, i);
                        entries.push((name, val));
                    }
                    rows.push(Value::Dict(entries));
                    if name == "sqlite.query_one" {
                        break;
                    }
                } else if sr == 101 {
                    // SQLITE_DONE
                    break;
                } else {
                    unsafe { (api.finalize)(stmt) };
                    return Err(db_err(api, db, "step failed", span, file, src));
                }
            }
            unsafe { (api.finalize)(stmt) };
            if name == "sqlite.query_one" {
                Ok(rows.into_iter().next().unwrap_or(Value::Null))
            } else {
                Ok(Value::List(rows))
            }
        }
        "sqlite.escape" => {
            let s = as_str(&args[0], 0, span, file, src)?;
            Ok(Value::Str(s.replace('\'', "''")))
        }
        "sqlite.last_insert_id" => {
            let handle = as_int(&args[0], 0, span, file, src)?;
            let db = get_db(handle, span, file, src)?;
            Ok(Value::Int(unsafe { (api.last_insert_id)(db) }))
        }
        "sqlite.changes" => {
            let handle = as_int(&args[0], 0, span, file, src)?;
            let db = get_db(handle, span, file, src)?;
            Ok(Value::Int(unsafe { (api.changes)(db) as i64 }))
        }
        _ => Err(zerr(
            codes::NOT_IMPLEMENTED,
            format!("unknown sqlite function `{}`", name),
            span,
            file,
            src,
            None::<&str>,
        )),
    }
}
