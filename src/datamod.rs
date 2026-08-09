// datamod.rs - 数据处理（csv.* 内置函数）
// 纯 Rust 实现，Windows / Linux / Termux 跨平台一致。
//   csv.parse(text)            -> list    解析 CSV 文本为行列表（每行为 str 列表）
//   csv.parse_dict(text)       -> list    解析 CSV 文本为 dict 列表（首行为表头）
//   csv.stringify(rows)        -> str     将行列表（list of list / list of dict）序列化为 CSV
//
// 解析遵循 RFC 4180 常用子集：支持引号包裹字段、" 用 "" 转义、字段内逗号/换行、CRLF 行尾。

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
            format!("`csv.*` expects a string for argument {}, got `{}`", arg + 1, other.type_name()),
            span,
            file,
            src,
            None::<&str>,
        )),
    }
}

/// 解析 CSV 文本为字段矩阵。处理引号包裹、"" 转义、字段内换行、CRLF。
fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let b: Vec<char> = text.chars().collect();
    let n = b.len();
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut i = 0usize;
    let mut in_quotes = false;

    while i < n {
        let c = b[i];
        if in_quotes {
            if c == '"' {
                if i + 1 < n && b[i + 1] == '"' {
                    field.push('"'); // "" -> 字面引号
                    i += 2;
                    continue;
                }
                in_quotes = false;
                i += 1;
                continue;
            }
            field.push(c);
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_quotes = true;
                i += 1;
            }
            ',' => {
                row.push(std::mem::take(&mut field));
                i += 1;
            }
            '\r' => {
                if i + 1 < n && b[i + 1] == '\n' {
                    i += 1; // CRLF 统一为行结束
                }
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
                i += 1;
            }
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
                i += 1;
            }
            _ => {
                field.push(c);
                i += 1;
            }
        }
    }
    // 末尾字段（可能无结尾换行）
    if !field.is_empty() || !row.is_empty() || in_quotes {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// 单行行 -> Value::List(Value::Str)
fn row_to_value(row: &[String]) -> Value {
    Value::List(row.iter().map(|s| Value::Str(s.clone())).collect())
}

/// 字段转字符串（stringify 用）。
fn field_str(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        other => other.display(),
    }
}

/// 单字段 CSV 转义：包含逗号/引号/换行时包裹引号并转义。
fn escape_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// 从 Value 提取行列表：list of list / list of dict（dict 键作为表头）。
fn rows_from_value(v: &Value, span: Span, file: &str, src: &str) -> Result<Vec<Vec<String>>, ZError> {
    let rows_v = match v {
        Value::List(rows) => rows,
        other => {
            return Err(zerr(
                codes::TYPE_MISMATCH,
                format!("`csv.stringify` expects a list of rows, got `{}`", other.type_name()),
                span,
                file,
                src,
                Some("pass [[\"a\", 1], [\"b\", 2]] or a list of dicts"),
            ))
        }
    };
    let mut out: Vec<Vec<String>> = Vec::with_capacity(rows_v.len());
    for (ri, row_v) in rows_v.iter().enumerate() {
        match row_v {
            Value::List(cells) => {
                let mut row = Vec::with_capacity(cells.len());
                for cell in cells {
                    row.push(field_str(cell));
                }
                out.push(row);
            }
            Value::Dict(entries) => {
                // dict 行：键为表头。首行为表头时补一次；其余行按首行键序取值。
                let mut row = Vec::with_capacity(entries.len());
                for (_, val) in entries {
                    row.push(field_str(val));
                }
                out.push(row);
            }
            other => {
                return Err(zerr(
                    codes::TYPE_MISMATCH,
                    format!(
                        "`csv.stringify` row {} must be a list or dict, got `{}`",
                        ri + 1,
                        other.type_name()
                    ),
                    span,
                    file,
                    src,
                    None::<&str>,
                ))
            }
        }
    }
    Ok(out)
}

/// csv 模块调用入口。
pub fn call(name: &str, args: &[Value], span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    match name {
        "csv.parse" => {
            let text = as_str(&args[0], 0, span, file, src)?;
            let rows = parse_csv(text);
            Ok(Value::List(rows.iter().map(|r| row_to_value(r)).collect()))
        }
        "csv.parse_dict" => {
            let text = as_str(&args[0], 0, span, file, src)?;
            let rows = parse_csv(text);
            // 首行为表头；无数据返回空列表
            let mut it = rows.into_iter();
            let headers = match it.next() {
                Some(h) => h,
                None => return Ok(Value::List(Vec::new())),
            };
            let mut out: Vec<Value> = Vec::new();
            for row in it {
                let mut entries: Vec<(String, Value)> = Vec::with_capacity(headers.len());
                for (idx, h) in headers.iter().enumerate() {
                    let cell = row.get(idx).cloned().unwrap_or_default();
                    entries.push((h.clone(), Value::Str(cell)));
                }
                out.push(Value::Dict(entries));
            }
            Ok(Value::List(out))
        }
        "csv.stringify" => {
            let rows = rows_from_value(&args[0], span, file, src)?;
            let mut out = String::new();
            for row in rows {
                let cells: Vec<String> = row.iter().map(|s| escape_field(s)).collect();
                out.push_str(&cells.join(","));
                out.push('\n');
            }
            Ok(Value::Str(out))
        }
        _ => Err(zerr(
            codes::NOT_IMPLEMENTED,
            format!("unknown csv function `{}`", name),
            span,
            file,
            src,
            None::<&str>,
        )),
    }
}
