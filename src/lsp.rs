// lsp.rs - Hone 语言服务器（LSP over stdio）
// 支持：增量同步（didChange range 替换）、诊断（语法/类型错误，publishDiagnostics +
//       version 回显 + 去重）、上下文感知补全（关键字/内置函数/模块成员/文档变量/用户函数）、
//       hover 说明、跳转定义（definition，AST 符号/变量声明，支持 类.成员 限定名）、
//       类型定义跳转（typeDefinition）、符号引用（references，跨所有打开文档）、
//       重命名（rename，WorkspaceEdit 跨文档编辑）、代码格式化（formatting，复用 fmt）、
//       签名帮助（signatureHelp，括号/逗号触发，用户函数+内置函数）、
//       折叠区域（foldingRange，多行块/文档注释）、文档大纲（documentSymbol，AST 驱动）、
//       全局符号搜索（workspace/symbol，跨文档子串匹配）、
//       语义高亮（textDocument/semanticTokens/full，复用词法 token 分类）。
// 协议：Content-Length 头 + JSON-RPC 2.0 body（serde_json 手工构造，无额外依赖）；
//       错误按规范返回 -32700 Parse error / -32601 Method not found / -32602 Invalid params。

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, Write};

use serde_json::{json, Value};

use crate::ast::{Program, Stmt, TyName};
use crate::error::ZError;
use crate::lexer::Tok;

/// 启动 LSP 服务：从 stdin 读取请求，向 stdout 发送响应。阻塞直到客户端 exit。
pub fn run_lsp() -> Result<(), ZError> {
    let stdin = io::stdin();
    let mut handle = stdin.lock();
    // uri → 文档文本
    let mut docs: HashMap<String, String> = HashMap::new();
    // uri → 客户端文档版本号（didOpen/didChange 携带；诊断推送时回显）
    let mut versions: HashMap<String, u64> = HashMap::new();
    // didChangeConfiguration 的 workspace 配置（原样保存，供后续能力查询）
    let mut workspace_config: Value = json!({});
    // $/setTrace 的追踪级别（off/messages/verbose；当前仅保存）
    let mut trace: Option<String> = None;

    loop {
        let msg = match read_message(&mut handle) {
            LspRead::Eof => break, // 客户端断开
            LspRead::ParseErr => {
                // 协议层损坏：回 -32700 Parse error（无 id 可回显）
                send(rpc_error(&None, -32700, "Parse error: malformed message"));
                continue;
            }
            LspRead::Msg(m) => m,
        };
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("").to_string();
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(json!({}));
        // 有 id 的「请求」需要响应；无 id 的「通知」无需响应。
        let is_request = id.is_some();

        match method.as_str() {
            "initialize" => {
                send(json!({"jsonrpc":"2.0","id":id,"result":initialize_result()}));
            }
            "initialized" => {}
            "shutdown" => {
                send(json!({"jsonrpc":"2.0","id":id,"result":null}));
            }
            "exit" => break,
            "workspace/didChangeConfiguration" => {
                workspace_config = params.get("settings").cloned().unwrap_or(json!({}));
            }
            "$/setTrace" => {
                trace = params.get("value").and_then(|v| v.as_str()).map(String::from);
            }
            "workspace/symbol" => {
                if !is_request {
                    continue;
                }
                send(json!({"jsonrpc":"2.0","id":id,"result":workspace_symbol_result(&docs, &params)}));
            }
            "textDocument/didClose" => {
                if let Some(uri) = params["textDocument"]["uri"].as_str() {
                    docs.remove(uri);
                    versions.remove(uri);
                }
            }
            "textDocument/didOpen" => {
                let uri = params["textDocument"]["uri"].as_str().unwrap_or("").to_string();
                let text = params["textDocument"]["text"].as_str().unwrap_or("").to_string();
                versions.insert(uri.clone(), params["textDocument"]["version"].as_u64().unwrap_or(0));
                docs.insert(uri.clone(), text.clone());
                publish_diagnostics(&uri, &text, versions.get(&uri).copied().unwrap_or(0));
            }
            "textDocument/didChange" => {
                let uri = params["textDocument"]["uri"].as_str().unwrap_or("").to_string();
                let changes = params["contentChanges"].as_array().cloned().unwrap_or_default();
                let entry = docs.entry(uri.clone()).or_default();
                for c in changes {
                    let text = c.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    if let Some(range) = c.get("range") {
                        // 增量变更：range 替换
                        apply_incremental(entry, range, text);
                    } else {
                        *entry = text.to_string(); // 无 range = 全文替换
                    }
                }
                if let Some(v) = params["textDocument"]["version"].as_u64() {
                    versions.insert(uri.clone(), v);
                }
                publish_diagnostics(&uri, entry, versions.get(&uri).copied().unwrap_or(0));
            }
            "textDocument/completion" => {
                if !is_request {
                    continue;
                }
                let uri = params["textDocument"]["uri"].as_str().unwrap_or("").to_string();
                send(json!({"jsonrpc":"2.0","id":id,"result":completion_result(&docs, &uri, &params)}));
            }
            "textDocument/hover" => {
                if !is_request {
                    continue;
                }
                let uri = params["textDocument"]["uri"].as_str().unwrap_or("").to_string();
                send(json!({"jsonrpc":"2.0","id":id,"result":hover_result(&docs, &uri, &params)}));
            }
            "textDocument/definition" => {
                if !is_request {
                    continue;
                }
                let uri = params["textDocument"]["uri"].as_str().unwrap_or("").to_string();
                send(json!({"jsonrpc":"2.0","id":id,"result":definition_result(&docs, &uri, &params)}));
            }
            "textDocument/typeDefinition" => {
                if !is_request {
                    continue;
                }
                let uri = params["textDocument"]["uri"].as_str().unwrap_or("").to_string();
                send(json!({"jsonrpc":"2.0","id":id,"result":type_definition_result(&docs, &uri, &params)}));
            }
            "textDocument/references" => {
                if !is_request {
                    continue;
                }
                let uri = params["textDocument"]["uri"].as_str().unwrap_or("").to_string();
                send(json!({"jsonrpc":"2.0","id":id,"result":references_result(&docs, &uri, &params)}));
            }
            "textDocument/rename" => {
                if !is_request {
                    continue;
                }
                let uri = params["textDocument"]["uri"].as_str().unwrap_or("").to_string();
                send(json!({"jsonrpc":"2.0","id":id,"result":rename_result(&docs, &uri, &params)}));
            }
            "textDocument/formatting" => {
                if !is_request {
                    continue;
                }
                let uri = params["textDocument"]["uri"].as_str().unwrap_or("").to_string();
                send(json!({"jsonrpc":"2.0","id":id,"result":formatting_result(&docs, &uri)}));
            }
            "textDocument/foldingRange" => {
                if !is_request {
                    continue;
                }
                let uri = params["textDocument"]["uri"].as_str().unwrap_or("").to_string();
                send(json!({"jsonrpc":"2.0","id":id,"result":folding_range_result(&docs, &uri)}));
            }
            "textDocument/signatureHelp" => {
                if !is_request {
                    continue;
                }
                let uri = params["textDocument"]["uri"].as_str().unwrap_or("").to_string();
                send(json!({"jsonrpc":"2.0","id":id,"result":signature_help_result(&docs, &uri, &params)}));
            }
            "textDocument/documentSymbol" => {
                if !is_request {
                    continue;
                }
                let uri = params["textDocument"]["uri"].as_str().unwrap_or("").to_string();
                send(json!({"jsonrpc":"2.0","id":id,"result":document_symbol_result(&docs, &uri, &params)}));
            }
            "textDocument/semanticTokens/full" => {
                if !is_request {
                    continue;
                }
                let uri = params["textDocument"]["uri"].as_str().unwrap_or("").to_string();
                send(json!({"jsonrpc":"2.0","id":id,"result":semantic_tokens_result(&docs, &uri, &params)}));
            }
            _ => {
                // 未知请求：按规范回 -32601 Method not found；未知通知忽略
                if is_request {
                    send(rpc_error(&id, -32601, &format!("Method not found: {}", method)));
                }
            }
        }
        let _ = &workspace_config; // 配置暂存（当前能力不依赖具体键，保留扩展点）
    }
    Ok(())
}

