// netmod.rs - 网络与通信（smtp.* / ws.* 内置函数）
// 复用 builtins 的 TLS 配置（rustls + rustls-rustcrypto，纯 Rust），跨平台一致。
//   smtp.send(host, port, opts) -> bool  发送邮件
//     opts: {from, to(单个或列表), subject, body, user?, password?, starttls?(默认 true)}
//     - port 465：隐式 TLS（SMTPS）；其他端口默认 STARTTLS，starttls=false 时明文
//     - user/password 提供时启用 AUTH LOGIN
//   ws.request(url, message[, timeout]) -> str  WebSocket 一次性请求-响应
//     - 建立连接 + 握手（校验 Sec-WebSocket-Accept），发送一个文本帧，
//       读取服务端文本帧直到 close 帧或超时，返回拼接文本
//     - 支持 ws:// 与 wss://（TLS）

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::builtins::TLS;
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
            format!("expected a string for argument {}, got `{}`", arg + 1, other.type_name()),
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
            format!("expected an int for argument {}, got `{}`", arg + 1, other.type_name()),
            span,
            file,
            src,
            None::<&str>,
        )),
    }
}

/// 连接抽象：明文 TCP 或 TLS（STARTTLS 需要从明文升级，故用枚举持有底层流）。
enum Conn {
    Plain(TcpStream),
    Tls(rustls::StreamOwned<rustls::ClientConnection, TcpStream>),
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Conn::Plain(s) => s.read(buf),
            Conn::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Conn::Plain(s) => s.write(buf),
            Conn::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Conn::Plain(s) => s.flush(),
            Conn::Tls(s) => s.flush(),
        }
    }
}

/// 建立到 host:port 的 TCP 连接；use_tls=true 时立即做 TLS 握手。
fn connect(host: &str, port: u16, use_tls: bool, timeout_secs: u64, span: Span, file: &str, src: &str) -> Result<Conn, ZError> {
    let addr = format!("{}:{}", host, port);
    let tcp = TcpStream::connect(&addr).map_err(|e| {
        zerr(
            codes::NETWORK,
            format!("connect {}: {}", addr, e),
            span,
            file,
            src,
            Some("check the host/port or your network"),
        )
    })?;
    tcp.set_read_timeout(Some(Duration::from_secs(timeout_secs))).ok();
    tcp.set_write_timeout(Some(Duration::from_secs(timeout_secs))).ok();
    if !use_tls {
        return Ok(Conn::Plain(tcp));
    }
    let connector = match TLS.as_ref() {
        Ok(c) => c.clone(),
        Err(e) => {
            return Err(zerr(codes::NETWORK, format!("TLS init failed: {}", e), span, file, src, None::<&str>))
        }
    };
    let server_name = match rustls::pki_types::ServerName::try_from(host.to_string()) {
        Ok(n) => n,
        Err(e) => {
            return Err(zerr(codes::NETWORK, format!("invalid hostname `{}`: {}", host, e), span, file, src, None::<&str>))
        }
    };
    match rustls::ClientConnection::new(connector, server_name) {
        Ok(conn) => Ok(Conn::Tls(rustls::StreamOwned::new(conn, tcp))),
        Err(e) => Err(zerr(
            codes::NETWORK,
            format!("TLS handshake with {} failed: {}", host, e),
            span,
            file,
            src,
            Some("the server certificate may be invalid or self-signed"),
        )),
    }
}

/// 将明文连接升级为 TLS（STARTTLS 用）。失败返回原连接语义的错误。
fn upgrade_tls(conn: Conn, host: &str, span: Span, file: &str, src: &str) -> Result<Conn, ZError> {
    match conn {
        Conn::Plain(tcp) => {
            let connector = match TLS.as_ref() {
                Ok(c) => c.clone(),
                Err(e) => {
                    return Err(zerr(codes::NETWORK, format!("TLS init failed: {}", e), span, file, src, None::<&str>))
                }
            };
            let server_name = match rustls::pki_types::ServerName::try_from(host.to_string()) {
                Ok(n) => n,
                Err(e) => {
                    return Err(zerr(codes::NETWORK, format!("invalid hostname `{}`: {}", host, e), span, file, src, None::<&str>))
                }
            };
            match rustls::ClientConnection::new(connector, server_name) {
                Ok(conn) => Ok(Conn::Tls(rustls::StreamOwned::new(conn, tcp))),
                Err(e) => Err(zerr(
                    codes::NETWORK,
                    format!("TLS handshake with {} failed: {}", host, e),
                    span,
                    file,
                    src,
                    Some("the server certificate may be invalid or self-signed"),
                )),
            }
        }
        tls => Ok(tls),
    }
}

