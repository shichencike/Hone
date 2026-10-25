// ffi.rs - 动态库 FFI 共享层（解释器与字节码 VM 共用）
//
// 设计要点：
//  - 解释器（interp）与 VM（vm）共用同一份 `load` / typed FFI 调用 / libffi 回调
//    实现，保证「两引擎输出（含报错）逐字节一致」的铁律——错误文案只在此处定义。
//  - 引擎差异只在「回调体的执行宿主」`CbEngine`：解释器用快照环境新建 `Interp`
//    实例执行，VM 用快照表新建 `Vm` 实例执行；两者共用同一个 libffi closure 入口。
//  - `FfiState`（已加载库 / 懒加载路径 / 签名表）作为 `Interp` 与 `Vm` 的字段持有；
//    调用前把它 `take()` 出来可让宿主在回调期间被借用（回调宿主是**全新实例**，
//    与解释器既有做法一致：C 调用期间不能复用当前引擎实例）。
//  - `FfiTy::Callback` 经 libffi `ffi_closure` 实现（0-8 标量参数、仅同步回调）。

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::ffi::c_void;
use std::os::raw::c_char;
use std::ptr::{addr_of_mut, null_mut};
use std::sync::Arc;

use libffi_sys::{
    ffi_abi_FFI_DEFAULT_ABI, ffi_call, ffi_cif, ffi_closure, ffi_closure_alloc, ffi_closure_free,
    ffi_prep_cif, ffi_prep_cif_var, ffi_prep_closure_loc, ffi_status_FFI_OK, ffi_type,
};

use crate::ast::{FfiParam, FfiSig, FfiTy, TyName};
use crate::error::{codes, ZError};
use crate::interp::{FnDef, LambdaVal, Value};
use crate::lexer::Span;
use crate::vm::{Chunk, Vm};

// ───────────────────────────── 状态 ─────────────────────────────

/// FFI 运行期状态：已加载动态库 + 懒加载路径表 + 签名块注册表。
/// 解释器与 VM 各持一份（互不共享），但读写逻辑全在本模块。
#[derive(Default)]
pub struct FfiState {
    /// 已加载的动态库（别名 → Library）
    pub libs: HashMap<String, libloading::Library>,
    /// 懒加载库（别名 → 路径），首次调用时加载
    pub lazy_libs: HashMap<String, String>,
    /// load 签名块声明的 FFI 函数（键为完整调用名 "alias.fn"）
    pub ffi_sigs: HashMap<String, FfiSig>,
}

impl FfiState {
    pub fn new() -> Self {
        FfiState::default()
    }

    fn load_library(&mut self, name: &str, path: &str, span: Span, file: &str, src: &str) -> Result<(), ZError> {
        let lib = unsafe { libloading::Library::new(path) }.map_err(|e| {
            rerr(
                codes::DLL_LOAD,
                format!("cannot load dynamic library `{}`: {}", path, e),
                span,
                Some("check the library path and architecture"),
                file,
                src,
            )
        })?;
        self.libs.insert(name.to_string(), lib);
        Ok(())
    }
}

/// 与解释器 `runtime_err` 同构：构造带源码上下文的错误（文案对齐的唯一入口）。
fn rerr(
    code: &'static str,
    msg: impl Into<String>,
    span: Span,
    help: Option<impl Into<String>>,
    file: &str,
    src: &str,
) -> ZError {
    ZError::new(code, msg, file, src, span.line, span.col, span.len.max(1), help)
}

// ───────────────────────────── load ─────────────────────────────

/// 注册并（可选）加载动态库：语义与解释器 `exec_load` 完全一致——
/// 签名注册（`from` 头文件先注册、签名块同名覆盖）→ lazy 只登记路径 → 立即加载
/// 并同步到 plugin 注册表（plugin.list / plugin.has 可见）。
pub fn load(
    st: &mut FfiState,
    lazy: bool,
    path: &str,
    alias: Option<&str>,
    from: Option<&str>,
    sigs: &[FfiSig],
    span: Span,
    file: &str,
    src: &str,
) -> Result<(), ZError> {
    let lib_name = match alias {
        Some(a) => a.to_string(),
        None => std::path::Path::new(path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "lib".to_string()),
    };
    if let Some(hpath) = from {
        let hsrc = std::fs::read_to_string(hpath).map_err(|e| {
            rerr(
                codes::NOT_FOUND,
                format!("cannot read header `{}`: {}", hpath, e),
                span,
                Some("check the header path, or remove the `from` clause"),
                file,
                src,
            )
        })?;
        let header_sigs = crate::header::parse(&hsrc);
        for sig in &header_sigs {
            st.ffi_sigs.insert(format!("{}.{}", lib_name, sig.name), sig.clone());
        }
    }
    for sig in sigs {
        st.ffi_sigs.insert(format!("{}.{}", lib_name, sig.name), sig.clone());
    }
    if lazy {
        st.lazy_libs.insert(lib_name, path.to_string());
        return Ok(());
    }
    st.load_library(&lib_name, path, span, file, src)?;
    // 同步到插件注册表（plugin.list / plugin.has 可见）
    crate::pluginmod::register(&lib_name, path);
    Ok(())
}

// ───────────────────────────── 调用 ─────────────────────────────

/// 回调体执行宿主的快照：把当前引擎的可见环境打包给「新实例」使用。
pub enum CbEngine {
    /// 解释器宿主：回调体在新 `Interp` 实例中执行
    Interp {
        fns: HashMap<String, Arc<FnDef>>,
        structs: HashMap<String, Vec<(String, TyName, bool)>>,
        enums: HashMap<String, Vec<String>>,
        alias_map: HashMap<String, String>,
    },
    /// VM 宿主：回调体在新 `Vm` 实例中执行（编译后的表按 Arc 共享，克隆 O(1)）
    Vm(VmCbEnv),
}

/// VM 侧回调宿主快照：编译后模块的全部表（Arc 共享，克隆代价 O(1)）。
#[derive(Clone)]
pub struct VmCbEnv {
    pub chunks: Arc<Vec<Chunk>>,
    pub func_map: Arc<HashMap<String, usize>>,
    pub struct_defs: Arc<HashMap<String, Vec<(String, bool)>>>,
    pub enum_defs: Arc<HashMap<String, Vec<(String, usize)>>>,
    pub type_defs: Arc<HashMap<String, (String, Vec<(String, TyName, bool)>)>>,
    pub aliases: Arc<Vec<(String, String)>>,
    pub async_fns: Arc<HashSet<String>>,
}

impl CbEngine {
    /// 回调可调用名是否存在（解释器查函数表，VM 查编译后的 func_map）。
    fn has_fn(&self, name: &str) -> bool {
        match self {
            CbEngine::Interp { fns, .. } => fns.contains_key(name),
            CbEngine::Vm(env) => env.func_map.contains_key(name),
        }
    }
}

