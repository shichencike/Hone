# 基准 4：字符串拼接 20 万次（与 bench/str.hn 等价）
s = ""
i = 0
while i < 200000:
    s = s + "abc"
    i = i + 1
print(len(s))
