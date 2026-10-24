-- 基准 5：浮点运算，莱布尼茨级数 2000 万项（与 bench/pi.hn / py/pi.py 等价）
local pi = 0.0
local sign = 1.0
local i = 0
while i < 20000000 do
    pi = pi + sign / (2.0 * i + 1.0)
    sign = -sign
    i = i + 1
end
print(pi)