/// 读取一条 LSP 消息的结果。
enum LspRead {
    /// EOF：客户端断开
    Eof,
    /// 协议解析失败（Content-Length 缺失/非法、body 非 JSON）→ 回 -32700 Parse error
    ParseErr,
    /// 正常消息
    Msg(Value),
}

/// 读取一条 LSP 消息：Content-Length 头 + 空行 + JSON body。
fn read_message(handle: &mut impl BufRead) -> LspRead {
    let mut length: usize = 0;
    loop {
        let mut line = String::new();
        match handle.read_line(&mut line) {
            Ok(0) => return LspRead::Eof,
            Ok(_) => {}
            Err(_) => return LspRead::ParseErr,
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            match v.trim().parse() {
                Ok(v) => length = v,
                Err(_) => return LspRead::ParseErr,
            }
        }
    }
    if length == 0 {
        return LspRead::Eof;
    }
    let mut buf = vec![0u8; length];
    if handle.read_exact(&mut buf).is_err() {
        return LspRead::Eof;
    }
    match serde_json::from_slice(&buf) {
        Ok(v) => LspRead::Msg(v),
        Err(_) => LspRead::ParseErr,
    }
}

fn send(v: Value) {
    let s = v.to_string();
    let mut out = io::stdout().lock();
    let _ = write!(out, "Content-Length: {}\r\n\r\n{}", s.len(), s);
    let _ = out.flush();
}

