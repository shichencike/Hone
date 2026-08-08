// bundle.rs - hone build --exe 打包与自释放启动器
//
// 文件布局: [hone.exe 字节][脚本名][脚本内容][56 字节定长尾部块]
// 尾部块(小端):
//   magic(8) hone_len(8) name_len(4) script_len(4)
//   ver_major(2) ver_minor(2) ver_patch(2) flags(2)
//   timestamp(8) hone_crc(8) script_crc(8)
//
// 运行时:检测自身尾部 magic → 将内嵌 hone.exe 与 script.hn 释放到 .hone_cache
//   (当前目录不可写则回退系统临时目录)→ 子进程执行 → 默认清理缓存。
//   普通 hone 启动时只读取自身尾部 56 字节做判断,开销可忽略。

use std::process::ExitCode;

use crate::builtins;
use crate::error::codes;
use crate::error::ZError;

const MAGIC: &[u8; 8] = b"KABND001";
const TAIL_LEN: usize = 56;

/// 打包 exe 中解析出的内嵌信息。
pub struct BundledInfo {
    pub hone: Vec<u8>,
    pub name: String,
    pub script: Vec<u8>,
    pub version: (u16, u16, u16),
    pub timestamp: u64,
}

/// FNV-1a 64 位哈希：校验嵌入数据完整性，零依赖。
fn fnv1a64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn rd_u64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}
fn rd_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}
fn rd_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(b[off..off + 2].try_into().unwrap())
}

/// 组装打包文件: [hone.exe][name][script][尾部块]。
pub fn build(
    hone: &[u8],
    script: &str,
    name: &str,
    version: (u16, u16, u16),
    timestamp: u64,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(hone.len() + name.len() + script.len() + TAIL_LEN);
    out.extend_from_slice(hone);
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(script.as_bytes());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(hone.len() as u64).to_le_bytes());
    out.extend_from_slice(&(name.len() as u32).to_le_bytes());
    out.extend_from_slice(&(script.len() as u32).to_le_bytes());
    out.extend_from_slice(&version.0.to_le_bytes());
    out.extend_from_slice(&version.1.to_le_bytes());
    out.extend_from_slice(&version.2.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // flags 保留
    out.extend_from_slice(&timestamp.to_le_bytes());
    out.extend_from_slice(&fnv1a64(hone).to_le_bytes());
    out.extend_from_slice(&fnv1a64(script.as_bytes()).to_le_bytes());
    out
}

/// 检测当前可执行文件是否携带打包数据。普通 hone 返回 Ok(None)。
/// 先读尾部 56 字节判断，命中后才读取整个文件。
pub fn detect() -> Result<Option<BundledInfo>, ZError> {
    use std::io::{Read, Seek, SeekFrom};

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return Ok(None),
    };
    let tail = {
        let f = match std::fs::File::open(&exe) {
            Ok(f) => f,
            Err(_) => return Ok(None),
        };
        let len = match f.metadata().map(|m| m.len()) {
            Ok(l) => l,
            Err(_) => return Ok(None),
        };
        if len < TAIL_LEN as u64 {
            return Ok(None);
        }
        let mut f = f;
        if f.seek(SeekFrom::Start(len - TAIL_LEN as u64)).is_err() {
            return Ok(None);
        }
        let mut buf = [0u8; TAIL_LEN];
        if f.read_exact(&mut buf).is_err() {
            return Ok(None);
        }
        buf
    };
    if &tail[0..8] != MAGIC {
        return Ok(None);
    }
    let hone_len = rd_u64(&tail, 8) as usize;
    let name_len = rd_u32(&tail, 16) as usize;
    let script_len = rd_u32(&tail, 20) as usize;

    let data = match std::fs::read(&exe) {
        Ok(d) => d,
        Err(_) => return Ok(None),
    };
    if hone_len + name_len + script_len + TAIL_LEN != data.len() {
        return Ok(None); // 结构不符 → 视为普通 hone（损坏文件兜底）
    }
    let hone = &data[..hone_len];
    let name = String::from_utf8_lossy(&data[hone_len..hone_len + name_len]).into_owned();
    let script = &data[hone_len + name_len..hone_len + name_len + script_len];
    if rd_u64(&tail, 40) != fnv1a64(hone) || rd_u64(&tail, 48) != fnv1a64(script) {
        return Err(ZError::plain(
            codes::NOT_FOUND,
            "bundled executable is corrupted (checksum mismatch)",
            Some("rebuild it with `hone build --exe`"),
        ));
    }
    Ok(Some(BundledInfo {
        hone: hone.to_vec(),
        name,
        script: script.to_vec(),
        version: (rd_u16(&tail, 24), rd_u16(&tail, 26), rd_u16(&tail, 28)),
        timestamp: rd_u64(&tail, 32),
    }))
}

