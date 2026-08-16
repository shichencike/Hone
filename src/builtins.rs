// builtins.rs - Hone 内置函数
// 全部通过 `hone` 直接可用，无需导入。运行期校验参数类型（动态值兜底），
// 失败统一按 error[Hxxx] 格式报告。

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use std::sync::LazyLock;

use sha2::digest::Digest;

use crate::error::codes;
use crate::error::ZError;
use crate::interp::Value;
use crate::lexer::Span;

/// 全局键值存储（db.set / db.get）
static KV_STORE: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// --resume 持久化目标：(状态文件路径, 脚本内容哈希)。启用后 db.set 自动落盘。
static STATE_FILE: Mutex<Option<(PathBuf, String)>> = Mutex::new(None);

/// 启用 db 持久化：db.set 后自动将整个 KV_STORE 连同脚本哈希写入状态文件。
/// 由 main.rs 在 `--resume` 模式下调用。
pub fn enable_persist(path: PathBuf, script_hash: String) {
    *STATE_FILE.lock().unwrap() = Some((path, script_hash));
}

/// 用持久化数据覆盖 KV_STORE（`--resume` 启动时调用，先于脚本执行）。
pub fn load_state(kv: HashMap<String, String>) {
    let mut store = KV_STORE.lock().unwrap();
    store.clear();
    store.extend(kv);
}

/// 命令行参数（args.get / args.has），由 main.rs 初始化
static CLI_ARGS: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// 初始化命令行参数解析（由 main.rs 调用）
pub fn init_args(args: &[String]) {
    let mut map = CLI_ARGS.lock().unwrap();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a.starts_with("--") {
            let key = a.trim_start_matches("--").to_string();
            if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                map.insert(key, args[i + 1].clone());
                i += 2;
            } else {
                map.insert(key, "true".to_string());
                i += 1;
            }
        } else if a.starts_with('-') && a.len() == 2 {
            let key = a.trim_start_matches('-').to_string();
            if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                map.insert(key, args[i + 1].clone());
                i += 2;
            } else {
                map.insert(key, "true".to_string());
                i += 1;
            }
        } else {
            i += 1;
        }
    }
}

fn err(code: &'static str, msg: impl Into<String>, span: Span, file: &str, src: &str, help: Option<impl Into<String>>) -> ZError {
    ZError::new(code, msg, file, src, span.line, span.col, span.len.max(1), help)
}

// ---------- 运行期参数类型校验 ----------

fn as_str<'a>(v: &'a Value, arg: usize, name: &str, span: Span, file: &str, src: &str) -> Result<&'a str, ZError> {
    match v {
        Value::Str(s) => Ok(s),
        other => Err(err(
            codes::TYPE_MISMATCH,
            format!(
                "`{}` expects a string for argument {}, got `{}`",
                name,
                arg + 1,
                other.type_name()
            ),
            span,
            file,
            src,
            Some("pass a string value"),
        )),
    }
}

fn as_int(v: &Value, arg: usize, name: &str, span: Span, file: &str, src: &str) -> Result<i64, ZError> {
    match v {
        Value::Int(i) => Ok(*i),
        other => Err(err(
            codes::TYPE_MISMATCH,
            format!(
                "`{}` expects an integer for argument {}, got `{}`",
                name,
                arg + 1,
                other.type_name()
            ),
            span,
            file,
            src,
            Some("pass an `int` value"),
        )),
    }
}

fn as_num(v: &Value, arg: usize, name: &str, span: Span, file: &str, src: &str) -> Result<f64, ZError> {
    match v {
        Value::Int(i) => Ok(*i as f64),
        Value::Float(f) => Ok(*f),
        other => Err(err(
            codes::TYPE_MISMATCH,
            format!(
                "`{}` expects a number for argument {}, got `{}`",
                name,
                arg + 1,
                other.type_name()
            ),
            span,
            file,
            src,
            Some("pass an `int` or `float` value"),
        )),
    }
}

/// 判断是否为内置函数名（interp 的 call_fn 用）。
pub fn is_builtin(name: &str) -> bool {
    matches!(
        name,
        "print"
            | "len"
            | "append"
            | "clone"
            | "copy"
            | "contains"
            | "index_of"
            | "keys"
            | "values"
            | "has_key"
            | "is_int"
            | "is_float"
            | "is_str"
            | "is_bool"
            | "is_list"
            | "is_dict"
            | "is_null"
            | "type_of"
            | "assert"
            | "to_str"
            | "to_int"
            | "to_float"
            | "read_file"
            | "write_file"
            | "file_exists"
            | "abs"
            | "max"
            | "min"
            | "str_contains"
            | "str_replace"
            | "str_trim"
            | "time.now"
            | "time.sleep"
            | "time.format"
            | "time.parse"
            | "time.add"
            | "time.diff"
            | "time.weekday"
            | "random.int"
            | "random.float"
            | "http_get"
            | "http_post"
            | "http.request"
            | "smtp.send"
            | "ws.request"
            | "json_parse"
            | "json_stringify"
            | "sys.run"
            | "sys.get_env"
            | "sys.msgbox"
            | "sys.beep"
            | "sys.clipboard_set"
            | "sys.get_screen_size"
            | "sys.reg_read"
            | "sys.reg_write"
            | "server.listen"
            | "server.poll"
            | "server.respond"
            | "ptr.alloc"
            | "ptr.free"
            | "ptr.is_null"
            | "ptr.is_valid"
            | "ptr.size"
            | "ptr.read_int"
            | "ptr.read_float"
            | "ptr.read_byte"
            | "ptr.write_int"
            | "ptr.write_float"
            | "ptr.write_byte"
            | "log.info"
            | "log.warn"
            | "log.error"
            | "log.debug"
            | "path.join"
            | "path.dirname"
            | "path.basename"
            | "args.get"
            | "args.has"
            | "env.get"
            | "env.set"
            | "db.set"
            | "db.get"
            | "regex.match"
            | "regex.replace"
            | "crypto.md5"
            | "crypto.sha1"
            | "crypto.sha256"
            | "crypto.hmac_sha256"
            | "crypto.base64_encode"
            | "crypto.base64_decode"
            | "archive.zip_list"
            | "archive.zip_read"
            | "archive.zip_extract"
            | "archive.zip_create"
            | "archive.tgz_list"
            | "archive.tgz_read"
            | "archive.tgz_extract"
            | "archive.tgz_create"
            | "zlib.compress"
            | "zlib.decompress"
            | "zlib.gzip"
            | "zlib.gunzip"
            | "csv.parse"
            | "csv.parse_dict"
            | "csv.stringify"
            | "glob.match"
            | "glob.list"
            | "temp.dir"
            | "temp.file"
            | "temp.remove"
            | "stat.sum"
            | "stat.mean"
            | "stat.median"
            | "stat.variance"
            | "stat.stddev"
            | "stat.min"
            | "stat.max"
            | "matrix.identity"
            | "matrix.transpose"
            | "matrix.add"
            | "matrix.mul"
            | "matrix.scale"
            | "diff.lines"
            | "diff.unified"
            | "regex.find"
            | "regex.groups"
            | "regex.split"
            | "plot.bar"
            | "plot.line"
            | "yaml.parse"
            | "yaml.stringify"
            | "sqlite.open"
            | "sqlite.close"
            | "sqlite.exec"
            | "sqlite.query"
            | "sqlite.query_one"
            | "sqlite.escape"
            | "sqlite.last_insert_id"
            | "sqlite.changes"
            | "plugin.load"
            | "plugin.has"
            | "plugin.list"
            | "plugin.unload"
            | "uuid.new"
    )
}

// ---------- 入口 ----------