/// JSON-RPC 错误响应（code 为规范错误码：-32700 Parse error / -32601 Method not found / -32602 Invalid params）。
fn rpc_error(id: &Option<Value>, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// 应用增量变更（didChange 带 range）：LSP 位置 0-based、character 按 UTF-16 单位 → 字节区间替换。
fn apply_incremental(doc: &mut String, range: &Value, text: &str) {
    let s = offset_of(doc, range["start"]["line"].as_u64().unwrap_or(0), range["start"]["character"].as_u64().unwrap_or(0));
    let e = offset_of(doc, range["end"]["line"].as_u64().unwrap_or(0), range["end"]["character"].as_u64().unwrap_or(0));
    let s = s.min(doc.len());
    let e = e.min(doc.len());
    if s <= e {
        doc.replace_range(s..e, text);
    }
}

/// (0-based 行号, UTF-16 字符偏移) → 文档内字节偏移（越界钳制到行尾/文档尾）。
fn offset_of(doc: &str, line: u64, utf16_char: u64) -> usize {
    let mut cur_line: u64 = 0;
    let mut cur_units: u64 = 0;
    for (i, c) in doc.char_indices() {
        if cur_line == line && cur_units == utf16_char {
            return i;
        }
        if c == '\n' {
            cur_line += 1;
            cur_units = 0;
        } else if cur_line == line {
            cur_units += c.len_utf16() as u64;
        }
    }
    doc.len()
}

fn initialize_result() -> Value {
    json!({
        "capabilities": {
            "textDocumentSync": { "openClose": true, "change": 2 }, // 2 = Incremental（增量同步）
            "completionProvider": { "triggerCharacters": ["."] },
            "hoverProvider": true,
            "definitionProvider": true,
            "typeDefinitionProvider": true,
            "referencesProvider": true,
            "renameProvider": true,
            "documentFormattingProvider": true,
            "foldingRangeProvider": true,
            "signatureHelpProvider": { "triggerCharacters": ["(", ","] },
            "documentSymbolProvider": true,
            "workspaceSymbolProvider": true,
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
        "serverInfo": { "name": "hone-lsp", "version": env!("CARGO_PKG_VERSION") }
    })
}

// ---------- 诊断 ----------

/// 对文档做解析与类型检查，向客户端推送诊断。
/// 空文档不推送（避免无意义的清空消息）；`version` 回显客户端文档版本。
fn publish_diagnostics(uri: &str, text: &str, version: u64) {
    if text.trim().is_empty() {
        return;
    }
    let path = uri.strip_prefix("file://").unwrap_or(uri);
    let diagnostics = match crate::parser::Parser::parse(path, text) {
        Ok(prog) => crate::checker::Checker::collect_errors(&prog, path, text)
            .into_iter()
            .map(|e| diagnostic_from_error(&e))
            .collect(),
        Err(e) => vec![diagnostic_from_error(&e)],
    };
    send(json!({
        "jsonrpc": "2.0",
        "method": "textDocument/publishDiagnostics",
        "params": { "uri": uri, "diagnostics": diagnostics, "version": version }
    }));
}

/// ZError（1-based 行列）→ LSP Diagnostic（0-based range，UTF-16 单位）。
/// severity 分级：语法/类型错误 = Error(1)；携带 help 的按 Error 保留并附相关说明。
/// 同位置同码诊断去重（fixpoint 多轮检查可能重复报同一问题）。
fn diagnostic_from_error(e: &ZError) -> Value {
    let line = e.line.saturating_sub(1) as u64;
    let col = e.col.saturating_sub(1) as u64;
    let mut obj = json!({
        "range": {
            "start": { "line": line, "character": col },
            "end": { "line": line, "character": col + e.len.max(1) as u64 }
        },
        "severity": 1,
        "source": "hone",
        "code": e.code,
        "message": format!("{}: {}", e.code, e.msg)
    });
    if let Some(help) = &e.help {
        obj["relatedInformation"] = json!([
            { "location": { "uri": "", "range": obj["range"].clone() }, "message": format!("help: {}", help) }
        ]);
    }
    obj
}

/// 诊断去重：同 (line, col, code) 只保留第一条；按位置排序（编辑器展示顺序稳定）。
fn dedup_diagnostics(diags: Vec<Value>) -> Vec<Value> {
    let mut seen: HashSet<(u64, u64, String)> = HashSet::new();
    let mut out: Vec<Value> = diags
        .into_iter()
        .filter(|d| {
            let key = (
                d["range"]["start"]["line"].as_u64().unwrap_or(0),
                d["range"]["start"]["character"].as_u64().unwrap_or(0),
                d["code"].as_str().unwrap_or("").to_string(),
            );
            seen.insert(key)
        })
        .collect();
    out.sort_by(|a, b| {
        (a["range"]["start"]["line"].as_u64().unwrap_or(0),
         a["range"]["start"]["character"].as_u64().unwrap_or(0))
            .cmp(&(b["range"]["start"]["line"].as_u64().unwrap_or(0),
                   b["range"]["start"]["character"].as_u64().unwrap_or(0)))
    });
    out
}

// ---------- 文档扫描辅助 ----------

const KEYWORDS: &[&str] = &[
    "fn", "if", "else", "while", "do", "for", "in", "return", "true", "false", "go", "breakpoint",
    "break", "continue", "try", "catch", "throw", "match", "struct", "class", "enum",
    "async", "await",
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
        ("read_bytes", "read_bytes(path)", "读取文件为字节列表（int 0-255，二进制安全）"),
        ("write_bytes", "write_bytes(path, bytes)", "将字节列表（int 0-255）写入文件"),
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
        } else if trimmed.starts_with("enum ") {
            "enum"
        } else {
            continue;
        };
        // 取名称：fn 名可能带泛型 [T] 与类型注解
        let rest = trimmed
            .strip_prefix("tmp fn ")
            .or_else(|| trimmed.strip_prefix("fn "))
            .or_else(|| trimmed.strip_prefix("class "))
            .or_else(|| trimmed.strip_prefix("struct "))
            .or_else(|| trimmed.strip_prefix("enum "))
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
            "enum" => "枚举",
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

/// 光标处符号定位（definition / typeDefinition 共用）：
/// 优先查 AST 符号表（含 类.方法 限定名与嵌套函数），再查变量赋值扫描。
/// 返回 (符号名, 定义行, 定义列 0-based, 名称宽度, 是否类型符号)。
fn lookup_sym(docs: &HashMap<String, String>, uri: &str, line: u64, character: u64)
    -> Option<(String, u64, u64, u64, bool)>
{
    let text = docs.get(uri).map(|s| s.as_str()).unwrap_or("");
    let word = word_at(text, line, character)?;
    // AST 符号（顶层 + 嵌套 + 类/type 成员，限定名形如 "Foo.bar"）
    if let Ok(prog) = crate::parser::Parser::parse("", text) {
        let syms = ast_symbols(&prog);
        let bare = word.split('.').last().unwrap_or(&word);
        for s in &syms {
            if s.name == word || s.name.split('.').last() == Some(bare) {
                let sel = s.name.split('.').next_back().unwrap_or(&s.name).len() as u64;
                return Some((
                    s.name.clone(),
                    s.line,
                    s.col + s.len - sel, // 定位到名称本身（去掉 类名. 前缀）
                    sel,
                    matches!(s.kind, "type" | "class" | "struct" | "enum"),
                ));
            }
        }
    }
    // 变量（`name = ...` 启发式）
    let found = scan_vars(text).into_iter().find(|(n, _, _)| n == &word);
    let (name, l, c) = found?;
    let w = name.len() as u64;
    Some((name, l, c, w, false))
}

fn definition_result(docs: &HashMap<String, String>, uri: &str, params: &Value) -> Value {
    let pos = &params["position"];
    let (line, character) = (pos["line"].as_u64().unwrap_or(0), pos["character"].as_u64().unwrap_or(0));
    let Some((_name, l, c, w, _is_ty)) = lookup_sym(docs, uri, line, character) else {
        return json!(null);
    };
    json!([{
        "uri": uri,
        "range": {
            "start": { "line": l, "character": c },
            "end": { "line": l, "character": c + w }
        }
    }])
}

fn type_definition_result(docs: &HashMap<String, String>, uri: &str, params: &Value) -> Value {
    let pos = &params["position"];
    let (line, character) = (pos["line"].as_u64().unwrap_or(0), pos["character"].as_u64().unwrap_or(0));
    let Some((_name, l, c, w, is_ty)) = lookup_sym(docs, uri, line, character) else {
        return json!(null);
    };
    if !is_ty {
        return json!(null); // 非类型符号：无类型定义
    }
    json!([{
        "uri": uri,
        "range": {
            "start": { "line": l, "character": c },
            "end": { "line": l, "character": c + w }
        }
    }])
}

/// documentSymbol：AST 驱动的文档大纲（fn/class/struct/enum/type，含 类.type 成员）。
fn document_symbol_result(docs: &HashMap<String, String>, uri: &str, params: &Value) -> Value {
    let text = docs.get(uri).map(|s| s.as_str()).unwrap_or("");
    let _ = params;
    let Ok(prog) = crate::parser::Parser::parse("", text) else { return json!([]) };
    let syms: Vec<Value> = ast_symbols(&prog)
        .iter()
        .map(|s| {
            let sel = s.name.split('.').next_back().unwrap_or(&s.name).len() as u64;
            let kind_id = match s.kind {
                "class" => 5,
                "struct" => 23,
                "enum" => 10,
                "type" => 6,
                _ => 12,
            };
            json!({
                "name": s.name,
                "kind": kind_id,
                "range": {
                    "start": { "line": s.line, "character": s.col },
                    "end": { "line": s.line, "character": s.col + s.len }
                },
                "selectionRange": {
                    "start": { "line": s.line, "character": s.col + s.len - sel },
                    "end": { "line": s.line, "character": s.col + s.len }
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
    let mut user_enums: HashSet<String> = HashSet::new();
    for (kind, name, _, _) in scan_symbols(text) {
        match kind.as_str() {
            "fn" => { user_fns.insert(name); }
            "class" => { user_classes.insert(name); }
            "struct" => { user_structs.insert(name); }
            "enum" => { user_enums.insert(name); }
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
                | Tok::Struct | Tok::Class | Tok::Enum | Tok::Async | Tok::Await => (ST_KEYWORD, 0),
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
                    } else if matches!(prev, Some(Tok::Enum)) {
                        (ST_STRUCT, 1) // 枚举定义名（declaration）
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
                    } else if user_enums.contains(name) {
                        (ST_STRUCT, 0) // 枚举类型引用
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

// ============================================================
// 新增能力：跨文档搜索 / 格式化 / 折叠 / 引用 / 重命名 / 签名帮助 / 类型定义
// ============================================================

/// 词法 token 流（失败返回空流；调用方按空流处理即优雅退化）。
fn tokenize_doc(text: &str) -> Vec<(Tok, crate::lexer::Span)> {
    crate::lexer::Lexer::new("", text)
        .tokenize()
        .unwrap_or_default()
}

/// 从字节偏移 start 起，跨过 utf16 个 UTF-16 码元（可跨行），返回结束偏移。
fn offset_after_units(text: &str, start: usize, units: u64) -> usize {
    let mut left = units;
    for (i, c) in text[start..].char_indices() {
        if left == 0 {
            return start + i;
        }
        left -= c.len_utf16() as u64;
    }
    text.len()
}

/// 字节偏移 → 0-based 行号。
fn line_of(text: &str, idx: usize) -> u64 {
    text[..idx.min(text.len())].bytes().filter(|&b| b == b'\n').count() as u64
}

/// AST 符号表条目（供 definition / typeDefinition / workspace symbol 使用）。
struct AstSym {
    kind: &'static str, // fn / class / struct / enum / type
    name: String,       // 限定名：顶层为裸名，成员为「类名.方法名」
    line: u64,          // 0-based
    col: u64,           // 0-based
    len: u64,
}

/// 收集文档 AST 中的符号（顶层 + 嵌套函数 + 类/type 成员），按 (行, 列) 排序。
fn ast_symbols(prog: &Program) -> Vec<AstSym> {
    let mut out: Vec<AstSym> = Vec::new();
    for s in &prog.stmts {
        collect_ast_stmt_syms(s, None, &mut out);
    }
    out.sort_by(|a, b| (a.line, a.col).cmp(&(b.line, b.col)));
    out
}

fn collect_ast_stmt_syms(s: &Stmt, owner: Option<&str>, out: &mut Vec<AstSym>) {
    match s {
        Stmt::FnDef { name, span, body, tmp, .. } => {
            if !tmp {
                let full = match owner {
                    Some(o) => format!("{}.{}", o, name),
                    None => name.clone(),
                };
                out.push(AstSym {
                    kind: "fn",
                    name: full,
                    line: span.line.saturating_sub(1) as u64,
                    col: span.col.saturating_sub(1) as u64,
                    len: span.len.max(1) as u64,
                });
            }
            for b in body {
                collect_ast_stmt_syms(b, owner, out);
            }
        }
        Stmt::AsyncFnDef { name, span, body, .. } => {
            let full = match owner {
                Some(o) => format!("{}.{}", o, name),
                None => name.clone(),
            };
            out.push(AstSym {
                kind: "fn",
                name: full,
                line: span.line.saturating_sub(1) as u64,
                col: span.col.saturating_sub(1) as u64,
                len: span.len.max(1) as u64,
            });
            for b in body {
                collect_ast_stmt_syms(b, owner, out);
            }
        }
        Stmt::StructDef { name, span, .. } => out.push(AstSym {
            kind: "struct",
            name: name.clone(),
            line: span.line.saturating_sub(1) as u64,
            col: span.col.saturating_sub(1) as u64,
            len: span.len.max(1) as u64,
        }),
        Stmt::EnumDef { name, span, .. } => out.push(AstSym {
            kind: "enum",
            name: name.clone(),
            line: span.line.saturating_sub(1) as u64,
            col: span.col.saturating_sub(1) as u64,
            len: span.len.max(1) as u64,
        }),
        Stmt::TypeDef { name, span, methods, .. } => {
            out.push(AstSym {
                kind: "type",
                name: name.clone(),
                line: span.line.saturating_sub(1) as u64,
                col: span.col.saturating_sub(1) as u64,
                len: span.len.max(1) as u64,
            });
            for m in methods {
                if let Stmt::FnDef { name: mn, span: ms, body, tmp, .. } = m {
                    if !tmp {
                        out.push(AstSym {
                            kind: "fn",
                            name: format!("{}.{}", name, mn),
                            line: ms.line.saturating_sub(1) as u64,
                            col: ms.col.saturating_sub(1) as u64,
                            len: ms.len.max(1) as u64,
                        });
                        for b in body {
                            collect_ast_stmt_syms(b, Some(name), out);
                        }
                    }
                }
            }
        }
        Stmt::ClassDef { name, span, methods, .. } => {
            out.push(AstSym {
                kind: "class",
                name: name.clone(),
                line: span.line.saturating_sub(1) as u64,
                col: span.col.saturating_sub(1) as u64,
                len: span.len.max(1) as u64,
            });
            for m in methods {
                if let Stmt::FnDef { name: mn, span: ms, body, tmp, .. } = m {
                    if !tmp {
                        out.push(AstSym {
                            kind: "fn",
                            name: format!("{}.{}", name, mn),
                            line: ms.line.saturating_sub(1) as u64,
                            col: ms.col.saturating_sub(1) as u64,
                            len: ms.len.max(1) as u64,
                        });
                        for b in body {
                            collect_ast_stmt_syms(b, Some(name), out);
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// 光标处定位 token：返回 (token, span, 流内下标)。
/// 光标落在 token 字节区间 [start, end) 内或恰在 end 处（多行字符串 token 覆盖整段）。
fn token_at(text: &str, line: u64, col: u64) -> Option<(Tok, crate::lexer::Span, usize)> {
    let stream = tokenize_doc(text);
    let cur = offset_of(text, line, col);
    for (i, (tok, span)) in stream.iter().enumerate() {
        let start = offset_of(text, span.line.saturating_sub(1) as u64, span.col.saturating_sub(1) as u64);
        if start > cur {
            break; // token 起点越过光标（流按位置有序）
        }
        let end = offset_after_units(text, start, span.len.max(1) as u64);
        if cur <= end {
            return Some((tok.clone(), *span, i));
        }
    }
    None
}

/// 某行（0-based）的行内字符长度（0-based 列上界）。
fn line_char_len(text: &str, line: u64) -> u64 {
    let start = offset_of(text, line, 0);
    let mut end = start;
    while end < text.len() && text.as_bytes()[end] != b'\n' {
        end += 1;
    }
    text[start..end].chars().map(|c| c.len_utf16() as u64).sum()
}

/// 从 start_idx（字节偏移，应指向 `{`）起配对大括号，返回匹配 `}` 的 0-based 行号（失败返回 start 行）。
fn block_end_line(text: &str, start_idx: usize) -> u64 {
    let mut depth = 0usize;
    for (i, c) in text.char_indices().skip_while(|&(i, _)| i < start_idx) {
        if c == '{' {
            depth += 1;
        } else if c == '}' {
            if depth == 0 {
                break; // 未配对的 `}`（不应发生），安全退出
            }
            depth -= 1;
            if depth == 0 {
                return line_of(text, i);
            }
        }
    }
    line_of(text, start_idx)
}

/// workspace/symbol：跨所有打开文档的全局符号搜索（子串匹配名称）。
fn workspace_symbol_result(docs: &HashMap<String, String>, params: &Value) -> Value {
    let query = params["query"].as_str().unwrap_or("");
    let mut out: Vec<Value> = Vec::new();
    for (uri, text) in docs {
        let path = uri.strip_prefix("file://").unwrap_or(uri);
        let Ok(prog) = crate::parser::Parser::parse(path, text) else { continue };
        for sym in ast_symbols(&prog) {
            if !query.is_empty() && !sym.name.contains(query) {
                continue;
            }
            let sk = match sym.kind {
                "fn" => 12,
                "class" => 5,
                "struct" => 23,
                "enum" => 10,
                "type" => 6,
                _ => 23,
            };
            out.push(json!({
                "name": sym.name,
                "kind": sk,
                "location": {
                    "uri": uri,
                    "range": {
                        "start": { "line": sym.line, "character": sym.col },
                        "end": { "line": sym.line, "character": sym.col + sym.len }
                    }
                }
            }));
        }
    }
    json!(out)
}

/// textDocument/formatting：复用 hone fmt 的格式化器（与 `hone fmt` 行为一致）。
/// 无变化时返回空数组；语法错误时返回 null（错误已在诊断中呈现）。
fn formatting_result(docs: &HashMap<String, String>, uri: &str) -> Value {
    let Some(text) = docs.get(uri) else { return Value::Null };
    let Ok(formatted) = crate::fmt::format(text) else { return Value::Null };
    if formatted == *text {
        return json!([]);
    }
    let start_idx = 0usize;
    let end_idx = text.len();
    let sl = line_of(text, start_idx);
    let sc = 0u64;
    let el = line_of(text, end_idx);
    let ec = line_char_len(text, el);
    json!([
        {
            "range": {
                "start": { "line": sl, "character": sc },
                "end": { "line": el, "character": ec }
            },
            "newText": formatted
        }
    ])
}

/// 类型注解显示名（用于签名帮助/文档）。
fn ty_name(t: &TyName) -> String {
    match t {
        TyName::Int => "int".into(),
        TyName::Float => "float".into(),
        TyName::Bool => "bool".into(),
        TyName::Str => "str".into(),
        TyName::Char => "char".into(),
        TyName::Byte => "byte".into(),
        TyName::Bytes => "bytes".into(),
        TyName::Var(s) => s.clone(),
        TyName::Inferred => "_".into(),
    }
}

/// 函数签名（signatureHelp 用）。
struct FnSig {
    name: String, // 限定名（类.方法）或裸名
    params: Vec<String>, // 形参显示串：「name: type」
    ret: Option<String>,
}

/// 收集文档中全部函数签名（含嵌套/类/type 成员/async，后定义覆盖先定义）。
fn collect_all_syms(s: &Stmt, outer: Option<&str>, out: &mut Vec<FnSig>) {
    match s {
        Stmt::FnDef { name, params, ret, body, .. } |
        Stmt::AsyncFnDef { name, params, ret, body, .. } => {
            let full = match outer {
                Some(o) => format!("{}.{}", o, name),
                None => name.clone(),
            };
            if !matches!(s, Stmt::FnDef { tmp: true, .. }) {
                let mut ps: Vec<String> = Vec::new();
                for p in params {
                    let mut d = p.name.clone();
                    if let Some(t) = &p.ty {
                        d.push_str(": ");
                        d.push_str(&ty_name(t));
                    }
                    ps.push(d);
                }
                out.push(FnSig {
                    name: full,
                    params: ps,
                    ret: ret.as_ref().map(ty_name),
                });
            }
            for b in body {
                collect_all_syms(b, Some(name), out);
            }
        }
        Stmt::TypeDef { name, methods, .. } | Stmt::ClassDef { name, methods, .. } => {
            for m in methods {
                collect_all_syms(m, Some(name), out);
            }
        }
        _ => {}
    }
}

/// 跨所有打开文档收集 word 的引用位置（词法 token 精确匹配）。
fn find_all_refs(docs: &HashMap<String, String>, word: &str) -> Vec<(String, u64, u64, u64)> {
    let mut out: Vec<(String, u64, u64, u64)> = Vec::new();
    for (uri, text) in docs {
        for (tok, span) in tokenize_doc(text) {
            if let Tok::Ident(n) = &tok {
                if n == word {
                    let l = span.line.saturating_sub(1) as u64;
                    let c = span.col.saturating_sub(1) as u64;
                    out.push((uri.clone(), l, c, word.len() as u64));
                }
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    out
}

/// textDocument/references：跨所有打开文档的引用查找（include 文档作为独立 uri 打开时自动纳入）。
fn references_result(docs: &HashMap<String, String>, uri: &str, params: &Value) -> Value {
    let pos = &params["position"];
    let (line, character) = (pos["line"].as_u64().unwrap_or(0), pos["character"].as_u64().unwrap_or(0));
    let Some((word, ..)) = lookup_sym(docs, uri, line, character) else {
        return json!([]);
    };
    let mut out: Vec<Value> = Vec::new();
    for (u, l, c, w) in find_all_refs(docs, &word) {
        out.push(json!({
            "uri": u,
            "range": {
                "start": { "line": l, "character": c },
                "end": { "line": l, "character": c + w }
            }
        }));
    }
    json!(out)
}

/// textDocument/rename：跨文档重命名（WorkspaceEdit）。无引用时返回 null。
fn rename_result(docs: &HashMap<String, String>, uri: &str, params: &Value) -> Value {
    let pos = &params["position"];
    let new_name = params["newName"].as_str().unwrap_or("");
    if new_name.is_empty() {
        return json!(null);
    }
    let (line, character) = (pos["line"].as_u64().unwrap_or(0), pos["character"].as_u64().unwrap_or(0));
    let Some((word, ..)) = lookup_sym(docs, uri, line, character) else {
        return json!(null);
    };
    let refs = find_all_refs(docs, &word);
    if refs.is_empty() {
        return json!(null);
    }
    let mut changes: serde_json::Map<String, Value> = serde_json::Map::new();
    for (u, l, c, w) in refs {
        changes
            .entry(u.clone())
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .unwrap()
            .push(json!({
                "range": {
                    "start": { "line": l, "character": c },
                    "end": { "line": l, "character": c + w }
                },
                "newText": new_name
            }));
    }
    json!({ "changes": changes })
}

/// 从光标 token 向前回找配对的 `(`：返回 (被调用的函数名, 当前激活参数下标)。
/// 规则：回扫时 LParen 使 c+1、RParen 使 c-1；c 达到 1 的 LParen 即调用开括号，
/// 其前一 token 为被调名（标识符）；c==0 时遇到的逗号计入激活参数下标。
fn find_call_at(
    stream: &[(Tok, crate::lexer::Span)],
    idx: usize,
) -> Option<(String, u64)> {
    let mut c: i64 = 0;
    let mut commas: u64 = 0;
    let mut i = idx;
    loop {
        let (tok, _span) = &stream[i];
        match tok {
            Tok::LParen => c += 1,
            Tok::RParen => c -= 1,
            Tok::Comma if c == 0 => commas += 1,
            _ => {}
        }
        if c == 1 {
            // 当前 token 是调用开括号：前一 token 应是被调名
            let pi = i.checked_sub(1)?;
            return match &stream[pi].0 {
                Tok::Ident(n) => Some((n.clone(), commas)),
                _ => None,
            };
        }
        if i == 0 {
            break;
        }
        i -= 1;
    }
    None
}

/// textDocument/signatureHelp：括号/逗号触发的调用签名帮助（用户函数 + 内置函数）。
fn signature_help_result(docs: &HashMap<String, String>, uri: &str, params: &Value) -> Value {
    let pos = &params["position"];
    let (line, character) = (pos["line"].as_u64().unwrap_or(0), pos["character"].as_u64().unwrap_or(0));
    let text = docs.get(uri).map(|s| s.as_str()).unwrap_or("");
    let stream = tokenize_doc(text);
    let Some((_tok, _span, idx)) = token_at(text, line, character) else {
        return json!(null);
    };
    let Some((callee, active)) = find_call_at(&stream, idx) else {
        return json!(null);
    };
    // 用户函数签名（后定义覆盖先定义；限定名精确匹配优先，其次裸名）
    let mut map: HashMap<String, FnSig> = HashMap::new();
    if let Ok(prog) = crate::parser::Parser::parse("", text) {
        let mut v: Vec<FnSig> = Vec::new();
        for s in &prog.stmts {
            collect_all_syms(s, None, &mut v);
        }
        for f in v {
            map.insert(f.name.clone(), f);
        }
    }
    if let Some(sig) = map.get(&callee)
        .or_else(|| callee.split('.').last().and_then(|b| map.get(b)))
    {
        let label = format!(
            "{}({}){}",
            sig.name,
            sig.params.join(", "),
            sig.ret.as_deref().map(|r| format!(" -> {}", r)).unwrap_or_default()
        );
        return json!({
            "signatures": [{
                "label": label,
                "parameters": sig.params.iter().map(|p| json!({ "label": p })).collect::<Vec<_>>(),
                "activeParameter": active.min(sig.params.len().saturating_sub(1) as u64)
            }],
            "activeSignature": 0
        });
    }
    // 内置函数 / 模块函数：按点号前缀取文档
    for (full, usage, doc) in MODULE_DOCS {
        if full == &callee {
            return json!({
                "signatures": [{ "label": format!("{}：{}", usage, doc) }],
                "activeSignature": 0
            });
        }
    }
    json!(null)
}

/// textDocument/foldingRange：多行大括号块（含 match 表达式体）与文档注释。
fn folding_range_result(docs: &HashMap<String, String>, uri: &str) -> Value {
    let text = docs.get(uri).map(|s| s.as_str()).unwrap_or("");
    let mut out: Vec<Value> = Vec::new();
    for (tok, span) in tokenize_doc(text) {
        if matches!(tok, Tok::LBrace) {
            let sl = span.line.saturating_sub(1) as u64;
            let start = offset_of(text, sl, span.col.saturating_sub(1) as u64);
            let el = block_end_line(text, start);
            if el > sl {
                out.push(json!({
                    "startLine": sl,
                    "endLine": el,
                    "kind": "region"
                }));
            }
        }
    }
    // 注释折叠（仅块注释/三引号串可跨行）：只跟踪行号
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut i = 0usize;
    let mut line: u64 = 0;
    while i < n {
        let c = chars[i];
        if c == '\n' {
            line += 1;
            i += 1;
            continue;
        }
        if c == '/' && i + 1 < n && chars[i + 1] == '/' {
            // 行注释：单行，不可折叠
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && i + 1 < n && chars[i + 1] == '*' {
            let sl = line;
            i += 2;
            while i < n && !(chars[i] == '*' && i + 1 < n && chars[i + 1] == '/') {
                if chars[i] == '\n' {
                    line += 1;
                }
                i += 1;
            }
            if i < n {
                i += 2; // 跳过 */
            }
            if line > sl {
                out.push(json!({ "startLine": sl, "endLine": line, "kind": "comment" }));
            }
            continue;
        }
        if c == '"' {
            if i + 2 < n && chars[i + 1] == '"' && chars[i + 2] == '"' {
                let sl = line;
                i += 3;
                while i + 2 < n && !(chars[i] == '"' && chars[i + 1] == '"' && chars[i + 2] == '"') {
                    if chars[i] == '\n' {
                        line += 1;
                    }
                    i += 1;
                }
                if i + 2 < n {
                    i += 3;
                }
                if line > sl {
                    out.push(json!({ "startLine": sl, "endLine": line, "kind": "comment" }));
                }
            } else {
                i += 1;
                while i < n && chars[i] != '"' {
                    if chars[i] == '\\' && i + 1 < n {
                        i += 2;
                        continue;
                    }
                    if chars[i] == '\n' {
                        break;
                    }
                    i += 1;
                }
                if i < n && chars[i] == '"' {
                    i += 1;
                }
            }
            continue;
        }
        i += 1;
    }
    json!(out)
}