/// 打包 exe 的 --version 输出：文件名 (Hone 版本) built at 构建时间 + 源脚本。
pub fn show_version(info: &BundledInfo) {
    let exe_name = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
        .unwrap_or_else(|| info.name.clone());
    println!(
        "{} (Hone v{}.{}.{}) built at {}",
        exe_name,
        info.version.0,
        info.version.1,
        info.version.2,
        builtins::format_timestamp(info.timestamp as i64, "YYYY-MM-DD HH:mm:SS"),
    );
    println!("script: {}", info.name);
}

/// 打包模式入口：--version 打印版本信息；否则释放运行时执行脚本，默认清理缓存。
pub fn run(info: &BundledInfo, args: &[String]) -> ExitCode {
    if args.iter().any(|a| a == "--version" || a == "-V") {
        show_version(info);
        return ExitCode::SUCCESS;
    }
    let keep_cache = args.iter().any(|a| a == "--keep-cache");
    let script_args: Vec<String> = args.iter().filter(|a| *a != "--keep-cache").cloned().collect();

    // 缓存目录：优先当前目录 .hone_cache，不可写则回退系统临时目录
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let cache_dir = match create_cache_dir(&cwd.join(".hone_cache")) {
        Ok(d) => d,
        Err(_) => {
            let fallback = std::env::temp_dir().join(format!(
                "hone-bundle-{}",
                info.name.replace(['.', '\\', '/'], "_")
            ));
            match create_cache_dir(&fallback) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("[bundle] cannot create cache directory: {}", e);
                    return ExitCode::FAILURE;
                }
            }
        }
    };

    // 释放运行时与脚本（已存在且校验一致则跳过，避免重复写盘）
    let runtime_name = if cfg!(windows) { "hone.exe" } else { "hone" };
    let hone_path = cache_dir.join(runtime_name);
    if !cache_file_ok(&hone_path, &info.hone) {
        if let Err(e) = std::fs::write(&hone_path, &info.hone) {
            eprintln!("[bundle] cannot write runtime: {}", e);
            return ExitCode::FAILURE;
        }
    }
    let script_path = cache_dir.join(&info.name);
    if !cache_file_ok(&script_path, &info.script) {
        if let Err(e) = std::fs::write(&script_path, &info.script) {
            eprintln!("[bundle] cannot write script: {}", e);
            return ExitCode::FAILURE;
        }
    }
    hide_dir(&cache_dir);

    // 子进程执行：hone.exe script.hn [脚本参数...]，继承标准流，传播退出码
    let code = match std::process::Command::new(&hone_path)
        .arg(&script_path)
        .args(&script_args)
        .status()
    {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("[bundle] cannot run bundled hone: {}", e);
            1
        }
    };

    // 默认清理缓存（--keep-cache 保留以加速下次启动）
    if !keep_cache {
        let _ = std::fs::remove_dir_all(&cache_dir);
    }
    ExitCode::from(code as u8)
}

fn create_cache_dir(dir: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    std::fs::create_dir_all(dir)?;
    Ok(dir.to_path_buf())
}

/// 缓存文件是否已就位（存在且哈希一致）。
fn cache_file_ok(path: &std::path::Path, expect: &[u8]) -> bool {
    match std::fs::read(path) {
        Ok(bytes) => bytes.len() == expect.len() && fnv1a64(&bytes) == fnv1a64(expect),
        Err(_) => false,
    }
}

/// Windows 下给缓存目录设置隐藏属性。
#[cfg(windows)]
fn hide_dir(dir: &std::path::Path) {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = dir
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        winapi::um::fileapi::SetFileAttributesW(
            wide.as_ptr(),
            winapi::um::winnt::FILE_ATTRIBUTE_HIDDEN,
        );
    }
}

#[cfg(not(windows))]
fn hide_dir(_dir: &std::path::Path) {}
