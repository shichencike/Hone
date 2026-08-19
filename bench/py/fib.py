# 基准 1：递归斐波那契 fib(26)（与 bench/fib.hn 等价）
def fib(n):
    if n <= 1:
        return n
    return fib(n - 1) + fib(n - 2)

print(fib(26))