/// 调用动态库函数（C ABI 约定 / typed FFI 两类）。
///
/// `host` 只在实参里出现回调原型时被调用一次，用于快照回调执行宿主——
/// 无回调的调用不产生任何快照开销。
pub fn call(
    st: &mut FfiState,
    callee: &str,
    args: Vec<Value>,
    span: Span,
    file: &str,
    src: &str,
    host: &dyn Fn() -> CbEngine,
) -> Result<Value, ZError> {
    let dot = callee.rfind('.').unwrap();
    let (lib_name, func_name) = (&callee[..dot], &callee[dot + 1..]);
    // 懒加载库：首次调用时加载
    if !st.libs.contains_key(lib_name) {
        if let Some(path) = st.lazy_libs.get(lib_name).cloned() {
            st.load_library(lib_name, &path, span, file, src)?;
        } else if let Some(path) = crate::pluginmod::lookup(lib_name) {
            // 运行期 plugin.load 注册的插件：调用时加载
            st.load_library(lib_name, &path, span, file, src)?;
        } else {
            return Err(rerr(
                codes::NOT_FOUND,
                format!("library `{}` is not loaded", lib_name),
                span,
                Some(format!(
                    "add `load \"path/to/lib\" as {};` or `plugin.load(path, \"{}\")` before calling",
                    lib_name, lib_name
                )),
                file,
                src,
            ));
        }
    }
    if let Some(sig) = st.ffi_sigs.get(callee).cloned() {
        // typed FFI：按签名块声明的类型转换参数与返回值
        return call_ffi_typed(st, &sig, lib_name, func_name, args, span, file, src, host);
    }
    if args.len() > 8 {
        return Err(rerr(
            codes::DLL_ARG,
            format!("`{}` takes at most 8 arguments", callee),
            span,
            Some("the C ABI convention supports up to 8 int64 parameters"),
            file,
            src,
        ));
    }
    let mut cargs = [0i64; 8];
    for (i, a) in args.iter().enumerate() {
        match a {
            Value::Int(v) => cargs[i] = *v,
            other => {
                return Err(rerr(
                    codes::TYPE_MISMATCH,
                    format!("`{}` expects `int` arguments, got `{}`", callee, other.type_name()),
                    span,
                    Some("the C ABI convention maps `int` to int64"),
                    file,
                    src,
                ));
            }
        }
    }
    let sym: libloading::Symbol<KaLibFn> = {
        let lib = st.libs.get(lib_name).unwrap();
        unsafe { lib.get(func_name.as_bytes()) }
    }
    .map_err(|e| {
        rerr(
            codes::NOT_FOUND,
            format!("symbol `{}` not found in library `{}`: {}", func_name, lib_name, e),
            span,
            Some("check the exported symbol name (e.g. `#[no_mangle] pub extern \"C\" fn`)"),
            file,
            src,
        )
    })?;
    let ret = unsafe { sym(cargs[0], cargs[1], cargs[2], cargs[3], cargs[4], cargs[5], cargs[6], cargs[7]) };
    Ok(Value::Int(ret))
}

/// load 加载的 C ABI 库函数签名约定：全 int64 参数（不足补 0，x64 ABI 安全）。
type KaLibFn = unsafe extern "C" fn(i64, i64, i64, i64, i64, i64, i64, i64) -> i64;

/// typed FFI 调用参数值载体：写入 u64 对齐槽，libffi 按声明的 C 类型宽度读取。
#[derive(Clone, Copy)]
enum CArg {
    /// int64_t
    I(i64),
    /// int32_t
    I32(i32),
    /// double
    F(f64),
    /// C float（32 位，仅固定参数位）
    F32(f32),
}

/// typed FFI 调用返回值。
#[derive(Clone, Copy)]
enum CRet {
    I(i64),
    F(f64),
}

/// FfiTy → libffi ffi_type（固定参数位，调用方向）。Bool 传 0/1 整数（C 侧 _Bool
/// 读低字节，ABI 安全）。
fn ffi_type_of(ty: &FfiTy) -> *mut ffi_type {
    match ty {
        FfiTy::Int | FfiTy::Bool => addr_of_mut!(libffi_sys::ffi_type_sint64),
        FfiTy::Int32 => addr_of_mut!(libffi_sys::ffi_type_sint32),
        FfiTy::Float => addr_of_mut!(libffi_sys::ffi_type_double),
        FfiTy::Float32 => addr_of_mut!(libffi_sys::ffi_type_float),
        FfiTy::Str | FfiTy::Ptr | FfiTy::Callback { .. } => addr_of_mut!(libffi_sys::ffi_type_pointer),
        FfiTy::Void => unreachable!("void is not a parameter type"),
    }
}

/// FfiTy（回调原型）→ libffi ffi_type（C→Hone 方向，按 C 真实布局读取：_Bool 1 字节）。
fn ffi_cb_type_of(ty: &FfiTy) -> *mut ffi_type {
    match ty {
        FfiTy::Int => addr_of_mut!(libffi_sys::ffi_type_sint64),
        FfiTy::Int32 => addr_of_mut!(libffi_sys::ffi_type_sint32),
        FfiTy::Float => addr_of_mut!(libffi_sys::ffi_type_double),
        FfiTy::Float32 => addr_of_mut!(libffi_sys::ffi_type_float),
        FfiTy::Bool => addr_of_mut!(libffi_sys::ffi_type_uint8),
        FfiTy::Str | FfiTy::Ptr | FfiTy::Callback { .. } => addr_of_mut!(libffi_sys::ffi_type_pointer),
        FfiTy::Void => unreachable!("void is not a parameter type"),
    }
}

/// FfiTy（回调返回）→ libffi ffi_type。
fn ffi_cb_ret_type_of(ty: &FfiTy) -> *mut ffi_type {
    match ty {
        FfiTy::Void => addr_of_mut!(libffi_sys::ffi_type_void),
        other => ffi_cb_type_of(other),
    }
}

/// C 变参位 → libffi ffi_type：按 C 默认实参提升（float32→double；int32 保持 int）。
fn vararg_ffi_type(c: &CArg) -> *mut ffi_type {
    match c {
        CArg::I(_) => addr_of_mut!(libffi_sys::ffi_type_sint64),
        CArg::I32(_) => addr_of_mut!(libffi_sys::ffi_type_sint32),
        CArg::F(_) | CArg::F32(_) => addr_of_mut!(libffi_sys::ffi_type_double),
    }
}

