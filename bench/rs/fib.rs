// 基准 1：递归斐波那契 fib(26)（与 bench/fib.hn 等价）
fn fib(n: u64) -> u64 {
    if n <= 1 {
        return n;
    }
    fib(n - 1) + fib(n - 2)
}

fn main() {
    println!("{}", fib(26));
}
