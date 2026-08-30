// 基准 5：浮点运算，莱布尼茨级数 2000 万项（与 bench/pi.hn 等价）
fn main() {
    let mut pi = 0.0f64;
    let mut sign = 1.0f64;
    let mut i = 0i64;
    while i < 20_000_000 {
        pi += sign / (2.0 * i as f64 + 1.0);
        sign = -sign;
        i += 1;
    }
    println!("{}", pi);
}
