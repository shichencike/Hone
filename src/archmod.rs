// archmod.rs - 压缩与归档（archive.* 内置函数）
// 纯 Rust 实现（zip/tar/flate2），Windows / Linux / Termux 跨平台一致。
//   archive.zip_list(path)      -> list    列出 zip 文件条目名
//   archive.zip_read(path, e)   -> str     读取 zip 中指定条目的文本内容
//   archive.zip_extract(path, d)-> int     解压 zip 到目录，返回条目数
//   archive.zip_create(path, d) -> bool    从 dict {条目名: 内容} 创建 zip
//   archive.tgz_list(path)      -> list    列出 tar.gz 条目名
//   archive.tgz_read(path, e)   -> str     读取 tar.gz 中指定条目的文本内容
//   archive.tgz_extract(path, d)-> int     解压 tar.gz 到目录，返回条目数
//   archive.tgz_create(path, d) -> bool    从 dict {条目名: 内容} 创建 tar.gz
//
// 安全措施：解压时跳过绝对路径与 `..` 穿越条目（防 zip-slip）。

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

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
            format!("`archive.*` expects a string for argument {}, got `{}`", arg + 1, other.type_name()),
            span,
            file,
            src,
            None::<&str>,
        )),
    }
}

/// 条目名安全校验：拒绝绝对路径与 `..` 穿越（zip-slip 防护）。
fn safe_entry(name: &str) -> bool {
    let p = Path::new(name);
    !p.is_absolute() && !p.components().any(|c| matches!(c, std::path::Component::ParentDir))
}

/// 将条目内容写入目标目录（自动建子目录），返回是否写入。
fn write_entry_to(dir: &str, name: &str, data: &[u8], span: Span, file: &str, src: &str) -> Result<bool, ZError> {
    if !safe_entry(name) {
        return Err(zerr(
            codes::SYSCALL,
            format!("unsafe archive entry `{}` (absolute path or `..` traversal rejected)", name),
            span,
            file,
            src,
            Some("the archive contains a suspicious path; it was not extracted"),
        ));
    }
    let target = Path::new(dir).join(name);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            zerr(codes::SYSCALL, format!("cannot create dir `{}`: {}", parent.display(), e), span, file, src, None::<&str>)
        })?;
    }
    std::fs::write(&target, data).map_err(|e| {
        zerr(codes::SYSCALL, format!("cannot write `{}`: {}", target.display(), e), span, file, src, None::<&str>)
    })?;
    Ok(true)
}

fn open(path: &str, span: Span, file: &str, src: &str) -> Result<File, ZError> {
    File::open(path).map_err(|e| {
        zerr(
            codes::FILE_NOT_FOUND,
            format!("cannot open `{}`: {}", path, e),
            span,
            file,
            src,
            Some("check the path"),
        )
    })
}

/// 从 dict 参数提取 {条目名: 文本内容}。
fn entries_from_dict(v: &Value, span: Span, file: &str, src: &str) -> Result<Vec<(String, Vec<u8>)>, ZError> {
    match v {
        Value::Dict(entries) => {
            let mut out = Vec::with_capacity(entries.len());
            for (k, val) in entries {
                let content = match val {
                    Value::Str(s) => s.as_bytes().to_vec(),
                    other => {
                        return Err(zerr(
                            codes::TYPE_MISMATCH,
                            format!("archive entry `{}` must be a string, got `{}`", k, other.type_name()),
                            span,
                            file,
                            src,
                            Some("entries map: {\"file.txt\": \"content\", ...}"),
                        ))
                    }
                };
                out.push((k.clone(), content));
            }
            Ok(out)
        }
        other => Err(zerr(
            codes::TYPE_MISMATCH,
            format!("`archive.*_create` expects a dict of entries, got `{}`", other.type_name()),
            span,
            file,
            src,
            Some("entries map: {\"file.txt\": \"content\", ...}"),
        )),
    }
}