/// 参数值写入 u64 对齐槽（槽 8 字节 ≥ 全部标量；按声明宽度写入低字节）。
fn write_slot(slot: &mut u64, c: &CArg) {
    let p = slot as *mut u64;
    unsafe {
        match c {
            CArg::I(v) => *(p as *mut i64) = *v,
            CArg::I32(v) => *(p as *mut i32) = *v,
            CArg::F(v) => *(p as *mut f64) = *v,
            CArg::F32(v) => *(p as *mut f32) = *v,
        }
    }
}

// ========== FFI 回调（零依赖受限方案） ==========
// 同步回调：C 必须在 Hone 发起的本次 FFI 调用（同线程）内回调。trampoline 由
// libffi 生成（ffi_closure，0-8 标量参数），把 C 传入的参数按原型读回，经
// thread_local 槽取回 Hone 可调用与原型，转换参数后用**全新引擎实例**执行 Hone 函数。
// 异步回调（C 保存指针日后调用）不支持：槽取空后回调会得到明确的 H999 诊断。

/// 回调窗口状态：C 回调进入 trampoline 时从中取回被代理的 Hone 可调用。
struct CbCtx {
    /// 回调原型（参数/返回类别）
    cb: FfiTy,
    /// Hone 可调用：lambda 值（二选一）
    lambda: Option<Arc<LambdaVal>>,
    /// Hone 可调用：全局函数名（二选一）
    fname: Option<String>,
    /// 回调体执行宿主快照（引擎 + 该引擎可见的全局环境）
    engine: CbEngine,
    /// 文件与源码（回调体报错的定位上下文）
    file: Arc<String>,
    src: Arc<String>,
}

thread_local! {
    /// 当前 FFI 调用的回调槽（单回调参数）。窗口语义：call_ffi_typed 安装、
    /// 收尾清理；窗口内 C 可多次回调（如 qsort 比较器），槽位持续有效。
    /// 嵌套窗口（回调体内再发起带回调的 FFI 调用）会覆盖外层槽位，不支持。
    /// 存 Arc：每次回调只克隆引用计数，不复制宿主快照表。
    static CB_CTX: RefCell<Option<Arc<CbCtx>>> = const { RefCell::new(None) };
    /// 回调体内捕获的错误（外层 FFI 调用收尾时原样上抛，保留 Hone 侧定位）
    static CB_ERR: RefCell<Option<ZError>> = const { RefCell::new(None) };
    /// Str 返回的存储（C 读指针的生命周期 = 到下一次回调前）
    static CB_STRRET: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// trampoline 分派：C 传入 n 个整数类参数 → 按槽内原型转换 → 执行 Hone 可调用。
/// 返回值按原型返回类别转 i64（C 侧读整数寄存器 / 指针）；失败返回 0 并记录诊断。
fn cb_dispatch(a: &[i64]) -> i64 {
    // 快照克隆（Arc<CbCtx>，O(1)）：槽位保持安装，供同窗口内的后续回调复用
    let Some(ctx) = CB_CTX.with(|c| c.borrow().clone()) else {
        CB_ERR.with(|e| {
            *e.borrow_mut() = Some(ZError::plain(
                codes::NOT_IMPLEMENTED,
                "callback invoked outside its FFI call window (async callbacks are not supported)",
                Some("the callback pointer is only valid during the Hone FFI call that passed it"),
            ))
        });
        return 0;
    };
    let FfiTy::Callback { params, ret } = &ctx.cb else {
        CB_ERR.with(|e| {
            *e.borrow_mut() = Some(ZError::plain(codes::INTERNAL, "internal: callback slot has no prototype", None::<&str>))
        });
        return 0;
    };
    if params.len() != a.len() {
        CB_ERR.with(|e| {
            *e.borrow_mut() = Some(ZError::plain(
                codes::DLL_ARG,
                format!("callback arity mismatch: prototype has {} parameters, C passed {}", params.len(), a.len()),
                Some("check the C side call against the declared prototype"),
            ))
        });
        return 0;
    }
    // 标量参数 → Hone 值（按原型类别读取槽内值：int32 取低 32 位、float/float32 按位重解释）
    let args: Vec<Value> = params
        .iter()
        .zip(a.iter())
        .map(|(ty, v)| match ty {
            FfiTy::Int => Value::Int(*v),
            FfiTy::Int32 => Value::Int(*v as i32 as i64),
            FfiTy::Float => Value::Float(f64::from_bits(*v as u64)),
            FfiTy::Float32 => Value::Float(f32::from_bits(*v as u32) as f64),
            FfiTy::Bool => Value::Bool(*v != 0),
            FfiTy::Str => {
                if *v == 0 {
                    Value::mk_str(String::new())
                } else {
                    Value::mk_str(unsafe { CStr::from_ptr(*v as *const c_char) }.to_string_lossy().into_owned())
                }
            }
            FfiTy::Ptr => Value::Ptr(*v as usize),
            other => unreachable!("callback prototype allows scalar params only, got {}", other.name()),
        })
        .collect();
    // 全新引擎实例执行回调体（快照环境）；panic 不得穿 C 栈
    let span = Span { line: 1, col: 1, len: 0 };
    let (lambda, fname, engine, file, src) = (
        ctx.lambda.clone(),
        ctx.fname.clone(),
        &ctx.engine,
        ctx.file.clone(),
        ctx.src.clone(),
    );
    let call_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match engine {
            CbEngine::Interp { fns, structs, enums, alias_map } => {
                let mut ip = crate::interp::Interp::new_cb_host(
                    &file,
                    &src,
                    fns.clone(),
                    structs.clone(),
                    enums.clone(),
                    alias_map.clone(),
                );
                if let Some(f) = &lambda {
                    ip.exec_lambda(f, args, span)
                } else if let Some(name) = &fname {
                    ip.call_fn(name, args, span)
                } else {
                    Err(ZError::plain(codes::INTERNAL, "internal: callback slot has no callable", None::<&str>))
                }
            }
            CbEngine::Vm(env) => {
                let mut vm = Vm::from_cb_env(env, &file, &src);
                if let Some(f) = &lambda {
                    vm.run_callback_lambda(f, args, span)
                } else if let Some(name) = &fname {
                    vm.run_callback_fn(name, args, span)
                } else {
                    Err(ZError::plain(codes::INTERNAL, "internal: callback slot has no callable", None::<&str>))
                }
            }
        }
    }));
    let value = match call_result {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            CB_ERR.with(|slot| *slot.borrow_mut() = Some(e));
            return 0;
        }
        Err(_) => {
            CB_ERR.with(|slot| {
                *slot.borrow_mut() = Some(ZError::plain(codes::INTERNAL, "FFI callback panicked", None::<&str>))
            });
            return 0;
        }
    };
    match ret.as_ref() {
        FfiTy::Int => match value {
            Value::Int(i) => i,
            other => {
                cb_ret_err("int", &other);
                0
            }
        },
        FfiTy::Int32 => match value {
            Value::Int(i) => i as i32 as i64,
            other => {
                cb_ret_err("int32", &other);
                0
            }
        },
        FfiTy::Float => match value {
            Value::Float(f) => f.to_bits() as i64,
            other => {
                cb_ret_err("float", &other);
                0
            }
        },
        FfiTy::Float32 => match value {
            Value::Float(f) => (f as f32).to_bits() as i64,
            other => {
                cb_ret_err("float32", &other);
                0
            }
        },
        FfiTy::Bool => match value {
            Value::Bool(b) => b as i64,
            other => {
                cb_ret_err("bool", &other);
                0
            }
        },
        FfiTy::Ptr => match value {
            Value::Ptr(p) => p as i64,
            Value::Int(i) => i,
            other => {
                cb_ret_err("ptr", &other);
                0
            }
        },
        FfiTy::Str => match value {
            Value::Str(s) => {
                // str 返回：转 C 字符串存入线程局部存储，返回指针（下一次回调前有效）
                let mut buf = CB_STRRET.with(|b| std::mem::take(&mut *b.borrow_mut()));
                buf.clear();
                buf.extend_from_slice(s.as_bytes());
                buf.push(0);
                let ptr = buf.as_ptr() as i64;
                CB_STRRET.with(|b| *b.borrow_mut() = buf);
                ptr
            }
            other => {
                cb_ret_err("str", &other);
                0
            }
        },
        FfiTy::Void | FfiTy::Callback { .. } => 0,
    }
}

