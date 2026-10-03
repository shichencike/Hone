// 基准 4：字符串拼接 20 万次（与 bench/str.hn 等价）
fn main() {
    let mut s = String::new();
    let mut i = 0;
    while i < 200_000 {
        s.push_str("abc");
        i += 1;
    }
    println!("{}", s.len());
}