/// 调用内置函数。未知函数名由调用方保证不会到达（checker 已拦截）。
pub fn call(name: &str, args: Vec<Value>, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    match name {
        "print" => {
            let v = args.get(0).ok_or_else(|| arg_err(name, 1, 0, span, file, src))?;
            println!("{}", v.display());
            Ok(Value::Null)
        }
        "len" => {
            let v = args.get(0).ok_or_else(|| arg_err(name, 1, 0, span, file, src))?;
            match v {
                Value::Str(s) => Ok(Value::Int(s.len() as i64)),
                Value::List(items) => Ok(Value::Int(items.len() as i64)),
                Value::Dict(entries) => Ok(Value::Int(entries.len() as i64)),
                other => Err(err(
                    codes::TYPE_MISMATCH,
                    format!("`len` expects a string, list, or dict, got `{}`", other.type_name()),
                    span,
                    file,
                    src,
                    Some("`len` returns the byte length of a string, or the element count of a list/dict"),
                )),
            }
        }
        "append" => {
            let list = args.get(0).ok_or_else(|| arg_err(name, 2, 0, span, file, src))?;
            let val = args.get(1).ok_or_else(|| arg_err(name, 2, 1, span, file, src))?;
            match list {
                // 列表是值类型：返回新列表，配合 `l = append(l, x)` 使用
                Value::List(items) => {
                    let mut new_items = items.clone();
                    new_items.push(val.clone());
                    Ok(Value::List(new_items))
                }
                other => Err(err(
                    codes::TYPE_MISMATCH,
                    format!("`append` expects a list, got `{}`", other.type_name()),
                    span,
                    file,
                    src,
                    Some("use `l = append(l, x)` to add `x` to the tail of list `l`"),
                )),
            }
        }
        "clone" | "copy" => {
            // 深度拷贝：递归复制集合（Value 的 Clone 对 List/Dict 即深拷贝），
            // 后续对副本的 append/修改不影响原值。
            let v = args.get(0).ok_or_else(|| arg_err(name, 1, 0, span, file, src))?;
            Ok(v.clone())
        }
        "contains" => {
            let list = args.get(0).ok_or_else(|| arg_err(name, 2, 0, span, file, src))?;
            let val = args.get(1).ok_or_else(|| arg_err(name, 2, 1, span, file, src))?;
            match list {
                Value::List(items) => Ok(Value::Bool(items.iter().any(|i| values_eq(i, val)))),
                Value::Str(s) => match val {
                    // 字符串包含：兼容 str_contains
                    Value::Str(sub) => Ok(Value::Bool(s.contains(sub))),
                    other => Err(err(
                        codes::TYPE_MISMATCH,
                        format!("`contains` on a string expects a string, got `{}`", other.type_name()),
                        span,
                        file,
                        src,
                        Some("pass a substring"),
                    )),
                },
                other => Err(err(
                    codes::TYPE_MISMATCH,
                    format!("`contains` expects a list or string, got `{}`", other.type_name()),
                    span,
                    file,
                    src,
                    Some("pass a list or string as the first argument"),
                )),
            }
        }
        "index_of" => {
            let list = args.get(0).ok_or_else(|| arg_err(name, 2, 0, span, file, src))?;
            let val = args.get(1).ok_or_else(|| arg_err(name, 2, 1, span, file, src))?;
            match list {
                Value::List(items) => {
                    for (i, item) in items.iter().enumerate() {
                        if values_eq(item, val) {
                            return Ok(Value::Int(i as i64));
                        }
                    }
                    Ok(Value::Int(-1))
                }
                other => Err(err(
                    codes::TYPE_MISMATCH,
                    format!("`index_of` expects a list, got `{}`", other.type_name()),
                    span,
                    file,
                    src,
                    Some("pass a list as the first argument"),
                )),
            }
        }
        "keys" => {
            let d = args.get(0).ok_or_else(|| arg_err(name, 1, 0, span, file, src))?;
            match d {
                Value::Dict(entries) => Ok(Value::List(
                    entries.iter().map(|(k, _)| Value::Str(k.clone())).collect(),
                )),
                other => Err(err(
                    codes::TYPE_MISMATCH,
                    format!("`keys` expects a dict, got `{}`", other.type_name()),
                    span,
                    file,
                    src,
                    Some("pass a dict as the argument"),
                )),
            }
        }
        "values" => {
            let d = args.get(0).ok_or_else(|| arg_err(name, 1, 0, span, file, src))?;
            match d {
                Value::Dict(entries) => Ok(Value::List(entries.iter().map(|(_, v)| v.clone()).collect())),
                other => Err(err(
                    codes::TYPE_MISMATCH,
                    format!("`values` expects a dict, got `{}`", other.type_name()),
                    span,
                    file,
                    src,
                    Some("pass a dict as the argument"),
                )),
            }
        }
        "has_key" => {
            let d = args.get(0).ok_or_else(|| arg_err(name, 2, 0, span, file, src))?;
            let k = as_str(&args[1], 1, name, span, file, src)?;
            match d {
                Value::Dict(entries) => Ok(Value::Bool(entries.iter().any(|(ek, _)| ek == k))),
                other => Err(err(
                    codes::TYPE_MISMATCH,
                    format!("`has_key` expects a dict, got `{}`", other.type_name()),
                    span,
                    file,
                    src,
                    Some("pass a dict as the first argument and a key string as the second"),
                )),
            }
        }
        "type_of" => {
            let v = args.get(0).ok_or_else(|| arg_err(name, 1, 0, span, file, src))?;
            Ok(Value::Str(v.type_name().to_string()))
        }
        "assert" => {
            // assert(条件[, 消息])：条件为 false 时抛 H700（测试框架用）
            let cond = args.get(0).ok_or_else(|| arg_err(name, 1, 0, span, file, src))?;
            let ok = match cond {
                Value::Bool(b) => *b,
                other => {
                    return Err(err(
                        codes::TYPE_MISMATCH,
                        format!("`assert` expects a `bool` condition, got `{}`", other.type_name()),
                        span,
                        file,
                        src,
                        Some("pass a boolean expression, e.g. `assert(x == 1)`"),
                    ))
                }
            };
            if !ok {
                let msg = match args.get(1) {
                    Some(Value::Str(s)) => s.clone(),
                    _ => "assertion failed".to_string(),
                };
                return Err(err(codes::ASSERT, msg, span, file, src, None::<&str>));
            }
            Ok(Value::Null)
        }
        "is_int" => {
            let v = args.get(0).ok_or_else(|| arg_err(name, 1, 0, span, file, src))?;
            Ok(Value::Bool(matches!(v, Value::Int(_))))
        }
        "is_float" => {
            let v = args.get(0).ok_or_else(|| arg_err(name, 1, 0, span, file, src))?;
            Ok(Value::Bool(matches!(v, Value::Float(_))))
        }
        "is_str" => {
            let v = args.get(0).ok_or_else(|| arg_err(name, 1, 0, span, file, src))?;
            Ok(Value::Bool(matches!(v, Value::Str(_))))
        }
        "is_bool" => {
            let v = args.get(0).ok_or_else(|| arg_err(name, 1, 0, span, file, src))?;
            Ok(Value::Bool(matches!(v, Value::Bool(_))))
        }
        "is_list" => {
            let v = args.get(0).ok_or_else(|| arg_err(name, 1, 0, span, file, src))?;
            Ok(Value::Bool(matches!(v, Value::List(_))))
        }
        "is_dict" => {
            let v = args.get(0).ok_or_else(|| arg_err(name, 1, 0, span, file, src))?;
            Ok(Value::Bool(matches!(v, Value::Dict(_))))
        }
        "is_null" => {
            let v = args.get(0).ok_or_else(|| arg_err(name, 1, 0, span, file, src))?;
            Ok(Value::Bool(matches!(v, Value::Null)))
        }
        "to_str" => {
            let v = args.get(0).ok_or_else(|| arg_err(name, 1, 0, span, file, src))?;
            match v {
                Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Error(_) | Value::Ptr(_) => {
                    Ok(Value::Str(v.display()))
                }
                Value::Str(s) => Ok(Value::Str(s.clone())),
                Value::List(_) | Value::Dict(_) | Value::Lambda(_) => Ok(Value::Str(v.display())),
                Value::Null => Ok(Value::Str("null".to_string())),
            }
        }
        "to_int" => {
            let v = args.get(0).ok_or_else(|| arg_err(name, 1, 0, span, file, src))?;
            match v {
                Value::Int(i) => Ok(Value::Int(*i)),
                Value::Float(f) => {
                    if f.is_finite() {
                        Ok(Value::Int(f.trunc() as i64))
                    } else {
                        Err(err(
                            codes::TYPE_MISMATCH,
                            "cannot convert NaN/infinity to `int`",
                            span,
                            file,
                            src,
                            None::<&str>,
                        ))
                    }
                }
                Value::Str(s) => {
                    let t = s.trim();
                    let digits = t.strip_prefix('-').unwrap_or(t);
                    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
                        Err(err(
                            codes::STR_TO_INT,
                            format!("cannot convert `{}` to `int`: not a pure digit string", s),
                            span,
                            file,
                            src,
                            Some("`to_int` on a string requires digits only (optional leading `-`)"),
                        ))
                    } else {
                        t.parse::<i64>().map(Value::Int).map_err(|_| {
                            err(
                                codes::STR_TO_INT,
                                format!("cannot convert `{}` to `int`: out of range", s),
                                span,
                                file,
                                src,
                                Some("the value does not fit in a 64-bit signed integer"),
                            )
                        })
                    }
                }
                other => Err(err(
                    codes::TYPE_MISMATCH,
                    format!("cannot convert `{}` to `int`", other.type_name()),
                    span,
                    file,
                    src,
                    Some("`to_int` accepts `int`, `float` or a pure-digit `str`"),
                )),
            }
        }
        "to_float" => {
            let v = args.get(0).ok_or_else(|| arg_err(name, 1, 0, span, file, src))?;
            match v {
                Value::Int(i) => Ok(Value::Float(*i as f64)),
                Value::Float(f) => Ok(Value::Float(*f)),
                Value::Str(s) => s.trim().parse::<f64>().map(Value::Float).map_err(|_| {
                    err(
                        codes::STR_TO_FLOAT,
                        format!("cannot convert `{}` to `float`: invalid format", s),
                        span,
                        file,
                        src,
                        Some("`to_float` on a string requires a number format like `2.718`"),
                    )
                }),
                other => Err(err(
                    codes::TYPE_MISMATCH,
                    format!("cannot convert `{}` to `float`", other.type_name()),
                    span,
                    file,
                    src,
                    Some("`to_float` accepts `int`, `float` or a numeric `str`"),
                )),
            }
        }
        "read_file" => {
            let p = as_str(&args[0], 0, name, span, file, src)?;
            std::fs::read_to_string(p).map(Value::Str).map_err(|e| {
                // 细分文件错误：不存在 / 权限不足 / 被占用锁定 / 其他
                let (code, hint): (&'static str, &'static str) = match e.kind() {
                    std::io::ErrorKind::NotFound => (codes::FILE_NOT_FOUND, "the file does not exist"),
                    std::io::ErrorKind::PermissionDenied => (codes::FILE_PERMISSION, "check file permissions"),
                    std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::ResourceBusy
                    | std::io::ErrorKind::Interrupted => (codes::FILE_LOCKED, "the file is locked by another process"),
                    _ => (codes::NOT_FOUND, "check the path and file permissions"),
                };
                err(
                    code,
                    format!("cannot read file `{}`: {}", p, e),
                    span,
                    file,
                    src,
                    Some(hint),
                )
            })
        }
        "write_file" => {
            let p = as_str(&args[0], 0, name, span, file, src)?;
            let c = as_str(&args[1], 1, name, span, file, src)?;
            std::fs::write(p, c).map_err(|e| {
                // 细分文件错误：不存在 / 权限不足 / 被占用锁定 / 其他
                let (code, hint): (&'static str, &'static str) = match e.kind() {
                    std::io::ErrorKind::NotFound => (codes::FILE_NOT_FOUND, "the file does not exist"),
                    std::io::ErrorKind::PermissionDenied => (codes::FILE_PERMISSION, "check file permissions"),
                    std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::ResourceBusy
                    | std::io::ErrorKind::Interrupted => (codes::FILE_LOCKED, "the file is locked by another process"),
                    _ => (codes::NOT_FOUND, "check the path and file permissions"),
                };
                err(
                    code,
                    format!("cannot write file `{}`: {}", p, e),
                    span,
                    file,
                    src,
                    Some(hint),
                )
            })?;
            Ok(Value::Null)
        }
        "file_exists" => {
            let p = as_str(&args[0], 0, name, span, file, src)?;
            Ok(Value::Bool(std::path::Path::new(p).exists()))
        }
        "abs" => match &args[0] {
            Value::Int(i) => i
                .checked_abs()
                .map(Value::Int)
                .ok_or_else(|| err(codes::INTEGER_OVERFLOW, "`abs` overflow on i64::MIN", span, file, src, None::<&str>)),
            Value::Float(f) => Ok(Value::Float(f.abs())),
            other => Err(err(
                codes::TYPE_MISMATCH,
                format!("`abs` expects a number, got `{}`", other.type_name()),
                span,
                file,
                src,
                Some("pass an `int` or `float`"),
            )),
        },
        "max" | "min" => {
            let (a, b) = (&args[0], &args[1]);
            let r = match (a, b) {
                (Value::Int(x), Value::Int(y)) => Value::Int(if name == "max" { (*x).max(*y) } else { (*x).min(*y) }),
                (Value::Float(x), Value::Float(y)) => Value::Float(if name == "max" { x.max(*y) } else { x.min(*y) }),
                _ => {
                    return Err(err(
                        codes::TYPE_MISMATCH,
                        format!(
                            "`{}` requires two operands of the same type, got `{}` and `{}`",
                            name,
                            a.type_name(),
                            b.type_name()
                        ),
                        span,
                        file,
                        src,
                        Some("Hone has no implicit type conversion"),
                    ));
                }
            };
            Ok(r)
        }
        "str_contains" => {
            let s = as_str(&args[0], 0, name, span, file, src)?;
            let sub = as_str(&args[1], 1, name, span, file, src)?;
            Ok(Value::Bool(s.contains(sub)))
        }
        "str_replace" => {
            let s = as_str(&args[0], 0, name, span, file, src)?;
            let old = as_str(&args[1], 1, name, span, file, src)?;
            let new = as_str(&args[2], 2, name, span, file, src)?;
            Ok(Value::Str(s.replace(old, new)))
        }
        "str_trim" => {
            let s = as_str(&args[0], 0, name, span, file, src)?;
            Ok(Value::Str(s.trim().to_string()))
        }
        "time.now" => {
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            Ok(Value::Int(secs))
        }
        "time.sleep" => {
            let secs = as_num(&args[0], 0, name, span, file, src)?;
            let d = Duration::try_from_secs_f64(secs).map_err(|_| {
                err(
                    codes::TYPE_MISMATCH,
                    "`time.sleep` duration must be a non-negative number",
                    span,
                    file,
                    src,
                    None::<&str>,
                )
            })?;
            std::thread::sleep(d);
            Ok(Value::Null)
        }
        "time.format" => {
            let ts = as_int(&args[0], 0, name, span, file, src)?;
            let fmt = as_str(&args[1], 1, name, span, file, src)?;
            Ok(Value::Str(format_timestamp(ts, fmt)))
        }
        "time.parse" => {
            let s = as_str(&args[0], 0, name, span, file, src)?;
            match parse_timestamp(s) {
                Some(secs) => Ok(Value::Int(secs)),
                None => Err(err(
                    codes::TYPE_MISMATCH,
                    format!("cannot parse `{}` as a timestamp", s),
                    span,
                    file,
                    src,
                    Some("supported formats: `YYYY-MM-DDTHH:MM:SSZ`, `YYYY-MM-DD HH:MM:SS`, optional `+08:00` offset"),
                )),
            }
        }
        "time.add" => {
            // 时间戳算术：time.add(ts, seconds) -> 新时间戳（秒）
            let ts = as_int(&args[0], 0, name, span, file, src)?;
            let secs = as_int(&args[1], 1, name, span, file, src)?;
            ts.checked_add(secs)
                .map(Value::Int)
                .ok_or_else(|| err(codes::INTEGER_OVERFLOW, "time.add: timestamp overflow", span, file, src, None::<&str>))
        }
        "time.diff" => {
            // 时间差：time.diff(a, b) -> a - b（秒）
            let a = as_int(&args[0], 0, name, span, file, src)?;
            let b = as_int(&args[1], 1, name, span, file, src)?;
            a.checked_sub(b)
                .map(Value::Int)
                .ok_or_else(|| err(codes::INTEGER_OVERFLOW, "time.diff: timestamp overflow", span, file, src, None::<&str>))
        }
        "time.weekday" => {
            // 星期几（ISO 8601）：1=周一 … 7=周日。1970-01-01 是周四。
            let ts = as_int(&args[0], 0, name, span, file, src)?;
            let days = ts.div_euclid(86400);
            let wd = ((days + 3).rem_euclid(7)) + 1;
            Ok(Value::Int(wd))
        }
        "random.int" => {
            let min = as_int(&args[0], 0, name, span, file, src)?;
            let max = as_int(&args[1], 1, name, span, file, src)?;
            if min > max {
                return Err(err(
                    codes::TYPE_MISMATCH,
                    format!("`random.int` range is invalid: min ({}) > max ({})", min, max),
                    span,
                    file,
                    src,
                    Some("swap the two arguments"),
                ));
            }
            Ok(Value::Int(random_int(min, max)))
        }
        "random.float" => Ok(Value::Float(random_float())),
        "http_get" => {
            let url = as_str(&args[0], 0, name, span, file, src)?;
            http_request(url, "GET", None, span, file, src).map(Value::Str)
        }
        "http_post" => {
            let url = as_str(&args[0], 0, name, span, file, src)?;
            let body = as_str(&args[1], 1, name, span, file, src)?;
            http_request(url, "POST", Some(body), span, file, src).map(Value::Str)
        }
        "http.request" => {
            // 通用 HTTP 请求：http.request(url, {method?, headers?, body?, timeout?})
            let url = as_str(&args[0], 0, name, span, file, src)?;
            let opts = &args[1];
            let mut method = "GET";
            let mut body: Option<String> = None;
            let mut headers: Vec<(String, String)> = Vec::new();
            let mut timeout: u64 = 15;
            match opts {
                Value::Dict(entries) => {
                    for (k, v) in entries {
                        match k.as_str() {
                            "method" => method = as_str(v, 0, name, span, file, src)?,
                            "body" => body = Some(as_str(v, 0, name, span, file, src)?.to_string()),
                            "headers" => {
                                if let Value::Dict(hdrs) = v {
                                    for (hk, hv) in hdrs {
                                        let hv_s = as_str(hv, 0, name, span, file, src)?;
                                        headers.push((hk.clone(), hv_s.to_string()));
                                    }
                                } else {
                                    return Err(err(
                                        codes::TYPE_MISMATCH,
                                        "`http.request` headers must be a dict of strings",
                                        span,
                                        file,
                                        src,
                                        Some("pass {\"headers\": {\"User-Agent\": \"...\"}}"),
                                    ));
                                }
                            }
                            "timeout" => {
                                timeout = match v {
                                    Value::Int(i) if *i >= 0 => *i as u64,
                                    Value::Float(f) if *f >= 0.0 => *f as u64,
                                    _ => {
                                        return Err(err(
                                            codes::TYPE_MISMATCH,
                                            "`http.request` timeout must be a non-negative number (seconds)",
                                            span,
                                            file,
                                            src,
                                            Some("pass an int or float number of seconds"),
                                        ))
                                    }
                                }
                            }
                            _ => {} // 忽略未知选项键
                        }
                    }
                }
                other => {
                    return Err(err(
                        codes::TYPE_MISMATCH,
                        format!("`http.request` expects a dict of options, got `{}`", other.type_name()),
                        span,
                        file,
                        src,
                        Some("form: http.request(url, {method, headers, body, timeout})"),
                    ))
                }
            }
            let header_refs: Vec<(&str, &str)> = headers.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
            let (head, body_bytes) = http_fetch_opts(url, method, body.as_deref(), &header_refs, timeout, span, file, src)?;
            let mut text = String::from_utf8_lossy(&body_bytes).into_owned();
            if head.to_lowercase().contains("transfer-encoding: chunked") {
                text = decode_chunked(&text);
            }
            Ok(Value::Str(text))
        }
        // 网络与通信（netmod 模块实现，SMTP 发邮件 / WebSocket 请求）
        "smtp.send" | "ws.request" => {
            crate::netmod::call(name, &args, span, file, src)
        }
        "json_parse" => {
            let s = as_str(&args[0], 0, name, span, file, src)?;
            json_to_value(s, span, file, src)
        }
        "json_stringify" => value_to_json(&args[0], span, file, src).map(Value::Str),
        "sys.run" => {
            let cmd = as_str(&args[0], 0, name, span, file, src)?;
            run_shell(cmd, span, file, src).map(Value::Str)
        }
        "sys.get_env" => {
            let k = as_str(&args[0], 0, name, span, file, src)?;
            Ok(Value::Str(std::env::var(k).unwrap_or_default()))
        }
        // Windows API 封装的 sys.* 函数（sysmod 模块实现）
        "sys.msgbox" | "sys.beep" | "sys.clipboard_set" | "sys.get_screen_size" | "sys.reg_read" | "sys.reg_write" => {
            crate::sysmod::call(name, &args, span, file, src)
        }
        // 本地 HTTP 服务器（srvmod 模块实现，纯 std::net，跨平台）
        "server.listen" | "server.poll" | "server.respond" => {
            crate::srvmod::call(name, &args, span, file, src)
        }
        // 指针类（ptrmod 模块实现，分配表跟踪防野指针）
        "ptr.alloc" | "ptr.free" | "ptr.is_null" | "ptr.is_valid" | "ptr.size"
        | "ptr.read_int" | "ptr.read_float" | "ptr.read_byte"
        | "ptr.write_int" | "ptr.write_float" | "ptr.write_byte" => {
            crate::ptrmod::call(name, &args, span, file, src)
        }
        // 压缩与归档（archmod 模块实现，zip/tar.gz 读写 + zlib/gzip 压缩）
        "archive.zip_list" | "archive.zip_read" | "archive.zip_extract" | "archive.zip_create"
        | "archive.tgz_list" | "archive.tgz_read" | "archive.tgz_extract" | "archive.tgz_create"
        | "zlib.compress" | "zlib.decompress" | "zlib.gzip" | "zlib.gunzip" => {
            crate::archmod::call(name, &args, span, file, src)
        }
        // 数据处理（datamod 模块实现，csv 解析/序列化）
        "csv.parse" | "csv.parse_dict" | "csv.stringify" => {
            crate::datamod::call(name, &args, span, file, src)
        }
        // 系统工具（sysutilmod 模块实现，glob 匹配 / temp 临时文件目录）
        "glob.match" | "glob.list" | "temp.dir" | "temp.file" | "temp.remove" => {
            crate::sysutilmod::call(name, &args, span, file, src)
        }
        // 科学计算（statmod 模块实现，stat 统计 / matrix 矩阵运算）
        "stat.sum" | "stat.mean" | "stat.median" | "stat.variance" | "stat.stddev"
        | "stat.min" | "stat.max" | "matrix.identity" | "matrix.transpose"
        | "matrix.add" | "matrix.mul" | "matrix.scale" => {
            crate::statmod::call(name, &args, span, file, src)
        }
        // 文本处理（textmod 模块实现，diff 对比 / regex find/groups/split）
        "diff.lines" | "diff.unified" | "regex.find" | "regex.groups" | "regex.split" => {
            crate::textmod::call(name, &args, span, file, src)
        }
        // 绘图与数据格式（plotmod 模块实现，SVG 图表 / YAML 解析）
        "plot.bar" | "plot.line" | "yaml.parse" | "yaml.stringify" => {
            crate::plotmod::call(name, &args, span, file, src)
        }
        // SQLite 轻量封装（sqlitemod 模块实现，运行时 FFI 加载系统 libsqlite3）
        "sqlite.open" | "sqlite.close" | "sqlite.exec" | "sqlite.query" | "sqlite.query_one"
        | "sqlite.escape" | "sqlite.last_insert_id" | "sqlite.changes" => {
            crate::sqlitemod::call(name, &args, span, file, src)
        }
        // 插件系统（pluginmod 模块实现，运行期动态注册）
        "plugin.load" | "plugin.has" | "plugin.list" | "plugin.unload" => {
            crate::pluginmod::call(name, &args, span, file, src)
        }
        // ---------- log ----------
        "log.info" => {
            let msg = as_str(&args[0], 0, name, span, file, src)?;
            eprintln!("\x1b[34m[INFO]\x1b[0m {}", msg);
            Ok(Value::Null)
        }
        "log.warn" => {
            let msg = as_str(&args[0], 0, name, span, file, src)?;
            eprintln!("\x1b[33m[WARN]\x1b[0m {}", msg);
            Ok(Value::Null)
        }
        "log.error" => {
            let msg = as_str(&args[0], 0, name, span, file, src)?;
            eprintln!("\x1b[31m[ERROR]\x1b[0m {}", msg);
            Ok(Value::Null)
        }
        "log.debug" => {
            let msg = as_str(&args[0], 0, name, span, file, src)?;
            eprintln!("\x1b[32m[DEBUG]\x1b[0m {}", msg);
            Ok(Value::Null)
        }
        // ---------- path ----------
        "path.join" => {
            let mut parts: Vec<&str> = Vec::new();
            for (i, arg) in args.iter().enumerate() {
                let s = as_str(arg, i, name, span, file, src)?;
                parts.push(s);
            }
            let p: std::path::PathBuf = parts.iter().collect();
            Ok(Value::Str(p.to_string_lossy().to_string()))
        }
        "path.dirname" => {
            let p = as_str(&args[0], 0, name, span, file, src)?;
            // Path::parent() 对无分隔符路径返回空 parent（而非 None），统一归为 "."
            let parent = std::path::Path::new(p)
                .parent()
                .filter(|d| !d.as_os_str().is_empty())
                .map(|d| d.to_string_lossy().to_string())
                .unwrap_or_else(|| ".".to_string());
            Ok(Value::Str(parent))
        }
        "path.basename" => {
            let p = as_str(&args[0], 0, name, span, file, src)?;
            let name = std::path::Path::new(p)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "".to_string());
            Ok(Value::Str(name))
        }
        // ---------- args ----------
        "args.get" => {
            let key = as_str(&args[0], 0, name, span, file, src)?;
            let raw = CLI_ARGS.lock().unwrap().get(key).cloned();
            match raw {
                Some(v) => {
                    // 带类型参数时按期望类型转换；无类型参数时保持字符串
                    if args.len() >= 2 {
                        let ty = as_str(&args[1], 1, name, span, file, src)?;
                        let t = v.trim();
                        match ty {
                            "int" => t.parse::<i64>().map(Value::Int).map_err(|_| {
                                err(
                                    codes::STR_TO_INT,
                                    format!("`args.get(\"{}\", int)` cannot parse `{}` as an integer", key, v),
                                    span,
                                    file,
                                    src,
                                    Some("pass a valid integer on the command line"),
                                )
                            }),
                            "float" => t.parse::<f64>().map(Value::Float).map_err(|_| {
                                err(
                                    codes::STR_TO_FLOAT,
                                    format!("`args.get(\"{}\", float)` cannot parse `{}` as a float", key, v),
                                    span,
                                    file,
                                    src,
                                    Some("pass a valid number on the command line"),
                                )
                            }),
                            "bool" => match t {
                                "true" | "1" => Ok(Value::Bool(true)),
                                "false" | "0" => Ok(Value::Bool(false)),
                                _ => Err(err(
                                    codes::TYPE_MISMATCH,
                                    format!("`args.get(\"{}\", bool)` cannot parse `{}` as a boolean", key, v),
                                    span,
                                    file,
                                    src,
                                    Some("use `true`/`false` or `1`/`0`"),
                                )),
                            },
                            "str" => Ok(Value::Str(v)),
                            other => Err(err(
                                codes::TYPE_MISMATCH,
                                format!("unknown type `{}` for `args.get`", other),
                                span,
                                file,
                                src,
                                Some("expected one of `int`, `float`, `bool`, `str`"),
                            )),
                        }
                    } else {
                        Ok(Value::Str(v))
                    }
                }
                // 键不存在：有默认值参数则返回默认值，否则返回 null
                None => {
                    if args.len() >= 3 {
                        Ok(args[2].clone())
                    } else {
                        Ok(Value::Null)
                    }
                }
            }
        }
        "args.has" => {
            let key = as_str(&args[0], 0, name, span, file, src)?;
            let map = CLI_ARGS.lock().unwrap();
            Ok(Value::Bool(map.contains_key(key)))
        }
        // ---------- env ----------
        "env.get" => {
            let key = as_str(&args[0], 0, name, span, file, src)?;
            Ok(Value::Str(std::env::var(key).unwrap_or_default()))
        }
        "env.set" => {
            let key = as_str(&args[0], 0, name, span, file, src)?;
            let val = as_str(&args[1], 1, name, span, file, src)?;
            std::env::set_var(key, val);
            Ok(Value::Null)
        }
        // ---------- db ----------
        "db.set" => {
            let key = as_str(&args[0], 0, name, span, file, src)?;
            let val = as_str(&args[1], 1, name, span, file, src)?;
            {
                let mut store = KV_STORE.lock().unwrap();
                store.insert(key.to_string(), val.to_string());
            }
            // --resume 模式下同步落盘，避免进程崩溃丢失检查点；写盘失败显式报错
            if let Some((path, hash)) = STATE_FILE.lock().unwrap().clone() {
                let kv = KV_STORE.lock().unwrap().clone();
                let json = serde_json::json!({ "script": hash, "kv": kv });
                std::fs::write(&path, json.to_string()).map_err(|e| {
                    err(
                        codes::FILE_PERMISSION,
                        format!("cannot persist db state to `{}`: {}", path.display(), e),
                        span,
                        file,
                        src,
                        Some("check disk space or file permissions"),
                    )
                })?;
            }
            Ok(Value::Null)
        }
        "db.get" => {
            let key = as_str(&args[0], 0, name, span, file, src)?;
            let store = KV_STORE.lock().unwrap();
            Ok(store.get(key).cloned().map(Value::Str).unwrap_or(Value::Null))
        }
        // ---------- regex ----------
        "regex.match" => {
            let pat = as_str(&args[0], 0, name, span, file, src)?;
            let text = as_str(&args[1], 1, name, span, file, src)?;
            let re = regex::Regex::new(pat).map_err(|e| {
                err(codes::SYNTAX, format!("invalid regex `{}`: {}", pat, e), span, file, src, None::<&str>)
            })?;
            Ok(Value::Bool(re.is_match(text)))
        }
        "regex.replace" => {
            let pat = as_str(&args[0], 0, name, span, file, src)?;
            let text = as_str(&args[1], 1, name, span, file, src)?;
            let repl = as_str(&args[2], 2, name, span, file, src)?;
            let re = regex::Regex::new(pat).map_err(|e| {
                err(codes::SYNTAX, format!("invalid regex `{}`: {}", pat, e), span, file, src, None::<&str>)
            })?;
            Ok(Value::Str(re.replace_all(text, repl).to_string()))
        }
        // ---------- crypto ----------
        "crypto.md5" => {
            let s = as_str(&args[0], 0, name, span, file, src)?;
            let hash = md5::Md5::digest(s.as_bytes());
            Ok(Value::Str(format!("{:x}", hash)))
        }
        "crypto.sha1" => {
            let s = as_str(&args[0], 0, name, span, file, src)?;
            let hash = sha1::Sha1::digest(s.as_bytes());
            Ok(Value::Str(format!("{:x}", hash)))
        }
        "crypto.sha256" => {
            let s = as_str(&args[0], 0, name, span, file, src)?;
            let mut hasher = sha2::Sha256::new();
            hasher.update(s.as_bytes());
            let hash = hasher.finalize();
            Ok(Value::Str(format!("{:x}", hash)))
        }
        "crypto.hmac_sha256" => {
            // HMAC-SHA256(密钥, 消息)：密钥与消息均为字符串
            let key = as_str(&args[0], 0, name, span, file, src)?;
            let msg = as_str(&args[1], 1, name, span, file, src)?;
            use hmac::{Hmac, Mac};
            let mut mac = Hmac::<sha2::Sha256>::new_from_slice(key.as_bytes()).map_err(|_| {
                err(codes::TYPE_MISMATCH, "invalid HMAC key", span, file, src, None::<&str>)
            })?;
            mac.update(msg.as_bytes());
            Ok(Value::Str(format!("{:x}", mac.finalize().into_bytes())))
        }
        "crypto.base64_encode" => {
            let s = as_str(&args[0], 0, name, span, file, src)?;
            use base64::Engine;
            Ok(Value::Str(base64::engine::general_purpose::STANDARD.encode(s.as_bytes())))
        }
        "crypto.base64_decode" => {
            let s = as_str(&args[0], 0, name, span, file, src)?;
            use base64::Engine;
            match base64::engine::general_purpose::STANDARD.decode(s.trim()) {
                Ok(bytes) => Ok(Value::Str(String::from_utf8_lossy(&bytes).into_owned())),
                Err(e) => Err(err(
                    codes::TYPE_MISMATCH,
                    format!("invalid base64 input: {}", e),
                    span,
                    file,
                    src,
                    Some("pass a valid base64 string, e.g. `aGVsbG8=`"),
                )),
            }
        }
        // ---------- uuid ----------
        "uuid.new" => {
            // UUID v4：128 位随机数，标记版本 4 与变体位
            let hi = next_u64();
            let lo = next_u64();
            let mut b = [0u8; 16];
            b[..8].copy_from_slice(&hi.to_be_bytes());
            b[8..].copy_from_slice(&lo.to_be_bytes());
            b[6] = (b[6] & 0x0f) | 0x40; // version 4
            b[8] = (b[8] & 0x3f) | 0x80; // variant 10xx
            let s = format!(
                "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
            );
            Ok(Value::Str(s))
        }
        _ => Err(err(
            codes::UNDEFINED,
            format!("undefined function `{}`", name),
            span,
            file,
            src,
            Some("check the spelling"),
        )),
    }
}