/// 回调返回值类型不符：记录诊断（外层 FFI 调用收尾时上抛）。
fn cb_ret_err(want: &str, got: &Value) {
    CB_ERR.with(|e| {
        *e.borrow_mut() = Some(ZError::plain(
            codes::TYPE_MISMATCH,
            format!("callback must return `{}`, got `{}`", want, got.type_name()),
            Some("align the Hone function's return value with the declared callback prototype"),
        ))
    });
}

// libffi 回调 closure：C→Hone 方向统一入口。closure 回调收到原始 CIF/参数指针，
// 按原型类别逐个读取槽内值（float32 槽 4 字节、bool 槽 1 字节，其余标量 8 字节），
// 调 cb_dispatch 完成 Hone 侧执行；返回值按原型返回类别写 rvalue。
unsafe extern "C" fn cb_closure_invoke(
    _cif: *mut ffi_cif,
    ret: *mut c_void,
    args: *mut *mut c_void,
    _user: *mut c_void,
) {
    // 原型来自窗口槽（closure 与窗口一一对应，参数类别与个数恒等）
    let Some(proto) = CB_CTX.with(|c| c.borrow().as_ref().map(|ctx| ctx.cb.clone())) else {
        return;
    };
    let FfiTy::Callback { params, ret: ret_ty } = &proto else {
        return;
    };
    // 按类别读槽：float32→u32 位模式、bool→u8、其余标量→u64（统一装进 i64 槽）
    let mut slots: Vec<i64> = Vec::with_capacity(params.len());
    for (i, ty) in params.iter().enumerate() {
        let p = *args.add(i) as *const c_void;
        if p.is_null() {
            slots.push(0);
            continue;
        }
        let v = match ty {
            FfiTy::Float32 => *(p as *const u32) as i64,
            FfiTy::Bool => *(p as *const u8) as i64,
            _ => *(p as *const u64) as i64,
        };
        slots.push(v);
    }
    let out = cb_dispatch(&slots);
    // cb_dispatch 返回「按位打包」的 i64（float 类别 = 位模式）；按原型返回类别落盘
    if ret.is_null() {
        return;
    }
    match ret_ty.as_ref() {
        FfiTy::Float => *(ret as *mut f64) = f64::from_bits(out as u64),
        FfiTy::Float32 => *(ret as *mut f32) = f32::from_bits(out as u32),
        FfiTy::Bool => *(ret as *mut u8) = out as u8,
        FfiTy::Int32 => *(ret as *mut i32) = out as i32,
        FfiTy::Void => {}
        _ => *(ret as *mut i64) = out,
    }
}

/// 按回调原型构造 libffi closure（工业级回调 trampoline）：
/// 返回 (传给 C 的函数指针地址, closure 可写指针, CIF 堆指针, atypes 堆指针)。
/// CIF 与参数类型表**必须**在 closure 存活期内有效（libffi trampoline 展开
/// 参数布局时读它们）——两者均 Box::leak 进堆，窗口收尾统一释放。
/// 窗口收尾：`ffi_closure_free(w)` + `drop(Box::from_raw(cif))` + `drop(Box::from_raw(atypes))`。
unsafe fn cb_closure_new(proto: &FfiTy) -> Result<(usize, *mut c_void, *mut ffi_cif, *mut *mut ffi_type), ZError> {
    let FfiTy::Callback { params, ret } = proto else {
        return Err(ZError::plain(codes::INTERNAL, "internal: not a callback prototype", None::<&str>));
    };
    let cif = Box::into_raw(Box::new(std::mem::zeroed::<ffi_cif>()));
    let atypes = Box::into_raw(
        params.iter().map(ffi_cb_type_of).collect::<Vec<*mut ffi_type>>().into_boxed_slice(),
    ) as *mut *mut ffi_type;
    let rtype = ffi_cb_ret_type_of(ret);
    let st = ffi_prep_cif(
        cif,
        ffi_abi_FFI_DEFAULT_ABI,
        params.len() as u32,
        rtype,
        atypes,
    );
    if st != ffi_status_FFI_OK {
        drop(Box::from_raw(cif));
        drop(Box::from_raw(std::slice::from_raw_parts_mut(atypes, params.len())));
        return Err(ZError::plain(
            codes::INTERNAL,
            format!("libffi: ffi_prep_cif failed with status {st}"),
            None::<&str>,
        ));
    }
    // closure_alloc 返回可写区（存 ffi_closure），code 侧写出可调用地址（trampoline 机器码）
    let mut code: *mut c_void = null_mut();
    let writable = ffi_closure_alloc(std::mem::size_of::<ffi_closure>(), &mut code);
    if writable.is_null() || code.is_null() {
        drop(Box::from_raw(cif));
        drop(Box::from_raw(std::slice::from_raw_parts_mut(atypes, params.len())));
        return Err(ZError::plain(
            codes::INTERNAL,
            "libffi: ffi_closure_alloc failed",
            Some("closure allocation requires an executable-code-capable allocator"),
        ));
    }
    let closure = writable as *mut ffi_closure;
    let st = ffi_prep_closure_loc(closure, cif, Some(cb_closure_invoke), writable, code);
    if st != ffi_status_FFI_OK {
        ffi_closure_free(writable);
        drop(Box::from_raw(cif));
        drop(Box::from_raw(std::slice::from_raw_parts_mut(atypes, params.len())));
        return Err(ZError::plain(
            codes::INTERNAL,
            format!("libffi: ffi_prep_closure_loc failed with status {st}"),
            None::<&str>,
        ));
    }
    Ok((code as usize, writable, cif, atypes))
}

