// header.rs - C 头文件原型解析器（受限子集，纯 Rust，零外部依赖）
// 用途：load "lib.so" as m from "header.h";  与  hone bind <header.h>
// 解析能力：
//   - 跳过注释（/* */、//）与预处理行（#...）
//   - 跳过 struct/enum/union 定义体与 extern "C" 裸块
//   - 提取函数原型：返回类型 函数名(参数列表);
//   - 简单 typedef 收集（标量别名 → 展开；struct 别名 → 按值报错/指针为 ptr；
//     函数指针别名 → 视为回调）
//   - 类型映射：int/long/short/size_t 等 → int；float/double → float；
//     bool/_Bool → bool；char* / const char* → str；其余指针 → ptr；void → void
//   - 不支持的原型（回调参数、变参 ...、数组参数、结构体按值、long double）
//     以 FfiSig.unsupported = Some(原因) 标记，调用时直接报错而非 ABI 崩溃

use std::collections::HashMap;

use crate::ast::{FfiParam, FfiSig, FfiTy};
use crate::lexer::Span;

/// 解析 C 头文件源码，返回 FFI 签名列表（含 unsupported 标记的原型）。
pub fn parse(src: &str, span: Span) -> Vec<FfiSig> {
    let toks = tokenize(src);
    let defs = collect_typedefs(&toks);
    let mut sigs = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        match &toks[i] {
            Tok::Ident(_) => {
                // 标识符后跟 `(` → 可能是函数原型（或函数指针变量）
                if toks.get(i + 1) == Some(&Tok::Punct("(".to_string())) {
                    // 函数指针变量 (*name)(...)：name 前是 `*` 且再前是 `(`
                    if i > 0 && toks[i - 1] == Tok::Punct("*".to_string()) && toks.get(i - 2) == Some(&Tok::Punct("(".to_string())) {
                        if let Some(j) = match_paren(&toks, i + 1) {
                            i = skip_to_semi(&toks, j + 1);
                            continue;
                        }
                    }
                    if let Some((sig, next)) = try_proto(&toks, i, &defs, span) {
                        sigs.push(sig);
                        i = next;
                        continue;
                    }
                }
                i += 1;
            }
            Tok::Punct(p) if p == "typedef" || p == "struct" || p == "enum" || p == "union" => {
                // 由 collect_typedefs / 扫描跳过；这里防止误把关键字当函数名
                i += 1;
            }
            Tok::Punct(p) if p == "{" || p == "}" => {
                // extern "C" 裸块 / 宏块：跳到大括号平衡
                i += 1;
            }
            _ => i += 1,
        }
    }
    sigs
}

// ---------- 词法 ----------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Ident(String),
    Str(String),
    Punct(String),
}

fn tokenize(src: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let b = src.as_bytes();
    let mut i = 0;
    let n = b.len();
    while i < n {
        let c = b[i] as char;
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        // 注释
        if c == '/' && b.get(i + 1) == Some(&b'/') {
            while i < n && b[i] as char != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && b.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < n && !(b[i] as char == '*' && b[i + 1] as char == '/') {
                i += 1;
            }
            i = (i + 2).min(n);
            continue;
        }
        // 预处理行
        if c == '#' {
            while i < n && b[i] as char != '\n' {
                i += 1;
            }
            continue;
        }
        // 字符串字面量（如 extern "C"）
        if c == '"' {
            i += 1;
            let mut s = String::new();
            while i < n && b[i] as char != '"' {
                if b[i] as char == '\\' && i + 1 < n {
                    i += 1;
                }
                s.push(b[i] as char);
                i += 1;
            }
            i += 1;
            out.push(Tok::Str(s));
            continue;
        }
        // 标识符 / 数字
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < n && (b[i] as char).is_ascii_alphanumeric() || (i < n && b[i] as char == '_') {
                i += 1;
            }
            out.push(Tok::Ident(src[start..i].to_string()));
            continue;
        }
        if c.is_ascii_digit() {
            let start = i;
            while i < n && ((b[i] as char).is_ascii_alphanumeric() || b[i] as char == '_') {
                i += 1;
            }
            out.push(Tok::Ident(src[start..i].to_string()));
            continue;
        }
        // 变参 ...
        if c == '.' && b.get(i + 1) == Some(&b'.') && b.get(i + 2) == Some(&b'.') {
            out.push(Tok::Punct("...".to_string()));
            i += 3;
            continue;
        }
        // 其他标点
        out.push(Tok::Punct(c.to_string()));
        i += 1;
    }
    out
}

