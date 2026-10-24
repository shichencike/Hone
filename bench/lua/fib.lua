-- 基准 1：递归斐波那契 fib(26)（与 bench/fib.hn / py/fib.py 等价）
local function fib(n)
    if n <= 1 then
        return n
    end
    return fib(n - 1) + fib(n - 2)
end

print(fib(26))
