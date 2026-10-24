# 基准 10：list 索引写 20 万 + 求和（与 bench/list_write.hn 等价）
arr = [0] * 1000
i = 0
while i < 200000:
    arr[i % 1000] = i
    i = i + 1
print(sum(arr))
