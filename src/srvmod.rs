// srvmod.rs - 本地 HTTP 服务器（server.* 内置函数）
// 纯 std::net 实现，Windows / Linux / Termux 跨平台一致，无 C 依赖。
//   server.listen(port)     -> int   启动后台监听线程，返回实际端口（port=0 自动分配）
//   server.poll()           -> str   取出排队请求，返回 JSON 数组 [{id,method,path,body}, ...]
//   server.respond(id, body[, status])-> bool  发送响应体（默认 HTTP 200，可指定状态码如 404/500），成功返回 true
// 事件模型：后台线程只做 TCP 收发与请求排队；Hone 脚本在主线程轮询（poll）并响应
// （respond），与解释器单线程模型完全兼容——Hone 函数只在脚本侧被调用，无跨线程状态。

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{mpsc, Mutex};
use std::time::Duration;

use std::sync::LazyLock;

use crate::error::codes;
use crate::error::ZError;
use crate::interp::Value;
use crate::lexer::Span;

/// 一个排队等待脚本处理的请求。
struct PendingReq {
    id: u64,
    method: String,
    path: String,
    body: String,
}

/// 待处理请求队列（server.poll 取走）。
static QUEUE: Mutex<VecDeque<PendingReq>> = Mutex::new(VecDeque::new());
/// id -> 响应通道（server.respond 发送 (状态码, 响应体) 后由后台线程写回浏览器）。
static RESPONDERS: LazyLock<Mutex<HashMap<u64, SyncSender<(u16, String)>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
/// 请求 id 分配器。
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// 队列上限：脚本不轮询时丢弃最旧请求，防止无限增长。
const QUEUE_CAP: usize = 1024;
/// 后台线程等待脚本响应的最长时间。
const RESPOND_TIMEOUT: Duration = Duration::from_secs(120);

fn zerr(code: &'static str, msg: impl Into<String>, span: Span, file: &str, src: &str, help: Option<impl Into<String>>) -> ZError {
    ZError::new(code, msg, file, src, span.line, span.col, span.len.max(1), help)
}

