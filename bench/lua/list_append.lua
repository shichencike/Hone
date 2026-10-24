-- 基准 6：列表构建 append 2 万 + 求和（与 bench/list_append.hn / py/list_append.py 等价）
local nums = {}
local i = 0
while i < 20000 do
    nums[#nums + 1] = i
    i = i + 1
end
local sum = 0
for _, v in ipairs(nums) do
    sum = sum + v
end
print(sum)
