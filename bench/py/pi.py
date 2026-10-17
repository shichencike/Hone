# 基准 5：浮点运算，莱布尼茨级数 2000 万项（与 bench/pi.hn 等价）
pi = 0.0
sign = 1.0
i = 0
while i < 20000000:
    pi = pi + sign / (2.0 * i + 1.0)
    sign = -sign
    i = i + 1
print(pi)
