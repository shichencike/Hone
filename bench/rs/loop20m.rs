// 基准 2：纯循环 2000 万次累加（与 bench/loop.hn 等价）
fn main() {
    let mut i: i64 = 0;
    let mut s: i64 = 0;
    while i < 20_000_000 {
        s += i;
        i += 1;
    }
    println!("{}", s);
}
