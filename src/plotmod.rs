// plotmod.rs - 绘图与数据格式（plot.* / yaml.* 内置函数）
// 纯 Rust 实现，Windows / Linux / Termux 跨平台一致。
//   plot.bar(values[, labels])   -> str  生成 SVG 柱状图
//   plot.line(xs, ys)            -> str  生成 SVG 折线图
//   yaml.parse(text)             -> value 解析 YAML 子集（map/list/标量/注释/引号字符串）
//   yaml.stringify(value)        -> str  将 Hone 值序列化为 YAML
//
// YAML 为常用子集：支持嵌套 map/list、缩进层级、行内注释、单/双引号字符串、
// 布尔/数值/null 标量。不支持的完整 YAML 特性（锚点/别名/多文档/流式 [] {}）会被拒绝或按字面处理。

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

fn as_num(v: &Value, what: &str, span: Span, file: &str, src: &str) -> Result<f64, ZError> {
    match v {
        Value::Int(i) => Ok(*i as f64),
        Value::Float(f) => Ok(*f),
        other => Err(zerr(
            codes::TYPE_MISMATCH,
            format!("`{}` expects a number, got `{}`", what, other.type_name()),
            span,
            file,
            src,
            None::<&str>,
        )),
    }
}

fn num_list(v: &Value, what: &str, span: Span, file: &str, src: &str) -> Result<Vec<f64>, ZError> {
    match v {
        Value::List(items) => items.iter().enumerate().map(|(i, x)| as_num(x, &format!("{} element {}", what, i + 1), span, file, src)).collect(),
        other => Err(zerr(
            codes::TYPE_MISMATCH,
            format!("`{}` expects a list of numbers, got `{}`", what, other.type_name()),
            span,
            file,
            src,
            None::<&str>,
        )),
    }
}

fn str_list(v: &Value, what: &str, span: Span, file: &str, src: &str) -> Result<Vec<String>, ZError> {
    match v {
        Value::List(items) => items.iter().enumerate().map(|(i, x)| Ok(as_str(x, i, span, file, src)?.to_string())).collect(),
        other => Err(zerr(
            codes::TYPE_MISMATCH,
            format!("`{}` expects a list of strings, got `{}`", what, other.type_name()),
            span,
            file,
            src,
            None::<&str>,
        )),
    }
}

