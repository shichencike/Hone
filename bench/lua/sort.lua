-- 基准 9：冒泡排序 1000 个确定性伪随机整数（与 bench/sort.hn / py/sort.py 等价，索引读写）
local nums = {}
local i = 0
while i < 1000 do
    nums[#nums + 1] = (i * 7919) % 1000000
    i = i + 1
end
local n = #nums
i = 0
while i < n - 1 do
    local j = 0
    while j < n - 1 - i do
        if nums[j + 1] > nums[j + 2] then
            local t = nums[j + 1]
            nums[j + 1] = nums[j + 2]
            nums[j + 2] = t
        end
        j = j + 1
    end
    i = i + 1
end
print(nums[1])
