// lsp.rs - Hone 语言服务器（LSP over stdio）
// 支持：全文同步（didOpen/didChange）、诊断（语法/类型错误，publishDiagnostics）、
//       上下文感知补全（关键字/内置函数/模块成员/文档变量/用户函数）、hover 说明、
//       跳转定义（textDocument/definition）、文档大纲（documentSymbol）、
//       语义高亮（textDocument/semanticTokens/full，复用词法 token 分类）。
// 协议：Content-Length 头 + JSON-RPC 2.0 body（serde_json 手工构造，无额外依赖）。

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, Write};

use serde_json::{json, Value};

use crate::error::ZError;
use crate::lexer::Tok;

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
            "textDocument/semanticTokens/full" => {
                let params = msg.get("params").cloned().unwrap_or(json!({}));
                let uri = params["textDocument"]["uri"].as_str().unwrap_or("").to_string();
                send(json!({"jsonrpc":"2.0","id":id,"result":semantic_tokens_result(&docs, &uri, &params)}));
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
            "documentSymbolProvider": true,
            "semanticTokensProvider": {
                "legend": {
                    "tokenTypes": [
                        "keyword", "type", "function", "variable", "string",
                        "number", "comment", "namespace", "class", "struct"
                    ],
                    "tokenModifiers": ["declaration"]
                },
                "full": true
            }
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
        ("http.sse_open", "http.sse_open(url, opts)", "打开 SSE 长连接，返回句柄"),
        ("http.sse_next", "http.sse_next(handle)", "读取下一个 SSE 事件 data（结束返回空串）"),
        ("http.sse_close", "http.sse_close(handle)", "关闭 SSE 连接"),
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

// ---------- 语义高亮（semantic tokens） ----------

/// semanticTokens legend 中 token 类型的下标（与 initialize_result 中声明顺序一致）。
const ST_KEYWORD: u64 = 0;
const ST_TYPE: u64 = 1;
const ST_FUNCTION: u64 = 2;
const ST_VARIABLE: u64 = 3;
const ST_STRING: u64 = 4;
const ST_NUMBER: u64 = 5;
const ST_COMMENT: u64 = 6;
const ST_NAMESPACE: u64 = 7;
const ST_CLASS: u64 = 8;
const ST_STRUCT: u64 = 9;

/// 语义高亮：复用词法分析器的精准 token 流（含位置），逐 token 分类为
/// 关键字/类型/函数/变量/字符串/数字/命名空间/类/结构体，输出 LSP delta 编码。
fn semantic_tokens_result(docs: &HashMap<String, String>, uri: &str, params: &Value) -> Value {
    let text = docs.get(uri).map(|s| s.as_str()).unwrap_or("");
    let _ = params;
    // (line, col, len, type_idx, modifier)
    let mut toks: Vec<(u64, u64, u64, u64, u64)> = Vec::new();

    // 注释：lexer 会跳过注释，这里单独扫描并追加
    scan_comments(text, &mut toks);

    // 用户符号表（函数/类/结构体名）用于标识符分类
    let mut user_fns: HashSet<String> = HashSet::new();
    let mut user_classes: HashSet<String> = HashSet::new();
    let mut user_structs: HashSet<String> = HashSet::new();
    for (kind, name, _, _) in scan_symbols(text) {
        match kind.as_str() {
            "fn" => { user_fns.insert(name); }
            "class" => { user_classes.insert(name); }
            "struct" => { user_structs.insert(name); }
            _ => {}
        }
    }
    let builtins = crate::checker::builtin_names();
    // 模块名（`mod.` 前缀）→ 命名空间
    let mut modules: HashSet<&str> = HashSet::new();
    for (full, _, _) in MODULE_DOCS {
        if let Some((m, _)) = full.split_once('.') {
            modules.insert(m);
        }
    }

    // 词法 token 流（失败时退化为仅注释高亮）
    if let Ok(stream) = crate::lexer::Lexer::new("", text).tokenize() {
        for (i, (tok, span)) in stream.iter().enumerate() {
            let line = span.line.saturating_sub(1) as u64;
            let col = span.col.saturating_sub(1) as u64;
            let len = span.len.max(1) as u64;
            let (ty, modif) = match tok {
                // 关键字
                Tok::Fn | Tok::If | Tok::Else | Tok::While | Tok::Do | Tok::For
                | Tok::In | Tok::Return | Tok::True | Tok::False | Tok::Go
                | Tok::Try | Tok::Catch | Tok::Throw | Tok::Continue | Tok::Match
                | Tok::Break | Tok::Breakpoint | Tok::Load | Tok::Lazy | Tok::Use
                | Tok::Import | Tok::Alias | Tok::As | Tok::From | Tok::Tmp
                | Tok::Struct | Tok::Class => (ST_KEYWORD, 0),
                // 类型关键字
                Tok::TInt | Tok::TFloat | Tok::TBool | Tok::TStr => (ST_TYPE, 0),
                // 数字
                Tok::IntLit(_) | Tok::FloatLit(_) => (ST_NUMBER, 0),
                // 字符串（含插值/多行）
                Tok::StrLit(_) | Tok::FStr(_) | Tok::MultiStr(_) => (ST_STRING, 0),
                // 标识符：结合上下文智能分类
                Tok::Ident(name) => {
                    let prev = stream.get(i.wrapping_sub(1)).map(|(t, _)| t);
                    let next = stream.get(i + 1).map(|(t, _)| t);
                    if matches!(prev, Some(Tok::Fn)) {
                        (ST_FUNCTION, 1) // 函数定义名（declaration）
                    } else if matches!(prev, Some(Tok::Class)) {
                        (ST_CLASS, 1)
                    } else if matches!(prev, Some(Tok::Struct)) {
                        (ST_STRUCT, 1)
                    } else if matches!(next, Some(Tok::LParen)) {
                        (ST_FUNCTION, 0) // 函数调用
                    } else if builtins.contains(name.as_str()) {
                        (ST_FUNCTION, 0) // 内置函数
                    } else if user_fns.contains(name) {
                        (ST_FUNCTION, 0)
                    } else if user_classes.contains(name) {
                        (ST_CLASS, 0)
                    } else if user_structs.contains(name) {
                        (ST_STRUCT, 0)
                    } else if modules.contains(name.as_str()) {
                        (ST_NAMESPACE, 0) // 模块名
                    } else {
                        (ST_VARIABLE, 0) // 变量
                    }
                }
                _ => continue, // 运算符/括号等不参与高亮
            };
            toks.push((line, col, len, ty, modif));
        }
    }

    // 按 (line, col) 排序后做 delta 编码
    toks.sort_by_key(|(l, c, _, _, _)| (*l, *c));
    let mut data: Vec<u64> = Vec::with_capacity(toks.len() * 5);
    let mut prev_line: u64 = 0;
    let mut prev_col: u64 = 0;
    for (line, col, len, ty, modif) in toks {
        if data.is_empty() {
            data.push(line);
            data.push(col);
        } else if line == prev_line {
            data.push(0);
            data.push(col.saturating_sub(prev_col));
        } else {
            data.push(line - prev_line);
            data.push(col);
        }
        data.push(len);
        data.push(ty);
        data.push(modif);
        prev_line = line;
        prev_col = col;
    }
    json!({ "data": data })
}

/// 扫描注释（`//` 行注释与 `/* */` 块注释），追加为 ST_COMMENT token。
/// lexer 在跳过空白时已吞掉注释，因此需要单独识别以参与高亮。
fn scan_comments(text: &str, out: &mut Vec<(u64, u64, u64, u64, u64)>) {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut i = 0usize;
    let mut line: u64 = 0;
    let mut col: u64 = 0;
    while i < n {
        let c = chars[i];
        if c == '\n' {
            line += 1;
            col = 0;
            i += 1;
            continue;
        }
        // 行注释
        if c == '/' && i + 1 < n && chars[i + 1] == '/' {
            let start_col = col;
            while i < n && chars[i] != '\n' {
                i += 1;
                col += 1;
            }
            out.push((line, start_col, col.saturating_sub(start_col), ST_COMMENT, 0));
            continue;
        }
        // 块注释（可能跨行：逐行输出）
        if c == '/' && i + 1 < n && chars[i + 1] == '*' {
            let start_line = line;
            let start_col = col;
            let mut cur_line = start_line;
            let mut cur_col = start_col;
            let mut cur_len: u64 = 0;
            let mut closed = false;
            i += 2;
            col += 2;
            cur_len += 2;
            while i < n {
                if chars[i] == '*' && i + 1 < n && chars[i + 1] == '/' {
                    cur_len += 2;
                    out.push((cur_line, cur_col, cur_len, ST_COMMENT, 0));
                    i += 2;
                    col += 2;
                    closed = true;
                    break;
                }
                if chars[i] == '\n' {
                    // 当前行结束，输出该行片段
                    out.push((cur_line, cur_col, cur_len, ST_COMMENT, 0));
                    line += 1;
                    col = 0;
                    i += 1;
                    cur_line = line;
                    cur_col = 0;
                    cur_len = 0;
                    continue;
                }
                cur_len += 1;
                i += 1;
                col += 1;
            }
            if !closed {
                // 未闭合：输出剩余部分
                out.push((cur_line, cur_col, cur_len, ST_COMMENT, 0));
            }
            continue;
        }
        // 字符串字面量：跳过，避免把字符串内的 `//` 误判为注释
        if c == '"' {
            if i + 2 < n && chars[i + 1] == '"' && chars[i + 2] == '"' {
                // 三引号原始字符串（可能跨行）
                i += 3;
                col += 3;
                while i + 2 < n && !(chars[i] == '"' && chars[i + 1] == '"' && chars[i + 2] == '"') {
                    if chars[i] == '\n' {
                        line += 1;
                        col = 0;
                    } else {
                        col += 1;
                    }
                    i += 1;
                }
                if i + 2 < n {
                    i += 3;
                    col += 3;
                }
            } else {
                // 普通字符串
                i += 1;
                col += 1;
                while i < n && chars[i] != '"' {
                    if chars[i] == '\\' && i + 1 < n {
                        i += 2;
                        col += 2;
                        continue;
                    }
                    if chars[i] == '\n' {
                        break;
                    }
                    i += 1;
                    col += 1;
                }
                if i < n && chars[i] == '"' {
                    i += 1;
                    col += 1;
                }
            }
            continue;
        }
        i += 1;
        col += 1;
    }
}