// ---------- typedef 收集 ----------

/// typedef 解析结果：标量别名（可展开）、结构体别名、函数指针别名。
#[derive(Debug, Clone)]
enum TyDef {
    /// 标量类型 token 列表，如 sqlite3_int64 → [long, long, int]
    Scalar(Vec<String>),
    /// struct/union/enum 定义别名：按值传参不支持，指针为 ptr
    Struct,
    /// 函数指针别名（回调）
    FnPtr,
}

fn collect_typedefs(toks: &[Tok]) -> HashMap<String, TyDef> {
    let mut defs = HashMap::new();
    let mut i = 0;
    while i < toks.len() {
        if toks[i] == Tok::Punct("typedef".to_string()) {
            // 收集到分号
            let mut j = i + 1;
            let mut body: Vec<String> = Vec::new();
            while j < toks.len() && toks[j] != Tok::Punct(";".to_string()) {
                match &toks[j] {
                    Tok::Ident(s) | Tok::Str(s) => body.push(s.clone()),
                    Tok::Punct(p) => body.push(p.clone()),
                }
                j += 1;
            }
            // 最后一个标识符是别名，其余是类型
            let mut name_idx = None;
            for (idx, t) in body.iter().enumerate() {
                if is_ident_str(t) {
                    name_idx = Some(idx);
                }
            }
            if let Some(ni) = name_idx {
                let name = body[ni].clone();
                let ty = &body[..ni];
                let def = if ty.iter().any(|t| t == "(") {
                    // 函数指针 typedef
                    TyDef::FnPtr
                } else if ty.iter().any(|t| t == "struct" || t == "union" || t == "enum" || t == "{") {
                    // 结构体定义别名（typedef struct {...} NAME;）
                    TyDef::Struct
                } else {
                    let mut clean: Vec<String> = ty
                        .iter()
                        .filter(|t| t.as_str() != ";" && t.as_str() != "{")
                        .cloned()
                        .collect();
                    if clean.is_empty() {
                        TyDef::Struct
                    } else {
                        clean.retain(|t| t != ";" && t != "{");
                        TyDef::Scalar(clean)
                    }
                };
                defs.insert(name, def);
            }
            i = j + 1;
            continue;
        }
        i += 1;
    }
    defs
}

// ---------- 原型提取 ----------

