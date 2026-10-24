// 基准 10：list 索引写 20 万 + 求和（与 bench/list_write.hn 等价）
fn main() {
    let mut arr: Vec<i64> = vec![0; 1000];
    let mut i = 0i64;
    while i < 200_000 {
        arr[(i % 1000) as usize] = i;
        i += 1;
    }
    let sum: i64 = arr.iter().sum();
    println!("{}", sum);
}
