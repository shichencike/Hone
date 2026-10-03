// sysutilmod.rs - 系统工具（glob.* / temp.* 内置函数）
// 纯 Rust + 复用已有 regex 依赖，Windows / Linux / Termux 跨平台一致。
//   glob.match(pattern, path) -> bool  判断路径是否匹配 glob 模式
//   glob.list(pattern)       -> list  递归列出匹配 glob 模式的文件路径
//   temp.dir([prefix])       -> str   创建临时目录并返回路径
//   temp.file([prefix])      -> str   创建临时文件并返回路径
//   temp.remove(path)        -> bool  递归删除临时文件/目录
//
// glob 语法：* 匹配单层内任意字符；? 匹配单个字符；** 跨目录任意层级；
//           [abc] / [a-z] 字符类；其余字符按字面匹配。路径分隔符统一按 / 处理。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::codes;
use crate::error::ZError;
use crate::interp::Value;
use crate::lexer::Span;

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn zerr(code: &'static str, msg: impl Into<String>, span: Span, file: &str, src: &str, help: Option<impl Into<String>>) -> ZError {
    ZError::new(code, msg, file, src, span.line, span.col, span.len.max(1), help)
}

fn as_str<'a>(v: &'a Value, arg: usize, span: Span, file: &str, src: &str) -> Result<&'a str, ZError> {
    match v {
        Value::Str(s) => Ok(s),
        other => Err(zerr(
            codes::TYPE_MISMATCH,
            format!("`glob/temp.*` expects a string for argument {}, got `{}`", arg + 1, other.type_name()),
            span,
            file,
            src,
            None::<&str>,
        )),
    }
}

/// 可选字符串参数：Value::Null 或缺失视为 None。
fn opt_str(v: Option<&Value>, arg: usize, span: Span, file: &str, src: &str) -> Result<Option<String>, ZError> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Str(s)) => Ok(Some(s.clone())),
        Some(other) => Err(zerr(
            codes::TYPE_MISMATCH,
            format!("expected a string or null for argument {}, got `{}`", arg + 1, other.type_name()),
            span,
            file,
            src,
            None::<&str>,
        )),
    }
}

/// glob 模式 → 正则表达式（整个路径匹配，`**` 跨目录）。
fn glob_to_regex(pattern: &str) -> String {
    let chars: Vec<char> = pattern.chars().collect();
    let n = chars.len();
    let mut out = String::from("^");
    let mut i = 0usize;
    while i < n {
        let c = chars[i];
        match c {
            '*' => {
                // `**` 跨目录任意层级；单个 `*` 只匹配单层
                if i + 1 < n && chars[i + 1] == '*' {
                    out.push_str(".*");
                    i += 2;
                    // 跳过 `**` 后多余的 `/`（`**/` 与 `/` 均可）
                    while i < n && chars[i] == '/' {
                        i += 1;
                    }
                } else {
                    out.push_str("[^/]*");
                    i += 1;
                }
            }
            '?' => {
                out.push_str("[^/]");
                i += 1;
            }
            '[' => {
                // 字符类 [abc] / [a-z] / [^...]：原样透传（regex 与 glob 语法近似）
                let start = i;
                i += 1;
                if i < n && (chars[i] == '^' || chars[i] == '!') {
                    i += 1;
                }
                if i < n && chars[i] == ']' {
                    i += 1; // 允许 ] 作为类内首字符
                }
                while i < n && chars[i] != ']' {
                    i += 1;
                }
                if i >= n {
                    // 未闭合：按字面处理
                    out.push_str(&regex::escape(&chars[start..].iter().collect::<String>()));
                    break;
                }
                i += 1;
                out.push_str(&chars[start..i].iter().collect::<String>());
            }
            _ => {
                out.push_str(&regex::escape(&c.to_string()));
                i += 1;
            }
        }
    }
    out.push('$');
    out
}

/// 递归收集 base 下所有文件的相对路径（/ 分隔）。
fn collect_files(base: &Path, dir: &Path, out: &mut Vec<String>) -> std::io::Result<()> {
    let rd = std::fs::read_dir(dir)?;
    let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_files(base, &path, out)?;
        } else if path.is_file() {
            if let Ok(rel) = path.strip_prefix(base) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    Ok(())
}

