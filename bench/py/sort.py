# 基准 9：冒泡排序 1000 个确定性伪随机整数（与 bench/sort.hn 等价，索引读写）
nums = []
i = 0
while i < 1000:
    nums.append((i * 7919) % 1000000)
    i = i + 1
n = len(nums)
i = 0
while i < n - 1:
    j = 0
    while j < n - 1 - i:
        if nums[j] > nums[j + 1]:
            t = nums[j]
            nums[j] = nums[j + 1]
            nums[j + 1] = t
        j = j + 1
    i = i + 1
print(nums[0])
