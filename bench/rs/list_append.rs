// 基准 6：列表构建 append 2 万 + 求和（与 bench/list_append.hn 等价）
fn main() {
    let mut nums: Vec<i64> = Vec::new();
    let mut i = 0i64;
    while i < 20_000 {
        nums.push(i);
        i += 1;
    }
    let sum: i64 = nums.iter().sum();
    println!("{}", sum);
}