fn esc_svg(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// 数值格式化：整数值不带小数点。
fn fmt_num(x: f64) -> String {
    if x.fract() == 0.0 && x.abs() < 9.007_199_254_740_992e15 {
        format!("{}", x as i64)
    } else {
        format!("{:.2}", x)
    }
}

/// plot/yaml 模块调用入口。
pub fn call(name: &str, args: &[Value], span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    match name {
        "plot.bar" => {
            let values = num_list(&args[0], "plot.bar", span, file, src)?;
            let labels: Vec<String> = if args.len() > 1 {
                str_list(&args[1], "plot.bar labels", span, file, src)?
            } else {
                values.iter().enumerate().map(|(i, _)| i.to_string()).collect()
            };
            if values.is_empty() {
                return Err(zerr(codes::TYPE_MISMATCH, "plot.bar: empty values", span, file, src, Some("pass at least one value")));
            }
            let n = values.len();
            let max_v = values.iter().cloned().fold(0.0f64, |a, b| a.max(b)).max(1.0);
            let w = (n as f64) * 60.0 + 80.0;
            let h = 320.0f64;
            let pad = 40.0f64;
            let chart_h = h - pad - 30.0;
            let mut out = String::new();
            out.push_str(&format!(
                "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\">\n",
                w, h, w, h
            ));
            out.push_str(&format!("<rect x=\"0\" y=\"0\" width=\"{}\" height=\"{}\" fill=\"#f8f9fa\"/>\n", w, h));
            let bar_w = 60.0f64;
            for (i, v) in values.iter().enumerate() {
                let bh = (v / max_v * chart_h).abs();
                let x = pad + i as f64 * 60.0;
                let y = pad + chart_h - bh;
                out.push_str(&format!(
                    "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" fill=\"#4c6ef5\" rx=\"2\"/>\n",
                    x, y, bar_w - 8.0, bh
                ));
                out.push_str(&format!(
                    "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"12\" fill=\"#333\" text-anchor=\"middle\">{}</text>\n",
                    x + (bar_w - 8.0) / 2.0,
                    y - 5.0,
                    fmt_num(*v)
                ));
                if i < labels.len() {
                    out.push_str(&format!(
                        "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"11\" fill=\"#555\" text-anchor=\"middle\">{}</text>\n",
                        x + (bar_w - 8.0) / 2.0,
                        h - 10.0,
                        esc_svg(&labels[i])
                    ));
                }
            }
            // 基线
            out.push_str(&format!("<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" stroke=\"#999\" stroke-width=\"1\"/>\n", pad, pad + chart_h, pad + n as f64 * 60.0, pad + chart_h));
            out.push_str("</svg>\n");
            Ok(Value::Str(out))
        }
        "plot.line" => {
            let xs = num_list(&args[0], "plot.line xs", span, file, src)?;
            let ys = num_list(&args[1], "plot.line ys", span, file, src)?;
            if xs.len() != ys.len() || xs.is_empty() {
                return Err(zerr(
                    codes::TYPE_MISMATCH,
                    "plot.line: xs and ys must be non-empty lists of equal length",
                    span,
                    file,
                    src,
                    Some("pass two lists of the same length"),
                ));
            }
            let n = xs.len();
            let w = 640.0f64;
            let h = 320.0f64;
            let pad = 40.0f64;
            let chart_w = w - pad * 2.0;
            let chart_h = h - pad * 2.0;
            let min_x = xs.iter().cloned().fold(f64::INFINITY, f64::min);
            let max_x = xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            let min_y = ys.iter().cloned().fold(f64::INFINITY, f64::min);
            let max_y = ys.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            let sx = if max_x > min_x { (max_x - min_x) / chart_w } else { 1.0 };
            let syy = if max_y > min_y { (max_y - min_y) / chart_h } else { 1.0 };
            let syoff = min_y;
            let px = |x: f64| pad + (x - min_x) / sx;
            let py = |y: f64| pad + chart_h - (y - syoff) / syy;
            let mut out = String::new();
            out.push_str(&format!("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\">\n", w, h, w, h));
            out.push_str(&format!("<rect x=\"0\" y=\"0\" width=\"{}\" height=\"{}\" fill=\"#f8f9fa\"/>\n", w, h));
            // 网格线
            for i in 0..=4 {
                let gy = pad + chart_h * i as f64 / 4.0;
                let val = max_y - (max_y - min_y) * i as f64 / 4.0;
                out.push_str(&format!("<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" stroke=\"#ddd\" stroke-width=\"1\"/>\n", pad, gy, w - pad, gy));
                out.push_str(&format!("<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"11\" fill=\"#666\" text-anchor=\"end\">{}</text>\n", pad - 6.0, gy + 4.0, fmt_num(val)));
            }
            // 折线
            let mut pts = String::new();
            for i in 0..n {
                pts.push_str(&format!("{:.1},{:.1} ", px(xs[i]), py(ys[i])));
            }
            out.push_str(&format!("<polyline points=\"{}\" fill=\"none\" stroke=\"#4c6ef5\" stroke-width=\"2\"/>\n", pts.trim()));
            // 数据点
            for i in 0..n {
                out.push_str(&format!("<circle cx=\"{:.1}\" cy=\"{:.1}\" r=\"3\" fill=\"#4c6ef5\"/>\n", px(xs[i]), py(ys[i])));
            }
            out.push_str("</svg>\n");
            Ok(Value::Str(out))
        }
        "yaml.parse" => {
            let text = as_str(&args[0], 0, span, file, src)?;
            let value = parse_yaml(text, span, file, src)?;
            Ok(value)
        }
        "yaml.stringify" => {
            let out = stringify_yaml(&args[0], 0, span, file, src)?;
            Ok(Value::Str(out))
        }
        _ => Err(zerr(
            codes::NOT_IMPLEMENTED,
            format!("unknown plot/yaml function `{}`", name),
            span,
            file,
            src,
            None::<&str>,
        )),
    }
}

// ---------- YAML 子集解析 ----------

/// 去除行内注释（# 前无引号包裹）。
fn strip_comment(line: &str) -> String {
    let mut in_single = false;
    let mut in_double = false;
    for (i, c) in line.char_indices() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '#' if !in_single && !in_double => return line[..i].to_string(),
            _ => {}
        }
    }
    line.to_string()
}

