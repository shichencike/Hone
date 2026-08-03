// upgrade.rs - zap upgrade 迁移工具
// 基于映射表自动迁移旧版本代码到新语法。
// v0.1 内置演示规则（可扩展）：旧版 `::` 路径前缀、旧版 `@native` 导出标记。

use crate::error::codes;
use crate::error::ZError;

struct Rule {
    pattern: &'static str,
    replacement: &'static str,
    description: &'static str,
}

/// 版本迁移映射表（按顺序应用）。
const RULES: &[Rule] = &[
    Rule {
        pattern: "::",
        replacement: ".",
        description: "旧版 `::` 多层路径前缀 → 点号命名空间（如 sys::now → sys.now）",
    },
    Rule {
        pattern: "@native",
        replacement: "@export",
        description: "旧版导出标记 `@native` → `@export`",
    },
];

/// 应用映射表迁移源码，返回（新源码, 迁移报告行）。
pub fn upgrade(src: &str) -> (String, Vec<String>) {
    let mut out = src.to_string();
    let mut report = Vec::new();
    for rule in RULES {
        let count = out.matches(rule.pattern).count();
        if count > 0 {
            out = out.replace(rule.pattern, rule.replacement);
            report.push(format!("  - {}：替换 {} 处", rule.description, count));
        }
    }
    (out, report)
}

/// zap upgrade [-w] <file.zp>...：应用迁移规则；-w 覆盖写入，否则打印到 stdout。
pub fn cmd_upgrade(args: &[String]) -> Result<(), ZError> {
    let mut overwrite = false;
    let mut files = Vec::new();
    for a in args {
        if a == "-w" || a == "--write" {
            overwrite = true;
        } else {
            files.push(a.clone());
        }
    }
    if files.is_empty() {
        return Err(ZError::plain(
            codes::SYNTAX,
            "missing file: `zap upgrade [-w] <file.zp>...`",
            Some("pass one or more .zp files, e.g. `zap upgrade -w *.zp`"),
        ));
    }
    for f in files {
        let src = std::fs::read_to_string(&f).map_err(|e| {
            ZError::plain(codes::NOT_FOUND, format!("cannot read `{}`: {}", f, e), Some("check the path"))
        })?;
        let (new_src, report) = upgrade(&src);
        if report.is_empty() {
            println!("{}: 无需迁移（已是最新语法）", f);
            continue;
        }
        println!("{}: 迁移报告", f);
        for r in &report {
            println!("{}", r);
        }
        if overwrite {
            std::fs::write(&f, &new_src).map_err(|e| {
                ZError::plain(codes::NOT_FOUND, format!("cannot write `{}`: {}", f, e), Some("check the path"))
            })?;
            println!("已覆盖写入 {}", f);
        } else {
            println!("--- 迁移后的代码 ---");
            print!("{}", new_src);
        }
    }
    Ok(())
}