fn arg_err(name: &str, want: usize, got: usize, span: Span, file: &str, src: &str) -> ZError {
    err(
        codes::ARG_COUNT,
        format!("wrong number of arguments: `{}` expects {}, got {}", name, want, got),
        span,
        file,
        src,
        Some("check the function signature"),
    )
}

/// 深度值相等（列表/字典逐元素比较），供 contains / index_of 使用。
fn values_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x == y,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Null, Value::Null) => true,
        (Value::List(x), Value::List(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(i, j)| values_eq(i, j))
        }
        (Value::Dict(x), Value::Dict(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y.iter())
                    .all(|((kx, vx), (ky, vy))| kx == ky && values_eq(vx, vy))
        }
        _ => false,
    }
}

// ---------- time ----------

/// 将 Unix 时间戳（秒）按格式串格式化（UTC）。占位符：YYYY MM DD HH mm SS。
pub(crate) fn format_timestamp(secs: i64, fmt: &str) -> String {
    let days = secs.div_euclid(86400);
    let sod = secs.rem_euclid(86400);
    let (y, mo, d) = civil_from_days(days);
    let h = sod / 3600;
    let mi = (sod % 3600) / 60;
    let s = sod % 60;

    let mut out = String::new();
    let chars: Vec<char> = fmt.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        if i + 4 <= n && chars[i..i + 4] == ['Y', 'Y', 'Y', 'Y'] {
            out.push_str(&format!("{:04}", y));
            i += 4;
            continue;
        }
        if i + 2 <= n {
            let seg: String = chars[i..i + 2].iter().collect();
            match seg.as_str() {
                "MM" => {
                    out.push_str(&format!("{:02}", mo));
                    i += 2;
                    continue;
                }
                "DD" => {
                    out.push_str(&format!("{:02}", d));
                    i += 2;
                    continue;
                }
                "HH" => {
                    out.push_str(&format!("{:02}", h));
                    i += 2;
                    continue;
                }
                "mm" => {
                    out.push_str(&format!("{:02}", mi));
                    i += 2;
                    continue;
                }
                "SS" => {
                    out.push_str(&format!("{:02}", s));
                    i += 2;
                    continue;
                }
                "WW" => {
                    // ISO 8601 星期几：1=周一 … 7=周日（1970-01-01 为周四）
                    let days = secs.div_euclid(86400);
                    out.push_str(&(((days + 3).rem_euclid(7)) + 1).to_string());
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// 自纪元起的天数 → (年, 月, 日)。算法：Howard Hinnant's civil_from_days。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// (年, 月, 日) → 自纪元起的天数。算法：Howard Hinnant's days_from_civil。
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// 解析时间戳字符串 → Unix 秒（UTC）。
/// 支持：`YYYY-MM-DD`、`YYYY-MM-DDTHH:MM:SS`、`YYYY-MM-DD HH:MM:SS`，
/// 可选小数秒（.fff）、可选时区（Z / +HH[:MM] / -HH[:MM]）。
fn parse_timestamp(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let n = b.len();
    let digit = |c: u8| c.is_ascii_digit();
    // 日期固定部分：YYYY-MM-DD
    if n < 10
        || !digit(b[0]) || !digit(b[1]) || !digit(b[2]) || !digit(b[3])
        || b[4] != b'-'
        || !digit(b[5]) || !digit(b[6])
        || b[7] != b'-'
        || !digit(b[8]) || !digit(b[9])
    {
        return None;
    }
    let y = (b[0] - b'0') as i64 * 1000 + (b[1] - b'0') as i64 * 100 + (b[2] - b'0') as i64 * 10 + (b[3] - b'0') as i64;
    let mo = (b[5] - b'0') as i64 * 10 + (b[6] - b'0') as i64;
    let d = (b[8] - b'0') as i64 * 10 + (b[9] - b'0') as i64;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    // 仅日期：2025-01-01 → 当日 00:00 UTC
    if n == 10 {
        return Some(days_from_civil(y, mo as u32, d as u32) * 86400);
    }
    // 分隔符：T 或空格
    let sep = b[10];
    if sep != b'T' && sep != b' ' {
        return None;
    }
    let two = |i: usize| -> Option<i64> {
        if i + 1 < n && digit(b[i]) && digit(b[i + 1]) {
            Some((b[i] - b'0') as i64 * 10 + (b[i + 1] - b'0') as i64)
        } else {
            None
        }
    };
    let h = two(11)?;
    if n < 14 || b[13] != b':' {
        return None;
    }
    let mi = two(14)?;
    let mut i = 16;
    let mut sec = 0i64;
    if i < n && b[i] == b':' {
        sec = two(i + 1)?;
        i += 3;
    }
    if h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    // 可选小数秒（忽略精度）
    if i < n && b[i] == b'.' {
        while i < n && digit(b[i]) {
            i += 1;
        }
    }
    // 可选时区：Z / +HH[:MM] / -HH[:MM]
    let mut offset = 0i64;
    if i < n {
        match b[i] {
            b'Z' | b'z' => i += 1,
            b'+' | b'-' => {
                let sign = if b[i] == b'-' { -1 } else { 1 };
                let oh = two(i + 1)?;
                let mut om = 0i64;
                let mut j = i + 3;
                if j < n && b[j] == b':' {
                    om = two(j + 1)?;
                    j += 3;
                }
                if oh > 23 || om > 59 {
                    return None;
                }
                offset = sign * (oh * 3600 + om * 60);
                i = j;
            }
            _ => return None,
        }
    }
    if i != n {
        return None;
    }
    Some(days_from_civil(y, mo as u32, d as u32) * 86400 + h * 3600 + mi * 60 + sec - offset)
}

// ---------- random（xorshift64*，线程本地） ----------

thread_local! {
    static RNG: RefCell<u64> = RefCell::new(seed_rng());
}

fn seed_rng() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    let addr = (&nanos as *const u64) as usize as u64;
    (nanos ^ addr).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1
}

fn next_u64() -> u64 {
    RNG.with(|r| {
        let mut x = r.borrow_mut();
        *x ^= *x >> 12;
        *x ^= *x << 25;
        *x ^= *x >> 27;
        *x = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        *x
    })
}

fn random_int(min: i64, max: i64) -> i64 {
    // 闭区间 [min, max]
    let span = (max as u64).wrapping_sub(min as u64).wrapping_add(1);
    if span == 0 {
        // 覆盖整个 i64 范围
        return next_u64() as i64;
    }
    (min as u64).wrapping_add(next_u64() % span) as i64
}

fn random_float() -> f64 {
    (next_u64() >> 11) as f64 / (1u64 << 53) as f64
}

// ---------- http（std::net + rustls 实现，支持 http:// 与 https://） ----------

/// 统一读写抽象：TcpStream 与 TlsStream 共用（netmod 等模块复用）
pub(crate) trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

/// TLS 配置：rustls + rustls-rustcrypto（纯 Rust 实现，无 C 依赖），
/// Windows/Linux/Termux 跨平台一致，webpki-roots 内置 Mozilla 根证书
pub(crate) static TLS: LazyLock<Result<std::sync::Arc<rustls::ClientConfig>, String>> = LazyLock::new(|| {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls_rustcrypto::provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| e.to_string())?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(std::sync::Arc::new(config))
});

/// 发送 HTTP 请求（默认超时 15 秒、无自定义头）。供 http_request / http_get_bytes 复用。
fn http_fetch_raw(
    url: &str,
    method: &str,
    body: Option<&str>,
    span: Span,
    file: &str,
    src: &str,
) -> Result<(String, Vec<u8>), ZError> {
    http_fetch_opts(url, method, body, &[], 15, span, file, src)
}

/// 发送 HTTP 请求并返回 (响应头文本, 原始响应体字节)。非 2xx 状态报错。
/// 支持自定义 Header（可覆盖 User-Agent / Content-Type）与超时秒数。
fn http_fetch_opts(
    url: &str,
    method: &str,
    body: Option<&str>,
    headers: &[(&str, &str)],
    timeout_secs: u64,
    span: Span,
    file: &str,
    src: &str,
) -> Result<(String, Vec<u8>), ZError> {
    // 按错误类型细分网络错误：超时 / 连接拒绝 / DNS 失败 / 其他
    let net_err = |act: &str, e: std::io::Error| {
        let (code, hint): (&'static str, &'static str) = match e.kind() {
            std::io::ErrorKind::TimedOut => (codes::NET_TIMEOUT, "the request timed out"),
            std::io::ErrorKind::ConnectionRefused => (codes::NET_CONN_REFUSED, "the connection was refused"),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::AddrNotAvailable => {
                (codes::NET_DNS, "DNS resolution failed")
            }
            _ => (codes::NETWORK, "check the URL or your network connection"),
        };
        err(code, format!("{}: {}: {}", act, url, e), span, file, src, Some(hint))
    };

    // 解析协议：http:// 走明文 TCP，https:// 走 TLS
    let (use_tls, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(err(
            codes::NETWORK,
            format!("{}: URL must start with `http://` or `https://`", url),
            span,
            file,
            src,
            Some("prefix the URL with `http://` or `https://`"),
        ));
    };
    let default_port = if use_tls { 443 } else { 80 };
    let (host_port, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match host_port.find(':') {
        Some(i) => (&host_port[..i], host_port[i + 1..].parse::<u16>().unwrap_or(default_port)),
        None => (host_port, default_port),
    };
    let addr = format!("{}:{}", host, port);

    let tcp = TcpStream::connect(&addr).map_err(|e| net_err("connect", e))?;
    tcp.set_read_timeout(Some(Duration::from_secs(timeout_secs))).ok();
    tcp.set_write_timeout(Some(Duration::from_secs(timeout_secs))).ok();

    // https 时做 TLS 握手（webpki-roots 内置 Mozilla 根证书验证）
    let mut stream: Box<dyn ReadWrite> = if use_tls {
        let connector = match TLS.as_ref() {
            Ok(c) => c.clone(),
            Err(e) => {
                return Err(err(
                    codes::NETWORK,
                    format!("TLS init failed: {}", e),
                    span,
                    file,
                    src,
                    None::<&str>,
                ));
            }
        };
        let server_name = match rustls::pki_types::ServerName::try_from(host.to_string()) {
            Ok(n) => n,
            Err(e) => {
                return Err(err(
                    codes::NETWORK,
                    format!("invalid hostname `{}`: {}", host, e),
                    span,
                    file,
                    src,
                    None::<&str>,
                ));
            }
        };
        match rustls::ClientConnection::new(connector, server_name) {
            Ok(conn) => Box::new(rustls::StreamOwned::new(conn, tcp)),
            Err(e) => {
                return Err(err(
                    codes::NETWORK,
                    format!("TLS handshake with {} failed: {}", host, e),
                    span,
                    file,
                    src,
                    Some("the server certificate may be invalid or self-signed"),
                ));
            }
        }
    } else {
        Box::new(tcp)
    };

    // Host 头：非默认端口时带上端口
    let host_header = if port == default_port {
        host.to_string()
    } else {
        format!("{}:{}", host, port)
    };

    // 请求头构造：默认头 + 自定义头（可覆盖 User-Agent / Content-Type）
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\n",
        method, path, host_header
    );
    let mut has_ua = false;
    let mut has_ct = false;
    for (k, v) in headers {
        let lower = k.to_ascii_lowercase();
        if lower == "user-agent" {
            has_ua = true;
        }
        if lower == "content-type" {
            has_ct = true;
        }
        head.push_str(&format!("{}: {}\r\n", k, v));
    }
    if !has_ua {
        head.push_str("User-Agent: hone/0.1.0\r\n");
    }
    let tail = match body {
        Some(b) => {
            if !has_ct {
                head.push_str("Content-Type: text/plain\r\n");
            }
            head.push_str(&format!("Content-Length: {}\r\n", b.len()));
            b.as_bytes().to_vec()
        }
        None => Vec::new(),
    };
    head.push_str("Connection: close\r\n\r\n");
    stream.write_all(head.as_bytes()).map_err(|e| net_err("write", e))?;
    if !tail.is_empty() {
        stream.write_all(&tail).map_err(|e| net_err("write", e))?;
    }

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).map_err(|e| net_err("read", e))?;

    // 拆分响应头与响应体（原始字节，供文本与二进制两种消费）
    let (head, body) = match buf.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(i) => (String::from_utf8_lossy(&buf[..i]).into_owned(), buf[i + 4..].to_vec()),
        None => (String::from_utf8_lossy(&buf).into_owned(), Vec::new()),
    };

    // 状态行检查
    let status_line = head.lines().next().unwrap_or("");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    if !(200..300).contains(&status) {
        return Err(err(
            codes::NET_HTTP_STATUS,
            format!("{}: HTTP status {}", url, status),
            span,
            file,
            src,
            Some("the server returned an error status"),
        ));
    }
    Ok((head, body))
}

