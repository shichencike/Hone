// lsp.rs - Hone 语言服务器（LSP over stdio）
// 支持：全文同步（didOpen/didChange）、诊断（语法/类型错误，publishDiagnostics）、
//       上下文感知补全（关键字/内置函数/模块成员/文档变量/用户函数）、hover 说明、
//       跳转定义（textDocument/definition）、文档大纲（documentSymbol）。
// 协议：Content-Length 头 + JSON-RPC 2.0 body（serde_json 手工构造，无额外依赖）。

use std::collections::{HashMap, HashSet};
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
                let params = msg.get("params").cloned().unwrap_or(json!({}));
                let uri = params["textDocument"]["uri"].as_str().unwrap_or("").to_string();
                send(json!({"jsonrpc":"2.0","id":id,"result":completion_result(&docs, &uri, &params)}));
            }
            "textDocument/hover" => {
                let params = msg.get("params").cloned().unwrap_or(json!({}));
                let uri = params["textDocument"]["uri"].as_str().unwrap_or("").to_string();
                send(json!({"jsonrpc":"2.0","id":id,"result":hover_result(&docs, &uri, &params)}));
            }
            "textDocument/definition" => {
                let params = msg.get("params").cloned().unwrap_or(json!({}));
                let uri = params["textDocument"]["uri"].as_str().unwrap_or("").to_string();
                send(json!({"jsonrpc":"2.0","id":id,"result":definition_result(&docs, &uri, &params)}));
            }
            "textDocument/documentSymbol" => {
                let params = msg.get("params").cloned().unwrap_or(json!({}));
                let uri = params["textDocument"]["uri"].as_str().unwrap_or("").to_string();
                send(json!({"jsonrpc":"2.0","id":id,"result":document_symbol_result(&docs, &uri, &params)}));
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
            "hoverProvider": true,
            "definitionProvider": true,
            "documentSymbolProvider": true
        },
        "serverInfo": { "name": "hone-lsp", "version": "0.7.0" }
    })
}

// ---------- 诊断 ----------

/// 对文档做解析与类型检查，向客户端推送诊断（报告第一个错误）。
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
        "source": "hone",
        "code": e.code,
        "message": format!("{}: {}", e.code, e.msg)
    })
}

// ---------- 文档扫描辅助 ----------

const KEYWORDS: &[&str] = &[
    "fn", "if", "else", "while", "do", "for", "in", "return", "true", "false", "go", "breakpoint",
    "break", "continue", "try", "catch", "throw", "match", "struct", "class",
    "int", "float", "bool", "str", "load", "lazy", "use", "import", "alias", "as", "from", "tmp",
    "null", "go",
];

/// 模块名 → 用于 `mod.` 前缀补全（点号后补成员）。
const MODULE_DOCS: &[(&str, &str, &str)] = &[
    ("time.now", "time.now()", "当前 Unix 时间戳（秒）"),
    ("time.sleep", "time.sleep(seconds)", "休眠（秒，支持小数）"),
    ("time.format", "time.format(ts, fmt)", "格式化时间戳（UTC）"),
    ("time.parse", "time.parse(str)", "解析时间戳 → Unix 秒"),
    ("time.add", "time.add(ts, seconds)", "时间戳加减秒"),
    ("time.diff", "time.diff(a, b)", "两个时间戳之差（秒）"),
    ("time.weekday", "time.weekday(ts)", "星期几（0=周日）"),
    ("random.int", "random.int(min, max)", "随机整数 [min, max]"),
    ("random.float", "random.float()", "随机浮点数 [0, 1)"),
    ("uuid.new", "uuid.new()", "生成 UUID v4"),
    ("sys.run", "sys.run(cmd)", "执行系统命令并返回输出"),
    ("sys.get_env", "sys.get_env(name)", "读取环境变量"),
    ("sys.msgbox", "sys.msgbox(title, text, type)", "消息框（Windows）"),
    ("sys.beep", "sys.beep(freq, dur)", "蜂鸣（Windows）"),
    ("sys.clipboard_set", "sys.clipboard_set(text)", "写入剪贴板（Windows）"),
    ("sys.get_screen_size", "sys.get_screen_size()", "屏幕尺寸「宽x高」（Windows）"),
    ("sys.reg_read", "sys.reg_read(key)", "读注册表（Windows）"),
    ("sys.reg_write", "sys.reg_write(key, val)", "写注册表（Windows）"),
    ("log.info", "log.info(msg)", "彩色 info 日志（stderr）"),
    ("log.warn", "log.warn(msg)", "彩色 warn 日志（stderr）"),
    ("log.error", "log.error(msg)", "彩色 error 日志（stderr）"),
    ("log.debug", "log.debug(msg)", "彩色 debug 日志（stderr）"),
    ("path.join", "path.join(a, b, ...)", "拼接路径"),
    ("path.dirname", "path.dirname(p)", "路径的目录部分"),
    ("path.basename", "path.basename(p)", "路径的文件名部分"),
    ("args.has", "args.has(key)", "命令行是否含该参数"),
    ("args.get", "args.get(key, default?)", "读取命令行参数值"),
    ("env.get", "env.get(name)", "读取环境变量"),
    ("env.set", "env.set(key, val)", "写入环境变量"),
    ("server.listen", "server.listen(port)", "启动本地监听线程（0=自动分配）"),
    ("server.poll", "server.poll()", "取出排队请求（JSON 数组）"),
    ("server.respond", "server.respond(id, body)", "发送 HTTP 200 响应"),
    ("json.parse", "json.parse(s)", "解析 JSON 字符串"),
    ("json.stringify", "json.stringify(x)", "序列化为 JSON 字符串"),
];

