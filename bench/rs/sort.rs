// 基准 9：冒泡排序 1000 个确定性伪随机整数（与 bench/sort.hn 等价，索引读写）
fn main() {
    let mut nums: Vec<i64> = Vec::new();
    let mut i = 0i64;
    while i < 1000 {
        nums.push((i * 7919) % 1_000_000);
        i += 1;
    }
    let n = nums.len();
    i = 0;
    while i < (n - 1) as i64 {
        let mut j = 0i64;
        while j < (n - 1) as i64 - i {
            let a = nums[j as usize];
            let b = nums[j as usize + 1];
            if a > b {
                nums[j as usize] = b;
                nums[j as usize + 1] = a;
            }
            j += 1;
        }
        i += 1;
    }
    println!("{}", nums[0]);
}
