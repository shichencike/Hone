// 基准 7：字典查找 200 万次（与 bench/dict.hn 等价）
use std::collections::HashMap;

fn main() {
    let mut d = HashMap::new();
    d.insert("a", 1);
    d.insert("b", 2);
    d.insert("c", 3);
    let mut i = 0i64;
    let mut s = 0i64;
    while i < 2_000_000 {
        if d.contains_key("a") {
            s += 1;
        }
        i += 1;
    }
    println!("{}", s);
}
