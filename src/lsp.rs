// lsp.rs - Zap 语言服务器（LSP over stdio，最小实现）
// 支持：全文同步（didOpen/didChange）、诊断（语法/类型错误，publishDiagnostics）、
//       补全（内置函数 + 关键字）、hover 说明。
// 协议：Content-Length 头 + JSON-RPC 2.0 body（serde_json 手工构造，无额外依赖）。

use std::collections::HashMap;
use std::io::{self, BufRead, Write};

use serde_json::{json, Value};

use crate::error::ZError;

/// 启动 LSP 服务：从 stdin 读取请求，向 stdout 发送响应。阻塞直到客户端 exit。
pub fn run_lsp() -> Result<(), ZError> {
    let stdin = io::stdin();
    let mut handle = stdin.lock();
    let mut docs: HashMap<String, String> = HashMap::new();

    loop {
        let Some(msg) = read_message(&mut handle) else {
            break; // EOF：客户端断开
        };
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = msg.get("id").cloned();
        match method {
            "initialize" => {
                send(json!({"jsonrpc":"2.0","id":id,"result":initialize_result()}));
            }
            "initialized" | "setTrace" => {}
            "shutdown" => {
                send(json!({"jsonrpc":"2.0","id":id,"result":null}));
            }
            "exit" => break,
            "textDocument/didClose" => {
                if let Some(params) = msg.get("params") {
                    let uri = params["textDocument"]["uri"].as_str().unwrap_or("").to_string();
                    docs.remove(&uri);
                }
            }
            "textDocument/didOpen" => {
                if let Some(params) = msg.get("params") {
                    let uri = params["textDocument"]["uri"].as_str().unwrap_or("").to_string();
                    let text = params["textDocument"]["text"].as_str().unwrap_or("").to_string();
                    docs.insert(uri.clone(), text.clone());
                    publish_diagnostics(&uri, &text);
                }
            }
            "textDocument/didChange" => {
                if let Some(params) = msg.get("params") {
                    let uri = params["textDocument"]["uri"].as_str().unwrap_or("").to_string();
                    let changes = params["contentChanges"].as_array().cloned().unwrap_or_default();
                    let entry = docs.entry(uri.clone()).or_default();
                    for c in changes {
                        if let Some(t) = c.get("text").and_then(|t| t.as_str()) {
                            *entry = t.to_string();
                        }
                    }
                    publish_diagnostics(&uri, entry);
                }
            }
            "textDocument/completion" => {
                send(json!({"jsonrpc":"2.0","id":id,"result":completion_result()}));
            }
            "textDocument/hover" => {
                send(json!({"jsonrpc":"2.0","id":id,"result":hover_result()}));
            }
            _ => {
                // 未实现的请求：返回 null 结果，避免客户端等待超时
                if id.is_some() {
                    send(json!({"jsonrpc":"2.0","id":id,"result":null}));
                }
            }
        }
    }
    Ok(())
}

/// 读取一条 LSP 消息：Content-Length 头 + 空行 + JSON body。
fn read_message(handle: &mut impl BufRead) -> Option<Value> {
    let mut length: usize = 0;
    loop {
        let mut line = String::new();
        if handle.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            length = v.trim().parse().ok()?;
        }
    }
    if length == 0 {
        return None;
    }
    let mut buf = vec![0u8; length];
    handle.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

fn send(v: Value) {
    let s = v.to_string();
    let mut out = io::stdout().lock();
    let _ = write!(out, "Content-Length: {}\r\n\r\n{}", s.len(), s);
    let _ = out.flush();
}

fn initialize_result() -> Value {
    json!({
        "capabilities": {
            "textDocumentSync": { "openClose": true, "change": 1 },
            "completionProvider": { "triggerCharacters": ["."] },
            "hoverProvider": true
        },
        "serverInfo": { "name": "zap-lsp", "version": "0.1.0" }
    })
}

/// 对文档做解析与类型检查，向客户端推送诊断（只报第一个错误）。
fn publish_diagnostics(uri: &str, text: &str) {
    let diagnostics: Vec<Value> = if text.trim().is_empty() {
        vec![]
    } else {
        let path = uri.strip_prefix("file://").unwrap_or(uri);
        let err = match crate::parser::Parser::parse(path, text) {
            Ok(prog) => crate::checker::Checker::check(&prog, path, text).err(),
            Err(e) => Some(e),
        };
        err.map(|e| vec![diagnostic_from_error(&e)]).unwrap_or_default()
    };
    send(json!({
        "jsonrpc": "2.0",
        "method": "textDocument/publishDiagnostics",
        "params": { "uri": uri, "diagnostics": diagnostics }
    }));
}

/// ZError（1-based 行列）→ LSP Diagnostic（0-based range）。
fn diagnostic_from_error(e: &ZError) -> Value {
    let line = e.line.saturating_sub(1) as u64;
    let col = e.col.saturating_sub(1) as u64;
    json!({
        "range": {
            "start": { "line": line, "character": col },
            "end": { "line": line, "character": col + e.len.max(1) as u64 }
        },
        "severity": 1,
        "source": "zap",
        "code": e.code,
        "message": format!("{}: {}", e.code, e.msg)
    })
}

fn completion_result() -> Value {
    let mut items = Vec::new();
    for kw in [
        "fn", "if", "else", "while", "return", "true", "false", "go", "breakpoint",
        "int", "float", "bool", "str",
    ] {
        items.push(json!({ "label": kw, "kind": 14, "detail": "关键字" }));
    }
    for f in crate::checker::builtin_names() {
        items.push(json!({ "label": f, "kind": 3, "detail": "内置函数" }));
    }
    json!({ "isIncomplete": false, "items": items })
}

fn hover_result() -> Value {
    json!({
        "contents": {
            "kind": "markdown",
            "value": "**Zap 语言**\n\n轻量级、跨平台、可嵌入的脚本语言。\n\n输入内置函数名可自动补全（如 `print`、`time.now`）；类型一经推导即锁定，禁止隐式转换。"
        }
    })
}
