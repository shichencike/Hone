// lexer.rs - Hone 词法分析器
// 生成 token 流，每个 token 携带 Span（行:列 + 长度）用于精准报错。

use crate::error::ZError;

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    // 标识符与字面量
    Ident(String),
    IntLit(i64),
    FloatLit(f64),
    StrLit(String),
    /// 字符字面量 'a'：恰好一个 Unicode 字符（转义后）
    CharLit(char),
    /// 插值字符串 f"..."：携带已拆分的片段（文字段 / 代码段原始文本），由 parser 子解析代码段
    FStr(Vec<FStrPart>),
    /// 字节字面量 0b01000001：单个 byte（0-255，词法层校验最多 8 位）
    ByteLit(u8),
    /// 字节序列字面量 b"..."：bytes 类型（转义 \n \t \r \\ \" \xNN；非 ASCII 按 UTF-8 编码）
    BytesLit(Vec<u8>),
    // 关键字
    Fn,
    If,
    Else,
    While,
    Do,
    For,
    In,
    Return,
    True,
    False,
    Go,
    Try,
    Catch,
    Throw,
    Continue,
    // match 模式匹配
    Match,
    Break,
    Breakpoint,
    Load,
    Lazy,
    Use,
    Import,
    Alias,
    As,
    From,
    Tmp,
    // struct 结构体定义
    Struct,
    // class 类定义（成员函数不进入全局符号表）
    Class,
    // enum 枚举定义（简单变体 + 带载荷变体）
    Enum,
    // 异步：async fn（后台线程执行，返回 future）与 await（等待 future 结果）
    Async,
    Await,
    // 标签与跳转：`label NAME;` / `NAME:` 定义标签，`goto NAME;` 跳转
    Goto,
    // 宏定义：`macro NAME(参数) => 表达式;` / `macro NAME(参数) { 语句 }`
    Macro,
    // 上下文管理器：with expr [as r] { ... }（__enter__/__exit__ 协议）
    With,
    // type 实例类：type 名称 [extends 父类] { 字段; fn 方法(self, ...) {} }；new 构造；extends 继承
    Type,
    New,
    Extends,
    // 只读修饰符：readonly 变量 / 参数 / struct 字段（禁止重新赋值）
    Readonly,
    // 类型关键字
    TInt,
    TFloat,
    TBool,
    TStr,
    TChar,
    TByte,
    TBytes,
    // 运算符与符号
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    EqEq,
    NotEq,
    Lt,
    Le,
    Gt,
    Ge,
    AndAnd,
    OrOr,
    Pipe, // |>（管道操作符）
    Bang,
    Assign,
    /// 复合赋值：+= -= *= /= %=
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,
    /// 自增/自减：++ --
    PlusPlus,
    MinusMinus,
    /// 三元表达式 `?` 与空值合并 `??`
    Question,
    QuestionQuestion,
    /// 可选链 `?.`（obj 为 null 时短路返回 null）
    QuestionDot,
    /// 三引号原始字符串 """..."""（保留换行，不做转义处理）
    MultiStr(String),
    Colon,
    Arrow, // ->
    FatArrow, // =>（match 模式分支）
    Comma,
    Semi,
    Dot,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    At,
    Eof,
}

