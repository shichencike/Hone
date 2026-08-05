// fmt.rs - Zap 代码格式化器（zap fmt）
// 规则（与规范一致）：
//   - 统一 Tab 缩进（每层块 +1 Tab）
//   - 二元运算符两侧空格，一元运算符后无空格
//   - 大括号位置：`{` 与语句同行（K&R），`}` 独立一行；`} else {` 同行
//   - 语句以 `;` 结束并换行；`,` 后一个空格
//   - 保留注释（// 与 /* */）与空行
// 使用保留注释的专用 tokenizer，注释随代码保持原顺序。

use crate::error::codes;
use crate::error::ZError;

#[derive(Debug, Clone, PartialEq)]
enum FTok {
    Punct(String),
    Ident(String),
    IntLit(String),
    FloatLit(String),
    StrLit(String),
    Keyword(String),
    LineComment(String),
    BlockComment(String),
    Eof,
}

/// token + 前导换行数（用于保留空行）。
struct Item {
    tok: FTok,
    nl: u32,
}

const KEYWORDS: &[&str] = &[
    "fn", "if", "else", "while", "return", "true", "false", "go", "breakpoint",
    "int", "float", "bool", "str", "load", "lazy", "use", "import", "alias", "as", "from",
    "tmp",
];

const BIN_OPS: &[&str] = &["+", "-", "*", "/", "%", "==", "!=", "<", "<=", ">", ">=", "&&", "||"];

/// 对 Zap 源码做格式化。语法错误按 error[Z005] 报告。
pub fn format(src: &str) -> Result<String, ZError> {
    let items = tokenize(src)?;
    let mut f = Fmt {
        out: String::new(),
        indent: 0,
        at_line_start: true,
        prev: None,
    };
    let n = items.len();
    let mut i = 0;
    while i < n {
        if matches!(items[i].tok, FTok::Eof) {
            break;
        }
        f.emit(&items, &mut i);
    }
    Ok(f.out)
}

// ---------- 保留注释的 tokenizer ----------

