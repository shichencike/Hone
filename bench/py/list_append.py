# 基准 6：列表构建 append 2 万 + 求和（与 bench/list_append.hn 等价）
nums = []
i = 0
while i < 20000:
    nums.append(i)
    i = i + 1
print(sum(nums))
