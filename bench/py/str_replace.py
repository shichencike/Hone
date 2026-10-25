# 基准 8：字符串替换 10 万次（与 bench/str_replace.hn 等价）
base = "abcabcabcabcabcabcabcabcabcabc"
s = base
i = 0
while i < 100000:
    s = s.replace("abc", "bca")
    i = i + 1
print(len(s))