/// 尝试在 toks[i]（函数名标识符）处解析一个函数原型。
/// 成功返回 (签名, 下一个扫描位置)；失败返回 None。
fn try_proto(toks: &[Tok], i: usize, defs: &HashMap<String, TyDef>, span: Span) -> Option<(FfiSig, usize)> {
    let name = match &toks[i] {
        Tok::Ident(s) => s.clone(),
        _ => return None,
    };
    // 名字前必须是类型 token 序列（若前一个是 `*` 或 `(` 则为函数指针，已在上层跳过）
    if i == 0 {
        return None;
    }
    // 收集返回类型：从 i-1 往前直到 ; { } 或起点
    let mut rstart = i - 1;
    while rstart > 0 {
        let t = &toks[rstart];
        if t == &Tok::Punct(";".to_string())
            || t == &Tok::Punct("{".to_string())
            || t == &Tok::Punct("}".to_string())
            || t == &Tok::Punct("(".to_string())
            || t == &Tok::Punct(",".to_string())
        {
            rstart += 1;
            break;
        }
        rstart -= 1;
    }
    if rstart == 0 && i > 0 {
        rstart = 0;
    }
    let ret_toks: Vec<String> = toks[rstart..i]
        .iter()
        .filter_map(|t| match t {
            Tok::Ident(s) | Tok::Str(s) | Tok::Punct(s) => Some(s.clone()),
        })
        .collect();
    if ret_toks.is_empty() {
        return None;
    }
    // 找匹配的右括号
    let open = i + 1;
    let close = match_paren(toks, open)?;
    // `)` 后应为 `;`（声明）或 `{`（定义）；跳过属性宏后再判断
    let mut k = close + 1;
    while k < toks.len() {
        match &toks[k] {
            Tok::Ident(s) if is_attr_prefix(s) => {
                // __attribute__((...)) / __declspec(...) / 全大写宏
                if toks.get(k + 1) == Some(&Tok::Punct("(".to_string())) {
                    if let Some(end) = match_paren(toks, k + 1) {
                        k = end + 1;
                        continue;
                    }
                }
                k += 1;
            }
            Tok::Ident(s) if is_allcaps(s) => k += 1,
            Tok::Ident(s) if matches!(s.as_str(), "const" | "volatile" | "restrict" | "inline" | "static" | "extern") => {
                k += 1
            }
            Tok::Punct(p) if p == ";" => break,
            Tok::Punct(p) if p == "{" => return None, // 函数定义，跳过
            _ => return None,
        }
    }
    if k >= toks.len() || toks[k] != Tok::Punct(";".to_string()) {
        return None;
    }
    let next = k + 1;

    // 解析返回类型
    let ret = match map_type(&ret_toks, defs, 0) {
        Ok(t) => t,
        Err(reason) => {
            return Some((
                FfiSig { name, params: Vec::new(), ret: FfiTy::Int, unsupported: Some(reason), span },
                next,
            ))
        }
    };

    // 解析参数列表
    let mut params: Vec<FfiParam> = Vec::new();
    let param_toks = split_top(&toks[open + 1..close]);
    for (pi, pt) in param_toks.iter().enumerate() {
        // 无参：void / 空
        let only: Vec<&String> = pt.iter().filter_map(|t| match t {
            Tok::Ident(s) | Tok::Punct(s) => Some(s),
            Tok::Str(_) => None,
        }).collect();
        if only.len() == 1 && only[0] == "void" {
            continue;
        }
        if only.is_empty() {
            continue;
        }
        // 不支持项
        if pt.iter().any(|t| t == &Tok::Punct("...".to_string())) {
            return Some((
                FfiSig { name, params: Vec::new(), ret: FfiTy::Int, unsupported: Some("variadic (`...`)"), span },
                next,
            ));
        }
        if pt.iter().any(|t| t == &Tok::Punct("(".to_string())) {
            return Some((
                FfiSig { name, params: Vec::new(), ret: FfiTy::Int, unsupported: Some("callback parameter"), span },
                next,
            ));
        }
        if pt.iter().any(|t| t == &Tok::Punct("[".to_string())) {
            return Some((
                FfiSig { name, params: Vec::new(), ret: FfiTy::Int, unsupported: Some("array parameter"), span },
                next,
            ));
        }
        // 分离参数名（最后一个非类型标识符）与类型
        let strs: Vec<String> = pt
            .iter()
            .filter_map(|t| match t {
                Tok::Ident(s) | Tok::Punct(s) => Some(s.clone()),
                Tok::Str(_) => None,
            })
            .collect();
        let (pty, pname) = extract_param_type(&strs, defs);
        let ty = match pty {
            Ok(t) => t,
            Err(reason) => {
                return Some((
                    FfiSig { name, params: Vec::new(), ret: FfiTy::Int, unsupported: Some(reason), span },
                    next,
                ))
            }
        };
        let pname = pname.unwrap_or_else(|| format!("p{}", pi + 1));
        params.push(FfiParam { name: pname, ty, span });
    }
    Some((FfiSig { name, params, ret, unsupported: None, span }, next))
}

/// 找到从 open 开始的匹配右括号索引。
fn match_paren(toks: &[Tok], open: usize) -> Option<usize> {
    if toks.get(open) != Some(&Tok::Punct("(".to_string())) {
        return None;
    }
    let mut depth = 0usize;
    for j in open..toks.len() {
        match &toks[j] {
            Tok::Punct(p) if p == "(" => depth += 1,
            Tok::Punct(p) if p == ")" => {
                depth -= 1;
                if depth == 0 {
                    return Some(j);
                }
            }
            _ => {}
        }
    }
    None
}

/// 从 idx 开始跳过属性宏等，直到分号。
fn skip_to_semi(toks: &[Tok], mut idx: usize) -> usize {
    while idx < toks.len() && toks[idx] != Tok::Punct(";".to_string()) {
        idx += 1;
    }
    idx + 1
}

