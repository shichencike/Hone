// builtins.rs - Zap 内置函数
// 全部通过 `zap` 直接可用，无需导入。运行期校验参数类型（动态值兜底），
// 失败统一按 error[Zxxx] 格式报告。

use std::cell::RefCell;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::codes;
use crate::error::ZError;
use crate::interp::Value;
use crate::lexer::Span;

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
            | "type_of"
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
            | "random.int"
            | "random.float"
            | "http_get"
            | "http_post"
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
                other => Err(err(
                    codes::TYPE_MISMATCH,
                    format!("`len` expects a string, got `{}`", other.type_name()),
                    span,
                    file,
                    src,
                    Some("`len` returns the byte length of a string"),
                )),
            }
        }
        "type_of" => {
            let v = args.get(0).ok_or_else(|| arg_err(name, 1, 0, span, file, src))?;
            Ok(Value::Str(v.type_name().to_string()))
        }
        "to_str" => {
            let v = args.get(0).ok_or_else(|| arg_err(name, 1, 0, span, file, src))?;
            match v {
                Value::Int(_) | Value::Float(_) | Value::Bool(_) => Ok(Value::Str(v.display())),
                Value::Str(s) => Ok(Value::Str(s.clone())),
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
                err(
                    codes::NOT_FOUND,
                    format!("cannot read file `{}`: {}", p, e),
                    span,
                    file,
                    src,
                    Some("check the path and file permissions"),
                )
            })
        }
        "write_file" => {
            let p = as_str(&args[0], 0, name, span, file, src)?;
            let c = as_str(&args[1], 1, name, span, file, src)?;
            std::fs::write(p, c).map_err(|e| {
                err(
                    codes::NOT_FOUND,
                    format!("cannot write file `{}`: {}", p, e),
                    span,
                    file,
                    src,
                    Some("check the path and file permissions"),
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
                        Some("Zap has no implicit type conversion"),
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

// ---------- time ----------

/// 将 Unix 时间戳（秒）按格式串格式化（UTC）。占位符：YYYY MM DD HH mm SS。
fn format_timestamp(secs: i64, fmt: &str) -> String {
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

// ---------- http（std::net 实现，仅 http://） ----------

/// 发送 HTTP 请求（interp 的 import 模块下载复用）。
pub(crate) fn http_request(
    url: &str,
    method: &str,
    body: Option<&str>,
    span: Span,
    file: &str,
    src: &str,
) -> Result<String, ZError> {
    let net_err = |m: String| err(codes::NETWORK, format!("{}: {}", url, m), span, file, src, Some("check the URL or your network connection"));

    let rest = match url.strip_prefix("http://") {
        Some(r) => r,
        None => {
            return Err(err(
                codes::NETWORK,
                format!("{}: only `http://` URLs are supported (no TLS in this build)", url),
                span,
                file,
                src,
                Some("use an `http://` URL"),
            ));
        }
    };
    let (host_port, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match host_port.find(':') {
        Some(i) => (&host_port[..i], host_port[i + 1..].parse::<u16>().unwrap_or(80)),
        None => (host_port, 80),
    };
    let addr = format!("{}:{}", host, port);

    let mut stream = TcpStream::connect(&addr).map_err(|e| net_err(e.to_string()))?;
    stream.set_read_timeout(Some(Duration::from_secs(15))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(15))).ok();

    let (head, tail) = match body {
        Some(b) => (
            format!(
                "{} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: zap/0.1.0\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                method,
                path,
                host,
                b.len()
            ),
            b.as_bytes().to_vec(),
        ),
        None => (
            format!(
                "{} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: zap/0.1.0\r\nConnection: close\r\n\r\n",
                method, path, host
            ),
            Vec::new(),
        ),
    };
    stream.write_all(head.as_bytes()).map_err(|e| net_err(e.to_string()))?;
    if !tail.is_empty() {
        stream.write_all(&tail).map_err(|e| net_err(e.to_string()))?;
    }

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).map_err(|e| net_err(e.to_string()))?;
    let text = String::from_utf8_lossy(&buf).into_owned();

    let (head, mut body_text) = match text.split_once("\r\n\r\n") {
        Some((h, b)) => (h.to_string(), b.to_string()),
        None => (text.clone(), String::new()),
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
            codes::NETWORK,
            format!("{}: HTTP status {}", url, status),
            span,
            file,
            src,
            Some("the server returned an error status"),
        ));
    }

    // 处理 chunked 传输编码
    if head.to_lowercase().contains("transfer-encoding: chunked") {
        body_text = decode_chunked(&body_text);
    }
    Ok(body_text)
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
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => Err(err(
            codes::NOT_IMPLEMENTED,
            "JSON arrays and objects are not supported yet in v0.1.0",
            span,
            file,
            src,
            Some("parse the JSON into a scalar value for now"),
        )),
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
        Value::Null => serde_json::Value::Null,
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