/// 内置函数 → (签名, 说明)。
fn builtin_doc(name: &str) -> Option<(&'static str, &'static str)> {
    const M: &[(&str, &str, &str)] = &[
        ("print", "print(x, ...)", "打印一个或多个值到标准输出"),
        ("len", "len(x)", "返回列表/字典/字符串的元素个数"),
        ("type_of", "type_of(x)", "返回值的类型名称"),
        ("read_file", "read_file(path)", "读取文本文件内容"),
        ("write_file", "write_file(path, content)", "写入文本文件"),
        ("file_exists", "file_exists(path)", "判断文件是否存在"),
        ("input", "input(prompt?)", "读取一行标准输入（EOF 报 H306）"),
        ("read_int", "read_int(prompt?)", "读取并解析为 int（格式错报 H006）"),
        ("read_float", "read_float(prompt?)", "读取并解析为 float（格式错报 H007）"),
        ("append", "append(list, x)", "向列表追加元素"),
        ("contains", "contains(coll, x)", "判断集合是否包含元素"),
        ("index_of", "index_of(list, x)", "返回元素下标（找不到返回 -1）"),
        ("keys", "keys(dict)", "返回字典键列表"),
        ("values", "values(dict)", "返回字典值列表"),
        ("has_key", "has_key(dict, k)", "判断字典是否包含键"),
        ("to_str", "to_str(x)", "转字符串"),
        ("to_int", "to_int(x)", "转 int"),
        ("to_float", "to_float(x)", "转 float"),
        ("is_int", "is_int(x)", "判断是否为 int"),
        ("is_float", "is_float(x)", "判断是否为 float"),
        ("is_str", "is_str(x)", "判断是否为 str"),
        ("is_bool", "is_bool(x)", "判断是否为 bool"),
        ("is_list", "is_list(x)", "判断是否为 list"),
        ("is_dict", "is_dict(x)", "判断是否为 dict"),
        ("is_null", "is_null(x)", "判断是否为 null"),
        ("str_contains", "str_contains(s, sub)", "判断字符串是否包含子串"),
        ("str_replace", "str_replace(s, from, to)", "字符串替换"),
        ("str_trim", "str_trim(s)", "去除首尾空白"),
        ("abs", "abs(x)", "绝对值"),
        ("max", "max(a, b)", "最大值"),
        ("min", "min(a, b)", "最小值"),
        ("http_get", "http_get(url)", "HTTP GET 请求（支持 http/https）"),
        ("http_post", "http_post(url, body)", "HTTP POST 请求"),
        ("json_parse", "json_parse(s)", "解析 JSON"),
        ("json_stringify", "json_stringify(x)", "序列化 JSON（标量）"),
    ];
    M.iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, sig, doc)| (*sig, *doc))
}