/// 按顶层逗号分割 token 序列（忽略括号内的逗号）。
fn split_top(toks: &[Tok]) -> Vec<Vec<Tok>> {
    let mut out = Vec::new();
    let mut cur: Vec<Tok> = Vec::new();
    let mut depth = 0usize;
    for t in toks {
        match t {
            Tok::Punct(p) if p == "(" || p == "[" || p == "{" => {
                depth += 1;
                cur.push(t.clone());
            }
            Tok::Punct(p) if p == ")" || p == "]" || p == "}" => {
                depth = depth.saturating_sub(1);
                cur.push(t.clone());
            }
            Tok::Punct(p) if p == "," && depth == 0 => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(t.clone()),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// 分离参数：返回 (类型映射结果, 参数名)。
fn extract_param_type(strs: &[String], defs: &HashMap<String, TyDef>) -> (Result<FfiTy, &'static str>, Option<String>) {
    // 参数名 = 最后一个既不是类型关键字、也不是 typedef 名、也不是宏的标识符
    let mut name_idx = None;
    for (idx, s) in strs.iter().enumerate() {
        if is_ident_str(s) && !is_typeish(s, defs) {
            name_idx = Some(idx);
        }
    }
    let (ty_strs, pname) = match name_idx {
        Some(ni) => {
            let mut t = strs.to_vec();
            let name = t.remove(ni);
            (t, Some(name))
        }
        None => (strs.to_vec(), None),
    };
    (map_type(&ty_strs, defs, 0), pname)
}

// ---------- 类型映射 ----------

fn map_type(toks: &[String], defs: &HashMap<String, TyDef>, depth: usize) -> Result<FfiTy, &'static str> {
    if depth > 8 {
        return Err("typedef recursion too deep");
    }
    let mut stars = 0usize;
    let mut base: Vec<String> = Vec::new();
    for t in toks {
        if t == "*" {
            stars += 1;
        } else if is_modifier(t) {
            // const/volatile/restrict/extern/static/inline 忽略
        } else {
            base.push(t.clone());
        }
    }
    // 展开 typedef（标量递归展开；结构体/函数指针别名单独处理）
    let mut resolved: Vec<String> = Vec::new();
    for t in &base {
        if let Some(def) = defs.get(t) {
            match def {
                TyDef::FnPtr => return Err("callback"),
                TyDef::Struct => {
                    if stars > 0 {
                        return Ok(FfiTy::Ptr);
                    }
                    return Err("struct by value");
                }
                TyDef::Scalar(toks2) => resolved.extend(toks2.iter().cloned()),
            }
        } else {
            resolved.push(t.clone());
        }
    }
    // 指针类型
    if stars > 0 {
        let has_char = resolved.iter().any(|t| t == "char");
        let has_void = resolved.iter().any(|t| t == "void");
        if stars == 1 && has_char && !has_void {
            return Ok(FfiTy::Str);
        }
        return Ok(FfiTy::Ptr);
    }
    // 标量类型：去掉宏前缀与属性
    let real: Vec<&String> = resolved
        .iter()
        .filter(|t| !is_allcaps(t) && t.as_str() != "__attribute__" && t.as_str() != "__declspec")
        .collect();
    if real.is_empty() {
        return Err("empty type");
    }
    if real.iter().any(|t| t.as_str() == "void") {
        return Ok(FfiTy::Void);
    }
    if real.iter().any(|t| t.as_str() == "float" || t.as_str() == "double") {
        if real.iter().any(|t| t.as_str() == "long") {
            return Err("`long double` is not supported");
        }
        return Ok(FfiTy::Float);
    }
    if real.iter().any(|t| t.as_str() == "bool" || t.as_str() == "_Bool") {
        return Ok(FfiTy::Bool);
    }
    if real.iter().any(|t| t.as_str() == "struct" || t.as_str() == "union") {
        return Err("struct by value");
    }
    if real.iter().any(|t| t.as_str() == "enum") {
        return Ok(FfiTy::Int);
    }
    // int/long/short/size_t/char 等整数族（含未知 typedef，如 time_t）
    Ok(FfiTy::Int)
}

// ---------- 工具 ----------

fn is_modifier(s: &str) -> bool {
    matches!(
        s,
        "const" | "volatile" | "restrict" | "extern" | "static" | "inline" | "register" | "signed" | "unsigned"
    )
}

fn is_ident_str(s: &str) -> bool {
    !s.is_empty()
        && (s.chars().next().unwrap().is_ascii_alphabetic() || s.chars().next().unwrap() == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_allcaps(s: &str) -> bool {
    s.len() > 1 && s.chars().all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
}

fn is_attr_prefix(s: &str) -> bool {
    s.starts_with("__") || s == "declspec"
}

/// 该标识符是否为类型关键字 / typedef 名 / 宏（不可作为参数名）。
fn is_typeish(s: &str, defs: &HashMap<String, TyDef>) -> bool {
    is_modifier(s)
        || is_allcaps(s)
        || is_attr_prefix(s)
        || matches!(
            s,
            "void" | "char" | "short" | "int" | "long" | "float" | "double" | "bool" | "_Bool" | "struct" | "union"
                | "enum"
        )
        || defs.contains_key(s)
}
