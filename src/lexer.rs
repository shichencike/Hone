// lexer.rs - Zap 词法分析器
// 生成 token 流，每个 token 携带 Span（行:列 + 长度）用于精准报错。

use crate::error::ZError;

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    // 标识符与字面量
    Ident(String),
    IntLit(i64),
    FloatLit(f64),
    StrLit(String),
    // 关键字
    Fn,
    If,
    Else,
    While,
    Return,
    True,
    False,
    Go,
    Try,
    Catch,
    Throw,
    Breakpoint,
    Load,
    Lazy,
    Use,
    Import,
    Alias,
    As,
    From,
    Tmp,
    // 类型关键字
    TInt,
    TFloat,
    TBool,
    TStr,
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
    Bang,
    Assign,
    Colon,
    Arrow, // ->
    Comma,
    Semi,
    Dot,
    LParen,
    RParen,
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
            Tok::Fn => "`fn`".into(),
            Tok::If => "`if`".into(),
            Tok::Else => "`else`".into(),
            Tok::While => "`while`".into(),
            Tok::Return => "`return`".into(),
            Tok::True => "`true`".into(),
            Tok::False => "`false`".into(),
            Tok::Go => "`go`".into(),
            Tok::Try => "`try`".into(),
            Tok::Catch => "`catch`".into(),
            Tok::Throw => "`throw`".into(),
            Tok::Breakpoint => "`breakpoint`".into(),
            Tok::Load => "`load`".into(),
            Tok::Lazy => "`lazy`".into(),
            Tok::Use => "`use`".into(),
            Tok::Import => "`import`".into(),
            Tok::Alias => "`alias`".into(),
            Tok::As => "`as`".into(),
            Tok::From => "`from`".into(),
            Tok::Tmp => "`tmp`".into(),
            Tok::TInt => "type `int`".into(),
            Tok::TFloat => "type `float`".into(),
            Tok::TBool => "type `bool`".into(),
            Tok::TStr => "type `str`".into(),
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
            Tok::Bang => "`!`".into(),
            Tok::Assign => "`=`".into(),
            Tok::Colon => "`:`".into(),
            Tok::Arrow => "`->`".into(),
            Tok::Comma => "`,`".into(),
            Tok::Semi => "`;`".into(),
            Tok::Dot => "`.`".into(),
            Tok::LParen => "`(`".into(),
            Tok::RParen => "`)`".into(),
            Tok::LBrace => "`{`".into(),
            Tok::RBrace => "`}`".into(),
            Tok::At => "`@`".into(),
            Tok::Eof => "end of file".into(),
        }
    }
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
                "return" => Tok::Return,
                "true" => Tok::True,
                "false" => Tok::False,
                "go" => Tok::Go,
                "try" => Tok::Try,
                "catch" => Tok::Catch,
                "throw" => Tok::Throw,
                "breakpoint" => Tok::Breakpoint,
                "load" => Tok::Load,
                "lazy" => Tok::Lazy,
                "use" => Tok::Use,
                "import" => Tok::Import,
                "alias" => Tok::Alias,
                "as" => Tok::As,
                "from" => Tok::From,
                "tmp" => Tok::Tmp,
                "int" => Tok::TInt,
                "float" => Tok::TFloat,
                "bool" => Tok::TBool,
                "str" => Tok::TStr,
                _ => Tok::Ident(s),
            });
        }

        // 数字字面量（整数 / 浮点数）
        if c.is_ascii_digit() || (c == '.' && self.peek2().map_or(false, |d| d.is_ascii_digit())) {
            return self.lex_number();
        }

        // 字符串字面量
        if c == '"' {
            return self.lex_string();
        }

        // 运算符与符号
        let tok = match c {
            '+' => {
                self.bump();
                Tok::Plus
            }
            '-' => {
                self.bump();
                if self.peek() == Some('>') {
                    self.bump();
                    Tok::Arrow
                } else {
                    Tok::Minus
                }
            }
            '*' => {
                self.bump();
                Tok::Star
            }
            '/' => {
                self.bump();
                Tok::Slash
            }
            '%' => {
                self.bump();
                Tok::Percent
            }
            '=' => {
                self.bump();
                if self.peek() == Some('=') {
                    self.bump();
                    Tok::EqEq
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
                } else {
                    return Err(self.err(
                        crate::error::codes::SYNTAX,
                        "expected `||` after `|`",
                        1,
                        Some("use `||` for logical OR"),
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
                    Some("Zap `int` is a 64-bit signed integer"),
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
}