impl Tok {
    /// 用于报错信息的可读描述。
    pub fn describe(&self) -> String {
        match self {
            Tok::Ident(s) => format!("identifier `{}`", s),
            Tok::IntLit(v) => format!("integer `{}`", v),
            Tok::FloatLit(v) => format!("float `{}`", v),
            Tok::StrLit(_) => "string literal".to_string(),
            Tok::CharLit(_) => "char literal".to_string(),
            Tok::FStr(_) => "f-string literal".to_string(),
            Tok::Fn => "`fn`".into(),
            Tok::If => "`if`".into(),
            Tok::Else => "`else`".into(),
            Tok::While => "`while`".into(),
            Tok::Do => "`do`".into(),
            Tok::For => "`for`".into(),
            Tok::In => "`in`".into(),
            Tok::Return => "`return`".into(),
            Tok::True => "`true`".into(),
            Tok::False => "`false`".into(),
            Tok::Go => "`go`".into(),
            Tok::Try => "`try`".into(),
            Tok::Catch => "`catch`".into(),
            Tok::Throw => "`throw`".into(),
            Tok::Continue => "`continue`".into(),
            Tok::Match => "`match`".into(),
            Tok::Break => "`break`".into(),
            Tok::Breakpoint => "`breakpoint`".into(),
            Tok::Load => "`load`".into(),
            Tok::Lazy => "`lazy`".into(),
            Tok::Use => "`use`".into(),
            Tok::Import => "`import`".into(),
            Tok::Alias => "`alias`".into(),
            Tok::As => "`as`".into(),
            Tok::From => "`from`".into(),
            Tok::Tmp => "`tmp`".into(),
            Tok::Struct => "`struct`".into(),
            Tok::Class => "`class`".into(),
            Tok::Enum => "`enum`".into(),
            Tok::Async => "`async`".into(),
            Tok::Await => "`await`".into(),
            Tok::Goto => "`goto`".into(),
            Tok::Macro => "`macro`".into(),
            Tok::TInt => "type `int`".into(),
            Tok::TFloat => "type `float`".into(),
            Tok::TBool => "type `bool`".into(),
            Tok::TStr => "type `str`".into(),
            Tok::TChar => "type `char`".into(),
            Tok::TByte => "type `byte`".into(),
            Tok::TBytes => "type `bytes`".into(),
            Tok::With => "`with`".into(),
            Tok::Type => "`type`".into(),
            Tok::New => "`new`".into(),
            Tok::Extends => "`extends`".into(),
            Tok::Readonly => "`readonly`".into(),
            Tok::ByteLit(_) => "byte literal".to_string(),
            Tok::BytesLit(_) => "bytes literal".to_string(),
            Tok::Plus => "`+`".into(),
            Tok::Minus => "`-`".into(),
            Tok::Star => "`*`".into(),
            Tok::Slash => "`/`".into(),
            Tok::Percent => "`%`".into(),
            Tok::EqEq => "`==`".into(),
            Tok::NotEq => "`!=`".into(),
            Tok::Lt => "`<`".into(),
            Tok::Le => "`<=`".into(),
            Tok::Gt => "`>`".into(),
            Tok::Ge => "`>=`".into(),
            Tok::AndAnd => "`&&`".into(),
            Tok::OrOr => "`||`".into(),
            Tok::Pipe => "`|>`".into(),
            Tok::Bang => "`!`".into(),
            Tok::Assign => "`=`".into(),
            Tok::PlusEq => "`+=`".into(),
            Tok::MinusEq => "`-=`".into(),
            Tok::StarEq => "`*=`".into(),
            Tok::SlashEq => "`/=`".into(),
            Tok::PercentEq => "`%=`".into(),
            Tok::PlusPlus => "`++`".into(),
            Tok::MinusMinus => "`--`".into(),
            Tok::Question => "`?`".into(),
            Tok::QuestionQuestion => "`??`".into(),
            Tok::QuestionDot => "`?.`".into(),
            Tok::MultiStr(_) => "triple-quoted string".into(),
            Tok::Colon => "`:`".into(),
            Tok::Arrow => "`->`".into(),
            Tok::FatArrow => "`=>`".into(),
            Tok::Comma => "`,`".into(),
            Tok::Semi => "`;`".into(),
            Tok::Dot => "`.`".into(),
            Tok::LParen => "`(`".into(),
            Tok::RParen => "`)`".into(),
            Tok::LBrace => "`{`".into(),
            Tok::RBrace => "`}`".into(),
            Tok::LBracket => "`[`".into(),
            Tok::RBracket => "`]`".into(),
            Tok::At => "`@`".into(),
            Tok::Eof => "end of file".into(),
        }
    }
}

/// f"..." 插值字符串的片段：文字段或 {代码} 段（代码段保留原始文本，由 parser 子解析）。
#[derive(Debug, Clone, PartialEq)]
pub enum FStrPart {
    Lit(String),
    Code(String),
}

/// 源码位置：line/col 均为 1-based，len 为 token 长度（字符数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub line: usize,
    pub col: usize,
    pub len: usize,
}

pub struct Lexer {
    file: String,
    src: String,
    chars: Vec<char>,
    pos: usize,
    line: usize,
    col: usize,
}