/// 发送 HTTP 请求（interp 的 import 模块下载复用），返回响应体文本。
pub(crate) fn http_request(
    url: &str,
    method: &str,
    body: Option<&str>,
    span: Span,
    file: &str,
    src: &str,
) -> Result<String, ZError> {
    let (head, body_bytes) = http_fetch_raw(url, method, body, span, file, src)?;
    let mut body_text = String::from_utf8_lossy(&body_bytes).into_owned();
    // 处理 chunked 传输编码
    if head.to_lowercase().contains("transfer-encoding: chunked") {
        body_text = decode_chunked(&body_text);
    }
    Ok(body_text)
}

/// 原始字节下载（self-update 等二进制下载用），返回响应体字节。
pub(crate) fn http_get_bytes(url: &str, span: Span, file: &str, src: &str) -> Result<Vec<u8>, ZError> {
    let (head, mut body) = http_fetch_raw(url, "GET", None, span, file, src)?;
    if head.to_lowercase().contains("transfer-encoding: chunked") {
        body = decode_chunked_bytes(&body);
    }
    Ok(body)
}

/// 字节版 chunked 解码（二进制响应体用）。
fn decode_chunked_bytes(mut s: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let line_end = match s.windows(2).position(|w| w == b"\r\n") {
            Some(i) => i,
            None => break,
        };
        let size = match std::str::from_utf8(&s[..line_end])
            .ok()
            .and_then(|t| usize::from_str_radix(t.trim(), 16).ok())
        {
            Some(v) => v,
            None => break,
        };
        s = &s[line_end + 2..];
        if size == 0 {
            break;
        }
        if s.len() < size + 2 {
            out.extend_from_slice(&s[..s.len().min(size)]);
            break;
        }
        out.extend_from_slice(&s[..size]);
        s = &s[size + 2..];
    }
    out
}

