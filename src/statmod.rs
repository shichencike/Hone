// statmod.rs - 科学计算（stat.* / matrix.* 内置函数）
// 纯 Rust 实现，Windows / Linux / Termux 跨平台一致。
//   stat.mean(nums)        -> float  算术平均值
//   stat.median(nums)      -> float  中位数
//   stat.variance(nums)    -> float  总体方差
//   stat.stddev(nums)      -> float  总体标准差
//   stat.min(nums)         -> number 最小值
//   stat.max(nums)         -> number 最大值
//   stat.sum(nums)         -> number 求和（int 列表返回 int，含 float 返回 float）
//   matrix.identity(n)     -> list   n×n 单位矩阵
//   matrix.transpose(m)    -> list   矩阵转置
//   matrix.add(a, b)       -> list   矩阵相加（形状相同）
//   matrix.mul(a, b)       -> list   矩阵乘法（a.cols == b.rows）
//   matrix.scale(m, k)     -> list   矩阵标量乘法
//
// 矩阵以「列表的列表」表示：[[a, b], [c, d]]。

use crate::error::codes;
use crate::error::ZError;
use crate::interp::Value;
use crate::lexer::Span;

fn zerr(code: &'static str, msg: impl Into<String>, span: Span, file: &str, src: &str, help: Option<impl Into<String>>) -> ZError {
    ZError::new(code, msg, file, src, span.line, span.col, span.len.max(1), help)
}

/// 提取数字（int 或 float）为 f64。
fn as_num(v: &Value, what: &str, span: Span, file: &str, src: &str) -> Result<f64, ZError> {
    match v {
        Value::Int(i) => Ok(*i as f64),
        Value::Float(f) => Ok(*f),
        other => Err(zerr(
            codes::TYPE_MISMATCH,
            format!("`{}` expects a number, got `{}`", what, other.type_name()),
            span,
            file,
            src,
            None::<&str>,
        )),
    }
}

/// 提取数值列表（int/float 混合），返回 f64 列表。
fn num_list(v: &Value, what: &str, span: Span, file: &str, src: &str) -> Result<Vec<f64>, ZError> {
    match v {
        Value::List(items) => items
            .iter()
            .enumerate()
            .map(|(i, x)| as_num(x, &format!("{} element {}", what, i + 1), span, file, src))
            .collect(),
        other => Err(zerr(
            codes::TYPE_MISMATCH,
            format!("`{}` expects a list of numbers, got `{}`", what, other.type_name()),
            span,
            file,
            src,
            None::<&str>,
        )),
    }
}

/// 提取二维矩阵（f64）。
fn matrix(v: &Value, what: &str, span: Span, file: &str, src: &str) -> Result<Vec<Vec<f64>>, ZError> {
    match v {
        Value::List(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for (ri, row) in rows.iter().enumerate() {
                let nums = num_list(row, &format!("{} row {}", what, ri + 1), span, file, src)?;
                out.push(nums);
            }
            // 校验矩形形状（每行列数一致）
            if let Some(first) = out.first() {
                let cols = first.len();
                for (ri, row) in out.iter().enumerate() {
                    if row.len() != cols {
                        return Err(zerr(
                            codes::TYPE_MISMATCH,
                            format!(
                                "`{}` requires a rectangular matrix: row 1 has {} columns but row {} has {}",
                                what,
                                cols,
                                ri + 1,
                                row.len()
                            ),
                            span,
                            file,
                            src,
                            Some("pad or trim rows to the same length"),
                        ));
                    }
                }
            }
            Ok(out)
        }
        other => Err(zerr(
            codes::TYPE_MISMATCH,
            format!("`{}` expects a list of lists (matrix), got `{}`", what, other.type_name()),
            span,
            file,
            src,
            None::<&str>,
        )),
    }
}

/// 数值 → Value（int 值若为整数则返回 Int，否则 Float）。
fn to_value(x: f64) -> Value {
    if x.fract() == 0.0 && x.abs() < 9.007_199_254_740_992e15 {
        Value::Int(x as i64)
    } else {
        Value::Float(x)
    }
}

fn matrix_to_value(m: &[Vec<f64>]) -> Value {
    Value::List(
        m.iter()
            .map(|row| Value::List(row.iter().map(|x| to_value(*x)).collect()))
            .collect(),
    )
}