fn tokenize(src: &str) -> Result<Vec<Item>, ZError> {
    let chars: Vec<char> = src.chars().collect();
    let mut items = Vec::new();
    let mut pos = 0;
    let mut nl = 0u32;
    let mut line = 1usize;
    let mut col = 1usize;

    while pos < chars.len() {
        let c = chars[pos];
        if c == '\n' {
            nl += 1;
            line += 1;
            col = 1;
            pos += 1;
            continue;
        }
        if c == ' ' || c == '\t' || c == '\r' {
            pos += 1;
            col += 1;
            continue;
        }

        let start_line = line;
        let start_col = col;

        // 注释
        if c == '/' && pos + 1 < chars.len() && chars[pos + 1] == '/' {
            let mut text = String::from("//");
            pos += 2;
            col += 2;
            while pos < chars.len() && chars[pos] != '\n' {
                text.push(chars[pos]);
                pos += 1;
                col += 1;
            }
            items.push(Item { tok: FTok::LineComment(text), nl });
            nl = 0;
            continue;
        }
        if c == '/' && pos + 1 < chars.len() && chars[pos + 1] == '*' {
            let mut text = String::from("/*");
            pos += 2;
            col += 2;
            let mut closed = false;
            while pos < chars.len() {
                if chars[pos] == '*' && pos + 1 < chars.len() && chars[pos + 1] == '/' {
                    text.push_str("*/");
                    pos += 2;
                    col += 2;
                    closed = true;
                    break;
                }
                if chars[pos] == '\n' {
                    text.push('\n');
                    line += 1;
                    col = 1;
                } else {
                    text.push(chars[pos]);
                    col += 1;
                }
                pos += 1;
            }
            if !closed {
                return Err(err_at(
                    codes::SYNTAX,
                    "unterminated block comment",
                    start_line,
                    start_col,
                    src,
                ));
            }
            items.push(Item { tok: FTok::BlockComment(text), nl });
            nl = 0;
            continue;
        }

        // 字符串
        if c == '"' {
            let mut text = String::from("\"");
            pos += 1;
            col += 1;
            loop {
                if pos >= chars.len() || chars[pos] == '\n' {
                    return Err(err_at(codes::SYNTAX, "unterminated string literal", start_line, start_col, src));
                }
                if chars[pos] == '"' {
                    text.push('"');
                    pos += 1;
                    col += 1;
                    break;
                }
                if chars[pos] == '\\' {
                    text.push('\\');
                    pos += 1;
                    col += 1;
                    if pos < chars.len() {
                        text.push(chars[pos]);
                        pos += 1;
                        col += 1;
                    }
                    continue;
                }
                text.push(chars[pos]);
                pos += 1;
                col += 1;
            }
            items.push(Item { tok: FTok::StrLit(text), nl });
            nl = 0;
            continue;
        }

        // 数字
        if c.is_ascii_digit() || (c == '.' && pos + 1 < chars.len() && chars[pos + 1].is_ascii_digit()) {
            let mut text = String::new();
            let mut is_float = false;
            if c == '.' {
                is_float = true;
                text.push('.');
                pos += 1;
                col += 1;
            }
            while pos < chars.len() && chars[pos].is_ascii_digit() {
                text.push(chars[pos]);
                pos += 1;
                col += 1;
            }
            if pos + 1 < chars.len() && chars[pos] == '.' && chars[pos + 1].is_ascii_digit() {
                is_float = true;
                text.push('.');
                pos += 1;
                col += 1;
                while pos < chars.len() && chars[pos].is_ascii_digit() {
                    text.push(chars[pos]);
                    pos += 1;
                    col += 1;
                }
            }
            items.push(Item {
                tok: if is_float { FTok::FloatLit(text) } else { FTok::IntLit(text) },
                nl,
            });
            nl = 0;
            continue;
        }

        // 标识符 / 关键字
        if c.is_ascii_alphabetic() || c == '_' {
            let mut text = String::new();
            while pos < chars.len() && (chars[pos].is_ascii_alphanumeric() || chars[pos] == '_') {
                text.push(chars[pos]);
                pos += 1;
                col += 1;
            }
            let tok = if KEYWORDS.contains(&text.as_str()) {
                FTok::Keyword(text)
            } else {
                FTok::Ident(text)
            };
            items.push(Item { tok, nl });
            nl = 0;
            continue;
        }

        // 符号（最长匹配）
        let two: Option<String> = if pos + 1 < chars.len() {
            Some(format!("{}{}", c, chars[pos + 1]))
        } else {
            None
        };
        let sym = match two.as_deref() {
            Some("==") | Some("!=") | Some("<=") | Some(">=") | Some("&&") | Some("||") | Some("->") => {
                let s = two.unwrap();
                pos += 2;
                col += 2;
                s
            }
            _ => {
                let s = c.to_string();
                pos += 1;
                col += 1;
                s
            }
        };
        items.push(Item { tok: FTok::Punct(sym), nl });
        nl = 0;
    }

    items.push(Item { tok: FTok::Eof, nl });
    Ok(items)
}

fn err_at(code: &'static str, msg: impl Into<String>, line: usize, col: usize, src: &str) -> ZError {
    ZError::new(code, msg, "<fmt>", src, line, col, 1, None::<&str>)
}

// ---------- 格式化器 ----------

struct Fmt {
    out: String,
    indent: usize,
    at_line_start: bool,
    prev: Option<FTok>,
}

impl Fmt {
    fn write_indent(&mut self) {
        if self.at_line_start {
            for _ in 0..self.indent {
                self.out.push('\t');
            }
            self.at_line_start = false;
        }
    }

    fn space(&mut self) {
        self.out.push(' ');
    }

    fn newline(&mut self) {
        self.out.push('\n');
        self.at_line_start = true;
    }

    fn is_kw(&self, kw: &str) -> bool {
        matches!(&self.prev, Some(FTok::Keyword(k)) if k == kw)
    }

