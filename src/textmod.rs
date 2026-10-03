// textmod.rs - 文本处理（diff.* / regex.* 增强）
// 纯 Rust + 复用已有 regex 依赖，Windows / Linux / Termux 跨平台一致。
//   diff.lines(a, b)      -> list  逐行对比，返回操作列表（"-" 删除 / "+" 新增 / " " 相同）
//   diff.unified(a, b)    -> str   生成 unified diff 文本（@@ 块头 + -/+ 行）
//   regex.find(pattern, text)  -> list   返回所有非重叠匹配的子串
//   regex.groups(pattern, text)-> list   返回首个匹配的捕获组（第 0 项为整体匹配）
//   regex.split(pattern, text) -> list   按正则拆分文本
//
// 说明：regex.match / regex.replace 已有内置实现；此处补充 find/groups/split。

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
            format!("`diff/regex.*` expects a string for argument {}, got `{}`", arg + 1, other.type_name()),
            span,
            file,
            src,
            None::<&str>,
        )),
    }
}

fn compile_re(pattern: &str, name: &str, span: Span, file: &str, src: &str) -> Result<regex::Regex, ZError> {
    regex::Regex::new(pattern).map_err(|e| {
        zerr(
            codes::SYNTAX,
            format!("invalid regex in `{}`: {}", name, e),
            span,
            file,
            src,
            Some("check the pattern syntax"),
        )
    })
}

/// 按行切分（保留末尾空行语义：末位空字符串忽略）。
fn split_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();
    if lines.last().map(|s| s.is_empty()).unwrap_or(false) {
        lines.pop();
    }
    lines
}

/// LCS 逐行 diff：返回 (操作, 行文本) 序列。op 为 '-' / '+' / ' '。
fn lcs_diff(a: &[String], b: &[String]) -> Vec<(char, String)> {
    let n = a.len();
    let m = b.len();
    // dp[i][j] = LCS 长度
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut out = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push((' ', a[i].clone()));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            out.push(('-', a[i].clone()));
            i += 1;
        } else {
            out.push(('+', b[j].clone()));
            j += 1;
        }
    }
    while i < n {
        out.push(('-', a[i].clone()));
        i += 1;
    }
    while j < m {
        out.push(('+', b[j].clone()));
        j += 1;
    }
    out
}

/// textmod 模块调用入口。
pub fn call(name: &str, args: &[Value], span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    match name {
        "diff.lines" => {
            let a = as_str(&args[0], 0, span, file, src)?;
            let b = as_str(&args[1], 1, span, file, src)?;
            let ops = lcs_diff(&split_lines(a), &split_lines(b));
            let items: Vec<Value> = ops
                .into_iter()
                .map(|(op, line)| {
                    let entries = vec![
                        ("op".to_string(), Value::Str(op.to_string())),
                        ("line".to_string(), Value::Str(line)),
                    ];
                    Value::Dict(entries)
                })
                .collect();
            Ok(Value::List(items))
        }
        "diff.unified" => {
            let a = as_str(&args[0], 0, span, file, src)?;
            let b = as_str(&args[1], 1, span, file, src)?;
            let ops = lcs_diff(&split_lines(a), &split_lines(b));
            let mut out = String::new();
            let (mut adds, mut dels) = (0i64, 0i64);
            for (op, line) in &ops {
                match op {
                    '+' => {
                        adds += 1;
                        out.push_str(&format!("+{}\n", line));
                    }
                    '-' => {
                        dels += 1;
                        out.push_str(&format!("-{}\n", line));
                    }
                    _ => {
                        out.push_str(&format!(" {}\n", line));
                    }
                }
            }
            // 构造 @@ 块头（简化：整个文件一个块）
            let header = format!("@@ -1,{} +1,{} @@\n", dels.max(1), adds.max(1));
            let mut result = String::new();
            result.push_str(&header);
            result.push_str(&out);
            Ok(Value::Str(result))
        }
        "regex.find" => {
            let pattern = as_str(&args[0], 0, span, file, src)?;
            let text = as_str(&args[1], 1, span, file, src)?;
            let re = compile_re(pattern, "regex.find", span, file, src)?;
            let matches: Vec<Value> = re.find_iter(text).map(|m| Value::Str(m.as_str().to_string())).collect();
            Ok(Value::List(matches))
        }
        "regex.groups" => {
            let pattern = as_str(&args[0], 0, span, file, src)?;
            let text = as_str(&args[1], 1, span, file, src)?;
            let re = compile_re(pattern, "regex.groups", span, file, src)?;
            match re.captures(text) {
                Some(caps) => {
                    let mut groups: Vec<Value> = Vec::with_capacity(caps.len());
                    for i in 0..caps.len() {
                        match caps.get(i) {
                            Some(m) => groups.push(Value::Str(m.as_str().to_string())),
                            None => groups.push(Value::Null), // 未参与匹配的可选组
                        }
                    }
                    Ok(Value::List(groups))
                }
                None => Ok(Value::List(Vec::new())), // 无匹配返回空列表
            }
        }
        "regex.split" => {
            let pattern = as_str(&args[0], 0, span, file, src)?;
            let text = as_str(&args[1], 1, span, file, src)?;
            let re = compile_re(pattern, "regex.split", span, file, src)?;
            let parts: Vec<Value> = re.split(text).map(|p| Value::Str(p.to_string())).collect();
            Ok(Value::List(parts))
        }
        _ => Err(zerr(
            codes::NOT_IMPLEMENTED,
            format!("unknown text function `{}`", name),
            span,
            file,
            src,
            None::<&str>,
        )),
    }
}