/// 读取一行（以 \n 结尾，去除 \r\n）。
fn read_line(conn: &mut Conn) -> Result<String, ZError> {
    let mut line = String::new();
    let mut buf = [0u8; 1];
    loop {
        let n = conn.read(&mut buf).map_err(|e| {
            zerr(codes::NET_TIMEOUT, format!("read timed out: {}", e), Span { line: 0, col: 0, len: 0 }, "", "", None::<&str>)
        })?;
        if n == 0 {
            break;
        }
        if buf[0] == b'\n' {
            break;
        }
        line.push(buf[0] as char);
    }
    Ok(line.trim_end_matches('\r').to_string())
}

/// SMTP 响应码提取：前 3 位数字。
fn smtp_code(line: &str) -> Option<u16> {
    let t = line.trim();
    if t.len() >= 3 && t.as_bytes()[..3].iter().all(|c| c.is_ascii_digit()) {
        t[..3].parse::<u16>().ok()
    } else {
        None
    }
}

/// 读取 SMTP 响应（可能多行：250-xxx ... 250 结束）。
fn smtp_reply(conn: &mut Conn) -> Result<(u16, String), ZError> {
    let mut code: u16;
    let mut text = String::new();
    loop {
        let line = read_line(conn)?;
        let c = match smtp_code(&line) {
            Some(c) => c,
            None => {
                return Err(zerr(codes::NETWORK, format!("invalid SMTP response: `{}`", line), Span { line: 0, col: 0, len: 0 }, "", "", None::<&str>))
            }
        };
        code = c;
        text.push_str(&line);
        text.push('\n');
        // 行首第 4 位为空格表示响应结束；'-' 表示还有后续行
        let trimmed = line.trim_start();
        let cont = trimmed.as_bytes().get(3) == Some(&b'-');
        if !cont {
            break;
        }
    }
    Ok((code, text.trim().to_string()))
}

/// 发送一行命令并等待响应码。
fn smtp_cmd(conn: &mut Conn, cmd: &str) -> Result<u16, ZError> {
    conn.write_all(cmd.as_bytes()).map_err(|e| zerr(codes::NETWORK, format!("smtp write failed: {}", e), Span { line: 0, col: 0, len: 0 }, "", "", None::<&str>))?;
    conn.write_all(b"\r\n").map_err(|e| zerr(codes::NETWORK, format!("smtp write failed: {}", e), Span { line: 0, col: 0, len: 0 }, "", "", None::<&str>))?;
    let (code, _) = smtp_reply(conn)?;
    Ok(code)
}