fn as_str<'a>(v: &'a Value, arg: usize, span: Span, file: &str, src: &str) -> Result<&'a str, ZError> {
    match v {
        Value::Str(s) => Ok(s),
        other => Err(zerr(
            codes::TYPE_MISMATCH,
            format!(
                "`server.respond` expects a string for argument {}, got `{}`",
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

/// server 模块调用入口（参数数量已由 checker 静态校验）。
pub fn call(name: &str, args: &[Value], span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    match name {
        "server.listen" => {
            let port = match args.get(0) {
                Some(Value::Int(p)) => *p,
                Some(other) => {
                    return Err(zerr(
                        codes::TYPE_MISMATCH,
                        format!("`server.listen` expects an integer port, got `{}`", other.type_name()),
                        span,
                        file,
                        src,
                        Some("pass `0` to auto-assign a free port"),
                    ));
                }
                None => {
                    return Err(zerr(codes::ARG_COUNT, "`server.listen` expects 1 argument (port)", span, file, src, None::<&str>));
                }
            };
            listen(port, span, file, src).map(Value::Int)
        }
        "server.poll" => Ok(Value::Str(poll())),
        "server.respond" => {
            let id = match args.get(0) {
                Some(Value::Int(i)) => *i,
                Some(other) => {
                    return Err(zerr(
                        codes::TYPE_MISMATCH,
                        format!("`server.respond` expects an integer request id, got `{}`", other.type_name()),
                        span,
                        file,
                        src,
                        Some("pass the `id` from `server.poll`"),
                    ));
                }
                None => {
                    return Err(zerr(codes::ARG_COUNT, "`server.respond` expects 2-3 arguments (id, body[, status])", span, file, src, None::<&str>));
                }
            };
            let body = match args.get(1) {
                Some(v) => as_str(v, 1, span, file, src)?,
                None => {
                    return Err(zerr(codes::ARG_COUNT, "`server.respond` expects 2-3 arguments (id, body[, status])", span, file, src, None::<&str>));
                }
            };
            // 可选第三参数：HTTP 状态码（默认 200）
            let status = match args.get(2) {
                None => 200u16,
                Some(Value::Int(s)) if (100..=599).contains(s) => *s as u16,
                Some(Value::Int(s)) => {
                    return Err(zerr(
                        codes::TYPE_MISMATCH,
                        format!("`server.respond` expects an HTTP status code in 100..=599, got `{}`", s),
                        span,
                        file,
                        src,
                        Some("common codes: 200, 404, 500"),
                    ));
                }
                Some(other) => {
                    return Err(zerr(
                        codes::TYPE_MISMATCH,
                        format!("`server.respond` expects an integer status code, got `{}`", other.type_name()),
                        span,
                        file,
                        src,
                        Some("pass the status code as the 3rd argument, e.g. `server.respond(id, body, 404)`"),
                    ));
                }
            };
            Ok(Value::Bool(respond(id, status, body)))
        }
        _ => Err(zerr(
            codes::NOT_IMPLEMENTED,
            format!("unknown server function `{}`", name),
            span,
            file,
            src,
            None::<&str>,
        )),
    }
}

/// 绑定 127.0.0.1:port 并启动后台监听线程，返回实际端口。
fn listen(port: i64, span: Span, file: &str, src: &str) -> Result<i64, ZError> {
    if !(0..=65535).contains(&port) {
        return Err(zerr(
            codes::SYSCALL,
            format!("invalid port `{}`", port),
            span,
            file,
            src,
            Some("port must be in 0..=65535"),
        ));
    }
    let addr = format!("127.0.0.1:{}", port);
    let listener = TcpListener::bind(&addr).map_err(|e| {
        zerr(
            codes::SYSCALL,
            format!("cannot bind {}: {}", addr, e),
            span,
            file,
            src,
            Some("check the port is not already in use"),
        )
    })?;
    let actual = listener.local_addr().map_err(|e| {
        zerr(codes::SYSCALL, format!("cannot resolve bound address: {}", e), span, file, src, None::<&str>)
    })?.port();
    std::thread::spawn(move || accept_loop(listener));
    Ok(actual as i64)
}

fn accept_loop(listener: TcpListener) {
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                std::thread::spawn(move || handle_conn(s));
            }
            Err(_) => continue,
        }
    }
}

fn handle_conn(mut stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let (method, path, body) = match read_request(&mut stream) {
        Some(r) => r,
        None => {
            let _ = write_resp(&mut stream, 400, "text/plain; charset=utf-8", "bad request");
            return;
        }
    };
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = mpsc::sync_channel::<(u16, String)>(1);
    RESPONDERS.lock().unwrap().insert(id, tx);
    {
        let mut q = QUEUE.lock().unwrap();
        if q.len() >= QUEUE_CAP {
            q.pop_front(); // 丢弃最旧请求，防止队列无限增长
        }
        q.push_back(PendingReq { id, method, path: path.clone(), body });
    }
    // 等待脚本通过 server.respond 发送响应
    let wait = match rx.recv_timeout(RESPOND_TIMEOUT) {
        Ok((status, body)) => (status, body),
        Err(_) => {
            cleanup(id);
            let _ = write_resp(&mut stream, 504, "text/plain; charset=utf-8", "no response from script");
            return;
        }
    };
    let ctype = content_type(&path);
    if write_resp(&mut stream, wait.0, &ctype, &wait.1).is_err() {
        cleanup(id); // 浏览器已断开，移除响应通道
    }
}

/// 移除指定 id 的响应通道（响应已发送或连接已失效）。
fn cleanup(id: u64) {
    RESPONDERS.lock().unwrap().remove(&id);
}

/// 读取并解析一个 HTTP 请求：请求行 + 头 + Content-Length 指定的 body。
fn read_request(stream: &mut TcpStream) -> Option<(String, String, String)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        match stream.read(&mut chunk) {
            Ok(0) => return None,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
                if buf.len() > 64 * 1024 {
                    return None; // 头过大
                }
            }
            Err(_) => return None,
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let req_line: Vec<&str> = lines.next()?.split_whitespace().collect();
    if req_line.len() < 2 {
        return None;
    }
    let method = req_line[0].to_string();
    let path = req_line[1].to_string();
    let mut clen = 0usize;
    for line in lines {
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            clen = v.trim().parse().ok()?;
        }
    }
    let mut body = buf[header_end..].to_vec();
    while body.len() < clen {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    body.truncate(clen);
    let body_str = String::from_utf8_lossy(&body).to_string();
    Some((method, path, body_str))
}

/// 按路径后缀推断 Content-Type。
fn content_type(path: &str) -> String {
    let p = path.split('?').next().unwrap_or("");
    let ext = p.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let base = match ext.as_str() {
        "html" | "htm" => "text/html",
        "js" | "mjs" => "text/javascript",
        "css" => "text/css",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "txt" => "text/plain",
        _ => "text/html",
    };
    if base.starts_with("text/") || base.contains("json") || base.contains("svg") {
        format!("{}; charset=utf-8", base)
    } else {
        base.to_string()
    }
}

/// 写出完整 HTTP 响应（Connection: close）。
fn write_resp(stream: &mut TcpStream, status: u16, ctype: &str, body: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Status",
    };
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
        status,
        reason,
        ctype,
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

/// 取出所有排队请求，序列化为 JSON 数组 [{id,method,path,body}, ...]。
fn poll() -> String {
    let mut q = QUEUE.lock().unwrap();
    if q.is_empty() {
        return "[]".to_string();
    }
    let mut out = String::from("[");
    let mut first = true;
    while let Some(r) = q.pop_front() {
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str(&format!(
            "{{\"id\":{},\"method\":{},\"path\":{},\"body\":{}}}",
            r.id,
            json_escape(&r.method),
            json_escape(&r.path),
            json_escape(&r.body)
        ));
    }
    out.push(']');
    out
}

/// JSON 字符串转义（值两侧加双引号）。
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// 发送响应体到指定请求的后台线程（非阻塞，成功后该 id 立即失效）。
fn respond(id: i64, status: u16, body: &str) -> bool {
    let mut resp = RESPONDERS.lock().unwrap();
    match resp.remove(&(id as u64)) {
        Some(tx) => {
            let _ = tx.send((status, body.to_string()));
            true
        }
        None => false,
    }
}
