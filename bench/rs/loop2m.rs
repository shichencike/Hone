// 基准 3：纯循环 200 万次累加（与 bench/loop2m.hn 等价）
fn main() {
    let mut i: i64 = 0;
    let mut s: i64 = 0;
    while i < 2_000_000 {
        s += i;
        i += 1;
    }
    println!("{}", s);
}