/// smtp/ws 模块调用入口。
pub fn call(name: &str, args: &[Value], span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    match name {
        "smtp.send" => {
            let host = as_str(&args[0], 0, span, file, src)?;
            let port = as_int(&args[1], 1, span, file, src)?;
            let opts = match &args[2] {
                Value::Dict(entries) => entries,
                other => {
                    return Err(zerr(
                        codes::TYPE_MISMATCH,
                        format!("`smtp.send` expects a dict of options, got `{}`", other.type_name()),
                        span,
                        file,
                        src,
                        Some("form: smtp.send(host, port, {from, to, subject, body, ...})"),
                    ))
                }
            };
            let mut from = "";
            let mut to: Vec<String> = Vec::new();
            let mut subject = "";
            let mut body = "";
            let mut user: Option<&str> = None;
            let mut password: Option<&str> = None;
            let mut starttls = true;
            for (k, v) in opts {
                match k.as_str() {
                    "from" => from = as_str(v, 0, span, file, src)?,
                    "to" => match v {
                        Value::Str(s) => to.push(s.clone()),
                        Value::List(items) => {
                            for it in items {
                                to.push(as_str(it, 0, span, file, src)?.to_string());
                            }
                        }
                        other => {
                            return Err(zerr(
                                codes::TYPE_MISMATCH,
                                format!("`smtp.send` `to` must be a string or list of strings, got `{}`", other.type_name()),
                                span,
                                file,
                                src,
                                None::<&str>,
                            ))
                        }
                    },
                    "subject" => subject = as_str(v, 0, span, file, src)?,
                    "body" => body = as_str(v, 0, span, file, src)?,
                    "user" => user = Some(as_str(v, 0, span, file, src)?),
                    "password" => password = Some(as_str(v, 0, span, file, src)?),
                    "starttls" => match v {
                        Value::Bool(b) => starttls = *b,
                        other => {
                            return Err(zerr(
                                codes::TYPE_MISMATCH,
                                format!("`smtp.send` starttls must be bool, got `{}`", other.type_name()),
                                span,
                                file,
                                src,
                                None::<&str>,
                            ))
                        }
                    },
                    _ => {}
                }
            }
            if from.is_empty() || to.is_empty() {
                return Err(zerr(
                    codes::TYPE_MISMATCH,
                    "`smtp.send` requires `from` and `to` options",
                    span,
                    file,
                    src,
                    Some("form: smtp.send(host, port, {from, to, subject, body})"),
                ));
            }
            // 端口 465 = 隐式 TLS；否则按 starttls 决定
            let implicit_tls = port == 465;
            let mut conn = connect(host, port as u16, implicit_tls, 30, span, file, src)?;
            let (greet_code, _) = smtp_reply(&mut conn)?;
            if greet_code != 220 {
                return Err(zerr(codes::NETWORK, format!("SMTP server refused connection (code {})", greet_code), span, file, src, None::<&str>));
            }
            // EHLO
            let ehlo = smtp_cmd(&mut conn, "EHLO hone")?;
            if ehlo != 250 {
                return Err(zerr(codes::NETWORK, format!("EHLO failed (code {})", ehlo), span, file, src, None::<&str>));
            }
            // STARTTLS（非隐式 TLS 且开启时）
            if !implicit_tls && starttls {
                let st = smtp_cmd(&mut conn, "STARTTLS")?;
                if st != 220 {
                    return Err(zerr(codes::NETWORK, format!("STARTTLS failed (code {})", st), span, file, src, Some("the server may not support STARTTLS; try starttls=false or port 465")));
                }
                conn = upgrade_tls(conn, host, span, file, src)?;
                let ehlo2 = smtp_cmd(&mut conn, "EHLO hone")?;
                if ehlo2 != 250 {
                    return Err(zerr(codes::NETWORK, format!("EHLO (TLS) failed (code {})", ehlo2), span, file, src, None::<&str>));
                }
            }
            // AUTH LOGIN（可选）
            if let Some(u) = user {
                let a = smtp_cmd(&mut conn, "AUTH LOGIN")?;
                if a != 334 {
                    return Err(zerr(codes::NETWORK, format!("AUTH LOGIN failed (code {})", a), span, file, src, Some("the server does not support AUTH LOGIN or auth is not needed")));
                }
                use base64::Engine;
                let ub = base64::engine::general_purpose::STANDARD.encode(u.as_bytes());
                let uc = smtp_cmd(&mut conn, &ub)?;
                if uc != 334 {
                    return Err(zerr(codes::NETWORK, format!("AUTH username rejected (code {})", uc), span, file, src, None::<&str>));
                }
                let pw = password.unwrap_or("");
                let pb = base64::engine::general_purpose::STANDARD.encode(pw.as_bytes());
                let pc = smtp_cmd(&mut conn, &pb)?;
                if pc != 235 {
                    return Err(zerr(codes::NETWORK, format!("AUTH password rejected (code {})", pc), span, file, src, Some("check the user/password")));
                }
            }
            // MAIL FROM / RCPT TO
            let mf = smtp_cmd(&mut conn, &format!("MAIL FROM:<{}>", from))?;
            if mf != 250 {
                return Err(zerr(codes::NETWORK, format!("MAIL FROM failed (code {})", mf), span, file, src, None::<&str>));
            }
            for rcpt in &to {
                let rc = smtp_cmd(&mut conn, &format!("RCPT TO:<{}>", rcpt))?;
                if rc != 250 {
                    return Err(zerr(codes::NETWORK, format!("RCPT TO `{}` failed (code {})", rcpt, rc), span, file, src, None::<&str>));
                }
            }
            // DATA
            let dc = smtp_cmd(&mut conn, "DATA")?;
            if dc != 354 {
                return Err(zerr(codes::NETWORK, format!("DATA failed (code {})", dc), span, file, src, None::<&str>));
            }
            let msg = format!(
                "From: {}\r\nTo: {}\r\nSubject: {}\r\n\r\n{}\r\n.\r\n",
                from,
                to.join(", "),
                subject,
                body.replace("\r\n.", "\r\n..") // 行首点转义（dot-stuffing）
            );
            conn.write_all(msg.as_bytes()).map_err(|e| zerr(codes::NETWORK, format!("smtp write failed: {}", e), span, file, src, None::<&str>))?;
            let (end_code, _) = smtp_reply(&mut conn)?;
            if end_code != 250 {
                return Err(zerr(codes::NETWORK, format!("message rejected (code {})", end_code), span, file, src, None::<&str>));
            }
            let _ = smtp_cmd(&mut conn, "QUIT");
            Ok(Value::Bool(true))
        }
        "ws.request" => {
            let url = as_str(&args[0], 0, span, file, src)?;
            let message = as_str(&args[1], 1, span, file, src)?;
            let timeout = match args.get(2) {
                Some(Value::Int(i)) if *i > 0 => *i as u64,
                Some(Value::Float(f)) if *f > 0.0 => *f as u64,
                None => 30u64,
                Some(other) => {
                    return Err(zerr(
                        codes::TYPE_MISMATCH,
                        format!("`ws.request` timeout must be a positive number, got `{}`", other.type_name()),
                        span,
                        file,
                        src,
                        None::<&str>,
                    ))
                }
            };
            // 解析 ws:// 或 wss:// URL
            let (use_tls, rest) = if let Some(r) = url.strip_prefix("wss://") {
                (true, r)
            } else if let Some(r) = url.strip_prefix("ws://") {
                (false, r)
            } else {
                return Err(zerr(
                    codes::NETWORK,
                    "`ws.request` URL must start with `ws://` or `wss://`",
                    span,
                    file,
                    src,
                    None::<&str>,
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
            if host.is_empty() {
                return Err(zerr(codes::NETWORK, "`ws.request` URL missing host", span, file, src, None::<&str>));
            }
            let mut conn = connect(host, port, use_tls, timeout, span, file, src)?;
            // 握手
            let key = ws_random_key();
            let req = format!(
                "GET {} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n\r\n",
                path,
                host_port,
                key
            );
            conn.write_all(req.as_bytes()).map_err(|e| zerr(codes::NETWORK, format!("ws write failed: {}", e), span, file, src, None::<&str>))?;
            // 读取响应头
            let mut resp = String::new();
            let mut buf = [0u8; 1];
            let mut found_sep = false;
            while resp.len() < 65536 {
                let n = conn.read(&mut buf).map_err(|e| zerr(codes::NET_TIMEOUT, format!("ws handshake timed out: {}", e), span, file, src, None::<&str>))?;
                if n == 0 {
                    break;
                }
                resp.push(buf[0] as char);
                if resp.ends_with("\r\n\r\n") {
                    found_sep = true;
                    break;
                }
            }
            if !found_sep {
                return Err(zerr(codes::NETWORK, "ws handshake: incomplete response", span, file, src, None::<&str>));
            }
            // 校验 101 与 Sec-WebSocket-Accept
            let status_ok = resp.starts_with("HTTP/1.1 101");
            let accept_line = resp
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("sec-websocket-accept:"))
                .map(|l| l.split_once(':').map(|(_, v)| v.trim().to_string()))
                .flatten();
            if !status_ok {
                let first = resp.lines().next().unwrap_or("").to_string();
                return Err(zerr(codes::NET_HTTP_STATUS, format!("ws handshake rejected: {}", first), span, file, src, None::<&str>));
            }
            if let Some(expected) = ws_accept(&key) {
                if let Some(actual) = accept_line {
                    if actual != expected {
                        return Err(zerr(codes::NETWORK, "ws handshake: Sec-WebSocket-Accept mismatch", span, file, src, None::<&str>));
                    }
                }
            }
            // 发送文本帧（客户端必须掩码）
            ws_send_frame(&mut conn, 0x1, message.as_bytes(), span, file, src)?;
            // 读取服务端帧，直到 close 帧或超时
            let mut out = String::new();
            loop {
                let frame = ws_read_frame(&mut conn, span, file, src);
                match frame {
                    Ok((opcode, payload)) => {
                        match opcode {
                            0x1 => out.push_str(&String::from_utf8_lossy(&payload)),
                            0x8 => break, // close
                            0x9 => {
                                // ping -> pong
                                let _ = ws_send_frame(&mut conn, 0xA, &payload, span, file, src);
                            }
                            _ => {}
                        }
                    }
                    Err(_) => break, // 超时或连接关闭
                }
            }
            Ok(Value::Str(out))
        }
        _ => Err(zerr(
            codes::NOT_IMPLEMENTED,
            format!("unknown smtp/ws function `{}`", name),
            span,
            file,
            src,
            None::<&str>,
        )),
    }
}

// ---------- WebSocket 帧与握手辅助 ----------

/// 生成 Sec-WebSocket-Key（base64 编码的 16 随机字节）。
fn ws_random_key() -> String {
    use base64::Engine;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x1234_5678_9ABC_DEF0);
    let addr = (&nanos as *const u64) as usize as u64;
    let mut x = (nanos ^ addr) | 1;
    let mut bytes = [0u8; 16];
    for b in bytes.iter_mut() {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        x = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        *b = (x >> 32) as u8;
    }
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// 计算期望的 Sec-WebSocket-Accept（SHA-1(key + GUID) 的 base64）。
fn ws_accept(key: &str) -> Option<String> {
    use base64::Engine;
    use sha1::Digest;
    let mut hasher = sha1::Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    let digest = hasher.finalize();
    Some(base64::engine::general_purpose::STANDARD.encode(digest))
}

/// 发送一个 WebSocket 帧（客户端必须掩码）。
fn ws_send_frame(conn: &mut Conn, opcode: u8, payload: &[u8], span: Span, file: &str, src: &str) -> Result<(), ZError> {
    let mut head = vec![0x80 | opcode]; // FIN=1
    let len = payload.len();
    if len < 126 {
        head.push(0x80 | len as u8); // MASK=1
    } else if len < 65536 {
        head.push(0x80 | 126);
        head.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        head.push(0x80 | 127);
        head.extend_from_slice(&(len as u64).to_be_bytes());
    }
    // 4 字节掩码
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u32)
        .unwrap_or(0xDEAD_BEEF);
    let mask = nanos.to_le_bytes();
    head.extend_from_slice(&mask);
    let mut masked = Vec::with_capacity(payload.len());
    for (i, b) in payload.iter().enumerate() {
        masked.push(b ^ mask[i % 4]);
    }
    conn.write_all(&head).map_err(|e| zerr(codes::NETWORK, format!("ws write failed: {}", e), span, file, src, None::<&str>))?;
    conn.write_all(&masked).map_err(|e| zerr(codes::NETWORK, format!("ws write failed: {}", e), span, file, src, None::<&str>))?;
    Ok(())
}

/// 读取一个 WebSocket 帧，返回 (opcode, payload)。服务端帧不掩码，但兼容掩码。
fn ws_read_frame(conn: &mut Conn, span: Span, file: &str, src: &str) -> Result<(u8, Vec<u8>), ZError> {
    let mut b0 = [0u8; 1];
    conn.read_exact(&mut b0).map_err(|_| zerr(codes::NET_TIMEOUT, "ws read timed out", span, file, src, None::<&str>))?;
    let opcode = b0[0] & 0x0F;
    let mut b1 = [0u8; 1];
    conn.read_exact(&mut b1).map_err(|_| zerr(codes::NET_TIMEOUT, "ws read timed out", span, file, src, None::<&str>))?;
    let masked = b1[0] & 0x80 != 0;
    let mut len7 = (b1[0] & 0x7F) as u64;
    if len7 == 126 {
        let mut b = [0u8; 2];
        conn.read_exact(&mut b).map_err(|_| zerr(codes::NET_TIMEOUT, "ws read timed out", span, file, src, None::<&str>))?;
        len7 = u16::from_be_bytes(b) as u64;
    } else if len7 == 127 {
        let mut b = [0u8; 8];
        conn.read_exact(&mut b).map_err(|_| zerr(codes::NET_TIMEOUT, "ws read timed out", span, file, src, None::<&str>))?;
        len7 = u64::from_be_bytes(b);
    }
    if len7 > 64 * 1024 * 1024 {
        return Err(zerr(codes::NETWORK, "ws frame too large", span, file, src, None::<&str>));
    }
    let mut mask = [0u8; 4];
    if masked {
        conn.read_exact(&mut mask).map_err(|_| zerr(codes::NET_TIMEOUT, "ws read timed out", span, file, src, None::<&str>))?;
    }
    let mut payload = vec![0u8; len7 as usize];
    conn.read_exact(&mut payload).map_err(|_| zerr(codes::NET_TIMEOUT, "ws read timed out", span, file, src, None::<&str>))?;
    if masked {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask[i % 4];
        }
    }
    Ok((opcode, payload))
}