/// stat/matrix 模块调用入口。
pub fn call(name: &str, args: &[Value], span: Span, file: &str, src: &str) -> Result<Value, ZError> {
    match name {
        // ---------- stat ----------
        "stat.sum" => {
            let nums = num_list(&args[0], "stat.sum", span, file, src)?;
            let sum: f64 = nums.iter().sum();
            Ok(to_value(sum))
        }
        "stat.mean" => {
            let nums = num_list(&args[0], "stat.mean", span, file, src)?;
            if nums.is_empty() {
                return Err(zerr(codes::TYPE_MISMATCH, "stat.mean: empty list", span, file, src, Some("pass at least one number")));
            }
            Ok(Value::Float(nums.iter().sum::<f64>() / nums.len() as f64))
        }
        "stat.median" => {
            let mut nums = num_list(&args[0], "stat.median", span, file, src)?;
            if nums.is_empty() {
                return Err(zerr(codes::TYPE_MISMATCH, "stat.median: empty list", span, file, src, Some("pass at least one number")));
            }
            nums.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let mid = nums.len() / 2;
            let med = if nums.len() % 2 == 1 {
                nums[mid]
            } else {
                (nums[mid - 1] + nums[mid]) / 2.0
            };
            Ok(Value::Float(med))
        }
        "stat.variance" | "stat.stddev" => {
            let nums = num_list(&args[0], name, span, file, src)?;
            if nums.len() < 2 {
                return Err(zerr(codes::TYPE_MISMATCH, format!("{}: need at least 2 numbers", name), span, file, src, Some("pass at least two numbers")));
            }
            let mean = nums.iter().sum::<f64>() / nums.len() as f64;
            let var: f64 = nums.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>() / nums.len() as f64;
            if name == "stat.variance" {
                Ok(Value::Float(var))
            } else {
                Ok(Value::Float(var.sqrt()))
            }
        }
        "stat.min" | "stat.max" => {
            let nums = num_list(&args[0], name, span, file, src)?;
            if nums.is_empty() {
                return Err(zerr(codes::TYPE_MISMATCH, format!("{}: empty list", name), span, file, src, Some("pass at least one number")));
            }
            let cmp = |a: &f64, b: &f64| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal);
            let v = if name == "stat.min" {
                nums.iter().copied().min_by(cmp).unwrap()
            } else {
                nums.iter().copied().max_by(cmp).unwrap()
            };
            Ok(to_value(v))
        }
        // ---------- matrix ----------
        "matrix.identity" => {
            let n = match &args[0] {
                Value::Int(i) if *i >= 0 => *i as usize,
                other => {
                    return Err(zerr(
                        codes::TYPE_MISMATCH,
                        format!("matrix.identity expects a non-negative int, got `{}`", other.type_name()),
                        span,
                        file,
                        src,
                        None::<&str>,
                    ))
                }
            };
            let mut m = vec![vec![0.0f64; n]; n];
            for i in 0..n {
                m[i][i] = 1.0;
            }
            Ok(matrix_to_value(&m))
        }
        "matrix.transpose" => {
            let m = matrix(&args[0], "matrix.transpose", span, file, src)?;
            if m.is_empty() {
                return Ok(Value::List(Vec::new()));
            }
            let rows = m.len();
            let cols = m[0].len();
            let mut t = vec![vec![0.0f64; rows]; cols];
            for i in 0..rows {
                for j in 0..cols {
                    t[j][i] = m[i][j];
                }
            }
            Ok(matrix_to_value(&t))
        }
        "matrix.add" => {
            let a = matrix(&args[0], "matrix.add", span, file, src)?;
            let b = matrix(&args[1], "matrix.add", span, file, src)?;
            if a.len() != b.len() || (a.first().map(|r| r.len()).unwrap_or(0) != b.first().map(|r| r.len()).unwrap_or(0)) {
                return Err(zerr(
                    codes::TYPE_MISMATCH,
                    "matrix.add: shapes must match",
                    span,
                    file,
                    src,
                    Some("both matrices must have the same rows and columns"),
                ));
            }
            let rows = a.len();
            let cols = a.first().map(|r| r.len()).unwrap_or(0);
            let mut c = vec![vec![0.0f64; cols]; rows];
            for i in 0..rows {
                for j in 0..cols {
                    c[i][j] = a[i][j] + b[i][j];
                }
            }
            Ok(matrix_to_value(&c))
        }
        "matrix.mul" => {
            let a = matrix(&args[0], "matrix.mul", span, file, src)?;
            let b = matrix(&args[1], "matrix.mul", span, file, src)?;
            if a.is_empty() || b.is_empty() {
                return Err(zerr(codes::TYPE_MISMATCH, "matrix.mul: empty matrix", span, file, src, None::<&str>));
            }
            let (ar, ac) = (a.len(), a[0].len());
            let (br, bc) = (b.len(), b[0].len());
            if ac != br {
                return Err(zerr(
                    codes::TYPE_MISMATCH,
                    format!("matrix.mul: shapes mismatch ({}x{} @ {}x{})", ar, ac, br, bc),
                    span,
                    file,
                    src,
                    Some("matrix A columns must equal matrix B rows"),
                ));
            }
            let mut c = vec![vec![0.0f64; bc]; ar];
            for i in 0..ar {
                for j in 0..bc {
                    let mut s = 0.0;
                    for k in 0..ac {
                        s += a[i][k] * b[k][j];
                    }
                    c[i][j] = s;
                }
            }
            Ok(matrix_to_value(&c))
        }
        "matrix.scale" => {
            let m = matrix(&args[0], "matrix.scale", span, file, src)?;
            let k = as_num(&args[1], "matrix.scale scalar", span, file, src)?;
            let rows = m.len();
            let cols = m.first().map(|r| r.len()).unwrap_or(0);
            let mut c = vec![vec![0.0f64; cols]; rows];
            for i in 0..rows {
                for j in 0..cols {
                    c[i][j] = m[i][j] * k;
                }
            }
            Ok(matrix_to_value(&c))
        }
        _ => Err(zerr(
            codes::NOT_IMPLEMENTED,
            format!("unknown stat/matrix function `{}`", name),
            span,
            file,
            src,
            None::<&str>,
        )),
    }
}
