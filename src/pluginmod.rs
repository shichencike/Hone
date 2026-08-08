// pluginmod.rs - 插件系统（plugin.* 内置函数）
// 在 typed FFI（load 动态库）基础上提供运行时注册机制：
//   plugin.load(path, alias) -> bool  加载动态库并注册（alias 前缀调用库函数）
//   plugin.has(name)         -> bool  查询别名是否已注册
//   plugin.list()            -> list  列出已注册插件 [{"name":..., "path":...}, ...]
//   plugin.unload(name)      -> bool  注销插件（库句柄由进程管理，仅移除注册）
//
// 与 load 语句的区别：load 是编译期声明 + 静态检查；plugin.* 是运行期动态注册，
// 调用仍走同一 C ABI 通道（int64 或 typed FFI 签名），注册后可 `alias.func(...)` 调用。

use std::collections::HashMap;
use std::sync::Mutex;

use once_cell::sync::Lazy;

use crate::error::codes;
use crate::error::ZError;
use crate::interp::Value;
use crate::lexer::Span;

/// 插件注册表：别名 -> 动态库路径。interp 调用库函数时若本实例未加载，会查询此表。
static PLUGINS: Lazy<Mutex<HashMap<String, String>>> = Lazy::new(|| Mutex::new(HashMap::new()));

fn zerr(code: &'static str, msg: impl Into<String>, span: Span, file: &str, src: &str, help: Option<impl Into<String>>) -> ZError {
    ZError::new(code, msg, file, src, span.line, span.col, span.len.max(1), help)
}

fn as_str<'a>(v: &'a Value, arg: usize, span: Span, file: &str, src: &str) -> Result<&'a str, ZError> {
    match v {
        Value::Str(s) => Ok(s),
        other => Err(zerr(
            codes::TYPE_MISMATCH,
            format!("`plugin.*` expects a string for argument {}, got `{}`", arg + 1, other.type_name()),
            span,
            file,
            src,
            None::<&str>,
        )),
    }
}

/// 注册插件（interp 的 load 语句成功后也会调用）。
pub fn register(name: &str, path: &str) {
    PLUGINS.lock().unwrap().insert(name.to_string(), path.to_string());
}

/// 查询插件路径（interp 调用库函数时使用）。
pub fn lookup(name: &str) -> Option<String> {
    PLUGINS.lock().unwrap().get(name).cloned()
}

/// 注销插件。
pub fn unregister(name: &str) -> bool {
    PLUGINS.lock().unwrap().remove(name).is_some()
}

/// plugin 模块调用入口。
pub fn call(name: &str, args: &[Value], span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    match name {
        "plugin.load" => {
            let path = as_str(&args[0], 0, span, file, src)?;
            let alias = as_str(&args[1], 1, span, file, src)?;
            // 校验库可加载（句柄即刻释放，进程内由调用时再次加载）
            match unsafe { libloading::Library::new(path) } {
                Ok(_) => {
                    register(alias, path);
                    Ok(Value::Bool(true))
                }
                Err(e) => Err(zerr(
                    codes::DLL_LOAD,
                    format!("cannot load plugin `{}`: {}", path, e),
                    span,
                    file,
                    src,
                    Some("check the plugin path and architecture (x64)"),
                )),
            }
        }
        "plugin.has" => {
            let alias = as_str(&args[0], 0, span, file, src)?;
            Ok(Value::Bool(PLUGINS.lock().unwrap().contains_key(alias)))
        }
        "plugin.list" => {
            let map = PLUGINS.lock().unwrap();
            let mut out = Vec::with_capacity(map.len());
            for (name, path) in map.iter() {
                out.push(Value::Dict(vec![
                    ("name".to_string(), Value::Str(name.clone())),
                    ("path".to_string(), Value::Str(path.clone())),
                ]));
            }
            Ok(Value::List(out))
        }
        "plugin.unload" => {
            let alias = as_str(&args[0], 0, span, file, src)?;
            Ok(Value::Bool(unregister(alias)))
        }
        _ => Err(zerr(
            codes::NOT_IMPLEMENTED,
            format!("unknown plugin function `{}`", name),
            span,
            file,
            src,
            None::<&str>,
        )),
    }
}
