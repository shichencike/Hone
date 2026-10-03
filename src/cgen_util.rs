// cgen_util.rs - 各 C 代码生成后端（aot.rs / codegen.rs）共用的机械工具。
//
// 为什么单独成文件：`aot.rs`（--exe -c）与 `codegen.rs`（--dll）的值模型完全不同
// （前者统一装箱 HnValue，后者用 int64_t/const char* 等 C 原生类型对接 C ABI），
// 二者的**代码生成主体不可能合并**。但纯机械的辅助函数——字符串/字符转义、
// 标识符合法性判断等——与值模型无关，两边各写一份只会漂移。
//
// 历史教训：两个 `c_str_lit` 曾经实现不一致（一个用八进制 `\001`，一个用十六进制
// `\x01`），而 C 的 `\x` 转义是**贪婪**的：`"\x01a"` 会被编译器解析成单字节 `\x1a`，
// 导致字符串被截短、内容错误。这类差异不会引发编译错误，只会静默产出错值，
// 所以必须由单一实现来保证一致。

/// 把 Rust 字符串转义为 C 字符串字面量（含首尾双引号）。
///
/// 控制字符统一用**三位八进制**（`\001`）而非十六进制（`\x01`）：
/// C 的 `\x` 转义会贪婪吞掉后续的十六进制数字符，例如 `"\x01a"` 实际被解析为
/// 单字节 `\x1a`（26）；`"\001a"` 则正确地解析为两字节 `\x01` + `'a'`。
/// 八进制转义最多吃 3 位数字，固定补足 3 位即可消除歧义。
pub fn c_str_lit(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                // 固定三位八进制，避免与后续字符连读
                out.push_str(&format!("\\{:03o}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_quotes_and_backslash() {
        assert_eq!(c_str_lit(r#"a"b"#), r#""a\"b""#);
        assert_eq!(c_str_lit(r"a\b"), r#""a\\b""#);
    }

    #[test]
    fn escapes_whitespace() {
        assert_eq!(c_str_lit("a\nb"), r#""a\nb""#);
        assert_eq!(c_str_lit("a\tb"), r#""a\tb""#);
        assert_eq!(c_str_lit("a\rb"), r#""a\rb""#);
    }

    /// 关键回归：控制字符必须用三位八进制，否则 `\x` 会贪婪吞掉后随的十六进制数字。
    /// 这里 `0x01` 后跟 `'a'`（十六进制字符）——用 `\x01` 写法会退化成单字节 `\x1a`。
    #[test]
    fn control_char_does_not_swallow_following_hex_digit() {
        assert_eq!(c_str_lit("\u{1}a"), r#""\001a""#);
        assert_eq!(c_str_lit("\u{1}f"), r#""\001f""#);
        assert_eq!(c_str_lit("\u{0}"), r#""\000""#);
        assert_eq!(c_str_lit("\u{1f}"), r#""\037""#);
    }

    #[test]
    fn keeps_utf8_intact() {
        assert_eq!(c_str_lit("中文"), "\"中文\"");
    }
}