/// archive 模块调用入口。
pub fn call(name: &str, args: &[Value], span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    match name {
        // ---------- zip ----------
        "archive.zip_list" => {
            let path = as_str(&args[0], 0, span, file, src)?;
            let f = open(path, span, file, src)?;
            let z = zip::ZipArchive::new(f).map_err(|e| {
                zerr(codes::SYSCALL, format!("invalid zip `{}`: {}", path, e), span, file, src, None::<&str>)
            })?;
            let names: Vec<Value> = z.file_names().map(|n| Value::Str(n.to_string())).collect();
            Ok(Value::List(names))
        }
        "archive.zip_read" => {
            let path = as_str(&args[0], 0, span, file, src)?;
            let entry = as_str(&args[1], 1, span, file, src)?;
            let f = open(path, span, file, src)?;
            let mut z = zip::ZipArchive::new(f).map_err(|e| {
                zerr(codes::SYSCALL, format!("invalid zip `{}`: {}", path, e), span, file, src, None::<&str>)
            })?;
            let mut zf = z.by_name(entry).map_err(|e| {
                zerr(
                    codes::NOT_FOUND,
                    format!("zip entry `{}` not found: {}", entry, e),
                    span,
                    file,
                    src,
                    Some("list entries with `archive.zip_list(path)`"),
                )
            })?;
            let mut buf = String::new();
            zf.read_to_string(&mut buf).map_err(|e| {
                zerr(codes::SYSCALL, format!("cannot read zip entry `{}`: {}", entry, e), span, file, src, None::<&str>)
            })?;
            Ok(Value::Str(buf))
        }
        "archive.zip_extract" => {
            let path = as_str(&args[0], 0, span, file, src)?;
            let dir = as_str(&args[1], 1, span, file, src)?;
            let f = open(path, span, file, src)?;
            let mut z = zip::ZipArchive::new(f).map_err(|e| {
                zerr(codes::SYSCALL, format!("invalid zip `{}`: {}", path, e), span, file, src, None::<&str>)
            })?;
            let names: Vec<String> = z.file_names().map(|s| s.to_string()).collect();
            let mut count = 0usize;
            for n in names {
                let mut zf = z.by_name(&n).map_err(|e| {
                    zerr(codes::SYSCALL, format!("cannot open zip entry `{}`: {}", n, e), span, file, src, None::<&str>)
                })?;
                let mut data = Vec::new();
                zf.read_to_end(&mut data).map_err(|e| {
                    zerr(codes::SYSCALL, format!("cannot read zip entry `{}`: {}", n, e), span, file, src, None::<&str>)
                })?;
                // 目录条目（以 / 结尾）仅建目录
                if n.ends_with('/') {
                    let target = Path::new(dir).join(&n);
                    std::fs::create_dir_all(&target).map_err(|e| {
                        zerr(codes::SYSCALL, format!("cannot create dir `{}`: {}", target.display(), e), span, file, src, None::<&str>)
                    })?;
                    continue;
                }
                write_entry_to(dir, &n, &data, span, file, src)?;
                count += 1;
            }
            Ok(Value::Int(count as i64))
        }
        "archive.zip_create" => {
            let path = as_str(&args[0], 0, span, file, src)?;
            let entries = entries_from_dict(&args[1], span, file, src)?;
            let f = File::create(path).map_err(|e| {
                zerr(codes::SYSCALL, format!("cannot create `{}`: {}", path, e), span, file, src, None::<&str>)
            })?;
            let mut w = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            for (name, data) in &entries {
                w.start_file(name, opts).map_err(|e| {
                    zerr(codes::SYSCALL, format!("cannot add zip entry `{}`: {}", name, e), span, file, src, None::<&str>)
                })?;
                w.write_all(data).map_err(|e| {
                    zerr(codes::SYSCALL, format!("cannot write zip entry `{}`: {}", name, e), span, file, src, None::<&str>)
                })?;
            }
            w.finish().map_err(|e| {
                zerr(codes::SYSCALL, format!("cannot finalize zip `{}`: {}", path, e), span, file, src, None::<&str>)
            })?;
            Ok(Value::Bool(true))
        }
        // ---------- tar.gz ----------
        "archive.tgz_list" => {
            let path = as_str(&args[0], 0, span, file, src)?;
            let f = open(path, span, file, src)?;
            let gz = flate2::read::GzDecoder::new(f);
            let mut ar = tar::Archive::new(gz);
            let mut names = Vec::new();
            for entry in ar.entries().map_err(|e| {
                zerr(codes::SYSCALL, format!("invalid tar.gz `{}`: {}", path, e), span, file, src, None::<&str>)
            })? {
                let e = entry.map_err(|err| {
                    zerr(codes::SYSCALL, format!("cannot read tar entry: {}", err), span, file, src, None::<&str>)
                })?;
                if let Ok(n) = e.path().map(|p| p.to_string_lossy().into_owned()) {
                    names.push(Value::Str(n));
                }
            }
            Ok(Value::List(names))
        }
        "archive.tgz_read" => {
            let path = as_str(&args[0], 0, span, file, src)?;
            let entry = as_str(&args[1], 1, span, file, src)?;
            let f = open(path, span, file, src)?;
            let gz = flate2::read::GzDecoder::new(f);
            let mut ar = tar::Archive::new(gz);
            let mut found: Option<String> = None;
            for e in ar.entries().map_err(|e| {
                zerr(codes::SYSCALL, format!("invalid tar.gz `{}`: {}", path, e), span, file, src, None::<&str>)
            })? {
                let mut e = e.map_err(|err| {
                    zerr(codes::SYSCALL, format!("cannot read tar entry: {}", err), span, file, src, None::<&str>)
                })?;
                let n = e.path().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                if n == entry {
                    let mut buf = String::new();
                    e.read_to_string(&mut buf).map_err(|err| {
                        zerr(codes::SYSCALL, format!("cannot read tar entry `{}`: {}", entry, err), span, file, src, None::<&str>)
                    })?;
                    found = Some(buf);
                    break;
                }
            }
            match found {
                Some(s) => Ok(Value::Str(s)),
                None => Err(zerr(
                    codes::NOT_FOUND,
                    format!("tar.gz entry `{}` not found", entry),
                    span,
                    file,
                    src,
                    Some("list entries with `archive.tgz_list(path)`"),
                )),
            }
        }
        "archive.tgz_extract" => {
            let path = as_str(&args[0], 0, span, file, src)?;
            let dir = as_str(&args[1], 1, span, file, src)?;
            let f = open(path, span, file, src)?;
            let gz = flate2::read::GzDecoder::new(f);
            let mut ar = tar::Archive::new(gz);
            let mut count = 0usize;
            for e in ar.entries().map_err(|e| {
                zerr(codes::SYSCALL, format!("invalid tar.gz `{}`: {}", path, e), span, file, src, None::<&str>)
            })? {
                let mut e = e.map_err(|err| {
                    zerr(codes::SYSCALL, format!("cannot read tar entry: {}", err), span, file, src, None::<&str>)
                })?;
                let n = e.path().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                let is_dir = e.header().entry_type().is_dir();
                if is_dir {
                    let target = Path::new(dir).join(&n);
                    std::fs::create_dir_all(&target).map_err(|err| {
                        zerr(codes::SYSCALL, format!("cannot create dir `{}`: {}", target.display(), err), span, file, src, None::<&str>)
                    })?;
                    continue;
                }
                let mut data = Vec::new();
                e.read_to_end(&mut data).map_err(|err| {
                    zerr(codes::SYSCALL, format!("cannot read tar entry `{}`: {}", n, err), span, file, src, None::<&str>)
                })?;
                write_entry_to(dir, &n, &data, span, file, src)?;
                count += 1;
            }
            Ok(Value::Int(count as i64))
        }
        "archive.tgz_create" => {
            let path = as_str(&args[0], 0, span, file, src)?;
            let entries = entries_from_dict(&args[1], span, file, src)?;
            let f = File::create(path).map_err(|e| {
                zerr(codes::SYSCALL, format!("cannot create `{}`: {}", path, e), span, file, src, None::<&str>)
            })?;
            let gz = flate2::write::GzEncoder::new(f, flate2::Compression::default());
            let mut ar = tar::Builder::new(gz);
            for (name, data) in &entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                ar.append_data(&mut header, name, data.as_slice()).map_err(|e| {
                    zerr(codes::SYSCALL, format!("cannot add tar entry `{}`: {}", name, e), span, file, src, None::<&str>)
                })?;
            }
            ar.finish().map_err(|e| {
                zerr(codes::SYSCALL, format!("cannot finalize tar.gz `{}`: {}", path, e), span, file, src, None::<&str>)
            })?;
            Ok(Value::Bool(true))
        }
        _ => Err(zerr(
            codes::NOT_IMPLEMENTED,
            format!("unknown archive function `{}`", name),
            span,
            file,
            src,
            None::<&str>,
        )),
    }
}