fn decode_chunked(mut s: &str) -> String {
    let mut out = String::new();
    loop {
        let line_end = match s.find("\r\n") {
            Some(i) => i,
            None => break,
        };
        let size = match usize::from_str_radix(s[..line_end].trim(), 16) {
            Ok(v) => v,
            Err(_) => break,
        };
        s = &s[line_end + 2..];
        if size == 0 {
            break;
        }
        if s.len() < size + 2 {
            out.push_str(&s[..s.len().min(size)]);
            break;
        }
        out.push_str(&s[..size]);
        s = &s[size + 2..];
    }
    out
}

// ---------- json ----------

fn json_to_value(s: &str, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    let jv: serde_json::Value = serde_json::from_str(s).map_err(|e| {
        err(
            codes::TYPE_MISMATCH,
            format!("invalid JSON: {}", e),
            span,
            file,
            src,
            Some("check the JSON syntax"),
        )
    })?;
    match jv {
        serde_json::Value::Null => Ok(Value::Null),
        serde_json::Value::Bool(b) => Ok(Value::Bool(b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Value::Int(i))
            } else if let Some(f) = n.as_f64() {
                Ok(Value::Float(f))
            } else {
                Err(err(codes::TYPE_MISMATCH, "invalid JSON number", span, file, src, None::<&str>))
            }
        }
        serde_json::Value::String(s) => Ok(Value::Str(s)),
        serde_json::Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                out.push(json_to_value(&it.to_string(), span, file, src)?);
            }
            Ok(Value::List(out))
        }
        serde_json::Value::Object(map) => {
            let mut out = Vec::with_capacity(map.len());
            for (k, v) in map {
                out.push((k, json_to_value(&v.to_string(), span, file, src)?));
            }
            Ok(Value::Dict(out))
        }
    }
}

