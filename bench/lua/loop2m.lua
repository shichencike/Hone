-- 基准 3：纯循环 200 万次累加（与 bench/loop2m.hn / py/loop2m.py 等价）
local i = 0
local s = 0
while i < 2000000 do
    s = s + i
    i = i + 1
end
print(s)
