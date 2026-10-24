-- 基准 2：纯循环 2000 万次累加（与 bench/loop.hn / py/loop20m.py 等价）
local i = 0
local s = 0
while i < 20000000 do
    s = s + i
    i = i + 1
end
print(s)
