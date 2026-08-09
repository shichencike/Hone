// error.rs - Hone 错误报告模块
// 格式：error[Hxxx]: 描述信息
//        --> 文件名.hn:行号:列号
//        行号 | 代码片段
//        |    ^^^^ 错误标记
//        help: 建议修复方案

use std::fmt;

/// Hone 错误。code 为 Hxxx 错误码，msg 为纯英文描述。
#[derive(Debug, Clone)]
pub struct ZError {
    pub code: &'static str,
    pub msg: String,
    pub file: String,
    pub line: usize,
    pub col: usize,
    pub len: usize,
    pub line_text: String,
    pub help: Option<String>,
}

impl ZError {
    /// 构造错误。line/col 为 1-based，len 为错误标记长度（字符数）。
    pub fn new(
        code: &'static str,
        msg: impl Into<String>,
        file: &str,
        src: &str,
        line: usize,
        col: usize,
        len: usize,
        help: Option<impl Into<String>>,
    ) -> Self {
        let line_text = src
            .lines()
            .nth(line.saturating_sub(1))
            .unwrap_or("")
            .to_string();
        ZError {
            code,
            msg: msg.into(),
            file: file.to_string(),
            line,
            col,
            len,
            line_text,
            help: help.map(Into::into),
        }
    }

    /// 无源码上下文时（如命令行参数错误）使用的构造方式。
    pub fn plain(code: &'static str, msg: impl Into<String>, help: Option<impl Into<String>>) -> Self {
        ZError {
            code,
            msg: msg.into(),
            file: String::new(),
            line: 0,
            col: 0,
            len: 0,
            line_text: String::new(),
            help: help.map(Into::into),
        }
    }
}

impl fmt::Display for ZError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            // 无定位信息（命令行错误）
            writeln!(f, "error[{}]: {}", self.code, self.msg)?;
            if let Some(h) = &self.help {
                writeln!(f, "help: {}", h)?;
            }
            return Ok(());
        }

        // Tab 展开为 4 空格，保持 caret 对齐
        let mut shown = String::new();
        let mut caret_col = 0usize;
        let chars: Vec<char> = self.line_text.chars().collect();
        for (i, c) in chars.iter().enumerate() {
            if i < self.col.saturating_sub(1) {
                if *c == '\t' {
                    shown.push_str("    ");
                    caret_col += 4;
                } else {
                    shown.push(*c);
                    caret_col += 1;
                }
            } else {
                shown.push(*c);
            }
        }

        let line_no = format!("{}", self.line);
        let pad = line_no.len();
        let caret_len = self.len.max(1).min(chars.len().saturating_sub(self.col.saturating_sub(1)).max(1));

        writeln!(f, "error[{}]: {}", self.code, self.msg)?;
        writeln!(f, "  --> {}:{}:{}", self.file, self.line, self.col)?;
        writeln!(f, "{:>pad$} | {}", line_no, shown, pad = pad)?;
        writeln!(f, "{} | {}{}", " ".repeat(pad), " ".repeat(caret_col), "^".repeat(caret_len))?;
        if let Some(h) = &self.help {
            writeln!(f, "help: {}", h)?;
        }
        Ok(())
    }
}

impl std::error::Error for ZError {}

/// 常用错误码（与设计规范 5.2 对齐，另补充部分编码）
pub mod codes {
    pub const TYPE_MISMATCH: &str = "H001"; // 类型冲突（期望 X，得到 Y）
    pub const UNDEFINED: &str = "H002"; // 未定义的变量或函数
    pub const CANNOT_INFER: &str = "H003"; // 无法自动推导类型，请添加显式类型
    pub const AMBIGUOUS_OP: &str = "H004"; // 运算符重载歧义
    pub const SYNTAX: &str = "H005"; // 语法错误
    pub const STR_TO_INT: &str = "H006"; // 字符串转整数失败
    pub const STR_TO_FLOAT: &str = "H007"; // 字符串转浮点数失败
    pub const COND_NOT_BOOL: &str = "H008"; // 条件表达式必须是 bool
    pub const DIV_ZERO: &str = "H009"; // 除零错误
    pub const INTEGER_OVERFLOW: &str = "H010"; // 整数溢出
    pub const ARG_COUNT: &str = "H011"; // 参数数量不匹配
    pub const RECURSION_DEPTH: &str = "H012"; // 递归过深
    pub const SYSCALL: &str = "H300"; // 系统调用失败
    pub const NETWORK: &str = "H200"; // 网络请求失败（通用）
    pub const NOT_FOUND: &str = "H404"; // 文件或库不存在（通用）
    pub const NOT_IMPLEMENTED: &str = "H999"; // 尚未实现

