-- 基准 10：list 索引写 20 万 + 求和（与 bench/list_write.hn / py/list_write.py 等价；lua 表 1-based）
local arr = {}
local i = 0
while i < 1000 do
    arr[i + 1] = 0
    i = i + 1
end
i = 0
while i < 200000 do
    arr[i % 1000 + 1] = i
    i = i + 1
end
local sum = 0
for _, v in ipairs(arr) do
    sum = sum + v
end
print(sum)