fn value_to_json(v: &Value, span: Span, file: &str, src: &str) -> Result<String, ZError> {
    let jv = match v {
        Value::Int(i) => serde_json::Value::Number((*i).into()),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .ok_or_else(|| err(codes::TYPE_MISMATCH, "cannot serialize NaN/infinity to JSON", span, file, src, None::<&str>))?,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Str(s) => serde_json::Value::String(s.clone()),
        Value::List(items) => {
            let mut arr = Vec::with_capacity(items.len());
            for it in items {
                arr.push(value_to_json(it, span, file, src).and_then(|s| {
                    serde_json::from_str(&s).map_err(|e| {
                        err(codes::TYPE_MISMATCH, format!("cannot serialize list item: {}", e), span, file, src, None::<&str>)
                    })
                })?);
            }
            serde_json::Value::Array(arr)
        }
        Value::Dict(entries) => {
            let mut map = serde_json::Map::new();
            for (k, v) in entries {
                let jv: serde_json::Value = value_to_json(v, span, file, src).and_then(|s| {
                    serde_json::from_str(&s).map_err(|e| {
                        err(codes::TYPE_MISMATCH, format!("cannot serialize dict value: {}", e), span, file, src, None::<&str>)
                    })
                })?;
                map.insert(k.clone(), jv);
            }
            serde_json::Value::Object(map)
        }
        Value::Null => serde_json::Value::Null,
        Value::Error(_) => {
            return Err(err(
                codes::TYPE_MISMATCH,
                "cannot serialize an `error` value to JSON",
                span,
                file,
                src,
                Some("convert the error to a string first, e.g. to_str(e)"),
            ));
        }
        Value::Ptr(_) => {
            return Err(err(
                codes::TYPE_MISMATCH,
                "cannot serialize a `ptr` value to JSON",
                span,
                file,
                src,
                Some("pointers are opaque handles; convert to a string first, e.g. to_str(p)"),
            ));
        }
        Value::Lambda(_) => {
            return Err(err(
                codes::TYPE_MISMATCH,
                "cannot serialize a `fn` (lambda) value to JSON",
                span,
                file,
                src,
                Some("call the lambda to get its result, or convert to a string first, e.g. to_str(f)"),
            ));
        }
    };
    Ok(jv.to_string())
}

// ---------- sys ----------

fn run_shell(cmd: &str, span: Span, file: &str, src: &str) -> Result<String, ZError> {
    let output = if cfg!(windows) {
        std::process::Command::new("cmd").args(["/C", cmd]).output()
    } else {
        std::process::Command::new("sh").args(["-c", cmd]).output()
    };
    match output {
        Ok(o) => {
            let mut out = String::from_utf8_lossy(&o.stdout).into_owned();
            out.push_str(&String::from_utf8_lossy(&o.stderr));
            Ok(out)
        }
        Err(e) => Err(err(
            codes::NETWORK,
            format!("cannot run command `{}`: {}", cmd, e),
            span,
            file,
            src,
            Some("check the command"),
        )),
    }
}