    // --- H100 区段：词法/语法细分 ---
    pub const ILLEGAL_CHAR: &str = "H101"; // 非法字符
    pub const UNTERMINATED_STRING: &str = "H102"; // 字符串未闭合
    pub const UNTERMINATED_COMMENT: &str = "H103"; // 注释未闭合
    pub const MISSING_SEMI: &str = "H104"; // 语句缺少分号
    // --- H200 区段：网络细分 ---
    pub const NET_TIMEOUT: &str = "H201"; // 连接/请求超时
    pub const NET_CONN_REFUSED: &str = "H202"; // 连接被拒绝
    pub const NET_DNS: &str = "H203"; // DNS 解析失败
    pub const NET_HTTP_STATUS: &str = "H204"; // 非 2xx 响应
    // --- H300 区段：系统/DLL 细分 ---
    pub const DLL_LOAD: &str = "H301"; // DLL 加载失败
    pub const DLL_ARG: &str = "H302"; // DLL 参数校验失败
    pub const PERMISSION: &str = "H303"; // 权限不足
    pub const PTR_INVALID: &str = "H304"; // 野指针：未分配/已释放/空指针
    pub const PTR_OOB: &str = "H305"; // 指针越界访问
    // --- H400 区段：文件细分 ---
    pub const FILE_NOT_FOUND: &str = "H401"; // 文件不存在
    pub const FILE_PERMISSION: &str = "H402"; // 文件权限不足
    pub const FILE_LOCKED: &str = "H403"; // 文件被占用/锁定
    // --- H600 区段：主动抛出 ---
    pub const THROW: &str = "H600"; // throw 语句抛出的用户错误
    pub const ASSERT: &str = "H700"; // assert 断言失败（测试框架）
}

