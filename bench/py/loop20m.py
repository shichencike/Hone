# 基准 2：纯循环 2000 万次累加（与 bench/loop.hn 等价）
i = 0
s = 0
while i < 20000000:
    s = s + i
    i = i + 1
print(s)
