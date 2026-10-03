# 基准 3：纯循环 200 万次累加（与 bench/loop2m.hn 等价）
i = 0
s = 0
while i < 2000000:
    s = s + i
    i = i + 1
print(s)
