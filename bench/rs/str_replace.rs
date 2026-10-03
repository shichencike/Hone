// 基准 8：字符串替换 10 万次（与 bench/str_replace.hn 等价）
fn main() {
    let base = "abcabcabcabcabcabcabcabcabcabc";
    let mut s = base.to_string();
    let mut i = 0;
    while i < 100_000 {
        s = s.replace("abc", "bca");
        i += 1;
    }
    println!("{}", s.len());
}