    fn emit(&mut self, items: &[Item], i: &mut usize) {
        let item = &items[*i];
        let nl_before = item.nl;
        match &item.tok {
            FTok::LineComment(text) => {
                // 空行保留
                if nl_before >= 2 && !self.at_line_start {
                    self.out.push('\n');
                    self.at_line_start = true;
                }
                if self.at_line_start {
                    self.write_indent();
                } else {
                    self.space();
                }
                self.out.push_str(text);
                self.newline();
            }
            FTok::BlockComment(text) => {
                if nl_before >= 2 && !self.at_line_start {
                    self.out.push('\n');
                    self.at_line_start = true;
                }
                if self.at_line_start {
                    self.write_indent();
                } else {
                    self.space();
                }
                self.out.push_str(text);
                // 块注释后若紧跟换行（原始换行）→ 换行；否则留在行内
                let has_following_nl = items.get(*i + 1).map(|it| it.nl >= 1).unwrap_or(false);
                if has_following_nl {
                    self.newline();
                }
            }
            FTok::Eof => return,
            FTok::Punct(s) => {
                match s.as_str() {
                    ";" => {
                        self.write_indent();
                        self.out.push(';');
                        self.newline();
                    }
                    "{" => {
                        // 空块 `{}`
                        let empty = matches!(items.get(*i + 1).map(|it| &it.tok), Some(FTok::Punct(p)) if p == "}");
                        let bol = self.at_line_start;
                        if bol {
                            self.write_indent();
                        }
                        if !bol {
                            self.space();
                        }
                        if empty {
                            self.out.push_str("{ }");
                            self.newline();
                            *i += 1; // 跳过紧随的 }
                        } else {
                            self.out.push('{');
                            self.indent += 1;
                            self.newline();
                        }
                    }
                    "}" => {
                        if self.indent > 0 {
                            self.indent -= 1;
                        }
                        if self.at_line_start {
                            self.write_indent();
                        }
                        self.out.push('}');
                        // `} else {` 同行（else 前的空格由 keyword 分支补上）
                        let is_else = matches!(items.get(*i + 1).map(|it| &it.tok), Some(FTok::Keyword(k)) if k == "else");
                        if !is_else {
                            self.newline();
                        }
                    }
                    "," => {
                        self.write_indent();
                        self.out.push(',');
                        self.space();
                    }
                    ":" => {
                        self.write_indent();
                        self.space();
                        self.out.push(':');
                        self.space();
                    }
                    "->" => {
                        self.write_indent();
                        self.space();
                        self.out.push_str("->");
                        self.space();
                    }
                    "(" => {
                        self.write_indent();
                        if self.is_kw("if") || self.is_kw("while") {
                            self.space();
                        }
                        self.out.push('(');
                    }
                    ")" => {
                        self.write_indent();
                        self.out.push(')');
                    }
                    "=" => {
                        self.write_indent();
                        self.space();
                        self.out.push('=');
                        self.space();
                    }
                    "!" => {
                        self.write_indent();
                        self.out.push('!');
                    }
                    "-" if self.is_unary_minus() => {
                        self.write_indent();
                        self.out.push('-');
                    }
                    op if BIN_OPS.contains(&op) => {
                        self.write_indent();
                        self.space();
                        self.out.push_str(op);
                        self.space();
                    }
                    other => {
                        self.write_indent();
                        self.out.push_str(other);
                    }
                }
            }
            FTok::Ident(t) | FTok::IntLit(t) | FTok::FloatLit(t) | FTok::StrLit(t) | FTok::Keyword(t) => {
                if nl_before >= 2 && !self.at_line_start {
                    self.out.push('\n');
                    self.at_line_start = true;
                }
                let bol = self.at_line_start;
                self.write_indent();
                if !bol {
                    // 行中：与前一个 token 之间需要空格
                    let need = match &self.prev {
                        Some(FTok::Keyword(_)) => true, // `return n`、`int x`、`fn name`、`go task`
                        Some(FTok::Ident(_))
                        | Some(FTok::IntLit(_))
                        | Some(FTok::FloatLit(_))
                        | Some(FTok::StrLit(_)) => true,
                        Some(FTok::Punct(p)) if p == "}" => true, // `} else`
                        _ => false,
                    };
                    if need {
                        self.space();
                    }
                }
                self.out.push_str(t);
            }
        }
        if !matches!(item.tok, FTok::LineComment(_) | FTok::BlockComment(_)) {
            self.prev = Some(item.tok.clone());
        }
        *i += 1;
    }

    /// 判断 `-` 是否为一元负号：前 token 为空、运算符、`(`、`,`、`=`、`:`、关键字或行首。
    fn is_unary_minus(&self) -> bool {
        match &self.prev {
            None => true,
            Some(FTok::Punct(p)) => p != ")" && p != "}",
            Some(FTok::Keyword(_)) => true,
            _ => false, // ident/字面量前 → 二元（x - 1）
        }
    }
}