/// 从模式中提取 base 目录：通配符之前的部分。
fn glob_base(pattern: &str) -> PathBuf {
    let pat = pattern.replace('\\', "/");
    // 找第一个通配符位置
    let cut = pat
        .char_indices()
        .find(|(_, c)| matches!(c, '*' | '?' | '['))
        .map(|(i, _)| i)
        .unwrap_or(pat.len());
    let prefix = &pat[..cut];
    // 取通配符前最后一个 / 之前的完整目录；无 / 则当前目录
    match prefix.rfind('/') {
        Some(i) => {
            let base = &prefix[..i];
            if base.is_empty() {
                PathBuf::from(".")
            } else {
                PathBuf::from(base)
            }
        }
        None => PathBuf::from("."),
    }
}

fn match_glob(pattern: &str, path: &str) -> bool {
    let re = match regex::Regex::new(&glob_to_regex(pattern)) {
        Ok(r) => r,
        Err(_) => return false,
    };
    re.is_match(path)
}

/// glob/temp 模块调用入口。
pub fn call(name: &str, args: &[Value], span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    match name {
        "glob.match" => {
            let pattern = as_str(&args[0], 0, span, file, src)?;
            let path = as_str(&args[1], 1, span, file, src)?;
            Ok(Value::Bool(match_glob(pattern, path.replace('\\', "/").as_str())))
        }
        "glob.list" => {
            let pattern = as_str(&args[0], 0, span, file, src)?;
            let re = regex::Regex::new(&glob_to_regex(pattern)).map_err(|e| {
                zerr(
                    codes::SYNTAX,
                    format!("invalid glob pattern `{}`: {}", pattern, e),
                    span,
                    file,
                    src,
                    Some("check the pattern syntax (`*`, `?`, `**`, `[...]`)"),
                )
            })?;
            let base = glob_base(pattern);
            let mut rels: Vec<String> = Vec::new();
            collect_files(&base, &base, &mut rels).map_err(|e| {
                zerr(
                    codes::SYSCALL,
                    format!("cannot scan `{}`: {}", base.display(), e),
                    span,
                    file,
                    src,
                    None::<&str>,
                )
            })?;
            let mut matched: Vec<Value> = rels
                .into_iter()
                .filter(|rel| re.is_match(rel))
                .map(Value::Str)
                .collect();
            matched.sort_by(|a, b| a.display().cmp(&b.display()));
            Ok(Value::List(matched))
        }
        "temp.dir" => {
            let prefix = opt_str(args.get(0), 0, span, file, src)?;
            let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let name = format!(
                "{}{}-{}-{}",
                prefix.unwrap_or_else(|| "hone-".to_string()),
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0),
                seq
            );
            let path = std::env::temp_dir().join(name);
            std::fs::create_dir_all(&path).map_err(|e| {
                zerr(
                    codes::SYSCALL,
                    format!("cannot create temp dir `{}`: {}", path.display(), e),
                    span,
                    file,
                    src,
                    None::<&str>,
                )
            })?;
            Ok(Value::Str(path.to_string_lossy().to_string()))
        }
        "temp.file" => {
            let prefix = opt_str(args.get(0), 0, span, file, src)?;
            let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let name = format!(
                "{}{}-{}-{}",
                prefix.unwrap_or_else(|| "hone-".to_string()),
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0),
                seq
            );
            let path = std::env::temp_dir().join(name);
            std::fs::File::create(&path).map_err(|e| {
                zerr(
                    codes::SYSCALL,
                    format!("cannot create temp file `{}`: {}", path.display(), e),
                    span,
                    file,
                    src,
                    None::<&str>,
                )
            })?;
            Ok(Value::Str(path.to_string_lossy().to_string()))
        }
        "temp.remove" => {
            let path = as_str(&args[0], 0, span, file, src)?;
            let p = Path::new(path);
            let r = if p.is_dir() {
                std::fs::remove_dir_all(p)
            } else {
                std::fs::remove_file(p)
            };
            match r {
                Ok(()) => Ok(Value::Bool(true)),
                Err(_) => Ok(Value::Bool(false)), // 不存在或删除失败
            }
        }
        _ => Err(zerr(
            codes::NOT_IMPLEMENTED,
            format!("unknown glob/temp function `{}`", name),
            span,
            file,
            src,
            None::<&str>,
        )),
    }
}
