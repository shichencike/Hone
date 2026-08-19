# 基准 7：字典查找 200 万次（与 bench/dict.hn 等价）
d = {"a": 1, "b": 2, "c": 3}
i = 0
s = 0
while i < 2000000:
    if "a" in d:
        s = s + 1
    i = i + 1
print(s)