/// 释放一次回调窗口的全部 libffi 分配（closure + CIF + 参数类型表）。
unsafe fn cb_closure_drop(writable: *mut c_void, cif: *mut ffi_cif, atypes: *mut *mut ffi_type, n: usize) {
    ffi_closure_free(writable);
    drop(Box::from_raw(cif));
    drop(Box::from_raw(std::slice::from_raw_parts_mut(atypes, n)));
}

/// 调用签名块/头文件声明的 FFI 函数：按签名将 Hone 参数转换为 C ABI 值，调用后转换返回值。
#[allow(clippy::too_many_arguments)]
fn call_ffi_typed(
    st: &FfiState,
    sig: &FfiSig,
    lib_name: &str,
    func_name: &str,
    args: Vec<Value>,
    span: Span,
    file: &str,
    src: &str,
    host: &dyn Fn() -> CbEngine,
) -> Result<Value, ZError> {
    // 头文件解析失败的原型（回调/变参/数组等）：调用时直接报错
    if let Some(reason) = sig.unsupported {
        return Err(rerr(
            codes::NOT_IMPLEMENTED,
            format!("`{}` cannot be called: {}", func_name, reason),
            span,
            Some("declare a manual signature for this function, or use `ptr` for the unsupported parts"),
            file,
            src,
        ));
    }
    // 参数个数：定长要求精确相等；变参要求至少覆盖全部具名参数
    if args.len() < sig.params.len() || (!sig.variadic && args.len() != sig.params.len()) {
        let expected = if sig.variadic {
            format!("at least {}", sig.params.len())
        } else {
            format!("{}", sig.params.len())
        };
        return Err(rerr(
            codes::DLL_ARG,
            format!("`{}` expects {} arguments, got {}", func_name, expected, args.len()),
            span,
            Some(format!(
                "declared signature: `fn {}({}{}) -> {}`",
                sig.name,
                sig.params.iter().map(|p| p.ty.name()).collect::<Vec<_>>().join(", "),
                if sig.variadic { ", ..." } else { "" },
                sig.ret.name()
            )),
            file,
            src,
        ));
    }
    // 参数转换：str 参数需 CString 保持存活直到调用结束
    let mut cargs: Vec<CArg> = Vec::with_capacity(args.len());
    let mut cstrings: Vec<CString> = Vec::new();
    // 回调窗口是否已安装（调用后清理）；closure 分配记录（writable, cif, atypes, n）
    let mut cb_installed = false;
    let mut cb_allocs: Vec<(*mut c_void, *mut ffi_cif, *mut *mut ffi_type, usize)> = Vec::new();
    for (i, a) in args.iter().enumerate() {
        if i >= sig.params.len() {
            // 变参实参（ctypes 式）：按运行时类型转换
            match a {
                Value::Int(v) => cargs.push(CArg::I(*v)),
                Value::Bool(b) => cargs.push(CArg::I(if *b { 1 } else { 0 })),
                Value::Ptr(p) => cargs.push(CArg::I(*p as i64)),
                Value::Float(v) => cargs.push(CArg::F(*v)),
                Value::Str(s) => {
                    let cs = CString::new(s.as_bytes()).map_err(|_| {
                        rerr(
                            codes::TYPE_MISMATCH,
                            format!("`{}` variadic argument {} contains a NUL byte", func_name, i + 1),
                            span,
                            Some("C strings cannot contain embedded NUL characters"),
                            file,
                            src,
                        )
                    })?;
                    let ptr = cs.as_ptr() as i64;
                    cstrings.push(cs);
                    cargs.push(CArg::I(ptr));
                }
                other => {
                    return Err(rerr(
                        codes::TYPE_MISMATCH,
                        format!("`{}` variadic argument must be int/float/bool/str/ptr, got `{}`", func_name, other.type_name()),
                        span,
                        Some("variadic arguments are converted by their runtime type (ctypes-style)"),
                        file,
                        src,
                    ))
                }
            }
            continue;
        }
        let p = &sig.params[i];
        match &p.ty {
            FfiTy::Int => match a {
                Value::Int(v) => cargs.push(CArg::I(*v)),
                other => {
                    return Err(rerr(
                        codes::TYPE_MISMATCH,
                        format!("`{}` parameter `{}` expects `int`, got `{}`", func_name, p.name, other.type_name()),
                        span,
                        Some("the declared FFI signature maps `int` to int64"),
                        file,
                        src,
                    ))
                }
            },
            FfiTy::Int32 => match a {
                Value::Int(v) if *v >= i32::MIN as i64 && *v <= i32::MAX as i64 => cargs.push(CArg::I32(*v as i32)),
                Value::Int(v) => {
                    return Err(rerr(
                        codes::TYPE_MISMATCH,
                        format!("`{}` parameter `{}` expects int32 range, got {}", func_name, p.name, v),
                        span,
                        Some("the declared FFI signature maps `int32` to a 32-bit C integer"),
                        file,
                        src,
                    ))
                }
                other => {
                    return Err(rerr(
                        codes::TYPE_MISMATCH,
                        format!("`{}` parameter `{}` expects `int`, got `{}`", func_name, p.name, other.type_name()),
                        span,
                        Some("the declared FFI signature maps `int32` to a 32-bit C integer"),
                        file,
                        src,
                    ))
                }
            },
            FfiTy::Float => match a {
                Value::Float(v) => cargs.push(CArg::F(*v)),
                other => {
                    return Err(rerr(
                        codes::TYPE_MISMATCH,
                        format!("`{}` parameter `{}` expects `float`, got `{}`", func_name, p.name, other.type_name()),
                        span,
                        Some("the declared FFI signature maps `float` to double"),
                        file,
                        src,
                    ))
                }
            },
            FfiTy::Float32 => match a {
                Value::Float(v) if *v >= f32::MIN as f64 && *v <= f32::MAX as f64 => cargs.push(CArg::F32(*v as f32)),
                Value::Float(v) => {
                    return Err(rerr(
                        codes::TYPE_MISMATCH,
                        format!("`{}` parameter `{}` expects float32 range, got {}", func_name, p.name, v),
                        span,
                        Some("the declared FFI signature maps `float32` to a 32-bit C float"),
                        file,
                        src,
                    ))
                }
                other => {
                    return Err(rerr(
                        codes::TYPE_MISMATCH,
                        format!("`{}` parameter `{}` expects `float`, got `{}`", func_name, p.name, other.type_name()),
                        span,
                        Some("the declared FFI signature maps `float32` to a 32-bit C float"),
                        file,
                        src,
                    ))
                }
            },
            FfiTy::Bool => match a {
                Value::Bool(b) => cargs.push(CArg::I(if *b { 1 } else { 0 })),
                other => {
                    return Err(rerr(
                        codes::TYPE_MISMATCH,
                        format!("`{}` parameter `{}` expects `bool`, got `{}`", func_name, p.name, other.type_name()),
                        span,
                        Some("the declared FFI signature maps `bool` to a C boolean"),
                        file,
                        src,
                    ))
                }
            },
            FfiTy::Str => match a.as_plain() {
                Value::Str(s) => {
                    let cs = CString::new(s.as_bytes()).map_err(|_| {
                        rerr(
                            codes::TYPE_MISMATCH,
                            format!("`{}` parameter `{}` contains a NUL byte", func_name, p.name),
                            span,
                            Some("C strings cannot contain embedded NUL characters"),
                            file,
                            src,
                        )
                    })?;
                    let ptr = cs.as_ptr() as i64;
                    cstrings.push(cs);
                    cargs.push(CArg::I(ptr));
                }
                other => {
                    return Err(rerr(
                        codes::TYPE_MISMATCH,
                        format!("`{}` parameter `{}` expects `str`, got `{}`", func_name, p.name, other.type_name()),
                        span,
                        Some("the declared FFI signature maps `str` to `const char*`"),
                        file,
                        src,
                    ))
                }
            },
            FfiTy::Ptr => match a {
                Value::Ptr(p) => cargs.push(CArg::I(*p as i64)),
                Value::Int(0) => cargs.push(CArg::I(0)), // 0 作为 NULL
                other => {
                    return Err(rerr(
                        codes::TYPE_MISMATCH,
                        format!("`{}` parameter `{}` expects `ptr`, got `{}`", func_name, p.name, other.type_name()),
                        span,
                        Some("pass a `ptr` value (e.g. from another FFI call) or `0` for NULL"),
                        file,
                        src,
                    ))
                }
            },
            FfiTy::Callback { params: cbp, .. } => {
                // 回调实参：Hone 可调用（lambda 或顶层函数名）→ libffi closure 地址。
                // 同步回调：指针仅在本调用窗口内有效（C 必须在返回前回调，可多次回调）。
                let (lambda, fname) = match a {
                    Value::Lambda(l) => (Some(l.clone()), None),
                    Value::Str(fname) => {
                        let engine = host();
                        if !engine.has_fn(fname) {
                            return Err(rerr(
                                codes::UNDEFINED,
                                format!("callback function `{}` is not defined", fname),
                                span,
                                Some("pass a lambda `fn(a, b) { ... }` or a top-level function name"),
                                file,
                                src,
                            ));
                        }
                        (None, Some(fname.to_string()))
                    }
                    other => {
                        return Err(rerr(
                            codes::TYPE_MISMATCH,
                            format!("`{}` parameter `{}` expects a callback (lambda or function name), got `{}`", func_name, p.name, other.type_name()),
                            span,
                            Some("pass a lambda `fn(a, b) { ... }` or a top-level function name"),
                            file,
                            src,
                        ))
                    }
                };
                // 构造 libffi closure（CIF 按原型：标量参数/返回全支持，上限 8 参）
                if cbp.len() > 8 {
                    return Err(rerr(
                        codes::DLL_ARG,
                        format!("`{}` parameter `{}`: callbacks support at most 8 parameters", func_name, p.name),
                        span,
                        Some("reduce the callback parameter count, or wrap it on the C side"),
                        file,
                        src,
                    ));
                }
                let (code_ptr, writable, cif, atypes) =
                    unsafe { cb_closure_new(&p.ty) }.map_err(|e| {
                        ZError::plain(e.code, format!("`{}` parameter `{}`: {}", func_name, p.name, e.msg), e.help)
                    })?;
                cb_allocs.push((writable, cif, atypes, cbp.len()));
                // 安装回调窗口：原型 + 可调用 + 引擎快照（回调在全新引擎实例中执行）
                CB_CTX.with(|c| {
                    *c.borrow_mut() = Some(Arc::new(CbCtx {
                        cb: p.ty.clone(),
                        lambda,
                        fname,
                        engine: host(),
                        file: Arc::new(file.to_string()),
                        src: Arc::new(src.to_string()),
                    }))
                });
                cb_installed = true;
                cargs.push(CArg::I(code_ptr as i64));
            }
            FfiTy::Void => unreachable!("void is not a parameter type"),
        }
    }
    // libffi CIF 调用：固定参数走 ffi_prep_cif；变参走 ffi_prep_cif_var
    //（nfixedargs = 具名参数数，其余实参按 C 默认实参提升规则声明类型）。
    let ret_ty = match &sig.ret {
        FfiTy::Void => addr_of_mut!(libffi_sys::ffi_type_void),
        FfiTy::Float => addr_of_mut!(libffi_sys::ffi_type_double),
        FfiTy::Float32 => addr_of_mut!(libffi_sys::ffi_type_float),
        _ => addr_of_mut!(libffi_sys::ffi_type_sint64),
    };
    let ntotal = cargs.len();
    let nfixed = sig.params.len();
    let mut atypes: Vec<*mut ffi_type> = Vec::with_capacity(ntotal);
    for (i, c) in cargs.iter().enumerate() {
        if i < nfixed {
            atypes.push(ffi_type_of(&sig.params[i].ty));
        } else {
            atypes.push(vararg_ffi_type(c));
        }
    }
    let mut cif: ffi_cif = unsafe { std::mem::zeroed() };
    let st_cif = if sig.variadic {
        unsafe {
            ffi_prep_cif_var(&mut cif, ffi_abi_FFI_DEFAULT_ABI, nfixed as u32, ntotal as u32, ret_ty, atypes.as_mut_ptr())
        }
    } else {
        unsafe { ffi_prep_cif(&mut cif, ffi_abi_FFI_DEFAULT_ABI, ntotal as u32, ret_ty, atypes.as_mut_ptr()) }
    };
    if st_cif != ffi_status_FFI_OK {
        return Err(rerr(
            codes::INTERNAL,
            format!("libffi: cif preparation failed with status {}", st_cif),
            span,
            Some("this is an internal error; please report it"),
            file,
            src,
        ));
    }
    // 实参槽（8 字节对齐 ≥ 全部标量）+ 指针数组；CIF 要求两者存活到 ffi_call 返回
    let mut slots: Vec<u64> = vec![0u64; ntotal.max(1)];
    for (slot, c) in slots.iter_mut().zip(cargs.iter()) {
        write_slot(slot, c);
    }
    let aptrs: Vec<*mut c_void> = slots.iter_mut().map(|s| s as *mut u64 as *mut c_void).collect();
    let mut rslot: u64 = 0;
    let lib = st.libs.get(lib_name).unwrap();
    let sym: libloading::Symbol<unsafe extern "C" fn()> = unsafe {
        lib.get(func_name.as_bytes())
    }.map_err(|e| {
        rerr(
            codes::NOT_FOUND,
            format!("symbol `{}` not found in library `{}`: {}", func_name, lib_name, e),
            span,
            Some("check the exported symbol name (e.g. `#[no_mangle] pub extern \"C\" fn`)"),
            file,
            src,
        )
    })?;
    unsafe {
        ffi_call(&mut cif, Some(*sym), &mut rslot as *mut u64 as *mut c_void, aptrs.as_ptr() as *mut *mut c_void);
    }
    // 按返回类别解读槽：float 类别 = 位模式，整数类别 = 数值
    let cret = match &sig.ret {
        FfiTy::Float => CRet::F(f64::from_bits(rslot)),
        FfiTy::Float32 => CRet::F(f32::from_bits(rslot as u32) as f64),
        _ => CRet::I(rslot as i64),
    };
    // 回调窗口收尾：清理槽位；回调体内的错误原样上抛（保留 Hone 侧定位）
    if cb_installed {
        CB_CTX.with(|c| *c.borrow_mut() = None);
        // libffi 分配统一释放（closure + 堆上的 CIF/atypes）
        for (w, cf, at, n) in cb_allocs.drain(..) {
            unsafe { cb_closure_drop(w, cf, at, n) };
        }
        if let Some(e) = CB_ERR.with(|e| e.borrow_mut().take()) {
            return Err(e);
        }
    } else {
        // 调用中途失败（早退路径）也必须释放已分配的 closure
        debug_assert!(cb_allocs.is_empty() || std::thread::panicking());
    }
    // cstrings 在此作用域内保持存活，调用完成后再释放
    Ok(match sig.ret {
        FfiTy::Int => Value::Int(match cret {
            CRet::I(v) => v,
            CRet::F(_) => unreachable!("return class mismatch"),
        }),
        FfiTy::Int32 => Value::Int(match cret {
            CRet::I(v) => v as i32 as i64,
            CRet::F(_) => unreachable!("return class mismatch"),
        }),
        FfiTy::Float => Value::Float(match cret {
            CRet::F(v) => v,
            CRet::I(_) => unreachable!("return class mismatch"),
        }),
        FfiTy::Float32 => Value::Float(match cret {
            CRet::F(v) => v,
            CRet::I(_) => unreachable!("return class mismatch"),
        }),
        FfiTy::Bool => Value::Bool(match cret {
            CRet::I(v) => v != 0,
            CRet::F(_) => unreachable!("return class mismatch"),
        }),
        FfiTy::Str => {
            let p = match cret {
                CRet::I(v) => v,
                CRet::F(_) => unreachable!("return class mismatch"),
            };
            if p == 0 {
                return Err(rerr(
                    codes::TYPE_MISMATCH,
                    format!("`{}` returned NULL where `str` was expected", func_name),
                    span,
                    Some("the C function returned a null `const char*`"),
                    file,
                    src,
                ));
            }
            let s = unsafe { CStr::from_ptr(p as *const c_char) };
            Value::mk_str(s.to_string_lossy().into_owned())
        }
        FfiTy::Ptr => Value::Ptr(match cret {
            CRet::I(v) => v as usize,
            CRet::F(_) => unreachable!("return class mismatch"),
        }),
        // C 返回函数指针（回调原型）：按不透明指针值带回
        FfiTy::Callback { .. } => Value::Ptr(match cret {
            CRet::I(v) => v as usize,
            CRet::F(_) => unreachable!("return class mismatch"),
        }),
        FfiTy::Void => Value::Null,
    })
}