/// 错误码解释表（`hone explain <code>` 使用）。未知错误码返回 None。
pub fn explain(code: &str) -> Option<&'static str> {
    Some(match code {
        "H001" => "类型冲突：期望某种类型，实际得到另一种。Hone 为静态强类型，禁止隐式转换。\n  修复：显式转换（to_int / to_str / to_float），或修正变量声明类型；泛型同一类型参数处必须传同型（如 pick[T](a: T, b: T) 两次实参类型需一致）。",
        "H002" => "未定义的变量或函数。\n  修复：先声明变量，或确认函数名（含模块前缀，如 time.now）拼写正确；类方法必须经 `类.方法(...)` 调用（成员函数不进全局符号表，裸名调用会报此错）；泛型注解中的类型参数需先在 `fn name[T]` 中声明。",
        "H003" => "无法自动推导类型。\n  修复：为该变量添加显式类型注解，如 `x : int = ...`。",
        "H004" => "运算符重载歧义：同一运算符对两个操作数类型存在多种解释。\n  修复：为操作数添加显式类型，消除歧义。",
        "H005" => "语法错误：源码不符合 Hone 语法。\n  修复：检查尖括号指示的位置附近的语句结构；类定义中成员必须是 `fn` 函数；类型参数不可重复声明（如 `fn dup[T, T]`）；类名与 struct 名不可重复。",
        "H006" => "字符串转整数失败（to_int）。\n  修复：确认字符串内容为合法整数格式，如 \"42\"。",
        "H007" => "字符串转浮点数失败（to_float）。\n  修复：确认字符串内容为合法浮点格式，如 \"3.14\"。",
        "H008" => "条件表达式必须是 bool。Hone 禁止 if/while 使用整数等隐式真值。\n  修复：显式比较，如 `if (x != 0)`。",
        "H009" => "除零错误：整数除法或取模的除数为 0。\n  修复：在除法前检查除数是否为 0。",
        "H010" => "整数溢出：64 位有符号整数运算超出范围。\n  修复：检查运算是否可能超出 i64 范围，必要时改用浮点数。",
        "H011" => "参数数量不匹配：调用时实参个数与函数/内置函数签名不一致。\n  修复：按签名补齐或删减参数；struct 构造按字段顺序传参，类方法按声明参数传参。",
        "H012" => "递归过深：调用深度超过 5000 层上限，疑似无限递归。\n  修复：检查递归终止条件，或改为迭代实现。",
        "H101" => "非法字符：源码中出现 Hone 不支持的字符。\n  修复：检查该位置的字符，删除或替换为合法符号。",
        "H102" => "字符串未闭合：双引号字符串缺少结束引号。\n  修复：在字符串末尾补上 `\"`。",
        "H103" => "注释未闭合：`/*` 多行注释缺少 `*/`。\n  修复：在注释末尾补上 `*/`。",
        "H104" => "语句缺少分号：Hone 要求每条语句以 `;` 结束。\n  修复：在语句末尾补上 `;`。",
        "H200" => "网络请求失败（通用）：http_get / http_post / http.request / smtp.send / ws.request 未能完成请求。\n  修复：检查网络连通性、URL 与代理设置；可使用 try-catch 做重试或降级。",
        "H201" => "网络连接/请求超时：在规定时间内未收到响应（http.request 的 timeout 选项、ws.request 超时）。\n  修复：检查远端服务状态、增大超时，或稍后重试。",
        "H202" => "连接被拒绝：目标端口无服务监听或防火墙拦截。\n  修复：确认服务已启动、端口与地址正确。",
        "H203" => "DNS 解析失败：无法将主机名解析为 IP 地址。\n  修复：检查主机名拼写与 DNS 配置。",
        "H204" => "非 2xx 响应：HTTP 请求成功送达，但服务端返回了错误状态码（http_get / http_post / http.request）。\n  修复：检查 URL 与参数，根据具体状态码（404/500 等）处理。",
        "H300" => "系统调用失败（通用）：操作系统 API 返回错误。\n  修复：根据错误信息检查系统状态（注册表、剪贴板、文件扫描、临时目录等）。",
        "H301" => "DLL 加载失败：load 指定的动态库或 sqlite 库无法加载。\n  修复：确认库文件存在、路径正确、位数匹配（x64）；sqlite.* 需系统 libsqlite3（Linux 安装 libsqlite3，Windows 将 sqlite3.dll 放到 hone.exe 旁边）。",
        "H302" => "DLL 参数校验失败：调用动态库函数时参数不符合 C ABI 约定。\n  修复：确认参数数量不超过 8 个且均为整数/数值类型。",
        "H303" => "权限不足：操作被系统拒绝（文件、注册表等）。\n  修复：以管理员身份运行，或调整文件/注册表权限。",
        "H304" => "野指针：指针未分配、已释放（use-after-free）、重复释放（double-free）或为空。\n  修复：只读写/释放由 ptr.alloc 返回且尚未 ptr.free 的指针；外部 FFI 句柄由库管理。",
        "H305" => "指针越界访问：读写区间超出 ptr.alloc 分配的大小。\n  修复：用 ptr.size(p) 查询分配大小，检查偏移是否在 0..size 内。",
        "H401" => "文件不存在：读取或写入的目标文件不存在。\n  修复：确认路径拼写，或先调用 write_file / file_exists 处理。",
        "H402" => "文件权限不足：无权限读取或写入该文件。\n  修复：检查文件只读属性与运行用户权限。",
        "H403" => "文件被占用/锁定：文件正被其他进程独占（Windows 常见）。\n  修复：关闭占用进程，稍后重试；可用 try-catch 配合重试。",
        "H404" => "文件或库不存在（通用）：NOT_FOUND。\n  修复：检查路径与模块缓存（~/.hone/cache/）。",
        "H600" => "用户主动抛出的错误（throw）。\n  修复：查看 throw 携带的 message 说明，或由调用方 try-catch 处理。",
        "H700" => "assert 断言失败（测试框架）。\n  修复：检查断言条件与期望值；断言失败即测试用例不通过。",
        "H999" => "尚未实现的功能。\n  修复：该特性仍在规划中，请改用其他方式或等待新版本。",
        _ => return None,
    })
}