/// 取光标所在（或紧邻之前）的单词：字母/数字/下划线/点号，用于补全前缀与 hover 命中。
fn word_at(text: &str, line: u64, character: u64) -> Option<String> {
    let l = text.lines().nth(line as usize)?;
    let chars: Vec<char> = l.chars().collect();
    if chars.is_empty() {
        return None;
    }
    let mut idx = (character as usize).min(chars.len());
    if idx == 0 {
        return None;
    }
    if idx == chars.len() {
        idx -= 1; // 光标在行尾：从最后一个字符开始向前
    }
    let c = chars[idx];
    if !(c.is_alphanumeric() || c == '_' || c == '.') {
        return None;
    }
    let mut start = idx;
    while start > 0
        && (chars[start - 1].is_alphanumeric() || chars[start - 1] == '_' || chars[start - 1] == '.')
    {
        start -= 1;
    }
    let mut end = idx + 1;
    while end < chars.len() && (chars[end].is_alphanumeric() || chars[end] == '_' || chars[end] == '.')
    {
        end += 1;
    }
    Some(chars[start..end].iter().collect())
}

/// 扫描文档中的用户符号：返回 (种类, 名称, 行号 0-based, 列号 0-based)。
fn scan_symbols(text: &str) -> Vec<(String, String, u64, u64)> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with("//") {
            continue;
        }
        let leading = (line.len() - trimmed.len()) as u64;
        let kind = if trimmed.starts_with("tmp fn ") || trimmed.starts_with("fn ") {
            "fn"
        } else if trimmed.starts_with("class ") {
            "class"
        } else if trimmed.starts_with("struct ") {
            "struct"
        } else {
            continue;
        };
        // 取名称：fn 名可能带泛型 [T] 与类型注解
        let rest = trimmed
            .strip_prefix("tmp fn ")
            .or_else(|| trimmed.strip_prefix("fn "))
            .or_else(|| trimmed.strip_prefix("class "))
            .or_else(|| trimmed.strip_prefix("struct "))
            .unwrap_or("");
        let name: String = rest
            .trim_start()
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() {
            out.push((kind.to_string(), name, i as u64, leading));
        }
    }
    out
}

/// 扫描文档中的变量赋值（`name = ...` 启发式，跳过 == 与字符串内容）。
fn scan_vars(text: &str) -> Vec<(String, u64, u64)> {
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (i, line) in text.lines().enumerate() {
        let code = line.split("//").next().unwrap_or("");
        let chars: Vec<char> = code.chars().collect();
        let mut j = 0;
        while j < chars.len() {
            let c = chars[j];
            if c == '"' {
                j += 1;
                while j < chars.len() && chars[j] != '"' {
                    if chars[j] == '\\' {
                        j += 1;
                    }
                    j += 1;
                }
                j += 1;
                continue;
            }
            if c.is_ascii_alphabetic() || c == '_' {
                let start = j;
                while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                let name: String = chars[start..j].iter().collect();
                let mut k = j;
                while k < chars.len() && chars[k].is_whitespace() {
                    k += 1;
                }
                if k < chars.len() && chars[k] == '=' && (k + 1 >= chars.len() || chars[k + 1] != '=') {
                    if seen.insert(name.clone()) {
                        out.push((name, i as u64, start as u64));
                    }
                }
                continue;
            }
            j += 1;
        }
    }
    out
}

// ---------- LSP 特性实现 ----------

