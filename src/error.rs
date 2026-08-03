// error.rs - Zap 错误报告模块
// 格式：error[Zxxx]: 描述信息
//        --> 文件名.zp:行号:列号
//        行号 | 代码片段
//        |    ^^^^ 错误标记
//        help: 建议修复方案

use std::fmt;

/// Zap 错误。code 为 Zxxx 错误码，msg 为纯英文描述。
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
    pub const TYPE_MISMATCH: &str = "Z001"; // 类型冲突（期望 X，得到 Y）
    pub const UNDEFINED: &str = "Z002"; // 未定义的变量或函数
    pub const CANNOT_INFER: &str = "Z003"; // 无法自动推导类型，请添加显式类型
    pub const AMBIGUOUS_OP: &str = "Z004"; // 运算符重载歧义
    pub const SYNTAX: &str = "Z005"; // 语法错误
    pub const STR_TO_INT: &str = "Z006"; // 字符串转整数失败
    pub const STR_TO_FLOAT: &str = "Z007"; // 字符串转浮点数失败
    pub const COND_NOT_BOOL: &str = "Z008"; // 条件表达式必须是 bool
    pub const DIV_ZERO: &str = "Z009"; // 除零错误
    pub const INTEGER_OVERFLOW: &str = "Z010"; // 整数溢出
    pub const ARG_COUNT: &str = "Z011"; // 参数数量不匹配
    pub const RECURSION_DEPTH: &str = "Z012"; // 递归过深
    pub const SYSCALL: &str = "Z300"; // 系统调用失败
    pub const NETWORK: &str = "Z200"; // 网络请求失败
    pub const NOT_FOUND: &str = "Z404"; // 文件或库不存在
    pub const NOT_IMPLEMENTED: &str = "Z999"; // 尚未实现
}