// ───────────────────────────── load 描述（IR 往返） ─────────────────────────────

/// `load` 语句的完整描述：VM 把它编译为 `Instr::LoadLib`，运行期调用 [`load`]。
/// 文本 IR 内联序列化（见 [`load_desc_text`] / [`parse_load_desc`]），可 1:1 还原。
#[derive(Debug, Clone)]
pub struct LoadDesc {
    pub lazy: bool,
    pub path: String,
    pub alias: Option<String>,
    pub from: Option<String>,
    pub sigs: Vec<FfiSig>,
}

/// FfiTy 文本（IR 用）：与 `FfiTy::name` 同名，回调原型写成 `fn(...) -> ret`。
fn ffi_ty_text(t: &FfiTy) -> String {
    match t {
        FfiTy::Callback { params, ret } => {
            let ps: Vec<String> = params.iter().map(ffi_ty_text).collect();
            format!("fn({}) -> {}", ps.join(","), ffi_ty_text(ret))
        }
        other => other.name().to_string(),
    }
}

/// 解析 ffi_ty_text 产物（递归下降，处理嵌套回调原型）。
fn parse_ffi_ty(s: &str, pos: &mut usize) -> Result<FfiTy, String> {
    let rest = &s[*pos..];
    if rest.starts_with("fn(") {
        *pos += 3;
        let mut params = Vec::new();
        if s[*pos..].starts_with(')') {
            *pos += 1;
        } else {
            loop {
                params.push(parse_ffi_ty(s, pos)?);
                if s[*pos..].starts_with(',') {
                    *pos += 1;
                } else if s[*pos..].starts_with(')') {
                    *pos += 1;
                    break;
                } else {
                    return Err(format!("回调原型缺少 `,` 或 `)`: {}", &s[*pos..]));
                }
            }
        }
        if !s[*pos..].starts_with(" -> ") {
            return Err(format!("回调原型缺少 ` -> `: {}", &s[*pos..]));
        }
        *pos += 4;
        let ret = parse_ffi_ty(s, pos)?;
        return Ok(FfiTy::Callback { params, ret: Box::new(ret) });
    }
    for name in ["float32", "int32", "float", "bool", "void", "ptr", "str", "int"] {
        if let Some(tail) = rest.strip_prefix(name) {
            // 名字边界：后随字符不能是标识符字符
            let ok = match tail.chars().next() {
                None => true,
                Some(c) => !(c.is_alphanumeric() || c == '_'),
            };
            if ok {
                *pos += name.len();
                return Ok(match name {
                    "int" => FfiTy::Int,
                    "int32" => FfiTy::Int32,
                    "float" => FfiTy::Float,
                    "float32" => FfiTy::Float32,
                    "bool" => FfiTy::Bool,
                    "str" => FfiTy::Str,
                    "ptr" => FfiTy::Ptr,
                    "void" => FfiTy::Void,
                    _ => unreachable!(),
                });
            }
        }
    }
    Err(format!("未知 FFI 类型: {}", rest))
}