/// 解析标量：数值 / 布尔 / null / 引号字符串 / 裸字符串。
fn parse_scalar(s: &str) -> Value {
    let t = s.trim();
    if t.is_empty() {
        return Value::Null;
    }
    if (t.starts_with('"') && t.ends_with('"') && t.len() >= 2)
        || (t.starts_with('\'') && t.ends_with('\'') && t.len() >= 2)
    {
        let inner = &t[1..t.len() - 1];
        return Value::Str(inner.to_string());
    }
    match t {
        "true" | "True" | "TRUE" => return Value::Bool(true),
        "false" | "False" | "FALSE" => return Value::Bool(false),
        "null" | "Null" | "NULL" | "~" => return Value::Null,
        _ => {}
    }
    if let Ok(i) = t.parse::<i64>() {
        return Value::Int(i);
    }
    if let Ok(f) = t.parse::<f64>() {
        return Value::Float(f);
    }
    Value::Str(t.to_string())
}

/// 解析一个块（map 或 list），indent 为该块的最小缩进。
fn parse_block(
    lines: &[(usize, String)],
    idx: &mut usize,
    indent: usize,
    span: Span,
    file: &str,
    src: &str,
) -> Result<Value, ZError> {
    if *idx >= lines.len() {
        return Ok(Value::Null);
    }
    let (first_indent, first) = &lines[*idx];
    if *first_indent < indent {
        return Ok(Value::Null);
    }
    // 判断块类型：首行以 "- " 开头为列表
    if first.trim_start().starts_with("-") && first.trim_start().len() > 1 {
        let mut items: Vec<Value> = Vec::new();
        while *idx < lines.len() {
            let (ind, line) = &lines[*idx];
            if *ind < indent {
                break;
            }
            let content = line.trim_start();
            if !content.starts_with('-') {
                break;
            }
            let rest = content[1..].trim_start().to_string();
            *idx += 1;
            if rest.is_empty() {
                // "-" 后无内容：下一行缩进更深的块作为元素
                let item = parse_block(lines, idx, indent + 1, span, file, src)?;
                items.push(item);
            } else if let Some((k, v)) = split_key_value(&rest) {
                // "- key: value" → dict 元素
                let mut entries: Vec<(String, Value)> = Vec::new();
                if v.trim().is_empty() {
                    let child = parse_block(lines, idx, *ind + 1, span, file, src)?;
                    entries.push((k, child));
                } else {
                    entries.push((k, parse_scalar(&v)));
                }
                // 后续同缩进的 "- " 项也可能属于同一 dict（如 "- a: 1" 后 "- b: 2"）——
                // 这里简化为每个 "-" 独立元素；嵌套 dict 由 "- key:" 下一层承载。
                items.push(Value::Dict(entries));
            } else {
                items.push(parse_scalar(&rest));
            }
        }
        return Ok(Value::List(items));
    }
    // map：key: value 序列
    let mut entries: Vec<(String, Value)> = Vec::new();
    while *idx < lines.len() {
        let (ind, line) = &lines[*idx];
        if *ind < indent {
            break;
        }
        let content = line.trim_start();
        if content.is_empty() || content.starts_with('-') {
            break;
        }
        let (k, v) = match split_key_value(content) {
            Some(pair) => pair,
            None => {
                return Err(zerr(
                    codes::SYNTAX,
                    format!("yaml: expected `key: value` at line `{}`", content),
                    span,
                    file,
                    src,
                    Some("map entries must be `key: value`"),
                ))
            }
        };
        *idx += 1;
        if v.trim().is_empty() {
            let child = parse_block(lines, idx, *ind + 1, span, file, src)?;
            entries.push((k, child));
        } else {
            entries.push((k, parse_scalar(&v)));
        }
    }
    Ok(Value::Dict(entries))
}