fn completion_result(docs: &HashMap<String, String>, uri: &str, params: &Value) -> Value {
    let pos = &params["position"];
    let line = pos["line"].as_u64().unwrap_or(0);
    let character = pos["character"].as_u64().unwrap_or(0);
    let text = docs.get(uri).map(|s| s.as_str()).unwrap_or("");
    let word = word_at(text, line, character).unwrap_or_default();
    let mut items: Vec<Value> = Vec::new();

    // 模块成员补全：光标位于 `mod.` 或 `mod.mem` 之后
    if let Some((prefix, _member)) = word.split_once('.') {
        let p = format!("{}.", prefix);
        for (full, sig, doc) in MODULE_DOCS.iter().filter(|(n, _, _)| n.starts_with(&p)) {
            let label = full.split_once('.').map(|(_, m)| m).unwrap_or(full);
            items.push(json!({
                "label": label, "kind": 3, "detail": *sig, "documentation": *doc
            }));
        }
        if !items.is_empty() {
            return json!({ "isIncomplete": false, "items": items });
        }
        // 不是已知模块前缀，退回普通补全
    }

    for kw in KEYWORDS {
        items.push(json!({ "label": kw, "kind": 14, "detail": "关键字" }));
    }
    for f in crate::checker::builtin_names() {
        match builtin_doc(f) {
            Some((sig, doc)) => {
                items.push(json!({ "label": f, "kind": 3, "detail": sig, "documentation": doc }));
            }
            None => items.push(json!({ "label": f, "kind": 3, "detail": "内置函数" })),
        }
    }
    // 模块名（带点号提示成员补全）
    let mut mods: HashSet<&str> = HashSet::new();
    for (full, _, _) in MODULE_DOCS {
        if let Some((m, _)) = full.split_once('.') {
            mods.insert(m);
        }
    }
    for m in mods {
        items.push(json!({ "label": format!("{}.", m), "kind": 9, "detail": "模块" }));
    }
    // 文档变量
    for (name, l, _c) in scan_vars(text) {
        items.push(json!({ "label": name, "kind": 6, "detail": format!("变量（L{}）", l + 1) }));
    }
    // 用户函数 / 类
    for (kind, name, l, _c) in scan_symbols(text) {
        let kind_id = if kind == "class" { 7 } else { 3 };
        items.push(json!({
            "label": name, "kind": kind_id,
            "detail": format!("{}（L{}）", if kind == "class" { "类" } else { "函数" }, l + 1)
        }));
    }
    json!({ "isIncomplete": false, "items": items })
}

fn hover_result(docs: &HashMap<String, String>, uri: &str, params: &Value) -> Value {
    let pos = &params["position"];
    let line = pos["line"].as_u64().unwrap_or(0);
    let character = pos["character"].as_u64().unwrap_or(0);
    let text = docs.get(uri).map(|s| s.as_str()).unwrap_or("");
    let Some(word) = word_at(text, line, character) else {
        return json!(null);
    };

    // 模块成员 / 内置函数
    let value: String = if let Some((_, sig, doc)) = MODULE_DOCS.iter().find(|(n, _, _)| *n == word) {
        format!("**`{}`**\n\n{}", sig, doc)
    } else if let Some((sig, doc)) = builtin_doc(&word) {
        format!("**`{}`**\n\n{}", sig, doc)
    } else if let Some((kind, name, l, _)) = scan_symbols(text).iter().find(|(_, n, _, _)| *n == word) {
        let kind_cn = match kind.as_str() {
            "class" => "类",
            "struct" => "结构体",
            _ => "函数",
        };
        format!("**{} `{}`**\n\n定义于第 {} 行", kind_cn, name, l + 1)
    } else if let Some((name, l, _)) = scan_vars(text).iter().find(|(n, _, _)| *n == word) {
        format!("**变量 `{}`**\n\n定义于第 {} 行", name, l + 1)
    } else {
        return json!(null);
    };

    json!({
        "contents": { "kind": "markdown", "value": value }
    })
}

fn definition_result(docs: &HashMap<String, String>, uri: &str, params: &Value) -> Value {
    let pos = &params["position"];
    let line = pos["line"].as_u64().unwrap_or(0);
    let character = pos["character"].as_u64().unwrap_or(0);
    let text = docs.get(uri).map(|s| s.as_str()).unwrap_or("");
    let Some(word) = word_at(text, line, character) else {
        return json!(null);
    };
    let syms = scan_symbols(text);
    let Some((_, _name, l, c)) = syms.iter().find(|(_, n, _, _)| *n == word) else {
        return json!(null);
    };
    json!([{
        "uri": uri,
        "range": {
            "start": { "line": *l, "character": *c },
            "end": { "line": *l, "character": *c + 2 }
        }
    }])
}

fn document_symbol_result(docs: &HashMap<String, String>, uri: &str, params: &Value) -> Value {
    let text = docs.get(uri).map(|s| s.as_str()).unwrap_or("");
    let _ = params;
    let syms: Vec<Value> = scan_symbols(text)
        .iter()
        .map(|(kind, name, l, c)| {
            let kind_id = match kind.as_str() {
                "class" => 5,
                "struct" => 23,
                _ => 12,
            };
            json!({
                "name": name,
                "kind": kind_id,
                "range": {
                    "start": { "line": *l, "character": *c },
                    "end": { "line": *l, "character": *c + 2 }
                },
                "selectionRange": {
                    "start": { "line": *l, "character": *c },
                    "end": { "line": *l, "character": *c + name.len() as u64 }
                }
            })
        })
        .collect();
    json!(syms)
}