/// 单条签名文本：`name(pname:ty, ...) -> ret`（变参以 `...` 结尾；unsupported 附注原因）。
fn sig_text(sig: &FfiSig) -> String {
    let mut ps: Vec<String> = sig
        .params
        .iter()
        .map(|p| format!("{}:{}", p.name, ffi_ty_text(&p.ty)))
        .collect();
    if sig.variadic {
        ps.push("...".to_string());
    }
    let mut out = format!("{}({}) -> {}", sig.name, ps.join(","), ffi_ty_text(&sig.ret));
    if let Some(reason) = sig.unsupported {
        out.push_str(&format!("@unsupported={}", reason));
    }
    out
}

/// `Instr::LoadLib` 的文本形态。
pub fn load_desc_text(d: &LoadDesc) -> String {
    let sigs: Vec<String> = d.sigs.iter().map(sig_text).collect();
    format!(
        "lazy={} alias={:?} path={:?} from={:?} sigs={}",
        if d.lazy { "true" } else { "false" },
        d.alias.as_deref().unwrap_or(""),
        d.path,
        d.from.as_deref().unwrap_or(""),
        sigs.join("|")
    )
}

/// 以顶层分隔符切分（括号内的分隔符不参与切分）。
fn split_top(s: &str, sep: char) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ if c == sep && depth == 0 => {
                out.push(&s[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// 找到顶层（括号深度 0）的 ` -> ` 位置。
fn find_arrow(s: &str) -> Option<usize> {
    let mut depth = 0usize;
    let b = s.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b' ' if depth == 0 && b[i..].starts_with(b" -> ") => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

/// 解析每条签名（与 [`sig_text`] 互逆）。
fn parse_sig(t: &str) -> Result<FfiSig, String> {
    let (body, unsupported) = match t.split_once("@unsupported=") {
        Some((b, reason)) => (
            b,
            // 头文件解析产出的原因都是 `&'static str`；IR 文本无法携回 'static，
            // 此处泄漏一份（assemble 为一次性 CLI 路径，泄漏量 = 每签名一条短串）。
            Some(Box::leak(reason.to_string().into_boxed_str()) as &'static str),
        ),
        None => (t, None),
    };
    let arrow = find_arrow(body).ok_or_else(|| format!("签名缺少顶层 ` -> `: {}", body))?;
    let head = &body[..arrow];
    let ret_s = &body[arrow + 4..];
    let (name, plist) = head
        .split_once('(')
        .ok_or_else(|| format!("签名缺少 `(`: {}", head))?;
    let plist = plist
        .strip_suffix(')')
        .ok_or_else(|| format!("签名缺少 `)`: {}", head))?;
    let mut params: Vec<FfiParam> = Vec::new();
    let mut variadic = false;
    if !plist.is_empty() {
        for part in split_top(plist, ',') {
            if part == "..." {
                variadic = true;
                continue;
            }
            let (pn, pt) = part
                .split_once(':')
                .ok_or_else(|| format!("形参缺少 `:`: {}", part))?;
            let mut pos = 0usize;
            let ty = parse_ffi_ty(pt, &mut pos)?;
            if pos != pt.len() {
                return Err(format!("形参类型尾部多余字符: {}", pt));
            }
            params.push(FfiParam { name: pn.to_string(), ty });
        }
    }
    let mut pos = 0usize;
    let ret = parse_ffi_ty(ret_s, &mut pos)?;
    if pos != ret_s.len() {
        return Err(format!("返回类型尾部多余字符: {}", ret_s));
    }
    Ok(FfiSig { name: name.to_string(), params, ret, variadic, unsupported })
}

/// 解析 `Instr::LoadLib` 的文本形态（与 [`load_desc_text`] 互逆）。
pub fn parse_load_desc(s: &str) -> Result<LoadDesc, String> {
    let rest = s
        .strip_prefix("lazy=")
        .ok_or_else(|| format!("load 描述缺少 `lazy=`: {}", s))?;
    let (lazy_s, rest) = rest.split_once(' ').ok_or_else(|| format!("load 描述缺字段: {}", s))?;
    let lazy = match lazy_s {
        "true" => true,
        "false" => false,
        other => return Err(format!("lazy 取值非法: {}", other)),
    };
    // sigs 恒为最后一个字段，且其值可含空格（`fn(int) -> int`）→ 就地切出余下全文。
    let (head, sigs_text) = rest
        .split_once(" sigs=")
        .ok_or_else(|| format!("load 描述缺少 `sigs=`: {}", s))?;
    let mut path = String::new();
    let mut alias: Option<String> = None;
    let mut from: Option<String> = None;
    for field in split_fields(head) {
        let (k, v) = field
            .split_once('=')
            .ok_or_else(|| format!("load 字段缺少 `=`: {}", field))?;
        match k {
            "alias" => alias = unquote_opt(v),
            "path" => path = unquote_opt(v).unwrap_or_default(),
            "from" => from = unquote_opt(v),
            other => return Err(format!("未知 load 字段: {}", other)),
        }
    }
    let mut sigs: Vec<FfiSig> = Vec::new();
    if !sigs_text.is_empty() {
        for one in split_sigs(sigs_text) {
            sigs.push(parse_sig(one)?);
        }
    }
    Ok(LoadDesc { lazy, path, alias, from, sigs })
}

/// 以空格切分 `k=v` 字段，但尊重 `{:?}` 引号内的空格与转义。
fn split_fields(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut start = 0usize;
    let mut in_str = false;
    let mut esc = false;
    for (i, &b) in bytes.iter().enumerate() {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        if b == b'"' {
            in_str = true;
        } else if b == b' ' {
            if start < i {
                out.push(&s[start..i]);
            }
            start = i + 1;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// 以 `|` 切分签名列表（签名文本内不会出现 `|`）。
fn split_sigs(s: &str) -> Vec<&str> {
    s.split('|').filter(|x| !x.is_empty()).collect()
}

/// `{:?}` 形式的字符串字面量 → 原值（空串视为「无」）。
fn unquote_opt(v: &str) -> Option<String> {
    if v.len() < 2 || !v.starts_with('"') {
        return None;
    }
    let inner = &v[1..v.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut esc = false;
    for c in inner.chars() {
        if esc {
            out.push(match c {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                '0' => '\0',
                '\\' => '\\',
                '"' => '"',
                other => other,
            });
            esc = false;
        } else if c == '\\' {
            esc = true;
        } else {
            out.push(c);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}