/// 拆分 `key: value`，返回 (key, value)。value 可为空。
fn split_key_value(line: &str) -> Option<(String, String)> {
    let mut in_single = false;
    let mut in_double = false;
    let chars: Vec<char> = line.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            ':' if !in_single && !in_double => {
                let key = line[..i].trim().to_string();
                if key.is_empty() {
                    return None;
                }
                let val = line[i + 1..].trim_start().to_string();
                return Some((key, val));
            }
            _ => {}
        }
    }
    None
}

fn parse_yaml(text: &str, span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    // 预处理：去注释、去空行，记录缩进（按空格计）
    let mut lines: Vec<(usize, String)> = Vec::new();
    for raw in text.lines() {
        let stripped = strip_comment(raw);
        if stripped.trim().is_empty() {
            continue;
        }
        let indent = stripped.chars().take_while(|c| *c == ' ').count();
        lines.push((indent, stripped.trim_end().to_string()));
    }
    if lines.is_empty() {
        return Ok(Value::Null);
    }
    let mut idx = 0usize;
    let base = lines[0].0;
    parse_block(&lines, &mut idx, base, span, file, src)
}

// ---------- YAML 序列化 ----------

fn yaml_scalar(v: &Value) -> Option<String> {
    match v {
        Value::Str(s) => Some(s.clone()),
        Value::Int(i) => Some(i.to_string()),
        Value::Float(f) => Some(f.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Null => Some("null".to_string()),
        _ => None,
    }
}

fn yaml_quote(s: &str) -> String {
    if s.contains(':') || s.contains('#') || s.starts_with('-') || s.trim() != s || s.is_empty() {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

fn stringify_yaml(v: &Value, depth: usize, span: Span, file: &str, src: &str) -> Result<String, ZError> {
    let pad = "  ".repeat(depth);
    match v {
        Value::Dict(entries) => {
            if entries.is_empty() {
                return Ok(format!("{}null", pad));
            }
            let mut out = String::new();
            for (k, val) in entries {
                match yaml_scalar(val) {
                    Some(s) => out.push_str(&format!("{}{}: {}\n", pad, yaml_quote(k), s)),
                    None => {
                        out.push_str(&format!("{}{}:\n", pad, yaml_quote(k)));
                        out.push_str(&stringify_yaml(val, depth + 1, span, file, src)?);
                        if !out.ends_with('\n') {
                            out.push('\n');
                        }
                    }
                }
            }
            Ok(out)
        }
        Value::List(items) => {
            if items.is_empty() {
                return Ok(format!("{}[]", pad));
            }
            let mut out = String::new();
            for item in items {
                match yaml_scalar(item) {
                    Some(s) => out.push_str(&format!("{}- {}\n", pad, s)),
                    None => {
                        let sub = stringify_yaml(item, depth + 1, span, file, src)?;
                        // 嵌套块：首行内联（若有），其余缩进
                        let sub_lines: Vec<&str> = sub.lines().collect();
                        out.push_str(&format!("{}- {}", pad, sub_lines.first().unwrap_or(&"").trim()));
                        for l in &sub_lines[1..] {
                            out.push('\n');
                            out.push_str(&format!("{}  {}", pad, l.trim_start()));
                        }
                        out.push('\n');
                    }
                }
            }
            Ok(out)
        }
        other => Ok(format!("{}{}", pad, yaml_scalar(other).unwrap_or_else(|| other.display()))),
    }
}