impl Lexer {
    pub fn new(file: &str, src: &str) -> Self {
        Lexer {
            file: file.to_string(),
            src: src.to_string(),
            chars: src.chars().collect(),
            pos: 0,
            line: 1,
            col: 1,
        }
    }

    /// 将整个源码 token 化。失败时返回带精准定位的 ZError。
    pub fn tokenize(mut self) -> Result<Vec<(Tok, Span)>, ZError> {
        let mut out = Vec::new();
        loop {
            self.skip_ws_and_comments()?;
            let start_line = self.line;
            let start_col = self.col;
            let tok = self.next_token()?;
            let span = Span {
                line: start_line,
                col: start_col,
                len: self.len_since(start_line, start_col),
            };
            let eof = tok == Tok::Eof;
            out.push((tok, span));
            if eof {
                break;
            }
        }
        Ok(out)
    }

    fn len_since(&self, line: usize, col: usize) -> usize {
        if line == self.line {
            self.col.saturating_sub(col)
        } else {
            1
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek2(&self) -> Option<char> {
        self.chars.get(self.pos + 1).copied()
    }

    fn peek3(&self) -> Option<char> {
        self.chars.get(self.pos + 2).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.chars.get(self.pos).copied()?;
        self.pos += 1;
        if c == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(c)
    }

    fn err(&self, code: &'static str, msg: impl Into<String>, len: usize, help: Option<impl Into<String>>) -> ZError {
        ZError::new(code, msg, &self.file, &self.src, self.line, self.col, len.max(1), help)
    }

    fn skip_ws_and_comments(&mut self) -> Result<(), ZError> {
        loop {
            match self.peek() {
                Some(' ') | Some('\t') | Some('\r') => {
                    self.bump();
                }
                Some('\n') => {
                    self.bump();
                }
                Some('/') if self.peek2() == Some('/') => {
                    // 单行注释：跳过至行尾
                    while let Some(c) = self.peek() {
                        if c == '\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                Some('/') if self.peek2() == Some('*') => {
                    // 多行注释：不嵌套，未闭合时报错
                    self.bump();
                    self.bump();
                    let mut closed = false;
                    while let Some(c) = self.peek() {
                        if c == '*' && self.peek2() == Some('/') {
                            self.bump();
                            self.bump();
                            closed = true;
                            break;
                        }
                        self.bump();
                    }
                    if !closed {
                        return Err(self.err(
                            crate::error::codes::UNTERMINATED_COMMENT,
                            "unterminated block comment",
                            2,
                            Some("close the comment with `*/`"),
                        ));
                    }
                }
                _ => break,
            }
        }
        Ok(())
    }

    fn next_token(&mut self) -> Result<Tok, ZError> {
        let c = match self.peek() {
            None => return Ok(Tok::Eof),
            Some(c) => c,
        };

        // 标识符 / 关键字
        if c.is_ascii_alphabetic() || c == '_' {
            let mut s = String::new();
            while let Some(c) = self.peek() {
                if c.is_ascii_alphanumeric() || c == '_' {
                    s.push(c);
                    self.bump();
                } else {
                    break;
                }
            }
            return Ok(match s.as_str() {
                "fn" => Tok::Fn,
                "if" => Tok::If,
                "else" => Tok::Else,
                "while" => Tok::While,
                "do" => Tok::Do,
                "for" => Tok::For,
                "in" => Tok::In,
                "return" => Tok::Return,
                "true" => Tok::True,
                "false" => Tok::False,
                "go" => Tok::Go,
                "try" => Tok::Try,
                "catch" => Tok::Catch,
                "throw" => Tok::Throw,
                "continue" => Tok::Continue,
                "match" => Tok::Match,
                "break" => Tok::Break,
                "breakpoint" => Tok::Breakpoint,
                "load" => Tok::Load,
                "lazy" => Tok::Lazy,
                "use" => Tok::Use,
                "import" => Tok::Import,
                "alias" => Tok::Alias,
                "as" => Tok::As,
                "from" => Tok::From,
                "tmp" => Tok::Tmp,
                "struct" => Tok::Struct,
                "class" => Tok::Class,
                "enum" => Tok::Enum,
                "async" => Tok::Async,
                "await" => Tok::Await,
                "goto" => Tok::Goto,
                "macro" => Tok::Macro,
                "with" => Tok::With,
                "type" => Tok::Type,
                "new" => Tok::New,
                "extends" => Tok::Extends,
                "readonly" => Tok::Readonly,
                "int" => Tok::TInt,
                "float" => Tok::TFloat,
                "bool" => Tok::TBool,
                "str" => Tok::TStr,
                "char" => Tok::TChar,
                "byte" => Tok::TByte,
                "bytes" => Tok::TBytes,
                _ => {
                    // 标识符恰好为 `f` 且紧跟引号 → 插值字符串 f"..."
                    if s == "f" && self.peek() == Some('"') {
                        return self.lex_fstring();
                    }
                    // 标识符恰好为 `b` 且紧跟引号 → 字节序列字面量 b"..."
                    if s == "b" && self.peek() == Some('"') {
                        return self.lex_bytes();
                    }
                    Tok::Ident(s)
                }
            });
        }

        // 数字字面量（整数 / 浮点数 / 0b 二进制字节）
        if c.is_ascii_digit() || (c == '.' && self.peek2().map_or(false, |d| d.is_ascii_digit())) {
            return self.lex_number();
        }

        // 字符串字面量
        if c == '"' {
            // 三引号原始字符串："""..."""（必须是三连引号，`""` 为空字符串）
            if self.peek2() == Some('"') && self.peek3() == Some('"') {
                return self.lex_multistr();
            }
            return self.lex_string();
        }

        // 字符字面量 'a'（单引号定界）
        if c == '\'' {
            return self.lex_char();
        }

        // 运算符与符号
        let tok = match c {
            '+' => {
                self.bump();
                if self.peek() == Some('+') {
                    self.bump();
                    Tok::PlusPlus
                } else if self.peek() == Some('=') {
                    self.bump();
                    Tok::PlusEq
                } else {
                    Tok::Plus
                }
            }
            '-' => {
                self.bump();
                if self.peek() == Some('-') {
                    self.bump();
                    Tok::MinusMinus
                } else if self.peek() == Some('=') {
                    self.bump();
                    Tok::MinusEq
                } else if self.peek() == Some('>') {
                    self.bump();
                    Tok::Arrow
                } else {
                    Tok::Minus
                }
            }
            '*' => {
                self.bump();
                if self.peek() == Some('=') {
                    self.bump();
                    Tok::StarEq
                } else {
                    Tok::Star
                }
            }
            '/' => {
                self.bump();
                if self.peek() == Some('=') {
                    self.bump();
                    Tok::SlashEq
                } else {
                    Tok::Slash
                }
            }
            '%' => {
                self.bump();
                if self.peek() == Some('=') {
                    self.bump();
                    Tok::PercentEq
                } else {
                    Tok::Percent
                }
            }
            '?' => {
                self.bump();
                if self.peek() == Some('?') {
                    self.bump();
                    Tok::QuestionQuestion
                } else if self.peek() == Some('.') {
                    self.bump();
                    Tok::QuestionDot
                } else {
                    Tok::Question
                }
            }
            '=' => {
                self.bump();
                if self.peek() == Some('=') {
                    self.bump();
                    Tok::EqEq
                } else if self.peek() == Some('>') {
                    self.bump();
                    Tok::FatArrow
                } else {
                    Tok::Assign
                }
            }
            '!' => {
                self.bump();
                if self.peek() == Some('=') {
                    self.bump();
                    Tok::NotEq
                } else {
                    Tok::Bang
                }
            }
            '<' => {
                self.bump();
                if self.peek() == Some('=') {
                    self.bump();
                    Tok::Le
                } else {
                    Tok::Lt
                }
            }
            '>' => {
                self.bump();
                if self.peek() == Some('=') {
                    self.bump();
                    Tok::Ge
                } else {
                    Tok::Gt
                }
            }
            '&' => {
                self.bump();
                if self.peek() == Some('&') {
                    self.bump();
                    Tok::AndAnd
                } else {
                    return Err(self.err(
                        crate::error::codes::SYNTAX,
                        "expected `&&` after `&`",
                        1,
                        Some("use `&&` for logical AND"),
                    ));
                }
            }
            '|' => {
                self.bump();
                if self.peek() == Some('|') {
                    self.bump();
                    Tok::OrOr
                } else if self.peek() == Some('>') {
                    self.bump();
                    Tok::Pipe
                } else {
                    return Err(self.err(
                        crate::error::codes::SYNTAX,
                        "expected `||` or `|>` after `|`",
                        1,
                        Some("use `||` for logical OR, or `|>` for piping"),
                    ));
                }
            }
            ':' => {
                self.bump();
                if self.peek() == Some(':') {
                    return Err(self.err(
                        crate::error::codes::SYNTAX,
                        "`::` is not supported",
                        2,
                        Some("use dotted names like `time.now()` instead of `::` paths"),
                    ));
                }
                Tok::Colon
            }
            ',' => {
                self.bump();
                Tok::Comma
            }
            ';' => {
                self.bump();
                Tok::Semi
            }
            '.' => {
                self.bump();
                Tok::Dot
            }
            '(' => {
                self.bump();
                Tok::LParen
            }
            ')' => {
                self.bump();
                Tok::RParen
            }
            '[' => {
                self.bump();
                Tok::LBracket
            }
            ']' => {
                self.bump();
                Tok::RBracket
            }
            '{' => {
                self.bump();
                Tok::LBrace
            }
            '}' => {
                self.bump();
                Tok::RBrace
            }
            '@' => {
                self.bump();
                Tok::At
            }
            _ => {
                return Err(self.err(
                    crate::error::codes::ILLEGAL_CHAR,
                    format!("unexpected character `{}`", c),
                    1,
                    Some("check the character near this position"),
                ));
            }
        };
        Ok(tok)
    }

    fn lex_number(&mut self) -> Result<Tok, ZError> {
        // 二进制字节字面量 0b01000001：byte 类型（0-255），最多 8 位，超位报错
        if self.peek() == Some('0') && self.peek2() == Some('b') {
            self.bump();
            self.bump();
            let mut bits = 0usize;
            let mut val: u8 = 0;
            while let Some(c) = self.peek() {
                if c == '0' || c == '1' {
                    if bits >= 8 {
                        return Err(self.err(
                            crate::error::codes::SYNTAX,
                            format!("byte literal `0b{}` has {} bits (max 8)", val, bits + 1),
                            1,
                            Some("a `byte` literal is at most 8 bits, e.g. `0b11111111`; use `int` literals for larger values"),
                        ));
                    }
                    val = val.wrapping_shl(1) | c as u8 - b'0';
                    bits += 1;
                    self.bump();
                } else {
                    break;
                }
            }
            if bits == 0 {
                return Err(self.err(
                    crate::error::codes::SYNTAX,
                    "byte literal `0b` is empty",
                    2,
                    Some("write at least one binary digit, e.g. `0b1001`"),
                ));
            }
            return Ok(Tok::ByteLit(val));
        }

        let mut is_float = false;
        let mut text = String::new();

        if self.peek() == Some('.') {
            // 前导小数点：.2
            is_float = true;
            text.push('.');
            self.bump();
        }

        while self.peek().map_or(false, |c| c.is_ascii_digit()) {
            text.push(self.peek().unwrap());
            self.bump();
        }

        // 小数部分：数字后跟 '.' 且后一位是数字 → 浮点数
        if self.peek() == Some('.') && self.peek2().map_or(false, |c| c.is_ascii_digit()) {
            is_float = true;
            text.push('.');
            self.bump();
            while self.peek().map_or(false, |c| c.is_ascii_digit()) {
                text.push(self.peek().unwrap());
                self.bump();
            }
        } else if self.peek() == Some('.') {
            // 2. 这种形式：必须有小数点后的数字
            return Err(self.err(
                crate::error::codes::SYNTAX,
                format!("expected digit after decimal point in `{}`", text),
                1,
                Some("write `2.0` instead of `2.`"),
            ));
        }

        // 数字后紧跟标识符字符 → 非法字面量
        if self.peek().map_or(false, |c| c.is_ascii_alphabetic() || c == '_') {
            return Err(self.err(
                crate::error::codes::SYNTAX,
                format!("invalid number literal `{}{}`", text, self.peek().unwrap()),
                1,
                Some("add a space or operator between the number and the identifier"),
            ));
        }

        if is_float {
            match text.parse::<f64>() {
                Ok(v) => Ok(Tok::FloatLit(v)),
                Err(_) => Err(self.err(
                    crate::error::codes::SYNTAX,
                    format!("invalid float literal `{}`", text),
                    text.len(),
                    None::<&str>,
                )),
            }
        } else {
            match text.parse::<i64>() {
                Ok(v) => Ok(Tok::IntLit(v)),
                Err(_) => Err(self.err(
                    crate::error::codes::SYNTAX,
                    format!("integer literal `{}` is out of range", text),
                    text.len(),
                    Some("Hone `int` is a 64-bit signed integer"),
                )),
            }
        }
    }

    fn lex_string(&mut self) -> Result<Tok, ZError> {
        self.bump(); // 开头的 "
        let mut s = String::new();
        loop {
            match self.peek() {
                None => {
                    return Err(self.err(
                        crate::error::codes::UNTERMINATED_STRING,
                        "unterminated string literal",
                        1,
                        Some("close the string with `\"`"),
                    ));
                }
                Some('\n') => {
                    return Err(self.err(
                        crate::error::codes::UNTERMINATED_STRING,
                        "unterminated string literal (newline inside string)",
                        1,
                        Some("close the string before the newline"),
                    ));
                }
                Some('"') => {
                    self.bump();
                    break;
                }
                Some('\\') => {
                    self.bump();
                    match self.peek() {
                        Some('n') => {
                            s.push('\n');
                            self.bump();
                        }
                        Some('t') => {
                            s.push('\t');
                            self.bump();
                        }
                        Some('\\') => {
                            s.push('\\');
                            self.bump();
                        }
                        Some('"') => {
                            s.push('"');
                            self.bump();
                        }
                        Some(c) => {
                            return Err(self.err(
                                crate::error::codes::SYNTAX,
                                format!("invalid escape sequence `\\{}`", c),
                                2,
                                Some("supported escapes: \\n \\t \\\\ \\\""),
                            ));
                        }
                        None => {
                            return Err(self.err(
                                crate::error::codes::UNTERMINATED_STRING,
                                "unterminated string literal",
                                1,
                                Some("close the string with `\"`"),
                            ));
                        }
                    }
                }
                Some(c) => {
                    s.push(c);
                    self.bump();
                }
            }
        }
        Ok(Tok::StrLit(s))
    }

    /// 词法分析字符字面量 'a'。调用前已确认当前字符为 `'`。
    /// 支持转义 \\n \\t \\\\ \\\" \\'；闭合后要求恰好一个 Unicode 字符（空/多字符报错）。
    fn lex_char(&mut self) -> Result<Tok, ZError> {
        self.bump(); // 开头的 '
        let mut s = String::new();
        loop {
            match self.peek() {
                None => {
                    return Err(self.err(
                        crate::error::codes::UNTERMINATED_STRING,
                        "unterminated char literal",
                        1,
                        Some("close the char literal with `'`"),
                    ));
                }
                Some('\n') => {
                    return Err(self.err(
                        crate::error::codes::UNTERMINATED_STRING,
                        "unterminated char literal (newline inside char)",
                        1,
                        Some("close the char literal before the newline"),
                    ));
                }
                Some('\'') => {
                    self.bump();
                    break;
                }
                Some('\\') => {
                    self.bump();
                    match self.peek() {
                        Some('n') => {
                            s.push('\n');
                            self.bump();
                        }
                        Some('t') => {
                            s.push('\t');
                            self.bump();
                        }
                        Some('\\') => {
                            s.push('\\');
                            self.bump();
                        }
                        Some('"') => {
                            s.push('"');
                            self.bump();
                        }
                        Some('\'') => {
                            s.push('\'');
                            self.bump();
                        }
                        Some(c) => {
                            return Err(self.err(
                                crate::error::codes::SYNTAX,
                                format!("invalid escape sequence `\\{}`", c),
                                2,
                                Some("supported escapes: \\n \\t \\\\ \\\" \\'"),
                            ));
                        }
                        None => {
                            return Err(self.err(
                                crate::error::codes::UNTERMINATED_STRING,
                                "unterminated char literal",
                                1,
                                Some("close the char literal with `'`"),
                            ));
                        }
                    }
                }
                Some(c) => {
                    s.push(c);
                    self.bump();
                }
            }
        }
        let mut chars = s.chars();
        let c = chars.next().ok_or_else(|| {
            self.err(
                crate::error::codes::SYNTAX,
                "empty char literal",
                1,
                Some("a char literal must contain exactly one character, e.g. `'a'`"),
            )
        })?;
        if chars.next().is_some() {
            return Err(self.err(
                crate::error::codes::SYNTAX,
                format!("char literal must contain exactly one character, got `{}`", s),
                1,
                Some("use a string `\"...\"` for multiple characters"),
            ));
        }
        Ok(Tok::CharLit(c))
    }

    /// 词法分析字节序列字面量 b"..."。调用前已消费 `b`，此处消费开头的 `"`。
    /// 支持转义 \n \t \r \\ \" 与 \xNN（单字节十六进制）；非 ASCII 字符按 UTF-8 编码入字节序列。
    fn lex_bytes(&mut self) -> Result<Tok, ZError> {
        self.bump(); // 开头的 "
        let mut bytes: Vec<u8> = Vec::new();
        loop {
            match self.peek() {
                None => {
                    return Err(self.err(
                        crate::error::codes::UNTERMINATED_STRING,
                        "unterminated bytes literal",
                        1,
                        Some("close the bytes literal with `\"`"),
                    ));
                }
                Some('\n') => {
                    return Err(self.err(
                        crate::error::codes::UNTERMINATED_STRING,
                        "unterminated bytes literal (newline inside literal)",
                        1,
                        Some("close the bytes literal before the newline"),
                    ));
                }
                Some('"') => {
                    self.bump();
                    break;
                }
                Some('\\') => {
                    self.bump();
                    match self.peek() {
                        Some('n') => {
                            bytes.push(b'\n');
                            self.bump();
                        }
                        Some('t') => {
                            bytes.push(b'\t');
                            self.bump();
                        }
                        Some('r') => {
                            bytes.push(b'\r');
                            self.bump();
                        }
                        Some('\\') => {
                            bytes.push(b'\\');
                            self.bump();
                        }
                        Some('"') => {
                            bytes.push(b'"');
                            self.bump();
                        }
                        Some('x') => {
                            self.bump();
                            let h1 = self.peek().and_then(|c| c.to_digit(16)).ok_or_else(|| {
                                self.err(
                                    crate::error::codes::SYNTAX,
                                    "invalid escape sequence `\\x` in bytes literal",
                                    2,
                                    Some("expected two hex digits after `\\x`, e.g. `\\x41`"),
                                )
                            })?;
                            let h1c = self.peek().unwrap();
                            self.bump();
                            let h2 = self.peek().and_then(|c| c.to_digit(16)).ok_or_else(|| {
                                self.err(
                                    crate::error::codes::SYNTAX,
                                    format!("invalid escape sequence `\\x{}` in bytes literal", h1c),
                                    3,
                                    Some("expected two hex digits after `\\x`, e.g. `\\x41`"),
                                )
                            })?;
                            let h2c = self.peek().unwrap();
                            self.bump();
                            bytes.push((h1 << 4 | h2) as u8);
                            let _ = h2c;
                        }
                        Some(c) => {
                            return Err(self.err(
                                crate::error::codes::SYNTAX,
                                format!("invalid escape sequence `\\{}` in bytes literal", c),
                                2,
                                Some("supported escapes: \\n \\t \\r \\\\ \\\" \\xNN"),
                            ));
                        }
                        None => {
                            return Err(self.err(
                                crate::error::codes::UNTERMINATED_STRING,
                                "unterminated bytes literal",
                                1,
                                Some("close the bytes literal with `\"`"),
                            ));
                        }
                    }
                }
                Some(c) => {
                    if (c as u32) < 0x80 {
                        bytes.push(c as u8);
                    } else {
                        // 非 ASCII 字符按 UTF-8 编码
                        let mut buf = [0u8; 4];
                        bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                    }
                    self.bump();
                }
            }
        }
        Ok(Tok::BytesLit(bytes))
    }

    /// 词法分析三引号原始字符串 """..."""。调用前已确认当前字符为 `"` 且后随 `""`。
    /// 内容不做任何转义处理，原样保留（含换行），直到遇到闭合的 `"""`。
    fn lex_multistr(&mut self) -> Result<Tok, ZError> {
        self.bump(); // "
        self.bump(); // "
        self.bump(); // "
        let mut s = String::new();
        loop {
            match self.peek() {
                None => {
                    return Err(self.err(
                        crate::error::codes::UNTERMINATED_STRING,
                        "unterminated triple-quoted string",
                        3,
                        Some("close the string with `\"\"\"`"),
                    ));
                }
                Some('"') => {
                    // 检查是否为闭合的 """（三连引号）
                    if self.peek2() == Some('"') && self.peek3() == Some('"') {
                        self.bump();
                        self.bump();
                        self.bump();
                        break;
                    }
                    s.push('"');
                    self.bump();
                }
                Some(c) => {
                    s.push(c);
                    self.bump();
                }
            }
        }
        Ok(Tok::MultiStr(s))
    }

    /// 词法分析插值字符串 f"..."。调用前已消费 `f`，此处消费开头的 `"`。
    /// 返回的片段：文字段保留原始转义（由 parser 解码），代码段保留原始文本（由 parser 子解析）。
    /// `{{` / `}}` 为转义的字面大括号，保留双写形式由 parser 折叠为单个。
    fn lex_fstring(&mut self) -> Result<Tok, ZError> {
        self.bump(); // 开头的 "
        let mut parts: Vec<FStrPart> = Vec::new();
        let mut lit = String::new();
        let mut code = String::new();
        let mut depth: usize = 0; // { 嵌套深度；0 = 文字段
        let mut in_code_str = false; // 代码段内的字符串字面量
        loop {
            let c = match self.peek() {
                None => {
                    return Err(self.err(
                        crate::error::codes::UNTERMINATED_STRING,
                        "unterminated f-string literal",
                        1,
                        Some("close the string with `\"`"),
                    ));
                }
                Some(c) => c,
            };
            if c == '\n' {
                return Err(self.err(
                    crate::error::codes::UNTERMINATED_STRING,
                    "unterminated f-string literal (newline inside string)",
                    1,
                    Some("close the string before the newline"),
                ));
            }
            if depth == 0 {
                // ---------- 文字段 ----------
                match c {
                    '"' => {
                        self.bump();
                        break;
                    }
                    '\\' => {
                        // 保留原始转义，parser 负责解码
                        lit.push(c);
                        self.bump();
                        if let Some(e) = self.peek() {
                            lit.push(e);
                            self.bump();
                        }
                    }
                    '{' => {
                        if self.peek2() == Some('{') {
                            // 转义的字面大括号 `{{`
                            lit.push_str("{{");
                            self.bump();
                            self.bump();
                        } else {
                            // 代码段开始
                            self.bump();
                            parts.push(FStrPart::Lit(std::mem::take(&mut lit)));
                            code.clear();
                            depth = 1;
                        }
                    }
                    '}' => {
                        if self.peek2() == Some('}') {
                            // 转义的字面大括号 `}}`
                            lit.push_str("}}");
                            self.bump();
                            self.bump();
                        } else {
                            return Err(self.err(
                                crate::error::codes::SYNTAX,
                                "unmatched `}` in f-string (use `}}` for a literal brace)",
                                1,
                                Some("escape a literal `}` as `}}`"),
                            ));
                        }
                    }
                    _ => {
                        lit.push(c);
                        self.bump();
                    }
                }
            } else {
                // ---------- 代码段 ----------
                if in_code_str {
                    match c {
                        '\\' => {
                            code.push(c);
                            self.bump();
                            if let Some(e) = self.peek() {
                                code.push(e);
                                self.bump();
                            }
                        }
                        '"' => {
                            in_code_str = false;
                            code.push(c);
                            self.bump();
                        }
                        _ => {
                            code.push(c);
                            self.bump();
                        }
                    }
                    continue;
                }
                match c {
                    '"' => {
                        in_code_str = true;
                        code.push(c);
                        self.bump();
                    }
                    '{' => {
                        depth += 1;
                        code.push(c);
                        self.bump();
                    }
                    '}' => {
                        depth -= 1;
                        self.bump();
                        if depth == 0 {
                            parts.push(FStrPart::Code(std::mem::take(&mut code)));
                        } else {
                            code.push('}');
                        }
                    }
                    _ => {
                        code.push(c);
                        self.bump();
                    }
                }
            }
        }
        if !lit.is_empty() {
            parts.push(FStrPart::Lit(lit));
        }
        Ok(Tok::FStr(parts))
    }
}
